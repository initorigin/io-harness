# IO Harness

> A production-grade Rust agent harness that runs AI agents from a typed task contract to a verified result.

The shared engine every initorigin app (io-cli, io-studio) and io-eval build on.

**Type:** Rust library crate
**Stack:** Rust · cargo · tokio · rusqlite · rmcp · own HTTP+SSE provider client
**License:** Apache-2.0
**Status:** Pre-release. Product is BRAINSTORMED; v0.1 targets a single-agent task through the orchestration loop with the filesystem tool and the OpenRouter provider, verified end to end.

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

## Usage (v0.1)

v0.1 ships one vertical slice: hand the harness a task contract to edit one file
to meet a spec; it runs the loop with the filesystem tool and the OpenRouter
provider, verifies the file deterministically, persists every step to rusqlite,
and stops on success or a step cap. Everything else in **Capabilities** is
roadmap.

### 1. Add the crate

```toml
[dependencies]
io-harness = "0.1"
tokio = { version = "1", features = ["rt-multi-thread", "macros"] }
```

### 2. Provide an OpenRouter key

Credentials are read from the environment and never logged. No default model is
guessed, so set the slug explicitly:

```bash
export OPENROUTER_API_KEY=sk-or-...
export OPENROUTER_MODEL=anthropic/claude-sonnet-4   # any OpenRouter model slug
```

### 3. Run one file-edit task

```rust
use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let contract = TaskContract::new(
        "add a `hello` function that returns 42",
        "src/hello.rs",
        Verification::FileContains("fn hello".into()),
    );

    let provider = OpenRouter::from_env()?;
    let store = Store::open("runs.db")?;

    let result = run(&contract, &provider, &store).await?;
    println!("{:?}", result.outcome); // Success { steps } | StepCapReached { steps }

    for step in store.steps(result.run_id)? {
        println!("step {}: {}", step.step, step.decision);
    }
    Ok(())
}
```

Or run it live end to end: `cargo run --example edit_file`.

Verification is deterministic on purpose — a model-judged check can pass a file
that does not meet the spec. v0.1 offers `Verification::FileContains(String)` and
`Verification::FileEquals(String)`.

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
