//! The orchestration loop: observe, reason, act, verify, stop — bounded by
//! budgets, resilient to transient step failures, and resumable.
//!
//! v0.2 adds three budgets (step, time, cost-in-tokens) each with its own stop
//! outcome, per-step retry with escalation, a full trace written to the store,
//! and [`resume`], which continues an interrupted run under its original id
//! instead of restarting.

use std::path::Path;
use std::time::Instant;

use serde_json::json;
use tracing::info;

use crate::contract::TaskContract;
use crate::error::Result;
use crate::provider::{CompletionRequest, CompletionResponse, Provider, ToolCall, ToolSpec};
use crate::state::{StepRecord, Store};
use crate::tools::{
    FsTool, Workspace, FIND_TOOL, GREP_TOOL, READ_FILE_TOOL, WRITE_FILE_TOOL,
};

/// Cap on how much of a read file / grep result is folded into the observation
/// log, so one large file cannot blow up the prompt.
// ponytail: fixed char caps; make them budget-aware if long files starve the loop.
const OBS_READ_CAP: usize = 4_000;
const OBS_GREP_CAP: usize = 50;

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
    store.set_provider(run_id, provider.name())?;
    match contract.root.clone() {
        Some(root) => run_workspace_from(contract, provider, store, run_id, &root, 1).await,
        None => run_from(contract, provider, store, run_id, 1).await,
    }
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
    store.set_provider(run_id, provider.name())?;
    match contract.root.clone() {
        Some(root) => run_workspace_from(contract, provider, store, run_id, &root, start_step).await,
        None => run_from(contract, provider, store, run_id, start_step).await,
    }
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

/// The workspace loop (0.3 multi-file mode): the agent greps, finds, reads, and
/// writes several files under `root`, carrying its own working memory as an
/// observation log folded into each turn's prompt. Budgets, retry, trace, and
/// resume behave as in single-file mode; verification is multi-file
/// ([`Verification::passes_in`]).
async fn run_workspace_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    root: &Path,
    start_step: u32,
) -> Result<RunResult> {
    let ws = Workspace::new(root);
    let system = workspace_system_prompt();
    let tools = workspace_tools();
    let started = Instant::now();
    let mut tokens_used: u64 = 0;
    let mut observations = String::new();

    for step in start_step..=contract.max_steps {
        if let Some(max) = contract.max_duration {
            if started.elapsed() > max {
                store.finish_run(run_id, "time_budget_exceeded")?;
                return Ok(RunResult {
                    outcome: RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                });
            }
        }

        let user = workspace_user_prompt(contract, &observations);
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: tools.clone(),
        };

        let response =
            complete_with_retry(provider, &request, contract.max_retries, store, run_id, step)
                .await?;

        let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
        tokens_used += step_tokens;

        // Dispatch every tool call the model made this step, in order, folding
        // each result into the observation log the next turn will see.
        let mut decisions: Vec<String> = Vec::new();
        let mut calls_json: Vec<String> = Vec::new();
        if response.tool_calls.is_empty() {
            let said = response.text.clone().unwrap_or_default();
            observations.push_str(&format!("\n[step {step}] (no tool call) {said}\n"));
            decisions.push("no tool call".into());
        }
        for call in &response.tool_calls {
            calls_json.push(format!("{}:{}", call.name, call.arguments));
            let (decision, obs) = dispatch(&ws, call);
            observations.push_str(&obs);
            decisions.push(decision);
        }

        store.record(
            run_id,
            &StepRecord::new(step, decisions.join("; "), tail(&observations, OBS_READ_CAP))
                .with_trace(user, calls_json.join(" | "), step_tokens),
        )?;
        info!(step, decisions = %decisions.join("; "), tokens = step_tokens, "workspace step");

        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                store.finish_run(run_id, "cost_budget_exceeded")?;
                return Ok(RunResult {
                    outcome: RunOutcome::CostBudgetExceeded { steps: step },
                    run_id,
                });
            }
        }

        if contract.verify.passes_in(root).await? {
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

/// Execute one tool call against the workspace. Tool-level failures (bad regex,
/// path escape, ...) become observations the agent can recover from rather than
/// failing the run — only the model can decide what to do about them.
fn dispatch(ws: &Workspace, call: &ToolCall) -> (String, String) {
    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    match call.name.as_str() {
        GREP_TOOL => {
            let pattern = s("pattern").unwrap_or_default();
            match ws.grep(pattern, s("path_glob")) {
                Ok(hits) => {
                    let shown: Vec<String> = hits
                        .iter()
                        .take(OBS_GREP_CAP)
                        .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                        .collect();
                    (
                        format!("grep {pattern:?} ({} hits)", hits.len()),
                        format!("\n[grep {pattern:?}]\n{}\n", shown.join("\n")),
                    )
                }
                Err(e) => ("grep error".into(), format!("\n[grep error] {e}\n")),
            }
        }
        FIND_TOOL => {
            let glob = s("name_glob").or_else(|| s("glob")).unwrap_or_default();
            match ws.find(glob) {
                Ok(paths) => (
                    format!("find {glob:?} ({} paths)", paths.len()),
                    format!("\n[find {glob:?}]\n{}\n", paths.join("\n")),
                ),
                Err(e) => ("find error".into(), format!("\n[find error] {e}\n")),
            }
        }
        READ_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            match ws.read_file(path) {
                Ok(c) => (
                    format!("read {path}"),
                    format!("\n[read {path}]\n{}\n", tail(&c, OBS_READ_CAP)),
                ),
                Err(e) => ("read error".into(), format!("\n[read error] {e}\n")),
            }
        }
        WRITE_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let content = s("content").unwrap_or_default();
            if path.is_empty() {
                return (
                    "write missing path".into(),
                    "\n[write error] write_file needs a \"path\" in workspace mode\n".into(),
                );
            }
            match ws.write_file(path, content) {
                Ok(()) => (format!("wrote {path}"), format!("\n[wrote {path}]\n")),
                Err(e) => ("write error".into(), format!("\n[write error] {e}\n")),
            }
        }
        other => (
            format!("unknown tool {other}"),
            format!("\n[unknown tool {other}]\n"),
        ),
    }
}

/// Keep only the last `cap` chars, so a big file/log doesn't blow up the prompt.
fn tail(s: &str, cap: usize) -> String {
    if s.len() <= cap {
        s.to_string()
    } else {
        let start = s.len() - cap;
        // Snap to a char boundary so we never slice mid-UTF-8.
        let start = (start..s.len()).find(|&i| s.is_char_boundary(i)).unwrap_or(s.len());
        format!("...(truncated)...{}", &s[start..])
    }
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

fn workspace_system_prompt() -> String {
    "You are an agent working across a repository to meet a stated specification. \
     Use `grep` to search file contents and `find` to locate files by name, then \
     `read_file` to inspect a file before changing it, and `write_file` with the \
     file's path and full new contents to edit it. You may edit several files. \
     Work in small steps; after each of your steps the whole set is checked \
     against the success criterion. Do not explain; call tools."
        .to_string()
}

fn workspace_user_prompt(contract: &TaskContract, observations: &str) -> String {
    let constraints = if contract.constraints.is_empty() {
        "(none)".to_string()
    } else {
        contract.constraints.join("; ")
    };
    let obs = if observations.is_empty() {
        "(nothing yet — start by grepping or finding)".to_string()
    } else {
        observations.to_string()
    };
    format!(
        "Goal: {goal}\nConstraints: {constraints}\nSuccess criterion: {criterion}\n\n\
         Observations so far (results of your tool calls):\n{obs}\n\n\
         Call a tool to make progress toward the success criterion.",
        goal = contract.goal,
        criterion = contract.verify.describe(),
    )
}

fn workspace_tools() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: GREP_TOOL.to_string(),
            description: "Search file contents by regex (a plain substring is valid). Returns file:line: matches.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Regex or substring to search for." },
                    "path_glob": { "type": "string", "description": "Optional glob limiting which files are searched, e.g. src/*.rs." }
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: FIND_TOOL.to_string(),
            description: "List files whose name or relative path matches a glob (* and ?).".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name_glob": { "type": "string", "description": "Glob to match, e.g. *.rs or src/*.rs." }
                },
                "required": ["name_glob"]
            }),
        },
        ToolSpec {
            name: READ_FILE_TOOL.to_string(),
            description: "Read a file (path relative to the workspace root) into context.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the workspace root." }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: WRITE_FILE_TOOL.to_string(),
            description: "Write the full new contents of a file (path relative to the workspace root); creates it if absent.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the workspace root." },
                    "content": { "type": "string", "description": "Full new file contents." }
                },
                "required": ["path", "content"]
            }),
        },
    ]
}
