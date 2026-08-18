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
