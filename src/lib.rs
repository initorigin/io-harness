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
//! v0.8 makes the harness **extensible, and its network reach governed**. It is
//! an MCP client: [`TaskContract::with_mcp`] connects [`McpServer`]s — spawned as
//! child processes ([`McpTransport::Stdio`]) or dialled over streamable HTTP
//! ([`McpTransport::Http`]) — and their tools are offered to the model beside the
//! built-ins under `mcp__<server>__<tool>`, so a server can never shadow
//! `write_file`. A capability the crate lacks is added by pointing it at a
//! server, not by forking it. Tool calls carry a timeout, results are size-capped,
//! and one session serves a whole v0.5 tree.
//!
//! Because a configured server is the first thing here that can dial an arbitrary
//! host, the v0.4 policy gains a fourth act: [`Act::Net`]. An outbound connection
//! has a target (`host` or `host:port`) decided by the same deny-first stack that
//! decides paths and binaries — [`Policy::allow_net`], [`Policy::deny_net`],
//! [`Policy::ask_net`] — and *every* connection the harness opens passes one
//! checked entry point before a socket exists. Network defaults to deny; the
//! harness contributes the configured provider's host as a visible layer named
//! `provider`, so a deny-all base still reaches its model and the trace says why.
//! An explicit deny of that host still wins. A v0.5 child inherits its parent's
//! network rules and can only narrow them, and a network `Ask` survives a full
//! restart on the v0.7 durable path.
//!
//! What it does **not** govern: a stdio server is a separate process, and once
//! running it dials what it likes. The harness decides whether it may start (an
//! [`Act::Exec`] check on its binary) and which of its tools may be called (an
//! [`Act::Exec`] check on the namespaced name) — not what it does afterwards.
//!
//! v0.8.1 stops the execution gate being defeated by the file it verifies. Until
//! then the subject and the caller's criterion were compiled as one crate, so the
//! subject could shadow a macro the criterion invoked — a file defining
//! `#[macro_export] macro_rules! assert` passed `assert!(false, ...)` — or delete
//! the criterion outright with `#![cfg(any())]` and pass on an empty test binary.
//! Shadowing is now stopped by re-importing the prelude macros explicitly around
//! the criterion, which makes a subject's `assert` ambiguous rather than
//! authoritative; deletion is caught by a probe item compiled with the subject,
//! which a subject that strips its own contents strips too. `test_src` is
//! unchanged, and so is what counts as a passing implementation — including a
//! private one. See [`Verification`] for what a passing gate proves, which is
//! narrower than it has been read to mean.
//!
//! ```no_run
//! use io_harness::{run_with, ApproveAll, McpServer, OpenRouter, Policy, Store,
//!                  TaskContract, Verification};
//!
//! # async fn mcp_demo() -> io_harness::Result<()> {
//! let contract = TaskContract::workspace(
//!     "summarise the repo's README into NOTES.md",
//!     "/path/to/repo",
//!     Verification::WorkspaceFileContains { file: "NOTES.md".into(), needle: "#".into() },
//! )
//! .with_mcp([McpServer::stdio("files", "my-mcp-file-server")]);
//!
//! // Deny-by-default egress. The provider's own host is allowed by the harness's
//! // `provider` layer; nothing else is reachable, and a stdio server may start
//! // only because the exec rule names it.
//! let policy = Policy::default()
//!     .layer("app")
//!     .allow_read("*")
//!     .allow_write("*")
//!     .allow_exec("my-mcp-file-server");
//!
//! let result = run_with(&contract, &OpenRouter::from_env()?, &Store::memory()?,
//!                       &policy, &ApproveAll).await?;
//! # Ok(())
//! # }
//! ```
//!
//! A refusal is not an error the caller has to catch mid-loop: an out-of-policy
//! *tool call* comes back to the model as an observation it can adapt to, while a
//! denied host or an unstartable server fails the run with [`Error::Refused`] or
//! [`Error::Mcp`] before anything happens.
//!
//! v0.9 closes the tool layer by adding the **in-process** half. Implement the
//! object-safe [`Tool`] trait for something the embedding program already knows
//! how to do — [`spec`](Tool::spec) returns the same vendor-neutral [`ToolSpec`]
//! the built-ins and MCP tools are described by, [`invoke`](Tool::invoke) takes
//! the arguments the model sent and returns a boxed [`ToolFuture`] — collect them
//! in a [`Toolbox`], and register it with [`TaskContract::with_tools`], which
//! mirrors [`with_mcp`](TaskContract::with_mcp). They are offered to the model
//! beside `grep`, `find`, `read_file`, and `write_file`, with no second process,
//! transport, or serialization hop for a function that is one `await` away. A
//! registered tool may not take a built-in's name, may not use the `mcp__` prefix
//! reserved for server tools, may not be nameless, and two may not share a name:
//! each is an [`Error::Config`] raised before the provider is called once, not a
//! silent shadowing found at dispatch. Registration makes a tool *available*; it
//! does not authorize it — every call is an [`Act::Exec`] check on the tool's
//! name under the same deny-first v0.4 stack that decides paths, binaries, and
//! hosts, so an operator can hand an agent a toolbox and still refuse one tool in
//! it. A v0.5 child inherits the toolbox and calls it under its own narrowed
//! policy.
//!
//! Alongside it, **skills**: [`TaskContract::with_skills`] points the run at a
//! directory of markdown instruction files that shape *how* the agent approaches
//! a class of task, without touching Rust. Both conventions in common use are
//! discovered — `<dir>/<name>.md` and `<dir>/<name>/SKILL.md` — with optional
//! YAML frontmatter supplying `name` and `description`, falling back to the file
//! stem (or the containing directory, for a `SKILL.md`) and the first prose line.
//! Names and descriptions go into the system prompt, never bodies; the agent
//! loads the one it judges relevant through the built-in
//! [`read_skill`](tools::READ_SKILL_TOOL) tool, which is offered only when a
//! contract configures skills and reads that file as an ordinary policy-checked
//! [`Act::Read`]. A missing path, a path that is not a directory, more than
//! [`MAX_SKILLS`](skills::MAX_SKILLS), or two skills with the same name is an
//! [`Error::Config`] at run start — a rejected set, not a silently truncated one.
//!
//! What it does **not** govern: a registered tool runs in the harness's own
//! process, with the embedding program's privileges. The policy governs whether
//! it is *called*; it does not govern what it does once running — no sandbox, no
//! path scoping, and no egress control applies inside it. This is exactly the
//! bound v0.8 states for a stdio MCP server, and for the same reason: the harness
//! decides what starts, not what a started thing then does. A skill is
//! instructions with no execution of its own — a skill saying "run `rm -rf /`" is
//! a sentence the model reads, and any action it then takes passes the same
//! policy every other action does. Anything that should actually *do* something
//! is a [`Tool`], where the permission layer can see it.
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
pub use policy::{Act, Effect, Policy, Rule, Verdict};
pub use provider::{
    Anthropic, CompletionRequest, CompletionResponse, OpenAi, OpenRouter, Provider, ToolCall,
    ToolSpec, Usage,
};
pub use resilience::{Progress, Progressing, RetryPolicy, StallPolicy};
pub use run::{
    resume, resume_tree, resume_tree_with_decision, resume_with_decision, run, run_tree, run_with,
    RunOutcome, RunResult, SPAWN_TOOL,
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
    RunStatus, SandboxEvent, SpawnRow, StepRecord, Store, BUSY_TIMEOUT, CHECKPOINT_FORMAT,
    MEMORY_MAX_CHARS, MEMORY_MAX_ENTRIES, MEMORY_MAX_ENTRY_CHARS,
};
pub use tools::{Tool, ToolFuture, Toolbox};
pub use verify::{ExecGuard, Verification, TEST_BINARY};
