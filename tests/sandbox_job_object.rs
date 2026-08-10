//! 0.24.0 — the Windows Job Object backend, live.
//!
//! Since 0.47.0 a contained Windows run takes the AppContainer when a profile can
//! be created, and the Job Object otherwise. The job is created either way — the
//! container path adopts the suspended process into one before resuming it — so
//! every cap asserted below is asserted on whichever backend the host gave, and
//! the tests name the disjunction rather than a single backend.
//!
//! Every test here is `cfg(windows)`, so on a macOS or Linux host this file
//! compiles to nothing and is silent rather than skipped-and-green. It runs on
//! the Windows CI runner and nowhere else, which is the only place the kernel
//! primitive under test exists.
//!
//! These drive the **sandbox's own spawn path** — `select()` plus
//! `Sandbox::run` — because that is the whole surface the job object has today.
//! The process-handle tools (`shell_start` / `shell_kill`) are a separate piece
//! of work; when they land, the tree-kill criterion needs a second test that
//! kills a *handle* rather than letting the run end, and asserts the same
//! grandchild is gone. The containment proved here is what that test will rest
//! on: it is the job handle closing that kills the tree, and a handle-based kill
//! closes the same handle.
//!
//! The payloads are batch files written into a directory the test owns, rather
//! than command lines. `cmd.exe` argument quoting is its own small horror, and a
//! test that fails because of it fails for a reason that has nothing to do with
//! job objects.
#![cfg(windows)]

use std::path::Path;
use std::time::Duration;

use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{select, Backend, Cap, SandboxConfig, SandboxLimits, SandboxOutcome};

/// Write `body` as `name.bat` in `dir` and hand back its absolute path.
fn bat(dir: &Path, name: &str, body: &str) -> String {
    let path = dir.join(format!("{name}.bat"));
    std::fs::write(&path, format!("@echo off\r\n{body}\r\n")).unwrap();
    path.display().to_string()
}

/// Run one batch file under `limits`, through whichever backend `config` picks.
async fn run_bat(config: &SandboxConfig, limits: SandboxLimits, script: &str) -> SandboxOutcome {
    let workdir = tempfile::tempdir().unwrap();
    let argv = vec!["cmd".to_string(), "/c".to_string(), script.to_string()];
    select(config)
        .run(RunSpec::new(&argv, workdir.path(), &limits).with_network(config.allow_network))
        .await
        .unwrap()
}

/// The default caps with a generous wall clock, so that when a test asserts a
/// cap it is asserting on the mechanism it meant to test. A wall-clock kill that
/// passed for a CPU or memory breach would make every one of these tests vacuous,
/// which is exactly the failure the assertions below are shaped to catch.
fn limits() -> SandboxLimits {
    SandboxLimits {
        max_cpu_secs: None,
        max_wall_secs: Some(90),
        max_memory_bytes: None,
        max_processes: None,
        max_open_files: None,
    }
}

// --- what the backend says it is --------------------------------------------

/// 0.47.0 changed this test's premise, deliberately: a contained Windows run now
/// takes the AppContainer where a profile can be created and the Job Object
/// otherwise, so "the native backend is the Job Object" is no longer a fact about
/// the platform. What is still exactly assertable — and is the claim this test was
/// written for in 0.24.0 — is that the native backend is a **real primitive** and
/// never the floor, and that the floor is still reachable on demand so the
/// negative controls below have something to run against.
///
/// The run's own backend is asserted against the same disjunction rather than
/// against the probe's answer: `Sandbox::backend` answers before the run and
/// `SandboxOutcome::backend` is what applied, and on this platform they may
/// legitimately differ by the container declining a particular run.
#[tokio::test]
async fn windows_reports_a_real_primitive_and_not_the_floor() {
    let native = select(&SandboxConfig::new()).backend();
    assert!(
        matches!(
            native,
            Backend::WindowsAppContainer | Backend::WindowsJobObject
        ),
        "the native Windows backend must name a primitive it actually creates, got {native:?}"
    );
    // And the floor is still reachable on demand — the negative control below
    // depends on being able to ask for a run with no job at all.
    assert_eq!(
        select(&SandboxConfig::new().floor_only()).backend(),
        Backend::PortableFloor
    );

    let dir = tempfile::tempdir().unwrap();
    let script = bat(dir.path(), "ok", "echo hello");
    let out = run_bat(&SandboxConfig::new(), limits(), &script).await;
    assert!(out.success(), "a plain command must still run: {out:?}");
    assert!(
        matches!(
            out.backend,
            Backend::WindowsAppContainer | Backend::WindowsJobObject
        ),
        "the run must report the primitive that applied to it, got {out:?}"
    );
    assert!(out.stdout.contains("hello"), "output is still captured");
}

/// A contained run must be able to execute a program resolved from `PATH`.
///
/// This is the whole of the Windows half seen from the outside: an AppContainer
/// denies everything not granted, so a payload it cannot load or whose own
/// toolchain it cannot reach fails in a way that looks like the payload being
/// broken. The crate's four `verify` gates found it first — they run `rustc`
/// against a scratch file and every one of them failed on `windows-latest` the
/// moment the container was actually selected.
///
/// `rustc` is the program deliberately: it is on `PATH` rather than beside the
/// test, it is what the crate's own verification gate runs, and on a rustup
/// installation it is a shim that starts a second binary somewhere else — which
/// is exactly the case a grant set derived from one directory has to answer.
///
/// The failure message carries the whole outcome, because on this backend the
/// payload's stderr arrives merged into `stdout` and it is the only thing that
/// says *which* access was refused.
#[tokio::test]
async fn a_contained_run_can_execute_a_program_from_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let argv = vec!["rustc".to_string(), "--version".to_string()];
    let out = select(&SandboxConfig::new())
        .run(RunSpec::new(&argv, dir.path(), &limits()).with_network(false))
        .await
        .unwrap();
    assert!(
        out.success(),
        "a contained run must be able to start a program on PATH — backend {:?}, \
         exit {:?}, merged output {:?}",
        out.backend,
        out.exit_code,
        out.stdout
    );
    assert!(
        out.stdout.contains("rustc"),
        "the payload's own output must come back: {out:?}"
    );
}

/// The crate's **own** verification gate, contained, with its own output.
///
/// `cargo test --offline` is what `tests/policy.rs`, `tests/sandbox.rs` and four
/// `verify` unit tests all use as their criterion, and every one of them fails on
/// `windows-latest` under the container while a `rustc` gate in the same shape
/// passes. Those tests assert a boolean, so the run's own account of what it could
/// not reach is discarded exactly where it is needed.
///
/// This runs the same command through the same backend and puts the merged output
/// in the failure message. It is not a diagnostic bolted on beside the release:
/// "the crate's own gate runs contained on this platform" is the release's claim,
/// and until now nothing asserted it with the evidence attached.
#[tokio::test]
async fn the_crates_own_cargo_gate_runs_contained_and_says_what_it_could_not_reach() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> u32 { 42 }\n#[test] fn t() { assert_eq!(hello(), 42); }\n",
    )
    .unwrap();

    // The roots the run's own gate resolves, so this is the gate's real
    // configuration rather than a stricter one invented here.
    let roots: Vec<std::path::PathBuf> = io_harness::toolchain::detect(root)
        .map(|tc| tc.cache_dirs())
        .unwrap_or_default()
        .into_iter()
        .filter(|p| p.is_dir())
        .collect();
    let limits = SandboxLimits {
        max_wall_secs: Some(600),
        ..limits()
    };
    let argv = vec![
        "cargo".to_string(),
        "test".to_string(),
        "--offline".to_string(),
    ];
    let out = select(&SandboxConfig::new())
        .run(
            RunSpec::new(&argv, root, &limits)
                .with_network(false)
                .with_writable_roots(&roots),
        )
        .await
        .unwrap();

    assert!(
        out.success(),
        "the crate's own gate could not run contained — backend {:?}, exit {:?}, \
         writable roots {:?}, merged output:\n{}",
        out.backend,
        out.exit_code,
        roots,
        out.stdout
    );
}

/// A contained run must not require a multi-threaded runtime, and this is
/// asserted rather than left to whichever other test happens to be a
/// `#[tokio::test]`.
///
/// `#[tokio::test]` builds a current-thread runtime by default, and so does an
/// embedder writing `#[tokio::main(flavor = "current_thread")]`. The container
/// path waited for its process with `tokio::task::block_in_place`, which *panics*
/// on that flavour. It stayed invisible for as long as the container was declined
/// on every host; the first CI run that selected it panicked four `verify` tests
/// that have nothing to do with containment. A backend may not impose a runtime
/// flavour on the process embedding it.
#[tokio::test(flavor = "current_thread")]
async fn a_contained_run_completes_on_a_current_thread_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let script = bat(dir.path(), "flavour", "echo current-thread");
    let out = run_bat(&SandboxConfig::new(), limits(), &script).await;
    assert!(
        out.success(),
        "a contained run must complete on a current-thread runtime: {out:?}"
    );
    assert!(out.stdout.contains("current-thread"), "{out:?}");
}

// --- F8: the active-process limit -------------------------------------------

/// The first platform on which `max_processes` means anything. The payload tries
/// to start one child; with the job holding the limit at one process — itself —
/// that `CreateProcess` is refused and the run fails within milliseconds.
#[tokio::test]
async fn the_process_limit_stops_the_run_and_the_job_is_what_says_so() {
    let dir = tempfile::tempdir().unwrap();
    let script = bat(
        dir.path(),
        "spawn",
        "cmd /c exit /b 0\r\nexit /b %errorlevel%",
    );

    let capped = run_bat(
        &SandboxConfig::new(),
        SandboxLimits {
            max_processes: Some(1),
            ..limits()
        },
        &script,
    )
    .await;

    assert!(!capped.success(), "the spawn must have been refused");
    assert_eq!(
        capped.cap_hit,
        Some(Cap::Processes),
        "the failure has to be attributed to the process limit, not left blank: {capped:?}"
    );
    assert_ne!(
        capped.cap_hit,
        Some(Cap::Wall),
        "a ninety-second wall clock cannot be what stopped a run that failed at once"
    );

    // The control that makes the assertion above mean something: the identical
    // script with no process limit runs fine, so what failed was the limit and
    // not the script.
    let free = run_bat(&SandboxConfig::new(), limits(), &script).await;
    assert!(
        free.success(),
        "without the limit the same payload must succeed: {free:?}"
    );
    assert_eq!(free.cap_hit, None);
}

// --- F9: memory and CPU are real bounds -------------------------------------

/// The job refuses the commit that would cross the cap, so the payload never
/// holds more than it was allowed and dies of its own failed allocation. The
/// chunked loop is deliberate: one enormous allocation would be refused while the
/// process still sat at its baseline, and the job would have accounted for a peak
/// nowhere near the limit it enforced.
#[tokio::test]
async fn the_memory_limit_is_a_real_bound_and_is_named_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let script = bat(
        dir.path(),
        "grow",
        "powershell -NoProfile -NonInteractive -Command \
         \"$ErrorActionPreference='Stop'; \
         $held = New-Object System.Collections.ArrayList; \
         while ($true) { [void]$held.Add((New-Object byte[] 16777216)) }\"\r\n\
         exit /b %errorlevel%",
    );

    let out = run_bat(
        &SandboxConfig::new(),
        SandboxLimits {
            max_memory_bytes: Some(512 * 1024 * 1024),
            ..limits()
        },
        &script,
    )
    .await;

    assert!(!out.success(), "an unbounded allocator must not succeed");
    assert_eq!(
        out.cap_hit,
        Some(Cap::Memory),
        "the memory cap is what stopped this, and the trace has to say so: {out:?}"
    );
}

/// A spin is the case the wall clock can also catch, which is why the wall cap
/// here is ninety seconds and the CPU cap is two: if the assertion passes, it
/// passed on the CPU mechanism.
#[tokio::test]
async fn the_cpu_limit_is_a_real_bound_and_is_named_as_one() {
    let dir = tempfile::tempdir().unwrap();
    let script = bat(
        dir.path(),
        "spin",
        "powershell -NoProfile -NonInteractive -Command \"while ($true) { }\"\r\n\
         exit /b %errorlevel%",
    );

    let out = run_bat(
        &SandboxConfig::new(),
        SandboxLimits {
            max_cpu_secs: Some(2),
            ..limits()
        },
        &script,
    )
    .await;

    assert!(!out.success(), "an infinite spin must not succeed");
    assert_eq!(
        out.cap_hit,
        Some(Cap::Cpu),
        "the job's CPU allotment is what ran out, not the wall clock: {out:?}"
    );
}

// --- F6: killing kills the tree, and the control that proves it -------------

/// Build the three-generation payload. The root batch starts a detached
/// grandchild and exits immediately, which is precisely the shape `taskkill /T`
/// cannot handle: by the time the kill runs there is no parent link left to walk
/// from the root to the grandchild.
///
/// The grandchild waits, then writes a sentinel. The sentinel existing after the
/// run means it was still alive to write it.
fn three_generations(dir: &Path) -> (String, std::path::PathBuf) {
    let sentinel = dir.join("grandchild-was-alive.txt");
    // `ping` rather than `timeout`, because `timeout` refuses to run with stdin
    // redirected and the sandbox redirects stdin to null.
    let inner = bat(
        dir,
        "inner",
        &format!(
            "ping -n 6 127.0.0.1 >nul\r\necho alive> \"{}\"",
            sentinel.display()
        ),
    );
    let outer = bat(dir, "outer", &format!("start \"\" /b cmd /c \"{inner}\""));
    (outer, sentinel)
}

#[tokio::test]
async fn closing_the_job_kills_the_grandchild() {
    let dir = tempfile::tempdir().unwrap();
    let (outer, sentinel) = three_generations(dir.path());

    run_bat(
        &SandboxConfig::new(),
        SandboxLimits {
            max_wall_secs: Some(2),
            ..limits()
        },
        &outer,
    )
    .await;

    // Well past the grandchild's own delay: if it were alive it would have
    // written by now.
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(
        !sentinel.exists(),
        "the grandchild outlived the run — the job did not contain it"
    );
}

/// The negative control, and the reason the job object is worth a dependency at
/// all. Same payload, same timing, no job: the grandchild survives and writes.
///
/// Without this the test above proves nothing — a grandchild that was never
/// going to write, or that died of something unrelated, would pass it just as
/// happily.
#[tokio::test]
async fn without_the_job_the_grandchild_survives() {
    let dir = tempfile::tempdir().unwrap();
    let (outer, sentinel) = three_generations(dir.path());

    run_bat(
        &SandboxConfig::new().floor_only(),
        SandboxLimits {
            max_wall_secs: Some(2),
            ..limits()
        },
        &outer,
    )
    .await;

    tokio::time::sleep(Duration::from_secs(8)).await;
    assert!(
        sentinel.exists(),
        "the floor was expected to leave the grandchild running; if it did not, \
         this control no longer proves the job object is what contains it"
    );
}
