//! Both crates, one binary, one process.
//!
//! This stands in for a downstream program that already uses `rusqlite` and
//! wants to add io-harness. Against io-harness 0.22.0 it is EXPECTED TO FAIL —
//! not here, but at dependency resolution, because `libsqlite3-sys` declares
//! `links = "sqlite3"` and 0.22.0 pinned an incompatible version of it. Against
//! 0.23.0 it resolves, builds and runs.
//!
//! Deliberately trivial. The evidence is that this binary exists and runs at
//! all; anything more would be testing `rusqlite` and io-harness rather than
//! testing that they can share a process.

fn main() {
    // The consumer's own rusqlite 0.40, used directly.
    let conn = rusqlite::Connection::open_in_memory().expect("open in-memory db");
    let two: i64 = conn
        .query_row("SELECT 1 + 1", [], |row| row.get(0))
        .expect("query through rusqlite 0.40");
    assert_eq!(two, 2);

    // io-harness's own SQLite store, in the same process, over the same
    // `libsqlite3-sys` — which is the thing that could not happen before.
    let store = io_harness::Store::memory().expect("open io-harness store");
    store
        .memory_put("/links-consumer", "fact", "both crates are live", 0, 0)
        .expect("write through io-harness");
    let entry = store
        .memory_get("/links-consumer", "fact")
        .expect("read through io-harness")
        .expect("written above");
    assert_eq!(entry.value, "both crates are live");

    println!(
        "links-consumer OK: rusqlite 0.40 and io-harness share one libsqlite3-sys (sqlite {})",
        rusqlite::version()
    );
}
