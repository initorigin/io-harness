//! 0.65.0 — a run killed in the middle of a call the harness cannot inspect
//! pauses for a decision instead of silently making the call twice.
//!
//! The subject is a durable journal and the rule that reads it. Everything a run
//! records normally is written at the step boundary that commits, deliberately,
//! "so an observation belonging to a step that never committed does not outlive
//! it" — which is exactly why an interrupted external call leaves no trace today.
//! An attempt row is the one thing in this crate that must outlive the step it
//! belongs to, and that is what these tests are about.
//!
//! Two `Store` handles over one file, never two processes: the two-process shape
//! over one SQLite file is the `DatabaseBusy` flake that has failed `release.yml`
//! itself here.

use io_harness::{Store, ToolRecovery};

/// F6 — an open attempt survives the handle that wrote it, and a closed one does
/// not come back.
///
/// The first handle writes both rows and is dropped before the second is opened,
/// so nothing in-memory can answer: the second handle's answer comes off disk or
/// not at all. Both directions are asserted — the open row is returned and the
/// closed row is not — because "the journal survives" and "the journal is not
/// noise on every resume" are two different claims and only one of them is about
/// durability.
#[test]
fn an_open_attempt_survives_the_process_and_a_closed_one_does_not_pause() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("runs.sqlite3");

    let (run, open_id) = {
        let store = Store::open(&path).expect("a store");
        let run = store.start_run("goal", "root").expect("a run");

        let closed = store
            .open_attempt(run, 3, "charge", ToolRecovery::Indeterminate)
            .expect("an attempt")
            .expect("a row for an indeterminate call");
        store.close_attempt(closed).expect("closing it");

        let open = store
            .open_attempt(run, 4, "deploy", ToolRecovery::Indeterminate)
            .expect("a second attempt")
            .expect("a row for an indeterminate call");

        // The writing handle already answers correctly. What the next block adds
        // is that the answer is on disk rather than in this handle.
        assert_eq!(store.open_attempts(run).expect("open attempts").len(), 1);
        (run, open)
    };

    let store = Store::open(&path).expect("a second store over the same file");
    let open = store.open_attempts(run).expect("open attempts");
    assert_eq!(open.len(), 1, "exactly the attempt that never completed");
    assert_eq!(open[0].id, open_id);
    assert_eq!(open[0].step, 4);
    assert_eq!(open[0].tool, "deploy");
}

/// An attempt belongs to its run, and a second run's open attempt does not pause
/// the first.
///
/// The control for the criterion above: `open_attempts` returning "every open row
/// in the store" would satisfy F6's assertions in a single-run fixture and would
/// pause every resume in a store that has ever crashed.
#[test]
fn an_open_attempt_belongs_to_its_own_run() {
    let store = Store::memory().expect("a store");
    let mine = store.start_run("mine", "root").expect("a run");
    let theirs = store.start_run("theirs", "root").expect("another run");

    store
        .open_attempt(theirs, 1, "charge", ToolRecovery::Indeterminate)
        .expect("an attempt");

    assert!(store.open_attempts(mine).expect("open attempts").is_empty());
    assert_eq!(store.open_attempts(theirs).expect("open attempts").len(), 1);
}

/// A replayable call is not journalled at all, so a run of built-in tools pays
/// nothing for the release.
///
/// Asserted on the store rather than on an outcome: "no pause happened" and "no
/// row was written" are two claims, and a journal that recorded everything and
/// then decided not to pause would satisfy only the first.
#[test]
fn a_replayable_call_writes_no_row() {
    let store = Store::memory().expect("a store");
    let run = store.start_run("goal", "root").expect("a run");

    let id = store
        .open_attempt(run, 1, "read_file", ToolRecovery::Replayable)
        .expect("no row, and no error");

    assert_eq!(id, None, "a replayable call is not journalled");
    assert!(store.open_attempts(run).expect("open attempts").is_empty());
}

// ---------------------------------------------------------------- the crash matrix

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    resume_with, resume_with_recovery, run_with, ApproveAll, Policy, Provider, RecoveryDecision,
    RunOutcome, TaskContract, ToolSpec, Verification,
};
use serde_json::json;

/// Where the process dies, relative to the one call the harness cannot inspect.
///
/// Death is a parked future the run is then dropped on, which is the honest
/// in-process kill: the step is left uncommitted and the run row stays `running`,
/// exactly as a killed process leaves them.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kill {
    /// (a) Before anything is journalled — the completion that would have asked
    /// for the call never returns, so nothing was started and nothing recorded.
    BeforeJournal,
    /// (b) After the journal row exists and before the effect happens.
    BeforeEffect,
    /// (c) After the effect happened and before the completion row is written.
    AfterEffect,
    /// (d) After the call completed, on the step that follows it.
    AfterCompletion,
}

/// A tool with an effect the harness cannot see, which counts the times it has
/// actually performed it.
///
/// The counter is the whole assertion: "was this call made twice" is a
/// measurement here rather than an argument about what a resume ought to do.
struct Charge {
    calls: Arc<AtomicUsize>,
    kill: Kill,
    read_only: bool,
}

impl Tool for Charge {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "charge".into(),
            description: "Charge the customer.".into(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            if self.kill == Kill::BeforeEffect {
                std::future::pending::<()>().await;
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            if self.kill == Kill::AfterEffect {
                std::future::pending::<()>().await;
            }
            Ok("charged".to_string())
        })
    }

    fn effect(&self) -> ToolEffect {
        if self.read_only {
            ToolEffect::ReadOnly
        } else {
            ToolEffect::Mutating
        }
    }
}

/// Asks for the charge, then finishes the work once the charge has been observed.
///
/// Stateless in the same sense the tree fixtures are: which reply it gives is
/// decided by what is in the request, so one provider serves the run and the
/// resume. `park_first` is case (a) — the completion that would have asked for
/// the call never returns.
/// Drive `run` until `ready` answers, then stop driving it — dropping it where it
/// stands (0.76.0).
///
/// The same shape `tests/checkpoint.rs` uses, and it is duplicated rather than
/// shared because Cargo compiles each file under `tests/` as its own crate: the
/// alternative is a `#[path]` module, which is more machinery than twenty lines.
///
/// The bound is a **liveness** bound, never a budget. A slow host takes longer
/// and still reaches the condition; a run that cannot reach it fails and says so.
/// `Ok(None)` is the intended outcome — the run was cut off. `Ok(Some(_))` means
/// it finished, which is a different failure and is reported as one.
async fn cut_off_when<F: std::future::Future>(
    run: F,
    mut ready: impl FnMut() -> bool,
) -> Result<Option<F::Output>, tokio::time::error::Elapsed> {
    const POLL: std::time::Duration = std::time::Duration::from_millis(5);
    const GIVE_UP: std::time::Duration = std::time::Duration::from_secs(30);
    let started = std::time::Instant::now();
    let mut run = Box::pin(run);
    loop {
        tokio::select! {
            done = &mut run => return Ok(Some(done)),
            _ = tokio::time::sleep(POLL) => {
                if ready() {
                    return Ok(None);
                }
                if started.elapsed() > GIVE_UP {
                    // The only error this returns, and it means the condition was
                    // never reached — not that a deadline for reaching it expired.
                    return tokio::time::timeout(std::time::Duration::ZERO, std::future::pending::<()>())
                        .await
                        .map(|_| None);
                }
            }
        }
    }
}

struct Cashier {
    park_first: bool,
    park_after_charge: bool,
    /// How many completions this provider has been asked for (0.76.0).
    ///
    /// The observable the cut-off waits on for `Kill::BeforeJournal`, which is
    /// the one arm with no durable row to poll for: the run parks *before* it
    /// journals anything, so "has the run got far enough to be cut off" cannot
    /// be answered from the store. Being asked for a completion can.
    entered: Arc<AtomicUsize>,
}

impl Provider for Cashier {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        if req.user.contains("[charge]") {
            if self.park_after_charge {
                std::future::pending::<()>().await;
            }
            return Ok(CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "done.txt", "content": "ok" }),
                }],
                ..Default::default()
            });
        }
        if self.park_first {
            std::future::pending::<()>().await;
        }
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "charge".into(),
                arguments: json!({ "amount": 1200 }),
            }],
            ..Default::default()
        })
    }
}

fn contract(
    root: &std::path::Path,
    calls: &Arc<AtomicUsize>,
    kill: Kill,
    read_only: bool,
) -> TaskContract {
    TaskContract::workspace("charge the customer, then record it", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(6)
        .with_tools(Toolbox::new().with(Charge {
            calls: Arc::clone(calls),
            kill,
            read_only,
        }))
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Drive a run until the process "dies", and hand back the store path, the run id
/// and how many times the effect actually happened.
async fn crashed(
    dir: &std::path::Path,
    kill: Kill,
    read_only: bool,
) -> (std::path::PathBuf, i64, Arc<AtomicUsize>) {
    let db = dir.join("runs.db");
    let calls = Arc::new(AtomicUsize::new(0));
    let store = Store::open(&db).unwrap();
    let contract = contract(dir, &calls, kill, read_only);
    let entered = Arc::new(AtomicUsize::new(0));
    let provider = Cashier {
        park_first: kill == Kill::BeforeJournal,
        park_after_charge: kill == Kill::AfterCompletion,
        entered: Arc::clone(&entered),
    };

    // 0.76.0 — the cut-off was a 400ms budget, and 400ms had to cover opening
    // SQLite, the startup boundary probe's child spawns, composing a prompt,
    // answering it and journalling the attempt. On a loaded Windows runner it did
    // not, and the run then parked in the probe rather than at the kill point —
    // so the timeout still elapsed, `cut_off.is_err()` still passed, and the
    // failure surfaced far away as an index panic on an empty attempts list. That
    // is the sharpest case in this repository of slow and broken reporting the
    // same way, and it is issue #232's `recovery.rs` site.
    //
    // The cut-off is now the state itself. `BeforeJournal` parks before anything
    // durable exists, so its observable is the provider having been asked at all;
    // every other arm journals an attempt, which is a row. The ceiling is a
    // liveness bound rather than the thing being measured.
    // Each arm parks somewhere different, so each has a different observable.
    // `BeforeJournal` parks inside its first completion, before anything durable
    // exists. A read-only tool is journalled nowhere on purpose — that is the
    // claim its own test makes — so there is never a row to wait for, and the
    // second completion being asked for is what says the tool has run.
    // `AfterCompletion` parks inside the charge's completion, which is also the
    // second. Everything else journals an attempt, which is a row.
    let reached = || {
        let asked = entered.load(Ordering::SeqCst);
        match kill {
            Kill::BeforeJournal => asked > 0,
            Kill::AfterCompletion => asked >= 2,
            // The read-only arm's own counter is the observable: its tool leaves
            // no row anywhere, which is the claim, so "the tool has run" cannot
            // be read from the store and the provider is asked only once.
            _ if read_only => calls.load(Ordering::SeqCst) > 0,
            _ => store
                .open_attempts(1)
                .map(|a| !a.is_empty())
                .unwrap_or(false),
        }
    };
    let cut_off = cut_off_when(
        run_with(&contract, &provider, &store, &open_policy(), &ApproveAll),
        reached,
    )
    .await;
    let cut_off = cut_off.unwrap_or_else(|_| {
        panic!(
            "the run never reached its kill point within the liveness bound: the provider was \
             asked {} time(s) and the store holds {} open attempt(s). That is a run that does \
             not get there, not a slow machine",
            entered.load(Ordering::SeqCst),
            store.open_attempts(1).map(|a| a.len()).unwrap_or(0)
        )
    });
    assert!(
        cut_off.is_none(),
        "the run finished instead of being cut off mid-flight: {cut_off:?}"
    );
    drop(store);
    (db, 1, calls)
}

/// F1(a) — killed before anything was journalled, the call replays.
///
/// The window this release does not close, asserted rather than glossed: between
/// deciding to make a call and the journal row committing there is nothing on
/// disk, and no journal can close that gap because the write and the call are not
/// one atomic act. What the release narrows it to is the width of one committed
/// `INSERT`.
#[tokio::test]
async fn killed_before_the_journal_the_call_is_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, calls) = crashed(dir.path(), Kill::BeforeJournal, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "nothing was charged");

    let store = Store::open(&db).unwrap();
    assert!(
        store.open_attempts(run).unwrap().is_empty(),
        "nothing was started, so nothing is open"
    );

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !matches!(result.outcome, RunOutcome::AwaitingRecovery { .. }),
        "there is nothing to decide about: {:?}",
        result.outcome
    );
    assert_eq!(fresh.load(Ordering::SeqCst), 1, "the call is made, once");
}

/// F1(b) — killed after the journal row and before the effect, the run pauses and
/// the call is not repeated.
#[tokio::test]
async fn killed_after_the_journal_and_before_the_effect_the_run_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, calls) = crashed(dir.path(), Kill::BeforeEffect, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0, "the effect never happened");

    let store = Store::open(&db).unwrap();
    let open = store.open_attempts(run).unwrap();
    assert_eq!(open.len(), 1, "the attempt is open");
    assert_eq!(open[0].tool, "charge");

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        result.outcome,
        RunOutcome::AwaitingRecovery {
            attempt_id: open[0].id,
            steps: 0
        }
    );
    assert_eq!(fresh.load(Ordering::SeqCst), 0, "nothing was called again");
}

/// F1(c) — killed after the effect happened and before it was recorded, the run
/// pauses and the effect is not repeated.
///
/// The case the release exists for: the charge landed, nothing durable says the
/// call finished, and 0.64.0 would have made it a second time.
#[tokio::test]
async fn killed_after_the_effect_and_before_the_completion_the_run_pauses() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, calls) = crashed(dir.path(), Kill::AfterEffect, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the effect happened once");

    let store = Store::open(&db).unwrap();
    let open = store.open_attempts(run).unwrap();
    assert_eq!(open.len(), 1, "the attempt is still open");

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::AwaitingRecovery { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(
        fresh.load(Ordering::SeqCst),
        0,
        "the charge was not made twice"
    );
}

/// F1(d) — killed after the call completed, the run resumes without a pause and
/// without repeating it.
#[tokio::test]
async fn killed_after_the_call_completed_the_run_resumes_without_pausing() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, calls) = crashed(dir.path(), Kill::AfterCompletion, false).await;
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the call completed");

    let store = Store::open(&db).unwrap();
    assert!(
        store.open_attempts(run).unwrap().is_empty(),
        "a completed call leaves nothing to decide"
    );

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !matches!(result.outcome, RunOutcome::AwaitingRecovery { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(
        fresh.load(Ordering::SeqCst),
        0,
        "the completed call is not repeated"
    );
}

/// F2 — a tool that declares itself read-only is journalled nowhere and pauses
/// nothing, at the same kill point that pauses a mutating one.
///
/// Asserted on the table as well as on the outcome: "no pause happened" and "no
/// row was written" are two claims, and a journal that recorded everything and
/// then chose not to pause would satisfy only the first.
#[tokio::test]
async fn a_read_only_tool_is_journalled_nowhere_and_pauses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, calls) = crashed(dir.path(), Kill::AfterEffect, true).await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the same kill point as F1(c)"
    );

    let store = Store::open(&db).unwrap();
    assert!(
        store.open_attempts(run).unwrap().is_empty(),
        "a replayable call writes no row at all"
    );

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, true);
    let result = resume_with(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !matches!(result.outcome, RunOutcome::AwaitingRecovery { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(
        fresh.load(Ordering::SeqCst),
        1,
        "it replays exactly as it did on 0.64.0"
    );
}

/// Drive to the pause of F1(c), and hand back what a decision needs.
async fn paused(dir: &std::path::Path) -> (std::path::PathBuf, i64, i64) {
    let (db, run, _) = crashed(dir, Kill::AfterEffect, false).await;
    let store = Store::open(&db).unwrap();
    let attempt = store.open_attempts(run).unwrap()[0].id;
    (db, run, attempt)
}

/// F4 — `Retry` makes the call again, exactly once.
#[tokio::test]
async fn retry_makes_the_call_again() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, attempt) = paused(dir.path()).await;
    let store = Store::open(&db).unwrap();

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with_recovery(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        attempt,
        RecoveryDecision::Retry,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(!matches!(
        result.outcome,
        RunOutcome::AwaitingRecovery { .. }
    ));
    assert_eq!(fresh.load(Ordering::SeqCst), 1, "made again, once");
    assert!(store.open_attempts(run).unwrap().is_empty());
}

/// F4 — `Completed` does not call the tool, and what the operator says the call
/// returned reaches the model.
///
/// Read back from the store's own observations rather than from anything the
/// model replied: what a run was *given* is a durable fact, and what a model says
/// about it is model behaviour.
#[tokio::test]
async fn completed_does_not_call_the_tool_and_the_operators_account_reaches_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, attempt) = paused(dir.path()).await;
    let store = Store::open(&db).unwrap();

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with_recovery(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        attempt,
        RecoveryDecision::Completed {
            observation: "charge ch_9f21 captured".into(),
        },
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(!matches!(
        result.outcome,
        RunOutcome::AwaitingRecovery { .. }
    ));
    assert_eq!(fresh.load(Ordering::SeqCst), 0, "the tool was not called");

    let observations = store.observations(run).unwrap();
    assert!(
        observations.iter().any(|o| o.text.contains("ch_9f21")),
        "the operator's account is in the run's ledger: {observations:?}"
    );
    assert!(store.open_attempts(run).unwrap().is_empty());
}

/// F4 — `Abort` ends the run without making the call.
#[tokio::test]
async fn abort_ends_the_run_without_making_the_call() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, attempt) = paused(dir.path()).await;
    let store = Store::open(&db).unwrap();

    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let result = resume_with_recovery(
        &contract,
        &Cashier {
            park_first: false,
            park_after_charge: false,
            entered: Arc::new(AtomicUsize::new(0)),
        },
        &store,
        run,
        attempt,
        RecoveryDecision::Abort,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Denied { .. }),
        "got {:?}",
        result.outcome
    );
    assert_eq!(fresh.load(Ordering::SeqCst), 0, "nothing was called");
    assert!(store.open_attempts(run).unwrap().is_empty());
}

/// A decision naming an attempt that is not open is refused rather than applied.
///
/// The control for the three above: without it, a second `Completed` on one
/// attempt would write a second observation of one call, and the caller would
/// believe they had authorised something.
#[tokio::test]
async fn a_decision_about_an_attempt_that_is_not_open_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (db, run, attempt) = paused(dir.path()).await;
    let store = Store::open(&db).unwrap();
    let fresh = Arc::new(AtomicUsize::new(0));
    let contract = contract(dir.path(), &fresh, Kill::AfterCompletion, false);
    let provider = Cashier {
        park_first: false,
        park_after_charge: false,
        entered: Arc::new(AtomicUsize::new(0)),
    };

    resume_with_recovery(
        &contract,
        &provider,
        &store,
        run,
        attempt,
        RecoveryDecision::Retry,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let again = resume_with_recovery(
        &contract,
        &provider,
        &store,
        run,
        attempt,
        RecoveryDecision::Retry,
        &open_policy(),
        &ApproveAll,
    )
    .await;
    assert!(again.is_err(), "an attempt can only be decided once");
}
