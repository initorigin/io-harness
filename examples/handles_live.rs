//! Live OpenRouter run against a process that does not finish — OP1, the
//! criterion the test suite cannot satisfy.
//!
//! The suite proves the mechanism: a handle starts, polls incrementally, and
//! dies. What it cannot prove is the thing the release is actually for — that a
//! *model*, told only that a server is misbehaving, reaches for `shell_start`
//! rather than `shell`, understands that the id it gets back is a thing it can
//! come back to, does other work in between, and cleans up after itself.
//!
//! That is a judgement about tool descriptions and observation wording, not
//! about code, and a scripted provider will agree with whatever the harness
//! wrote no matter how confusing it is. So this is a run with nothing scripted.
//!
//! The task is shaped so the foreground tool cannot do it. A server that never
//! exits would block `shell` until the exec timeout and burn the step; the only
//! way through is to start it, poll it, read what it logged, and kill it. If a
//! model can do that from the tool descriptions alone, the descriptions are
//! right. If it deadlocks on `shell` instead, they are not, and that is a
//! finding worth having before publication rather than after.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example handles_live
//! ```
//!
//! Needs `node` on PATH and no network access at all — the fixture server is
//! written here and listens on loopback.

use std::time::Duration;

use io_harness::{run_with, ApproveAll, OpenRouter, Policy, Store, TaskContract};

/// A server that starts, logs, serves nothing useful, and never exits.
///
/// It prints a startup banner and then a line a second, unbuffered, because the
/// whole point of a poll is to see output while the process is still alive. The
/// deliberate mistake is in the banner: it says it is listening on the wrong
/// port, which is a fact the agent can only discover by reading the log of a
/// process it started and left running.
const SERVER: &str = r#"const PORT = 8080;
console.log(`[boot] listening on http://127.0.0.1:${PORT + 1}`);
console.log('[boot] ready');
let n = 0;
setInterval(() => {
  n += 1;
  console.log(`[tick] ${n} request(s) served`);
}, 1000);
"#;

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let root = std::env::temp_dir().join("io-harness-handles-live");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root)?;
    std::fs::write(root.join("server.js"), SERVER)?;

    // No success criterion. This is exactly the kind of work 0.24.0's
    // positioning fix made expressible: "watch this and tell me what it says"
    // has no command that returns zero when it is done, and demanding one would
    // have meant inventing a fake gate to run the task at all.
    let contract = TaskContract::workspace(
        "server.js is a small Node server that runs until it is stopped. Start it so that it \
         keeps running, wait for it to print some of its log, and read the log to find out \
         which port it says it is listening on. Then stop it, and write the port number and \
         nothing else into port.txt. Do not read server.js to answer this — the answer has to \
         come from the running process's own output.",
        &root,
    )
    .with_max_steps(14)
    .with_token_budget(300_000);

    // Written the way an operator would: node is allowed, and the two things
    // that would reach outside this task are not.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("node*")
        .deny_exec("rm*")
        .deny_exec("curl*");

    let provider = OpenRouter::from_env()?;
    let store = Store::open(root.join("runs.db"))?;

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;

    println!("outcome:  {:?}", result.outcome);
    println!("provider: {:?}", store.provider(result.run_id)?);
    if let Some(s) = result.summary(&store)? {
        println!("spend:    {} tokens", s.tokens);
    }

    println!("\n-- what the agent did --");
    for step in store.steps(result.run_id)? {
        println!("  {:>2}. {}", step.step, step.decision);
    }

    // The handle lifecycle, read back through the public API rather than
    // scraped out of the log. This is the half a scripted test also checks; it
    // is printed here because a live run that ended with a process still alive
    // is the failure this release cares most about, and an operator reading
    // this output should be able to see that it did not.
    println!("\n-- handles --");
    let handles = store.process_handles(result.run_id)?;
    if handles.is_empty() {
        println!("  (none — the model never started one, which is the finding)");
    }
    for h in &handles {
        println!(
            "  handle {}: {:?} state={} pids={:?}",
            h.handle, h.line, h.state, h.pids
        );
        let output = store.handle_output(result.run_id, h.handle)?;
        let shown: Vec<&str> = output.lines().take(6).collect();
        for line in shown {
            println!("      | {line}");
        }
    }

    // The answer, and whether it came from the running process.
    let answer = std::fs::read_to_string(root.join("port.txt")).unwrap_or_default();
    println!("\nport.txt: {:?}", answer.trim());
    println!(
        "expected: \"8081\" — the port the banner CLAIMS, which is only in the process's output"
    );

    // Said plainly rather than left to inference: every handle this run started
    // must be finished with, and a live process outliving the run is the one
    // outcome that reaches the machine rather than the trace.
    let leaked: Vec<u64> = handles
        .iter()
        .filter(|h| h.state == "running")
        .map(|h| h.handle)
        .collect();
    if leaked.is_empty() {
        println!("no handle was left running");
    } else {
        println!("LEAKED handles still marked running: {leaked:?}");
    }

    // Not a hard failure: an example that panics on a model's judgement call is
    // an example nobody can run twice. The numbers above are the evidence.
    let _ = Duration::from_secs(0);
    Ok(())
}
