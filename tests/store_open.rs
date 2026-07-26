//! `Store::open` must hand out a database a second reader can use.
//!
//! Before 0.12.0 `Store::open` was a bare `Connection::open`: it set neither
//! `journal_mode` nor `busy_timeout`, so anything wanting to watch a run in
//! flight had to open the file itself and set both — reaching around this API,
//! and having to win the race to do it before the harness opened the file.
//!
//! These tests are the contract that this no longer applies. They deliberately
//! open their second connection with NO pragmas of their own, because that is
//! the whole point: the store the crate hands out is already safe to read.

use io_harness::{StepRecord, Store, BUSY_TIMEOUT};

/// `Store::open` puts the file in WAL mode, and it sticks — WAL is a property of
/// the database, not of the connection that set it.
#[test]
fn a_store_opened_from_a_file_is_in_wal_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("run.sqlite3");

    let store = Store::open(&db).expect("open");
    let run = store.start_run("goal", "file.rs").expect("start_run");
    store
        .record(run, &StepRecord::new(1, "decision", "result"))
        .expect("record");

    // A plain connection, no pragmas set: it reports the mode the file is in.
    let reader = rusqlite::Connection::open(&db).expect("reader");
    let mode: String = reader
        .query_row("PRAGMA journal_mode", [], |r| r.get(0))
        .expect("journal_mode");
    assert_eq!(mode.to_lowercase(), "wal", "Store::open must leave WAL on");
}

/// The reason WAL matters: a second connection reads committed steps while the
/// store is still open and writing, and neither side blocks or aborts.
///
/// This is the polling shape the crate forced on every consumer before the
/// observer existed. It still has to work — a durable trace readable after the
/// fact is a promise the crate keeps — but it must work without the reader
/// configuring the file.
#[test]
fn a_second_reader_sees_committed_steps_while_the_writer_is_still_open() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = dir.path().join("run.sqlite3");

    let store = Store::open(&db).expect("open");
    let run = store.start_run("goal", "file.rs").expect("start_run");

    // No pragmas, and opened while the writer holds the file.
    let reader = rusqlite::Connection::open(&db).expect("reader");
    let count = |c: &rusqlite::Connection| -> i64 {
        c.query_row("SELECT COUNT(*) FROM steps WHERE run_id = ?1", [run], |r| {
            r.get(0)
        })
        .expect("count")
    };

    assert_eq!(count(&reader), 0, "nothing committed yet");

    for step in 1..=3 {
        store
            .record(run, &StepRecord::new(step, "decision", "result"))
            .expect("record");
        assert_eq!(
            count(&reader),
            i64::from(step),
            "step {step} must be visible to the reader as soon as it commits"
        );
    }

    // And the writer is still usable after all that reading — the reader did not
    // take a lock that outlived its statement.
    store.finish_run(run, "success").expect("finish_run");
    assert_eq!(
        store.outcome(run).expect("outcome").as_deref(),
        Some("success")
    );
}

/// An in-memory store is not shared with anything, so it needs no journal mode.
/// It must still open — the pragma work belongs to the file path only, and a
/// regression that made `Store::memory()` fail would break most of the suite.
#[test]
fn an_in_memory_store_still_opens() {
    let store = Store::memory().expect("memory");
    let run = store.start_run("goal", "file.rs").expect("start_run");
    store
        .record(run, &StepRecord::new(1, "decision", "result"))
        .expect("record");
    assert_eq!(store.last_step(run).expect("last_step"), 1);
}

/// The busy timeout is public so a caller can reason about how long a contended
/// read waits before it fails, rather than discovering it.
#[test]
fn the_busy_timeout_is_a_documented_non_zero_wait() {
    assert!(
        BUSY_TIMEOUT > std::time::Duration::ZERO,
        "a zero busy timeout is rusqlite's fail-immediately default, which is \
         what this constant exists to replace"
    );
}
