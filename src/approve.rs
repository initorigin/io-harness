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
