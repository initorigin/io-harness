//! The orchestration loop: observe, reason, act, verify, stop — bounded by
//! budgets, resilient to transient step failures, and resumable.
//!
//! v0.2 adds three budgets (step, time, cost-in-tokens) each with its own stop
//! outcome, per-step retry with escalation, a full trace written to the store,
//! and [`resume`], which continues an interrupted run under its original id
//! instead of restarting.

use std::cell::Cell;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;

use serde_json::json;
use tracing::info;

use crate::approve::{ApproveAll, Approver, Decision, Request};
use crate::containment::{Containment, Draw, Ledger};
use crate::context::{
    assemble, bound, entry_cap_chars, Assembly, Ledger as ContextLedger, ObsKind, Observation,
};
use crate::contract::TaskContract;
use crate::error::{Error, Result};
use crate::mcp::McpSession;
use crate::net::{self, NetGuard};
use crate::observe::{EventKind, Ignore, Observer, RunEvent};
use crate::policy::{Act, Effect, Policy, Rule};
use crate::provider::{CompletionRequest, CompletionResponse, Provider, ToolCall, ToolSpec};
use crate::resilience::{Progress, Progressing};
use crate::skills::Skills;
use crate::state::PolicyEvent;
use crate::state::{AgentEvent, ContextEvent, RunStatus, StepRecord, Store};
use crate::tools::{
    FsTool, Toolbox, Workspace, FIND_TOOL, GREP_TOOL, READ_FILE_TOOL, READ_SKILL_TOOL,
    REMEMBER_TOOL, WRITE_FILE_TOOL,
};
use crate::verify::{ExecGuard, Verification};

/// The tool a parent agent calls to spawn a contained sub-agent.
pub const SPAWN_TOOL: &str = "spawn_agent";

/// How many grep hits are folded into one observation. A relevance ceiling, not a
/// size one — the size ceiling is the budget-derived per-entry cap on top of it.
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
    /// The agent stopped making progress: for `StallPolicy::window` consecutive
    /// steps it changed nothing in the workspace while repeating a tool call it had
    /// already made, and it had already been told once. The run stops here rather
    /// than spending the rest of its step budget proving it is stuck. `steps` is
    /// how many steps completed.
    Stalled { steps: u32 },
    /// A provider failure exhausted its retries and the run was escalated to the
    /// caller. `retryable` is whether the failure was one another attempt could have
    /// survived — a rate limit or a 503 — as opposed to a wrong key or an
    /// unacceptable request. Reached through [`resume`] after the fact: the run that
    /// escalated returned the `Err` itself.
    Escalated { steps: u32, retryable: bool },
    /// (sub-agent trees) The tree's aggregate spend ceiling was crossed, so the
    /// whole tree halts — not this one agent hitting its own budget. `steps` is
    /// how many steps this agent completed before the tree-wide halt.
    BudgetCeilingReached { steps: u32 },
    /// The run never started, because reaching the provider needed network access
    /// the policy asked about and a human denied. The authorization happens before
    /// the run's first step, so `steps` is normally 0.
    ///
    /// Reached through [`resume`] after the fact: the run that was refused
    /// returned the `Err` itself, exactly as an escalation does. Added in 0.12.0 —
    /// `"refused"` was written to the store from 0.8.0 onward with no variant and
    /// no mapping, so resuming a refused run fell back into the loop and asked
    /// the human again.
    Refused { steps: u32 },
    /// An [`Observer`] asked the run to stop, and it stopped — at the next step
    /// boundary rather than where the request landed, so no step was abandoned
    /// half-done. `steps` is how many steps completed before it stopped.
    ///
    /// Added in 0.12.0 with [`Flow::Cancel`](crate::Flow::Cancel), which is the
    /// first supported way to stop a run in flight: dropping the run's future
    /// abandons it mid-step and leaves `runs.status` as `running` forever, which
    /// nothing can tell apart from a process that crashed. A cancelled run is
    /// finished rather than abandoned, and stays resumable — a resume reports this
    /// outcome instead of re-driving the loop.
    Cancelled { steps: u32 },
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
    /// What this run cost and whether it worked, read back from the store.
    ///
    /// Added in 0.12.0. Before it, a caller holding a `RunResult` had an outcome
    /// discriminant and a `run_id`: spend needed a follow-up query, and latency
    /// was not recorded anywhere at all.
    ///
    /// `None` when the run has not finished — a run paused awaiting a human has
    /// no ending to summarise yet.
    ///
    /// A method rather than a field, deliberately. A field would have to be
    /// filled at every one of the entry points' return sites, including the ones
    /// that return `Err` and never build a `RunResult`, so the two could drift.
    /// Reading it from the store means the caller and an auditor are looking at
    /// the same row by construction. It also keeps this struct's existing
    /// exhaustive-pattern compatibility intact: no new field, no break.
    pub fn summary(&self, store: &Store) -> Result<Option<crate::RunSummary>> {
        store.run_summary(self.run_id)
    }
}

impl RunResult {
    fn new(outcome: RunOutcome, run_id: i64) -> Self {
        Self {
            outcome,
            run_id,
            remembered: Vec::new(),
        }
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
    run_observed(contract, provider, store, &Ignore).await
}

/// [`run`], reporting to `observer` as it happens.
///
/// The observed twin of every entry point takes the [`Observer`] last and does
/// exactly what its unobserved original does — the originals *are* these
/// functions, called with [`Ignore`]. Adding a parameter to the seven existing
/// signatures would have broken every caller of a 0.11.0 API to add something
/// opt-in; a builder would have added a second way to start a run for the same
/// reason.
pub async fn run_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    observer: &dyn Observer,
) -> Result<RunResult> {
    run_with_observed(
        contract,
        provider,
        store,
        &Policy::permissive(),
        &ApproveAll,
        observer,
    )
    .await
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
    run_with_observed(contract, provider, store, policy, approver, &Ignore).await
}

/// [`run_with`], reporting to `observer` as it happens. See [`run_observed`].
pub async fn run_with_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    // Arbitration before anything else: a toolbox that cannot be dispatched
    // unambiguously is a configuration mistake, and the caller should hear about
    // it before a run row exists and before the provider is billed for a turn.
    contract.tools.validate()?;
    // Same reason, same point: a skills directory that cannot be read is a
    // configuration mistake, and the caller hears the path before a run row
    // exists rather than getting a silently empty catalogue mid-run.
    let skills = contract.discover_skills()?;
    let file_str = contract.file.display().to_string();
    let run_id = store.start_run(&contract.goal, &file_str)?;
    store.set_provider(run_id, provider.name())?;
    // The caller's policy, before the provider layer below is merged into it —
    // what the caller asked for, not what the harness added. Recorded so a later
    // `resume` can tell a run that had a boundary from one that never did; see
    // [`resume`], which refuses the first rather than resuming it permissively.
    store.record_run_policy(run_id, policy)?;
    // The run row exists and the provider is set, which is what `Started` reports:
    // emitted before the network authorization below, so an observer watching a run
    // that is refused before its first step still saw it begin.
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        0,
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));
    // Decided against the *caller's* policy, before the provider layer is merged
    // in: the harness adding a network layer of its own must not turn a
    // permissive caller into a policy-bearing one and push it off the
    // single-file path.
    let caller_enforces = !policy.is_permissive();
    let policy = &match authorize_provider(provider, policy, store, run_id, approver, watch).await?
    {
        ProviderAccess::Granted(p) => p,
        ProviderAccess::Pending(request_id) => {
            return Ok(RunResult::new(
                RunOutcome::AwaitingApproval {
                    request_id,
                    steps: 0,
                },
                run_id,
            ))
        }
    };
    match contract.root.clone() {
        Some(root) => {
            let mcp = McpSession::connect(&contract.mcp, policy, store, run_id, watch).await?;
            let result = run_workspace_from(
                contract, provider, store, run_id, &root, 1, policy, approver, &mcp, &skills, watch,
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            result
        }
        // Single-file mode has no policy-aware tool layer in 0.4.0. Silently
        // ignoring a policy here would be worse than not supporting it: the
        // caller would believe a boundary was enforced when nothing was
        // checking. Refuse loudly instead.
        None if caller_enforces => Err(crate::error::Error::Config(
            "a permission policy requires workspace mode — build the contract \
             with TaskContract::workspace(goal, root, verify). Single-file \
             contracts are not policy-enforced in 0.4.0."
                .into(),
        )),
        None => run_from(contract, provider, store, run_id, 1, watch).await,
    }
}

/// Resume an interrupted run under its original `run_id`. Continues from the
/// step after the last one recorded, reusing the file on disk as the current
/// state — it does not restart from step one.
///
/// This is the resume for a run that had **no** permission boundary. It drives
/// the loop permissively, and a run that *was* started under a policy is refused
/// with [`Error::Resume`] rather than resumed without it — use [`resume_with`]
/// and supply the policy. Through 0.12.0 this function substituted
/// [`Policy::permissive`] for every workspace run it resumed, so a caller who
/// ran under a deny-by-default policy and crashed came back with no boundary and
/// nothing said so. Refusing is the only behaviour that cannot silently widen
/// what an agent may do.
///
/// What it preserves: the run id, the step it reached, its token and wall-clock
/// budgets, and — since 0.13.0 — the observation ledger it had assembled, so the
/// resumed run asks the model what the interrupted one would have. What it does
/// not: a permission policy, which it refuses to guess at rather than
/// substituting one. A run with no recorded policy, which is every run
/// checkpointed before 0.13.0, resumes exactly as it did then.
pub async fn resume<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
) -> Result<RunResult> {
    resume_observed(contract, provider, store, run_id, &Ignore).await
}

/// [`resume`], reporting to `observer` as it happens. See [`run_observed`].
///
/// A resume of an already-finished run is a no-op and reports no events: it drives
/// nothing, so there is nothing to watch.
pub async fn resume_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    observer: &dyn Observer,
) -> Result<RunResult> {
    // Refuse a store from a newer checkpoint format or a missing run with a
    // typed error, rather than misreading it or panicking. Before the policy
    // gate below, so an unknown run still reports as an unknown run.
    store.check_resumable(run_id)?;
    // Before the gate, because a finished run is a read and not a resume: it
    // drives no loop, performs no action, and asks no provider, so there is no
    // boundary to drop and refusing it would break the "report, don't re-drive"
    // contract a refused or escalated run relies on.
    if let Some(o) = finished_outcome(store, run_id)? {
        return Ok(RunResult::new(o, run_id));
    }
    // The gate. `None` means nothing recorded a policy for this run — a run
    // written by 0.12.0 or earlier — and is deliberately not read as "the caller
    // chose permissive": it resumes as it always did, which is what 0.7.0's
    // resume contract promised those runs. A recorded permissive policy is the
    // same case, said explicitly. Anything else had a boundary, and this
    // function cannot honour it.
    if let Some(recorded) = store.run_policy(run_id)? {
        if !recorded.is_permissive() {
            return Err(crate::error::Error::Resume {
                reason: format!(
                    "run {run_id} was started under a permission policy; resume it with \
                     resume_with (or resume_with_observed), supplying that policy — resuming \
                     here would drop the boundary the run was executing under"
                ),
            });
        }
    }
    resume_with_observed(
        contract,
        provider,
        store,
        run_id,
        &Policy::permissive(),
        &ApproveAll,
        observer,
    )
    .await
}

/// Resume an interrupted run under `policy`, routing anything the policy marks
/// [`Effect::Ask`] to `approver` — the resume twin of [`run_with`].
///
/// The policy given here is the one that governs the resumed run; it is recorded
/// against the run, so the store answers what rules the run actually executed
/// under rather than only what it started under. Supplying
/// [`Policy::permissive`] deliberately downgrades a run that had a boundary,
/// which is a caller's decision to make explicitly and is exactly what [`resume`]
/// will not do on its behalf.
pub async fn resume_with<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    resume_with_observed(contract, provider, store, run_id, policy, approver, &Ignore).await
}

/// [`resume_with`], reporting to `observer` as it happens. See [`run_observed`].
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    contract.tools.validate()?;
    let skills = contract.discover_skills()?;
    store.check_resumable(run_id)?;

    if let Some(o) = finished_outcome(store, run_id)? {
        return Ok(RunResult::new(o, run_id));
    }

    let caller_enforces = !policy.is_permissive();
    let start_step = record_resume_markers(store, run_id)?;
    store.set_provider(run_id, provider.name())?;
    store.record_run_policy(run_id, policy)?;
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        start_step.saturating_sub(1),
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));
    match contract.root.clone() {
        Some(root) => {
            // Re-authorized on resume rather than trusted from the interrupted
            // run, for the reason [`resume_tree_observed`] gives: the policy
            // handed to the resume is the one that governs it, and a host allowed
            // before a crash may not be allowed after.
            let policy = &match authorize_provider(provider, policy, store, run_id, approver, watch)
                .await?
            {
                ProviderAccess::Granted(p) => p,
                ProviderAccess::Pending(request_id) => {
                    return Ok(RunResult::new(
                        RunOutcome::AwaitingApproval {
                            request_id,
                            steps: start_step.saturating_sub(1),
                        },
                        run_id,
                    ))
                }
            };
            let mcp = McpSession::connect(&contract.mcp, policy, store, run_id, watch).await?;
            let result = run_workspace_from(
                contract, provider, store, run_id, &root, start_step, policy, approver, &mcp,
                &skills, watch,
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            result
        }
        // The same refusal [`run_with_observed`] makes, for the same reason:
        // single-file mode has no policy-aware tool layer, and silently ignoring
        // a policy would leave the caller believing a boundary was enforced when
        // nothing was checking.
        None if caller_enforces => Err(crate::error::Error::Config(
            "a permission policy requires workspace mode — build the contract \
             with TaskContract::workspace(goal, root, verify). Single-file \
             contracts are not policy-enforced."
                .into(),
        )),
        None => run_from(contract, provider, store, run_id, start_step, watch).await,
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
///
/// Preserves the policy — it is an argument — and, since 0.13.0, the run's
/// observation ledger. It is for a run that *paused*, though: a run that crashed
/// has no `request_id` and no pending decision to supply, and wants
/// [`resume_with`].
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
    resume_with_decision_observed(
        contract, provider, store, run_id, request_id, decision, policy, approver, &Ignore,
    )
    .await
}

/// [`resume_with_decision`], reporting to `observer` as it happens. See
/// [`run_observed`].
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_decision_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    request_id: i64,
    decision: Decision,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    contract.tools.validate()?;
    let skills = contract.discover_skills()?;
    let pending = store
        .pending(request_id)?
        .ok_or_else(|| crate::error::Error::Config(format!("no pending request {request_id}")))?;
    if pending.run_id != run_id {
        return Err(crate::error::Error::Config(format!(
            "request {request_id} belongs to run {}, not {run_id}",
            pending.run_id
        )));
    }

    let root = contract.root.clone().ok_or_else(|| {
        crate::error::Error::Config("resume_with_decision needs a workspace".into())
    })?;
    let step = pending.step;
    // The run row and its provider have existed since the run that paused, so
    // `Started` here says "this process is now driving it" — one per entry point,
    // never zero, so a `Finished` below is never the first thing an observer hears.
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        step,
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));

    match decision {
        // Deferring again leaves it pending and the run paused.
        Decision::Defer => Ok(RunResult::new(
            RunOutcome::AwaitingApproval {
                request_id,
                steps: step,
            },
            run_id,
        )),
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
            finish(store, watch, run_id, 0, step, "denied")?;
            Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id))
        }
        // A deferred *network* action has no filesystem effect to replay: the
        // run paused before its first step, so approving it grants the host and
        // starts the loop. Routing it through the write path below would check
        // a host against the path policy and then try to create a file named
        // after it.
        Decision::Approve { ref remember, .. } if pending.act == "net" => {
            let effective = policy
                .clone()
                .merge(net::provider_layer(&pending.target))
                .merge(remembered_layer(remember));
            store.resolve_pending(request_id, "approve")?;
            store.record_event(
                run_id,
                &PolicyEvent::decision(
                    step,
                    "net",
                    &pending.target,
                    "approve",
                    format!("resumed:{request_id}"),
                ),
            )?;
            let remember = remember.clone();
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let result = run_workspace_from(
                contract,
                provider,
                store,
                run_id,
                &root,
                step + 1,
                &effective,
                approver,
                &mcp,
                &skills,
                watch,
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            result.map(|r| r.with_remembered(remember))
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
            let act = if pending.act == "read" {
                Act::Read
            } else {
                Act::Write
            };
            let recheck = ws.check_path(act, &target);
            if recheck.effect == Effect::Deny {
                let mut ev = PolicyEvent::refusal(step, &pending.act, &target);
                ev.rule = recheck.rule.clone();
                ev.layer = recheck.layer.clone();
                store.record_event(run_id, &ev)?;
                refused(watch, run_id, 0, &ev);
                store.resolve_pending(request_id, "deny")?;
                finish(store, watch, run_id, 0, step, "denied")?;
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
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let result = run_workspace_from(
                contract,
                provider,
                store,
                run_id,
                &root,
                step + 1,
                &effective,
                approver,
                &mcp,
                &skills,
                watch,
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            result.map(|r| r.with_remembered(remember))
        }
    }
}

/// Continue an agent *tree* that paused at [`RunOutcome::AwaitingApproval`],
/// once a human has decided — the tree counterpart of [`resume_with_decision`].
///
/// The pending action belongs to whichever agent in the tree deferred (often a
/// child, not the root), so the decision is validated against the whole tree,
/// not just the root run id. On approve the deferred action is performed once,
/// the pending is resolved, and the tree is resumed from the store exactly as
/// [`resume_tree`] does: the root replays its (deliberately uncommitted) pause
/// step, re-adopts the paused child, and the child continues past the
/// now-applied action. A denial stops the tree.
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_decision<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    request_id: i64,
    decision: Decision,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    resume_tree_with_decision_observed(
        contract,
        provider,
        store,
        run_id,
        request_id,
        decision,
        policy,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`resume_tree_with_decision`], reporting to `observer` as it happens. See
/// [`run_observed`].
///
/// One observer watches the whole tree: every agent's events carry that agent's
/// own `run_id` and `depth`, so a consumer routes on those rather than being
/// handed one observer per child.
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_decision_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    request_id: i64,
    decision: Decision,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    store.check_resumable(run_id)?;
    contract.tools.validate()?;
    let skills = contract.discover_skills()?;
    let pending = store
        .pending(request_id)?
        .ok_or_else(|| crate::error::Error::Config(format!("no pending request {request_id}")))?;
    // The pending may belong to any agent in this tree, not only the root.
    if !store.tree_run_ids(run_id)?.contains(&pending.run_id) {
        return Err(crate::error::Error::Config(format!(
            "request {request_id} belongs to run {}, which is not in the tree rooted at {run_id}",
            pending.run_id
        )));
    }
    let root = contract.root.clone().ok_or_else(|| {
        crate::error::Error::Config("resume_tree_with_decision needs a workspace".into())
    })?;
    let step = pending.step;
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        step,
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));

    match decision {
        Decision::Defer => Ok(RunResult::new(
            RunOutcome::AwaitingApproval {
                request_id,
                steps: step,
            },
            run_id,
        )),
        Decision::Deny { reason } => {
            store.resolve_pending(request_id, "deny")?;
            store.record_event(
                pending.run_id,
                &PolicyEvent::decision(
                    step,
                    &pending.act,
                    &pending.target,
                    "deny",
                    format!("resumed:{request_id}"),
                ),
            )?;
            info!(run_id, request_id, %reason, "deferred tree action denied; tree stops");
            finish(store, watch, run_id, 0, step, "denied")?;
            Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id))
        }
        // As in `resume_with_decision`: an approved network action grants the
        // host and starts the tree, with no filesystem effect to replay.
        Decision::Approve { ref remember, .. } if pending.act == "net" => {
            let effective = policy
                .clone()
                .merge(net::provider_layer(&pending.target))
                .merge(remembered_layer(remember));
            store.resolve_pending(request_id, "approve")?;
            store.record_event(
                pending.run_id,
                &PolicyEvent::decision(
                    step,
                    "net",
                    &pending.target,
                    "approve",
                    format!("resumed:{request_id}"),
                ),
            )?;
            let ledger = Arc::new(Ledger::from_state(
                containment,
                store.spent_tokens_tree(run_id)?,
                store.agent_count_tree(run_id)?,
            ));
            let start_step = record_resume_markers(store, run_id)?;
            store.set_provider(run_id, provider.name())?;
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let tree = Tree {
                mcp: &mcp,
                tools: &contract.tools,
                skills: &skills,
                provider,
                store,
                approver,
                watch,
                ledger,
                containment,
                root,
                root_run_id: run_id,
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step).await;
            mcp.shutdown(store, run_id, watch).await;
            Ok(RunResult::new(outcome?, run_id).with_remembered(remember.clone()))
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

            // ponytail: the deferred write is re-checked against the tree policy,
            // not the child's narrowed policy (not reconstructed here). A denial
            // beneath still holds; a narrower child allow is not re-enforced on
            // this one performed action. Tighten if child-specific deny of an
            // approved action becomes a requirement.
            let ws = Workspace::with_policy(&root, policy.clone());
            let act = if pending.act == "read" {
                Act::Read
            } else {
                Act::Write
            };
            if ws.check_path(act, &target).effect == Effect::Deny {
                store.resolve_pending(request_id, "deny")?;
                finish(store, watch, run_id, 0, step, "denied")?;
                return Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id));
            }
            if act == Act::Write {
                ws.write_file(&target, content.as_deref().unwrap_or_default())?;
            }
            store.resolve_pending(request_id, "approve")?;
            store.record_event(
                pending.run_id,
                &PolicyEvent::decision(
                    step,
                    &pending.act,
                    &pending.target,
                    "approve",
                    format!("resumed:{request_id}"),
                ),
            )?;

            // Resume the whole tree; the root replays its uncommitted pause step
            // and re-adopts the (now-unblocked) child.
            let mut effective = policy.clone();
            if !remember.is_empty() {
                let mut layer = Policy::permissive().layer("remembered");
                for r in &remember {
                    layer = layer.rule(r.act, r.effect, r.pattern.clone());
                }
                effective = effective.merge(layer);
            }
            let ledger = Arc::new(Ledger::from_state(
                containment,
                store.spent_tokens_tree(run_id)?,
                store.agent_count_tree(run_id)?,
            ));
            let start_step = record_resume_markers(store, run_id)?;
            store.set_provider(run_id, provider.name())?;
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let tree = Tree {
                mcp: &mcp,
                tools: &contract.tools,
                skills: &skills,
                provider,
                store,
                approver,
                watch,
                ledger,
                containment,
                root,
                root_run_id: run_id,
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step).await;
            mcp.shutdown(store, run_id, watch).await;
            Ok(RunResult::new(outcome?, run_id).with_remembered(remember))
        }
    }
}

/// Reconstruct the *final* [`RunOutcome`] of a run that cannot be meaningfully
/// re-driven, so a resume of such a run is a faithful no-op. Only genuinely
/// final outcomes are returned: `success`, `denied` (a human's no), and a tree
/// `budget_ceiling_reached`. A run that merely ran out of step / token / time
/// budget is deliberately NOT final — a caller resumes it with a larger budget
/// to continue — so those return `None` and resume re-drives the loop (which is
/// itself idempotent: re-running with the same budget skips the exhausted loop
/// and reports the same outcome without spending anything). `awaiting_approval`
/// is `Paused`, not `Completed`, and resumes via [`resume_with_decision`].
/// Record the resume marker and one skipped marker per already-committed step,
/// so a multi-crash run's full history is reconstructable from the store alone.
/// Returns the step to resume from (last committed + 1).
fn record_resume_markers(store: &Store, run_id: i64) -> Result<u32> {
    let last = store.last_step(run_id)?;
    let start_step = last + 1;
    store.record_checkpoint_event(&crate::state::CheckpointEvent::resume(
        run_id,
        start_step,
        format!("resuming at step {start_step}, {last} committed step(s) skipped"),
    ))?;
    for s in 1..=last {
        store.record_checkpoint_event(&crate::state::CheckpointEvent::skipped(run_id, s))?;
    }
    Ok(start_step)
}

/// A run's ledger as the store has it, with the count already durable.
///
/// The count is the watermark [`persist_ledger`] appends from: everything below
/// it is on disk, everything above it was observed since the last committed
/// step.
fn restore_ledger(store: &Store, run_id: i64) -> Result<(ContextLedger, usize)> {
    let mut ledger = ContextLedger::new();
    for obs in store.observations(run_id)? {
        ledger.push(obs);
    }
    let written = ledger.len();
    Ok((ledger, written))
}

/// Append everything observed since the last committed step, and return the new
/// watermark.
///
/// Called at the step boundary that commits, so an observation belonging to a
/// step that never committed does not outlive it — the ledger stays consistent
/// with the trace rather than running ahead of it.
fn persist_ledger(
    store: &Store,
    run_id: i64,
    ledger: &ContextLedger,
    written: usize,
) -> Result<usize> {
    store.record_observations(run_id, &ledger.entries()[written..])?;
    Ok(ledger.len())
}

/// The outcome of a run that is already over, if it is over.
///
/// Idempotence for every resume entry point: a run that already finished is
/// returned as-is, so resuming twice does not re-drive the loop or re-charge the
/// budget. A run still `Running` — its process died mid-loop — reads as `None`
/// here and is resumed from its last committed step.
fn finished_outcome(store: &Store, run_id: i64) -> Result<Option<RunOutcome>> {
    if store.run_status(run_id)? != Some(RunStatus::Completed) {
        return Ok(None);
    }
    terminal_outcome(store, run_id)
}

fn terminal_outcome(store: &Store, run_id: i64) -> Result<Option<RunOutcome>> {
    let last = store.last_step(run_id)?;
    Ok(store.outcome(run_id)?.and_then(|o| match o.as_str() {
        "success" => Some(RunOutcome::Success { steps: last }),
        "denied" => Some(RunOutcome::Denied { steps: last }),
        "budget_ceiling_reached" => Some(RunOutcome::BudgetCeilingReached { steps: last }),
        "stalled" => Some(RunOutcome::Stalled { steps: last }),
        // Before 0.11.0 `"escalated"` was unmapped and `finish_run` reported it as a
        // plain completion, so resuming an escalated run fell straight back into the
        // loop and re-ran it. An unattended run that escalated at 3am was silently
        // restarted by the next resume.
        "escalated_retryable" => Some(RunOutcome::Escalated {
            steps: last,
            retryable: true,
        }),
        "escalated_terminal" | "escalated" => Some(RunOutcome::Escalated {
            steps: last,
            retryable: false,
        }),
        // 0.12.0: the same defect, found by the same kind of audit. `"refused"` is
        // written when a human denies the network access the provider needs
        // (`authorize_provider`), and it was unmapped — so resuming a refused run
        // re-entered the loop and asked the human the same question again. A human's
        // no is as final as a policy's, which is why `denied` above is final too.
        "refused" => Some(RunOutcome::Refused { steps: last }),
        // Also 0.12.0: a run its observer stopped. Final for the same reason a
        // human's `denied` is — the caller asked for it — so a resume reports it
        // rather than quietly starting the run up again.
        "cancelled" => Some(RunOutcome::Cancelled { steps: last }),
        _ => None,
    }))
}

/// The [`Observer`] a run reports to, plus the one bit of state it can set.
///
/// A wrapper rather than a bare `&dyn Observer` because a cancellation has to
/// outlive the `event()` call that asked for it: [`Flow::Cancel`](crate::Flow::Cancel)
/// is honoured at the next step boundary, not where it was returned, so the
/// request is remembered here — and one `Watch` shared by a whole tree means a
/// child's observer can stop the tree, not only itself.
///
/// A [`Cell`] is enough: [`Store`] is `!Sync` and `run_agent` returns a
/// non-`Send` future, so a run and every agent in its tree are driven on one
/// task. Nothing here needs a lock, and adding one would be the only `Sync`
/// requirement in the loop.
pub(crate) struct Watch<'a> {
    observer: &'a dyn Observer,
    cancelled: Cell<bool>,
}

impl<'a> Watch<'a> {
    fn new(observer: &'a dyn Observer) -> Self {
        Self {
            observer,
            cancelled: Cell::new(false),
        }
    }

    /// Report one event, remembering a cancellation for the next step boundary.
    pub(crate) fn emit(&self, event: RunEvent) {
        if self.observer.event(&event).is_cancel() {
            self.cancelled.set(true);
        }
    }

    /// Whether stopping has been asked for. Read at a step boundary only.
    fn cancelled(&self) -> bool {
        self.cancelled.get()
    }
}

/// Announce a refusal, reading the event straight off the row that records it.
///
/// Every `Refused` in the crate goes through here, and takes the `PolicyEvent`
/// rather than the four fields, so an event cannot carry a rule or a layer the
/// `policy_events` row does not. The two surfaces agree by construction instead
/// of by four call sites remembering to keep in step.
pub(crate) fn refused(watch: &Watch<'_>, run_id: i64, depth: u32, ev: &PolicyEvent) {
    watch.emit(RunEvent::at_depth(
        run_id,
        ev.step,
        depth,
        EventKind::Refused {
            act: ev.act.clone(),
            target: ev.target.clone(),
            rule: ev.rule.clone(),
            layer: ev.layer.clone(),
        },
    ));
}

/// Announce a human's answer, from the row that records it. As [`refused`]: the
/// event's `decision` is the row's, never a second literal beside it.
fn decided(watch: &Watch<'_>, run_id: i64, depth: u32, ev: &PolicyEvent) {
    watch.emit(RunEvent::at_depth(
        run_id,
        ev.step,
        depth,
        EventKind::ApprovalDecided {
            act: ev.act.clone(),
            target: ev.target.clone(),
            decision: ev.decision.clone().unwrap_or_default(),
        },
    ));
}

/// The one step boundary: commit the step, log it, and tell the observer.
///
/// Before 0.12.0 the single-file loop, the workspace loop and the sub-agent loop
/// each had their own copy of this — their own inline [`StepRecord`], their own
/// `checkpoint_step`, and their own differently-named `info!` ("loop step" /
/// "workspace step" / "agent step"). One boundary is what stops the three
/// drifting, and what makes [`EventKind::Step`] one fact about a committed step
/// rather than three approximations of one.
///
/// `commit` is `false` for exactly one case: the sub-agent loop's step that
/// paused because one of its CHILDREN deferred. That step is deliberately left
/// uncommitted so a resume replays it and re-adopts the paused child — only the
/// parent re-entering `spawn_child` can wait on that child again — and committing
/// it would skip the replay, which is the double-execution defect 0.7.0's
/// checkpointing exists to prevent. Nothing is committed and no
/// [`EventKind::Step`] is emitted, because there is no committed step to report:
/// what the caller hears about is the pause, through
/// [`RunOutcome::AwaitingApproval`].
fn commit_step(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    record: StepRecord,
    changed: bool,
    commit: bool,
) -> Result<()> {
    if !commit {
        info!(
            run_id,
            depth,
            step = record.step,
            "tree paused for a child's approval (step left uncommitted for replay)"
        );
        return Ok(());
    }
    store.checkpoint_step(run_id, &record)?;
    info!(
        run_id,
        depth,
        step = record.step,
        decision = %record.decision,
        tokens = record.tokens,
        changed,
        "step"
    );
    // The record's own fields, moved rather than cloned: an event must report
    // exactly what was committed, and the unobserved path must not pay an
    // allocation per step for the privilege of being ignored.
    watch.emit(RunEvent::at_depth(
        run_id,
        record.step,
        depth,
        EventKind::Step {
            decision: record.decision,
            tool_call: record.tool_call,
            tokens: record.tokens,
            changed,
        },
    ));
    Ok(())
}

/// Stop the run if the observer asked it to, recording `"cancelled"` as the
/// outcome. `None` means carry on.
///
/// Call this at a step boundary and nowhere else — that is the contract
/// [`Flow::Cancel`](crate::Flow::Cancel) states, and the whole reason the request
/// is remembered in [`Watch`] rather than acted on where it was returned. The run
/// is *finished*, not abandoned: `runs.status` stops being `running`, a summary is
/// written, and `terminal_outcome` maps the string back so a resume reports the
/// cancellation instead of re-driving the loop.
fn cancelled(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    steps: u32,
) -> Result<Option<RunOutcome>> {
    if !watch.cancelled() {
        return Ok(None);
    }
    finish(store, watch, run_id, depth, steps, "cancelled")?;
    info!(run_id, depth, steps, "run cancelled by its observer");
    Ok(Some(RunOutcome::Cancelled { steps }))
}

/// End a run: write the outcome and tell the observer, so no terminal path can do
/// one without the other. Every `finish_run` in this file goes through here.
///
/// `steps` is what the outcome reports, which is not always the step the loop was
/// on — a time-budget stop reports the last step that completed.
fn finish(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    depth: u32,
    steps: u32,
    outcome: &str,
) -> Result<()> {
    store.finish_run(run_id, outcome)?;
    watch.emit(RunEvent::at_depth(
        run_id,
        steps,
        depth,
        EventKind::Finished {
            outcome: outcome.to_string(),
            steps,
            // Read back from the store rather than carried in a local: the store
            // is what an audit will read, and the two must agree.
            tokens: store.spent_tokens(run_id)?,
        },
    ));
    Ok(())
}

async fn run_from<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    start_step: u32,
    watch: &Watch<'_>,
) -> Result<RunResult> {
    let fs = FsTool::new(&contract.file);
    let system = system_prompt();
    let tool = write_file_tool();
    // Durable budget: spend and elapsed time are restored from the store, so a
    // resume continues one continuous budget instead of restarting it at zero.
    let mut tokens_used: u64 = store.spent_tokens(run_id)?;
    // Single-file mode is not policy-enforced (0.4.0), but the verify gate is
    // still sandboxed (0.6.0). A permissive guard carries the trace so the
    // sandbox lifecycle is recorded for single-file runs too.
    let permissive = Policy::permissive();

    for step in start_step..=contract.max_steps {
        // A cancellation is acted on here, at the boundary between two steps, and
        // nowhere else: the points inside a step are not safe to stop at — a tool
        // call is in flight, a file may be half-written — and stopping there is
        // what dropping the future already does badly. Checked before the budgets
        // because the caller asking to stop outranks a budget saying so.
        if let Some(o) = cancelled(store, watch, run_id, 0, step - 1)? {
            return Ok(RunResult::new(o, run_id));
        }
        // Time budget: checked before doing the step's work, against real
        // wall-clock elapsed since the run started (durable across a restart).
        if let Some(max) = contract.max_duration {
            if store.elapsed_secs(run_id)? > max.as_secs_f64() {
                finish(store, watch, run_id, 0, step - 1, "time_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                ));
            }
        }

        let current = fs.read().await?;
        // The whole file goes back every turn, so it is bounded on the same terms
        // as any observation: one large file must not exhaust the request. The tail
        // is kept, because the end of a file is what a writer needs.
        let user =
            user_prompt(
                contract,
                &bound(
                    &current,
                    entry_cap_chars(contract.context.effective_tokens(
                        contract.max_tokens.map(|m| m.saturating_sub(tokens_used)),
                    )),
                    ObsKind::Read,
                ),
            );
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: vec![tool.clone()],
        };

        let response =
            complete_with_retry(provider, &request, contract, store, run_id, step, watch, 0)
                .await?;

        // Which provider answered, when that is not a foregone conclusion. A
        // `Fallback` that fell over served this step from its secondary, and a trace
        // reader has no other way to know.
        if let Some(served) = provider.last_served() {
            store.record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
            watch.emit(RunEvent::new(
                run_id,
                step,
                EventKind::FellBackTo { provider: served },
            ));
        }
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
        // The file write (if any) is already applied above, before this commit:
        // a crash between the write and the commit replays this step, and the
        // model re-observes the already-written file, so the edit lands exactly
        // once. The committed checkpoint is the step's completion marker.
        //
        // Single-file mode has no workspace-change signal of its own, so `changed`
        // is whether this step wrote the file at all — the nearest true statement
        // the mode can make.
        commit_step(
            store,
            watch,
            run_id,
            0,
            StepRecord::new(step, decision, result_text).with_trace(
                user,
                tool_call_json,
                step_tokens,
            ),
            write.is_some(),
            true,
        )?;

        // Cost budget: checked after this step's tokens are counted.
        if let Some(max) = contract.max_tokens {
            if tokens_used > max {
                finish(store, watch, run_id, 0, step, "cost_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::CostBudgetExceeded { steps: step },
                    run_id,
                ));
            }
        }

        let contents = fs.read().await?;
        let guard = ExecGuard::new(&permissive)
            .tracing(store, run_id, step)
            .watching(watch, 0);
        if contract
            .verify
            .passes_guarded(&contract.file, &contents, &guard)
            .await?
        {
            finish(store, watch, run_id, 0, step, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id));
        }
    }

    finish(
        store,
        watch,
        run_id,
        0,
        contract.max_steps,
        "step_cap_reached",
    )?;
    Ok(RunResult::new(
        RunOutcome::StepCapReached {
            steps: contract.max_steps,
        },
        run_id,
    ))
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
    mcp: &McpSession,
    skills: &Skills,
    watch: &Watch<'_>,
) -> Result<RunResult> {
    // The effective policy grows as approvers remember rules; it is rebuilt as a
    // merge so a remembered allow can still never defeat a deny beneath it.
    let mut effective = policy.clone();
    let mut remembered: Vec<Rule> = Vec::new();
    let mut ws = Workspace::with_policy(root, effective.clone());
    // MCP tools sit beside the built-ins under their namespaced names, so the
    // model chooses between them the same way it chooses between grep and find.
    // Registered in-process tools and MCP tools sit beside the built-ins under
    // their own names, so the model chooses between them the same way it chooses
    // between grep and find.
    let mut extra = contract.tools.specs();
    extra.extend(mcp.tool_specs());
    extra.extend(skill_tool(skills));
    let system = with_skill_catalog(with_extra_tools(workspace_system_prompt(), &extra), skills);
    let mut tools = workspace_tools();
    tools.extend(extra);
    // Durable budget: restored from the store so a resume continues the same
    // token and wall-clock budget rather than restarting it at zero.
    let mut tokens_used: u64 = store.spent_tokens(run_id)?;
    // History, append-only. What the model sees of it is decided per turn by
    // `assemble`, under the contract's context budget — the log itself is never
    // trimmed, so the trace keeps everything.
    //
    // Restored from the store, so a resumed run continues with the context it had
    // rather than re-deriving one from the workspace and asking the model a
    // different question than the process before it would have. Empty for a fresh
    // run, and empty for a run checkpointed before 0.13.0, which is the same
    // re-derivation that binary did.
    let (mut ledger, mut written) = restore_ledger(store, run_id)?;
    // Is the agent getting anywhere? Restored from nothing on resume by design: a
    // resumed run has just been given a fresh chance, and condemning it for the
    // window it stalled in before the crash would be a poor welcome.
    let mut progress = Progress::new();
    let mem_key = memory_key(root);

    for step in start_step..=contract.max_steps {
        // The step boundary, where a cancellation is honoured (see `cancelled`).
        if let Some(o) = cancelled(store, watch, run_id, 0, step - 1)? {
            return Ok(RunResult::new(o, run_id).with_remembered(remembered));
        }
        if let Some(max) = contract.max_duration {
            if store.elapsed_secs(run_id)? > max.as_secs_f64() {
                finish(store, watch, run_id, 0, step - 1, "time_budget_exceeded")?;
                return Ok(RunResult::new(
                    RunOutcome::TimeBudgetExceeded { steps: step - 1 },
                    run_id,
                )
                .with_remembered(remembered));
            }
        }

        // One budget, derived once per turn: it sets both this request's ceiling
        // and the per-observation cap the results of this step enter under.
        let budget_tokens = contract
            .context
            .effective_tokens(contract.max_tokens.map(|m| m.saturating_sub(tokens_used)));
        let entry_cap = entry_cap_chars(budget_tokens);
        // Re-read each turn rather than once at the start, so the notes the model
        // sees are the notes the store holds — including one written this run, and
        // not one the operator has since cleared.
        let notes = store.memory_list(&mem_key)?;
        let assembled = assemble(
            &ledger,
            budget_tokens,
            &notes,
            Assembly {
                ws: Some(&ws),
                policy: &effective,
                store,
                run_id,
                step,
            },
        )
        .await?;
        let user = workspace_user_prompt(contract, &assembled.text);
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: tools.clone(),
        };

        let response =
            complete_with_retry(provider, &request, contract, store, run_id, step, watch, 0)
                .await?;

        // Which provider answered, when that is not a foregone conclusion. A
        // `Fallback` that fell over served this step from its secondary, and a trace
        // reader has no other way to know.
        if let Some(served) = provider.last_served() {
            store.record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
            watch.emit(RunEvent::new(
                run_id,
                step,
                EventKind::FellBackTo { provider: served },
            ));
        }
        let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
        tokens_used += step_tokens;
        // The provider's own number for the request `assemble` just built, beside
        // the estimate: the pair is what makes the estimator's drift auditable. A
        // silent provider leaves it null rather than recording a zero.
        if step_tokens > 0 {
            store.record_context_reported(run_id, step, step_tokens)?;
        }

        // Dispatch every tool call the model made this step, in order, folding
        // each result into the observation log the next turn will see.
        let mut decisions: Vec<String> = Vec::new();
        let mut calls_json: Vec<String> = Vec::new();
        // Did this step move the workspace? Only a write that wrote something
        // different can, and it is the half of the stall signal that says the agent
        // is not merely repeating itself but achieving nothing.
        let mut step_changed = false;
        if response.tool_calls.is_empty() {
            let said = response.text.clone().unwrap_or_default();
            ledger.push(Observation::new(
                step,
                ObsKind::Message,
                None,
                bound(
                    &format!("\n[step {step}] (no tool call) {said}\n"),
                    entry_cap,
                    ObsKind::Message,
                ),
            ));
            decisions.push("no tool call".into());
        }
        let mut paused: Option<i64> = None;
        let mut new_rules: Vec<Rule> = Vec::new();
        for call in &response.tool_calls {
            calls_json.push(format!("{}:{}", call.name, call.arguments));
            match dispatch(
                &ws,
                call,
                approver,
                store,
                run_id,
                step,
                mcp,
                &contract.tools,
                skills,
                entry_cap,
                &mem_key,
                watch,
                0,
            )
            .await?
            {
                Dispatched::Continue {
                    decision,
                    obs,
                    kind,
                    target,
                    changed,
                    remember,
                } => {
                    step_changed |= changed;
                    ledger.push(Observation::new(step, kind, target, obs));
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

        // The trace gets this step's observations unelided, so concatenating the
        // rows in step order reproduces the whole log: bounding what the model
        // sees must not bound what an operator can audit. A delta rather than the
        // whole log per row, so the trace is linear in the step count and a
        // 24-hour run does not write the same text hundreds of times.
        // ponytail: each row repeats the whole log, so the column grows with the
        // square of the step count. Bounded in practice by the step budget times
        // the entry cap; write per-step deltas if a long run's store size matters.
        //
        // The assembly stats this line used to log (`carried`, `stubbed`,
        // `est_tokens`) are not lost with the loop's own `info!`: `assemble`
        // records them as the step's `"assembled"` context event, which is where a
        // reader could already find them.
        commit_step(
            store,
            watch,
            run_id,
            0,
            StepRecord::new(step, decisions.join("; "), ledger.text_for_step(step)).with_trace(
                user,
                calls_json.join(" | "),
                step_tokens,
            ),
            step_changed,
            true,
        )?;
        // The step is committed, so the observations behind it are safe to make
        // durable. After the commit rather than before: a ledger that ran ahead of
        // the trace would restore observations for a step the run never took.
        written = persist_ledger(store, run_id, &ledger, written)?;

        // Did that step get anywhere? A stall needs both halves — nothing changed
        // in the workspace AND a tool call this window already saw — because a
        // legitimate exploration phase changes nothing either, and flagging that
        // would degrade healthy runs to add resilience.
        let signature = calls_json.join(" | ");
        match progress.step(contract.stall, step_changed, &signature) {
            Progressing::Fine => {}
            Progressing::Replan => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::replan(
                        step,
                        format!(
                            "{} steps without progress; replanning",
                            contract.stall.window
                        ),
                    ),
                )?;
                // The directive is an observation like any other, so it is bounded
                // by the same budget and, carrying no target, can never be
                // superseded away.
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &progress.replan_directive(contract.stall.window, &decisions),
                        entry_cap,
                        ObsKind::Message,
                    ),
                ));
                info!(run_id, step, "agent told to change approach");
                watch.emit(RunEvent::new(
                    run_id,
                    step,
                    EventKind::Replan {
                        window: contract.stall.window,
                    },
                ));
            }
            Progressing::Stalled => {
                store.record_context_event(
                    run_id,
                    &ContextEvent::stalled(step, "still no progress after replanning"),
                )?;
                info!(run_id, step, "run stopped: stalled");
                watch.emit(RunEvent::new(run_id, step, EventKind::Stalled));
                finish(store, watch, run_id, 0, step, "stalled")?;
                return Ok(RunResult::new(RunOutcome::Stalled { steps: step }, run_id)
                    .with_remembered(remembered));
            }
        }

        // An approver deferred: persist nothing further, stop, and let the
        // caller resume once a human has decided.
        if let Some(request_id) = paused {
            finish(store, watch, run_id, 0, step, "awaiting_approval")?;
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
                finish(store, watch, run_id, 0, step, "cost_budget_exceeded")?;
                return Ok(
                    RunResult::new(RunOutcome::CostBudgetExceeded { steps: step }, run_id)
                        .with_remembered(remembered),
                );
            }
        }

        if contract
            .verify
            .passes_in_guarded(
                root,
                &ExecGuard::new(&effective)
                    .tracing(store, run_id, step)
                    .watching(watch, 0),
            )
            .await?
        {
            finish(store, watch, run_id, 0, step, "success")?;
            return Ok(RunResult::new(RunOutcome::Success { steps: step }, run_id)
                .with_remembered(remembered));
        }
    }

    finish(
        store,
        watch,
        run_id,
        0,
        contract.max_steps,
        "step_cap_reached",
    )?;
    Ok(RunResult::new(
        RunOutcome::StepCapReached {
            steps: contract.max_steps,
        },
        run_id,
    ))
}

/// Shared context for one agent tree: everything every agent in the tree
/// draws on — the provider, the store, the one approver, the shared spend
/// ledger, the containment caps, and the workspace root.
struct Tree<'a, P: Provider> {
    /// One MCP session for the whole tree. A server is a stateful process, so
    /// 100 concurrent agents get 100 views of one connection, not 100 of their
    /// own — the same reason the ledger and the store are shared here.
    mcp: &'a McpSession,
    /// The caller's registered tools, shared by the whole tree. A child is
    /// offered exactly what its parent was: inheritance grants the tool, and the
    /// child's own narrowed policy still decides each call. Carried here rather
    /// than read from each agent's contract so a spawned child — whose contract
    /// the *model* writes — cannot register a tool its parent never had.
    tools: &'a Toolbox,
    /// The catalogue discovered from the ROOT contract, shared by the whole tree
    /// for the same reason `tools` is: a child contract the *model* wrote must
    /// not be able to conjure skills its parent was never offered.
    skills: &'a Skills,
    provider: &'a P,
    store: &'a Store,
    approver: &'a dyn Approver,
    /// One observer for the whole tree, exactly as there is one approver: every
    /// event carries the agent's own `run_id` and `depth`, so a consumer routes on
    /// those rather than being handed an observer per child. It also carries the
    /// tree's single cancellation flag, so a `Flow::Cancel` from any agent's event
    /// stops the tree at the next boundary rather than only that agent.
    watch: &'a Watch<'a>,
    ledger: Arc<Ledger>,
    containment: &'a Containment,
    root: PathBuf,
    /// The tree root's run id, so `Containment::max_total_duration` can be
    /// measured against when the TREE started rather than when this agent did.
    /// A child spawned twenty hours into a run has its own young `started_at`;
    /// the ceiling is about the whole tree, so the root's stamp is the only
    /// correct clock. Held here because [`Ledger`] has no store access.
    root_run_id: i64,
}

/// Run a workspace contract as the root of an agent tree under `containment`.
///
/// The root agent runs the workspace loop with one extra tool, [`SPAWN_TOOL`],
/// which launches a contained sub-agent. A child inherits the parent policy and
/// can only narrow it ([`Policy::contain`]); the whole tree draws its token
/// spend from one shared ledger no child contract can raise; and every spawn,
/// refusal, and budget draw is recorded so the tree is a reconstructable graph.
///
/// Sub-agents are opt-in: this is the only entry point that offers the spawn
/// tool. [`run_with`] and [`run`] are unchanged and never expose it.
pub async fn run_tree<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    run_tree_observed(
        contract,
        provider,
        store,
        policy,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`run_tree`], reporting to `observer` as it happens. See [`run_observed`].
///
/// One observer watches the whole tree: a child's events carry that child's own
/// `run_id` and its non-zero `depth`.
#[allow(clippy::too_many_arguments)]
pub async fn run_tree_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    contract.tools.validate()?;
    let skills = contract.discover_skills()?;
    let root = contract.root.clone().ok_or_else(|| {
        crate::error::Error::Config(
            "run_tree needs a workspace contract — build it with TaskContract::workspace".into(),
        )
    })?;
    let ledger = Arc::new(Ledger::new(containment));
    let run_id = store.start_run(&contract.goal, &root.display().to_string())?;
    store.set_provider(run_id, provider.name())?;
    // As in [`run_with_observed`]: the caller's policy, recorded before the
    // provider layer is merged in. A tree's own resume already takes a policy,
    // so this is for the audit rather than for a gate.
    store.record_run_policy(run_id, policy)?;
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        0,
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));
    // Authorized once at the root. Children inherit the root's policy through
    // `Policy::contain`, so the provider layer flows down the tree and no child
    // needs (or gets) its own chance to widen network access.
    let policy = &match authorize_provider(provider, policy, store, run_id, approver, watch).await?
    {
        ProviderAccess::Granted(p) => p,
        ProviderAccess::Pending(request_id) => {
            return Ok(RunResult::new(
                RunOutcome::AwaitingApproval {
                    request_id,
                    steps: 0,
                },
                run_id,
            ))
        }
    };
    let mcp = McpSession::connect(&contract.mcp, policy, store, run_id, watch).await?;
    let tree = Tree {
        mcp: &mcp,
        tools: &contract.tools,
        skills: &skills,
        provider,
        store,
        approver,
        watch,
        ledger,
        containment,
        root,
        root_run_id: run_id,
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, 1).await;
    mcp.shutdown(store, run_id, watch).await;
    Ok(RunResult::new(outcome?, run_id))
}

/// Resume a crashed agent tree under its original root `run_id`. Reconstructs
/// the whole 0.5.0 tree from the store: the shared spend ledger is restored from
/// the tree's durable total spend and agent count (so the resumed tree draws
/// against one continuous ceiling, never a reset one), and the root agent
/// resumes from its last committed step. As it replays its crashed step it
/// adopts the children it had already spawned and resumes each from that child's
/// own checkpoint (see `spawn_child`), so every agent in the tree continues
/// where it stopped rather than restarting.
///
/// Additive to [`run_tree`], mirroring how [`resume_with`] complements
/// [`run_with`]. Takes the policy and the approver, so a tree's boundary was
/// never at risk of being dropped across a resume the way the flat workspace
/// loop's was; since 0.13.0 every agent in the tree also restores its own
/// observation ledger.
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    resume_tree_observed(
        contract,
        provider,
        store,
        run_id,
        policy,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`resume_tree`], reporting to `observer` as it happens. See [`run_observed`].
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    contract.tools.validate()?;
    let skills = contract.discover_skills()?;
    store.check_resumable(run_id)?;

    // A finished tree is returned as-is — resume is idempotent for the whole tree.
    if store.run_status(run_id)? == Some(RunStatus::Completed) {
        if let Some(o) = terminal_outcome(store, run_id)? {
            return Ok(RunResult::new(o, run_id));
        }
    }

    let root = contract.root.clone().ok_or_else(|| {
        crate::error::Error::Config(
            "resume_tree needs a workspace contract — build it with TaskContract::workspace".into(),
        )
    })?;

    // Restore the shared ledger from durable tree-wide totals, so the budget is
    // continuous across the crash rather than reset to zero.
    let ledger = Arc::new(Ledger::from_state(
        containment,
        store.spent_tokens_tree(run_id)?,
        store.agent_count_tree(run_id)?,
    ));
    let start_step = record_resume_markers(store, run_id)?;
    store.set_provider(run_id, provider.name())?;
    let watch = &Watch::new(observer);
    watch.emit(RunEvent::new(
        run_id,
        start_step.saturating_sub(1),
        EventKind::Started {
            goal: contract.goal.clone(),
            provider: provider.name().to_string(),
        },
    ));
    // Re-authorized on resume rather than trusted from the crashed run: the
    // policy handed to the resume is the one that governs it, and a host allowed
    // before a crash may not be allowed after.
    let policy = &match authorize_provider(provider, policy, store, run_id, approver, watch).await?
    {
        ProviderAccess::Granted(p) => p,
        ProviderAccess::Pending(request_id) => {
            return Ok(RunResult::new(
                RunOutcome::AwaitingApproval {
                    request_id,
                    steps: start_step.saturating_sub(1),
                },
                run_id,
            ))
        }
    };
    let mcp = McpSession::connect(&contract.mcp, policy, store, run_id, watch).await?;
    let tree = Tree {
        mcp: &mcp,
        tools: &contract.tools,
        skills: &skills,
        provider,
        store,
        approver,
        watch,
        ledger,
        containment,
        root,
        root_run_id: run_id,
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, start_step).await;
    mcp.shutdown(store, run_id, watch).await;
    Ok(RunResult::new(outcome?, run_id))
}

/// One agent's loop, reused for the root and every child. Identical to the
/// workspace loop, plus: it may spawn children (recursively, via [`SPAWN_TOOL`]),
/// and its token spend is drawn from the tree's shared ledger rather than only
/// its own contract budget.
///
/// `depth` is 0 at the root; a child's depth is its parent's + 1. Returns the
/// agent's [`RunOutcome`]; a tree-wide budget halt propagates up as
/// [`RunOutcome::BudgetCeilingReached`].
fn run_agent<'f, P: Provider>(
    tree: &'f Tree<'_, P>,
    contract: &'f TaskContract,
    run_id: i64,
    depth: u32,
    policy: &'f Policy,
    start_step: u32,
) -> Pin<Box<dyn Future<Output = Result<RunOutcome>> + 'f>> {
    // Boxed so the loop can recurse into itself when an agent spawns a child.
    Box::pin(async move {
        let ws = Workspace::with_policy(&tree.root, policy.clone());
        // The tree shares one MCP session, so every agent in it — root or child —
        // is offered the same server tools beside its built-ins. Connecting a
        // session and then not offering its tools would leave the model unable to
        // call something the run had already paid to set up.
        let mut extra = tree.tools.specs();
        extra.extend(tree.mcp.tool_specs());
        extra.extend(skill_tool(tree.skills));
        let system =
            with_skill_catalog(with_extra_tools(tree_system_prompt(), &extra), tree.skills);
        let mut tools = tree_tools();
        tools.extend(extra);
        // The budget this agent runs under is the smaller of what its contract
        // asked for and what the tree has left — a contract cannot raise it.
        let token_cap = tree.ledger.effective_token_budget(contract.max_tokens);
        // Durable per-agent budget, restored across a restart.
        let mut tokens_used: u64 = tree.store.spent_tokens(run_id)?;
        // Same ledger and same per-turn assembly as the workspace loop: a tree of
        // 100 children each re-sending its own unbounded log is the multiplied
        // version of the problem 0.10.0 exists to fix — and, since 0.13.0, the
        // same restore, keyed on this agent's own run id. A child that is resumed
        // is the same child, at whatever depth it sits.
        let (mut ledger, mut written) = restore_ledger(tree.store, run_id)?;
        let mut progress = Progress::new();
        // Children share their parent's workspace, so they share its memory: one
        // note store per workspace, every entry attributed to the run that wrote it.
        let mem_key = memory_key(&tree.root);

        for step in start_step..=contract.max_steps {
            // The step boundary, where a cancellation is honoured (see `cancelled`).
            // One flag for the whole tree, so a cancel asked for while a sibling was
            // mid-flight stops this agent too.
            if let Some(o) = cancelled(tree.store, tree.watch, run_id, depth, step - 1)? {
                return Ok(o);
            }
            if let Some(max) = contract.max_duration {
                if tree.store.elapsed_secs(run_id)? > max.as_secs_f64() {
                    finish(
                        tree.store,
                        tree.watch,
                        run_id,
                        depth,
                        step - 1,
                        "time_budget_exceeded",
                    )?;
                    return Ok(RunOutcome::TimeBudgetExceeded { steps: step - 1 });
                }
            }

            let budget_tokens = contract
                .context
                .effective_tokens(Some(token_cap.saturating_sub(tokens_used)));
            let entry_cap = entry_cap_chars(budget_tokens);
            let notes = tree.store.memory_list(&mem_key)?;
            let assembled = assemble(
                &ledger,
                budget_tokens,
                &notes,
                Assembly {
                    ws: Some(&ws),
                    policy,
                    store: tree.store,
                    run_id,
                    step,
                },
            )
            .await?;
            let user = workspace_user_prompt(contract, &assembled.text);
            let request = CompletionRequest {
                system: system.clone(),
                user: user.clone(),
                tools: tools.clone(),
            };
            let response = complete_with_retry(
                tree.provider,
                &request,
                contract,
                tree.store,
                run_id,
                step,
                tree.watch,
                depth,
            )
            .await?;

            // Which provider answered, when that is not a foregone conclusion. A
            // `Fallback` that fell over served this step from its secondary, and a
            // trace reader has no other way to know.
            if let Some(served) = tree.provider.last_served() {
                tree.store
                    .record_context_event(run_id, &ContextEvent::served(step, served.clone()))?;
                tree.watch.emit(RunEvent::at_depth(
                    run_id,
                    step,
                    depth,
                    EventKind::FellBackTo { provider: served },
                ));
            }
            let step_tokens = response.usage.map(|u| u.total_tokens).unwrap_or(0);
            tokens_used += step_tokens;
            if step_tokens > 0 {
                tree.store
                    .record_context_reported(run_id, step, step_tokens)?;
            }

            let mut decisions: Vec<String> = Vec::new();
            let mut calls_json: Vec<String> = Vec::new();
            let mut step_changed = false;
            if response.tool_calls.is_empty() {
                let said = response.text.clone().unwrap_or_default();
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &format!("\n[step {step}] (no tool call) {said}\n"),
                        entry_cap,
                        ObsKind::Message,
                    ),
                ));
                decisions.push("no tool call".into());
            }
            // Non-spawn tools mutate the workspace and the observation log, so
            // they run in order. Spawn calls are independent sub-agents, so they
            // fan out concurrently, bounded by the tree's `max_concurrent`.
            let mut paused: Option<i64> = None;
            let mut paused_by_child = false;
            let mut spawn_calls: Vec<&ToolCall> = Vec::new();
            for call in &response.tool_calls {
                calls_json.push(format!("{}:{}", call.name, call.arguments));
                if call.name == SPAWN_TOOL {
                    spawn_calls.push(call);
                    continue;
                }
                match dispatch(
                    &ws,
                    call,
                    tree.approver,
                    tree.store,
                    run_id,
                    step,
                    tree.mcp,
                    tree.tools,
                    tree.skills,
                    entry_cap,
                    &mem_key,
                    tree.watch,
                    depth,
                )
                .await?
                {
                    Dispatched::Continue {
                        decision,
                        obs,
                        kind,
                        target,
                        changed,
                        ..
                    } => {
                        step_changed |= changed;
                        ledger.push(Observation::new(step, kind, target, obs));
                        decisions.push(decision);
                    }
                    Dispatched::Pause { request_id } => {
                        decisions.push(format!("awaiting approval (request {request_id})"));
                        paused = Some(request_id);
                        break;
                    }
                }
            }
            if paused.is_none() && !spawn_calls.is_empty() {
                use futures_util::stream::{self, StreamExt};
                let max_c = tree.containment.max_concurrent.max(1) as usize;
                // `buffered`, not `buffer_unordered`: up to `max_c` children still
                // run at once, but their results are collected in the order the
                // model asked for them rather than the order they happen to finish.
                //
                // Until 0.12.0 this was `buffer_unordered`, which made a tree run
                // non-reproducible: the composed child observations and the
                // `decisions` list — both of which become the `steps.result` and
                // `steps.decision` columns — came back in completion order, so the
                // same task over the same workspace produced a different trace and
                // a different next prompt depending on which child won a race.
                // Deterministic replay cannot be built on that.
                //
                // The cost is that a child which finishes early has its result held
                // until the children before it are done. That is bounded by `max_c`
                // and changes when a result is *read*, never when the work runs.
                let results: Vec<Result<SpawnResult>> = stream::iter(
                    spawn_calls
                        .into_iter()
                        .map(|c| spawn_child(tree, c, run_id, depth, policy, step)),
                )
                .buffered(max_c)
                .collect()
                .await;
                for r in results {
                    match r? {
                        SpawnResult::Composed { decision, obs } => {
                            // A child's composed result is an observation like any
                            // other, and is bounded like any other.
                            ledger.push(Observation::new(
                                step,
                                ObsKind::Child,
                                None,
                                bound(&obs, entry_cap, ObsKind::Child),
                            ));
                            decisions.push(decision);
                            // A child that ran did work the parent did not have to.
                            // Whether it changed the workspace is the child's own
                            // stall problem, tracked in the child's own loop.
                            step_changed = true;
                        }
                        // A child deferred; pause the tree with its request_id.
                        SpawnResult::Paused { request_id } => {
                            decisions
                                .push(format!("child awaiting approval (request {request_id})"));
                            paused = Some(request_id);
                            paused_by_child = true;
                        }
                    }
                }
            }

            // An agent paused because one of its CHILDREN deferred does NOT commit
            // this step: on resume it must replay it to re-adopt and resume that
            // paused child (only the parent re-entering `spawn_child` can wait on
            // the child again). An agent paused by its OWN gate commits normally —
            // it resumes from the step after, past the now-approved action.
            //
            // The condition is passed to the one boundary rather than branching
            // around it, so the uncommitted case is a stated argument at the single
            // commit point instead of a second, quieter commit path that a later
            // change could forget about. `commit_step` emits no `EventKind::Step`
            // for it either: there is no committed step to report, and a resume is
            // going to run this step again.
            let committed = !(paused.is_some() && paused_by_child);
            commit_step(
                tree.store,
                tree.watch,
                run_id,
                depth,
                StepRecord::new(step, decisions.join("; "), ledger.text_for_step(step)).with_trace(
                    user,
                    calls_json.join(" | "),
                    step_tokens,
                ),
                step_changed,
                committed,
            )?;
            // Only when the step actually committed. A step paused by a child is
            // deliberately left uncommitted so the resume replays it (0.7.0's
            // fix for double execution); persisting its observations would mean
            // the replay observed everything twice.
            if committed {
                written = persist_ledger(tree.store, run_id, &ledger, written)?;
            }

            // 0.5.0 spawns up to a hundred of these, so an agent burning its whole
            // step budget going nowhere is the multiplied version of the problem.
            let signature = calls_json.join(" | ");
            match progress.step(contract.stall, step_changed, &signature) {
                Progressing::Fine => {}
                Progressing::Replan => {
                    tree.store.record_context_event(
                        run_id,
                        &ContextEvent::replan(
                            step,
                            format!(
                                "{} steps without progress; replanning",
                                contract.stall.window
                            ),
                        ),
                    )?;
                    ledger.push(Observation::new(
                        step,
                        ObsKind::Message,
                        None,
                        bound(
                            &progress.replan_directive(contract.stall.window, &decisions),
                            entry_cap,
                            ObsKind::Message,
                        ),
                    ));
                    info!(run_id, depth, step, "agent told to change approach");
                    tree.watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::Replan {
                            window: contract.stall.window,
                        },
                    ));
                }
                Progressing::Stalled => {
                    tree.store.record_context_event(
                        run_id,
                        &ContextEvent::stalled(step, "still no progress after replanning"),
                    )?;
                    info!(run_id, depth, step, "agent stopped: stalled");
                    tree.watch
                        .emit(RunEvent::at_depth(run_id, step, depth, EventKind::Stalled));
                    finish(tree.store, tree.watch, run_id, depth, step, "stalled")?;
                    return Ok(RunOutcome::Stalled { steps: step });
                }
            }

            if let Some(request_id) = paused {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "awaiting_approval",
                )?;
                return Ok(RunOutcome::AwaitingApproval {
                    request_id,
                    steps: step,
                });
            }

            // Draw this step's tokens against the tree. The draw is recorded even
            // when it crosses the ceiling — the tokens were already spent — and a
            // crossing halts the whole tree, not just this agent.
            let draw = tree.ledger.draw_tokens(step_tokens);
            let remaining = tree.ledger.remaining_tokens();
            tree.store.record_agent_event(&AgentEvent::budget_draw(
                run_id,
                step,
                step_tokens,
                remaining,
            ))?;
            tree.watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::SpendDraw {
                    tokens: step_tokens,
                    // A tree always has a ceiling, so there is always a number here;
                    // the field is optional for a future draw against no ceiling.
                    remaining: Some(remaining),
                },
            ));
            if draw == Draw::Halted {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "budget_ceiling_reached",
                )?;
                return Ok(RunOutcome::BudgetCeilingReached { steps: step });
            }
            // The tree's wall-clock ceiling, measured from the ROOT's `started_at`
            // rather than this agent's: a child spawned twenty hours in has a young
            // stamp of its own, and the ceiling is about the whole tree. Checked
            // beside the token draw because both are the same kind of limit — one a
            // contract cannot raise, crossing which halts the tree rather than the
            // agent that noticed.
            //
            // `max_total_duration` has existed on `Containment` since 0.5.0 and was
            // never read, so a caller could set a ceiling on a 24-hour tree and have
            // it silently ignored. Enforced in 0.12.0. Its sibling `max_total_cost`
            // still cannot be: there is no price telemetry to compare against.
            if let Some(max) = tree.containment.max_total_duration {
                if tree.store.elapsed_secs(tree.root_run_id)? > max.as_secs_f64() {
                    finish(
                        tree.store,
                        tree.watch,
                        run_id,
                        depth,
                        step,
                        "budget_ceiling_reached",
                    )?;
                    info!(run_id, depth, step, "tree stopped: duration ceiling");
                    return Ok(RunOutcome::BudgetCeilingReached { steps: step });
                }
            }
            // This agent's own contract budget (never looser than the tree's).
            if tokens_used > token_cap {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "cost_budget_exceeded",
                )?;
                return Ok(RunOutcome::CostBudgetExceeded { steps: step });
            }

            if contract
                .verify
                .passes_in_guarded(
                    &tree.root,
                    &ExecGuard::new(policy)
                        .tracing(tree.store, run_id, step)
                        .watching(tree.watch, depth),
                )
                .await?
            {
                finish(tree.store, tree.watch, run_id, depth, step, "success")?;
                return Ok(RunOutcome::Success { steps: step });
            }
        }

        finish(
            tree.store,
            tree.watch,
            run_id,
            depth,
            contract.max_steps,
            "step_cap_reached",
        )?;
        Ok(RunOutcome::StepCapReached {
            steps: contract.max_steps,
        })
    })
}

/// The result of one [`SPAWN_TOOL`] call.
enum SpawnResult {
    /// The child finished; fold its composed result into the parent's log.
    Composed { decision: String, obs: String },
    /// The child deferred a sensitive action to a human. The pending action is
    /// persisted under `request_id`; the whole tree pauses so the caller can
    /// resume it with [`resume_with_decision`], exactly as a single run does.
    Paused { request_id: i64 },
}

/// Handle one [`SPAWN_TOOL`] call: enforce the containment caps, derive the
/// child's narrowed policy, run it, and compose its result back for the parent's
/// next turn. A refused spawn is a typed observation the parent can adapt to,
/// never a failure of the parent run; a child that defers propagates the pause
/// up so the caller can resume the child once a human decides.
async fn spawn_child<P: Provider>(
    tree: &Tree<'_, P>,
    call: &ToolCall,
    parent_run_id: i64,
    depth: u32,
    parent_policy: &Policy,
    step: u32,
) -> Result<SpawnResult> {
    let a = &call.arguments;
    let goal = a.get("goal").and_then(|v| v.as_str()).unwrap_or_default();
    let file = a
        .get("verify_file")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let needle = a
        .get("verify_contains")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    if goal.is_empty() || file.is_empty() {
        return Ok(SpawnResult::Composed {
            decision: "spawn missing fields".into(),
            obs: "\n[spawn error] spawn_agent needs \"goal\" and \"verify_file\"\n".into(),
        });
    }

    let child_depth = depth + 1;

    // A child inherits the parent policy and may only narrow it. Optional
    // `deny_write` globs let the parent tighten the child further.
    let mut overlay = Policy::permissive().layer("child");
    if let Some(denies) = a.get("deny_write").and_then(|v| v.as_array()) {
        for d in denies.iter().filter_map(|v| v.as_str()) {
            overlay = overlay.deny_write(d);
        }
    }
    if let Some(denies) = a.get("deny_net").and_then(|v| v.as_array()) {
        for d in denies.iter().filter_map(|v| v.as_str()) {
            overlay = overlay.deny_net(d);
        }
    }
    let child_policy = parent_policy.contain(&overlay);

    let verify = Verification::WorkspaceFileContains {
        file: file.into(),
        needle: needle.into(),
    };
    let mut child_contract = TaskContract::workspace(goal, &tree.root, verify);
    if let Some(n) = a.get("max_steps").and_then(|v| v.as_u64()) {
        child_contract = child_contract.with_max_steps(n as u32);
    }

    // Spawn-or-adopt. On a fresh run this spawn has no persisted record, so a new
    // child is created. On a tree resume the parent replays the same spawn step
    // and finds the child it already spawned (keyed by parent+step+goal): it
    // adopts that child and resumes it from its OWN last committed step instead
    // of creating a duplicate or restarting it. This is what lets every agent in
    // a crashed tree continue from its own checkpoint.
    let (child_run, child_start) = match tree.store.find_spawn(parent_run_id, step, goal)? {
        Some(row) => {
            // Adopted: already counted in the reconstructed ledger, so do NOT
            // register it again. A finished child is composed from its recorded
            // outcome without re-running; a mid-flight child resumes from its
            // next step.
            if let Some(o) = terminal_outcome(tree.store, row.child_run_id)? {
                return Ok(compose_child(row.child_run_id, goal, o));
            }
            (
                row.child_run_id,
                tree.store.last_step(row.child_run_id)? + 1,
            )
        }
        None => {
            // Fresh: the containment boundary decides whether it may exist, and
            // its contract is persisted so a later resume can adopt it.
            if let Err(refusal) = tree.ledger.register_agent(child_depth) {
                tree.store.record_agent_event(&AgentEvent::spawn_refused(
                    parent_run_id,
                    step,
                    refusal.cap(),
                ))?;
                // The parent's event, at the parent's depth: no child exists to
                // attribute it to, which is the point of the refusal.
                tree.watch.emit(RunEvent::at_depth(
                    parent_run_id,
                    step,
                    depth,
                    EventKind::SpawnRefused {
                        cap: refusal.cap().to_string(),
                    },
                ));
                return Ok(SpawnResult::Composed {
                    decision: format!("spawn refused ({})", refusal.cap()),
                    obs: format!(
                        "\n[spawn refused] {refusal} — adapt or finish with what you have\n"
                    ),
                });
            }
            let child_run = tree.store.start_child_run(
                goal,
                &tree.root.display().to_string(),
                parent_run_id,
                child_depth,
            )?;
            tree.store.record_agent_event(&AgentEvent::spawn(
                parent_run_id,
                step,
                child_run,
                goal,
            ))?;
            // Attributed to the PARENT's run and depth: the parent is what spawned
            // it, and the child's own events (which carry `child_depth`) start
            // arriving next.
            tree.watch.emit(RunEvent::at_depth(
                parent_run_id,
                step,
                depth,
                EventKind::Spawned {
                    child_run_id: child_run,
                    goal: goal.to_string(),
                },
            ));
            let deny_json = a
                .get("deny_write")
                .map(|v| v.to_string())
                .unwrap_or_else(|| "[]".into());
            tree.store.record_spawn(
                parent_run_id,
                step,
                child_run,
                goal,
                file,
                needle,
                a.get("max_steps")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
                &deny_json,
            )?;
            (child_run, 1)
        }
    };

    let outcome = run_agent(
        tree,
        &child_contract,
        child_run,
        child_depth,
        &child_policy,
        child_start,
    )
    .await?;

    // A child that deferred pauses the whole tree, surfacing its request_id so
    // the caller can resume that child once a human decides.
    if let RunOutcome::AwaitingApproval { request_id, .. } = outcome {
        return Ok(SpawnResult::Paused { request_id });
    }

    Ok(compose_child(child_run, goal, outcome))
}

/// Fold one child's finished result back into the parent's observation log.
fn compose_child(child_run: i64, goal: &str, outcome: RunOutcome) -> SpawnResult {
    SpawnResult::Composed {
        decision: format!("spawned child {child_run}: {outcome:?}"),
        obs: format!("\n[child {child_run} \"{goal}\" -> {outcome:?}]\n"),
    }
}

/// The rules an approver asked to remember, as a mergeable top layer.
///
/// A layer rather than an edit: merging is what keeps a remembered allow from
/// defeating a deny beneath it, since deny is absolute across the stack.
fn remembered_layer(rules: &[Rule]) -> Policy {
    let mut layer = Policy::permissive().layer("remembered");
    for r in rules {
        layer = layer.rule(r.act, r.effect, r.pattern.clone());
    }
    layer
}

/// The outcome of authorizing the provider's own endpoint, before a run makes
/// its first outbound call.
enum ProviderAccess {
    /// Cleared to run, under the policy the provider layer has been merged into.
    Granted(Policy),
    /// An approver deferred; the pending decision is persisted under this id and
    /// the run stops before it ever dials.
    Pending(i64),
}

/// Authorize the provider's endpoint once, before the first completion.
///
/// Once per run rather than once per step: a provider's endpoint is fixed for
/// the life of the provider, so re-asking each step would be the same question
/// with the same answer — and asking a human it repeatedly would train them to
/// wave it through.
///
/// The provider layer is merged *before* the check, not consulted after it. That
/// ordering is what makes a network-deny base usable: the `net` default denies,
/// the provider layer's allow rule beats a default, and a caller's explicit
/// `deny_net` still beats the allow because deny is absolute across layers. So
/// "deny everything but the model" needs no host list from the caller, while
/// "deny even the model" remains expressible — and fails fast as a refusal
/// rather than hanging on a call that is never made.
async fn authorize_provider<P: Provider>(
    provider: &P,
    policy: &Policy,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
    watch: &Watch<'_>,
) -> Result<ProviderAccess> {
    // A provider that opens no connection (the mock providers tests drive the
    // loop with) has no endpoint to authorize.
    // Every host in the chain, not just the first: a `Fallback` can dial its
    // secondary, and a host the policy never checked would be a hole in an egress
    // model that is deny-by-default everywhere else.
    let urls = provider.endpoints();
    if urls.is_empty() {
        // A provider that opens no connection (the mock providers the tests drive
        // the loop with) has no endpoint to authorize.
        return Ok(ProviderAccess::Granted(policy.clone()));
    }

    let mut effective = policy.clone();
    let mut ask: Option<String> = None;
    for url in urls {
        let Some(target) = net::target(url) else {
            return Err(crate::error::Error::Refused {
                act: "net".into(),
                target: url.to_string(),
                rule: None,
                layer: None,
            });
        };
        effective = effective.merge(net::provider_layer(&target));
        // Step 0: the authorization happens before the run's first step.
        let verdict = NetGuard::new(&effective)
            .tracing(store, run_id, 0)
            .watching(watch, 0)
            .check_target(&target)?;
        if verdict.effect == Effect::Ask {
            // One human decision covers the run; the first host that needs asking
            // is the one asked about.
            ask = Some(target.clone());
        }
    }

    let Some(target) = ask else {
        return Ok(ProviderAccess::Granted(effective));
    };

    // The run is now waiting on a human, before its first step. Step 0 for the
    // same reason the rows below are: the authorization precedes step 1.
    watch.emit(RunEvent::new(
        run_id,
        0,
        EventKind::ApprovalRequested {
            act: "net".into(),
            target: target.clone(),
        },
    ));
    match approver.decide(&Request::new(Act::Net, &target)).await {
        Decision::Approve { .. } => {
            let ev = PolicyEvent::decision(0, "net", &target, "approve", "approver");
            store.record_event(run_id, &ev)?;
            decided(watch, run_id, 0, &ev);
            Ok(ProviderAccess::Granted(effective))
        }
        Decision::Deny { reason } => {
            let ev = PolicyEvent::decision(0, "net", &target, "deny", "approver");
            store.record_event(run_id, &ev)?;
            decided(watch, run_id, 0, &ev);
            // Step 0: the run never started, so it finished having taken no steps.
            finish(store, watch, run_id, 0, 0, "refused")?;
            Err(crate::error::Error::Refused {
                act: "net".into(),
                target: format!("{target} — {reason}"),
                // The approver denied it, so the refusal is the human's, not a
                // rule's: there is no rule to name.
                rule: None,
                layer: None,
            })
        }
        Decision::Defer => {
            let ev = PolicyEvent::decision(0, "net", &target, "defer", "approver");
            store.record_event(run_id, &ev)?;
            decided(watch, run_id, 0, &ev);
            let request_id = store.put_pending(run_id, 0, "net", &target, None)?;
            finish(store, watch, run_id, 0, 0, "awaiting_approval")?;
            Ok(ProviderAccess::Pending(request_id))
        }
    }
}

/// The result of dispatching one tool call.
enum Dispatched {
    /// The call resolved; fold `obs` into the observation log and carry any
    /// rules an approver asked to remember.
    ///
    /// `kind` and `target` travel with `obs` because assembly reasons about them:
    /// what a later observation supersedes, and which read a write invalidates,
    /// is a question about the tool and its subject — not something to recover by
    /// re-parsing the text afterwards.
    Continue {
        decision: String,
        obs: String,
        kind: ObsKind,
        target: Option<String>,
        /// Whether this call moved the workspace. Only a write can, and only a
        /// write that wrote something different — the signal stall detection reads.
        changed: bool,
        remember: Vec<Rule>,
    },
    /// An approver deferred; the action is persisted under `request_id` and the
    /// run stops until a human decides.
    Pause { request_id: i64 },
}

impl Dispatched {
    /// A tool result: what it was, and the subject it names (if any).
    fn seen(
        decision: impl Into<String>,
        obs: impl Into<String>,
        kind: ObsKind,
        target: Option<String>,
    ) -> Self {
        Dispatched::Continue {
            decision: decision.into(),
            obs: obs.into(),
            kind,
            target,
            changed: false,
            remember: Vec::new(),
        }
    }

    /// A failure or a refusal. Kept subject-less on purpose: an error about a
    /// path is not an observation *of* that path, so it must never supersede one.
    fn go(decision: impl Into<String>, obs: impl Into<String>) -> Self {
        Self::seen(decision, obs, ObsKind::Error, None)
    }
}

/// Execute one tool call against the workspace, enforcing the policy and
/// consulting `approver` for anything it marks [`Effect::Ask`].
///
/// Tool-level failures (bad regex, path escape, a policy refusal) become
/// observations the agent can recover from rather than failing the run — only
/// the model can decide what to do about them.
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    ws: &Workspace,
    call: &ToolCall,
    approver: &dyn Approver,
    store: &Store,
    run_id: i64,
    step: u32,
    mcp: &McpSession,
    custom: &Toolbox,
    skills: &Skills,
    cap: usize,
    memory_key: &str,
    watch: &Watch<'_>,
    depth: u32,
) -> Result<Dispatched> {
    let a = &call.arguments;
    let s = |k: &str| a.get(k).and_then(|v| v.as_str());
    // Announced before the call is made, so a watcher sees what the run is about
    // to do rather than only what it did. The subject is whichever of the
    // conventional argument names this tool uses; a tool that names none of them
    // is its own subject, which is what an MCP or registered tool call is.
    watch.emit(RunEvent::at_depth(
        run_id,
        step,
        depth,
        EventKind::ToolCall {
            name: call.name.clone(),
            target: ["path", "pattern", "name_glob", "glob", "key", "name"]
                .into_iter()
                .find_map(s)
                .unwrap_or(&call.name)
                .to_string(),
        },
    ));
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
                    Dispatched::seen(
                        format!("grep {pattern:?} ({} hits)", hits.len()),
                        bound(
                            &format!("\n[grep {pattern:?}]\n{}\n", shown.join("\n")),
                            cap,
                            ObsKind::Grep,
                        ),
                        ObsKind::Grep,
                        Some(pattern.to_string()),
                    )
                }
                Err(e) => Dispatched::go("grep error", format!("\n[grep error] {e}\n")),
            }
        }
        FIND_TOOL => {
            let glob = s("name_glob").or_else(|| s("glob")).unwrap_or_default();
            match ws.find(glob) {
                Ok(paths) => Dispatched::seen(
                    format!("find {glob:?} ({} paths)", paths.len()),
                    bound(
                        &format!("\n[find {glob:?}]\n{}\n", paths.join("\n")),
                        cap,
                        ObsKind::Find,
                    ),
                    ObsKind::Find,
                    Some(glob.to_string()),
                ),
                Err(e) => Dispatched::go("find error", format!("\n[find error] {e}\n")),
            }
        }
        REMEMBER_TOOL => {
            let key = s("key").unwrap_or_default();
            let value = s("value").unwrap_or_default();
            if key.is_empty() || value.is_empty() {
                return Ok(Dispatched::go(
                    "remember error",
                    "\n[remember error] both key and value are required\n",
                ));
            }
            // The store bounds the entry and evicts oldest-first to hold the caps;
            // it writes no trace rows of its own, so the write and every eviction
            // are recorded here, where the run_id and step are known.
            let evicted = store.memory_put(memory_key, key, value, run_id, step)?;
            store.record_context_event(
                run_id,
                &ContextEvent::memory_write(
                    step,
                    format!("{key} ({} chars)", value.chars().count()),
                ),
            )?;
            // The key only: the row's detail carries a character count too, but
            // that is prose about the write, not the note's identity.
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::MemoryWrote {
                    key: key.to_string(),
                },
            ));
            for gone in &evicted {
                store.record_context_event(
                    run_id,
                    &ContextEvent::memory_evict(step, format!("{gone} (evicted to hold the cap)")),
                )?;
            }
            info!(run_id, step, key, evicted = evicted.len(), "remembered");
            // No target: two notes under one key are the store's business, and a
            // remember is not an observation OF anything that could go stale.
            Dispatched::seen(
                format!("remembered {key}"),
                format!("\n[remember {key}]\n"),
                ObsKind::Tool,
                None,
            )
        }
        READ_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                path,
                None,
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match ws.read_file(&target) {
                    Ok(c) => Dispatched::Continue {
                        decision: format!("read {target}"),
                        obs: format!("\n[read {target}]\n{}\n", bound(&c, cap, ObsKind::Read)),
                        kind: ObsKind::Read,
                        target: Some(target.clone()),
                        changed: false,
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
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(content),
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target,
                    content,
                    remember,
                } => {
                    let body = content.unwrap_or_default();
                    match ws.write_file(&target, &body) {
                        Ok(wrote) => Dispatched::Continue {
                            decision: format!("wrote {target}"),
                            // A write that changed nothing says so, to the model as
                            // well as to the trace: an agent rewriting a file with
                            // what it already held is the shape of a stall, and it
                            // cannot correct for what it is not told.
                            obs: bound(
                                &format!(
                                    "\n[wrote {target}] ({} chars{})\n",
                                    body.chars().count(),
                                    if wrote.moved_the_workspace() {
                                        ""
                                    } else {
                                        ", identical to what was already there — the \
                                         workspace did not change"
                                    }
                                ),
                                cap,
                                ObsKind::Write,
                            ),
                            kind: ObsKind::Write,
                            target: Some(target.clone()),
                            changed: wrote.moved_the_workspace(),
                            remember,
                        },
                        Err(e) => Dispatched::go("write error", format!("\n[write error] {e}\n")),
                    }
                }
            }
        }
        // Loading one skill's body. Offered only when skills are configured, so
        // the name is not special otherwise: a run without skills falls through
        // to the unknown-tool arm like any other name.
        //
        // The body is read through the policy at the moment it is asked for,
        // against the skill file's ABSOLUTE path — a skills directory usually
        // sits outside the workspace root, so this is a policy check, not a
        // workspace-relative one (see `gate`).
        READ_SKILL_TOOL if !skills.is_empty() => {
            let name = s("name").unwrap_or_default();
            let Some(skill) = skills.get(name) else {
                // Not an error and not a failed run: the model asked for
                // something that does not exist, so it is told what does.
                return Ok(Dispatched::go(
                    format!("unknown skill {name}"),
                    format!(
                        "\n[read_skill] there is no skill named {name:?}. Available: {}\n",
                        skills.names().join(", ")
                    ),
                ));
            };
            let path = skill.path.display().to_string();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Read,
                &path,
                None,
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match std::fs::read_to_string(&target) {
                    Ok(body) => {
                        // Capped where it enters the context, like every other
                        // tool result, under the same budget-derived cap.
                        let (body, truncated) = crate::tools::cap_result(body, cap);
                        info!(run_id, step, skill = name, truncated, "skill read");
                        Dispatched::Continue {
                            decision: format!("read skill {name}"),
                            obs: format!("\n[skill {name}]\n{body}\n"),
                            kind: ObsKind::Skill,
                            target: Some(name.to_string()),
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => Dispatched::go(
                        format!("skill {name} read error"),
                        format!("\n[skill {name} error] {e}\n"),
                    ),
                },
            }
        }
        // A tool the embedding program registered. Registration made it
        // available; this check is what authorizes the call, on exactly the terms
        // an MCP tool gets — an exec check on the name the model used. Deciding
        // it here rather than at registration is what lets one policy layer hand
        // over a toolbox and another refuse a single tool in it.
        //
        // Registered tools are matched before the MCP arm and after the
        // built-ins, and `Toolbox::validate` has already guaranteed the three
        // sets are disjoint, so the order is documentation rather than a
        // tie-break.
        name if custom.owns(name) => {
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Exec,
                name,
                None,
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go { remember, .. } => {
                    // `validate` ran at run start, so the lookup cannot miss.
                    let tool = custom.get(name).expect("owns() and get() agree");
                    match tool.invoke(&call.arguments).await {
                        Ok(out) => {
                            let (out, truncated) = crate::tools::cap_result(out, cap);
                            info!(run_id, step, tool = name, truncated, "registered tool call");
                            Dispatched::Continue {
                                decision: format!("called {name}"),
                                obs: format!("\n[{name}]\n{out}\n"),
                                kind: ObsKind::Tool,
                                target: Some(name.to_string()),
                                changed: false,
                                remember,
                            }
                        }
                        // A tool's own failure is the model's problem to route
                        // around, not the run's to die on — the same treatment a
                        // bad regex gets from grep.
                        Err(e) => {
                            info!(run_id, step, tool = name, error = %e, "registered tool failed");
                            Dispatched::Continue {
                                decision: format!("{name} failed"),
                                obs: format!("\n[{name} error] {e}\n"),
                                kind: ObsKind::Error,
                                target: None,
                                changed: false,
                                remember,
                            }
                        }
                    }
                }
            }
        }
        // An MCP tool. Invoking it is an exec check on its namespaced name, so a
        // policy can allow a server generally and still deny one of its tools.
        name if mcp.owns(name) => {
            let verdict = ws.policy().check(Act::Exec, name);
            if verdict.effect != Effect::Allow {
                let mut ev = PolicyEvent::refusal(step, "exec", name);
                ev.rule = verdict.rule.clone();
                ev.layer = verdict.layer.clone();
                store.record_event(run_id, &ev)?;
                refused(watch, run_id, depth, &ev);
                let why = verdict
                    .rule
                    .as_deref()
                    .map(|r| format!(" (rule {r})"))
                    .unwrap_or_default();
                return Ok(Dispatched::go(
                    format!("{name} refused"),
                    format!("\n[{name} refused]{why} — the policy forbids calling this tool\n"),
                ));
            }
            let out = mcp
                .call(
                    name,
                    &call.arguments,
                    store,
                    run_id,
                    step,
                    cap,
                    watch,
                    depth,
                )
                .await?;
            Dispatched::seen(
                format!("called {name}"),
                format!("\n[{name}]\n{out}\n"),
                ObsKind::Mcp,
                Some(name.to_string()),
            )
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
    watch: &Watch<'_>,
    depth: u32,
) -> Result<Gated> {
    let kind = format!("{act:?}").to_lowercase();
    // Read and write targets are workspace paths, and are resolved so a symlink
    // cannot smuggle one outside the root. Exec and net targets are *names* — a
    // binary, an MCP tool, a registered tool, a host — and must not be resolved
    // against the root, or a file that happens to share a tool's name would
    // change what the policy said about calling it.
    //
    // An ABSOLUTE read/write target is not a workspace path at all — a skill
    // file normally lives outside the root — so it is decided by the policy
    // directly. `check_path` would resolve it against the root and deny it
    // unconditionally, which would make `read_skill` refusable only by accident.
    // This relaxes what the *gate* says, not what the workspace does:
    // `Workspace::resolve` rejects absolute paths outright and both `read_file`
    // and `write_file` go through it, so an absolute path still cannot leave the
    // root (asserted in tests/skills.rs).
    let check = |act: Act, target: &str| match act {
        Act::Exec | Act::Net => ws.policy().check(act, target),
        Act::Read | Act::Write if Path::new(target).is_absolute() => ws.policy().check(act, target),
        Act::Read | Act::Write => ws.check_path(act, target),
    };
    let verdict = check(act, target);

    match verdict.effect {
        Effect::Deny => {
            let mut ev = PolicyEvent::refusal(step, &kind, target);
            if let (Some(rule), layer) = (verdict.rule.clone(), verdict.layer.clone()) {
                ev.rule = Some(rule);
                ev.layer = layer;
            }
            store.record_event(run_id, &ev)?;
            refused(watch, run_id, depth, &ev);
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
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::ApprovalRequested {
                    act: kind.clone(),
                    target: target.to_string(),
                },
            ));
            match approver.decide(&request).await {
                Decision::Approve { modified, remember } => {
                    let performed = modified.unwrap_or_else(|| request.clone());
                    // The rewritten action gets the same scrutiny as the original.
                    let recheck = check(act, &performed.target);
                    if recheck.effect == Effect::Deny {
                        let mut ev = PolicyEvent::refusal(step, &kind, &performed.target);
                        ev.rule = recheck.rule.clone();
                        ev.layer = recheck.layer.clone();
                        store.record_event(run_id, &ev)?;
                        // A refusal, not a decision: the row is a refusal too, and
                        // the approval it overrode never took effect.
                        refused(watch, run_id, depth, &ev);
                        return Ok(Gated::Refused {
                            decision: format!("{kind} refused after approval"),
                            obs: format!(
                                "\n[{kind} refused] {} — an approved change may not cross a deny\n",
                                performed.target
                            ),
                        });
                    }
                    let mut ev = PolicyEvent::decision(step, &kind, target, "approve", "approver");
                    if performed.target != target {
                        ev = ev.with_performed(&performed.target);
                    }
                    store.record_event(run_id, &ev)?;
                    decided(watch, run_id, depth, &ev);
                    Ok(Gated::Go {
                        target: performed.target,
                        content: performed.content,
                        remember,
                    })
                }
                Decision::Deny { reason } => {
                    let ev = PolicyEvent::decision(step, &kind, target, "deny", "approver");
                    store.record_event(run_id, &ev)?;
                    decided(watch, run_id, depth, &ev);
                    Ok(Gated::Refused {
                        decision: format!("{kind} denied"),
                        obs: format!("\n[{kind} denied] {target} — {reason}\n"),
                    })
                }
                Decision::Defer => {
                    let ev = PolicyEvent::decision(step, &kind, target, "defer", "approver");
                    store.record_event(run_id, &ev)?;
                    decided(watch, run_id, depth, &ev);
                    let request_id = store.put_pending(run_id, step, &kind, target, content)?;
                    Ok(Gated::Paused { request_id })
                }
            }
        }
    }
}

/// The key one workspace's durable memory is stored under.
///
/// Canonicalised, so the same directory reached by two different paths is one
/// workspace rather than two. The path as given is the fallback: a root that cannot
/// be canonicalised yet should still have memory rather than none.
fn memory_key(root: &Path) -> String {
    std::fs::canonicalize(root)
        .unwrap_or_else(|_| root.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// Call the provider, retrying a failing call up to `max_retries` times. Each
/// failed attempt is recorded in the trace. After the limit the error is
/// escalated (recorded, the run marked `escalated`, and returned).
#[allow(clippy::too_many_arguments)]
async fn complete_with_retry<P: Provider>(
    provider: &P,
    request: &CompletionRequest,
    contract: &TaskContract,
    store: &Store,
    run_id: i64,
    step: u32,
    watch: &Watch<'_>,
    depth: u32,
) -> Result<CompletionResponse> {
    let max_retries = contract.max_retries;
    let retry = contract.retry;
    let max_duration = contract.max_duration;
    let mut attempt = 0;
    loop {
        match provider.complete(request.clone()).await {
            Ok(response) => return Ok(response),
            // Only ask again if asking again could answer differently. Before
            // 0.11.0 every error was retried identically — including a 401 and a
            // missing API key, which cost three calls each to learn nothing, while
            // the one failure worth waiting for got no wait at all.
            Err(e) if attempt < max_retries && retryable(&e) => {
                attempt += 1;
                let wait = retry.wait(attempt, retry_after(&e));
                // A wait is not a way to escape the time budget: if the run's
                // deadline falls inside this sleep, stop now rather than after it.
                if let Some(max) = max_duration {
                    let elapsed = store.elapsed_secs(run_id)?;
                    if elapsed + wait.as_secs_f64() > max.as_secs_f64() {
                        store.record(
                            run_id,
                            &StepRecord::new(
                                step,
                                // Same "escalated after <kind>" prefix as every
                                // other escalation, so a trace reader grepping for
                                // escalations does not miss this class of them.
                                format!(
                                    "escalated after {} (a retry would outlast the time budget)",
                                    kind_of(&e)
                                ),
                                e.to_string(),
                            ),
                        )?;
                        // The step that failed never completed, so the count comes from the
                        // trace itself — which is also what `terminal_outcome` reports
                        // when this run is later resumed, so the two cannot disagree.
                        let steps = store.last_step(run_id)?;
                        finish(store, watch, run_id, depth, steps, escalation_outcome(&e))?;
                        return Err(e);
                    }
                }
                store.record(
                    run_id,
                    &StepRecord::new(
                        step,
                        format!("retry {attempt} after {} in {:?}", kind_of(&e), wait),
                        e.to_string(),
                    ),
                )?;
                // The same `kind_of` string the row above records, so the event and
                // the trace name the failure identically rather than nearly so.
                watch.emit(RunEvent::at_depth(
                    run_id,
                    step,
                    depth,
                    EventKind::Retry {
                        kind: kind_of(&e),
                        attempt,
                        delay_ms: wait.as_millis() as u64,
                    },
                ));
                if !wait.is_zero() {
                    tokio::time::sleep(wait).await;
                }
            }
            Err(e) => {
                store.record(
                    run_id,
                    &StepRecord::new(
                        step,
                        format!("escalated after {}", kind_of(&e)),
                        e.to_string(),
                    ),
                )?;
                // The step that failed never completed, so the count comes from the
                // trace itself — which is also what `terminal_outcome` reports
                // when this run is later resumed, so the two cannot disagree.
                let steps = store.last_step(run_id)?;
                finish(store, watch, run_id, depth, steps, escalation_outcome(&e))?;
                return Err(e);
            }
        }
    }
}

/// Whether this failure is worth another attempt. A non-provider error — a bad
/// configuration, an IO failure — is not: it will fail the same way next time.
fn retryable(e: &Error) -> bool {
    matches!(e, Error::Provider { kind, .. } if kind.is_retryable())
}

/// What the server asked us to wait, if it asked.
fn retry_after(e: &Error) -> Option<std::time::Duration> {
    match e {
        Error::Provider { retry_after, .. } => *retry_after,
        _ => None,
    }
}

/// A short name for the trace row, so a reader can tell a wait from a hammer.
fn kind_of(e: &Error) -> String {
    match e {
        Error::Provider { kind, status, .. } => match status {
            Some(s) => format!("{kind:?} (HTTP {s})"),
            None => format!("{kind:?}"),
        },
        other => format!("{other}"),
    }
}

/// The outcome string an escalation records, carrying whether the failure was one
/// another attempt could have survived.
///
/// Two strings rather than one because a resumed run and a trace reader have to
/// reach the same conclusion the caller did, and the caller's `Error` does not
/// survive into the store.
fn escalation_outcome(e: &Error) -> &'static str {
    if retryable(e) {
        "escalated_retryable"
    } else {
        "escalated_terminal"
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

/// Tell the model about tools the built-in prompt does not enumerate.
///
/// Without this the system prompt describes a world of exactly four tools while
/// the request carries more, and a model that trusts the prose over the schema
/// either ignores an MCP tool or, worse, calls one repeatedly without noticing
/// it already answered. Naming them — and saying plainly that a result lands in
/// the observations — is what turns a discovered tool into a usable one.
fn with_extra_tools(base: String, extra: &[ToolSpec]) -> String {
    if extra.is_empty() {
        return base;
    }
    let names: Vec<&str> = extra.iter().map(|t| t.name.as_str()).collect();
    format!(
        "{base} These extra tools are also available and work the same way: {}. \
         Each tool's result appears in the observations below; once a tool has \
         returned what you asked for, move on rather than calling it again.",
        names.join(", ")
    )
}

/// [`READ_SKILL_TOOL`], offered only when the contract configures skills — a
/// tool that could do nothing but fail would cost a slot in every request of
/// every other run. Same rule MCP tools get: they appear when servers do.
fn skill_tool(skills: &Skills) -> Option<ToolSpec> {
    if skills.is_empty() {
        return None;
    }
    Some(ToolSpec {
        name: READ_SKILL_TOOL.to_string(),
        description: "Load one skill's full instructions into your observations, by the name it \
                      is listed under. Read a skill when its description says it covers what you \
                      are about to do."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "The skill's name, as listed in the system prompt." }
            },
            "required": ["name"]
        }),
    })
}

/// Name the available skills in the system prompt: one line each, name and
/// description. A body is never here — that is what [`READ_SKILL_TOOL`] loads,
/// once, on demand, so a caller with twenty skills does not pay for twenty
/// bodies on every turn.
fn with_skill_catalog(base: String, skills: &Skills) -> String {
    if skills.is_empty() {
        return base;
    }
    format!(
        "{base}\n\nSkills available to you — instructions written for this repository. Only each \
         skill's name and description is shown; call `{READ_SKILL_TOOL}` with a name to read that \
         skill's full text when its description matches what you are doing.\n{}",
        skills.catalog()
    )
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

fn tree_system_prompt() -> String {
    "You are an agent working across a repository to meet a stated specification. \
     Use `grep`, `find`, `read_file`, and `write_file` as in a normal run. You may \
     also decompose the work: call `spawn_agent` to launch a sub-agent that pursues \
     a smaller goal over the same workspace, and its result is reported back to you. \
     A sub-agent inherits your permissions and can only be more restricted, never \
     less. Prefer spawning when parts of the task are independent. Work in small \
     steps; the whole set is checked against the success criterion after each. Do \
     not explain; call tools."
        .to_string()
}

/// Workspace tools plus [`SPAWN_TOOL`] — offered only inside an agent tree.
fn tree_tools() -> Vec<ToolSpec> {
    let mut tools = workspace_tools();
    tools.push(ToolSpec {
        name: SPAWN_TOOL.to_string(),
        description: "Spawn a contained sub-agent to pursue a smaller goal over the same \
                      workspace. The sub-agent inherits your permissions (it can only be \
                      further restricted) and its outcome is reported back to you."
            .to_string(),
        parameters: json!({
            "type": "object",
            "properties": {
                "goal": { "type": "string", "description": "The sub-agent's goal." },
                "verify_file": { "type": "string", "description": "File (relative to the workspace root) whose contents decide the sub-agent's success." },
                "verify_contains": { "type": "string", "description": "Text that file must contain for the sub-agent to succeed." },
                "deny_write": { "type": "array", "items": { "type": "string" }, "description": "Optional globs the sub-agent must not write — tightens its inherited policy." },
                "deny_net": { "type": "array", "items": { "type": "string" }, "description": "Optional host globs (host or host:port) the sub-agent must not reach — tightens its inherited policy." },
                "max_steps": { "type": "integer", "description": "Optional step budget for the sub-agent." }
            },
            "required": ["goal", "verify_file", "verify_contains"]
        }),
    });
    tools
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
            name: REMEMBER_TOOL.to_string(),
            description: "Record a short fact or decision worth keeping for a later run over this \
                          workspace — a build command, a layout you had to discover, a decision and \
                          why. Notes are yours, not instructions, and are recalled at the start of \
                          later runs so you do not rediscover the same thing twice."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "key": { "type": "string", "description": "Short name to recall it by; writing the same key again replaces it." },
                    "value": { "type": "string", "description": "The fact, in one or two sentences." }
                },
                "required": ["key", "value"]
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
