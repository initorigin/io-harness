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
//! session driven by two processes at once no longer interleaves silently: both
//! head advances here — [`Session::branch_from`] and the one at the end of a turn
//! — are a compare-and-swap on the head that was read, so the second writer is
//! told with [`Error::Conflict`](crate::Error::Conflict) and its turn is left out
//! of the tree. That reports a dropped turn; it does not make both of them land.

use std::path::{Path, PathBuf};

use tokio::sync::mpsc;

use crate::approve::Approver;
use crate::containment::Containment;
use crate::context::{bound, entry_cap_chars, ObsKind};
use crate::error::{Error, Result};
use crate::observe::{Ignore, Observer};
use crate::policy::Policy;
use crate::provider::Provider;
use crate::run::{RunOutcome, TurnExtras, NO_TOOL_CALL};
use crate::state::{Store, Turn};
use crate::verify::Verification;
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
    /// Images staged by [`Session::attach`] for the next turn only (0.43.0).
    ///
    /// Not durable, and deliberately: a screenshot is about the thing the operator
    /// is saying now, and a conversation that silently re-sent it on every later
    /// turn would be paying for it every turn.
    #[cfg(feature = "media")]
    staged: Vec<crate::provider::Media>,
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
            #[cfg(feature = "media")]
            staged: Vec::new(),
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
            #[cfg(feature = "media")]
            staged: Vec::new(),
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
        // Compare-and-swap against the head this handle believes it is moving
        // (0.62.0). A branch is a deliberate move to an earlier turn, and it is
        // still a lost update if another process moved the head while this one was
        // deciding — the local head is only advanced once the store took the write.
        store.set_session_head_if(self.id, self.head, Some(turn_id))?;
        self.head = Some(turn_id);
        Ok(())
    }

    /// Hand the next turn images to look at, alongside whatever it says (0.43.0).
    ///
    /// [`TaskContract::with_images`](crate::TaskContract::with_images) has taken
    /// images since the `media` feature shipped, and every turn entry point builds
    /// its contract from a `&str` — so the one path an operator would hand a
    /// screenshot to was the only path that could not take one. This is that path,
    /// and it is one method rather than an images-carrying variant of each of the
    /// six turn shapes: staging is orthogonal to how the turn is driven, and a
    /// seventh entry point would owe an eighth the next time a turn shape is added.
    ///
    /// **The next turn only.** The staging is cleared once the turn has been
    /// driven, whatever its outcome, because a screenshot is about the thing being
    /// said now and re-sending it every later turn would bill for it every turn.
    /// A contract's own [`with_images`](crate::TaskContract::with_images) is the
    /// other half and still means what it always did — for the whole run — so a
    /// [`turn_bounded`](Session::turn_bounded) carrying both sends both.
    ///
    /// A provider that does not accept images refuses the turn before anything is
    /// sent; see [`Provider::accepts_images`].
    ///
    /// ```no_run
    /// use io_harness::provider::Media;
    /// use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    /// let shot = Media::image("image/png", &std::fs::read("screenshot.png")?)?;
    ///
    /// session.attach([shot]);
    /// session.turn("why is this button misaligned?", &OpenRouter::from_env()?,
    ///              store, policy, &ApproveAll).await?;
    ///
    /// // The next turn carries no image unless another is attached.
    /// session.turn("and the one below it?", &OpenRouter::from_env()?,
    ///              store, policy, &ApproveAll).await?;
    /// # Ok(()) }
    /// ```
    #[cfg(feature = "media")]
    pub fn attach<I>(&mut self, images: I) -> &mut Self
    where
        I: IntoIterator<Item = crate::provider::Media>,
    {
        self.staged.extend(images);
        self
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
            None,
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
            None,
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
            None,
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
            None,
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
            None,
            TurnExtras {
                stream: true,
                ..Default::default()
            },
        )
        .await
    }

    /// Take a turn that may fan out: the agent answering it is offered
    /// [`SPAWN_TOOL`](crate::SPAWN_TOOL) and can decompose the work into contained
    /// sub-agents, under `containment` (0.39.0).
    ///
    /// This is the one turn shape that can. The five above drive the flat
    /// workspace loop, which has never offered the spawn tool and still does not,
    /// so a session that never calls this behaves exactly as it did in 0.38.0.
    ///
    /// What the fan-out inherits, and what bounds it:
    ///
    /// - **The session's policy.** A child gets it through
    ///   [`Policy::contain`](crate::Policy::contain) and may only narrow it, at any
    ///   depth, however its goal is worded.
    /// - **One shared [`Ledger`](crate::Ledger), per turn.** Every agent in the
    ///   turn draws on it and no child's contract can raise it. It is built fresh
    ///   for each turn, so a conversation's total spend is the sum of its turns'
    ///   rather than one ceiling across all of them.
    /// - **The transcript.** Children report to the same observer with their own
    ///   `run_id` and a non-zero `depth`, and the whole fan-out is reconstructable
    ///   from [`Store::agent_events`](crate::Store::agent_events) on
    ///   [`TurnResult::run_id`].
    ///
    /// A child is given its goal, not the conversation — forty children each
    /// carrying the transcript is the multiplied version of the cost the context
    /// budget exists to bound — and a child is a run, never a second turn:
    /// [`Session::history`] still renders one entry for this turn.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, Containment, OpenRouter, Policy, Session, Store};
    ///
    /// # async fn demo(store: &Store) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    ///
    /// // The boundary for the whole fan-out: a child inherits it and may only
    /// // narrow it, so no descendant writes outside docs/ whatever it is asked.
    /// let policy = Policy::default().layer("app").allow_read("*").allow_write("docs/*");
    ///
    /// let turn = session
    ///     .turn_contained(
    ///         "document every public module under docs/, one file per module",
    ///         &OpenRouter::from_env()?,
    ///         store,
    ///         &policy,
    ///         &ApproveAll,
    ///         // Twelve agents in all, four at once per tier, two deep, and one
    ///         // token ceiling for the turn. A spawn past the concurrency cap
    ///         // queues; one past the total cap is refused.
    ///         &Containment::new(12, 4, 2, 500_000),
    ///     )
    ///     .await?;
    ///
    /// // Still one turn in the conversation, whatever it spawned — the children
    /// // are runs under this turn's run, which is where they are counted.
    /// println!("{:?} {}", turn.kind, store.children(turn.run_id)?.len());
    /// # Ok(()) }
    /// ```
    ///
    /// A turn that stops for an approval, a question or a plan is continued with
    /// the tree resumes — [`resume_tree_with_decision`](crate::resume_tree_with_decision),
    /// [`resume_tree_with_answer`](crate::resume_tree_with_answer) or
    /// [`resume_tree_with_plan_decision`](crate::resume_tree_with_plan_decision) —
    /// on `TurnResult::run_id`, not the flat ones.
    pub async fn turn_contained<P: Provider>(
        &mut self,
        text: impl Into<String>,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        containment: &Containment,
    ) -> Result<TurnResult> {
        let contract = self.default_contract(text);
        self.drive(
            &contract,
            provider,
            store,
            policy,
            approver,
            &Ignore,
            Some(containment),
            TurnExtras::default(),
        )
        .await
    }

    /// [`turn_contained`](Session::turn_contained), reporting to `observer` as the
    /// fan-out happens.
    ///
    /// A tree is where an observer stops being a nicety: children run at once and
    /// their output interleaves, so `depth` and `run_id` are what turn the stream
    /// back into something readable. `EventKind::Spawned`, `SpawnRefused`, `Fleet`
    /// and `SpendDraw` reach a session's observer here for the first time.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, Containment, EventKind, Flow, Observer, OpenRouter,
    ///                  Policy, RunEvent, Session, Store};
    ///
    /// struct Transcript;
    ///
    /// impl Observer for Transcript {
    ///     fn event(&self, event: &RunEvent) -> Flow {
    ///         let pad = "  ".repeat(event.depth as usize);
    ///         match &event.kind {
    ///             EventKind::Spawned { child_run_id, goal } => {
    ///                 println!("{pad}+ run {child_run_id}: {goal}");
    ///             }
    ///             EventKind::Fleet { tier, working, queued, done } => {
    ///                 println!("{pad}  tier {tier}: {working} working, {queued} queued, {done} done");
    ///             }
    ///             _ => {}
    ///         }
    ///         Flow::Continue
    ///     }
    /// }
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    /// let turn = session
    ///     .turn_contained_observed(
    ///         "review every file under src/, one sub-agent per file",
    ///         &OpenRouter::from_env()?, store, policy, &ApproveAll,
    ///         &Containment::new(12, 4, 2, 500_000), &Transcript,
    ///     )
    ///     .await?;
    /// println!("{:?}", turn.outcome);
    /// # Ok(()) }
    /// ```
    ///
    /// Returning [`Flow::Cancel`](crate::Flow::Cancel) from the observer stops the
    /// whole turn at the next step boundary — one flag for the tree, honoured at
    /// the point where no child is in flight.
    #[allow(clippy::too_many_arguments)]
    pub async fn turn_contained_observed<P: Provider>(
        &mut self,
        text: impl Into<String>,
        provider: &P,
        store: &Store,
        policy: &Policy,
        approver: &dyn Approver,
        containment: &Containment,
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
            Some(containment),
            TurnExtras {
                stream: true,
                ..Default::default()
            },
        )
        .await
    }

    /// The whole conversation as one readable artifact (0.43.0).
    ///
    /// A pure read: no provider is called, no row is written, and a session whose
    /// runs have all finished can be exported forever without costing anything.
    ///
    /// **The whole tree, not the path.** [`Session::history`] returns the turns
    /// the model currently sees, which is what a next turn needs and is
    /// deliberately not what an export needs: a [`branch_from`](Session::branch_from)
    /// leaves earlier turns off the path, and those are exactly the ones no other
    /// surface will show you. Every turn of the session is here, oldest first, with
    /// [`TranscriptTurn::on_path`] marking which ones the model can still see.
    ///
    /// It is also the other half of compaction. A fold replaces the older half of
    /// a run's observations with a paragraph, which is acceptable precisely because
    /// the observations stay on disk — and this is how they come back out: each
    /// turn carries the [`Summary`](crate::Summary) rows its run wrote, so a reader can see where a
    /// paragraph stands in for the steps behind it.
    ///
    /// ```no_run
    /// use io_harness::{Session, Store};
    ///
    /// # fn demo(store: &Store, session: &Session) -> io_harness::Result<()> {
    /// let transcript = session.transcript(store)?;
    /// println!("{}", transcript.to_markdown());
    /// # Ok(()) }
    /// ```
    pub fn transcript(&self, store: &Store) -> Result<Transcript> {
        let on_path: std::collections::HashSet<i64> =
            self.history(store)?.iter().map(|t| t.id).collect();
        let mut turns = Vec::new();
        for turn in store.session_turns(self.id)? {
            turns.push(TranscriptTurn {
                turn_id: turn.id,
                parent_turn_id: turn.parent_turn_id,
                run_id: turn.run_id,
                on_path: on_path.contains(&turn.id),
                prompt: turn.prompt,
                reply: turn.reply,
                outcome: turn.outcome,
                created_at: turn.created_at,
                summaries: store.summaries(turn.run_id)?,
            });
        }
        Ok(Transcript {
            session_id: self.id,
            root: self.root.clone(),
            turns,
        })
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
        // 0.39.0 — `Some` for a contained turn, which is the only shape that
        // reaches the loop owning the spawn tool. `None` for the five turn
        // entry points that predate it, which therefore drive exactly the loop
        // they always drove.
        containment: Option<&Containment>,
        mut extras: TurnExtras<'_>,
    ) -> Result<TurnResult> {
        // 0.43.0 — the staged images ride this turn and only this turn. Appended
        // to whatever the contract carries, so a `turn_bounded` caller's own
        // `with_images` are kept; taken rather than copied, so the staging is clear
        // however the turn ends.
        #[cfg(feature = "media")]
        let contract = &{
            let mut owned = contract.clone();
            owned.images.extend(std::mem::take(&mut self.staged));
            owned
        };
        let seed = self.seed(store, contract)?;
        extras.seed = &seed;

        // 0.37.0 — may this turn's first completion decide that the turn was
        // conversation? Only when the caller declared no criterion. A caller who
        // said how the turn is judged has said it is work, and handing back an
        // answer instead of running the gate would be answering a different
        // question. An unbounded turn carries `Verification::None` by
        // construction, so `turn` and `turn_observed` always classify and
        // `turn_bounded` classifies exactly when its contract has no gate.
        //
        // One place, not five: every entry point reaches the loop through here, so
        // the rule cannot hold at four of them and lapse at the fifth.
        extras.classify = matches!(contract.verify, Verification::None);

        // The run row and the turn row before the first completion is billed, so a
        // turn whose process dies mid-answer is in the tree with a run id a resume
        // can continue from — the same order `run_with_observed` starts a run in.
        extras.turn = Some(SessionTurn {
            session_id: self.id,
            parent_turn_id: self.head,
            prompt: &contract.goal,
        });
        // The one branch, and it decides which loop answers the turn — not what a
        // turn is. Everything below this call is the same for both: the turn row
        // is read back from the run, closed once, and the head moves once.
        let result = match containment {
            Some(containment) => {
                crate::run::run_tree_with_extras(
                    contract,
                    provider,
                    store,
                    policy,
                    approver,
                    containment,
                    observer,
                    &extras,
                )
                .await?
            }
            None => {
                crate::run::run_with_extras(
                    contract, provider, store, policy, approver, observer, &extras,
                )
                .await?
            }
        };

        let turn_id = store
            .turn_for_run(result.run_id)?
            .ok_or_else(|| Error::Config(format!("run {} recorded no turn", result.run_id)))?;
        let reply = last_message(store, result.run_id)?;
        let outcome = store
            .run_summary(result.run_id)?
            .map(|s| s.outcome)
            .unwrap_or_else(|| "running".into());
        store.finish_turn(turn_id, reply.as_deref(), &outcome)?;
        // Compare-and-swap against the head this turn was taken on (0.62.0). Two
        // processes taking a turn on one session used to both write their own turn
        // id and the second won outright, leaving the first process's turn in
        // `session_turns` with its parent intact but off the head path — answered,
        // billed, and invisible to the next turn.
        //
        // The turn row is deliberately left exactly as it is when this refuses.
        // This release makes a dropped turn *reported*, not landed: the answer was
        // produced and paid for, and deleting it would destroy the one copy of what
        // the model said. The local head is not advanced either, so this handle
        // does not go on believing it owns a head the store gave to somebody else.
        store.set_session_head_if(self.id, self.head, Some(turn_id))?;
        self.head = Some(turn_id);

        // What the loop decided, read from the row it wrote rather than re-derived
        // here from a step count — the "built from what the run recorded, not from
        // a second guess" rule 0.32.0 and 0.36.0 both paid for. A run that is not a
        // classifying turn has no kind recorded and is a run, which is what it has
        // always been.
        let kind = match store.turn_kind(result.run_id)?.as_deref() {
            Some(crate::run::TURN_KIND_REPLY) => TurnKind::Reply,
            _ => TurnKind::Run,
        };
        // Emitted here rather than in the loop because this is the first place that
        // knows the turn's id: the loop is told a session's bookkeeping, not asked
        // to read it back. An attached process, a hook and a transcript can now
        // tell an answer from a run without opening the store.
        //
        // Only for a turn that was actually answered. A turn refused at the token
        // ceiling before its answer was served is a `Reply` that said nothing, and
        // announcing it as an answer would be reporting a message nobody wrote.
        if kind == TurnKind::Reply && matches!(result.outcome, RunOutcome::Finished { .. }) {
            observer.event(&crate::observe::RunEvent::new(
                result.run_id,
                0,
                // The run is the envelope's own id. A second copy in the kind is a
                // duplicate key once the kind is flattened onto the wire, which is
                // the constraint `Rewound` records.
                crate::observe::EventKind::Answered { turn_id },
            ));
        }

        Ok(TurnResult {
            turn_id,
            run_id: result.run_id,
            outcome: result.outcome,
            reply,
            kind,
        })
    }

    /// The conversation, rendered as the observations the next turn starts from.
    ///
    /// One entry per prior turn on the path, each bounded by the same per-entry cap
    /// the loop bounds a tool result by, so a long conversation is compacted by the
    /// assembler rather than by a rule of this module's own.
    ///
    /// **(0.49.0) Each entry says who was speaking**, and the run loop turns that
    /// into a real user or assistant message. Through 0.48.0 they were narration —
    /// "the operator asked: …" and "you answered: …" — inside the single user
    /// message the request could carry, which told the model about its own past
    /// turn in the third person. The attribution moved from the prose to the role,
    /// which is where the model was trained to read it.
    fn seed(&self, store: &Store, contract: &TaskContract) -> Result<Vec<(&'static str, String)>> {
        let cap = entry_cap_chars(contract.context.effective_tokens(contract.max_tokens));
        let mut out = Vec::new();
        for turn in self.history(store)? {
            out.push((
                crate::context::SEED_OPERATOR,
                bound(
                    &format!("\n[operator] {}\n", turn.prompt),
                    cap,
                    ObsKind::Message,
                ),
            ));
            if let Some(reply) = turn.reply.as_deref().filter(|r| !r.is_empty()) {
                out.push((
                    crate::context::SEED_AGENT,
                    bound(&format!("\n[agent] {reply}\n"), cap, ObsKind::Message),
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
#[non_exhaustive]
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
    /// Whether this turn was answered or run (0.37.0).
    ///
    /// Branch on this rather than inferring it from a step count: a run that was
    /// refused at its first step also has no steps, and the two are not the same
    /// thing at all.
    ///
    /// ```no_run
    /// use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store, TurnKind};
    ///
    /// # async fn demo(store: &Store, policy: &Policy) -> io_harness::Result<()> {
    /// let mut session = Session::open(store, "/repo")?;
    /// let turn = session.turn("hi", &OpenRouter::from_env()?, store, policy, &ApproveAll).await?;
    ///
    /// match turn.kind {
    ///     // Nothing was staged: no step, no gate, no checkpoint. Print it and wait
    ///     // for the next thing the operator says.
    ///     TurnKind::Reply => println!("{}", turn.reply.unwrap_or_default()),
    ///     // A run happened, and everything the crate offers about a run applies.
    ///     _ => println!("{:?} after a real run", turn.outcome),
    /// }
    /// # Ok(()) }
    /// ```
    pub kind: TurnKind,
}

/// What a turn turned out to be: conversation, or work (0.37.0).
///
/// Decided by the turn's own first completion, at no extra cost — the completion
/// the loop was going to make anyway is read rather than assumed. A completion
/// that stops on text is a [`Reply`](TurnKind::Reply); one carrying a tool call is
/// a [`Run`](TurnKind::Run), and the loop continues from that same completion, so
/// neither shape pays for a second call.
///
/// There is no list of greetings behind this, in this crate or in the program
/// embedding it. A list is a list in one language, matches `hi` and not `namaste`,
/// and answers `hi, the login page is broken` correctly only by accident.
///
/// ```
/// use io_harness::TurnKind;
///
/// // Two states and one meaning each: did this turn stage work, or answer?
/// assert_ne!(TurnKind::Reply, TurnKind::Run);
///
/// // `#[non_exhaustive]`, so match with a wildcard — a later release may record
/// // *why* a turn was classified as it was without breaking this.
/// let describe = |kind: TurnKind| match kind {
///     TurnKind::Reply => "answered",
///     _ => "ran",
/// };
/// assert_eq!(describe(TurnKind::Reply), "answered");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnKind {
    /// The turn was answered. One completion was made, billed and recorded, and
    /// nothing was staged: no step, no gate attempt, no checkpoint, no snapshot,
    /// no plan gate and no call to the [`Approver`].
    ///
    /// Also what a turn reports when its one completion crossed the token ceiling
    /// and was refused rather than served: no run was opened either way, and
    /// `outcome` is what says how it ended.
    Reply,
    /// The turn was work: the first completion reached for a tool and the loop ran
    /// from there. Everything the crate offers about a run applies, unchanged.
    Run,
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

/// A whole conversation, read back (0.43.0).
///
/// Built by [`Session::transcript`], which is a read and never a provider call.
/// The turns are every turn of the session, oldest first — including the ones a
/// [`branch_from`](Session::branch_from) took off the path, which no other surface
/// will show you.
///
/// ```no_run
/// use io_harness::{Session, Store};
///
/// # fn demo(store: &Store, session: &Session) -> io_harness::Result<()> {
/// let transcript = session.transcript(store)?;
///
/// // Every turn of the conversation, including the ones a branch left behind.
/// for turn in &transcript.turns {
///     let seen = if turn.on_path { "" } else { " (branched away from)" };
///     println!("{}{seen}: {}", turn.turn_id, turn.prompt);
/// }
///
/// std::fs::write("session.md", transcript.to_markdown())?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Transcript {
    /// The session this is.
    pub session_id: i64,
    /// The workspace it was held over.
    pub root: PathBuf,
    /// Every turn, oldest first.
    pub turns: Vec<TranscriptTurn>,
}

impl Transcript {
    /// Render it as Markdown.
    ///
    /// One `String`, and one method. A library does not choose the caller's file,
    /// its encoding or its pagination; what it owes is the text.
    ///
    /// Each turn is a section: what was asked, what was answered, and — where the
    /// run folded — a marked line saying what a summary stands in for, so the part
    /// compaction took out of the model's context is visible rather than merely
    /// still in the database. A turn that is off the current path says so, because
    /// a reader comparing a transcript against what the model seems to know needs
    /// to know which turns it can still see.
    pub fn to_markdown(&self) -> String {
        let mut out = format!(
            "# Session {}\n\n`{}`\n",
            self.session_id,
            self.root.display()
        );
        if self.turns.is_empty() {
            out.push_str("\n_No turns._\n");
            return out;
        }
        for turn in &self.turns {
            out.push_str(&format!("\n## Turn {}", turn.turn_id));
            if !turn.on_path {
                out.push_str(" — branched away from");
            }
            out.push('\n');
            out.push_str(&format!("\n> {}\n", turn.prompt.replace('\n', "\n> ")));
            match turn.reply.as_deref().filter(|r| !r.is_empty()) {
                Some(reply) => out.push_str(&format!("\n{reply}\n")),
                None => out.push_str("\n_No reply._\n"),
            }
            for summary in &turn.summaries {
                out.push_str(&format!(
                    "\n_At step {}, {} earlier observations were summarised as:_ {}\n",
                    summary.through_step, summary.folded, summary.text
                ));
            }
            if let Some(outcome) = &turn.outcome {
                out.push_str(&format!("\n_({outcome}, run {})_\n", turn.run_id));
            }
        }
        out
    }
}

/// One turn in a [`Transcript`].
///
/// ```no_run
/// use io_harness::{Session, Store, TranscriptTurn};
///
/// /// What a turn cost, and what it folded away to stay affordable.
/// fn describe(turn: &TranscriptTurn) -> String {
///     format!(
///         "turn {} ({} folds) — {}",
///         turn.turn_id,
///         turn.summaries.len(),
///         turn.outcome.as_deref().unwrap_or("still running")
///     )
/// }
///
/// # fn demo(store: &Store, session: &Session) -> io_harness::Result<()> {
/// for turn in &session.transcript(store)?.turns {
///     println!("{}", describe(turn));
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TranscriptTurn {
    /// The turn's own id.
    pub turn_id: i64,
    /// The turn it answers from, or `None` for the root of a conversation.
    pub parent_turn_id: Option<i64>,
    /// The run that served it.
    pub run_id: i64,
    /// Whether the model can still see this turn — that is, whether it is on the
    /// path [`Session::history`] returns. `false` for a turn a branch left behind.
    pub on_path: bool,
    /// What the operator said.
    pub prompt: String,
    /// What the agent said back, where it said anything.
    pub reply: Option<String>,
    /// Why the turn stopped, as the run's outcome string.
    pub outcome: Option<String>,
    /// UTC creation time.
    pub created_at: String,
    /// The folds this turn's run wrote, oldest first. Empty for a turn that never
    /// compacted, which is most of them.
    pub summaries: Vec<crate::state::Summary>,
}
