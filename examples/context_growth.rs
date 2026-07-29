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

// The Rust-specific `Verification` variants are deprecated in 0.17.0 and removed
// in 0.18.0. They are kept here deliberately: these files are what F10 asserts
// still work, and the fixtures are loose `.rs` files rather than cargo projects,
// so `Verification::Command { argv: ["cargo", "test"], .. }` — the replacement —
// has no project to run in. See docs/guide/verification.md for the migration.
#![allow(deprecated)]

use io_harness::{run, OpenRouter, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    // A four-file workspace, so the agent greps, reads and writes enough times
    // for the observation log to matter.
    let root = std::env::temp_dir().join("io-harness-context-growth");
    std::fs::remove_dir_all(&root).ok();
    let src = root.join("src");
    std::fs::create_dir_all(&src).ok();
    // Each file carries several KB of filler, so a read is a real observation and
    // the log is big enough for re-sending it to cost something measurable.
    for name in ["a", "b", "c", "d"] {
        let filler: String = (0..120)
            .map(|i| format!("// {name} line {i}: padding so a read costs real context\n"))
            .collect();
        std::fs::write(
            src.join(format!("{name}.rs")),
            format!("{filler}pub fn {name}() -> u32 {{ 0 }}\n"),
        )?;
    }

    let contract = TaskContract::workspace(
        "Edit the four source files so a() + b() + c() + d() == 100. Read every file \
         before you change it, and read a file again after you have written it to \
         confirm what it now contains.",
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
