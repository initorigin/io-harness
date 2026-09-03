//! What a real io-harness 0.76.0 from crates.io can see in — and can write into — a
//! store that 0.77.0 wrote. Driven by `tests/cross_version_0_76_0.rs`.
//!
//! The point of the crate is that the *other* binary is the previous release, not this
//! tree with a 0.76.0 label on it. 0.77.0 adds one thing to a store and calls it
//! additive: a nullable `origin` column on `ledger_observations`, recording where a
//! piece of transcript content came from. It does not bump `CHECKPOINT_FORMAT`. 0.72.0
//! also believed its own change was additive, its whole suite agreed, and only a real
//! previous-release binary showed otherwise; a belief that a release touched no
//! persisted surface is worth exactly as much as the binary that checks it.
//!
//! **What this fixture is really guarding against is the design that was not taken.**
//! Provenance could have been a new `ObsKind` variant instead of a column. `ObsKind` is
//! read back through a function that hard-errors on a value it does not know, so a
//! 0.76.0 binary handed a ledger containing that variant would refuse to restore the
//! run — not lose a field, refuse the run. Every test in 0.77.0's own suite would still
//! have passed, because every one of them reads rows the same tree just wrote. This
//! binary is the only thing that can tell the two designs apart.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-76-0 read    <database>                       # what 0.76.0 sees, as JSON
//! gen-0-76-0 observe <database> <workspace> <text>    # a 0.76.0 write into the ledger
//! ```
//!
//! `read` is the half that answers "does the previous release still understand this
//! store". It reports the format number and, for every run, the whole observation
//! ledger and the canonical trace. Every observation written by 0.77.0 carries a column
//! this binary's schema does not have; all of them must read back as though it did not
//! exist, in the same order, with every other column intact.
//!
//! `observe` is the half nothing before this release needed. A 0.76.0 `Observation` has
//! four fields and its insert names five columns, so a row it writes leaves `origin`
//! NULL. That is not a hypothetical — it is what every 0.76.0 process touching a
//! migrated store does, and a NULL there is the one state 0.77.0 promises reads back as
//! `Origin::Unmarked` rather than as an error or as a guess. This mode is how that gets
//! asked with a real 0.76.0 binary rather than a simulation of one.
//!
//! The workspace is an argument rather than a constant here, so the test that spawns
//! this binary owns the one spelling of it and the two files cannot drift apart.
//!
//! Fully offline and deterministic: no run loop, no provider, no network, no API key.

use io_harness::context::{ObsKind, Observation};
use io_harness::Store;
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one useful
/// behaviour — print why and exit non-zero — so there is nothing for a typed error to
/// decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The goal the run behind an `observe` records. Fixed rather than absent: a `NULL` and
/// a value that round-tripped are different facts, and only the second proves the
/// column survived a write by the older binary.
const GOAL: &str = "port the parser";

/// The highest run id this binary looks for. Run ids are dense from 1 in a fixture
/// database, and a run that is not there is skipped rather than fatal — `read` must be
/// able to open a store it did not write and report what is actually in it.
const MAX_RUN_ID: i64 = 64;

/// Everything 0.76.0 can see of one observation, as JSON.
///
/// Four fields, because four is all a 0.76.0 `Observation` has. The `origin` column
/// beside them in a 0.77.0 store is invisible from here, and its invisibility is the
/// claim: this is a reader that must lose nothing it used to have, not one that must
/// understand something new.
fn observation(o: &Observation) -> Value {
    json!({
        "step": o.step,
        // Through the debug rendering rather than the stored word, because the stored
        // word is what the newer binary writes and this binary's own understanding of
        // it is what is being tested. A kind that failed to parse would not reach here
        // at all — it would have failed the whole read, which is exactly the failure
        // mode a new `ObsKind` variant would have produced.
        "kind": format!("{:?}", o.kind),
        "target": o.target,
        "text": o.text,
    })
}

/// Everything 0.76.0 can see of one run and its ledger, as JSON.
fn run(store: &Store, run_id: i64) -> Res<Value> {
    Ok(json!({
        "id": run_id,
        "status": store.status(run_id)?,
        "outcome": store.outcome(run_id)?,
        "last_step": store.last_step(run_id)?,
        // The ledger, whole and in order. Recorded as a list rather than a map so the
        // ordering is part of what the test compares — a reader that got every column
        // right and the order wrong is still a reader that would assemble a context
        // nobody can account for.
        "observations": store.observations(run_id)?.iter().map(observation).collect::<Vec<_>>(),
        // The canonical trace is one string over every step and context event, and it
        // is the crate's own answer to "is this the same trace" — see
        // `tests/determinism.rs`.
        "canonical_trace": store.canonical_trace(run_id)?,
        // 0.76.0's verdict, not the test's guess. `check_resumable` compares the file's
        // `user_version` against `CHECKPOINT_FORMAT`, so it is the one call that fails
        // first if a release bumps the format — and 0.77.0 claims not to have.
        "resumable": store.check_resumable(run_id).is_ok(),
    }))
}

/// Print everything 0.76.0 can see of `db`, as JSON on stdout.
///
/// Nothing here writes. Anything that opens and selects is fair game.
fn read(db: &str) -> Res<()> {
    let store = Store::open(db)?;
    let mut runs = Vec::new();
    for run_id in 1..=MAX_RUN_ID {
        // `status` is the existence check as well as a column: a run this binary cannot
        // find has none, and asking `check_resumable` about it would be an error rather
        // than an answer.
        if store.status(run_id)?.is_none() {
            continue;
        }
        runs.push(run(&store, run_id)?);
    }
    let out = json!({
        "reader": "io-harness 0.76.0",
        "checkpoint_format": io_harness::CHECKPOINT_FORMAT,
        "runs": runs,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Write one ledger observation the way 0.76.0 writes one, and report what happened.
///
/// A 0.76.0 `record_observations` inserts five columns and knows nothing of a sixth, so
/// the row it leaves behind has `origin` NULL. Every field a 0.77.0 reader asks about
/// afterwards is answered from a row a real previous-release binary actually wrote.
///
/// The run is started rather than reused: `ledger_observations.run_id` records which run
/// observed a thing, and an observation attributed to a run the older binary did not
/// create would not be the write the older binary performs.
fn observe(db: &str, workspace: &str, text: &str) -> Res<()> {
    let store = Store::open(db)?;
    let run_id = store.start_run(GOAL, workspace)?;
    // `ObsKind::Read` with a path target, so the row a 0.77.0 reader picks up is one
    // whose origin would obviously have been `File` had this binary known how to say
    // so. That the reader must NOT infer `File` from the kind is the point: an
    // inferred mark is a guess wearing the shape of a record, and `Unmarked` is the
    // honest answer.
    store.record_observations(
        run_id,
        &[Observation::new(
            1,
            ObsKind::Read,
            Some("src/lib.rs".into()),
            text,
        )],
    )?;
    let out = json!({
        "writer": "io-harness 0.76.0",
        "run_id": run_id,
        "text": text,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn main() -> Res<()> {
    let usage = "usage: gen-0-76-0 read <database>\n       \
                 gen-0-76-0 observe <database> <workspace> <text>";
    let owned: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    match args[..] {
        ["read", db] => read(db),
        ["observe", db, workspace, text] => observe(db, workspace, text),
        _ => Err(usage.into()),
    }
}
