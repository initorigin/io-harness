//! The `summaries` table (0.43.0): what a fold wrote, kept so a resumed run does
//! not pay a model to write the same paragraph twice.
//!
//! The migration half of `O2` lives in `tests/cross_version.rs`, which compares
//! the whole schema against 0.22.0's and permits only the objects a release
//! declared. What is asserted here is the half that file cannot see: that the row
//! round-trips, that a boundary is a boundary, and that `CHECKPOINT_FORMAT` did
//! not move to buy it.

use io_harness::{Store, CHECKPOINT_FORMAT};

#[test]
fn a_summary_round_trips_through_the_store() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port the parser", "openrouter").unwrap();

    let id = store
        .put_summary(run_id, 12, 40, "Read the lexer; kept the token enum.", 11)
        .unwrap();
    assert!(id > 0, "a write returns the row it wrote");

    let found = store.summary_for(run_id, 40).unwrap().expect("a row at 40");
    assert_eq!(found.id, id);
    assert_eq!(found.through_step, 12);
    assert_eq!(found.folded, 40);
    assert_eq!(found.text, "Read the lexer; kept the token enum.");
    assert_eq!(found.est_tokens, 11);
    assert!(!found.at.is_empty(), "the row is stamped by SQLite's clock");
}

#[test]
fn the_ledger_position_is_the_key_and_a_neighbouring_one_is_not_it() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port the parser", "openrouter").unwrap();
    store
        .put_summary(run_id, 12, 40, "the fold at forty observations", 4)
        .unwrap();

    assert!(
        store.summary_for(run_id, 39).unwrap().is_none(),
        "a shorter history is a different boundary"
    );
    assert!(
        store.summary_for(run_id, 41).unwrap().is_none(),
        "and so is a longer one — a lookup never falls back to a neighbour"
    );
}

#[test]
fn a_corrected_fold_reads_back_as_the_correction() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("port the parser", "openrouter").unwrap();
    store
        .put_summary(run_id, 12, 40, "first attempt", 2)
        .unwrap();
    store
        .put_summary(run_id, 12, 40, "second attempt", 2)
        .unwrap();

    assert_eq!(
        store.summary_for(run_id, 40).unwrap().unwrap().text,
        "second attempt",
        "the newest row at a boundary wins"
    );
    assert_eq!(
        store.summaries(run_id).unwrap().len(),
        2,
        "and both are still on disk: a fold is recorded, not overwritten"
    );
}

#[test]
fn summaries_are_per_run_and_ordered_oldest_first() {
    let store = Store::memory().unwrap();
    let mine = store.start_run("mine", "openrouter").unwrap();
    let theirs = store.start_run("theirs", "openrouter").unwrap();

    store.put_summary(mine, 8, 20, "the first fold", 3).unwrap();
    store
        .put_summary(theirs, 4, 10, "another run's fold", 3)
        .unwrap();
    store
        .put_summary(mine, 19, 44, "the second fold", 3)
        .unwrap();

    let folds = store.summaries(mine).unwrap();
    assert_eq!(folds.len(), 2, "another run's fold is not this run's");
    assert_eq!(folds[0].through_step, 8);
    assert_eq!(folds[1].through_step, 19);
}

#[test]
fn a_run_that_never_folded_has_no_summaries() {
    let store = Store::memory().unwrap();
    let run_id = store.start_run("short enough", "openrouter").unwrap();
    assert!(store.summaries(run_id).unwrap().is_empty());
    assert!(store.summary_for(run_id, 1).unwrap().is_none());
}

#[test]
fn the_checkpoint_format_did_not_move_for_an_additive_table() {
    // The claim O2 rests on. A bump would make `check_resumable` refuse every
    // 0.42.x store over one table an older binary never names.
    assert_eq!(CHECKPOINT_FORMAT, 7);
}
