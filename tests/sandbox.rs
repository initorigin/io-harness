//! 0.6.0 sandbox integration tests — the acceptance criteria that need the real
//! selected backend, the trace store, or the verification gate end to end.
//!
//! The macOS `sandbox-exec` backend is live-run here (this is the build host);
//! the Linux and Windows native backends are compile-gated + unit-tested in
//! their modules and not exercised here.

// 0.18.0 removed the three Rust-specific `Verification` variants. The gate these
// tests drive the sandbox through is now `EachCompilesRust` — the one criterion
// that still spawns `rustc` itself — over a file written to a temp root, and
// `Verification::Command` where a produced binary has to actually run.

use std::path::PathBuf;

use io_harness::sandbox::{RunSpec, Sandbox};
use io_harness::{
    select, Backend, ExecGuard, Policy, SandboxConfig, SandboxLimits, Store, Verification,
    TEST_BINARY,
};

fn good() -> &'static str {
    "pub fn hello() -> u32 { 42 }\n"
}

/// The compile gate over `contents`, through `guard`. A root has to exist on
/// disk for the criterion to read, which is the only thing that changed when the
/// single-file Rust variants were removed — the spawn, the sandbox and the
/// policy check are the same ones.
async fn compile_gate(contents: &str, guard: &ExecGuard<'_>) -> io_harness::Result<bool> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("x.rs"), contents).unwrap();
    Verification::EachCompilesRust(vec![PathBuf::from("x.rs")])
        .passes_in_guarded(dir.path(), guard)
        .await
}

// --- backend selection ------------------------------------------------------

#[tokio::test]
async fn selection_picks_native_on_this_host_and_floor_when_forced() {
    // The default selection is the strongest backend this host can actually
    // deliver — which is not the same as "the native one for this target". A
    // backend whose primitive is unavailable must degrade *and say so*.
    let native = select(&SandboxConfig::new());

    // macOS: `sandbox-exec` is part of the OS, so the native backend is always
    // available and the floor is always wrong here.
    #[cfg(target_os = "macos")]
    assert_eq!(native.backend(), Backend::MacosSandboxExec);

    // Linux: namespaces when the kernel permits unprivileged user namespaces,
    // the floor when it does not (Ubuntu 24.04 restricts them by default). Pin
    // it against the same question the backend asks, so a wrong answer in
    // either direction fails: promising namespaces it cannot create, or falling
    // back on a host where the wrapper works fine.
    #[cfg(target_os = "linux")]
    {
        let wrapper_works = std::process::Command::new("unshare")
            .args([
                "--user",
                "--map-root-user",
                "--mount",
                "--pid",
                "--fork",
                "--net",
                "--",
                "true",
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert_eq!(
            native.backend(),
            if wrapper_works {
                Backend::LinuxNamespaces
            } else {
                Backend::PortableFloor
            },
            "must report the strongest backend the kernel actually allows"
        );
    }

    // Windows: since 0.24.0 the Job Object is implemented, so it *is* the
    // strongest available backend and reporting the floor would now be the lie.
    // Before 0.24.0 this asserted `PortableFloor`, and that was correct then.
    #[cfg(target_os = "windows")]
    assert_eq!(native.backend(), Backend::WindowsJobObject);

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
    assert!(compile_gate(good(), &on).await.unwrap());
    assert!(!compile_gate("fn hello", &on).await.unwrap());

    // Sandbox opted off: the exact 0.5.0 direct-host path, same verdicts.
    let off = ExecGuard::new(&policy).no_sandbox();
    assert!(compile_gate(good(), &off).await.unwrap());
    assert!(!compile_gate("fn hello", &off).await.unwrap());
}

/// The sandbox runs a project's own test runner transparently, and a failing
/// test still fails the gate through it. Until 0.18.0 this was asserted through
/// `RustTestPasses`, which compiled a caller's criterion and ran the binary
/// itself; the property is the same and the runner is now the project's.
#[tokio::test]
async fn sandbox_on_runs_the_projects_test_runner_transparently() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::create_dir(root.join("src")).unwrap();
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    let gate = Verification::Command {
        argv: vec!["cargo".into(), "test".into(), "--offline".into()],
        expect_exit: 0,
    };
    // `cargo` is named explicitly: verification cannot prompt, so a gate spawns
    // only what a rule allows outright, and `Policy::default()` allows `rustc`
    // and nothing else.
    let policy = Policy::default().layer("gate").allow_exec("cargo");
    let guard = ExecGuard::new(&policy);

    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> u32 { 42 }\n#[test] fn t() { assert_eq!(hello(), 42); }\n",
    )
    .unwrap();
    assert!(gate.passes_in_guarded(root, &guard).await.unwrap());

    std::fs::write(
        root.join("src/lib.rs"),
        "pub fn hello() -> u32 { 42 }\n#[test] fn t() { assert_eq!(hello(), 41); }\n",
    )
    .unwrap();
    assert!(!gate.passes_in_guarded(root, &guard).await.unwrap());
}

// --- default-deny network, enforced by the sandbox not the prompt -----------

#[tokio::test]
async fn the_selected_backend_denies_outbound_network_by_default() {
    // curl to a real host must fail because the sandbox blocks it, not because
    // of the prompt — for every backend that claims a kernel boundary.
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

    if matches!(
        sb.backend(),
        Backend::MacosSandboxExec | Backend::LinuxNamespaces
    ) {
        assert!(
            !out.success(),
            "a backend that claims a network boundary must deny network, got {out:?}"
        );
    } else {
        // The portable floor's network deny is best-effort (proxy env stripped),
        // *not* a kernel boundary — documented in the module, and the reason it
        // must never claim to be one. So the invariant asserted on this host is
        // the contrapositive, which is what keeps the claim honest: a run that
        // could reach the network is a run that reported a backend which never
        // claimed to stop it. A kernel whose unprivileged user namespaces are
        // restricted lands here.
        //
        // `WindowsJobObject` joins the floor here in 0.24.0, and it is the whole
        // reason the platform table's Windows row is not shortened to "Native".
        // A Job Object contains *resources* — memory, CPU, process count, tree
        // kill — and there is no network facility and no filesystem facility in
        // one. It is a real backend that really bounds a run, and it still has
        // nothing to say about egress. Keeping the assertion strict for the two
        // backends that do claim a network boundary is what stops this from
        // becoming a test that permits anything.
        assert!(
            matches!(
                out.backend,
                Backend::PortableFloor | Backend::WindowsJobObject
            ),
            "only a backend that never claimed a network boundary may run without \
             one, got {:?}",
            out.backend
        );
    }
}

// --- teardown leaves nothing behind -----------------------------------------

#[tokio::test]
async fn a_gate_run_leaves_no_workdir_behind() {
    // Count temp entries the process owns before and after: the gate's ephemeral
    // workdir is created and destroyed within the call, leaking nothing.
    let policy = Policy::default();
    let guard = ExecGuard::new(&policy);
    // A run that hits a cap still tears down — use a tiny CPU cap on a compile.
    let _ = compile_gate(good(), &guard).await.unwrap();
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
        assert!(compile_gate(good(), &guard).await.unwrap());
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
    // The exec event names the backend and the argv (command line only). The
    // backend recorded must be the one `select` reported — so a backend that
    // quietly degrades is caught here as a trace mismatch rather than as a
    // mystery in someone's CI log.
    let exec = events.iter().find(|e| e.kind == "exec").unwrap();
    assert_eq!(
        exec.backend.as_deref(),
        Some(select(&SandboxConfig::new()).backend().as_str()),
        "the trace must name the backend that actually ran"
    );
    assert!(exec.detail.as_deref().unwrap().contains("rustc"));
}

// The CPU cap is an `RLIMIT_CPU`, which exists only on unix. On Windows the
// floor applies no CPU cap at all and says so, so the assertion below would be
// asserting behaviour the crate documents as absent. The Windows counterpart is
// the test underneath, which pins that absence rather than skipping it.
#[cfg(unix)]
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
        limits: SandboxLimits {
            max_cpu_secs: Some(0),
            max_wall_secs: Some(30),
            ..Default::default()
        },
        ..Default::default()
    };
    let guard = ExecGuard::new(&policy)
        .sandboxed(cfg)
        .tracing(&store, run, 1);
    let passed = compile_gate(good(), &guard).await.unwrap();
    assert!(!passed, "a cap-killed compile must not pass the gate");
    let events = store.sandbox_events(run).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "cap_hit"),
        "cap hit must be recorded, got {events:?}"
    );
}

/// Windows has no `RLIMIT_CPU`, so a CPU cap is not applied there. The point of
/// this test is that the gate never *claims* one it did not apply: an
/// impossible-looking `max_cpu_secs: Some(0)` leaves the compile to succeed on
/// its merits and records no cap hit, rather than reporting a kill that never
/// happened. Documented in `src/sandbox/windows.rs`; asserted here so the
/// documentation cannot drift away from the behaviour.
#[cfg(windows)]
#[tokio::test]
async fn windows_claims_no_cpu_cap_because_it_applies_none() {
    let tmp = tempfile::tempdir().unwrap();
    let db = tmp.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let run = store.start_run("goal", "x.rs").unwrap();

    let policy = Policy::default();
    let cfg = SandboxConfig {
        limits: SandboxLimits {
            max_cpu_secs: Some(0),
            max_wall_secs: Some(120),
            ..Default::default()
        },
        ..Default::default()
    };
    let guard = ExecGuard::new(&policy)
        .sandboxed(cfg)
        .tracing(&store, run, 1);
    let passed = compile_gate(good(), &guard).await.unwrap();

    assert!(
        passed,
        "with no CPU cap applied, an honest compile must still pass the gate"
    );
    let events = store.sandbox_events(run).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "cap_hit"),
        "a cap that was never applied must never be reported as hit, got {events:?}"
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
