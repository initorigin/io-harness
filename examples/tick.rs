//! A process that keeps running and keeps printing — the fixture the process
//! handle tests need.
//!
//! `shell_start` exists for work that does not finish on its own, so proving it
//! works needs a program that does not finish on its own. There is no such
//! program that behaves the same on macOS, Linux and Windows: `tail -f` and
//! `sleep` are not on a Windows runner, `timeout` means two different things,
//! and a shell loop is exactly what this crate refuses to hand to a shell. So
//! the fixture is written here, in Rust, and is the same program everywhere.
//!
//! It prints `tick N` once every `TICK_MS` milliseconds and flushes each line,
//! because a line sitting in a buffer is indistinguishable from a process that
//! has not written it, and the whole point of a poll is to see output while the
//! process is still alive.
//!
//! Arguments: `tick [count]`. With no count it runs until it is killed, which is
//! the case the kill tests want. With one it exits after that many lines, which
//! is the case the self-exit test wants.
//!
//! Run directly rather than as a test: `cargo run --example tick 3`.

use std::io::Write;

/// Slow enough that a test can poll between two lines without racing, fast
/// enough that a test waiting for three of them is not slow.
const TICK_MS: u64 = 120;

fn main() {
    let limit: Option<u64> = std::env::args().nth(1).and_then(|a| a.parse().ok());
    let mut n: u64 = 0;
    loop {
        n += 1;
        println!("tick {n}");
        // Unbuffered on purpose: the reader is another process reading a file
        // this one is appending to, and an unflushed line is a line that has not
        // happened as far as that reader is concerned.
        let _ = std::io::stdout().flush();
        if let Some(limit) = limit {
            if n >= limit {
                return;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(TICK_MS));
    }
}
