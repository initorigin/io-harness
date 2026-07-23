//! The orchestration loop: observe, reason, act, verify, stop.

use serde_json::json;
use tracing::info;

use crate::contract::TaskContract;
use crate::error::Result;
use crate::provider::{CompletionRequest, Provider, ToolSpec};
use crate::state::Store;
use crate::tools::{FsTool, WRITE_FILE_TOOL};

/// Why a run stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunOutcome {
    /// Verification passed. `steps` is how many loop iterations it took.
    Success { steps: u32 },
    /// The step cap was reached before verification passed.
    StepCapReached { steps: u32 },
}

/// The result of a run, including the persisted run id for audit.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Why the run stopped.
    pub outcome: RunOutcome,
    /// The run's id in the [`Store`], for reading its steps back.
    pub run_id: i64,
}

/// Run a task contract to a verified result using `provider` and `store`.
///
/// Each iteration: read the file into context, ask the model (offering the
/// `write_file` tool), apply any write, then verify. Stops on the first passing
/// verify, or when `contract.max_steps` is reached.
pub async fn run<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
) -> Result<RunResult> {
    let fs = FsTool::new(&contract.file);
    let file_str = contract.file.display().to_string();
    let run_id = store.start_run(&contract.goal, &file_str)?;

    let system = system_prompt();
    let tool = write_file_tool();

    for step in 1..=contract.max_steps {
        let current = fs.read().await?;
        let user = user_prompt(contract, &current);

        let response = provider
            .complete(CompletionRequest {
                system: system.clone(),
                user,
                tools: vec![tool.clone()],
            })
            .await?;

        let write = response
            .tool_calls
            .iter()
            .find(|c| c.name == WRITE_FILE_TOOL)
            .and_then(|c| c.arguments.get("content").and_then(|v| v.as_str()));

        let decision = match write {
            Some(content) => {
                fs.write(content).await?;
                store.record_step(run_id, step, "wrote file", content)?;
                "wrote file"
            }
            None => {
                let text = response.text.clone().unwrap_or_default();
                store.record_step(run_id, step, "no tool call", &text)?;
                "no tool call"
            }
        };
        info!(step, decision, "loop step");

        let contents = fs.read().await?;
        if contract.verify.check(&contents) {
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
