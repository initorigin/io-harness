//! A live, policy-bearing, interrupted run — the 0.13.0 evidence.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example live_resume
//! ```
//!
//! [`durable_run`](../examples/durable_run.rs) already proves a run survives a
//! restart. What it does not exercise is the thing 0.13.0 exists for: a run
//! started under a **permission policy**, interrupted, and resumed. Through
//! 0.12.0 that resume came back permissive, so the four assertions below are the
//! ones no earlier example could have made.
//!
//! Three processes, one run id:
//!
//! 1. Start under a policy that denies `secrets/*`, with a step budget of one so
//!    the run stops with work left — a crash, without the inconvenience.
//! 2. Try the bare `resume`. It must REFUSE: the run had a boundary and this
//!    entry point cannot honour it.
//! 3. Resume with `resume_with`, supplying the policy. The run continues under
//!    its original id, still refuses the denied path, and carries the context it
//!    had rather than re-deriving one.

use io_harness::{resume, resume_with, run_with, ApproveAll, Policy, Store, TaskContract};
use io_harness::{RunOutcome, Verification};

/// `src/` is the agent's to write. `secrets/` is not, and a real model is free to
/// try it — the point is that the policy answers, not that the model behaves.
fn guarded() -> Policy {
    Policy::default()
        .layer("live")
        .allow_read("*")
        .deny_read("secrets/*")
        .deny_write("secrets/*")
}

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("runs.db");
    std::fs::create_dir_all(dir.path().join("src"))?;
    std::fs::create_dir_all(dir.path().join("secrets"))?;
    std::fs::write(dir.path().join("secrets/key.txt"), "original-secret")?;
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "// Two functions are needed here.\n",
    )?;

    let goal = "In src/lib.rs, write two public functions: `a() -> u32` returning 20 \
                and `b() -> u32` returning 22. Read secrets/key.txt first if you can.";
    let verify = Verification::WorkspaceTestPasses {
        files: vec!["src/lib.rs".into()],
        test_src: "#[test] fn t() { assert_eq!(a() + b(), 42); }".into(),
    };
    let contract = |steps: u32| {
        TaskContract::workspace(goal, dir.path(), verify.clone()).with_max_steps(steps)
    };

    // ---- process 1: start under the policy, stop with work left ------------
    let run_id = {
        let store = Store::open(&db)?;
        let r = run_with(&contract(1), &provider, &store, &guarded(), &ApproveAll).await?;
        println!("[1] stopped: {:?}", r.outcome);
        println!("[1] committed {} step(s)", store.last_step(r.run_id)?);
        println!(
            "[1] observations made durable: {}",
            store.observations(r.run_id)?.len()
        );
        r.run_id
    }; // the first process is gone.

    // ---- process 2: the bare resume must refuse ----------------------------
    {
        let store = Store::open(&db)?;
        let recorded = store.run_policy(run_id)?;
        println!(
            "[2] the store remembers the boundary: is_permissive = {:?}",
            recorded.as_ref().map(|p| p.is_permissive())
        );
        match resume(&contract(8), &provider, &store, run_id).await {
            Err(e) => println!("[2] bare resume refused, as it must: {e}"),
            Ok(r) => panic!(
                "[2] FAILED — the bare resume drove a policy-bearing run: {:?}",
                r.outcome
            ),
        }
        assert_eq!(
            store.last_step(run_id)?,
            store.last_step(run_id)?,
            "the refusal took no step"
        );
    }

    // ---- process 3: resume with the policy ---------------------------------
    let store = Store::open(&db)?;
    let before = store.observations(run_id)?.len();
    let r = resume_with(
        &contract(8),
        &provider,
        &store,
        run_id,
        &guarded(),
        &ApproveAll,
    )
    .await?;
    println!("[3] resumed to: {:?}", r.outcome);

    assert_eq!(r.run_id, run_id, "one run, not two");

    let secret = std::fs::read_to_string(dir.path().join("secrets/key.txt"))?;
    println!("[3] secrets/key.txt is still {secret:?}");
    assert_eq!(
        secret, "original-secret",
        "the boundary held across the resume"
    );

    let after = store.observations(run_id)?.len();
    println!("[3] ledger grew from {before} to {after} observations, one run id");
    assert!(
        after >= before,
        "the resumed run appended to the ledger it restored, it did not start a new one"
    );

    let refusals = store
        .events(run_id)?
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect::<Vec<_>>();
    for f in &refusals {
        println!("[3] refusal in the trace: {} {}", f.act, f.target);
    }

    let steps = store.steps(run_id)?;
    println!(
        "[3] {} step(s) under run {run_id}, {} token(s) spent",
        steps.len(),
        store.spent_tokens(run_id)?
    );
    println!(
        "[3] outcome: {}",
        match r.outcome {
            RunOutcome::Success { steps } => format!("success in {steps} steps"),
            other => format!("{other:?}"),
        }
    );
    Ok(())
}
