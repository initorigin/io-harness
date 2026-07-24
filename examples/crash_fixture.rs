//! A deterministic, offline harness run used by `tests/checkpoint.rs` to prove
//! real-crash recovery: the test spawns this binary, lets it commit a few steps
//! to a file-backed store, kills it with SIGKILL mid-run, then resumes the same
//! store in-process and asserts it reaches the verified result without re-running
//! a committed step or double-applying the edit.
//!
//! It never reaches its goal on its own — every step writes "WORKING" (which the
//! `FileContains("SOLUTION-DONE")` verify rejects) and sleeps, so the loop keeps
//! going, giving the parent test a wide, race-free window to kill it. The resume
//! side (in the test) supplies the finishing content.
//!
//! Usage: `crash_fixture <db_path> <target_file>`. No network, no API key.

use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{run, Provider, Store, TaskContract, Verification};
use serde_json::json;

/// Writes "WORKING" every step (never satisfying the verify) and sleeps, so the
/// run loops until the parent test kills the process. Each completion reports a
/// fixed token count so the durable budget has something to accumulate.
struct Working;

impl Provider for Working {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        // Sleep first, so committed steps trickle out at a catchable rate.
        tokio::time::sleep(Duration::from_millis(150)).await;
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": "WORKING\n" }),
            }],
            usage: Some(Usage { total_tokens: 10, ..Default::default() }),
            ..Default::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io_harness::Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("db path");
    let file = args.next().expect("target file");

    let store = Store::open(&db)?;
    let contract = TaskContract::new(
        "write SOLUTION-DONE",
        &file,
        Verification::FileContains("SOLUTION-DONE".into()),
    )
    .with_max_steps(1000);

    // Runs until SIGKILLed by the test — it can never satisfy the verify itself.
    let _ = run(&contract, &Working, &store).await?;
    Ok(())
}
