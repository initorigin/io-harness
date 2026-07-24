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
pub trait Approver: Send + Sync {
    /// Decide on one request.
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a>;
}

/// Approves everything. For tests and for callers who want the policy's denies
/// but no interactive gate.
#[derive(Debug, Clone, Copy, Default)]
pub struct ApproveAll;

impl Approver for ApproveAll {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::approve() })
    }
}

/// Denies everything the policy routed to approval. The safe default for an
/// unattended run that must never take a sensitive action.
#[derive(Debug, Clone, Copy, Default)]
pub struct DenyAll;

impl Approver for DenyAll {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::deny("no approver available") })
    }
}

/// Prompts on the terminal and reads a decision from stdin, so a CLI gets an
/// approval flow without writing one. Anything other than `y` is a denial.
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
        assert!(matches!(
            DenyAll.decide(&req).await,
            Decision::Deny { .. }
        ));
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
