//! Live OpenRouter run: a repository task. The agent greps/finds across a
//! workspace and edits several files, verified together.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example edit_repo
//! ```
//!
//! Swap `OpenRouter::from_env()` for `Anthropic::from_env()` or
//! `OpenAi::from_env()` (with that provider's env vars) to run the *same*
//! contract on a different provider — nothing else changes.

use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    // A tiny two-file workspace where neither file yet satisfies the spec.
    let root = std::env::temp_dir().join("io-harness-repo-example");
    let src = root.join("src");
    std::fs::create_dir_all(&src).ok();
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n")?;
    std::fs::write(src.join("b.rs"), "pub fn b() -> u32 { 0 }\n")?;

    let contract = TaskContract::workspace(
        "Edit the two source files so a() + b() == 42.",
        &root,
        Verification::WorkspaceTestPasses {
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            test_src: "#[test] fn t() { assert_eq!(a() + b(), 42); }".into(),
        },
    )
    .with_max_steps(12)
    .with_token_budget(400_000);

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;

    let result = run(&contract, &provider, &store).await?;
    println!("outcome: {:?}", result.outcome);
    println!("provider: {:?}", store.provider(result.run_id)?);
    for f in ["src/a.rs", "src/b.rs"] {
        println!("--- {f} ---\n{}", std::fs::read_to_string(root.join(f))?);
    }
    for step in store.steps(result.run_id)? {
        println!("step {}: {}", step.step, step.decision);
    }
    Ok(())
}
