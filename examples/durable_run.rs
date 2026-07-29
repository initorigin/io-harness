//! A live, durable, unattended run: start a task, "crash" partway (stop at a
//! deliberately small step budget while the file-backed store keeps every
//! committed step), then **reopen the store in a fresh `Store` — as a restarted
//! process would — and resume** to a verified result. The resume skips the steps
//! already committed and continues one continuous run under its original id.
//!
//! ```text
//! OPENROUTER_API_KEY=... OPENROUTER_MODEL=openai/gpt-5.6-luna \
//!     cargo run --example durable_run
//! ```
//!
//! Nothing about the interrupted run is lost: the trace, the token spend, and the
//! resume point all live in the on-disk rusqlite store, not in the process.

use io_harness::{resume, run, RunStatus, Store, TaskContract, Verification};

#[tokio::main]
async fn main() -> io_harness::Result<()> {
    let provider = io_harness::OpenRouter::from_env()?;
    let dir = tempfile::tempdir()?;
    let db = dir.path().join("runs.db");
    let file = dir.path().join("hello.rs");

    let goal = "Write a Rust file with a public function `hello` returning the u32 42.";
    let verify = Verification::Command {
        // 0.18.0: the Rust-specific criteria are gone. A compiler is invoked by
        // argv, in the edited file's own directory, like any other language's.
        argv: vec![
            "rustc".into(),
            "--edition".into(),
            "2021".into(),
            "--crate-type".into(),
            "lib".into(),
            "hello.rs".into(),
        ],
        expect_exit: 0,
    };

    // First process: a tight step budget stands in for a crash partway through.
    let run_id = {
        let store = Store::open(&db)?;
        let contract = TaskContract::new(goal, &file, verify.clone()).with_max_steps(1);
        let r = run(&contract, &provider, &store).await?;
        println!("[process 1] stopped: {:?}", r.outcome);
        println!(
            "[process 1] committed {} step(s); status = {:?}",
            store.last_step(r.run_id)?,
            store.run_status(r.run_id)?.unwrap_or(RunStatus::Running),
        );
        r.run_id
    }; // store dropped here — the first process is gone.

    // Second process: a brand-new Store over the same file. Resume continues the
    // same run id from the step after the last committed one.
    let store = Store::open(&db)?;
    let contract = TaskContract::new(goal, &file, verify).with_max_steps(8);
    let r = resume(&contract, &provider, &store, run_id).await?;
    println!("[process 2] resumed to: {:?}", r.outcome);

    let events = store.checkpoint_events(run_id)?;
    let skipped = events.iter().filter(|e| e.kind == "skipped").count();
    let checkpoints = events.iter().filter(|e| e.kind == "checkpoint").count();
    println!(
        "[process 2] {checkpoints} step(s) checkpointed, {skipped} skipped as already-done on resume",
    );
    println!(
        "[process 2] final status = {:?}",
        store.run_status(run_id)?.unwrap()
    );
    Ok(())
}
