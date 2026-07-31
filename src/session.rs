//! Sessions: a durable, branchable conversation with the harness.
//!
//! A [`Session`] is a tree of turns over one workspace, held in the same
//! [`Store`] the runs are in. A turn **is** a run — its own trace, its own
//! budgets, its own policy boundary, its own checkpoint — so a session is durable
//! for exactly the reason a run already is, and a turn that crashes mid-answer is
//! resumable by its run id.
//!
//! Three things this module adds over the one-shot entry points:
//!
//! * **Continuity.** The turns from the tree's root to its head are handed to the
//!   next turn as observations, so the model reads the conversation through the
//!   same context assembler — and the same [`ContextBudget`](crate::ContextBudget)
//!   — that already decides what a long run's history gets to say.
//! * **Streaming.** An observed turn asks the provider for deltas and emits them
//!   as [`EventKind::Token`](crate::EventKind::Token) while the model is still
//!   producing them, instead of accumulating in silence until the step is over.
//! * **Steering.** A [`Steer`] lets an operator say something else mid-turn, or
//!   interrupt, and both land at the next step boundary — the only point in the
//!   loop that is safe to change course from.
//!
//! What a session is not: an authorization channel. An operator's mid-turn
//! message is text the model reads, exactly as a constraint is; every tool call it
//! leads to is checked against the same [`Policy`] by the same code. And one
//! session driven by two processes at once is not supported — the turns would
//! interleave into one tree with no ordering anybody chose.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::approve::Approver;
use crate::context::{bound, entry_cap_chars, ObsKind};
use crate::error::{Error, Result};
use crate::observe::{Ignore, Observer};
use crate::policy::Policy;
use crate::provider::Provider;
use crate::run::{RunOutcome, TurnExtras, NO_TOOL_CALL};
use crate::state::{Store, Turn};
use crate::TaskContract;

/// A durable conversation over one workspace.
///
/// Open one, take turns against it, and pick it up in a later process by its id.
/// The conversation is a tree: [`branch_from`](Session::branch_from) makes any
/// earlier turn the parent of the next one, and nothing is rewritten to do it, so
/// the branch you left is still readable.
///
/// ```no_run
/// use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};
///
/// # async fn demo(policy: &Policy) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let provider = OpenRouter::from_env()?;
/// let mut session = Session::open(&store, "/path/to/repo")?;
///
/// // Each turn is a run: budgeted, policy-bounded, and in the trace under its
/// // own run id.
/// let first = session.turn("what does the retry policy actually retry?",
///                          &provider, &store, policy, &ApproveAll).await?;
/// println!("{}", first.reply.unwrap_or_default());
///
/// // The second turn reads the first, because the conversation is the context.
/// session.turn("now make it retry a 503 as well",
///              &provider, &store, policy, &ApproveAll).await?;
///
/// // Keep the id. It is all a later process needs.
/// let id = session.id();
/// let mut later = Session::reopen(&store, id)?;
/// later.turn("did that land?", &provider, &store, policy, &ApproveAll).await?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct Session {
    id: i64,
    root: PathBuf,
    head: Option<i64>,
}

impl Session {
    /// Open a new session over `root`.
    ///
    /// ```
    /// use io_harness::{Session, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let session = Session::open(&store, "/repo")?;
    ///
    /// // A fresh conversation: an id that outlives the process, and no head yet.
    /// assert!(session.id() > 0);
    /// assert_eq!(session.head(), None);
    /// assert_eq!(session.root(), std::path::Path::new("/repo"));
    /// # Ok(())
    /// # }
    /// ```
    pub fn open(store: &Store, root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        let id = store.create_session(&root.display().to_string())?;
        Ok(Self {
            id,
            root,
            head: None,
        })
    }

    /// Pick up an existing session by id, in this or any later process.
    ///
    /// The root comes from the store rather than from the caller: a session whose
    /// workspace argument changed between processes would otherwise carry a
    /// conversation about one directory into another.
    ///
    /// ```
    /// use io_harness::{Session, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let id = Session::open(&store, "/repo")?.id();
    ///
    /// let again = Session::reopen(&store, id)?;
    /// assert_eq!(again.root(), std::path::Path::new("/repo"));
    ///
    /// // An id that was never opened is an error, not an empty conversation.
    /// assert!(Session::reopen(&store, id + 999).is_err());
    /// # Ok(())
    /// # }
    /// ```
    pub fn reopen(store: &Store, id: i64) -> Result<Self> {
        let Some(root) = store.session_root(id)? else {
            return Err(Error::Config(format!(
                "no session {id} in this store; the id must come from Session::id() \
                 on a session opened against the same database"
            )));
        };
        Ok(Self {
            id,
            root: PathBuf::from(root),
            head: store.session_head(id)?,
        })
    }

    /// The session's durable id — the one thing a later process needs.
    pub fn id(&self) -> i64 {
        self.id
    }

    /// The workspace this conversation is about.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The turn the next one will answer from, or `None` before the first turn.
    pub fn head(&self) -> Option<i64> {
        self.head
    }

    /// The conversation as the model sees it: the path from the tree's root to the
    /// head, oldest first.
    ///
    /// Not every turn of the session — that is the whole tree, and
    /// [`Store::session_turns`] returns it. A branch's turns are not on another
    /// branch's path, which is the point of branching.
    ///
    /// ```no_run
    /// use io_harness::{Session, Store};
    ///
    /// # fn demo(store: &Store, session: &Session) -> io_harness::Result<()> {
    /// for turn in session.history(store)? {
    ///     println!("> {}", turn.prompt);
    ///     println!("{}", turn.reply.unwrap_or_default());
    /// }
    /// # Ok(()) }
    /// ```
    pub fn history(&self, store: &Store) -> Result<Vec<Turn>> {
        let all = store.session_turns(self.id)?;
        let mut path = Vec::new();
        let mut at = self.head;
        // Bounded by the session's own turn count: a parent id is always smaller
        // than its child's, so a cycle cannot be written through this API — and a
        // store edited by hand must not be able to hang a caller either.
        while let Some(id) = at {
            let Some(turn) = all.iter().find(|t| t.id == id) else {
                break;
            };
            path.push(turn.clone());
            at = turn.parent_turn_id;
            if path.len() > all.len() {
                break;
            }
        }
        path.reverse();
        Ok(path)
    }

    /// Make `turn_id` the parent of the next turn.
    ///
    /// A branch, not an edit: the turns that came after it are untouched and still
    /// readable, and the next turn simply does not see them. Refused if the turn
    /// belongs to another session — a conversation cannot be grafted onto one it
    /// was never part of.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let provider = OpenRouter::from_env()?;
    /// let mut session = Session::open(store, "/repo")?;
    ///
    /// let first = session.turn("draft a migration plan", &provider, store, policy, &ApproveAll).await?;
    /// session.turn("do it with a blue-green cutover", &provider, store, policy, &ApproveAll).await?;
    ///
    /// // That was the wrong direction. Go back to the plan and take the other one,
    /// // without losing what the first attempt said.
    /// session.branch_from(store, first.turn_id)?;
    /// session.turn("do it with a read-only window instead", &provider, store, policy, &ApproveAll).await?;
    /// # Ok(()) }
    /// ```
    pub fn branch_from(&mut self, store: &Store, turn_id: i64) -> Result<()> {
        let Some(turn) = store.session_turn(turn_id)? else {
            return Err(Error::Config(format!("no turn {turn_id} in this store")));
        };
        if turn.session_id != self.id {
            return Err(Error::Config(format!(
                "turn {turn_id} belongs to session {}, not {}",
                turn.session_id, self.id
            )));
        }
        self.head = Some(turn_id);
        store.set_session_head(self.id, self.head)?;
        Ok(())
    }

    /// Take one turn: say `text` and let the agent work until it stops calling
    /// tools.
    ///
    /// Unbounded in the verification sense — there is no criterion to pass, so the
    /// turn ends as [`RunOutcome::Finished`] on an assistant turn that calls no
    /// tool. Bound it with [`turn_bounded`](Session::turn_bounded) when the turn
    /// has a checkable definition of done.
    ///
    /// Quiet: no observer, so no [`EventKind::Token`](crate::EventKind::Token)
    /// events and no streaming request to the provider.
    pub async fn turn<P: Provider>(
        &mut self,
        text: impl Into<String>,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
    ) -> Result<TurnResult> {
        let contract = self.default_contract(text);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            &Ignore,
            TurnExtras::default(),
        )
        .await
    }

    /// [`turn`](Session::turn), reporting to `observer` as it happens — including
    /// each chunk of assistant text as the model produces it.
    ///
    /// This is the entry point a terminal or a desktop window wants: the deltas
    /// arrive as [`EventKind::Token`](crate::EventKind::Token) while the request is
    /// still open, so there is something to render before the step is over.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, EventKind, Flow, Observer, OpenRouter, Policy, RunEvent,
    ///                  Session, Store};
    /// use std::io::Write;
    ///
    /// /// Prints the answer as it is typed.
    /// struct Live;
    ///
    /// impl Observer for Live {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         if let EventKind::Token { text } = &event.kind {
    ///             print!("{text}");
    ///             let _ = std::io::stdout().flush();
    ///         }
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    /// session.turn_observed("summarise what this crate does", &OpenRouter::from_env()?,
    ///                       store, policy, &ApproveAll, &Live).await?;
    /// # Ok(()) }
    /// ```
    pub async fn turn_observed<P: Provider>(
        &mut self,
        text: impl Into<String>,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        observer: &dyn Observer,
    ) -> Result<TurnResult> {
        let contract = self.default_contract(text);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            observer,
            TurnExtras {
                stream: true,
                ..Default::default()
            },
        )
        .await
    }

    /// [`turn_observed`](Session::turn_observed), reading `steer` at every step
    /// boundary so an operator can change course or interrupt mid-turn.
    ///
    /// The turn's future is driven on the caller's task — [`Store`] is `!Sync` — so
    /// steer it from a `select!` beside the turn, or from another thread or task
    /// through the [`Steer`], which is `Send + Sync`.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, Ignore, OpenRouter, Policy, Session, Steer, Store};
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let (steer, inbox) = Steer::channel();
    /// let mut session = Session::open(store, "/repo")?;
    ///
    /// // Another task, another thread, a UI event handler: `Steer` is `Send + Sync`.
    /// let handle = steer.clone();
    /// tokio::spawn(async move {
    ///     // "not the tests, the docs" — read at the next step boundary.
    ///     let _ = handle.say("actually, only touch the docs");
    /// });
    ///
    /// let result = session.turn_steered("bring the docs up to date", &OpenRouter::from_env()?,
    ///                                   store, policy, &ApproveAll, &Ignore, &inbox).await?;
    ///
    /// // An interrupt ends the turn as `Cancelled`, whole steps only, still resumable.
    /// println!("{:?}", result.outcome);
    /// # Ok(()) }
    /// ```
    #[allow(clippy::too_many_arguments)]
    pub async fn turn_steered<P: Provider>(
        &mut self,
        text: impl Into<String>,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        observer: &dyn Observer,
        steer: &SteerInbox,
    ) -> Result<TurnResult> {
        let contract = self.default_contract(text);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            observer,
            TurnExtras {
                stream: true,
                steer: Some(steer),
                ..Default::default()
            },
        )
        .await
    }

    /// One turn bounded by a caller-supplied contract: a verification gate, its own
    /// budgets, its own tools, MCP servers or skills.
    ///
    /// The contract's `goal` is what the operator said, and its `root` is replaced
    /// by the session's — a turn is about the conversation's workspace, and a
    /// contract naming another one would be answering about a different project.
    /// Bounds apply to this turn only; the next turn is unbounded again unless it
    /// carries its own contract.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store, TaskContract,
    ///                  Verification};
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    /// session.turn("the retry test is flaky", &OpenRouter::from_env()?, store, policy,
    ///              &ApproveAll).await?;
    ///
    /// // This turn has a checkable definition of done, so it gets a gate: the
    /// // project's own command decides, and the turn reports `Success` only if it
    /// // passes.
    /// let contract = TaskContract::workspace("fix it", "/repo")
    /// .with_verification(Verification::Command {
    ///     argv: vec!["cargo".into(), "test".into()],
    ///     expect_exit: 0,
    /// })
    /// .with_max_steps(20);
    /// let result = session.turn_bounded(&contract, &OpenRouter::from_env()?, store, policy,
    ///                                   &ApproveAll).await?;
    /// println!("{:?}", result.outcome);
    /// # Ok(()) }
    /// ```
    pub async fn turn_bounded<P: Provider>(
        &mut self,
        contract: &TaskContract,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
    ) -> Result<TurnResult> {
        let contract = self.rooted(contract);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            &Ignore,
            TurnExtras::default(),
        )
        .await
    }

    /// [`turn_bounded`](Session::turn_bounded), reporting to `observer` and
    /// streaming the model's text as it arrives.
    pub async fn turn_bounded_observed<P: Provider>(
        &mut self,
        contract: &TaskContract,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        observer: &dyn Observer,
    ) -> Result<TurnResult> {
        let contract = self.rooted(contract);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            observer,
            TurnExtras {
                stream: true,
                ..Default::default()
            },
        )
        .await
    }

    /// What an unbounded turn runs under: the session's workspace, no criterion,
    /// and the crate's own defaults for everything else.
    fn default_contract(&self, text: impl Into<String>) -> TaskContract {
        TaskContract::workspace(text, self.root.clone())
    }

    /// The caller's contract, over the session's workspace.
    fn rooted(&self, contract: &TaskContract) -> TaskContract {
        let mut contract = contract.clone();
        contract.root = Some(self.root.clone());
        contract.file = self.root.clone();
        contract
    }

    /// Every turn is this function: record the turn, drive the run, close the turn,
    /// move the head. One place, so a turn taken through any of the five entry
    /// points is the same turn in the tree.
    #[allow(clippy::too_many_arguments)]
    async fn drive<P: Provider>(
        &mut self,
        contract: &TaskContract,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        observer: &dyn Observer,
        mut extras: TurnExtras<'_>,
    ) -> Result<TurnResult> {
        let seed = self.seed(store, contract)?;
        extras.seed = &seed;

        // The run row and the turn row before the first completion is billed, so a
        // turn whose process dies mid-answer is in the tree with a run id a resume
        // can continue from — the same order `run_with_observed` starts a run in.
        extras.turn = Some(SessionTurn {
            session_id: self.id,
            parent_turn_id: self.head,
            prompt: &contract.goal,
        });
        let result = crate::run::run_with_extras(
            contract, provider, store, policy, approver, observer, &extras,
        )
        .await?;

        let turn_id = store
            .turn_for_run(result.run_id)?
            .ok_or_else(|| Error::Config(format!("run {} recorded no turn", result.run_id)))?;
        let reply = last_message(store, result.run_id)?;
        let outcome = store
            .run_summary(result.run_id)?
            .map(|s| s.outcome)
            .unwrap_or_else(|| "running".into());
        store.finish_turn(turn_id, reply.as_deref(), &outcome)?;
        self.head = Some(turn_id);
        store.set_session_head(self.id, self.head)?;

        Ok(TurnResult {
            turn_id,
            run_id: result.run_id,
            outcome: result.outcome,
            reply,
        })
    }

    /// The conversation, rendered as the observations the next turn starts from.
    ///
    /// One entry per prior turn on the path, each bounded by the same per-entry cap
    /// the loop bounds a tool result by, so a long conversation is compacted by the
    /// assembler rather than by a rule of this module's own.
    fn seed(&self, store: &Store, contract: &TaskContract) -> Result<Vec<String>> {
        let cap = entry_cap_chars(contract.context.effective_tokens(contract.max_tokens));
        let mut out = Vec::new();
        for turn in self.history(store)? {
            out.push(bound(
                &format!("\n[earlier turn] the operator asked: {}\n", turn.prompt),
                cap,
                ObsKind::Message,
            ));
            if let Some(reply) = turn.reply.as_deref().filter(|r| !r.is_empty()) {
                out.push(bound(
                    &format!("\n[earlier turn] you answered: {reply}\n"),
                    cap,
                    ObsKind::Message,
                ));
            }
        }
        Ok(out)
    }
}

/// What one turn produced.
///
/// The `turn_id` is the handle for [`Session::branch_from`]; the `run_id` is the
/// handle for everything else the crate already offers — [`crate::resume`],
/// [`Store::run_summary`], the whole trace.
///
/// ```no_run
/// use io_harness::{ApproveAll, OpenRouter, Policy, RunOutcome, Session, Store};
///
/// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
/// let mut session = Session::open(store, "/repo")?;
/// let turn = session.turn("what changed in the last commit?", &OpenRouter::from_env()?,
///                         store, policy, &ApproveAll).await?;
///
/// // A conversational turn ends when the agent stops calling tools.
/// assert!(matches!(turn.outcome, RunOutcome::Finished { .. }));
///
/// // What it cost is read from the run, because a turn IS a run.
/// if let Some(summary) = store.run_summary(turn.run_id)? {
///     println!("{} tokens", summary.tokens);
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
pub struct TurnResult {
    /// This turn's id in the conversation tree.
    pub turn_id: i64,
    /// The run that served it.
    pub run_id: i64,
    /// Why it stopped.
    pub outcome: RunOutcome,
    /// What the agent said last, when it ended by saying something. `None` for a
    /// turn that stopped on a ceiling, a refusal or an interrupt mid-work — there
    /// was no closing message, and inventing one would misreport the ending.
    pub reply: Option<String>,
}

/// The tree bookkeeping one turn needs, handed to the run loop so the turn row is
/// written under the same run id the loop just created.
pub(crate) struct SessionTurn<'a> {
    pub session_id: i64,
    pub parent_turn_id: Option<i64>,
    pub prompt: &'a str,
}

/// An operator's channel into a running turn: say something else, or stop.
///
/// `Send + Sync` and cheap to clone, so a UI thread, a signal handler or another
/// task can hold one while the turn runs on the task that started it.
///
/// Both a message and an interrupt land at the **next step boundary** — the same
/// point [`Flow::Cancel`](crate::Flow::Cancel) is honoured at, and for the same
/// reason: in between, a tool call is in flight and a file may be half-written.
///
/// A message is text, not permission. It reaches the model exactly as a
/// constraint does, and every tool call it leads to is checked against the same
/// [`Policy`] by the same code — "just do it" in a steer does not widen a
/// boundary.
///
/// ```
/// use io_harness::Steer;
///
/// # fn main() -> io_harness::Result<()> {
/// let (steer, inbox) = Steer::channel();
/// steer.say("prefer the smaller diff")?;
/// steer.interrupt()?;
///
/// // Nothing is delivered until the turn reads its inbox at a step boundary, so
/// // an operator who says three things in one second has said three things.
/// let (messages, interrupted) = inbox.pending();
/// assert_eq!(messages, vec!["prefer the smaller diff".to_string()]);
/// assert!(interrupted);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone)]
pub struct Steer {
    tx: mpsc::UnboundedSender<Steered>,
}

/// The receiving half, handed to [`Session::turn_steered`].
///
/// Not `Clone`: one turn reads one inbox. Two readers would each get some of the
/// operator's messages and neither would get all of them.
///
/// The turn drains it at every step boundary. A caller holds one only to hand it
/// to a turn — and, when a turn is over, to see what was still in it:
///
/// ```
/// use io_harness::Steer;
///
/// # fn main() -> io_harness::Result<()> {
/// let (steer, inbox) = Steer::channel();
/// steer.say("use the smaller diff")?;
///
/// // Nothing is lost by being early: a message sent before the turn's first step
/// // is read at that step, like any other.
/// let (messages, interrupted) = inbox.pending();
/// assert_eq!(messages, vec!["use the smaller diff".to_string()]);
/// assert!(!interrupted);
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SteerInbox {
    rx: std::cell::RefCell<mpsc::UnboundedReceiver<Steered>>,
}

/// One thing an operator sent.
#[derive(Debug, Clone)]
enum Steered {
    Say(String),
    Interrupt,
}

impl Steer {
    /// A steer and the inbox its messages arrive in.
    pub fn channel() -> (Steer, SteerInbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Steer { tx },
            SteerInbox {
                rx: std::cell::RefCell::new(rx),
            },
        )
    }

    /// Say something to the agent mid-turn. Delivered at the next step boundary.
    ///
    /// An error means the turn has already ended and nobody will read it — said
    /// rather than swallowed, because an operator whose correction went nowhere
    /// needs to know it went nowhere.
    pub fn say(&self, text: impl Into<String>) -> Result<()> {
        self.send(Steered::Say(text.into()))
    }

    /// Stop the turn at the next step boundary.
    ///
    /// The turn is finished, not abandoned: the step it is on commits whole, the
    /// run records `cancelled`, and the outcome is
    /// [`RunOutcome::Cancelled`]. It stays resumable,
    /// and the session goes on — the interrupted turn is in the tree with its
    /// outcome, and the next turn reads it like any other.
    pub fn interrupt(&self) -> Result<()> {
        self.send(Steered::Interrupt)
    }

    fn send(&self, message: Steered) -> Result<()> {
        self.tx.send(message).map_err(|_| {
            Error::Config(
                "this turn has ended, so nothing will read the steer; start another turn".into(),
            )
        })
    }
}

impl SteerInbox {
    /// Everything sent since the last read: the messages in order, and whether an
    /// interrupt was among them.
    ///
    /// Public so a caller can drain an inbox they are no longer going to hand to a
    /// turn and see what was in it, rather than discovering that an operator's last
    /// message vanished with the channel.
    pub fn pending(&self) -> (Vec<String>, bool) {
        let drained = self.drain();
        (drained.messages, drained.interrupted)
    }

    /// What the run loop reads at a step boundary.
    pub(crate) fn drain(&self) -> Drained {
        let mut out = Drained::default();
        let mut rx = self.rx.borrow_mut();
        while let Ok(message) = rx.try_recv() {
            match message {
                Steered::Say(text) => out.messages.push(text),
                // Not a `break`: an operator who typed a correction and then hit
                // interrupt sent both, and the trace should hold both.
                Steered::Interrupt => out.interrupted = true,
            }
        }
        out
    }
}

/// One step boundary's worth of steering.
#[derive(Debug, Default)]
pub(crate) struct Drained {
    pub messages: Vec<String>,
    pub interrupted: bool,
}

/// The last thing the agent said, if its last word was a message rather than a
/// tool call.
///
/// Read out of the run's own observations rather than carried back through
/// [`crate::RunResult`], which would have meant a new public field on a struct
/// callers match exhaustively. The marker is
/// [`NO_TOOL_CALL`](crate::run::NO_TOOL_CALL), shared with the loop that writes it,
/// so the two cannot drift.
fn last_message(store: &Store, run_id: i64) -> Result<Option<String>> {
    for obs in store.observations(run_id)?.into_iter().rev() {
        if obs.kind != ObsKind::Message {
            continue;
        }
        if let Some((_, said)) = obs.text.split_once(NO_TOOL_CALL) {
            let said = said.trim();
            return Ok((!said.is_empty()).then(|| said.to_string()));
        }
    }
    Ok(None)
}
