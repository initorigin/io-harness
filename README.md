# IO Harness

> A Rust agent harness that runs AI agents from a typed task contract to a checked result.

The shared engine every initorigin app (io-cli, io-studio) and io-eval build on.

**Type:** Rust library crate
**Stack:** Rust · cargo · tokio · rusqlite · rmcp · own HTTP+SSE provider client
**License:** Apache-2.0
**Status:** Pre-release. v0.1 shipped the single-agent file-edit loop (filesystem tool, OpenRouter provider, deterministic verify, rusqlite audit). v0.2 adds step/time/cost budgets, retry with escalation, a full trace, resumable runs, and execution-based verification that compiles the produced file so a substring stub cannot pass.

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

## Usage (v0.2)

Hand the harness a task contract to edit one file to meet a spec; it runs the
loop with the filesystem tool and the OpenRouter provider, verifies the result,
records every step to rusqlite, and stops on success or a budget. Everything
else in **Capabilities** is roadmap.

### 1. Add the crate

```toml
[dependencies]
io-harness = "0.2"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

Upgrading from 0.1: `TaskContract::new` is unchanged, so existing callers keep
compiling. `CompletionResponse` gained a `usage` field — if you implement
`Provider` yourself and build the struct literally, add `..Default::default()`.
A 0.1 rusqlite database is migrated in place on open (new trace columns are
added; a 0.1 binary still reads it).

### 2. Provide an OpenRouter key

Credentials are read from the environment and never logged. No default model is
guessed, so set the slug explicitly:

```bash
export OPENROUTER_API_KEY=sk-or-...
export OPENROUTER_MODEL=anthropic/claude-sonnet-4   # any OpenRouter model slug
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
