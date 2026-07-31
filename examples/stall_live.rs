//! Live run that cannot make progress, and is stopped for it.
//!
//! The task asks the agent to edit a file the policy forbids it to write. It will
//! read, try, be refused, and try again — which is exactly the shape of the stall
//! the 0.10.0 live evidence recorded: steps that change nothing in the workspace
//! while repeating a call already made.
//!
//! Before 0.11 this ran to its step cap. Now it is told once to change approach and
//! then stopped, with both decisions in the trace.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example stall_live
//! ```

use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-stall-live");
    std::fs::remove_dir_all(&root).ok();
    let src = root.join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n")?;

    let contract = TaskContract::workspace("Edit src/a.rs so a() == 7.", &root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "src/a.rs".into(),
            needle: "7".into(),
        })
        .with_max_steps(20)
        .with_token_budget(200_000);

    // Reads allowed, writes denied. The agent can look all it likes and can never
    // move the workspace, which is the condition stall detection exists for.
    let policy = Policy::default()
        .layer("read-only")
        .allow_read("*")
        .deny_write("*");

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("outcome: {:?}", result.outcome);
    println!("step cap was: {}", contract.max_steps);
    println!("stall policy: {:?}", contract.stall);

    println!("\nresilience rows:");
    for e in store.context_events(result.run_id)? {
        if e.kind == "stalled" {
            println!("  step {}: {}", e.step, e.detail.unwrap_or_default());
        }
    }
    println!("\nsteps taken:");
    for s in store.steps(result.run_id)? {
        println!("  {:>2}: {}", s.step, s.decision);
    }
    Ok(())
}
