//! A deterministic, offline agent tree used by `tests/fleet.rs` to prove that a
//! fleet's queue survives a real crash: the test spawns this binary, waits until
//! the store holds a non-empty backlog, kills it with SIGKILL, then reads the
//! queue back from its own process.
//!
//! The shape is chosen so the kill lands with the queue full and stays full. The
//! containment allows one working agent per tier, the root asks for five children
//! in one step, and the child that wins the single slot parks forever. So exactly
//! one child ever becomes a run, four are waiting in `agent_queue` with no run row
//! and no spend of their own, and the parent has a wide, race-free window in which
//! it is unambiguously still alive.
//!
//! A fixture that exited on its own would prove only that rows were written, not
//! that nothing else was needed to keep them.
//!
//! Usage: `fleet_fixture <db_path> <workspace_dir>`. No network, no API key.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_tree, ApproveAll, Containment, Policy, Provider, Store, TaskContract, Verification,
};
use serde_json::json;

/// How many children the root asks for. One is admitted; the rest queue.
const FANOUT: usize = 5;

/// Tokens the admitted child's one committed step reports. The test asserts the
/// tree's whole recorded spend is exactly this — every queued child contributed
/// nothing, because nothing about it was started.
const ADMITTED_TOKENS: u64 = 90;

/// Fans out once; the child that wins the single slot writes one step and then
/// parks, so the slot is never released and the queue behind it stays full.
#[derive(Default)]
struct ParkingFleet {
    child_calls: AtomicUsize,
}

impl Provider for ParkingFleet {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        // Driven off the goal rather than a step counter, so the fixture keeps
        // working if the loop's step numbering ever changes.
        if !req.user.contains("FLEET-ROOT") {
            if self.child_calls.fetch_add(1, Ordering::SeqCst) == 0 {
                // One real, committed step with a real token cost, so the tree has
                // spend that belongs to the one child that actually started.
                return Ok(CompletionResponse {
                    tool_calls: vec![ToolCall {
                        name: "write_file".into(),
                        arguments: json!({ "path": "c0.txt", "content": "admitted\n" }),
                    }],
                    usage: Some(Usage {
                        total_tokens: ADMITTED_TOKENS,
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
            // Park, so the slot is never released and the four behind it stay
            // queued for as long as the test needs.
            loop {
                tokio::time::sleep(Duration::from_secs(3600)).await;
            }
        }
        let calls: Vec<ToolCall> = (0..FANOUT)
            .map(|i| ToolCall {
                name: "spawn_agent".into(),
                arguments: json!({
                    "goal": format!("child-{i}"),
                    "verify_file": format!("c{i}.txt"),
                    "verify_contains": "never-satisfied",
                    // Room for more than one step, so a child adopted on resume
                    // has real work left. A child resumed straight into its step
                    // cap never calls the provider, and a test measuring overlap
                    // would then measure nothing.
                    "max_steps": 3
                }),
            })
            .collect();
        Ok(CompletionResponse {
            tool_calls: calls,
            // The root's own step never commits — it is still awaiting its
            // children when the test kills the process — so it reports nothing.
            ..Default::default()
        })
    }
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> io_harness::Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("db path");
    let root = args.next().expect("workspace dir");

    let store = Store::open(&db)?;
    let contract = TaskContract::workspace("FLEET-ROOT: fan out across the fleet.", &root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(1);
    let policy = Policy::default()
        .layer("fixture")
        .allow_read("*")
        .allow_write("*");

    // Plenty of room in the tree; one slot per tier. The cap that bites here is
    // the one that throttles, which is the whole point: nothing is refused, four
    // children are simply waiting.
    let containment = Containment::new(FANOUT as u32 + 1, 1, 3, 1_000_000);

    // Never returns: the admitted child parks and the root is awaiting it.
    let provider = ParkingFleet::default();
    let _ = run_tree(&contract, &provider, &store, &policy, &ApproveAll, &containment).await?;
    Ok(())
}
