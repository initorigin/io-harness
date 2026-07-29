//! Live OpenRouter run: edit a file to meet a spec.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example edit_file
//! ```

use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    tracing_subscriber_init();

    let dir = std::env::temp_dir().join("io-harness-example");
    std::fs::create_dir_all(&dir).ok();
    let file = dir.join("hello.rs");

    let contract = TaskContract::new(
        "Create a Rust function `hello` that returns the u32 value 42.",
        &file,
        // Execution-based: the file must compile. A substring stub (the 0.1.0
        // I01 failure) cannot satisfy this, which is the whole reason 0.2.0
        // introduced an execution gate.
        Verification::Command {
            argv: vec![
                "rustc".into(),
                "--edition".into(),
                "2021".into(),
                "--crate-type".into(),
                "lib".into(),
                "hello.rs".into(),
            ],
            expect_exit: 0,
        },
    )
    .with_max_steps(4)
    .with_token_budget(200_000);

    let provider = OpenRouter::from_env()?;
    let store = Store::open(dir.join("runs.db"))?;

    let result = run(&contract, &provider, &store).await?;
    println!("outcome: {:?}", result.outcome);
    println!("file: {}", file.display());
    println!("--- contents ---\n{}", std::fs::read_to_string(&file)?);

    for step in store.steps(result.run_id)? {
        println!("step {}: {}", step.step, step.decision);
    }
    Ok(())
}

fn tracing_subscriber_init() {
    // ponytail: examples don't ship a log subscriber; env_logger/tracing-subscriber
    // is the consumer's choice. This is a no-op placeholder for clarity.
}
