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
//! v0.5 adds agent composition. [`run_tree`] runs a workspace contract as the
//! root of a tree: the agent gains one tool, [`SPAWN_TOOL`], that launches a
//! contained sub-agent over the same workspace, and its result composes back for
//! the parent's next turn. Children may nest, and many may run at once — the
//! fan-out is bounded by [`Containment::max_concurrent`]. Containment is the
//! safety half: a child inherits its parent's policy and can only *narrow* it
//! ([`Policy::contain`] — allows intersect, denies union, downward at any depth),
//! and the whole tree draws its token spend from one shared [`Ledger`] no spawned
//! [`TaskContract`] can raise, capped by a [`Containment`] handed in at the root.
//! Every spawn, refusal, and budget draw lands in the rusqlite trace, so the tree
//! is a reconstructable graph. Sub-agents are opt-in: [`run`] and [`run_with`]
//! never expose the spawn tool.
//!
//! v0.6 adds the execution [`sandbox`]. Every command the verification gate runs
//! — the `rustc` compile and the test binary it has run since v0.2 — now executes
//! inside an ephemeral [`Sandbox`]: an isolated workdir, resource caps that
//! *kill* rather than throttle ([`SandboxLimits`]), outbound network denied by
//! default, and guaranteed teardown. It is **OS-native and OS-neutral**: one
//! trait with a native backend per platform (macOS `sandbox-exec`, Linux
//! namespaces, Windows Job Objects) over a portable floor that runs everywhere,
//! chosen by [`select`] and recorded in the trace. Sandboxing is the new default
//! and is transparent to verification; a caller who wants the exact v0.5 direct
//! execution opts it off. A configurable network egress allow-list is deferred to
//! v0.8; v0.6 is deny-by-default only.
//!
//! v0.7 makes a run **durable and unattended**. After every completed step the
//! harness commits that step's trace, its budget draw, and a checkpoint marker in
//! one rusqlite transaction, so the committed checkpoint *is* the step's
//! completion marker: a crash leaves either a whole step or none of it. On a
//! restart [`resume`] (single/workspace) and [`resume_tree`] (a whole v0.5 tree)
//! reconstruct the run from the store and continue every agent from its own last
//! committed step — completed steps are skipped, the aggregate [`Ledger`] budget
//! is restored from durable totals (never reset or double-charged), and the time
//! budget counts real wall-clock elapsed across the downtime ([`RunStatus`] and
//! [`Store::run_status`] report where a run stands). Replay is idempotent: an
//! irreversible edit already applied is re-observed, not repeated, and re-running
//! a resume is a no-op. Ephemeral v0.6 sandboxes are never checkpointed — an exec
//! in flight at crash time simply re-runs in a fresh sandbox. A v0.4 approval
//! survives a full process exit and resumes the tree via
//! [`resume_tree_with_decision`]. A resume against a newer-format or missing
//! checkpoint is a typed [`Error::Resume`], never a panic or a half-resume.
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
pub mod containment;
mod contract;
mod error;
pub mod policy;
pub mod provider;
mod run;
pub mod sandbox;
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
pub use containment::{Containment, Draw, Ledger, SpawnRefusal};
pub use approve::{ApproveAll, Approver, Decision, DenyAll, Request, StdinApprover};
pub use run::{
    resume, resume_tree, resume_tree_with_decision, resume_with_decision, run, run_tree, run_with,
    RunOutcome, RunResult, SPAWN_TOOL,
};
pub use state::{
    CheckpointEvent, Pending, PolicyEvent, RunStatus, SandboxEvent, StepRecord, Store,
    CHECKPOINT_FORMAT,
};
pub use sandbox::{
    copy_back, select, Backend, Cap, Sandbox, SandboxConfig, SandboxLimits, SandboxOutcome, Selected,
};
pub use verify::{ExecGuard, Verification, TEST_BINARY};
