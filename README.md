<div align="center">

<img src="https://raw.githubusercontent.com/initorigin/io-harness/main/assets/initorigin-logo.png" alt="InitOrigin" width="112" height="112">

# IO Harness

**An embeddable agent runtime for Rust. Any task, any provider, in your process —
with a permission boundary, a sandbox, and a durable trace you own.**

[![crates.io](https://img.shields.io/crates/v/io-harness.svg)](https://crates.io/crates/io-harness)
[![downloads](https://img.shields.io/crates/d/io-harness.svg)](https://crates.io/crates/io-harness)
[![docs.rs](https://img.shields.io/docsrs/io-harness)](https://docs.rs/io-harness)
[![CI](https://github.com/initorigin/io-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/initorigin/io-harness/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/io-harness)](Cargo.toml)
[![License](https://img.shields.io/crates/l/io-harness.svg)](LICENSE)

</div>

You hand it a contract: the task, the workspace it may touch, and what it may
read, write, run and dial. It runs the loop — observe, reason, act, check, stop —
and hands back an outcome, with every step, refusal and budget draw in a SQLite
trace you can read afterwards.

## Contents

- [Quickstart](#quickstart)
- [Who it is for](#who-it-is-for)
- [What you get](#what-you-get)
- [How it compares](#how-it-compares)
- [Measured cost](#measured-cost)
- [Capabilities in depth](#capabilities-in-depth)
- [Guides](#guides)
- [Supported files and languages](#supported-files-and-languages)
- [Feature flags](#feature-flags)
- [Platform support](#platform-support)
- [Stability](#stability)
- [Contributing](#contributing)
- [Security](#security)
- [License](#license)

## Quickstart

```toml
[dependencies]
io-harness = "0.80"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

**MSRV: Rust 1.95** or later. The whole surface below is on that floor. The
default build compiles no optional dependency at all, and the dependency tree is
held deliberately small.

```rust,no_run
use io_harness::{ApproveAll, OpenRouter, Policy, Session, Store};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
    let store = Store::open("runs.db")?;

    // src/ is writable, secrets/ is refused outright and never reaches a human,
    // and the agent may run the test runner and nothing that publishes.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("src/*")
        .deny_read("secrets/*")
        .allow_exec("npm*")
        .deny_exec("npm publish*");

    // A durable conversation about one workspace. Every turn is a run of its
    // own: its own steps, its own budgets, its own boundary, its own trace.
    let mut session = Session::open(&store, "/path/to/repo")?;

    let first = session
        .turn("the test suite is failing; why?", &provider, &store, &policy, &ApproveAll)
        .await?;
    println!("{}", first.reply.unwrap_or_default());

    // The next turn reads the previous ones: the conversation *is* the context.
    session
        .turn("fix it, then run the suite", &provider, &store, &policy, &ApproveAll)
        .await?;

    // Keep the id. It is all a later process needs to pick this up again.
    println!("session {}", session.id());
    Ok(())
}
```

For unattended, one-shot work there is `run_with`: the same loop, the same
boundary and the same trace, driven by a `TaskContract` instead of a
conversation, and gated on a criterion the project itself decides.

```rust,no_run
use io_harness::{run_with, ApproveAll, TaskContract, Verification};

let contract = TaskContract::workspace(
    "the test suite is failing; find out why and fix it",
    "/path/to/repo",
)
// The project's own command decides whether the work is done. Nothing here is
// Rust-specific — `go test ./...` or `pytest` reads the same. Verification is
// opt-in: without this the contract has no gate at all.
.with_verification(Verification::Command {
    argv: vec!["npm".into(), "test".into()],
    expect_exit: 0,
});

let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
println!("{:?}", result.outcome);
```

## Who it is for

You are writing a Rust program that needs an agent inside it — a CLI, a service,
a desktop app, a test harness — and you want the loop, the boundary and the trace
to be yours rather than a vendor's. The agent can run the project's own
toolchain, so the language the *project* is written in is not the harness's
business; only the language the *embedder* is written in is.

It is a library and nothing else. There is no binary to install, no daemon, no
UI, no account, and no telemetry. The browser dance for a subscription login, the
terminal interface, the keybindings and the notification tray all belong to the
program that embeds this crate, and none of them is in scope here. If what you
want is an agent to *use* rather than one to *build with*, this is the wrong
layer.

## What you get

| Capability | What it gives you | Depth |
| --- | --- | --- |
| **Orchestration loop** | Observe, reason, act, check, stop — driven by a `TaskContract` or by a conversation | [sessions](docs/guide/sessions.md) |
| **Permission boundary** | Layered, deny-first rules over read, write, exec and network; every refusal in the trace, attributed to the rule and the layer | [permissions](docs/guide/permissions.md) |
| **Human approval** | Approve, deny, or defer past the end of the process and resume on a decision made later | [permissions](docs/guide/permissions.md) |
| **Plan gate** | The agent reads, writes nothing, and exits only through an ordered plan you approve, correct or cancel | [agency](docs/guide/agency.md) |
| **Command execution** | The project's own build, test, lint and package-manager commands, checked on the whole argv, never a shell string | [command execution](docs/guide/command-execution.md) |
| **Containment** | Commands run inside the sandbox backend the host offers, writing to the workspace and nowhere else, with per-host egress | [sandbox](docs/guide/sandbox.md) |
| **Execution sandbox** | Model-produced code in an ephemeral workdir with caps that kill rather than throttle, network denied by default | [sandbox](docs/guide/sandbox.md) |
| **Programs instead of call chains** | Behind `codeact`: one contained Python program in place of six round trips, where every act it takes re-enters the same dispatch, policy, gate and trace a tool call does | [CodeAct](docs/guide/codeact.md) |
| **Verification** | Any language's own test command, a second model against a rubric, or no gate at all | [verification](docs/guide/verification.md) |
| **Structured output** | `OutputSchema` demands a shape of a run's final answer — a closed, stated JSON Schema subset, validated here whatever the vendor did with it | [what you may depend on](docs/CONTRACT.md) |
| **Withholding a tool for a turn** | `ToolMask` names the tools one turn may not call, without moving the catalogue the vendor has already cached | [tools and skills](docs/guide/tools-and-skills.md) |
| **Budgets** | Steps, wall-clock and token spend, from one ledger a whole tree of agents shares and no child can raise | [composition](docs/guide/composition.md) |
| **Durable runs** | Trace, budget draw and checkpoint commit together after every completed step; a crash resumes the whole tree | [durable runs](docs/guide/durable-runs.md) |
| **Indeterminate effects pause** | A run killed mid-call to something the harness cannot inspect — a charge, a deployment, an MCP server — resumes to an operator's decision instead of making the call again | [durable runs](docs/guide/durable-runs.md) |
| **One driver per run** | Every run and resume takes a lease on the run it drives; a second driver is refused by name before it can interleave its steps into the trace | [durable runs](docs/guide/durable-runs.md) |
| **Undo** | Per-file restore points and a whole-run rewind that leaves the ledger and the trace intact | [durable runs](docs/guide/durable-runs.md) |
| **Agent composition** | Nested sub-agents over a shared workspace, inherit-and-narrow, with refusing and queueing caps | [composition](docs/guide/composition.md) |
| **The mailbox** | Every agent in a tree has an address; siblings send findings and wait on each other, exactly once, replayed identically | [mailbox](docs/guide/mailbox.md) |
| **Providers** | OpenRouter, Anthropic and OpenAI natively, one `Compatible` provider for any OpenAI-shaped endpoint, 21 vendor presets, fallback between them | [providers](docs/guide/providers.md) |
| **Context and memory** | Per-turn assembly to a stated budget share, compaction, invalidation, and durable memory kept by evidence | [context and memory](docs/guide/context-and-memory.md) |
| **Sessions** | Durable, branchable conversations with token streaming, mid-turn steering and interruption | [sessions](docs/guide/sessions.md) |
| **Configuration** | One `io.toml` over four scopes, projected onto the typed API, where a file inside the workspace may narrow and never widen | [configuration](docs/guide/configuration.md) |
| **Hooks and bundles** | An audit log, notification, formatter or blocking check from config; a directory that contributes skills, agents, servers, the executables it ships and deny rules at once | [hooks](docs/guide/hooks.md), [bundles](docs/guide/plugins.md) |
| **Extensibility** | The `Tool` trait in-process, MCP over stdio and streamable HTTP, and markdown skills whose `read_skill` opens the references beside them under the same policy and never anything outside | [tools and skills](docs/guide/tools-and-skills.md) |
| **Serving MCP** | Behind `mcp-server`: another harness calls **these** tools on stdio, through this crate's policy, gate, journal and trace rather than its own | [MCP and network egress](docs/guide/mcp-and-network.md) |
| **Accounting** | Input, output, cache-read, cache-write and reasoning tokens per call, with latency and TTFT; cost derived on read from a price table you own | [accounting](docs/guide/accounting.md) |
| **Observability** | An observer called as the run happens, a recorded provider that replays a case identically, and — behind `otel` — the same run exported as OpenTelemetry spans to any OTLP collector | [observability](docs/guide/observability.md) |
| **Retention** | What the store holds, deleting a session whole, sweeping to a date, archiving the words while keeping the numbers | [retention](docs/guide/retention.md) |
| **Reach** | A browser under the policy, LSP navigation, provider-executed web search, documents, images and fixed-argv git | [browser](docs/guide/browser.md), [web](docs/guide/web.md) |

## How it compares

Two different questions, so two tables. Every cell was read from that project's
own current documentation or repository on **2026-09-05**, and a cell reads *not
documented* when the project does not state the property — which is not the same
as the property being absent. Check the sources; they move.

**Against the agent harnesses you would otherwise run.** These are CLIs and
applications, not libraries, so the comparison is about the guarantees each one
gives an operator, not about what an embedder can build on.

| | Traced permission boundary | Durable per-step resume | Execution sandbox | Tree-wide spend ceiling |
| --- | --- | --- | --- | --- |
| **io-harness** | Deny-first layered rules; every decision and refusal recorded, attributed to the rule and layer that produced it | Trace, budget and checkpoint commit in one transaction after each completed step; a crashed tree resumes without re-running steps or double-charging | macOS, Linux and Windows backends over a portable floor; what actually applied is recorded | One ledger shared by the whole tree, which a spawned contract cannot raise |
| [Claude Code](https://code.claude.com/docs/en/permission-modes) | Modes and rules in layered settings files, plus a classifier gate in auto mode; denials are recorded in `/permissions` under *Recently denied* and reach a `PermissionDenied` hook | Per **user prompt**, over files and conversation — [not per step, and bash-made changes are not tracked](https://code.claude.com/docs/en/checkpointing) | [Yes — Seatbelt on macOS, packages on Linux/WSL2, writes confined to cwd and temp, egress through a proxy with an allow-list](https://code.claude.com/docs/en/sandboxing); not available on native Windows, and falls back to unsandboxed unless `failIfUnavailable` | [For headless runs — `--max-budget-usd` caps spend in print mode and a subagent's spend counts against it; whether a child may raise it is not documented](https://code.claude.com/docs/en/costs) |
| [Codex CLI](https://learn.chatgpt.com/docs/sandboxing) | `approval_policy` of `untrusted`, `on-request`, `never`, or a `granular` table of per-surface rules, separate from the sandbox mode; refusal audit not documented | not documented — session resume and fork are, and `sqlite_home` names a SQLite database for "resumable runtime state", but nothing states per-step or crash durability | Yes — Seatbelt on macOS, `bubblewrap` on Linux/WSL2, Windows Sandbox or WSL2; `sandbox_mode` is `read-only`, `workspace-write` or `danger-full-access` | not documented |
| [opencode](https://opencode.ai/docs/permissions/) | Per-tool wildcard patterns; auditing of denials not addressed | not documented — [sub-agents and a max-step control, with no persistence between steps described](https://opencode.ai/docs/agents/) | not documented | not documented |
| [Goose](https://goose-docs.ai/docs/guides/managing-tools/tool-permissions/) | Four modes with per-tool overrides, decisions persisted; Smart Approval is an LLM classifier rather than a rule | not documented | [None — the macOS seatbelt sandbox was removed, and the server process that executes tools "runs with the same permissions as your user account and is not sandboxed at the OS level"](https://goose-docs.ai/blog/2026/02/23/goose-v1-25-0/) | not documented |
| [pi](https://pi.dev/docs/latest) | None by design — "Pi does not include a built-in sandbox", and the documented answer is to run pi inside a contained environment | not documented | External: the container you run it in | not documented |

**Against the Rust libraries you would otherwise embed.** This is the closer
comparison, because it is about what arrives with the crate rather than what you
write yourself.

| | Agent loop | Policy boundary over tool calls | OS sandbox | Durable crash-resume | Per-call token and cost accounting |
| --- | --- | --- | --- | --- | --- |
| **io-harness** | Yes | Yes | Yes | Yes | Yes, stored as raw counts |
| [rig](https://github.com/0xPlaygrounds/rig) | Yes | not documented | not documented | Partial — a serializable step machine a run "can be suspended between steps and resumed in another process"; no storage layer or crash recovery | Partial — `Usage` per request, with the cache and reasoning splits; no cost |
| [swiftide](https://github.com/bosun-ai/swiftide) | Yes | not documented | not documented; a docker executor is a separate integration | Partial — pause, resume and reset, for human approval, external callbacks or persisted state; no durability across crashes stated | Partial — `Usage` on each chat-completion response; no cost |
| [langchain-rust](https://github.com/Abraxas-365/langchain-rust) | Yes | not documented | not documented | not documented | Partial — `TokenUsage` per call; no cost |

The last column of the first table is where this crate is still alone, and it is
narrower than it was: one of these now caps spend for a headless run and counts a
subagent's spend against that cap, which is most of the property. What is still
unmatched is the shape — a ledger the whole tree draws from at every step, that a
spawned contract cannot raise, on an interactive run as much as an unattended one.
The rest is a matter of degree — several of these have real sandboxes and real
permission systems, and saying otherwise would be worth less than saying nothing.
The degree moves in both directions: one of them removed its sandbox between this
table's last two readings.

## Measured cost

Numbers this repository has actually measured, each on a named machine, each with
its method stated. **None of them is a gate**: no test asserts a duration, because
a duration asserted on a CI runner is a flake waiting to be written. Acceptance
criteria assert structure; these record timing.

| What | Result | Machine |
| --- | --- | --- |
| Removing one session of 1,000 steps from a store | 5.832 ms, and a sweep of ten sessions is ~4–5× cheaper than ten deletions | Apple M1, release |
| Ranking a turn's memory recall, at the default 64 entries | 1.106 ms per turn — linear in entries, flat in the recall table | Apple M1, release |
| A capped memory write that evicts, at the default 64 entries | 0.965 ms; ~73 ms at 4,096 entries, which is why the cap is the operator's | Apple M1, release |
| Converting an image at the door (BMP/TIFF/TGA/PNM → PNG) | 1.75–2.55 ms on a 512×512 image; a pass-through is 0.01–0.14 ms | Apple M1, release |
| Starting a read before the completion ends | 303.8 ms saved on a 400 ms window and a 300 ms read — bounded above by `min(window, read)` | Apple M1, release |

[docs/MEASUREMENTS.md](docs/MEASUREMENTS.md) holds the method for each, the exact
command to reproduce it, and what each number does *not* say — including a defect
one of these measurements found that the test suite could not.

## Capabilities in depth

### The loop and the workspace

A `TaskContract` names the goal, the subject, and — optionally — the criterion.
Workspace mode gives the agent `grep`, `find`, `read_file`, `write_file`,
`edit_file` and `patch_file` across a repository root. A read returns the file,
the range of lines it asked for, or a refusal saying why — never a shortened file
wearing the shape of a whole one, and never an empty string for a binary.
Single-file mode edits one file.

A contract can also demand the *shape* of what the run ends with.
`TaskContract::with_output_schema` takes an `OutputSchema` — a JSON Schema over a
closed, stated keyword subset, walked once at declaration time so a keyword this
crate does not implement is refused by name and by pointer rather than passed
through and silently unchecked. An `output_schema` key in `io.toml` is the same
declaration made by an operator, refused the same way while they are still reading
their own file. On the OpenAI-shaped wire it is sent as `response_format`; the
Anthropic Messages API has no counterpart and is deliberately sent none. Either
way the check happens here: a final answer that does not validate is re-prompted
with the validation error as an ordinary observation, bounded by the run's
retries, and exhausting them ends the run as `SchemaUnsatisfied` rather than as a
finish over malformed output. What the subset holds and what it refuses by name is
in [docs/CONTRACT.md](docs/CONTRACT.md).

A change touching four places in one file is one `patch_file` call taking a
unified diff — applied as a unit or not at all, so a patch that no longer fits is
refused with the hunk named rather than half written. In a project whose ecosystem
the harness recognises, a successful write runs that project's own type-check
command and attaches what it found, so a mistake arrives with the edit rather than
twenty steps later; `check` asks the same question *before* a write, and is
refused by the same policy that refuses `exec`.

Every write records the change as a unified diff of the whole file, so a trace can
show what a step changed and not only how many lines it changed. `Store::patch`
renders a run's whole change as a step-ordered patch series for review, and
`rewind_step` reverse-applies one step's hunks — undo at a granularity finer than
"throw the run away", walking backwards from the newest step.

### The boundary

Layered, deny-first rules decide what the agent may read, write, execute, and
connect to. Anything marked *ask* goes to an approver that can approve, deny, or
defer past the end of the process and resume on a human decision later. Every
refusal and decision is in the trace, attributed to the rule and the layer that
produced it.

A turn can also withhold a tool without changing what the model is shown it has.
`TaskContract::with_tool_mask` takes a `ToolMask` naming the tools that turn may
not call: the definitions sent are byte-identical to an unmasked run's, because
the tool array sits ahead of the cache breakpoint at the end of the system block
and any byte changed in it invalidates that entry and everything after it. The
withheld names are stated past both breakpoints instead, so a mask that changes
between turns costs no cache entry, and a call to a masked tool is refused at both
places a call can begin — a batched read-only call does not route through the
dispatch path the others take. See
[tools and skills](docs/guide/tools-and-skills.md).

`TaskContract::with_plan_gate` opens a run in a planning phase: the agent reads,
writes nothing, and the only exit is an ordered plan — each step optionally naming
the agent that owns it — which a `PlanGate` approves, corrects or cancels. With no
in-process answer the plan persists and the run stops, so the decision can be made
after the process exits. Beside it the agent can ask a question about intent
rather than guessing, run under a named roster of agent definitions, and be told
to think harder or less through an effort level each vendor projects into its own
shape.

### Running commands

The agent runs the project's own build, tests, linter or package manager through
an `exec` tool taking a fixed argv and never a shell string. Every call is checked
against the policy on the program *and* on the whole argv, so
`allow_exec("cargo test*")` beside `deny_exec("cargo publish*")` means what it
reads. A whole command line — pipelines, redirects, `&&` — is `shell`, parsed in
this crate rather than by a shell and checked stage by stage before the first
process starts. `shell_start`, `shell_poll` and `shell_kill` are the same line
with a longer life, for a dev server or a log tail that has to outlive the step
that started it.

The harness ships a table of what each ecosystem's commands conventionally are, so
the agent does not spend turns discovering that this is a pnpm workspace.

Behind the `codeact` feature, off unless you ask for it, a chain of calls can be
one program instead. `run_program` takes a complete Python program as its
argument — source, not a command line — and the tools the run already has are
ordinary functions inside it, so a loop over twelve files is control flow the
interpreter runs rather than twelve round trips the model has to stay coherent
across. Every act the program takes re-enters the same `dispatch` a model's own
call takes, so the policy gate decides it, the trace records it and the journal
pairs it: a program is a shorter way of asking, not a wider door. The interpreter
is the host's — named in `[codeact]` or resolved from a documented list and
version-probed — and nothing is downloaded, ever; a host without a usable one runs
the turn exactly as it did before, with the fallback recorded as an event rather
than left to be inferred. See [CodeAct](docs/guide/codeact.md).

### Containment

Every command `exec` and the foreground `shell` start runs inside the sandbox
backend this host offers, writing to the workspace root, the system temporary
directory and the detected toolchain's own caches — and nowhere else. `ExecMode`
names the three grants: `ReadOnly`, `WorkspaceWrite` (the default) and
`FullAccess`, which is `TaskContract::with_full_access()` — a sentence in your
source rather than a field you never set, because it is the widest thing the crate
grants. The default carries **no resource cap**: containment says where a command
may write, and `with_contained_exec(SandboxConfig::new())` is how you add the
ceilings.

The **workspace root** stays the working directory — nothing is copied in and
nothing is discarded — so an incremental build survives between commands, and the
toolchain's own cache being writable is what lets a default-contained `cargo` or
`npm` run at all. Backgrounded `shell_start` handles and the git built-ins are
contained like everything else.

What each platform actually enforces differs, and the difference is not cosmetic —
see [platform support](#platform-support). A host that can deliver none of it
falls back to the portable floor and **records the floor**, so a run contained less
than you asked for is legible afterwards: in the trace, in `EventKind::Contained`,
and in the agent's own prompt. `select().backend()` answers before the run, and
the trace records what actually applied.

A run whose policy names hosts reaches those hosts and no others, through a proxy
the run owns. What that proxy is worth differs per backend and is stated per
backend in [docs/CONTRACT.md](docs/CONTRACT.md) rather than discovered.

Underneath every rule an operator writes there is an **address floor**:
loopback, link-local, cloud-metadata, carrier-grade-NAT, unique-local and RFC 1918
addresses are refused whatever the policy says, because a rule is a hostname glob
and nobody thinks to deny their own admin ports.
`IO_HARNESS_ALLOW_LOCAL_ADDRESSES=1` is the only way off it, and it is an
environment variable rather than an `io.toml` key on purpose — a key that widens is
one a cloned repository could set. A run against a local model runtime needs it;
see [MCP and network egress](docs/guide/mcp-and-network.md).

Beside all of that, model-produced code runs in an ephemeral **execution sandbox**
with an isolated workdir, resource caps that kill rather than throttle, and
network denied by default — and the same machinery is what a contained `exec`
reaches, so the project's own commands run behind it rather than beside it.

### Verification

A criterion can be the project's own test command in whatever language it is
written, or nothing at all when the task has no checkable criterion — "work out
why the deploy fails" is a run, not a gate. `Verification::Review` is the other
direction: a model reads what the run wrote against a rubric you set and returns a
verdict with its reasons, and it **refuses to run on the model that wrote the
change**. An exit status cannot catch a change that compiles, passes and is still
wrong; a review can, and is not a proof — the two compose. What a passing gate
does and does not prove is stated exactly in the
[verification guide](docs/guide/verification.md); it is narrower than it reads.

Every gate evaluation is recorded as `Passed`, `Failed` or `Errored` — the last
being a criterion that could not run at all, which is a different problem from one
that ran and said no. `retry_gate` re-runs *only* the criterion, against the
workspace the run left, so a transport failure on the gate does not cost the forty
steps that produced the work.

### Budgets, routing and caching

Steps, wall-clock time, and token spend are capped. A tree of agents draws from
one shared ledger no spawned contract can raise.

`Routing` escalates to a stronger model after repeated gate failures, downshifts
while the change is small, and — the rule an unattended job needs — refuses to
start at all when the primary provider reports it is unreachable, rather than
quietly spending the night on a fallback nobody chose.

The stable prefix is cached where the vendor sells it. One cache breakpoint sits
at the end of the system block, which on the Anthropic wire covers the tool
schemas and the instructions together, and OpenRouter carries the same marker. A
second sits at the end of the frozen transcript prefix once the run has compacted —
everything from the top of the prompt through the written summary stops changing,
so it can be cached, while the observations after it are still rewritten every turn
and are still never marked. The reads land in the accounting rows beside every
other token. The crate never asks a vendor to cache a prefix it has not already
sent once, so a marker it places cannot be billed as a cache write on a prefix
that then moves.

### Agents, and agents talking

A root run can spawn contained sub-agents over a shared workspace, nested, many at
once. Two caps, different in kind: `max_total_agents` refuses the spawn that
crosses it, `max_concurrent_agents` queues it — so a hundred-agent task runs
sixteen at a time until it is done rather than failing at its seventeenth child.
The queue is durable, counted per tier and reported to an `Observer`, and a child
that only ever waited is never charged. A child inherits its parent's policy and
can only narrow it — never grant itself what the parent lacks.

`spawn_agent` takes `wait` and `background_after_secs`: a parent can carry on while
a sub-agent works and read its report at a later step, and a child that outlasts a
stated wall clock moves to the background instead of holding the tree. It is not
cancelled — a parent that stops waiting is not a parent that stops the work — and a
tree still never returns while a child it started is running. A child comes back
with what it cost and what it concluded, not only a discriminant and a step count.

Every agent in a tree also has an **address**. `spawn_agent` takes an `as`, unique
within the tree, and one is derived when it is omitted. That address names **one**
agent, unlike a configured agent's name, which is a role several may share.
`send_message` tells a named agent something; `read_messages` returns what was sent
to you, oldest first, exactly once, and may block for a stated number of seconds.
Every wait is bounded — `[run] max_wait_secs`, 30 seconds by default, lowerable by
a project scope and never raisable — because an agent that blocks holds its
concurrency slot and the sibling that would answer it may be queued behind that
slot. A finishing agent posts one short line to its parent, so waiting for a named
child and waiting for a message are the same call. Messages are rows: a resumed
tree reads the same ones in the same order, once. Nothing is delivered unbidden, an
address reaches inside its own tree and nowhere else, and a message grants nothing
— the boundary is still the policy.

### Durability, undo and two processes

After every completed step the trace, the budget draw, and a checkpoint commit in
one transaction. A crash resumes the whole tree where it stopped: completed steps
are not re-run, the budget is not double-charged, and an irreversible action
already taken is not taken twice.

Every write records what was there before the run first touched that file, in the
store rather than in memory, so `rewind(&workspace, &store, run_id, path)` puts a
file back — including deleting one the run created — after a crash and a resume.
One restore point per file per run, and it says plainly when it has none rather
than guessing. `rewind_run` widens that to the whole run: the files, the memory
entries it wrote, and the spawn backlog it left queued, in one call — while the
steps, the events and the ledger stay exactly as they were, because the spend
happened and an undo that erased them would make the trace lie.

A run is driven under a **lease**. Every `run_*` and `resume_*` takes one on the
run it is about to drive and gives it back on the way out, whichever way it
leaves; no entry point grew a parameter. While a live owner holds it, a second
driver is refused with `Error::Conflict`, which names the holder and when its
lease lapses — before it has driven a step. Unleased, both processes drive: their
steps interleave into one trace under one run id that describes a run neither of
them performed, with no error and nothing in the store afterwards to tell it from
a real one.

A crash is not a lock. A dead owner's lease is takeable at once, so `kill -9` and
resume stays immediate on unix and on Windows alike — `kill(pid, 0)` there,
`OpenProcess` plus `GetExitCodeProcess` here, neither a dependency the crate did
not already have. The ttl — `TaskContract::lease_ttl`, `DEFAULT_LEASE_TTL`
by default — governs only what liveness cannot answer: a recycled pid, an owner id
with no readable pid, a third kind of platform, and a Windows process that exited
with code 259, which is `STILL_ACTIVE` and so reads as running. A takeover raises the generation, and the previous owner's next
durable commit is refused inside the transaction that would have written it, so
it writes neither a step row nor a checkpoint event. A session head is not leased,
because a conversation is meant to be branched: it advances by compare-and-swap
instead, and the losing turn is reported with `Error::Conflict` and its row left
intact rather than silently dropped.

A run does not belong to the process that started it. `Broadcast` wraps whatever
`Observer` you already have and writes each event to the store as it passes, so a
second process can `Attach` to a run that is *still going*, read the same events,
see whether it is parked on an approval, a question or an unreviewed plan, and
answer it — without killing it and without resuming it. The first answer wins and
the loser is told, because the write is a compare-and-swap on the row the run
already reads. An attaching process reads and decides: there is no method on it
that starts, resumes or steps a run, so killing the watcher changes nothing and
killing the owner leaves a resumable run — its lease is takeable as soon as that
process is gone, so a resume is refused only while a live owner still holds one.

### Providers

OpenRouter, Anthropic and OpenAI behind one trait, over the crate's own HTTP+SSE
client. Beside them one `Compatible` provider reaches any OpenAI-shaped endpoint
from a base URL, an auth style, a key and a model, and 21 vendors of that shape
have a named constructor so nobody types a URL: 13 hosted — Groq, xAI, Mistral,
DeepSeek, Together, Fireworks, Cerebras, Perplexity, Gemini through its
compatibility endpoint, Moonshot, Zhipu, Qwen, MiniMax — and 8 local runtimes —
Ollama, llama.cpp, vLLM, LM Studio, LocalAI, Jan, SGLang, KoboldCpp — where a model
on the developer's own laptop costs nothing to run. `Provider::models()` reports
what a provider can run and what it costs. A provider that is down or rate-limited
falls back to the next configured one; failures are classified so a caller can tell
a retryable transport error from a terminal one.

A request carries an ordered transcript — user turns, assistant turns holding the
calls the model made, and the results answering them — and each wire maps it onto
that vendor's own block types: `tool_use`/`tool_result` on Anthropic,
`tool_calls` plus `role: "tool"` messages on the OpenAI wire. The model is asked a
question it was trained to answer.

A completion arrives in pieces, and a tool call inside it is complete long before
the message is. When a provider reports a finished call while it is still
streaming, the harness starts the read-only ones then — so a turn that reads three
files and then answers gets those reads back sooner by the width of everything the
model said afterwards. It is bounded by what can be undone: only the completion's
*leading* run of read-only calls, so a read never sees the state before a write
that has not run yet; only what the policy allows outright, so no approver is ever
asked about a turn the model may still abandon; and a result is used only if the
finished completion asks for that same call with the same arguments — otherwise the
work is thrown away with nothing recorded. The trace, the observations and the
events are identical either way; `EventKind::Speculated` reports what was started
and what was discarded, because unlike ordinary concurrency this trade can lose.

### Context and memory

Each turn is assembled to fit a stated share of the token budget: superseded
observations are compacted, and an observation a later write invalidated is re-read
rather than trusted.

Beneath a fold there is a gentler rung. `TaskContract::with_collapse` takes a
`Collapse`, and an entry that will not fit the turn's budget is then carried
shortened rather than replaced by a one-line stub — at no provider call and no
stored summary, leaving the ledger exactly as long as it was, so turning it off
assembles those entries whole again, which a fold cannot do. A shortened entry
keeps its kind and its target, so the stale-read rules still see it. It is off
unless configured, and `fold` stays the last rung and the default trigger; the
[context and memory guide](docs/guide/context-and-memory.md) has the ordering.

Durable memory survives between runs — as a fact or a decision, pinnable so a run
cannot overwrite a correction, with a per-run record of which entries it actually
drew on. It is kept for what it is worth rather than for how recently it was
written: at a cap the store drops the entry the fewest separate runs have carried,
not the oldest. When a store outgrows its share of a turn, the notes that survive
the fit are the ones whose words the turn is about — the run's goal, and every path
a tool has already named — then the ones the most separate runs have carried; the
block is still *printed* in the store's own order, so a store that fits its share
assembles the same bytes it did before. A note that restates one already held under
a different key is reported back at the moment it is written, rather than left for a
later run to trip over. An agent that learns something wrong can `forget` it, and a
fact true of every repository can be kept once, above the workspace, where a
workspace's own note still overrides it.

### Conversations

`Session::open(store, root)` holds a conversation instead of firing one task: turns
that read the turns before them, text streamed to an observer as the model produces
it, a mid-turn steer or an interrupt honoured at the next step boundary, and a tree
an operator can branch from any earlier turn. A turn is a run, so every turn is
budgeted, policy-bounded, checkpointed and resumable.

`Session::turn_contained` takes a `Containment` and lets the agent answering that
turn decompose the work into contained sub-agents — under the session's own policy,
one shared ledger for the turn, and the observer the operator is already reading. It
stays one turn in the conversation whatever it spawned, and the turn entry points
that predate it never offer the spawn tool. `Session::turn_contained_bounded` takes
a `TaskContract` beside the `Containment`, so a turn that may fan out also carries a
plan gate, registered tools, a budget or a verification gate — and
`Harness::turn_contained_with` is the same with the host bound once.
`Session::turn_bounded_steered` and `Session::turn_contained_bounded_steered` add
the operator's `SteerInbox` to that same call, so a turn carrying a contract can be
corrected while it runs instead of only before it starts. The correction reaches
the root of a fan-out and never a child, which is never steerable by an operator it
has not spoken to.

And not every turn is work. A turn's own first completion decides what the turn
was: stopped on text, it closes as `TurnKind::Reply` with no step, no gate, no
checkpoint and no plan gate; carrying a tool call, the loop continues from that same
completion. So `hi` costs one completion and stages nothing, while `hi, the login
page is broken` runs — decided by the model, at no extra call, with no list of
greetings anywhere in this crate or in yours.

### Configuration, hooks and bundles

`Config::discover(root)` reads one `io.toml` across four scopes — the crate's
defaults, a user file, a committed project file, and a gitignored local one — and
projects it onto the typed API: a `Policy`, a `SandboxConfig`, the run budgets, the
toolchain commands, a price table, MCP servers, and **which provider and model to
run with what behind it**, so an embedder reads a `ProviderSpec` rather than writing
provider-selection code. `[app]` is a section the crate stores and never validates,
so the programs built on it keep their own settings in the same file;
`[profile.<name>]` overlays a named set of choices; `[instructions]` discovers the
`AGENTS.md` a repository already carries and puts it in the system block as the
repository's own guidance, with `files = []` as the opt-out. `${env:...}`,
`${file:...}` and `${cmd:...}` keep a credential out of the file, an unknown key is
an error rather than a shrug, and nothing is loaded implicitly: the caller reads the
file, before the run, once. A file **inside the workspace** may narrow the boundary
and may never widen it: the twelve keys that would remove containment, a
`[[policy.layers]]` rule whose effect is anything but `deny`, and every section
that names a program to run, names an endpoint a credential is sent to, or opens a
route out of the boundary the policy does not gate — `[[hook]]`, `[browser]`,
`[[provider]]`, `[[mcp]]`, `[[lsp]]`, `[web]` — are refused in the committed
`io.toml` *and* in the gitignored `io.local.toml`, which sits at the workspace root
where the agent can write it. The user-scope file, outside every workspace, is the
one that can still widen, and every refusal names it.

A `[[hook]]` table names the events it wants and one thing to do with them: a path to
append the event stream to is an audit log, an argv to run is a notification or a
formatter, and that argv with `on_failure = "cancel"` is a local policy check that
ends the run. `Config::hooks()` returns an `Observer` the caller installs like any
other, so no run loop changed — and the whole array is refused in both workspace
files, for the same reason `${cmd:...}` is refused in the committed one. It lives in
the user-scope file.

A **capability bundle** is a directory with a `plugin.toml` that contributes skills,
prompt templates, an agent roster, MCP servers, hooks, the executables it ships and
deny-only policy at once,
named by a `[[plugin]]` entry in any scope. Every contributed name is namespaced
`<plugin>__<name>` as it loads, so a refusal, a tool call and a child's spend already
say which bundle introduced them — with no new table. A bundle declared from any file
inside the workspace may not contribute a hook, an MCP server or a `[[bin]]`, because
all three name a program this machine would run, and a bundle that fails to load is
dropped and reported rather than taking the run with it. A `[[bin]]` is a declaration
and not a grant: the harness says what a bundle ships, and where a host puts it — and
whether the policy lets the agent invoke it — stays the host's decision.

### Reach: browser, LSP, web, documents, git

**A browser**, behind the `browser` feature, off by default: open a page, click,
type, scroll, read the text the page actually renders, and take a screenshot the
agent is then shown — with the console output and the uncaught errors each action
produced. **Every document navigation is an `Act::Net` check against the host,
decided at the paused request rather than at the URL a tool was handed**, so a
navigation caused by a click, a redirect or a script is decided by the same rule as
one the model typed, and every decision is a row in the trace. A URL that reaches no
host is decided by its scheme instead, before the navigation is issued: `about:blank`
and nothing else, so `file:`, `data:` and `javascript:` are refused and recorded
rather than slipping past a check that only ever saw hosts. It is driven over a
pipe on the child's own descriptors and opens **no debugging port**, which is both
the smaller attack surface and the reason it needs no new dependency. Nothing is ever
downloaded: the browser is one already installed, and its absence is a refusal.

**LSP navigation.** Point the harness at a language server — on the contract, or in
the user-scope `io.toml` — and the agent
asks the questions an editor answers rather than the ones a text search answers:
`lsp_definition`, `lsp_references`, `lsp_symbols`, `lsp_hover` and `lsp_rename`. The
server starts once per run in the background, so its index is paid for once and not
inside a tool call, and starting it is an `Act::Exec` check on the binary the operator
named, while the `path` a question names is an `Act::Read` check taken before the
server is told anything. `lsp_rename` **writes nothing** — it answers with a patch
series you apply with `patch_file`, one gate check per file, and renders only the
files the policy allows reading outright, because the server chooses which files a
rename names. Configure no server and nothing changes: the five schemas are absent
from the catalogue entirely.

**Provider-executed web search and fetch.** `TaskContract::with_web` declares what the
provider may look up on the agent's behalf — search, optionally fetch, a cap on
requests, and the hosts to allow or block — and each vendor translates that one
declaration into its own shape, refusing outright what it cannot express. The sources
an answer drew on are rows in the trace. The provider dials the URL, so this crate
opens no socket for it and the domain filter is the vendor's, not the policy's: that
bound is stated in full in the [web guide](docs/guide/web.md).

**Documents and images**, behind opt-in features: spreadsheets, Word, PowerPoint text,
PDF, and barcode decoding, each gated on the real path the model named; and image
passthrough to any provider whose model accepts one.

**Git**, as fixed-argv built-ins: status, diff, log, add, commit, branch and worktree,
so a run ends as a reviewable commit on a branch of its own rather than a working tree
someone has to reconstruct. The model supplies paths, a message and a branch name,
never a subcommand or a flag, so push, fetch, reset and rebase are unreachable by
construction — and `git switch --create`, the one checkout that cannot discard a
change, is the only one of them that is reachable. An agent definition can ask for its
own worktree, so concurrent children stop overwriting each other's files.

### Observability and retention

Register an observer and be called as the run happens — steps, tool calls, approvals,
refusals, spend draws, retries, fallbacks, outcomes — instead of polling the store. A
recorded provider replays a case so it runs identically twice.

Behind the `otel` feature that record also leaves the process. `OtelExporter` is an
observer like any other, and exports a run as OpenTelemetry spans over OTLP/HTTP —
a span for the run, one per step, one per tool call, and one per provider call
carrying the model, the token split and the latency it cost — so an agent appears in
the dashboard already watching the services it calls. It sends structure and numbers
and never transcript content: the prompt, the replies, tool arguments and tool output
are not implemented as attributes at all, so no flag can include them by accident. A
collector that is down or slow cannot change what a run does.

Nothing expires on its own: there is no background job, no default retention window,
and how long an audit record survives is not a library's decision. What the store
offers is the instrument. `store_size` and `session_size` answer what the file and one
session are actually holding; `delete_session` removes a session whole — its turns, the
runs those turns drove and every run those spawned, in one transaction;
`sweep_sessions` applies that to everything older than a date while **refusing** any
session holding a run that is still resumable; and `compact` returns the freed pages to
the filesystem, because SQLite frees them into the file rather than out of it.
`archive_session` is the other half: every row and every number stays — what it cost,
how long it took, which files it touched — and every column holding words is emptied,
so an audit obligation and a privacy obligation can be satisfied at once. A deletion
cannot be undone by this crate, and none of it is reachable by a model.

## Guides

One page per capability, each carrying the depth this page does not — including the
limits that capability actually has.

| Guide | What it covers |
| --- | --- |
| [Permissions and approval](docs/guide/permissions.md) | Layered rules, what asks and what is refused, deferring past process exit, and the plan gate that reviews the approach before anything is written |
| [Command execution](docs/guide/command-execution.md) | Running a project's own toolchain, checked on the whole argv, in the step or beyond it — and the bound that is not there |
| [Language support](docs/guide/language-support.md) | Toolchain detection, a criterion in any language, migrating off the Rust-specific gates |
| [Verification](docs/guide/verification.md) | The criteria, execution-based gates, and exactly what a pass proves |
| [Agent composition](docs/guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](docs/guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [CodeAct](docs/guide/codeact.md) | One contained Python program instead of a chain of calls, how the interpreter is found, what a program may not call, and the two bounds beyond the sandbox's |
| [Durable runs](docs/guide/durable-runs.md) | Checkpoints, resume, approvals that survive a restart |
| [MCP and network egress](docs/guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy does not govern |
| [Tools and skills](docs/guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery and its boundary |
| [Context and memory](docs/guide/context-and-memory.md) | Per-turn assembly, compaction, invalidation, durable memory, pinning and recall |
| [Resilience](docs/guide/resilience.md) | Failure classification, retry, provider fallback, stall detection |
| [Observability and replay](docs/guide/observability.md) | Observers, events, outcome records, deterministic replay |
| [Sessions](docs/guide/sessions.md) | Durable conversations: a turn is a run, token streaming, steering, branching |
| [Agency](docs/guide/agency.md) | A visible plan, a question about intent, named agents, prompt templates |
| [Driving a browser](docs/guide/browser.md) | Opening a page, using it, reading what it rendered and looking at it — with every navigation decided at the paused request |
| [Web search and fetch](docs/guide/web.md) | Provider-executed lookups, the three vendor translations, citations, and where the boundary is not |
| [Configuration](docs/guide/configuration.md) | One `io.toml`, four layered scopes, projected onto the typed API, and which file decided each key |
| [Accounting](docs/guide/accounting.md) | Per-call rows, cache and reasoning tokens, latency, derived cost, and grouped outcome, gate and recovery counts |
| [Documents](docs/guide/documents.md) | Spreadsheets, Word, PowerPoint, PDF, barcodes — and what was cut |
| [Images and git](docs/guide/images-and-git.md) | Image passthrough and the fixed-argv git built-ins |
| [Hooks](docs/guide/hooks.md) | An audit log, a notification, a formatter or a check that stops the run, declared in the user-scope `io.toml` |
| [Providers](docs/guide/providers.md) | One compatible provider, the 21 vendor presets, running a model locally, what a model costs, and asking one to think harder |
| [Capability bundles](docs/guide/plugins.md) | A directory that contributes skills, templates, agents, MCP servers, hooks and deny rules at once, what a cloned repository may not hand you, and how a contribution names its bundle |
| [Retention](docs/guide/retention.md) | What a store is holding, removing a session whole, sweeping to a date and what it refuses, archiving the words while keeping the numbers, and reclaiming the space |
| [The mailbox](docs/guide/mailbox.md) | Giving each agent in a tree an address, sending a finding to a named sibling, an inbox read oldest-first exactly once, and a bounded wait |

[docs/CAPABILITIES.md](docs/CAPABILITIES.md) indexes them and carries the release
each capability arrived in. [docs/CONTRACT.md](docs/CONTRACT.md) is the public
contract: what is stable, what may change, and the limits that hold today.

## Supported files and languages

### Text and source: everything, and no list

There is no allowlist of source extensions, and that is the design rather than an
omission. `read_file`, `write_file`, `edit_file` and `patch_file` treat any text
file the same way — `.md`, `.txt`, `.rs`, `.py`, `.ts`, `.go`, `.java`, `.rb`,
`.sql`, `.yaml`, `.toml`, `.json`, a `Dockerfile`, a shell script, a file with no
extension at all. A harness that shipped a list of the languages it could edit
would be wrong about your repository the first week, so it ships none.

What the tools do enforce is the boundary and the honesty of the answer: a read
returns the file, the line range it asked for, or a refusal naming the size and
the ceiling — never a shortened file wearing the shape of a whole one. A read of a
binary is a refusal saying so, not an empty string.

The formats below are the ones that need more than reading text, and each is
listed by what the agent can actually *do* with it — read, generate, or edit in
place — because those are three different answers.

### Documents, behind `documents` or the per-format feature

| Format | Feature | Read | Generate | Edit in place |
| --- | --- | --- | --- | --- |
| Excel workbook, `.xlsx` | `xlsx` | Yes, one sheet at a time, plus a sheet listing | Yes, a new workbook | Yes — a single cell, keeping the rest of the workbook |
| Word document, `.docx` | `docx` | Yes, the text | Yes, a new document from paragraphs | **No, deliberately** — the round trip drops OOXML this crate does not model, which for someone's real document is data loss |
| PowerPoint deck, `.pptx` | `pptx` | Yes, slide by slide | **No** — there is no writer | **No** |
| PDF, `.pdf` | `pdf` | Yes, text extraction, best effort across columns and tables | Yes, one page per string | Partly — stamp a watermark across every page, and fill AcroForm fields by name, both keeping the existing content |
| Barcode and QR, in a `.png` or `.jpg` | `barcode` | Yes, decode | **No** — decoding only, no encoder | — |

Spreadsheet edits do round-trip a workbook the harness did not create, but
preservation is not a guarantee: charts, drawings, pivots and macros are where it
is likeliest to cost something.

**Images**, behind `media`, passed to any provider whose model accepts one:

| Media type | Extensions | What happens |
| --- | --- | --- |
| `image/jpeg` | `.jpg`, `.jpeg` | Passed through byte-identically |
| `image/png` | `.png` | Passed through byte-identically |
| `image/gif` | `.gif` | Passed through byte-identically |
| `image/webp` | `.webp` | Passed through byte-identically |
| `image/bmp` | `.bmp` | Decoded and re-encoded to PNG at the door |
| `image/tiff` | `.tif`, `.tiff` | Decoded and re-encoded to PNG at the door |
| `image/x-icon` | `.ico` | Decoded and re-encoded to PNG at the door |
| `image/x-tga` | `.tga` | Decoded and re-encoded to PNG at the door |
| `image/x-portable-anymap` | `.pnm`, `.pbm`, `.pgm`, `.ppm` | Decoded and re-encoded to PNG at the door |
| `image/svg+xml` | `.svg` | **Refused by name**, with the conversion that fixes it |
| `image/heic` | `.heic`, `.heif` | **Refused by name**, with the conversion that fixes it |
| `image/avif` | `.avif` | **Refused by name**, with the conversion that fixes it |

The first four are the types every vendor documents. The next five need a decoder
this crate already carries, so they are converted rather than refused — which
costs single-digit milliseconds on an image the size a model actually looks at,
and often moves the file *under* the size bound on the way. The last three would
need a decoder this crate does not carry, so they are named in the refusal along
with the conversion, rather than reported as "unsupported media type".

Two bounds decide whether an image attaches at all: **5 MiB** per image, which is
the smaller of the two vendor limits and therefore the honest one, and
**50,000,000 pixels**, which is checked from the header before anything is
decoded.

### Files the harness itself reads

With no feature and no model involved: one `io.toml` across four scopes, the
`AGENTS.md` a repository already carries, markdown skill files, a bundle's
`plugin.toml`, and unified diffs for `patch_file` — applied as a unit or not at
all.

### Languages and toolchains

Language support here is not a set of parsers. It is command execution, plus a
shipped table mapping a marker file to that ecosystem's own commands, plus a
verification criterion that runs whatever command the project uses. That is why
the agent can work in a language this crate has never heard of: it runs *your*
toolchain.

The table is the default, and every command in it is overridable in `io.toml`.
Markers are tried in order and the first match wins:

| Marker in the root | Ecosystem | Driver | Test command |
| --- | --- | --- | --- |
| `Cargo.toml` | cargo | `cargo` | `cargo test` |
| `deno.json`, `deno.jsonc` | deno | `deno` | `deno test` |
| `package.json` | node | `bun`, `pnpm`, `yarn` or `npm`, chosen by the lockfile present | `<driver> test` |
| `go.mod` | go | `go` | `go test ./...` |
| `pyproject.toml`, `requirements.txt` | python | `uv`, `poetry` or `pip`, chosen by the lockfile present | `uv run pytest`, `poetry run pytest`, or `python -m pytest` |
| `pom.xml` | maven | `mvn` | `mvn -B test` |
| `build.gradle`, `build.gradle.kts` | gradle | `gradle` | `gradle test` |
| `mix.exs` | elixir | `mix` | `mix test` |
| `Gemfile` | ruby | `bundler` | `bundle exec rake test` |
| `composer.json` | php | `composer` | `composer test` |
| `Package.swift` | swift | `swift` | `swift test` |
| `CMakeLists.txt` | cmake | `cmake` | `ctest --test-dir build` |
| `*.csproj`, `*.fsproj`, `*.sln` | dotnet | `dotnet` | `dotnet test` |
| `Makefile` | make | `make` | `make test` |

Each row also carries install, build, lint, format and run commands. Two
orderings are deliberate: `deno.json` is tried before `package.json`, because a
Deno project may carry both and `npm test` is wrong for it; `Makefile` is tried
last, because a great many projects have one *beside* their real build system. A
root with no marker reports nothing rather than guessing.

**A write is type-checked where a cheap checker exists and the policy allows it.**
After a successful write, five ecosystems run their own check and attach what it
found, so a mistake arrives with the edit rather than twenty steps later:

| Ecosystem | Check after a write |
| --- | --- |
| cargo | `cargo check --message-format=json` |
| deno | `deno check .` |
| node | `tsc --noEmit` |
| go | `go build ./...` |
| python | `pyright` |

The other nine are skipped on purpose. They have no type-check step separate from
their build, and running a build after every single edit would make editing
unusable. An ecosystem with no cheap checker is a fact about the ecosystem, not a
gap in the harness: nothing runs, and nothing is said to the model.

The check is an `Act::Exec` on the program and on the whole argv, like any other
spawn, and it runs inside the run's containment — a checker is a compiler, and a
compiler runs the workspace's own build script. Only an outright allow runs it:
there is no approver on the path after a write, so an `ask` is a silent skip, and
`Policy::default()` asks about exec. Name the checker with `allow_exec` for the
projects you want checked; the write itself is unaffected either way.

### Language servers

There is no built-in list of languages here either. Name a server on the contract or
in the user-scope `io.toml` — its command, and the extensions it answers for — and
the agent
gains five tools for that language: `lsp_definition`, `lsp_references`,
`lsp_symbols`, `lsp_hover` and `lsp_rename`. `rust-analyzer`, `gopls`,
`typescript-language-server`, `pyright`, `clangd` and anything else speaking LSP
work the same way, because the crate speaks the protocol rather than the language.

Configure no server and nothing changes: the five schemas are absent from the
catalogue entirely. Starting one is an `Act::Exec` check on the binary the
operator named, and `lsp_rename` writes nothing — it answers with a patch series
you apply with `patch_file`, one gate check per file.

Two bounds worth knowing before turning one on: a server indexes the whole root,
including files a `deny_read` rule covers, and it runs at the harness's own
privilege — like an MCP stdio server. The
[language support guide](docs/guide/language-support.md) states both in full.

## Feature flags

Everything below is off by default. The default build compiles no optional
dependency at all.

| Feature | What it adds |
| --- | --- |
| `media` | Images to providers that accept them: the four types they document, plus BMP/TIFF/ICO/TGA/PNM converted to PNG on the way |
| `documents` | Umbrella over the five below |
| `xlsx` | Spreadsheet read, generate, and preserving single-cell edit |
| `docx` | Word read and generate (no in-place edit, deliberately) |
| `pptx` | PowerPoint text extraction (read-only, no writer) |
| `pdf` | PDF generate, extract text, watermark, fill AcroForm fields |
| `barcode` | Barcode and QR decoding from an image |
| `browser` | Driving an already-installed browser over a pipe, under the run's own policy |
| `otel` | Exporting a run as OpenTelemetry spans over OTLP/HTTP, following the GenAI semantic conventions |
| `mcp-server` | Serving this crate's own tools over MCP on stdio, under this crate's policy, gate and trace |
| `codeact` | One contained Python program in place of a chain of tool calls, run by a host interpreter that is found rather than downloaded |

The last four add **no crate at all**. They are features because a build that did
not ask for a browser, an outbound telemetry writer, a door onto its own tools or
a program driving them should not compile one — not because anything is being
pulled in.

## Platform support

| Platform | Containment | Egress under a host-naming policy |
| --- | --- | --- |
| macOS | Native, `sandbox-exec` | Scoped to the proxy's address exactly |
| Linux | Native, a chain: Landlock, `bwrap`, namespaces, floor | The Landlock rung scopes to a **port**, so another host on that port number is reachable; the namespace rungs cannot reach the host's loopback and are not given such a run |
| Windows, default | Native, Job Object (memory, CPU, process count, tree kill) — **resources only, no filesystem or network boundary** | The proxy is **advisory**: a command that ignores it reaches the network, and the agent's own prompt uses that word |
| Windows, opt-in | Native, AppContainer inside a Job Object — writes confined to the paths the run resolved | No proxy at all: a process inside an AppContainer cannot reach a loopback listener under any capability set, which was measured rather than assumed. Egress is the capability — all of the network, or none |
| Portable floor | Whatever the host allows, recorded as the floor | Advisory, as above |

The full suite runs on macOS, Linux and Windows in CI.

The rung a host takes is the strongest that can enforce what the run asked for, and
a run denying egress is never given one that cannot deny egress. The Linux chain's
first rung is Landlock, which needs no namespace at all — which matters because a
stock Ubuntu host ships `kernel.apparmor_restrict_unprivileged_userns=1` and refuses
the unprivileged user namespace the namespace backend needs. The namespace rung
also needs `setpriv` (util-linux, beside `unshare`), so that the payload cannot
inherit the capabilities that made its own read-only mounts; a host without it
skips the rung and reports the backend it actually got.

What a run reports about its boundary is **measured**: a probe attempts a write
and a dial outside it before the first step, and an arm that could not be
attempted reads as *not established* rather than as confined.

**Windows access confinement is opt-in.** A Job Object has no filesystem facility and
no network facility, so a contained Windows command gets the resource caps and nothing
else unless you ask for more. The access half is an AppContainer: a token that is
default-deny on every securable object and reaches only what an explicit ACE granted
it. `SandboxConfig::with_access_confinement()` selects it — except under
`FullAccess`, which says the payload may write anywhere, and putting that inside a
default-deny container would refuse the very thing the mode grants. It is opt-in because the
grant set is derived from the run's own facts — the workspace, the toolchain's cache
roots, the temporary directory, the program's own directory, `%SystemRoot%` — and
derived is not complete. A toolchain reading a machine-wide file outside that set is
refused, and a default boundary that cannot run an arbitrary payload is worse than one
you reach for deliberately.

And it does not degrade. Everywhere else an unavailable primitive falls back to a
weaker rung and reports it; a boundary you asked for by name and that cannot be applied
is an error naming the grant that failed, because a run that quietly took the Job Object
instead would have no boundary at all while every assertion about it still passed.

## Stability

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. A minor
release may break the public contract — SemVer permits it below 1.0, and this project
uses it. What you can rely on is not that a break will not happen, but that when it does
it is marked in [CHANGELOG.md](CHANGELOG.md) with a migration note saying what to write
instead, and that a renamed or removed item goes through a deprecation cycle rather than
vanishing between two releases.

[docs/CONTRACT.md](docs/CONTRACT.md) states the whole of it. The MSRV floor comes from
`libsqlite3-sys`, which publishes no `rust-version` of its own, so cargo cannot catch a
too-old toolchain at resolve time — the build fails inside that dependency rather than
here. The contract records why no version of it avoided that.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Work branches from `develop`, lands via PR, and
every user-facing change updates [CHANGELOG.md](CHANGELOG.md). Releases follow
[docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md).

## Security

Report vulnerabilities per [SECURITY.md](SECURITY.md).

## License

Apache-2.0. Copyright 2026 Aakash Pawar (InitOrigin). See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
