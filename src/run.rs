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
use crate::approve::{ApproveAll, Approver, Decision, Request};
use crate::policy::{Act, Effect, Policy, Rule};
use crate::state::PolicyEvent;
use crate::verify::ExecGuard;
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
    /// A human denied a deferred action on resume, so the run stopped without
    /// performing it. `steps` is how many steps completed.
    Denied { steps: u32 },
    /// An approver deferred a decision. The run is paused, not finished: the
    /// pending action is persisted under `request_id` and survives this
    /// process, so [`resume_with_decision`] can continue it once a human
    /// decides. `steps` is how many steps completed.
    AwaitingApproval { request_id: i64, steps: u32 },
}

/// The result of a run, including the persisted run id for audit.
#[derive(Debug, Clone)]
pub struct RunResult {
    /// Why the run stopped.
    pub outcome: RunOutcome,
    /// The run's id in the [`Store`], for reading its trace back.
    pub run_id: i64,
    /// Rules an approver asked to remember during this run. The crate applies
    /// them for the rest of the run and hands them back here; persisting them
    /// across runs is the caller's decision, since config files are app-owned.
    pub remembered: Vec<Rule>,
}

impl RunResult {
    fn new(outcome: RunOutcome, run_id: i64) -> Self {
        Self { outcome, run_id, remembered: Vec::new() }
    }

    fn with_remembered(mut self, remembered: Vec<Rule>) -> Self {
        self.remembered = remembered;
        self
    }
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
    run_with(contract, provider, store, &Policy::permissive(), &ApproveAll).await
}

/// Run a task contract under a permission `policy`, routing anything the policy
/// marks [`Effect::Ask`] to `approver` before it happens.
///
/// An action the policy *denies* is refused without consulting the approver and
/// reported to the model as a tool result it can adapt to; the refusal consumes
/// the step, so a model that keeps retrying a denied action reaches the step cap
/// rather than looping forever.
pub async fn run_with<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    let file_str = contract.file.display().to_string();
    let run_id = store.start_run(&contract.goal, &file_str)?;
    store.set_provider(run_id, provider.name())?;
    match contract.root.clone() {
        Some(root) => {
            run_workspace_from(
                contract, provider, store, run_id, &root, 1, policy, approver,
            )
            .await
        }
        // Single-file mode has no policy-aware tool layer in 0.4.0. Silently
        // ignoring a policy here would be worse than not supporting it: the
        // caller would believe a boundary was enforced when nothing was
        // checking. Refuse loudly instead.
        None if !policy.is_permissive() => Err(crate::error::Error::Config(
            "a permission policy requires workspace mode — build the contract \
             with TaskContract::workspace(goal, root, verify). Single-file \
             contracts are not policy-enforced in 0.4.0."
                .into(),
        )),
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
        Some(root) => {
            run_workspace_from(
                contract,
                provider,
                store,
                run_id,
                &root,
                start_step,
                &Policy::permissive(),
                &ApproveAll,
            )
            .await
        }
        None => run_from(contract, provider, store, run_id, start_step).await,
    }
}

/// Continue a run that stopped at [`RunOutcome::AwaitingApproval`], once a
/// human has decided about the pending action.
///
/// An approval performs exactly the action that was persisted — the same target
/// and the same content the human was shown — and then continues the run under
/// its original `run_id`. The decision is re-checked against the policy first,
/// so a deny that landed after the pause still holds. A denial closes the run
/// without performing the action.
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_decision<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    request_id: i64,
    decision: Decision,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    let pending = store
        .pending(request_id)?
        .ok_or_else(|| crate::error::Error::Config(format!("no pending request {request_id}")))?;
    if pending.run_id != run_id {
        return Err(crate::error::Error::Config(format!(
            "request {request_id} belongs to run {}, not {run_id}",
            pending.run_id
        )));
    }

    let root = contract
        .root
        .clone()
        .ok_or_else(|| crate::error::Error::Config("resume_with_decision needs a workspace".into()))?;
    let step = pending.step;

    match decision {
        // Deferring again leaves it pending and the run paused.
        Decision::Defer => {
            Ok(RunResult::new(
                RunOutcome::AwaitingApproval { request_id, steps: step },
                run_id,
            ))
        }
        Decision::Deny { reason } => {
            store.resolve_pending(request_id, "deny")?;
            store.record_event(
                run_id,
                &PolicyEvent::decision(
                    step,
                    &pending.act,
                    &pending.target,
                    "deny",
                    format!("resumed:{request_id}"),
                ),
            )?;
            info!(run_id, request_id, %reason, "deferred action denied");
            store.finish_run(run_id, "denied")?;
            Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id))
        }
        Decision::Approve { modified, remember } => {
            let target = modified
                .as_ref()
                .map(|m| m.target.clone())
                .unwrap_or_else(|| pending.target.clone());
            let content = modified
                .as_ref()
                .and_then(|m| m.content.clone())
                .or_else(|| pending.content.clone());

            let mut effective = policy.clone();
            if !remember.is_empty() {
                let mut layer = Policy::permissive().layer("remembered");
                for r in &remember {
                    layer = layer.rule(r.act, r.effect, r.pattern.clone());
                }
                effective = effective.merge(layer);
            }
            let ws = Workspace::with_policy(&root, effective.clone());

            // The pause does not grant immunity: the policy still decides.
            let act = if pending.act == "read" { Act::Read } else { Act::Write };
            let recheck = ws.check_path(act, &target);
            if recheck.effect == Effect::Deny {
                let mut ev = PolicyEvent::refusal(step, &pending.act, &target);
                ev.rule = recheck.rule.clone();
                ev.layer = recheck.layer.clone();
                store.record_event(run_id, &ev)?;
                store.resolve_pending(request_id, "deny")?;
                store.finish_run(run_id, "denied")?;
                return Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id));
            }

            if act == Act::Write {
                ws.write_file(&target, content.as_deref().unwrap_or_default())?;
            }
            store.resolve_pending(request_id, "approve")?;
            let mut ev = PolicyEvent::decision(
                step,
                &pending.act,
                &pending.target,
                "approve",
                format!("resumed:{request_id}"),
            );
            if target != pending.target {
                ev = ev.with_performed(&target);
            }
            store.record_event(run_id, &ev)?;

            // Continue the run under its original id, from the next step.
            run_workspace_from(
                contract,
                provider,
                store,
                run_id,
                &root,
                step + 1,
                &effective,
                approver,
            )
            .await
            .map(|r| r.with_remembered(remember))
        }
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
                return Ok(RunResult::new(RunOutcome::TimeBudgetExceeded { steps: step - 1 }, run_id));
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
                return Ok(RunResult::new(RunOutcome::CostBudgetExceeded { steps: step }, run_id));
            }
        }

        let contents = fs.read().await?;
        if contract.verify.passes(&contract.file, &contents).await? {
            store.finish_run(run_id, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id));
        }
    }

    store.finish_run(run_id, "step_cap_reached")?;
    Ok(RunResult::new(RunOutcome::StepCapReached {
            steps: contract.max_steps,
        }, run_id))
}

/// The workspace loop (0.3 multi-file mode): the agent greps, finds, reads, and
/// writes several files under `root`, carrying its own working memory as an
/// observation log folded into each turn's prompt. Budgets, retry, trace, and
/// resume behave as in single-file mode; verification is multi-file
/// ([`Verification::passes_in`]).
#[allow(clippy::too_many_arguments)]
async fn run_workspace_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    root: &Path,
    start_step: u32,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    // The effective policy grows as approvers remember rules; it is rebuilt as a
    // merge so a remembered allow can still never defeat a deny beneath it.
    let mut effective = policy.clone();
    let mut remembered: Vec<Rule> = Vec::new();
    let mut ws = Workspace::with_policy(root, effective.clone());
    let system = workspace_system_prompt();
    let tools = workspace_tools();
    let started = Instant::now();
    let mut tokens_used: u64 = 0;
    let mut observations = String::new();

    for step in start_step..=contract.max_steps {
        if let Some(max) = contract.max_duration {
            if started.elapsed() > max {
                store.finish_run(run_id, "time_budget_exceeded")?;
                return Ok(RunResult::new(RunOutcome::TimeBudgetExceeded { steps: step - 1 }, run_id).with_remembered(remembered));
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
        let mut paused: Option<i64> = None;
        let mut new_rules: Vec<Rule> = Vec::new();
        for call in &response.tool_calls {
            calls_json.push(format!("{}:{}", call.name, call.arguments));
            match dispatch(&ws, call, approver, store, run_id, step).await? {
                Dispatched::Continue {
                    decision,
                    obs,
                    remember,
                } => {
                    observations.push_str(&obs);
                    decisions.push(decision);
                    new_rules.extend(remember);
                }
                Dispatched::Pause { request_id } => {
                    decisions.push(format!("awaiting approval (request {request_id})"));
                    paused = Some(request_id);
                    break;
                }
            }
        }

        store.record(
            run_id,
            &StepRecord::new(step, decisions.join("; "), tail(&observations, OBS_READ_CAP))
                .with_trace(user, calls_json.join(" | "), step_tokens),
        )?;
        info!(step, decisions = %decisions.join("; "), tokens = step_tokens, "workspace step");

        // An approver deferred: persist nothing further, stop, and let the
        // caller resume once a human has decided.
        if let Some(request_id) = paused {
            store.finish_run(run_id, "awaiting_approval")?;
            return Ok(RunResult::new(
                RunOutcome::AwaitingApproval {
                    request_id,
                    steps: step,
                },
                run_id,
            )
            .with_remembered(remembered));
        }

        // Rules an approver asked to remember apply as a top layer for the rest
        // of the run. Merging (rather than editing) is what keeps a remembered
        // allow from overriding a deny beneath it.
        if !new_rules.is_empty() {
            let mut layer = Policy::permissive().layer("remembered");
            for r in &new_rules {
                layer = layer.rule(r.act, r.effect, r.pattern.clone());
            }
            effective = effective.merge(layer);
            ws = Workspace::with_policy(root, effective.clone());
            remembered.extend(new_rules);
        }

        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                store.finish_run(run_id, "cost_budget_exceeded")?;
                return Ok(RunResult::new(RunOutcome::CostBudgetExceeded { steps: step }, run_id).with_remembered(remembered));
            }
        }

        if contract
            .verify
            .passes_in_guarded(root, &ExecGuard::new(&effective).tracing(store, run_id, step))
            .await?
        {
            store.finish_run(run_id, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id).with_remembered(remembered));
        }
    }

    store.finish_run(run_id, "step_cap_reached")?;
    Ok(RunResult::new(RunOutcome::StepCapReached {
            steps: contract.max_steps,
        }, run_id))
}

/// The result of dispatching one tool call.
enum Dispatched {
    /// The call resolved; fold `obs` into the observation log and carry any
    /// rules an approver asked to remember.
    Continue {
        decision: String,
        obs: String,
        remember: Vec<Rule>,
    },
    /// An approver deferred; the action is persisted under `request_id` and the
    /// run stops until a human decides.
    Pause { request_id: i64 },
}

impl Dispatched {
    fn go(decision: impl Into<String>, obs: impl Into<String>) -> Self {
        Dispatched::Continue {
            decision: decision.into(),
            obs: obs.into(),
            remember: Vec::new(),
        }
    }
}

/// Execute one tool call against the workspace, enforcing the policy and
/// consulting `approver` for anything it marks [`Effect::Ask`].
///
/// Tool-level failures (bad regex, path escape, a policy refusal) become
/// observations the agent can recover from rather than failing the run — only
/// the model can decide what to do about them.
async fn dispatch(
    ws: &Workspace,
    call: &ToolCall,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
) -> Result<Dispatched> {
    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    Ok(match call.name.as_str() {
        GREP_TOOL => {
            let pattern = s("pattern").unwrap_or_default();
            match ws.grep(pattern, s("path_glob")) {
                Ok(hits) => {
                    let shown: Vec<String> = hits
                        .iter()
                        .take(OBS_GREP_CAP)
                        .map(|m| format!("{}:{}: {}", m.path, m.line, m.text))
                        .collect();
                    Dispatched::go(
                        format!("grep {pattern:?} ({} hits)", hits.len()),
                        format!("\n[grep {pattern:?}]\n{}\n", shown.join("\n")),
                    )
                }
                Err(e) => Dispatched::go("grep error", format!("\n[grep error] {e}\n")),
            }
        }
        FIND_TOOL => {
            let glob = s("name_glob").or_else(|| s("glob")).unwrap_or_default();
            match ws.find(glob) {
                Ok(paths) => Dispatched::go(
                    format!("find {glob:?} ({} paths)", paths.len()),
                    format!("\n[find {glob:?}]\n{}\n", paths.join("\n")),
                ),
                Err(e) => Dispatched::go("find error", format!("\n[find error] {e}\n")),
            }
        }
        READ_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            match gate(ws, approver, store, run_id, step, Act::Read, path, None).await? {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go { target, remember, .. } => match ws.read_file(&target) {
                    Ok(c) => Dispatched::Continue {
                        decision: format!("read {target}"),
                        obs: format!("\n[read {target}]\n{}\n", tail(&c, OBS_READ_CAP)),
                        remember,
                    },
                    Err(e) => Dispatched::go("read error", format!("\n[read error] {e}\n")),
                },
            }
        }
        WRITE_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let content = s("content").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "write missing path",
                    "\n[write error] write_file needs a \"path\" in workspace mode\n",
                ));
            }
            match gate(ws, approver, store, run_id, step, Act::Write, path, Some(content)).await? {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go { target, content, remember } => {
                    let body = content.unwrap_or_default();
                    match ws.write_file(&target, &body) {
                        Ok(()) => Dispatched::Continue {
                            decision: format!("wrote {target}"),
                            obs: format!("\n[wrote {target}]\n"),
                            remember,
                        },
                        Err(e) => Dispatched::go("write error", format!("\n[write error] {e}\n")),
                    }
                }
            }
        }
        other => Dispatched::go(
            format!("unknown tool {other}"),
            format!("\n[unknown tool {other}]\n"),
        ),
    })
}

/// What the policy and the approver together decided about one action.
enum Gated {
    /// Perform it, possibly in the form an approver rewrote it into.
    Go {
        target: String,
        content: Option<String>,
        remember: Vec<Rule>,
    },
    /// Do not perform it; `obs` is what the model is told.
    Refused { decision: String, obs: String },
    /// An approver deferred the decision.
    Paused { request_id: i64 },
}

/// Evaluate one action against the policy, consulting `approver` only for the
/// sensitive-but-permitted tier.
///
/// A denied action never reaches the approver — refusal and approval are
/// different things. An approver that rewrites the action has the rewritten
/// form re-evaluated here, so it can narrow or redirect within the policy but
/// cannot move an action across a deny.
#[allow(clippy::too_many_arguments)]
async fn gate(
    ws: &Workspace,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    act: Act,
    target: &str,
    content: Option<&str>,
) -> Result<Gated> {
    let kind = format!("{act:?}").to_lowercase();
    let verdict = ws.check_path(act, target);

    match verdict.effect {
        Effect::Deny => {
            let mut ev = PolicyEvent::refusal(step, &kind, target);
            if let (Some(rule), layer) = (verdict.rule.clone(), verdict.layer.clone()) {
                ev.rule = Some(rule);
                ev.layer = layer;
            }
            store.record_event(run_id, &ev)?;
            let why = verdict
                .rule
                .as_deref()
                .map(|r| format!(" (rule {r})"))
                .unwrap_or_default();
            Ok(Gated::Refused {
                decision: format!("{kind} refused"),
                obs: format!("\n[{kind} refused] {target}{why} — the policy forbids this; try another path\n"),
            })
        }
        Effect::Allow => Ok(Gated::Go {
            target: target.to_string(),
            content: content.map(str::to_string),
            remember: Vec::new(),
        }),
        Effect::Ask => {
            let mut request = Request::new(act, target);
            if let Some(c) = content {
                request = request.with_content(c);
            }
            match approver.decide(&request).await {
                Decision::Approve { modified, remember } => {
                    let performed = modified.unwrap_or_else(|| request.clone());
                    // The rewritten action gets the same scrutiny as the original.
                    let recheck = ws.check_path(act, &performed.target);
                    if recheck.effect == Effect::Deny {
                        let mut ev = PolicyEvent::refusal(step, &kind, &performed.target);
                        ev.rule = recheck.rule.clone();
                        ev.layer = recheck.layer.clone();
                        store.record_event(run_id, &ev)?;
                        return Ok(Gated::Refused {
                            decision: format!("{kind} refused after approval"),
                            obs: format!(
                                "\n[{kind} refused] {} — an approved change may not cross a deny\n",
                                performed.target
                            ),
                        });
                    }
                    let mut ev =
                        PolicyEvent::decision(step, &kind, target, "approve", "approver");
                    if performed.target != target {
                        ev = ev.with_performed(&performed.target);
                    }
                    store.record_event(run_id, &ev)?;
                    Ok(Gated::Go {
                        target: performed.target,
                        content: performed.content,
                        remember,
                    })
                }
                Decision::Deny { reason } => {
                    store.record_event(
                        run_id,
                        &PolicyEvent::decision(step, &kind, target, "deny", "approver"),
                    )?;
                    Ok(Gated::Refused {
                        decision: format!("{kind} denied"),
                        obs: format!("\n[{kind} denied] {target} — {reason}\n"),
                    })
                }
                Decision::Defer => {
                    store.record_event(
                        run_id,
                        &PolicyEvent::decision(step, &kind, target, "defer", "approver"),
                    )?;
                    let request_id = store.put_pending(run_id, step, &kind, target, content)?;
                    Ok(Gated::Paused { request_id })
                }
            }
        }
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
