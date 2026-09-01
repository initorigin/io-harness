//! The class-level guard: a run measures its boundary instead of declaring it.
//!
//! C1 (macOS), H11 (Windows) and H12 (Linux) are three instances of one failure —
//! a backend reporting a containment it did not apply. All three are fixed
//! individually, and fixing three instances leaves the fourth one silent. These
//! tests are what make the fourth one loud: [`BoundaryProbe`] attempts a write and
//! a dial outside the boundary at run start, and the run's answers come from what
//! happened rather than from what the backend says about itself.
//!
//! **Nothing here asserts a duration.** Every arm asserts an outcome — a file that
//! is or is not there, a connection a listener did or did not receive, an
//! `Option<bool>` — and the one test that waits (`h13_`) can only go *green*
//! spuriously on a loaded runner, never red: it waits for a process it expects to
//! be dead and asserts the absence of what that process would have written.
//!
//! Two arms are host-shaped and say so out loud rather than passing quietly. A
//! host with no probe tool, or no directory outside the boundary to aim at, cannot
//! measure anything — and on such a host the *fail-closed* arms still run, because
//! answering `false` with no evidence is exactly what is being asserted.

use std::path::PathBuf;
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse};
use io_harness::sandbox::{copy_back, select, BoundaryProbe, Sandbox, SandboxConfig};
use io_harness::{
    run_with, ApproveAll, Backend, Error, ExecMode, Policy, Provider, SandboxEvent, Store,
    TaskContract,
};

/// The home directory the probe aims at, which is also what a sabotage arm has to
/// grant in order to break the confinement deliberately.
fn outside_dir() -> Option<PathBuf> {
    let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    let dir = PathBuf::from(std::env::var_os(key)?);
    (dir.is_absolute() && dir.is_dir()).then_some(dir)
}

/// The probe runs, records both arms, and the accessors answer from what it
/// recorded.
///
/// The load-bearing assertion is the last one, and it is the whole point of the
/// release's class-level guard: **where the probe could measure an arm, the
/// measurement must agree with what the backend claims.** A backend that names a
/// containment this host did not apply fails here, on the platform that has it,
/// instead of being discovered by an audit two releases later. On 0.73.0 there was
/// nothing to disagree with — `confines_writes()` was a `match` over a variant and
/// no code path ever tested it against the machine.
#[tokio::test]
async fn probe_measures_this_hosts_backend_and_the_claims_follow_the_measurement() {
    let config = SandboxConfig::new();
    let probe = BoundaryProbe::measure(&config, &[]).await;
    eprintln!("probe: {}", probe.trace_label());

    assert_eq!(probe.backend, select(&config).backend());
    // The accessors are the measurement and nothing else.
    assert_eq!(probe.confines_writes(), probe.write_refused == Some(true));
    assert_eq!(probe.denies_egress(), probe.dial_refused == Some(true));

    match probe.write_refused {
        Some(measured) => assert_eq!(
            measured,
            probe.backend.confines_writes(),
            "{} claims confines_writes()={} and this host measured {}",
            probe.backend.as_str(),
            probe.backend.confines_writes(),
            measured
        ),
        None => eprintln!("skipped the write arm: this host could not attempt it"),
    }
    match probe.dial_refused {
        Some(measured) => assert_eq!(
            measured,
            probe.backend.denies_egress(),
            "{} claims denies_egress()={} and this host measured {}",
            probe.backend.as_str(),
            probe.backend.denies_egress(),
            measured
        ),
        None => eprintln!("skipped the dial arm: this host could not attempt it"),
    }
    assert!(
        !probe.contradicts_claim(),
        "the backend named a containment this host did not apply: {}",
        probe.trace_label()
    );
}

/// Fail closed: an arm that did not run, and an arm that ran and saw the boundary
/// fail, both answer `false`.
///
/// The release's rule, at the one place it matters most. An unproven claim must
/// not read as a proven one, so `None` is not "probably fine" and it is not the
/// backend's declaration either — for every backend in the enum, including the
/// four that declare both boundaries.
#[test]
fn probe_that_could_not_run_never_claims_a_boundary() {
    for backend in [
        Backend::MacosSandboxExec,
        Backend::LinuxLandlock,
        Backend::LinuxBubblewrap,
        Backend::LinuxNamespaces,
        Backend::WindowsAppContainer,
        Backend::WindowsJobObject,
        Backend::PortableFloor,
    ] {
        let unmeasured = BoundaryProbe::unmeasured(backend);
        assert!(
            !unmeasured.confines_writes() && !unmeasured.denies_egress(),
            "{} claimed a boundary from a probe that never ran",
            backend.as_str()
        );
        // Unknown is not a contradiction — it is an absence of evidence, and it
        // has already cost the claim above.
        assert!(!unmeasured.contradicts_claim());
        assert!(unmeasured.trace_label().contains("unmeasured"));

        let failed = BoundaryProbe {
            backend,
            write_refused: Some(false),
            dial_refused: Some(false),
        };
        assert!(
            !failed.confines_writes() && !failed.denies_egress(),
            "{} claimed a boundary the probe watched fail",
            backend.as_str()
        );
        assert_eq!(
            failed.contradicts_claim(),
            backend.confines_writes() || backend.denies_egress(),
            "{} must report the gap between what it claims and what was measured",
            backend.as_str()
        );
    }
}

/// **The sabotage arm.** Break the backend's confinement deliberately, and the
/// probe fails and the claim drops.
///
/// The hole is a real one and not a stub: the run grants the very directory the
/// probe aims at as a writable root, and permits egress. Under a native backend
/// the write then lands and the dial connects — the boundary is genuinely gone —
/// so a probe that is load-bearing answers `false` to both. A probe that were
/// decorative, answering from `Backend::confines_writes()`, would keep saying
/// `true` on macOS and on both Linux rungs and fail this test, which is exactly
/// what it is for.
///
/// The assertion holds on every platform because it is a negative: no host may
/// claim a boundary that was granted away. The *proof it is not vacuous* is the
/// second half — on a host that measured the boundary intact a moment ago, the
/// sabotaged probe must measure it broken.
#[tokio::test]
async fn breaking_a_backends_confinement_makes_the_probe_fail_and_the_claim_drop() {
    let Some(outside) = outside_dir() else {
        eprintln!("skipped: no home directory to aim the probe at on this host");
        return;
    };
    let baseline = BoundaryProbe::measure(&SandboxConfig::new(), &[]).await;

    let mut sabotaged_config = SandboxConfig::new();
    sabotaged_config.allow_network = true;
    let roots = vec![outside];
    let sabotaged = BoundaryProbe::measure(&sabotaged_config, &roots).await;
    eprintln!(
        "baseline: {} / sabotaged: {}",
        baseline.trace_label(),
        sabotaged.trace_label()
    );

    assert!(
        !sabotaged.confines_writes(),
        "confinement was granted away and the claim survived: {}",
        sabotaged.trace_label()
    );
    assert!(
        !sabotaged.denies_egress(),
        "egress was permitted and the denial survived: {}",
        sabotaged.trace_label()
    );

    // Not vacuous: where the boundary was measured intact, the sabotage is what
    // moved the answer, rather than the probe answering `false` to everything.
    if baseline.write_refused == Some(true) {
        assert_eq!(
            sabotaged.write_refused,
            Some(false),
            "the granted write must have landed, not gone unmeasured: {}",
            sabotaged.trace_label()
        );
        assert!(
            sabotaged.contradicts_claim(),
            "a measured failure under a backend that claims confinement is a contradiction"
        );
    }
}

/// H13 — a payload that forks twice does not outlive the run.
///
/// The kill set was built by walking `ppid` links from `ps`. The payload below
/// leaves a grandchild whose parent exits immediately, so by the time the wall
/// clock fires the grandchild has been reparented to pid 1 and the walk cannot
/// find it: on 0.73.0 it survived the run and wrote its marker four seconds later,
/// while the module header said nothing outlives the run. The fix is the process
/// group, which is inherited across `fork` and does not care who the parent is by
/// then.
///
/// The wait is not an assertion about time: it waits past the marker's own delay
/// and asserts the marker is *absent*. A runner too loaded to have written it yet
/// passes for the wrong reason; none can fail for the wrong reason.
#[cfg(unix)]
#[tokio::test]
async fn h13_the_wall_clock_kill_reaches_a_double_forked_grandchild() {
    // Imported here rather than at the top: these three are used by this test
    // alone, and an import used only by a `cfg(unix)` test is an unused import on
    // the Windows leg, which `-D warnings` fails on.
    use io_harness::sandbox::{FloorSandbox, RunSpec};
    use io_harness::SandboxLimits;

    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("survived");
    // `( ( ... ) & )` — the middle shell exits at once, orphaning the sleeper onto
    // pid 1; the outer shell stays alive so the wall clock is what ends the run.
    let argv = vec![
        "sh".into(),
        "-c".into(),
        "((sleep 4; touch survived) &) ; sleep 30".into(),
    ];
    let limits = SandboxLimits {
        max_wall_secs: Some(1),
        max_cpu_secs: None,
        ..SandboxLimits::default()
    };
    let out = FloorSandbox
        .run(RunSpec::new(&argv, dir.path(), &limits))
        .await
        .unwrap();
    assert!(!out.success(), "the wall clock must end this run: {out:?}");

    tokio::time::sleep(std::time::Duration::from_secs(6)).await;
    assert!(
        !marker.exists(),
        "a double-forked grandchild outlived the run that spawned it"
    );
}

/// L12 — `copy_back` refuses a path that would leave either root.
///
/// No in-crate caller passes such a path today, which is what makes this a
/// public-API hazard rather than a live hole: the predicate is handed the
/// *relative* path, so an application consulting its write policy about
/// `../../.ssh/authorized_keys` answers about one file and writes another.
#[tokio::test]
async fn l12_copy_back_refuses_a_path_that_leaves_the_destination_root() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    tokio::fs::write(src.path().join("ok.txt"), "y")
        .await
        .unwrap();

    for hostile in [
        PathBuf::from("..").join("escaped.txt"),
        PathBuf::from("a").join("..").join("..").join("escaped.txt"),
        outside.path().join("absolute.txt"),
    ] {
        let err = copy_back(
            src.path(),
            dst.path(),
            std::slice::from_ref(&hostile),
            |_| true,
        )
        .await
        .expect_err("a path that leaves the root must be refused, not skipped");
        assert!(
            matches!(err, Error::Refused { .. }),
            "expected a refusal for {}, got {err:?}",
            hostile.display()
        );
    }

    // And the ordinary path still works, so the guard refuses what escapes rather
    // than everything.
    let copied = copy_back(src.path(), dst.path(), &[PathBuf::from("ok.txt")], |_| true)
        .await
        .unwrap();
    assert_eq!(copied, vec![PathBuf::from("ok.txt")]);
    assert!(dst.path().join("ok.txt").exists());
}

/// L12's other half — a symbolic link in the sandbox workdir is not a file the
/// sandbox produced, and copying it would copy whatever it points at.
#[cfg(unix)]
#[tokio::test]
async fn l12_copy_back_refuses_a_symlinked_source() {
    let src = tempfile::tempdir().unwrap();
    let dst = tempfile::tempdir().unwrap();
    let secret = tempfile::tempdir().unwrap();
    let secret_file = secret.path().join("id_ed25519");
    tokio::fs::write(&secret_file, "PRIVATE").await.unwrap();
    std::os::unix::fs::symlink(&secret_file, src.path().join("key.txt")).unwrap();

    let files = [PathBuf::from("key.txt")];
    let err = copy_back(src.path(), dst.path(), &files, |_| true)
        .await
        .expect_err("a symlinked source must be refused");
    assert!(matches!(err, Error::Refused { .. }), "got {err:?}");
    assert!(
        !dst.path().join("key.txt").exists(),
        "nothing may be written for a refused path"
    );
}

/// A read-only mode is still a boundary, and `FullAccess` is honestly no boundary
/// at all — the one case the probe answers without spawning anything, because a
/// command under it is never wrapped.
#[tokio::test]
async fn probe_reports_full_access_as_the_absence_of_a_boundary() {
    let config = SandboxConfig::new().with_mode(ExecMode::FullAccess);
    let probe = BoundaryProbe::measure(&config, &[]).await;
    assert_eq!(probe.write_refused, Some(false));
    assert_eq!(probe.dial_refused, Some(false));
    assert!(!probe.confines_writes() && !probe.denies_egress());
}

// ------------------------------------------------- the run reads the probe
//
// Everything above measures the probe. These two measure the *wiring*: that a run
// records what it measured, and that the sentence the model is given about its own
// boundary is that measurement rather than the backend's declaration. A guard
// nothing consults is decoration, and until 0.74.0 wired this in, nothing did.

/// Keeps every system block it is sent and writes one file, which is the same
/// one-step script `tests/prompt.rs` drives its prompt assertions with.
#[derive(Default)]
struct FirstPrompt(Mutex<Vec<String>>);

impl Provider for FirstPrompt {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.0.lock().unwrap().push(req.system);
        Ok(CompletionResponse {
            tool_calls: vec![io_harness::provider::ToolCall {
                name: "write_file".into(),
                arguments: serde_json::json!({ "path": "a.txt", "content": "ok" }),
            }],
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "first-prompt"
    }
}

/// One contained run: what it was told about its boundary, and what its trace
/// holds.
async fn run_and_read(sandbox: Option<SandboxConfig>) -> (String, Vec<SandboxEvent>) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello\n").unwrap();
    let base = TaskContract::workspace("do the thing", dir.path()).with_max_steps(1);
    let contract = match sandbox {
        Some(config) => base.with_contained_exec(config),
        None => base.with_full_access(),
    };
    let provider = FirstPrompt::default();
    let store = Store::memory().unwrap();
    let run = run_with(
        &contract,
        &provider,
        &store,
        &Policy::default(),
        &ApproveAll,
    )
    .await
    .expect("the run reaches its step cap rather than failing");
    let system = provider.0.lock().unwrap()[0].clone();
    (system, store.sandbox_events(run.run_id).unwrap())
}

/// The line a run is given about what contains its commands.
fn containment_line(system: &str) -> &str {
    system
        .lines()
        .find(|l| l.starts_with("- Commands you run"))
        .expect("every run is told what contains its commands")
}

/// **The wiring, end to end.** The run records its probe, and the sentence it is
/// given agrees with the row.
///
/// The two halves of the acceptance criterion in one assertion each: *recorded in
/// the trace* is the row, whose `detail` names both attempts and how each one
/// ended rather than only naming a backend; and *the accessors reflect it* is the
/// last assertion, which ties the model's own sentence to that row on whatever
/// host this runs on. Neither half asserts a duration — the row either says
/// `refused` or it does not, and the sentence either claims containment or it does
/// not, and the test is that those two cannot disagree.
///
/// A `containment_line` still reading `Backend::confines_writes()` fails this on
/// any host where the probe cannot measure the write arm, and on any host where
/// the backend claims a confinement it did not deliver — which is the whole class
/// C1, H11 and H12 were three instances of.
#[tokio::test]
async fn a_run_records_its_probe_and_is_told_what_it_measured() {
    let config = SandboxConfig::new();
    let (system, events) = run_and_read(Some(config.clone())).await;

    let rows: Vec<&SandboxEvent> = events
        .iter()
        .filter(|e| e.kind == "boundary_probe")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "one probe row per run, written at run start: {events:?}"
    );
    assert_eq!(
        rows[0].backend.as_deref(),
        Some(select(&config).backend().as_str()),
        "the row names the backend that actually applied"
    );
    let detail = rows[0]
        .detail
        .as_deref()
        .expect("the row carries what was attempted");
    assert!(
        detail.contains("write-outside=") && detail.contains("dial-outside="),
        "a reader must see what was attempted and how it ended, not just a label: {detail}"
    );

    let line = containment_line(&system);
    assert_eq!(
        line.contains("are contained"),
        detail.contains("write-outside=refused"),
        "the boundary sentence and the probe row disagree:\n{line}\n{detail}"
    );
}

/// A run given the host's own privileges makes no containment claim, so it writes
/// no probe row — and says so in the sentence instead.
///
/// **Why the absence is right here and wrong for `report_containment`.** That
/// event answers "was this run contained", which every run has an answer to, so
/// an absent event would be an unanswered question. The probe answers something
/// narrower: it is *evidence for a claim*, and a `full-access` run claims
/// nothing — its boundary line says commands are not contained, and its
/// `Contained` event reports mode `full-access` and backend `none`. There is no
/// claim to check.
///
/// Recording one anyway would put a row in `sandbox_events` — the table of what a
/// sandbox did — for a run whose commands no sandbox ever wrapped, naming the
/// backend `select` *would* have chosen. Three tests have asserted since 0.46.0
/// that an uncontained run leaves that table empty
/// (`exec_contained::an_uncontained_command_records_no_sandbox_at_all`,
/// `exec_contained::the_escape_hatch_is_one_call_and_it_is_complete`,
/// `exec_mode::full_access_narrows_nothing_and_wraps_nothing`), and they are
/// asserting the same rule from the other side: no row may suggest a containment
/// that was not applied.
///
/// **Not vacuous.** The control is the contained run below, through the same
/// helper: it must write exactly one probe row. A skip that had swallowed every
/// run's row would fail there.
#[tokio::test]
async fn a_full_access_run_makes_no_claim_so_it_records_no_probe() {
    let (system, events) = run_and_read(None).await;

    assert!(
        events.is_empty(),
        "an uncontained run has no sandbox to describe and must leave no row \
         claiming one: {events:?}"
    );
    assert!(
        containment_line(&system).contains("not contained"),
        "and the absence is not silence — the run is told plainly: {system}"
    );

    // The control. The same helper, one contained run, and the row is there — so
    // the assertion above is the `full-access` case and not a probe that stopped
    // recording anything.
    let (_, contained) = run_and_read(Some(SandboxConfig::new())).await;
    let rows: Vec<&SandboxEvent> = contained
        .iter()
        .filter(|e| e.kind == "boundary_probe")
        .collect();
    assert_eq!(
        rows.len(),
        1,
        "a run with a boundary to check still records what it measured: {contained:?}"
    );
}

/// n5 — what the probe costs a run.
///
/// Prints and asserts nothing: a duration asserted on a CI runner is a flake, and
/// this number's job is to be read, not to gate. Run it with
/// `cargo nextest run --run-ignored ignored-only --success-output immediate -E 'test(n5_)'`.
#[tokio::test]
#[ignore = "measurement: prints a timing, asserts nothing"]
async fn n5_the_startup_probe_cost() {
    let config = SandboxConfig::new();
    let started = std::time::Instant::now();
    let probe = BoundaryProbe::measure(&config, &[]).await;
    let elapsed = started.elapsed();
    eprintln!(
        "probe cost: {:?} ({}), backend {}",
        elapsed,
        probe.trace_label(),
        probe.backend.as_str()
    );
    // Named so the reader knows what the number covers: one uncontained control
    // child and one contained child per arm.
    eprintln!("method: one control spawn + one contained spawn per measured arm");
}
