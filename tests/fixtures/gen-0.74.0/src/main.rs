//! What a real io-harness 0.74.0 from crates.io can see in — and can write into — a
//! store that 0.75.0 wrote. Driven by `tests/cross_version_0_74_0.rs`.
//!
//! The point of the crate is that the *other* binary is the previous release, not this
//! tree with a 0.74.0 label on it. 0.75.0 adds two things to a store and calls both
//! additive: a `memory_token_cache` table beside `memory`, holding each entry's
//! pre-computed token lines, and five nullable latency columns on `steps`. Neither
//! bumps `CHECKPOINT_FORMAT`. 0.72.0 also believed its own change was additive, its
//! whole suite agreed, and only a real previous-release binary showed otherwise; a
//! belief that a release touched no persisted surface is worth exactly as much as the
//! binary that checks it.
//!
//! Hence two modes rather than one:
//!
//! ```text
//! gen-0-74-0 read     <database> <workspace>              # what 0.74.0 sees, as JSON
//! gen-0-74-0 remember <database> <workspace> <key> <value> # a 0.74.0 write into it
//! ```
//!
//! `read` is the half that answers "does the previous release still understand this
//! store". It reports the format number, every memory entry of one workspace, and every
//! run with its whole trace — the two surfaces 0.75.0 moves. A step row written by
//! 0.75.0 carries five columns this binary's schema does not have, and a memory entry
//! written by 0.75.0 has a cache row this binary will never look at; both must read
//! back as though neither existed.
//!
//! `remember` is the half nothing before this release needed. The new cache table is
//! keyed to `memory` and stamped with the entry's `created_at`, so a binary that knows
//! nothing about it writes a `memory` row and leaves the cache either missing a line or
//! holding a stale one. That is not a hypothetical: it is what every 0.74.0 process
//! touching a migrated store does. What a 0.75.0 reader then answers is the question
//! the backwards half exists to ask, and this mode is how it gets asked with a real
//! 0.74.0 binary rather than a simulation of one.
//!
//! The workspace is an argument rather than a constant here, so the test that spawns
//! this binary owns the one spelling of it and the two files cannot drift apart.
//!
//! Fully offline and deterministic: no run loop, no provider, no network, no API key.

use io_harness::{MemoryEntry, StepRecord, Store};
use serde_json::{json, Value};

/// The binary's own error type: a fixture generator that fails has exactly one useful
/// behaviour — print why and exit non-zero — so there is nothing for a typed error to
/// decide.
type Res<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The goal the run behind a `remember` records. Fixed rather than absent: a `NULL` and
/// a value that round-tripped are different facts, and only the second proves the
/// column survived a write by the older binary.
const GOAL: &str = "port the parser";

/// The highest run id this binary looks for. Run ids are dense from 1 in a fixture
/// database, and a run that is not there is skipped rather than fatal — `read` must be
/// able to open a store it did not write and report what is actually in it.
const MAX_RUN_ID: i64 = 64;

/// Everything 0.74.0 can see of one memory entry, as JSON.
///
/// Every column of the row, including the ones 0.75.0's cache duplicates. `created_at`
/// is reported because it is the stamp the new cache is keyed against: an entry whose
/// value a 0.74.0 binary replaced without touching the cache is exactly an entry whose
/// `created_at` moved and whose cached tokens did not.
fn entry(e: &MemoryEntry) -> Value {
    json!({
        "key": e.key,
        "value": e.value,
        "run_id": e.run_id,
        "step": e.step,
        "created_at": e.created_at,
        "kind": format!("{:?}", e.kind),
        "pinned": e.pinned,
    })
}

/// Everything 0.74.0 can see of one trace step, as JSON. Every column of the row, not a
/// summary of it: a step whose `tokens` survived and whose `tool_call` did not is a
/// store that lost data, and a comparison of the parts that are easy to compare would
/// pass.
///
/// The five latency columns 0.75.0 adds are deliberately absent — this binary's schema
/// has no name for them, which is the whole point. What it must not do is let their
/// presence on the row disturb the six columns it does read.
fn step(s: &StepRecord) -> Value {
    json!({
        "step": s.step,
        "decision": s.decision,
        "result": s.result,
        "prompt": s.prompt,
        "tool_call": s.tool_call,
        "tokens": s.tokens,
    })
}

/// Everything 0.74.0 can see of one run and its trace, as JSON.
fn run(store: &Store, run_id: i64) -> Res<Value> {
    Ok(json!({
        "id": run_id,
        "status": store.status(run_id)?,
        "outcome": store.outcome(run_id)?,
        "last_step": store.last_step(run_id)?,
        "steps": store.steps(run_id)?.iter().map(step).collect::<Vec<_>>(),
        // The canonical trace is one string over every step and context event, and it
        // is the crate's own answer to "is this the same trace" — see
        // `tests/determinism.rs`. Recorded whole so a reader that got every column
        // right and the ordering wrong still fails.
        "canonical_trace": store.canonical_trace(run_id)?,
        // 0.74.0's verdict, not the test's guess. `check_resumable` compares the file's
        // `user_version` against `CHECKPOINT_FORMAT`, so it is the one call that fails
        // first if a release bumps the format — and 0.75.0 claims not to have.
        "resumable": store.check_resumable(run_id).is_ok(),
    }))
}

/// Print everything 0.74.0 can see of `db`, as JSON on stdout.
///
/// Nothing here writes. Anything that opens and selects is fair game.
fn read(db: &str, workspace: &str) -> Res<()> {
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
        "reader": "io-harness 0.74.0",
        "checkpoint_format": io_harness::CHECKPOINT_FORMAT,
        // In `memory_list`'s order, which is a total order since 0.57.0 — recency, then
        // the key. Reported as a list rather than a map so that order is part of what
        // the test compares.
        "memory": store.memory_list(workspace)?.iter().map(entry).collect::<Vec<_>>(),
        "runs": runs,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

/// Write one memory entry the way 0.74.0 writes one, and report what happened.
///
/// A 0.74.0 `memory_put` is a single `INSERT ... ON CONFLICT DO UPDATE` against
/// `memory`. It knows nothing of `memory_token_cache`, so after this the store holds
/// either an entry with no cache line at all (a key that was not there) or an entry
/// whose cache line describes a value that is no longer stored (a key that was). Both
/// are states a 0.75.0 reader has to be right about, and both are produced here by the
/// binary that actually produces them in the field.
///
/// The run is started rather than reused: `memory.run_id` records which run wrote a
/// fact, and a write attributed to a run the older binary did not create would not be
/// the write the older binary performs.
fn remember(db: &str, workspace: &str, key: &str, value: &str) -> Res<()> {
    let store = Store::open(db)?;
    let run_id = store.start_run(GOAL, workspace)?;
    let evicted = store.memory_put(workspace, key, value, run_id, 1)?;
    let out = json!({
        "writer": "io-harness 0.74.0",
        "run_id": run_id,
        "key": key,
        // Reported rather than assumed: an eviction here would mean the entry the test
        // is about to ask after was pushed out by a cap, and a test that read back
        // `None` would blame the wrong thing.
        "evicted": evicted,
    });
    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}

fn main() -> Res<()> {
    let usage = "usage: gen-0-74-0 read <database> <workspace>\n       \
                 gen-0-74-0 remember <database> <workspace> <key> <value>";
    let owned: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = owned.iter().map(String::as_str).collect();
    match args[..] {
        ["read", db, workspace] => read(db, workspace),
        ["remember", db, workspace, key, value] => remember(db, workspace, key, value),
        _ => Err(usage.into()),
    }
}
