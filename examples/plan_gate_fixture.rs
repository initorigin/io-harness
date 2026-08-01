//! A deterministic, offline plan-gated run used by `tests/plan_gate.rs` to prove
//! that a plan survives a real crash: the test spawns this binary, waits until it
//! has durably persisted a proposed plan and stopped on it, kills it with SIGKILL,
//! then approves the plan from its own process and resumes the same store.
//!
//! The gate here is [`PlanGateNone`], so nothing in *this* process can answer and
//! the run stops with `RunOutcome::AwaitingPlan`. The binary then parks forever, so
//! the parent has a wide, race-free window to kill a process that is unambiguously
//! still alive — a fixture that exited on its own would prove only that a row was
//! written, not that nothing else was needed.
//!
//! Usage: `plan_gate_fixture <db_path> <workspace_dir>`. No network, no API key.

use std::sync::Arc;
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, PlanGateNone, Policy, Provider, Store, TaskContract, PROPOSE_PLAN_TOOL,
};
use serde_json::json;

/// Looks around, then proposes. Anything after the proposal is never reached,
/// because the gate does not answer.
struct Proposer;

impl Provider for Proposer {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        tokio::time::sleep(Duration::from_millis(50)).await;
        // Driven off the tools it was offered rather than a step counter, so the
        // fixture keeps working if the loop's step numbering ever changes.
        let planning = req.tools.iter().any(|t| t.name == PROPOSE_PLAN_TOOL);
        let call = match planning {
            true => ToolCall {
                name: PROPOSE_PLAN_TOOL.into(),
                arguments: json!({
                    "steps": [
                        { "intent": "read the existing notes" },
                        { "intent": "write SOLUTION-DONE into out.txt" }
                    ]
                }),
            },
            false => ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "out.txt", "content": "SOLUTION-DONE\n" }),
            },
        };
        Ok(CompletionResponse {
            tool_calls: vec![call],
            usage: Some(Usage {
                total_tokens: 10,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> io_harness::Result<()> {
    let mut args = std::env::args().skip(1);
    let db = args.next().expect("db path");
    let root = args.next().expect("workspace dir");

    let store = Store::open(&db)?;
    let contract = TaskContract::workspace("write SOLUTION-DONE into out.txt", &root)
        .with_plan_gate(Arc::new(PlanGateNone))
        .with_max_steps(1000);
    let policy = Policy::default()
        .layer("fixture")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*");

    // Stops on `AwaitingPlan`: nothing here answers, and the plan is now durable.
    let _ = run_with(&contract, &Proposer, &store, &policy, &ApproveAll).await?;

    // Park, so the parent kills a live process rather than racing an exit.
    loop {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
}
