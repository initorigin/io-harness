//! 0.77.0's cross-version fixture: a store this tree wrote is read by a real io-harness
//! 0.76.0 from crates.io, and then written into by it.
//!
//! 0.77.0 adds one thing a store can see, and calls it additive:
//!
//! * **a nullable `origin` column on `ledger_observations`** — where each piece of
//!   transcript content came from, as the snake_case rendering of
//!   [`Origin`](io_harness::Origin).
//!
//! It does not bump `CHECKPOINT_FORMAT`. **Additive is a claim, and this file is the
//! only thing in the tree that tests it.** Every other test that touches the new column
//! reads rows the same tree has just written, which is precisely the shape of suite that
//! was green all the way through 0.72.0 while a serialization change made every question
//! in a 0.72.0 store unreadable to a real 0.71.0 binary. A release's own belief that it
//! touched nothing persisted is worth exactly as much as the previous binary that checks
//! it.
//!
//! **What this file is really guarding is the design that was not taken.** Provenance
//! could have been a new `ObsKind` variant rather than a column. `ObsKind` is read back
//! through a function that hard-errors on a value it does not know, so a 0.76.0 binary
//! handed a ledger containing that variant would refuse to restore the run — not lose a
//! field, refuse the run. Every test in 0.77.0's own suite would still have passed. The
//! two designs are indistinguishable from inside this tree and are told apart here.
//!
//! **The backwards direction only, on purpose.** The fixtures before this one each carry
//! a forwards half as well — a committed database written by the previous release and
//! read back here. There is none for 0.76.0 and no `write`-only mode to produce one. A
//! release that only adds a nullable column reads an older store by construction: the
//! column reads back `NULL`, and the forwards half has passed from its first run since
//! 0.72.0 without ever being in a position to fail. The direction that has caught
//! something is the one where the *older* binary is handed the newer store, and that one
//! needs a binary rather than a file.
//!
//! Two tests, because 0.77.0 puts the older binary in two different positions:
//!
//! * [`f17_a_current_store_is_read_by_a_0_76_0_binary`] — 0.76.0 opens a store this tree
//!   wrote, holding a ledger whose every observation carries an origin, and must lose
//!   nothing and refuse nothing.
//! * [`f17_a_0_76_0_write_into_a_current_store_is_read_back_as_unmarked`] — 0.76.0 then
//!   *writes* into that store. It knows nothing of `origin`, so it leaves a row with the
//!   column `NULL`. What this tree reads back afterwards is the assertion, and the answer
//!   must be `Unmarked` rather than a guess inferred from the row's kind.
//!
//! Both are `#[ignore]`d because both need `tests/fixtures/gen-0.76.0` built, which
//! resolves `io-harness =0.76.0` from crates.io. CI's `cross-version-0.76.0` job builds
//! it and runs this file with `-- --ignored`; by hand it is
//! `cargo build --manifest-path tests/fixtures/gen-0.76.0/Cargo.toml` and then
//! `cargo test --test cross_version_0_76_0 -- --ignored`.
//!
//! Nothing here reads a committed fixture, so nothing here can leave a `-wal` or `-shm`
//! sidecar in the tree: every database these tests open is created inside a temp
//! directory that goes away with the test.
//!
//! Expectations are the *current* tree's own readings, taken from the same store before
//! the older binary is handed it, rather than literals. Two binaries reading one store
//! and disagreeing is the finding; a literal in this file would only record which of
//! them was written down.

use std::path::{Path, PathBuf};

use io_harness::context::{ObsKind, Observation};
use io_harness::{Origin, Store};
use serde_json::Value;

// ---------------------------------------------------------------- scaffolding

/// The workspace the runs are keyed to. A fixed string rather than the temp directory's
/// path, so this machine's `/var/folders/...` never reaches an argument the generator is
/// spawned with.
const WORKSPACE: &str = "fixture-workspace";

/// The goal every run this file starts records. Fixed rather than absent: a `NULL` and a
/// value that round-tripped are different facts, and only the second proves the column
/// survived.
const GOAL: &str = "port the parser";

/// One observation per origin this tree can write, in the order it writes them.
///
/// Every external origin plus the three conversation ones. `Unmarked` is deliberately
/// absent: new code never constructs it, and a row carrying it is what the *second* test
/// produces by having the older binary write one.
///
/// The texts differ from each other so that a reader which got the rows back in the
/// wrong order fails on content rather than passing by symmetry.
const WRITTEN: [(ObsKind, Option<&str>, &str, Origin); 11] = [
    (
        ObsKind::Message,
        None,
        "the operator asked",
        Origin::Operator,
    ),
    (ObsKind::Message, None, "the agent answered", Origin::Agent),
    (
        ObsKind::Message,
        None,
        "the harness narrated",
        Origin::Prose,
    ),
    (
        ObsKind::Read,
        Some("src/lib.rs"),
        "a file was read",
        Origin::File,
    ),
    (
        ObsKind::Tool,
        Some("shell"),
        "a command printed",
        Origin::Shell,
    ),
    (
        ObsKind::Tool,
        Some("browser"),
        "a page was fetched",
        Origin::Web,
    ),
    (ObsKind::Mcp, Some("fix"), "a server replied", Origin::Mcp),
    (
        ObsKind::Tool,
        Some("lsp_hover"),
        "a server hovered",
        Origin::Lsp,
    ),
    (
        ObsKind::Skill,
        Some("porting"),
        "a skill was loaded",
        Origin::Skill,
    ),
    (ObsKind::Child, None, "a child concluded", Origin::Child),
    (
        ObsKind::Tool,
        Some("custom"),
        "a tool returned",
        Origin::Tool,
    ),
];

/// A run holding one observation of every origin, and its id.
fn a_run_with_every_origin(store: &Store) -> i64 {
    let run_id = store.start_run(GOAL, WORKSPACE).unwrap();
    let entries: Vec<Observation> = WRITTEN
        .iter()
        .enumerate()
        .map(|(i, (kind, target, text, origin))| {
            Observation::new(
                i as u32 + 1,
                *kind,
                target.map(str::to_string),
                *text,
                *origin,
            )
        })
        .collect();
    store.record_observations(run_id, &entries).unwrap();
    run_id
}

/// The pinned 0.76.0 binary, or a panic naming the step that was skipped.
fn generator() -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/gen-0.76.0/target/debug/gen-0-76-0");
    assert!(
        path.is_file(),
        "build it first: cargo build --manifest-path \
         tests/fixtures/gen-0.76.0/Cargo.toml ({path:?})"
    );
    path
}

/// Run the pinned binary and parse its JSON, or fail with what it printed.
///
/// A non-zero exit is the loudest possible form of the thing under test — it is the
/// previous release refusing a store this one wrote — so its stderr is the failure
/// message rather than a swallowed detail.
fn run_0_76_0(args: &[&std::ffi::OsStr]) -> Value {
    let generator = generator();
    let out = std::process::Command::new(&generator)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "0.76.0 failed against a 0.77.0 store: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// What 0.76.0 sees in `db`.
fn read_with_0_76_0(db: &Path) -> Value {
    run_0_76_0(&["read".as_ref(), db.as_ref()])
}

// ---------------------------------------------------------------------- tests

/// F17, backwards: 0.76.0 opens a store this tree wrote and loses nothing.
///
/// The assertion is not "it did not crash". A refusal would be an error exit and is
/// caught by [`run_0_76_0`]; what this checks is that every observation came back, in
/// order, with every field 0.76.0 has intact — because the failure mode a new `ObsKind`
/// variant would have produced is a *refusal to restore*, and the failure mode a careless
/// column could produce is a silently shorter ledger.
#[test]
#[ignore = "needs tests/fixtures/gen-0.76.0 built against crates.io"]
fn f17_a_current_store_is_read_by_a_0_76_0_binary() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    let run_id = {
        let store = Store::open(&db).unwrap();
        a_run_with_every_origin(&store)
    };

    let seen = read_with_0_76_0(&db);

    // The format was not bumped, so the previous release's own constant still matches
    // and its `check_resumable` still says yes. This is the assertion that fails first
    // if someone bumps `CHECKPOINT_FORMAT` for an additive column.
    assert_eq!(
        seen["checkpoint_format"],
        io_harness::CHECKPOINT_FORMAT,
        "0.76.0 read a different checkpoint format than this tree writes"
    );

    let runs = seen["runs"].as_array().expect("a runs array");
    assert_eq!(runs.len(), 1, "0.76.0 found the wrong number of runs");
    let run = &runs[0];
    assert_eq!(run["id"], run_id);
    assert_eq!(
        run["resumable"], true,
        "0.76.0 refused to resume a store 0.77.0 wrote"
    );

    let observations = run["observations"]
        .as_array()
        .expect("an observations array");
    assert_eq!(
        observations.len(),
        WRITTEN.len(),
        "0.76.0 came back with a different number of observations than were written"
    );
    for (seen, (kind, target, text, _)) in observations.iter().zip(WRITTEN.iter()) {
        assert_eq!(seen["text"], *text);
        assert_eq!(seen["kind"], format!("{kind:?}"));
        match target {
            Some(t) => assert_eq!(seen["target"], *t),
            None => assert!(seen["target"].is_null(), "{seen:?}"),
        }
    }
}

/// F17, forwards over a backwards write: 0.76.0 writes into the store, and what it left
/// behind reads back as `Unmarked`.
///
/// This is the state every 0.76.0 process touching a migrated store produces, and it is
/// the one case the compatibility promise is actually about. Two things are asserted and
/// the second is the one that could be got wrong quietly: the row is `Unmarked`, and it
/// is **not** `File` — nothing may infer a provenance from the row's kind, because an
/// inferred mark is a guess wearing the shape of a record. The generator writes a `Read`
/// with a path target precisely so that a wrong inference would look right.
#[test]
#[ignore = "needs tests/fixtures/gen-0.76.0 built against crates.io"]
fn f17_a_0_76_0_write_into_a_current_store_is_read_back_as_unmarked() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("store.db");
    {
        let store = Store::open(&db).unwrap();
        a_run_with_every_origin(&store);
    }

    let wrote = run_0_76_0(&[
        "observe".as_ref(),
        db.as_ref(),
        WORKSPACE.as_ref(),
        "what 0.76.0 left behind".as_ref(),
    ]);
    let older_run = wrote["run_id"].as_i64().expect("the run 0.76.0 started");

    let store = Store::open(&db).unwrap();
    let observations = store.observations(older_run).unwrap();
    assert_eq!(observations.len(), 1, "0.76.0 wrote more than one row");

    let row = &observations[0];
    assert_eq!(row.text, "what 0.76.0 left behind");
    assert_eq!(
        row.origin,
        Origin::Unmarked,
        "a row written before the column existed must read back unmarked"
    );
    assert!(
        !row.origin.is_external(),
        "unmarked is not a trust claim in either direction"
    );
    assert_ne!(
        row.origin,
        Origin::File,
        "the origin was inferred from the row's kind — an inferred mark is a guess \
         wearing the shape of a record"
    );

    // And the rows this tree wrote are untouched by the older binary's write.
    let ours = store.observations(older_run - 1).unwrap();
    assert_eq!(ours.len(), WRITTEN.len());
    for (row, (.., origin)) in ours.iter().zip(WRITTEN.iter()) {
        assert_eq!(row.origin, *origin);
    }
}
