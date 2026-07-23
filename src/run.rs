//! The orchestration loop: observe, reason, act, verify, stop — bounded by
//! budgets, resilient to transient step failures, and resumable.
//!
//! v0.2 adds three budgets (step, time, cost-in-tokens) each with its own stop
//! outcome, per-step retry with escalation, a full trace written to the store,
//! and [`resume`], which continues an interrupted run under its original id
//! instead of restarting.

use std::time::Instant;

use serde_json::json;
use tracing::info;

use crate::contract::TaskContract;
use crate::error::Result;
use crate::provider::{CompletionRequest, CompletionResponse, Provider, ToolSpec};
use crate::state::{StepRecord, Store};
use crate::tools::{FsTool, WRITE_FILE_TOOL};

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Verification passed. `steps` is the step it passed on.
    Success { steps: u32 },
    /// The step budget was reached before verification passed.
    StepCapReached { steps: u32 },
    /// The time budget was exceeded. `steps` is how many steps completed.
    TimeBudgetExceeded { steps: u32 },
    /// The cost (token) budget was exceeded. `steps` is how many steps completed.
    CostBudgetExceeded { steps: u32 },
}

/// The result of a run, including the persisted run id for audit.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Why the run stopped.
    pub outcome: RunOutcome,
    /// The run's id in the [`Store`], for reading its trace back.
    pub run_id: i64,
}

/// Run a task contract to a verified result using `provider` and `store`.
///
/// Each iteration: read the file into context, ask the model (offering the
/// `write_file` tool, retrying transient failures), apply any write, record the
/// trace, then verify. Stops on the first passing verify, or when any budget —
/// steps, time, or tokens — is reached.
pub async fn run<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
) -> Result<RunResult> {
    let file_str = contract.file.display().to_string();
    let run_id = store.start_run(&contract.goal, &file_str)?;
    run_from(contract, provider, store, run_id, 1).await
}

/// Resume an interrupted run under its original `run_id`. Continues from the
/// step after the last one recorded, reusing the file on disk as the current
/// state — it does not restart from step one.
pub async fn resume<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
) -> Result<RunResult> {
    let start_step = store.last_step(run_id)? + 1;
    run_from(contract, provider, store, run_id, start_step).await
}

async fn run_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    start_step: u32,
) -> Result<RunResult> {
    let fs = FsTool::new(&contract.file);
    let system = system_prompt();
    let tool = write_file_tool();
    let started = Instant::now();
    let mut tokens_used: u64 = 0;

    for step in start_step..=contract.max_steps {
        // Time budget: checked before doing the step's work.
        if let Some(max) = contract.max_duration {
            if started.elapsed() > max {
                store.finish_run(run_id, "time_budget_exceeded")?;
                return Ok(RunResult {
                    outcome: RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                });
            }
        }

        let current = fs.read().await?;
        let user = user_prompt(contract, &current);
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: vec![tool.clone()],
        };

        let response =
            complete_with_retry(provider, &request, contract.max_retries, store, run_id, step)
                .await?;

        let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
        tokens_used += step_tokens;

        let call = response
            .tool_calls
            .iter()
            .find(|c| c.name == WRITE_FILE_TOOL);
        let tool_call_json = call.map(|c| c.arguments.to_string()).unwrap_or_default();
        let write = call.and_then(|c| c.arguments.get("content").and_then(|v| v.as_str()));

        let (decision, result_text) = match write {
            Some(content) => {
                fs.write(content).await?;
                ("wrote file", content.to_string())
            }
            None => ("no tool call", response.text.clone().unwrap_or_default()),
        };
        store.record(
            run_id,
            &StepRecord::new(step, decision, result_text).with_trace(
                user,
                tool_call_json,
                step_tokens,
            ),
        )?;
        info!(step, decision, tokens = step_tokens, "loop step");

        // Cost budget: checked after this step's tokens are counted.
        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                store.finish_run(run_id, "cost_budget_exceeded")?;
                return Ok(RunResult {
                    outcome: RunOutcome::CostBudgetExceeded { steps: step },
                    run_id,
                });
            }
        }

        let contents = fs.read().await?;
        if contract.verify.passes(&contract.file, &contents).await? {
            store.finish_run(run_id, "success")?;
            return Ok(RunResult {
                outcome: RunOutcome::Success { steps: step },
                run_id,
            });
        }
    }

    store.finish_run(run_id, "step_cap_reached")?;
    Ok(RunResult {
        outcome: RunOutcome::StepCapReached {
            steps: contract.max_steps,
        },
        run_id,
    })
}

/// Call the provider, retrying a failing call up to `max_retries` times. Each
/// failed attempt is recorded in the trace. After the limit the error is
/// escalated (recorded, the run marked `escalated`, and returned).
async fn complete_with_retry<P: Provider>(
    provider: &P,
    request: &CompletionRequest,
    max_retries: u32,
    store: &Store,
    run_id: i64,
    step: u32,
) -> Result<CompletionResponse> {
    let mut attempt = 0;
    loop {
        match provider.complete(request.clone()).await {
            Ok(response) => return Ok(response),
            Err(e) if attempt < max_retries => {
                attempt += 1;
                store.record(
                    run_id,
                    &StepRecord::new(step, format!("retry {attempt} after error"), e.to_string()),
                )?;
            }
            Err(e) => {
                store.record(run_id, &StepRecord::new(step, "escalated", e.to_string()))?;
                store.finish_run(run_id, "escalated")?;
                return Err(e);
            }
        }
    }
}

fn system_prompt() -> String {
    "You are an agent that edits exactly one file to meet a stated specification. \
     Call the `write_file` tool with the file's full new contents. Do not explain; \
     make the edit. The file will be checked against the success criterion after \
     each write."
        .to_string()
}

fn user_prompt(contract: &TaskContract, current: &str) -> String {
    let constraints = if contract.constraints.is_empty() {
        "(none)".to_string()
    } else {
        contract.constraints.join("; ")
    };
    format!(
        "Goal: {goal}\nConstraints: {constraints}\nSuccess criterion: {criterion}\n\n\
         Current file contents:\n---\n{current}\n---\n\n\
         Call write_file with the full new contents that satisfy the success criterion.",
        goal = contract.goal,
        criterion = contract.verify.describe(),
    )
}

fn write_file_tool() -> ToolSpec {
    ToolSpec {
        name: WRITE_FILE_TOOL.to_string(),
        description: "Write the full new contents of the target file.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "content": { "type": "string", "description": "Full new file contents." }
            },
            "required": ["content"]
        }),
    }
}
