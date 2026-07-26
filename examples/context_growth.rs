//! Live run that measures what the harness sends the model each turn.
//!
//! Prints one line per step: the recorded prompt length in chars and the
//! estimated tokens for it. Run it on 0.9.1 and on 0.10.0 against the same
//! provider and the same contract, and the two outputs are the release's
//! before/after evidence — 0.9.1 grows with every step because the observation
//! log is re-sent whole, 0.10.0 does not.
//!
//! It uses nothing newer than the 0.9.1 public API on purpose, so the same file
//! compiles and runs on both versions.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example context_growth
//! ```

use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    // A four-file workspace, so the agent greps, reads and writes enough times
    // for the observation log to matter.
    let root = std::env::temp_dir().join("io-harness-context-growth");
    std::fs::remove_dir_all(&root).ok();
    let src = root.join("src");
    std::fs::create_dir_all(&src).ok();
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n")?;
    std::fs::write(src.join("b.rs"), "pub fn b() -> u32 { 0 }\n")?;
    std::fs::write(src.join("c.rs"), "pub fn c() -> u32 { 0 }\n")?;
    std::fs::write(src.join("d.rs"), "pub fn d() -> u32 { 0 }\n")?;

    let contract = TaskContract::workspace(
        "Edit the four source files so a() + b() + c() + d() == 100. \
         Read each file before you change it.",
        &root,
        Verification::WorkspaceTestPasses {
            files: vec![
                "src/a.rs".into(),
                "src/b.rs".into(),
                "src/c.rs".into(),
                "src/d.rs".into(),
            ],
            test_src: "#[test] fn t() { assert_eq!(a() + b() + c() + d(), 100); }".into(),
        },
    )
    .with_max_steps(20)
    .with_token_budget(400_000);

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;

    let result = run(&contract, &provider, &store).await?;
    println!("outcome: {:?}", result.outcome);
    println!("crate version: {}", env!("CARGO_PKG_VERSION"));
    println!("\nstep  prompt_chars  est_tokens  tokens_reported  decision");
    let mut prev = 0usize;
    for step in store.steps(result.run_id)? {
        let chars = step.prompt.chars().count();
        let est = chars / 4;
        let arrow = if chars > prev { "+" } else { " " };
        prev = chars;
        println!(
            "{:>4}  {:>12}{} {:>10}  {:>15}  {}",
            step.step, chars, arrow, est, step.tokens, step.decision
        );
    }
    Ok(())
}
