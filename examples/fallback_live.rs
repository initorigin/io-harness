//! Live run behind a failing primary.
//!
//! The primary is a fixture that always returns 503 — a retryable, provider-side
//! failure, which is the only kind `Fallback` falls through on. The secondary is a
//! real OpenRouter provider, so the task is genuinely completed by a real model
//! after a real fall-over decision.
//!
//! A deliberately broken *real* primary would not exercise this: a bad key is
//! `Auth` and a bad model is `Request`, and neither falls over on purpose — trying
//! the same rejected request at another vendor is two failures instead of one.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example fallback_live
//! ```

// The Rust-specific `Verification` variants are deprecated in 0.17.0 and removed
// in 0.18.0. They are kept here deliberately: these files are what F10 asserts
// still work, and the fixtures are loose `.rs` files rather than cargo projects,
// so `Verification::Command { argv: ["cargo", "test"], .. }` — the replacement —
// has no project to run in. See docs/guide/verification.md for the migration.
#![allow(deprecated)]

use io_harness::provider::{CompletionRequest, CompletionResponse, Fallback, Provider};
use io_harness::{run, Error, OpenRouter, Store, TaskContract, Verification};

/// Always 503. Stands in for a provider mid-deploy.
struct Down;

impl Provider for Down {
    async fn complete(
        &self,
        _request: CompletionRequest,
    ) -> io_harness::Result<CompletionResponse> {
        Err(Error::provider_status(503, None, "service unavailable"))
    }
    fn name(&self) -> &str {
        "down-fixture"
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-fallback-live");
    std::fs::remove_dir_all(&root).ok();
    let src = root.join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n")?;

    let contract = TaskContract::workspace(
        "Edit src/a.rs so a() == 7.",
        &root,
        Verification::WorkspaceTestPasses {
            files: vec!["src/a.rs".into()],
            test_src: "#[test] fn t() { assert_eq!(a(), 7); }".into(),
        },
    )
    .with_max_steps(6)
    .with_token_budget(200_000);

    let provider = Fallback::new(Down, OpenRouter::from_env()?);
    let store = Store::open(root.join("runs.db"))?;

    println!("provider: {}", provider.name());
    let result = run(&contract, &provider, &store).await?;
    println!("outcome: {:?}", result.outcome);
    println!("run provider label: {:?}", store.provider(result.run_id)?);
    println!("\nwhich provider served each step:");
    for e in store.context_events(result.run_id)? {
        if e.kind == "served" {
            println!("  step {}: {}", e.step, e.detail.unwrap_or_default());
        }
    }
    println!(
        "\nfinal file:\n{}",
        std::fs::read_to_string(src.join("a.rs"))?
    );
    Ok(())
}
