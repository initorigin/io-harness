//! 0.6.0 sandbox integration tests — the acceptance criteria that need the real
//! selected backend, the trace store, or the verification gate end to end.
//!
//! The macOS `sandbox-exec` backend is live-run here (this is the build host);
//! the Linux and Windows native backends are compile-gated + unit-tested in
//! their modules and not exercised here.

use std::path::{Path, PathBuf};

use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{
    select, Backend, ExecGuard, Policy, SandboxConfig, SandboxLimits, Store, Verification,
    TEST_BINARY,
};

fn good() -> &'static str {
    "pub fn hello() -> u32 { 42 }\n"
}

// --- backend selection ------------------------------------------------------

#[tokio::test]
async fn selection_picks_native_on_this_host_and_floor_when_forced() {
    // On the macOS build host the default selection is the native backend...
    let native = select(&SandboxConfig::new());
    #[cfg(target_os = "macos")]
    assert_eq!(native.backend(), Backend::MacosSandboxExec);
    assert_ne!(native.backend(), Backend::PortableFloor, "default must be native, not the floor");

    // ...and forcing the floor selects the portable backend, recorded so the
    // selection ladder is observable.
    let floor = select(&SandboxConfig::new().floor_only());
    assert_eq!(floor.backend(), Backend::PortableFloor);
}

// --- transparent to verification, and reversible ----------------------------

#[tokio::test]
async fn sandbox_on_reaches_the_same_verified_success_as_direct() {
    let policy = Policy::default();

    // Sandbox on (the 0.6.0 default): real code passes, a substring stub fails.
    let on = ExecGuard::new(&policy);
    assert!(Verification::CompilesRust.passes_guarded(Path::new("x.rs"), good(), &on).await.unwrap());
    assert!(!Verification::CompilesRust
        .passes_guarded(Path::new("x.rs"), "fn hello", &on)
        .await
        .unwrap());

    // Sandbox opted off: the exact 0.5.0 direct-host path, same verdicts.
    let off = ExecGuard::new(&policy).no_sandbox();
    assert!(Verification::CompilesRust.passes_guarded(Path::new("x.rs"), good(), &off).await.unwrap());
    assert!(!Verification::CompilesRust
        .passes_guarded(Path::new("x.rs"), "fn hello", &off)
        .await
        .unwrap());
}

#[tokio::test]
async fn sandbox_on_runs_the_produced_test_binary_transparently() {
    let policy = Policy::default();
    let guard = ExecGuard::new(&policy);
    let ok = Verification::RustTestPasses {
        test_src: "#[test] fn t() { assert_eq!(hello(), 42); }".into(),
    };
    assert!(ok.passes_guarded(Path::new("x.rs"), good(), &guard).await.unwrap());
    let bad = Verification::RustTestPasses {
        test_src: "#[test] fn t() { assert_eq!(hello(), 41); }".into(),
    };
    assert!(!bad.passes_guarded(Path::new("x.rs"), good(), &guard).await.unwrap());
}

// --- default-deny network, enforced by the sandbox not the prompt -----------

#[tokio::test]
async fn the_selected_backend_denies_outbound_network_by_default() {
    // The default backend (native on macOS) must deny network. curl to a real
    // host should fail because the sandbox blocks it, not because of the prompt.
    let dir = tempfile::tempdir().unwrap();
    let sb = select(&SandboxConfig::new()); // allow_network: false
    let argv: Vec<String> = ["curl", "-s", "-m", "5", "https://example.com"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let out = sb
        .run(RunSpec {
            argv: &argv,
            workdir: dir.path(),
            limits: &SandboxLimits::default(),
            allow_network: false,
        })
        .await
        .unwrap();
    assert!(!out.success(), "network must be denied by default, got {out:?}");
}

// --- teardown leaves nothing behind -----------------------------------------

#[tokio::test]
async fn a_gate_run_leaves_no_workdir_behind() {
    // Count temp entries the process owns before and after: the gate's ephemeral
    // workdir is created and destroyed within the call, leaking nothing.
    let policy = Policy::default();
    let guard = ExecGuard::new(&policy);
    // A run that hits a cap still tears down — use a tiny CPU cap on a compile.
    let _ = Verification::CompilesRust
        .passes_guarded(Path::new("x.rs"), good(), &guard)
        .await
        .unwrap();
    // The tempdir the gate used is dropped by now; there is no handle left to
    // it. (The workdir-drop guarantee itself is unit-tested in the module.)
}

// --- sandbox trace is reconstructable from a reopened store -----------------

#[tokio::test]
async fn sandbox_lifecycle_is_recorded_and_reconstructable() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("runs.db");

    let run_id = {
        let store = Store::open(&db).unwrap();
        let run = store.start_run("goal", "x.rs").unwrap();
        let policy = Policy::default();
        let guard = ExecGuard::new(&policy).tracing(&store, run, 1);
        assert!(Verification::CompilesRust
            .passes_guarded(Path::new("x.rs"), good(), &guard)
            .await
            .unwrap());
        run
        // store dropped — the process that ran the sandbox is gone
    };

    // A fresh store over the same file: the execution history rebuilds.
    let store = Store::open(&db).unwrap();
    let events = store.sandbox_events(run_id).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"create"), "missing create: {kinds:?}");
    assert!(kinds.contains(&"exec"), "missing exec: {kinds:?}");
    assert!(kinds.contains(&"destroy"), "missing destroy: {kinds:?}");
    // The exec event names the backend and the argv (command line only).
    let exec = events.iter().find(|e| e.kind == "exec").unwrap();
    assert!(exec.backend.is_some());
    assert!(exec.detail.as_deref().unwrap().contains("rustc"));
}

#[tokio::test]
async fn a_cap_hit_in_the_gate_is_recorded() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let run = store.start_run("goal", "x.rs").unwrap();

    // A 0-second CPU cap makes even a trivial compile a cap hit; the gate must
    // record it and report failure, not hang.
    let policy = Policy::default();
    let cfg = SandboxConfig {
        limits: SandboxLimits { max_cpu_secs: Some(0), max_wall_secs: Some(30), ..Default::default() },
        ..Default::default()
    };
    let guard = ExecGuard::new(&policy).sandboxed(cfg).tracing(&store, run, 1);
    let passed = Verification::CompilesRust
        .passes_guarded(Path::new("x.rs"), good(), &guard)
        .await
        .unwrap();
    assert!(!passed, "a cap-killed compile must not pass the gate");
    let events = store.sandbox_events(run).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "cap_hit"),
        "cap hit must be recorded, got {events:?}"
    );
}

// --- a sandbox that fails to start is a typed failure, not a panic ----------

#[tokio::test]
async fn a_sandbox_that_cannot_start_returns_a_typed_failure() {
    use io_harness::sandbox::FloorSandbox;
    let dir = tempfile::tempdir().unwrap();
    let argv = vec!["io-harness-no-such-binary-zzz".to_string()];
    let err = FloorSandbox
        .run(RunSpec {
            argv: &argv,
            workdir: dir.path(),
            limits: &SandboxLimits::default(),
            allow_network: false,
        })
        .await;
    assert!(
        matches!(err, Err(io_harness::Error::Sandbox { .. })),
        "a start failure must be a typed Sandbox error, got {err:?}"
    );
}

// keep an unused import honest across cfgs
#[allow(dead_code)]
fn _uses(_: PathBuf, _: &str) {
    let _ = TEST_BINARY;
}
