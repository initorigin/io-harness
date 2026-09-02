//! 0.74.0 — the state layer's untrusted inputs.
//!
//! A SQLite file the harness is pointed at is input, not infrastructure. An
//! agent following instructions it read out of a hostile repository can be told
//! to open a database that shipped with it, so what that file's schema says is
//! attacker-controlled — and what the trace the harness then writes is readable
//! by is a decision this crate makes rather than one the umask should.
//!
//! Every test here is named for the audit finding it closes.

use io_harness::Store;
use rusqlite::Connection;

/// A column name written to close the expression it is interpolated into and
/// run a statement of its own. The class L1 is about: the name is not a name,
/// it is SQL, and the only thing that makes it a name again is quoting.
const HOSTILE: &str = "note), 0); DELETE FROM memory; --";

/// A column name that is odd and entirely legitimate. Quoting has to survive
/// this as well as refuse the one above, or the fix trades an injection for a
/// store nobody with an unusual schema can open.
const AWKWARD: &str = "he said \"hi\" -- twice";

/// One identifier, spelled the way SQLite wants it inside a statement.
fn quoted(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// A database this crate did not write, carrying a `todos` table with one extra
/// text column named `column`, and a store opened over it.
///
/// `todos` because it is one of the run-keyed tables retention walks, and the
/// walk reads the columns from the schema in front of it rather than from a
/// list compiled into the binary — which is exactly why a foreign schema
/// reaches the statements at all.
fn foreign_store(dir: &std::path::Path, column: &str) -> (std::path::PathBuf, Store) {
    let path = dir.join("foreign.sqlite3");
    {
        let conn = Connection::open(&path).expect("a database file");
        conn.execute_batch(&format!(
            "CREATE TABLE todos (
                 id         INTEGER PRIMARY KEY AUTOINCREMENT,
                 run_id     INTEGER NOT NULL,
                 position   INTEGER NOT NULL,
                 text       TEXT NOT NULL,
                 state      TEXT NOT NULL,
                 written_at TEXT NOT NULL DEFAULT '',
                 {} TEXT NOT NULL DEFAULT ''
             );",
            quoted(column)
        ))
        .expect("the foreign schema");
    }
    let store = Store::open(&path).expect("a store over a foreign database file");
    (path, store)
}

/// One session, one turn, one run, a memory entry the archive has no business
/// touching, and a `todos` row with something in the foreign column so the
/// archive has a reason to write. Returns the session and the run.
fn seed(store: &Store, path: &std::path::Path, column: &str) -> (i64, i64) {
    let session = store.create_session("/repo").expect("a session");
    let run = store.start_run("a goal", "/repo").expect("a run");
    let turn = store
        .record_turn(session, None, run, "something private")
        .expect("a turn");
    store
        .finish_turn(turn, Some("an answer"), "ok")
        .expect("a finished turn");
    store
        .memory_put("/repo", "kept", "a note the archive keeps", run, 1)
        .expect("a memory entry");

    let conn = Connection::open(path).expect("a second handle on the same file");
    conn.execute(
        &format!(
            "INSERT INTO todos (run_id, position, text, state, written_at, {})
             VALUES (?1, 0, 'a todo', 'open', '', 'content')",
            quoted(column)
        ),
        [run],
    )
    .expect("a row in the foreign table");
    (session, run)
}

/// L1 — a column name that is SQL is treated as a name.
///
/// On 0.73.0 the name is interpolated raw into the sums retention builds and
/// into the `UPDATE` the archive runs, so opening a foreign database and asking
/// what a session costs ends in a SQL failure — with the statement the name
/// carries one refactor away from running. Reading the size and archiving both
/// have to succeed, the odd column has to be cleared like any other, and the
/// `memory` table the name names has to still be there.
#[test]
fn l1_a_column_name_that_is_sql_is_still_only_a_column_name() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (path, store) = foreign_store(dir.path(), HOSTILE);
    let (session, _run) = seed(&store, &path, HOSTILE);

    let size = store
        .session_size(session)
        .expect("a size over a foreign schema")
        .expect("the session exists");
    assert!(size.bytes > 0, "the foreign table's bytes are counted");

    let archived = store
        .archive_session(session)
        .expect("an archive over a foreign schema");
    assert_eq!(archived.turns, 1);
    assert!(archived.bytes > 0, "the archive cleared something");

    let conn = Connection::open(&path).expect("a reader");
    let left: String = conn
        .query_row(&format!("SELECT {} FROM todos", quoted(HOSTILE)), [], |r| {
            r.get(0)
        })
        .expect("the foreign column is readable");
    assert_eq!(left, "", "the odd column is cleared like every other one");

    assert!(
        store
            .memory_get("/repo", "kept")
            .expect("memory is readable")
            .is_some(),
        "the statement the column name spells never ran"
    );
}

/// L1 — the companion. A legitimate name holding a double quote and a comment
/// marker survives the quoting rather than being refused by it.
///
/// On 0.73.0 this name is interpolated raw and fails the same way the hostile
/// one does, so the test is a negative control on the fix as much as on the
/// defect: quoting that escaped nothing would pass the test above and fail
/// this one.
#[test]
fn l1_a_column_name_holding_a_quote_survives_being_quoted() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let (path, store) = foreign_store(dir.path(), AWKWARD);
    let (session, _run) = seed(&store, &path, AWKWARD);

    assert!(
        store
            .session_size(session)
            .expect("a size over an awkward schema")
            .expect("the session exists")
            .bytes
            > 0
    );
    let archived = store
        .archive_session(session)
        .expect("an archive over an awkward schema");
    assert!(archived.bytes > 0);

    let conn = Connection::open(&path).expect("a reader");
    let left: String = conn
        .query_row(&format!("SELECT {} FROM todos", quoted(AWKWARD)), [], |r| {
            r.get(0)
        })
        .expect("the awkward column is readable");
    assert_eq!(left, "", "an awkward name is a name, and it is cleared");
}

/// L2 — the trace holds whatever the run saw, so the file it holds it in is the
/// user's own.
///
/// Two arms. The first is the store this release creates; the second is a store
/// an earlier release left at the umask's 0644, and it is the arm that cannot
/// pass by accident — a runner whose umask is already 0077 would make the first
/// arm green on 0.73.0, and no umask changes the mode of a file that already
/// exists.
///
/// Unix only: Windows has no mode bits, and there the file takes what the
/// containing directory's ACL grants it.
#[cfg(unix)]
#[test]
fn l2_a_trace_file_is_readable_only_by_the_user_that_made_it() {
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &std::path::Path) -> u32 {
        std::fs::metadata(path)
            .expect("the store file exists")
            .permissions()
            .mode()
            & 0o777
    }

    let dir = tempfile::tempdir().expect("a temporary directory");

    let fresh = dir.path().join("fresh.sqlite3");
    let store = Store::open(&fresh).expect("a store");
    store.start_run("a goal", "/repo").expect("a run");
    drop(store);
    assert_eq!(
        mode(&fresh),
        0o600,
        "a trace this release creates is the user's own"
    );

    let legacy = dir.path().join("legacy.sqlite3");
    {
        let conn = Connection::open(&legacy).expect("a database file");
        conn.execute_batch("CREATE TABLE leftover (a INTEGER);")
            .expect("something in it");
    }
    std::fs::set_permissions(&legacy, std::fs::Permissions::from_mode(0o644))
        .expect("the mode an earlier release left");
    let store = Store::open(&legacy).expect("a store over an existing file");
    drop(store);
    assert_eq!(
        mode(&legacy),
        0o600,
        "a trace an earlier release left world-readable stops being so"
    );
}
