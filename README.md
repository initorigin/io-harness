# IO Harness

[![crates.io](https://img.shields.io/crates/v/io-harness.svg)](https://crates.io/crates/io-harness)
[![docs.rs](https://img.shields.io/docsrs/io-harness)](https://docs.rs/io-harness)
[![CI](https://github.com/initorigin/io-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/initorigin/io-harness/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/crates/l/io-harness.svg)](LICENSE)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue.svg)](Cargo.toml)

> A Rust agent harness that runs AI agents from a typed task contract to a checked result.

The shared engine every initorigin app (io-cli, io-studio) and io-eval build on.

- **Type:** Rust library crate
- **Stack:** Rust · cargo · tokio · rusqlite · rmcp · own HTTP+SSE provider client
- **API docs:** [docs.rs/io-harness](https://docs.rs/io-harness)
- **License:** Apache-2.0
- **MSRV:** Rust **1.88**. The floor comes from `rmcp`, which publishes no
  `rust-version` of its own, so cargo cannot catch it at resolve time — on 1.87
  the build fails inside that dependency rather than here.
- **Status:** Pre-release (0.x), published on crates.io. Released through
  0.13.0; 0.12.0 closed the twelfth of the twelve pillars and all twelve still
  hold. 0.14.0 (documents) is the release in progress. Per-release detail is in
  [CHANGELOG.md](CHANGELOG.md).

## Capabilities

- Task contract — goal, constraints, expected output, success criteria
- Context construction — feed the model only relevant, current, trusted info ✅ **v0.10**: per-turn assembly under a `ContextBudget`, superseded observations compacted, a read invalidated by a later write re-read
- Tool layer — narrow, typed actions the agent invokes ✅ **v0.9** completes it: the public `Tool` trait, in-process and policy-governed
- Orchestration loop — observe, reason, act, check, stop
- State and memory — progress, intermediate results, decisions (rusqlite) ✅ **v0.10** adds durable cross-run memory, keyed to the workspace
- Verification layer — tests, schemas, read-backs confirm the task is done
- Permissions and guardrails — what the agent may access, change, send, spend ✅ **v0.8** completes *send*: `Act::Net`, deny-by-default egress
- Recovery and retry — retries, fallbacks, replanning, escalation ✅ **v0.11**: classified provider failures, kind-aware retry with backoff, `Fallback`, stall detection
- Stop conditions and budgets — cap steps, time, cost, retries, risky actions
- Observability and tracing — record prompts, decisions, tool calls, cost ✅ **v0.12** completes it
- Evaluation layer — success, reliability, safety, latency, cost across cases ✅ **v0.12**
- Human approval layer — review before sensitive or irreversible actions
- Providers — OpenRouter first, then Anthropic and OpenAI (own HTTP+SSE client)
- Agent composition — spawn and nest many agents (100+) with shared context
- Long-running autonomous tasks — 24h+ with no user input
- Ephemeral local code-exec sandboxes — write, run, capture, destroy ✅ **v0.6** (OS-native + OS-neutral: macOS `sandbox-exec`, Linux namespaces, portable floor everywhere — on Windows the floor only, where the Job Object is designed but not implemented and just the wall clock is enforced)
- Built-in tools — `write_file`, `read_file`, `grep`, `find` ✅ **v0.1**, **v0.3**; `read_skill` ✅ **v0.9**; `remember` ✅ **v0.10**; `spawn_agent` (in `run_tree` only) ✅ **v0.5**
- Office and document tools ✅ **v0.14**, behind an opt-in `documents` feature: Excel read/generate/preserving-edit, Word read and generate, PowerPoint text read-only, PDF generate/extract/watermark/AcroForm form-fill, barcode and QR decode. OCR, PowerPoint authoring, barcode/QR generation, in-place Word edit, and document delete are **cut from the roadmap**, not deferred
- Media and git — image and video passthrough when the model supports it; real repository work — 🚧 **planned v0.15**
- Extensibility — MCP (rmcp) ✅ **v0.8** (client; stdio + streamable HTTP), in-process `Tool` implementations and skills ✅ **v0.9**

See [docs/CAPABILITIES.md](docs/CAPABILITIES.md) for detail and
[docs/CONTRACT.md](docs/CONTRACT.md) for the public contract.

## Usage (v0.4)

Hand the harness a task contract; it runs the loop, verifies the result, records
every step to rusqlite, and stops on success or a budget. A single-file task uses
the filesystem tool; a workspace task lets the agent `grep`/`find` across a
repository and edit several files. Pick any of three providers at construction.
Everything shipped since — the permission boundary, sub-agents, the sandbox,
durable resume, MCP and egress, custom tools and skills, context assembly and
memory, resilience — has its own section further down. Only the items marked
🚧 **planned** in **Capabilities** are still roadmap.

### 1. Add the crate

```toml
[dependencies]
io-harness = "0.12"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

Upgrading from 0.2: `TaskContract::new`, `run`, `resume`, and the single-file
loop are unchanged, so existing callers keep compiling. New in 0.3:
`TaskContract::workspace`, the `grep`/`find`/`read_file` tools, the
`EachCompilesRust` / `WorkspaceTestPasses` verifications, and the `Anthropic` /
`OpenAi` providers. If you implement `Provider` yourself, note it gained a
defaulted `name()` method (override it to label your provider in the trace — no
change required to keep compiling). A 0.2 rusqlite database is migrated in place
on open (a `provider` column is added; a 0.2 binary still reads it).

### 2. Choose a provider

Each provider reads its own key + model slug from the environment; credentials
are never logged, and no default model is guessed. Selecting a provider is just
constructing a different one — the task contract does not change.

```bash
# OpenRouter
export OPENROUTER_API_KEY=sk-or-...
export OPENROUTER_MODEL=anthropic/claude-sonnet-4
# Anthropic
export ANTHROPIC_API_KEY=sk-ant-...
export ANTHROPIC_MODEL=claude-sonnet-4
# OpenAI
export OPENAI_API_KEY=sk-...
export OPENAI_MODEL=gpt-4o
```

```rust
use io_harness::{Anthropic, OpenAi, OpenRouter};

let provider = OpenRouter::from_env()?;   // or:
let provider = Anthropic::from_env()?;    // or:
let provider = OpenAi::from_env()?;
// ...then hand `&provider` to `run` — nothing else changes.
```

### 3. Run one file-edit task, bounded and verified by execution

```rust
use std::time::Duration;
use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let contract = TaskContract::new(
        "add a `hello` function that returns 42",
        "src/hello.rs",
        // Execution-based: the file must compile AND pass this test. A stub
        // that only contains the right substring fails the gate. Since v0.8.1
        // the file is compiled as its own crate and this test compiles against
        // it, so the file cannot shadow `assert_eq!` or delete the test to pass.
        Verification::RustTestPasses {
            test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
        },
    )
    .with_max_steps(8)                              // step budget
    .with_time_budget(Duration::from_secs(120))     // time budget
    .with_token_budget(200_000)                     // cost budget, in tokens
    .with_max_retries(2);                           // retry transient failures

    let provider = OpenRouter::from_env()?;
    let store = Store::open("runs.db")?;

    let result = run(&contract, &provider, &store).await?;
    // Success | StepCapReached | TimeBudgetExceeded | CostBudgetExceeded |
    // Denied | AwaitingApproval | Stalled | Escalated | BudgetCeilingReached |
    // Refused — the full `RunOutcome`.
    println!("{:?}", result.outcome);
    Ok(())
}
```

### 4. Run a repository task: grep, find, and multi-file edits

Give the agent a workspace root instead of one file. It can `grep` file contents
(regex or substring), `find` files by name/path glob, `read_file` to inspect
what it found, and `write_file` to edit several files — all confined to the root
(a `..` or absolute path that escapes is refused). Verification spans the set:
`WorkspaceTestPasses` compiles the edited files together with a test, so the run
only succeeds when the whole set is correct.

```rust
use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

let contract = TaskContract::workspace(
    "Make a() + b() equal 42 by editing the two source files.",
    "my-repo",                                  // workspace root
    Verification::WorkspaceTestPasses {
        files: vec!["src/a.rs".into(), "src/b.rs".into()],
        test_src: "#[test] fn t() { assert_eq!(a() + b(), 42); }".into(),
    },
);

let result = run(&contract, &OpenRouter::from_env()?, &Store::memory()?).await?;
println!("{:?}", result.outcome);
```

The grep/find tools skip `.git`, `target`, and `node_modules`. Use
`Verification::EachCompilesRust(files)` when each edited file only needs to
compile on its own.

### Execution-based verification

Content checks (`FileContains`, `FileEquals`) confirm the file *says* the right
thing but not that it *works* — the 0.1 live run passed `FileContains("fn hello")`
by writing the literal string `fn hello`, which does not compile. v0.2 adds gates
that run the artifact:

- `Verification::CompilesRust` — passes only if the file compiles (`rustc --crate-type lib`).
- `Verification::RustTestPasses { test_src }` — compiles `test_src` against the file and passes only if the test binary passes.

Compilation runs locally in a throwaway temp dir (removed afterwards) and touches
no network.

**What a passing gate proves.** That the stated criterion was satisfied — not
that the artifact is correct. A gate asserting `hello() == 42` is silent about
everything else the file does. Execution gates are stronger than content gates
because a stub cannot compile and run, but stronger is not sufficient.

**The subject cannot defeat its own gate (since v0.8.1).** Until v0.8.1 the file
under verification and your `test_src` were compiled as one crate, so the file
could change how your criterion resolved. A file defining `#[macro_export]
macro_rules! assert` made `assert!(false, "this gate can never pass")` report a
pass; a file opening with `#![cfg(any())]` deleted the criterion entirely and the
empty test binary exited 0. An agent found the first of these unprompted during
io-cli 0.1.0's live runs. Your criterion now sits in a module of the file's crate
that re-imports the prelude macros explicitly, so a file defining its own `assert`
makes the name ambiguous instead of capturing it, and the gate fails to compile
rather than passing; a probe compiled alongside the file catches one that deleted
its own contents. Nothing changed in what you write: `test_src` still calls the
file's items unqualified, a macro the file legitimately exports still reaches it,
and your criterion still reaches the file's **private** items — an implementation
does not have to be `pub` to pass.

The compile-only gates had the same hole by a different route, needing no
criterion at all: `#![cfg(any())]` strips a file's items *before* rustc
type-checks them, so `pub fn hello() -> u32 { "not a u32" }` compiled clean and
passed. `CompilesRust` and `EachCompilesRust` now confirm the file's items
survived to be checked. Legitimate crate-level attributes (`#![allow(dead_code)]`,
`#![no_std]`) are unaffected.

If a run stopped passing when you upgraded to v0.8.1, the gate was being
defeated. `SandboxEvent::gate_phase_failed` in the trace says which phase failed
— `subject-compile`, `criterion-compile`, `test-run`, or `subject-emptied`.

### Trace and resume

Every step's prompt, decision, tool call, and token usage is persisted. Read the
full trace back, and resume an interrupted run under its original id instead of
restarting:

```rust
use io_harness::{resume, Store};

// After a crash or a hit budget, continue the same run from where it stopped.
let store = Store::open("runs.db")?;
let result = resume(&contract, &provider, &store, run_id).await?;

for step in store.steps(result.run_id)? {
    println!("step {}: {} ({} tokens)", step.step, step.decision, step.tokens);
}
```

A resumed run keeps the step it reached, what it spent, how long it has been
alive, and the context it had assembled — it continues the same run rather than
re-deriving one. If the run was started under a permission policy, resume it with
`resume_with` and supply that policy; plain `resume` refuses a policy-bearing run
rather than continuing it without a boundary.

```rust
let result = io_harness::resume_with(
    &contract, &provider, &store, run_id, &policy, &approver,
).await?;
```

Or run it live end to end: `cargo run --example edit_file`.

## Permissions and approval (v0.4)

A `Policy` is a stack of named layers plus a per-action default. It is evaluated
**deny-first across the whole stack**: a deny in any layer beats an allow in any
other, so a layer can add capability but can never re-allow what a layer beneath
it denied.

```rust
use io_harness::{run_with, ApproveAll, Policy, Store, TaskContract, Verification};

let policy = Policy::default() // reads open, writes/execs ask, egress denied, secrets denied
    .layer("project")
    .allow_read("*")
    .deny_read("secrets/*")
    .deny_write("secrets/*");

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

// Why was that refused? Same function the tool layer enforces with.
let verdict = policy.explain(io_harness::Act::Write, "secrets/key.txt");
println!("{:?} by rule {:?} in layer {:?}", verdict.effect, verdict.rule, verdict.layer);
```

**The default is permissive.** A caller who passes no policy — plain `run()` —
gets no enforcement and the exact 0.3.0 behaviour. The boundary is opt-in. This
is a deliberate trade-off for backward compatibility, not an oversight.

### What asks, what is refused

`Policy::default()` sets the tiers, following the same shape Claude Code uses:

| Action | Default | Note |
| --- | --- | --- |
| Read | allow | `.env`, `*.pem`, `id_rsa`, `id_ed25519`, `*.key` denied outright |
| Write | **ask** | including overwriting a file the path rules already allow |
| Exec | ask | `rustc` and `<test-binary>` allowed, so verification works |
| Net (since v0.8) | **deny** | an outbound host is not something a human can meaningfully approve on sight mid-run; name your hosts with `allow_net` — your configured provider is allowed for you |

A **denied** action never reaches the approver — it is refused and reported to
the model as a tool result it can adapt to, and the refusal consumes a step, so
a model retrying it reaches the step cap rather than looping. Only the
**ask** tier prompts.

### The approver

```rust
use io_harness::approve::{Approver, Decision, DecisionFuture, Request};

impl Approver for MyUi {
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async move {
            match self.ask_the_human(request).await {
                Answer::Yes      => Decision::approve(),
                Answer::No       => Decision::deny("not this one"),
                Answer::NotNow   => Decision::Defer,   // persist and decide later
            }
        })
    }
}
```

The trait is object-safe (`Box<dyn Approver>`) and the future may stay pending
indefinitely — the run waits rather than timing out. `Decision::Approve` can
also carry a rewritten action (`modified`) or rules to `remember` for the rest
of the run. Both are re-checked against the policy: **an approval cannot move an
action across a deny, and a remembered allow cannot override one.**
Remembered rules come back on `RunResult::remembered` for you to persist.

Built-ins: `ApproveAll`, `DenyAll`, `StdinApprover`.

### Deferring past the end of the process

```rust
match result.outcome {
    RunOutcome::AwaitingApproval { request_id, .. } => {
        // ...hours later, another process, same rusqlite file
        let store = Store::open("runs.db")?;
        io_harness::resume_with_decision(
            &contract, &provider, &store, run_id, request_id,
            Decision::approve(), &policy, &approver,
        ).await?
    }
    _ => result,
};
```

The pending action is persisted with the content the human was shown, so the
resumed action is exactly the one approved. The policy is re-checked on resume,
so a deny that landed while it waited still holds.

### Sharing one policy between apps

`Policy` is `serde`-serializable, so io-cli and io-studio read the same format
and neither writes its own parser. Compose layers with `merge`:

```rust
let effective = shared_base.merge(app_local);
```

The recommended convention is **user base → project layer → app overlay**, each
app keeping its own config file over a shared base. The crate composes a stack
it is handed; **it does not discover config files** — locations and precedence
are the adopting app's responsibility. Because denies are absolute across
layers, a shared base stays trustworthy no matter what an app stacks on top.

Run it live: `cargo run --example policy_run`.

## Agent composition and containment (v0.5)

A single loop does work one step wide. For large or parallelisable tasks, a
**parent agent decomposes the work and spawns sub-agents** — up to 100+ at once —
each running the same observe/reason/act/verify/stop loop over the **same
workspace and the same trace**. A child's result composes back so the parent
continues from what it produced, and children may nest.

Sub-agents are **opt-in**: only `run_tree` offers the `spawn_agent` tool. Pass a
`Containment` and the tree runs under it.

```rust
use io_harness::{run_tree, ApproveAll, Containment, Policy, Store, TaskContract, Verification};

let provider = io_harness::OpenRouter::from_env()?;
let store = Store::memory()?;

let contract = TaskContract::workspace(
    "Coordinate: delegate each file to a sub-agent, then combine.",
    "path/to/workspace",
    Verification::WorkspaceFileContains { file: "summary.txt".into(), needle: "DONE".into() },
);

// Caps for the WHOLE tree — no spawned task can raise them.
let containment = Containment::new(
    /* max_total_agents */ 100,
    /* max_concurrent   */ 16,
    /* max_depth         */ 3,
    /* max_total_tokens  */ 500_000,
);

let result = run_tree(&contract, &provider, &store, &Policy::permissive(), &ApproveAll, &containment).await?;
```

### Containment is inherit-and-narrow

The 0.4 policy becomes the boundary for spawned agents. Where `Policy::merge`
lets an overlay *widen* a base (allows union), `Policy::contain` derives a
**child** policy that can only *narrow*:

- **denies union** downward — a child adds restrictions;
- **allows intersect** downward — a child can never read, write, or execute
  anything its parent could not;
- the rule holds at **any depth** — no descendant can hold an effective allow the
  root did not grant.

```rust
let child_effective = parent_policy.contain(&child_overlay); // child cannot widen
```

### One spend ceiling above the task contract

The whole tree draws its token spend from **one shared ledger**. A spawned
`TaskContract` can set a *tighter* budget but never a looser one than the tree has
left; when the aggregate `max_total_tokens` is reached the tree halts as a whole.
A spawn that would breach any cap — agents, depth, or budget — is **refused** as a
tool result the parent can adapt to, and every spawn, refusal, and budget draw is
in the rusqlite trace as one reconstructable graph.

Run it live: `cargo run --example subagents`.

## Execution sandbox (v0.6)

Since v0.2 the verification gate has compiled and run model-produced code. Until
v0.6 that ran **directly on the host** — the "compiles locally, no isolation"
limitation, made sharper by v0.5's many concurrent agents. v0.6 routes every such
execution through a `Sandbox`:

- **Ephemeral workdir** — created per run and **destroyed** on every exit path
  (success, failure, cap kill), so nothing it writes or spawns outlives it.
- **Resource caps that kill, not throttle** — `SandboxLimits` caps CPU time,
  wall-clock, and memory; a breach returns a *typed* cap hit, never a hang. The
  CPU and memory caps are unix mechanisms (`RLIMIT_CPU`, an RSS monitor); on
  Windows only the wall clock is enforced, and a cap that was not applied is
  never reported as hit.
- **Network denied by default** — a configurable egress allow-list is deferred to
  v0.8; v0.6 is deny-by-default only.
- **Trace** — every create, the argv and the backend that ran it, each cap hit,
  and each teardown land in the rusqlite trace, so an operator can audit *where*
  each piece of code ran and *how* it was isolated.

### OS-native and OS-neutral

One trait, a real native backend per platform, over a portable floor that runs
everywhere — so a task isolates the same way on mac, linux, and windows:

| Backend | Isolation |
| --- | --- |
| **macOS `sandbox-exec`** | profile confines writes to the workdir and **denies network**; rlimits cap CPU/fds; an RSS monitor caps memory (macOS does not enforce address-space rlimits) |
| **Linux namespaces** | user/mount/pid/**net** namespaces — a *hard* network boundary and a private root — plus rlimits *(cfg-gated; compiled + unit-tested, not live-run on the macOS build host)* |
| **Windows** | **no native backend yet** — the portable floor, wall clock only (see the note under this table) |
| **Portable floor** | the guaranteed minimum on every OS: fresh subprocess, ephemeral workdir, resource caps, network env stripped. Deliberately the **weakest** backend — filesystem-scoped and resource-capped, *not* a full syscall jail |

**Windows, stated plainly.** The Job Object is designed but **unimplemented** — no
Win32 call is made — so a Windows run gets the portable floor and reports it as
such. On Windows that floor
enforces the **wall clock only**: no CPU cap, no memory cap, no process cap (all
three are unix `rlimit`/`ps` mechanisms) and no kernel network boundary. The
wall-clock kill does reach the whole process tree. Caps that are not applied are
never reported — a Windows run never claims a CPU or memory cap hit. Tracked for a
dedicated release.

`select` picks the strongest backend available on the running OS and records which
ran. Sandboxing is the **new default** for the verification gate and is transparent
to it — the same code passes or fails as before — and a caller who wants the exact
v0.5 direct-host execution can opt it off, so the change is additive and reversible.

Run it live: `cargo run --example sandbox_run`.

## Durable, unattended runs (v0.7)

Start a long task and walk away. v0.7 makes a run survive a crash or a full
process restart, so the harness can run unattended for a long horizon (24h+) with
no user input and pick up exactly where it stopped.

- **Checkpoint after every step, transactionally** — each completed step's trace
  row, its budget draw, and a checkpoint marker are committed in one rusqlite
  transaction. The committed checkpoint *is* the step's completion marker: a crash
  mid-commit leaves either a whole step or none of it, never a torn half.
- **Resume the whole tree** — `resume` (or `resume_with`, for a run started under
  a policy) continues a single or workspace run;
  `resume_tree` reconstructs a crashed v0.5 tree and continues **every** agent from
  its own last committed step. A parent *adopts* the children it had already
  spawned and resumes each from its checkpoint, rather than duplicating or
  restarting them.
- **Idempotent by construction** — a completed step is skipped (recorded as a
  `skipped` event), the aggregate `Ledger` budget is restored from durable totals
  (never reset, never double-charged), the time budget counts real wall-clock
  elapsed across the downtime, an already-applied edit is re-observed rather than
  repeated, and re-running a resume is a no-op.
- **Approval survives a restart** — a v0.4 sensitive action that pauses the tree
  outlives the process; a fresh process delivers the decision with
  `resume_tree_with_decision` and the tree continues.
- **Sandboxes are re-created, never resumed** — an ephemeral v0.6 sandbox is never
  checkpointed, so an exec in flight at crash time simply re-runs in a fresh
  sandbox; a committed sandboxed step is skipped.
- **Typed failure** — a resume against a newer-format or missing checkpoint is an
  `Error::Resume`, not a panic or a half-resume.

The 24h horizon is proven by a real `kill -9`-then-resume test plus a time-scaled
long unattended run; a literal 24h wall-clock run is noted, not gated on.

Run it live: `cargo run --example durable_run` (kills itself mid-run and resumes).

## MCP and network egress (v0.8)

Point the harness at MCP servers and their tools reach the agent beside the
built-ins. Because a configured server is the first thing here that can dial an
arbitrary host, the v0.4 policy grows a fourth act at the same time.

```rust
use io_harness::{run_with, ApproveAll, McpServer, OpenRouter, Policy, Store,
                 TaskContract, Verification};

let contract = TaskContract::workspace(
    "summarise the repo's README into NOTES.md",
    "/path/to/repo",
    Verification::WorkspaceFileContains { file: "NOTES.md".into(), needle: "#".into() },
)
.with_mcp([
    McpServer::stdio("files", "my-mcp-file-server"),
    McpServer::http("search", "https://mcp.example.com/mcp"),
]);

let policy = Policy::default()
    .layer("app")
    .allow_read("*")
    .allow_write("*")
    // The stdio server may start; nothing else may be executed.
    .allow_exec("my-mcp-file-server")
    // The HTTP server may be reached; every other host stays denied.
    .allow_net("mcp.example.com");

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

- **Namespaced tools** — a server's tools arrive as `mcp__<server>__<tool>`, so a
  server advertising `write_file` cannot shadow the built-in. Both stay callable
  and distinct.
- **Two transports** — `McpServer::stdio` spawns a child process,
  `McpServer::http` dials a streamable-HTTP endpoint. One session serves a whole
  v0.5 agent tree, not one connection per agent.
- **`Act::Net`** — an outbound connection is a policy decision with a target
  (`host` or `host:port`), matched by the same glob matcher as paths and binaries
  and decided by the same deny-first stack: `allow_net`, `deny_net`, `ask_net`. An
  `ask_net` routes to the `Approver` and, if deferred, survives a full process
  restart like any other v0.4 approval.
- **Your provider still works under deny-all** — the harness contributes its
  configured provider's host as a *named* layer, `provider`, so you need not list
  it. `Policy::explain` attributes the allowance to that layer rather than hiding
  it. An explicit `deny_net` of your own provider still wins, and fails fast as a
  refusal rather than hanging.
- **Two gates per server** — starting a stdio server is an exec check on its
  binary; calling one of its tools is an exec check on the namespaced tool name.
  So a policy can allow a server generally and still deny one of its tools.
- **Contained downward** — a v0.5 child inherits its parent's network rules and can
  only narrow them; the spawn tool takes `deny_net` alongside `deny_write`.
- **Everything is traced** — connects (with transport), tools discovered, each call
  with latency and outcome, and every network verdict with the layer that decided
  it. A 0.7.0 database migrates in place.

### What a refusal looks like

An out-of-policy **tool call** is an observation the model can adapt to, not a
crashed run — the same treatment a refused path already gets:

```text
[mcp__files__delete_everything refused] (rule mcp__files__delete_*) — the policy forbids calling this tool
```

A denied **host**, or a configured server that will not start, stops the run
before anything happens, with `Error::Refused { act: "net", .. }` or
`Error::Mcp { server, reason }` — because unlike a single tool call, there is no
useful way for the agent to work around a capability it was told it had.

### The limit, stated plainly

The harness governs the connections **it** opens. A stdio MCP server is a separate
process: the harness decides whether it may start and which of its tools may be
called, but once running it dials whatever it likes. Isolating a server's own
egress would need OS-level containment, which v0.8 does not build.

## In-process tools and skills (v0.9)

v0.8 made the harness extensible **out of process**. That is the right boundary
for a capability that already lives elsewhere and the wrong one for a capability
already linked into the same binary: a second process, a transport, and a
serialization hop to call a function that is one `await` away. v0.9 adds the
in-process half — implement `Tool` for something your product already knows how
to do — plus **skills**, markdown instruction files that shape *how* the agent
works without touching Rust.

### Register a tool your program already has

```rust
use io_harness::tools::{Tool, ToolFuture, Toolbox};
use io_harness::{run_with, ApproveAll, Policy, TaskContract, ToolSpec, Verification};
use serde_json::json;

struct LookupOrder { db: OrderDb }

impl Tool for LookupOrder {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "lookup_order".into(),
            description: "Look up an order by id. Returns its status and line items.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }),
        }
    }

    fn invoke<'a>(&'a self, arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        // Read defensively: a model can send a missing or mistyped field, and
        // that is an observation to adapt to, not a crash.
        let id = arguments.get("id").and_then(|v| v.as_str()).unwrap_or("").to_string();
        Box::pin(async move {
            Ok(match self.db.order(&id).await {
                Some(order) => format!("{order}"),
                None => format!("no order with id {id:?}"),
            })
        })
    }
}

let contract = TaskContract::workspace(
    "Write the status of order 4471 into REPORT.md.",
    "/path/to/repo",
    Verification::WorkspaceFileContains { file: "REPORT.md".into(), needle: "4471".into() },
)
.with_tools(Toolbox::new().with(LookupOrder { db }));

let policy = Policy::default()
    .layer("app")
    .allow_read("*")
    .allow_write("*")
    // Registering the tool offers it. This is what allows it to be called.
    .allow_exec("lookup_order");

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
```

- **Registration is availability, not authority** — a call is an `Act::Exec`
  check on the tool's name, decided by the same deny-first stack that decides
  paths, binaries, and hosts. Hand an agent a toolbox and still refuse one tool
  in it; the refusal is an observation the agent adapts to, with the deciding
  rule and layer in the trace. An `ask_exec` routes to the `Approver` and
  survives a restart like any other v0.4 approval.
- **Nothing may shadow anything** — a registered tool cannot take the name of a
  built-in (`write_file`, `grep`, `find`, `read_file`, `read_skill`,
  `remember`, `spawn_agent`), cannot use the `mcp__` prefix reserved for server tools, and two
  registered tools cannot share a name. Each is an `Error::Config` raised
  **before the provider is called once**, not a silent shadowing found at
  dispatch.
- **A failing tool is an observation** — returning `Err` puts the message in the
  observations and the run continues, the same treatment `grep` gives a bad
  regex. Only the model can tell "try another id" from "give up on this
  approach".
- **It cannot flood the context** — a result over the cap is truncated with a
  visible marker before it enters the observations, and the truncated form is
  what the trace records.
- **Inherited by the tree** — a v0.5 child is offered its parent's toolbox and
  calls it under the child's own narrowed policy. Inheritance grants the tool;
  `Policy::contain` still decides the call.
- **Traced like a built-in** — same decision, argument, and observation rows, so
  an audit does not have to distinguish extension from core.

Run it live: `cargo run --example custom_tool`.

### Skills: instructions, not code

Point the contract at a directory of markdown. Both conventions in common use
are accepted, so a directory written for another agent tool usually works
unchanged:

```text
skills/
  migrations.md          -> skill "migrations"
  api-style/
    SKILL.md             -> skill "api-style"
```

```rust
let contract = TaskContract::workspace(
    "Add the `orders` table migration.",
    "/path/to/repo",
    Verification::EachCompilesRust(vec!["migrations/003_orders.rs".into()]),
)
.with_skills("skills");   // discovered once per run, not once per step
```

Optional YAML frontmatter names and describes a skill; without it the name comes
from the file stem (or the containing directory, for a `SKILL.md`) and the
description from the first prose line:

```yaml
---
name: migrations
description: How to write a reversible database migration in this repo.
---

Always write the down-migration first...
```

- **Names and descriptions reach the prompt; bodies do not** — twenty skills
  would otherwise be paid for on every turn of every run. The agent is told what
  exists and calls the built-in `read_skill` tool for the one it judges
  relevant, which enters the observations once. The harness does not rank,
  match, or auto-inject — automatic relevance selection is a context-construction
  question and is deliberately not here.
- **Reading one is an ordinary policy-checked read** — a policy denying
  `Act::Read` over the skills directory keeps the catalogue in the prompt and the
  bodies out of the context, with the refusal in the trace. An unknown skill name
  returns an observation listing what does exist, not an error.
- **A bad directory fails honestly** — a missing path, a path that is not a
  directory, more than 64 skills, or two skills with the same name is an
  `Error::Config` at run start. A rejected set, not a silently truncated one the
  caller believes is complete.

Run it live: `cargo run --example skills_run`.

### The boundary, stated plainly

A registered tool runs **in the harness's own process, with the embedding
program's privileges**. The policy governs whether it is *called*; it does not
govern what it does once running — no sandbox, no path scoping, and no egress
control applies inside it. This is exactly the bound v0.8 already states for a
stdio MCP server, and for the same reason: the harness decides what starts, not
what a started thing then does. A tool that shells out, writes outside the
workspace, or dials a host has done so with your full authority.

A **skill** is instructions with no execution of its own. A skill saying "run
`rm -rf /`" is a sentence the model reads, and any action it then takes passes
the same policy every other action does. Anything that should actually *do*
something is a `Tool`, where the permission layer can see it.

## Context and memory (v0.10)

Through v0.9 the workspace loop kept one string, appended every tool result to
it, and re-sent the whole thing verbatim every turn. Nothing bounded it, nothing
dropped a read the agent had already replaced, and nothing noticed when a write
made an earlier read wrong. A long run spent most of every request re-sending
stale text: the token budget was enforced, it was just being spent on
repetition.

v0.10 assembles the prompt instead of accumulating it.

```rust
use io_harness::{context::ContextBudget, TaskContract, Verification};

let contract = TaskContract::workspace("Refactor the parser.", &root, verify)
    .with_token_budget(400_000)
    // Absolute ceiling per request, and the share of the *unspent* token
    // budget a request may carry of what the run has already observed.
    .with_context_budget(ContextBudget { max_tokens: 24_000, share: 0.5 });
```

Each turn, under that budget:

- **Superseded observations compact.** Two reads of one file, or two greps of
  one pattern, are one answer. The later one is carried whole; the earlier
  becomes a one-line stub naming the step that replaced it.
- **A stale read is re-read, not trusted.** If the agent wrote to a file it read
  earlier, the earlier read is refreshed at assembly time — through the same
  policy and the same workspace containment as any other read. If the policy
  refuses, or the path is gone, the entry becomes a stub naming the write that
  invalidated it and why the refresh failed.
- **Every observation is bounded where it enters the context**, with the elision
  visible to the model so it can ask for the rest. One budget derives both the
  request ceiling and the per-observation cap.
- **The trace keeps everything.** `steps.result` records the full, unelided log.
  Bounding what the model sees must never bound what an operator can audit.

### Durable memory

The agent can also record what it learned, keyed to the workspace rather than to
the run, and get it back on a later run over the same workspace:

```rust
// The agent calls the built-in `remember` tool during a run; the operator reads
// and clears what it wrote.
for entry in store.memory_list(&workspace)? {
    println!("{}: {}  (run {}, step {})", entry.key, entry.value, entry.run_id, entry.step);
}
store.memory_delete(&workspace, "build-command")?;
store.memory_clear(&workspace)?;
```

Entries are attributed to the run and step that wrote them, capped in count and
in total size with oldest-first eviction, and every write, eviction and recall is
in the trace.

### The limits, stated plainly

Assembly **bounds** what a request carries and applies exactly the two staleness
rules above. It does **not** promise the model sees everything relevant to the
task: an observation older than the current window is a stub, and a stub the
model does not act on is information it does not have. If your application needs
a particular observation present, put it in the task contract, where it is not
subject to elision.

The token figure the assembler enforces against is an **estimate** — four chars
per token, computed in-crate, no tokenizer dependency. The trace records that
estimate beside the provider's own reported usage for the same request, so the
drift is a number you can read rather than a claim. The default share leaves
margin for it; a provider that rejects a request for context length is reported
as such rather than retried identically.

Memory entries are **agent-authored notes, not instructions**. A fact one run
recorded is read by later runs over that workspace, so a wrong or planted note
persists until someone removes it — which is why entries carry their origin, are
rendered to the model as its own notes rather than as directives, and are
listable and deletable through `Store`. An operator can always see and clear what
the agent believes.

## Resilience (v0.11)

Through v0.10 a run survived a crash and nothing else. Every provider failure was
one `String` — a 429 and a wrong API key were the same value, retried equally
eagerly, with no backoff. Three providers were implemented and only one was
reachable in a run. And three failures had no handling at all: a server that
accepted a connection and then stopped writing hung the run forever, a malformed
response was read as "the model chose not to call a tool", and an escalated run
was silently re-run by the next `resume`.

### Failures you can branch on

```rust
use io_harness::{Error, ProviderErrorKind};

match harness_result {
    Err(Error::Provider { kind: ProviderErrorKind::Auth, .. }) => // your key
    Err(Error::Provider { kind: ProviderErrorKind::RateLimited, retry_after, .. }) => // wait
    Err(Error::Provider { kind, .. }) if kind.is_retryable() => // transient
    _ => {}
}
```

`Transport`, `Timeout`, `RateLimited`, `Server` and `Malformed` are retryable;
`Auth` and `Request` are not, because re-sending a request the server read and
refused is two failures instead of one. Retries wait, doubling to a ceiling and
honouring the server's `Retry-After` above that ceiling — and never sleeping past
the run's own time budget.

### A second provider behind the first

```rust
use io_harness::provider::Fallback;
use io_harness::{Anthropic, OpenRouter};

let provider = Fallback::new(OpenRouter::from_env()?, Anthropic::from_env()?);
let result = io_harness::run(&contract, &provider, &store).await?;
```

`Fallback` is itself a `Provider`, so no entry point changes, and it nests for
three. It falls through only on a failure another provider might not have. Both
hosts are authorized against the v0.8 egress policy before the first step — a
fallback is not a way around it.

### An agent that stops getting anywhere

```rust
use io_harness::StallPolicy;

let contract = contract.with_stall_policy(StallPolicy { window: 3, max_replans: 1 });
```

A stall needs both halves: `window` consecutive steps that changed nothing in the
workspace **and** repeated a tool call the window already saw. One alone is not
enough — a run reading four different files is working, not stuck. On the first
stall the agent is told what it already tried and given one chance to change
approach; if it stalls again the run ends as `RunOutcome::Stalled` instead of
spending the rest of its step budget. `window: 0` switches it off.

This is a failure that was measured, not imagined: the v0.10 evidence records a
live run re-reading the same four files for sixteen consecutive turns before
hitting its step cap.

### The limits, stated plainly

**Fallback does not promise equivalence.** Falling over swaps the model mid-run,
and the harness cannot see whether the replacement is as capable or even the same
size. The provider that answered is recorded per step so you can tell.

**The request deadline defaults to 600 seconds** — long enough that no realistic
single completion reaches it, since a full 8192-token stream at a sluggish 15
tokens/second is about nine minutes. A hung socket is caught by any finite
deadline, so the default is deliberately generous. Since 0.12.0 you can name it
and replace it — `with_timeout` on any of the three providers, and the default is
public as `REQUEST_TIMEOUT`:

```rust
use std::time::Duration;
use io_harness::{OpenRouter, REQUEST_TIMEOUT};

// Slower model: raise it. Impatient caller: lower it. Rebuilds the HTTP client,
// so call it before handing the provider to a run.
let provider = OpenRouter::from_env()?.with_timeout(Duration::from_secs(1_800));
assert_eq!(REQUEST_TIMEOUT, Duration::from_secs(600));   // the default it replaces
```

`Anthropic::with_timeout` and `OpenAi::with_timeout` are the same call, and each
provider module re-exports `REQUEST_TIMEOUT` beside its own type.

**Stall detection is a heuristic on a signal, not a proof.** It reads whether a
write changed the file's bytes and whether a tool call repeated. An agent that
makes real progress the harness cannot see — thinking, or work in a tool whose
effects are outside the workspace — could in principle be flagged, which is why
the window is configurable and can be disabled.

**A retry is not a queue.** Honouring `Retry-After` is not the same as modelling a
provider's rate limit, and nothing here tracks a provider's health across runs: a
provider that failed the last run starts the next one trusted.

## Documents (v0.14)

Through v0.13 every file the agent could open was text. A workbook, a report, or
a form was a byte blob it could neither read nor produce. v0.14 adds five
formats, all optional:

```toml
io-harness = { version = "0.14", features = ["documents"] }
```

`documents` is an umbrella over five per-format features — `xlsx`, `docx`,
`pptx`, `pdf`, `barcode` — so a consumer who wants spreadsheets does not compile
a PDF stack. The default build is unchanged: `default = []`, every document
dependency optional, nothing paid for by a consumer who does not ask.

### What the agent gains

Twelve tools, offered beside `read_file` and `write_file` when the feature is on:
`xlsx_read`, `xlsx_sheets`, `xlsx_write`, `xlsx_set_cell`, `docx_read`,
`docx_write`, `pptx_read`, `pdf_read`, `pdf_write`, `pdf_watermark`,
`pdf_fill_form`, `barcode_decode`.

They are **built-ins, not registered `Tool` implementations**, and that is
deliberate. A registered tool is authorised once by an `Act::Exec` check on its
name, and v0.9 states plainly that the policy governs whether a tool is *called*
and not what it does once running. A document tool's whole job is reading and
writing files in the user's workspace, so dispatch gates each call on `Act::Read`
or `Act::Write` against the path the model named: `deny_write("secrets/*")` stops
`xlsx_set_cell` in the same place and for the same reason it stops `write_file`.
Every byte in and out goes through `Workspace::read_bytes` / `write_bytes` —
nothing here opens a path itself.

### Verifying a document

```rust
use io_harness::{TaskContract, Verification};

let contract = TaskContract::workspace(
    "Add the Q3 revenue row to the summary workbook.",
    "/path/to/repo",
    Verification::DocumentContains {
        file: "reports/summary.xlsx".into(),
        needle: "Q3".into(),
    },
);
```

It gates on the document's *extracted* text, and it exists because the criterion
you would otherwise reach for cannot work here.
`Verification::WorkspaceFileContains` reads with
`read_to_string(..).unwrap_or_default()`; every format above is a binary
container, so that read yields the empty string and the criterion reports "does
not contain" for every document — including one whose visible text plainly does
contain the needle. That is a silent, permanent false FAIL, not a false pass: a
gate that can never say yes, ending the run as `StepCapReached` so it looks like
an agent that could not do the work.

The reader is chosen by extension — `.xlsx`, `.docx`, `.pptx`, `.pdf`. Anything
else is an error, not a fallback to matching raw bytes. The variant exists in
every build; one compiled without the matching feature returns a typed
`Error::Config` saying so rather than vanishing from the enum.

### The limits, stated plainly

**Word is generate-and-read, with no in-place edit.** `docx-rs`'s reader models
the OOXML it knows and drops what it does not, so read-then-write is a lossy
rewrite rather than an edit — on a user's real document it silently deletes the
comment thread, content control, field or vendor extension the reader could not
name. The capability is not claimed rather than claimed and unsafe.

**PowerPoint is read-only and deliberately has no writer.** `pptx_read` pulls the
slide text out; there is no `pptx_write`.

**PDF text extraction is best-effort on reading order.** A PDF stores placed
glyphs, not a document: columns can interleave, tables lose their shape, and a
scanned page returns empty text rather than an error. Treat the output as
evidence, not as the document. `pdf-extract` is also written to panic on input it
does not understand, so the call catches the unwind and reports a typed error —
which does nothing under a `panic = "abort"` profile.

**The spreadsheet preserving-edit is preservation in practice, not a guarantee.**
`xlsx_set_cell` is a real `umya-spreadsheet` round trip: what the library models
it keeps, what it does not model it can drop or normalise on the way out. It is
not guaranteed for chart-, drawing-, pivot- or macro-heavy workbooks — that is
the case to check before trusting an edit, not the case to assume.

**OCR and PowerPoint authoring were cut from the roadmap, not deferred.** Barcode
generation, the in-place Word edit and document delete were on the old capability
line and are not here either; none of the three is on a later release. See
[CHANGELOG.md](CHANGELOG.md) for what each cut cost and why it was made.

## Part of initorigin

`IO Harness` is one of the [initorigin](https://github.com/initorigin) products:

| Repo | What it is |
| --- | --- |
| [io-harness](https://github.com/initorigin/io-harness) | The Rust agent harness (the center product) |
| [io-eval](https://github.com/initorigin/io-eval) | Benchmark harness for io-harness |
| [io-cli](https://github.com/initorigin/io-cli) | Terminal app on io-harness |
| [io-studio](https://github.com/initorigin/io-studio) | Desktop coding studio on io-harness |
| [website](https://github.com/initorigin/website) | Marketing site, docs, and blog |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Work branches from `develop`, lands via
PR, and every user-facing change updates [CHANGELOG.md](CHANGELOG.md). Releases
follow [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md).

## Security

Report vulnerabilities per [SECURITY.md](SECURITY.md).

## License

Apache-2.0. Copyright 2026 Aakash Pawar (InitOrigin). See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
