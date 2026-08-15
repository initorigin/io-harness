//! 0.58.0 — retention. What a store is holding, and what an operator can decide
//! to keep.
//!
//! Every test here builds a real file on disk rather than
//! [`Store::memory`](io_harness::Store::memory), because F12 is about a file's
//! size on a filesystem and an in-memory database has no pages to reclaim. The
//! rest use a file for the same reason the release does: the instrument is for
//! stores an operator actually has.

use io_harness::{RunStatus, StepRecord, Store};
use rusqlite::Connection;

/// A store on disk, and the directory that keeps it alive for the test.
fn on_disk() -> (tempfile::TempDir, Store, std::path::PathBuf) {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let path = dir.path().join("store.db");
    let store = Store::open(&path).expect("a store");
    (dir, store, path)
}

/// One session with `turns` turns, each driving one run of `steps` steps, with
/// something written into every table this release has to account for. Returns
/// the session id.
fn seed_session(store: &Store, root: &str, turns: usize, steps: usize, needle: &str) -> i64 {
    let session = store.create_session(root).expect("a session");
    let mut parent_turn = None;
    for turn in 0..turns {
        let run = store
            .start_run(&format!("{needle} goal {turn}"), root)
            .expect("a run");
        let turn_id = store
            .record_turn(
                session,
                parent_turn,
                run,
                &format!("{needle} prompt {turn}"),
            )
            .expect("a turn");
        store
            .finish_turn(turn_id, Some(&format!("{needle} reply {turn}")), "ok")
            .expect("a finished turn");
        store
            .set_session_head(session, Some(turn_id))
            .expect("head");
        parent_turn = Some(turn_id);
        for step in 1..=steps {
            let record = StepRecord::new(
                step as u32,
                format!("{needle} decision {step}"),
                format!("{needle} result {step}"),
            )
            .with_trace(
                format!("{needle} prompt for step {step}"),
                format!("{needle} tool call {step}"),
                12,
            );
            store.record(run, &record).expect("a step");
        }
    }
    session
}

/// Every table the store creates, read from the schema rather than from a list
/// this test would have to be remembered to update.
fn tables(path: &std::path::Path) -> Vec<String> {
    let conn = Connection::open(path).expect("the store, read directly");
    let mut stmt = conn
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite_%'")
        .expect("the schema");
    let names = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .expect("the table names")
        .collect::<Result<Vec<_>, _>>()
        .expect("the table names");
    assert!(
        names.len() >= 30,
        "the schema should have grown, not shrunk: {names:?}"
    );
    names
}

/// F13. The size call is exact about what it counts — asserted against a sum
/// computed here, from the schema, rather than against the number the crate
/// produced.
#[test]
fn the_size_of_a_session_is_the_bytes_of_its_own_rows() {
    let (_dir, store, path) = on_disk();
    let session = seed_session(&store, "/repo", 3, 4, "alpha");
    seed_session(&store, "/other", 2, 2, "beta");

    let size = store
        .session_size(session)
        .expect("a size")
        .expect("the session exists");

    // The independent sum: every text or blob column of every table, restricted
    // to the rows belonging to this session's runs, added up here.
    let conn = Connection::open(&path).expect("the store, read directly");
    let runs: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT run_id FROM session_turns WHERE session_id = ?1")
            .expect("the turns");
        stmt.query_map([session], |r| r.get(0))
            .expect("the run ids")
            .collect::<Result<Vec<_>, _>>()
            .expect("the run ids")
    };
    assert_eq!(runs.len(), 3, "three turns, three runs");

    let ids = runs
        .iter()
        .map(|r| r.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let mut expected_bytes: i64 = 0;
    let mut expected_rows: i64 = 0;
    for table in tables(&path) {
        let key = match table.as_str() {
            "sessions" | "memory" | "memory_recalls" => continue,
            "runs" => "id",
            "session_turns" => "run_id",
            "spawns" | "agent_queue" => "parent_run_id",
            _ => "run_id",
        };
        let mut info = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("the columns");
        let text_cols: Vec<String> = info
            .query_map([], |r| Ok((r.get::<_, String>(1)?, r.get::<_, String>(2)?)))
            .expect("the columns")
            .filter_map(|c| {
                let (name, ty) = c.ok()?;
                let ty = ty.to_uppercase();
                (ty.contains("TEXT") || ty.contains("BLOB")).then_some(name)
            })
            .collect();
        if text_cols.is_empty() {
            continue;
        }
        let sum = text_cols
            .iter()
            .map(|c| format!("COALESCE(SUM(LENGTH({c})), 0)"))
            .collect::<Vec<_>>()
            .join(" + ");
        let (bytes, rows): (i64, i64) = conn
            .query_row(
                &format!("SELECT {sum}, COUNT(*) FROM {table} WHERE {key} IN ({ids})"),
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap_or((0, 0));
        expected_bytes += bytes;
        expected_rows += rows;
    }

    assert!(expected_bytes > 0, "the fixture wrote something");
    assert_eq!(
        size.bytes, expected_bytes as u64,
        "the reported bytes are the bytes of the session's own rows"
    );
    assert_eq!(size.rows, expected_rows as u64, "and so are the rows");
    assert_eq!(size.turns, 3);
    assert_eq!(size.runs, 3);
}

/// F13, second half. The store's own figure is the file's real page arithmetic,
/// not a sum of anything.
#[test]
fn the_size_of_a_store_is_the_files_own_page_arithmetic() {
    let (_dir, store, path) = on_disk();
    seed_session(&store, "/repo", 4, 8, "alpha");
    seed_session(&store, "/other", 2, 2, "beta");

    let size = store.store_size().expect("a store size");

    let conn = Connection::open(&path).expect("the store, read directly");
    let page_size: u64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .expect("a page size");
    let page_count: u64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .expect("a page count");
    let freelist: u64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .expect("a freelist count");

    assert_eq!(size.file_bytes, page_size * page_count);
    assert_eq!(size.free_bytes, page_size * freelist);
    assert_eq!(size.sessions, 2);
    assert_eq!(size.runs, 6);
    assert!(
        !size.tables.is_empty(),
        "dbstat answers the per-table breakdown"
    );
    let named: u64 = size.tables.iter().map(|(_, bytes)| bytes).sum();
    assert!(
        named > 0 && named <= size.file_bytes,
        "the breakdown fits inside the file: {named} of {}",
        size.file_bytes
    );
    assert!(
        size.tables.windows(2).all(|w| w[0].1 >= w[1].1),
        "the breakdown is largest first, so the answer to \"what is this holding\" is the first line"
    );
}

/// F14. Asking the size of nothing has no answer.
#[test]
fn the_size_of_a_session_that_is_not_there_is_none() {
    let (_dir, store, _path) = on_disk();
    seed_session(&store, "/repo", 1, 1, "alpha");

    assert!(
        store.session_size(9_999).expect("no error").is_none(),
        "an unknown session has no size, rather than a size of zero"
    );
}

/// The status column is what the sweep's refusal reads, so the fixture has to be
/// able to produce every value of it. Asserted here so a later change to
/// `RunStatus` fails this file rather than silently weakening F6.
#[test]
fn a_seeded_run_can_be_put_into_every_status_the_sweep_reads() {
    let (_dir, store, _path) = on_disk();
    let run = store.start_run("a goal", "/repo").expect("a run");
    for (text, status) in [
        ("running", RunStatus::Running),
        ("paused", RunStatus::Paused),
        ("completed", RunStatus::Completed),
        ("failed", RunStatus::Failed),
    ] {
        store.set_status(run, text).expect("a status");
        assert_eq!(store.run_status(run).expect("readable"), Some(status));
    }
}
