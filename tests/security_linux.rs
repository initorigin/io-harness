//! The Linux mount rungs, exercised live — 0.74.0's H8, H10 and L11.
//!
//! **These are the second line, not the first.** The assertions that decide
//! whether these findings are closed read the argv and the mount script the rungs
//! *build*, and they live in `src/sandbox/linux.rs` beside the functions that
//! build them, because those functions are crate-internal and because a shape
//! assertion runs on every host in the matrix. This file is what a real kernel
//! adds to that: it attempts the write, or the read, and requires it to be
//! refused — the distinction 0.40.0 paid for, where a mount's exit status said
//! the tree was read-only and a write into it landed anyway.
//!
//! ## Why every test here can skip, and why that is not a hole
//!
//! A criterion that is about one rung has to run on that rung. The chain picks
//! the strongest rung a host can serve, so on a kernel with Landlock ≥ 4 — a
//! stock Ubuntu 24.04, and the CI runner — the selected backend is
//! `LinuxLandlock` and neither mount rung is reachable through the public API at
//! all. The public API is deliberately the only seam these tests use: an
//! environment variable that pinned a rung would be an ambient, attacker-
//! reachable way to *downgrade* containment, which is a worse thing to ship than
//! a test that skips.
//!
//! So each test states which backend it got and returns when that backend is not
//! one of the two mount rungs. That skip is not treated as a CI failure here,
//! unlike `tests/exec_contained.rs`: the claim being asserted belongs to the
//! mount rungs specifically, and a runner that took Landlock has not failed it —
//! it has answered a different question, which Landlock's own tests ask.
#![cfg(target_os = "linux")]

use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{
    select, Backend, ExecMode, SandboxConfig, SandboxLimits, SandboxOutcome, Selected,
};

/// The chain's selected backend when it is one of the two rungs that confine by
/// building a mount namespace, and `None` — with the reason said out loud — when
/// it is anything else.
fn mount_rung(what: &str) -> Option<Selected> {
    let selected = select(&SandboxConfig::new());
    match selected.backend() {
        Backend::LinuxBubblewrap | Backend::LinuxNamespaces => Some(selected),
        // 0.80.0, O1 — a skip reads as a pass, and every one of these skipped on
        // every machine this crate has ever run on: `MOUNT_SETUP` carries H8's
        // fix, H10's and L11's, and the shell had never executed anywhere.
        //
        // The `mount-rungs` CI job makes Landlock unavailable **at the syscall**
        // — a seccomp profile answering `ENOSYS`, which is what a kernel built
        // without it answers — and sets this variable so that a chain landing
        // anywhere but a mount rung fails the job instead of printing a line
        // nobody reads. It is a test-only assertion in the shape of
        // `IO_HARNESS_EXPECT_BACKEND`: it cannot select a rung and cannot
        // weaken one, so it is not a downgrade an attacker could reach for —
        // which is the reason the job varies the kernel's answer rather than
        // this crate's configuration.
        other if std::env::var_os("IO_HARNESS_REQUIRE_MOUNT_RUNG").is_some() => panic!(
            "({what}): IO_HARNESS_REQUIRE_MOUNT_RUNG is set, so this leg exists to run the \
             mount rungs — and the chain selected {other:?}. Either Landlock is still \
             available to this process or another rung came first; skipping here would \
             report a pass for a suite that ran nothing."
        ),
        other => {
            eprintln!(
                "skipped ({what}): this host's chain selected {other:?}, which is not a \
                 mount rung — the assertion below is about what a mount namespace confines"
            );
            None
        }
    }
}

/// `/bin/sh -c <script>`, the same shape the rung-level tests in
/// `src/sandbox/linux.rs` use.
fn sh(script: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), script.into()]
}

async fn run(backend: &Selected, argv: &[String], workdir: &std::path::Path) -> SandboxOutcome {
    let limits = SandboxLimits::none();
    backend
        .run(
            RunSpec::new(argv, workdir, &limits)
                .with_network(true)
                .with_mode(ExecMode::WorkspaceWrite),
        )
        .await
        .expect("the rung must run the command")
}

/// H8 — a filesystem that is mounted separately from `/` is read-only too.
///
/// `/dev/shm` is a `tmpfs` mounted `1777` on every ordinary Linux host, so the
/// caller's own uid can write it — and the payload is uid 0 in a user namespace
/// mapped to that uid, so it writes with the caller's rights. On 0.73.0 the
/// namespace rung's whole filesystem confinement was `remount,bind,ro /`, which
/// changes the `/` mount and nothing else, so this write landed while the rung
/// reported that it confined writes.
///
/// The control is in the same test: a write *inside* the workspace must still
/// succeed, or "confined" would only mean "broken".
#[tokio::test]
async fn h8_a_separately_mounted_filesystem_is_read_only_too() {
    let Some(rung) = mount_rung("H8") else { return };
    let dir = tempfile::tempdir().unwrap();
    let target = format!("/dev/shm/io-harness-h8-{}", std::process::id());

    let escape = run(&rung, &sh(&format!("echo x > {target}")), dir.path()).await;
    assert!(
        !escape.success(),
        "a write to /dev/shm was permitted: {escape:?}"
    );
    assert!(
        !std::path::Path::new(&target).exists(),
        "and it landed — the rung reported a boundary it applied to `/` alone"
    );

    let inside = run(&rung, &sh("echo in > ./inside"), dir.path()).await;
    assert!(
        inside.success(),
        "a write inside the workspace must still land: {inside:?}"
    );
    assert!(dir.path().join("inside").exists());
}

/// H10, the release's acceptance criterion, on the two rungs this task owns:
/// from inside the sandbox, `/proc/<harness-pid>/environ` does not yield a value
/// that matches a provider key set in the parent's environment.
///
/// Asserted structurally, which is the stronger form: the harness's pid is not
/// in the run's pid namespace at all, so there is no `/proc/<harness-pid>` to
/// read and no value it could yield — for the provider keys the audit named and
/// for every other variable the process holds. On 0.73.0 the namespace rung
/// unshared a pid namespace and left the *host's* `procfs` mounted over it, and
/// the bubblewrap rung mounted a fresh `procfs` with no pid namespace for it to
/// belong to; both handed the payload the harness's environment.
///
/// The control matters as much as the assertion: `/proc/self/environ` must
/// still read, or a sandbox with no working `/proc` at all would pass this
/// having proven nothing. That control also shows what this finding does *not*
/// close — the child inherits the parent's environment directly, so its own
/// `environ` still holds whatever the harness was spawned with. Scrubbing that
/// is a change to the one place every backend's spawn converges, `run_capped_hooked`
/// in `src/sandbox.rs`, and not to either rung.
#[tokio::test]
async fn h10_the_harness_environ_is_not_reachable_from_inside_the_sandbox() {
    let Some(rung) = mount_rung("H10") else {
        return;
    };
    let dir = tempfile::tempdir().unwrap();
    let pid = std::process::id();
    // A value that is certainly in this process's environment and certainly not
    // in the sandbox's own working directory, so finding it in the output can
    // only mean the harness's `environ` was read.
    let marker = env!("CARGO_MANIFEST_DIR");

    let control = run(&rung, &sh("cat /proc/self/environ"), dir.path()).await;
    assert!(
        control.success(),
        "the payload's own /proc must work, or this test asserts nothing: {control:?}"
    );

    let read = run(&rung, &sh(&format!("cat /proc/{pid}/environ")), dir.path()).await;
    assert!(
        !read.success(),
        "the harness's own /proc entry was readable from inside the sandbox: {read:?}"
    );
    assert!(
        !read.stdout.contains(marker),
        "and it yielded the harness's environment"
    );
}

/// L11 — one run cannot reach another run's workspace.
///
/// `io_harness::sandbox::workdir` puts every run's ephemeral workspace inside the
/// system temporary directory, and on 0.73.0 both mount rungs bound the whole of
/// that directory writable — so every concurrent run could read and rewrite every
/// other run's workspace from inside its own sandbox, whatever its mode said. The
/// two directories here are exactly that shape: both are `tempfile::tempdir()`s,
/// which is the arrangement the crate itself produces.
#[tokio::test]
async fn l11_another_runs_workspace_is_not_writable_from_inside_this_one() {
    let Some(rung) = mount_rung("L11") else {
        return;
    };
    let mine = tempfile::tempdir().unwrap();
    let theirs = tempfile::tempdir().unwrap();
    let target = theirs.path().join("stolen");

    let out = run(
        &rung,
        &sh(&format!("echo x > {}", target.display())),
        mine.path(),
    )
    .await;
    assert!(
        !out.success(),
        "another run's workspace was writable from inside this one: {out:?}"
    );
    assert!(
        !target.exists(),
        "and the write landed: the system temporary directory is still granted whole"
    );

    // The control: a temporary file must still be creatable, because a toolchain
    // that cannot open one cannot run. `TMPDIR` is set by the rung to the one
    // place this run may write.
    let tmp = run(&rung, &sh("echo t > \"$TMPDIR/probe\""), mine.path()).await;
    assert!(
        tmp.success(),
        "the run's own temporary directory must be writable: {tmp:?}"
    );
}
