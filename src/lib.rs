//! # io-harness
//!
//! A production-grade Rust agent harness: run an AI agent from a typed
//! [`TaskContract`] to a *verified* result. Provider-agnostic, embeddable
//! in-process, with a deterministic verification layer.
//!
//! The agent edits one file to meet a [`Verification`] criterion, using the
//! filesystem tool and the OpenRouter provider, persisting every step to
//! rusqlite. v0.2 bounds the run with step, time, and cost (token) budgets,
//! retries transient step failures, records a full trace, adds execution-based
//! verification ([`Verification::CompilesRust`], [`Verification::RustTestPasses`])
//! that compiles the produced file so a substring stub cannot pass, and can
//! [`resume`] an interrupted run.
//!
//! v0.4 adds the permission boundary: a [`Policy`] of layered allow/deny rules
//! decides what the agent may read, write, and execute, enforced in the tool and
//! verification layers rather than the prompt. Anything the policy marks
//! [`Effect::Ask`] goes to an [`Approver`], which may approve (optionally
//! rewriting the action or remembering a rule), deny, or
//! [`Defer`](Decision::Defer) — persisting the pending action so a human can
//! decide after this process has exited, and [`resume_with_decision`] continues
//! it. Every refusal and decision lands in the rusqlite trace, attributed to the
//! rule and layer that produced it.
//!
//! A caller who passes no policy gets [`Policy::permissive`] and the exact 0.3.0
//! behaviour — the boundary is opt-in.
//!
//! v0.3 adds repository work: [`TaskContract::workspace`] runs a multi-tool loop
//! where the agent greps, finds, reads, and writes several files under one root,
//! verified together ([`Verification::WorkspaceTestPasses`]). It also adds the
//! [`Anthropic`] and [`OpenAi`] providers behind the same [`Provider`] trait —
//! choose one at run construction; the task contract does not change.
//!
//! ```no_run
//! use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};
//!
//! # async fn demo() -> io_harness::Result<()> {
//! let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
//! let store = Store::memory()?;
//! let contract = TaskContract::new(
//!     "add a hello function returning 42",
//!     "src/hello.rs",
//!     Verification::FileContains("fn hello".into()),
//! );
//! // src/ is writable; secrets/ is denied outright and never reaches the approver.
//! let policy = Policy::default()
//!     .layer("app")
//!     .allow_read("*")
//!     .deny_read("secrets/*")
//!     .deny_write("secrets/*");
//! let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
//! println!("{:?}", result.outcome);
//! # Ok(())
//! # }
//! ```

pub mod approve;
mod contract;
mod error;
pub mod policy;
pub mod provider;
mod run;
mod state;
pub mod tools;
mod verify;

pub use contract::TaskContract;
pub use error::{Error, Result};
pub use provider::{
    Anthropic, CompletionRequest, CompletionResponse, OpenAi, OpenRouter, Provider, ToolCall,
    ToolSpec, Usage,
};
pub use policy::{Act, Effect, Policy, Rule, Verdict};
pub use approve::{ApproveAll, Approver, Decision, DenyAll, Request, StdinApprover};
pub use run::{resume, resume_with_decision, run, run_with, RunOutcome, RunResult};
pub use state::{Pending, PolicyEvent, StepRecord, Store};
pub use verify::{ExecGuard, Verification, TEST_BINARY};
