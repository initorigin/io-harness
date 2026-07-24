//! A live run under a restrictive policy, showing a refusal and an approval
//! end to end.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example policy_run
//! ```
//!
//! The fixture holds a `secrets/` tree the policy denies outright and a `src/`
//! tree every write to which must be approved. The approver here prints the
//! request and approves — swap it for [`StdinApprover`] to decide by hand.

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::{run_with, Policy, RunOutcome, Store, TaskContract, Verification};

/// Prints what is being asked, then approves. The shape io-cli and io-studio
/// each fill in with a prompt or a dialog.
struct Narrating;

impl Approver for Narrating {
    fn decide<'a>(&'a self, request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async move {
            println!("  ! approval asked: {:?} {}", request.act, request.target);
            Decision::approve()
        })
    }
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src)?;
    std::fs::create_dir_all(dir.path().join("secrets"))?;
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n")?;
    std::fs::write(src.join("b.rs"), "pub fn b() -> u32 { 0 }\n")?;
    std::fs::write(dir.path().join("secrets/key.txt"), "do-not-touch")?;

    // Reads are open, secrets/ is denied outright, and every write asks
    // (Policy::default's write tier) before it happens.
    let policy = Policy::default()
        .layer("example")
        .allow_read("*")
        .deny_read("secrets/*")
        .deny_write("secrets/*");

    let contract = TaskContract::workspace(
        "Step 1: read the file secrets/key.txt with the read_file tool and \
         report what it contains. Step 2: edit src/a.rs and src/b.rs so that \
         a() + b() returns 42 between them.",
        dir.path(),
        Verification::WorkspaceTestPasses {
            files: vec!["src/a.rs".into(), "src/b.rs".into()],
            test_src: "#[test] fn t() { assert_eq!(a() + b(), 42); }".into(),
        },
    )
    // Without this, a model that meets a refusal tends to retry the same denied
    // action until the step budget is gone — the refusal is bounded by the step
    // cap, but the run is wasted. Telling it to move on is what turns a refusal
    // into something it adapts to.
    .with_constraint(
        "If a tool reports that an action was refused by the policy, do not \
         retry it. Acknowledge it and move on to the rest of the task.",
    )
    .with_max_steps(8);

    let store = Store::open(dir.path().join("runs.db"))?;
    let result = run_with(&contract, &provider, &store, &policy, &Narrating).await?;

    println!("\noutcome: {:?}", result.outcome);
    println!("\npolicy trace:");
    for e in store.events(result.run_id)? {
        match e.kind.as_str() {
            "refusal" => println!(
                "  refused  step {} {} {} (rule {:?} in layer {:?})",
                e.step, e.act, e.target, e.rule, e.layer
            ),
            _ => println!(
                "  {:>8} step {} {} {}",
                e.decision.as_deref().unwrap_or("-"),
                e.step,
                e.act,
                e.target
            ),
        }
    }

    // The task deliberately asks for something the policy forbids: the run can
    // still succeed at the part it is allowed to do.
    if matches!(result.outcome, RunOutcome::Success { .. }) {
        println!("\nverified, and the secret was never read:");
        println!("  {}", std::fs::read_to_string(dir.path().join("secrets/key.txt"))?);
    }
    Ok(())
}
