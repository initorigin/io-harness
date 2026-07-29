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

    // A real cargo project: the gate is the project's own `cargo test` since
    // 0.18.0 removed the criteria that compiled loose `.rs` files.
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )?;
    std::fs::write(
        src.join("lib.rs"),
        "pub mod a;\npub mod b;\npub mod c;\npub mod d;\n\
         #[test] fn t() { assert_eq!(a::a() + b::b() + c::c() + d::d(), 100); }\n",
    )?;

    let contract = TaskContract::workspace(
        "Edit the four source files so a() + b() + c() + d() == 100. Read every file \
         before you change it, and read a file again after you have written it to \
         confirm what it now contains.",
        &root,
        Verification::Command {
            argv: vec!["cargo".into(), "test".into(), "--offline".into()],
            expect_exit: 0,
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
