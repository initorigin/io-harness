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
//! v0.3 adds repository work: [`TaskContract::workspace`] runs a multi-tool loop
//! where the agent greps, finds, reads, and writes several files under one root,
//! verified together ([`Verification::WorkspaceTestPasses`]). It also adds the
//! [`Anthropic`] and [`OpenAi`] providers behind the same [`Provider`] trait —
//! choose one at run construction; the task contract does not change.
//!
//! ```no_run
//! use io_harness::{run, OpenRouter, Store, TaskContract, Verification};
//!
//! # async fn demo() -> io_harness::Result<()> {
//! let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
//! let store = Store::memory()?;
//! let contract = TaskContract::new(
//!     "add a hello function returning 42",
//!     "src/hello.rs",
//!     Verification::FileContains("fn hello".into()),
//! );
//! let result = run(&contract, &provider, &store).await?;
//! println!("{:?}", result.outcome);
//! # Ok(())
//! # }
//! ```

mod contract;
mod error;
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
pub use run::{resume, run, RunOutcome, RunResult};
pub use state::{StepRecord, Store};
pub use verify::Verification;
