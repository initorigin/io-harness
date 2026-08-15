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
            // A note outlives the run that wrote it, so it is not part of the
            // session's size any more than it is part of its removal.
            "memory" => continue,
            "sessions" => "id",
            "runs" => "id",
            "session_turns" => "run_id",
            "spawns" | "agent_queue" => "parent_run_id",
            _ => "run_id",
        };
        // The session row is keyed by the session, not by a run.
        let ids = if table == "sessions" {
            session.to_string()
        } else {
            ids.clone()
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

/// Rows of `table` whose run is in `runs`, read directly rather than through the
/// crate — F1's whole point is that the check does not share the crate's own
/// idea of which tables exist.
fn rows_for(conn: &Connection, table: &str, key: &str, ids: &str) -> i64 {
    conn.query_row(
        &format!("SELECT COUNT(*) FROM {table} WHERE {key} IN ({ids})"),
        [],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// The run-keying column for a table, or `None` for the tables a session's
/// removal reaches by another route or deliberately leaves alone.
fn run_key(table: &str) -> Option<&'static str> {
    match table {
        "sessions" | "session_turns" => None,
        // A note outlives the run that wrote it. This is the one table whose
        // survival is the assertion.
        "memory" => None,
        "runs" => Some("id"),
        "spawns" | "agent_queue" => Some("parent_run_id"),
        _ => Some("run_id"),
    }
}

/// Every run in a session's tree, read directly.
fn tree_of(conn: &Connection, session: i64) -> Vec<i64> {
    let mut stmt = conn
        .prepare(
            "WITH RECURSIVE tree(id) AS (
                 SELECT run_id FROM session_turns WHERE session_id = ?1
                 UNION
                 SELECT r.id FROM runs r JOIN tree t ON r.parent_run_id = t.id
             )
             SELECT id FROM tree ORDER BY id",
        )
        .expect("the tree");
    stmt.query_map([session], |r| r.get(0))
        .expect("the run ids")
        .collect::<Result<Vec<_>, _>>()
        .expect("the run ids")
}

fn as_list(ids: &[i64]) -> String {
    ids.iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// F1. A deleted session leaves no row anywhere, and the check is driven from
/// the schema rather than from a list this test would have to be remembered to
/// update when the crate grows a table.
#[test]
fn a_deleted_session_leaves_no_row_in_any_table_the_schema_names() {
    let (_dir, store, path) = on_disk();
    let doomed = seed_session(&store, "/repo", 3, 4, "alpha");
    let kept_a = seed_session(&store, "/other", 2, 3, "beta");
    let kept_b = seed_session(&store, "/third", 1, 2, "gamma");

    let conn = Connection::open(&path).expect("the store, read directly");
    let doomed_runs = as_list(&tree_of(&conn, doomed));
    store
        .memory_put(
            "/repo",
            "a-note",
            "kept across the deletion",
            tree_of(&conn, doomed)[0],
            1,
        )
        .expect("a note");

    store.delete_session(doomed).expect("the deletion");

    for table in tables(&path) {
        let Some(key) = run_key(&table) else { continue };
        assert_eq!(
            rows_for(&conn, &table, key, &doomed_runs),
            0,
            "{table} still holds rows for the deleted session's runs"
        );
    }
    assert_eq!(
        rows_for(&conn, "session_turns", "session_id", &doomed.to_string()),
        0,
        "the turns went with the session"
    );
    assert_eq!(
        rows_for(&conn, "sessions", "id", &doomed.to_string()),
        0,
        "and so did the session row"
    );

    // The one table whose survival is the point: a note is a workspace asset
    // that outlives the run which wrote it.
    assert_eq!(
        rows_for(&conn, "memory", "workspace", "'/repo'"),
        1,
        "a note is not a session's property"
    );

    assert!(store.session_size(kept_a).expect("a size").is_some());
    assert!(store.session_size(kept_b).expect("a size").is_some());
}

/// F2. The sibling sessions are untouched, row for row.
#[test]
fn deleting_one_session_leaves_the_others_row_for_row() {
    let (_dir, store, path) = on_disk();
    let doomed = seed_session(&store, "/repo", 2, 3, "alpha");
    let kept = seed_session(&store, "/other", 3, 4, "beta");

    let before = store
        .session_size(kept)
        .expect("a size")
        .expect("the session exists");

    let conn = Connection::open(&path).expect("the store, read directly");
    let kept_runs = as_list(&tree_of(&conn, kept));
    let per_table: Vec<(String, i64)> = tables(&path)
        .into_iter()
        .filter_map(|t| run_key(&t).map(|k| (t.clone(), rows_for(&conn, &t, k, &kept_runs))))
        .collect();

    store.delete_session(doomed).expect("the deletion");

    for (table, rows) in per_table {
        let key = run_key(&table).expect("filtered above");
        assert_eq!(
            rows_for(&conn, &table, key, &kept_runs),
            rows,
            "{table} lost rows belonging to a session nobody deleted"
        );
    }
    assert_eq!(
        store.session_size(kept).expect("a size"),
        Some(before),
        "the surviving session is the same size it was"
    );
}

/// F3. A spawned child goes with the parent that spawned it, transitively.
#[test]
fn a_spawned_child_goes_with_the_parent_that_spawned_it() {
    let (_dir, store, path) = on_disk();
    let session = store.create_session("/repo").expect("a session");
    let root = store.start_run("the root goal", "/repo").expect("a run");
    let turn = store
        .record_turn(session, None, root, "do the thing")
        .expect("a turn");
    store.finish_turn(turn, Some("done"), "ok").expect("closed");

    let child_a = store
        .start_child_run("the first child", "/repo", root, 1)
        .expect("a child");
    let child_b = store
        .start_child_run("the second child", "/repo", root, 1)
        .expect("a child");
    let grandchild = store
        .start_child_run("the grandchild", "/repo", child_a, 2)
        .expect("a grandchild");
    for run in [root, child_a, child_b, grandchild] {
        store
            .record(run, &StepRecord::new(1, "a decision", "a result"))
            .expect("a step");
    }

    let conn = Connection::open(&path).expect("the store, read directly");
    let tree = tree_of(&conn, session);
    assert_eq!(
        tree.len(),
        4,
        "the tree is the root and everything under it: {tree:?}"
    );

    let pruned = store.delete_session(session).expect("the deletion");
    assert_eq!(pruned.runs, 4, "all four runs went");

    for run in [root, child_a, child_b, grandchild] {
        assert_eq!(
            rows_for(&conn, "runs", "id", &run.to_string()),
            0,
            "run {run} survived as an orphan"
        );
        assert_eq!(
            rows_for(&conn, "steps", "run_id", &run.to_string()),
            0,
            "run {run}'s steps survived"
        );
    }
}

/// F4. The deletion is one transaction — asserted by driving a failure partway
/// through the cascade, not by reading the code for a `tx`.
#[test]
fn a_deletion_that_fails_partway_leaves_the_store_exactly_as_it_was() {
    let (_dir, store, path) = on_disk();
    let session = seed_session(&store, "/repo", 2, 3, "alpha");
    let before = store
        .session_size(session)
        .expect("a size")
        .expect("the session exists");

    // A trigger on a table late in the cascade: the deletion reaches it, SQLite
    // aborts the statement, and everything already deleted must come back.
    let conn = Connection::open(&path).expect("the store, read directly");
    conn.execute_batch(
        "CREATE TRIGGER refuse_summaries BEFORE DELETE ON summaries
         BEGIN SELECT RAISE(ABORT, 'injected'); END;",
    )
    .expect("the trigger");
    store
        .put_summary(
            tree_of(&conn, session)[0],
            1,
            1,
            "a summary that cannot be deleted",
            0,
        )
        .expect("a summary");

    let failed = store.delete_session(session);
    assert!(failed.is_err(), "the injected failure reached the caller");

    conn.execute_batch("DROP TRIGGER refuse_summaries")
        .expect("the trigger goes");
    assert_eq!(
        store
            .session_size(session)
            .expect("a size")
            .map(|s| s.turns),
        Some(before.turns),
        "the turns are back"
    );
    assert_eq!(
        store.session_size(session).expect("a size").map(|s| s.runs),
        Some(before.runs),
        "and so are the runs"
    );
}

/// F5. `Pruned` reports what actually went, asserted against the size read
/// immediately before rather than against itself.
#[test]
fn the_pruned_report_equals_what_the_store_held_a_moment_earlier() {
    let (_dir, store, path) = on_disk();
    let session = seed_session(&store, "/repo", 3, 5, "alpha");
    seed_session(&store, "/other", 1, 1, "beta");

    let conn = Connection::open(&path).expect("the store, read directly");
    let runs = tree_of(&conn, session);
    // Seeded through the raw connection: the crate's own writer is `pub(crate)`,
    // and a restore point written from outside is a stricter fixture anyway.
    for (i, run) in runs.iter().enumerate() {
        conn.execute(
            "INSERT INTO snapshots (run_id, step, path, before, state)
             VALUES (?1, 1, ?2, 'the file as it was', 'text')",
            rusqlite::params![run, format!("src/file{i}.rs")],
        )
        .expect("a restore point");
    }
    let snapshots = rows_for(&conn, "snapshots", "run_id", &as_list(&runs));
    assert!(snapshots > 0, "the fixture made restore points");

    let before = store
        .session_size(session)
        .expect("a size")
        .expect("the session exists");

    let pruned = store.delete_session(session).expect("the deletion");

    assert_eq!(pruned.sessions, 1);
    assert_eq!(pruned.turns, before.turns);
    assert_eq!(pruned.runs, before.runs);
    assert_eq!(
        pruned.rows, before.rows,
        "the rows are the rows that were there"
    );
    assert_eq!(pruned.bytes, before.bytes, "and so are the bytes");
    assert_eq!(
        pruned.restore_points, snapshots as u64,
        "an undo's restore points are counted where they go"
    );
    assert!(pruned.refused.is_empty(), "nothing was refused");
}

/// F14, the deletion half. Deleting nothing succeeds.
#[test]
fn deleting_a_session_that_is_not_there_succeeds_and_reports_nothing() {
    let (_dir, store, _path) = on_disk();
    seed_session(&store, "/repo", 1, 1, "alpha");

    let pruned = store.delete_session(9_999).expect("no error");
    assert_eq!(pruned.sessions, 0);
    assert_eq!(pruned.runs, 0);
    assert_eq!(pruned.rows, 0);
    assert_eq!(pruned.bytes, 0);
    assert!(pruned.refused.is_empty());
}
