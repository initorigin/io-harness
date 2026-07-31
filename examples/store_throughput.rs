//! Measures durable store write throughput — the one number the 0.23.0
//! `rusqlite` 0.32 -> 0.40 bump (SQLite 3.46.0 -> 3.53.2) could plausibly move
//! without any test noticing, because a slower engine is still a correct one.
//!
//! It times [`Store::checkpoint_step`], not a synthetic INSERT: that is the call
//! a real run hammers. Every completed step goes through it, and it is the
//! expensive one by construction — two INSERTs wrapped in a transaction that is
//! committed to disk, so the durability guarantee the resume path depends on
//! costs a WAL commit per step. `record` writes one row and does not commit a
//! transaction of its own; measuring that would flatter the engine and measure
//! the wrong thing.
//!
//! The store is file-backed in a temp dir, never in-memory: `Store::memory()`
//! never touches the filesystem, so it would measure SQLite's page cache and
//! hide the exact layer — WAL commit and fsync — where an engine bump is most
//! likely to show up.
//!
//! Setup (opening the store, creating the run) is outside the timer; only the
//! checkpoint loop is inside it. The output line is machine-readable so the same
//! example can be run five times and the samples compared without re-reading
//! prose:
//!
//! ```text
//! store_throughput iterations=20000 elapsed_s=12.345678 writes_per_sec=1620.1
//! ```
//!
//! Usage: `cargo run --release --example store_throughput [iterations]`
//! (default 20000). No network, no API key, no provider — this example never
//! constructs a run loop, only the store underneath one.
//!
//! The comparison half lives in `tests/fixtures/throughput-0.22.0/`, a standalone
//! crate pinned to `io-harness =0.22.0` that runs this identical workload against
//! the old engine. Both sides must stay the same work: change the loop here and
//! the number stops being comparable to anything published before it.

use io_harness::{StepRecord, Store};
use std::time::Instant;

fn main() -> io_harness::Result<()> {
    let iterations: u32 = std::env::args()
        .nth(1)
        .map(|a| a.parse().expect("iterations must be a positive integer"))
        .unwrap_or(20_000);

    // A fresh directory per invocation, removed when `dir` drops. Reusing a
    // database across samples would measure a growing file rather than the same
    // workload five times.
    let dir = tempfile::tempdir().expect("temp dir");
    let store = Store::open(dir.path().join("throughput.db"))?;
    let run_id = store.start_run("measure store write throughput", "src/lib.rs")?;

    // A step payload sized like a real one rather than empty strings: a prompt of
    // a few hundred bytes and a tool call of a hundred or so. Empty columns would
    // measure transaction overhead alone and understate the row work every real
    // checkpoint does. Built once, outside the loop — this is a store benchmark,
    // not a `String` benchmark.
    let prompt = "goal: measure store write throughput\n\
                  file: src/lib.rs\n\
                  step: apply the edit the model asked for, then verify\n\
                  context: the previous step wrote the file and the verify command failed\n";
    let tool_call =
        r#"{"name":"edit_file","arguments":{"path":"src/lib.rs","old":"fn a()","new":"fn b()"}}"#;

    let start = Instant::now();
    for step in 1..=iterations {
        // The hot durable path: two INSERTs and a commit, per step, per run.
        store.checkpoint_step(
            run_id,
            &StepRecord::new(step, "edit_file", "applied the edit")
                .with_trace(prompt, tool_call, 1_280),
        )?;
    }
    let elapsed = start.elapsed();

    println!(
        "store_throughput iterations={} elapsed_s={:.6} writes_per_sec={:.1}",
        iterations,
        elapsed.as_secs_f64(),
        f64::from(iterations) / elapsed.as_secs_f64(),
    );
    Ok(())
}
