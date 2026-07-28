# IO Harness

[![crates.io](https://img.shields.io/crates/v/io-harness.svg)](https://crates.io/crates/io-harness)
[![docs.rs](https://img.shields.io/docsrs/io-harness)](https://docs.rs/io-harness)
[![CI](https://github.com/initorigin/io-harness/actions/workflows/ci.yml/badge.svg)](https://github.com/initorigin/io-harness/actions/workflows/ci.yml)
[![License: Apache-2.0](https://img.shields.io/crates/l/io-harness.svg)](LICENSE)
[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue.svg)](Cargo.toml)

**Run an AI agent from a typed task contract to a verified result — in your own
process, under a permission boundary you control.**

You hand it a contract: the task, the file or workspace it may touch, and the
criterion that decides whether the work is done. It runs the loop — observe,
reason, act, verify, stop — and hands back a result that a check actually
passed, with every step in a SQLite trace you can read afterwards.

## Quickstart

```toml
[dependencies]
io-harness = "0.16"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

```rust,no_run
use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = OpenRouter::from_env()?; // OPENROUTER_API_KEY + OPENROUTER_MODEL
    let store = Store::memory()?;

    let contract = TaskContract::new(
        "add a hello function returning 42",
        "src/hello.rs",
        // Not a substring check: this compiles the file the agent produced.
        Verification::CompilesRust,
    );

    // src/ is writable; secrets/ is refused outright and never reaches a human.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("src/*")
        .deny_read("secrets/*");

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("{:?}", result.outcome);
    Ok(())
}
```

**Requires Rust 1.88** or later. The floor comes from `rmcp`, which publishes no
`rust-version` of its own, so cargo cannot catch it at resolve time — on 1.87 the
build fails inside that dependency rather than here.

## What it does

**The loop.** A [`TaskContract`] names the goal, the subject, and the
verification criterion. Single-file mode edits one file. Workspace mode gives the
agent `grep`, `find`, `read_file` and `write_file` across a repository root.

**Verification that executes.** A criterion can compile the produced file or run
a test against it, in a sandbox, so a substring stub cannot pass. What a passing
gate does and does not prove is stated exactly in the
[verification guide](docs/guide/verification.md) — it is narrower than it reads.

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

## Guides

| Guide | What it covers |
| --- | --- |
| [Permissions and approval](docs/guide/permissions.md) | Layered rules, what asks and what is refused, deferring past process exit |
| [Verification](docs/guide/verification.md) | The criteria, execution-based gates, and exactly what a pass proves |
| [Agent composition](docs/guide/composition.md) | Sub-agents, inherit-and-narrow containment, the shared ledger |
| [Execution sandbox](docs/guide/sandbox.md) | Backends per platform, resource caps, the portable floor |
| [Durable runs](docs/guide/durable-runs.md) | Checkpoints, resume, approvals that survive a restart |
| [MCP and network egress](docs/guide/mcp-and-network.md) | Stdio and HTTP servers, `Act::Net`, what the policy does not govern |
| [Tools and skills](docs/guide/tools-and-skills.md) | The `Tool` trait, the toolbox, skill discovery and its boundary |
| [Context and memory](docs/guide/context-and-memory.md) | Per-turn assembly, compaction, invalidation, durable memory |
| [Resilience](docs/guide/resilience.md) | Failure classification, retry, provider fallback, stall detection |
| [Observability and replay](docs/guide/observability.md) | Observers, events, outcome records, deterministic replay |
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
| Windows | Portable floor only — the Job Object backend is designed, not implemented, and only the wall clock is enforced |

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
