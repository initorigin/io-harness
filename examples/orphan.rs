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
//! - `orphan --leaf <pidfile> <middle-pid>` waits until the middle really is
//!   gone (see [`orphaned`], which asks a different question on each platform)
//!   and only then writes its own pid to `<pidfile>` and runs forever. The wait
//!   is what makes the fixture deterministic: when the pid file appears, the
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
//! Both platforms since 0.26.0, because both have the gap and neither has the
//! same mechanism for it. The shape of the chain is identical; only the leaf's
//! definition of "my parent is gone" differs, and it differs because the
//! operating systems genuinely do:
//!
//! - **unix** — the leaf is *reparented* to init, so `getppid` changing away from
//!   the middle's pid is a direct observation of the event that breaks a table
//!   walk. The containment that closes it is a process group.
//! - **Windows** — there is no reparenting and no process group. A process
//!   carries the pid of whatever created it forever, dead or not, which is
//!   exactly why `taskkill /T` cannot reach the leaf: it walks from the top to a
//!   middle that is no longer in the table and stops there. So the leaf waits for
//!   the middle to *leave the process table* instead, which is the same moment
//!   observed a different way. The containment that closes it is a Job Object.
//!
//! Run directly rather than as a test: `cargo run --example orphan /tmp/leaf.pid`.

/// How often the leaf asks whether it has been reparented yet, and how long the
/// two surviving processes sleep between doing nothing. Short enough that a test
/// is not waiting on it, long enough that neither process is a busy loop.
const POLL_MS: u64 = 20;

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
            while !orphaned(middle) {
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
fn forever() -> ! {
    let started = std::time::Instant::now();
    while started.elapsed() < std::time::Duration::from_secs(300) {
        std::thread::sleep(std::time::Duration::from_millis(POLL_MS));
    }
    std::process::exit(0);
}

/// Has the process that started this one gone?
///
/// The two implementations observe the same instant by different means, because
/// the platforms disagree about what happens to a child whose parent dies. See
/// the module docs.
#[cfg(unix)]
fn orphaned(middle: u32) -> bool {
    // Reparenting: the kernel moves this process to init, so the answer is
    // simply that its parent is no longer the one it was told to expect.
    std::os::unix::process::parent_id() != middle
}

#[cfg(windows)]
fn orphaned(middle: u32) -> bool {
    // No reparenting to observe — this process's recorded parent stays the
    // middle's pid forever, which is precisely the stale link that makes
    // `taskkill /T` miss it. So the question is asked of the process table
    // instead: the middle is gone when its pid is no longer in it.
    //
    // `tasklist` rather than `OpenProcess`, so the fixture needs no crate at all
    // and stays a plain example. The same command answers the same question in
    // `tests/handles.rs`.
    let out = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {middle}"), "/NH"])
        .output();
    match out {
        Ok(o) => !String::from_utf8_lossy(&o.stdout).contains(&middle.to_string()),
        // A `tasklist` that will not run answers nothing, and answering "yes,
        // orphaned" there would publish the pid file before the scenario the
        // test needs has happened. Keep waiting; the ceiling in `forever` bounds
        // it either way.
        Err(_) => false,
    }
}
