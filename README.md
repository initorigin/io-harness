<div align="center">

<img src="https://raw.githubusercontent.com/initorigin/io-harness/main/assets/initorigin-logo.png" alt="InitOrigin" width="112" height="112">

# IO Harness

[![crates.io](https://img.shields.io/crates/v/io-harness.svg)](https://crates.io/crates/io-harness)
[![docs.rs](https://img.shields.io/docsrs/io-harness)](https://docs.rs/io-harness)
[![CI](https://github.com/initorigin/io-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/initorigin/io-harness/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/crates/l/io-harness.svg)](LICENSE)
[![MSRV 1.95](https://img.shields.io/badge/MSRV-1.95-blue.svg)](Cargo.toml)

</div>

**An embeddable agent runtime for Rust. Any task, any provider, in your process —
with a permission boundary, a sandbox, and a durable trace you own.**

You hand it a contract: the task, the workspace it may touch, and what it may
read, write, run and dial. It runs the loop — observe, reason, act, check, stop —
and hands back an outcome, with every step, refusal and budget draw in a SQLite
trace you can read afterwards. The agent can run the project's own toolchain, so
the language the project is written in is not the harness's business.

## Quickstart

```toml
[dependencies]
io-harness = "0.22"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

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

**Requires Rust 1.95** or later. The floor comes from `libsqlite3-sys`, which
publishes no `rust-version` of its own, so cargo cannot catch it at resolve
time — on 1.94 the build fails inside that dependency's build script rather
than here, with an error about a missing `cfg_select` macro. It rose from 1.88
in 0.23.0; see [docs/CONTRACT.md](docs/CONTRACT.md) for why there was no
version of that dependency which avoided it.

## What it does

**The loop.** A [`TaskContract`] names the goal, the subject, and — optionally —
the criterion. Workspace mode gives the agent `grep`, `find`, `read_file`,
`write_file` and `edit_file` across a repository root. Single-file mode edits one
file.

**Commands, under the same boundary as everything else.** The agent runs the
project's own build, tests, linter or package manager through an `exec` tool
taking a fixed argv and never a shell string. Every call is checked against the
policy on the program *and* on the whole argv, so `allow_exec("cargo test*")`
beside `deny_exec("cargo publish*")` means what it reads. A command runs with
your process's privileges and is **not** sandboxed — that bound is stated in full
in the [command execution guide](docs/guide/command-execution.md), because it is
the widest thing the crate grants.

**Verification in any language, or none.** A criterion can be the project's own
test command in whatever language it is written, or nothing at all when the task
has no checkable criterion — "work out why the deploy fails" is a run, not a
gate. What a passing gate does and does not prove is stated exactly in the
[verification guide](docs/guide/verification.md); it is narrower than it reads.
The harness also ships a table of what each ecosystem's commands conventionally
are, so the agent does not spend turns discovering that this is a pnpm workspace
— see [language support](docs/guide/language-support.md).

**A permission boundary.** Layered, deny-first rules decide what the agent may
read, write, execute, and connect to. Anything marked *ask* goes to an approver
that can approve, deny, or defer past the end of the process and resume on a
human decision later. Every refusal and decision is in the trace, attributed to
the rule and the layer that produced it.

**Budgets and stop conditions.** Steps, wall-clock time, and token spend are
capped. A tree of agents draws from one shared ledger no spawned contract can
raise.

**Agent composition.** A root run can spawn contained sub-agents over a shared
workspace, nested, many at once. A child inherits its parent's policy and can
only narrow it — never grant itself what the parent lacks.

**An execution sandbox.** Model-produced code runs in an ephemeral sandbox with
an isolated workdir, resource caps that kill rather than throttle, and network
denied by default. Native backends on macOS and Linux over a portable floor that
runs everywhere.

**Durable, unattended runs.** After every completed step the trace, the budget
draw, and a checkpoint commit in one transaction. A crash resumes the whole tree
where it stopped: completed steps are not re-run, the budget is not
double-charged, and an irreversible action already taken is not taken twice.

**Providers, with fallback.** OpenRouter, Anthropic, and OpenAI behind one trait,
over the crate's own HTTP+SSE client. A provider that is down or rate-limited
falls back to the next configured one; failures are classified so a caller can
tell a retryable transport error from a terminal one.

**Extensibility, in-process and out.** Implement the `Tool` trait for something
your program already does, or point the harness at MCP servers over stdio or
streamable HTTP. Skills are markdown instruction files that shape how the agent
approaches a class of task, with no Rust at all.

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
the workspace survives between runs.

**Observation and replay.** Register an observer and be called as the run
happens — steps, tool calls, approvals, refusals, spend draws, retries,
fallbacks, outcomes — instead of polling the store. A recorded provider replays a
case so it runs identically twice.

**Documents and images**, behind opt-in features: spreadsheets, Word, PowerPoint
text, PDF, and barcode decoding, each gated on the real path the model named; and
image passthrough to any provider whose model accepts one.

**Git**, as fixed-argv built-ins: status, diff, log, add, and commit, so a run
ends as a reviewable commit rather than a working tree someone has to
reconstruct. The model supplies paths and a message, never a subcommand or a
flag, so push, fetch, reset and rebase are unreachable by construction.

**Durable conversations.** `Session::open(store, root)` holds a conversation
instead of firing one task: turns that read the turns before them, text streamed
to an observer as the model produces it, a mid-turn steer or an interrupt honoured
at the next step boundary, and a tree an operator can branch from any earlier turn.
A turn is a run, so every turn is budgeted, policy-bounded, checkpointed and
resumable — see the [sessions guide](docs/guide/sessions.md).

**Configuration in a file.** `Config::discover(root)` reads one `io.toml` across
four scopes — the crate's defaults, a user file, a committed project file, and a
gitignored local one — and projects it onto the typed API: a `Policy`, a
`SandboxConfig`, the run budgets, the toolchain commands, a price table, and MCP
servers. `${env:...}` and `${file:...}` keep a credential out of the file, an
unknown key is an error rather than a shrug, and nothing is loaded implicitly:
the caller reads the file, before the run, once.

## Guides

| Guide | What it covers |
| --- | --- |
| [Permissions and approval](docs/guide/permissions.md) | Layered rules, what asks and what is refused, deferring past process exit |
| [Command execution](docs/guide/command-execution.md) | Running a project's own toolchain, checked on the whole argv — and the bound that is not there |
| [Language support](docs/guide/language-support.md) | Toolchain detection, a criterion in any language, migrating off the Rust-specific gates |
| [Verification](docs/guide/verification.md) | The criteria, execution-based gates, and exactly what a pass proves |
| [Agent composition](docs/guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](docs/guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [Durable runs](docs/guide/durable-runs.md) | Checkpoints, resume, approvals that survive a restart |
| [MCP and network egress](docs/guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy does not govern |
| [Tools and skills](docs/guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery and its boundary |
| [Context and memory](docs/guide/context-and-memory.md) | Per-turn assembly, compaction, invalidation, durable memory |
| [Resilience](docs/guide/resilience.md) | Failure classification, retry, provider fallback, stall detection |
| [Observability and replay](docs/guide/observability.md) | Observers, events, outcome records, deterministic replay |
| [Sessions](docs/guide/sessions.md) | Durable conversations: a turn is a run, token streaming, steering, branching |
| [Agency](docs/guide/agency.md) | A visible plan, a question about intent, named agents, prompt templates |
| [Web search and fetch](docs/guide/web.md) | Provider-executed lookups, the three vendor translations, citations, and where the boundary is not |
| [Configuration](docs/guide/configuration.md) | One `io.toml`, four layered scopes, projected onto the typed API |
| [Accounting](docs/guide/accounting.md) | Per-call rows, cache and reasoning tokens, latency, derived cost |
| [Documents](docs/guide/documents.md) | Spreadsheets, Word, PowerPoint, PDF, barcodes — and what was cut |
| [Images and git](docs/guide/images-and-git.md) | Image passthrough and the fixed-argv git built-ins |

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
| Linux | Native, namespaces and rlimits |
| Windows | Native resource containment (memory, CPU, process count, tree kill); no filesystem or network boundary |

The full suite runs on all three in CI.

## Stability

The crate is **pre-1.0 and stays pre-1.0** until its owner says otherwise. A
minor release may break the public contract — SemVer permits it below 1.0, and
this project uses it. What you can rely on is not that a break will not happen,
but that when it does it is marked in [CHANGELOG.md](CHANGELOG.md) with a
migration note saying what to write instead, and that a renamed or removed item
goes through a deprecation cycle rather than vanishing between two releases.

[docs/CONTRACT.md](docs/CONTRACT.md) states the whole of it.

## Part of initorigin

`IO Harness` is one of the [initorigin](https://github.com/initorigin) products:

| Product | What it is | Status |
| --- | --- | --- |
| [io-harness](https://github.com/initorigin/io-harness) | The Rust agent harness (the center product) | Released |
| io-cli | Terminal app on io-harness | In development |
| io-studio | Desktop coding studio on io-harness | In development |

io-harness is the only public repository today, which is why it is the only one
linked. io-cli and io-studio open when they release.

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md). Work branches from `develop`, lands via
PR, and every user-facing change updates [CHANGELOG.md](CHANGELOG.md). Releases
follow [docs/RELEASE_PROCESS.md](docs/RELEASE_PROCESS.md).

## Security

Report vulnerabilities per [SECURITY.md](SECURITY.md).

## License

Apache-2.0. Copyright 2026 Aakash Pawar (InitOrigin). See [LICENSE](LICENSE) and
[NOTICE](NOTICE).
