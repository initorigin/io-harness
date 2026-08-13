<div align="center">

<img src="https://raw.githubusercontent.com/initorigin/io-harness/main/assets/initorigin-logo.png" alt="InitOrigin" width="112" height="112">

# IO Harness

[![crates.io](https://img.shields.io/crates/v/io-harness.svg)](https://crates.io/crates/io-harness)
[![downloads](https://img.shields.io/crates/d/io-harness.svg)](https://crates.io/crates/io-harness)
[![docs.rs](https://img.shields.io/docsrs/io-harness)](https://docs.rs/io-harness)
[![CI](https://github.com/initorigin/io-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/initorigin/io-harness/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/crates/msrv/io-harness)](Cargo.toml)
[![License](https://img.shields.io/crates/l/io-harness.svg)](LICENSE)

</div>

**An embeddable agent runtime for Rust. Any task, any provider, in your process —
with a permission boundary, a sandbox, and a durable trace you own.**

You hand it a contract: the task, the workspace it may touch, and what it may
read, write, run and dial. It runs the loop — observe, reason, act, check, stop —
and hands back an outcome, with every step, refusal and budget draw in a SQLite
trace you can read afterwards.

## Quickstart

```toml
[dependencies]
io-harness = "0.53"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

**MSRV: Rust 1.95** or later. The whole surface below is on that floor.

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

## Requirements

The MSRV floor comes from `libsqlite3-sys`, which publishes no `rust-version` of
its own, so cargo cannot catch it at resolve time — on 1.94 the build fails
inside that dependency's build script rather than here, with an error about a
missing `cfg_select` macro. It rose from 1.88 in 0.23.0; see
[docs/CONTRACT.md](docs/CONTRACT.md) for why there was no version of that
dependency which avoided it.

The default build compiles no optional dependency at all, and the whole
dependency tree is held deliberately small.

## What it does

**The loop.** A [`TaskContract`] names the goal, the subject, and — optionally —
the criterion. Workspace mode gives the agent `grep`, `find`, `read_file`,
`write_file`, `edit_file` and `patch_file` across a repository root. Single-file
mode edits one file. A change touching four places in one file is one
`patch_file` call taking a unified diff — applied as a unit or not at all, so a
patch that no longer fits is refused with the hunk named rather than half
written. In a project whose ecosystem the harness recognises, a successful write
runs that project's own type-check command and attaches what it found, so a
mistake arrives with the edit rather than twenty steps later; `check` asks the
same question *before* a write, and is refused by the same policy that refuses
`exec` — see [language support](docs/guide/language-support.md).

**Navigating the code.** Point the harness at a language server in `io.toml` and
the agent asks the questions an editor answers rather than the ones a text search
answers: `lsp_definition`, `lsp_references`, `lsp_symbols`, `lsp_hover` and
`lsp_rename`. Where is this defined, who calls it, what is in this file, what is
this. The server starts once per run in the background, so its index is paid for
once and not inside a tool call, and starting it is an `Act::Exec` check on the
binary the operator named. `lsp_rename` **writes nothing** — it answers with a
patch series you apply with `patch_file`, one gate check per file. Configure no
server and nothing changes: the five schemas are absent from the catalogue
entirely. See [language support](docs/guide/language-support.md).

**Driving a browser.** Behind the `browser` feature, off by default, the agent can
open a page, click, type, scroll, read the text the page actually renders, and take
a screenshot it is then shown — with the console output and the uncaught errors each
action produced. **Every document navigation is an `Act::Net` check against the
host, decided at the paused request rather than at the URL a tool was handed**, so a
navigation caused by a click, a redirect or a script is decided by the same rule as
one the model typed, and every decision is a row in the trace. It is driven over a
pipe on the child's own descriptors and opens **no debugging port**, which is both
the smaller attack surface and the reason it needs no new dependency. Nothing is
ever downloaded: the browser is one already installed, and its absence is a refusal.
See [driving a browser](docs/guide/browser.md).

**What a change is kept as.** Every write records the change as a unified diff of
the whole file, so a trace can show what a step changed and not only how many
lines it changed. `Store::patch` renders a run's whole change as a step-ordered
patch series for review, and `rewind_step` reverse-applies one step's hunks —
undo at a granularity finer than "throw the run away", walking backwards from the
newest step.

**Commands, under the same boundary as everything else.** The agent runs the
project's own build, tests, linter or package manager through an `exec` tool
taking a fixed argv and never a shell string. Every call is checked against the
policy on the program *and* on the whole argv, so `allow_exec("cargo test*")`
beside `deny_exec("cargo publish*")` means what it reads. A whole command line —
pipelines, redirects, `&&` — is `shell`, parsed in this crate rather than by a
shell and checked stage by stage before the first process starts. `shell_start`,
`shell_poll` and `shell_kill` are the same line with a longer life, for a dev
server or a log tail that has to outlive the step that started it.

**A run is contained unless you say otherwise.** Since 0.46.0 every command
`exec` and the foreground `shell` start runs inside the sandbox backend this host
offers, writing to the workspace root, the system temporary directory and the
detected toolchain's own caches — and nowhere else. `ExecMode` names the three
grants: `ReadOnly`, `WorkspaceWrite` (the default) and `FullAccess`, which is what
every release up to 0.45.0 did by default and is now
`TaskContract::with_full_access()` — a sentence in your source rather than a field
you never set, because it is the widest thing the crate grants. The default
carries **no resource cap**: containment says where a command may write, and
`with_contained_exec(SandboxConfig::new())` is how you add the ceilings.

The **workspace root** stays the working directory — nothing is copied in and
nothing is discarded — so an incremental build survives between commands, and the
toolchain's own cache being writable is what lets a default-contained `cargo` or
`npm` run at all. What each platform actually enforces differs and the difference
is not cosmetic: macOS and Linux confine writes and deny egress — Linux since
0.47.0 through a chain whose first rung needs no namespace — while **Windows
contains resources and not access**, and a host that can deliver none of it
falls back to the portable floor and **records the floor**, so a run contained
less than you asked for is legible afterwards — in the trace, in `EventKind::Contained`, and in the agent's own
prompt. **0.48.0 closed the last two gaps**: a backgrounded `shell_start` handle
and the git built-ins are contained like everything else, and a run whose policy
names hosts reaches those hosts and no others — through a proxy the run owns,
whose guarantee differs per backend and is stated per backend in
[docs/CONTRACT.md](docs/CONTRACT.md) rather than discovered.

**Verification in any language, or none — or a second model.** A
criterion can be the project's own test command in whatever language it is
written, or nothing at all when the task has no checkable criterion — "work out
why the deploy fails" is a run, not a gate. `Verification::Review` is the other
direction: a model reads what the run wrote against a rubric you set and returns a
verdict with its reasons, and it **refuses to run on the model that wrote the
change**. An exit status cannot catch a change that compiles, passes and is still
wrong; a review can, and is not a proof — the two compose. What a passing gate does and does not prove is stated exactly in the
[verification guide](docs/guide/verification.md); it is narrower than it reads.
The harness also ships a table of what each ecosystem's commands conventionally
are, so the agent does not spend turns discovering that this is a pnpm workspace
— see [language support](docs/guide/language-support.md).

**A permission boundary.** Layered, deny-first rules decide what the agent may
read, write, execute, and connect to. Anything marked *ask* goes to an approver
that can approve, deny, or defer past the end of the process and resume on a
human decision later. Every refusal and decision is in the trace, attributed to
the rule and the layer that produced it.

**A plan before anything is written.** `TaskContract::with_plan_gate` opens a run
in a planning phase: the agent reads, writes nothing, and the only exit is an
ordered plan — each step optionally naming the agent that owns it — which a
`PlanGate` approves, corrects or cancels. With no in-process answer the plan
persists and the run stops, so the decision can be made after the process exits.
Beside it the agent can ask a question about intent rather than guessing, run
under a named roster of agent definitions, and be told to think harder or less
through an effort level each vendor projects into its own shape — see the
[agency guide](docs/guide/agency.md).

**Budgets and stop conditions.** Steps, wall-clock time, and token spend are
capped. A tree of agents draws from one shared ledger no spawned contract can
raise.

**The stable prefix is cached, where the vendor sells it.** One cache breakpoint
sits at the end of the system block, which on the Anthropic wire covers the tool
schemas and the instructions together, and OpenRouter carries the same marker. A
second sits at the end of the frozen transcript prefix once the run has compacted —
everything from the top of the prompt through the written summary stops changing,
so it can be cached, while the observations after it are still rewritten every turn
and are still never marked. The reads land in the accounting rows beside every other
token, so a long conversation over one workspace stops paying full price for the
part of the request that never changes. The crate never asks a vendor to cache a
prefix it has not already sent once, so a marker it places cannot be billed as a
cache write on a prefix that then moves.

**One failed gate is retried on its own.** Every gate evaluation is recorded as
`Passed`, `Failed` or `Errored` — the last being a criterion that could not run at
all, which is a different problem from one that ran and said no. `retry_gate`
re-runs *only* the criterion, against the workspace the run left, so a transport
failure on the gate does not cost the forty steps that produced the work.

**Which model answers can change mid-run.** `Routing` escalates to a
stronger model after repeated gate failures, downshifts while the change is small,
and — the rule an unattended job needs — refuses to start at all when the primary
provider reports it is unreachable, rather than quietly spending the night on a
fallback nobody chose.

**Agent composition.** A root run can spawn contained sub-agents over a shared
workspace, nested, many at once. Two caps, different in kind:
`max_total_agents` refuses the spawn that crosses it, `max_concurrent_agents`
queues it — so a hundred-agent task runs sixteen at a time until it is done
rather than failing at its seventeenth child. The queue is durable, counted per
tier and reported to an `Observer`, and a child that only ever waited is never
charged. A child inherits its parent's policy and can only narrow it — never
grant itself what the parent lacks.

**An execution sandbox.** Model-produced code runs in an ephemeral sandbox with
an isolated workdir, resource caps that kill rather than throttle, and network
denied by default. Native backends on macOS and Linux over a portable floor that
runs everywhere — and the same machine is what a contained `exec` reaches, so the
project's own commands can run behind it rather than beside it.

**Durable, unattended runs.** After every completed step the trace, the budget
draw, and a checkpoint commit in one transaction. A crash resumes the whole tree
where it stopped: completed steps are not re-run, the budget is not
double-charged, and an irreversible action already taken is not taken twice.

**Two processes, one run.** A run no longer belongs to the process that started
it. `Broadcast` wraps whatever `Observer` you already have and writes each event
to the store as it passes, so a second process can `Attach` to a run that is
*still going*, read the same events, see whether it is parked on an approval, a
question or an unreviewed plan, and answer it — without killing it and without
resuming it. The first answer wins and the loser is told, because the write is a
compare-and-swap on the row the run already reads. An attaching process reads and
decides: there is no method on it that starts, resumes or steps a run, so killing
the watcher changes nothing and killing the owner leaves exactly the resumable run
it always did.

**Providers, with fallback.** OpenRouter, Anthropic and OpenAI behind one trait,
over the crate's own HTTP+SSE client. Beside them one `Compatible` provider
reaches any OpenAI-shaped endpoint from a base URL, an auth style, a key and a
model, and 21 vendors of that shape have a named constructor so nobody types a
URL: 13 hosted — Groq, xAI, Mistral, DeepSeek, Together, Fireworks, Cerebras,
Perplexity, Gemini through its compatibility endpoint, Moonshot, Zhipu, Qwen,
MiniMax — and 8 local runtimes — Ollama, llama.cpp, vLLM, LM Studio, LocalAI,
Jan, SGLang, KoboldCpp — where a model on the developer's own laptop costs
nothing to run. `Provider::models()` reports what a provider can run and what it
costs. A provider that is down or rate-limited falls back to the next configured
one; failures are classified so a caller can tell a retryable transport error
from a terminal one.

**Extensibility, in-process and out.** Implement the `Tool` trait for something
your program already does, or point the harness at MCP servers over stdio or
streamable HTTP. Skills are markdown instruction files that shape how the agent
approaches a class of task, with no Rust at all.

**The model is asked a question it was trained to answer.** A request carries an
ordered transcript — user turns, assistant turns holding the calls the model made,
and the results answering them — and each wire maps it onto that vendor's own
block types, `tool_use`/`tool_result` on Anthropic and `tool_calls` plus
`role: "tool"` messages on the OpenAI wire. Through 0.48.0 a request held one
system string and one user string, so the crate parsed the tool protocol off a
response and discarded it on the way back in: a step's results were re-rendered as
bracketed prose narrating the assistant's own past actions in the third person.
That produced no error and nothing in a log, only degraded instruction following.
`CompletionRequest::user` is still filled and byte-identical to what it was, so a
provider written against an earlier release keeps working.

**A parent chooses how a child comes back, and a child says what it found.**
`spawn_agent` takes `wait` and `background_after_secs`: a parent can carry on
while a sub-agent works and read its report at a later step, and a child that
outlasts a stated wall clock moves to the background instead of holding the tree.
It is not cancelled — a parent that stops waiting is not a parent that stops the
work — and a tree still never returns while a child it started is running. Through
0.49.0 a child came back as `[child 7 "goal" -> Success { steps: 4 }]`: a
discriminant and a step count, with nothing it concluded, so the only way a
finding could travel was a file the parent then read. It now comes back with what
it cost and what it said.

**Provider-executed web search and fetch.** `TaskContract::with_web` declares what
the provider may look up on the agent's behalf — search, optionally fetch, a cap on
requests, and the hosts to allow or block — and each vendor translates that one
declaration into its own shape, refusing outright what it cannot express. The
sources an answer drew on are rows in the trace. The provider dials the URL, so
this crate opens no socket for it and the domain filter is the vendor's, not the
policy's: that bound is stated in full in the [web guide](docs/guide/web.md).

**Context that stays relevant.** Each turn is assembled to fit a stated share of
the token budget: superseded observations are compacted, and an observation a
later write invalidated is re-read rather than trusted. Durable memory keyed to
the workspace survives between runs — as a fact or a decision, pinnable so a run
cannot overwrite a correction, with a per-run record of which entries it actually
drew on.

**Observation and replay.** Register an observer and be called as the run
happens — steps, tool calls, approvals, refusals, spend draws, retries,
fallbacks, outcomes — instead of polling the store. A recorded provider replays a
case so it runs identically twice.

**Documents and images**, behind opt-in features: spreadsheets, Word, PowerPoint
text, PDF, and barcode decoding, each gated on the real path the model named; and
image passthrough to any provider whose model accepts one.

**Git**, as fixed-argv built-ins: status, diff, log, add, commit, branch and
worktree, so a run ends as a reviewable commit on a branch of its own rather
than a working tree someone has to reconstruct. The model supplies paths, a
message and a branch name, never a subcommand or a flag, so push, fetch, reset
and rebase are unreachable by construction — and `git switch --create`, the one
checkout that cannot discard a change, is the only one of them that is
reachable. An agent definition can ask for its own worktree, so concurrent
children stop overwriting each other's files.

**Durable conversations.** `Session::open(store, root)` holds a conversation
instead of firing one task: turns that read the turns before them, text streamed
to an observer as the model produces it, a mid-turn steer or an interrupt honoured
at the next step boundary, and a tree an operator can branch from any earlier turn.
A turn is a run, so every turn is budgeted, policy-bounded, checkpointed and
resumable — see the [sessions guide](docs/guide/sessions.md).

**And a conversation can fan out.** `Session::turn_contained` takes a
`Containment` and lets the agent answering that turn decompose the work into
contained sub-agents — under the session's own policy, one shared ledger for the
turn, and the observer the operator is already reading. It stays one turn in the
conversation whatever it spawned, and the five turn entry points that predate it
still never offer the spawn tool.

**And not every turn is work.** A turn's own first completion decides what the
turn was: stopped on text, it closes as `TurnKind::Reply` with no step, no gate,
no checkpoint and no plan gate; carrying a tool call, the loop continues from that
same completion. So `hi` costs one completion and stages nothing, while `hi, the
login page is broken` runs — decided by the model, at no extra call, with no list
of greetings anywhere in this crate or in yours.

**Configuration in a file.** `Config::discover(root)` reads one `io.toml` across
four scopes — the crate's defaults, a user file, a committed project file, and a
gitignored local one — and projects it onto the typed API: a `Policy`, a
`SandboxConfig`, the run budgets, the toolchain commands, a price table, MCP
servers, and **which provider and model to run with what behind it**, so an
embedder reads a `ProviderSpec` rather than writing provider-selection code.
`[app]` is a section the crate stores and never validates, so the programs built
on it keep their own settings in the same file; `[profile.<name>]` overlays a
named set of choices; `[instructions]` discovers the `AGENTS.md` a repository
already carries — by default as of 0.45.0, with `files = []` as the opt-out — and
carries it in the system block as the repository's own guidance. `${env:...}`, `${file:...}` and `${cmd:...}` keep a credential
out of the file, an unknown key is an error rather than a shrug, and nothing is
loaded implicitly: the caller reads the file, before the run, once. A **project**
file may narrow the boundary and may never widen it — the keys that would make
cloning a repository dangerous are refused in the one file a clone delivers.

**Hooks, so an operator shapes a run without writing Rust.** A `[[hook]]` table
names the events it wants and one thing to do with them: a path to append the
event stream to is an audit log, an argv to run is a notification or a formatter,
and that argv with `on_failure = "cancel"` is a local policy check that ends the
run. `Config::hooks()` returns an `Observer` the caller installs like any other,
so no run loop changed — and the whole array is refused in the committed project
file, for the same reason `${cmd:...}` is. See the [hooks guide](docs/guide/hooks.md).

**Capability bundles, so a set of capabilities travels as one thing.** A
directory with a `plugin.toml` contributes skills, prompt templates, an agent
roster, MCP servers, hooks and deny-only policy at once, named by a `[[plugin]]`
entry in any scope. Every contributed name is namespaced `<plugin>__<name>` as it
loads, so a refusal, a tool call and a child's spend already say which bundle
introduced them — with no new table. A bundle declared in the committed project
file may not contribute a hook or an MCP server, because both name a program this
machine would run, and a bundle that fails to load is dropped and reported rather
than taking the run with it. See the [bundles guide](docs/guide/plugins.md).

**Undo.** Every write records what was there before the run first touched that
file, in the store rather than in memory, so `rewind(&workspace, &store, run_id,
path)` puts a file back — including deleting one the run created — after a crash
and a resume. One restore point per file per run, and it says plainly when it has
none rather than guessing. `rewind_run` widens that to the whole run: the files,
the memory entries it wrote, and the spawn backlog it left queued, in one call —
while the steps, the events and the ledger stay exactly as they were, because the
spend happened and an undo that erased them would make the trace lie.

## Guides

| Guide | What it covers |
| --- | --- |
| [Permissions and approval](docs/guide/permissions.md) | Layered rules, what asks and what is refused, deferring past process exit, and the plan gate that reviews the approach before anything is written |
| [Command execution](docs/guide/command-execution.md) | Running a project's own toolchain, checked on the whole argv, in the step or beyond it — and the bound that is not there |
| [Language support](docs/guide/language-support.md) | Toolchain detection, a criterion in any language, migrating off the Rust-specific gates |
| [Verification](docs/guide/verification.md) | The criteria, execution-based gates, and exactly what a pass proves |
| [Agent composition](docs/guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](docs/guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [Durable runs](docs/guide/durable-runs.md) | Checkpoints, resume, approvals that survive a restart |
| [MCP and network egress](docs/guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy does not govern |
| [Tools and skills](docs/guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery and its boundary |
| [Context and memory](docs/guide/context-and-memory.md) | Per-turn assembly, compaction, invalidation, durable memory, pinning and recall |
| [Resilience](docs/guide/resilience.md) | Failure classification, retry, provider fallback, stall detection |
| [Observability and replay](docs/guide/observability.md) | Observers, events, outcome records, deterministic replay |
| [Sessions](docs/guide/sessions.md) | Durable conversations: a turn is a run, token streaming, steering, branching |
| [Agency](docs/guide/agency.md) | A visible plan, a question about intent, named agents, prompt templates |
| [Web search and fetch](docs/guide/web.md) | Provider-executed lookups, the three vendor translations, citations, and where the boundary is not |
| [Configuration](docs/guide/configuration.md) | One `io.toml`, four layered scopes, projected onto the typed API, and which file decided each key |
| [Accounting](docs/guide/accounting.md) | Per-call rows, cache and reasoning tokens, latency, derived cost, and grouped outcome, gate and recovery counts |
| [Documents](docs/guide/documents.md) | Spreadsheets, Word, PowerPoint, PDF, barcodes — and what was cut |
| [Images and git](docs/guide/images-and-git.md) | Image passthrough and the fixed-argv git built-ins |
| [Hooks](docs/guide/hooks.md) | An audit log, a notification, a formatter or a check that stops the run, declared in `io.toml` |
| [Providers](docs/guide/providers.md) | One compatible provider, the 21 vendor presets, running a model locally, what a model costs, and asking one to think harder |
| [Capability bundles](docs/guide/plugins.md) | A directory that contributes skills, templates, agents, MCP servers, hooks and deny rules at once, what a cloned repository may not hand you, and how a contribution names its bundle |

[docs/CAPABILITIES.md](docs/CAPABILITIES.md) indexes them.
[docs/CONTRACT.md](docs/CONTRACT.md) is the public contract: what is stable, what
may change, and the limits that hold today.

## Feature flags

Everything below is off by default. The default build compiles no optional
dependency at all.

| Feature | What it adds |
| --- | --- |
| `media` | Image passthrough to providers that accept images |
| `documents` | Umbrella over the five below |
| `xlsx` | Spreadsheet read, generate, and preserving single-cell edit |
| `docx` | Word read and generate (no in-place edit, deliberately) |
| `pptx` | PowerPoint text extraction (read-only, no writer) |
| `pdf` | PDF generate, extract text, watermark, fill AcroForm fields |
| `barcode` | Barcode and QR decoding from an image |

## Platform support

| Platform | Sandbox containment |
| --- | --- |
| macOS | Native, `sandbox-exec` |
| Linux | Native, a chain: Landlock, `bwrap`, namespaces, floor |
| Windows | Native, Job Object (memory, CPU, process count, tree kill) — **resources only, no filesystem or network boundary** |

The full suite runs on all three in CI.

**0.47.0 closed the Linux hole in this table**, which was the easiest thing on
this page to over-read. The namespace backend needs an unprivileged user
namespace; Ubuntu 24.04 ships
`kernel.apparmor_restrict_unprivileged_userns=1` and refuses one, so on a stock
24.04 host — which is what `ubuntu-latest` is — every contained run took the
portable floor and the filesystem confinement was applied nowhere. Linux is now
a chain, and its first rung is Landlock, which needs no namespace at all. The
rung a host takes is the strongest that can enforce what the run asked for, and
a run denying egress is never given one that cannot deny egress.

**The Windows hole is still open, and it is the one to read carefully.** A Job
Object has no filesystem facility and no network facility, so a contained Windows
command gets the resource caps and nothing else — `ExecMode` is routed and
reported there and enforces nothing for the filesystem. The access half was
planned for 0.47.0 and moved whole to 0.59.0; nothing on Windows changed in this
release, in either direction.

**0.48.0 made egress per-host, and what that is worth also differs by row.** A run
whose policy names hosts routes its contained commands through a loopback proxy it
owns. macOS scopes the route to that address exactly; Linux's Landlock rung scopes
it to a **port**, so another host on that port number is reachable and the contract
says so; the namespace rungs cannot reach the host's loopback at all and are not
given such a run; and on Windows and the portable floor the proxy is **advisory** —
a command that ignores it reaches the network, and the agent's own prompt uses that
word.

Both are still reported rather than assumed: `select().backend()` answers before
the run and the trace records what actually applied.

## Stability

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. A
minor release may break the public contract — SemVer permits it below 1.0, and
this project uses it. What you can rely on is not that a break will not happen,
but that when it does it is marked in [CHANGELOG.md](CHANGELOG.md) with a
migration note saying what to write instead, and that a renamed or removed item
goes through a deprecation cycle rather than vanishing between two releases.

[docs/CONTRACT.md](docs/CONTRACT.md) states the whole of it.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Work branches from `develop`, lands via
PR, and every user-facing change updates [CHANGELOG.md](CHANGELOG.md). Releases
follow [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md).

## Security

Report vulnerabilities per [SECURITY.md](SECURITY.md).

## License

Apache-2.0. Copyright 2026 Aakash Pawar (InitOrigin). See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
