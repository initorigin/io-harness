//! A live run of an agent tree: a parent decomposes a task, spawns contained
//! sub-agents over one shared workspace, and composes their results back — under
//! a `Containment` that caps how many agents may exist, how many run at once, how
//! deep they nest, and what the whole tree may spend.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example subagents
//! ```
//!
//! The parent is asked to produce two files by delegating each to a sub-agent.
//! One `Containment` cap (max_total_agents) is deliberately tight, so a third
//! spawn is refused end to end — the refusal is a tool result the parent adapts
//! to, and it lands in the trace attributed to the agent cap.

use io_harness::{
    run_tree, ApproveAll, Containment, Policy, RunOutcome, Store, TaskContract, Verification,
};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;

    // The parent succeeds only when both delegated files exist and combine.
    let contract = TaskContract::workspace(
        "You are a coordinator. Do NOT write files yourself. Use the spawn_agent \
         tool twice: spawn one sub-agent whose goal is to create the file a.txt \
         containing the word ALPHA (verify_file a.txt, verify_contains ALPHA), \
         and another whose goal is to create b.txt containing BETA (verify_file \
         b.txt, verify_contains BETA). After both sub-agents finish, write \
         summary.txt containing the word DONE.",
        dir.path(),
        Verification::WorkspaceFileContains { file: "summary.txt".into(), needle: "DONE".into() },
    )
    .with_max_steps(8);

    // Inherit-and-narrow: children get this policy and can only tighten it.
    let policy = Policy::permissive();

    // Two children plus the root fit; a third spawn is refused. Two at a time,
    // one level of nesting, a generous token ceiling for the whole tree.
    let containment = Containment::new(3, 2, 1, 200_000);

    let store = Store::open(dir.path().join("runs.db"))?;
    let result = run_tree(&contract, &provider, &store, &policy, &ApproveAll, &containment).await?;

    println!("outcome: {:?}", result.outcome);
    println!("\ntree:");
    for child in store.children(result.run_id)? {
        println!("  child run {child}: depth {}", store.depth(child)?);
    }
    for e in store.agent_events(result.run_id)? {
        match e.kind.as_str() {
            "spawn" => println!("  spawn -> run {:?} ({:?})", e.child_run_id, e.detail),
            "spawn_refused" => println!("  spawn REFUSED (cap: {:?})", e.detail),
            "budget_draw" => println!("  drew {:?} tokens, {:?} left", e.tokens, e.remaining),
            _ => {}
        }
    }

    for f in ["a.txt", "b.txt", "summary.txt"] {
        let p = dir.path().join(f);
        println!("  {f}: {}", if p.exists() { "present" } else { "absent" });
    }

    if !matches!(result.outcome, RunOutcome::Success { .. }) {
        eprintln!("note: the parent did not reach success; see the trace above");
    }
    Ok(())
}
