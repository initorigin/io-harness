//! 0.10.0: durable, cross-run memory. Everything a run learns and writes
//! deliberately is keyed to a *workspace*, not a run id, so a second run over
//! the same workspace starts knowing what the first one found out — and two
//! workspaces never leak into each other.
//!
//! The cap arithmetic and the migration are proven in `src/state.rs`'s unit
//! tests (they need the private connection and the cap constants). These are the
//! promises a caller sees: recall across runs, isolation, overwrite, forget.

use io_harness::Store;

/// Mirrors `state::MEMORY_MAX_ENTRIES`. Kept as a literal because the constant is
/// not re-exported from the crate root yet.
const MAX_ENTRIES: usize = 64;

#[test]
fn a_fact_written_by_one_run_is_readable_by_another() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("runs.db");

    // Run 1 learns something and writes it down, then the process goes away.
    {
        let store = Store::open(&path).unwrap();
        store
            .memory_put("/ws/app", "build_cmd", "cargo test --lib", 1, 4)
            .unwrap();
    }

    // Run 2 is a different process, a different run id, the same workspace.
    let store = Store::open(&path).unwrap();
    let entry = store
        .memory_get("/ws/app", "build_cmd")
        .unwrap()
        .expect("the earlier run's fact survived");
    assert_eq!(entry.value, "cargo test --lib");
    // Attribution survives too, so the reader knows where the fact came from.
    assert_eq!(entry.run_id, 1);
    assert_eq!(entry.step, 4);
    assert!(!entry.created_at.is_empty());

    // And it is in the workspace's listing, not just findable by key.
    let listed = store.memory_list("/ws/app").unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0], entry);
}

#[test]
fn two_workspaces_never_see_each_others_entries() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws/a", "k", "a's value", 1, 1).unwrap();
    store.memory_put("/ws/b", "k", "b's value", 1, 1).unwrap();

    assert_eq!(
        store.memory_get("/ws/a", "k").unwrap().unwrap().value,
        "a's value"
    );
    assert_eq!(
        store.memory_get("/ws/b", "k").unwrap().unwrap().value,
        "b's value"
    );
    assert_eq!(store.memory_list("/ws/a").unwrap().len(), 1);
    assert_eq!(store.memory_list("/ws/b").unwrap().len(), 1);
    // A workspace nobody wrote to holds nothing.
    assert!(store.memory_list("/ws/c").unwrap().is_empty());
    assert!(store.memory_get("/ws/c", "k").unwrap().is_none());
}

#[test]
fn re_putting_a_key_replaces_the_value_and_re_attributes_it() {
    let store = Store::memory().unwrap();
    store
        .memory_put("/ws", "api_base", "http://localhost:1", 1, 2)
        .unwrap();
    let evicted = store
        .memory_put("/ws", "api_base", "http://localhost:2", 9, 7)
        .unwrap();
    assert!(evicted.is_empty(), "an overwrite evicts nothing");

    // One row, not two — the key is unique within its workspace.
    let entries = store.memory_list("/ws").unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].value, "http://localhost:2");
    // The latest writer owns the fact.
    assert_eq!(entries[0].run_id, 9);
    assert_eq!(entries[0].step, 7);
}

#[test]
fn a_write_past_the_entry_cap_reports_the_keys_it_evicted() {
    let store = Store::memory().unwrap();
    for i in 0..MAX_ENTRIES {
        assert!(store
            .memory_put("/ws", &format!("k{i}"), "v", 1, 1)
            .unwrap()
            .is_empty());
    }
    // The write that overflows names its cost, so the caller can trace it.
    let evicted = store.memory_put("/ws", "newest", "v", 2, 1).unwrap();
    assert_eq!(evicted, vec!["k0"]);
    assert_eq!(store.memory_list("/ws").unwrap().len(), MAX_ENTRIES);
    assert!(store.memory_get("/ws", "k0").unwrap().is_none());
    // The just-written entry is still there — a write is never a silent no-op.
    assert!(store.memory_get("/ws", "newest").unwrap().is_some());
}

#[test]
fn memory_delete_returns_true_then_false_and_the_key_stays_gone() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws", "k", "v", 1, 1).unwrap();

    assert!(store.memory_delete("/ws", "k").unwrap());
    // Deleting again is honest about there being nothing left to delete.
    assert!(!store.memory_delete("/ws", "k").unwrap());
    assert!(!store.memory_delete("/ws", "never-existed").unwrap());

    assert!(store.memory_get("/ws", "k").unwrap().is_none());
    assert!(store.memory_list("/ws").unwrap().is_empty());
}

#[test]
fn memory_clear_empties_one_workspace_and_reports_the_count() {
    let store = Store::memory().unwrap();
    store.memory_put("/ws/a", "k1", "v", 1, 1).unwrap();
    store.memory_put("/ws/a", "k2", "v", 1, 1).unwrap();
    store.memory_put("/ws/a", "k3", "v", 1, 1).unwrap();
    store.memory_put("/ws/b", "k1", "v", 1, 1).unwrap();

    assert_eq!(store.memory_clear("/ws/a").unwrap(), 3);
    assert!(store.memory_list("/ws/a").unwrap().is_empty());
    // The other workspace is untouched.
    assert_eq!(store.memory_list("/ws/b").unwrap().len(), 1);
    // Clearing an empty workspace is a no-op, not an error.
    assert_eq!(store.memory_clear("/ws/a").unwrap(), 0);
}

#[test]
fn an_oversized_value_is_remembered_truncated_rather_than_refused() {
    let store = Store::memory().unwrap();
    // Multibyte throughout: a byte-wise cut would not be valid UTF-8 at all.
    let huge = "日".repeat(50_000);
    store.memory_put("/ws", "log", &huge, 1, 1).unwrap();

    let stored = store.memory_get("/ws", "log").unwrap().unwrap().value;
    assert!(stored.chars().count() < huge.chars().count());
    assert!(
        stored.ends_with("…[truncated]"),
        "the cut is visible: {stored:.40}"
    );
    // Every kept char is a whole char, never a half of one.
    assert!(stored.chars().take_while(|c| *c == '日').count() > 0);
    assert!(stored
        .trim_end_matches("…[truncated]")
        .chars()
        .all(|c| c == '日'));
}
