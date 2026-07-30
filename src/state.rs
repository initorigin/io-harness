//! Run state in rusqlite: the full trace of a run — prompts, decisions, tool
//! calls, token usage, and outcome — readable back afterwards for audit, and
//! enough to resume an interrupted run under the same run id.
//!
//! The 0.2.0 schema adds `prompt`, `tool_call`, and `tokens` columns to `steps`.
//! An existing 0.1.0 database is migrated in place with `ALTER TABLE ADD COLUMN`
//! (additive — a 0.1.0 binary still reads a migrated database).

use rusqlite::Connection;

use crate::context::{ObsKind, Observation};
use crate::error::{Error, Result};
use crate::policy::Policy;
use crate::pricing::{PriceTable, Spend};
use crate::provider::Usage;

/// The group a call with no recorded model falls into. Named rather than
/// silently merged into a neighbour, and counted as unpriced.
///
/// ```
/// use io_harness::pricing::PriceTable;
/// use io_harness::{ProviderCall, Store, Usage, UNKNOWN_MODEL};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("goal", "NOTES.md")?;
/// // A custom provider that reports no model — a test double, or a wire that
/// // omits it. The tokens are real and are summed; the money is not knowable.
/// store.record_provider_call(run_id, &ProviderCall {
///     step: 1,
///     provider: "scripted".into(),
///     usage: Some(Usage { total_tokens: 12, ..Default::default() }),
///     ..Default::default()
/// })?;
///
/// let spend = store.spend_by_model(&PriceTable::new("2026-07-29"))?;
/// assert_eq!(spend[0].key, UNKNOWN_MODEL);
/// assert_eq!(spend[0].usage.total_tokens, 12);
/// // Counted as unpriced rather than as free, which is the distinction the
/// // whole accounting release rests on.
/// assert_eq!((spend[0].cost_micros, spend[0].unpriced_calls), (0, 1));
/// # Ok(())
/// # }
/// ```
pub const UNKNOWN_MODEL: &str = "(unknown model)";

/// An observation kind as it is stored: the serde rendering, not
/// [`ObsKind::label`], which is English for a prompt reader and renders `Write`
/// as "wrote". Going through serde rather than a hand-written match is what
/// keeps the mapping total — a new variant cannot be added without a rendering,
/// and the pair below cannot drift apart.
fn kind_wire(kind: ObsKind) -> String {
    match serde_json::to_value(kind) {
        Ok(serde_json::Value::String(s)) => s,
        // Unreachable for a unit-variant enum with `rename_all`; falling back to
        // the debug form keeps the write infallible without inventing a kind that
        // would read back as a different observation.
        other => format!("{other:?}"),
    }
}

/// The inverse of [`kind_wire`]. A kind that does not parse is an error and not
/// a skipped row: a ledger that came back silently shorter than it was would
/// assemble a context nobody can account for, which is worse than refusing to
/// restore it at all.
fn kind_from_wire(kind: &str, run_id: i64) -> Result<ObsKind> {
    serde_json::from_value(serde_json::Value::String(kind.to_string())).map_err(|e| Error::Resume {
        reason: format!("run {run_id} has a ledger observation of unknown kind {kind:?}: {e}"),
    })
}

/// The checkpoint layout version stamped into `PRAGMA user_version`. Bump when
/// the on-disk checkpoint format changes incompatibly. A store whose version is
/// higher than this is from a newer binary and is refused on resume.
///
/// The reason a caller ever reads it: a resume against a store a *newer*
/// io-harness wrote fails, typed, before anything is replayed — and this constant
/// is the version to name when reporting that.
///
/// ```
/// use io_harness::{Error, Store, CHECKPOINT_FORMAT};
///
/// # fn main() -> io_harness::Result<()> {
/// let path = std::env::temp_dir().join("io-harness-doc-newer-checkpoint.sqlite3");
/// let _ = std::fs::remove_file(&path);
/// {
///     // Stand in for a store written by a future release.
///     let conn = rusqlite::Connection::open(&path).unwrap();
///     conn.pragma_update(None, "user_version", CHECKPOINT_FORMAT + 1).unwrap();
/// }
///
/// let store = Store::open(&path)?;
/// match store.check_resumable(1) {
///     Err(Error::Resume { reason }) => {
///         // Not a panic, and not a half-resume that reads a layout this binary
///         // does not understand. Tell the operator what to do about it.
///         assert!(reason.contains("newer"), "{reason}");
///         eprintln!("this binary understands checkpoint format {CHECKPOINT_FORMAT}: {reason}");
///     }
///     other => panic!("a newer store must refuse to resume, got {other:?}"),
/// }
/// # let _ = std::fs::remove_file(&path);
/// # Ok(())
/// # }
/// ```
pub const CHECKPOINT_FORMAT: i64 = 7;

/// The one outcome string that means the run did what it was asked.
///
/// Named rather than inlined so that [`RunSummary::success`] and any future
/// reader agree by construction. Eleven outcome strings exist; exactly one of
/// them is the task being done.
///
/// ```
/// use io_harness::{Store, SUCCESS_OUTCOME};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let worked = store.start_run("add a hello function", "src/hello.rs")?;
/// let gave_up = store.start_run("add a goodbye function", "src/bye.rs")?;
/// store.finish_run(worked, SUCCESS_OUTCOME)?;
/// store.finish_run(gave_up, "step_cap_reached")?;
///
/// // Scoring a batch of runs: compare against this rather than against the
/// // literal `"success"`, so a reader and the crate cannot drift apart about
/// // which of the eleven endings counts.
/// let succeeded = |id| -> io_harness::Result<bool> {
///     Ok(store.outcome(id)?.as_deref() == Some(SUCCESS_OUTCOME))
/// };
/// assert!(succeeded(worked)?);
/// assert!(!succeeded(gave_up)?);
///
/// // Which is exactly what `RunSummary::success` already carries, computed the
/// // same way at the moment the run ended.
/// assert_eq!(store.run_summary(worked)?.map(|s| s.success), Some(true));
/// # Ok(())
/// # }
/// ```
pub const SUCCESS_OUTCOME: &str = "success";

/// The durable lifecycle status of a run, so a caller can tell a crashed run
/// (still `Running`) from one paused for a human (`Paused`) or finished
/// (`Completed`). OS- and rusqlite-free, so it is safe in the public API.
///
/// ```
/// use io_harness::{RunStatus, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let crashed = store.start_run("summarise the README", "NOTES.md")?;
/// let waiting = store.start_run("rewrite the changelog", "CHANGELOG.md")?;
/// let done = store.start_run("add a hello function", "src/hello.rs")?;
/// store.finish_run(waiting, "awaiting_approval")?;
/// store.finish_run(done, "success")?;
///
/// // The triage a supervisor does on startup. A run left `Running` by a store
/// // nobody is driving is one whose process died mid-loop: resume it. `Paused` is
/// // waiting on a human and resumes with their decision, not without it.
/// let resumable: Vec<i64> = store
///     .runs()?
///     .into_iter()
///     .filter(|id| store.run_status(*id).ok().flatten() == Some(RunStatus::Running))
///     .collect();
/// assert_eq!(resumable, [crashed]);
/// assert_eq!(store.run_status(waiting)?, Some(RunStatus::Paused));
/// assert_eq!(store.run_status(done)?, Some(RunStatus::Completed));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunStatus {
    /// The run is in progress — or was, until the process died mid-loop. A
    /// `Running` run found in a store is the resume target.
    Running,
    /// The run paused for a human decision and can be resumed once it arrives.
    Paused,
    /// The run finished (with success or a terminal budget/deny outcome).
    Completed,
    /// The run ended in an error.
    Failed,
}

impl RunStatus {
    fn from_str(s: &str) -> Self {
        match s {
            "paused" => RunStatus::Paused,
            "completed" => RunStatus::Completed,
            "failed" => RunStatus::Failed,
            _ => RunStatus::Running,
        }
    }
}

/// A persisted spawned-child contract, enough to rebuild and resume that exact
/// child on a tree resume rather than spawning a duplicate.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let parent = store.start_run("summarise the repo", "NOTES.md")?;
/// let child = store.start_child_run("summarise src/", "NOTES.md", parent, 1)?;
/// store.record_spawn(parent, 4, child, "summarise src/", "NOTES.md", "#", Some(8), "[]")?;
///
/// // What a tree resume does with it: the parent replays step 4, looks the spawn
/// // up by (parent, step, goal), and adopts the child it already made. Without
/// // this row the replay would spawn a second child and spend the tree's ledger
/// // twice for one piece of work.
/// let row = store.find_spawn(parent, 4, "summarise src/")?.expect("recorded above");
/// assert_eq!(row.child_run_id, child);
/// assert_eq!(row.max_steps, Some(8));
/// // The narrowing the parent applied is stored too, so the adopted child resumes
/// // under the policy it was contained by rather than the parent's wider one.
/// assert_eq!(row.deny_write, "[]");
///
/// // A step that never spawned has no row, which is how a replay tells "already
/// // done" from "not done yet".
/// assert!(store.find_spawn(parent, 5, "summarise src/")?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct SpawnRow {
    /// The child run id already allocated for this spawn.
    pub child_run_id: i64,
    /// The child's goal.
    pub goal: String,
    /// The workspace-relative file the child's verification reads.
    pub verify_file: String,
    /// The substring the child's verification requires.
    pub needle: String,
    /// The child's step cap, if the parent set one.
    pub max_steps: Option<u32>,
    /// JSON array of `deny_write` globs the parent narrowed the child with.
    pub deny_write: String,
}

/// What one finished run cost and whether it worked.
///
/// Written once by [`Store::finish_run`] and read back with
/// [`Store::run_summary`]. Before 0.12.0 a consumer had to assemble this itself
/// from three different queries plus knowledge of which of eleven outcome
/// strings count as success — and could not get `duration_ms` at all, because
/// nothing recorded when a run ended.
///
/// Serialisable, so a scoring tool can store or ship it without restating the
/// shape.
///
/// ```
/// use io_harness::{Store, StepRecord};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("add a hello function", "src/hello.rs")?;
/// store.record(run_id, &StepRecord::new(1, "wrote src/hello.rs", "ok").with_trace("", "", 1_280))?;
/// store.finish_run(run_id, "step_cap_reached")?;
///
/// // One row instead of three queries plus knowledge of which of eleven outcome
/// // strings mean success. This is what a scoring tool reads per run.
/// let summary = store.run_summary(run_id)?.expect("the run has finished");
/// assert!(!summary.success);
/// // `outcome` and `success` are both kept on purpose: the flag says whether it
/// // worked, the string says *how* it ended, and a step cap, a stall and a
/// // human's refusal are three different things to act on.
/// assert_eq!(summary.outcome, "step_cap_reached");
/// assert_eq!((summary.steps, summary.tokens), (1, 1_280));
///
/// // `None` while a run is unfinished or paused for a human — absent rather than
/// // a row of zeroes, which would read like a run that did nothing.
/// let paused = store.start_run("rewrite the changelog", "CHANGELOG.md")?;
/// store.finish_run(paused, "awaiting_approval")?;
/// assert!(store.run_summary(paused)?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RunSummary {
    /// The run this describes.
    pub run_id: i64,
    /// The raw outcome string, as written to `runs.outcome`.
    ///
    /// Kept alongside [`Self::success`] rather than replaced by it: the string
    /// says *which* ending, the flag says whether it was the good one, and
    /// collapsing them would throw away the distinction between a step cap, a
    /// stall and a human's refusal.
    pub outcome: String,
    /// Whether the run achieved what it was asked to do.
    ///
    /// True for exactly one outcome — `success`. Every other ending, including
    /// the ones that are nobody's fault like a rate-limited provider, is not the
    /// task being done.
    pub success: bool,
    /// Steps completed, as `MAX(step)`.
    ///
    /// Not `COUNT(*)` over `steps`: a retry writes its own row under the same
    /// step number, so counting rows counts trace entries rather than agent
    /// steps. For a tree this is the ROOT agent's step count, not the tree's
    /// total — each agent has its own summary.
    pub steps: u32,
    /// Tokens spent by this run, summed from its committed steps.
    ///
    /// Tokens, not money. A provider reports usage and never a price, so the
    /// crate has nothing to convert with; see
    /// [`Containment::max_total_cost`](crate::Containment::max_total_cost).
    pub tokens: u64,
    /// Wall-clock milliseconds from the run's start to its end.
    ///
    /// `None` for a run started before 0.7.0, which has no `started_at` to
    /// measure from. Includes time the process was not running — a run that
    /// crashed at midnight and resumed at nine counts the nine hours, because
    /// that is how long the run took even though it was not working.
    pub duration_ms: Option<u64>,
    /// When the run ended, from the database clock.
    pub finished_at: String,
}

/// A persisted run store. Use [`Store::open`] for a file, or [`Store::memory`]
/// for an ephemeral in-memory database.
///
/// The store is not a log. It is what makes a run resumable after a crash and
/// auditable afterwards, so which constructor you choose is a decision about
/// whether either matters for this run.
///
/// ```no_run
/// use io_harness::{run, OpenRouter, Store, TaskContract, Verification};
///
/// # async fn demo() -> io_harness::Result<()> {
/// // A file store outlives the process: a run that dies mid-loop is resumable
/// // from its last committed step, and a second process may read the trace while
/// // this one is still writing it (WAL, plus `BUSY_TIMEOUT`).
/// let store = Store::open("runs.sqlite3")?;
///
/// let contract = TaskContract::new(
///     "add a hello function returning 42",
///     "src/hello.rs",
///     Verification::FileContains("fn hello".into()),
/// );
/// let result = run(&contract, &OpenRouter::from_env()?, &store).await?;
///
/// // Everything the run did, read back by id: the trace, the budget draws, the
/// // policy refusals, and what it cost.
/// for step in store.steps(result.run_id)? {
///     println!("{}: {} ({} tokens)", step.step, step.decision, step.tokens);
/// }
/// println!("{:?}", store.run_summary(result.run_id)?);
/// # Ok(())
/// # }
/// ```
///
/// [`Store::memory`] is the same API with no file: a throwaway run, or a test.
/// Nothing survives the process, so nothing is resumable — which is the right
/// trade only when a failed run is cheaper to restart than to continue.
pub struct Store {
    conn: Connection,
}

/// One durable checkpoint-lifecycle event: a step was checkpointed, a run was
/// resumed, or an already-committed step was skipped on resume. Together they
/// make a crashed-and-resumed run's history reconstructable from the store.
///
/// ```
/// use io_harness::{CheckpointEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("summarise the repo", "NOTES.md")?;
/// # store.record_checkpoint_event(&CheckpointEvent::checkpoint(run_id, 1))?;
/// # store.record_checkpoint_event(&CheckpointEvent::checkpoint(run_id, 2))?;
/// # store.record_checkpoint_event(&CheckpointEvent::resume(run_id, 3, "restarted after a crash"))?;
/// # store.record_checkpoint_event(&CheckpointEvent::skipped(run_id, 1))?;
/// # store.record_checkpoint_event(&CheckpointEvent::skipped(run_id, 2))?;
/// // Answers the question a crashed run leaves behind: did it restart, and did
/// // the restart re-do work that was already committed?
/// let events = store.checkpoint_events(run_id)?;
/// let resumes = events.iter().filter(|e| e.kind == "resume").count();
/// let replayed: Vec<u32> = events.iter().filter(|e| e.kind == "skipped").map(|e| e.step).collect();
///
/// assert_eq!(resumes, 1, "this run died once and came back");
/// // Two steps were replayed and recognised as already done, so they cost
/// // nothing the second time — that is what makes a resume idempotent rather
/// // than a second charge for the same steps.
/// assert_eq!(replayed, [1, 2]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct CheckpointEvent {
    /// The run this event belongs to.
    pub run_id: i64,
    /// The step it concerns.
    pub step: u32,
    /// `"checkpoint"`, `"resume"`, or `"skipped"`.
    pub kind: String,
    /// Optional human-readable detail (never file contents or secrets).
    pub detail: Option<String>,
}

impl CheckpointEvent {
    /// A step was durably checkpointed.
    pub fn checkpoint(run_id: i64, step: u32) -> Self {
        Self {
            run_id,
            step,
            kind: "checkpoint".into(),
            detail: None,
        }
    }
    /// A run was resumed, re-driving from `step`.
    pub fn resume(run_id: i64, step: u32, detail: impl Into<String>) -> Self {
        Self {
            run_id,
            step,
            kind: "resume".into(),
            detail: Some(detail.into()),
        }
    }
    /// An already-committed step was skipped on resume.
    pub fn skipped(run_id: i64, step: u32) -> Self {
        Self {
            run_id,
            step,
            kind: "skipped".into(),
            detail: None,
        }
    }
}

/// One recorded loop step — the full trace entry, as written and read back.
///
/// ```
/// use io_harness::{StepRecord, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("add a hello function", "src/hello.rs")?;
///
/// // A transient provider failure, and then the step that worked. Both are step
/// // 1: a retry writes its own row under the step number it retried.
/// store.record(run_id, &StepRecord::new(1, "retry 1 after a 503", ""))?;
///
/// // `new` alone records a decision and its result; `with_trace` adds the audit
/// // half — the exact prompt sent, the call the model made, and what it cost.
/// // Without it the trace says what happened and not why.
/// store.record(
///     run_id,
///     &StepRecord::new(1, "wrote src/hello.rs", "ok").with_trace(
///         "<the assembled prompt>",
///         r#"{"name":"write_file","arguments":{"path":"src/hello.rs"}}"#,
///         1_280,
///     ),
/// )?;
///
/// // Read back for audit — and the reason `RunSummary::steps` is `MAX(step)`
/// // rather than a row count: two rows here, one agent step.
/// let steps = store.steps(run_id)?;
/// assert_eq!(steps.len(), 2);
/// assert_eq!(store.last_step(run_id)?, 1);
/// assert!(steps[1].tool_call.contains("write_file"));
/// assert_eq!(steps.iter().map(|s| s.tokens).sum::<u64>(), 1_280);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct StepRecord {
    /// 1-based step number within the run.
    pub step: u32,
    /// What the agent decided this step (e.g. "wrote file", "retry 1 after error").
    pub decision: String,
    /// Intermediate result / model text for the step.
    pub result: String,
    /// The prompt sent to the model this step.
    pub prompt: String,
    /// The tool call the model made, as JSON, or "" if none.
    pub tool_call: String,
    /// Total tokens used this step, 0 if the provider reported none.
    pub tokens: u64,
}

impl StepRecord {
    /// A trace entry with the audit fields empty — for callers that only record
    /// a decision and result.
    pub fn new(step: u32, decision: impl Into<String>, result: impl Into<String>) -> Self {
        Self {
            step,
            decision: decision.into(),
            result: result.into(),
            prompt: String::new(),
            tool_call: String::new(),
            tokens: 0,
        }
    }

    /// Attach the prompt, tool call, and token count for the full trace.
    pub fn with_trace(
        mut self,
        prompt: impl Into<String>,
        tool_call: impl Into<String>,
        tokens: u64,
    ) -> Self {
        self.prompt = prompt.into();
        self.tool_call = tool_call.into();
        self.tokens = tokens;
        self
    }
}

/// One policy event in the trace: an action refused, or a human decision.
///
/// Records the path, command, rule, layer, and decision — never file contents
/// or credentials. (The write payload of a *deferred* action is held separately
/// in the pending-approval row, because resuming it requires replaying exactly
/// what was approved.)
///
/// ```
/// use io_harness::{PolicyEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("tidy the repo", "src/lib.rs")?;
/// # store.record_event(run_id, &PolicyEvent::refusal(3, "write", "secrets/id_rsa")
/// #     .with_rule("secrets/*", "app"))?;
/// # store.record_event(run_id, &PolicyEvent::decision(4, "exec", "git push", "approve", "stdin")
/// #     .with_performed("git push --dry-run"))?;
/// // The audit an operator actually asks for: what did the agent try that it was
/// // not allowed to do, and which rule stopped it?
/// let events = store.events(run_id)?;
/// let refused = events.iter().find(|e| e.kind == "refusal").expect("one refusal");
/// assert_eq!(refused.target, "secrets/id_rsa");
/// // Attributed to the rule and the layer, so "why was this denied" is answered
/// // from the store rather than by re-deriving the policy stack by hand.
/// assert_eq!((refused.rule.as_deref(), refused.layer.as_deref()), (Some("secrets/*"), Some("app")));
///
/// // And the case that is easy to miss: an approval that changed the action. The
/// // agent asked for one command and a human let a different one through.
/// let approved = events.iter().find(|e| e.kind == "decision").expect("one decision");
/// assert_eq!(approved.decision.as_deref(), Some("approve"));
/// assert_eq!(approved.performed.as_deref(), Some("git push --dry-run"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyEvent {
    /// 1-based step the event occurred on.
    pub step: u32,
    /// `"refusal"` or `"decision"`.
    pub kind: String,
    /// `"read"`, `"write"`, or `"exec"`.
    pub act: String,
    /// The path, or the binary name plus argv for an exec.
    pub target: String,
    /// The glob that decided, when a rule rather than a tier default did.
    pub rule: Option<String>,
    /// The layer the deciding rule came from.
    pub layer: Option<String>,
    /// `"approve"`, `"deny"`, or `"defer"` for a decision.
    pub decision: Option<String>,
    /// Which approver decided, or `"remembered"` when a remembered rule did.
    pub source: Option<String>,
    /// The action actually performed, when approve-with-changes altered it.
    pub performed: Option<String>,
}

impl PolicyEvent {
    /// An action refused by the policy.
    pub fn refusal(step: u32, act: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            step,
            kind: "refusal".into(),
            act: act.into(),
            target: target.into(),
            rule: None,
            layer: None,
            decision: None,
            source: None,
            performed: None,
        }
    }

    /// A human (or built-in approver) decision on a sensitive action.
    pub fn decision(
        step: u32,
        act: impl Into<String>,
        target: impl Into<String>,
        decision: impl Into<String>,
        source: impl Into<String>,
    ) -> Self {
        Self {
            kind: "decision".into(),
            decision: Some(decision.into()),
            source: Some(source.into()),
            ..Self::refusal(step, act, target)
        }
    }

    /// Attribute the event to the rule and layer that produced it.
    pub fn with_rule(mut self, rule: impl Into<String>, layer: impl Into<String>) -> Self {
        self.rule = Some(rule.into());
        self.layer = Some(layer.into());
        self
    }

    /// Record that the action performed differed from the one requested.
    pub fn with_performed(mut self, performed: impl Into<String>) -> Self {
        self.performed = Some(performed.into());
        self
    }
}

/// An action paused awaiting a human decision, persisted so it outlives the
/// process that requested it.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("update the deploy config", "deploy/prod.yaml")?;
///
/// // The agent asked to write a production file, the policy said `Ask`, and the
/// // approver deferred. The payload is stored with it, because resuming has to
/// // replay exactly what was approved and not whatever the model would say now.
/// let request_id = store.put_pending(run_id, 2, "write", "deploy/prod.yaml", Some("replicas: 4"))?;
///
/// // This process may now exit. A reviewer — a web UI, a CLI, a person the next
/// // morning — reads the request back and decides.
/// let pending = store.pending(request_id)?.expect("just written");
/// assert_eq!((pending.act.as_str(), pending.target.as_str()), ("write", "deploy/prod.yaml"));
/// assert_eq!(pending.content.as_deref(), Some("replicas: 4"));
/// assert!(pending.resolved.is_none(), "nobody has decided yet");
///
/// store.resolve_pending(request_id, "approve")?;
/// assert_eq!(store.pending(request_id)?.unwrap().resolved.as_deref(), Some("approve"));
/// // `resume_with_decision` then continues the run from here.
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The request id, as returned by [`Store::put_pending`].
    pub id: i64,
    /// The run this action belongs to.
    pub run_id: i64,
    /// The step it paused on.
    pub step: u32,
    /// `"read"`, `"write"`, or `"exec"`.
    pub act: String,
    /// The target path or binary.
    pub target: String,
    /// The write payload, needed to replay exactly what was approved.
    pub content: Option<String>,
    /// `None` while pending; otherwise `"approve"` or `"deny"`.
    pub resolved: Option<String>,
}

/// One event in a tree of agents: a parent spawning a child, a spawn refused by
/// the containment boundary, or a draw against the tree's shared spend ceiling.
///
/// Together with each run's `parent_run_id` these make the tree a reconstructable
/// graph — who spawned whom, what was refused, and what the tree spent — long
/// after the process that ran it has exited.
///
/// ```
/// use io_harness::{AgentEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("summarise the repo", "NOTES.md")?;
/// # store.record_agent_event(&AgentEvent::budget_draw(run_id, 1, 1_280, 8_720))?;
/// # store.record_agent_event(&AgentEvent::spawn(run_id, 2, 2, "summarise src/"))?;
/// # store.record_agent_event(&AgentEvent::budget_draw(run_id, 2, 4_100, 4_620))?;
/// # store.record_agent_event(&AgentEvent::spawn_refused(run_id, 3, "agents"))?;
/// # store.record_agent_event(&AgentEvent::budget_draw(run_id, 3, 3_900, 720))?;
/// // The only audit of what each step drew against the tree's *shared* ceiling.
/// // A run's own token total does not show this: the ceiling is tree-wide, so
/// // what matters is how fast `remaining` is falling for everyone.
/// let events = store.agent_events(run_id)?;
///
/// let drawn: u64 = events.iter().filter_map(|e| e.tokens).sum();
/// let left = events.iter().filter_map(|e| e.remaining).last();
/// assert_eq!(drawn, 9_280);
/// // 720 left after three steps that averaged over 3,000: this tree ends in
/// // `BudgetCeilingReached` next step, and the row says so before it happens.
/// assert_eq!(left, Some(720));
///
/// // The same table carries the shape of the tree and what it was denied.
/// let children: Vec<i64> = events.iter().filter_map(|e| e.child_run_id).collect();
/// let refusals: Vec<&str> = events
///     .iter()
///     .filter(|e| e.kind == "spawn_refused")
///     .filter_map(|e| e.detail.as_deref())
///     .collect();
/// assert_eq!(children, [2]);
/// // A parent that wanted a second child and hit the agent cap — a run doing
/// // less work than it planned to, which is invisible in its outcome.
/// assert_eq!(refusals, ["agents"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentEvent {
    /// The agent this event belongs to (the parent, for a spawn; the drawing
    /// agent, for a budget draw).
    pub run_id: i64,
    /// The step it occurred on.
    pub step: u32,
    /// `"spawn"`, `"spawn_refused"`, or `"budget_draw"`.
    pub kind: String,
    /// The spawned child's run id, for a `"spawn"`.
    pub child_run_id: Option<i64>,
    /// Free-form detail: the child's goal for a spawn, the breached cap for a
    /// refusal.
    pub detail: Option<String>,
    /// Tokens drawn, for a `"budget_draw"`.
    pub tokens: Option<u64>,
    /// The tree's remaining tokens after the draw.
    pub remaining: Option<u64>,
}

impl AgentEvent {
    /// A parent spawned a child.
    pub fn spawn(run_id: i64, step: u32, child_run_id: i64, goal: impl Into<String>) -> Self {
        Self {
            run_id,
            step,
            kind: "spawn".into(),
            child_run_id: Some(child_run_id),
            detail: Some(goal.into()),
            tokens: None,
            remaining: None,
        }
    }

    /// A spawn was refused by the containment boundary.
    pub fn spawn_refused(run_id: i64, step: u32, cap: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "spawn_refused".into(),
            child_run_id: None,
            detail: Some(cap.into()),
            tokens: None,
            remaining: None,
        }
    }

    /// An agent drew `tokens` against the tree, leaving `remaining`.
    pub fn budget_draw(run_id: i64, step: u32, tokens: u64, remaining: u64) -> Self {
        Self {
            run_id,
            step,
            kind: "budget_draw".into(),
            child_run_id: None,
            detail: None,
            tokens: Some(tokens),
            remaining: Some(remaining),
        }
    }
}

/// One event in the life of a sandboxed execution: the sandbox created for a
/// run, a command run in it (with the backend that isolated it), a resource cap
/// that killed it, a denied network attempt, or the sandbox torn down.
///
/// Together these let an operator audit not just *what* code ran but *where* and
/// *how* it was isolated, reconstructable from the store alone after the run.
///
/// ```
/// use io_harness::{SandboxEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("make the tests pass", "src/lib.rs")?;
/// # store.record_sandbox_event(&SandboxEvent::create(run_id, 4, "macos-sandbox-exec"))?;
/// # store.record_sandbox_event(&SandboxEvent::exec(run_id, 4, "macos-sandbox-exec", "rustc --test subject.rs"))?;
/// # store.record_sandbox_event(&SandboxEvent::gate_phase_failed(run_id, 4, "criterion-compile"))?;
/// # store.record_sandbox_event(&SandboxEvent::destroy(run_id, 4))?;
/// let events = store.sandbox_events(run_id)?;
///
/// // Which backend actually isolated the run. The crate picks a native one per
/// // platform and falls back to a portable floor, so "was this really contained"
/// // is a question about this field and not about the configuration.
/// let backend = events.iter().find_map(|e| e.backend.as_deref());
/// assert_eq!(backend, Some("macos-sandbox-exec"));
///
/// // And the phase to look for when a run that used to pass stops passing:
/// // `criterion-compile` means the criterion no longer compiles *against* the
/// // subject, which before 0.8.1 could be reported as a pass.
/// let phases: Vec<&str> = events
///     .iter()
///     .filter(|e| e.kind == "gate_phase_failed")
///     .filter_map(|e| e.detail.as_deref())
///     .collect();
/// assert_eq!(phases, ["criterion-compile"]);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxEvent {
    /// The run this execution belongs to.
    pub run_id: i64,
    /// The step it occurred on.
    pub step: u32,
    /// `"create"`, `"exec"`, `"cap_hit"`, `"destroy"`, `"gate_phase_failed"`
    /// (whose `detail` names the phase), or `"gate_output"` (whose `detail` is
    /// what a failing gate command printed).
    ///
    /// A `"net_deny"` kind was documented here from 0.6.0 to 0.11.0 and never
    /// existed: nothing constructed it and nothing emitted it. It was removed in
    /// 0.12.0 rather than implemented, because a sandbox denies egress
    /// *structurally* — the backend gives the child no route out, so there is no
    /// attempt to observe and nothing to count. Network decisions the harness
    /// actually makes are in `policy_events` with `act = "net"`.
    pub kind: String,
    /// The backend that isolated the run (e.g. `"macos-sandbox-exec"`).
    pub backend: Option<String>,
    /// The argv for an `"exec"`, the breached cap for a `"cap_hit"`, or the
    /// bounded output of a failing gate command for a `"gate_output"`. Never the
    /// agent's file contents and never credentials — the command line, or what
    /// the caller's own criterion printed.
    pub detail: Option<String>,
}

impl SandboxEvent {
    /// A sandbox was created for a run, isolated by `backend`.
    pub fn create(run_id: i64, step: u32, backend: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "create".into(),
            backend: Some(backend.into()),
            detail: None,
        }
    }

    /// A command ran in the sandbox under `backend`.
    pub fn exec(run_id: i64, step: u32, backend: &str, argv: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "exec".into(),
            backend: Some(backend.into()),
            detail: Some(argv.into()),
        }
    }

    /// A resource cap killed the run.
    pub fn cap_hit(run_id: i64, step: u32, cap: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "cap_hit".into(),
            backend: None,
            detail: Some(cap.into()),
        }
    }

    /// The sandbox was torn down (workdir removed, processes reaped).
    pub fn destroy(run_id: i64, step: u32) -> Self {
        Self {
            run_id,
            step,
            kind: "destroy".into(),
            backend: None,
            detail: None,
        }
    }

    /// Which phase of an execution gate failed: `"subject-compile"` (the file
    /// under verification does not compile), `"criterion-compile"` (the
    /// criterion does not compile *against* it), `"test-run"` (it compiled and
    /// the test failed), or `"subject-emptied"` (the file compiled but a
    /// crate-level attribute stripped its items, so nothing was type-checked —
    /// the compile-only gates).
    ///
    /// 0.8.1 added this because the release deliberately makes some previously
    /// passing runs fail. `criterion-compile` is the one to look for: before
    /// 0.8.1 the subject and the criterion were one crate, so a subject could
    /// shadow the names the criterion used — or delete it outright — and be
    /// reported as passing. An operator whose run stopped passing on upgrade can
    /// tell that case from an ordinary failed criterion without reading the
    /// harness's source.
    ///
    /// A new `kind` value, not a new table or column: a 0.8.0 store takes it
    /// with no migration.
    pub fn gate_phase_failed(run_id: i64, step: u32, phase: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "gate_phase_failed".into(),
            backend: None,
            detail: Some(phase.into()),
        }
    }

    /// What a failing gate command printed, already bounded by the caller
    /// (0.17.0).
    ///
    /// [`Verification::Command`](crate::Verification::Command) can run any
    /// command in any language, so "the criterion did not pass" stops being a
    /// self-explaining outcome: `cargo test` failing because the agent's change
    /// is wrong and `npm test` failing because the machine has no `node_modules`
    /// are the same discriminant and need opposite responses. The command's own
    /// output is the only thing that tells them apart.
    ///
    /// This is the one `detail` that is not a command line. It is a *gate*
    /// command's output — a caller-supplied criterion, never the agent's file
    /// contents — and it is what makes a language-agnostic gate diagnosable at
    /// all. Bounded by the caller before it arrives here.
    ///
    /// ```
    /// use io_harness::{SandboxEvent, Store};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// let store = Store::memory()?;
    /// let run_id = store.start_run("make the suite pass", "/repo")?;
    /// store.record_sandbox_event(&SandboxEvent::gate_output(
    ///     run_id, 3, "FAIL test/parse.test.js\n  expected 2 received 1",
    /// ))?;
    ///
    /// let why = store.sandbox_events(run_id)?;
    /// assert!(why.iter().any(|e| e.kind == "gate_output"
    ///     && e.detail.as_deref().is_some_and(|d| d.contains("expected 2"))));
    /// # Ok(()) }
    /// ```
    ///
    /// A new `kind` value, not a new table or column: a 0.6.0 store takes it with
    /// no migration.
    pub fn gate_output(run_id: i64, step: u32, output: &str) -> Self {
        Self {
            run_id,
            step,
            kind: "gate_output".into(),
            backend: None,
            detail: Some(output.into()),
        }
    }
}

/// One event in the life of an MCP connection: a server connected, a tool it
/// offered, a tool called (with how long it took and whether it worked), or a
/// server disconnected.
///
/// The `net` half of a run's egress history lives in [`PolicyEvent`] — an MCP
/// server's host is checked by the same policy as any other outbound call — so
/// this table is about the MCP conversation itself, not about permission.
///
/// ```
/// use io_harness::{McpEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("summarise the repo", "NOTES.md")?;
/// # store.record_mcp(run_id, &McpEvent::connected("files", "stdio").with_millis(42))?;
/// # store.record_mcp(run_id, &McpEvent::discovered("files", "mcp__files__read"))?;
/// # store.record_mcp(run_id, &McpEvent::discovered("files", "mcp__files__list"))?;
/// # store.record_mcp(run_id, &McpEvent::called("files", "mcp__files__read", true).at_step(2).with_millis(31))?;
/// # store.record_mcp(run_id, &McpEvent::called("files", "mcp__files__read", false).at_step(3).with_millis(30_000).with_detail("timeout"))?;
/// let events = store.mcp_events(run_id)?;
///
/// // What a server actually offered this run — the answer to "why did the model
/// // not use the tool I configured", which is usually that it was never
/// // discovered. Namespaced, so a server can never shadow `write_file`.
/// let offered: Vec<&str> = events
///     .iter()
///     .filter(|e| e.kind == "discovered")
///     .filter_map(|e| e.tool.as_deref())
///     .collect();
/// assert_eq!(offered, ["mcp__files__read", "mcp__files__list"]);
///
/// // And the call that went wrong, with how long it took before it did. Detail
/// // carries a short note only — never arguments or results, which can carry
/// // secrets.
/// let failed = events.iter().find(|e| e.ok == Some(false)).expect("one failure");
/// assert_eq!((failed.step, failed.millis), (3, Some(30_000)));
/// assert_eq!(failed.detail.as_deref(), Some("timeout"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpEvent {
    /// The step it occurred on. `0` for connect/discover, which happen before
    /// the run's first step.
    pub step: u32,
    /// `"connected"`, `"discovered"`, `"called"`, or `"disconnected"`.
    pub kind: String,
    /// The configured server's id.
    pub server: String,
    /// The namespaced tool name, for `"discovered"` and `"called"`.
    pub tool: Option<String>,
    /// Whether a `"called"` tool succeeded.
    pub ok: Option<bool>,
    /// How long a connect or call took, in milliseconds.
    pub millis: Option<u64>,
    /// Transport for a connect, or a note such as `"truncated"`. Never tool
    /// arguments or results — those can carry secrets.
    pub detail: Option<String>,
}

impl McpEvent {
    fn new(kind: &str, server: &str) -> Self {
        Self {
            step: 0,
            kind: kind.into(),
            server: server.into(),
            tool: None,
            ok: None,
            millis: None,
            detail: None,
        }
    }

    /// A server connected over `transport`.
    pub fn connected(server: &str, transport: &str) -> Self {
        Self::new("connected", server).with_detail(transport)
    }

    /// A server offered a tool, under its namespaced name.
    pub fn discovered(server: &str, tool: &str) -> Self {
        let mut e = Self::new("discovered", server);
        e.tool = Some(tool.into());
        e
    }

    /// A tool was called, and whether it worked.
    pub fn called(server: &str, tool: &str, ok: bool) -> Self {
        let mut e = Self::new("called", server);
        e.tool = Some(tool.into());
        e.ok = Some(ok);
        e
    }

    /// A server was disconnected.
    pub fn disconnected(server: &str) -> Self {
        Self::new("disconnected", server)
    }

    /// Attach the step this happened on.
    pub fn at_step(mut self, step: u32) -> Self {
        self.step = step;
        self
    }

    /// Attach a duration in milliseconds.
    pub fn with_millis(mut self, millis: u64) -> Self {
        self.millis = Some(millis);
        self
    }

    /// Attach a short note. An empty note is dropped rather than stored blank.
    pub fn with_detail(mut self, detail: impl Into<String>) -> Self {
        let detail = detail.into();
        self.detail = (!detail.is_empty()).then_some(detail);
        self
    }
}

// ---- 0.10.0: durable cross-run memory ----

/// One durable memory entry: a fact or decision an agent wrote deliberately,
/// keyed to a workspace rather than to a run.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let first = store.start_run("make the tests pass", "src/lib.rs")?;
///
/// // What an agent learned the expensive way, written once so the next run over
/// // this workspace does not spend three steps rediscovering it.
/// store.memory_put("/repo", "test-command", "cargo test --features documents", first, 6)?;
///
/// // A later run — a different process, days afterwards — recalls it by key.
/// let entry = store.memory_get("/repo", "test-command")?.expect("written above");
/// assert_eq!(entry.value, "cargo test --features documents");
/// // Attributed, which is what makes a stale fact traceable: this came from run
/// // 1, step 6, and you can go and read what that step actually did.
/// assert_eq!((entry.run_id, entry.step), (first, 6));
///
/// // Keyed to the workspace, never to the run: another workspace sees nothing.
/// assert!(store.memory_get("/other-repo", "test-command")?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryEntry {
    /// The name it is recalled by, unique within its workspace.
    pub key: String,
    /// The remembered text.
    pub value: String,
    /// The run that wrote it, so a later reader knows where a fact came from.
    pub run_id: i64,
    /// The step of that run which wrote it.
    pub step: u32,
    /// UTC write time, refreshed on every overwrite so ordering is by recency.
    pub created_at: String,
}

/// One turn of a conversation: what was asked, which run answered it, and which
/// earlier turn it answers from.
///
/// A node of the tree [`Session`](crate::Session) walks. `parent_turn_id` is what
/// makes it a tree rather than a list: two turns may share a parent, which is
/// what a branch is, and nothing is rewritten to create one.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let session = store.create_session("/repo")?;
///
/// // A turn is recorded against the run that will serve it, before that run
/// // starts, so a turn whose process dies is still in the tree.
/// let run = store.start_run("where is the retry policy applied?", "/repo")?;
/// let first = store.record_turn(session, None, run, "where is the retry policy applied?")?;
/// store.finish_turn(first, Some("in `complete_with_retry`"), "finished")?;
///
/// let turn = store.session_turn(first)?.expect("recorded above");
/// assert_eq!(turn.run_id, run);
/// assert_eq!(turn.parent_turn_id, None); // the root of this conversation
/// assert_eq!(turn.reply.as_deref(), Some("in `complete_with_retry`"));
///
/// // Everything that turn cost, refused or committed is in the run tables under
/// // `turn.run_id` — a turn adds a conversation over a run, not a second trace.
/// assert!(store.run_status(turn.run_id)?.is_some());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    /// The turn's own id, and what [`Session::branch_from`](crate::Session::branch_from) takes.
    pub id: i64,
    /// The session it belongs to.
    pub session_id: i64,
    /// The turn it answers from, or `None` for the root of a conversation.
    pub parent_turn_id: Option<i64>,
    /// The run that served it. Every step, refusal and budget draw is under this id.
    pub run_id: i64,
    /// What the operator said.
    pub prompt: String,
    /// What the agent said back, when it said anything. `None` while the turn is
    /// still running, and for a turn that stopped without a closing message.
    pub reply: Option<String>,
    /// Why the turn stopped, as the run's outcome string. `None` while it runs.
    pub outcome: Option<String>,
    /// UTC creation time.
    pub created_at: String,
}

/// Read one `session_turns` row. One place, so the two queries that read the
/// table cannot drift in their column order.
fn turn_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Turn> {
    Ok(Turn {
        id: r.get(0)?,
        session_id: r.get(1)?,
        parent_turn_id: r.get(2)?,
        run_id: r.get(3)?,
        prompt: r.get(4)?,
        reply: r.get(5)?,
        outcome: r.get(6)?,
        created_at: r.get(7)?,
    })
}

/// One question the agent asked the operator, and its answer if it has one (0.21.0).
///
/// The mirror of [`Pending`], for the other channel: [`Pending`] is an action awaiting
/// permission, this is an intent awaiting clarification. An answer here is text the
/// model reads and authorizes nothing.
///
/// ```
/// use io_harness::{Question, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("port the parser", "/repo")?;
///
/// let id = store.put_question(run, 3, &Question::new("Which config should I edit?")
///     .with_choices(["io.toml", "io.local.toml"]))?;
///
/// // Unanswered, and readable by whatever process is going to answer it.
/// let q = store.question(id)?.expect("just written");
/// assert!(!q.resolved && q.answer.is_none());
/// assert_eq!(q.choices, ["io.toml", "io.local.toml"]);
///
/// store.answer_question(id, "io.local.toml", "human")?;
/// let q = store.question(id)?.unwrap();
/// assert_eq!(q.answer.as_deref(), Some("io.local.toml"));
/// assert_eq!(q.answered_by.as_deref(), Some("human"));
///
/// // Answering twice is an error, not a silent second write.
/// assert!(store.answer_question(id, "io.toml", "human").is_err());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    /// The question's own id, and what [`resume_with_answer`](crate::resume_with_answer) takes.
    pub id: i64,
    /// The run that asked.
    pub run_id: i64,
    /// The step it was asked on. The step was not committed, so a resume replays it.
    pub step: u32,
    /// What the agent asked.
    pub question: String,
    /// What the agent already knew, if it said.
    pub context: Option<String>,
    /// Options the agent offered. An answer need not be one of them.
    pub choices: Vec<String>,
    /// The answer, once there is one.
    pub answer: Option<String>,
    /// `"responder"` if a [`Responder`](crate::Responder) in the run's own process
    /// answered, `"human"` if the answer arrived through a resume after a pause.
    /// "The machine decided" and "a person decided" are different facts about a run.
    pub answered_by: Option<String>,
    /// Whether it has been answered.
    pub resolved: bool,
}

/// Read one `pending_questions` row. One place, so the three queries that read the
/// table cannot drift in their column order.
fn question_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingQuestion> {
    let choices: Option<String> = r.get(5)?;
    Ok(PendingQuestion {
        id: r.get(0)?,
        run_id: r.get(1)?,
        step: r.get(2)?,
        question: r.get(3)?,
        context: r.get(4)?,
        choices: choices
            .and_then(|c| serde_json::from_str(&c).ok())
            .unwrap_or_default(),
        answer: r.get(6)?,
        answered_by: r.get(7)?,
        resolved: r.get::<_, i64>(8)? != 0,
    })
}

/// Where one item of an agent's plan has got to (0.21.0).
///
/// Three states and no more. A plan is for an operator to read at a glance, and
/// every state past these three — blocked, cancelled, deferred, in-review — is a
/// distinction the harness would have to define, the model would have to choose
/// correctly, and nothing would ever check.
///
/// ```
/// use io_harness::TodoState;
///
/// // The wire form is what the model writes and what the column holds.
/// assert_eq!(TodoState::Active.as_str(), "active");
/// assert_eq!(TodoState::parse("done"), Some(TodoState::Done));
///
/// // An unknown state is not silently a default: a plan that says something the
/// // crate does not understand is a plan whose author should hear about it.
/// assert_eq!(TodoState::parse("blocked"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoState {
    /// Not started.
    Pending,
    /// Being worked on now. Nothing enforces that only one item is active.
    Active,
    /// Finished, as far as the agent is concerned. Nothing verifies it.
    Done,
}

impl TodoState {
    /// The wire and column form.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Active => "active",
            Self::Done => "done",
        }
    }

    /// Parse the wire form, or `None` for anything else.
    ///
    /// `None` rather than a default, so a model that invents a state gets told
    /// instead of having its plan quietly rewritten.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "pending" => Some(Self::Pending),
            "active" => Some(Self::Active),
            "done" => Some(Self::Done),
            _ => None,
        }
    }
}

/// One line of an agent's plan (0.21.0).
///
/// A plan is the agent's *stated intent*: it is written by the agent, read by an
/// operator, and enforced by nothing. No [`RunOutcome`](crate::RunOutcome) depends
/// on it, no verification consults it, and writing one is not an act the policy
/// gates — see the plan section of `docs/CONTRACT.md`. What it buys is a long run
/// that can be recognised as going the wrong way before it ends.
///
/// There is no item id. The whole list is replaced on every write, which is why
/// there is nothing for a model to mis-address.
///
/// ```
/// use io_harness::{Store, TodoItem, TodoState};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("port the parser", "/repo")?;
///
/// store.write_todos(run, &[
///     TodoItem::new("read the current parser", TodoState::Done),
///     TodoItem::new("port the tokenizer", TodoState::Active),
///     TodoItem::new("port the error paths", TodoState::Pending),
/// ])?;
///
/// // Read back in the order it was written — which is the order an operator reads.
/// let plan = store.todos(run)?;
/// assert_eq!(plan.len(), 3);
/// assert_eq!(plan[1].text, "port the tokenizer");
/// assert_eq!(plan[1].state, TodoState::Active);
///
/// // A second write replaces the list rather than merging into it.
/// store.write_todos(run, &[TodoItem::new("ship it", TodoState::Active)])?;
/// assert_eq!(store.todos(run)?.len(), 1);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TodoItem {
    /// What the agent said it would do, in its own words.
    pub text: String,
    /// Where it says that has got to.
    pub state: TodoState,
}

impl TodoItem {
    /// One item.
    pub fn new(text: impl Into<String>, state: TodoState) -> Self {
        Self {
            text: text.into(),
            state,
        }
    }
}

/// Most items one plan may hold.
///
/// A plan longer than this is not a plan an operator reads; it is a transcript.
/// A write past the cap is truncated to the cap rather than refused, and the tool
/// says so in its observation — the same shape as every other bounded tool result
/// in the crate, which caps and tells rather than failing.
pub const TODO_MAX_ITEMS: usize = 64;

/// Longest one item's text may be, in characters.
pub const TODO_TEXT_CAP: usize = 200;

/// Most entries one workspace may hold.
///
/// A write past the cap is not refused — the oldest entry is evicted to make
/// room, and the evicted keys come back so the caller can record the loss in the
/// trace rather than discovering it later as a fact that quietly stopped existing.
///
/// ```
/// use io_harness::{Store, MEMORY_MAX_ENTRIES};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// for i in 0..MEMORY_MAX_ENTRIES {
///     let evicted = store.memory_put("/repo", &format!("fact-{i}"), "small", 1, 1)?;
///     assert!(evicted.is_empty(), "everything fits until the cap");
/// }
///
/// // One more. The oldest goes, oldest first, and is named.
/// let evicted = store.memory_put("/repo", "fact-new", "small", 1, 2)?;
/// assert_eq!(evicted, ["fact-0"]);
/// assert_eq!(store.memory_list("/repo")?.len(), MEMORY_MAX_ENTRIES);
/// assert!(store.memory_get("/repo", "fact-0")?.is_none());
/// # Ok(())
/// # }
/// ```
pub const MEMORY_MAX_ENTRIES: usize = 64;

/// Most characters one workspace's entries may total.
///
/// The second of the two caps, and the one that actually binds: sixty-four
/// entries of a paragraph each is a context section nobody budgeted for, so the
/// character total evicts even when the entry count is nowhere near its limit.
///
/// ```
/// use io_harness::{Store, MEMORY_MAX_CHARS, MEMORY_MAX_ENTRIES, MEMORY_MAX_ENTRY_CHARS};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let long_note = "x".repeat(MEMORY_MAX_ENTRY_CHARS);
///
/// // Eight full-size notes fill the workspace exactly — well inside the entry
/// // count, and exactly on the character ceiling.
/// for i in 0..8 {
///     assert!(store.memory_put("/repo", &format!("note-{i}"), &long_note, 1, 1)?.is_empty());
/// }
/// assert!(8 < MEMORY_MAX_ENTRIES);
/// assert_eq!(long_note.chars().count() * 8, MEMORY_MAX_CHARS);
///
/// // The ninth evicts, on characters rather than on count.
/// assert_eq!(store.memory_put("/repo", "note-8", &long_note, 1, 2)?, ["note-0"]);
/// # Ok(())
/// # }
/// ```
pub const MEMORY_MAX_CHARS: usize = 16_000;

/// Most characters one entry may hold. A longer value is cut down to this and
/// marked, never refused — a too-long fact is still worth remembering.
///
/// ```
/// use io_harness::{Store, MEMORY_MAX_ENTRY_CHARS};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let rambling = "y".repeat(MEMORY_MAX_ENTRY_CHARS * 3);
/// store.memory_put("/repo", "build-notes", &rambling, 1, 4)?;
///
/// // Cut to the ceiling and *marked*, so a later reader can see the value is a
/// // fragment. A silent truncation reads like a complete fact that happens to
/// // end mid-sentence.
/// let stored = store.memory_get("/repo", "build-notes")?.expect("written above");
/// assert_eq!(stored.value.chars().count(), MEMORY_MAX_ENTRY_CHARS);
/// assert!(stored.value.ends_with("[truncated]"));
/// # Ok(())
/// # }
/// ```
pub const MEMORY_MAX_ENTRY_CHARS: usize = MEMORY_MAX_CHARS / 8;

/// The visible marker appended to a value that was cut to the per-entry ceiling.
const MEMORY_TRUNCATED: &str = "…[truncated]";

/// Cut `value` to [`MEMORY_MAX_ENTRY_CHARS`] on a char boundary, marking the
/// cut. Returned unchanged when it already fits.
fn truncate_memory_value(value: &str) -> String {
    if value.chars().count() <= MEMORY_MAX_ENTRY_CHARS {
        return value.to_string();
    }
    let keep = MEMORY_MAX_ENTRY_CHARS - MEMORY_TRUNCATED.chars().count();
    let mut out: String = value.chars().take(keep).collect();
    out.push_str(MEMORY_TRUNCATED);
    out
}

// ---- 0.10.0: what the context assembler decided ----

/// One decision the context assembler made: the section it built for a turn, or
/// a stale read it re-read (or was refused).
///
/// One row per turn plus one per re-read — never one per elided observation,
/// which would put a row explosion in the trace to say nothing new. Together
/// they answer "why did the model not see the thing it read at step 3", and
/// `est_tokens` beside `reported_tokens` is what records the estimator's drift
/// from the provider's own count (see [`crate::context::estimate_tokens`]).
///
/// ```
/// use io_harness::{ContextEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("summarise the repo", "NOTES.md")?;
/// # store.record_context_event(run_id, &ContextEvent::assembled(3, "carried=3 stubbed=5 reread=1", 4_100))?;
/// # store.record_context_reported(run_id, 3, 4_690)?;
/// # store.record_context_event(run_id, &ContextEvent::reread_refused(3, "secrets/id_rsa: denied by policy"))?;
/// let events = store.context_events(run_id)?;
///
/// // Why the model did not see something it read earlier: five observations were
/// // stubbed to fit the budget, and one stale read could not be refreshed at all
/// // because the policy now refuses that path.
/// let refused: Vec<&str> = events
///     .iter()
///     .filter(|e| e.kind == "reread_refused")
///     .filter_map(|e| e.detail.as_deref())
///     .collect();
/// assert_eq!(refused, ["secrets/id_rsa: denied by policy"]);
///
/// // And the pair that makes the budget trustworthy: what the assembler
/// // estimated, beside what the provider actually charged for the same turn.
/// // Drift here is the estimator being wrong, not the budget being generous.
/// let assembled = events.iter().find(|e| e.kind == "assembled").expect("one per turn");
/// assert_eq!((assembled.est_tokens, assembled.reported_tokens), (Some(4_100), Some(4_690)));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEvent {
    /// The step it belongs to.
    pub step: u32,
    /// `"assembled"`, `"reread"`, or `"reread_refused"`.
    pub kind: String,
    /// For `"assembled"`, the turn's summary (`carried=3 stubbed=5 reread=1`);
    /// for a re-read, the path and why it was or was not re-read.
    pub detail: Option<String>,
    /// The assembler's own estimate for the section it built.
    pub est_tokens: Option<u64>,
    /// What the provider said the request actually cost, when it says anything.
    pub reported_tokens: Option<u64>,
}

impl ContextEvent {
    /// The section built for one turn.
    pub fn assembled(step: u32, detail: impl Into<String>, est_tokens: u64) -> Self {
        Self {
            step,
            kind: "assembled".into(),
            detail: Some(detail.into()),
            est_tokens: Some(est_tokens),
            reported_tokens: None,
        }
    }

    /// A stale read was re-read at assembly time.
    pub fn reread(step: u32, detail: impl Into<String>) -> Self {
        Self {
            step,
            kind: "reread".into(),
            detail: Some(detail.into()),
            est_tokens: None,
            reported_tokens: None,
        }
    }

    /// A stale read could not be re-read — the policy refused it, or it is gone.
    pub fn reread_refused(step: u32, detail: impl Into<String>) -> Self {
        Self {
            step,
            kind: "reread_refused".into(),
            detail: Some(detail.into()),
            est_tokens: None,
            reported_tokens: None,
        }
    }

    /// The agent recorded a durable note for later runs over this workspace.
    pub fn memory_write(step: u32, detail: impl Into<String>) -> Self {
        Self::of("memory_write", step, detail)
    }

    /// A note was dropped to hold the workspace's memory caps.
    pub fn memory_evict(step: u32, detail: impl Into<String>) -> Self {
        Self::of("memory_evict", step, detail)
    }

    /// Notes from earlier runs were carried into this turn's context.
    pub fn memory_recall(step: u32, detail: impl Into<String>) -> Self {
        Self::of("memory_recall", step, detail)
    }

    /// The agent wrote down its plan at this step (0.21.0). The detail is the shape
    /// of the plan — how many items and how many done — rather than its text, which
    /// is already in the `todos` table and would otherwise be stored twice.
    pub fn todo_write(step: u32, detail: impl Into<String>) -> Self {
        Self::of("todo_write", step, detail)
    }

    /// The agent asked the operator about intent at this step (0.21.0).
    pub fn question_asked(step: u32, detail: impl Into<String>) -> Self {
        Self::of("question_asked", step, detail)
    }

    /// An answer to that question entered the run (0.21.0). The detail names who
    /// answered — a `Responder` in the process, or a human after a pause.
    pub fn question_answered(step: u32, detail: impl Into<String>) -> Self {
        Self::of("question_answered", step, detail)
    }

    /// An operator's mid-turn message entered the conversation at this step
    /// (0.20.0). Recorded rather than only delivered: a turn that changed course
    /// because a human said something must be readable as that afterwards, and the
    /// detail is the message's length rather than its text — the message itself is
    /// already in the step's observations.
    pub fn steered(step: u32, detail: impl Into<String>) -> Self {
        Self::of("steered", step, detail)
    }

    /// Which provider actually answered this step.
    ///
    /// Recorded only when the answer is not obvious from configuration — a
    /// [`Fallback`](crate::provider::Fallback) that fell over. `runs.provider` is one
    /// label for a whole run and stops being true the moment a run can use two.
    pub fn served(step: u32, provider: impl Into<String>) -> Self {
        Self::of("served", step, provider)
    }

    /// The agent made no progress and was told once to change approach. The run
    /// continues.
    ///
    /// Split from [`Self::stalled`] in 0.12.0. Both were recorded under the one
    /// `"stalled"` kind, distinguishable only by prose in `detail` — so anything
    /// scoring a run could not tell "was nudged and carried on" from "gave up"
    /// without string-matching an English sentence the crate never promised.
    pub fn replan(step: u32, detail: impl Into<String>) -> Self {
        Self::of("replan", step, detail)
    }

    /// The agent made no progress, had already been told once, and the run is
    /// ending here. Terminal.
    ///
    /// A nudge that did not work is [`Self::replan`]; see there for why the two
    /// are separate kinds since 0.12.0.
    pub fn stalled(step: u32, detail: impl Into<String>) -> Self {
        Self::of("stalled", step, detail)
    }

    fn of(kind: &str, step: u32, detail: impl Into<String>) -> Self {
        Self {
            step,
            kind: kind.into(),
            detail: Some(detail.into()),
            est_tokens: None,
            reported_tokens: None,
        }
    }
}

/// One call to a provider: what it cost, which model served it, and how long it
/// took (0.18.0).
///
/// **Per call, not per step.** A step that failed twice and then answered is
/// three of these, and the two that failed are kept because a model that
/// produced tokens before erroring was still billed for them. `steps.tokens`
/// holds one integer per step and cannot express any of that.
///
/// ```
/// use io_harness::{ProviderCall, Store, Usage};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("summarise the repo", "NOTES.md")?;
/// // The first attempt died after the model had already produced tokens...
/// store.record_provider_call(run_id, &ProviderCall {
///     step: 1,
///     attempt: 0,
///     provider: "anthropic".into(),
///     model: Some("claude-sonnet-4".into()),
///     usage: Some(Usage { prompt_tokens: 900, completion_tokens: 20, total_tokens: 920,
///                         ..Default::default() }),
///     latency_ms: 4_100,
///     failure: Some("Server (HTTP 529)".into()),
///     ..Default::default()
/// })?;
/// // ...and the retry answered.
/// store.record_provider_call(run_id, &ProviderCall {
///     step: 1,
///     attempt: 1,
///     provider: "anthropic".into(),
///     model: Some("claude-sonnet-4".into()),
///     usage: Some(Usage { prompt_tokens: 900, completion_tokens: 310, total_tokens: 1_210,
///                         cache_read_tokens: 850, ..Default::default() }),
///     latency_ms: 2_300,
///     ttft_ms: Some(420),
///     finish_reason: Some("end_turn".into()),
///     ..Default::default()
/// })?;
///
/// let calls = store.provider_calls(run_id)?;
/// assert_eq!(calls.len(), 2);
///
/// // What the step actually cost is the sum over its calls — including the one
/// // that failed, which a trace keeping only the winner would have hidden.
/// let billed: u64 = calls.iter().filter_map(|c| c.usage).map(|u| u.total_tokens).sum();
/// assert_eq!(billed, 2_130);
/// assert_eq!(calls[0].failure.as_deref(), Some("Server (HTTP 529)"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProviderCall {
    /// The step this call was made for.
    pub step: u32,
    /// Which attempt at that step this was, counting from 0.
    pub attempt: u32,
    /// The provider that was asked, by [`Provider::name`](crate::Provider::name).
    /// For a [`Fallback`](crate::provider::Fallback) this is the combined label;
    /// [`ProviderCall::model`] is what identifies who actually answered.
    pub provider: String,
    /// The model that served the call, as the provider named it. `None` when it
    /// did not say — a custom provider, or a wire that omits it.
    pub model: Option<String>,
    /// Tokens, as reported. `None` when the provider reported none, which is not
    /// the same fact as zero.
    pub usage: Option<Usage>,
    /// Milliseconds the whole call took, measured by the harness around
    /// [`Provider::complete`](crate::Provider::complete) — so it includes the
    /// crate's own request building and stream consumption, not only the
    /// provider's part.
    pub latency_ms: u64,
    /// Milliseconds to the first content-bearing chunk, where the path streamed
    /// and measured it. `None`, never zero, when nothing measured it.
    pub ttft_ms: Option<u64>,
    /// The provider's own word for why the model stopped, verbatim.
    pub finish_reason: Option<String>,
    /// `None` when the call answered; the failure's short name when it did not.
    pub failure: Option<String>,
}

/// One file change a run made, and how many lines it added and removed
/// (0.18.0).
///
/// The counts come from comparing the file's lines before and after, trimming
/// the common head and tail — not from a minimal diff. A one-line replacement is
/// therefore one added and one removed, and a rewrite of the middle of a file is
/// the size of that middle. It answers "how much did this run change" without
/// the crate carrying a diff algorithm it has no other use for.
///
/// ```
/// use io_harness::{Edit, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// # let store = Store::memory()?;
/// # let run_id = store.start_run("tidy the notes", "NOTES.md")?;
/// // A run writes rows like this itself; recorded by hand here so the example
/// // needs no model.
/// store.record_edit(run_id, &Edit::measure(
///     2,
///     "edit_file",
///     "src/parse.rs",
///     "fn parse() {}\nfn other() {}\n",
///     "fn parse(s: &str) {}\nfn other() {}\n",
/// ))?;
///
/// let edits = store.edits(run_id)?;
/// assert_eq!(edits[0].path, "src/parse.rs");
/// // One line out, one line in — the untouched line is not counted as either.
/// assert_eq!((edits[0].lines_added, edits[0].lines_removed), (1, 1));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Edit {
    /// The step that made the change.
    pub step: u32,
    /// The tool that made it — `write_file` or `edit_file`.
    pub tool: String,
    /// The path, as the agent named it (relative to the workspace root).
    pub path: String,
    /// Lines present after and not before.
    pub lines_added: u64,
    /// Lines present before and not after.
    pub lines_removed: u64,
}

impl Edit {
    /// The change `new` makes to `old`, by common head and tail comparison.
    ///
    /// ```
    /// use io_harness::Edit;
    ///
    /// // A one-line replacement inside a file: one line out, one line in.
    /// let edit = Edit::measure(1, "edit_file", "a.rs", "fn one() {}\nfn two() {}\n",
    ///                                                 "fn one() {}\nfn three() {}\n");
    /// assert_eq!((edit.lines_added, edit.lines_removed), (1, 1));
    ///
    /// // A new file is all addition; rewriting a file with what it already held
    /// // is neither, which is the fact a byte-count would have missed.
    /// assert_eq!(Edit::measure(1, "write_file", "b.rs", "", "one\ntwo\n").lines_added, 2);
    /// let same = Edit::measure(1, "write_file", "b.rs", "one\ntwo\n", "one\ntwo\n");
    /// assert_eq!((same.lines_added, same.lines_removed), (0, 0));
    /// ```
    pub fn measure(step: u32, tool: &str, path: &str, old: &str, new: &str) -> Self {
        let old: Vec<&str> = old.lines().collect();
        let new: Vec<&str> = new.lines().collect();
        let head = old
            .iter()
            .zip(&new)
            .take_while(|(a, b)| a == b)
            .count()
            .min(old.len().min(new.len()));
        let tail = old[head..]
            .iter()
            .rev()
            .zip(new[head..].iter().rev())
            .take_while(|(a, b)| a == b)
            .count();
        Self {
            step,
            tool: tool.to_string(),
            path: path.to_string(),
            lines_added: (new.len() - head - tail) as u64,
            lines_removed: (old.len() - head - tail) as u64,
        }
    }
}

/// How long a contended statement waits for the writer before giving up, set on
/// every store opened from a file. Without it rusqlite's default is to fail
/// immediately with `SQLITE_BUSY`, which turns a moment of contention into an
/// error rather than a short wait.
///
/// ```no_run
/// use io_harness::{Store, BUSY_TIMEOUT};
///
/// # fn main() -> io_harness::Result<()> {
/// // A dashboard tailing a run another process is still writing. `Store::open`
/// // sets WAL and this timeout, so this read waits for the writer instead of
/// // failing — which is why watching a live run needs no coordination with it.
/// let store = Store::open("runs.sqlite3")?;
/// let run_id = store.last_run()?.expect("at least one run in the store");
/// println!("step {} so far", store.last_step(run_id)?);
///
/// // The value is public because it is the bound on how long such a read can
/// // block: a poller on a shorter interval than this can queue up behind a busy
/// // writer, and it should size its own deadline knowing that.
/// assert!(BUSY_TIMEOUT >= std::time::Duration::from_secs(1));
/// # Ok(())
/// # }
/// ```
pub const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

impl Store {
    /// Open (creating if absent) a store at `path` and ensure the schema exists.
    ///
    /// Sets `journal_mode = WAL` and a [`BUSY_TIMEOUT`], so a second process may
    /// read the trace while a run is still writing it without either side
    /// blocking or aborting the other. Before 0.12.0 this was a bare
    /// `Connection::open`, which left every reader to configure the file itself
    /// — reaching around this API to do it, and having to do it before the
    /// harness opened the file at all.
    ///
    /// WAL is a persistent property of the database file, not of this
    /// connection: a store opened once by 0.12.0 stays in WAL mode afterwards.
    /// That is why it is documented as a migration.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(BUSY_TIMEOUT)?;
        // `query_row` rather than `execute`: this pragma returns the resulting
        // mode as a row, and rusqlite's `execute` rejects a statement that
        // yields rows. The returned mode is not asserted — a database on a
        // filesystem that cannot support WAL stays in its previous journal mode
        // and still works, just without concurrent readers.
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        Self::from_conn(conn)
    }

    /// An in-memory store, for tests and throwaway runs.
    pub fn memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> Result<Self> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 goal     TEXT NOT NULL,
                 file     TEXT NOT NULL,
                 outcome  TEXT,
                 provider TEXT
             );
             CREATE TABLE IF NOT EXISTS steps (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL REFERENCES runs(id),
                 step     INTEGER NOT NULL,
                 decision TEXT NOT NULL,
                 result   TEXT NOT NULL,
                 prompt    TEXT NOT NULL DEFAULT '',
                 tool_call TEXT NOT NULL DEFAULT '',
                 tokens    INTEGER NOT NULL DEFAULT 0
             );",
        )?;

        // Migrate a 0.1.0 database whose `steps` table predates the trace
        // columns. ADD COLUMN errors on an already-present column; ignore it.
        for col in [
            "prompt TEXT NOT NULL DEFAULT ''",
            "tool_call TEXT NOT NULL DEFAULT ''",
            "tokens INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = conn.execute(&format!("ALTER TABLE steps ADD COLUMN {col}"), []);
        }
        // 0.3.0: record which provider ran. Additive — a 0.1/0.2 database gains
        // the column and a 0.2 binary still reads a migrated database.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN provider TEXT", []);

        // 0.4.0: policy refusals/decisions, and actions paused awaiting a human.
        // New tables only — a 0.3.0 database gains them and a 0.3.0 binary,
        // which never queries them, still reads a migrated database.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS policy_events (
                 id        INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id    INTEGER NOT NULL,
                 step      INTEGER NOT NULL,
                 kind      TEXT NOT NULL,
                 act       TEXT NOT NULL,
                 target    TEXT NOT NULL,
                 rule      TEXT,
                 layer     TEXT,
                 decision  TEXT,
                 source    TEXT,
                 performed TEXT
             );
             CREATE TABLE IF NOT EXISTS pending_approvals (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL,
                 step     INTEGER NOT NULL,
                 act      TEXT NOT NULL,
                 target   TEXT NOT NULL,
                 content  TEXT,
                 resolved TEXT
             );",
        )?;

        // 0.5.0: sub-agent trees. Runs gain a parent edge and a depth; a new
        // table records spawns, spawn refusals, and draws against the tree's
        // shared spend ceiling. All additive — a 0.4.0 database gains the column
        // and table and a 0.4.0 binary still reads a migrated database.
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN parent_run_id INTEGER", []);
        let _ = conn.execute(
            "ALTER TABLE runs ADD COLUMN depth INTEGER NOT NULL DEFAULT 0",
            [],
        );
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS agent_events (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id       INTEGER NOT NULL,
                 step         INTEGER NOT NULL,
                 kind         TEXT NOT NULL,
                 child_run_id INTEGER,
                 detail       TEXT,
                 tokens       INTEGER,
                 remaining    INTEGER
             );",
        )?;

        // 0.6.0: sandbox lifecycle events (create, exec+backend, cap hit, net
        // deny, destroy). New table only — a 0.5.0 database gains it and a 0.5.0
        // binary, which never queries it, still reads a migrated database.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sandbox_events (
                 id       INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id   INTEGER NOT NULL,
                 step     INTEGER NOT NULL,
                 kind     TEXT NOT NULL,
                 backend  TEXT,
                 detail   TEXT
             );",
        )?;

        // 0.7.0: durable checkpoint + resume. `runs` gains a resumable status and
        // a start timestamp so wall-clock elapsed survives a restart; a new table
        // records checkpoint / resume / step-skipped events so a multi-crash run's
        // history is reconstructable from the store alone. All additive — a 0.6.0
        // database gains the columns/table and a 0.6.0 binary still reads it.
        let _ = conn.execute(
            "ALTER TABLE runs ADD COLUMN status TEXT NOT NULL DEFAULT 'running'",
            [],
        );
        let _ = conn.execute("ALTER TABLE runs ADD COLUMN started_at TEXT", []);
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS checkpoint_events (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 detail TEXT
             );
             CREATE TABLE IF NOT EXISTS spawns (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 parent_run_id INTEGER NOT NULL,
                 step          INTEGER NOT NULL,
                 child_run_id  INTEGER NOT NULL,
                 goal          TEXT NOT NULL,
                 verify_file   TEXT NOT NULL,
                 needle        TEXT NOT NULL,
                 max_steps     INTEGER,
                 deny_write    TEXT NOT NULL DEFAULT '[]'
             );",
        )?;

        // 0.8.0: the MCP conversation — connects, tool discovery, tool calls,
        // disconnects. New table only, so a 0.7.0 database gains it and a 0.7.0
        // binary, which never queries it, still reads a migrated database. The
        // network *verdicts* deliberately do not live here: they go to
        // policy_events beside every other permission decision, because an
        // operator auditing "what was this run allowed to do" should find them
        // in one place.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS mcp_events (
                 id     INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 server TEXT NOT NULL,
                 tool   TEXT,
                 ok     INTEGER,
                 millis INTEGER,
                 detail TEXT
             );",
        )?;

        // 0.10.0: durable cross-run memory — facts and decisions an agent wrote
        // deliberately, keyed to a *workspace* instead of a run, so a later run
        // recalls what an earlier one learned. New table only, so a 0.9.1
        // database gains it and a 0.9.1 binary, which never queries it, still
        // reads a migrated database. Deliberately NOT a CHECKPOINT_FORMAT bump:
        // no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.9.1 checkpoint.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS memory (
                 id         INTEGER PRIMARY KEY,
                 workspace  TEXT NOT NULL,
                 key        TEXT NOT NULL,
                 value      TEXT NOT NULL,
                 run_id     INTEGER NOT NULL,
                 step       INTEGER NOT NULL,
                 created_at TEXT NOT NULL,
                 UNIQUE(workspace, key)
             );",
        )?;

        // 0.10.0: what the context assembler decided each turn — one row per turn
        // plus one per re-read. New table only, so a 0.9.1 database gains it and a
        // 0.9.1 binary, which never queries it, still opens and resumes a migrated
        // database. Deliberately NOT a `CHECKPOINT_FORMAT` bump: nothing about a
        // checkpoint's layout changed, and bumping it would refuse every 0.9.1
        // store on resume for an additive audit table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS context_events (
                 id              INTEGER PRIMARY KEY,
                 run_id          INTEGER NOT NULL,
                 step            INTEGER NOT NULL,
                 kind            TEXT NOT NULL,
                 detail          TEXT,
                 est_tokens      INTEGER,
                 reported_tokens INTEGER
             );",
        )?;

        // 0.12.0: one row per finished run, so "did it work, how long did it take,
        // what did it cost" is one read rather than a reconstruction.
        //
        // Every field but the end stamp was already derivable, and derivable was
        // not good enough: a consumer had to know that success is one of eleven
        // free-text strings, that steps means MAX(step) and not COUNT(*) because
        // retry rows share a step number, and that spend is SUM(steps.tokens). That
        // is schema knowledge the crate never promised, so io-eval would have been
        // coupled to internals from its first line.
        //
        // `finished_at` is the genuinely new fact. Nothing in the schema recorded
        // when a run ENDED — only `runs.started_at` — and `Store::elapsed_secs`
        // measures against `julianday('now')`, so it keeps growing after the run is
        // over and cannot reconstruct a finished run's latency. Stamped from
        // SQLite's clock for the same reason `started_at` is: the pair must come
        // from one clock or the difference is meaningless.
        //
        // A separate table rather than columns on `runs`: additive, and `runs` is
        // read by resume on the hot path. New table only, so a 0.11.0 database gains
        // it and a 0.11.0 binary, which never queries it, still opens and resumes a
        // migrated database. Deliberately NOT a `CHECKPOINT_FORMAT` bump — no
        // checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.11.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_outcomes (
                 run_id      INTEGER PRIMARY KEY,
                 outcome     TEXT NOT NULL,
                 success     INTEGER NOT NULL,
                 steps       INTEGER NOT NULL,
                 tokens      INTEGER NOT NULL,
                 duration_ms INTEGER,
                 finished_at TEXT NOT NULL
             );",
        )?;

        // 0.13.0: the policy a run was started under, kept so a later resume can
        // tell what boundary the caller enforced instead of guessing. Nothing in
        // the schema recorded it: `policy_events` holds the decisions a policy
        // produced, which is the opposite direction — a run that was never asked
        // to do anything forbidden leaves no events at all, and a permissive run
        // leaves none either, so the two are indistinguishable after the fact.
        //
        // Stored as JSON in one column rather than shredded into rule rows: the
        // only reader wants the whole [`Policy`] back, and a serialised blob
        // cannot drift from the type the way a hand-written flattening would.
        //
        // New table only, so a 0.12.0 database gains it and a 0.12.0 binary, which
        // never queries it, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.12.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS run_policies (
                 run_id INTEGER PRIMARY KEY,
                 policy TEXT NOT NULL
             );",
        )?;

        // 0.13.0: the observation ledger the context assembler builds, made
        // durable so a resumed run restores the context it had instead of
        // re-deriving one from the workspace.
        //
        // The text was already durable — `steps.result` holds one step's
        // observations concatenated — but concatenated is the problem: a step with
        // three observations stores one string, and the typed triple assembly
        // actually reasons about (`step`, `kind`, `target`) is not recoverable
        // from it at all. `ObsKind::target_is_the_subject` decides supersession
        // from `kind`, so a ledger rebuilt from `steps.result` would assemble
        // differently from the one it replaced, which is worse than the honest
        // re-derivation it would be replacing.
        //
        // One row per observation, ordered by `id` like every other event table
        // here, because the ledger is an ordered log and `step` alone does not
        // order the observations within a step.
        //
        // New table only, so a 0.12.0 database gains it and a 0.12.0 binary, which
        // never queries it, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.12.0 store for an additive table.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS ledger_observations (
                 id     INTEGER PRIMARY KEY,
                 run_id INTEGER NOT NULL,
                 step   INTEGER NOT NULL,
                 kind   TEXT NOT NULL,
                 target TEXT,
                 text   TEXT NOT NULL
             );",
        )?;

        // 0.18.0: accounting. One row per provider call and one per file change.
        //
        // `provider_calls` is per CALL, not per step, which is the whole point:
        // `steps.tokens` holds one integer for a step, so a step that retried
        // twice and then fell over to a second vendor collapsed into a single
        // number attributed to nothing. A row per attempt keeps what was actually
        // paid for — including the attempts that failed after the model had
        // already produced tokens.
        //
        // No cost column, deliberately. Money is derived at query time from a
        // price table the operator owns ([`crate::pricing`]), because a stored
        // dollar figure is wrong the moment a price changes or was entered wrong,
        // and cannot then be repaired without rewriting history.
        //
        // `at` comes from SQLite's clock, like `runs.started_at`, so the day a
        // call is grouped into and the run's elapsed time come from one clock
        // rather than two that can disagree.
        //
        // New tables only, so a 0.17.0 database gains them and a 0.17.0 binary,
        // which never queries them, still opens and resumes a migrated database.
        // Deliberately NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout
        // changed, and bumping it would make [`Store::check_resumable`] refuse
        // every 0.17.0 store for two additive tables.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS provider_calls (
                 id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id               INTEGER NOT NULL,
                 step                 INTEGER NOT NULL,
                 attempt              INTEGER NOT NULL,
                 provider             TEXT NOT NULL,
                 model                TEXT,
                 prompt_tokens        INTEGER,
                 completion_tokens    INTEGER,
                 total_tokens         INTEGER,
                 cache_read_tokens    INTEGER,
                 cache_write_tokens   INTEGER,
                 reasoning_tokens     INTEGER,
                 server_tool_requests INTEGER,
                 latency_ms           INTEGER NOT NULL,
                 ttft_ms              INTEGER,
                 finish_reason        TEXT,
                 failure              TEXT,
                 at                   TEXT NOT NULL DEFAULT (datetime('now'))
             );
             CREATE TABLE IF NOT EXISTS edits (
                 id            INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id        INTEGER NOT NULL,
                 step          INTEGER NOT NULL,
                 tool          TEXT NOT NULL,
                 path          TEXT NOT NULL,
                 lines_added   INTEGER NOT NULL,
                 lines_removed INTEGER NOT NULL,
                 at            TEXT NOT NULL DEFAULT (datetime('now'))
             );",
        )?;

        // 0.20.0 — the session layer. A conversation is a tree of turns and a turn
        // is a run, so the only new state is the tree itself: which turns a session
        // has, which turn each one answers, and which run served it. Everything a
        // turn cost, refused, or committed is already in the tables above under its
        // `run_id`.
        //
        // New tables only, as every addition since 0.13.0 has been, and deliberately
        // NOT a `CHECKPOINT_FORMAT` bump: no checkpoint layout changed, and bumping
        // it would make [`Store::check_resumable`] refuse every 0.19.0 store for two
        // additive tables. A 0.19.0 binary never queries them and opens a migrated
        // database unchanged.
        //
        // `head_turn_id` is a column rather than "the last row", because branching
        // means the head is a choice and not a maximum.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS sessions (
                 id           INTEGER PRIMARY KEY AUTOINCREMENT,
                 root         TEXT NOT NULL,
                 head_turn_id INTEGER,
                 created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE TABLE IF NOT EXISTS session_turns (
                 id             INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id     INTEGER NOT NULL,
                 parent_turn_id INTEGER,
                 run_id         INTEGER NOT NULL,
                 prompt         TEXT NOT NULL,
                 reply          TEXT,
                 outcome        TEXT,
                 created_at     TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;

        // 0.21.0. The agent's plan, one row per item, and the question channel that
        // asks an operator what they actually wanted.
        //
        // New tables again, and again deliberately NOT a `CHECKPOINT_FORMAT` bump:
        // no checkpoint layout changed, and bumping it would make
        // [`Store::check_resumable`] refuse every 0.20.0 store over two additive
        // tables a 0.20.0 binary never queries.
        //
        // `todos.position` is a column rather than "the rowid order", because the
        // list is replaced wholesale and an operator reads it in the order the agent
        // wrote it — which after a replace is not the order the ids run in.
        //
        // `pending_questions` mirrors `pending_approvals` field for field, including
        // the `resolved` marker, so a question survives a process exit for exactly
        // the reason a pending approval does. `answer` is NULL until a human writes
        // one, and `answered_by` records whether it was a `Responder` in the process
        // or a person after a pause — "the machine decided" and "a person decided"
        // are different facts about a run.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS todos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id     INTEGER NOT NULL,
                 position   INTEGER NOT NULL,
                 text       TEXT NOT NULL,
                 state      TEXT NOT NULL,
                 written_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );
             CREATE INDEX IF NOT EXISTS todos_run ON todos(run_id, position);
             CREATE TABLE IF NOT EXISTS pending_questions (
                 id          INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id      INTEGER NOT NULL,
                 step        INTEGER NOT NULL,
                 question    TEXT NOT NULL,
                 context     TEXT,
                 choices     TEXT,
                 answer      TEXT,
                 answered_by TEXT,
                 resolved    INTEGER NOT NULL DEFAULT 0,
                 asked_at    TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             );",
        )?;

        // Stamp the checkpoint-format version. A fresh or pre-0.7.0 database reads
        // back 0; we bump it to the current format. A database written by a NEWER
        // format reads back a higher number and [`Store::check_resumable`] refuses
        // it with a typed [`Error::Resume`] rather than resuming a layout it does
        // not understand.
        let format: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if format < CHECKPOINT_FORMAT {
            conn.execute_batch(&format!("PRAGMA user_version = {CHECKPOINT_FORMAT}"))?;
        }

        Ok(Self { conn })
    }

    /// Record a policy refusal or a human decision against a run.
    pub fn record_event(&self, run_id: i64, e: &PolicyEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO policy_events
                 (run_id, step, kind, act, target, rule, layer, decision, source, performed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.act,
                &e.target,
                &e.rule,
                &e.layer,
                &e.decision,
                &e.source,
                &e.performed,
            ),
        )?;
        Ok(())
    }

    /// Every policy event recorded for a run, in order.
    pub fn events(&self, run_id: i64) -> Result<Vec<PolicyEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, act, target, rule, layer, decision, source, performed
             FROM policy_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(PolicyEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                act: r.get(2)?,
                target: r.get(3)?,
                rule: r.get(4)?,
                layer: r.get(5)?,
                decision: r.get(6)?,
                source: r.get(7)?,
                performed: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Persist an action awaiting a human decision; returns its request id.
    pub fn put_pending(
        &self,
        run_id: i64,
        step: u32,
        act: &str,
        target: &str,
        content: Option<&str>,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO pending_approvals (run_id, step, act, target, content)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (run_id, step, act, target, content),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read a pending action back by request id.
    pub fn pending(&self, request_id: i64) -> Result<Option<Pending>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, act, target, content, resolved
             FROM pending_approvals WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map([request_id], |r| {
            Ok(Pending {
                id: r.get(0)?,
                run_id: r.get(1)?,
                step: r.get::<_, i64>(2)? as u32,
                act: r.get(3)?,
                target: r.get(4)?,
                content: r.get(5)?,
                resolved: r.get(6)?,
            })
        })?;
        Ok(rows.next().transpose()?)
    }

    /// Mark a pending action decided, so a resume knows what the human chose.
    pub fn resolve_pending(&self, request_id: i64, decision: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE pending_approvals SET resolved = ?1 WHERE id = ?2",
            (decision, request_id),
        )?;
        Ok(())
    }

    /// Start a run row; returns its id. Stamps `started_at` (UTC, from SQLite's
    /// clock) so a 24h wall-clock budget survives a restart, and marks the run
    /// `running`.
    pub fn start_run(&self, goal: &str, file: &str) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (goal, file, status, started_at)
             VALUES (?1, ?2, 'running', strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (goal, file),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Start a child run under `parent_run_id` at `depth`, so the tree records
    /// who spawned whom. Returns the child's run id.
    pub fn start_child_run(
        &self,
        goal: &str,
        file: &str,
        parent_run_id: i64,
        depth: u32,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO runs (goal, file, parent_run_id, depth, status, started_at)
             VALUES (?1, ?2, ?3, ?4, 'running', strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
            (goal, file, parent_run_id, depth),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Record a spawn, a spawn refusal, or a budget draw against the tree.
    pub fn record_agent_event(&self, e: &AgentEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO agent_events (run_id, step, kind, child_run_id, detail, tokens, remaining)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                e.run_id,
                e.step,
                &e.kind,
                e.child_run_id,
                &e.detail,
                e.tokens,
                e.remaining,
            ),
        )?;
        Ok(())
    }

    /// Every agent event recorded for a run, in order.
    pub fn agent_events(&self, run_id: i64) -> Result<Vec<AgentEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, child_run_id, detail, tokens, remaining
             FROM agent_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(AgentEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                child_run_id: r.get(3)?,
                detail: r.get(4)?,
                tokens: r.get::<_, Option<i64>>(5)?.map(|n| n as u64),
                remaining: r.get::<_, Option<i64>>(6)?.map(|n| n as u64),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one sandbox lifecycle event against a run.
    pub fn record_sandbox_event(&self, e: &SandboxEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO sandbox_events (run_id, step, kind, backend, detail)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            (e.run_id, e.step, &e.kind, &e.backend, &e.detail),
        )?;
        Ok(())
    }

    /// Record one MCP event.
    pub fn record_mcp(&self, run_id: i64, e: &McpEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO mcp_events (run_id, step, kind, server, tool, ok, millis, detail)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.server,
                &e.tool,
                e.ok,
                e.millis.map(|m| m as i64),
                &e.detail,
            ),
        )?;
        Ok(())
    }

    /// Every MCP event recorded for a run, in order.
    pub fn mcp_events(&self, run_id: i64) -> Result<Vec<McpEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, server, tool, ok, millis, detail
             FROM mcp_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(McpEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                server: r.get(2)?,
                tool: r.get(3)?,
                ok: r.get(4)?,
                millis: r.get::<_, Option<i64>>(5)?.map(|m| m as u64),
                detail: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one context-assembly event against a run.
    pub fn record_context_event(&self, run_id: i64, e: &ContextEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO context_events (run_id, step, kind, detail, est_tokens, reported_tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                run_id,
                e.step,
                &e.kind,
                &e.detail,
                e.est_tokens.map(|n| n as i64),
                e.reported_tokens.map(|n| n as i64),
            ),
        )?;
        Ok(())
    }

    /// Fill in what the provider said one turn's request cost, once the
    /// completion has returned. The estimate is left as it was: the pair is the
    /// point — one row carries both numbers, so drift is readable.
    pub fn record_context_reported(&self, run_id: i64, step: u32, reported: u64) -> Result<()> {
        self.conn.execute(
            "UPDATE context_events SET reported_tokens = ?1
             WHERE run_id = ?2 AND step = ?3 AND kind = 'assembled'",
            (reported as i64, run_id, step),
        )?;
        Ok(())
    }

    /// Every context-assembly event recorded for a run, in order.
    pub fn context_events(&self, run_id: i64) -> Result<Vec<ContextEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, detail, est_tokens, reported_tokens
             FROM context_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(ContextEvent {
                step: r.get::<_, i64>(0)? as u32,
                kind: r.get(1)?,
                detail: r.get(2)?,
                est_tokens: r.get::<_, Option<i64>>(3)?.map(|n| n as u64),
                reported_tokens: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record one call to a provider (0.18.0).
    ///
    /// Called once per attempt, by the run loop, for a call that answered and
    /// for one that failed alike. See [`ProviderCall`] for why the failures are
    /// kept.
    pub fn record_provider_call(&self, run_id: i64, call: &ProviderCall) -> Result<()> {
        let u = call.usage;
        self.conn.execute(
            "INSERT INTO provider_calls
                 (run_id, step, attempt, provider, model, prompt_tokens, completion_tokens,
                  total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                  server_tool_requests, latency_ms, ttft_ms, finish_reason, failure)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
            rusqlite::params![
                run_id,
                call.step,
                call.attempt,
                &call.provider,
                &call.model,
                u.map(|u| u.prompt_tokens),
                u.map(|u| u.completion_tokens),
                u.map(|u| u.total_tokens),
                u.map(|u| u.cache_read_tokens),
                u.map(|u| u.cache_write_tokens),
                u.map(|u| u.reasoning_tokens),
                u.map(|u| u.server_tool_requests),
                call.latency_ms,
                call.ttft_ms,
                &call.finish_reason,
                &call.failure,
            ],
        )?;
        Ok(())
    }

    /// Every provider call recorded for a run, in the order they were made.
    ///
    /// A run that predates 0.18.0 has no rows, and this returns an empty vector
    /// rather than zeros — an unrecorded run and a free one are different facts.
    pub fn provider_calls(&self, run_id: i64) -> Result<Vec<ProviderCall>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, attempt, provider, model, prompt_tokens, completion_tokens,
                    total_tokens, cache_read_tokens, cache_write_tokens, reasoning_tokens,
                    server_tool_requests, latency_ms, ttft_ms, finish_reason, failure
             FROM provider_calls WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            // `total_tokens` decides whether the provider reported anything at
            // all: a NULL there is the `None` the caller stored, and reading it
            // back as a zeroed `Usage` would turn "unknown" into "free".
            let total: Option<u64> = r.get(6)?;
            Ok(ProviderCall {
                step: r.get(0)?,
                attempt: r.get(1)?,
                provider: r.get(2)?,
                model: r.get(3)?,
                usage: match total {
                    Some(total_tokens) => Some(Usage {
                        prompt_tokens: r.get::<_, Option<u64>>(4)?.unwrap_or(0),
                        completion_tokens: r.get::<_, Option<u64>>(5)?.unwrap_or(0),
                        total_tokens,
                        cache_read_tokens: r.get::<_, Option<u64>>(7)?.unwrap_or(0),
                        cache_write_tokens: r.get::<_, Option<u64>>(8)?.unwrap_or(0),
                        reasoning_tokens: r.get::<_, Option<u64>>(9)?.unwrap_or(0),
                        server_tool_requests: r.get::<_, Option<u64>>(10)?.unwrap_or(0),
                    }),
                    None => None,
                },
                latency_ms: r.get(11)?,
                ttft_ms: r.get(12)?,
                finish_reason: r.get(13)?,
                failure: r.get(14)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Every provider call in the store, with the run and the day it belongs to.
    ///
    /// The grouped views are built from this one read: pricing is arithmetic the
    /// database cannot do, so the rows come back and the grouping happens in
    /// Rust rather than in half-SQL that would still need a second pass.
    fn all_provider_calls(&self) -> Result<Vec<(i64, String, ProviderCall)>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, date(at), step, attempt, provider, model, prompt_tokens,
                    completion_tokens, total_tokens, cache_read_tokens, cache_write_tokens,
                    reasoning_tokens, server_tool_requests, latency_ms, ttft_ms, finish_reason,
                    failure
             FROM provider_calls ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            let total: Option<u64> = r.get(8)?;
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                ProviderCall {
                    step: r.get(2)?,
                    attempt: r.get(3)?,
                    provider: r.get(4)?,
                    model: r.get(5)?,
                    usage: match total {
                        Some(total_tokens) => Some(Usage {
                            prompt_tokens: r.get::<_, Option<u64>>(6)?.unwrap_or(0),
                            completion_tokens: r.get::<_, Option<u64>>(7)?.unwrap_or(0),
                            total_tokens,
                            cache_read_tokens: r.get::<_, Option<u64>>(9)?.unwrap_or(0),
                            cache_write_tokens: r.get::<_, Option<u64>>(10)?.unwrap_or(0),
                            reasoning_tokens: r.get::<_, Option<u64>>(11)?.unwrap_or(0),
                            server_tool_requests: r.get::<_, Option<u64>>(12)?.unwrap_or(0),
                        }),
                        None => None,
                    },
                    latency_ms: r.get(13)?,
                    ttft_ms: r.get(14)?,
                    finish_reason: r.get(15)?,
                    failure: r.get(16)?,
                },
            ))
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Spend grouped by the model that served each call, priced by `prices`
    /// (0.18.0).
    ///
    /// Calls whose provider named no model group under `"(unknown model)"` and
    /// are counted in [`Spend::unpriced_calls`], because attributing them to
    /// anything else would be a guess.
    ///
    /// ```
    /// use io_harness::pricing::{Price, PriceTable};
    /// use io_harness::{ProviderCall, Store, Usage};
    ///
    /// # fn main() -> io_harness::Result<()> {
    /// # let store = Store::memory()?;
    /// # let run_id = store.start_run("goal", "NOTES.md")?;
    /// # store.record_provider_call(run_id, &ProviderCall {
    /// #     step: 1, provider: "anthropic".into(), model: Some("m".into()),
    /// #     usage: Some(Usage { prompt_tokens: 1_000_000, total_tokens: 1_000_000,
    /// #                         ..Default::default() }), ..Default::default() })?;
    /// let cheap = PriceTable::new("2026-07-29").with("m", Price { input: 1_000_000, ..Price::ZERO });
    /// let dear = PriceTable::new("2026-07-29").with("m", Price { input: 2_000_000, ..Price::ZERO });
    ///
    /// // The same unchanged trace, two price tables, two answers — which is what
    /// // "correcting a price repairs the whole history" means in practice.
    /// assert_eq!(store.spend_by_model(&cheap)?[0].cost_micros, 1_000_000);
    /// assert_eq!(store.spend_by_model(&dear)?[0].cost_micros, 2_000_000);
    /// # Ok(())
    /// # }
    /// ```
    pub fn spend_by_model(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |_, _, call| {
            call.model.clone().unwrap_or_else(|| UNKNOWN_MODEL.into())
        })
    }

    /// Spend grouped by day (`YYYY-MM-DD`, UTC, from the database clock), priced
    /// by `prices` (0.18.0).
    pub fn spend_by_day(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |_, day, _| day.to_string())
    }

    /// Spend grouped by run id, priced by `prices` (0.18.0).
    pub fn spend_by_run(&self, prices: &PriceTable) -> Result<Vec<Spend>> {
        self.grouped(prices, |run_id, _, _| run_id.to_string())
    }

    /// The shared body of the three groupings: read once, key by `key`, sum and
    /// price each group. Rows come back ordered by key, which is the only
    /// ordering promised.
    fn grouped(
        &self,
        prices: &PriceTable,
        key: impl Fn(i64, &str, &ProviderCall) -> String,
    ) -> Result<Vec<Spend>> {
        let calls = self.all_provider_calls()?;
        let mut groups: std::collections::BTreeMap<String, Vec<&ProviderCall>> =
            std::collections::BTreeMap::new();
        for (run_id, day, call) in &calls {
            groups
                .entry(key(*run_id, day, call))
                .or_default()
                .push(call);
        }
        Ok(groups
            .into_iter()
            .map(|(k, calls)| crate::pricing::group(k, &calls, prices))
            .collect())
    }

    /// Record one file change and its line counts (0.18.0).
    pub fn record_edit(&self, run_id: i64, edit: &Edit) -> Result<()> {
        self.conn.execute(
            "INSERT INTO edits (run_id, step, tool, path, lines_added, lines_removed)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            (
                run_id,
                edit.step,
                &edit.tool,
                &edit.path,
                edit.lines_added,
                edit.lines_removed,
            ),
        )?;
        Ok(())
    }

    /// Every file change recorded for a run, in the order they were made.
    pub fn edits(&self, run_id: i64) -> Result<Vec<Edit>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, tool, path, lines_added, lines_removed
             FROM edits WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(Edit {
                step: r.get(0)?,
                tool: r.get(1)?,
                path: r.get(2)?,
                lines_added: r.get(3)?,
                lines_removed: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Record the policy a run was started under.
    ///
    /// `INSERT OR REPLACE`, like every other per-run row, so recording twice for
    /// one run — a resume that re-states its boundary — replaces rather than
    /// duplicates or fails.
    pub fn record_run_policy(&self, run_id: i64, policy: &Policy) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO run_policies (run_id, policy) VALUES (?1, ?2)",
            (
                run_id,
                serde_json::to_string(policy).expect("a Policy is always serialisable"),
            ),
        )?;
        Ok(())
    }

    /// The policy a run was started under, or `None` if none was recorded.
    ///
    /// `None` is not [`Policy::permissive`] and must never be read as it: a run
    /// written by 0.12.0 has no row at all, so the honest answer is "nobody
    /// recorded what the boundary was", not "the caller chose to enforce
    /// nothing". A caller that needs a policy either way has to decide which to
    /// assume, and it should decide that knowingly.
    /// Unlike the other getters in this file, a failed read is an error rather
    /// than `None`. They can fold the two together because a missing memory
    /// entry and an unreadable one lead to the same recovery; here they do not.
    /// `None` is what tells [`crate::resume`] the run had no boundary and may be
    /// resumed permissively, so a disk error that read as `None` would hand a
    /// policy-bearing run an agent with no policy — silently, and by exactly the
    /// route this table exists to close.
    pub fn run_policy(&self, run_id: i64) -> Result<Option<Policy>> {
        let json: Option<String> = match self.conn.query_row(
            "SELECT policy FROM run_policies WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        ) {
            Ok(json) => Some(json),
            Err(rusqlite::Error::QueryReturnedNoRows) => None,
            Err(e) => return Err(e.into()),
        };
        json.map(|j| {
            serde_json::from_str(&j).map_err(|e| Error::Resume {
                reason: format!("run {run_id} has an unreadable recorded policy: {e}"),
            })
        })
        .transpose()
    }

    /// Append observations to a run's durable ledger, in one transaction.
    ///
    /// Called once at a committed step boundary rather than once per
    /// observation: the step is the unit the rest of the checkpoint works in, and
    /// an observation belonging to a step that never committed must not survive a
    /// crash the step itself did not survive.
    pub fn record_observations(&self, run_id: i64, entries: &[Observation]) -> Result<()> {
        if entries.is_empty() {
            return Ok(());
        }
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO ledger_observations (run_id, step, kind, target, text)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
            )?;
            for e in entries {
                stmt.execute((run_id, e.step as i64, kind_wire(e.kind), &e.target, &e.text))?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// A run's durable ledger, in the order it was observed.
    ///
    /// Empty for a run that recorded nothing and for a run written before 0.13.0
    /// — the two are the same to a reader, and both mean "there is nothing to
    /// restore", which is 0.12.0's behaviour and not a lie about it.
    pub fn observations(&self, run_id: i64) -> Result<Vec<Observation>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, kind, target, text
             FROM ledger_observations WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok((
                r.get::<_, i64>(0)? as u32,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (step, kind, target, text) = row?;
            out.push(Observation::new(
                step,
                kind_from_wire(&kind, run_id)?,
                target,
                text,
            ));
        }
        Ok(out)
    }

    /// Every sandbox event recorded for a run, in order.
    pub fn sandbox_events(&self, run_id: i64) -> Result<Vec<SandboxEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, backend, detail
             FROM sandbox_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(SandboxEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                backend: r.get(3)?,
                detail: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The run ids of the direct children of `run_id`, in spawn order.
    pub fn children(&self, run_id: i64) -> Result<Vec<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM runs WHERE parent_run_id = ?1 ORDER BY id ASC")?;
        let rows = stmt.query_map([run_id], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The parent run id of `run_id`, or `None` for a root run.
    pub fn parent(&self, run_id: i64) -> Result<Option<i64>> {
        Ok(self.conn.query_row(
            "SELECT parent_run_id FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?)
    }

    /// The nesting depth recorded for a run (0 at the root).
    pub fn depth(&self, run_id: i64) -> Result<u32> {
        let d: i64 =
            self.conn
                .query_row("SELECT depth FROM runs WHERE id = ?1", [run_id], |r| {
                    r.get(0)
                })?;
        Ok(d as u32)
    }

    /// Record one step's full trace entry.
    pub fn record(&self, run_id: i64, step: &StepRecord) -> Result<()> {
        self.conn.execute(
            "INSERT INTO steps (run_id, step, decision, result, prompt, tool_call, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                step.step,
                &step.decision,
                &step.result,
                &step.prompt,
                &step.tool_call,
                step.tokens,
            ),
        )?;
        Ok(())
    }

    /// Durably checkpoint one completed step: the step's trace row and its
    /// checkpoint event are written in a single transaction, so a crash leaves
    /// either both (the step is done) or neither (it replays) — never a torn
    /// half. The committed checkpoint is the step's completion marker.
    pub fn checkpoint_step(&self, run_id: i64, step: &StepRecord) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO steps (run_id, step, decision, result, prompt, tool_call, tokens)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                step.step,
                &step.decision,
                &step.result,
                &step.prompt,
                &step.tool_call,
                step.tokens,
            ),
        )?;
        tx.execute(
            "INSERT INTO checkpoint_events (run_id, step, kind, detail)
             VALUES (?1, ?2, 'checkpoint', NULL)",
            (run_id, step.step),
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Record a checkpoint/resume/skipped event on its own (not tied to a step
    /// commit) — used for resume and skip markers.
    pub fn record_checkpoint_event(&self, e: &CheckpointEvent) -> Result<()> {
        self.conn.execute(
            "INSERT INTO checkpoint_events (run_id, step, kind, detail) VALUES (?1, ?2, ?3, ?4)",
            (e.run_id, e.step, &e.kind, &e.detail),
        )?;
        Ok(())
    }

    /// Every checkpoint-lifecycle event recorded for a run, in order.
    pub fn checkpoint_events(&self, run_id: i64) -> Result<Vec<CheckpointEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT run_id, step, kind, detail
             FROM checkpoint_events WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(CheckpointEvent {
                run_id: r.get(0)?,
                step: r.get::<_, i64>(1)? as u32,
                kind: r.get(2)?,
                detail: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Set the durable run status (`running`, `paused`, `completed`, `failed`).
    pub fn set_status(&self, run_id: i64, status: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET status = ?1 WHERE id = ?2",
            (status, run_id),
        )?;
        Ok(())
    }

    /// The durable run status, if the run exists.
    pub fn status(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT status FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .ok())
    }

    /// Real wall-clock seconds elapsed since the run's `started_at`, from the
    /// database clock — so a budget over duration counts time that passed while
    /// the process was down, not just this process's uptime. Zero if the run has
    /// no start stamp (a pre-0.7.0 run).
    pub fn elapsed_secs(&self, run_id: i64) -> Result<f64> {
        let secs: Option<f64> = self.conn.query_row(
            "SELECT (julianday('now') - julianday(started_at)) * 86400.0
             FROM runs WHERE id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(secs.unwrap_or(0.0).max(0.0))
    }

    /// Total tokens recorded across this run's steps — the durable spend, so a
    /// resume restores the token budget instead of restarting it at zero.
    pub fn spent_tokens(&self, run_id: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(tokens), 0) FROM steps WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Every run id in the tree rooted at `root` (the root plus all descendants),
    /// via the `parent_run_id` edge — the set a tree-level resume re-drives.
    pub fn tree_run_ids(&self, root: i64) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT id FROM tree ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([root], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// Total tokens spent across the whole tree rooted at `root` — the durable
    /// aggregate-ledger spend restored on a tree resume.
    pub fn spent_tokens_tree(&self, root: i64) -> Result<u64> {
        let n: i64 = self.conn.query_row(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT COALESCE(SUM(s.tokens), 0)
             FROM steps s JOIN tree ON s.run_id = tree.id",
            [root],
            |r| r.get(0),
        )?;
        Ok(n as u64)
    }

    /// Number of agents (run rows) in the tree rooted at `root` — the durable
    /// agent count restored on a tree resume.
    pub fn agent_count_tree(&self, root: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "WITH RECURSIVE tree(id) AS (
                 SELECT id FROM runs WHERE id = ?1
                 UNION ALL
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT COUNT(*) FROM tree",
            [root],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Persist a spawned child's contract so a crashed tree can rebuild and
    /// resume that exact child on resume instead of spawning a duplicate. Keyed
    /// by (parent, step, goal) so a replayed spawn step adopts the existing child.
    #[allow(clippy::too_many_arguments)]
    pub fn record_spawn(
        &self,
        parent_run_id: i64,
        step: u32,
        child_run_id: i64,
        goal: &str,
        verify_file: &str,
        needle: &str,
        max_steps: Option<u32>,
        deny_write_json: &str,
    ) -> Result<()> {
        self.conn.execute(
            "INSERT INTO spawns
                 (parent_run_id, step, child_run_id, goal, verify_file, needle, max_steps, deny_write)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            (
                parent_run_id,
                step,
                child_run_id,
                goal,
                verify_file,
                needle,
                max_steps,
                deny_write_json,
            ),
        )?;
        Ok(())
    }

    /// Find the child spawned by `parent_run_id` at `step` for `goal`, if any —
    /// the adopt-on-resume lookup that makes a replayed spawn step idempotent.
    pub fn find_spawn(
        &self,
        parent_run_id: i64,
        step: u32,
        goal: &str,
    ) -> Result<Option<SpawnRow>> {
        Ok(self
            .conn
            .query_row(
                "SELECT child_run_id, goal, verify_file, needle, max_steps, deny_write
                 FROM spawns WHERE parent_run_id = ?1 AND step = ?2 AND goal = ?3
                 ORDER BY id ASC LIMIT 1",
                (parent_run_id, step, goal),
                |r| {
                    Ok(SpawnRow {
                        child_run_id: r.get(0)?,
                        goal: r.get(1)?,
                        verify_file: r.get(2)?,
                        needle: r.get(3)?,
                        max_steps: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
                        deny_write: r.get(5)?,
                    })
                },
            )
            .ok())
    }

    /// Check a run can be resumed from its checkpoint, or return a typed
    /// [`Error::Resume`]. Refuses a store written by a newer checkpoint format
    /// (rather than misreading a layout it does not understand) and a run id that
    /// does not exist. An already-`completed` run is resumable as a no-op, so it
    /// is not refused here.
    pub fn check_resumable(&self, run_id: i64) -> Result<()> {
        let format: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if format > CHECKPOINT_FORMAT {
            return Err(Error::Resume {
                reason: format!(
                    "checkpoint format {format} is newer than supported {CHECKPOINT_FORMAT}; \
                     upgrade io-harness to resume this run"
                ),
            });
        }
        let exists: bool = self
            .conn
            .query_row("SELECT 1 FROM runs WHERE id = ?1", [run_id], |_| Ok(true))
            .unwrap_or(false);
        if !exists {
            return Err(Error::Resume {
                reason: format!("no run with id {run_id} in the store"),
            });
        }
        Ok(())
    }

    /// Record which provider ran this run, for the audit trace.
    pub fn set_provider(&self, run_id: i64, provider: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE runs SET provider = ?1 WHERE id = ?2",
            (provider, run_id),
        )?;
        Ok(())
    }

    /// The provider recorded for a run, if any.
    pub fn provider(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT provider FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })?)
    }

    /// Record the run's final outcome, and derive the durable status from it:
    /// `success` completes the run, `awaiting_approval` pauses it, any other
    /// terminal outcome completes it (finished, just not with success). A run
    /// that crashed mid-loop never reaches here, so it stays `running` and is
    /// resumable.
    pub fn finish_run(&self, run_id: i64, outcome: &str) -> Result<()> {
        let status = match outcome {
            "awaiting_approval" => "paused",
            _ => "completed",
        };
        self.conn.execute(
            "UPDATE runs SET outcome = ?1, status = ?2 WHERE id = ?3",
            (outcome, status, run_id),
        )?;
        // A paused run has not finished — it is waiting for a human and will be
        // resumed — so it gets no summary yet. It gets one when it really ends.
        if status == "completed" {
            self.write_summary(run_id, outcome)?;
        }
        Ok(())
    }

    /// Record the run's outcome summary. Called by [`Self::finish_run`].
    ///
    /// Written here rather than assembled by the caller because a run that
    /// escalates or is refused returns `Err` and never reaches a
    /// [`RunResult`](crate::RunResult) at all — so a summary built at the call
    /// site would be missing for exactly the endings a scoring tool most wants to
    /// count.
    fn write_summary(&self, run_id: i64, outcome: &str) -> Result<()> {
        // Both stamps from the database clock, like `started_at`. Mixing SQLite's
        // clock with the process's would make the difference meaningless.
        let (finished_at, duration_ms): (String, Option<f64>) = self.conn.query_row(
            "SELECT strftime('%Y-%m-%dT%H:%M:%fZ','now'),
                    (julianday('now') - julianday(started_at)) * 86400000.0
             FROM runs WHERE id = ?1",
            [run_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        let duration_ms = duration_ms.map(|ms| ms.max(0.0) as u64);
        // `INSERT OR REPLACE`, because `finish_run` is reachable more than once for
        // one run: a paused run resumes and finishes, and a resume of an already
        // finished run is documented as idempotent. The last ending is the true one.
        self.conn.execute(
            "INSERT OR REPLACE INTO run_outcomes
                 (run_id, outcome, success, steps, tokens, duration_ms, finished_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            (
                run_id,
                outcome,
                i64::from(outcome == SUCCESS_OUTCOME),
                self.last_step(run_id)?,
                self.spent_tokens(run_id)?,
                duration_ms,
                &finished_at,
            ),
        )?;
        Ok(())
    }

    /// What a finished run cost and whether it worked.
    ///
    /// `None` if the run has not finished, is paused awaiting a human, or was
    /// finished by a pre-0.12.0 binary — a missing summary is reported as absent
    /// rather than as a row of zeroes, which would be indistinguishable from a run
    /// that did nothing.
    pub fn run_summary(&self, run_id: i64) -> Result<Option<RunSummary>> {
        let mut q = self.conn.prepare(
            "SELECT run_id, outcome, success, steps, tokens, duration_ms, finished_at
             FROM run_outcomes WHERE run_id = ?1",
        )?;
        let mut rows = q.query_map([run_id], |r| {
            Ok(RunSummary {
                run_id: r.get(0)?,
                outcome: r.get(1)?,
                success: r.get::<_, i64>(2)? != 0,
                steps: r.get(3)?,
                tokens: r.get(4)?,
                duration_ms: r.get(5)?,
                finished_at: r.get(6)?,
            })
        })?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    /// Every run in this store, newest first.
    ///
    /// Exists because an escalation returns `Err` rather than a
    /// [`RunResult`](crate::RunResult), so a caller whose run escalated has no
    /// `run_id` to resume with and therefore no way to reach
    /// [`RunOutcome::Escalated`](crate::RunOutcome::Escalated) — the outcome added
    /// for exactly that case. A caller who did not record the id before starting
    /// can find it here.
    pub fn runs(&self) -> Result<Vec<i64>> {
        let mut stmt = self.conn.prepare("SELECT id FROM runs ORDER BY id DESC")?;
        let rows = stmt.query_map([], |r| r.get(0))?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// The most recently started run, if this store holds one.
    ///
    /// A convenience over [`Store::runs`] for the common single-run case. With
    /// concurrent runs in one store, "most recent" is by insertion order and a
    /// caller that cares should track its own ids.
    pub fn last_run(&self) -> Result<Option<i64>> {
        Ok(self.runs()?.into_iter().next())
    }

    /// The recorded final outcome string of a run, if it has finished.
    pub fn outcome(&self, run_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT outcome FROM runs WHERE id = ?1", [run_id], |r| {
                r.get(0)
            })
            .ok()
            .flatten())
    }

    /// The durable run status as a typed [`RunStatus`], if the run exists.
    pub fn run_status(&self, run_id: i64) -> Result<Option<RunStatus>> {
        Ok(self.status(run_id)?.map(|s| RunStatus::from_str(&s)))
    }

    /// The highest step number recorded for a run, or 0 if none — the resume
    /// point for [`crate::resume`].
    pub fn last_step(&self, run_id: i64) -> Result<u32> {
        let n: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(step), 0) FROM steps WHERE run_id = ?1",
            [run_id],
            |r| r.get(0),
        )?;
        Ok(n as u32)
    }

    /// Read every step of a run back, in order, as the full trace.
    /// The run's trace reduced to the part that two identical runs must match,
    /// as diffable text.
    ///
    /// This is the crate's definition of "the same run twice", and it exists
    /// because equality could not be row identity: `steps` has no
    /// `UNIQUE(run_id, step)` and a retry inserts its own row under the step
    /// number the eventual commit will reuse, so counting or comparing rows
    /// compares trace entries rather than agent behaviour.
    ///
    /// # What is compared
    ///
    /// Every `steps` row — step number, decision, result, prompt, tool call and
    /// tokens — and every `context_events` row's step, kind and detail. Between
    /// them these are what the agent was shown, what it decided, what it did, and
    /// what that cost.
    ///
    /// # What is excluded, and why
    ///
    /// Everything whose value is a fact about *this* execution rather than about
    /// the run:
    ///
    /// - **Wall-clock stamps** — `runs.started_at`, `memory.created_at`,
    ///   `run_outcomes.finished_at` and `duration_ms`. Two runs of the same case
    ///   take different amounts of time; that is not a divergence.
    /// - **`mcp_events.millis`** — a measured duration, for the same reason.
    /// - **`sandbox_events.detail`** — it carries the argv, and the argv carries
    ///   an ephemeral tempdir path that is different every run by design.
    /// - **Run and child ids** — `AUTOINCREMENT` values, meaningful only within
    ///   one store.
    ///
    /// Excluding a field is a decision that this crate cannot promise it, not a
    /// convenience. Anything added to this list should be added to this doc with
    /// its reason, because a comparison that quietly excludes what it cannot
    /// match is a comparison that asserts nothing.
    ///
    /// # What it assumes
    ///
    /// That each run being compared has its **own fresh store**. Run ids are
    /// excluded from the text, but a child agent's run id is embedded in the
    /// parent's composed observation (`[child 5 "goal" -> …]`), which is real
    /// content the model was shown. In a fresh store those ids start at 1 and are
    /// allocated in spawn order, so they match; in a shared store the second run's
    /// ids are higher and the traces differ for a reason that has nothing to do
    /// with the agent.
    ///
    /// Deterministic replay also requires the provider to answer identically —
    /// see [`Replay`](crate::provider::Replay) — and the same workspace state to
    /// start from.
    pub fn canonical_trace(&self, run_id: i64) -> Result<String> {
        let mut out = String::new();
        for s in self.steps(run_id)? {
            out.push_str(&format!(
                "step {} | tokens {} | decision {} | tool_call {} | prompt {} | result {}\n",
                s.step, s.tokens, s.decision, s.tool_call, s.prompt, s.result
            ));
        }
        for e in self.context_events(run_id)? {
            out.push_str(&format!(
                "context {} | {} | {}\n",
                e.step,
                e.kind,
                e.detail.as_deref().unwrap_or("")
            ));
        }
        Ok(out)
    }

    pub fn steps(&self, run_id: i64) -> Result<Vec<StepRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT step, decision, result, prompt, tool_call, tokens
             FROM steps WHERE run_id = ?1 ORDER BY step ASC, id ASC",
        )?;
        let rows = stmt.query_map([run_id], |r| {
            Ok(StepRecord {
                step: r.get::<_, i64>(0)? as u32,
                decision: r.get(1)?,
                result: r.get(2)?,
                prompt: r.get(3)?,
                tool_call: r.get(4)?,
                tokens: r.get::<_, i64>(5)? as u64,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.21.0: the question channel ----

    /// Persist a question nobody in this process could answer, and return its id.
    ///
    /// The mirror of [`Self::put_pending`], deliberately: a question survives a
    /// process exit for exactly the reason a pending approval does, and the two stay
    /// in separate tables because they are separate things — one asks whether an act
    /// is permitted, the other what the operator meant.
    pub fn put_question(
        &self,
        run_id: i64,
        step: u32,
        q: &crate::approve::Question,
    ) -> Result<i64> {
        let choices = if q.choices.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&q.choices).unwrap_or_default())
        };
        self.conn.execute(
            "INSERT INTO pending_questions (run_id, step, question, context, choices)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![run_id, step, q.question, q.context, choices],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Read one question by id, answered or not.
    pub fn question(&self, question_id: i64) -> Result<Option<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions WHERE id = ?1",
        )?;
        let mut rows = stmt.query([question_id])?;
        match rows.next()? {
            Some(r) => Ok(Some(question_row(r)?)),
            None => Ok(None),
        }
    }

    /// The answer already recorded for this exact question on this run and step, if
    /// there is one.
    ///
    /// A query for a caller reconstructing a run, **not** the mechanism a resume uses.
    /// The step that asks a question is committed before the run pauses, so a resume
    /// starts at the step after it and the `ask_question` call is never replayed —
    /// [`resume_with_answer`](crate::resume_with_answer) delivers the answer as an
    /// observation instead. See [`Self::questions`] for the whole conversation.
    pub fn answered_question(
        &self,
        run_id: i64,
        step: u32,
        question: &str,
    ) -> Result<Option<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions
             WHERE run_id = ?1 AND step = ?2 AND question = ?3 AND resolved = 1
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut rows = stmt.query(rusqlite::params![run_id, step, question])?;
        match rows.next()? {
            Some(r) => Ok(Some(question_row(r)?)),
            None => Ok(None),
        }
    }

    /// Record an answer and mark the question resolved.
    ///
    /// `by` is `"responder"` or `"human"`. Answering an already-answered question is
    /// an [`Error::Resume`] rather than a silent second write, the way
    /// [`Self::resolve_pending`] refuses a second decision: two answers to one
    /// question means one of them was never acted on, and a caller should hear which.
    pub fn answer_question(&self, question_id: i64, answer: &str, by: &str) -> Result<()> {
        let existing = self.question(question_id)?;
        match existing {
            None => {
                return Err(Error::Resume {
                    reason: format!("no question {question_id} to answer"),
                })
            }
            Some(q) if q.resolved => {
                return Err(Error::Resume {
                    reason: format!("question {question_id} was already answered"),
                })
            }
            Some(_) => {}
        }
        self.conn.execute(
            "UPDATE pending_questions SET answer = ?2, answered_by = ?3, resolved = 1
             WHERE id = ?1",
            rusqlite::params![question_id, answer, by],
        )?;
        Ok(())
    }

    /// Every question asked on a run, in the order they were asked.
    pub fn questions(&self, run_id: i64) -> Result<Vec<PendingQuestion>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, step, question, context, choices, answer, answered_by, resolved
             FROM pending_questions WHERE run_id = ?1 ORDER BY id",
        )?;
        let rows = stmt.query_map([run_id], question_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // ---- 0.21.0: the agent's plan ----

    /// Replace this run's plan with `items`.
    ///
    /// Wholesale, in one transaction: the old rows go and the new ones land, so a
    /// reader on another connection sees the previous plan or the next one and never
    /// a half-written mixture of the two. That atomicity is the whole reason an
    /// operator can read a plan mid-run and trust what they see.
    ///
    /// Bounded like every other tool result in the crate rather than refused: at most
    /// [`TODO_MAX_ITEMS`] items, each at most [`TODO_TEXT_CAP`] characters. Returns
    /// how many items were dropped to hold the cap, so the caller can say so in the
    /// observation instead of letting a plan quietly lose its tail.
    ///
    /// Writes no trace row of its own — the run loop records the write where the
    /// step number is known, exactly as it does for [`Self::memory_put`].
    pub fn write_todos(&self, run_id: i64, items: &[TodoItem]) -> Result<usize> {
        let kept = items.len().min(TODO_MAX_ITEMS);
        let dropped = items.len() - kept;
        // One transaction: a reader on another connection sees the old plan or the
        // new one, never both halves.
        let tx = self.conn.unchecked_transaction()?;
        tx.execute("DELETE FROM todos WHERE run_id = ?1", [run_id])?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO todos (run_id, position, text, state) VALUES (?1, ?2, ?3, ?4)",
            )?;
            for (i, item) in items.iter().take(kept).enumerate() {
                let text: String = item.text.chars().take(TODO_TEXT_CAP).collect();
                stmt.execute(rusqlite::params![
                    run_id,
                    i as i64,
                    text,
                    item.state.as_str()
                ])?;
            }
        }
        tx.commit()?;
        Ok(dropped)
    }

    /// This run's plan, in the order the agent wrote it.
    ///
    /// Empty for a run that never wrote one, and empty — not absent — for a run that
    /// cleared its plan, because an agent that finished its work and emptied its list
    /// is not an agent that never had one.
    ///
    /// A row whose `state` is not one [`TodoState`] understands is skipped rather than
    /// guessed at; the writer above only ever writes the three, so this can only
    /// happen to a database another program has written to.
    pub fn todos(&self, run_id: i64) -> Result<Vec<TodoItem>> {
        let mut stmt = self
            .conn
            .prepare("SELECT text, state FROM todos WHERE run_id = ?1 ORDER BY position")?;
        let rows = stmt.query_map([run_id], |r| {
            let text: String = r.get(0)?;
            let state: String = r.get(1)?;
            Ok((text, state))
        })?;
        let mut out = Vec::new();
        for row in rows {
            let (text, state) = row?;
            if let Some(state) = TodoState::parse(&state) {
                out.push(TodoItem { text, state });
            }
        }
        Ok(out)
    }

    // ---- 0.10.0: durable cross-run memory ----

    /// Write or replace `key` for `workspace`, attributed to the run and step
    /// that wrote it. A value past [`MEMORY_MAX_ENTRY_CHARS`] is truncated with
    /// a visible marker rather than refused. Returns the keys evicted to stay
    /// inside the caps, oldest first — the caller records the eviction in the
    /// trace; this never writes a trace row itself.
    pub fn memory_put(
        &self,
        workspace: &str,
        key: &str,
        value: &str,
        run_id: i64,
        step: u32,
    ) -> Result<Vec<String>> {
        let value = truncate_memory_value(value);
        // An overwrite re-attributes the entry and refreshes `created_at`, so
        // recency ordering reflects the latest write rather than the first.
        self.conn.execute(
            "INSERT INTO memory (workspace, key, value, run_id, step, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, strftime('%Y-%m-%dT%H:%M:%fZ','now'))
             ON CONFLICT(workspace, key) DO UPDATE SET
                 value      = excluded.value,
                 run_id     = excluded.run_id,
                 step       = excluded.step,
                 created_at = excluded.created_at",
            (workspace, key, &value, run_id, step),
        )?;
        self.enforce_memory_caps(workspace, key)
    }

    /// Evict this workspace's oldest entries until both caps hold, never the
    /// entry `keep` (the one just written — evicting it would make a write a
    /// silent no-op). Returns the evicted keys in eviction order.
    fn enforce_memory_caps(&self, workspace: &str, keep: &str) -> Result<Vec<String>> {
        // LENGTH() on TEXT counts characters, not bytes — the cap is in chars.
        let rows: Vec<(String, i64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT key, LENGTH(value) FROM memory WHERE workspace = ?1
                 ORDER BY created_at ASC, id ASC",
            )?;
            let rows = stmt.query_map([workspace], |r| Ok((r.get(0)?, r.get(1)?)))?;
            rows.collect::<std::result::Result<_, _>>()?
        };

        let mut count = rows.len();
        let mut chars: i64 = rows.iter().map(|(_, n)| *n).sum();
        let mut evicted = Vec::new();
        for (key, n) in &rows {
            if count <= MEMORY_MAX_ENTRIES && chars <= MEMORY_MAX_CHARS as i64 {
                break;
            }
            if key == keep {
                continue;
            }
            self.conn.execute(
                "DELETE FROM memory WHERE workspace = ?1 AND key = ?2",
                (workspace, key),
            )?;
            count -= 1;
            chars -= n;
            evicted.push(key.clone());
        }
        Ok(evicted)
    }

    /// Every entry for `workspace`, oldest first. Never another workspace's.
    pub fn memory_list(&self, workspace: &str) -> Result<Vec<MemoryEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT key, value, run_id, step, created_at FROM memory
             WHERE workspace = ?1 ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([workspace], |r| {
            Ok(MemoryEntry {
                key: r.get(0)?,
                value: r.get(1)?,
                run_id: r.get(2)?,
                step: r.get::<_, i64>(3)? as u32,
                created_at: r.get(4)?,
            })
        })?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    // -----------------------------------------------------------------------
    // 0.20.0 — the session tree. The conversation's shape lives here; what a
    // turn did lives in the run tables under its `run_id`.
    // -----------------------------------------------------------------------

    /// Open a new session over `root`. Returns its id, which is all a later
    /// process needs to pick the conversation back up.
    pub fn create_session(&self, root: &str) -> Result<i64> {
        self.conn
            .execute("INSERT INTO sessions (root) VALUES (?1)", [root])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The root a session was opened over, or `None` if no such session exists.
    ///
    /// A reopen reads the root from here rather than taking it from the caller
    /// again: a session whose workspace moved between processes would otherwise
    /// carry a conversation about one directory into another.
    pub fn session_root(&self, session_id: i64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT root FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get(0),
            )
            .ok())
    }

    /// Which turn a session is currently answering from.
    pub fn session_head(&self, session_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT head_turn_id FROM sessions WHERE id = ?1",
                [session_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten())
    }

    /// Move a session's head. Called when a turn is taken and when a caller
    /// branches from an earlier one.
    pub fn set_session_head(&self, session_id: i64, turn_id: Option<i64>) -> Result<()> {
        self.conn.execute(
            "UPDATE sessions SET head_turn_id = ?1 WHERE id = ?2",
            (turn_id, session_id),
        )?;
        Ok(())
    }

    /// Record a turn against a session, under the run that will serve it.
    ///
    /// Written before the run loop starts, so a turn whose process dies mid-answer
    /// is still in the tree with a `run_id` a resume can continue from — the same
    /// reason a run row exists before the first completion is billed.
    pub fn record_turn(
        &self,
        session_id: i64,
        parent_turn_id: Option<i64>,
        run_id: i64,
        prompt: &str,
    ) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO session_turns (session_id, parent_turn_id, run_id, prompt)
             VALUES (?1, ?2, ?3, ?4)",
            (session_id, parent_turn_id, run_id, prompt),
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Close a turn with what the agent said and why it stopped. Append-only in
    /// spirit: the prompt and the parentage a turn was created with never change.
    pub fn finish_turn(&self, turn_id: i64, reply: Option<&str>, outcome: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE session_turns SET reply = ?1, outcome = ?2 WHERE id = ?3",
            (reply, outcome, turn_id),
        )?;
        Ok(())
    }

    /// One turn by id, if it exists.
    pub fn session_turn(&self, turn_id: i64) -> Result<Option<Turn>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, session_id, parent_turn_id, run_id, prompt, reply, outcome, created_at
                 FROM session_turns WHERE id = ?1",
                [turn_id],
                turn_row,
            )
            .ok())
    }

    /// Which turn a run served, if it served one.
    ///
    /// The seam between the two halves of a turn: the run loop writes the row, and
    /// the session reads its id back rather than being handed it — the run id is
    /// the only thing both halves are guaranteed to know.
    pub fn turn_for_run(&self, run_id: i64) -> Result<Option<i64>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id FROM session_turns WHERE run_id = ?1",
                [run_id],
                |r| r.get(0),
            )
            .ok())
    }

    /// Every turn of a session, oldest first — the whole tree, not one path
    /// through it. [`crate::Session::history`] is the path.
    pub fn session_turns(&self, session_id: i64) -> Result<Vec<Turn>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, session_id, parent_turn_id, run_id, prompt, reply, outcome, created_at
             FROM session_turns WHERE session_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map([session_id], turn_row)?;
        Ok(rows.collect::<std::result::Result<_, _>>()?)
    }

    /// One entry of `workspace` by key, if it holds one.
    pub fn memory_get(&self, workspace: &str, key: &str) -> Result<Option<MemoryEntry>> {
        Ok(self
            .conn
            .query_row(
                "SELECT key, value, run_id, step, created_at FROM memory
                 WHERE workspace = ?1 AND key = ?2",
                (workspace, key),
                |r| {
                    Ok(MemoryEntry {
                        key: r.get(0)?,
                        value: r.get(1)?,
                        run_id: r.get(2)?,
                        step: r.get::<_, i64>(3)? as u32,
                        created_at: r.get(4)?,
                    })
                },
            )
            .ok())
    }

    /// Forget one entry of `workspace`. True when an entry was removed.
    pub fn memory_delete(&self, workspace: &str, key: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM memory WHERE workspace = ?1 AND key = ?2",
            (workspace, key),
        )?;
        Ok(n > 0)
    }

    /// Removes every entry for `workspace`; returns how many. Other workspaces
    /// keep theirs.
    pub fn memory_clear(&self, workspace: &str) -> Result<usize> {
        Ok(self
            .conn
            .execute("DELETE FROM memory WHERE workspace = ?1", [workspace])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refusals_record_action_target_rule_and_layer() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::refusal(2, "write", "secrets/key.txt").with_rule("secrets/*", "base"),
            )
            .unwrap();

        let events = store.events(run).unwrap();
        assert_eq!(events.len(), 1);
        let e = &events[0];
        assert_eq!(e.kind, "refusal");
        assert_eq!(e.act, "write");
        assert_eq!(e.target, "secrets/key.txt");
        assert_eq!(e.rule.as_deref(), Some("secrets/*"));
        // Attributable to the layer that refused, so a base-layer deny is findable.
        assert_eq!(e.layer.as_deref(), Some("base"));
    }

    #[test]
    fn decisions_record_their_value_source_and_any_altered_target() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::decision(1, "write", "src/a.rs", "approve", "stdin")
                    .with_performed("src/sandbox/a.rs"),
            )
            .unwrap();
        store
            .record_event(
                run,
                &PolicyEvent::decision(2, "write", "src/b.rs", "approve", "remembered"),
            )
            .unwrap();

        let events = store.events(run).unwrap();
        assert_eq!(events.len(), 2);
        // Requested and performed forms are distinguishable.
        assert_eq!(events[0].decision.as_deref(), Some("approve"));
        assert_eq!(events[0].target, "src/a.rs");
        assert_eq!(events[0].performed.as_deref(), Some("src/sandbox/a.rs"));
        // An auto-approval by a remembered rule is not confusable with a fresh one.
        assert_eq!(events[1].source.as_deref(), Some("remembered"));
        assert_eq!(events[1].performed, None);
    }

    #[test]
    fn a_pre_0_4_database_migrates_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.3.0-shaped database: runs + steps only, no policy tables.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL,
                     file TEXT NOT NULL, outcome TEXT, provider TEXT);
                 CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL,
                     step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL,
                     prompt TEXT NOT NULL DEFAULT '', tool_call TEXT NOT NULL DEFAULT '',
                     tokens INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (goal, file) VALUES ('old goal', 'old.txt');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        // The pre-existing row survives; the new tables are usable.
        assert_eq!(store.last_step(1).unwrap(), 0);
        store
            .record_event(1, &PolicyEvent::refusal(1, "read", ".env"))
            .unwrap();
        assert_eq!(store.events(1).unwrap().len(), 1);
    }

    /// NF1 — a 0.19.0 database gains the two session tables on open and keeps
    /// everything it had. The integration test cannot write a pre-session schema
    /// (`Store::open` always creates them), so the legacy shape is built here.
    #[test]
    fn a_pre_session_database_gains_the_session_tables_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.19.0-shaped database: everything except `sessions` and
        // `session_turns`, which is what the version before this one wrote.
        {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("an older run", "notes.md").unwrap();
            store.finish_run(run, "success").unwrap();
            store
                .conn
                .execute_batch("DROP TABLE sessions; DROP TABLE session_turns;")
                .unwrap();
            // No format bump means the old file is still resumable by this binary.
            let format: i64 = store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap();
            assert_eq!(format, CHECKPOINT_FORMAT);
        }

        let store = Store::open(&path).unwrap();
        // The run it already had is untouched...
        assert_eq!(
            store.run_summary(1).unwrap().map(|s| s.outcome),
            Some("success".to_string())
        );
        // ...and a conversation works over the same file.
        let session = store.create_session("/repo").unwrap();
        let run = store.start_run("a turn", "/repo").unwrap();
        let turn = store.record_turn(session, None, run, "hello").unwrap();
        assert_eq!(store.turn_for_run(run).unwrap(), Some(turn));
        assert_eq!(store.session_turns(session).unwrap().len(), 1);
        assert_eq!(
            store.session_root(session).unwrap().as_deref(),
            Some("/repo")
        );
    }

    /// A branch is two turns with one parent, and reading one path never sees the
    /// other's turns. The tree half of F3 at the store level, where the walk
    /// [`crate::Session::history`] performs is one query.
    #[test]
    fn two_turns_may_share_a_parent_and_neither_is_rewritten() {
        let store = Store::memory().unwrap();
        let session = store.create_session("/repo").unwrap();
        let run = |n: &str| store.start_run(n, "/repo").unwrap();

        let root = store
            .record_turn(session, None, run("t1"), "plan it")
            .unwrap();
        let left = store
            .record_turn(session, Some(root), run("t2"), "plan A")
            .unwrap();
        let right = store
            .record_turn(session, Some(root), run("t3"), "plan B")
            .unwrap();

        store.finish_turn(left, Some("did A"), "finished").unwrap();
        // Closing one branch does not touch the other.
        assert_eq!(
            store.session_turn(right).unwrap().unwrap().reply,
            None,
            "closing a sibling turn changed this one"
        );
        assert_eq!(
            store.session_turn(left).unwrap().unwrap().reply.as_deref(),
            Some("did A")
        );
        let all = store.session_turns(session).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!(
            all.iter()
                .filter(|t| t.parent_turn_id == Some(root))
                .count(),
            2
        );
    }

    #[test]
    fn a_pending_approval_survives_the_store_being_reopened() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        let request_id = {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("goal", "root").unwrap();
            store
                .put_pending(run, 3, "write", "src/a.rs", Some("fn a() {}"))
                .unwrap()
        };

        // A different Store over the same file — the process that created it is gone.
        let store = Store::open(&path).unwrap();
        let p = store.pending(request_id).unwrap().expect("still pending");
        assert_eq!(p.step, 3);
        assert_eq!(p.act, "write");
        assert_eq!(p.target, "src/a.rs");
        assert_eq!(p.content.as_deref(), Some("fn a() {}"));
        assert_eq!(p.resolved, None);

        store.resolve_pending(request_id, "approve").unwrap();
        let p = store.pending(request_id).unwrap().unwrap();
        assert_eq!(p.resolved.as_deref(), Some("approve"));
    }

    #[test]
    fn the_tree_is_reconstructable_from_a_reopened_store() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A parent spawns two children (one nests a grandchild) and the tree
        // draws against its ceiling, then everything is dropped.
        let (root, c1, c2, gc) = {
            let store = Store::open(&path).unwrap();
            let root = store.start_run("root goal", "ws").unwrap();
            let c1 = store.start_child_run("child 1", "ws", root, 1).unwrap();
            let c2 = store.start_child_run("child 2", "ws", root, 1).unwrap();
            let gc = store.start_child_run("grandchild", "ws", c1, 2).unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(root, 1, c1, "child 1"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(root, 1, c2, "child 2"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(c1, 1, gc, "grandchild"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::spawn_refused(root, 2, "agents"))
                .unwrap();
            store
                .record_agent_event(&AgentEvent::budget_draw(c1, 1, 30, 70))
                .unwrap();
            (root, c1, c2, gc)
        };

        // A fresh Store over the same file — the process that built the tree is gone.
        let store = Store::open(&path).unwrap();
        // The parent/child edges rebuild the graph.
        assert_eq!(store.children(root).unwrap(), vec![c1, c2]);
        assert_eq!(store.children(c1).unwrap(), vec![gc]);
        assert_eq!(store.parent(gc).unwrap(), Some(c1));
        assert_eq!(store.parent(root).unwrap(), None);
        assert_eq!(store.depth(gc).unwrap(), 2);

        // Spawns, the refusal, and the draw are all recorded.
        let root_events = store.agent_events(root).unwrap();
        assert_eq!(root_events.iter().filter(|e| e.kind == "spawn").count(), 2);
        assert_eq!(
            root_events
                .iter()
                .filter(|e| e.kind == "spawn_refused")
                .count(),
            1
        );
        let draws = store.agent_events(c1).unwrap();
        let draw = draws.iter().find(|e| e.kind == "budget_draw").unwrap();
        assert_eq!(draw.tokens, Some(30));
        assert_eq!(draw.remaining, Some(70));
    }

    #[test]
    fn a_pre_0_5_database_migrates_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.4.0-shaped database: runs (no parent_run_id/depth), steps, and the
        // policy tables, with a row.
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL,
                     file TEXT NOT NULL, outcome TEXT, provider TEXT);
                 CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL,
                     step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL,
                     prompt TEXT NOT NULL DEFAULT '', tool_call TEXT NOT NULL DEFAULT '',
                     tokens INTEGER NOT NULL DEFAULT 0);
                 INSERT INTO runs (goal, file) VALUES ('old', 'old.txt');",
            )
            .unwrap();
        }

        let store = Store::open(&path).unwrap();
        // The pre-existing row survives and reads as a root at depth 0.
        assert_eq!(store.parent(1).unwrap(), None);
        assert_eq!(store.depth(1).unwrap(), 0);
        // The new table is usable.
        let child = store.start_child_run("c", "ws", 1, 1).unwrap();
        assert_eq!(store.children(1).unwrap(), vec![child]);
    }

    #[test]
    fn a_pre_0_8_database_migrates_in_place_and_keeps_its_rows() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.7.0-shaped database: everything through checkpoints, and no
        // mcp_events table.
        {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("old goal", "old.txt").unwrap();
            store
                .checkpoint_step(run, &StepRecord::new(1, "wrote", "ok"))
                .unwrap();
            store
                .record_event(run, &PolicyEvent::refusal(1, "write", "secrets/k"))
                .unwrap();
            store
                .conn
                .execute("DROP TABLE IF EXISTS mcp_events", [])
                .unwrap();
        }

        // Reopening migrates it: the old rows are intact and the new table works.
        let store = Store::open(&path).unwrap();
        assert_eq!(store.last_step(1).unwrap(), 1);
        assert_eq!(store.events(1).unwrap().len(), 1);
        assert!(store.mcp_events(1).unwrap().is_empty());
        store
            .record_mcp(1, &McpEvent::connected("files", "stdio"))
            .unwrap();
        let events = store.mcp_events(1).unwrap();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].detail.as_deref(), Some("stdio"));

        // And a 0.7.0 binary, which never queries mcp_events, still reads it —
        // nothing it knows about was altered or rewritten.
        assert_eq!(store.steps(1).unwrap().len(), 1);
        assert_eq!(store.run_status(1).unwrap(), Some(RunStatus::Running));
    }

    #[test]
    fn full_trace_persists_and_reads_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "out.txt").unwrap();
        store
            .record(
                run,
                &StepRecord::new(1, "wrote file", "content v1").with_trace(
                    "the prompt",
                    r#"{"content":"content v1"}"#,
                    128,
                ),
            )
            .unwrap();
        store
            .record(run, &StepRecord::new(2, "verified", "ok"))
            .unwrap();
        store.finish_run(run, "success").unwrap();

        let steps = store.steps(run).unwrap();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].decision, "wrote file");
        assert_eq!(steps[0].prompt, "the prompt");
        assert_eq!(steps[0].tokens, 128);
        assert_eq!(steps[1].result, "ok");
        assert_eq!(store.last_step(run).unwrap(), 2);
    }

    #[test]
    fn migrates_a_0_1_0_steps_table_in_place() {
        // A 0.1.0 database: `steps` without the trace columns, with a row.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT);
             CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL, step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');
             INSERT INTO steps (run_id, step, decision, result) VALUES (1, 1, 'wrote file', 'old');",
        )
        .unwrap();

        // Opening through Store migrates it; the old row survives with defaults.
        let store = Store::from_conn(conn).unwrap();
        let steps = store.steps(1).unwrap();
        assert_eq!(steps.len(), 1);
        assert_eq!(steps[0].result, "old");
        assert_eq!(steps[0].prompt, "");
        assert_eq!(steps[0].tokens, 0);
    }

    #[test]
    fn provider_is_recorded_and_read_back() {
        let store = Store::memory().unwrap();
        let run = store.start_run("g", "f").unwrap();
        assert_eq!(store.provider(run).unwrap(), None);
        store.set_provider(run, "anthropic").unwrap();
        assert_eq!(store.provider(run).unwrap().as_deref(), Some("anthropic"));
    }

    #[test]
    fn migrates_a_pre_0_3_runs_table_adding_provider() {
        // A 0.1/0.2 database: `runs` without the provider column.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT);
             CREATE TABLE steps (id INTEGER PRIMARY KEY AUTOINCREMENT, run_id INTEGER NOT NULL, step INTEGER NOT NULL, decision TEXT NOT NULL, result TEXT NOT NULL);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');",
        )
        .unwrap();

        // Opening through Store adds the provider column; the old row survives.
        let store = Store::from_conn(conn).unwrap();
        assert_eq!(store.provider(1).unwrap(), None);
        store.set_provider(1, "openai").unwrap();
        assert_eq!(store.provider(1).unwrap().as_deref(), Some("openai"));
    }

    // ---- 0.7.0: durable checkpoint + resume ----

    #[test]
    fn checkpoint_step_commits_the_step_and_its_event_together() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "act", "ok"))
            .unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(2, "act", "ok"))
            .unwrap();

        assert_eq!(store.last_step(run).unwrap(), 2);
        assert_eq!(store.steps(run).unwrap().len(), 2);
        let cps: Vec<_> = store
            .checkpoint_events(run)
            .unwrap()
            .into_iter()
            .filter(|e| e.kind == "checkpoint")
            .collect();
        assert_eq!(cps.len(), 2);
        // NF4: a checkpoint event carries no file content — only step metadata.
        assert!(cps.iter().all(|e| e.detail.is_none()));
    }

    #[test]
    fn a_rolled_back_step_leaves_the_prior_checkpoint_intact() {
        // The committed checkpoint is the completion marker: a step whose
        // transaction never commits (a crash mid-commit) vanishes entirely and
        // the prior checkpoint stands — never a torn half recorded as done.
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "act", "ok"))
            .unwrap();

        // Simulate a crash mid-commit: open the step's transaction, write both
        // rows, then drop without committing (as a killed process would).
        {
            let tx = store.conn.unchecked_transaction().unwrap();
            tx.execute(
                "INSERT INTO steps (run_id, step, decision, result) VALUES (?1, 2, 'act', 'ok')",
                [run],
            )
            .unwrap();
            tx.execute(
                "INSERT INTO checkpoint_events (run_id, step, kind) VALUES (?1, 2, 'checkpoint')",
                [run],
            )
            .unwrap();
            // no tx.commit() — dropped here, rolling back.
        }

        assert_eq!(
            store.last_step(run).unwrap(),
            1,
            "the torn step must not survive"
        );
        assert_eq!(store.steps(run).unwrap().len(), 1);
    }

    #[test]
    fn check_resumable_refuses_a_newer_format_and_a_missing_run() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        assert!(store.check_resumable(run).is_ok());

        // A run id that does not exist is a typed Resume error, not a panic.
        assert!(matches!(
            store.check_resumable(9999),
            Err(Error::Resume { .. })
        ));

        // A store written by a newer checkpoint format is refused rather than
        // misread.
        store
            .conn
            .execute_batch(&format!("PRAGMA user_version = {}", CHECKPOINT_FORMAT + 1))
            .unwrap();
        assert!(matches!(
            store.check_resumable(run),
            Err(Error::Resume { .. })
        ));
    }

    #[test]
    fn spent_tokens_and_elapsed_are_durable_reads() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(1, "a", "ok").with_trace("p", "t", 30))
            .unwrap();
        store
            .checkpoint_step(run, &StepRecord::new(2, "a", "ok").with_trace("p", "t", 12))
            .unwrap();
        assert_eq!(store.spent_tokens(run).unwrap(), 42);
        assert!(store.elapsed_secs(run).unwrap() >= 0.0);
    }

    #[test]
    fn tree_aggregate_reads_span_root_and_descendants() {
        let store = Store::memory().unwrap();
        let root = store.start_run("goal", "root").unwrap();
        let child = store.start_child_run("sub", "root", root, 1).unwrap();
        let grandchild = store.start_child_run("subsub", "root", child, 2).unwrap();
        store
            .checkpoint_step(
                root,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 10),
            )
            .unwrap();
        store
            .checkpoint_step(
                child,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 20),
            )
            .unwrap();
        store
            .checkpoint_step(
                grandchild,
                &StepRecord::new(1, "a", "ok").with_trace("p", "t", 5),
            )
            .unwrap();

        assert_eq!(
            store.tree_run_ids(root).unwrap(),
            vec![root, child, grandchild]
        );
        assert_eq!(store.spent_tokens_tree(root).unwrap(), 35);
        assert_eq!(store.agent_count_tree(root).unwrap(), 3);
    }

    #[test]
    fn status_round_trips_and_a_pre_0_7_database_migrates() {
        // A 0.6.0-shaped database: runs without status/started_at.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE runs (id INTEGER PRIMARY KEY AUTOINCREMENT, goal TEXT NOT NULL, file TEXT NOT NULL, outcome TEXT, provider TEXT, parent_run_id INTEGER, depth INTEGER NOT NULL DEFAULT 0);
             INSERT INTO runs (goal, file) VALUES ('g', 'f');",
        )
        .unwrap();
        let store = Store::from_conn(conn).unwrap();
        // The old row gains a default status and no start stamp.
        assert_eq!(store.status(1).unwrap().as_deref(), Some("running"));
        store.set_status(1, "completed").unwrap();
        assert_eq!(store.status(1).unwrap().as_deref(), Some("completed"));
    }

    // ---- 0.10.0: durable cross-run memory ----

    #[test]
    fn the_entry_count_cap_evicts_oldest_first_and_never_the_new_entry() {
        let store = Store::memory().unwrap();
        for i in 0..MEMORY_MAX_ENTRIES {
            let evicted = store.memory_put("ws", &format!("k{i}"), "v", 1, 1).unwrap();
            assert!(evicted.is_empty(), "no eviction while under the cap");
        }
        assert_eq!(store.memory_list("ws").unwrap().len(), MEMORY_MAX_ENTRIES);

        // Three more writes cost exactly the three oldest keys, in order.
        let mut evicted = Vec::new();
        for i in 0..3 {
            evicted.extend(
                store
                    .memory_put("ws", &format!("new{i}"), "v", 2, 2)
                    .unwrap(),
            );
        }
        assert_eq!(evicted, vec!["k0", "k1", "k2"]);

        let keys: Vec<String> = store
            .memory_list("ws")
            .unwrap()
            .into_iter()
            .map(|e| e.key)
            .collect();
        assert_eq!(
            keys.len(),
            MEMORY_MAX_ENTRIES,
            "the cap holds after eviction"
        );
        assert!(!keys.contains(&"k0".to_string()));
        // The entry just written is never the one evicted to make room for it.
        for i in 0..3 {
            assert!(keys.contains(&format!("new{i}")));
        }
    }

    #[test]
    fn the_total_chars_cap_evicts_before_the_count_cap_is_reached() {
        let store = Store::memory().unwrap();
        let big = "x".repeat(MEMORY_MAX_ENTRY_CHARS);
        let mut evicted = Vec::new();
        // 10 entries of 2_000 chars = 20_000, past the 16_000 char cap while the
        // 64-entry cap is nowhere near.
        for i in 0..10 {
            evicted.extend(
                store
                    .memory_put("ws", &format!("k{i}"), &big, 1, 1)
                    .unwrap(),
            );
        }
        assert_eq!(
            evicted,
            vec!["k0", "k1"],
            "oldest first, count cap untouched"
        );

        let entries = store.memory_list("ws").unwrap();
        assert!(entries.len() < MEMORY_MAX_ENTRIES);
        let total: usize = entries.iter().map(|e| e.value.chars().count()).sum();
        assert!(total <= MEMORY_MAX_CHARS, "{total} chars is over the cap");
    }

    #[test]
    fn an_oversized_value_is_truncated_with_a_marker_not_rejected() {
        let store = Store::memory().unwrap();
        // Multibyte throughout, so a byte-wise cut would not be valid UTF-8.
        let huge = "é".repeat(MEMORY_MAX_ENTRY_CHARS * 2);
        assert!(store.memory_put("ws", "k", &huge, 1, 1).is_ok());

        let stored = store.memory_get("ws", "k").unwrap().unwrap().value;
        assert_eq!(stored.chars().count(), MEMORY_MAX_ENTRY_CHARS);
        assert!(stored.ends_with(MEMORY_TRUNCATED), "the cut is visible");
        // Cut on a char boundary: every kept char is the whole 'é', never a half.
        let kept = MEMORY_MAX_ENTRY_CHARS - MEMORY_TRUNCATED.chars().count();
        assert!(stored.chars().take(kept).all(|c| c == 'é'));
    }

    #[test]
    fn a_0_9_1_store_opens_unchanged_and_still_resumes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("runs.db");

        // A 0.9.1-shaped database: every table through mcp_events, rows in the
        // ones a resume reads, and no `memory` table.
        let before_format: i64 = {
            let store = Store::open(&path).unwrap();
            let run = store.start_run("old goal", "old.txt").unwrap();
            store
                .checkpoint_step(run, &StepRecord::new(1, "wrote", "ok"))
                .unwrap();
            store
                .record_event(run, &PolicyEvent::refusal(1, "write", "secrets/k"))
                .unwrap();
            store
                .put_pending(run, 1, "write", "src/a.rs", None)
                .unwrap();
            let child = store.start_child_run("sub", "ws", run, 1).unwrap();
            store
                .record_agent_event(&AgentEvent::spawn(run, 1, child, "sub"))
                .unwrap();
            store
                .record_sandbox_event(&SandboxEvent::create(run, 1, "proc"))
                .unwrap();
            store
                .record_spawn(run, 1, child, "sub", "out.txt", "ok", None, "[]")
                .unwrap();
            store
                .record_mcp(run, &McpEvent::connected("files", "stdio"))
                .unwrap();
            store.conn.execute("DROP TABLE memory", []).unwrap();
            store
                .conn
                .query_row("PRAGMA user_version", [], |r| r.get(0))
                .unwrap()
        };

        // Reopening under 0.10.0 adds `memory` and touches nothing else.
        let store = Store::open(&path).unwrap();
        let after_format: i64 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            after_format, before_format,
            "the checkpoint format must not move — a 0.9.1 checkpoint still resumes"
        );
        assert_eq!(after_format, CHECKPOINT_FORMAT);
        // The 0.7.0 durability promise: the pre-existing run still resumes.
        assert!(store.check_resumable(1).is_ok());

        // Every pre-existing table is intact, with its rows.
        assert_eq!(store.steps(1).unwrap().len(), 1);
        assert_eq!(store.last_step(1).unwrap(), 1);
        assert_eq!(store.events(1).unwrap().len(), 1);
        assert_eq!(store.pending(1).unwrap().unwrap().act, "write");
        assert_eq!(store.checkpoint_events(1).unwrap().len(), 1);
        assert_eq!(store.agent_events(1).unwrap().len(), 1);
        assert_eq!(store.sandbox_events(1).unwrap().len(), 1);
        assert_eq!(store.mcp_events(1).unwrap().len(), 1);
        assert_eq!(store.children(1).unwrap(), vec![2]);
        assert!(store.find_spawn(1, 1, "sub").is_ok());
        assert_eq!(store.run_status(1).unwrap(), Some(RunStatus::Running));
        // And the new table is there and usable.
        assert!(store.memory_list("ws").unwrap().is_empty());
        store.memory_put("ws", "k", "v", 1, 1).unwrap();
        assert_eq!(store.memory_get("ws", "k").unwrap().unwrap().value, "v");
    }

    #[test]
    fn a_layered_policy_reads_back_exactly_as_it_was_recorded() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        let policy = Policy::default()
            .layer("task")
            .deny_write("vendor/**")
            .rule(
                crate::policy::Act::Exec,
                crate::policy::Effect::Allow,
                "cargo",
            );

        store.record_run_policy(run, &policy).unwrap();

        // Equal, not merely similar: the layers, their order, and the defaults
        // are the boundary, so a lossy round trip is a wrong boundary.
        assert_eq!(store.run_policy(run).unwrap(), Some(policy));
    }

    #[test]
    fn a_permissive_policy_reads_back_permissive() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store.record_run_policy(run, &Policy::permissive()).unwrap();

        let back = store.run_policy(run).unwrap().expect("a row was recorded");
        assert!(back.is_permissive());
    }

    #[test]
    fn a_run_with_no_recorded_policy_reads_back_none_not_permissive() {
        let store = Store::memory().unwrap();
        let unrecorded = store.start_run("goal", "root").unwrap();
        let permissive = store.start_run("goal", "root").unwrap();
        store
            .record_run_policy(permissive, &Policy::permissive())
            .unwrap();

        // The distinction the table exists for: a 0.12.0 run wrote no row, and
        // "nobody recorded a policy" must never be read as "the caller chose to
        // enforce nothing".
        assert_eq!(store.run_policy(unrecorded).unwrap(), None);
        assert!(store.run_policy(permissive).unwrap().is_some());
    }

    #[test]
    fn re_recording_a_policy_for_the_same_run_replaces_it() {
        let store = Store::memory().unwrap();
        let run = store.start_run("goal", "root").unwrap();
        store.record_run_policy(run, &Policy::permissive()).unwrap();
        store.record_run_policy(run, &Policy::default()).unwrap();

        assert_eq!(store.run_policy(run).unwrap(), Some(Policy::default()));
        let rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM run_policies WHERE run_id = ?1",
                [run],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(rows, 1);
    }
}
