//! The compaction rungs between Collapse and a fold (0.81.0).
//!
//! Through 0.80.0 this crate had two rungs where the field's reference
//! implementation has five: Context Collapse, a non-destructive read-time
//! projection, and the fold, which spends a model call and destroys detail.
//! Everything between them was a cliff, and compaction quality — not window size —
//! is what a long run is actually limited by.
//!
//! Three rungs fill it, in order of increasing loss: reduction is lossless, snip
//! drops old lookups by kind, microcompact replaces a run of one step's results
//! with a counted line. Each is asserted here against the thing it claims to do,
//! and each has a control showing what it does **not** touch — a rung that dropped
//! more than it says would pass a test that only counted tokens.

use std::sync::Arc;

use io_harness::context::{
    assemble, Assembly, Collapse, Ladder, Ledger, ObsKind, Observation, Origin, Snip,
};
use io_harness::{MemoryEntry, MemoryKind, Policy, Store};

// ---------------------------------------------------------------- scaffolding

struct Fixture {
    store: Store,
    policy: Policy,
    _dir: tempfile::TempDir,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    store
        .start_run("assemble", &dir.path().display().to_string())
        .unwrap();
    Fixture {
        store,
        policy: Policy::permissive(),
        _dir: dir,
    }
}

fn obs(step: u32, kind: ObsKind, target: &str, text: &str) -> Observation {
    Observation::new(
        step,
        kind,
        Some(target.to_string()),
        text.to_string(),
        Origin::Unmarked,
    )
}

/// A ledger of `n` steps, each carrying one `find` and one `read`, both large
/// enough that the ceiling matters.
fn ledger(n: u32) -> Ledger {
    let mut l = Ledger::default();
    for step in 1..=n {
        l.push(obs(
            step,
            ObsKind::Find,
            &format!("src/step{step}"),
            &format!("[find src/step{step}]\n{}\n", "path/to/a/file\n".repeat(20)),
        ));
        l.push(obs(
            step,
            ObsKind::Read,
            &format!("src/step{step}.rs"),
            &format!(
                "[read src/step{step}.rs]\n{}\n",
                "fn body() {}\n".repeat(20)
            ),
        ));
    }
    l
}

fn notes() -> Vec<MemoryEntry> {
    (0..6)
        .map(|i| MemoryEntry {
            key: format!("note-{i}"),
            value: "something worth remembering, at length, so the block has a size ".repeat(6),
            run_id: 1,
            step: 1,
            created_at: "2026-09-06T00:00:00Z".into(),
            kind: MemoryKind::Fact,
            pinned: false,
        })
        .collect()
}

async fn at(
    f: &Fixture,
    l: &Ledger,
    budget: u64,
    step: u32,
    ladder: Ladder,
    notes: &[MemoryEntry],
) -> io_harness::context::Assembled {
    assemble(
        l,
        budget,
        notes,
        &[],
        Assembly {
            ws: None,
            policy: &f.policy,
            store: &f.store,
            run_id: 1,
            step,
            collapse: Collapse::default(),
            ladder,
        },
    )
    .await
    .unwrap()
}

// ---------------------------------------------------------------------- F9

/// Every rung off assembles what 0.80.0 assembled.
///
/// The control the whole release rests on: three rungs landing in one function is
/// exactly the shape that changes behaviour for a caller who asked for nothing.
#[tokio::test]
async fn f9_a_run_that_configures_no_rung_assembles_what_it_did_before() {
    let f = fixture();
    let l = ledger(12);
    let notes = notes();

    let off = at(&f, &l, 1_200, 13, Ladder::default(), &notes).await;
    assert!(!off.reduced);
    assert_eq!(off.snipped, 0);
    assert_eq!(off.microcompacted, 0);
    assert!(off.carried > 0, "something was carried");
}

// --------------------------------------------------------------------- F10

/// Reduction gives the observations the memory block's room, and loses nothing.
#[tokio::test]
async fn f10_reduction_trims_the_notes_share_and_drops_nothing() {
    let f = fixture();
    let l = ledger(12);
    let notes = notes();

    let off = at(&f, &l, 1_200, 13, Ladder::default(), &notes).await;
    let on = at(
        &f,
        &l,
        1_200,
        13,
        Ladder {
            reduce: true,
            ..Ladder::default()
        },
        &notes,
    )
    .await;

    assert!(on.reduced, "the ledger overflows, so the rung fires");
    assert!(
        on.carried >= off.carried,
        "reduction buys room for observations: {} carried against {}",
        on.carried,
        off.carried
    );
    assert_eq!(
        on.snipped + on.microcompacted,
        0,
        "the lossless rung drops nothing"
    );
}

/// A run that fits is not reduced, because there is nothing to gain.
///
/// The negative control. A rung that always fired would shorten the memory block
/// of every short run for no reason, and nothing in the token count would say so.
#[tokio::test]
async fn f10_a_ledger_that_fits_is_not_reduced() {
    let f = fixture();
    let l = ledger(1);
    let notes = notes();

    let on = at(
        &f,
        &l,
        200_000,
        2,
        Ladder {
            reduce: true,
            ..Ladder::default()
        },
        &notes,
    )
    .await;
    assert!(!on.reduced);
}

// --------------------------------------------------------------------- F11

/// Snip drops old lookups and keeps everything else, including recent lookups.
#[tokio::test]
async fn f11_snip_drops_old_lookups_and_keeps_reads_of_the_same_age() {
    let f = fixture();
    let l = ledger(30);
    let ladder = Ladder {
        snip: Some(Snip {
            older_than_steps: 10,
        }),
        ..Ladder::default()
    };

    let on = at(&f, &l, 100_000, 31, ladder, &[]).await;

    // Steps 1..=20 are more than ten steps behind step 31, and each carries one
    // `find`. Steps 21..=30 are recent and keep theirs.
    assert_eq!(
        on.snipped, 20,
        "one lookup per step older than the window, and no more"
    );
    assert!(
        on.text.contains("src/step25"),
        "a recent lookup is untouched"
    );
    assert!(
        on.text.contains("fn body()"),
        "a read of the same age as a dropped lookup is a finding, not a lookup, \
         and is kept"
    );
}

/// A run with no snip configured drops no lookup however old.
#[tokio::test]
async fn f11_without_the_rung_no_lookup_is_dropped() {
    let f = fixture();
    let l = ledger(30);
    let off = at(&f, &l, 100_000, 31, Ladder::default(), &[]).await;
    assert_eq!(off.snipped, 0);
}

// --------------------------------------------------------------------- F12

/// Microcompact replaces a run of one step's results with a counted line, and
/// spends no model call doing it.
#[tokio::test]
async fn f12_microcompact_replaces_a_run_of_results_with_one_counted_line() {
    let f = fixture();
    let mut l = Ledger::default();
    // One step with four results, and a later step with one.
    for i in 0..4 {
        l.push(obs(
            1,
            ObsKind::Read,
            &format!("src/a{i}.rs"),
            &format!("[read src/a{i}.rs]\nfn a{i}() {{}}\n"),
        ));
    }
    l.push(obs(
        2,
        ObsKind::Read,
        "src/b.rs",
        "[read src/b.rs]\nfn b() {}\n",
    ));

    let ladder = Ladder {
        microcompact: true,
        ..Ladder::default()
    };
    let on = at(&f, &l, 100_000, 3, ladder, &[]).await;

    assert_eq!(on.microcompacted, 1, "one run of results, one compaction");
    assert!(
        on.text.contains("step 1 made 4 calls"),
        "the line says what it stands for: {}",
        on.text
    );
    assert!(
        on.text.contains("fn b()"),
        "a step with one result is not a run and is untouched: {}",
        on.text
    );
}

/// Two results are not a run.
///
/// The negative control for the threshold: compacting a pair costs a reader both
/// results to save one line, which is the wrong trade at every ceiling.
#[tokio::test]
async fn f12_a_pair_of_results_is_not_compacted() {
    let f = fixture();
    let mut l = Ledger::default();
    for i in 0..2 {
        l.push(obs(
            1,
            ObsKind::Read,
            &format!("src/a{i}.rs"),
            &format!("[read src/a{i}.rs]\nfn a{i}() {{}}\n"),
        ));
    }

    let on = at(
        &f,
        &l,
        100_000,
        3,
        Ladder {
            microcompact: true,
            ..Ladder::default()
        },
        &[],
    )
    .await;
    assert_eq!(on.microcompacted, 0);
    assert!(on.text.contains("fn a0()") && on.text.contains("fn a1()"));
}

/// The step being assembled for is never compacted.
///
/// The agent has just made those calls and is about to read their results;
/// compacting them would answer a question with a summary of the answer.
#[tokio::test]
async fn f12_the_current_steps_own_results_are_never_compacted() {
    let f = fixture();
    let mut l = Ledger::default();
    for i in 0..4 {
        l.push(obs(
            5,
            ObsKind::Read,
            &format!("src/a{i}.rs"),
            &format!("[read src/a{i}.rs]\nfn a{i}() {{}}\n"),
        ));
    }

    let on = at(
        &f,
        &l,
        100_000,
        5,
        Ladder {
            microcompact: true,
            ..Ladder::default()
        },
        &[],
    )
    .await;
    assert_eq!(on.microcompacted, 0);
    assert!(on.text.contains("fn a3()"));
}

/// The trace says which rungs ran, so an operator reading a run can tell a
/// reduction from a snip from a fold.
#[tokio::test]
async fn f9_the_assembly_trace_names_every_rung_that_ran() {
    let f = fixture();
    let l = ledger(30);
    let _ = at(
        &f,
        &l,
        1_200,
        31,
        Ladder {
            reduce: true,
            snip: Some(Snip {
                older_than_steps: 5,
            }),
            microcompact: true,
        },
        &notes(),
    )
    .await;

    let detail: String = f
        .store
        .context_events(1)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "assembled")
        .filter_map(|e| e.detail.clone())
        .collect();
    for key in ["reduced=", "snipped=", "microcompacted="] {
        assert!(detail.contains(key), "{key} missing from {detail}");
    }
}

// A `Ledger` is built by pushing; nothing here needs the `Arc` the run loop uses.
const _: Option<Arc<()>> = None;
