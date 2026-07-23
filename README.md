# IO Harness

> A Rust agent harness that runs AI agents from a typed task contract to a checked result.

The shared engine every initorigin app (io-cli, io-studio) and io-eval build on.

**Type:** Rust library crate
**Stack:** Rust · cargo · tokio · rusqlite · rmcp · own HTTP+SSE provider client
**License:** Apache-2.0
**Status:** Pre-release. v0.1 shipped the single-agent file-edit loop (filesystem tool, OpenRouter provider, deterministic verify, rusqlite audit). v0.2 adds step/time/cost budgets, retry with escalation, a full trace, resumable runs, and execution-based verification that compiles the produced file so a substring stub cannot pass. v0.3 adds repository-wide work — `grep` and `find` tools over a workspace and multi-file edits in one run — and two more providers (Anthropic and OpenAI) behind the same provider-agnostic surface, selected at run construction.

## Capabilities

- Task contract — goal, constraints, expected output, success criteria
- Context construction — feed the model only relevant, current, trusted info
- Tool layer — narrow, typed actions the agent invokes
- Orchestration loop — observe, reason, act, check, stop
- State and memory — progress, intermediate results, decisions (rusqlite)
- Verification layer — tests, schemas, read-backs confirm the task is done
- Permissions and guardrails — what the agent may access, change, send, spend
- Recovery and retry — retries, fallbacks, replanning, escalation
- Stop conditions and budgets — cap steps, time, cost, retries, risky actions
- Observability and tracing — record prompts, decisions, tool calls, cost
- Human approval layer — review before sensitive or irreversible actions
- Providers — OpenRouter first, then Anthropic and OpenAI (own HTTP+SSE client)
- Agent composition — spawn and nest many agents (100+) with shared context
- Long-running autonomous tasks — 24h+ with no user input
- Ephemeral local code-exec sandboxes — write, run, capture, destroy
- Built-in tools — filesystem, git, grep, find
- Office and document tools — Word/Excel/PowerPoint/PDF create/edit/delete, PDF watermark, PDF form fill, OCR, barcode/QR read and generate
- Media — image and video passthrough when the model supports it
- Extensibility — MCP (rmcp), plugins, skills

See [docs/CAPABILITIES.md](docs/CAPABILITIES.md) for detail and
[docs/CONTRACT.md](docs/CONTRACT.md) for the public contract.

## Usage (v0.3)

Hand the harness a task contract; it runs the loop, verifies the result, records
every step to rusqlite, and stops on success or a budget. A single-file task uses
the filesystem tool; a workspace task lets the agent `grep`/`find` across a
repository and edit several files. Pick any of three providers at construction.
Everything else in **Capabilities** is roadmap.

### 1. Add the crate

```toml
[dependencies]
io-harness = "0.3"
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
        // that only contains the right substring fails the gate.
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
    // Success | StepCapReached | TimeBudgetExceeded | CostBudgetExceeded
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
- `Verification::RustTestPasses { test_src }` — appends `test_src` and passes only if the test binary passes.

Compilation runs locally in a throwaway temp dir (removed afterwards) and touches
no network.

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

Or run it live end to end: `cargo run --example edit_file`.

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
