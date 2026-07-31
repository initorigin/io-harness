//! A process that prints far more than one poll's window and then keeps
//! running — the fixture the bounded-output tests need.
//!
//! `examples/tick.rs` exists because nothing portable both keeps running and
//! keeps printing; this exists for the same reason at the other end of the
//! scale. Proving that a handle nobody polls cannot exhaust memory needs a
//! program that produces much more output than the poll window, as fast as the
//! operating system will take it, and `yes`, `seq` and a shell loop are each
//! either absent on a Windows runner or exactly the construct this crate
//! refuses to hand to a shell. So the flood is written here, in Rust, and is the
//! same program everywhere.
//!
//! Two properties the tests depend on, both deliberate:
//!
//! * **It stops writing and stays alive.** The volume is an argument rather than
//!   "until killed", so the total is the test's choice and not the runner's
//!   disk's — a genuinely unbounded writer would put however many megabytes a
//!   loaded machine's scheduling happened to allow into a temporary file, and
//!   the bound under test is per-poll and does not depend on the total. Staying
//!   alive afterwards is what lets a test ask the operating system whether a
//!   handle that flooded is still killable.
//! * **The lines are fixed width.** `flood 000001` is not a prefix of any other
//!   line, so a test can assert that the *earliest* output is absent from a
//!   window without the assertion being satisfied by `flood 000010`.
//!
//! Arguments: `flood [lines]`, default one line. Run directly rather than as a
//! test: `cargo run --example flood 1000`.

use std::io::{BufWriter, Write};

fn main() {
    let lines: u64 = std::env::args()
        .nth(1)
        .and_then(|a| a.parse().ok())
        .unwrap_or(1);
    {
        // Buffered, unlike `tick`: this fixture is about producing bytes faster
        // than anything reads them, and a flush per line would measure the
        // write syscall instead. The flush at the end of the scope is what makes
        // the whole flood visible to the reader before the process goes quiet.
        let stdout = std::io::stdout();
        let mut w = BufWriter::new(stdout.lock());
        for n in 1..=lines {
            let _ = writeln!(w, "flood {n:06}");
        }
        let _ = w.flush();
    }
    // Silent, and still here. A handle that has finished flooding is still a
    // live process the run owns, which is the state the kill tests want.
    // Bounded for the reason tick.rs states: a fixture that outlives its caller
    // outlives a caller that died badly, and those accumulate across runs.
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(300) {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}
