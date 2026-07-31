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
use std::time::Duration;

use serde_json::json;
use tracing::info;

use crate::agent::{AgentDef, Agents};
use crate::approve::{ApproveAll, Approver, Decision, Request};
use crate::approve::{Question, Responder, ResponderNone};
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
use crate::state::{AgentEvent, ContextEvent, RunStatus, StepRecord, Store, TodoItem, TodoState};
use crate::toolchain::Toolchain;
use crate::tools::exec::{Exec, ExecOutcome};
use crate::tools::git::{Git, GitCmd, GitOutcome};
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
    Entry, FsTool, Toolbox, Workspace, ASK_QUESTION_TOOL, EDIT_FILE_TOOL, EXEC_TOOL, FIND_TOOL,
    GREP_TOOL, LIST_DIR_TOOL, READ_FILE_TOOL, READ_SKILL_TOOL, REMEMBER_TOOL, TODO_WRITE_TOOL,
    WRITE_FILE_TOOL,
};
#[cfg(feature = "docx")]
use crate::tools::{DOCX_READ_TOOL, DOCX_WRITE_TOOL};
use crate::tools::{GIT_ADD_TOOL, GIT_COMMIT_TOOL, GIT_DIFF_TOOL, GIT_LOG_TOOL, GIT_STATUS_TOOL};
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
///     // Ceilings, and they mean different things. More steps or more time is a
///     // knob; a token ceiling that keeps being hit is usually a task too big
///     // for one contract.
///     RunOutcome::StepCapReached { .. } | RunOutcome::TimeBudgetExceeded { .. } => "raise the bound and resume",
///     RunOutcome::CostBudgetExceeded { .. } | RunOutcome::BudgetCeilingReached { .. } => "split the task",
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
/// }
/// # }
/// ```
///
/// Every variant carries `steps`, which is how many steps *completed* — so a
/// `StepCapReached { steps: 12 }` and a `Success { steps: 12 }` cost the same
/// and only one of them produced anything. For what the run actually spent, use
/// [`RunResult::summary`].
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
    pub seed: &'a [String],
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
}

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
                contract, provider, store, run_id, &root, 1, policy, approver, &mcp, &skills,
                watch, extras,
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
    store.answer_question(question_id, answer, "human")?;

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
                contract,
                provider,
                store,
                run_id,
                &root,
                start_step,
                policy,
                approver,
                &mcp,
                &skills,
                watch,
                &TurnExtras::default(),
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
             with TaskContract::workspace(goal, root). Single-file \
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
                &TurnExtras::default(),
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
                &TurnExtras::default(),
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
                agents: &contract.agents,
                provider,
                store,
                approver,
                responder: responder_of(contract),
                watch,
                ledger,
                containment,
                root,
                root_run_id: run_id,
                web: contract.web.clone(),
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step, None).await;
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
                agents: &contract.agents,
                provider,
                store,
                approver,
                responder: responder_of(contract),
                watch,
                ledger,
                containment,
                root,
                root_run_id: run_id,
                web: contract.web.clone(),
            };
            let outcome = run_agent(&tree, contract, run_id, 0, &effective, start_step, None).await;
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
/// Did this step end the run, because there is no gate and the agent stopped
/// acting?
///
/// Both halves are required. Without [`Verification::None`] an assistant turn
/// with no tool call is an ordinary unproductive step — the agent thinking aloud,
/// or asking a question the loop cannot answer — and ending the run there would
/// silently cap every existing contract at its first quiet turn. Without the
/// empty tool-call list there is no signal at all: no `done` tool is added, so an
/// unverified run has exactly the tool surface a verified one has, and a model
/// that has finished says so by saying something.
fn finished(contract: &TaskContract, response: &CompletionResponse) -> bool {
    matches!(contract.verify, Verification::None)
        && response.tool_calls.is_empty()
        // 0.22.0 — and the turn actually ended. A provider running a long web
        // search hands back what it has so far with a *paused* stop reason and no
        // tool call, which is indistinguishable from a finished answer by the two
        // conditions above. Ending there would stop an unverified run in the
        // middle of the search it was told to make.
        && !paused_turn(response)
}

/// Did the provider pause this turn rather than end it?
///
/// Anthropic says `pause_turn` when a server-executed tool has been running long
/// enough that it would rather hand back control than hold the connection. It is
/// a continuation signal, not a finish: the loop takes another step, and the
/// partial text is an observation like any other.
///
/// What this crate does NOT do is echo the vendor's partial assistant blocks back
/// verbatim — the request has been one flattened user turn since 0.1.0 — so a
/// paused turn resumes as a fresh request and the provider may repeat a search it
/// already charged for. `docs/CONTRACT.md` says so.
fn paused_turn(response: &CompletionResponse) -> bool {
    response.finish_reason.as_deref() == Some("pause_turn")
}

/// The note a failed provider-executed call leaves in the observation log, if
/// this response reported one.
///
/// A vendor reports a broken search inside an otherwise successful response, so
/// without this the model sees an answer with no results and concludes the web
/// had nothing to say. Naming the failure lets it retry or proceed knowingly.
fn web_failure_note(response: &CompletionResponse) -> Option<String> {
    let failed: Vec<String> = response
        .server_tools
        .iter()
        .filter(|c| !c.succeeded())
        .map(|c| {
            format!(
                "{} failed ({})",
                c.tool,
                c.error.as_deref().unwrap_or("no reason given")
            )
        })
        .collect();
    (!failed.is_empty()).then(|| failed.join("; "))
}

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
        // 0.17.0: a `Verification::None` run that ended on its own terms. Final —
        // there is no criterion left to re-check, so a resume reports it rather
        // than driving the loop again to watch the agent say nothing twice.
        "finished" => Some(RunOutcome::Finished { steps: last }),
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
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: vec![tool.clone()],
            // 0.22.0 — what the provider may look up, as the contract declared it.
            // `None` for every contract that declared nothing, which is what the
            // three built-in providers read as "send the 0.21.0 body".
            web: contract.web.clone(),
            // Single-file mode has no `view_image` tool, so only the caller's
            // images are in play here.
            #[cfg(feature = "media")]
            media: attach_media(contract, &mut PendingMedia::default())?,
            ..Default::default()
        };

        let response = complete_with_retry(
            provider, &request, contract, store, run_id, step, watch, 0, false,
        )
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

        // A run with no criterion ends when the agent stops calling tools. After
        // the budget checks, because a step that also crossed a ceiling crossed
        // it — and before the gate, which for this variant can never pass.
        if finished(contract, &response) {
            finish(store, watch, run_id, 0, step, "finished")?;
            return Ok(RunResult::new(RunOutcome::Finished { steps: step }, run_id));
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
    extras: &TurnExtras<'_>,
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
    // A session turn continues a conversation, and the conversation enters as
    // observations rather than as a longer goal: the assembler then bounds and
    // compacts it under the contract's context budget, which is the machinery that
    // already decides what a long run's history gets to say. Step 0, because no
    // step of THIS run produced them.
    //
    // Only when the ledger is empty, which is a fresh turn. A resumed turn already
    // has them from the store — the seed was persisted with the first step — and
    // seeding again would say everything twice.
    if ledger.is_empty() {
        for entry in extras.seed {
            ledger.push(Observation::new(0, ObsKind::Message, None, entry.clone()));
        }
    }
    // Is the agent getting anywhere? Restored from nothing on resume by design: a
    // resumed run has just been given a fresh chance, and condemning it for the
    // window it stalled in before the crash would be a poor welcome.
    let mut progress = Progress::new();
    // Detected once, before the first turn. The marker files do not change under
    // a run often enough to be worth a filesystem walk every step, and a run that
    // creates its own `package.json` is creating a project rather than working in
    // one.
    let toolchain = crate::toolchain::detect(root);
    let mem_key = memory_key(root);
    // Images the agent looked at last step, carried into this one's request and
    // dropped once shown. A viewed image is a tool result, not a permanent part
    // of the conversation: the model that wants it again asks again, and the
    // request stays bounded by what one step actually needed.
    let pending_media = &mut PendingMedia::default();

    for step in start_step..=contract.max_steps {
        // The step boundary, where a cancellation is honoured (see `cancelled`).
        if let Some(o) = cancelled(store, watch, run_id, 0, step - 1)? {
            return Ok(RunResult::new(o, run_id).with_remembered(remembered));
        }
        // The same boundary is where an operator's steering lands, for the same
        // reason: the points in between are not safe to stop at or to change course
        // from — a tool call is in flight and a file may be half-written.
        if let Some(inbox) = extras.steer {
            let steered = inbox.drain();
            if steered.interrupted {
                // The cancel path, not a second one. An interrupted turn is
                // finished rather than abandoned: `runs.status` stops being
                // `running`, a summary is written, and a resume reports the
                // cancellation instead of re-driving the loop.
                finish(store, watch, run_id, 0, step - 1, "cancelled")?;
                info!(run_id, steps = step - 1, "turn interrupted by its operator");
                return Ok(
                    RunResult::new(RunOutcome::Cancelled { steps: step - 1 }, run_id)
                        .with_remembered(remembered),
                );
            }
            for message in steered.messages {
                // An observation like any other: bounded by the same budget,
                // recorded in the same trace, and — carrying no target — never
                // superseded away. It is text the model reads, not permission the
                // model has: every tool call it leads to is checked against the
                // same policy by the same code.
                info!(run_id, step, "operator steered the turn");
                store.record_context_event(
                    run_id,
                    &ContextEvent::steered(
                        step,
                        format!("operator message ({} chars)", message.len()),
                    ),
                )?;
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    format!("\n[operator, mid-turn] {message}\n"),
                ));
            }
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
        let user = workspace_user_prompt(contract, &assembled.text, toolchain.as_ref());
        #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
        let request = CompletionRequest {
            system: system.clone(),
            user: user.clone(),
            tools: tools.clone(),
            // 0.22.0 — the run's web declaration, unchanged per step.
            web: contract.web.clone(),
            #[cfg(feature = "media")]
            media: attach_media(contract, pending_media)?,
            ..Default::default()
        };

        let response = complete_with_retry(
            provider,
            &request,
            contract,
            store,
            run_id,
            step,
            watch,
            0,
            extras.stream,
        )
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
        // 0.22.0 — a provider-executed search that broke reaches the model as an
        // observation, so it can retry or answer knowingly. Without it the model
        // sees an answer with no sources and concludes the web had nothing.
        if let Some(note) = web_failure_note(&response) {
            ledger.push(Observation::new(
                step,
                ObsKind::Message,
                None,
                bound(
                    &format!("\n[step {step}] provider web tool: {note}\n"),
                    entry_cap,
                    ObsKind::Message,
                ),
            ));
        }
        if response.tool_calls.is_empty() {
            let said = response.text.clone().unwrap_or_default();
            ledger.push(Observation::new(
                step,
                ObsKind::Message,
                None,
                bound(
                    &format!("\n[step {step}] {NO_TOOL_CALL} {said}\n"),
                    entry_cap,
                    ObsKind::Message,
                ),
            ));
            decisions.push("no tool call".into());
        }
        let mut paused: Option<i64> = None;
        // 0.21.0 — the other reason a step can stop short: a question nobody here
        // would answer. Kept separate from `paused` so the two pauses cannot be
        // confused for one another in the outcome.
        let mut asked: Option<i64> = None;
        let mut new_rules: Vec<Rule> = Vec::new();
        for call in &response.tool_calls {
            calls_json.push(format!("{}:{}", call.name, call.arguments));
            match dispatch(
                &ws,
                call,
                approver,
                responder_of(contract),
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
                pending_media,
                &contract.commit_identity,
                contract.exec_timeout,
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
                Dispatched::Ask { question_id } => {
                    decisions.push(format!("awaiting answer (question {question_id})"));
                    asked = Some(question_id);
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
        if let Some(question_id) = asked {
            finish(store, watch, run_id, 0, step, "awaiting_answer")?;
            return Ok(RunResult::new(
                RunOutcome::AwaitingAnswer {
                    question_id,
                    steps: step,
                },
                run_id,
            )
            .with_remembered(remembered));
        }

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

        // A run with no criterion ends when the agent stops calling tools. Checked
        // after the budgets, so a step that also crossed a ceiling reports the
        // ceiling, and before the gate, which for this variant can never pass.
        if finished(contract, &response) {
            finish(store, watch, run_id, 0, step, "finished")?;
            return Ok(RunResult::new(RunOutcome::Finished { steps: step }, run_id)
                .with_remembered(remembered));
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
///     max_concurrent: 4,   // the fan-out bound
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
///             // `max_depth` or `max_concurrent`. The parent adapts; nothing fails.
///             EventKind::SpawnRefused { cap } => println!("{pad}! spawn refused: {cap} cap"),
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
        agents: &contract.agents,
        provider,
        store,
        approver,
        responder: responder_of(contract),
        watch,
        ledger,
        containment,
        root,
        root_run_id: run_id,
        web: contract.web.clone(),
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, 1, None).await;
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
        agents: &contract.agents,
        provider,
        store,
        approver,
        responder: responder_of(contract),
        watch,
        ledger,
        containment,
        root,
        root_run_id: run_id,
        web: contract.web.clone(),
    };
    let outcome = run_agent(&tree, contract, run_id, 0, policy, start_step, None).await;
    mcp.shutdown(store, run_id, watch).await;
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

/// One agent's loop, reused for the root and every child. Identical to the
/// workspace loop, plus: it may spawn children (recursively, via [`SPAWN_TOOL`]),
/// and its token spend is drawn from the tree's shared ledger rather than only
/// its own contract budget.
///
/// `depth` is 0 at the root; a child's depth is its parent's + 1. Returns the
/// agent's [`RunOutcome`]; a tree-wide budget halt propagates up as
/// [`RunOutcome::BudgetCeilingReached`].
#[allow(clippy::too_many_arguments)]
fn run_agent<'f, P: Provider>(
    tree: &'f Tree<'_, P>,
    contract: &'f TaskContract,
    run_id: i64,
    depth: u32,
    policy: &'f Policy,
    start_step: u32,
    // 0.21.0 — the definition this agent was spawned from, if it was spawned from
    // one. Crate-internal rather than a `TaskContract` field: a run-level model
    // override is a separate decision about whether a conversation may change models
    // mid-thread, and 0.20.0 deliberately left that shut.
    identity: Option<&'f AgentDef>,
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
        // A role is PREPENDED, never a replacement: the tree prompt is what tells an
        // agent how to use its tools and that its result composes back into its
        // parent, and a role that replaced it would produce an agent that did not
        // know how to be one.
        let system = match identity.and_then(|d| d.role.as_deref()) {
            Some(role) => format!("{}\n\n{system}", role.trim()),
            None => system,
        };
        let mut tools = tree_tools(tree.agents);
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
        // Children share their parent's workspace, so they share its detection too.
        let toolchain = crate::toolchain::detect(&tree.root);
        // Children share their parent's workspace, so they share its memory: one
        // note store per workspace, every entry attributed to the run that wrote it.
        let mem_key = memory_key(&tree.root);
        // See the workspace loop: viewed images ride one step and are dropped.
        let pending_media = &mut PendingMedia::default();

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
            let user = workspace_user_prompt(contract, &assembled.text, toolchain.as_ref());
            #[allow(clippy::needless_update)] // `media` is cfg'd out in the default build
            let request = CompletionRequest {
                system: system.clone(),
                user: user.clone(),
                tools: tools.clone(),
                // 0.21.0 — a named agent's model. `None` for the root and for any
                // child spawned without a definition, which is what every provider
                // reads as "the model you were built with".
                model: identity.and_then(|d| d.model.clone()),
                // 0.22.0 — this agent's declaration, which for a child is the
                // root's, copied in by `spawn_child` rather than taken from the
                // spawn arguments.
                web: contract.web.clone(),
                #[cfg(feature = "media")]
                media: attach_media(contract, pending_media)?,
                ..Default::default()
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
                false,
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
            // The same note as the flat loop: a broken provider-executed search is
            // an observation the child can act on, not silence.
            if let Some(note) = web_failure_note(&response) {
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &format!("\n[step {step}] provider web tool: {note}\n"),
                        entry_cap,
                        ObsKind::Message,
                    ),
                ));
            }
            if response.tool_calls.is_empty() {
                let said = response.text.clone().unwrap_or_default();
                ledger.push(Observation::new(
                    step,
                    ObsKind::Message,
                    None,
                    bound(
                        &format!("\n[step {step}] {NO_TOOL_CALL} {said}\n"),
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
            // 0.21.0 — the other reason a step can stop short: a question nobody here
            // would answer. Kept separate from `paused` so the two pauses cannot be
            // confused for one another in the outcome.
            let mut asked: Option<i64> = None;
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
                    tree.responder,
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
                    pending_media,
                    &contract.commit_identity,
                    contract.exec_timeout,
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
                    Dispatched::Ask { question_id } => {
                        decisions.push(format!("awaiting answer (question {question_id})"));
                        asked = Some(question_id);
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
                        // A child asked the operator something; pause the tree with its
                        // question_id, the same way.
                        SpawnResult::Asked { question_id } => {
                            decisions
                                .push(format!("child awaiting answer (question {question_id})"));
                            asked = Some(question_id);
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
            // 0.21.0 — `asked` counts the same as `paused` here. A parent step whose
            // child stopped for a human must be replayed on resume so the parent
            // re-adopts that child; committing it would leave the child stranded and
            // the parent believing the spawn was done.
            let committed = !((paused.is_some() || asked.is_some()) && paused_by_child);
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

            if let Some(question_id) = asked {
                finish(
                    tree.store,
                    tree.watch,
                    run_id,
                    depth,
                    step,
                    "awaiting_answer",
                )?;
                return Ok(RunOutcome::AwaitingAnswer {
                    question_id,
                    steps: step,
                });
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

            // As in the workspace loop: no criterion means the agent's own quiet
            // turn ends it. A child composes back into its parent carrying this
            // outcome, so a parent can tell a child that finished from one that
            // ran out of steps.
            if finished(contract, &response) {
                finish(tree.store, tree.watch, run_id, depth, step, "finished")?;
                return Ok(RunOutcome::Finished { steps: step });
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
    /// (0.21.0) The child asked the operator about intent and nobody in this process
    /// answered. The question is persisted under `question_id`; the whole tree pauses
    /// so the caller can resume it with [`resume_tree_with_answer`], exactly as a
    /// deferred approval does.
    Asked { question_id: i64 },
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

    // 0.21.0 — an optional named definition. Unknown is an error observation and no
    // child: a spawn that silently became an unnarrowed agent because its definition
    // was misspelled is exactly the failure a roster must not have.
    let named = a
        .get("agent")
        .and_then(|v| v.as_str())
        .filter(|n| !n.is_empty());
    let def = match named {
        Some(name) => match tree.agents.get(name) {
            Some(def) => Some(def),
            None => {
                let known = tree.agents.names().join(", ");
                return Ok(SpawnResult::Composed {
                    decision: format!("unknown agent {name}"),
                    obs: format!(
                        "\n[spawn error] no agent named `{name}`. Available: {}\n",
                        if known.is_empty() { "none" } else { &known }
                    ),
                });
            }
        },
        None => None,
    };

    // A child inherits the parent policy and may only narrow it. Optional
    // `deny_write` globs let the parent tighten the child further, and a definition
    // narrows it again — both through `Policy::contain`, which is why neither can
    // widen anything. There is no path here that adds an allow.
    let mut overlay = Policy::permissive().layer("child");
    if let Some(def) = def {
        if def.deny_write {
            overlay = overlay.deny_write("*");
        }
        if def.deny_net {
            overlay = overlay.deny_net("*");
        }
    }
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
    let mut child_contract = TaskContract::workspace(goal, &tree.root).with_verification(verify);
    // 0.22.0 — the tree's web declaration, not one the model asked for. A child
    // inherits exactly what the root was given and has no way to widen it: the
    // spawn arguments are never read for this, so "give the sub-agent web access"
    // is unwritable in the JSON the model controls.
    child_contract.web = tree.web.clone();
    if let Some(n) = a.get("max_steps").and_then(|v| v.as_u64()) {
        child_contract = child_contract.with_max_steps(n as u32);
    }
    // A definition's cap is the operator's and outranks the model's own request.
    if let Some(n) = def.and_then(|d| d.max_steps) {
        child_contract = child_contract.with_max_steps(n);
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
            // `detail` is free-form and documented as the child's goal; a child
            // spawned from a definition records that too, so the trace says which
            // role ran without the `spawns` table gaining a column (and this
            // release alters no table).
            tree.store.record_agent_event(&AgentEvent::spawn(
                parent_run_id,
                step,
                child_run,
                match def {
                    Some(d) => format!("as {}: {goal}", d.name),
                    None => goal.to_string(),
                },
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
        def,
    )
    .await?;

    // A child that deferred pauses the whole tree, surfacing its request_id so
    // the caller can resume that child once a human decides.
    if let RunOutcome::AwaitingApproval { request_id, .. } = outcome {
        return Ok(SpawnResult::Paused { request_id });
    }

    // And a child that asked the operator something pauses it the same way. Without
    // this the child's `AwaitingAnswer` would fall through to `compose_child` and read
    // as a child that had finished, so the tree would carry on having never heard the
    // question — which is how this was found.
    if let RunOutcome::AwaitingAnswer { question_id, .. } = outcome {
        return Ok(SpawnResult::Asked { question_id });
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

/// The responder a contract carries, or one that answers nothing.
///
/// A `static` rather than a local, so the "nobody answers" case is a reference to one
/// value instead of a temporary every call site has to keep alive. Answering nothing
/// is the default and the honest one for unattended work: the question persists and
/// the run pauses, so waiting costs nothing.
static NO_RESPONDER: ResponderNone = ResponderNone;

fn responder_of(contract: &TaskContract) -> &dyn Responder {
    match &contract.responder {
        Some(r) => r.as_ref(),
        None => &NO_RESPONDER,
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
    /// (0.21.0) The agent asked the operator about intent and no `Responder` in this
    /// process would answer. The question is persisted under `question_id` and the
    /// run stops until a human answers it.
    Ask { question_id: i64 },
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
/// The images one request carries: the caller's, which are the task's subject and
/// ride every step, plus whatever the agent looked at last step, which rides one.
///
/// Bounded here rather than at either source, because neither can see the total.
/// Over the bound the oldest viewed images are dropped first and the model is not
/// told a lie about it — the drop is reported in the trace by the caller. The
/// caller's own images are never dropped: a task about an image that silently
/// stops carrying it is the failure this whole boundary exists to prevent, so an
/// over-budget contract is an error at the first step instead.
#[cfg(feature = "media")]
fn attach_media(
    contract: &TaskContract,
    pending: &mut PendingMedia,
) -> Result<Vec<crate::provider::Media>> {
    use crate::provider::MAX_REQUEST_IMAGE_BYTES;
    let fixed: usize = contract.images.iter().map(|m| m.byte_len()).sum();
    if fixed > MAX_REQUEST_IMAGE_BYTES {
        return Err(Error::Config(format!(
            "the contract's images total {fixed} bytes, over the \
             {MAX_REQUEST_IMAGE_BYTES}-byte per-request bound"
        )));
    }
    let mut out = contract.images.clone();
    let mut used = fixed;
    for m in pending.drain(..) {
        if used + m.byte_len() > MAX_REQUEST_IMAGE_BYTES {
            continue;
        }
        used += m.byte_len();
        out.push(m);
    }
    Ok(out)
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

/// Read a `todo_write` argument object into a plan, or say what was wrong with it.
///
/// Strict on purpose, and the error goes back to the model as an observation rather
/// than out of the run: an item whose state the crate does not understand is an item
/// whose state nobody knows, and guessing `pending` would show an operator a plan the
/// agent did not write. The message names the three legal states, so the correction
/// costs one step and needs no documentation.
fn parse_todo_items(args: &serde_json::Value) -> std::result::Result<Vec<TodoItem>, String> {
    let list = args
        .get("items")
        .ok_or_else(|| "`items` is required: send the whole plan as a list".to_string())?
        .as_array()
        .ok_or_else(|| {
            "`items` must be a list of {text, state} objects, and the whole plan is sent \
             every time"
                .to_string()
        })?;
    let mut out = Vec::with_capacity(list.len());
    for (i, raw) in list.iter().enumerate() {
        let text = raw
            .get("text")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .ok_or_else(|| format!("item {} needs a non-empty `text`", i + 1))?;
        let state = raw
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| format!("item {} (`{text}`) needs a `state`", i + 1))?;
        let state = TodoState::parse(state).ok_or_else(|| {
            format!(
                "item {} (`{text}`) has state `{state}`; use pending, active or done",
                i + 1
            )
        })?;
        out.push(TodoItem::new(text, state));
    }
    Ok(out)
}

/// observations the agent can recover from rather than failing the run — only
/// the model can decide what to do about them.
#[allow(clippy::too_many_arguments)]
// `pending_media` is `()` without the feature, and nothing reads it there.
#[cfg_attr(not(feature = "media"), allow(unused_variables))]
async fn dispatch(
    ws: &Workspace,
    call: &ToolCall,
    approver: &dyn Approver,
    // 0.21.0 — who answers a question about intent. Separate from `approver` on
    // purpose: one decides whether an act is permitted, the other what was wanted.
    responder: &dyn Responder,
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
    pending_media: &mut PendingMedia,
    identity: &crate::tools::git::Identity,
    exec_timeout: Duration,
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
    let name = call.name.as_str();
    Ok(match name {
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
        TODO_WRITE_TOOL => {
            // Not gated, and deliberately so: this writes into the harness's own
            // store, not into the workspace, the network or a binary, so there is no
            // `Act` it could be checked against. Inventing one would put a permission
            // rule in front of the agent stating its intentions. See the plan section
            // of `docs/CONTRACT.md`.
            let items = match parse_todo_items(a) {
                Ok(items) => items,
                Err(why) => {
                    return Ok(Dispatched::go(
                        "todo error",
                        format!("\n[todo error] {why}\n"),
                    ))
                }
            };
            // The store caps the list and reports what it dropped; it writes no trace
            // row of its own, so both are recorded here, where the step is known.
            let dropped = store.write_todos(run_id, &items)?;
            let done = items
                .iter()
                .filter(|i| i.state == crate::state::TodoState::Done)
                .count();
            store.record_context_event(
                run_id,
                &ContextEvent::todo_write(step, format!("{} items, {done} done", items.len())),
            )?;
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::TodoWrote {
                    items: items.clone(),
                },
            ));
            info!(run_id, step, items = items.len(), done, dropped, "plan");
            // The plan back in one line, so the model sees what was actually recorded
            // rather than assuming its write landed verbatim — the cap is visible.
            let mut obs = format!("\n[plan {} items, {done} done]\n", items.len());
            for item in &items {
                obs.push_str(&format!("- [{}] {}\n", item.state.as_str(), item.text));
            }
            if dropped > 0 {
                obs.push_str(&format!(
                    "({dropped} item(s) past the {} the plan holds were dropped)\n",
                    crate::state::TODO_MAX_ITEMS
                ));
            }
            // No target: a plan supersedes nothing and cannot go stale — the next
            // write replaces it wholesale.
            Dispatched::seen(
                format!("plan: {} items, {done} done", items.len()),
                obs,
                ObsKind::Tool,
                None,
            )
        }
        ASK_QUESTION_TOOL => {
            // Not gated, and for a sharper reason than the todo tool's: this asks a
            // human something. Putting a permission rule in front of the channel
            // whose whole purpose is to ask would be a category error, and there is
            // no `Act` that means "ask about intent" — see `docs/CONTRACT.md`.
            let Some(text) = s("question").map(str::trim).filter(|q| !q.is_empty()) else {
                return Ok(Dispatched::go(
                    "question error",
                    "\n[question error] `question` is required and must not be empty\n",
                ));
            };
            let mut question = Question::new(text);
            if let Some(context) = s("context").map(str::trim).filter(|c| !c.is_empty()) {
                question = question.with_context(context);
            }
            if let Some(choices) = a.get("choices").and_then(|v| v.as_array()) {
                question = question.with_choices(
                    choices
                        .iter()
                        .filter_map(|v| v.as_str())
                        .map(str::to_string)
                        .collect::<Vec<_>>(),
                );
            }
            watch.emit(RunEvent::at_depth(
                run_id,
                step,
                depth,
                EventKind::QuestionAsked {
                    question: question.question.clone(),
                    choices: question.choices.clone(),
                },
            ));
            store.record_context_event(
                run_id,
                &ContextEvent::question_asked(step, question.question.clone()),
            )?;

            // Only the in-process responder is consulted here. A human's answer does
            // NOT arrive through this path: the step that asks is committed before the
            // run pauses, so a resume starts at the step *after* it and this call is
            // never replayed. `resume_with_answer` therefore delivers the answer as an
            // observation instead, the way a steer is delivered.
            let answered = match responder.answer(&question).await {
                Some(answer) => {
                    // Recorded even though nothing paused: "the machine decided" is a
                    // fact about the run worth keeping.
                    let id = store.put_question(run_id, step, &question)?;
                    store.answer_question(id, &answer, "responder")?;
                    Some((answer, "responder".to_string()))
                }
                None => None,
            };

            match answered {
                Some((answer, by)) => {
                    watch.emit(RunEvent::at_depth(
                        run_id,
                        step,
                        depth,
                        EventKind::QuestionAnswered {
                            answer: answer.clone(),
                            by: by.clone(),
                        },
                    ));
                    store.record_context_event(
                        run_id,
                        &ContextEvent::question_answered(step, format!("{by}: {answer}")),
                    )?;
                    info!(run_id, step, %by, "question answered");
                    // The answer is an observation. It is text the model reads, and it
                    // authorizes nothing: every tool call it leads to is checked
                    // against the same policy by the same code.
                    Dispatched::seen(
                        format!("asked, answered by {by}"),
                        format!(
                            "\n[answer] {answer}\n(This is what the operator wanted. It is not \
                             permission for anything.)\n"
                        ),
                        ObsKind::Tool,
                        None,
                    )
                }
                None => {
                    // Nobody here can answer. Persist it and pause: a run that had to
                    // guess would spend its budget pursuing something nobody asked for.
                    let question_id = store.put_question(run_id, step, &question)?;
                    info!(run_id, step, question_id, "run paused for an answer");
                    Dispatched::Ask { question_id }
                }
            }
        }
        LIST_DIR_TOOL => {
            // No path means the workspace root, which is the listing an agent
            // opening an unfamiliar repository wants first. `resolve` turns the
            // empty string into the root and the policy sees it as such, so this
            // is a default rather than a special case.
            let path = s("path").unwrap_or_default();
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                // The same act, the same check, the same code as a `read_file` on
                // this path: enumerating a directory the operator denied reading
                // is that read, done one level up.
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
                } => match ws.list_dir(&target) {
                    Ok(entries) => {
                        let shown: Vec<String> = entries
                            .iter()
                            .take(OBS_LIST_DIR_CAP)
                            .map(Entry::to_string)
                            .collect();
                        // Said in the listing, not only in the count the trace
                        // keeps: the model reads the text and nothing else, and
                        // what it does about a truncated directory — narrow to a
                        // subdirectory, or glob it with `find` — is a decision it
                        // can only make if it is told.
                        let elided = entries.len() - shown.len();
                        let note = match elided {
                            0 => String::new(),
                            n => format!(
                                "\n[showing {} of {} entries; {n} not listed — list a \
                                 subdirectory or use find to narrow]",
                                shown.len(),
                                entries.len()
                            ),
                        };
                        Dispatched::Continue {
                            decision: format!("list_dir {target} ({} entries)", entries.len()),
                            obs: bound(
                                &format!("\n[list_dir {target}]\n{}{note}\n", shown.join("\n")),
                                cap,
                                ObsKind::Find,
                            ),
                            // A listing is a filename answer about a path, like
                            // `find`'s: a later listing of the same directory is
                            // the same question asked again and supersedes this
                            // one. It is not its own kind because nothing in the
                            // context layer would treat it differently.
                            kind: ObsKind::Find,
                            target: Some(target.clone()),
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => Dispatched::go("list_dir error", format!("\n[list_dir error] {e}\n")),
                },
            }
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
        #[cfg(feature = "media")]
        VIEW_IMAGE_TOOL => {
            let path = s("path").unwrap_or_default();
            // The extension decides the media type, and an unknown one is
            // reported rather than guessed. Checked before the gate only because
            // it costs nothing: the gate still runs for every path that could
            // actually be read, so this cannot be used to probe for a file's
            // existence outside the policy.
            let Some(media_type) = crate::provider::Media::media_type_for(path) else {
                return Ok(Dispatched::go(
                    "view_image unsupported type",
                    format!(
                        "\n[view_image error] {path} is not an image this crate can send. \
                         Supported: {}\n",
                        crate::provider::IMAGE_MEDIA_TYPES.join(", ")
                    ),
                ));
            };
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
                } => match ws
                    .read_bytes(&target)
                    .map_err(|e| e.to_string())
                    .and_then(|bytes| {
                        crate::provider::Media::image(media_type, &bytes).map_err(|e| e.to_string())
                    }) {
                    Ok(media) => {
                        // The observation records what was sent, not the image:
                        // a digest, a size and a type. A trace that held the
                        // bytes would grow by megabytes a step in exactly the
                        // long unattended runs this crate exists for.
                        let obs = format!(
                            "\n[view_image {target}] attached to the next request \
                             ({media_type}, {} bytes, digest {})\n",
                            media.byte_len(),
                            media.digest()
                        );
                        pending_media.push(media);
                        Dispatched::Continue {
                            decision: format!("viewed {target}"),
                            obs,
                            kind: ObsKind::Read,
                            target: Some(target.clone()),
                            changed: false,
                            remember,
                        }
                    }
                    Err(e) => {
                        Dispatched::go("view_image error", format!("\n[view_image error] {e}\n"))
                    }
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
                    // What is there now, read before the write so the line counts
                    // below have something to compare against. The write gate has
                    // already passed at this point; this is measurement, and the
                    // content never reaches the model.
                    let before = ws
                        .resolve(&target)
                        .ok()
                        .and_then(|abs| std::fs::read_to_string(abs).ok())
                        .unwrap_or_default();
                    match ws.write_file(&target, &body) {
                        Ok(wrote) => {
                            record_edit(
                                store,
                                run_id,
                                step,
                                WRITE_FILE_TOOL,
                                &target,
                                &before,
                                &body,
                            );
                            Dispatched::Continue {
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
                            }
                        }
                        Err(e) => Dispatched::go("write error", format!("\n[write error] {e}\n")),
                    }
                }
            }
        }
        EDIT_FILE_TOOL => {
            let path = s("path").unwrap_or_default();
            let search = s("search").unwrap_or_default();
            let replacement = s("replace").unwrap_or_default();
            if path.is_empty() || search.is_empty() {
                return Ok(Dispatched::go(
                    "edit missing arguments",
                    "\n[edit error] edit_file needs a \"path\" and a non-empty \"search\"\n",
                ));
            }
            // The same act as `write_file`, so the same gate on the same path — a
            // partial edit is not a lesser write, and a policy that refuses one
            // refuses the other. The replacement text is offered as the content so
            // a human deciding an `Ask` sees what is going in rather than only
            // where.
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(replacement),
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
                    let replacement = content.unwrap_or_default();
                    match ws.edit_file(&target, search, &replacement) {
                        Ok(wrote) => {
                            // The replaced text against the text that replaced it.
                            // Everything outside the match is byte-identical by
                            // construction, so this is the same answer comparing the
                            // whole file would give, without reading it twice.
                            record_edit(
                                store,
                                run_id,
                                step,
                                EDIT_FILE_TOOL,
                                &target,
                                search,
                                &replacement,
                            );
                            Dispatched::Continue {
                                decision: format!("edited {target}"),
                                obs: bound(
                                    &format!(
                                        "\n[edited {target}] replaced {} chars with {}{}\n",
                                        search.chars().count(),
                                        replacement.chars().count(),
                                        if wrote.moved_the_workspace() {
                                            ""
                                        } else {
                                            " — the replacement is identical to what was there, so \
                                         the workspace did not change"
                                        }
                                    ),
                                    cap,
                                    ObsKind::Write,
                                ),
                                kind: ObsKind::Write,
                                target: Some(target.clone()),
                                changed: wrote.moved_the_workspace(),
                                remember,
                            }
                        }
                        // A miss or an ambiguity is the model's to fix and says how:
                        // an edit that guessed which of three occurrences was meant
                        // is the failure this tool exists to make impossible.
                        Err(e) => Dispatched::go("edit error", format!("\n[edit error] {e}\n")),
                    }
                }
            }
        }
        EXEC_TOOL => {
            let argv: Vec<String> = a
                .get("argv")
                .and_then(|v| v.as_array())
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let Some(program) = argv.first().filter(|p| !p.trim().is_empty()).cloned() else {
                return Ok(Dispatched::go(
                    "exec missing argv",
                    "\n[exec error] exec needs a non-empty \"argv\" array whose first element is \
                     the program\n",
                ));
            };
            // Two checks, and the second is the one that makes a useful rule
            // writable. The program alone is what `deny_exec(\"rm\")` names, and it
            // holds whatever the arguments are. The whole argv is what
            // `allow_exec(\"git log*\")` and `deny_exec(\"git push*\")` name — a
            // check on the program could not tell those two apart, which is the
            // weakness the git built-ins were built to route around and the reason
            // they still exist.
            let joined = argv.join(" ");
            let mut remembered: Vec<Rule> = Vec::new();
            let mut targets = vec![program.clone()];
            if joined != program {
                targets.push(joined.clone());
            }
            for target in targets {
                match gate(
                    ws,
                    approver,
                    store,
                    run_id,
                    step,
                    Act::Exec,
                    &target,
                    None,
                    watch,
                    depth,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => return Ok(Dispatched::go(decision, obs)),
                    Gated::Paused { request_id } => return Ok(Dispatched::Pause { request_id }),
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }

            let outcome = Exec::new(ws.root(), exec_timeout, cap).run(&argv).await?;
            let (decision, obs) = match &outcome {
                ExecOutcome::Unavailable { reason } => (
                    format!("{program} unavailable"),
                    format!(
                        "\n[exec unavailable] {reason}. This machine cannot run that command; \
                         try another, or carry on without it.\n"
                    ),
                ),
                ExecOutcome::TimedOut { after } => (
                    format!("{program} timed out"),
                    format!(
                        "\n[exec timed out] `{joined}` was killed after {}s without finishing. \
                         Nothing it printed was captured. Run something narrower, or expect this \
                         command to need longer than this run allows.\n",
                        after.as_secs()
                    ),
                ),
                ExecOutcome::Ran {
                    code,
                    stdout,
                    stderr,
                    elided,
                } => {
                    let body = crate::verify::joined_streams(stdout, stderr);
                    (
                        format!(
                            "exec {program} {}",
                            code.map_or("(signal)".to_string(), |c| format!("exit {c}"))
                        ),
                        format!(
                            "\n[exec `{joined}` {}]{}\n{}\n",
                            code.map_or("killed by a signal".to_string(), |c| format!("exit {c}")),
                            if *elided > 0 {
                                format!(
                                    " ({elided} characters of output elided from the middle; the \
                                     start and the end are both here)"
                                )
                            } else {
                                String::new()
                            },
                            if body.trim().is_empty() {
                                "(no output)"
                            } else {
                                body.trim_end()
                            }
                        ),
                    )
                }
            };
            Dispatched::Continue {
                decision,
                obs,
                kind: ObsKind::Tool,
                target: None,
                // Deliberately not `changed`, even for a command that plainly
                // wrote files. The stall signal asks whether the agent is getting
                // anywhere, and running the same build a fourth time without
                // editing anything in between is the shape of an agent that is
                // not — the argv is part of the call signature, so a *different*
                // command is never mistaken for a repeat.
                changed: false,
                remember: remembered,
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
        // Spreadsheet built-ins (0.14.0). Each one gates on the path the model
        // named, with `Act::Read` or `Act::Write`, through the same `gate` the
        // file built-ins use — so a refusal names the workbook rather than the
        // tool, a child's narrowed policy applies to documents exactly as it
        // applies to source, and the underlying module reaches the file only
        // through `Workspace`'s policy-checked byte IO.
        //
        // This is why they are built-ins and not registered `Tool`s: a registered
        // tool is authorised once by an exec check on its name and then does
        // whatever it likes to the filesystem, which for a capability whose whole
        // job is reading and writing the user's files is the wrong boundary.
        #[cfg(feature = "xlsx")]
        XLSX_SHEETS_TOOL | XLSX_READ_TOOL => {
            let path = s("path").unwrap_or_default();
            let sheet = s("sheet");
            let listing = name == XLSX_SHEETS_TOOL;
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
                } => {
                    let read = if listing {
                        crate::tools::documents::xlsx::sheet_names(ws, &target)
                            .map(|names| names.join("\n"))
                    } else {
                        crate::tools::documents::xlsx::read_sheet(ws, &target, sheet)
                    };
                    match read {
                        Ok(text) => Dispatched::Continue {
                            decision: format!("read {target}"),
                            obs: format!(
                                "\n[{name} {target}]\n{}\n",
                                bound(&text, cap, ObsKind::Read)
                            ),
                            kind: ObsKind::Read,
                            target: Some(target.clone()),
                            changed: false,
                            remember,
                        },
                        // A corrupt or non-xlsx file is the model's problem to
                        // route around, not the run's to die on.
                        Err(e) => Dispatched::go(
                            "spreadsheet read error",
                            format!("\n[{name} error] {e}\n"),
                        ),
                    }
                }
            }
        }
        #[cfg(feature = "xlsx")]
        XLSX_WRITE_TOOL | XLSX_SET_CELL_TOOL => {
            let path = s("path").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "spreadsheet missing path",
                    format!("\n[{name} error] needs a \"path\" relative to the workspace root\n"),
                ));
            }
            let sheet = s("sheet").unwrap_or_default().to_string();
            let cell = s("cell").unwrap_or_default().to_string();
            let value = s("value").unwrap_or_default().to_string();
            let rows: Vec<Vec<String>> = call
                .arguments
                .get("rows")
                .and_then(|r| serde_json::from_value(r.clone()).ok())
                .unwrap_or_default();
            let creating = name == XLSX_WRITE_TOOL;
            // The approval preview is the change being asked for, not the file's
            // bytes: a human deciding on a spreadsheet write needs to see what it
            // does, and a workbook's raw bytes tell them nothing.
            let preview = if creating {
                format!(
                    "create workbook {path} sheet {sheet} with {} row(s)",
                    rows.len()
                )
            } else {
                format!("set {sheet}!{cell} to {value:?} in {path}")
            };
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(&preview),
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => {
                    let wrote = if creating {
                        crate::tools::documents::xlsx::write_new(ws, &target, &sheet, &rows)
                    } else {
                        crate::tools::documents::xlsx::set_cell(ws, &target, &sheet, &cell, &value)
                    };
                    match wrote {
                        Ok(w) => Dispatched::Continue {
                            decision: format!("wrote {target}"),
                            obs: format!(
                                "\n[{name} {target}] {}{}\n",
                                preview,
                                if w.moved_the_workspace() {
                                    ""
                                } else {
                                    " — identical to what was already there, the \
                                     workspace did not change"
                                }
                            ),
                            kind: ObsKind::Write,
                            target: Some(target.clone()),
                            changed: w.moved_the_workspace(),
                            remember,
                        },
                        Err(e) => Dispatched::go(
                            "spreadsheet write error",
                            format!("\n[{name} error] {e}\n"),
                        ),
                    }
                }
            }
        }
        // The remaining document readers. Same gate, same reason as the
        // spreadsheet arms above: `Act::Read` on the path the model named.
        #[cfg(any(
            feature = "docx",
            feature = "pptx",
            feature = "pdf",
            feature = "barcode"
        ))]
        n if is_document_read(n) => {
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
                } => match read_document(ws, name, &target) {
                    Ok(text) => Dispatched::Continue {
                        decision: format!("read {target}"),
                        obs: format!(
                            "\n[{name} {target}]\n{}\n",
                            bound(&text, cap, ObsKind::Read)
                        ),
                        kind: ObsKind::Read,
                        target: Some(target.clone()),
                        changed: false,
                        remember,
                    },
                    Err(e) => {
                        Dispatched::go("document read error", format!("\n[{name} error] {e}\n"))
                    }
                },
            }
        }
        // The remaining document writers.
        #[cfg(any(feature = "docx", feature = "pdf"))]
        n if is_document_write(n) => {
            let path = s("path").unwrap_or_default();
            if path.is_empty() {
                return Ok(Dispatched::go(
                    "document missing path",
                    format!("\n[{name} error] needs a \"path\" relative to the workspace root\n"),
                ));
            }
            let preview = describe_document_write(name, &call.arguments);
            match gate(
                ws,
                approver,
                store,
                run_id,
                step,
                Act::Write,
                path,
                Some(&preview),
                watch,
                depth,
            )
            .await?
            {
                Gated::Refused { decision, obs } => Dispatched::go(decision, obs),
                Gated::Paused { request_id } => Dispatched::Pause { request_id },
                Gated::Go {
                    target, remember, ..
                } => match write_document(ws, name, &target, &call.arguments) {
                    Ok(w) => Dispatched::Continue {
                        decision: format!("wrote {target}"),
                        obs: format!(
                            "\n[{name} {target}] {preview}{}\n",
                            if w.moved_the_workspace() {
                                ""
                            } else {
                                " — identical to what was already there, the workspace \
                                 did not change"
                            }
                        ),
                        kind: ObsKind::Write,
                        target: Some(target.clone()),
                        changed: w.moved_the_workspace(),
                        remember,
                    },
                    Err(e) => {
                        Dispatched::go("document write error", format!("\n[{name} error] {e}\n"))
                    }
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
        GIT_LOG_TOOL | GIT_STATUS_TOOL | GIT_DIFF_TOOL | GIT_ADD_TOOL | GIT_COMMIT_TOOL => {
            // Paths the model named, if any. Every one of them is data: `argv`
            // puts them after `--` and refuses a leading `-`.
            let paths: Vec<String> = a
                .get("paths")
                .and_then(|v| v.as_array())
                .map(|v| {
                    v.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();

            // What the policy is asked, per tool. Reading history reads `.git`.
            // Staging copies a file's bytes into the object store, so it needs
            // `Act::Read` on that file — which is what stops a path the policy
            // denies from reaching a commit. Committing writes `.git`.
            let (repo_act, path_act) = match name {
                GIT_ADD_TOOL => (Act::Write, Some(Act::Read)),
                GIT_COMMIT_TOOL => (Act::Write, None),
                _ => (Act::Read, Some(Act::Read)),
            };

            let mut remembered: Vec<Rule> = Vec::new();
            let mut targets: Vec<(Act, String)> = vec![(repo_act, GIT_DIR.to_string())];
            if let Some(act) = path_act {
                targets.extend(paths.iter().map(|p| (act, p.clone())));
            }
            let mut refused: Option<Dispatched> = None;
            for (act, target) in targets {
                match gate(
                    ws, approver, store, run_id, step, act, &target, None, watch, depth,
                )
                .await?
                {
                    Gated::Refused { decision, obs } => {
                        refused = Some(Dispatched::go(decision, obs));
                        break;
                    }
                    Gated::Paused { request_id } => {
                        refused = Some(Dispatched::Pause { request_id });
                        break;
                    }
                    Gated::Go { remember, .. } => remembered.extend(remember),
                }
            }
            if let Some(d) = refused {
                return Ok(d);
            }

            let cmd = match name {
                GIT_STATUS_TOOL => GitCmd::Status { paths },
                GIT_DIFF_TOOL => GitCmd::Diff {
                    staged: a.get("staged").and_then(serde_json::Value::as_bool) == Some(true),
                    paths,
                },
                GIT_LOG_TOOL => GitCmd::Log {
                    // Clamped rather than trusted: a model asking for the whole
                    // history of a large repository would blow the observation
                    // cap and learn nothing the first twenty commits do not say.
                    max_count: a
                        .get("max_count")
                        .and_then(serde_json::Value::as_u64)
                        .unwrap_or(20)
                        .clamp(1, 200) as u32,
                    paths,
                },
                GIT_ADD_TOOL => {
                    if paths.is_empty() {
                        return Ok(Dispatched::go(
                            "git_add missing paths",
                            "\n[git error] git_add needs a non-empty \"paths\" array\n".to_string(),
                        ));
                    }
                    GitCmd::Add { paths }
                }
                _ => {
                    let Some(message) = s("message").filter(|m| !m.trim().is_empty()) else {
                        return Ok(Dispatched::go(
                            "git_commit missing message",
                            "\n[git error] git_commit needs a non-empty \"message\"\n".to_string(),
                        ));
                    };
                    GitCmd::Commit {
                        message: message.to_string(),
                        identity: identity.clone(),
                    }
                }
            };

            let git = Git::new(ws.policy(), ws.root(), cap);
            // 0.21.0 — a refused git built-in costs a step, not the run.
            //
            // Until here, `Git::run`'s refusal left the loop as `Error::Refused`, so
            // one speculative `git status` under a policy denying `Act::Exec` for
            // `git` escalated the whole run. Found while running the 0.20.0 live
            // session and recorded in `docs/CONTRACT.md`; not fixed there because
            // that release touched no tool. Every other refusal in this crate is an
            // observation the model reads and adapts to, and this is now one too.
            //
            // Both of `Git::run`'s refusals land here — the policy denying the `git`
            // program, and a path that would be read as an option — and the row is
            // written exactly as `gate` writes one, so a reader cannot tell a git
            // refusal from any other and does not have to.
            let outcome = match git.run(&cmd).await {
                Ok(out) => out,
                Err(Error::Refused {
                    act,
                    target,
                    rule,
                    layer,
                }) => {
                    let mut ev = PolicyEvent::refusal(step, act.clone(), target.clone());
                    ev.rule = rule.clone();
                    ev.layer = layer.clone();
                    store.record_event(run_id, &ev)?;
                    // Qualified: this arm has a local `refused` holding the gate's
                    // verdict for the paths, which shadows the function.
                    crate::run::refused(watch, run_id, depth, &ev);
                    let why = rule
                        .as_deref()
                        .map(|r| format!(" (rule {r})"))
                        .unwrap_or_default();
                    return Ok(Dispatched::go(
                        format!("{name} refused"),
                        format!(
                            "\n[{act} refused] {target}{why} — the policy forbids this; carry on \
                             without git\n"
                        ),
                    ));
                }
                Err(e) => return Err(e),
            };
            match outcome {
                GitOutcome::Unavailable { reason } => Dispatched::go(
                    "git unavailable",
                    format!(
                        "\n[git unavailable] {reason}. This workspace cannot be worked as a git \
                         repository; carry on without it.\n"
                    ),
                ),
                out @ GitOutcome::Ran { .. } => {
                    let GitOutcome::Ran {
                        code,
                        stdout,
                        stderr,
                    } = &out
                    else {
                        unreachable!()
                    };
                    let ok = out.ok();
                    let body = if stdout.trim().is_empty() && !ok {
                        stderr.clone()
                    } else {
                        stdout.clone()
                    };
                    // A git that ran and failed is an observation, not a run
                    // failure — the same treatment a malformed regex gets from
                    // `grep`. The model reads the message and adapts.
                    Dispatched::Continue {
                        decision: format!(
                            "{name} {}",
                            if ok {
                                "ok".to_string()
                            } else {
                                format!("exit {}", code.map_or("signal".into(), |c| c.to_string()))
                            }
                        ),
                        obs: format!(
                            "\n[{name}{}]\n{}\n",
                            if ok {
                                String::new()
                            } else {
                                " failed".to_string()
                            },
                            if body.trim().is_empty() {
                                "(no output)"
                            } else {
                                body.trim_end()
                            }
                        ),
                        kind: ObsKind::Tool,
                        target: None,
                        changed: matches!(cmd, GitCmd::Add { .. } | GitCmd::Commit { .. }) && ok,
                        remember: remembered,
                    }
                }
            }
        }
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
                .call_media(
                    name,
                    &call.arguments,
                    store,
                    run_id,
                    step,
                    cap,
                    watch,
                    depth,
                    pending_media,
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
    stream: bool,
) -> Result<CompletionResponse> {
    // The general media boundary. Every completion in every loop goes through
    // here, so this covers an out-of-tree `Provider` as well as the three built
    // in — and it runs before the first attempt, so a refused request costs no
    // retry, no token and no wall clock. The built-in providers check again
    // inside their own `complete`, which is what stops a caller reaching one
    // directly from bypassing it.
    #[cfg(feature = "media")]
    crate::provider::ensure_media_accepted(provider.name(), provider.accepts_images(), request)?;
    let max_retries = contract.max_retries;
    let retry = contract.retry;
    let max_duration = contract.max_duration;
    let mut attempt = 0;
    loop {
        // 0.18.0: one `provider_calls` row per attempt, written here because this
        // is the only place that knows an attempt happened — a `Fallback` is one
        // `complete` call from the outside, and `steps.tokens` collapses a
        // retried step into a single integer attributed to nothing.
        //
        // The clock is the harness's own and brackets `complete`, so it includes
        // this crate's request building and stream consumption as well as the
        // provider's part. `CONTRACT.md` says so rather than implying the figure
        // is the vendor's.
        let started = std::time::Instant::now();
        // A streamed attempt and a plain one are the same attempt: the same clock,
        // the same `provider_calls` row, the same retry classification. The only
        // difference is that the deltas of a streamed one reach the observer while
        // it is still in flight instead of being accumulated in silence.
        let outcome = if stream {
            stream_completion(provider, request, watch, run_id, step, depth).await
        } else {
            provider.complete(request.clone()).await
        };
        let latency_ms = started.elapsed().as_millis() as u64;
        record_provider_call(
            store,
            run_id,
            step,
            attempt,
            provider.name(),
            latency_ms,
            &outcome,
        );
        // 0.22.0 — and what the provider looked up while serving it: the sources
        // it cited and the server-side calls it ran, including the ones that
        // failed inside an otherwise successful response.
        record_web_activity(store, watch, run_id, step, depth, &outcome);
        match outcome {
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

/// One completion whose text deltas reach the observer while the model is still
/// producing them.
///
/// The provider's sink has to be `Send + Sync` — its future is `Send` and a
/// built-in provider's is driven inside `reqwest`'s stream — and [`Watch`] is
/// neither: it holds a `Cell` so a cancellation can outlive the `event()` call
/// that asked for it, and `Store` is `!Sync` besides. A channel is the seam
/// between the two halves: the sink is a closure over an `mpsc` sender, which is
/// `Send + Sync`, and the emitting happens here, on the loop's own task, where
/// `Watch` already lives.
///
/// Which also means an observer that returns [`Flow::Cancel`](crate::Flow::Cancel)
/// from a `Token` event is honoured exactly where every other cancellation is —
/// at the next step boundary — rather than tearing down a request mid-body.
async fn stream_completion<P: Provider>(
    provider: &P,
    request: &CompletionRequest,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    depth: u32,
) -> Result<CompletionResponse> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    // Unbounded deliberately: a bounded sender would either block the provider's
    // stream on a slow observer — turning rendering latency into wire latency — or
    // drop a delta, and a stream missing one delta reads like a complete answer
    // and is not.
    let sink = move |text: &str| {
        // A closed receiver means this function has already returned, which cannot
        // happen while the future it awaits is still alive. Ignored rather than
        // escalated: a completion must not fail because nobody was listening.
        let _ = tx.send(text.to_string());
    };
    let completion = provider.complete_streaming(request.clone(), &sink);
    tokio::pin!(completion);
    let outcome = loop {
        tokio::select! {
            // Deltas first, so a burst that lands with the final chunk is emitted
            // in order rather than after the response it belongs to.
            biased;
            Some(text) = rx.recv() => {
                watch.emit(RunEvent::at_depth(run_id, step, depth, EventKind::Token { text }));
            }
            done = &mut completion => break done,
        }
    };
    // Whatever the provider sent between the last poll and its return. Without this
    // the last delta of every stream is lost, which is the failure mode a
    // concatenation assertion exists to catch.
    while let Ok(text) = rx.try_recv() {
        watch.emit(RunEvent::at_depth(
            run_id,
            step,
            depth,
            EventKind::Token { text },
        ));
    }
    outcome
}

/// Record one file change and the lines it added and removed (0.18.0).
///
/// Swallowed on a store failure for the same reason a provider call is: an edit
/// that reached the disk is not undone by failing to write its bookkeeping row,
/// and turning the run into an error here would lose the work as well as the
/// row.
fn record_edit(
    store: &Store,
    run_id: i64,
    step: u32,
    tool: &str,
    path: &str,
    before: &str,
    after: &str,
) {
    let edit = crate::state::Edit::measure(step, tool, path, before, after);
    if let Err(e) = store.record_edit(run_id, &edit) {
        tracing::warn!("could not record the edit to {path} at step {step}: {e}");
    }
}

/// Record one provider call, answered or failed (0.18.0).
///
/// A failed attempt is recorded too, and deliberately: a model that produced
/// tokens and then hit a broken connection was still billed for them, so a trace
/// that kept only the winning attempt would understate the money.
///
/// A store that cannot take the row is logged and swallowed. The alternative is
/// failing a run that the provider answered because the accounting could not be
/// written, which trades a real answer for a bookkeeping entry.
fn record_provider_call(
    store: &Store,
    run_id: i64,
    step: u32,
    attempt: u32,
    provider: &str,
    latency_ms: u64,
    outcome: &Result<CompletionResponse>,
) {
    let call = crate::state::ProviderCall {
        step,
        attempt,
        provider: provider.to_string(),
        model: outcome.as_ref().ok().and_then(|r| r.model.clone()),
        usage: outcome.as_ref().ok().and_then(|r| r.usage),
        latency_ms,
        ttft_ms: outcome.as_ref().ok().and_then(|r| r.ttft_ms),
        finish_reason: outcome.as_ref().ok().and_then(|r| r.finish_reason.clone()),
        // The same short name the retry and escalation rows use, so the two
        // surfaces name one failure identically rather than nearly so.
        failure: outcome.as_ref().err().map(kind_of),
    };
    if let Err(e) = store.record_provider_call(run_id, &call) {
        tracing::warn!("could not record the provider call for step {step}: {e}");
    }
}

/// Record what the provider looked up while serving one call (0.22.0).
///
/// Citations and server-tool rows are written here, beside the `provider_calls`
/// row, because this is the only place that knows which attempt produced them —
/// and because a failed attempt that still ran a search was still billed for it.
///
/// A store that cannot take a row is logged and swallowed, exactly as the
/// accounting row is: failing a run the provider answered because a citation
/// could not be written trades a real answer for a bookkeeping entry.
fn record_web_activity(
    store: &Store,
    watch: &Watch<'_>,
    run_id: i64,
    step: u32,
    depth: u32,
    outcome: &Result<CompletionResponse>,
) {
    let Ok(response) = outcome else { return };
    if !response.citations.is_empty() {
        if let Err(e) = store.record_citations(run_id, step, &response.citations) {
            tracing::warn!("could not record the citations for step {step}: {e}");
        }
    }
    if response.server_tools.is_empty() {
        return;
    }
    if let Err(e) = store.record_server_tool_calls(run_id, step, &response.server_tools) {
        tracing::warn!("could not record the server-tool calls for step {step}: {e}");
    }
    for call in &response.server_tools {
        watch.emit(RunEvent::at_depth(
            run_id,
            step,
            depth,
            EventKind::ServerToolUsed {
                provider: call.provider.clone(),
                tool: call.tool.clone(),
                ok: call.succeeded(),
            },
        ));
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

fn workspace_user_prompt(
    contract: &TaskContract,
    observations: &str,
    toolchain: Option<&Toolchain>,
) -> String {
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
    // Every turn, not only the first. An agent forty steps into a run has had the
    // first turn compacted out from under it by `ContextBudget`, and the project's
    // build command is exactly the fact it would then have to rediscover — which
    // is what this exists to stop it paying for twice.
    let project = match toolchain {
        Some(t) => format!("Project: {}\n", t.describe()),
        None => String::new(),
    };
    format!(
        "Goal: {goal}\nConstraints: {constraints}\nSuccess criterion: {criterion}\n\
         {project}\n\
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
fn tree_tools(agents: &Agents) -> Vec<ToolSpec> {
    let mut tools = workspace_tools();
    // 0.21.0 — the roster is named in the description rather than as a schema `enum`,
    // because a model that asks for a name nobody registered gets a plain error
    // observation naming what IS available, and that recovers in one step. An `enum`
    // would instead make the whole call malformed at the provider.
    let roster = if agents.is_empty() {
        String::new()
    } else {
        format!(
            " Named agents you may spawn with \"agent\", each with its own role, model and \
             (possibly narrower) permissions:\n{}",
            agents.catalog()
        )
    };
    tools.push(ToolSpec {
        name: SPAWN_TOOL.to_string(),
        description: format!(
            "Spawn a contained sub-agent to pursue a smaller goal over the same \
             workspace. The sub-agent inherits your permissions (it can only be \
             further restricted) and its outcome is reported back to you.{roster}"
        ),
        parameters: json!({
            "type": "object",
            "properties": {
                "goal": { "type": "string", "description": "The sub-agent's goal." },
                "agent": { "type": "string", "description": "Optional name of a configured agent to spawn, which gives the sub-agent that agent's role, model and permissions." },
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

/// Whether `name` is a document tool that only reads.
///
/// A free function rather than a match arm per format: the arms differ only in
/// which module they call, and four near-identical arms would drift.
#[cfg(any(
    feature = "docx",
    feature = "pptx",
    feature = "pdf",
    feature = "barcode"
))]
fn is_document_read(name: &str) -> bool {
    #[cfg(feature = "docx")]
    if name == DOCX_READ_TOOL {
        return true;
    }
    #[cfg(feature = "pptx")]
    if name == PPTX_READ_TOOL {
        return true;
    }
    #[cfg(feature = "pdf")]
    if name == PDF_READ_TOOL {
        return true;
    }
    #[cfg(feature = "barcode")]
    if name == BARCODE_DECODE_TOOL {
        return true;
    }
    false
}

/// Whether `name` is a document tool that writes.
#[cfg(any(feature = "docx", feature = "pdf"))]
fn is_document_write(name: &str) -> bool {
    #[cfg(feature = "docx")]
    if name == DOCX_WRITE_TOOL {
        return true;
    }
    #[cfg(feature = "pdf")]
    if name == PDF_WRITE_TOOL || name == PDF_WATERMARK_TOOL || name == PDF_FILL_FORM_TOOL {
        return true;
    }
    false
}

/// Read one document, choosing the reader by the tool the model called rather
/// than by the file's extension: the model named the format it believes it is
/// dealing with, and letting the extension decide would silently read something
/// else than what was asked for.
#[cfg(any(
    feature = "docx",
    feature = "pptx",
    feature = "pdf",
    feature = "barcode"
))]
fn read_document(ws: &Workspace, name: &str, target: &str) -> Result<String> {
    use crate::tools::documents;
    match name {
        #[cfg(feature = "docx")]
        n if n == DOCX_READ_TOOL => documents::docx::read_text(ws, target),
        #[cfg(feature = "pptx")]
        n if n == PPTX_READ_TOOL => documents::pptx::read_text(ws, target),
        #[cfg(feature = "pdf")]
        n if n == PDF_READ_TOOL => documents::pdf::read_text(ws, target),
        #[cfg(feature = "barcode")]
        n if n == BARCODE_DECODE_TOOL => documents::barcode::decode(ws, target).map(|found| {
            if found.is_empty() {
                // Not an error: "I looked and there was nothing there" is a fact
                // the model can act on, and the run continues.
                "no barcode or QR code found in this image".to_string()
            } else {
                found
                    .iter()
                    .map(|d| format!("{}: {}", d.format, d.text))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }),
        other => Err(crate::error::Error::Config(format!(
            "not a document read tool: {other}"
        ))),
    }
}

/// What a document write is about to do, for the approval preview and the trace.
/// The change, never the bytes — a human deciding on a document write cannot
/// decide on a blob.
#[cfg(any(feature = "docx", feature = "pdf"))]
fn describe_document_write(name: &str, args: &serde_json::Value) -> String {
    let count = |key: &str| {
        args.get(key)
            .and_then(|v| v.as_array())
            .map(|a| a.len())
            .unwrap_or(0)
    };
    match name {
        #[cfg(feature = "docx")]
        n if n == DOCX_WRITE_TOOL => {
            format!("create a document of {} paragraph(s)", count("paragraphs"))
        }
        #[cfg(feature = "pdf")]
        n if n == PDF_WRITE_TOOL => format!("create a PDF of {} page(s)", count("pages")),
        #[cfg(feature = "pdf")]
        n if n == PDF_WATERMARK_TOOL => format!(
            "watermark every page with {:?}",
            args.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
        ),
        #[cfg(feature = "pdf")]
        n if n == PDF_FILL_FORM_TOOL => format!("fill {} form field(s)", count("fields")),
        other => format!("write via {other}"),
    }
}

/// Perform one document write, chosen by the tool the model called.
#[cfg(any(feature = "docx", feature = "pdf"))]
fn write_document(
    ws: &Workspace,
    name: &str,
    target: &str,
    args: &serde_json::Value,
) -> Result<crate::tools::workspace::Wrote> {
    use crate::tools::documents;
    #[allow(unused_variables)]
    let strings = |key: &str| -> Vec<String> {
        args.get(key)
            .and_then(|v| serde_json::from_value(v.clone()).ok())
            .unwrap_or_default()
    };
    match name {
        #[cfg(feature = "docx")]
        n if n == DOCX_WRITE_TOOL => documents::docx::write_new(ws, target, &strings("paragraphs")),
        #[cfg(feature = "pdf")]
        n if n == PDF_WRITE_TOOL => documents::pdf::write_new(ws, target, &strings("pages")),
        #[cfg(feature = "pdf")]
        n if n == PDF_WATERMARK_TOOL => documents::pdf::watermark(
            ws,
            target,
            args.get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default(),
        ),
        #[cfg(feature = "pdf")]
        n if n == PDF_FILL_FORM_TOOL => {
            let fields: Vec<(String, String)> = args
                .get("fields")
                .and_then(|v| v.as_object().cloned())
                .map(|m| {
                    m.into_iter()
                        .map(|(k, v)| (k, v.as_str().unwrap_or_default().to_string()))
                        .collect()
                })
                .unwrap_or_default();
            documents::pdf::fill_form(ws, target, &fields)
        }
        other => Err(crate::error::Error::Config(format!(
            "not a document write tool: {other}"
        ))),
    }
}

fn workspace_tools() -> Vec<ToolSpec> {
    #[allow(unused_mut)]
    let mut v = vec![
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
            name: LIST_DIR_TOOL.to_string(),
            description: "List what is immediately inside one directory: each entry with its \
                          kind (file, dir, link) and each file's size in bytes. One level only \
                          — a subdirectory is named, not descended into, so list it in turn to \
                          go deeper. Use this to learn the shape of an unfamiliar tree before \
                          reading anything; use find when you already know what the name looks \
                          like, and grep when you know what the contents say."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Directory relative to the workspace root, e.g. src. Omit it for the root itself." }
                }
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
            name: GIT_STATUS_TOOL.to_string(),
            description: "Show what has changed in the git repository at the workspace root: \
                          modified, staged and untracked files."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional paths to limit the report to." }
                }
            }),
        },
        ToolSpec {
            name: GIT_DIFF_TOOL.to_string(),
            description: "Show the diff of the working tree, or of what is staged.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "staged": { "type": "boolean", "description": "Diff what is staged instead of the working tree." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional paths to limit the diff to." }
                }
            }),
        },
        ToolSpec {
            name: GIT_LOG_TOOL.to_string(),
            description: "Read the repository's recent commit history, newest first.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "max_count": { "type": "integer", "description": "How many commits to show (1-200, default 20)." },
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Optional paths to limit the history to." }
                }
            }),
        },
        ToolSpec {
            name: GIT_ADD_TOOL.to_string(),
            description: "Stage the named files for the next commit. Honours .gitignore; an \
                          ignored file is reported rather than staged."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "paths": { "type": "array", "items": { "type": "string" }, "description": "Files to stage, relative to the workspace root." }
                },
                "required": ["paths"]
            }),
        },
        ToolSpec {
            name: GIT_COMMIT_TOOL.to_string(),
            description: "Commit what you have staged, on the branch that is checked out. There \
                          is no push, no branch switching and no history rewriting: your work \
                          stays local for a human to review."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "The commit message." }
                },
                "required": ["message"]
            }),
        },
        #[cfg(feature = "media")]
        ToolSpec {
            name: VIEW_IMAGE_TOOL.to_string(),
            description: "Look at an image in the workspace. The image is attached to your next \
                          message, so you see it on the following step rather than in this tool's \
                          result. Costs a step; the file must be a jpeg, png, gif or webp."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Image path relative to the workspace root." }
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
            name: TODO_WRITE_TOOL.to_string(),
            description: "Write down your plan so the operator can see where you are. Send the \
                          WHOLE list every time — it replaces the previous one, so include the items \
                          already done with state \"done\". Keep one item \"active\". This is for the \
                          human watching; nothing here is checked, and writing a plan does not do \
                          any of the work in it."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "items": {
                        "type": "array",
                        "description": "The whole plan, in order. Replaces any previous plan.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": { "type": "string", "description": "One step of the plan, in a short phrase." },
                                "state": { "type": "string", "enum": ["pending", "active", "done"], "description": "Where this step has got to." }
                            },
                            "required": ["text", "state"]
                        }
                    }
                },
                "required": ["items"]
            }),
        },
        ToolSpec {
            name: ASK_QUESTION_TOOL.to_string(),
            description: "Ask the operator what they actually want, when the task is genuinely \
                          ambiguous and guessing would waste the run. This is NOT for permission — \
                          you never need permission, the policy decides that and will refuse you if \
                          the answer is no. Use it for intent: which of two files they meant, \
                          whether to keep or drop something, which behaviour is correct. Offer \
                          choices when you have them. Do not ask what you could find out by \
                          looking."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string", "description": "The question, in one sentence." },
                    "context": { "type": "string", "description": "Optional: what you already established, so they can answer without re-deriving it." },
                    "choices": { "type": "array", "items": { "type": "string" }, "description": "Optional options you are offering. The answer need not be one of them." }
                },
                "required": ["question"]
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
        ToolSpec {
            name: EDIT_FILE_TOOL.to_string(),
            description: "Change part of an existing file, leaving the rest of it exactly as it \
                          was. Prefer this to write_file for anything but a new file. The search \
                          text must appear EXACTLY ONCE: if it appears zero times or more than \
                          once the edit is refused and nothing changes, so include enough \
                          surrounding lines to make it unique."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to the workspace root." },
                    "search": { "type": "string", "description": "The exact text to replace, copied from the file including its whitespace. Must occur exactly once." },
                    "replace": { "type": "string", "description": "What to put in its place. An empty string deletes the searched text." }
                },
                "required": ["path", "search", "replace"]
            }),
        },
        ToolSpec {
            name: EXEC_TOOL.to_string(),
            description: "Run a command in the workspace root and read back its exit status and \
                          its output — the project's own build, tests, linter, formatter or \
                          package manager. There is NO shell: give the command as an array of \
                          strings, one element per argument. Pipes, redirection, `&&`, `;` and \
                          `$(...)` are not interpreted; they are ordinary characters inside \
                          whichever argument contains them. Run one command per call. A command \
                          that runs too long is killed and reported as a timeout, and very long \
                          output keeps its start and its end with the middle elided."
                .to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "argv": {
                        "type": "array",
                        "description": "The command, one array element per argument, program first — e.g. [\"npm\", \"test\"] or [\"cargo\", \"test\", \"--all-features\"].",
                        "items": { "type": "string" }
                    }
                },
                "required": ["argv"]
            }),
        },
    ];
    #[cfg(feature = "xlsx")]
    // Offered only when the feature is on. A model is told about a tool it can
    // actually call, never about one the build does not contain.
    v.extend([
        ToolSpec {
            name: XLSX_SHEETS_TOOL.to_string(),
            description: "List the sheet names of an .xlsx workbook in the workspace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workbook path relative to the workspace root." }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: XLSX_READ_TOOL.to_string(),
            description: "Read one sheet of an .xlsx workbook as text. Omit \"sheet\" for the first sheet.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workbook path relative to the workspace root." },
                    "sheet": { "type": "string", "description": "Sheet name; the first sheet if omitted." }
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: XLSX_WRITE_TOOL.to_string(),
            description: "Create a NEW .xlsx workbook with one sheet of rows. Replaces the file if it exists; to change one cell of an existing workbook use xlsx_set_cell instead, which keeps the rest of it.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workbook path relative to the workspace root." },
                    "sheet": { "type": "string", "description": "Name for the sheet." },
                    "rows": {
                        "type": "array",
                        "description": "Rows, each an array of cell values as strings.",
                        "items": { "type": "array", "items": { "type": "string" } }
                    }
                },
                "required": ["path", "sheet", "rows"]
            }),
        },
        ToolSpec {
            name: XLSX_SET_CELL_TOOL.to_string(),
            description: "Set one cell of an EXISTING .xlsx workbook, keeping every other sheet, cell and format as it was.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Workbook path relative to the workspace root." },
                    "sheet": { "type": "string", "description": "Sheet name." },
                    "cell": { "type": "string", "description": "A1-style cell reference, e.g. B7." },
                    "value": { "type": "string", "description": "New cell value." }
                },
                "required": ["path", "sheet", "cell", "value"]
            }),
        },
    ]);
    #[cfg(feature = "docx")]
    v.extend([
        ToolSpec {
            name: DOCX_READ_TOOL.to_string(),
            description: "Read the text of a .docx Word document in the workspace.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "Document path relative to the workspace root." } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: DOCX_WRITE_TOOL.to_string(),
            description: "Create a NEW .docx Word document from paragraphs. There is no in-place edit for Word: to change an existing document, read it and write a new one, accepting that formatting this crate does not model is not carried over.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Document path relative to the workspace root." },
                    "paragraphs": { "type": "array", "description": "Paragraphs, in order.", "items": { "type": "string" } }
                },
                "required": ["path", "paragraphs"]
            }),
        },
    ]);
    #[cfg(feature = "pptx")]
    v.push(ToolSpec {
        name: PPTX_READ_TOOL.to_string(),
        description: "Read the text of a .pptx slide deck, slide by slide. Reading only — this crate cannot write PowerPoint.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Deck path relative to the workspace root." } },
            "required": ["path"]
        }),
    });
    #[cfg(feature = "pdf")]
    v.extend([
        ToolSpec {
            name: PDF_READ_TOOL.to_string(),
            description: "Extract the text of a PDF. Best effort: reading order across columns and tables is not guaranteed.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": { "path": { "type": "string", "description": "PDF path relative to the workspace root." } },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: PDF_WRITE_TOOL.to_string(),
            description: "Create a NEW PDF, one page per string.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "PDF path relative to the workspace root." },
                    "pages": { "type": "array", "description": "Page text, one entry per page.", "items": { "type": "string" } }
                },
                "required": ["path", "pages"]
            }),
        },
        ToolSpec {
            name: PDF_WATERMARK_TOOL.to_string(),
            description: "Stamp text across every page of an existing PDF, keeping its content.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "PDF path relative to the workspace root." },
                    "text": { "type": "string", "description": "Watermark text." }
                },
                "required": ["path", "text"]
            }),
        },
        ToolSpec {
            name: PDF_FILL_FORM_TOOL.to_string(),
            description: "Fill the form fields of an existing PDF, by field name.".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "PDF path relative to the workspace root." },
                    "fields": { "type": "object", "description": "Field name to value.", "additionalProperties": { "type": "string" } }
                },
                "required": ["path", "fields"]
            }),
        },
    ]);
    #[cfg(feature = "barcode")]
    v.push(ToolSpec {
        name: BARCODE_DECODE_TOOL.to_string(),
        description: "Decode barcodes and QR codes from a PNG or JPEG in the workspace. Reports plainly when the image contains none.".to_string(),
        parameters: json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "Image path relative to the workspace root." } },
            "required": ["path"]
        }),
    });
    v
}
