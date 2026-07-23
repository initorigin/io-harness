//! # io-harness
//!
//! A production-grade Rust agent harness: run an AI agent from a typed
//! [`TaskContract`] to a *verified* result. Provider-agnostic, embeddable
//! in-process, with a deterministic verification layer.
//!
//! v0.1 scope: the agent edits one file to meet a [`Verification`] criterion,
//! using the filesystem tool and the OpenRouter provider, persisting every step
//! to rusqlite, and stopping on success or a step cap.
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
pub use provider::{CompletionRequest, CompletionResponse, OpenRouter, Provider, ToolCall, ToolSpec};
pub use run::{run, RunOutcome, RunResult};
pub use state::{StepRecord, Store};
pub use verify::Verification;
