//! Run state in rusqlite: the full trace of a run — prompts, decisions, tool
//! calls, token usage, and outcome — readable back afterwards for audit, and
//! enough to resume an interrupted run under the same run id.
//!
//! The 0.2.0 schema adds `prompt`, `tool_call`, and `tokens` columns to `steps`.
//! An existing 0.1.0 database is migrated in place with `ALTER TABLE ADD COLUMN`
//! (additive — a 0.1.0 binary still reads a migrated database).

use rusqlite::{Connection, OptionalExtension};

use crate::context::{ObsKind, Observation, Origin};
use crate::error::{Error, Result};
use crate::policy::Policy;
use crate::pricing::{PriceTable, Spend};
use crate::provider::{ToolCall, Usage};
use crate::web::{Citation, ServerToolCall};

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

/// An observation's origin as it is stored (0.77.0), through serde for the same
/// reason [`kind_wire`] is: the mapping stays total and the pair cannot drift.
fn origin_wire(origin: Origin) -> String {
    match serde_json::to_value(origin) {
        Ok(serde_json::Value::String(s)) => s,
        other => format!("{other:?}"),
    }
}

/// The inverse of [`origin_wire`], and **deliberately not the inverse of
/// [`kind_from_wire`]'s strictness** — the two columns fail in opposite
/// directions on purpose.
///
/// A kind that does not parse is a refusal, because a ledger that came back
/// silently shorter than it was would assemble a context nobody can account for.
/// An origin that does not parse must not refuse, because `origin` is additive
/// and `CHECKPOINT_FORMAT` was deliberately not bumped for it: a 0.78.0 store
/// carrying an origin this binary has never heard of is a store this binary is
/// still allowed to open, and refusing it would re-introduce exactly the
/// incompatibility the column was chosen over an [`ObsKind`] variant to avoid.
///
/// So the two absent cases are answered differently, and the difference is the
/// point:
///
/// * `NULL` — written before 0.77.0, when no origin existed — is
///   [`Origin::Unmarked`]. That is the honest answer and it renders exactly as
///   the row always rendered: [`Piece::Result`](crate::context::Piece), which is
///   what the old `(kind, target)` derivation produced.
/// * A **non-null value this binary does not know** is [`Origin::Tool`], the
///   unattributed *external* origin — not `Unmarked`. Some newer binary wrote a
///   word it thought mattered, and the safe reading of an origin we cannot
///   interpret is the conservative one: external content gets framed as
///   untrusted, and the failure mode of guessing wrong is a frame around
///   something that did not need one rather than a missing frame around
///   something that did.
fn origin_from_wire(origin: Option<&str>) -> Origin {
    let Some(origin) = origin else {
        return Origin::Unmarked;
    };
    serde_json::from_value(serde_json::Value::String(origin.to_string()))
        .unwrap_or(Origin::Tool)
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

/// The address of the agent at the top of a tree (0.60.0).
///
/// Every other agent in a tree is addressed by the instance name its parent gave
/// it at spawn time, recorded in [`SpawnRow::as_name`]. The root has no spawn row
/// to carry one, and it is the one agent every child can be sure exists — so it
/// has a reserved word instead, and no spawn may take it.
///
/// Named rather than inlined because three places compare against it — the spawn
/// that refuses it, the resolution that answers it, and the listing a refusal
/// prints — and a literal repeated three times is a literal that will disagree
/// with itself.
///
/// ```
/// use io_harness::{Store, ROOT_ADDRESS};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let root = store.start_run("coordinate the fan-out", "/repo")?;
///
/// // A tree with no children is still addressable, and the root is the address
/// // in it. Nothing has to be spawned for that to be true.
/// assert_eq!(store.tree_addresses(root)?, vec![(ROOT_ADDRESS.to_string(), root)]);
/// # Ok(())
/// # }
/// ```
pub const ROOT_ADDRESS: &str = "root";

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

/// The durable lifecycle status of a run, so a caller can tell one paused for a
/// human (`Paused`) from one finished (`Completed`) or still going (`Running`).
/// It does not separate a live `Running` run from a crashed one — that is
/// [`Store::run_lease`](crate::Store::run_lease)'s question, and its answer is
/// what says whether the run can be taken over. OS- and rusqlite-free, so it is
/// safe in the public API.
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
/// // The triage a supervisor does on startup: the `Running` rows are the
/// // candidates, and `run_lease` is what says which of them nobody is driving.
/// // `Paused` is waiting on a human and resumes with their decision, not without it.
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
    /// `Running` run is the resume target only once its lease has lapsed;
    /// resuming one another process still holds is refused with
    /// [`Error::Conflict`](crate::Error::Conflict).
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
/// store.record_spawn(parent, 4, child, "summarise src/", "NOTES.md", "#", Some(8), "[]", "scout")?;
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
    /// (0.60.0) The child's address inside the tree — the instance name the
    /// parent gave it, or the one derived for it. Empty for every row written
    /// before 0.60.0, which is what a store read back by this release looks like
    /// and not an error.
    pub as_name: String,
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
    /// This handle's opaque lease owner id (0.62.0). Per **handle**, not per
    /// process: two `Store`s opened over one file are two drivers as far as a run
    /// lease is concerned, which is what lets the conflict tests be written with
    /// two handles in one process instead of two processes over one SQLite file —
    /// the shape that has failed `release.yml` itself here with `DatabaseBusy`.
    owner: String,
    /// The run leases this handle currently holds, as run id to generation
    /// (0.62.0).
    ///
    /// This is what lets [`Self::checkpoint_step`] enforce the lease **without
    /// changing its signature or the signature of anything that calls it**. The
    /// alternative was threading a generation from six run-loop entry points down
    /// through the loop to `commit_step`, which is a large diff whose only failure
    /// mode is a path that forgot to pass it — and a check that can be forgotten at
    /// a call site is the shape of defect this release exists to close.
    ///
    /// A handle that holds no lease for a run commits exactly as it did before,
    /// which is what keeps every existing test and every direct caller of
    /// `checkpoint_step` unchanged.
    leases: std::cell::RefCell<std::collections::HashMap<i64, i64>>,
    /// The assistant turn this handle is about to commit, per run (0.64.0).
    ///
    /// Set by the run loop immediately before the step commits and consumed
    /// *inside* [`Self::checkpoint_step`]'s transaction, for the same two reasons
    /// the lease generation above is checked there and not threaded:
    ///
    /// - **`checkpoint_step` takes a [`StepRecord`], and the turn is deliberately
    ///   not part of that type.** `StepRecord` has six public fields and no
    ///   `#[non_exhaustive]`, so a seventh is a compile break for anyone building
    ///   one with a struct literal; and adding a parameter breaks every direct
    ///   caller of a public method.
    /// - **A turn written outside that transaction is a turn a driver that lost
    ///   its lease can still write.** It would then replace the winner's turn for
    ///   the same step, and a resume would compose an assistant turn the run never
    ///   took — the one-driver-per-run guarantee 0.62.0 bought, given back at the
    ///   only table that quotes the model.
    ///
    /// A step that does not commit leaves no turn behind, because the staged value
    /// is written by the same transaction that writes the `steps` row or by
    /// nothing at all.
    turn: std::cell::RefCell<std::collections::HashMap<i64, AssistantTurn>>,
    /// Where the step now being driven has spent its wall clock, per run
    /// (0.75.0).
    ///
    /// The third use of the staging shape the two fields above describe, and it
    /// is here for their reasons rather than a third one: [`StepRecord`] has
    /// public fields and no `#[non_exhaustive]`, so a seventh is a compile break
    /// for anyone building one with a literal, and `checkpoint_step` is `pub`, so
    /// it cannot grow a parameter either.
    ///
    /// It also buys something the other two did not need. The phases are measured
    /// in three different modules — the run loop brackets the provider call and
    /// the dispatch, and `run/dispatch.rs` brackets the policy gate inside one —
    /// and a cell on the handle every one of them already holds is what lets the
    /// gate's number reach the commit without threading an out-parameter through
    /// a dispatch that takes twenty-eight arguments already.
    ///
    /// Opened by the loop at the top of a step and consumed *inside*
    /// [`Self::checkpoint_step`]'s transaction, after the lease check, so a step
    /// that never commits and a driver that lost its run both leave no
    /// attribution behind. A phase measured while no step is open — a sub-agent
    /// tree's dispatch, or a direct caller's own gate — is dropped rather than
    /// attributed to a step that did not spend it.
    attribution: std::cell::RefCell<std::collections::HashMap<i64, StagedAttribution>>,
}

/// The attribution of the step a handle is driving, while it is still being
/// measured (0.75.0).
///
/// Separate from [`StepAttribution`], the shape a reader gets back, over one
/// field: a span that is not measured yet. A staged row with no span is a step
/// still running, and it is what stops a commit from writing an attribution for
/// a step whose phases were never closed off — where a `0` span would be
/// indistinguishable from a step that really did finish in under a millisecond.
#[derive(Debug, Default)]
pub(crate) struct StagedAttribution {
    /// The step these numbers belong to. A commit of any other step ignores them,
    /// the way a staged turn is matched on its own step number.
    pub(crate) step: u32,
    pub(crate) span_ms: Option<u64>,
    pub(crate) provider_ms: Option<u64>,
    pub(crate) tool_ms: Option<u64>,
    pub(crate) gate_ms: Option<u64>,
    pub(crate) store_ms: Option<u64>,
}

/// Which phase of a step one clock reading belongs to (0.75.0).
///
/// An enum rather than a field accessor per phase, so the four staging methods
/// on [`Store`] share one body and cannot come to disagree about what
/// accumulating a reading means.
#[derive(Debug, Clone, Copy)]
pub(crate) enum StepPhase {
    Provider,
    Tool,
    Gate,
    Store,
}

/// Whether the lease row in hand has lapsed, as SQL (0.62.0).
///
/// Whole seconds, compared as integers. This file's other date arithmetic is
/// `julianday` in floating point (`:5890`, `:6691`), and that is right for
/// *reporting* an elapsed duration and wrong here: this expression decides a
/// takeover at its boundary, and a boundary is the one place a rounding is a
/// defect. `>=` and not `>`, so a lease is lapsed the instant its ttl is up rather
/// than a second after — the boundary pair that pins it is written as a test,
/// because a criterion that claims a boundary and never lands on it is prose.
///
/// One definition, named once, because the acquire's `WHERE`, the read-back and
/// every test share it: two spellings of "expired" is how the write and the read
/// come to disagree about who holds a run.
const LEASE_EXPIRED: &str = "CAST(strftime('%s','now') AS INTEGER) \
                             - CAST(strftime('%s', run_leases.renewed_at) AS INTEGER) \
                             >= run_leases.ttl_secs";

/// When the lease row in hand lapses, as SQL: `renewed_at + ttl_secs`, in the same
/// ISO-8601 shape every other timestamp in this store is written in.
///
/// Computed by the database and never by the caller, for the reason [`LEASE_EXPIRED`]
/// is: a conflict's `expires_at` has to be a moment on the clock the acquire will
/// use, not on the clock of whoever is reading the error.
const LEASE_EXPIRES_AT: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', run_leases.renewed_at, \
                                '+' || run_leases.ttl_secs || ' seconds')";

/// The [`Error::Conflict`] a refused lease write reports, from the row that
/// refused it.
///
/// A `None` row is not reachable through either caller — the acquire's `WHERE`
/// declines only against a row that exists, and a refused renew has just failed to
/// match one that does. It is still reported rather than unwrapped: a lease that
/// vanished between the write and the read is a conflict the caller retries, and a
/// durable runtime does not panic over a race it was written to survive.
fn conflict_from(run_id: i64, held: Option<LeaseRow>) -> Error {
    held.map_or_else(
        || Error::Conflict {
            run_id,
            owner: String::new(),
            expires_at: String::new(),
        },
        |row| Error::Conflict {
            run_id,
            owner: row.owner,
            expires_at: row.expires_at,
        },
    )
}

/// Whether the process that took a lease is still running (0.62.0).
///
/// **This is what keeps `kill -9` and resume working.** Waiting out a ttl before
/// a killed run could be picked up would have made this crate's oldest promise —
/// a run survives the death of the process driving it and resumes at once —
/// wait half an hour, which is trading a silent corruption for an outage. So a
/// lease is takeable the moment its owner is gone, and the ttl is the fallback
/// for the case this cannot answer rather than the primary rule.
///
/// **It errs towards "alive", and that direction is the safe one.** A `true` for
/// a process that has in fact died costs a wait until the ttl lapses. A `false`
/// for a process that is alive would hand its run to a second driver — and even
/// that is bounded, because the first driver's next commit is then refused inside
/// the transaction rather than interleaving. Unknown answers therefore return
/// `true`: an owner id from an older release with no pid to read, a pid this
/// platform cannot ask about, and a Windows process that exited with code 259,
/// which is indistinguishable from one still running because 259 is
/// `STILL_ACTIVE`. Answered on unix with `kill(pid, 0)` and on Windows with
/// `OpenProcess` + `GetExitCodeProcess`, neither of which costs a dependency this
/// crate did not already have.
///
/// **The pid may have been recycled**, which reads as "alive" and costs the wait.
/// The dangerous direction — a live owner reported dead — requires the pid to be
/// genuinely absent, which a live process's pid cannot be. Cross-host reuse is out
/// of scope by the same bound the guide states: the owner id is process-scoped and
/// the lease is a row in a SQLite file, so two hosts sharing one file over a
/// network filesystem is outside what the release claims.
fn owner_is_alive(owner: &str) -> bool {
    let Some(pid) = owner.split('-').next().and_then(|p| p.parse::<i32>().ok()) else {
        return true;
    };
    #[cfg(unix)]
    {
        // Signal 0 performs the permission and existence checks and delivers
        // nothing. `EPERM` means the process is there and is somebody else's,
        // which is still there.
        if unsafe { libc::kill(pid, 0) } == 0 {
            return true;
        }
        std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        // `OpenProcess` + `GetExitCodeProcess`, on the features this crate already
        // enables — 0.24.0's Job Object backend brought `Win32_System_Threading`,
        // so this costs no new dependency here either.
        //
        // The error direction is preserved deliberately. A handle that cannot be
        // opened for lack of rights means the process is **there** and somebody
        // else's, which is alive; only `ERROR_INVALID_PARAMETER` means no such
        // process. And a process that exits with code 259 is indistinguishable from
        // one still running, because 259 *is* `STILL_ACTIVE` — which errs alive, as
        // everything unknown here does.
        use windows_sys::Win32::Foundation::{
            CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, STILL_ACTIVE,
        };
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid as u32);
            if handle.is_null() {
                return GetLastError() != ERROR_INVALID_PARAMETER;
            }
            let mut code: u32 = 0;
            let read = GetExitCodeProcess(handle, &mut code);
            CloseHandle(handle);
            read == 0 || code == STILL_ACTIVE as u32
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        true
    }
}

/// A per-process counter mixed into every owner id (0.62.0).
///
/// The wall clock alone is not enough. Two `Store::memory()` handles opened in a
/// tight loop can read the same `SystemTime` on a coarse clock — Windows is the
/// platform where that is ordinary rather than exotic — and two handles sharing an
/// owner id do not merely collide: they look like *the same driver*, so the test
/// that proves a second driver is refused would pass by taking over its own lease.
/// A counter costs nothing and closes the class.
static OWNER_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A fresh opaque owner id for one [`Store`] handle.
///
/// Deliberately built from `std::process::id()`, the wall clock and [`OWNER_SEQ`]
/// and nothing else: no `uuid`, no `rand`, no `hostname`, because a new runtime
/// dependency is a criterion failure in this release rather than a trade-off. It
/// is opaque — nothing parses it back — and it never reaches a filesystem path, so
/// the stability rules that bind a hashed cache key do not bind here.
fn new_owner_id() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = OWNER_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}-{nanos}-{seq}", std::process::id())
}

/// A run lease held by one [`Store`] handle (0.62.0).
///
/// Holding one is what entitles a driver to commit steps against a run. It is
/// released on drop, so no exit path in the run loop had to grow a release call
/// and none of the thirty-four public entry points changed signature — an early
/// return, a `?`, a panic unwinding through the loop and an ordinary finish all
/// release it the same way.
///
/// A dropped lease is released *best-effort*: `Drop` cannot report an error, and
/// the failure it would report is one a takeover already handles. That is the
/// whole reason the lease carries a ttl rather than a flag — a driver that dies
/// without releasing anything, in the way a killed process does, must not lock its
/// run for good.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("port it", "openrouter")?;
///
/// {
///     let held = store.acquire_lease(run_id, 300)?;
///     assert_eq!(held.run_id(), run_id);
///     assert_eq!(held.generation(), 1, "the first acquire of a free run");
///     assert!(store.run_lease(run_id)?.is_some(), "held while it is in scope");
/// } // released here, without a call on any exit path.
///
/// assert!(store.run_lease(run_id)?.is_none(), "a released lease leaves no row");
/// # Ok(())
/// # }
/// ```
pub struct Lease<'a> {
    store: &'a Store,
    run_id: i64,
    generation: i64,
    released: std::cell::Cell<bool>,
}

/// Written by hand rather than derived because [`Store`] holds a `Connection` and
/// is not `Debug`. What a reader of a lease wants is which run at which
/// generation, and the store it came from adds nothing.
impl std::fmt::Debug for Lease<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lease")
            .field("run_id", &self.run_id)
            .field("generation", &self.generation)
            .field("owner", &self.store.owner)
            .field("released", &self.released.get())
            .finish()
    }
}

impl Lease<'_> {
    /// The run this lease is over.
    pub fn run_id(&self) -> i64 {
        self.run_id
    }

    /// The generation this acquire won. It rises by exactly one each time the run
    /// is taken over from a different owner, and a step committed under an earlier
    /// generation is refused.
    pub fn generation(&self) -> i64 {
        self.generation
    }

    /// Extend the lease, keeping the generation. Called by the run loop as part of
    /// each durable step commit, which is what bounds a healthy run's staleness at
    /// one step rather than at a timer's period.
    ///
    /// Refused with [`Error::Conflict`] once another owner has taken the run over:
    /// renewing is not a way back in, taking over is.
    pub fn renew(&self) -> Result<()> {
        self.store.renew_lease(self.run_id, self.generation)
    }

    /// Release the lease now rather than at drop, and report a failure to do so.
    ///
    /// The run becomes immediately acquirable by anybody, which is the difference
    /// between a released lease and an expired one: a released run has no row, an
    /// expired one has a row somebody may take over.
    pub fn release(self) -> Result<()> {
        self.released.set(true);
        self.store.release_lease(self.run_id, self.generation)
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if !self.released.get() {
            let _ = self.store.release_lease(self.run_id, self.generation);
        }
    }
}

/// Who holds a run right now, as [`Store::run_lease`] reads it back (0.62.0).
///
/// An operator can ask this instead of inferring liveness from `runs.status`,
/// which has never distinguished a live process from a crashed one and still does
/// not.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("port it", "openrouter")?;
///
/// // A ttl of zero is a lease that is lapsed as soon as it is written — which is
/// // how a test, or an operator inspecting a crashed run, sees the expired state
/// // without waiting for a clock.
/// let _held = store.acquire_lease(run_id, 0)?;
/// let row = store.run_lease(run_id)?.expect("the run is held");
///
/// assert_eq!(row.run_id, run_id);
/// assert_eq!(row.owner, store.owner(), "this handle is the holder");
/// assert_eq!(row.generation, 1);
/// assert_eq!(row.ttl_secs, 0);
/// assert!(row.expired, "and it has already lapsed, so anyone may take it over");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct LeaseRow {
    /// The run the lease is over.
    pub run_id: i64,
    /// The opaque id of the holder.
    pub owner: String,
    /// The generation this holder acquired at.
    pub generation: i64,
    /// When this owner first took the run, as an ISO-8601 UTC timestamp. A renewal
    /// does not move it; a takeover does.
    pub acquired_at: String,
    /// When the lease was last extended, as an ISO-8601 UTC timestamp.
    pub renewed_at: String,
    /// When the lease lapses — `renewed_at + ttl_secs`, computed by the database
    /// against the clock an acquire will use.
    pub expires_at: String,
    /// How long after `renewed_at` the lease lapses.
    pub ttl_secs: i64,
    /// Whether the lease has already lapsed, evaluated by the database against the
    /// same clock the acquire uses — never by the caller against its own.
    pub expired: bool,
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

/// What one step asked for, kept so a resumed run can send it back (0.64.0).
///
/// The assistant half of a transcript: the text the model wrote this step, if
/// any, and the tool calls it made, in the order it made them. The *results* half
/// has been durable since 0.13.0 in the ledger, so this is the one piece a
/// process death used to take with it — and the reason every step a resumed run
/// did not itself drive arrived at the model as third-person prose rather than as
/// its own turn.
///
/// **`text` distinguishes `None` from `Some("")`.** A model that wrote nothing
/// beside its calls is not a model that wrote an empty string, and a resumed run
/// that cannot tell them apart sends a turn the live run did not.
///
/// **This is not [`StepRecord::tool_call`].** That field is one human-readable
/// string — `name:args` joined with ` | ` — which is what a trace dump prints and
/// what the stall signature compares, and which cannot be split back apart when a
/// tool name contains `:` or an argument contains ` | `. This type is the
/// structured record, and the two are kept separate rather than one being made to
/// serve both.
///
/// ```
/// use io_harness::{AssistantTurn, Store, ToolCall};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("write the file", "gpt-5")?;
///
/// store.record_step_turn(
///     run_id,
///     &AssistantTurn::new(
///         1,
///         Some("I will write it now."),
///         vec![ToolCall {
///             name: "write_file".into(),
///             arguments: serde_json::json!({ "path": "NOTES.md" }),
///         }],
///     ),
/// )?;
///
/// let turns = store.step_turns(run_id)?;
/// assert_eq!(turns.len(), 1);
/// assert_eq!(turns[0].calls[0].name, "write_file");
/// assert_eq!(turns[0].text.as_deref(), Some("I will write it now."));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct AssistantTurn {
    /// The step this turn was taken on.
    pub step: u32,
    /// What the model wrote this step, or `None` if it wrote nothing.
    pub text: Option<String>,
    /// The calls it made, in the order it made them.
    pub calls: Vec<ToolCall>,
}

impl AssistantTurn {
    /// A turn at `step`, with the text the model wrote and the calls it made.
    pub fn new(step: u32, text: Option<impl Into<String>>, calls: Vec<ToolCall>) -> Self {
        Self {
            step,
            text: text.map(Into::into),
            calls,
        }
    }
}

/// Where one committed step's wall clock went (0.75.0).
///
/// Read with [`Store::step_attributions`], which answers "where did this step go"
/// for every attributed step of a run — including the provider's TTFT for the
/// step's own call, so the question costs one query rather than two joined by
/// hand.
///
/// **A phase that did not happen is absent, not zero.** A step that called no
/// provider reports `provider_ms: None`, which is a different fact from a call
/// that returned inside a millisecond — the distinction
/// [`ProviderCall::ttft_ms`] already draws, kept here for the same reason: a zero
/// averaged into a report flatters whatever produced it.
///
/// **The phases are disjoint and they do not tile the span.** `provider_ms`,
/// `tool_ms` and `store_ms` are separate stretches of the step; compaction,
/// prompt assembly and the loop's own bookkeeping are none of them, and what they
/// cost is [`Self::unattributed_ms`] — reported, rather than folded into whatever
/// phase happens to be adjacent. `gate_ms` is the one nested number: a policy
/// resolution happens *inside* a tool dispatch, so it is part of `tool_ms` the
/// way a call's TTFT is part of its latency, and [`Self::attributed_ms`] does not
/// count it twice.
///
/// **`store_ms` is the commit that ended the previous step, not this one's.** A
/// row cannot time the write that creates it, and writing the number afterwards
/// would put it outside the transaction whose lease check is what stops a driver
/// that lost the run from writing at all. So a step's span runs from the moment
/// the previous step's commit began to the moment this step's own commit begins,
/// and the store phase inside it is that commit plus the ledger persist that
/// followed it. The first committed step of a run reports no store phase, because
/// the only commit it could name is its own.
///
/// Only the flat workspace loop attributes; a sub-agent tree's steps and every
/// step committed before 0.75.0 record nothing and are not returned.
///
/// `#[non_exhaustive]`: a later release may attribute a phase this one does not
/// separate — compaction and prompt assembly are the two candidates already in
/// the remainder — and adding a field to a struct callers construct is the break
/// [`StepRecord`] cannot pay. [`Self::new`] and the `with_*` builders are the
/// door outside the crate, because the attribute forbids every struct expression
/// there, `..Default::default()` included.
///
/// ```
/// # fn main() -> io_harness::Result<()> {
/// let store = io_harness::Store::memory()?;
/// let run_id = store.start_run("write the notes", "gpt-5")?;
///
/// // Written by the run loop, in the transaction that commits each step. A run
/// // that has taken no step has nothing to report yet.
/// assert!(store.step_attributions(run_id)?.is_empty());
///
/// for a in store.step_attributions(run_id)? {
///     println!(
///         "step {}: {}ms of {}ms attributed, {}ms elsewhere, first token {:?}",
///         a.step,
///         a.attributed_ms(),
///         a.span_ms,
///         a.unattributed_ms(),
///         a.ttft_ms,
///     );
/// }
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StepAttribution {
    /// The step these numbers belong to.
    pub step: u32,
    /// The step's own measured span, in milliseconds. Every phase below is a part
    /// of it, and no sum of them may exceed it.
    pub span_ms: u64,
    /// Waiting on the provider, across every attempt the step made — the retries
    /// and the backoff between them included, because a step that retried twice
    /// spent that time on the provider and nowhere else.
    pub provider_ms: Option<u64>,
    /// Executing tool calls, summed over the calls the step dispatched.
    pub tool_ms: Option<u64>,
    /// Resolving the policy for those calls, *of which* — a part of `tool_ms`,
    /// and the part that includes waiting for a human when a call is one an
    /// approver must answer.
    pub gate_ms: Option<u64>,
    /// The durable write that ended the previous step: its commit transaction and
    /// the ledger persist after it. `None` on a run's first committed step.
    pub store_ms: Option<u64>,
    /// The provider's time to first token on this step's last attempt, from
    /// [`ProviderCall::ttft_ms`]. `None` when the step made no call and when the
    /// call measured nothing — a provider that streamed no tokens reports no
    /// TTFT rather than zero.
    pub ttft_ms: Option<u64>,
}

impl StepAttribution {
    /// A step's attribution over a measured span, with no phase attributed yet.
    pub fn new(step: u32, span_ms: u64) -> Self {
        Self {
            step,
            span_ms,
            provider_ms: None,
            tool_ms: None,
            gate_ms: None,
            store_ms: None,
            ttft_ms: None,
        }
    }

    /// Set the provider phase; `None` leaves it absent.
    pub fn with_provider_ms(mut self, ms: Option<u64>) -> Self {
        self.provider_ms = ms;
        self
    }

    /// Set the tool-execution phase; `None` leaves it absent.
    pub fn with_tool_ms(mut self, ms: Option<u64>) -> Self {
        self.tool_ms = ms;
        self
    }

    /// Set the policy-gate phase, which is part of the tool phase and not beside
    /// it; `None` leaves it absent.
    pub fn with_gate_ms(mut self, ms: Option<u64>) -> Self {
        self.gate_ms = ms;
        self
    }

    /// Set the store phase; `None` leaves it absent.
    pub fn with_store_ms(mut self, ms: Option<u64>) -> Self {
        self.store_ms = ms;
        self
    }

    /// Set the step's time to first token; `None` leaves it absent.
    pub fn with_ttft_ms(mut self, ms: Option<u64>) -> Self {
        self.ttft_ms = ms;
        self
    }

    /// The span this step accounted for: the provider, the tools and the store.
    ///
    /// `gate_ms` is not added — it is measured inside the dispatch `tool_ms`
    /// already covers, and adding it would report a step spending longer than it
    /// lasted.
    pub fn attributed_ms(&self) -> u64 {
        self.provider_ms.unwrap_or(0) + self.tool_ms.unwrap_or(0) + self.store_ms.unwrap_or(0)
    }

    /// The span this step did not account for: compaction, prompt assembly, the
    /// loop's per-step bookkeeping, and whatever the host was doing instead.
    ///
    /// Saturating, so a remainder is never a wrapped enormous number: the parts
    /// are measured with separate clock readings and rounded down to whole
    /// milliseconds each, and a step whose phases round up to its span is a fast
    /// step, not a corrupt row.
    pub fn unattributed_ms(&self) -> u64 {
        self.span_ms.saturating_sub(self.attributed_ms())
    }
}

/// One attempt at a call the harness cannot inspect (0.65.0).
///
/// Written before the call and closed after it, on its own rather than at the
/// step boundary — so a process that died between the two leaves this row behind
/// and a resumed run can find out that something was started and never finished.
///
/// `#[non_exhaustive]`: a later release may need to carry the arguments, an
/// idempotency key or the owner that wrote it, and adding a field to a struct
/// callers construct is the break 0.64.0 could not pay.
///
/// ```
/// use io_harness::{Store, ToolRecovery};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("charge the customer", "root")?;
///
/// // The run loop writes this before it makes the call. Shown here directly,
/// // because what an operator needs is the read below.
/// let id = store.open_attempt(run, 3, "charge", ToolRecovery::Indeterminate)?;
///
/// let open = store.open_attempts(run)?;
/// assert_eq!(open.len(), 1);
/// assert_eq!(open[0].tool, "charge");
/// assert_eq!(Some(open[0].id), id);
///
/// // Closed once the call returned, and then there is nothing to decide.
/// store.close_attempt(open[0].id)?;
/// assert!(store.open_attempts(run)?.is_empty());
/// # Ok(()) }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ToolAttempt {
    /// The row's own id, and what a decision names.
    pub id: i64,
    /// The run the call belongs to.
    pub run_id: i64,
    /// The step the call was made on.
    pub step: u32,
    /// The tool that was called.
    pub tool: String,
    /// When it was started, as `%Y-%m-%dT%H:%M:%fZ`.
    pub started_at: String,
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

/// One message from one agent in a tree to another (0.60.0).
///
/// The first horizontal edge in this schema. A tree could already nest, share one
/// ledger, queue past its concurrency cap and hand a child's report up to its
/// parent — every one of those a *vertical* edge. Two children investigating two
/// subsystems had no way to tell each other what they found: the only channel
/// between them was a file one wrote and the other happened to read, which is
/// unaddressed, unordered, invisible to the trace and indistinguishable from
/// ordinary workspace churn.
///
/// A message is addressed to a run id, because a run id is the only per-instance
/// identity an agent has. [`AgentDef::name`](crate::AgentDef::name) is a *role*,
/// and two children spawned from one definition — the ordinary shape of a
/// fan-out — are both called by it. [`from_name`](Self::from_name) is the sender's
/// own instance name, which is what a reader renders and what a `from` filter
/// matches.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let scout = store.start_run("locate the symbol", "openrouter")?;
/// let author = store.start_run("make the edit", "openrouter")?;
///
/// store.send_message(scout, author, "scout", 3, "it is at src/auth.rs:210")?;
///
/// // The author drains its inbox. Reading marks, so the same message is not
/// // delivered twice — including to a process that resumed this tree.
/// let inbox = store.read_messages(author, None)?;
/// assert_eq!(inbox.len(), 1);
/// assert_eq!(inbox[0].from_name, "scout");
/// assert_eq!(inbox[0].body, "it is at src/auth.rs:210");
/// assert!(store.read_messages(author, None)?.is_empty(), "delivered exactly once");
///
/// // An auditor reads without consuming.
/// assert_eq!(store.messages_for(author)?.len(), 1);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct AgentMessage {
    /// The row id. Also the delivery order: messages are read oldest first, and
    /// `AUTOINCREMENT` makes this monotonic across the whole store rather than
    /// merely unique, so one number orders a tree whose agents interleave.
    pub id: i64,
    /// The run that sent it.
    pub from_run_id: i64,
    /// The run it was addressed to.
    pub to_run_id: i64,
    /// The sender's instance name — its address inside the tree, not the name of
    /// the roster definition it was spawned from.
    pub from_name: String,
    /// The sender's step when it sent this.
    pub step: u32,
    /// What was said. Text, and deliberately nothing more: a reply-to id or a
    /// request/response framing is the embedder's to choose.
    pub body: String,
    /// When it was sent.
    pub sent_at: String,
    /// When it was delivered, or `None` while it is still waiting. This column is
    /// what makes delivery exactly-once across a process boundary — a resumed tree
    /// reads it rather than an in-memory set that did not survive the restart.
    pub read_at: Option<String>,
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
    /// `"spawn"`, `"spawn_refused"`, `"budget_draw"`, `"said"`, or
    /// `"spawn_args"`.
    pub kind: String,
    /// The spawned child's run id, for a `"spawn"`.
    pub child_run_id: Option<i64>,
    /// Free-form detail: the child's goal for a spawn, the breached cap for a
    /// refusal, what the agent said for a `"said"`.
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

    /// One agent sent another a message (0.60.0).
    ///
    /// `child_run_id` is the RECIPIENT, which is a widening of that field's
    /// meaning and a deliberate one: it has always held "the other run this event
    /// is about", and a mailbox event whose other run lived in a free-form string
    /// would be unqueryable. The recipient of a message is not always a child —
    /// a child answering its parent sends upward, and two siblings send sideways.
    ///
    /// `detail` is the recipient's address and the message's length, never the
    /// body. A trace answering "who told whom, and when" is an audit; a trace
    /// holding every word an agent said to another is a second copy of the
    /// mailbox that no retention call would know to delete.
    pub fn message_sent(run_id: i64, step: u32, to_run_id: i64, to: &str, chars: usize) -> Self {
        Self {
            run_id,
            step,
            kind: "message_sent".into(),
            child_run_id: Some(to_run_id),
            detail: Some(format!("to {to}, {chars} chars")),
            tokens: None,
            remaining: None,
        }
    }

    /// An agent read its mailbox, and how many messages it was given (0.60.0).
    ///
    /// Recorded even when the answer is none, because "I looked and there was
    /// nothing" is the fact that explains a step that did nothing else.
    pub fn message_read(run_id: i64, step: u32, delivered: usize, from: Option<&str>) -> Self {
        Self {
            run_id,
            step,
            kind: "message_read".into(),
            child_run_id: None,
            detail: Some(match from {
                Some(f) => format!("{delivered} from {f}"),
                None => format!("{delivered} delivered"),
            }),
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

    /// The arguments of a spawn, kept so a detached child can be re-adopted
    /// after a restart (0.50.0).
    ///
    /// A blocking child never needs it: its parent's step is left uncommitted, so
    /// the resume replays the spawn call and the arguments come with it. A child
    /// the parent stopped waiting for commits its step and the call is gone —
    /// while `spawns` holds only five of the nine arguments, so rebuilding from
    /// that row would silently drop `agent` and `deny_net` and resume the child
    /// under a wider policy than it was spawned with.
    pub fn spawn_args(
        run_id: i64,
        step: u32,
        child_run_id: i64,
        arguments: &serde_json::Value,
    ) -> Self {
        Self {
            run_id,
            step,
            kind: "spawn_args".into(),
            child_run_id: Some(child_run_id),
            detail: Some(arguments.to_string()),
            tokens: None,
            remaining: None,
        }
    }

    /// What an agent said on this step, beside whatever it called (0.50.0).
    ///
    /// An agent's own words were durable nowhere before this: `steps.result`
    /// holds the observations a step produced, and a completion's prose reached
    /// the ledger only in the one case where it carried no tool call at all
    /// (`(no tool call) …`). So an agent that wrote a file and explained why left
    /// the explanation in memory and nothing else.
    ///
    /// The last of these rows for a run is what a parent composes as its child's
    /// conclusion, and recording it per step rather than once at the end is what
    /// makes that readable after the process that ran the child has exited —
    /// including for a child a *later* process adopts. One row per step that said
    /// something, alongside the `"budget_draw"` row every step already writes.
    pub fn said(run_id: i64, step: u32, text: impl Into<String>) -> Self {
        Self {
            run_id,
            step,
            kind: "said".into(),
            child_run_id: None,
            detail: Some(text.into()),
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
    /// `"create"` (whose `detail` names the [`ExecMode`](crate::ExecMode) that
    /// call resolved to, since 0.48.0), `"exec"`, `"cap_hit"`, `"destroy"`,
    /// `"dial"` (0.48.0, whose `detail` is the `host:port` a contained command
    /// asked for), `"gate_phase_failed"` (whose `detail` names the phase), or
    /// `"gate_output"` (whose `detail` is what a failing gate command printed).
    ///
    /// **0.48.0 made an old sentence here false, and this is the corrected one.**
    /// A `"net_deny"` kind was documented from 0.6.0 to 0.11.0 and never existed;
    /// it was removed in 0.12.0 rather than implemented, on the reasoning that a
    /// sandbox denies egress *structurally* — the backend gives the child no route
    /// out, so there is no attempt to observe and nothing to count. That was true
    /// of every release up to 0.47.0. Since 0.48.0 a run whose policy names hosts
    /// routes its contained commands through a proxy it owns, so there **is** an
    /// attempt and it **is** counted: one `"dial"` row per outbound connection,
    /// permitted or refused, beside the decision itself in `policy_events` with
    /// `act = "net"` — which is still where the harness's own network decisions
    /// live, and now where its commands' decisions live too.
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
    /// Whether this is something somebody decided or something a run observed
    /// (0.30.0). An entry written before the column existed reads as
    /// [`MemoryKind::Fact`], which is what it was.
    pub kind: MemoryKind,
    /// Whether a run may overwrite it (0.30.0).
    ///
    /// Set by a caller and never by a run: this is how a human makes a correction
    /// stick when the agent keeps re-learning something wrong.
    pub pinned: bool,
}

/// What kind of thing a [`MemoryEntry`] is (0.30.0).
///
/// The distinction a flat list of strings cannot make: a decision somebody took
/// and a run must not quietly reverse, versus a fact a run observed and a later
/// run may correct. Nothing in the run loop treats the two differently — the
/// crate stores what it was told and reports it — because the difference is one a
/// person makes, and a harness inferring it would be guessing at intent.
///
/// `#[non_exhaustive]` from the line it exists, so the third kind the contract
/// leaves open (an observation, distinct from an asserted fact) arrives later
/// without a break.
///
/// ```
/// use io_harness::{MemoryKind, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("port the parser", "/repo")?;
///
/// store.memory_write("/repo", "parser", "stays in-crate", run, 4, MemoryKind::Decision)?;
/// let entry = store.memory_get("/repo", "parser")?.expect("written above");
/// assert_eq!(entry.kind, MemoryKind::Decision);
///
/// // The default for everything the agent writes itself, and for every entry
/// // written before this release: what a run observed is a fact, not a ruling.
/// store.memory_put("/repo", "test-command", "cargo test", run, 5)?;
/// assert_eq!(store.memory_get("/repo", "test-command")?.unwrap().kind, MemoryKind::Fact);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum MemoryKind {
    /// Something a run observed. The default, and what every pre-0.30.0 entry is.
    #[default]
    Fact,
    /// Something somebody decided. Carries no enforcement of its own; what stops
    /// a run overwriting one is [`MemoryEntry::pinned`].
    Decision,
}

impl MemoryKind {
    /// Its stored spelling, which is also what a consumer renders.
    fn as_str(self) -> &'static str {
        match self {
            MemoryKind::Fact => "fact",
            MemoryKind::Decision => "decision",
        }
    }

    /// Read a stored spelling back. An unknown one — or `NULL`, which is every
    /// entry written before 0.30.0 — is [`MemoryKind::Fact`]: a row this crate
    /// cannot classify is not a reason to refuse a database, and a fact is the
    /// weaker claim of the two.
    fn from_stored(text: Option<String>) -> Self {
        match text.as_deref() {
            Some("decision") => MemoryKind::Decision,
            _ => MemoryKind::Fact,
        }
    }
}

/// One group of an aggregate: what it is, and how many (0.30.0).
///
/// One row type for every grouped count this crate returns, so a caller reads
/// `key`/`count` whether it asked for outcomes, days or gate phases. What the key
/// *means* is the accessor's business and is documented there.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("goal", "/repo")?;
/// store.finish_run(run, "success")?;
///
/// let rows = store.runs_by_outcome()?;
/// assert_eq!((rows[0].key.as_str(), rows[0].count), ("success", 1));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tally {
    /// What this group is — an outcome, a day, a gate phase.
    pub key: String,
    /// How many rows fell into it.
    pub count: u64,
}

/// How often a run was verified without a gate failing first (0.30.0).
///
/// Counts, never a rate: which denominator is the right one is the consumer's
/// judgement, and returning a single number would make that choice invisibly.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("goal", "/repo")?;
/// store.finish_run(run, "success")?;
///
/// let first = store.first_try()?;
/// assert_eq!((first.runs, first.succeeded, first.first_try), (1, 1, 1));
/// // "of the ones that worked" and "of everything we tried" are both legitimate,
/// // and the crate declines to pick.
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FirstTry {
    /// Runs that finished, whatever the ending.
    pub runs: u64,
    /// Of those, the ones that succeeded.
    pub succeeded: u64,
    /// Of those, the ones with no failed gate phase recorded against them.
    pub first_try: u64,
}

/// How many times each recovery mechanism carried a run through (0.30.0).
///
/// ```
/// use io_harness::{ContextEvent, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("goal", "/repo")?;
/// store.record_context_event(run, &ContextEvent::replan(2, "no progress"))?;
///
/// let recovery = store.recovery()?;
/// assert_eq!(recovery.replans, 1);
/// assert_eq!(recovery.fallbacks, 0, "no provider fell over");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Recovery {
    /// Steps served by a fallback provider after the first one failed.
    pub fallbacks: u64,
    /// Times an agent making no progress was told once to change approach.
    pub replans: u64,
    /// Times a run was resumed from its checkpoint.
    pub resumes: u64,
}

/// One recall: a run drew on one memory entry at one step (0.30.0).
///
/// Returned by [`Store::memory_recalls`]. Per (run, key, step) rather than a flag
/// on the entry, because an entry recalled by two runs is two facts and a flag
/// would keep only the later one.
///
/// ```
/// use io_harness::{MemoryRecall, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("port the parser", "/repo")?;
/// // The context assembler writes these; a run that recalled nothing has none.
/// let recalls: Vec<MemoryRecall> = store.memory_recalls(run)?;
/// assert!(recalls.is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecall {
    /// The run that drew on the entry.
    pub run_id: i64,
    /// The step whose context it was carried into.
    pub step: u32,
    /// The workspace the entry belongs to.
    pub workspace: String,
    /// The entry's key.
    pub key: String,
    /// UTC time of the recall, from the database clock.
    pub at: String,
}

/// What a run's `forget` did (0.56.0).
///
/// Three answers rather than a `bool`, for the reason [`MemoryWrite::refused`]
/// exists: "there was nothing there" and "an operator pinned it" are different
/// facts, and a model told only that nothing was removed cannot tell which of
/// them it is looking at.
///
/// ```
/// use io_harness::{MemoryForget, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("fix the flake", "/repo")?;
/// store.memory_put("/repo", "retries", "three", run, 1)?;
///
/// // Each of the three is reachable, and they are not interchangeable.
/// assert_eq!(store.memory_forget("/repo", "retries", run, 2)?, MemoryForget::Removed);
/// assert_eq!(store.memory_forget("/repo", "retries", run, 3)?, MemoryForget::Absent);
///
/// store.memory_put("/repo", "owner", "the platform team", run, 4)?;
/// store.memory_pin("/repo", "owner", true)?;
/// assert_eq!(store.memory_forget("/repo", "owner", run, 5)?, MemoryForget::Pinned);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemoryForget {
    /// The entry was there and is gone.
    Removed,
    /// An operator pinned it, so it is not a run's to withdraw. The entry
    /// stands.
    Pinned,
    /// There was no such key in this workspace. Not an error — a run that
    /// withdraws a fact twice has still withdrawn it.
    Absent,
}

/// What a write to memory did (0.30.0).
///
/// Returned by [`Store::memory_write`]. The `refused` flag is the half that
/// matters: an agent that believes it corrected something and did not is the
/// failure the pinned flag exists to prevent, so the refusal is a value the
/// caller has to receive rather than a silence it has to notice.
///
/// ```
/// use io_harness::{MemoryKind, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("fix the flake", "/repo")?;
///
/// store.memory_write("/repo", "retries", "three", run, 1, MemoryKind::Decision)?;
/// store.memory_pin("/repo", "retries", true)?;
///
/// let wrote = store.memory_write("/repo", "retries", "one", run, 7, MemoryKind::Fact)?;
/// assert!(wrote.refused, "a pinned entry is not a run's to overwrite");
/// assert_eq!(store.memory_get("/repo", "retries")?.unwrap().value, "three");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MemoryWrite {
    /// True when the entry was pinned and the write did not happen.
    pub refused: bool,
    /// Keys dropped to hold the workspace's caps, oldest first. Empty on a
    /// refused write, which evicts nothing because it stored nothing.
    pub evicted: Vec<String>,
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

/// Read one `memory` row. One place, so the two queries that read the table
/// cannot drift in their column order — which is exactly how a nullable column
/// added late ends up read as the wrong field in one of them.
fn memory_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<MemoryEntry> {
    Ok(MemoryEntry {
        key: r.get(0)?,
        value: r.get(1)?,
        run_id: r.get(2)?,
        step: r.get::<_, i64>(3)? as u32,
        created_at: r.get(4)?,
        kind: MemoryKind::from_stored(r.get(5)?),
        // NULL for every entry written before 0.30.0, and false is what those
        // entries were: nobody had pinned anything.
        pinned: r.get::<_, Option<i64>>(6)?.unwrap_or(0) == 1,
    })
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
/// // `choices` is a `Vec<Choice>` since 0.72.0, so an offer can carry a sentence
/// // saying what it means. Read the label to compare against a bare string.
/// let labels: Vec<&str> = q.choices.iter().map(|c| c.label.as_str()).collect();
/// assert_eq!(labels, ["io.toml", "io.local.toml"]);
///
/// store.answer_question(id, "io.local.toml", "human")?;
/// let q = store.question(id)?.unwrap();
/// assert_eq!(q.answer.as_deref(), Some("io.local.toml"));
/// assert_eq!(q.answered_by.as_deref(), Some("human"));
///
/// // Answering twice does not overwrite, and says so: `false` means somebody
/// // else's answer is the one the run acted on (0.33.0).
/// assert!(!store.answer_question(id, "io.toml", "human")?);
/// assert_eq!(store.question(id)?.unwrap().answer.as_deref(), Some("io.local.toml"));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingQuestion {
    /// The question's own id, and what [`resume_with_answer`](crate::resume_with_answer) takes.
    pub id: i64,
    /// The run that asked.
    pub run_id: i64,
    /// The step it was asked on.
    ///
    /// That step **is** committed before the run pauses, so a resume starts after it and
    /// the `ask_question` call is not replayed;
    /// [`resume_with_answer`](crate::resume_with_answer) delivers the answer as a ledger
    /// observation. (A *parent* whose child asked is the different case: its own spawn
    /// step is left uncommitted so the resume re-adopts that child.)
    pub step: u32,
    /// What the agent asked.
    pub question: String,
    /// What the agent already knew, if it said.
    pub context: Option<String>,
    /// Options the agent offered. An answer need not be one of them.
    ///
    /// [`Choice`](crate::Choice) since 0.72.0. A row written by 0.71.0 holds these as
    /// a JSON array of plain strings and reads back as labels with no description —
    /// the deserializer accepts both spellings, which is what lets an existing store
    /// load without a migration.
    pub choices: Vec<crate::approve::Choice>,
    /// Every question of a batched ask, when this row is one (0.72.0). Empty for the
    /// singular `ask_question`, whose one question is [`Self::question`].
    pub questions: Vec<crate::approve::Question>,
    /// The per-question answers of a batched ask, in the order of
    /// [`Self::questions`]. Empty until the batch is answered, and empty for an
    /// answer that arrived through a resume — that one is the assembled
    /// [`Self::answer`], because a human resuming a run supplies one text.
    pub answers: Vec<Option<String>>,
    /// The answer, once there is one. For a batch, the assembled reply the model reads.
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
    /// A nullable JSON column, or the empty value when it is NULL or unreadable.
    fn json<T: serde::de::DeserializeOwned + Default>(
        r: &rusqlite::Row<'_>,
        at: usize,
    ) -> rusqlite::Result<T> {
        let raw: Option<String> = r.get(at)?;
        Ok(raw
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default())
    }
    Ok(PendingQuestion {
        id: r.get(0)?,
        run_id: r.get(1)?,
        step: r.get(2)?,
        question: r.get(3)?,
        context: r.get(4)?,
        choices: json(r, 5)?,
        answer: r.get(6)?,
        answered_by: r.get(7)?,
        resolved: r.get::<_, i64>(8)? != 0,
        questions: json(r, 9)?,
        answers: json(r, 10)?,
    })
}

/// The tree-wide event tail, as one statement (0.33.0).
///
/// A `const` rather than a literal at the call site so the query-plan test can
/// `EXPLAIN` the statement the crate actually runs. A test that re-typed the SQL
/// would keep passing after somebody "tidied" the `CROSS JOIN ... INDEXED BY`
/// into a plain join, which is exactly the change it exists to catch.
const TREE_EVENTS_SQL: &str = "WITH RECURSIVE tree(id) AS (
         SELECT id FROM runs WHERE id = ?1
         UNION ALL
         SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
     )
     SELECT e.id, e.json
     FROM tree CROSS JOIN run_events e INDEXED BY run_events_run
         ON e.run_id = tree.id
     WHERE e.id > ?2
     ORDER BY e.id ASC LIMIT ?3";

/// One run's gate attempts, oldest first (0.34.0).
///
/// A `const` for the reason [`TREE_EVENTS_SQL`] is: the query-plan test
/// `EXPLAIN`s the statement the crate runs, so dropping the index would fail the
/// test rather than pass a re-typed copy of the SQL.
///
/// **No `INDEXED BY` here, deliberately, and it was measured rather than
/// assumed.** 0.33.0's tree tail needs the hint because a recursive CTE is a
/// co-routine the planner cannot seek into; this is a plain
/// `WHERE run_id = ? ORDER BY id`, whose left prefix is the index's own, and
/// removing the hint changed the plan not at all across forty runs. The hint is
/// left out where it buys nothing; the query-plan test still fails if the index
/// itself goes.
const GATE_ATTEMPTS_SQL: &str = "SELECT id, step, phase, outcome, detail, at
     FROM gate_attempts
     WHERE run_id = ?1
     ORDER BY id ASC";

/// The latest gate attempt for one run (0.34.0).
const LAST_GATE_ATTEMPT_SQL: &str = "SELECT id, step, phase, outcome, detail, at
     FROM gate_attempts
     WHERE run_id = ?1
     ORDER BY id DESC LIMIT 1";

/// How one gate evaluation ended (0.34.0).
///
/// The variant this crate has never had is `Errored`. Before 0.34.0 a gate
/// answered `bool`, so a criterion that could not be evaluated at all — a
/// provider that returned a 529, a verdict nobody could parse — was
/// indistinguishable from work that was judged and found wanting. They call for
/// opposite responses: one is retried,
/// [`retry_gate`](crate::retry_gate) being how, and the other needs the work
/// changed.
///
/// ```
/// use io_harness::GateOutcome;
///
/// assert!(GateOutcome::Errored.is_retryable());
/// assert!(!GateOutcome::Failed.is_retryable());
/// assert_eq!(GateOutcome::Passed.as_str(), "passed");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum GateOutcome {
    /// The criterion was evaluated and satisfied.
    Passed,
    /// The criterion was evaluated and not satisfied. Nothing about the work has
    /// changed, so re-running the same criterion over the same tree would say the
    /// same thing.
    Failed,
    /// The criterion could not be evaluated. Whatever stopped it is in
    /// [`GateAttempt::detail`].
    Errored,
}

impl GateOutcome {
    /// The stored form.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            GateOutcome::Passed => "passed",
            GateOutcome::Failed => "failed",
            GateOutcome::Errored => "errored",
        }
    }

    /// Whether re-running the criterion could honestly produce a different
    /// answer.
    #[must_use]
    pub fn is_retryable(self) -> bool {
        matches!(self, GateOutcome::Errored)
    }

    /// Read the stored form back. An unknown string is `Errored` rather than a
    /// parse failure: a row written by a newer binary describes an evaluation
    /// this one cannot interpret, and treating that as "it did not run" is the
    /// conservative reading.
    fn from_str(s: &str) -> Self {
        match s {
            "passed" => GateOutcome::Passed,
            "failed" => GateOutcome::Failed,
            _ => GateOutcome::Errored,
        }
    }
}

/// One recorded gate evaluation (0.34.0).
///
/// ```
/// use io_harness::{GateOutcome, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("port it", "openrouter")?;
/// store.put_gate_attempt(run_id, 3, "review", GateOutcome::Errored, "HTTP 529")?;
///
/// let attempt = store.last_gate_attempt(run_id)?.unwrap();
/// assert_eq!(attempt.outcome, GateOutcome::Errored);
/// assert_eq!(attempt.detail, "HTTP 529");
/// assert!(attempt.outcome.is_retryable());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateAttempt {
    /// Row id, ascending in evaluation order.
    pub id: i64,
    /// The step the gate ran after.
    pub step: u32,
    /// Which criterion ran, as a short name — `review`, `command`, `contains`.
    pub phase: String,
    /// How it ended.
    pub outcome: GateOutcome,
    /// The verdict's reasons, or what stopped the evaluation. Empty for a plain
    /// pass.
    pub detail: String,
    /// When it ran.
    pub at: String,
}

/// One fold of a run's history into a paragraph (0.43.0).
///
/// Written when compaction replaces the older half of the observation ledger with
/// a model-written summary, and read back rather than rewritten when the same run
/// is resumed, branched or replayed. That is the whole reason it is a row: a
/// summary is the one thing in the ledger that cost a provider call to produce,
/// so paying for it twice is paying for the same sentence twice.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("port the parser", "openrouter")?;
/// store.put_summary(run_id, 12, 40, "Read the lexer, decided to keep the token enum.", 11)?;
///
/// // Looked up by where in the history it folded, which is what survives a
/// // resume — the step a run restarts at is one later than the step it died on.
/// let found = store.summary_for(run_id, 40)?.unwrap();
/// assert_eq!(found.through_step, 12);
/// assert_eq!(found.folded, 40, "this paragraph stands in for forty entries");
/// assert!(store.summary_for(run_id, 41)?.is_none(), "another prefix is another summary");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Summary {
    /// Row id, ascending in the order the folds happened.
    pub id: i64,
    /// The step whose assembly triggered the fold. For a trace reader; not the
    /// key, because it is not stable across a resume.
    pub through_step: u32,
    /// How many entries from the front of the ledger this paragraph stands in
    /// for. The lookup key, and what a resume replays the fold by.
    pub folded: u32,
    /// The summary itself: what was attempted, which files were touched, what was
    /// decided, and what is still open.
    pub text: String,
    /// The summary's estimated tokens, by the same estimator assembly uses.
    pub est_tokens: u64,
    /// When it was written.
    pub at: String,
}

fn summary_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Summary> {
    Ok(Summary {
        id: r.get(0)?,
        through_step: r.get::<_, i64>(1)? as u32,
        folded: r.get::<_, i64>(2)? as u32,
        text: r.get(3)?,
        est_tokens: r.get::<_, i64>(4)? as u64,
        at: r.get(5)?,
    })
}

fn gate_attempt_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<GateAttempt> {
    let outcome: String = r.get(3)?;
    Ok(GateAttempt {
        id: r.get(0)?,
        step: r.get(1)?,
        phase: r.get(2)?,
        outcome: GateOutcome::from_str(&outcome),
        detail: r.get(4)?,
        at: r.get(5)?,
    })
}

/// Read one `run_events` row back as `(cursor, event)` (0.33.0).
///
/// A row whose JSON will not parse is a `FromSqlConversionFailure` rather than a
/// silently skipped event: a gap in a stream a reader is using to decide things
/// is worse than an error, because nothing downstream can tell a missing event
/// from one that never happened.
fn event_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, crate::observe::RunEvent)> {
    let json: String = r.get(1)?;
    let event = serde_json::from_str(&json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok((r.get(0)?, event))
}

/// A plan the agent proposed, and what was decided about it (0.31.0).
///
/// The stored half of the plan gate, and the reason a run can be approved by a
/// process that never saw the one that proposed. It mirrors [`PendingQuestion`]
/// field for field because it exists for the same reason: a decision a human has
/// not made yet must outlive the process waiting for it.
///
/// ```
/// use io_harness::{Plan, PlanStep, PlanVerdict, Store};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run_id = store.start_run("port the parser", "openrouter")?;
/// let proposed = Plan::new([PlanStep::new("read the call sites"),
///                           PlanStep::new("port them").by("writer")]);
/// let id = store.put_plan(run_id, 2, &proposed)?;
///
/// // What a second process reads: the plan, undecided.
/// let pending = store.plan(id)?.expect("proposed above");
/// assert_eq!(pending.plan, proposed);
/// assert!(!pending.resolved && pending.verdict.is_none());
/// // And nothing is approved yet, which is what keeps the run from writing.
/// assert!(store.approved_plan(run_id)?.is_none());
///
/// store.decide_plan(id, &PlanVerdict::Approve, "human")?;
/// assert_eq!(store.approved_plan(run_id)?.as_ref(), Some(&proposed));
/// assert_eq!(store.plan(id)?.unwrap().decided_by.as_deref(), Some("human"));
///
/// // Deciding twice does not overwrite, and says so (0.33.0).
/// assert!(!store.decide_plan(id, &PlanVerdict::Cancel, "human")?);
/// assert_eq!(store.plan(id)?.unwrap().verdict, Some(PlanVerdict::Approve));
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPlan {
    /// The plan's own id, and what
    /// [`resume_with_plan_decision`](crate::resume_with_plan_decision) takes.
    pub id: i64,
    /// The run that proposed it.
    pub run_id: i64,
    /// The step it was proposed on. That step **is** committed before the run
    /// pauses, so a resume starts after it and the `propose_plan` call is not
    /// replayed — the approved plan is delivered as a ledger observation instead,
    /// exactly as an answer to a question is.
    pub step: u32,
    /// What the agent proposed.
    pub plan: crate::approve::Plan,
    /// What was decided, once something was.
    pub verdict: Option<crate::approve::PlanVerdict>,
    /// `"gate"` if a [`PlanGate`](crate::PlanGate) in the run's own process
    /// decided, `"human"` if the decision arrived through a resume after a pause.
    /// "The machine decided" and "a person decided" are different facts about a
    /// run, and the distinction is sharper here than anywhere else in the crate:
    /// this is the decision that spends the rest of the budget.
    pub decided_by: Option<String>,
    /// Whether it has been decided.
    pub resolved: bool,
}

/// Read one `plans` row. One place, so the queries that read the table cannot
/// drift in their column order.
fn plan_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<PendingPlan> {
    let steps: String = r.get(3)?;
    let verdict: Option<String> = r.get(4)?;
    let correction: Option<String> = r.get(5)?;
    Ok(PendingPlan {
        id: r.get(0)?,
        run_id: r.get(1)?,
        step: r.get(2)?,
        plan: crate::approve::Plan::new(
            serde_json::from_str::<Vec<crate::approve::PlanStep>>(&steps).unwrap_or_default(),
        ),
        // An unrecognised spelling reads back as undecided rather than as a
        // guess: a row this binary does not understand must not be reported as
        // an approval, because an approval is what lets the run write.
        verdict: match (verdict.as_deref(), correction) {
            (Some("approve"), _) => Some(crate::approve::PlanVerdict::Approve),
            (Some("revise"), c) => Some(crate::approve::PlanVerdict::Revise {
                correction: c.unwrap_or_default(),
            }),
            (Some("cancel"), _) => Some(crate::approve::PlanVerdict::Cancel),
            _ => None,
        },
        decided_by: r.get(6)?,
        resolved: r.get::<_, i64>(7)? != 0,
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
///
/// ```
/// use io_harness::{Store, TodoItem, TodoState, TODO_MAX_ITEMS};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("a very long plan", "/repo")?;
///
/// // Two past the cap. The plan is held to the cap and the overflow is reported,
/// // rather than the write being refused.
/// let long: Vec<TodoItem> = (0..TODO_MAX_ITEMS + 2)
///     .map(|i| TodoItem::new(format!("step {i}"), TodoState::Pending))
///     .collect();
/// let dropped = store.write_todos(run, &long)?;
///
/// assert_eq!(dropped, 2);
/// assert_eq!(store.todos(run)?.len(), TODO_MAX_ITEMS);
/// # Ok(())
/// # }
/// ```
pub const TODO_MAX_ITEMS: usize = 64;

/// Longest one item's text may be, in characters.
///
/// Truncated to fit rather than refused, like every other bounded tool result here.
///
/// ```
/// use io_harness::{Store, TodoItem, TodoState, TODO_TEXT_CAP};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("a wordy plan", "/repo")?;
///
/// let wordy = "x".repeat(TODO_TEXT_CAP + 50);
/// store.write_todos(run, &[TodoItem::new(wordy, TodoState::Active)])?;
///
/// assert_eq!(store.todos(run)?[0].text.chars().count(), TODO_TEXT_CAP);
/// # Ok(())
/// # }
/// ```
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
pub(crate) fn truncate_memory_value(value: &str, cap: usize) -> String {
    if value.chars().count() <= cap {
        return value.to_string();
    }
    // `saturating_sub` since 0.56.0, where the cap is an operator's number: a cap
    // shorter than the marker would otherwise panic on the subtraction, and a
    // marker alone is still an honest answer — it says the value did not fit.
    let keep = cap.saturating_sub(MEMORY_TRUNCATED.chars().count());
    let mut out: String = value.chars().take(keep).collect();
    out.push_str(MEMORY_TRUNCATED);
    out
}

/// The normalised words of a text (0.57.0): lowercased, split on anything that
/// is not alphanumeric, and anything shorter than three characters dropped.
///
/// One normaliser, called by both halves of 0.57.0 — the recall ranking in
/// [`crate::context`] and [`Store::memory_similar`] — because two answers to
/// "what counts as a word here" is how the two come to disagree.
///
/// The three-character floor is a stopword list nobody has to maintain: it
/// removes `a`, `of`, `is`, `to` and the rest of the closed class, and keeps
/// every identifier a note or a goal is actually about. Splitting on
/// non-alphanumerics is what makes `src/state.rs` in a note match the same path
/// in a run's ledger — the two tokens are `src` and `state` either way, and `rs`
/// falls under the floor.
///
/// A [`BTreeSet`](std::collections::BTreeSet) and not a `HashSet`: the iteration
/// order of a `HashSet` is seeded per process, and 0.57.0's ranking must be a
/// pure function of the store and the turn or a replayed run recalls differently
/// than the run it replays.
pub(crate) fn memory_tokens(text: &str) -> std::collections::BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 3)
        .map(|w| w.to_lowercase())
        .collect()
}

/// How much two token sets have in common, as `(shared, total)` — the two halves
/// of `|A ∩ B| / |A ∪ B|`, returned rather than divided (0.57.0).
///
/// Returned as a pair so the caller compares by cross-multiplication instead of
/// by float division: `shared * 100 >= total * percent` is exact at the
/// threshold, where `shared as f64 / total as f64 >= 0.6` is at the mercy of
/// whichever way the division rounded.
pub(crate) fn memory_overlap(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> (usize, usize) {
    let shared = a.intersection(b).count();
    (shared, a.len() + b.len() - shared)
}

/// How much of two texts must overlap before one is reported as restating the
/// other, in percent of their union (0.57.0).
///
/// High deliberately. A hit is handed to the model as "you already hold this
/// under another key", and a threshold that fires on a neighbouring subject
/// teaches the model to ignore the line — which costs more than the report is
/// worth. At 60 the two texts share three words in five.
const MEMORY_SIMILAR_PERCENT: usize = 60;

/// Whether one text restates another: at least [`MEMORY_SIMILAR_PERCENT`] of the
/// two token sets' union is shared (0.57.0).
///
/// Cross-multiplied rather than divided, so nothing rounds. Two texts with no
/// words at all between them are not similar — an empty union would otherwise
/// divide by zero, and "these two say nothing" is not a restatement.
pub(crate) fn memory_is_similar(
    a: &std::collections::BTreeSet<String>,
    b: &std::collections::BTreeSet<String>,
) -> bool {
    let (shared, total) = memory_overlap(a, b);
    total > 0 && shared * 100 >= total * MEMORY_SIMILAR_PERCENT
}

/// The workspace key the scope above every workspace is stored under (0.56.0).
///
/// Durable memory is keyed by a workspace's canonical path. A fact true of every
/// repository an operator owns — the package manager they use, a convention they
/// never want broken — had to be learned again per workspace or written by hand
/// into each one's instructions. Entries under this key are recalled by every
/// run over every workspace.
///
/// **A key in both scopes resolves to the workspace's**, and the global entry is
/// not carried at all: the specific place always knows better than the general
/// one, which is also what makes a wrong global note locally correctable.
///
/// Not a path, and it cannot collide with one: `std::fs::canonicalize` returns
/// an absolute path on every platform this crate supports, and `<` and `>` are
/// not legal in a Windows path at all. A directory *named* `<global>` still
/// keys on its own canonical path.
///
/// ```
/// use io_harness::{Store, GLOBAL_MEMORY_WORKSPACE};
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("port the parser", "/repo")?;
/// store.memory_put(GLOBAL_MEMORY_WORKSPACE, "package-manager", "pnpm", run, 1)?;
///
/// // It is an ordinary workspace bucket, so it holds its own caps, its own
/// // pins and its own eviction — everything a workspace's memory does.
/// assert_eq!(store.memory_list(GLOBAL_MEMORY_WORKSPACE)?.len(), 1);
/// assert!(store.memory_list("/repo")?.is_empty(), "and it is not /repo's");
/// # Ok(())
/// # }
/// ```
pub const GLOBAL_MEMORY_WORKSPACE: &str = "<global>";

/// The three caps a workspace's memory is held inside (0.56.0).
///
/// [`Default`] is the crate's own numbers — [`MEMORY_MAX_ENTRIES`],
/// [`MEMORY_MAX_CHARS`] and [`MEMORY_MAX_ENTRY_CHARS`] — so a caller that sets
/// nothing keeps 0.10.0's behaviour exactly. An operator moves them with
/// `[memory]` in `io.toml` or [`TaskContract::with_memory_limits`].
///
/// Raising them is not free, and the coupling is worth stating where the type
/// is: the memory block gets a quarter of a turn's effective tokens, and the
/// defaults were chosen so a whole store fits inside that share. Past that
/// point recall can no longer carry everything and selection begins deciding
/// what the model sees — which is safe only because, since this release,
/// selection is by evidence rather than by the clock.
///
/// [`TaskContract::with_memory_limits`]: crate::TaskContract::with_memory_limits
///
/// ```
/// use io_harness::{MemoryLimits, Store, MEMORY_MAX_ENTRIES};
///
/// # fn main() -> io_harness::Result<()> {
/// assert_eq!(MemoryLimits::default().max_entries, MEMORY_MAX_ENTRIES);
///
/// let store = Store::memory()?;
/// let run = store.start_run("goal", "/repo")?;
/// let tight = MemoryLimits {
///     max_entries: 2,
///     ..MemoryLimits::default()
/// };
/// for key in ["a", "b", "c"] {
///     store.memory_write_with("/repo", key, "v", run, 1, Default::default(), tight)?;
/// }
/// // Three writes under a cap of two, and the one nothing has drawn on goes.
/// assert_eq!(store.memory_list("/repo")?.len(), 2);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLimits {
    /// How many entries one workspace may hold.
    pub max_entries: usize,
    /// How many characters one workspace's entries may total.
    pub max_chars: usize,
    /// How many characters a single entry may hold before it is truncated with
    /// a visible marker.
    pub max_entry_chars: usize,
}

impl Default for MemoryLimits {
    fn default() -> Self {
        Self {
            max_entries: MEMORY_MAX_ENTRIES,
            max_chars: MEMORY_MAX_CHARS,
            max_entry_chars: MEMORY_MAX_ENTRY_CHARS,
        }
    }
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

    /// A run tried to overwrite a pinned note and was refused (0.30.0).
    ///
    /// Recorded rather than silently dropped: an agent that believes it corrected
    /// something and did not will act on the correction it thinks it made. A
    /// trace row rather than a new [`EventKind`](crate::observe::EventKind)
    /// variant, because the question this answers — *did my pin hold* — is asked
    /// after the run by somebody reading the store, not during it by an observer.
    pub fn memory_refused(step: u32, detail: impl Into<String>) -> Self {
        Self::of("memory_refused", step, detail)
    }
    /// A run withdrew a note (0.56.0). Its own kind rather than an eviction, so
    /// a trace tells "the agent decided this was wrong" apart from "the cap
    /// dropped it" — two different facts about the same disappearance.
    pub fn memory_forget(step: u32, detail: impl Into<String>) -> Self {
        Self::of("memory_forget", step, detail)
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

    /// The agent proposed a plan at this step and did nothing else (0.31.0). The
    /// detail is the plan's shape rather than its text, which is already in the
    /// `plans` table and would otherwise be stored twice.
    pub fn plan_proposed(step: u32, detail: impl Into<String>) -> Self {
        Self::of("plan_proposed", step, detail)
    }

    /// A verdict on that plan entered the run (0.31.0). The detail names who
    /// decided — a `PlanGate` in the process, or a human after a pause — and what
    /// they decided.
    pub fn plan_decided(step: u32, detail: impl Into<String>) -> Self {
        Self::of("plan_decided", step, detail)
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

    /// What the failing criterion printed was carried into this step's request
    /// (0.70.0). The step is the one that was *told*, not the one that failed —
    /// the failure is already in `sandbox_events` under the step it happened on,
    /// and what this answers is the other question: which attempt was informed
    /// and which was blind.
    ///
    /// The detail names the failed step and the size of what was carried, not the
    /// output itself, which is already stored once as a `gate_output` sandbox row
    /// and would otherwise be stored twice — the same rule
    /// [`Self::todo_write`] and [`Self::plan_proposed`] follow.
    pub fn gate_feedback(step: u32, detail: impl Into<String>) -> Self {
        Self::of("gate_feedback", step, detail)
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
    /// The tool that made it — `write_file`, `edit_file` or `patch_file`.
    pub tool: String,
    /// The path, as the agent named it (relative to the workspace root).
    pub path: String,
    /// Lines present after and not before.
    pub lines_added: u64,
    /// Lines present before and not after.
    pub lines_removed: u64,
    /// The change as a unified diff of the whole file, or `None` (0.51.0).
    ///
    /// `@@` line numbers are the **file's**, so the text reads and applies
    /// against the file a human opens. It is a body only — no `---`/`+++`
    /// headers, which [`Store::patch`] writes because it is the only caller that
    /// knows the path.
    ///
    /// `None` has three causes and none of them is "nothing happened":
    /// the row was written before 0.51.0; the file's previous contents were not
    /// kept, so there was nothing to diff against (over the snapshot cap or not
    /// UTF-8 — the reason is on that path's snapshot row); or the rendered
    /// diff would itself have exceeded that cap. An absent hunk is reported as
    /// absent everywhere it is read, never treated as an empty patch.
    pub hunk: Option<String>,
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
            hunk: None,
        }
    }

    /// Attach the change as a unified diff of the **whole file** (0.51.0).
    ///
    /// Separate from [`Edit::measure`], and called with different texts, which is
    /// the point rather than an inconvenience. `measure` is handed the fragment
    /// an `edit_file` replaced, so its counts have meant "the size of the
    /// replacement" since 0.18.0 and the reason is written at its call site.
    /// A hunk needs the file's own line numbers, so it is computed from the
    /// file's two texts. Folding the two together would be the natural tidy-up
    /// and would silently change every number in every existing trace.
    ///
    /// `None` — the field is left as it was — when nothing changed, or when the
    /// rendered diff would exceed the store's 1 MiB snapshot cap. A caller that
    /// could not read the previous contents simply does not call this.
    ///
    /// Public for the reason [`Edit::measure`] and [`Store::record_edit`] are: a
    /// caller recording its own edit rows would otherwise be able to write a
    /// count and never a change, which makes [`Store::patch`] useless to them.
    ///
    /// ```
    /// use io_harness::Edit;
    ///
    /// let before = "fn parse() {}\n";
    /// let after = "fn parse(s: &str) {}\n";
    /// let edit = Edit::measure(2, "edit_file", "src/parse.rs", before, after)
    ///     .with_hunk(before, after);
    ///
    /// let hunk = edit.hunk.expect("a change renders a hunk");
    /// assert!(hunk.starts_with("@@ -1,1 +1,1 @@"));
    /// assert!(hunk.contains("-fn parse() {}"));
    /// assert!(hunk.contains("+fn parse(s: &str) {}"));
    ///
    /// // Nothing changed, so there is nothing to render.
    /// assert_eq!(Edit::measure(2, "write_file", "a", before, before).with_hunk(before, before).hunk, None);
    /// ```
    pub fn with_hunk(mut self, before: &str, after: &str) -> Self {
        self.hunk = crate::diff::render(before, after).filter(|h| h.len() <= MAX_SNAPSHOT_BYTES);
        self
    }
}

/// The largest previous file kept as a restore point, in bytes (0.28.0).
///
/// A cap rather than no cap because the store is a SQLite file an operator keeps
/// for the trace, and a run that rewrites a 200 MB fixture would otherwise put
/// 200 MB into it for a rewind nobody asked for. A file over the cap is recorded
/// as [`Kept::Unkept`] with its size, which is honest — the alternative,
/// recording nothing at all, would make it indistinguishable from a path the run
/// never touched, and a caller would then be told a rewritten file was untouched.
pub(crate) const MAX_SNAPSHOT_BYTES: usize = 1 << 20;

/// What was kept of a file's contents from before a run first wrote it
/// (0.28.0).
///
/// Three cases and not two, because "there is no text to put back" has two
/// causes that undo differently: a file that did not exist is put back by
/// deleting it, and a file whose contents were too large or not text cannot be
/// put back at all and must be left alone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Kept {
    /// The file's previous contents, verbatim.
    Text(String),
    /// The file did not exist before the run wrote it.
    Absent,
    /// The file existed and its contents were deliberately not kept. The payload
    /// is the short human reason — over [`MAX_SNAPSHOT_BYTES`], or not UTF-8.
    Unkept(String),
}

/// The state of one file before a run first wrote it (0.28.0).
///
/// One row per file per run, written at the *first* edit, which is what makes
/// "the way it was" well-defined: a run that edits the same file five times has
/// one restore point, the state before edit one, not the state before edit five.
/// Storage is therefore bounded by how many files a run touched rather than how
/// many edits it made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// The step that made the first write.
    pub step: u32,
    /// The path, as the agent named it (relative to the workspace root).
    pub path: String,
    /// What was there, or why it was not kept.
    pub kept: Kept,
}

/// What one memory entry looked like before a run first wrote it (0.36.0).
///
/// One row per `(run, workspace, key)`, at the run's first write, which is what
/// makes "the way it was" one answer rather than one per edit — the same
/// definition and the same guard [`Snapshot`] has for files.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MemorySnapshot {
    /// The workspace the entry belongs to.
    pub workspace: String,
    /// The entry's key.
    pub key: String,
    /// The value that was there, or `None` when there was no entry.
    pub before: Option<String>,
    /// The kind it had, or `None` when there was no entry.
    pub kind: Option<String>,
    /// The step the run was on when it took this restore point (0.56.0). Carried
    /// so a restore that has to INSERT — the entry the run *removed* rather than
    /// edited — has a step to attribute the row to.
    pub step: u32,
    /// True when the run *created* this entry, so putting it back means removing
    /// it. Kept apart from `before.is_none()` for the reason `snapshots.state`
    /// is: the two ways to be wrong are refusing to restore and deleting an entry
    /// the run only edited, and only the first is recoverable.
    pub created: bool,
}

/// What one rewind of one run put back, took away and cleared (0.36.0).
///
/// Returned by [`Store::rewinds`]. A rewind changes rows that already exist, so
/// this is written *before* they change: it is the durable half of "the trace
/// keeps both branches", and the reason nothing in `steps`, the event stream, the
/// spawn records or the ledger has to be deleted for an undo to be honest.
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
/// // An earlier run left a note behind.
/// let first = store.start_run("learn something", &root)?;
/// store.memory_write(&root, "retries", "three", first, 1, MemoryKind::Fact)?;
///
/// // This run corrects it, and is then put back.
/// let second = store.start_run("get it wrong", &root)?;
/// store.memory_write(&root, "retries", "nine", second, 1, MemoryKind::Fact)?;
/// rewind_run(&ws, &store, second)?;
///
/// assert_eq!(store.memory_get(&root, "retries")?.unwrap().value, "three");
/// let record = &store.rewinds(second)?[0];
/// assert_eq!(record.memory_restored, ["retries"]);
/// assert!(record.memory_removed.is_empty(), "it was edited, not created");
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewindRecord {
    /// UTC time of the rewind, from the database clock.
    pub at: String,
    /// The paths that were put back, in the order the run first wrote them.
    pub files: Vec<String>,
    /// Memory keys whose earlier value was restored.
    pub memory_restored: Vec<String>,
    /// Memory keys the run created, and which were therefore removed.
    pub memory_removed: Vec<String>,
    /// The goals of the children that were still queued and no longer are.
    pub queue_cleared: Vec<String>,
    /// The step this undid, or `None` for a whole-run rewind (0.51.0).
    ///
    /// Two different acts share this table and a reader has to be able to tell
    /// them apart: [`crate::rewind_run`] puts a run back to before it started,
    /// and [`crate::rewind_step`] reverse-applies one step's stored hunks. A
    /// trace that reported both as "something was undone" could not be audited.
    pub undid_step: Option<u32>,
}

/// One background process the run started, as the store last knew it.
///
/// A row is written when the handle starts and updated when it ends, so a handle
/// still running reads back with `state` of `running`, no `code`, and no `reason`.
/// What it records is what this process last observed, never a claim about the
/// machine now: the pids may have been reused since, which is why a resume reads
/// this to know something was left behind and does not signal it.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let run = store.start_run("bring the dev server up", "/repo")?;
/// store.record_handle_started(run, 1, 1, "npm run dev")?;
/// store.record_handle_pids(run, 1, &[4021, 4022])?;
///
/// let handles = store.process_handles(run)?;
/// assert_eq!(handles[0].line, "npm run dev");
/// assert_eq!(handles[0].pids, vec![4021, 4022]);
/// // Still running, so there is no exit to report yet.
/// assert_eq!(handles[0].state, "running");
/// assert_eq!(handles[0].code, None);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessHandle {
    /// The handle the run allocated, unique within the run.
    pub handle: u64,
    /// The step that started it.
    pub step: u32,
    /// The command line, as the agent wrote it.
    pub line: String,
    /// Every pid the process tree was last seen to hold — the leader first.
    ///
    /// Empty for a handle whose pids were never recorded, which is both a handle
    /// that failed to spawn and a handle recorded by a writer that only knew the
    /// command. Stored as one comma-joined column rather than a child table
    /// because the pids are only ever read as a unit, with the handle they belong
    /// to; nothing queries by pid, so a second table would buy a join and an
    /// index for a list that is read whole or not at all.
    pub pids: Vec<u32>,
    /// What the handle was last known to be doing: `running`, or the terminal
    /// state it reached.
    pub state: String,
    /// The exit code, once it exited. `None` while it runs, and `None` for a
    /// handle that ended without one — killed, or never spawned.
    pub code: Option<i32>,
    /// Why it left `running`, in the words of whatever ended it. `None` while it
    /// runs.
    pub reason: Option<String>,
}

/// The columns [`handle_row`] reads, in the order it reads them. Named once so
/// the two queries that select handles cannot drift out of step with the mapping.
const HANDLE_COLUMNS: &str = "handle, step, line, pids, state, code, reason";

fn handle_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<ProcessHandle> {
    let pids: String = r.get(3)?;
    Ok(ProcessHandle {
        handle: r.get(0)?,
        step: r.get(1)?,
        line: r.get(2)?,
        // A pid that does not parse is dropped rather than guessed at. This crate
        // only ever writes decimal pids, so that can only happen to a database
        // another program has written to.
        pids: pids.split(',').filter_map(|p| p.parse().ok()).collect(),
        state: r.get(4)?,
        code: r.get(5)?,
        reason: r.get(6)?,
    })
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

/// Every table this crate creates that hangs off a run, paired with the column
/// that names the run.
///
/// 0.58.0's retention calls all walk this list: the size call sums over it, the
/// deletion clears it, and the archive empties the words out of it. It is one
/// list rather than three because three would drift, and a table missing from
/// one of them is a silent orphan — the schema declares exactly one foreign key
/// (`steps.run_id`) and never enables `PRAGMA foreign_keys`, so nothing in
/// SQLite will say a word about a row whose run no longer exists.
///
/// **`memory` is deliberately absent.** A note carries the `run_id` that wrote
/// it, but it is a workspace asset that outlives that run — 0.56.0 made this
/// explicit by adding a scope above the workspace — so removing a session never
/// removes a note. `memory_recalls` *is* here: a recall row names a run, and a
/// recall by a run that no longer exists is not evidence.
///
/// `sessions`, `session_turns` and `runs` are absent too, because they are keyed
/// by the session or are the run row itself, and the retention calls handle them
/// by name in the order that keeps the walk answerable while it runs.
pub(crate) const RUN_TABLES: &[(&str, &str)] = &[
    ("steps", "run_id"),
    ("policy_events", "run_id"),
    ("pending_approvals", "run_id"),
    ("agent_events", "run_id"),
    ("sandbox_events", "run_id"),
    ("checkpoint_events", "run_id"),
    ("spawns", "parent_run_id"),
    ("mcp_events", "run_id"),
    ("memory_recalls", "run_id"),
    ("memory_snapshots", "run_id"),
    ("context_events", "run_id"),
    ("run_outcomes", "run_id"),
    ("run_policies", "run_id"),
    ("ledger_observations", "run_id"),
    ("provider_calls", "run_id"),
    ("edits", "run_id"),
    ("todos", "run_id"),
    ("pending_questions", "run_id"),
    ("citations", "run_id"),
    ("server_tool_calls", "run_id"),
    ("process_handles", "run_id"),
    ("handle_output", "run_id"),
    ("snapshots", "run_id"),
    ("plans", "run_id"),
    ("agent_queue", "parent_run_id"),
    ("run_events", "run_id"),
    ("rewinds", "run_id"),
    ("gate_attempts", "run_id"),
    ("summaries", "run_id"),
    // 0.60.0 — keyed by the RECIPIENT, and the sender end is covered because a
    // mailbox lives inside one tree and a session's run list is that whole tree, so
    // both ends of every message are in the list. That is an argument rather than a
    // guarantee, which is why `a_deleted_session_leaves_no_message_at_either_end`
    // asserts the table is empty afterwards instead of trusting it.
    ("agent_messages", "to_run_id"),
    // 0.62.0 — the lease is run-keyed like everything above it, and 0.58.0's
    // schema-driven seeder is what said so: deleting a session left three lease
    // rows behind pointing at runs that no longer existed. A stale lease is worse
    // than an ordinary orphan, because the run id it holds is one SQLite will
    // eventually hand to a different run — which would then start life refused by a
    // driver that died before it existed.
    ("run_leases", "run_id"),
    // 0.64.0 — the assistant turn is run-keyed like everything above it. It holds
    // what a step asked for, which is the model's own words and arguments, so a
    // session deleted for retention that left these behind would be leaving the
    // most quotable rows of the run it claimed to remove.
    ("step_turns", "run_id"),
    // 0.65.0 — the journal of calls the harness could not inspect, run-keyed like
    // everything above it. 0.58.0's schema-driven seeder found this one too, in
    // the same round it was written: an attempt row names a run, a tool and a
    // time, and a session deleted for retention that left them behind would be
    // keeping a record of what the operator was charged for by a run it claims to
    // have removed.
    ("tool_attempts", "run_id"),
];

/// What one session is holding, in the bytes of its own rows.
///
/// **These are content bytes, not pages on disk, and the distinction is not
/// pedantry.** SQLite stores rows in b-tree pages shared between whatever
/// happens to be adjacent, and `dbstat` — which this crate's bundled SQLite does
/// compile in — reports a page's owner as a *table*, never as a session. A
/// per-session page count would therefore be a number with no way to be right.
/// What is exactly answerable is how many bytes of text and blob this session's
/// rows hold, and how many rows that is, which is what a growth question is
/// really asking. For the file's own arithmetic, use [`Store::store_size`].
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let session = store.create_session("/repo")?;
/// let run = store.start_run("summarise the changelog", "/repo")?;
/// let turn = store.record_turn(session, None, run, "what changed in 0.57?")?;
/// store.finish_turn(turn, Some("three things"), "ok")?;
///
/// let size = store.session_size(session)?.expect("the session exists");
/// assert_eq!(size.turns, 1);
/// assert_eq!(size.runs, 1);
/// assert!(size.bytes > 0);
///
/// // Asking the size of a session that is not there has no answer, which is a
/// // different fact from a session that is there and holds nothing.
/// assert!(store.session_size(9_999)?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionSize {
    /// The session this describes.
    pub session_id: i64,
    /// Turns in the conversation.
    pub turns: u64,
    /// Runs in the session's tree — its turns' runs, and everything those runs
    /// spawned.
    pub runs: u64,
    /// Rows across every table keyed to those runs, plus the turns themselves.
    pub rows: u64,
    /// The summed `length()` of every text and blob column of those rows.
    pub bytes: u64,
}

/// Columns whose declared type is text but whose content is a fact rather than
/// a word — a kind, a verdict, a status, a path, an identifier, a timestamp.
///
/// [`Store::archive_session`] empties everything else. This list is the
/// release's actual decision about where the line between "what did this cost
/// and what did it touch" and "what exactly was said" falls, so it is written
/// out and reviewable rather than inferred from a column name.
///
/// The default is to **clear**: a column added by a later release and not named
/// here is treated as words, which is the safe direction — an archive that
/// clears one number too many loses a figure the trace can live without, while
/// one that keeps one sentence too many is a promise it did not keep.
fn is_fact_column(table: &str, column: &str) -> bool {
    // Universal across the schema: what kind of thing this row is, what was
    // decided about it, where it happened, and when.
    if matches!(
        column,
        "kind"
            | "act"
            | "state"
            | "status"
            | "outcome"
            | "verdict"
            | "source"
            | "layer"
            | "rule"
            | "provider"
            | "model"
            | "finish_reason"
            | "tool"
            | "path"
            | "at"
            | "created_at"
            | "started_at"
            | "finished_at"
            | "workspace"
            | "turn_kind"
    ) {
        return true;
    }
    match (table, column) {
        // Which workspace the run ran over. **Not the goal**: for a run driving
        // a session turn the goal IS the user's prompt, so keeping it would
        // leave the question in the trace after the archive removed it from the
        // conversation. Found by the needle sweep, not by reading the schema.
        ("runs", "file") => true,
        // `decision` is a verdict here and the model's own words in `steps`,
        // which is why this is a per-table judgement and not a column-name one.
        ("policy_events", "decision") => true,
        // The session's root directory.
        ("sessions", "root") => true,
        // Which file a restore point or an edit is about, kept while its
        // contents go.
        ("snapshots", "path") | ("edits", "path") => true,
        // What a policy event was about — the target is a path or a command,
        // which is what "what did it touch" means for a refusal.
        ("policy_events", "target") | ("ledger_observations", "target") => true,
        // 0.77.0 — where an observation's content came from. A classification,
        // not a word: it says an MCP server spoke, never what the server said,
        // and the text column beside it is still cleared.
        //
        // Named here **because the default is to clear**, and an archive that
        // silently emptied this column would leave a trace that cannot answer
        // the one question the release added — was this content external? —
        // while every other test in the tree passed, since they all read rows
        // the same run had just written. This one line is the whole of F16.
        ("ledger_observations", "origin") => true,
        // A memory key names an entry that still exists; the note itself is not
        // this session's to clear.
        ("memory_recalls", "key") | ("memory_snapshots", "key") => true,
        _ => false,
    }
}

/// What an archive cleared.
///
/// `rows` and `bytes` are what was **removed**, not what remains — an archive
/// keeps every row, so a count of what is left would be the same before and
/// after and would say nothing. A second archive of the same session reports
/// zero for both, which is how idempotence is visible rather than assumed.
///
/// See [`Store::archive_session`] for what survives and why.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let session = store.create_session("/repo")?;
/// let run = store.start_run("a goal", "/repo")?;
/// let turn = store.record_turn(session, None, run, "something private")?;
/// store.finish_turn(turn, Some("an answer"), "ok")?;
///
/// let first = store.archive_session(session)?;
/// assert_eq!(first.turns, 1, "the conversation still has its shape");
/// assert!(first.bytes > 0, "and it no longer has its words");
///
/// // Idempotent, and visibly so: nothing was left to clear.
/// let second = store.archive_session(session)?;
/// assert_eq!(second.rows, 0);
/// assert_eq!(second.bytes, 0);
/// assert_eq!(second.turns, 1);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Archived {
    /// Turns in the archived session. Unchanged by the archive — the
    /// conversation still has the shape it had, without the words.
    pub turns: u64,
    /// Rows whose text was cleared.
    pub rows: u64,
    /// Bytes of text cleared.
    pub bytes: u64,
}

/// What a removal took, and what it refused to take.
///
/// Returned by [`Store::delete_session`] and [`Store::sweep_sessions`]. Both
/// report the same shape because they are the same removal reached two ways —
/// by naming a session, or by naming a date — and an operator comparing a
/// sweep's result against a targeted deletion should not have to translate
/// between two kinds of receipt.
///
/// `refused` is only ever non-empty for a sweep. A date is a policy applied to
/// sessions nobody looked at, so a session holding a run that could still be
/// resumed is left alone and named here; [`Store::delete_session`] takes one id
/// and removes it, because that is somebody's decision rather than a policy.
///
/// **A deletion cannot be undone by this crate.** The counts exist so the caller
/// can record what happened while the information still exists.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// let session = store.create_session("/repo")?;
/// let run = store.start_run("summarise the changelog", "/repo")?;
/// let turn = store.record_turn(session, None, run, "what changed in 0.57?")?;
/// store.finish_turn(turn, Some("three things"), "ok")?;
///
/// let pruned = store.delete_session(session)?;
/// assert_eq!(pruned.sessions, 1);
/// assert_eq!(pruned.turns, 1);
/// assert_eq!(pruned.runs, 1);
/// assert!(pruned.refused.is_empty());
///
/// // Deleting what is not there succeeds and reports nothing — which is a
/// // different answer from asking its size, and deliberately so.
/// assert_eq!(store.delete_session(session)?.sessions, 0);
/// assert!(store.session_size(session)?.is_none());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Pruned {
    /// Sessions removed.
    pub sessions: u64,
    /// Turns removed with them.
    pub turns: u64,
    /// Runs removed, including every run those sessions' runs spawned.
    pub runs: u64,
    /// Rows removed across every table.
    pub rows: u64,
    /// The bytes those rows held, on the same measure as [`SessionSize::bytes`].
    pub bytes: u64,
    /// Restore points removed. An undo depends on these, so a removal says how
    /// many promises it withdrew at the time rather than at the moment somebody
    /// reaches for one.
    pub restore_points: u64,
    /// Sessions a sweep left alone because they hold a run that can still be
    /// resumed. Always empty for [`Store::delete_session`].
    pub refused: Vec<i64>,
}

/// What the whole store is holding: the file's real page arithmetic, and where
/// the pages went.
///
/// `file_bytes` is `page_size × page_count` — the size SQLite believes the
/// database to be, which is the size on disk for everything but the trailing
/// journal. `free_bytes` is the part of that already free *inside* the file and
/// therefore reusable without growing it. A deletion moves bytes from the first
/// figure into the second and shrinks nothing; [`Store::compact`] is what moves
/// them out of the file altogether.
///
/// `tables` is `dbstat`'s per-table page usage, largest first, so the answer to
/// "what is this store holding" is the first line rather than a search.
///
/// ```
/// use io_harness::Store;
///
/// # fn main() -> io_harness::Result<()> {
/// let store = Store::memory()?;
/// store.create_session("/repo")?;
/// store.start_run("a goal", "/repo")?;
///
/// let size = store.store_size()?;
/// assert_eq!(size.sessions, 1);
/// assert_eq!(size.runs, 1);
/// assert!(size.file_bytes > 0);
/// assert!(size.free_bytes <= size.file_bytes);
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreSize {
    /// `page_size × page_count`: what the database file occupies.
    pub file_bytes: u64,
    /// `page_size × freelist_count`: the part of it already free inside the
    /// file, which a write reuses and a deletion adds to.
    pub free_bytes: u64,
    /// Sessions in the store.
    pub sessions: u64,
    /// Runs in the store, including runs no session reaches.
    pub runs: u64,
    /// Per-table bytes from `dbstat`, largest first.
    pub tables: Vec<(String, u64)>,
}

// The `impl Store` blocks live in `src/state/<subject>.rs` from 0.62.0, with the
// tests that exercise them. **No public type moved**: `Store` itself and every
// type this module exports are still defined here, so `docs/public-api.txt` —
// which records the file each public name is defined in — does not move a single
// existing line, and a sabotage that moves a public *type* into a submodule turns
// `tests/public_api.rs` red.
//
// The axis is the `impl` block rather than the type for exactly that reason. An
// inherent method is invisible to the snapshot (the gap 0.60.3 recorded when a
// newly-`pub` method left it green), which is a liability elsewhere and is what
// makes this particular move provably shape-preserving.
//
// A child module sees its parent's private items, so these modules reach `conn`,
// `owner`, `leases` and the row helpers above through `use super::*` without any
// of them becoming `pub(crate)` for the split's convenience.
mod accounting;
mod agents;
mod approvals;
mod leases;
mod memory;
mod runs;
mod schema;
mod sessions;
#[cfg(test)]
mod testutil;
mod trace;

/// Row ids as a SQL list. Every caller passes integers this crate minted, so
/// there is nothing here to escape — the function exists because SQLite has no
/// array parameter and a `?` per id would rebuild the statement per call.
pub(crate) fn id_list(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}
