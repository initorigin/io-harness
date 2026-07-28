//! Run an AI agent from a typed task contract to a verified result — in your own
//! process, under a permission boundary you control.
//!
//! You hand it a [`TaskContract`]: the task, the file or workspace it may touch,
//! and the [`Verification`] criterion that decides whether the work is done. The
//! harness runs the loop — observe, reason, act, verify, stop — and returns a
//! [`RunResult`] whose [`RunOutcome`] says a check actually passed, with every
//! step, refusal, and budget draw in a SQLite trace ([`Store`]) you can read
//! afterwards. Provider-agnostic, embeddable in-process, no daemon and no CLI.
//!
//! # Quickstart
//!
//! ```no_run
//! use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};
//!
//! #[tokio::main]
//! async fn main() -> io_harness::Result<()> {
//!     let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
//!     let store = Store::memory()?;
//!
//!     let contract = TaskContract::new(
//!         "add a hello function returning 42",
//!         "src/hello.rs",
//!         // Not a substring check: this compiles the file the agent produced.
//!         Verification::CompilesRust,
//!     );
//!
//!     // src/ is writable; secrets/ is refused outright and never reaches a human.
//!     let policy = Policy::default()
//!         .layer("app")
//!         .allow_read("*")
//!         .allow_write("src/*")
//!         .deny_read("secrets/*");
//!
//!     let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
//!     println!("{:?}", result.outcome);
//!     Ok(())
//! }
//! ```
//!
//! [`run`] is the same loop with no policy — it uses [`Policy::permissive`],
//! which enforces nothing. Every entry point has an observed twin taking an
//! [`Observer`] as its last argument ([`run_with_observed`], and so on).
//!
//! # What it does
//!
//! **The loop.** [`TaskContract::new`] names one file to edit;
//! [`TaskContract::workspace`] gives the agent `grep`, `find`, `read_file` and
//! `write_file` across a repository root, verified together.
//!
//! **Verification that executes.** A [`Verification`] criterion can compile the
//! produced file ([`Verification::CompilesRust`]) or run a test against it
//! ([`Verification::RustTestPasses`]), inside a sandbox, so a substring stub
//! cannot pass. What a passing gate proves is narrower than it reads, and
//! [`Verification`] states it exactly.
//!
//! **A permission boundary.** A [`Policy`] of layered, deny-first [`Rule`]s
//! decides what the agent may read, write, execute ([`Act::Exec`]) and connect
//! to ([`Act::Net`]), enforced in the tool and verification layers rather than
//! in the prompt. Anything marked [`Effect::Ask`] goes to an [`Approver`], which
//! may approve (optionally rewriting the action or remembering a rule), deny, or
//! [`Defer`](Decision::Defer) — persisting the pending action so a human can
//! decide after this process has exited, with [`resume_with_decision`] and
//! [`resume_tree_with_decision`] continuing on that answer. Every refusal is in
//! the trace, attributed to the rule and the layer that produced it.
//!
//! **Budgets and stop conditions.** Steps, wall-clock time, and token spend are
//! capped by the contract. A whole tree of agents draws from one shared
//! [`Ledger`] that no spawned contract can raise.
//!
//! **Agent composition.** [`run_tree`] runs a workspace contract as the root of
//! a tree: the agent gains [`SPAWN_TOOL`], which launches a contained sub-agent
//! over the same workspace, and the child's result composes back into the
//! parent's next turn. Children nest, and many run at once up to
//! [`Containment::max_concurrent`]. A child inherits its parent's policy and can
//! only narrow it ([`Policy::contain`]: allows intersect, denies union, at any
//! depth). [`run`] and [`run_with`] never expose the spawn tool.
//!
//! **An execution sandbox.** Commands the verification gate runs execute in an
//! ephemeral [`Sandbox`]: an isolated workdir, resource caps that *kill* rather
//! than throttle ([`SandboxLimits`]), outbound network denied by default, and
//! guaranteed teardown. One trait, a native [`Backend`] per platform over a
//! portable floor that runs everywhere, chosen by [`select`] and recorded in the
//! trace.
//!
//! **Durable, unattended runs.** After every completed step the trace, the
//! budget draw, and a checkpoint commit in one transaction, so a crash leaves
//! either a whole step or none of it. [`resume`] and [`resume_tree`] reconstruct
//! the run from the store and continue every agent from its own last committed
//! step: completed steps are not re-run, the [`Ledger`] is restored from durable
//! totals rather than reset or double-charged, and an irreversible edit already
//! applied is re-observed rather than repeated. [`resume_from_stored_policy`]
//! and [`resume_tree_from_stored_policy`] read the boundary the run was started
//! under back out of the store instead of trusting the caller to pass the same
//! one again — prefer them when the policy is what matters.
//! [`Store::run_status`] and [`RunStatus`] report where a run stands; a resume
//! against a missing or newer-format checkpoint is a typed [`Error::Resume`],
//! never a panic or a half-resume.
//!
//! **Providers, with fallback.** [`OpenRouter`], [`Anthropic`] and [`OpenAi`]
//! behind one [`Provider`] trait, over the crate's own HTTP+SSE client.
//! [`provider::Fallback`] moves to the next configured provider when one is down
//! or rate-limited, and failures are classified ([`ProviderErrorKind`]) so a
//! caller can tell a retryable transport error from a terminal one.
//! [`RetryPolicy`] governs the backoff and [`StallPolicy`] detects a run that is
//! repeating itself rather than progressing.
//!
//! **Extensibility, in-process and out.** Implement the object-safe [`Tool`]
//! trait for something the embedding program already does, collect them in a
//! [`Toolbox`], and register it with [`TaskContract::with_tools`] — no second
//! process, transport, or serialization hop. Or point the harness at MCP servers
//! with [`TaskContract::with_mcp`]: [`McpServer`]s spawned as child processes
//! ([`McpTransport::Stdio`]) or dialled over streamable HTTP
//! ([`McpTransport::Http`]), offered to the model under `mcp__<server>__<tool>`
//! so a server can never shadow a built-in. Either way registration makes a tool
//! *available*, not authorized: every call is an [`Act::Exec`] check on its
//! name. [`TaskContract::with_skills`] adds [`Skills`] — markdown instruction
//! files that shape how the agent approaches a class of task, loaded through
//! [`read_skill`](tools::READ_SKILL_TOOL) as an ordinary policy-checked read,
//! with no Rust at all.
//!
//! **Context that stays relevant.** Each turn is assembled to fit a stated share
//! of the token budget ([`ContextBudget`]): superseded observations are
//! compacted, and an observation a later write invalidated is re-read rather
//! than trusted. Durable memory keyed to the workspace ([`MemoryEntry`],
//! [`Store::memory_list`]) survives between runs.
//!
//! **Observation and replay.** Register an [`Observer`] and be called as the run
//! happens — [`RunEvent`]s covering steps, tool calls, approvals, refusals,
//! spend draws, retries, fallbacks and outcomes — instead of polling the store;
//! [`Flow`] lets an observer ask the run to stop. [`provider::Record`] captures
//! a case and [`provider::Replay`] runs it back identically.
//!
//! **Documents and images**, behind opt-in features: spreadsheets, Word,
//! PowerPoint text, PDF, and barcode decoding, each gated on [`Act::Read`] or
//! [`Act::Write`] against the real path the model named, and verified with
//! [`Verification::DocumentContains`] rather than a container read as the empty
//! string; plus image passthrough to any provider whose model accepts one.
//!
//! **Git**, as fixed-argv built-ins: status, diff, log, add, and commit (under a
//! caller-supplied [`Identity`]), so a run ends as a reviewable commit rather
//! than a working tree someone has to reconstruct. The model supplies paths and
//! a message, never a subcommand or a flag, so push, fetch, reset and rebase are
//! unreachable by construction.
//!
//! What none of it governs: a stdio MCP server and a registered [`Tool`] both
//! run outside the sandbox with the privileges of whoever started them. The
//! harness decides what may *start* and what may be *called* — not what a
//! started thing then does.
//!
//! # Feature flags
//!
//! `default = []`. The default build compiles no optional dependency at all.
//!
//! | Feature | What it adds |
//! | --- | --- |
//! | `media` | Image passthrough to providers that accept images |
//! | `documents` | Umbrella over the five below |
//! | `xlsx` | Spreadsheet read, generate, and preserving single-cell edit |
//! | `docx` | Word read and generate (no in-place edit, deliberately) |
//! | `pptx` | PowerPoint text extraction (read-only, no writer) |
//! | `pdf` | PDF generate, extract text, watermark, fill AcroForm fields |
//! | `barcode` | Barcode and QR decoding from an image |
//!
//! # Minimum supported Rust
//!
//! **MSRV: Rust 1.88.** The floor comes from `rmcp`, which publishes no
//! `rust-version` of its own, so cargo cannot catch it at resolve time — on
//! 1.87 the build fails inside that dependency rather than here.
//!
//! # Platform support
//!
//! | Platform | Sandbox containment |
//! | --- | --- |
//! | macOS | Native, `sandbox-exec` |
//! | Linux | Native, namespaces and rlimits |
//! | Windows | Portable floor only |
//!
//! The portable floor is the honest floor: on Windows the
//! [`Backend::WindowsJobObject`] path was designed and never implemented, so a
//! Windows run reports [`Backend::PortableFloor`] and only the wall-clock cap
//! fires. [`Cap::Cpu`] and [`Cap::Memory`] rest on unix `rlimit` mechanisms with
//! no Windows equivalent, and there is no kernel network boundary there either.
//! The full suite runs on all three in CI.
//!
//! # Guides
//!
//! Longer prose than a doc comment should carry, one page per capability:
//!
//! - [Permissions and approval](https://github.com/initorigin/io-harness/blob/main/docs/guide/permissions.md)
//! - [Verification](https://github.com/initorigin/io-harness/blob/main/docs/guide/verification.md)
//! - [Agent composition](https://github.com/initorigin/io-harness/blob/main/docs/guide/composition.md)
//! - [Execution sandbox](https://github.com/initorigin/io-harness/blob/main/docs/guide/sandbox.md)
//! - [Durable runs](https://github.com/initorigin/io-harness/blob/main/docs/guide/durable-runs.md)
//! - [MCP and network egress](https://github.com/initorigin/io-harness/blob/main/docs/guide/mcp-and-network.md)
//! - [Tools and skills](https://github.com/initorigin/io-harness/blob/main/docs/guide/tools-and-skills.md)
//! - [Context and memory](https://github.com/initorigin/io-harness/blob/main/docs/guide/context-and-memory.md)
//! - [Resilience](https://github.com/initorigin/io-harness/blob/main/docs/guide/resilience.md)
//! - [Observability and replay](https://github.com/initorigin/io-harness/blob/main/docs/guide/observability.md)
//! - [Documents](https://github.com/initorigin/io-harness/blob/main/docs/guide/documents.md)
//! - [Images and git](https://github.com/initorigin/io-harness/blob/main/docs/guide/images-and-git.md)
//!
//! [The public contract](https://github.com/initorigin/io-harness/blob/main/docs/CONTRACT.md)
//! states what is stable, what may change, and the limits that hold today. The
//! crate is pre-1.0: a minor release may break the contract, and when it does it
//! is marked in
//! [CHANGELOG.md](https://github.com/initorigin/io-harness/blob/main/CHANGELOG.md)
//! with a migration note. That file is where the release history lives.
//!

// docs.rs builds with every feature on and sets the `docsrs` cfg (see
// Cargo.toml). This labels each gated item with the feature it needs, so a
// reader browsing the rendered docs is never shown an item that would not exist
// in their build without being told why. Nightly-only, and reached only under
// that cfg — a stable `cargo doc` is unaffected.
#![cfg_attr(docsrs, feature(doc_auto_cfg))]

pub mod approve;
pub mod containment;
pub mod context;
mod contract;
mod error;
pub mod mcp;
mod net;
pub mod observe;
pub mod policy;
pub mod provider;
pub mod resilience;
mod run;
pub mod sandbox;
pub mod skills;
mod state;
pub mod tools;
mod verify;

pub use approve::{ApproveAll, Approver, Decision, DenyAll, Request, StdinApprover};
pub use containment::{Containment, Draw, Ledger, SpawnRefusal};
pub use context::ContextBudget;
pub use contract::TaskContract;
pub use error::{Error, ProviderErrorKind, Result};
pub use mcp::{McpServer, McpTransport, MCP_TOOL_PREFIX};
// The `net` module itself stays private, so the default request deadline is
// surfaced here as well as from each provider module. A caller overriding it with
// `with_timeout` should be able to name the value they are overriding without
// reaching into a provider's namespace to find it.
pub use net::REQUEST_TIMEOUT;
pub use observe::{EventKind, Flow, Ignore, Observer, RunEvent};
pub use policy::{Act, Defaults, Effect, Layer, Policy, Rule, Verdict};
pub use provider::{
    Anthropic, CompletionRequest, CompletionResponse, OpenAi, OpenRouter, Provider, ToolCall,
    ToolSpec, Usage,
};
#[cfg(feature = "media")]
pub use provider::{Media, IMAGE_MEDIA_TYPES};
pub use resilience::{Progress, Progressing, RetryPolicy, StallPolicy};
// Each entry point has an observed twin: a separate function rather than an extra
// parameter on the existing seven, so 0.11.0 code compiles unchanged against
// 0.12.0. The observer is this release's headline, not a reason to break every
// caller that does not want one.
pub use run::{
    resume, resume_from_stored_policy, resume_from_stored_policy_observed, resume_observed,
    resume_tree, resume_tree_from_stored_policy, resume_tree_from_stored_policy_observed,
    resume_tree_observed, resume_tree_with_decision,
    resume_tree_with_decision_observed, resume_with, resume_with_decision,
    resume_with_decision_observed, resume_with_observed, run, run_observed, run_tree,
    run_tree_observed, run_with, run_with_observed, RunOutcome, RunResult, SPAWN_TOOL,
};
pub use sandbox::{
    copy_back, select, Backend, Cap, Sandbox, SandboxConfig, SandboxLimits, SandboxOutcome,
    Selected,
};
pub use skills::{Skill, Skills};
// `AgentEvent` and `SpawnRow` were `pub` inside this private module but were not
// re-exported, so `Store::agent_events` and `Store::find_spawn` returned values an
// external caller could hold and could not name — which made `agent_events`, the
// only audit of per-step budget draws against the shared tree ledger, unreadable
// through the public API. Exported in 0.12.0: an observability release cannot ship
// leaving its own audit table reachable only by opening the SQLite file.
pub use state::{
    AgentEvent, CheckpointEvent, ContextEvent, McpEvent, MemoryEntry, Pending, PolicyEvent,
    RunStatus, RunSummary, SandboxEvent, SpawnRow, StepRecord, Store, BUSY_TIMEOUT,
    CHECKPOINT_FORMAT, MEMORY_MAX_CHARS, MEMORY_MAX_ENTRIES, MEMORY_MAX_ENTRY_CHARS,
    SUCCESS_OUTCOME,
};
pub use tools::git::Identity;
pub use tools::{Tool, ToolFuture, Toolbox};
pub use verify::{ExecGuard, Verification, TEST_BINARY};
