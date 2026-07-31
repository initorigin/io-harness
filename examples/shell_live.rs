//! Live OpenRouter run against the `shell` tool — OP1, the criterion the test
//! suite cannot satisfy.
//!
//! The suite proves the mechanism against a scripted provider: the lines it runs
//! are lines this repository's author wrote. Only a live run proves the claim
//! that matters, which is that a *model* composes a command line, the harness
//! parses what it actually wrote, and the boundary holds against it.
//!
//! The task is chosen so that a single `exec` argv cannot do it. The agent has to
//! compose a pipeline or a sequence with a redirect, because the answer it is
//! asked for is a count written to a file — and it has to do that under a policy
//! that allows the reading tools and denies the writing ones, so a refusal is
//! reachable if it reaches for the wrong thing.
//!
//! ```text
//! export OPENROUTER_API_KEY=sk-or-...
//! export OPENROUTER_MODEL=anthropic/claude-sonnet-4
//! cargo run --example shell_live
//! ```
//!
//! Needs network access to OpenRouter and nothing else: every command the policy
//! allows is a POSIX text utility, so there is no toolchain to install and
//! nothing is fetched.

use std::time::Duration;

use io_harness::{
    run_with, ApproveAll, OpenRouter, Policy, RunOutcome, Store, TaskContract, Verification,
};

const NOTES: &str = "alpha ERROR disk full
beta ok
gamma ERROR timeout
delta ok
epsilon ERROR disk full
zeta ok
";

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let dir = tempfile::tempdir().expect("a workspace");
    std::fs::write(dir.path().join("notes.txt"), NOTES).expect("the fixture");

    let provider = OpenRouter::from_env()?;
    let store = Store::open(dir.path().join("trace.db"))?;

    // The criterion is the count, in a file the agent has to create. It cannot be
    // satisfied by answering in prose, and it cannot be satisfied by one argv:
    // counting matching lines and putting the number in a file is a pipeline and
    // a redirect.
    let contract = TaskContract::workspace(
        "notes.txt has one record per line. Count how many lines contain the word ERROR \
         and write just that number into a file called count.txt in the workspace root. \
         Use the shell tool.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "count.txt".into(),
        needle: "3".into(),
    })
    .with_max_steps(8)
    .with_exec_timeout(Duration::from_secs(60));

    // Reading is open and the text utilities are allowed. Writing is allowed only
    // for `count.txt` — every other path in the workspace is denied, so a redirect
    // anywhere else is refused by the same path check `write_file` takes.
    let policy = Policy::default()
        .layer("live")
        .allow_read("*")
        .allow_write("count.txt")
        .deny_write("notes.txt")
        .allow_exec("grep*")
        .allow_exec("wc*")
        .allow_exec("cat*")
        .allow_exec("sort*")
        .allow_exec("uniq*")
        .allow_exec("tr*")
        .allow_exec("head*")
        .allow_exec("tail*")
        .deny_exec("rm*")
        .deny_exec("curl*")
        .deny_exec("git*");

    println!("workspace: {}", dir.path().display());
    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll).await?;
    println!("outcome: {:?}", result.outcome);

    // The trace is the evidence, not the console. Read it back the way an
    // operator would, from the store rather than from what the run returned.
    println!("\n--- what the model actually ran ---");
    for step in store.steps(result.run_id)? {
        if !step.tool_call.is_empty() {
            // The store keeps the tool call's *arguments*, not its name — the name
            // is in the step's decision. So `line` is at the top level here.
            let args: serde_json::Value = serde_json::from_str(&step.tool_call).unwrap_or_default();
            match args.get("line").and_then(|v| v.as_str()) {
                Some(l) => println!("  step {}: {l}", step.step),
                None => println!("  step {}: {}", step.step, step.tool_call),
            }
        }
        println!("      -> {}", step.decision);
    }

    // Every policy decision the line produced, one row per sub-command and per
    // redirect target. This is the criterion's actual content: a pipeline is not
    // checked as a string, it is checked stage by stage.
    println!("\n--- policy decisions ---");
    for ev in store.events(result.run_id)? {
        println!(
            "  step {} {} {} {:?} (rule {:?}, layer {:?})",
            ev.step, ev.kind, ev.act, ev.target, ev.rule, ev.layer
        );
    }

    let produced = std::fs::read_to_string(dir.path().join("count.txt"));
    println!("\ncount.txt = {produced:?}");
    println!(
        "verified: {}",
        matches!(result.outcome, RunOutcome::Success { .. })
    );
    Ok(())
}
