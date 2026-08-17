//! 0.62.0 — one driver per run. Who holds a run, what happens to a driver that
//! has lost it, and what a session head does when two turns race for it.
//!
//! **Nothing here sleeps, and nothing here asserts an elapsed duration.** The
//! feature is a clock, which makes it the easiest thing in this repository to
//! write a flaky test for — ten recorded instances, one of which cost a Release.
//! Every expiry assertion is made under an *injected* ttl: `0` for a lease that is
//! already lapsed, [`LIVE`] for one that cannot lapse while a test runs. A test
//! that cannot sleep cannot flake on sleeping.
//!
//! `ttl_secs: 0` is also the exact boundary pair the release owes. Expiry is
//! `now - renewed_at >= ttl_secs`, so a zero ttl is the one case where the elapsed
//! time lands *on* the threshold rather than past it: under `>=` it is lapsed,
//! under `>` it is not. 0.57.0's F13 claimed a boundary its three cases never
//! touched and `>` in place of `>=` survived all of them — so the boundary is a
//! case here, not a sentence.
//!
//! Two `Store` handles over one file are two owners, which is what lets the
//! conflict cases be written in one process. Two OS processes over one SQLite file
//! is the `DatabaseBusy` shape that has failed `release.yml` itself here.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume, run, Error, Provider, RunOutcome, StepRecord, Store, TaskContract, Verification,
};
use serde_json::json;

/// A provider that writes the scripted content for each step, in order, and past
/// the end writes something that does not satisfy the contract — so a run only
/// finishes on a step the script finishes deliberately. The same shape
/// `tests/checkpoint.rs` uses, for the same reason: nothing here reaches a network
/// and every run is replayable.
struct Script {
    writes: Vec<String>,
    at: AtomicUsize,
}
impl Script {
    fn new(writes: &[&str]) -> Self {
        Self {
            writes: writes.iter().map(|w| (*w).to_string()).collect(),
            at: AtomicUsize::new(0),
        }
    }
}
impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        let content = self
            .writes
            .get(i)
            .cloned()
            .unwrap_or_else(|| "WORKING\n".into());
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": content }),
            }],
            usage: Some(Usage {
                total_tokens: 1,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// A ttl no test can outlive. Never `1`: a one-second ttl is a race with the
/// second hand, which is the flake this whole file is written to avoid.
const LIVE: i64 = 3_600;

/// Two stores over one file, and the directory that keeps it alive.
fn two_drivers() -> (tempfile::TempDir, Store, Store) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("store.db");
    let first = Store::open(&path).expect("a store");
    let second = Store::open(&path).expect("a second store over the same file");
    assert_ne!(
        first.owner(),
        second.owner(),
        "two handles must be two owners, or every conflict test passes by taking over its own lease"
    );
    (dir, first, second)
}

/// **F3** — the four cells of (lapsed / live) × (same owner / another owner),
/// including the pair that lands exactly on the ttl.
#[test]
fn an_expired_lease_is_taken_over_and_a_live_one_is_not() {
    let (_dir, first, second) = two_drivers();
    let run = first.start_run("port it", "openrouter").expect("a run");

    // Cell 1 — same owner, live lease: a re-acquire renews and the generation does
    // NOT move. A driver reconnecting to its own run must not invalidate the steps
    // it is in the middle of committing.
    let held = first.acquire_lease(run, LIVE).expect("the first acquire");
    assert_eq!(held.generation(), 1);
    let again = first.acquire_lease(run, LIVE).expect("a re-acquire");
    assert_eq!(
        again.generation(),
        1,
        "re-acquiring one's own live lease is a renewal, not a takeover"
    );

    // Cell 2 — another owner, live lease: refused, and told by whom and until when.
    match second.acquire_lease(run, LIVE) {
        Err(Error::Conflict {
            run_id,
            owner,
            expires_at,
        }) => {
            assert_eq!(run_id, run);
            assert_eq!(owner, first.owner(), "the conflict names the actual holder");
            assert!(
                !expires_at.is_empty(),
                "a caller deciding between backing off and waiting needs the expiry"
            );
        }
        other => panic!("a live lease must refuse another owner, got {other:?}"),
    }

    // Cell 3 — same owner, lapsed lease: still this owner's, and still generation 1.
    // Ownership is checked before expiry, so a slow driver does not take its own
    // run over from itself and invalidate its own uncommitted work.
    drop(again);
    drop(held);
    let zero = first.acquire_lease(run, 0).expect("a zero-ttl acquire");
    assert_eq!(zero.generation(), 1);
    let renewed = first
        .acquire_lease(run, 0)
        .expect("an owner re-acquiring its own lapsed lease");
    assert_eq!(
        renewed.generation(),
        1,
        "expiry does not take a run away from the owner that already holds it"
    );

    // Cell 4 — another owner, lapsed lease: THE BOUNDARY PAIR. The row's ttl is 0
    // and no time has to pass for `now - renewed_at >= 0` to hold, so this case
    // lands exactly on the threshold. It passes under `>=` and fails under `>`.
    let taken = second
        .acquire_lease(run, LIVE)
        .expect("a lapsed lease is taken over");
    assert_eq!(
        taken.generation(),
        2,
        "a takeover moves the generation by exactly one"
    );
    assert_eq!(
        first.run_lease(run).expect("a lease read").expect("a row").owner,
        second.owner(),
        "the row names the new holder"
    );

    // And the loser's renew is refused rather than quietly landing: renewing is not
    // a way back into a run somebody else now holds.
    match renewed.renew() {
        Err(Error::Conflict { run_id, owner, .. }) => {
            assert_eq!(run_id, run);
            assert_eq!(owner, second.owner());
        }
        other => panic!("a superseded owner must not renew, got {other:?}"),
    }
}

/// **F5** — a run whose owner died is resumable. A lease that turns a crash into a
/// permanent lock has traded a silent corruption for an outage, which is not an
/// improvement.
///
/// This is the ttl half: the owner here is *this* process, which is alive, so the
/// liveness check cannot be what allows the takeover and expiry has to be. The
/// liveness half — an owner that no longer exists, taken over with a ttl that
/// cannot lapse — is `a_lease_whose_owner_no_longer_exists_is_taken_over_without_waiting`
/// in `src/state/leases.rs`, where writing a row for a foreign pid needs the
/// private connection.
///
/// The crash is `std::mem::forget` on the guard: a killed process runs no
/// destructor, so neither does this test. Nothing else in the file may do that —
/// it is the one place where skipping the release is the behaviour under test.
#[test]
fn a_run_whose_owner_died_without_releasing_is_taken_over_once_the_lease_lapses() {
    let (_dir, first, second) = two_drivers();
    let run = first.start_run("port it", "openrouter").expect("a run");

    let abandoned = first.acquire_lease(run, 0).expect("the first acquire");
    assert_eq!(abandoned.generation(), 1);
    std::mem::forget(abandoned); // the process died here.

    let row = first
        .run_lease(run)
        .expect("a lease read")
        .expect("the dead owner's row is still there");
    assert_eq!(row.owner, first.owner());
    assert!(row.expired, "a zero ttl is lapsed the moment it is written");

    let taken = second
        .acquire_lease(run, LIVE)
        .expect("an abandoned run is recoverable");
    assert_eq!(taken.generation(), 2);
    assert!(
        !second
            .run_lease(run)
            .expect("a lease read")
            .expect("a row")
            .expired,
        "the new holder's lease is live"
    );
}

/// A released lease leaves no row at all, and an expired one leaves a row. That is
/// the whole difference between the two states, and both the takeover path and the
/// F6 control in `tests/durable.rs` rest on it.
#[test]
fn a_released_lease_leaves_no_row_and_an_expired_one_leaves_one() {
    let (_dir, first, second) = two_drivers();
    let run = first.start_run("port it", "openrouter").expect("a run");

    let held = first.acquire_lease(run, LIVE).expect("an acquire");
    held.release().expect("a release");
    assert!(
        first.run_lease(run).expect("a lease read").is_none(),
        "a released lease is deleted, not merely marked"
    );

    // And a released run is *acquired* by the next driver rather than taken over,
    // so the generation starts again rather than counting handovers that did not
    // happen.
    let next = second.acquire_lease(run, LIVE).expect("a fresh acquire");
    assert_eq!(next.generation(), 1);
}

/// Dropping the guard releases the lease, which is what keeps the run loop's
/// thirty-four entry points free of a release call on every exit path.
#[test]
fn dropping_the_guard_releases_the_lease() {
    let (_dir, first, second) = two_drivers();
    let run = first.start_run("port it", "openrouter").expect("a run");
    {
        let _held = first.acquire_lease(run, LIVE).expect("an acquire");
        assert!(second.acquire_lease(run, LIVE).is_err(), "held while in scope");
    }
    assert!(
        first.run_lease(run).expect("a lease read").is_none(),
        "the guard released on the way out of scope"
    );
    second
        .acquire_lease(run, LIVE)
        .expect("free once the guard is gone");
}

/// **F1** — a second live driver is refused with a typed conflict naming the
/// holder, and it is refused *before* it drives anything.
///
/// On today's tree this test fails: the second `resume` drives the run and the two
/// processes interleave their steps into one trace. That failure is the evidence
/// the release changed something, which is why the test is written this way round
/// rather than asserting on the fixed behaviour alone.
#[tokio::test]
async fn a_second_driver_is_refused_while_the_first_holds_the_run() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("store.db");
    let file = dir.path().join("out.txt");
    let first = Store::open(&path).expect("a store");
    let second = Store::open(&path).expect("a second driver over the same file");

    // A run that stops short of its verification, so it is genuinely resumable
    // rather than finished — a finished run is a read, not a resume.
    let contract = TaskContract::new(
        "write",
        &file,
        Verification::FileEquals("SOLUTION\n".into()),
    );
    let stopped = run(
        &contract.clone().with_max_steps(1),
        &Script::new(&["WORKING\n"]),
        &first,
    )
    .await
    .expect("a run that stopped short");
    let run_id = stopped.run_id;

    // The first driver takes the run back and holds it for the length of this test.
    let held = first
        .acquire_lease(run_id, LIVE)
        .expect("the first driver takes the run");

    match resume(&contract, &Script::new(&["SOLUTION\n"]), &second, run_id).await {
        Err(Error::Conflict {
            run_id: conflicted,
            owner,
            expires_at,
        }) => {
            assert_eq!(conflicted, run_id);
            assert_eq!(owner, first.owner(), "the refusal names the actual holder");
            assert!(!expires_at.is_empty());
        }
        other => panic!("a held run must refuse a second driver, got {other:?}"),
    }

    // And it was refused before driving: the second process left no step behind.
    let steps_after = second.steps(run_id).expect("the steps").len();
    assert_eq!(
        steps_after,
        first.steps(run_id).expect("the steps").len(),
        "a refused driver commits nothing"
    );

    // Once the holder is gone the same resume goes through, so the refusal is
    // ownership and not an unresumable run.
    held.release().expect("a release");
    let finished = resume(&contract, &Script::new(&["SOLUTION\n"]), &second, run_id)
        .await
        .expect("a released run is resumable");
    assert!(matches!(finished.outcome, RunOutcome::Success { .. }));
}

/// **F2** — a driver whose generation has moved commits nothing at all.
///
/// Asserted on the store rather than on the returned error: the `steps` row and
/// the checkpoint event are both absent, because the generation is verified inside
/// the transaction that writes them. A check made *before* that transaction would
/// return the same error and leave the same window open.
#[test]
fn a_step_from_a_superseded_driver_lands_nowhere() {
    let (_dir, first, second) = two_drivers();
    let run = first.start_run("port it", "openrouter").expect("a run");

    // The first driver holds the run under a lapsed ttl and commits one step
    // normally, which is the control: the same call succeeds while it is the owner.
    let _held = first.acquire_lease(run, 0).expect("an acquire");
    first
        .checkpoint_step(run, &StepRecord::new(1, "wrote", "ok"))
        .expect("its own step");
    let committed = first.steps(run).expect("the steps").len();
    let events = first.checkpoint_events(run).expect("the events").len();

    // The run is taken over. The first driver still believes it holds it.
    let taken = second.acquire_lease(run, LIVE).expect("a takeover");
    assert_eq!(taken.generation(), 2);

    match first.checkpoint_step(run, &StepRecord::new(2, "wrote", "ok")) {
        Err(Error::Conflict { run_id, owner, .. }) => {
            assert_eq!(run_id, run);
            assert_eq!(owner, second.owner());
        }
        other => panic!("a superseded driver must not commit, got {other:?}"),
    }
    assert_eq!(
        first.steps(run).expect("the steps").len(),
        committed,
        "the refused step left no trace row"
    );
    assert_eq!(
        first.checkpoint_events(run).expect("the events").len(),
        events,
        "and no checkpoint event either — the check is inside the transaction"
    );
}

/// **F6** — a single-process run is unchanged: it acquires, commits, and leaves
/// the lease *released* rather than merely expired.
///
/// The distinction is the point. A run that ended and released has no row at all,
/// so the next driver acquires at generation 1 rather than taking over something
/// nobody holds.
#[tokio::test]
async fn a_single_process_run_never_conflicts_and_releases_what_it_took() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let file = dir.path().join("out.txt");
    let store = Store::open(dir.path().join("store.db")).expect("a store");
    let contract = TaskContract::new(
        "write",
        &file,
        Verification::FileEquals("SOLUTION\n".into()),
    );

    let first = run(&contract, &Script::new(&["SOLUTION\n"]), &store)
        .await
        .expect("an ordinary run");
    assert!(matches!(first.outcome, RunOutcome::Success { .. }));
    assert!(
        store
            .run_lease(first.run_id)
            .expect("a lease read")
            .is_none(),
        "a finished run released its lease, it did not leave it to expire"
    );

    // And resume on the same store, in the same process, is refused by nothing.
    let again = resume(&contract, &Script::new(&["SOLUTION\n"]), &store, first.run_id)
        .await
        .expect("resume in the same process is not a conflict");
    assert!(matches!(again.outcome, RunOutcome::Success { .. }));
    assert!(store
        .run_lease(first.run_id)
        .expect("a lease read")
        .is_none());
}

/// **F4** — a lost session-head update is a returned conflict, and the losing turn
/// is left intact.
///
/// This release makes a dropped turn *reported*; it does not make both turns land.
/// The answer the loser produced was paid for, so it stays in `session_turns` with
/// its parent unchanged, and a caller can rebase onto the head that won.
#[test]
fn a_lost_session_head_update_is_reported_and_the_turn_survives() {
    let (_dir, store, _second) = two_drivers();
    let session = store.create_session("/repo").expect("a session");

    let first_run = store.start_run("one", "/repo").expect("a run");
    let first_turn = store
        .record_turn(session, None, first_run, "first")
        .expect("a turn");
    store
        .set_session_head_if(session, None, Some(first_turn))
        .expect("the first head write, against an empty head");

    // A second process took a turn on the same head and won.
    let racing_run = store.start_run("two", "/repo").expect("a run");
    let racing_turn = store
        .record_turn(session, Some(first_turn), racing_run, "two")
        .expect("a turn");
    store
        .set_session_head_if(session, Some(first_turn), Some(racing_turn))
        .expect("the winner");

    // The loser was working from the same head and is refused.
    let losing_run = store.start_run("three", "/repo").expect("a run");
    let losing_turn = store
        .record_turn(session, Some(first_turn), losing_run, "three")
        .expect("a turn");
    match store.set_session_head_if(session, Some(first_turn), Some(losing_turn)) {
        Err(Error::Conflict {
            run_id, owner, ..
        }) => {
            assert_eq!(run_id, session, "a head conflict names the session");
            assert!(
                owner.is_empty(),
                "a head has a value that moved, not a holder"
            );
        }
        other => panic!("a stale head write must be refused, got {other:?}"),
    }

    let head = store.session_head(session).expect("a session head");
    assert_eq!(head, Some(racing_turn), "the winner keeps the head");
    let lost = store
        .session_turn(losing_turn)
        .expect("a turn read")
        .expect("the losing turn is still there");
    assert_eq!(
        lost.parent_turn_id,
        Some(first_turn),
        "the losing turn is untouched — reported, not deleted"
    );
}

// N4's query plan is asserted in `src/state.rs`'s own `mod tests`, where the
// connection is reachable — `EXPLAIN QUERY PLAN` needs the private handle, which
// is why every other plan assertion in this crate lives there too.
