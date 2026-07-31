//! The human-approval gate: one trait, three decisions.
//!
//! [`Policy`](crate::policy::Policy) decides what is refused outright. What it
//! marks [`Ask`](crate::policy::Effect::Ask) reaches an [`Approver`], which the
//! loop awaits before performing the action. A caller may hold that future open
//! indefinitely — the run stays paused, it does not time out — and
//! [`Decision::Defer`] is the escape hatch for a decision that must outlive the
//! process.
//!
//! An action the policy *denies* never reaches the approver at all. Refusal and
//! approval are different things: only the sensitive-but-permitted tier prompts.

use std::fmt;
use std::future::Future;
use std::pin::Pin;

use crate::policy::{Act, Rule};

/// The action a human is being asked to approve.
///
/// This is everything an [`Approver`] gets to decide on, and it is deliberately
/// enough to render a prompt without going back to the store: the act, what it
/// targets, and — for a write — the bytes that would land there, so a UI can
/// show the change before anyone says yes.
///
/// ```
/// use io_harness::{Act, Request};
///
/// // What the harness builds for a pending write.
/// let request = Request::new(Act::Write, "src/lib.rs")
///     .with_content("pub fn hello() -> u32 { 42 }");
///
/// let prompt = match (request.act, request.content.as_deref()) {
///     (Act::Write, Some(body)) => format!("write {} ({} bytes)", request.target, body.len()),
///     // Reads, execs and net checks carry no payload — the target is the whole
///     // question, and for `Act::Net` it is a host, normally `host:port`.
///     (act, _) => format!("{act:?} {}", request.target),
/// };
/// assert_eq!(prompt, "write src/lib.rs (28 bytes)");
/// ```
///
/// A `Request` also travels in the other direction: hand a modified one back in
/// [`Decision::Approve`] to perform something other than what was asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// What kind of action it is.
    pub act: Act,
    /// The path, or the binary name for [`Act::Exec`].
    pub target: String,
    /// The content a write would produce, so an approver can show it.
    pub content: Option<String>,
}

impl Request {
    /// A request to perform `act` on `target`.
    pub fn new(act: Act, target: impl Into<String>) -> Self {
        Self {
            act,
            target: target.into(),
            content: None,
        }
    }

    /// Attach the write payload.
    pub fn with_content(mut self, content: impl Into<String>) -> Self {
        self.content = Some(content.into());
        self
    }
}

/// What an approver decided.
///
/// The three answers are not "yes / no / later" — they are three different
/// shapes, and the third is the one worth knowing about. [`Decision::Defer`]
/// persists the pending action and stops the run, so the process may exit
/// entirely and a human decide tomorrow:
///
/// ```
/// use io_harness::approve::DecisionFuture;
/// use io_harness::{Act, Approver, Decision, Effect, Request, Rule};
///
/// struct Reviewer;
///
/// impl Approver for Reviewer {
///     fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
///         Box::pin(async move {
///             match request.target.as_str() {
///                 // Say yes once and stop being asked. `remember` installs a
///                 // run-scoped top layer, and it is handed back on `RunResult`
///                 // so the caller decides whether it outlives the run. A
///                 // remembered allow still cannot defeat a deny beneath it.
///                 t if t.ends_with(".rs") => Decision::Approve {
///                     modified: None,
///                     remember: vec![Rule {
///                         act: Act::Write,
///                         effect: Effect::Allow,
///                         pattern: "*.rs".into(),
///                     }],
///                 },
///                 // The reason reaches the model as an observation, so it adapts
///                 // rather than retrying the same refused write to the step cap.
///                 t if t.ends_with(".lock") => {
///                     Decision::deny("lockfiles are generated, not hand-edited")
///                 }
///                 // Nobody is here to answer. The run stops with
///                 // `RunOutcome::AwaitingApproval { request_id, .. }` and the
///                 // action is persisted; this process may now exit.
///                 _ => Decision::Defer,
///             }
///         })
///     }
/// }
/// ```
///
/// The deferred half, in whatever process picks the decision up later — the run
/// id and the request id are all that need to survive in between:
///
/// ```no_run
/// use io_harness::{resume_with_decision, ApproveAll, Decision, OpenRouter, Policy,
///                  RunOutcome, Store, TaskContract, Verification};
///
/// # async fn later(contract: &TaskContract, policy: &Policy, outcome: RunOutcome)
/// #     -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// if let RunOutcome::AwaitingApproval { request_id, .. } = outcome {
///     // Show the human what was actually pending, read back from the store.
///     let pending = store.pending(request_id)?.expect("a pending request");
///     println!("{} {}", pending.act, pending.target);
///
///     // Approving performs exactly the persisted action — the same target and
///     // the same bytes the human was shown — then continues the run under its
///     // original id. It is re-checked against the policy first, so a deny added
///     // while the run was paused still holds.
///     resume_with_decision(
///         contract, &OpenRouter::from_env()?, &store, pending.run_id, request_id,
///         Decision::approve(), policy, &ApproveAll,
///     )
///     .await?;
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Perform the action, optionally in a modified form, optionally
    /// remembering rules so matching later actions stop asking.
    Approve {
        /// A rewritten action to perform instead of the requested one. It is
        /// re-evaluated against the policy before it runs, so an approver
        /// cannot rewrite an action across a deny.
        modified: Option<Request>,
        /// Rules to apply as a run-scoped top layer. A remembered allow still
        /// cannot defeat a deny beneath it.
        remember: Vec<Rule>,
    },
    /// Do not perform the action. The reason reaches the model.
    Deny {
        /// Why, surfaced to the model so it can adapt.
        reason: String,
    },
    /// Stop the run and persist the pending action for a decision later.
    Defer,
}

impl Decision {
    /// Approve the action exactly as requested.
    pub fn approve() -> Self {
        Decision::Approve {
            modified: None,
            remember: Vec::new(),
        }
    }

    /// Deny with a reason the model will see.
    pub fn deny(reason: impl Into<String>) -> Self {
        Decision::Deny {
            reason: reason.into(),
        }
    }
}

/// A boxed decision future. Boxed rather than `async fn` in the trait so that
/// [`Approver`] stays object-safe — io-studio holds a `Box<dyn Approver>`
/// backed by a UI channel while io-cli uses [`StdinApprover`].
pub type DecisionFuture<'a> = Pin<Box<dyn Future<Output = Decision> + Send + 'a>>;

/// Decides whether a sensitive action may proceed.
///
/// Implementations may take as long as they like; the run stays paused until
/// the future resolves.
///
/// Implement it when neither "approve everything the policy permits" nor "refuse
/// everything it asks about" is the right answer — which is most unattended
/// runs. The gate is a function of the request, so the rule that would have been
/// too fiddly to write in the policy can be written here instead:
///
/// ```
/// use io_harness::approve::DecisionFuture;
/// use io_harness::{Act, Approver, Decision, Request};
///
/// /// Unattended, but not blanket-deny: generated output is writable, anything
/// /// else the policy routed here is refused with a reason the model can act on.
/// struct GeneratedOnly;
///
/// impl Approver for GeneratedOnly {
///     fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
///         Box::pin(async move {
///             match request.act {
///                 Act::Write if request.target.starts_with("build/") => Decision::approve(),
///                 // Rewriting the action is allowed, and is re-checked against
///                 // the policy before it runs — an approver cannot rewrite
///                 // something across a deny.
///                 Act::Write => Decision::Approve {
///                     modified: Some(
///                         Request::new(Act::Write, format!("build/{}", request.target))
///                             .with_content(request.content.clone().unwrap_or_default()),
///                     ),
///                     remember: Vec::new(),
///                 },
///                 _ => Decision::deny("this run only writes, and only under build/"),
///             }
///         })
///     }
/// }
/// ```
///
/// `&self` rather than `&mut self`, and `Send + Sync`, because one approver
/// serves a whole [`run_tree`](crate::run_tree) — every agent in the tree asks
/// the same one. State it needs goes behind a `Mutex` or a channel, as
/// io-studio's does.
pub trait Approver: Send + Sync {
    /// Decide on one request.
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a>;
}

/// Approves everything. For tests and for callers who want the policy's denies
/// but no interactive gate.
///
/// "Approves everything" is narrower than it sounds, and the distinction is the
/// reason to reach for it: a denied action never reaches an approver, so the
/// policy's boundary is fully enforced around an `ApproveAll`. What it removes is
/// only the [`Ask`](crate::Effect::Ask) tier — the prompt, not the wall.
///
/// ```no_run
/// use io_harness::{run_with, Act, ApproveAll, Effect, OpenRouter, Policy, Store,
///                  TaskContract, Verification};
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// // Unattended, and still bounded. `Policy::default()` asks about every write;
/// // `ApproveAll` answers yes to those, and cannot answer anything about the
/// // denies below — they were never a question.
/// let policy = Policy::default()
///     .layer("app")
///     .allow_write("src/*")
///     .deny_write("src/main.rs");
/// assert_eq!(policy.check(Act::Write, "src/main.rs").effect, Effect::Deny);
///
/// run_with(contract, &OpenRouter::from_env()?, &Store::memory()?, &policy, &ApproveAll).await?;
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproveAll;

impl Approver for ApproveAll {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::approve() })
    }
}

/// Denies everything the policy routed to approval. The safe default for an
/// unattended run that must never take a sensitive action.
///
/// It is the honest pairing for a scheduled job: every action the run may take
/// has to be named in the policy up front, and anything the operator left in the
/// grey [`Ask`](crate::Effect::Ask) tier is refused rather than waved through by
/// a machine standing in for a human who is not there.
///
/// ```no_run
/// use io_harness::{run_with, DenyAll, OpenRouter, Policy, Store, TaskContract};
///
/// # async fn nightly(contract: &TaskContract) -> io_harness::Result<()> {
/// // Explicit allows only. `Policy::default()`'s write tier is `Ask`, so any
/// // write outside `reports/` reaches `DenyAll` and is refused — with a reason
/// // the model reads, so it retargets rather than failing the run.
/// let policy = Policy::default().layer("nightly").allow_write("reports/*");
/// run_with(contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, &policy, &DenyAll)
///     .await?;
/// # Ok(()) }
/// ```
///
/// Use [`Decision::Defer`] instead when the action should still be *possible*
/// later: `DenyAll` closes the question, deferring parks it.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::deny("no approver available") })
    }
}

/// Prompts on the terminal and reads a decision from stdin, so a CLI gets an
/// approval flow without writing one. Anything other than `y` is a denial.
///
/// Pair it with a policy whose grey tier is where you actually want a human
/// looking — `Policy::default()` asks about every write, which is the right
/// setting for an interactive session and the wrong one for a batch job:
///
/// ```no_run
/// use io_harness::{run_with, OpenRouter, Policy, StdinApprover, Store, TaskContract};
///
/// # async fn interactive(contract: &TaskContract) -> io_harness::Result<()> {
/// // Reads are free, writes stop at the terminal, and the secret paths
/// // `Policy::default()` denies are never offered as a question at all.
/// let policy = Policy::default().layer("cli").allow_read("*");
/// let result = run_with(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, &policy, &StdinApprover,
/// )
/// .await?;
/// println!("{:?}", result.outcome);
/// # Ok(()) }
/// ```
///
/// It blocks the run's task on a blocking stdin read, which is fine for a
/// foreground CLI and wrong for a server. Anything with a UI implements
/// [`Approver`] over its own channel instead — the run waits either way, for as
/// long as it takes.
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinApprover;

impl Approver for StdinApprover {
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async move {
            use std::io::Write;
            let mut out = std::io::stdout();
            let _ = write!(out, "\nallow {:?} {}? [y/N] ", request.act, request.target);
            let _ = out.flush();
            let mut line = String::new();
            match std::io::stdin().read_line(&mut line) {
                Ok(_) if line.trim().eq_ignore_ascii_case("y") => Decision::approve(),
                _ => Decision::deny("denied at the terminal"),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::oneshot;

    #[tokio::test]
    async fn built_in_approvers_decide_as_advertised() {
        let req = Request::new(Act::Write, "src/a.rs");
        assert_eq!(ApproveAll.decide(&req).await, Decision::approve());
        assert!(matches!(DenyAll.decide(&req).await, Decision::Deny { .. }));
    }

    /// io-studio's shape: an approver that is not a terminal, held behind a
    /// trait object, answering from a channel.
    struct ChannelApprover {
        answer: std::sync::Mutex<Option<oneshot::Receiver<Decision>>>,
    }

    impl Approver for ChannelApprover {
        fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
            let rx = self.answer.lock().unwrap().take();
            Box::pin(async move {
                match rx {
                    Some(rx) => rx.await.unwrap_or(Decision::deny("channel closed")),
                    None => Decision::deny("already decided"),
                }
            })
        }
    }

    #[tokio::test]
    async fn an_approver_can_be_held_as_a_trait_object_and_answer_out_of_band() {
        let (tx, rx) = oneshot::channel();
        let approver: Box<dyn Approver> = Box::new(ChannelApprover {
            answer: std::sync::Mutex::new(Some(rx)),
        });

        // The decision arrives from elsewhere, after the call is already awaiting.
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            let _ = tx.send(Decision::approve());
        });

        let req = Request::new(Act::Write, "src/a.rs");
        assert_eq!(approver.decide(&req).await, Decision::approve());
    }

    #[tokio::test]
    async fn a_slow_approver_keeps_the_caller_waiting_rather_than_timing_out() {
        struct Slow;
        impl Approver for Slow {
            fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
                Box::pin(async {
                    tokio::time::sleep(std::time::Duration::from_millis(60)).await;
                    Decision::approve()
                })
            }
        }
        let started = std::time::Instant::now();
        let d = Slow.decide(&Request::new(Act::Write, "x")).await;
        assert_eq!(d, Decision::approve());
        assert!(started.elapsed() >= std::time::Duration::from_millis(50));
    }
}

// ===========================================================================
// 0.21.0 — asking the operator what they MEANT, which is not asking permission
// ===========================================================================

/// A question the agent is asking the operator about **intent**.
///
/// The distinction from [`Request`] is the whole reason this type exists, and the
/// crate keeps it everywhere:
///
/// | | asks | an answer can |
/// |---|---|---|
/// | [`Request`] / [`Approver`] | may I do this action? | only *narrow* what happens |
/// | `Question` / [`Responder`] | what did you actually want? | only add *text the model reads* |
///
/// An answer authorizes nothing. Every tool call that follows one is checked against
/// the same [`Policy`](crate::Policy) by the same code — the rule 0.20.0 set for
/// steering, and for the same reason: "just do it" is the most natural thing anyone
/// will ever type, and the boundary must not care.
///
/// ```
/// use io_harness::Question;
///
/// let q = Question::new("Which config should I edit?")
///     .with_context("There is a committed io.toml and a gitignored io.local.toml.")
///     .with_choices(["io.toml", "io.local.toml"]);
///
/// assert_eq!(q.question, "Which config should I edit?");
/// assert_eq!(q.choices, ["io.toml", "io.local.toml"]);
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub struct Question {
    /// What the agent wants to know.
    pub question: String,
    /// What it already knows, so a human can answer without re-deriving the
    /// situation. `None` when the question stands on its own.
    pub context: Option<String>,
    /// Options the agent is offering, if any. A UI renders these as buttons; an
    /// answer is **not** obliged to be one of them, because an operator whose real
    /// answer is "neither, do this instead" must not be forced to pick a wrong one.
    pub choices: Vec<String>,
}

impl Question {
    /// A bare question.
    ///
    /// ```
    /// use io_harness::Question;
    ///
    /// let q = Question::new("Should the old column be dropped or kept?");
    /// assert!(q.context.is_none() && q.choices.is_empty());
    /// ```
    pub fn new(question: impl Into<String>) -> Self {
        Self {
            question: question.into(),
            ..Default::default()
        }
    }

    /// Add what the agent already established.
    ///
    /// ```
    /// use io_harness::Question;
    ///
    /// let q = Question::new("Which one?").with_context("Both exist.");
    /// assert_eq!(q.context.as_deref(), Some("Both exist."));
    /// ```
    pub fn with_context(mut self, context: impl Into<String>) -> Self {
        self.context = Some(context.into());
        self
    }

    /// Offer options.
    ///
    /// ```
    /// use io_harness::Question;
    ///
    /// let q = Question::new("Which?").with_choices(["a", "b"]);
    /// assert_eq!(q.choices.len(), 2);
    /// ```
    pub fn with_choices<I, S>(mut self, choices: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.choices = choices.into_iter().map(Into::into).collect();
        self
    }
}

/// The future a [`Responder`] returns. Boxed for the same reason
/// [`DecisionFuture`] is: object safety.
///
/// `Option<String>`, not `String`: `None` is "nobody here can answer", which is a real
/// answer and the one that makes the run pause for a human rather than guess.
///
/// ```
/// use io_harness::{AnswerFuture, Question, Responder};
///
/// #[derive(Debug)]
/// struct AlwaysDeclines;
///
/// impl Responder for AlwaysDeclines {
///     fn answer<'a>(&'a self, _question: &'a Question) -> AnswerFuture<'a> {
///         Box::pin(async { None })
///     }
/// }
///
/// let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// assert!(rt.block_on(AlwaysDeclines.answer(&Question::new("Which?"))).is_none());
/// ```
pub type AnswerFuture<'a> = Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>>;

/// Answers an agent's question about intent, in this process.
///
/// `None` means "nobody here can answer this", which is a real answer and not a
/// failure: the run then persists the question and pauses, so a human can answer it
/// after this process has exited — see
/// [`resume_with_answer`](crate::resume_with_answer). A run that could not ask would
/// have to guess, and a wrong guess spends a whole run.
///
/// `&self` and `Send + Sync`, exactly like [`Approver`]: one responder serves a whole
/// [`run_tree`](crate::run_tree), and state it needs goes behind a `Mutex` or a
/// channel.
///
/// Registered on the contract with
/// [`TaskContract::with_responder`](crate::TaskContract::with_responder) rather than
/// passed to every entry point, which is how a [`Toolbox`](crate::Toolbox) is carried
/// too. `Debug` is required for that reason and that reason only —
/// [`TaskContract`](crate::TaskContract) derives `Debug`, and a `#[derive(Debug)]` on
/// your responder is the whole obligation.
///
/// ```
/// use io_harness::{AnswerFuture, Question, Responder};
///
/// /// Always picks the first option offered, and declines an open question.
/// #[derive(Debug)]
/// struct FirstChoice;
///
/// impl Responder for FirstChoice {
///     fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
///         Box::pin(async move { question.choices.first().cloned() })
///     }
/// }
///
/// let responder = FirstChoice;
/// let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
///
/// let answered = rt.block_on(responder.answer(
///     &Question::new("Which?").with_choices(["left", "right"]),
/// ));
/// assert_eq!(answered.as_deref(), Some("left"));
///
/// // No options, so this responder has nothing to say and the run will pause.
/// assert!(rt.block_on(responder.answer(&Question::new("Why?"))).is_none());
/// ```
pub trait Responder: Send + Sync + fmt::Debug {
    /// Answer one question, or return `None` to let the run pause for a human.
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a>;
}

/// Answers nothing, so every question pauses the run for a human.
///
/// The default, and the honest one for an unattended run: a machine standing in for
/// an absent human is exactly what a question about intent must not have. The
/// question is persisted and the run is resumable, so nothing is lost by waiting.
///
/// ```
/// use io_harness::{Question, Responder, ResponderNone};
///
/// let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// let answered = rt.block_on(ResponderNone.answer(&Question::new("Which config?")));
/// assert!(answered.is_none(), "it declines, and the run pauses");
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct ResponderNone;

impl Responder for ResponderNone {
    fn answer<'a>(&'a self, _question: &'a Question) -> AnswerFuture<'a> {
        Box::pin(async { None })
    }
}

/// Answers every question with the same text. For tests.
///
/// ```
/// use io_harness::{FixedResponder, Question, Responder};
///
/// let responder = FixedResponder::new("the second one");
/// let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
/// let answered = rt.block_on(responder.answer(&Question::new("Which one?")));
/// assert_eq!(answered.as_deref(), Some("the second one"));
/// ```
#[derive(Debug, Clone, Default)]
pub struct FixedResponder {
    answer: String,
}

impl FixedResponder {
    /// A responder that always says `answer`.
    pub fn new(answer: impl Into<String>) -> Self {
        Self {
            answer: answer.into(),
        }
    }
}

impl Responder for FixedResponder {
    fn answer<'a>(&'a self, _question: &'a Question) -> AnswerFuture<'a> {
        Box::pin(async { Some(self.answer.clone()) })
    }
}

/// Prints the question on the terminal and reads one line back, so a CLI can hold a
/// conversation about intent without building an event loop.
///
/// An empty line means "I would rather not answer here", which returns `None` and
/// pauses the run — the same escape hatch [`StdinApprover`] gives.
///
/// Blocking stdin on the async runtime is acceptable for the same reason it is in
/// [`StdinApprover`]: a run that is waiting for a human has nothing else to do.
///
/// ```no_run
/// use io_harness::{StdinResponder, TaskContract};
/// use std::sync::Arc;
///
/// // A CLI that can hold a conversation about intent: the agent asks, the operator
/// // types, the run carries on. An empty line declines, and the run pauses instead —
/// // the question is durable, so nothing is lost by deciding later.
/// let contract = TaskContract::workspace("port the parser", "/repo")
///     .with_responder(Arc::new(StdinResponder));
/// # let _ = contract;
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct StdinResponder;

impl Responder for StdinResponder {
    fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
        Box::pin(async move {
            use std::io::Write;
            println!("\n[the agent is asking] {}", question.question);
            if let Some(context) = &question.context {
                println!("  context: {context}");
            }
            for (i, choice) in question.choices.iter().enumerate() {
                println!("  {}) {choice}", i + 1);
            }
            print!("your answer (empty to decide later): ");
            let _ = std::io::stdout().flush();
            let mut line = String::new();
            if std::io::stdin().read_line(&mut line).is_err() {
                return None;
            }
            let line = line.trim();
            if line.is_empty() {
                return None;
            }
            // A bare number picks an offered choice, which is what anyone types.
            if let Ok(n) = line.parse::<usize>() {
                if n >= 1 && n <= question.choices.len() {
                    return Some(question.choices[n - 1].clone());
                }
            }
            Some(line.to_string())
        })
    }
}
