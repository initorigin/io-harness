//! The OLD side of the 0.23.0 engine bump: the identical workload from the main
//! crate's `examples/store_throughput.rs`, run against `io-harness =0.22.0` and
//! therefore against `rusqlite` 0.32 with bundled SQLite 3.46.0. The new side
//! runs the same loop on `rusqlite` 0.40.1 with SQLite 3.53.2. Subtracting the
//! two medians is the only thing that answers "did the engine bump cost write
//! throughput", and it is only a valid subtraction while the two loops do the
//! same work — same call, same payload, same iteration count. Do not "improve"
//! this file; port any change to both sides or to neither.
//!
//! This crate is pinned to the previous release on purpose and must never be
//! bumped. See the note in `Cargo.toml`.
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
//! checkpoint loop is inside it. The output line is byte-compatible with the new
//! side's, so the samples can be compared without re-reading prose:
//!
//! ```text
//! store_throughput iterations=20000 elapsed_s=12.345678 writes_per_sec=1620.1
//! ```
//!
//! Usage, from THIS directory so it builds into its own `target/` and never
//! drags a second `libsqlite3-sys` into the main crate's build:
//! `cargo run --release [iterations]` (default 20000). No network, no API key.

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
