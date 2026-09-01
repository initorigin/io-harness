//! The orchestration loop: observe, reason, act, verify, stop — bounded by
//! budgets, resilient to transient step failures, and resumable.
//!
//! v0.2 adds three budgets (step, time, cost-in-tokens) each with its own stop
//! outcome, per-step retry with escalation, a full trace written to the store,
//! and [`resume`], which continues an interrupted run under its original id
//! instead of restarting.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::{select, Either};
use futures_util::stream::{FuturesUnordered, StreamExt};
use futures_util::FutureExt;
use serde_json::json;
use tracing::info;

use crate::agent::{AgentDef, Agents};
use crate::approve::{ApprovalContext, ApproveAll, Approver, Decision, Request};
use crate::approve::{Choice, Question, Responder, ResponderNone};
use crate::approve::{Plan, PlanGate, PlanStep, PlanVerdict};
use crate::containment::{Containment, Draw, Ledger};
use crate::context::{
    assemble, bound, entry_cap_chars, last_lines, Assembled, Assembly, Ledger as ContextLedger,
    ObsKind, Observation, Piece, GATE_FEEDBACK_CHARS, GATE_FEEDBACK_LINES,
};
use crate::contract::{Preset, SystemPrompt, TaskContract};
use crate::error::{Error, Result};
use crate::lsp::LspSession;
#[cfg(feature = "browser")]
use crate::tools::browser::BrowserSession;

/// The browser session a run carries when the feature is compiled out.
///
/// A shim rather than a `#[cfg]` at each of the ten sites that thread the real
/// one: the threading is identical either way, and the difference that actually
/// exists — whether any browser behaviour is reachable — lives in the two places
/// that read `configured()`.
#[cfg(not(feature = "browser"))]
pub(crate) struct BrowserSession;

#[cfg(not(feature = "browser"))]
impl BrowserSession {
    /// Shut down nothing. The one method the shim genuinely needs: the run loop
    /// calls it unconditionally, beside every language-server shutdown.
    ///
    /// There is deliberately no `configured()` here. The two sites that ask it
    /// are themselves behind `#[cfg(feature = "browser")]`, so a shim method
    /// would be dead code in every build that compiles this file — and a
    /// `dead_code` warning in the default build is exactly the kind of wart that
    /// a release should not ship.
    async fn shutdown(&self) {}
}
use crate::mcp::McpSession;
use crate::net::{self, NetGuard};
use crate::observe::{EventKind, Ignore, Observer, RunEvent};
use crate::policy::{Act, Effect, Policy, Rule};
use crate::provider::{
    CompletionRequest, CompletionResponse, Message, PromptFamily, Provider, ToolCall, ToolResult,
    ToolSpec,
};
use crate::resilience::{Progress, Progressing};
use crate::sandbox::{Sandbox, SandboxConfig};
use crate::skills::Skills;
use crate::state::PolicyEvent;
use crate::state::{
    AgentEvent, AssistantTurn, ContextEvent, GateOutcome, Kept, MemoryEntry, MemoryForget,
    MemoryKind, MemoryLimits, RunStatus, Snapshot, StepRecord, Store, TodoItem, TodoState,
    GLOBAL_MEMORY_WORKSPACE, MAX_SNAPSHOT_BYTES, ROOT_ADDRESS,
};
use crate::toolchain::Toolchain;
use crate::tools::exec::{Exec, ExecOutcome};
use crate::tools::git::{Git, GitCmd, GitOutcome};
use crate::tools::shell::{Shell, ShellOutcome};
use crate::tools::workspace::Wrote;
#[cfg(feature = "barcode")]
use crate::tools::BARCODE_DECODE_TOOL;
#[cfg(feature = "pptx")]
use crate::tools::PPTX_READ_TOOL;

/// The path a git built-in names when it asks the policy about the repository
/// itself. Reading history reads it; committing writes it. A run under a narrow
/// write policy must allow it explicitly, which is stated where the tools are
/// documented rather than left to be discovered by a refusal.
const GIT_DIR: &str = ".git";
#[cfg(feature = "media")]
use crate::tools::VIEW_IMAGE_TOOL;
use crate::tools::{
    Entry, FsTool, ToolEffect, Toolbox, Workspace, ASK_QUESTIONS_TOOL, ASK_QUESTION_TOOL,
    CHECK_TOOL, EDIT_FILE_TOOL, EXEC_TOOL, FIND_TOOL, FORGET_TOOL, GREP_TOOL, LIST_DIR_TOOL,
    LSP_DEFINITION_TOOL, LSP_HOVER_TOOL, LSP_REFERENCES_TOOL, LSP_RENAME_TOOL, LSP_SYMBOLS_TOOL,
    PATCH_FILE_TOOL, PREVIEW_MAX_BYTES, PREVIEW_MAX_LINES, PROPOSE_PLAN_TOOL, QUESTIONS_MAX,
    READ_FILE_TOOL, READ_SKILL_TOOL, REMEMBER_TOOL, SHELL_KILL_TOOL, SHELL_POLL_TOOL,
    SHELL_START_TOOL, SHELL_TOOL, TODO_WRITE_TOOL, WRITE_FILE_TOOL,
};
#[cfg(feature = "docx")]
use crate::tools::{DOCX_READ_TOOL, DOCX_WRITE_TOOL};
use crate::tools::{
    GIT_ADD_TOOL, GIT_BRANCH_TOOL, GIT_COMMIT_TOOL, GIT_DIFF_TOOL, GIT_LOG_TOOL, GIT_STATUS_TOOL,
    GIT_WORKTREE_TOOL,
};
#[cfg(feature = "pdf")]
use crate::tools::{PDF_FILL_FORM_TOOL, PDF_READ_TOOL, PDF_WATERMARK_TOOL, PDF_WRITE_TOOL};
#[cfg(feature = "xlsx")]
use crate::tools::{XLSX_READ_TOOL, XLSX_SET_CELL_TOOL, XLSX_SHEETS_TOOL, XLSX_WRITE_TOOL};
use crate::verify::{ExecGuard, Verification};

/// The tool a parent agent calls to spawn a contained sub-agent.
///
/// It is the name the model sees, and the name that appears in the trace and in
/// [`EventKind::ToolCall`](crate::EventKind::ToolCall) when an agent fans out —
/// which is the reason to know it. A consumer watching a tree matches on it to
/// tell composition apart from ordinary work:
///
/// ```
/// use io_harness::{EventKind, Flow, Observer, RunEvent, SPAWN_TOOL};
///
/// struct TreeShape;
///
/// impl Observer for TreeShape {
///     fn event(&self, event: &RunEvent) -> Flow {
///         match &event.kind {
///             // A parent asking for a child, before the child exists.
///             EventKind::ToolCall { name, target } if name == SPAWN_TOOL => {
///                 println!("{:indent$}spawning: {target}", "", indent = event.depth as usize * 2);
///             }
///             // The child that resulted, with its own run id to route on.
///             EventKind::Spawned { child_run_id, goal } => {
///                 println!("{:indent$}run {child_run_id}: {goal}", "",
///                          indent = (event.depth as usize + 1) * 2);
///             }
///             _ => {}
///         }
///         Flow::Continue
///     }
/// }
/// ```
///
/// Only [`run_tree`] offers it. [`run`] and [`run_with`] never put it in the
/// tool list, so a contract cannot opt into sub-agents by accident.
///
/// It is deliberately *not* governed by the exec policy the way a registered
/// tool's name is — a spawn is intercepted by the tree loop before dispatch, and
/// its ceilings are [`Containment`]'s: total agents, concurrency, depth, and the
/// shared token ledger. To forbid composition, use [`run_with`]; to bound it,
/// lower those caps.
pub const SPAWN_TOOL: &str = "spawn_agent";

/// What the loop writes into an observation when the model called no tool.
///
/// Shared with [`crate::session`], which reads a turn's closing message back out
/// of the observations: two literals would drift the first time one of them was
/// reworded, and the symptom would be a session that silently stopped reporting
/// replies.
pub(crate) const NO_TOOL_CALL: &str = "(no tool call)";

/// How many grep hits are folded into one observation. A relevance ceiling, not a
/// size one — the size ceiling is the budget-derived per-entry cap on top of it.
const OBS_GREP_CAP: usize = 50;

/// How many directory entries are folded into one observation (0.24.0).
///
/// Higher than the grep ceiling because the units differ: a grep hit is a line of
/// source and an entry is a name, so a listing costs roughly an order of
/// magnitude less per item. It exists because a directory is not obliged to be
/// small — `node_modules` and a build output tree run to thousands of entries,
/// and a tool that spent a whole turn's budget on one of them would be unusable
/// exactly where looking before reading matters most.
///
/// What is dropped is *stated* in the observation, with the true total, because a
/// model that cannot tell a partial listing from a complete one will conclude the
/// files it needs do not exist.
const OBS_LIST_DIR_CAP: usize = 200;

/// Why a run stopped.
///
/// A run stopping is not a run failing — only one of these variants is success,
/// and lumping the rest together as "it didn't work" loses the difference
/// between a run that needs more budget, one that needs a human, and one that
/// needs a different task. The match is what a caller writes around every entry
/// point:
///
/// ```
/// use io_harness::RunOutcome;
///
/// # fn next_step(outcome: RunOutcome) -> &'static str {
/// match outcome {
///     RunOutcome::Success { .. } => "verification passed; ship it",
///
///     // Paused, not finished. The pending action is persisted under
///     // `request_id` and this process may exit; whoever decides later calls
///     // `resume_with_decision` with that id. Never retry the run from scratch.
///     RunOutcome::AwaitingApproval { request_id: _, .. } => "ask a human, then resume",
///
///     // (0.21.0) Also paused, and a different question: the agent asked what you
///     // MEANT, not whether it may act. `resume_with_answer` continues it with the
///     // answer, which reaches the model as text and authorizes nothing.
///     RunOutcome::AwaitingAnswer { question_id: _, .. } => "answer it, then resume",
///
///     // (0.31.0) Paused before it did anything at all: the agent proposed an
///     // approach and is waiting. Show the plan, then `resume_with_plan_decision`.
///     // Nothing in the workspace has been written at this point.
///     RunOutcome::AwaitingPlan { plan_id: _, .. } => "review the plan, then resume",
///     // A human refused the approach. As final as a `Denied`, and cheaper: the
///     // run stopped having read and thought and written nothing.
///     RunOutcome::PlanRejected { .. } => "the approach was refused; rewrite the goal",
///
///     // Ceilings, and they mean different things. More steps or more time is a
///     // knob; a token ceiling that keeps being hit is usually a task too big
///     // for one contract.
///     RunOutcome::StepCapReached { .. } | RunOutcome::TimeBudgetExceeded { .. } => "raise the bound and resume",
///     RunOutcome::CostBudgetExceeded { .. } | RunOutcome::BudgetCeilingReached { .. } => "split the task",
///
///     // (0.70.0) Also the step cap, but the criterion had judged the work and
///     // refused it every time. Raising the bound buys more of the same answer:
///     // read `gate_output` in `sandbox_events` first.
///     RunOutcome::VerificationFailed { .. } => "read why the gate failed before resuming",
///
///     // The agent is going in circles and was already told once. Resuming
///     // spends the rest of the budget proving it again — change the goal.
///     RunOutcome::Stalled { .. } => "rewrite the contract",
///
///     // Both are reported *after the fact*: the run that escalated or was
///     // refused returned the `Err` itself, and a later `resume` reports this
///     // instead of re-driving the loop.
///     RunOutcome::Escalated { retryable: true, .. } => "transient provider failure; resume",
///     RunOutcome::Escalated { .. } => "wrong key or bad request; fix it first",
///     RunOutcome::Refused { .. } => "the provider's host was denied; widen the net policy",
///
///     RunOutcome::Denied { .. } => "a human said no; the action never happened",
///     // An observer returned `Flow::Cancel`. Finished cleanly, and still resumable.
///     RunOutcome::Cancelled { .. } => "resume when you want it to continue",
///
///     // Only a `Verification::None` run reaches this: it stopped because the
///     // agent stopped, not because a ceiling did. Nothing checked the work —
///     // read it, rather than shipping it the way a `Success` may be shipped.
///     RunOutcome::Finished { .. } => "the agent is done; nothing verified it",
///
///     // (0.65.0) The run died mid-call to something this crate cannot inspect,
///     // and whether that call landed is not knowable from here. Nothing has been
///     // repeated: `resume_with_recovery` carries an operator's decision.
///     RunOutcome::AwaitingRecovery { attempt_id: _, .. } => "decide about the call, then resume",
///
///     // `RunOutcome` is `#[non_exhaustive]` from 0.65.0, so a later variant is a
///     // line here rather than a compile break. This arm is what pays for that.
///     _ => "a later release added an outcome this program does not know",
/// }
/// # }
/// ```
///
/// Every variant carries `steps`, which is how many steps *completed* — so a
/// `StepCapReached { steps: 12 }` and a `Success { steps: 12 }` cost the same
/// and only one of them produced anything. For what the run actually spent, use
/// [`RunResult::summary`].
/// `#[non_exhaustive]` from 0.65.0, which is a break taken deliberately and
/// once. Adding `AwaitingRecovery` already broke every exhaustive `match` on this
/// enum, so the attribute that stops the next addition breaking anybody is paid
/// for by the same edit rather than by a second one later. The fix in a caller is
/// one wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RunOutcome {
    /// Verification passed. `steps` is the step it passed on.
    Success { steps: u32 },
    /// The step budget was reached and no criterion ever judged the work — the
    /// contract carries none, or no step of this run got as far as evaluating
    /// one. There is no verdict either way.
    ///
    /// Narrowed in 0.70.0: a run that *was* judged and failed now reports
    /// [`VerificationFailed`](RunOutcome::VerificationFailed) instead. Before
    /// that release this variant carried both, and a caller reading it could not
    /// tell "unfinished" from "wrong".
    StepCapReached { steps: u32 },
    /// (0.70.0) The step budget was reached *and* the criterion had been
    /// evaluated and did not hold. The run got as far as being judged, every
    /// time, and every time the answer was no.
    ///
    /// The distinction is from [`StepCapReached`](RunOutcome::StepCapReached),
    /// which before this release absorbed both cases. That one means the budget
    /// ran out with the work possibly fine and simply unfinished, so raising
    /// `max_steps` is the reasonable next move. This one means the work was
    /// checked and rejected, so raising `max_steps` buys more of the same
    /// rejection: what the run needs is a look at *why* the criterion failed —
    /// which is in `sandbox_events` under `gate_output` — before it is given
    /// another budget. A fleet operator re-driving on the step count alone
    /// cannot tell those two apart, and pays for the second as if it were the
    /// first.
    ///
    /// Not a claim the criterion can never pass, which is why this is not a
    /// terminal outcome: a `Verification::Command` gate that failed because the
    /// test runner is not installed is a machine to fix, and the run resumes
    /// unchanged once it is. See the note in `terminal_outcome`.
    ///
    /// [`Verification::None`](crate::Verification::None) can never reach this.
    /// That criterion answers `false` for every entry point — it is the absence
    /// of a gate, not a gate that says no — and a run under it that spends its
    /// whole budget still reports `StepCapReached`.
    VerificationFailed { steps: u32 },
    /// (0.65.0) The run died in the middle of a call the harness cannot inspect —
    /// a charge, a deployment, a posted message, an MCP call, any registered
    /// [`Tool`](crate::tools::Tool) whose
    /// [`recovery`](crate::tools::Tool::recovery) is
    /// [`ToolRecovery::Indeterminate`](crate::ToolRecovery::Indeterminate) — and
    /// whether that call landed cannot be established from here. The run is
    /// paused, not finished: the attempt is persisted under `attempt_id` and
    /// survives this process, so [`resume_with_recovery`] continues it once a
    /// human decides. `steps` is how many steps completed.
    ///
    /// Distinct from every other pause in this enum because nothing is being
    /// asked *of* the agent: the question is not whether an action is permitted
    /// or what was wanted, it is whether an action that may already have happened
    /// should happen again. Only the operator can answer it, and until they do
    /// the safe move is to do nothing — which is what this outcome is.
    AwaitingRecovery { attempt_id: i64, steps: u32 },
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
    /// (0.21.0) The agent asked the operator about *intent* and nothing in this
    /// process would answer. The run is paused, not finished: the question is
    /// persisted under `question_id` and survives this process, so
    /// [`resume_with_answer`] continues it once a human answers. `steps` is how many
    /// steps completed.
    ///
    /// Distinct from [`AwaitingApproval`](RunOutcome::AwaitingApproval) because the
    /// two questions differ: that one asks whether an action is permitted, this one
    /// asks what was wanted. An answer to this one authorizes nothing.
    AwaitingAnswer { question_id: i64, steps: u32 },
    /// (0.31.0) The agent proposed a plan and no [`PlanGate`](crate::PlanGate) in
    /// this process would decide on it. The run is paused, not finished: the plan
    /// is persisted under `plan_id` and survives this process, so
    /// [`resume_with_plan_decision`] continues it once a human answers. `steps` is
    /// how many steps completed.
    ///
    /// Nothing in the workspace has been written at this point, and that is the
    /// difference from every other pause in this enum: the planning phase denies
    /// every [`Act::Write`](crate::Act::Write) and [`Act::Exec`](crate::Act::Exec),
    /// so a run that stops here stops having read and thought and done nothing else.
    AwaitingPlan { plan_id: i64, steps: u32 },
    /// (0.31.0) A [`PlanGate`](crate::PlanGate) cancelled the plan, so the run
    /// stopped without doing the work. `steps` is how many steps completed —
    /// reading and proposing, never writing.
    ///
    /// Distinct from [`Cancelled`](RunOutcome::Cancelled), which is an
    /// [`Observer`](crate::Observer) stopping a run already under way, and from
    /// [`Denied`](RunOutcome::Denied), which is a human refusing one *action* a run
    /// had already decided to take. This one is a human refusing the approach.
    PlanRejected { steps: u32 },
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
    /// The agent finished. Only a [`Verification::None`] run reaches this: with
    /// no criterion to pass, an assistant turn that calls no tool is the run
    /// saying it is done, and the loop stops there.
    ///
    /// Distinct from every ceiling on purpose. An unattended run that completed
    /// its work and one that ran out of steps both stop, and treating them alike
    /// is how a fleet operator ends up re-driving finished work — or worse,
    /// shipping the output of a run that never got there. `steps` is the step it
    /// finished on.
    ///
    /// It is **not** a claim the work is correct. Nothing checked it; that is
    /// what choosing [`Verification::None`] means. A run with a criterion reports
    /// [`RunOutcome::Success`], and that one *is* a claim — bounded by what the
    /// criterion checked and no wider.
    ///
    /// Added in 0.17.0 with [`Verification::None`].
    ///
    /// [`Verification::None`]: crate::Verification::None
    Finished { steps: u32 },
}

/// The result of a run, including the persisted run id for audit.
///
/// Three things, and the `run_id` is the one that outlives the process. Every
/// step, refusal, spawn and budget draw is in the store under it, so a run is
/// still readable long after the program that drove it exited — and it is the
/// handle every `resume*` entry point takes:
///
/// ```no_run
/// use io_harness::{run_with, ApproveAll, OpenRouter, Policy, RunOutcome, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract, policy: &Policy) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let result = run_with(contract, &OpenRouter::from_env()?, &store, policy, &ApproveAll).await?;
///
/// // What it cost and how long it took, read back from the same row an auditor
/// // would read — so the caller and the audit cannot disagree. `None` while a
/// // run is paused awaiting a human: there is no ending to summarise yet.
/// if let Some(summary) = result.summary(&store)? {
///     println!("{} tokens", summary.tokens);
/// }
///
/// // Rules an approver asked to remember. The crate applied them for the rest of
/// // this run and hands them back here; persisting them across runs is the
/// // caller's decision, because config files are the application's to own.
/// for rule in &result.remembered {
///     println!("remember: {:?} {} {:?}", rule.act, rule.pattern, rule.effect);
/// }
///
/// // Keep the id if the run is not finished — it is all a later process needs.
/// if !matches!(result.outcome, RunOutcome::Success { .. }) {
///     println!("resume run {}", result.run_id);
/// }
/// # Ok(()) }
/// ```
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

/// What [`rewind`] did about one file (0.28.0).
///
/// Four variants and not two, for the same reason
/// `crate::tools::diagnostics::Outcome` has four: the caller must be able to tell
/// **"nothing was changed because there was nothing to change" from "nothing was
/// changed because the previous contents were not kept"**. Collapsing
/// [`Rewind::NotKept`] into [`Rewind::NotRecorded`] would tell a caller a file was
/// untouched when the run had in fact rewritten it and the harness simply cannot
/// undo that — which is the one answer a human deciding whether to restore from
/// their own backup needs. That distinction is the whole reason for four
/// variants.
///
/// The same argument splits [`Rewind::Restored`] from [`Rewind::Removed`]: "the
/// way it was" is not existing for a file the run created, and a caller writing a
/// report cannot say "put back" about a file it deleted.
///
/// ```
/// use io_harness::Rewind;
///
/// /// The one line a report writes about each file, which is only writable
/// /// because the four cases stayed apart.
/// fn line(path: &str, r: &Rewind) -> String {
///     match r {
///         Rewind::Restored(_) => format!("{path}: put back"),
///         Rewind::Removed => format!("{path}: removed, the run created it"),
///         Rewind::NotKept(why) => format!("{path}: LEFT AS THE RUN LEFT IT — {why}"),
///         Rewind::NotRecorded => format!("{path}: untouched by this run"),
///     }
/// }
///
/// assert_eq!(line("a.rs", &Rewind::Removed), "a.rs: removed, the run created it");
/// // The two that both changed nothing still read differently, which is the point.
/// assert!(line("big.bin", &Rewind::NotKept("not valid UTF-8".into())).contains("LEFT AS"));
/// assert!(line("other.rs", &Rewind::NotRecorded).contains("untouched"));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rewind {
    /// Put back. Carries the [`Wrote`] the workspace returned, so a caller can
    /// see whether the file was already in that state — a rewind of a file the
    /// run rewrote with what it already held reports
    /// [`Wrote::Unchanged`](crate::tools::workspace::Wrote::Unchanged), and a
    /// caller counting how much it actually undid needs to know that.
    Restored(Wrote),
    /// The run created it, so "the way it was" is not existing. Deleted.
    Removed,
    /// The run changed it and the previous contents were deliberately not kept —
    /// over the 1 MiB cap, or not UTF-8. **Nothing was changed**, and the file is
    /// exactly as the run left it. Carries the short reason.
    NotKept(String),
    /// This run never wrote this path. Nothing was changed.
    NotRecorded,
}

/// Put a file back the way it was before this run first wrote it (0.28.0).
///
/// The restore point is the state of the file before the run's *first* write to
/// it, not before its last: a run that edited one file five times rewinds to
/// where it started, which is the only definition under which "undo what this run
/// did" is one operation rather than five.
///
/// Restoring writes through [`Workspace::write_file`], so the same path policy the
/// edit obeyed governs the undo — a rewind cannot put bytes anywhere the run could
/// not have written them. Removing is stricter and checks
/// [`Workspace::check_path`] itself, refusing anything that is not an outright
/// [`Effect::Allow`]: a `write_file` is content a human can inspect afterwards,
/// where a delete under an `Ask` verdict is not recoverable by inspecting
/// anything.
///
/// Removing a file that is already gone is [`Rewind::Removed`], not an error, so
/// calling this twice is safe.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind, Rewind, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(dir.path().join("notes.md"), "the original\n")?;
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
/// let run_id = store.start_run("tidy the notes", "notes.md")?;
///
/// // A run writes its own restore points as it edits. This one wrote none, so
/// // there is nothing to put back — and, crucially, nothing is touched.
/// assert_eq!(rewind(&ws, &store, run_id, "notes.md")?, Rewind::NotRecorded);
/// assert_eq!(ws.read_file("notes.md")?, "the original\n");
///
/// // After a real run, the same call answers `Restored` for a file it rewrote,
/// // `Removed` for one it created, and `NotKept` for one whose previous
/// // contents were over the 1 MiB cap or were not text.
/// # Ok(())
/// # }
/// ```
pub fn rewind(ws: &Workspace, store: &Store, run_id: i64, path: &str) -> Result<Rewind> {
    let Some(snap) = store.snapshot(run_id, path)? else {
        return Ok(Rewind::NotRecorded);
    };
    match snap.kept {
        Kept::Text(before) => Ok(Rewind::Restored(ws.write_file(path, &before)?)),
        Kept::Unkept(why) => Ok(Rewind::NotKept(why)),
        Kept::Absent => {
            let verdict = ws.check_path(Act::Write, path);
            if verdict.effect != Effect::Allow {
                return Err(Error::Refused {
                    act: "write".to_string(),
                    target: path.to_string(),
                    rule: verdict.rule,
                    layer: verdict.layer,
                });
            }
            let abs = ws.resolve(path)?;
            match std::fs::remove_file(abs) {
                Ok(()) => Ok(Rewind::Removed),
                // Already gone is the state being asked for, so this is done,
                // not failed. Reporting an error here would make a rewind of a
                // whole run fail on the one file a human had already deleted.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Rewind::Removed),
                Err(e) => Err(e.into()),
            }
        }
    }
}

/// What [`rewind_run`] put back (0.36.0).
///
/// Three kinds of effect, kept apart because a caller reporting to a human has to
/// say which happened: files come back with the four verdicts [`Rewind`] already
/// distinguishes, memory entries were either restored to an earlier value or
/// removed because this run created them, and queued children were dropped.
///
/// A rewind that restores the files and leaves the memory is the failure this
/// type exists to make visible — the two effects that outlive a run's files are
/// exactly the two that change what the *next* run does.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind_run, Rewind, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// std::fs::write(dir.path().join("notes.md"), "the original\n")?;
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
/// let run = store.start_run("tidy the notes", &dir.path().display().to_string())?;
///
/// // This run wrote nothing, remembered nothing and queued nothing, so there is
/// // nothing to put back — and, crucially, nothing is touched.
/// let done = rewind_run(&ws, &store, run)?;
/// assert!(done.files.is_empty());
/// assert!(done.memory_restored.is_empty() && done.memory_removed.is_empty());
/// assert!(done.queue_cleared.is_empty());
/// assert_eq!(ws.read_file("notes.md")?, "the original\n");
///
/// // After a real run, `files` carries one entry per path it wrote, with the
/// // same verdict `rewind` would have given for that path on its own.
/// let _: fn(&Rewind) = |_| ();
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rewound {
    /// Every path the run recorded a restore point for, with what happened to it,
    /// in the order the run first wrote them.
    pub files: Vec<(String, Rewind)>,
    /// Memory keys put back to the value that was there before this run's first
    /// write to them.
    pub memory_restored: Vec<String>,
    /// Memory keys this run created, and which are therefore now gone.
    pub memory_removed: Vec<String>,
    /// Children still queued under this run when it was rewound, as
    /// `(depth, goal)` — the shape [`Store::queued_agents`] returns.
    pub queue_cleared: Vec<(u32, String)>,
}

/// Put a whole run back: its files, what it remembered, and what it had queued
/// (0.36.0).
///
/// [`rewind`] answers "undo this edit". This answers "undo this run", which is
/// not the same question and was not previously answerable: a run that wrote
/// three files, recorded two decisions in memory and queued four children leaves
/// three of those five effects in place after an operator has restored every
/// file — and the two that remain are the ones that change what the next run
/// does. Memory is read into context, so a wrong fact a rewound run learned
/// outlives the files it was learned from; a queue backlog is adopted on resume,
/// so work the operator undid is re-admitted. A partial undo is worse than none,
/// because it looks complete.
///
/// Each file gets the verdict [`rewind`] would have given it alone, for the same
/// restore point: before the run's *first* write to that path.
///
/// **Nothing in the trace is deleted.** The steps, the event stream, the spawn
/// records and the ledger are untouched, and the rewind is written down as a row
/// of its own naming what it restored, removed and cleared — readable through
/// [`Store::rewinds`]. The spend happened; an undo that erased the rows would
/// make the ledger disagree with the invoice and make "this agent has tried this
/// three times" unanswerable.
///
/// What it does **not** undo, plainly rather than by implication: a commit the
/// run made is still there (`git reset` is unreachable from this crate by
/// construction), a push is not recalled, a migration is not reversed, a
/// provider call is not un-billed, and a worktree is never removed. It is one
/// run, not a tree — a caller who wants a subtree loops over it.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind_run, MemoryKind, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let root = dir.path().display().to_string();
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
///
/// // What an earlier run learned.
/// let earlier = store.start_run("learn", &root)?;
/// store.memory_write(&root, "retries", "three", earlier, 1, MemoryKind::Fact)?;
///
/// // This run corrects it wrongly, and invents a second note of its own.
/// let run = store.start_run("get it wrong", &root)?;
/// store.memory_write(&root, "retries", "nine", run, 1, MemoryKind::Fact)?;
/// store.memory_write(&root, "flaky", "always", run, 2, MemoryKind::Fact)?;
///
/// let done = rewind_run(&ws, &store, run)?;
///
/// // Edited, so it comes back. Created, so it goes.
/// assert_eq!(done.memory_restored, ["retries"]);
/// assert_eq!(store.memory_get(&root, "retries")?.unwrap().value, "three");
/// assert_eq!(done.memory_removed, ["flaky"]);
/// assert!(store.memory_get(&root, "flaky")?.is_none());
///
/// // And the undoing is itself in the trace.
/// assert_eq!(store.rewinds(run)?.len(), 1);
/// # Ok(())
/// # }
/// ```
pub fn rewind_run(ws: &Workspace, store: &Store, run_id: i64) -> Result<Rewound> {
    rewind_run_observed(ws, store, run_id, &crate::observe::Ignore)
}

/// [`rewind_run`], reporting to an [`Observer`](crate::Observer) (0.36.0).
///
/// One [`EventKind::Rewound`] once the work is done, carrying the counts from the
/// value being returned rather than from a second query — a number re-read from
/// the store would be true whether or not the rewind happened, which is the
/// defect 0.32.0 paid to learn.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind_run_observed, EventKind, Flow, Observer, RunEvent, Store};
/// use std::sync::Mutex;
///
/// #[derive(Default)]
/// struct Seen(Mutex<Vec<String>>);
/// impl Observer for Seen {
///     fn event(&self, e: &RunEvent) -> Flow {
///         if let EventKind::Rewound { files, memory, queued } = &e.kind {
///             self.0.lock().unwrap().push(format!("{files}/{memory}/{queued}"));
///         }
///         Flow::Continue
///     }
/// }
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
/// let run = store.start_run("tidy up", &dir.path().display().to_string())?;
///
/// let seen = Seen::default();
/// rewind_run_observed(&ws, &store, run, &seen)?;
/// assert_eq!(*seen.0.lock().unwrap(), ["0/0/0"], "it happened, and undid nothing");
/// # Ok(())
/// # }
/// ```
pub fn rewind_run_observed(
    ws: &Workspace,
    store: &Store,
    run_id: i64,
    observer: &dyn Observer,
) -> Result<Rewound> {
    // Read everything that is about to change BEFORE changing any of it, so the
    // record names rows that existed rather than rows the code assumed. This is
    // the same rule 0.32.0 paid for on its backlog event: a number read from the
    // query that produced it is true whether or not the work happened.
    let paths = store.snapshot_paths(run_id)?;
    let notes = store.memory_snapshots(run_id)?;
    let queue_cleared = store.clear_queue_under(run_id)?;

    let mut files = Vec::with_capacity(paths.len());
    for path in paths {
        let verdict = rewind(ws, store, run_id, &path)?;
        files.push((path, verdict));
    }

    let mut memory_restored = Vec::new();
    let mut memory_removed = Vec::new();
    for note in notes {
        match note.created {
            true => {
                store.memory_delete(&note.workspace, &note.key)?;
                memory_removed.push(note.key);
            }
            false => {
                store.memory_restore(
                    &note.workspace,
                    &note.key,
                    note.before.as_deref().unwrap_or_default(),
                    note.kind.as_deref(),
                    run_id,
                    note.step,
                )?;
                memory_restored.push(note.key);
            }
        }
    }

    let done = Rewound {
        files,
        memory_restored,
        memory_removed,
        queue_cleared,
    };
    let names: Vec<String> = done.files.iter().map(|(p, _)| p.clone()).collect();
    store.record_rewind(
        run_id,
        &names,
        &done.memory_restored,
        &done.memory_removed,
        &done.queue_cleared,
        // `None`: this undid the run, not a step. The column is what lets a
        // reader tell the two acts apart.
        None,
    )?;
    // Built from the value being returned, never re-queried. The counts and the
    // `Rewound` a caller receives cannot disagree, because there is only one of
    // them.
    observer.event(&RunEvent::at_depth(
        run_id,
        0,
        0,
        EventKind::Rewound {
            files: done.files.len() as u32,
            memory: (done.memory_restored.len() + done.memory_removed.len()) as u32,
            queued: done.queue_cleared.len() as u32,
        },
    ));
    Ok(done)
}

/// What reverting one step did to one file (0.51.0).
///
/// Three variants and not two, because "it did not happen" has two causes that
/// an operator must be able to tell apart: the change is still there and this
/// build **could** have undone it but the file has moved on, and the change is
/// still there and this build has **nothing to undo it with**. The first is
/// answered by reverting the later steps first; the second never will be.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind_step, Reverted, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
/// let run = store.start_run("tidy the notes", &dir.path().display().to_string())?;
///
/// // This run wrote nothing, so its first step has nothing to put back — and,
/// // crucially, nothing is touched.
/// assert!(rewind_step(&ws, &store, run, 1)?.is_empty());
///
/// // After a real run there is one entry per path the step wrote, and each says
/// // which of three things happened. Only the first changed anything.
/// fn what_happened(r: &Reverted) -> &'static str {
///     match r {
///         Reverted::Applied(_) => "put back",
///         Reverted::Stale(_) => "the file has moved on; nothing was changed",
///         Reverted::NoHunk(_) => "there is no hunk to undo with; nothing was changed",
///         _ => "something a later release added",
///     }
/// }
/// # let _ = what_happened;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Reverted {
    /// Put back, carrying what the workspace said — a revert that reproduces
    /// what the file already held reports
    /// [`Wrote::Unchanged`](crate::tools::Wrote::Unchanged).
    Applied(Wrote),
    /// The file no longer matches the hunk's context, so **nothing was
    /// changed**. Carries the reason, naming the hunk and the line it expected.
    ///
    /// The ordinary cause is reverting out of order: a later step changed the
    /// same lines and is still standing on top of this one. Revert newest first
    /// and it applies.
    Stale(String),
    /// No hunk was stored for this edit, so there is nothing to reverse-apply,
    /// and **nothing was changed**.
    ///
    /// Either the row predates 0.51.0, or the file's previous contents were not
    /// kept — over the snapshot cap, or not text — in which case the reason is
    /// on that path's snapshot row. [`rewind`] is what puts such a file back,
    /// and it puts it back to before the run's first write rather than to before
    /// this step.
    NoHunk(String),
}

/// Undo one step's file changes by reverse-applying their stored hunks (0.51.0).
///
/// [`rewind`] answers "undo this file", [`rewind_run`] answers "undo this run",
/// and neither answers "undo *that*". A run's restore point is the state of a
/// file before the run's **first** write to it, so a twenty-step run whose step
/// eighteen was wrong could be thrown away whole or not at all. This is the
/// granularity in between, and it exists because 0.51.0 keeps the hunk.
///
/// **Walk backwards.** Reverse-application is order-sensitive: a step reverted
/// while a later step's change still sits on top of it finds context that has
/// moved, and the honest answer is [`Reverted::Stale`] and an untouched file,
/// never a fuzzy match that quietly corrupts it. To walk a run back, call this
/// for the newest step first and descend:
///
/// ```no_run
/// use io_harness::{rewind_step, tools::Workspace, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let (ws, store, run_id) = (Workspace::new("."), Store::memory()?, 1i64);
/// for step in (1..=18).rev() {
///     for (path, what) in rewind_step(&ws, &store, run_id, step)? {
///         println!("{step} {path}: {what:?}");
///     }
/// }
/// # Ok(())
/// # }
/// ```
///
/// One entry per path this step wrote, in the order it wrote them. A step that
/// wrote nothing returns an empty vector and changes nothing, which is not an
/// error — asking to undo a step that read three files is a reasonable question
/// with a short answer.
///
/// Writing goes through [`Workspace::write_file`], so the same path policy the
/// edit obeyed governs the undo: a revert cannot put bytes anywhere the run
/// could not have written them. Nothing in the trace is deleted, and the revert
/// is itself written down — [`Store::rewinds`] reports it with `undid_step` set,
/// which is what distinguishes it from a whole-run rewind.
pub fn rewind_step(
    ws: &Workspace,
    store: &Store,
    run_id: i64,
    step: u32,
) -> Result<Vec<(String, Reverted)>> {
    rewind_step_observed(ws, store, run_id, step, &crate::observe::Ignore)
}

/// [`rewind_step`], reporting to an [`Observer`](crate::Observer) (0.51.0).
///
/// One [`EventKind::Reverted`] once the work is done, carrying its counts from
/// the value being returned rather than from a second query — a number re-read
/// from the store would be true whether or not the revert happened, which is the
/// defect 0.32.0 paid to learn.
///
/// ```
/// use io_harness::tools::Workspace;
/// use io_harness::{rewind_step_observed, EventKind, Flow, Observer, RunEvent, Store};
/// use std::sync::Mutex;
///
/// #[derive(Default)]
/// struct Seen(Mutex<Vec<String>>);
/// impl Observer for Seen {
///     fn event(&self, e: &RunEvent) -> Flow {
///         if let EventKind::Reverted { undid_step, files } = &e.kind {
///             self.0.lock().unwrap().push(format!("{undid_step}/{files}"));
///         }
///         Flow::Continue
///     }
/// }
///
/// # fn main() -> io_harness::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let ws = Workspace::new(dir.path());
/// let store = Store::memory()?;
/// let run = store.start_run("tidy up", &dir.path().display().to_string())?;
///
/// let seen = Seen::default();
/// rewind_step_observed(&ws, &store, run, 4, &seen)?;
/// assert_eq!(*seen.0.lock().unwrap(), ["4/0"], "it happened, and undid nothing");
/// # Ok(())
/// # }
/// ```
pub fn rewind_step_observed(
    ws: &Workspace,
    store: &Store,
    run_id: i64,
    step: u32,
    observer: &dyn Observer,
) -> Result<Vec<(String, Reverted)>> {
    let mut done: Vec<(String, Reverted)> = Vec::new();
    for edit in store.edits(run_id)?.into_iter().filter(|e| e.step == step) {
        let Some(hunk) = edit.hunk.as_deref() else {
            done.push((
                edit.path,
                Reverted::NoHunk(
                    "no hunk was stored for this edit — it predates 0.51.0, or the file's \
                     previous contents were not kept. `rewind` puts the file back to before \
                     this run first wrote it"
                        .to_string(),
                ),
            ));
            continue;
        };
        // Read through the workspace, so a path the policy will not let us read
        // is refused here rather than after a partial write.
        let current = match ws.read_file(&edit.path) {
            Ok(text) => text,
            Err(e) => {
                done.push((edit.path, Reverted::Stale(e.to_string())));
                continue;
            }
        };
        let restored = crate::diff::parse(hunk)
            .and_then(|hunks| crate::diff::apply(&current, &crate::diff::reverse(&hunks)));
        match restored {
            Ok(text) => {
                let wrote = ws.write_file(&edit.path, &text)?;
                done.push((edit.path, Reverted::Applied(wrote)));
            }
            // Not an error: the file has moved on, which is an ordinary answer to
            // an out-of-order revert and something the caller acts on rather than
            // something that should end their loop over the other paths.
            Err(e) => done.push((edit.path, Reverted::Stale(e.to_string()))),
        }
    }

    let names: Vec<String> = done
        .iter()
        .filter(|(_, r)| matches!(r, Reverted::Applied(_)))
        .map(|(p, _)| p.clone())
        .collect();
    let applied = names.len() as u32;
    store.record_rewind(run_id, &names, &[], &[], &[], Some(step))?;
    observer.event(&RunEvent::at_depth(
        run_id,
        step,
        0,
        EventKind::Reverted {
            // Not `step`: the kind is `#[serde(flatten)]`ed into `RunEvent`,
            // which already carries one, and a duplicate key compiles,
            // serialises, and fails only on the way back.
            undid_step: step,
            files: applied,
        },
    ));
    Ok(done)
}

/// Run a task contract to a verified result using `provider` and `store`.
///
/// Each iteration: read the file into context, ask the model (offering the
/// `write_file` tool, retrying transient failures), apply any write, record the
/// trace, then verify. Stops on the first passing verify, or when any budget —
/// steps, time, or tokens — is reached.
///
/// The smallest thing that works, and the right entry point when the boundary is
/// the *task* rather than a policy: one file, one tool, and a criterion the model
/// cannot talk its way past.
///
/// ```no_run
/// use io_harness::{run, OpenRouter, RunOutcome, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let contract = TaskContract::new(
///     "add a `hello` function returning 42",
///     "src/hello.rs",
///     // Execution-based: the project's own build has to succeed, so `fn hello`
///     // written as a literal string fails — which is exactly what a model did to
///     // the cheaper `FileContains` in the 0.1.0 live run.
///     Verification::Command { argv: vec!["cargo".into(), "build".into()], expect_exit: 0 },
/// )
/// .with_max_steps(6);
///
/// let result = run(&contract, &OpenRouter::from_env()?, &Store::open("runs.db")?).await?;
/// match result.outcome {
///     RunOutcome::Success { steps } => println!("verified in {steps} steps"),
///     // Keep the id: the file on disk is the run's state, so a resume continues
///     // from it rather than starting over.
///     other => println!("{other:?} — resume run {}", result.run_id),
/// }
/// # Ok(()) }
/// ```
///
/// It applies [`Policy::permissive`] and approves everything, because there is no
/// policy-aware tool layer in single-file mode — passing a real policy to
/// [`run_with`] with a single-file contract is refused with
/// [`Error::Config`](crate::Error::Config) rather than silently ignored. Use
/// [`TaskContract::workspace`] and [`run_with`] as soon as a boundary matters.
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
///
/// Reach for it when a run is long enough that silence is a problem. Without an
/// observer the only thing between "started" and "finished" is the SQLite trace,
/// which nobody is watching while it happens:
///
/// ```no_run
/// use io_harness::{run_observed, EventKind, Flow, Observer, OpenRouter, RunEvent,
///                  Store, TaskContract};
///
/// /// A progress line per committed step — the boundary at which work is durable.
/// struct Progress;
///
/// impl Observer for Progress {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Step { decision, tokens, changed, .. } = &event.kind {
///             let mark = if *changed { "*" } else { " " };
///             println!("{mark} step {} ({tokens} tokens): {decision}", event.step);
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// run_observed(contract, &OpenRouter::from_env()?, &Store::memory()?, &Progress).await?;
/// # Ok(()) }
/// ```
///
/// Events are delivered in order, on the run's own task, so an observer that
/// blocks holds the run up. Anything slow belongs on a channel.
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
///
/// This is the entry point most callers want: a workspace, a boundary, and a
/// human only for the grey tier.
///
/// ```no_run
/// use io_harness::{run_with, OpenRouter, Policy, StdinApprover, Store, TaskContract,
///                  Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// let contract = TaskContract::workspace(
///     "make the failing test in tests/parse.rs pass",
///     "/path/to/repo",
/// )
///     .with_verification(Verification::Command {
///         argv: vec!["cargo".into(), "test".into()],
///         expect_exit: 0,
///     });
///
/// // Three tiers, and the middle one is the only one anybody is asked about.
/// // `Policy::default()` already denies `.env`, `*.pem` and the other secret
/// // paths outright, so those never become a question at 3am.
/// let policy = Policy::default()
///     .layer("app")
///     .allow_read("*")
///     .allow_write("src/*")     // routine, proceeds silently
///     .deny_write("src/main.rs"); // never, and the approver is not consulted
///
/// // Everything else the policy marks `Ask` — a write outside src/, say —
/// // stops here and waits, for as long as it takes.
/// let result = run_with(
///     &contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, &policy, &StdinApprover,
/// )
/// .await?;
/// println!("{:?}", result.outcome);
/// # Ok(()) }
/// ```
///
/// The policy is recorded against the run, so a later [`resume_from_stored_policy`]
/// can recover the boundary this run executed under without the caller having to
/// reconstruct it.
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
///
/// The events a *policed* run adds are the ones worth watching: a refusal names
/// the rule and layer that made it, which turns "the agent kept failing" into
/// "one line of the ops baseline is too tight".
///
/// ```no_run
/// use io_harness::{run_with_observed, ApproveAll, EventKind, Flow, Observer, OpenRouter,
///                  Policy, RunEvent, Store, TaskContract};
/// use std::sync::Mutex;
///
/// /// Collects every refusal so the operator sees which rules the task ran into,
/// /// rather than only that it ran out of steps.
/// #[derive(Default)]
/// struct Friction(Mutex<Vec<String>>);
///
/// impl Observer for Friction {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Refused { act, target, rule, layer } = &event.kind {
///             self.0.lock().unwrap().push(format!(
///                 "{act} {target} <- {} in {}",
///                 rule.as_deref().unwrap_or("tier default"),
///                 layer.as_deref().unwrap_or("-"),
///             ));
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy) -> io_harness::Result<()> {
/// let friction = Friction::default();
/// run_with_observed(
///     contract, &OpenRouter::from_env()?, &Store::memory()?, policy, &ApproveAll, &friction,
/// )
/// .await?;
/// for line in friction.0.lock().unwrap().iter() {
///     println!("refused: {line}");
/// }
/// # Ok(()) }
/// ```
///
/// `&self`, not `&mut self`: one observer serves a whole tree, so state goes
/// behind a `Mutex` as above.
pub async fn run_with_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    run_with_extras(
        contract,
        provider,
        store,
        policy,
        approver,
        observer,
        &TurnExtras::default(),
    )
    .await
}

/// What a session turn adds to a run, and nothing else does.
///
/// Three fields, all inert by default, which is what makes 0.20.0's session layer
/// a layer: every 0.19.0 entry point drives the loop with `TurnExtras::default()`
/// and therefore behaves exactly as it did — no seeded conversation, no steer
/// inbox to read, and `complete` rather than `complete_streaming`.
#[derive(Default)]
pub(crate) struct TurnExtras<'a> {
    /// The conversation this turn continues, rendered one entry per prior turn on
    /// the path from the tree's root to the head. Seeded into the observation
    /// ledger before the first step, so it is compacted by the assembler that
    /// already compacts a long run rather than by a second rule of its own.
    pub seed: &'a [(&'static str, String)],
    /// Where an operator's mid-turn messages and an interrupt arrive. Drained at
    /// the step boundary and nowhere else.
    pub steer: Option<&'a crate::session::SteerInbox>,
    /// Whether to ask the provider for deltas as they arrive.
    pub stream: bool,
    /// The conversation this run is a turn of, when it is one. Written as a
    /// `session_turns` row immediately after the run row exists, so a turn whose
    /// process dies mid-answer is in the tree with a run id a resume can continue
    /// from.
    pub turn: Option<crate::session::SessionTurn<'a>>,
    /// Whether this turn's own first completion is allowed to decide that the turn
    /// was conversation rather than work (0.37.0).
    ///
    /// `false` by default, which is what keeps every one-shot entry point exactly
    /// as it was: `run_with` and `run_with_observed` drive the loop with
    /// `TurnExtras::default()`, so no classification code is reachable from them.
    pub classify: bool,
}

/// A session turn that answered rather than ran, as `runs.turn_kind` spells it.
pub(crate) const TURN_KIND_REPLY: &str = "reply";

/// A session turn that reached for a tool, as `runs.turn_kind` spells it.
pub(crate) const TURN_KIND_RUN: &str = "run";

/// [`run_with_observed`] with the session layer's extras. Crate-internal: the
/// public surface gains named session methods, not a seventh parameter on the
/// entry points every caller already uses.
pub(crate) async fn run_with_extras<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
    extras: &TurnExtras<'_>,
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
    // The lease, taken the moment the run exists and released when this function
    // returns however it returns (0.62.0). A fresh run's acquire cannot conflict —
    // nobody else has this id yet — so the `?` here is for a store that failed to
    // write, not for a race.
    let _lease = store.acquire_lease(run_id, contract.lease_ttl.as_secs() as i64)?;
    // A session turn joins the tree here: after the run exists, before the first
    // completion is billed. The order matters — a turn row with no run to point at
    // would be a conversation entry nothing can explain.
    if let Some(turn) = &extras.turn {
        store.record_turn(turn.session_id, turn.parent_turn_id, run_id, turn.prompt)?;
    }
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
    emit_plugins(watch, run_id, contract);
    // Decided against the *caller's* policy, before the provider layer is merged
    // in: the harness adding a network layer of its own must not turn a
    // permissive caller into a policy-bearing one and push it off the
    // single-file path.
    let caller_enforces = !policy.is_permissive();
    let policy = &match authorize_provider(
        provider,
        policy,
        store,
        run_id,
        approver,
        watch,
        &contract.goal,
    )
    .await?
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
            let lsp = lsp_for(contract, policy, store, run_id, watch).await?;
            let browser = browser_for(contract, policy);
            let result = run_workspace_from(
                contract, provider, store, run_id, &root, 1, policy, approver, &mcp, &lsp,
                &browser, &skills, watch, extras,
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
            result
        }
        // Single-file mode has no policy-aware tool layer in 0.4.0. Silently
        // ignoring a policy here would be worse than not supporting it: the
        // caller would believe a boundary was enforced when nothing was
        // checking. Refuse loudly instead.
        None if caller_enforces => Err(crate::error::Error::Config(
            "a permission policy requires workspace mode — build the contract \
             with TaskContract::workspace(goal, root). Single-file \
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
///
/// The shape a supervisor process wants: the run id is the only thing that has
/// to survive the crash, and resuming twice is a no-op rather than a second run.
///
/// ```no_run
/// use io_harness::{run, resume, OpenRouter, RunOutcome, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let provider = OpenRouter::from_env()?;
///
/// let first = run(contract, &provider, &store).await?;
/// // ... the process is killed here, mid-step ...
///
/// // A crash leaves either a whole step or none of it — the trace, the budget
/// // draw and the checkpoint commit in one transaction — so this continues from
/// // the last committed step rather than restarting. Completed steps are
/// // skipped, spend is restored from durable totals rather than reset, and the
/// // time budget counts the downtime as elapsed.
/// let again = resume(contract, &provider, &store, first.run_id).await?;
///
/// // Idempotent: a finished run reports its outcome instead of re-driving.
/// if let RunOutcome::Success { steps } = again.outcome {
///     println!("done at step {steps}");
/// }
/// # Ok(()) }
/// ```
///
/// It refuses a run that had a boundary — that is the point of it. Use
/// [`resume_with`] when you hold the policy, and [`resume_from_stored_policy`]
/// when you do not and want the one the run actually executed under.
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
///
/// That silence is the useful signal, and it is the reason to observe a resume at
/// all: an observer that hears nothing but `Started` and `Finished` is looking at
/// a run that had already completed before the crash, not one that did work.
///
/// ```no_run
/// use io_harness::{resume_observed, EventKind, Flow, Observer, OpenRouter, RunEvent,
///                  Store, TaskContract};
/// use std::sync::atomic::{AtomicU32, Ordering};
///
/// /// Counts only what *this* process drove. Steps committed before the crash
/// /// are replayed from the store, not re-run, so they emit nothing.
/// #[derive(Default)]
/// struct DrivenHere(AtomicU32);
///
/// impl Observer for DrivenHere {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if matches!(event.kind, EventKind::Step { .. }) {
///             self.0.fetch_add(1, Ordering::Relaxed);
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, run_id: i64) -> io_harness::Result<()> {
/// let driven = DrivenHere::default();
/// resume_observed(contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, &driven)
///     .await?;
/// println!("{} new steps after the restart", driven.0.load(Ordering::Relaxed));
/// # Ok(()) }
/// ```
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
    // 0.65.0 — refuse to re-drive a run whose journal still holds a call the
    // harness cannot inspect. Before anything drives, because the first thing a
    // resumed run does is replay the step that died, and by then the decision to
    // repeat the call has already been taken.
    if let Some(paused) = recovery_pause(store, run_id, observer)? {
        return Ok(RunResult::new(paused, run_id));
    }
    // The lease (0.62.0). Taken after the resumability checks so an unknown run
    // still reports as an unknown run, and before any step is driven so a second
    // live driver is refused rather than interleaving its steps with the first
    // one's. Released when this function returns, however it returns.
    let _lease = store.acquire_lease(run_id, DEFAULT_LEASE_TTL.as_secs() as i64)?;
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
///
/// Use it when the policy is something your program *builds* — from config it
/// still holds, or from config that has since changed and should now apply:
///
/// ```no_run
/// use io_harness::{resume_with, OpenRouter, Policy, StdinApprover, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract, run_id: i64) -> io_harness::Result<()> {
/// // The same policy the run started under, rebuilt from the same config — plus
/// // one deny added since, which now applies to the rest of the run. A resume is
/// // the natural place to tighten: nothing forces the resumed run to inherit a
/// // boundary the operator has since decided was too wide.
/// let policy = Policy::default()
///     .layer("app")
///     .allow_read("*")
///     .allow_write("src/*")
///     .layer("incident-2026-07")
///     .deny_write("src/billing/*");
///
/// resume_with(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, &policy,
///     &StdinApprover,
/// )
/// .await?;
/// # Ok(()) }
/// ```
///
/// The policy given here is recorded against the run, so the store keeps
/// answering what the run actually executed under. If you cannot reconstruct one,
/// do not pass [`Policy::permissive`] to get moving — use
/// [`resume_from_stored_policy`], which reads back the real one.
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

/// Continue a run that paused on a question, with the answer a human gave.
///
/// The counterpart to [`resume_with_decision`], for the other channel. `answer` is
/// text the model reads and it authorizes **nothing**: every tool call it leads to is
/// checked against the same [`Policy`] by the same code. A human answering "just write
/// the file" does not make a denied write permitted.
///
/// This is deliberately thin, and the thinness is the design: recording the answer and
/// resuming normally is enough, so there is no second resume path for the ledger, the
/// checkpoint and the policy to be got wrong in.
///
/// The step that asked **was** committed — a paused step commits and the resume starts
/// after it — so the `ask_question` call is not replayed and cannot be handed its own
/// answer. The answer is delivered instead as an observation on the run's ledger, which
/// is what puts it in the next assembled prompt. This is the same mechanism 0.20.0 uses
/// to deliver a steer.
///
/// Answering a question that was already answered is an [`Error::Resume`] rather than a
/// second run — see [`Store::answer_question`].
///
/// ```no_run
/// use io_harness::{resume_with_answer, ApproveAll, OpenRouter, Policy, RunOutcome,
///                  Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// # let contract = TaskContract::workspace("port it", "/repo");
/// let store = Store::open("runs.db")?;
/// let provider = OpenRouter::from_env()?;
/// let policy = Policy::permissive();
///
/// // Some earlier process left this run waiting on a question.
/// let run_id = 7;
/// let question_id = 3;
/// let q = store.question(question_id)?.expect("asked earlier");
/// println!("the agent asked: {}", q.question);
///
/// let result = resume_with_answer(
///     &contract, &provider, &store, run_id, question_id,
///     "io.local.toml — the committed one is the template",
///     &policy, &ApproveAll,
/// ).await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_answer<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    question_id: i64,
    answer: &str,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    resume_with_answer_observed(
        contract,
        provider,
        store,
        run_id,
        question_id,
        answer,
        policy,
        approver,
        &Ignore,
    )
    .await
}

/// [`resume_with_answer`] with an [`Observer`].
///
/// ```no_run
/// use io_harness::{resume_with_answer_observed, ApproveAll, Ignore, OpenRouter, Policy,
///                  Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// # let contract = TaskContract::workspace("port it", "/repo");
/// let result = resume_with_answer_observed(
///     &contract, &OpenRouter::from_env()?, &Store::open("runs.db")?,
///     7, 3, "io.local.toml", &Policy::permissive(), &ApproveAll, &Ignore,
/// ).await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_answer_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    question_id: i64,
    answer: &str,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    record_answer(store, run_id, question_id, answer)?;
    resume_with_observed(
        contract, provider, store, run_id, policy, approver, observer,
    )
    .await
}

/// Continue a tree that paused on a child's question.
///
/// A child's question pauses the whole tree, exactly as a child's deferred approval
/// does, so this is the tree's counterpart to [`resume_tree_with_decision`]. `run_id`
/// is the ROOT's run id and `question_id` identifies the question, which may belong to
/// any agent in the tree — the resume walks the tree and every agent continues from its
/// own last committed step.
///
/// ```no_run
/// use io_harness::{resume_tree_with_answer, ApproveAll, Containment, OpenRouter, Policy,
///                  Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// # let contract = TaskContract::workspace("port it", "/repo");
/// let store = Store::open("runs.db")?;
///
/// // The question belongs to whichever agent asked it — often a child — but the run id
/// // passed here is the root's, because what resumes is the tree.
/// let root_run_id = 7;
/// let question_id = 4;
/// let asked_by = store.question(question_id)?.map(|q| q.run_id);
/// println!("the question came from run {asked_by:?}");
///
/// let result = resume_tree_with_answer(
///     &contract, &OpenRouter::from_env()?, &store, root_run_id, question_id,
///     "keep the old column", &Policy::permissive(), &ApproveAll,
///     &Containment::new(10, 4, 3, 1_000_000),
/// ).await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_answer<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    question_id: i64,
    answer: &str,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    resume_tree_with_answer_observed(
        contract,
        provider,
        store,
        run_id,
        question_id,
        answer,
        policy,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`resume_tree_with_answer`] with an [`Observer`].
///
/// ```no_run
/// use io_harness::{resume_tree_with_answer_observed, ApproveAll, Containment, Ignore,
///                  OpenRouter, Policy, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// # let contract = TaskContract::workspace("port it", "/repo");
/// let result = resume_tree_with_answer_observed(
///     &contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, 7, 4,
///     "keep the old column", &Policy::permissive(), &ApproveAll,
///     &Containment::new(10, 4, 3, 1_000_000), &Ignore,
/// ).await?;
/// # let _ = result;
/// # Ok(())
/// # }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_answer_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    question_id: i64,
    answer: &str,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    // The question may belong to a child, so it is resolved against its own run id
    // rather than the root's.
    let owner = store
        .question(question_id)?
        .map(|q| q.run_id)
        .unwrap_or(run_id);
    record_answer(store, owner, question_id, answer)?;
    resume_tree_observed(
        contract,
        provider,
        store,
        run_id,
        policy,
        approver,
        containment,
        observer,
    )
    .await
}

/// Record a human's answer against the question that paused a run.
///
/// One place, so the four entry points above cannot drift in what they check. The
/// question must exist, must belong to this run, and must not already be answered —
/// resuming a run with an answer to somebody else's question would replay a step that
/// then asked again and paused again, which reads as a hang rather than an error.
fn record_answer(store: &Store, run_id: i64, question_id: i64, answer: &str) -> Result<()> {
    let question = store.question(question_id)?.ok_or_else(|| Error::Resume {
        reason: format!("no question {question_id} to answer"),
    })?;
    if question.run_id != run_id {
        return Err(Error::Resume {
            reason: format!(
                "question {question_id} belongs to run {}, not {run_id}",
                question.run_id
            ),
        });
    }
    // 0.33.0: the swap answers "was it me". A resume that finds the question
    // already answered — by a second process through `Attach`, or by an earlier
    // resume — must refuse rather than replay the step: the run acted on somebody
    // else's answer, and driving it again with a different one is exactly the
    // silent double-answer the store's compare-and-swap exists to make impossible.
    if !store.answer_question(question_id, answer, "human")? {
        return Err(Error::Resume {
            reason: format!("question {question_id} was already answered"),
        });
    }

    // The answer has to reach the model, and an observation is the only thing that
    // does. The step that asked WAS committed — a paused step is committed and the
    // resume starts after it — so there is no replay of the `ask_question` call to
    // hand the answer back to. Appending it to the run's observation ledger is what
    // puts it in the next assembled prompt, which is exactly how 0.20.0 delivers a
    // steer.
    //
    // `ObsKind::Message` and no target, so nothing can supersede it away: an answer
    // is not an observation *of* anything, and the assembler must not stub it as
    // stale when a later read touches the same path.
    store.record_observations(
        run_id,
        &[Observation::new(
            question.step,
            ObsKind::Message,
            None,
            format!(
                "\n[answer] {answer}\n(This is what the operator wanted, in reply to: \
                 {}. It is not permission for anything.)\n",
                question.question
            ),
        )],
    )?;
    Ok(())
}

/// Record a human's verdict on a plan and put it where the model will read it
/// (0.31.0).
///
/// The counterpart of [`record_answer`], and thin for the same reason: the step
/// that proposed **was** committed — a paused step commits and the resume starts
/// after it — so the `propose_plan` call is not replayed and cannot be handed its
/// own verdict. It arrives as an observation instead, which is what puts it in the
/// next assembled prompt.
///
/// A plan belonging to a different run is an [`Error::Resume`] rather than a
/// silent no-op, for the reason [`record_answer`] refuses one: approving somebody
/// else's plan would leave this run planning forever, which reads as a hang.
fn record_plan_decision(
    store: &Store,
    run_id: i64,
    plan_id: i64,
    verdict: &PlanVerdict,
) -> Result<()> {
    let pending = store.plan(plan_id)?.ok_or_else(|| Error::Resume {
        reason: format!("no plan {plan_id} to decide"),
    })?;
    if pending.run_id != run_id {
        return Err(Error::Resume {
            reason: format!(
                "plan {plan_id} belongs to run {}, not {run_id}",
                pending.run_id
            ),
        });
    }
    // Refused for the reason `record_answer` refuses: a plan a second process
    // already decided has already moved the run, and deciding it again here would
    // drive it a second time from a verdict nothing recorded.
    if !store.decide_plan(plan_id, verdict, "human")? {
        return Err(Error::Resume {
            reason: format!("plan {plan_id} was already decided"),
        });
    }

    // `ObsKind::Message` and no target, exactly as an answer is, so the assembler
    // cannot stub it away as stale when a later read touches the same path.
    let text = match verdict {
        PlanVerdict::Approve => format!(
            "\n[plan approved]\n{}\n(This is the approach you agreed to. Carry it out.)\n",
            pending.plan.render()
        ),
        PlanVerdict::Revise { correction } => format!(
            "\n[plan not approved] {correction}\n(Propose a different plan with \
             `{PROPOSE_PLAN_TOOL}`. Nothing has been done yet and nothing will be until a \
             plan is approved.)\n"
        ),
        // A cancelled run is not resumed, so nothing reads this. It is written
        // anyway: the ledger is the run's own account of itself and a run that
        // stopped because a human said no should say so in it.
        PlanVerdict::Cancel => "\n[plan cancelled] the operator stopped this run.\n".to_string(),
    };
    store.record_observations(
        run_id,
        &[Observation::new(pending.step, ObsKind::Message, None, text)],
    )?;
    Ok(())
}

/// Continue a run that paused on a plan, with a human's verdict (0.31.0).
///
/// The counterpart of [`resume_with_answer`], and the half of the plan gate that
/// makes it a gate rather than a prompt: the deciding process need not be the one
/// that proposed, need not be on the same machine, and need not have started yet
/// when the run stopped.
///
/// [`PlanVerdict::Approve`] ends the planning phase — the loop reads that from the
/// store, so it stays ended across every later restart — and the plan reaches the
/// model as an observation. [`PlanVerdict::Revise`] leaves the phase on and puts
/// the correction in front of the model. [`PlanVerdict::Cancel`] does **not**
/// resume: the run is finished as [`RunOutcome::PlanRejected`], because a human
/// refusing the approach is as final as a human denying an action.
///
/// ```no_run
/// use io_harness::{resume_with_plan_decision, ApproveAll, OpenRouter, PlanVerdict, Policy,
///                  RunOutcome, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract, outcome: RunOutcome) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// if let RunOutcome::AwaitingPlan { plan_id, .. } = outcome {
///     // Show the human what was actually proposed, read back from the store.
///     // Nothing in the workspace has been touched at this point.
///     let pending = store.plan(plan_id)?.expect("a pending plan");
///     println!("{}", pending.plan.render());
///
///     resume_with_plan_decision(
///         contract, &OpenRouter::from_env()?, &store, pending.run_id, plan_id,
///         PlanVerdict::Approve, &Policy::permissive(), &ApproveAll,
///     )
///     .await?;
/// }
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_plan_decision<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    plan_id: i64,
    verdict: PlanVerdict,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    resume_with_plan_decision_observed(
        contract, provider, store, run_id, plan_id, verdict, policy, approver, &Ignore,
    )
    .await
}

/// [`resume_with_plan_decision`] with an [`Observer`].
///
/// ```no_run
/// use io_harness::{resume_with_plan_decision_observed, ApproveAll, Ignore, OpenRouter,
///                  PlanVerdict, Policy, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// let result = resume_with_plan_decision_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, 7, 2,
///     PlanVerdict::revise("start with the tests"), &Policy::permissive(), &ApproveAll,
///     &Ignore,
/// ).await?;
/// # let _ = result;
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_plan_decision_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    plan_id: i64,
    verdict: PlanVerdict,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    record_plan_decision(store, run_id, plan_id, &verdict)?;
    if verdict == PlanVerdict::Cancel {
        return cancel_the_run(store, run_id, observer);
    }
    resume_with_observed(
        contract, provider, store, run_id, policy, approver, observer,
    )
    .await
}

/// Finish a run whose plan a human cancelled, without re-entering the loop.
///
/// Separate from the resume path because there is nothing to resume: a cancelled
/// plan means the approach was refused, so driving the loop again would ask the
/// model to propose the same thing to the same person.
fn cancel_the_run(store: &Store, run_id: i64, observer: &dyn Observer) -> Result<RunResult> {
    let watch = Watch::new(observer);
    let steps = store.last_step(run_id)?;
    finish(store, &watch, run_id, 0, steps, "plan_rejected")?;
    Ok(RunResult::new(RunOutcome::PlanRejected { steps }, run_id))
}

/// Continue a tree that paused on its root's plan (0.31.0).
///
/// The tree's counterpart to [`resume_with_plan_decision`]. Only the root holds a
/// plan — a child that could hold its own would mean a hundred pending plans from
/// one [`run_tree`] — so `run_id` is the root's and is also the plan's owner.
///
/// ```no_run
/// use io_harness::{resume_tree_with_plan_decision, ApproveAll, Containment, OpenRouter,
///                  PlanVerdict, Policy, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
///
/// // What a hundred-agent run is really being approved on: which definitions the
/// // root intends to spend on, before a single one is spawned.
/// let plan = store.plan(2)?.expect("proposed earlier");
/// println!("this will spawn: {:?}", plan.plan.agents().collect::<Vec<_>>());
///
/// let result = resume_tree_with_plan_decision(
///     contract, &OpenRouter::from_env()?, &store, 7, 2, PlanVerdict::Approve,
///     &Policy::permissive(), &ApproveAll, &Containment::new(10, 4, 3, 1_000_000),
/// ).await?;
/// # let _ = result;
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_plan_decision<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    plan_id: i64,
    verdict: PlanVerdict,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    resume_tree_with_plan_decision_observed(
        contract,
        provider,
        store,
        run_id,
        plan_id,
        verdict,
        policy,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`resume_tree_with_plan_decision`] with an [`Observer`].
///
/// ```no_run
/// use io_harness::{resume_tree_with_plan_decision_observed, ApproveAll, Containment,
///                  Ignore, OpenRouter, PlanVerdict, Policy, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract) -> io_harness::Result<()> {
/// let result = resume_tree_with_plan_decision_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, 7, 2,
///     PlanVerdict::Cancel, &Policy::permissive(), &ApproveAll,
///     &Containment::new(10, 4, 3, 1_000_000), &Ignore,
/// ).await?;
/// # let _ = result;
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_with_plan_decision_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    plan_id: i64,
    verdict: PlanVerdict,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    record_plan_decision(store, run_id, plan_id, &verdict)?;
    if verdict == PlanVerdict::Cancel {
        return cancel_the_run(store, run_id, observer);
    }
    resume_tree_observed(
        contract,
        provider,
        store,
        run_id,
        policy,
        approver,
        containment,
        observer,
    )
    .await
}

/// Resume a run under the policy it was started with, read back from the store.
///
/// [`resume_with`] takes a policy because a resumed run must not silently lose
/// its boundary — that was 0.13.0's subject. But the caller still had to
/// reconstruct one, and a caller resuming after a crash in another process may
/// have nothing to reconstruct it from. The policy has been durable since
/// 0.13.0; this is the entry point that uses it.
///
/// It matters more from 0.15.0 on than it did when it was first noticed: this is
/// the first release in which a crashed run may already have taken an
/// irreversible action — a commit — under a policy the resuming caller cannot
/// name.
///
/// Fails with [`Error::Resume`] when the store holds no policy for the run,
/// rather than substituting a permissive one. A run whose boundary cannot be
/// recovered is not resumed under no boundary; that substitution is exactly the
/// defect 0.13.0 closed.
///
/// The entry point for a restart supervisor, which knows a run id and nothing
/// else — it did not build the policy and has no config to rebuild it from:
///
/// ```no_run
/// use io_harness::{resume_from_stored_policy, DenyAll, Error, OpenRouter, RunStatus,
///                  Store, TaskContract};
///
/// # async fn sweep(contract: &TaskContract) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let provider = OpenRouter::from_env()?;
///
/// for run_id in [17_i64, 18, 19] {
///     if store.run_status(run_id)? != Some(RunStatus::Running) {
///         continue; // already finished, or paused on a human
///     }
///     // No policy argument, and that is the point: the boundary comes back from
///     // the store, so a supervisor cannot silently widen what an agent may do by
///     // being the process that happened to restart it.
///     match resume_from_stored_policy(contract, &provider, &store, run_id, &DenyAll).await {
///         Ok(result) => println!("run {run_id}: {:?}", result.outcome),
///         // Refused rather than resumed unbounded. A run whose boundary cannot
///         // be recovered stays stopped until a human names one.
///         Err(Error::Resume { reason }) => eprintln!("run {run_id} needs a human: {reason}"),
///         Err(e) => return Err(e),
///     }
/// }
/// # Ok(()) }
/// ```
///
/// Prefer it to [`resume_with`] whenever the resuming process is not the one that
/// wrote the policy — from 0.15.0 a crashed run may already have committed, so
/// the boundary it was working under is not a detail that can be approximated.
pub async fn resume_from_stored_policy<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
) -> Result<RunResult> {
    resume_from_stored_policy_observed(contract, provider, store, run_id, approver, &Ignore).await
}

/// [`resume_from_stored_policy`], reporting to `observer` as it happens. See
/// [`run_observed`].
///
/// The one thing this combination shows that no other does: the recovered
/// boundary in action. The caller never names the policy, so the refusals the
/// observer reports — each attributed to its rule and layer — are the only
/// visible evidence of which boundary came back from the store.
///
/// ```no_run
/// use io_harness::{resume_from_stored_policy_observed, DenyAll, EventKind, Flow, Observer,
///                  OpenRouter, RunEvent, Store, TaskContract};
///
/// /// Logs the layers the recovered policy is actually enforcing.
/// struct RecoveredBoundary;
///
/// impl Observer for RecoveredBoundary {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Refused { act, target, layer, .. } = &event.kind {
///             println!("still enforcing {}: refused {act} {target}",
///                      layer.as_deref().unwrap_or("tier default"));
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, run_id: i64) -> io_harness::Result<()> {
/// resume_from_stored_policy_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, &DenyAll,
///     &RecoveredBoundary,
/// )
/// .await?;
/// # Ok(()) }
/// ```
pub async fn resume_from_stored_policy_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    let Some(policy) = store.run_policy(run_id)? else {
        return Err(Error::Resume {
            reason: format!(
                "run {run_id} has no recorded policy, so the boundary it ran under cannot be \
                 recovered; pass one explicitly with `resume_with` if you know what it was"
            ),
        });
    };
    resume_with_observed(
        contract, provider, store, run_id, &policy, approver, observer,
    )
    .await
}

/// [`resume_with`], reporting to `observer` as it happens. See [`run_observed`].
///
/// An observer can also *stop* a run, which is worth pairing with a resume
/// because the two are the same mechanism seen from both ends: cancelling
/// finishes a run cleanly at the next step boundary and leaves it resumable, so
/// "stop it for now" and "carry on later" are one loop.
///
/// ```no_run
/// use io_harness::{resume_with_observed, ApproveAll, EventKind, Flow, Observer, OpenRouter,
///                  Policy, RunEvent, Store, TaskContract};
/// use std::sync::atomic::{AtomicU64, Ordering};
///
/// /// Stops the run once it has spent more than the operator is willing to.
/// struct SpendCeiling { limit: u64, spent: AtomicU64 }
///
/// impl Observer for SpendCeiling {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Step { tokens, .. } = &event.kind {
///             if self.spent.fetch_add(*tokens, Ordering::Relaxed) + tokens > self.limit {
///                 // Honoured at the next step boundary, never mid-step: no tool
///                 // call is abandoned in flight and no file is left half-written.
///                 // The run records `cancelled` and stays resumable.
///                 return Flow::Cancel;
///             }
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, run_id: i64)
/// #     -> io_harness::Result<()> {
/// let ceiling = SpendCeiling { limit: 50_000, spent: AtomicU64::new(0) };
/// let result = resume_with_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, policy,
///     &ApproveAll, &ceiling,
/// )
/// .await?;
/// // `RunOutcome::Cancelled` — finished, not abandoned. Dropping the future
/// // instead would leave `runs.status` as `running` forever, indistinguishable
/// // from a crashed process.
/// println!("{:?}", result.outcome);
/// # Ok(()) }
/// ```
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
    // 0.65.0 — refuse to re-drive a run whose journal still holds a call the
    // harness cannot inspect. Before anything drives, because the first thing a
    // resumed run does is replay the step that died, and by then the decision to
    // repeat the call has already been taken.
    if let Some(paused) = recovery_pause(store, run_id, observer)? {
        return Ok(RunResult::new(paused, run_id));
    }
    // The lease (0.62.0). Taken after the resumability checks so an unknown run
    // still reports as an unknown run, and before any step is driven so a second
    // live driver is refused rather than interleaving its steps with the first
    // one's. Released when this function returns, however it returns.
    let _lease = store.acquire_lease(run_id, contract.lease_ttl.as_secs() as i64)?;

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
    emit_plugins(watch, run_id, contract);
    match contract.root.clone() {
        Some(root) => {
            // Re-authorized on resume rather than trusted from the interrupted
            // run, for the reason [`resume_tree_observed`] gives: the policy
            // handed to the resume is the one that governs it, and a host allowed
            // before a crash may not be allowed after.
            let policy = &match authorize_provider(
                provider,
                policy,
                store,
                run_id,
                approver,
                watch,
                &contract.goal,
            )
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
            let lsp = lsp_for(contract, policy, store, run_id, watch).await?;
            let browser = browser_for(contract, policy);
            let result = run_workspace_from(
                contract,
                provider,
                store,
                run_id,
                &root,
                start_step,
                policy,
                approver,
                &mcp,
                &lsp,
                &browser,
                &skills,
                watch,
                &TurnExtras::default(),
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
            result
        }
        // The same refusal [`run_with_observed`] makes, for the same reason:
        // single-file mode has no policy-aware tool layer, and silently ignoring
        // a policy would leave the caller believing a boundary was enforced when
        // nothing was checking.
        None if caller_enforces => Err(crate::error::Error::Config(
            "a permission policy requires workspace mode — build the contract \
             with TaskContract::workspace(goal, root). Single-file \
             contracts are not policy-enforced."
                .into(),
        )),
        None => run_from(contract, provider, store, run_id, start_step, watch).await,
    }
}

/// What to do about a call that was in flight when the process died (0.65.0).
///
/// The three answers an operator has, and no more: the runtime deliberately has
/// no fourth, because anything else is a variation on one of these that only the
/// caller's own system can perform.
///
/// `#[non_exhaustive]` from birth, for the reason
/// [`SystemPrompt`](crate::SystemPrompt) is: a later release naming a fourth
/// answer must not break a caller who matched on these.
///
/// ```
/// use io_harness::RecoveryDecision;
///
/// // What an operator who checked the payment provider and found the charge
/// // captured sends back. The text is what the model is told the call returned.
/// let decided = RecoveryDecision::Completed {
///     observation: "charge ch_9f21 captured".into(),
/// };
/// assert_ne!(decided, RecoveryDecision::Retry);
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RecoveryDecision {
    /// Make the call again. The run resumes exactly as it would have without the
    /// journal — the step replays and the model re-issues the call — which is
    /// what a caller should choose when they have established the effect did not
    /// land, or when repeating it is harmless.
    Retry,
    /// The call landed; do not make it again. `observation` is what the model is
    /// told the call returned, and it reaches the model as an ordinary tool
    /// observation on the step the call was made — so the run continues from a
    /// transcript in which the tool answered, which is the truth.
    ///
    /// Nothing validates the text. The operator is asserting a fact about the
    /// outside world that the crate has no way to check, and a crate that
    /// pretended otherwise would be back where this release started.
    Completed {
        /// What the call returned, or the operator's account of it.
        observation: String,
    },
    /// Do not make the call, and do not continue. The run ends as
    /// [`RunOutcome::Denied`], which is what it already means for a human to
    /// refuse an action on resume and stop the run rather than perform it.
    Abort,
}

/// Continue a run paused at
/// [`RunOutcome::AwaitingRecovery`](crate::RunOutcome::AwaitingRecovery), with an
/// operator's decision about the call that was in flight (0.65.0).
///
/// ```no_run
/// use io_harness::{resume_with_recovery, ApproveAll, OpenRouter, Policy, RecoveryDecision,
///                  RunOutcome, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, run_id: i64) -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let provider = OpenRouter::from_env()?;
///
/// // The operator checked the payment provider: the charge did land, and the
/// // reference is what the tool would have returned.
/// if let RunOutcome::AwaitingRecovery { attempt_id, .. } =
///     io_harness::resume(contract, &provider, &store, run_id).await?.outcome
/// {
///     resume_with_recovery(
///         contract, &provider, &store, run_id, attempt_id,
///         RecoveryDecision::Completed { observation: "charge ch_9f21 captured".into() },
///         policy, &ApproveAll,
///     )
///     .await?;
/// }
/// # Ok(()) }
/// ```
///
/// The attempt is closed by this call whichever decision is made, and what was
/// decided is recorded against it — so a store read later says not only that a
/// call was interrupted but how it was resolved. A second decision on the same
/// attempt is an error rather than a second resolution.
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_recovery<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    attempt_id: i64,
    decision: RecoveryDecision,
    policy: &Policy,
    approver: &dyn Approver,
) -> Result<RunResult> {
    resume_with_recovery_observed(
        contract, provider, store, run_id, attempt_id, decision, policy, approver, &Ignore,
    )
    .await
}

/// [`resume_with_recovery`], reporting to `observer` as it happens. See
/// [`run_observed`].
///
/// ```no_run
/// use io_harness::{resume_with_recovery_observed, ApproveAll, Flow, Observer, OpenRouter,
///                  Policy, RecoveryDecision, RunEvent, Store, TaskContract};
///
/// struct Trail;
///
/// impl Observer for Trail {
///     fn event(&self, event: &RunEvent) -> Flow {
///         println!("run {} step {}: {:?}", event.run_id, event.step, event.kind);
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, run_id: i64, attempt_id: i64)
/// #     -> io_harness::Result<()> {
/// // The operator established the deployment never started, so making the call
/// // again is the safe answer rather than the optimistic one.
/// resume_with_recovery_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, attempt_id,
///     RecoveryDecision::Retry, policy, &ApproveAll, &Trail,
/// )
/// .await?;
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_with_recovery_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    attempt_id: i64,
    decision: RecoveryDecision,
    policy: &Policy,
    approver: &dyn Approver,
    observer: &dyn Observer,
) -> Result<RunResult> {
    store.check_resumable(run_id)?;
    // The attempt must be this run's and must still be open. A decision about an
    // attempt that is already resolved is refused rather than applied twice: the
    // second caller believes they authorised something, and under
    // `Completed` they would be writing a second observation of one call.
    let attempt = store
        .open_attempts(run_id)?
        .into_iter()
        .find(|a| a.id == attempt_id)
        .ok_or_else(|| {
            crate::error::Error::Config(format!(
                "run {run_id} has no open attempt {attempt_id} — it was never opened, belongs to \
                 another run, or has already been decided"
            ))
        })?;

    match decision {
        RecoveryDecision::Retry => {
            store.resolve_attempt(attempt_id, "retry")?;
        }
        RecoveryDecision::Completed { observation } => {
            // Written in the ledger's own shape, on the step the call was made,
            // so the resumed run assembles it exactly as it would have assembled
            // the answer the tool never got to give. Recorded BEFORE the attempt
            // is resolved: an interrupted decision then leaves the attempt open
            // and the run still paused, which is the recoverable direction.
            store.record_observations(
                run_id,
                &[crate::context::Observation::new(
                    attempt.step,
                    crate::context::ObsKind::Tool,
                    Some(attempt.tool.clone()),
                    format!("\n[{}]\n{}\n", attempt.tool, observation),
                )],
            )?;
            store.resolve_attempt(attempt_id, "completed")?;
        }
        RecoveryDecision::Abort => {
            store.resolve_attempt(attempt_id, "abort")?;
            store.finish_run(run_id, "denied")?;
            return Ok(RunResult::new(
                RunOutcome::Denied {
                    steps: store.last_step(run_id)?,
                },
                run_id,
            ));
        }
    }

    resume_with_observed(
        contract, provider, store, run_id, policy, approver, observer,
    )
    .await
}

/// The act a resumed approval replays, or `None` for a pending row this path
/// cannot replay (0.74.0).
///
/// The words are the ones `run::gate` persisted, asked for through `act_word`
/// rather than re-spelled here, so the writer and the reader of the column
/// cannot drift apart. Only the two filesystem acts have an effect to
/// replay: `exec` and `net` are claimed by the arm above this one's caller, which
/// grants what was approved and lets the model re-issue the call.
///
/// Before this, the column was read as "`read`, or else write". Everything that
/// was not the word `read` became [`Act::Write`] on a target that may be a
/// program name or a host — checked against the *path* policy, so `deny_exec` was
/// never consulted, and then created as a file at that name while the command
/// itself never ran. [`Store::put_pending`](crate::Store::put_pending) is public,
/// so the column can hold a word this crate never wrote, and a fifth [`Act`]
/// would have inherited the same arm. Refusing is the fail-closed answer: an
/// approval that cannot be replayed is not an approval to perform something else.
fn replayable_act(kind: &str) -> Option<Act> {
    let act = [Act::Read, Act::Write, Act::Exec, Act::Net]
        .into_iter()
        .find(|act| act_word(*act) == kind)?;
    // Total over `Act`, so a fifth one has to be answered for here rather than
    // inheriting the write.
    match act {
        Act::Read | Act::Write => Some(act),
        // Reaching this means the guard arm above the caller was removed, and a
        // file named after a program or a host is exactly what it prevents.
        Act::Exec | Act::Net => None,
    }
}

/// Refuse a resume whose persisted act this path cannot replay, recording it
/// (0.74.0).
///
/// A refusal row rather than a decision row, and the pending is deliberately left
/// unresolved: nothing was performed, so nothing was decided, and a row this
/// crate did not write is not one to spend a caller's approval on. The message
/// names the target, why it could not be replayed, and what to do instead.
fn unreplayable(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    event_run_id: i64,
    step: u32,
    pending: &crate::state::Pending,
) -> Error {
    let ev = PolicyEvent::refusal(step, &pending.act, &pending.target);
    if let Err(e) = store.record_event(event_run_id, &ev) {
        return e;
    }
    refused(watch, run_id, 0, &ev);
    Error::Refused {
        act: pending.act.clone(),
        target: format!(
            "{} — the approval was persisted as the act \"{}\", which a resume has no \
             way to replay, so nothing was performed. Deny the request and let the run \
             re-issue the action.",
            pending.target, pending.act
        ),
        rule: None,
        layer: None,
    }
}

/// Refuse a resumed `exec` or `net` approval that carries a rewrite (0.74.0).
///
/// That arm grants what was *persisted* and the model re-issues the call itself,
/// so a rewritten target has nowhere to take effect. Applying it would grant one
/// program and run another; dropping it silently would overrule an approver that
/// meant to narrow the grant without ever telling it. Neither is honest, so the
/// resume refuses and the pending stays open for a decision that can be carried
/// out.
fn unreplayable_rewrite(pending: &crate::state::Pending, rewrite: &Request) -> Error {
    Error::Refused {
        act: pending.act.clone(),
        target: format!(
            "{} — the approval rewrote it to {} ({}), and a resumed {} approval grants \
             exactly what was persisted, so nothing was performed. Approve it as asked, \
             or deny it and let the run re-issue the action.",
            pending.target,
            rewrite.target,
            act_word(rewrite.act),
            pending.act
        ),
        rule: None,
        layer: None,
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
/// *Exactly* is the whole claim, so a resume that cannot honour it refuses
/// instead of approximating it (0.74.0), leaving the request pending and
/// performing nothing:
///
/// - A pending row whose `act` is not one of the four this crate writes — the
///   column is reachable through the public
///   [`Store::put_pending`](crate::Store::put_pending) — is
///   [`Error::Refused`](crate::Error::Refused). It is not replayed as a write at
///   the target's name.
/// - A `modified` request on a pending `exec` or `net` is
///   [`Error::Refused`](crate::Error::Refused) too. Those two are resumed by
///   *granting* what was approved and letting the model re-issue the call, so a
///   rewritten target has nowhere to take effect; an approver is told rather than
///   left believing a narrowing applied. Rewriting a pending `read` or `write`
///   works as before.
///
/// Preserves the policy — it is an argument — and, since 0.13.0, the run's
/// observation ledger. It is for a run that *paused*, though: a run that crashed
/// has no `request_id` and no pending decision to supply, and wants
/// [`resume_with`].
///
/// This is the other half of [`Decision::Defer`], and it is what makes an
/// approval able to outlive the process that asked for it — a web app can show
/// the pending action, close the request, and continue the run when someone
/// clicks approve tomorrow:
///
/// ```no_run
/// use io_harness::{resume_with_decision, ApproveAll, Act, Decision, OpenRouter, Policy,
///                  Request, RunOutcome, Store, TaskContract};
///
/// # async fn on_click(contract: &TaskContract, policy: &Policy, request_id: i64, approved: bool)
/// #     -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let pending = store.pending(request_id)?.expect("a pending request");
///
/// let decision = if approved {
///     // Approving performs exactly what was persisted — the same target, the
///     // same bytes the human was shown. Hand back a `modified` request to
///     // perform something else instead; it is re-checked against the policy
///     // first, so an approver cannot rewrite an action across a deny.
///     Decision::Approve {
///         modified: Some(Request::new(Act::Write, "docs/NOTES.md")
///             .with_content(pending.content.clone().unwrap_or_default())),
///         remember: Vec::new(),
///     }
/// } else {
///     // The action never happens and the run closes as `RunOutcome::Denied`.
///     Decision::deny("rejected in review")
/// };
///
/// let result = resume_with_decision(
///     contract, &OpenRouter::from_env()?, &store, pending.run_id, request_id, decision,
///     policy, &ApproveAll,
/// )
/// .await?;
///
/// // Deferring again is legal and leaves it pending — the run stays paused.
/// if let RunOutcome::AwaitingApproval { .. } = result.outcome {
///     println!("still waiting");
/// }
/// # Ok(()) }
/// ```
///
/// For a paused *tree*, use [`resume_tree_with_decision`]: the pending action
/// often belongs to a child rather than the root, and only that function
/// validates the request against the whole tree.
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
///
/// The event worth listening for here is
/// [`EventKind::ApprovalDecided`](crate::EventKind::ApprovalDecided): it is the
/// audit record that the decision a human made was the decision the run
/// performed, which is the one claim an approval flow has to be able to prove.
///
/// ```no_run
/// use io_harness::{resume_with_decision_observed, ApproveAll, Decision, EventKind, Flow,
///                  Observer, OpenRouter, Policy, RunEvent, Store, TaskContract};
///
/// struct AuditTrail;
///
/// impl Observer for AuditTrail {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::ApprovalDecided { act, target, decision } = &event.kind {
///             println!("run {} step {}: {decision} {act} {target}", event.run_id, event.step);
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, run_id: i64, request_id: i64)
/// #     -> io_harness::Result<()> {
/// resume_with_decision_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, run_id, request_id,
///     Decision::approve(), policy, &ApproveAll, &AuditTrail,
/// )
/// .await?;
/// # Ok(()) }
/// ```
///
/// A re-check against the policy happens before the action, so a deny added
/// while the run was paused still wins — in which case the observer sees a
/// refusal rather than an approval, and the run closes as
/// [`RunOutcome::Denied`].
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

    // 0.33.0: a request a second process already answered is not resumable. Since
    // `Attach::answer_approval` can decide a *live* run, "there is a pending row"
    // stopped meaning "nobody has decided this" — and the row is now written before
    // the approver is consulted, so one exists even for approvals answered
    // instantly. Driving the run again from a decision the store did not record
    // would be the silent double-answer this release exists to prevent. The
    // compare-and-swap below is what makes it airtight; this is the readable error.
    if let Some(already) = &pending.resolved {
        return Err(crate::error::Error::Config(format!(
            "request {request_id} was already decided ({already})"
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
    emit_plugins(watch, run_id, contract);

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
        // A deferred *network* or *exec* action has no filesystem effect to
        // replay, and approving it grants the thing that was approved for the
        // rest of the run. Routing either through the write path below would
        // check a host or a program name against the path policy and then try to
        // create a file named after it.
        //
        // `exec` joins `net` here in 0.70.0, when `Effect::Ask` on `Act::Exec`
        // started reaching an approver instead of refusing. Without this arm,
        // approving a paused git built-in would write an empty file called `git`
        // into the workspace root and resume without ever running the command.
        // The grant matters for the same reason it does for a host: without it
        // the model re-issues the call and the approver is asked a second time
        // for what they have just allowed.
        Decision::Approve {
            ref modified,
            ref remember,
        } if pending.act == "net" || pending.act == "exec" => {
            // 0.74.0 — a rewrite has no consumer on this path. See
            // `unreplayable_rewrite`.
            if let Some(rewrite) = modified
                .as_ref()
                .filter(|m| m.target != pending.target || act_word(m.act) != pending.act)
            {
                return Err(unreplayable_rewrite(&pending, rewrite));
            }
            let granted = if pending.act == "net" {
                net::provider_layer(&pending.target)
            } else {
                Policy::permissive()
                    .layer("approved-exec")
                    .allow_exec(&pending.target)
            };
            let effective = policy
                .clone()
                .merge(granted)
                .merge(remembered_layer(remember));
            store.resolve_pending(request_id, "approve")?;
            store.record_event(
                run_id,
                &PolicyEvent::decision(
                    step,
                    &pending.act,
                    &pending.target,
                    "approve",
                    format!("resumed:{request_id}"),
                ),
            )?;
            let remember = remember.clone();
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let lsp = lsp_for(contract, &effective, store, run_id, watch).await?;
            let browser = browser_for(contract, &effective);
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
                &lsp,
                &browser,
                &skills,
                watch,
                &TurnExtras::default(),
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
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

            // The pause does not grant immunity: the policy still decides — and
            // what it decides about is the act that was persisted, not whatever a
            // filesystem check happens to accept. An act with no replay here is
            // refused before anything is claimed or written.
            let Some(act) = replayable_act(&pending.act) else {
                return Err(unreplayable(store, watch, run_id, run_id, step, &pending));
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

            // Claimed BEFORE the effect, not after: a swap that loses means a
            // second process decided this request while we were checking it, and a
            // file written on a decision that lost would be an effect nothing in
            // the store accounts for.
            if !store.resolve_pending(request_id, "approve")? {
                return Err(crate::error::Error::Config(format!(
                    "request {request_id} was decided by another process"
                )));
            }
            if act == Act::Write {
                ws.write_file(&target, content.as_deref().unwrap_or_default())?;
            }
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
            let lsp = lsp_for(contract, &effective, store, run_id, watch).await?;
            let browser = browser_for(contract, &effective);
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
                &lsp,
                &browser,
                &skills,
                watch,
                &TurnExtras::default(),
            )
            .await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
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
///
/// It refuses the same two resumes [`resume_with_decision`] refuses — a pending
/// act with no replay, and a `modified` request on a pending `exec` or `net` —
/// for the same reasons and with the same effect: nothing performed, the request
/// still pending, [`Error::Refused`](crate::Error::Refused) returned.
///
/// The trap this exists to avoid: the `run_id` you pass is the tree's **root**,
/// while `pending.run_id` is whichever agent actually asked — often three levels
/// down. Passing the child's id to [`resume_with_decision`] resumes that child
/// alone and orphans the tree around it.
///
/// ```no_run
/// use io_harness::{resume_tree_with_decision, ApproveAll, Containment, Decision, OpenRouter,
///                  Policy, RunOutcome, Store, TaskContract};
///
/// # async fn decide(contract: &TaskContract, policy: &Policy, paused: RunOutcome, root_run_id: i64)
/// #     -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let RunOutcome::AwaitingApproval { request_id, .. } = paused else { return Ok(()) };
///
/// // Show the human which agent in the tree is asking, not just what for.
/// let pending = store.pending(request_id)?.expect("a pending request");
/// println!("agent {} wants to {} {}", pending.run_id, pending.act, pending.target);
///
/// // The ROOT id, and the request id from anywhere in the tree. Containment is
/// // supplied again because the resumed tree draws against one continuous
/// // ceiling — it is restored from durable totals, never reset.
/// resume_tree_with_decision(
///     contract, &OpenRouter::from_env()?, &store, root_run_id, request_id,
///     Decision::approve(), policy, &ApproveAll, &Containment::new(8, 3, 2, 400_000),
/// )
/// .await?;
/// # Ok(()) }
/// ```
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
///
/// Which is what makes it usable here: after a tree-wide pause you want to see
/// the deferred action land and then watch the *right* agent carry on, out of
/// the several that resume at once.
///
/// ```no_run
/// use io_harness::{resume_tree_with_decision_observed, ApproveAll, Containment, Decision,
///                  EventKind, Flow, Observer, OpenRouter, Policy, RunEvent, Store, TaskContract};
///
/// /// Follows one agent out of a whole tree resuming around it.
/// struct FollowAgent { run_id: i64 }
///
/// impl Observer for FollowAgent {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if event.run_id != self.run_id {
///             return Flow::Continue; // some other agent in the tree
///         }
///         match &event.kind {
///             EventKind::ApprovalDecided { decision, target, .. } => {
///                 println!("resumed on: {decision} {target}");
///             }
///             EventKind::Step { decision, .. } => println!("  step {}: {decision}", event.step),
///             _ => {}
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, root_run_id: i64, request_id: i64)
/// #     -> io_harness::Result<()> {
/// let store = Store::open("runs.db")?;
/// let pending = store.pending(request_id)?.expect("a pending request");
/// let follow = FollowAgent { run_id: pending.run_id };
///
/// resume_tree_with_decision_observed(
///     contract, &OpenRouter::from_env()?, &store, root_run_id, request_id,
///     Decision::approve(), policy, &ApproveAll, &Containment::new(8, 3, 2, 400_000), &follow,
/// )
/// .await?;
/// # Ok(()) }
/// ```
///
/// A [`Flow::Cancel`](crate::Flow::Cancel) from any agent's event stops the whole
/// tree at the next boundary, not only the agent that emitted it — there is one
/// cancellation flag per tree, as there is one approver and one ledger.
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
    // 0.65.0 — refuse to re-drive a run whose journal still holds a call the
    // harness cannot inspect. Before anything drives, because the first thing a
    // resumed run does is replay the step that died, and by then the decision to
    // repeat the call has already been taken.
    if let Some(paused) = recovery_pause(store, run_id, observer)? {
        return Ok(RunResult::new(paused, run_id));
    }
    // The lease (0.62.0). Taken after the resumability checks so an unknown run
    // still reports as an unknown run, and before any step is driven so a second
    // live driver is refused rather than interleaving its steps with the first
    // one's. Released when this function returns, however it returns.
    let _lease = store.acquire_lease(run_id, contract.lease_ttl.as_secs() as i64)?;
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

    // 0.33.0: a request a second process already answered is not resumable. Since
    // `Attach::answer_approval` can decide a *live* run, "there is a pending row"
    // stopped meaning "nobody has decided this" — and the row is now written before
    // the approver is consulted, so one exists even for approvals answered
    // instantly. Driving the run again from a decision the store did not record
    // would be the silent double-answer this release exists to prevent. The
    // compare-and-swap below is what makes it airtight; this is the readable error.
    if let Some(already) = &pending.resolved {
        return Err(crate::error::Error::Config(format!(
            "request {request_id} was already decided ({already})"
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
    emit_plugins(watch, run_id, contract);

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
        // The tree twin of the arm in `resume_with_decision` — see the comment
        // there. `exec` joined `net` in 0.70.0 and for the same reason: a spawn
        // or a git built-in paused for approval must not come back as an empty
        // file named after the program.
        Decision::Approve {
            ref modified,
            ref remember,
        } if pending.act == "net" || pending.act == "exec" => {
            // 0.74.0 — as in `resume_with_decision`: a rewrite has no consumer on
            // this path. See `unreplayable_rewrite`.
            if let Some(rewrite) = modified
                .as_ref()
                .filter(|m| m.target != pending.target || act_word(m.act) != pending.act)
            {
                return Err(unreplayable_rewrite(&pending, rewrite));
            }
            let granted = if pending.act == "net" {
                net::provider_layer(&pending.target)
            } else {
                Policy::permissive()
                    .layer("approved-exec")
                    .allow_exec(&pending.target)
            };
            let effective = policy
                .clone()
                .merge(granted)
                .merge(remembered_layer(remember));
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
            let (ledger, backlog) = restore_tree_ledger(store, run_id, containment)?;
            let start_step = record_resume_markers(store, run_id)?;
            store.set_provider(run_id, provider.name())?;
            emit_backlog(
                watch,
                run_id,
                start_step.saturating_sub(1),
                &ledger,
                &backlog,
            );
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let lsp = lsp_for(contract, &effective, store, run_id, watch).await?;
            let browser = browser_for(contract, &effective);
            // Once for the whole tree, before the root agent runs. See `Tree::probe`.
            let probe =
                probe_tree_boundary(store, watch, run_id, &contract.exec_sandbox, &root).await;
            let tree = Tree {
                mcp: &mcp,
                lsp: &lsp,
                browser: &browser,
                tools: &contract.tools,
                skills: &skills,
                agents: &contract.agents,
                provider,
                store,
                approver,
                responder: responder_of(contract),
                watch,
                ledger,
                containment,
                probe,
                probed: contract.exec_sandbox.clone(),
                turn: None,
                root,
                root_run_id: run_id,
                web: contract.web.clone(),
                spawn_background_after: contract.spawn_background_after,
                detached_spawns: contract.detached_spawns,
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step, None).await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
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
            // 0.74.0 — the same total read of the persisted act the flat form
            // takes; a fix in one form and not the other is no fix.
            let Some(act) = replayable_act(&pending.act) else {
                return Err(unreplayable(
                    store,
                    watch,
                    run_id,
                    pending.run_id,
                    step,
                    &pending,
                ));
            };
            if ws.check_path(act, &target).effect == Effect::Deny {
                store.resolve_pending(request_id, "deny")?;
                finish(store, watch, run_id, 0, step, "denied")?;
                return Ok(RunResult::new(RunOutcome::Denied { steps: step }, run_id));
            }
            // Claimed BEFORE the effect, not after: a swap that loses means a
            // second process decided this request while we were checking it, and a
            // file written on a decision that lost would be an effect nothing in
            // the store accounts for.
            if !store.resolve_pending(request_id, "approve")? {
                return Err(crate::error::Error::Config(format!(
                    "request {request_id} was decided by another process"
                )));
            }
            if act == Act::Write {
                ws.write_file(&target, content.as_deref().unwrap_or_default())?;
            }
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
            let (ledger, backlog) = restore_tree_ledger(store, run_id, containment)?;
            let start_step = record_resume_markers(store, run_id)?;
            store.set_provider(run_id, provider.name())?;
            emit_backlog(
                watch,
                run_id,
                start_step.saturating_sub(1),
                &ledger,
                &backlog,
            );
            let mcp = McpSession::connect(&contract.mcp, &effective, store, run_id, watch).await?;
            let lsp = lsp_for(contract, &effective, store, run_id, watch).await?;
            let browser = browser_for(contract, &effective);
            // Once for the whole tree, before the root agent runs. See `Tree::probe`.
            let probe =
                probe_tree_boundary(store, watch, run_id, &contract.exec_sandbox, &root).await;
            let tree = Tree {
                mcp: &mcp,
                lsp: &lsp,
                browser: &browser,
                tools: &contract.tools,
                skills: &skills,
                agents: &contract.agents,
                provider,
                store,
                approver,
                responder: responder_of(contract),
                watch,
                ledger,
                containment,
                probe,
                probed: contract.exec_sandbox.clone(),
                turn: None,
                root,
                root_run_id: run_id,
                web: contract.web.clone(),
                spawn_background_after: contract.spawn_background_after,
                detached_spawns: contract.detached_spawns,
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step, None).await;
            mcp.shutdown(store, run_id, watch).await;
            lsp.shutdown().await;
            browser.shutdown().await;
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
/// Re-run a run's criterion, and nothing else (0.34.0).
///
/// The unit of retry is the gate rather than the run, because that is the unit
/// the failure had. A run that spent an hour and forty model calls and then lost
/// its review gate to a 529 has forty calls' worth of work sitting in the
/// workspace, and every way this crate had of getting a verdict on it started by
/// running the task again.
///
/// It re-evaluates [`TaskContract::verify`] against the workspace as it now
/// stands and appends the result to the run's gate attempts. **No step is
/// re-executed**, no tool is called, and the only provider call made is the one
/// the criterion itself needs — so a run's `steps` rows and its token ledger are
/// unchanged by a retry except for the review it asked for.
///
/// It refuses what it cannot honestly do. A gate that `Failed` was evaluated and
/// said no: nothing about the work has changed, so running it again is a way of
/// asking the same question until the answer is convenient. Only
/// [`GateOutcome::Errored`](crate::GateOutcome) — a criterion that never ran — is
/// retryable, and a run that never gated at all is refused too.
///
/// ```no_run
/// use io_harness::{retry_gate, GateOutcome, Store, TaskContract};
///
/// # async fn demo(contract: &TaskContract, store: &Store, run_id: i64) -> io_harness::Result<()> {
/// // The gate errored: the review never happened, so the work has not been judged.
/// assert!(store.last_gate_attempt(run_id)?.unwrap().outcome.is_retryable());
///
/// // One criterion, re-run. The forty steps that produced the work are not.
/// let passed = retry_gate(contract, store, run_id).await?;
/// assert!(matches!(passed, GateOutcome::Passed | GateOutcome::Failed));
/// # Ok(()) }
/// ```
pub async fn retry_gate(
    contract: &TaskContract,
    store: &Store,
    run_id: i64,
) -> Result<GateOutcome> {
    retry_gate_observed(contract, store, run_id, &Ignore).await
}

/// [`retry_gate`], reporting what it does to `observer` (0.34.0).
///
/// The verdict reaches an [`Observer`] as
/// [`EventKind::Reviewed`](crate::EventKind), so a retry is visible in the same
/// stream the run was — including to a process attached with
/// [`Attach`](crate::Attach), since 0.33.0's `Broadcast` carries whatever it is
/// wrapped around.
///
/// ```no_run
/// use io_harness::observe::{Flow, Observer, RunEvent};
/// use io_harness::{retry_gate_observed, Store, TaskContract};
///
/// struct Print;
/// impl Observer for Print {
///     fn event(&self, event: &RunEvent) -> Flow {
///         println!("{:?}", event.kind);
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, store: &Store, run_id: i64) -> io_harness::Result<()> {
/// let outcome = retry_gate_observed(contract, store, run_id, &Print).await?;
/// println!("the gate now says {}", outcome.as_str());
/// # Ok(()) }
/// ```
pub async fn retry_gate_observed(
    contract: &TaskContract,
    store: &Store,
    run_id: i64,
    observer: &dyn Observer,
) -> Result<GateOutcome> {
    let root = contract.root.as_deref().ok_or_else(|| {
        Error::Config("retry_gate needs a workspace contract; single-file runs have no root".into())
    })?;
    let last = store
        .last_gate_attempt(run_id)?
        .ok_or_else(|| Error::Resume {
            reason: format!("run {run_id} has no gate attempt to retry"),
        })?;
    if !last.outcome.is_retryable() {
        return Err(Error::Resume {
            reason: format!(
                "run {run_id}'s last gate attempt {} — the criterion ran and answered, so the work                  has to change before it can answer differently",
                last.outcome.as_str()
            ),
        });
    }

    let watch = Watch::new(observer);
    // The same evaluation the run itself does, at the same step, through the same
    // function — so a retry cannot drift into a second, laxer implementation of
    // the criterion. The guard is permissive-by-policy only in the sense every
    // gate's is: a `Command` criterion still checks its program against the
    // contract's policy through `ExecGuard`.
    let policy = Policy::default();
    let guard = crate::verify::ExecGuard::new(&policy).tracing(store, run_id, last.step);
    match evaluate_gate(contract, root, &guard, store, run_id, last.step, &watch, 0).await {
        Ok(true) => Ok(GateOutcome::Passed),
        Ok(false) => Ok(GateOutcome::Failed),
        // `evaluate_gate` has already recorded the `Errored` attempt; the error is
        // handed back so a caller can decide whether to wait and ask again.
        Err(e) => Err(e),
    }
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

    /// Ask for the run to stop, without an event to hang it off (0.42.0).
    ///
    /// What `on_failure = "cancel"` on a `before_tool` hook reaches. An event hook
    /// gets here through `emit`, because it is answering an event; a lifecycle
    /// hook is answering a call, and the refusal it produces is reported as a
    /// refusal rather than as a second event kind.
    pub(crate) fn cancel(&self) {
        self.cancelled.set(true);
    }

    /// Whether stopping has been asked for. Read at a step boundary only.
    fn cancelled(&self) -> bool {
        self.cancelled.get()
    }
}

/// How often a run parked on a gate looks to see whether a second process
/// answered for it (0.33.0).
///
/// The one number that decides how quickly an
/// [`Attach`](crate::Attach)-supplied answer reaches a live run. It is the
/// latency of a person's decision, not of a step, so a fifth of a second is far
/// below anything anyone notices while costing one indexed read per interval —
/// and only while a run is actually parked, because the poll never fires when
/// the in-process gate answers.
pub const ATTACH_POLL: Duration = Duration::from_millis(200);

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

/// Shared context for one agent tree: everything every agent in the tree
/// draws on — the provider, the store, the one approver, the shared spend
/// ledger, the containment caps, and the workspace root.
struct Tree<'a, P: Provider> {
    /// One MCP session for the whole tree. A server is a stateful process, so
    /// 100 concurrent agents get 100 views of one connection, not 100 of their
    /// own — the same reason the ledger and the store are shared here.
    mcp: &'a McpSession,
    /// One language-server session for the whole tree, for the reason `mcp` is
    /// shared: a server is a stateful process with one index, and a child agent
    /// asking where a symbol is defined is asking the same server its parent did.
    lsp: &'a LspSession,
    /// One browser for the whole tree, for the reason `mcp` and `lsp` are shared:
    /// it is a stateful process, and a child agent looking at a page is looking
    /// at the page its parent left open.
    browser: &'a BrowserSession,
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
    /// The roster a spawn may name (0.21.0). Shared tree-wide for the same reason
    /// `skills` is: a definition is the operator's, so a child cannot conjure one
    /// its parent's contract never registered.
    agents: &'a Agents,
    provider: &'a P,
    store: &'a Store,
    approver: &'a dyn Approver,
    /// Who answers a question about intent, for the whole tree (0.21.0) — one
    /// responder, exactly as there is one approver.
    responder: &'a dyn Responder,
    /// One observer for the whole tree, exactly as there is one approver: every
    /// event carries the agent's own `run_id` and `depth`, so a consumer routes on
    /// those rather than being handed an observer per child. It also carries the
    /// tree's single cancellation flag, so a `Flow::Cancel` from any agent's event
    /// stops the tree at the next boundary rather than only that agent.
    watch: &'a Watch<'a>,
    ledger: Arc<Ledger>,
    containment: &'a Containment,
    /// What this tree **measured** about the boundary its agents run under, taken
    /// once before the root agent starts (0.74.0).
    ///
    /// One containment, one measurement. Every agent in a tree shares its
    /// parent's workspace and its containment, so probing per agent would ask one
    /// question N times and pay N times the child processes to get N answers to
    /// it. Held here beside the ledger and the MCP session for the same reason
    /// those are — it belongs to the tree — and, like them, it dies with the tree:
    /// nothing static, nothing that survives into a second tree or a second run.
    probe: crate::sandbox::BoundaryProbe,
    /// The config `probe` was measured under, so it is never read for a boundary
    /// it did not measure.
    ///
    /// A spawned child's contract carries its own `exec_sandbox`. Where it is the
    /// root's, it is the same boundary and the same measurement; where it differs,
    /// that agent measures its own. This field is what makes the difference a
    /// comparison rather than an assumption.
    probed: SandboxConfig,
    /// What a session turn adds to this tree's ROOT, and nothing else (0.39.0).
    ///
    /// `None` for every `run_tree`/`resume_tree` entry point, which is what keeps
    /// them exactly as they were. `Some` only when
    /// [`crate::Session::turn_contained`] drove the tree, and even then the root
    /// is the only agent that reads it: `Tree::extras` hands a child the empty
    /// set, so a child is never seeded with the conversation, never classified as
    /// a reply, and never steerable by an operator it has not spoken to.
    turn: Option<&'a TurnExtras<'a>>,
    root: PathBuf,
    /// The tree root's run id, so `Containment::max_total_duration` can be
    /// measured against when the TREE started rather than when this agent did.
    /// A child spawned twenty hours into a run has its own young `started_at`;
    /// the ceiling is about the whole tree, so the root's stamp is the only
    /// correct clock. Held here because [`Ledger`] has no store access.
    root_run_id: i64,
    /// The ROOT contract's web declaration (0.22.0), shared tree-wide for exactly
    /// the reason `tools`, `skills` and `agents` are: it is the operator's, and a
    /// child contract the *model* writes must not be able to conjure web access
    /// its parent was never given — nor to lose the one its parent had, which
    /// would leave a sub-agent answering from memory on a task that needs the
    /// current answer.
    web: Option<crate::web::WebAccess>,
    /// The ROOT contract's answer to how long a parent waits for a child, and
    /// whether a child may outlive the step that spawned it (0.50.0).
    ///
    /// Tree-wide and read from the root for exactly the reason `web` is: a child
    /// contract the *model* writes must not be able to buy its own children more
    /// patience, or the right to leave one running, than the operator allowed.
    spawn_background_after: Option<Duration>,
    detached_spawns: bool,
}

/// What an agent that is not a session turn's root runs with: nothing.
///
/// A `const` rather than `TurnExtras::default()` so it can be borrowed for as
/// long as the tree lives — a value built inside the loop would not outlive the
/// recursion into a child.
const NO_EXTRAS: TurnExtras<'static> = TurnExtras {
    seed: &[],
    steer: None,
    turn: None,
    stream: false,
    classify: false,
};

/// The session extras an agent at `depth` runs under — the turn's at the root,
/// and nothing below it.
///
/// A free function rather than a method (0.69.0) so the rule can be asserted
/// without a live tree, which needs a provider, a store and a boundary. The
/// assertion matters more since the operator's fold arrives through the inbox
/// this decides a child does not get: "a child is work, not conversation" is what
/// stops a fold asked for at the root from folding a child's own ledger, and a
/// test that restated the match instead of calling it would go on passing while
/// the match said something else.
fn extras_for<'a>(turn: Option<&'a TurnExtras<'a>>, depth: u32) -> &'a TurnExtras<'a> {
    match depth {
        0 => turn.unwrap_or(&NO_EXTRAS),
        _ => &NO_EXTRAS,
    }
}

impl<P: Provider> Tree<'_, P> {
    /// The session extras this agent runs under — the turn's at the root, and
    /// nothing at any other depth.
    ///
    /// The depth test lives in [`extras_for`] rather than at each of the four use
    /// sites, so "a child is work, not conversation" is one rule that cannot hold
    /// at three of them and lapse at the fourth.
    fn extras(&self, depth: u32) -> &TurnExtras<'_> {
        extras_for(self.turn, depth)
    }
}

/// The longest an agent may block on its mailbox when the contract names no
/// ceiling (0.60.0).
///
/// A number rather than "forever", and the absence of "forever" is the design.
/// An agent that blocks holds its concurrency slot, and the sibling that would
/// answer it may be the one queued behind that slot — so an unbounded wait turns a
/// tree that would have carried on into a tree that stops. Thirty seconds is long
/// enough that a sibling doing real work can answer within it and short enough
/// that a whole fan-out cannot be lost to one agent's patience.
///
/// ```
/// use io_harness::{TaskContract, DEFAULT_MAX_WAIT};
///
/// // What an agent may ask for when the contract names no ceiling of its own.
/// assert_eq!(DEFAULT_MAX_WAIT.as_secs(), 30);
/// assert_eq!(
///     TaskContract::workspace("coordinate the fan-out", "/repo").max_wait_secs,
///     None,
///     "unset means this constant, never `forever`",
/// );
///
/// // An operator who wants a tighter one says so, and a project-scoped
/// // `io.toml` may lower it further and may not raise it.
/// let bounded = TaskContract::workspace("coordinate the fan-out", "/repo")
///     .with_max_wait_secs(5);
/// assert!(bounded.max_wait_secs.unwrap() < DEFAULT_MAX_WAIT.as_secs());
/// ```
pub const DEFAULT_MAX_WAIT: Duration = Duration::from_secs(30);

/// How long a run's lease outlives its last durable step before another process
/// may take the run over (0.62.0).
///
/// **Derived from this crate's own defaults rather than picked.** The lease is
/// renewed by every step commit, so what it has to outlast is one step: one
/// provider completion, plus at most one tool execution — and the crate already
/// says how long a tool execution may be, at
/// [`DEFAULT_EXEC_TIMEOUT`](crate::DEFAULT_EXEC_TIMEOUT), which is 900 seconds.
/// Twice that leaves the same margin again for the completion that asked for the
/// command and for a retry behind it.
///
/// **A killed process does not cost this wait where liveness can be checked.** A
/// lease whose owner no longer exists is takeable immediately, so `kill -9` and
/// resume stays immediate; this bounds the cases that cannot be checked — a
/// recycled pid and an owner id with no readable pid. It is a lease with a ttl
/// rather than a lock for that residue: a lock a dead process holds is an outage
/// with no recovery at all.
///
/// ```
/// use io_harness::{TaskContract, Verification, DEFAULT_EXEC_TIMEOUT, DEFAULT_LEASE_TTL};
///
/// // Long enough for one step: a command that runs to the exec ceiling, and the
/// // completion that asked for it.
/// assert_eq!(DEFAULT_LEASE_TTL, DEFAULT_EXEC_TIMEOUT * 2);
/// let contract = TaskContract::new("tidy the notes", "NOTES.md", Verification::None);
/// assert_eq!(contract.lease_ttl, DEFAULT_LEASE_TTL);
///
/// // An operator who wants the un-checkable cases to resolve sooner says so.
/// let brisk = contract.with_lease_ttl(std::time::Duration::from_secs(60));
/// assert!(brisk.lease_ttl < DEFAULT_LEASE_TTL);
/// ```
pub const DEFAULT_LEASE_TTL: Duration = Duration::from_secs(1_800);

/// The tool an agent uses to tell another agent in its tree something (0.60.0).
///
/// Named like [`SPAWN_TOOL`] and for the same reason: an [`Observer`] matching on
/// [`EventKind::ToolCall`] needs the string, and a literal typed into an
/// embedder's match arm is one that goes stale silently.
///
/// ```
/// use io_harness::{EventKind, Flow, Observer, RunEvent, SEND_MESSAGE_TOOL};
///
/// // An observer that counts what one agent told another, without matching on a
/// // string literal of its own.
/// struct Chatter(std::sync::atomic::AtomicUsize);
/// impl Observer for Chatter {
///     fn event(&self, e: &RunEvent) -> Flow {
///         if let EventKind::ToolCall { name, .. } = &e.kind {
///             if name == SEND_MESSAGE_TOOL {
///                 self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
///             }
///         }
///         Flow::Continue
///     }
/// }
///
/// assert_eq!(SEND_MESSAGE_TOOL, "send_message");
/// ```
pub const SEND_MESSAGE_TOOL: &str = "send_message";

/// The tool an agent uses to read what other agents have sent it (0.60.0).
///
/// ```
/// use io_harness::{READ_MESSAGES_TOOL, SEND_MESSAGE_TOOL};
///
/// // The pair. Both are offered inside a tree and in neither `run` nor
/// // `run_with`, which never expose a tool that needs somebody to talk to.
/// assert_eq!(READ_MESSAGES_TOOL, "read_messages");
/// assert_ne!(READ_MESSAGES_TOOL, SEND_MESSAGE_TOOL);
/// ```
pub const READ_MESSAGES_TOOL: &str = "read_messages";

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
///
/// Reach for it when a task decomposes into parts that do not have to share one
/// agent's context — and note that the [`Containment`], not the contract, is
/// what actually bounds the result:
///
/// ```no_run
/// use io_harness::{run_tree, Containment, OpenRouter, Policy, StdinApprover, Store,
///                  TaskContract, Verification};
/// use std::time::Duration;
///
/// # async fn demo() -> io_harness::Result<()> {
/// let contract = TaskContract::workspace(
///     "document every public module under docs/, one file per module",
///     "/path/to/repo",
/// )
///     .with_verification(Verification::WorkspaceFileContains {
///         file: "docs/index.md".into(),
///         needle: "##".into(),
///     });
///
/// // The root's boundary, and therefore the ceiling for the entire tree: a child
/// // inherits it through `Policy::contain` and may only narrow it, so no
/// // descendant at any depth can write outside docs/ however its goal is worded.
/// let policy = Policy::default()
///     .layer("app")
///     .allow_read("*")
///     .allow_write("docs/*");
///
/// // The spend ceiling belongs here rather than on the contract, because a
/// // spawned child's contract is written by the *model* — anything it could set
/// // is something it could raise.
/// let containment = Containment {
///     max_total_agents: 12,
///     max_concurrent_agents: 4,   // four working at once; the fifth queues
///     max_depth: 2,
///     max_total_tokens: 500_000, // drawn down by the whole tree together
///     max_total_cost: None,      // reserved and inert; bound money in tokens
///     max_total_duration: Some(Duration::from_secs(3600)),
/// };
///
/// let result = run_tree(
///     &contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, &policy,
///     &StdinApprover, &containment,
/// )
/// .await?;
/// # Ok(()) }
/// ```
///
/// Every spawn, refusal and budget draw is recorded against the tree, so
/// `Store::agent_events` reconstructs who spawned whom and what each drew long
/// after the process exited.
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
///
/// A tree is where an observer stops being a nicety. Children run concurrently
/// and their output interleaves, so `depth` and `run_id` are what turn a stream
/// of events back into a shape a person can read:
///
/// ```no_run
/// use io_harness::{run_tree_observed, Containment, EventKind, Flow, Observer, OpenRouter,
///                  Policy, RunEvent, StdinApprover, Store, TaskContract};
///
/// /// Indents by depth, so concurrent children are legible rather than interleaved.
/// struct TreeLog;
///
/// impl Observer for TreeLog {
///     fn event(&self, event: &RunEvent) -> Flow {
///         let pad = "  ".repeat(event.depth as usize);
///         match &event.kind {
///             EventKind::Spawned { child_run_id, goal } => {
///                 println!("{pad}+ run {child_run_id}: {goal}");
///             }
///             // Containment refused the spawn — the tree hit `max_total_agents`,
///             // `max_depth`, or its spend ceiling. Never the concurrency cap: that
///             // one queues the child. The parent adapts; nothing fails.
///             EventKind::SpawnRefused { cap } => println!("{pad}! spawn refused: {cap} cap"),
///             // The tier's shape after that spawn: how many are working, how many
///             // are still waiting for a slot, how many are done.
///             EventKind::Fleet { tier, working, queued, done } => {
///                 println!("{pad}  tier {tier}: {working} working, {queued} queued, {done} done");
///             }
///             // What the tree has left of its ONE shared ceiling, after this draw.
///             EventKind::SpendDraw { remaining, .. } => {
///                 println!("{pad}  budget left: {remaining:?}");
///             }
///             EventKind::Step { decision, .. } => println!("{pad}  {decision}"),
///             _ => {}
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy) -> io_harness::Result<()> {
/// run_tree_observed(
///     contract, &OpenRouter::from_env()?, &Store::memory()?, policy, &StdinApprover,
///     &Containment::new(12, 4, 2, 500_000), &TreeLog,
/// )
/// .await?;
/// # Ok(()) }
/// ```
///
/// Events arrive on the run's own task and children share it, so a slow observer
/// slows every agent in the tree, not just one.
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
    run_tree_with_extras(
        contract,
        provider,
        store,
        policy,
        approver,
        containment,
        observer,
        &NO_EXTRAS,
    )
    .await
}

/// [`run_tree_observed`] with the session layer's extras, applied to the root
/// agent only (0.39.0).
///
/// Crate-internal, and the exact tree-side counterpart of [`run_with_extras`]:
/// the public entry points above drive it with [`NO_EXTRAS`] and therefore
/// behave exactly as they did, while [`crate::Session::turn_contained`] passes a
/// turn's seed, steer inbox, classification flag and streaming choice through to
/// the one agent that is a conversation — the root.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_tree_with_extras<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    policy: &Policy,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
    extras: &TurnExtras<'_>,
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
    // As in `run_with_extras`. One lease, on the root: a tree is driven by one
    // process by construction, and a second process is refused at the root before
    // it can reach any child.
    let _lease = store.acquire_lease(run_id, contract.lease_ttl.as_secs() as i64)?;
    // A session turn joins the tree here, in the same order and for the same
    // reason `run_with_extras` does it: after the run exists, before the first
    // completion is billed. A turn row with no run to point at would be a
    // conversation entry nothing can explain.
    if let Some(turn) = &extras.turn {
        store.record_turn(turn.session_id, turn.parent_turn_id, run_id, turn.prompt)?;
    }
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
    emit_plugins(watch, run_id, contract);
    // Authorized once at the root. Children inherit the root's policy through
    // `Policy::contain`, so the provider layer flows down the tree and no child
    // needs (or gets) its own chance to widen network access.
    // 0.42.0 — a tree does not come through `preflight_review_and_routing`, and an
    // unattended tree is where a model answering for its own call would do the
    // most damage. Same helper, same refusal, before anything is billed.
    refuse_self_approval(contract, provider, approver)?;

    let policy = &match authorize_provider(
        provider,
        policy,
        store,
        run_id,
        approver,
        watch,
        &contract.goal,
    )
    .await?
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
    let lsp = lsp_for(contract, policy, store, run_id, watch).await?;
    let browser = browser_for(contract, policy);
    // Once for the whole tree, before the root agent runs. See `Tree::probe`.
    let probe = probe_tree_boundary(store, watch, run_id, &contract.exec_sandbox, &root).await;
    let tree = Tree {
        mcp: &mcp,
        lsp: &lsp,
        browser: &browser,
        tools: &contract.tools,
        skills: &skills,
        agents: &contract.agents,
        provider,
        store,
        approver,
        responder: responder_of(contract),
        watch,
        ledger,
        containment,
        probe,
        probed: contract.exec_sandbox.clone(),
        turn: Some(extras),
        root,
        root_run_id: run_id,
        web: contract.web.clone(),
        spawn_background_after: contract.spawn_background_after,
        detached_spawns: contract.detached_spawns,
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, 1, None).await;
    mcp.shutdown(store, run_id, watch).await;
    lsp.shutdown().await;
    browser.shutdown().await;
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
///
/// One call, whole tree — you never resume a child yourself, and the containment
/// you pass is what the restored ledger is measured against:
///
/// ```no_run
/// use io_harness::{resume_tree, Containment, OpenRouter, Policy, StdinApprover, Store,
///                  TaskContract};
///
/// # async fn after_crash(contract: &TaskContract, policy: &Policy, root_run_id: i64)
/// #     -> io_harness::Result<()> {
/// // The SAME ceiling the tree started under. The ledger is rebuilt from the
/// // tree's durable total spend, so a tree that had already used 400k of 500k
/// // resumes with 100k left — pass a fresh, larger number and you have raised the
/// // ceiling, not restored it.
/// let containment = Containment::new(12, 4, 2, 500_000);
///
/// // The root's run id. Children are re-adopted from the store as the root
/// // replays its crashed step, each continuing from its own checkpoint.
/// let result = resume_tree(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, root_run_id, policy,
///     &StdinApprover, &containment,
/// )
/// .await?;
/// println!("{:?}", result.outcome);
/// # Ok(()) }
/// ```
///
/// One caveat worth knowing before you choose this over
/// [`resume_tree_from_stored_policy`]: `run_tree` and the two flat loops record
/// the caller's policy against the run, and this function does not — so a tree
/// resumed here under a widened policy leaves an audit that understates what was
/// permitted.
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
///
/// What this shows that a fresh [`run_tree_observed`] cannot: how much of the
/// tree was already done. Adopted children emit nothing for the steps they
/// committed before the crash, so the events that do arrive are exactly the work
/// this process is driving.
///
/// ```no_run
/// use io_harness::{resume_tree_observed, ApproveAll, Containment, EventKind, Flow, Observer,
///                  OpenRouter, Policy, RunEvent, Store, TaskContract};
/// use std::collections::BTreeSet;
/// use std::sync::Mutex;
///
/// /// Which agents in the tree still had work left after the restart.
/// #[derive(Default)]
/// struct StillWorking(Mutex<BTreeSet<i64>>);
///
/// impl Observer for StillWorking {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if matches!(event.kind, EventKind::Step { .. }) {
///             self.0.lock().unwrap().insert(event.run_id);
///         }
///         Flow::Continue
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, policy: &Policy, root_run_id: i64)
/// #     -> io_harness::Result<()> {
/// let working = StillWorking::default();
/// resume_tree_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, root_run_id, policy,
///     &ApproveAll, &Containment::new(12, 4, 2, 500_000), &working,
/// )
/// .await?;
/// println!("{:?} had steps left", working.0.lock().unwrap());
/// # Ok(()) }
/// ```
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
    // 0.65.0 — refuse to re-drive a run whose journal still holds a call the
    // harness cannot inspect. Before anything drives, because the first thing a
    // resumed run does is replay the step that died, and by then the decision to
    // repeat the call has already been taken.
    if let Some(paused) = recovery_pause(store, run_id, observer)? {
        return Ok(RunResult::new(paused, run_id));
    }
    // The lease (0.62.0). Taken after the resumability checks so an unknown run
    // still reports as an unknown run, and before any step is driven so a second
    // live driver is refused rather than interleaving its steps with the first
    // one's. Released when this function returns, however it returns.
    let _lease = store.acquire_lease(run_id, contract.lease_ttl.as_secs() as i64)?;

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

    let (ledger, backlog) = restore_tree_ledger(store, run_id, containment)?;
    let start_step = record_resume_markers(store, run_id)?;
    store.set_provider(run_id, provider.name())?;
    // The policy this resume runs under, recorded against the run — as
    // `run_tree_observed` has always done and this function, until 0.32.0, never
    // did. A tree resumed in a second process left no record of the boundary it
    // was actually resumed under, so an audit of a crashed-and-continued run could
    // read only the boundary of the process that died.
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
    emit_plugins(watch, run_id, contract);
    emit_backlog(
        watch,
        run_id,
        start_step.saturating_sub(1),
        &ledger,
        &backlog,
    );
    // Re-authorized on resume rather than trusted from the crashed run: the
    // policy handed to the resume is the one that governs it, and a host allowed
    // before a crash may not be allowed after.
    let policy = &match authorize_provider(
        provider,
        policy,
        store,
        run_id,
        approver,
        watch,
        &contract.goal,
    )
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
    let lsp = lsp_for(contract, policy, store, run_id, watch).await?;
    let browser = browser_for(contract, policy);
    // Once for the whole tree, before the root agent runs. See `Tree::probe`.
    let probe = probe_tree_boundary(store, watch, run_id, &contract.exec_sandbox, &root).await;
    let tree = Tree {
        mcp: &mcp,
        lsp: &lsp,
        browser: &browser,
        tools: &contract.tools,
        skills: &skills,
        agents: &contract.agents,
        provider,
        store,
        approver,
        responder: responder_of(contract),
        watch,
        ledger,
        containment,
        probe,
        probed: contract.exec_sandbox.clone(),
        turn: None,
        root,
        root_run_id: run_id,
        web: contract.web.clone(),
        spawn_background_after: contract.spawn_background_after,
        detached_spawns: contract.detached_spawns,
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, start_step, None).await;
    mcp.shutdown(store, run_id, watch).await;
    lsp.shutdown().await;
    browser.shutdown().await;
    Ok(RunResult::new(outcome?, run_id))
}

/// Resume a crashed agent tree under the policy it was started with, read back
/// from the store — [`resume_from_stored_policy`] for the 0.5.0 tree loop.
///
/// [`resume_tree`] takes a policy, so a tree's boundary was never at risk of
/// being silently dropped the way the flat workspace loop's was. But the caller
/// still had to have one to hand, and a process that comes up after a crash in
/// another process may have nothing to reconstruct it from. The policy has been
/// durable since 0.13.0 and the single-file and workspace loops have resumed from
/// it since then; the tree loop had no such entry point until 0.16.0, so the
/// three resume paths disagreed about whether a restart preserves the boundary —
/// a contradiction a release documenting the public contract would otherwise have
/// had to write down as a caveat.
///
/// Fails with [`Error::Resume`] when the store holds no policy for the run,
/// rather than substituting a permissive one. That substitution is the exact
/// defect 0.13.0 closed in the other two loops, and it is sharper for a tree:
/// every child inherits the root's policy through [`Policy::contain`], so a
/// guessed-at root boundary is guessed at for the whole tree, which may already
/// have taken an irreversible action under the real one.
///
/// Prefer it to [`resume_tree`] whenever the boundary matters, and not only
/// because you might get the policy wrong: it is also the only tree resume that
/// leaves an accurate audit. `resume_tree` does not call `record_run_policy`, so
/// a tree resumed there under a different policy keeps reporting the one it
/// started with; this one reads that row back rather than writing over it.
///
/// ```no_run
/// use io_harness::{resume_tree_from_stored_policy, Containment, DenyAll, Error, OpenRouter,
///                  Store, TaskContract};
///
/// # async fn supervisor(contract: &TaskContract, root_run_id: i64) -> io_harness::Result<()> {
/// // No policy argument. A process that comes up after a crash in another
/// // process has nothing to reconstruct one from, and guessing here would guess
/// // for every agent in the tree at once.
/// let resumed = resume_tree_from_stored_policy(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, root_run_id, &DenyAll,
///     &Containment::new(12, 4, 2, 500_000),
/// )
/// .await;
///
/// match resumed {
///     Ok(result) => println!("{:?}", result.outcome),
///     // No recorded policy — a tree checkpointed by 0.12.0 or earlier. It stays
///     // stopped rather than resuming unbounded; a human names the boundary and
///     // uses `resume_tree`.
///     Err(Error::Resume { reason }) => eprintln!("cannot recover the boundary: {reason}"),
///     Err(e) => return Err(e),
/// }
/// # Ok(()) }
/// ```
pub async fn resume_tree_from_stored_policy<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
    containment: &Containment,
) -> Result<RunResult> {
    resume_tree_from_stored_policy_observed(
        contract,
        provider,
        store,
        run_id,
        approver,
        containment,
        &Ignore,
    )
    .await
}

/// [`resume_tree_from_stored_policy`], reporting to `observer` as it happens. See
/// [`run_observed`].
///
/// The combination an unattended supervisor actually wants: recover the boundary
/// from the store, resume the whole tree, and keep a live handle on it — because
/// a tree resumed by a process nobody is watching should still be stoppable.
///
/// ```no_run
/// use io_harness::{resume_tree_from_stored_policy_observed, Containment, DenyAll, EventKind,
///                  Flow, Observer, OpenRouter, RunEvent, Store, TaskContract};
/// use std::sync::atomic::{AtomicBool, Ordering};
///
/// /// Logs the recovered boundary doing its job, and stops the tree on request.
/// struct Supervised { stop: AtomicBool }
///
/// impl Observer for Supervised {
///     fn event(&self, event: &RunEvent) -> Flow {
///         if let EventKind::Refused { act, target, layer, .. } = &event.kind {
///             println!("agent {} refused {act} {target} ({})",
///                      event.run_id, layer.as_deref().unwrap_or("tier default"));
///         }
///         // One flag for the whole tree: cancelling from any agent's event stops
///         // every agent at its next step boundary, and the tree stays resumable.
///         if self.stop.load(Ordering::Relaxed) { Flow::Cancel } else { Flow::Continue }
///     }
/// }
///
/// # async fn demo(contract: &TaskContract, root_run_id: i64) -> io_harness::Result<()> {
/// let supervised = Supervised { stop: AtomicBool::new(false) };
/// resume_tree_from_stored_policy_observed(
///     contract, &OpenRouter::from_env()?, &Store::open("runs.db")?, root_run_id, &DenyAll,
///     &Containment::new(12, 4, 2, 500_000), &supervised,
/// )
/// .await?;
/// # Ok(()) }
/// ```
#[allow(clippy::too_many_arguments)]
pub async fn resume_tree_from_stored_policy_observed<P: Provider>(
    contract: &TaskContract,
    provider: &P,
    store: &Store,
    run_id: i64,
    approver: &dyn Approver,
    containment: &Containment,
    observer: &dyn Observer,
) -> Result<RunResult> {
    let Some(policy) = store.run_policy(run_id)? else {
        return Err(Error::Resume {
            reason: format!(
                "tree {run_id} has no recorded policy, so the boundary it ran under cannot be \
                 recovered; pass one explicitly with `resume_tree` if you know what it was"
            ),
        });
    };
    resume_tree_observed(
        contract,
        provider,
        store,
        run_id,
        &policy,
        approver,
        containment,
        observer,
    )
    .await
}

impl<'a> Speculation<'a> {
    #[allow(clippy::too_many_arguments)]
    fn new(
        ws: Workspace,
        tools: &'a Toolbox,
        sandbox: Option<std::sync::Arc<crate::sandbox::ExecContainment>>,
        cap: usize,
        max_read: Option<usize>,
        max_parallel: usize,
        run_id: i64,
        step: u32,
    ) -> Self {
        Self {
            ws,
            tools,
            sandbox,
            cap,
            max_read,
            max_parallel,
            run_id,
            step,
            started: Vec::new(),
            set: tokio::task::JoinSet::new(),
            closed: false,
            done: std::collections::HashMap::new(),
            started_total: 0,
            used_total: 0,
        }
    }

    /// Begin a fresh attempt, dropping everything the previous one started.
    ///
    /// A completion that failed is not the completion the next attempt will
    /// return, so nothing speculated against it may be carried across. Replacing
    /// the [`JoinSet`](tokio::task::JoinSet) aborts its children as it drops it,
    /// which is the same guarantee `read_batch` relies on.
    fn reset(&mut self) {
        self.set = tokio::task::JoinSet::new();
        self.started.clear();
        self.done.clear();
        self.closed = false;
    }

    /// Offer the call the provider has finished streaming at position `at`.
    fn offer(&mut self, at: usize, call: &ToolCall) {
        if self.closed {
            return;
        }
        // Strictly the leading run and strictly in order: a report that skips a
        // position closes speculation rather than guessing what fills the gap,
        // and a cap already full closes it rather than queueing work whose whole
        // value was starting early.
        if at != self.started.len()
            || self.started.len() >= self.max_parallel
            || tool_effect(&call.name, self.tools) != ToolEffect::ReadOnly
        {
            self.closed = true;
            return;
        }
        // What this call may do is decided before it starts, exactly as `dispatch`
        // decides it: a tool needing more containment than the run grants leaves
        // with nothing started. Speculation skipping this would start it — and
        // start it before the completion that asked for it had settled.
        if matches!(
            resolve_call_mode(&call.name, self.tools, self.sandbox.as_ref()),
            CallMode::Refused { .. }
        ) {
            self.closed = true;
            return;
        }
        let Some(work) = speculable(&self.ws, call, self.tools) else {
            self.closed = true;
            return;
        };
        let ws = self.ws.clone();
        let (cap, max_read, run_id, step) = (self.cap, self.max_read, self.run_id, self.step);
        self.set
            .spawn(async move { (at, work.run(&ws, cap, max_read, run_id, step).await) });
        self.started.push((at, call.clone()));
        self.started_total += 1;
    }

    /// Collect what was started, and keep only what the settled completion asked
    /// for.
    ///
    /// The match is on the whole call at that position — same name, same
    /// arguments — and not on the position alone. A model that streamed one path
    /// and settled on another would otherwise have a different file's bytes
    /// folded under its call, and nothing downstream could tell: the observation
    /// has the shape of a successful read either way.
    async fn settle(&mut self, response: &CompletionResponse) -> Result<()> {
        while let Some(joined) = self.set.join_next().await {
            match joined {
                Ok((at, done)) => {
                    self.done.insert(at, done);
                }
                // As `read_batch`: a tool that panics panicked before this
                // release too, and the run died with it.
                Err(e) if e.is_panic() => std::panic::resume_unwind(e.into_panic()),
                Err(e) => {
                    return Err(Error::Config(format!(
                        "a speculated read-only tool call was cancelled: {e}"
                    )))
                }
            }
        }
        // Kept as a PREFIX, not as a set. The first call the settled completion
        // disagrees with ends it, and everything after that is discarded even
        // where it happens to match — because what survives here has to be a
        // contiguous run from position zero for the fold to stay simple: the
        // batch that forms for the first unspeculated call must not overlap a
        // later speculated one, or one call's result is folded under another's.
        let settled = &response.tool_calls;
        let mut keep = std::collections::HashMap::new();
        for (at, call) in &self.started {
            if settled.get(*at) != Some(call) {
                break;
            }
            match self.done.remove(at) {
                Some(done) => keep.insert(*at, done),
                None => break,
            };
        }
        self.done = keep;
        self.used_total += self.done.len();
        Ok(())
    }

    /// Whether the call at `at` already has a result waiting.
    fn has(&self, at: usize) -> bool {
        self.done.contains_key(&at)
    }

    /// The result already computed for the call at `at`, if there is one.
    fn take(&mut self, at: usize) -> Option<Dispatched> {
        self.done.remove(&at)
    }

    /// Started, used, discarded — across every attempt of this step.
    fn counts(&self) -> (usize, usize, usize) {
        (
            self.started_total,
            self.used_total,
            self.started_total - self.used_total,
        )
    }
}

/// Images the agent looked at this step, waiting to be attached to the next
/// request.
///
/// An alias rather than a `cfg` on the parameter itself, so the two call sites
/// and the signature read the same in both feature states. Without the feature
/// it is `()`: there is nothing to carry and nothing to bound.
#[cfg(feature = "media")]
pub(crate) type PendingMedia = Vec<crate::provider::Media>;
/// See the `media` form above.
#[cfg(not(feature = "media"))]
pub(crate) type PendingMedia = ();

#[cfg(test)]
mod tests {
    use super::*;

    /// F5 (0.68.0) — `fold_forced` decides three things, and the depth one is
    /// asserted here because no fixture can reach it.
    ///
    /// A spawned child gets a **fresh** `TaskContract::workspace(goal, root)`
    /// carrying only `web` and `max_steps`, so `fold_now` is already `false` at
    /// depth 1 before the gate is consulted. The end-to-end criterion is
    /// therefore over-determined: `tests/session_fanout.rs` proves a child does
    /// not fold on the root's request, and would go on proving it with the gate
    /// deleted. That is a branch nothing would fail on, which is the same thing
    /// as a rule nobody wrote.
    ///
    /// So it is asserted where the rule lives. The gate is kept rather than
    /// dropped as redundant because it is the lock that decides the question the
    /// day a child inherits its parent's contract — inheriting a compaction
    /// setting or a step cap is a plausible next release, and `fold_now` is the
    /// field that must not ride along.
    #[test]
    fn a_requested_fold_is_consumed_once_and_never_by_a_child() {
        // The root: honoured once, and gone afterwards. Read instead of taken
        // and every step of the run would fold.
        let mut asked = true;
        assert!(fold_forced(false, 0, &mut asked));
        assert!(!asked, "the request must be consumed, not merely read");
        assert!(!fold_forced(false, 0, &mut asked));

        // A child: never, whatever it was asked. This is the assertion the
        // fan-out test cannot make today.
        let mut asked = true;
        assert!(
            !fold_forced(false, 1, &mut asked),
            "a child folded on a request that was never made of it"
        );

        // An overflow recovery forces a fold at any depth — it is the vendor
        // refusing the request, not the operator asking.
        let mut none = false;
        assert!(fold_forced(true, 3, &mut none));

        // And a recovery at the root consumes the caller's request too: it has
        // already folded, and the caller who asked for one fold is not owed a
        // second at the next step.
        let mut asked = true;
        assert!(fold_forced(true, 0, &mut asked));
        assert!(
            !asked,
            "a recovery that folded left the caller's request outstanding"
        );
    }

    /// F6 (0.69.0) — the operator's fold reaches the root and no child, and the
    /// reason is that a child has no inbox to read it from.
    ///
    /// Asserted here for the reason the criterion above is: end to end the rule is
    /// over-determined twice over — a child's contract is fresh, *and* the extras
    /// it runs under are `NO_EXTRAS` — so a fan-out fixture would go on passing
    /// with either lock removed. What the fan-out test proves is that the root
    /// still folds while children are in flight, which is reachability rather than
    /// the rule.
    ///
    /// The two locks are not redundant with each other. `fold_forced`'s depth gate
    /// answers "may this agent fold on a request", and this one answers "can this
    /// agent hear a request at all" — the day a child inherits its parent's
    /// contract, only the first still holds, and the day a child is handed a
    /// stream to write to, only the second does.
    #[test]
    fn a_child_has_no_inbox_to_hear_the_operators_fold_in() {
        let (_steer, inbox) = crate::session::Steer::channel();
        let turn = TurnExtras {
            steer: Some(&inbox),
            ..Default::default()
        };

        assert!(
            extras_for(Some(&turn), 0).steer.is_some(),
            "the root cannot hear its own operator"
        );
        for depth in 1..4 {
            assert!(
                extras_for(Some(&turn), depth).steer.is_none(),
                "a child at depth {depth} was handed the operator's inbox"
            );
        }
        // And what a missing inbox means at the drain: `drain_steer` returns
        // before it reads anything, so a child cannot fold, cannot be corrected
        // and cannot be interrupted through a channel it was never given.
        assert!(NO_EXTRAS.steer.is_none());
    }

    /// F6 (0.64.0) — the OTHER prose case is untouched and still falls back.
    ///
    /// Two cases were sent as prose. This release closes the first — a resumed
    /// run's pre-crash history — by restoring the turns it used to lose. The
    /// second is a step whose results do not line up with the calls it made, and
    /// it is correct as it stands: correlating them positionally would answer the
    /// wrong call, and a `tool_result` naming a call that turn did not make is a
    /// 400 on at least one vendor.
    ///
    /// Both cases run through the same lookup, so a change that quietly closed
    /// this one too would be shipping an unreviewed behaviour under this
    /// release's name. Asserted here rather than end to end because a run that
    /// produces more results than calls is not something a fixture can ask a
    /// provider for — it is a disagreement between two counts, and this is where
    /// the two counts meet.
    #[test]
    fn a_step_whose_results_outnumber_its_calls_is_still_sent_as_prose() {
        use crate::context::{Emitted, Piece};

        let section = "\n[read a.txt]\nA\n\n[read b.txt]\nB\n";
        let user = format!("HEAD{section}TAIL");
        let assembled = Assembled {
            text: section.to_string(),
            emitted: vec![
                Emitted {
                    step: 1,
                    ordinal: 0,
                    piece: Piece::Result,
                    text: "\n[read a.txt]\nA\n".into(),
                },
                Emitted {
                    step: 1,
                    ordinal: 1,
                    piece: Piece::Result,
                    text: "\n[read b.txt]\nB\n".into(),
                },
            ],
            ..Default::default()
        };

        // One call, two results: the pairing is not knowable, so the step is prose.
        let mut turns = BTreeMap::new();
        turns.insert(
            1u32,
            StepTurn {
                text: None,
                calls: vec![ToolCall {
                    name: "read_file".into(),
                    arguments: serde_json::json!({ "path": "a.txt" }),
                }],
            },
        );
        let out = transcript(&user, &assembled, &turns);
        assert!(
            !out.iter()
                .any(|m| matches!(m, Message::Assistant { .. }) || matches!(m, Message::Results(_))),
            "a step whose counts disagree carries no assistant turn and no result batch: {out:?}"
        );

        // The control, and it is what makes the assertion above about the counts
        // rather than about a transcript that never pairs anything: give the turn
        // the second call and the same emission pairs.
        turns.get_mut(&1).unwrap().calls.push(ToolCall {
            name: "read_file".into(),
            arguments: serde_json::json!({ "path": "b.txt" }),
        });
        let paired = transcript(&user, &assembled, &turns);
        assert!(
            paired
                .iter()
                .any(|m| matches!(m, Message::Results(rs) if rs.len() == 2)),
            "with both calls present the same results are one batch: {paired:?}"
        );
    }

    /// 0.57.0 — a cohort with nothing to separate it keeps the order the store
    /// returned, at a size where an unstable sort would not.
    ///
    /// Written after a sabotage: dropping the index from the sort key and sorting
    /// unstably failed nothing, because every other test here ties at most two
    /// entries and a two-element unstable sort does not move equal keys. Sixty-four
    /// is past the threshold where the sort switches strategy, which is where the
    /// guarantee stops being free. Without the index term this is the assertion
    /// that goes — and with it, "an entry with no signal and no evidence keeps
    /// exactly the position it had before this release" is a fact rather than a
    /// property of whichever sort the standard library ships.
    #[test]
    fn a_cohort_with_nothing_to_separate_it_keeps_the_order_the_store_returned() {
        use crate::state::{MemoryKind, MemoryLimits};

        let store = Store::memory().unwrap();
        // Two tied cohorts, **interleaved** rather than contiguous: every even
        // entry shares the goal's word and every odd one shares nothing. A run of
        // equal keys all together is the one case an unstable sort leaves alone;
        // equals that have to be partitioned past each other are the case where
        // it does not, and it is also the ordinary shape of a real store.
        let mut notes: Vec<crate::state::MemoryEntry> = (0..64)
            .map(|i| crate::state::MemoryEntry {
                key: format!("k{i:02}"),
                value: if i % 2 == 0 {
                    "the parser".to_string()
                } else {
                    "unrelated bookkeeping".to_string()
                },
                run_id: 1,
                step: 1,
                created_at: format!("2026-08-15T00:00:00.{i:03}Z"),
                kind: MemoryKind::Fact,
                pinned: false,
            })
            .collect();
        for e in &notes {
            store
                .memory_write_with(
                    "/ws",
                    &e.key,
                    &e.value,
                    1,
                    1,
                    MemoryKind::Fact,
                    MemoryLimits::default(),
                )
                .unwrap();
        }
        let signals = crate::state::memory_tokens("fix the parser");
        rank_notes(&store, "/ws", &mut notes, &signals).unwrap();

        let after: Vec<String> = notes.iter().map(|e| e.key.clone()).collect();
        // The unmatched cohort first, then the matched one — worst-first, which is
        // what the fit reads in reverse — and **within each, the order the store
        // returned**. That second half is the tail of the sort key, and it is what
        // makes "an entry with no signal and no evidence keeps the position it
        // had" a fact rather than a property of whichever sort the standard
        // library happens to ship.
        let expected: Vec<String> = (0..64)
            .filter(|i| i % 2 == 1)
            .chain((0..64).filter(|i| i % 2 == 0))
            .map(|i| format!("k{i:02}"))
            .collect();
        assert_eq!(
            after, expected,
            "two interleaved tied cohorts must come back grouped and, inside each \
             group, in the store's own order"
        );
    }

    /// 0.57.0 N5 and N6 — what ranking a turn's recall costs, and what the
    /// duplicate check adds to a write, at the three store sizes an operator can
    /// now reach.
    ///
    /// A measurement, not a gate: it prints and asserts nothing about a clock.
    /// The shape to expect is linear in entries — every entry is tokenised once
    /// per turn — and flat in the size of the recall table, which is what
    /// 0.56.0's index buys. **A timing that does not move when the input grows
    /// eightfold is a defect report and not a pass**, which is the lesson
    /// 0.56.0's own N5 paid for.
    ///
    /// ```text
    /// cargo test --release --lib memory_recall_cost -- --ignored --nocapture
    /// ```
    #[test]
    #[ignore = "a measurement, not a gate: prints timings, asserts none of them"]
    fn memory_recall_cost() {
        use crate::state::{MemoryKind, MemoryLimits};

        // A goal and two hundred read observations, which is a long run's ledger.
        let goal = "make the parser report the column it stopped at";
        let mut ledger = crate::context::Ledger::new();
        for i in 0..200u32 {
            ledger.push(crate::context::Observation::new(
                i,
                crate::context::ObsKind::Read,
                Some(format!("src/module{i}/handler.rs")),
                "…",
            ));
        }
        let signals = recall_signals(goal, &ledger);
        println!("signal tokens: {}", signals.len());
        println!("entries  recall rows  ms/rank  ms/remember (medians of 20)");

        for entries in [64usize, 512, 4_096] {
            let store = Store::memory().unwrap();
            let limits = MemoryLimits {
                max_entries: entries,
                max_chars: usize::MAX,
                ..MemoryLimits::default()
            };
            for i in 0..entries {
                let value = format!(
                    "note {i} about the parser and the column it stopped at, {}",
                    "detail ".repeat(10)
                );
                store
                    .memory_write_with(
                        "/ws",
                        &format!("k{i}"),
                        &value,
                        1,
                        1,
                        MemoryKind::Fact,
                        limits,
                    )
                    .unwrap();
            }
            let runs = 20i64;
            for run in 100..(100 + runs) {
                let keys: Vec<String> = (0..entries).map(|i| format!("k{i}")).collect();
                store.record_memory_recall(run, 1, "/ws", &keys).unwrap();
            }

            let mut rank = Vec::new();
            for _ in 0..20 {
                let mut notes = store.memory_list("/ws").unwrap();
                let at = std::time::Instant::now();
                rank_notes(&store, "/ws", &mut notes, &signals).unwrap();
                rank.push(at.elapsed());
                assert_eq!(notes.len(), entries, "ranking never drops an entry");
            }

            let mut write = Vec::new();
            for n in 0..20 {
                let at = std::time::Instant::now();
                let restates = store
                    .memory_similar("/ws", "fresh", "note about the parser and the column")
                    .unwrap();
                store
                    .memory_write_with(
                        "/ws",
                        &format!("fresh{n}"),
                        "note about the parser and the column",
                        2,
                        2,
                        MemoryKind::Fact,
                        limits,
                    )
                    .unwrap();
                write.push(at.elapsed());
                assert!(
                    restates.is_some(),
                    "the fixture's notes restate the written one, or this measures the miss path"
                );
            }

            rank.sort();
            write.sort();
            println!(
                "{entries:>7}  {:>11}  {:>7.3}  {:>11.3}",
                entries as i64 * runs,
                rank[rank.len() / 2].as_secs_f64() * 1_000.0,
                write[write.len() / 2].as_secs_f64() * 1_000.0,
            );
        }
    }

    /// 0.50.0 — the operator's ceiling, arm by arm.
    ///
    /// Written because a sabotage found nothing: turning `narrowed`'s `min` into a
    /// `max` — which is the whole difference between a ceiling and a suggestion —
    /// left every test in the suite passing. The end-to-end criterion exercises
    /// the refusal switch, and the switch returns before the clock arithmetic is
    /// ever reached, so the arithmetic had no test at all.
    #[test]
    fn a_contract_clock_is_a_ceiling_the_model_cannot_raise() {
        let min = Duration::from_secs(30);
        let cap = Duration::from_secs(60);

        // A spawn asking for longer than the operator allows gets the operator's.
        assert_eq!(
            narrowed(Return::WaitUntil(Duration::from_secs(600)), Some(cap), true).0,
            Return::WaitUntil(cap),
        );
        // A spawn asking for less keeps its own — narrowing works downward only.
        assert_eq!(
            narrowed(Return::WaitUntil(min), Some(cap), true).0,
            Return::WaitUntil(min),
        );
        // A spawn that named no clock gets the operator's.
        assert_eq!(
            narrowed(Return::Wait, Some(cap), true).0,
            Return::WaitUntil(cap),
        );
        // With no contract clock, what the model asked for stands.
        assert_eq!(narrowed(Return::Wait, None, true).0, Return::Wait);
        assert_eq!(
            narrowed(Return::WaitUntil(min), None, true).0,
            Return::WaitUntil(min),
        );
        // Not waiting at all is already narrower than any clock, so a clock does
        // not turn a detach back into a wait.
        assert_eq!(narrowed(Return::Detach, Some(cap), true).0, Return::Detach);

        // And the refusal outranks every shape, with a line explaining itself.
        for want in [Return::Detach, Return::WaitUntil(min)] {
            let (got, why) = narrowed(want, Some(cap), false);
            assert_eq!(got, Return::Wait);
            assert!(why.is_some(), "a narrowed request is never silent");
        }
        // A plain wait was not narrowed by anything, so it says nothing.
        assert!(narrowed(Return::Wait, None, false).1.is_none());
    }

    /// 0.50.0 — the two arguments, including the pair that means nothing.
    #[test]
    fn a_spawn_states_how_it_wants_its_child_back() {
        let r = |v: serde_json::Value| spawn_return(&v);
        assert_eq!(r(json!({})).unwrap(), Return::Wait);
        assert_eq!(r(json!({ "wait": true })).unwrap(), Return::Wait);
        assert_eq!(r(json!({ "wait": false })).unwrap(), Return::Detach);
        assert_eq!(
            r(json!({ "background_after_secs": 90 })).unwrap(),
            Return::WaitUntil(Duration::from_secs(90)),
        );
        // "Wait zero seconds for it" and "do not wait for it" are one request.
        assert_eq!(
            r(json!({ "background_after_secs": 0 })).unwrap(),
            Return::Detach,
        );
        // And the pair with no meaning is refused, in words naming both.
        let why = r(json!({ "wait": false, "background_after_secs": 30 })).unwrap_err();
        assert!(
            why.contains("wait") && why.contains("background_after_secs"),
            "{why}"
        );
        // 0.60.0 — the OTHER spelling of a zero clock, which this rule was
        // written for in 0.50.0 and never applied to. `wait: false` beside a
        // clock of zero is the same request as `wait: false` alone, and was
        // refused as a contradiction for ten releases. A live run found it: a
        // model filling every property of the schema with its zero value sent
        // exactly this on every call and could not spawn a detached child at all.
        assert_eq!(
            r(json!({ "wait": false, "background_after_secs": 0 })).unwrap(),
            Return::Detach,
            "a zero clock means the same thing whichever way it is spelt",
        );
        // The boundary, asserted exactly: zero is not a contradiction and one is.
        assert!(r(json!({ "wait": false, "background_after_secs": 1 })).is_err());
    }
}

// The private machinery lives in `src/run/<subject>.rs` from 0.63.0, moved out
// with the facade work that had to open this file anyway.
//
// The axis is NOT the one `src/state.rs` used in 0.62.0, and the difference is
// the whole design. That file's public surface is inherent methods on `Store`,
// which `docs/public-api.txt` never enumerates, so its `impl` blocks could move
// freely. This file's public surface is 45 free items re-exported from
// `src/lib.rs`, and the snapshot records each one's *defining file* — moving one
// would rewrite a line of it, which `tests/public_api.rs` proves by sabotage. So
// every one of the 45 stays here, along with the two engines, `Watch`, `refused`
// and `PendingMedia` — the last two because `src/net.rs`, `src/verify.rs` and
// `src/mcp.rs` import them by path, and leaving them here costs nothing and
// keeps those three `use` lines compiling verbatim.
//
// What moves is private machinery, promoted to `pub(super)` and glob-imported
// back, so a moved item is still reachable from here and from its siblings. A
// private member is visible in its own module and its children but never in its
// parent, so a moved type's fields and a moved impl's methods widen with it.
mod dispatch;
mod gate;
mod mailbox;
mod memory;
mod outcome;
mod prompts;
mod read;
mod record;
mod step;
mod tree;

use dispatch::*;
use gate::*;
use mailbox::*;
use memory::*;
use outcome::*;
use prompts::*;
use read::*;
use record::*;
use step::*;
use tree::*;
