//! A process that leaves a grandchild behind whose parent is already dead — the
//! fixture the tree-kill test needs.
//!
//! `examples/tick.rs` proves a handle can be killed. It cannot prove the case
//! that actually breaks a kill, because it has no descendants: killing its pid
//! is the whole job. The shape that breaks a kill is the ordinary one a dev
//! server has — a process starts a second, the second starts a third, the second
//! exits — and it breaks it because every kill built on the process table walks
//! parent/child links, and the middle link is gone. The third process is still
//! the run's responsibility and there is no longer any path from the run to it.
//!
//! This program is that shape, on purpose, and it is written to be *hostile* to
//! a table walk rather than merely to happen to defeat one:
//!
//! - `orphan <pidfile>` is the top process. It starts the middle and then runs
//!   until it is killed, so the pid the harness recorded stays live and a test
//!   can tell "the kill reached the top" from "the kill reached the leaf".
//! - `orphan --middle <pidfile>` starts the leaf and returns immediately. It is
//!   alive for milliseconds and its only job is to stop being alive.
//! - `orphan --leaf <pidfile> <middle-pid>` waits until it has actually been
//!   reparented — until the kernel says its parent is no longer the middle — and
//!   only then writes its own pid to `<pidfile>` and runs forever. The wait is
//!   what makes the fixture deterministic: when the pid file appears, the
//!   parent/child link a walk would have followed is provably gone, so a test
//!   that kills after reading it is not racing the middle's exit.
//!
//! The middle's pid is passed down rather than read with `getppid` at startup,
//! because the middle is usually gone *before the leaf runs its first line* —
//! which is the whole design working, and which would make a leaf that compared
//! against whatever `getppid` said at startup wait for a change that had already
//! happened. Comparing against the pid it was told to expect has no such state
//! in it: the leaf is orphaned exactly when its parent is no longer that pid.
//!
//! Unix only, and it says so rather than pretending: the reparenting this turns
//! on is a POSIX behaviour, `getppid` is how it is observed, and the containment
//! it exercises is a process group. The Windows answer to the same problem is a
//! Job Object and is a different fixture.
//!
//! Run directly rather than as a test: `cargo run --example orphan /tmp/leaf.pid`.

/// How often the leaf asks whether it has been reparented yet, and how long the
/// two surviving processes sleep between doing nothing. Short enough that a test
/// is not waiting on it, long enough that neither process is a busy loop.
#[cfg(unix)]
const POLL_MS: u64 = 20;

#[cfg(unix)]
fn main() {
    let mut args = std::env::args().skip(1);
    let first = args.next().unwrap_or_default();
    let mut rest = args;
    match first.as_str() {
        "--middle" => {
            let pidfile = rest.next().expect("--middle takes a pid file path");
            spawn_self(&["--leaf", &pidfile, &std::process::id().to_string()]);
            // Returns at once. Nothing is waited for, and that is the point:
            // the leaf loses its parent while the top process is still running.
        }
        "--leaf" => {
            let pidfile = rest.next().expect("--leaf takes a pid file path");
            let middle: u32 = rest
                .next()
                .and_then(|a| a.parse().ok())
                .expect("--leaf takes the pid of the process that started it");
            while std::os::unix::process::parent_id() == middle {
                std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
            }
            announce(&pidfile);
            forever();
        }
        pidfile => {
            assert!(
                !pidfile.is_empty(),
                "orphan takes a pid file path for its leaf to announce itself in"
            );
            spawn_self(&["--middle", pidfile]);
            forever();
        }
    }
}

/// Start this same binary again with `args`, inheriting stdout, stderr and the
/// working directory — so a handle's capture file sees the whole family and a
/// kill that misses one of them is visible in the log as well as in the process
/// table.
///
/// Never waited on, which is the fixture's entire subject: the middle process
/// exits without anyone reaping it and the leaf is never awaited by anybody at
/// all. The zombie that leaves behind is welcome — a table walk from the top
/// process finds a dead middle with no children, which is exactly the misleading
/// picture a real dev-server tree presents to the kill this test is about.
#[cfg(unix)]
#[allow(clippy::zombie_processes)]
fn spawn_self(args: &[&str]) {
    let exe = std::env::current_exe().expect("a running program knows its own path");
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .expect("the orphan fixture could not start the next process in its chain");
}

/// Write this process's pid where the test can read it, atomically.
///
/// Written to a neighbouring path and renamed rather than written in place: a
/// reader polling for the file would otherwise be able to open it between the
/// create and the write and parse an empty string as a pid. `rename` within one
/// directory is atomic, so the file either is not there or is complete.
#[cfg(unix)]
fn announce(pidfile: &str) {
    let tmp = format!("{pidfile}.partial");
    std::fs::write(&tmp, std::process::id().to_string())
        .expect("the orphan fixture could not write its pid file");
    std::fs::rename(&tmp, pidfile).expect("the orphan fixture could not publish its pid file");
}

/// Run until something kills this process, or until the ceiling gives up on it.
///
/// The ceiling exists because this fixture's whole job is to outlive its caller,
/// so it will also outlive a caller that dies badly — a `SIGKILL`ed test binary
/// runs no `Drop` and kills nothing it started. The negative control here is
/// worse than most: it asserts that a grandchild SURVIVES a naive kill, so a
/// failure between that assertion and its cleanup leaks by construction. Left
/// alone these accumulate across runs. Far longer than any test that uses this,
/// short enough that a leak is measured in minutes rather than until reboot.
#[cfg(unix)]
fn forever() -> ! {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(300) {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
    std::process::exit(0);
}

#[cfg(not(unix))]
fn main() {
    // Compiled everywhere so `cargo build --all-targets` is the same command on
    // every host, and does nothing off unix so it cannot be mistaken for a
    // fixture that proves something here. See the module docs.
    eprintln!("the orphan fixture exercises POSIX reparenting and process groups; it does nothing on this platform");
}
