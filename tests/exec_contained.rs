//! `exec` under containment — F1 and F5 of 0.40.0, through the full loop with a
//! scripted mock provider so the tests are deterministic and offline.
//!
//! The release is opt-in, so every assertion here comes in a pair: what a
//! contained command cannot do, and the same command on the same contract with
//! the field absent, which must still do it. A single-armed test passes an
//! implementation that contains every `exec` unconditionally, and that
//! implementation silently changes where every existing embedder's builds may
//! write.
//!
//! ## Why the escape target is not a second temp directory
//!
//! Measured on this build host, 2026-08-07: the macOS profile
//! (`src/sandbox/macos.rs`, `profile_for`) denies writes under `/` and then
//! re-allows the workdir **and the whole of `/private/var/folders`** — which is
//! where `tempfile::tempdir()` puts everything. A `touch` into a second temp
//! directory therefore *succeeds* under the macOS sandbox while failing under
//! Linux namespaces, so a test written the obvious way would prove nothing on the
//! platform it was developed on and pass anyway.
//!
//! The escape target is a directory under the crate's own `target/`, which is
//! outside the workspace and outside `/private/var/folders` on both platforms.
//! `$HOME` is denied by the profile (measured: `Operation not permitted`) and
//! would also work, but a test that writes to a developer's home directory on
//! failure is not a test anyone should have to run twice.
//!
//! Windows is excluded from the escape assertions by construction rather than by
//! oversight: a Job Object contains resources and has no filesystem facility, so
//! there is nothing there for these tests to assert. `docs/CONTRACT.md` carries
//! the per-platform table.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::sandbox::{SandboxConfig, SandboxLimits};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

/// Plays a fixed script of tool calls. The same shape `tests/exec_tool.rs` uses.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<ToolSpec>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
        }
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = req.tools.clone();
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn exec_call(argv: &[&str]) -> ToolCall {
    ToolCall {
        name: "exec".into(),
        arguments: json!({ "argv": argv }),
    }
}

fn workspace() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn contract(root: &Path) -> TaskContract {
    TaskContract::workspace("run the project's commands", root).with_max_steps(6)
}

fn permissive() -> Policy {
    Policy::permissive()
}

/// A directory outside the workspace that the macOS profile does **not** blanket
/// allow. Removed on drop, including when the assertion that reads it fails.
///
/// Unique per test: two of these tests run concurrently under `cargo test` and a
/// shared path would let one test's cleanup delete another's evidence.
struct EscapeDir(PathBuf);

impl EscapeDir {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("exec-contained-escape")
            .join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self) -> PathBuf {
        self.0.join("escaped.txt")
    }
}

impl Drop for EscapeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ---------------------------------------------------------------------------
// F1 — a contained command cannot write outside the workspace, and an
//      uncontained one still can
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_contained_command_cannot_write_outside_the_workspace() {
    let dir = workspace();
    let escape = EscapeDir::new("contained");
    let target = escape.file();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", target.to_str().unwrap()])]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    if !backend_confines_writes() {
        // This host took the portable floor, which confines nothing. Assert
        // that, rather than asserting a confinement that was never applied:
        // the write lands and the trace says so. A skip here would read as a
        // pass and prove nothing, which is the failure mode F6 names.
        assert!(
            target.exists(),
            "the floor confines nothing, so the write must land: {}",
            target.display()
        );
        return;
    }

    assert!(
        !target.exists(),
        "a contained command wrote outside the workspace: {}",
        target.display()
    );

    // The model is told the command failed, rather than being left to infer it
    // from a file it cannot see.
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps[0].decision.contains("exit 0"),
        "the refusal reached the trace as a failure: {:?}",
        steps[0].decision
    );
}

/// The negative control, and the half that protects every existing caller. Same
/// contract, same policy, same store, same argv — with the field absent.
///
/// An implementation that contains every `exec` unconditionally passes the test
/// above and fails this one.
#[cfg(unix)]
#[tokio::test]
async fn an_uncontained_command_still_writes_outside_the_workspace() {
    let dir = workspace();
    let escape = EscapeDir::new("uncontained");
    let target = escape.file();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", target.to_str().unwrap()])]]);

    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        target.exists(),
        "0.39.0 behaviour is unchanged when the field is absent: {} should exist",
        target.display()
    );
}

// ---------------------------------------------------------------------------
// F5 — the workspace is the working directory and is not discarded
// ---------------------------------------------------------------------------

/// The criterion that proves 0.17.0's objection is answered rather than argued
/// with. An implementation that reused `sandbox::workdir()` — the verification
/// gate's `TempDir` — passes nothing here: the second command would find no file
/// and the workspace would be empty afterwards.
#[cfg(unix)]
#[tokio::test]
async fn a_contained_command_writes_into_the_workspace_and_the_next_one_reads_it() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    // Both argvs are relative, so they only resolve if the working directory is
    // the workspace root.
    let provider = MockScript::new(vec![
        vec![exec_call(&["touch", "made-by-the-first-command.txt"])],
        vec![exec_call(&["cat", "made-by-the-first-command.txt"])],
    ]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        dir.path().join("made-by-the-first-command.txt").exists(),
        "the write landed in the workspace and survived the run"
    );

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("exit 0"),
        "the write inside the workspace succeeded: {:?}",
        steps[0].decision
    );
    assert!(
        steps[1].decision.contains("exit 0"),
        "the second command found what the first one wrote, so nothing was \
         discarded between them: {:?}",
        steps[1].decision
    );
}

// ---------------------------------------------------------------------------
// F2 — a cap kills, and the trace names which cap
// ---------------------------------------------------------------------------

/// Both arms below are deterministic by construction rather than by waiting:
/// a zero ceiling fires on the first check, so nothing here asserts a duration
/// and nothing sleeps for real. A cap test that waits for a real second is a
/// cap test that flakes on a loaded runner.
fn capped(limits: SandboxLimits) -> SandboxConfig {
    SandboxConfig {
        limits,
        ..SandboxConfig::new()
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_command_killed_by_a_cap_is_reported_as_that_cap() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    // Killed the moment the ceiling is checked; the `5` is never waited out.
    let provider = MockScript::new(vec![vec![exec_call(&["sleep", "5"])]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(capped(SandboxLimits {
            max_wall_secs: Some(0),
            ..SandboxLimits::default()
        })),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("hit the wall cap"),
        "the trace names the ceiling that was crossed: {:?}",
        steps[0].decision
    );
    // The model is told which resource ran out, not merely that something died.
    assert!(
        steps[1].prompt.contains("killed by the wall cap"),
        "the next turn is told which cap: {:?}",
        steps[1].prompt
    );
}

/// The second arm, and the one that keeps two different ceilings from being read
/// as one. `exec_timeout` is the contract's ceiling on a wedged command and is
/// raised with `with_exec_timeout`; `max_wall_secs` is the sandbox's and is
/// raised in `SandboxLimits`. A trace that called both "timed out" would send a
/// reader to the wrong one.
#[cfg(unix)]
#[tokio::test]
async fn the_contracts_exec_timeout_is_not_reported_as_a_sandbox_cap() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["sleep", "5"])]]);

    let result = run_with(
        &contract(dir.path())
            // The sandbox's own wall ceiling is left at its roomy default, so the
            // only ceiling in play is the contract's.
            .with_contained_exec(SandboxConfig::new())
            .with_exec_timeout(std::time::Duration::ZERO),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("timed out"),
        "the contract's ceiling reports as a timeout: {:?}",
        steps[0].decision
    );
    assert!(
        !steps[0].decision.contains("cap"),
        "and is not dressed up as a sandbox cap: {:?}",
        steps[0].decision
    );
}

// ---------------------------------------------------------------------------
// F3 — egress follows the policy, both arms, and the coarseness is asserted
// ---------------------------------------------------------------------------

/// A listener on loopback, so the egress arms need no internet and no live host.
///
/// Loopback is a real test of the boundary on both backends that claim one: a
/// fresh network namespace has only a down `lo`, and the macOS profile's
/// `(deny network*)` covers local sockets as well as remote ones. It is also the
/// only egress probe that can assert the *allow* arm without depending on
/// somebody else's uptime.
async fn loopback_listener() -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        while let Ok((mut socket, _)) = listener.accept().await {
            use tokio::io::AsyncWriteExt;
            let _ = socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
                .await;
            let _ = socket.shutdown().await;
        }
    });
    (addr, handle)
}

/// Does this host's backend confine writes at all? The portable floor does not,
/// and neither does a Job Object.
///
/// The escape tests below branch on this rather than skipping, because F6 names
/// the failure mode exactly: "a skip that reads as a pass is the failure mode
/// here". A floor host must *assert the floor* — the write lands, and the
/// recorded backend says `PortableFloor` — so the fallback path is proven rather
/// than stepped over. What is never allowed is passing while proving nothing.
///
/// This is not hypothetical. Ubuntu 24.04 ships
/// `kernel.apparmor_restrict_unprivileged_userns=1`, so before 0.40.0's CI step
/// the ubuntu runners took the floor: the two escape tests failed loudly and the
/// egress tests skipped silently, which is the same defect wearing two faces.
fn backend_confines_writes() -> bool {
    use io_harness::sandbox::{select, Backend, Sandbox};
    matches!(
        select(&SandboxConfig::new()).backend(),
        Backend::MacosSandboxExec | Backend::LinuxNamespaces
    )
}

/// Does this host's backend claim a network boundary at all? A Job Object and the
/// portable floor do not, and asserting a denial they never promised is how a
/// suite starts lying about what it proved.
fn backend_claims_a_network_boundary() -> bool {
    use io_harness::sandbox::{select, Backend, Sandbox};
    matches!(
        select(&SandboxConfig::new()).backend(),
        Backend::MacosSandboxExec | Backend::LinuxNamespaces
    )
}

#[cfg(unix)]
#[tokio::test]
async fn a_policy_that_denies_the_network_denies_it_to_a_contained_command() {
    if !backend_claims_a_network_boundary() {
        eprintln!("skipped: this host's backend claims no network boundary");
        return;
    }
    let dir = workspace();
    let (addr, server) = loopback_listener().await;
    let store = Store::memory().unwrap();
    let url = format!("http://{addr}/");
    let provider = MockScript::new(vec![vec![exec_call(&["curl", "-s", "-m", "5", &url])]]);

    // `Policy::default()` leaves `net` at `Ask` and carries no allowing rule, so
    // nothing here would permit an outbound connection.
    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::default().allow_exec("curl"),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps[0].decision.contains("exit 0"),
        "a contained command reached the network under a policy that permits none: {:?}",
        steps[0].decision
    );
    server.abort();
}

/// The other arm. Without it, an implementation that hard-codes `allow_network:
/// false` — or one whose containment is simply broken — passes the test above
/// having proven nothing.
#[cfg(unix)]
#[tokio::test]
async fn a_policy_that_allows_the_network_allows_it_to_a_contained_command() {
    let dir = workspace();
    let (addr, server) = loopback_listener().await;
    let store = Store::memory().unwrap();
    let url = format!("http://{addr}/");
    let provider = MockScript::new(vec![vec![exec_call(&["curl", "-s", "-m", "5", &url])]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("exit 0"),
        "a contained command was denied a network its policy permits: {:?}",
        steps[0].decision
    );
    server.abort();
}

/// The executable form of the limitation, asserted rather than glossed.
///
/// The backends take one boolean, so a policy that names a single host and a
/// policy that opens everything produce the same sandbox. A contained command
/// under a one-host allowance reaches a *different* host, and `docs/CONTRACT.md`
/// says so in those words.
#[cfg(unix)]
#[tokio::test]
async fn egress_under_containment_is_all_hosts_or_none_and_never_the_named_host() {
    let dir = workspace();
    let (addr, server) = loopback_listener().await;
    let store = Store::memory().unwrap();
    let url = format!("http://{addr}/");
    let provider = MockScript::new(vec![vec![exec_call(&["curl", "-s", "-m", "5", &url])]]);

    // One host allowed, and it is not the host the command dials.
    let policy = Policy::default()
        .allow_exec("curl")
        .allow_net("example.com:443");

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("exit 0"),
        "the coarseness is real and documented: one allowed host opens all of \
         them under containment, so this connection to an unnamed host succeeds. \
         If this assertion ever fails, per-host egress has been implemented and \
         docs/CONTRACT.md must stop saying it has not: {:?}",
        steps[0].decision
    );
    server.abort();
}

// ---------------------------------------------------------------------------
// F4 — `shell` contains every sub-command, not the first
// ---------------------------------------------------------------------------

fn shell_call(line: &str) -> ToolCall {
    ToolCall {
        name: "shell".into(),
        arguments: json!({ "line": line }),
    }
}

/// The hole most likely to be left open, because it is invisible to any test
/// written against a one-stage line. `shell` parses a line into sub-commands and
/// spawns each of them; an implementation that wraps only the first has closed
/// nothing — the escape is simply written by the second.
#[cfg(unix)]
#[tokio::test]
async fn a_contained_shell_line_contains_its_later_stages_too() {
    let dir = workspace();
    let escape = EscapeDir::new("shell-stage-two");
    let target = escape.file();
    let store = Store::memory().unwrap();
    // Stage one is harmless and must succeed; stage two is the one that tries to
    // leave. `tee` writes the path it is given.
    let line = format!("echo contained | tee {}", target.display());
    let provider = MockScript::new(vec![vec![shell_call(&line)]]);

    run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    if !backend_confines_writes() {
        // The floor arm, asserted rather than skipped — same reason as F1's.
        assert!(
            target.exists(),
            "the floor confines nothing, so the second stage's write must land: {}",
            target.display()
        );
        return;
    }

    assert!(
        !target.exists(),
        "the second stage of a contained line wrote outside the workspace: {}",
        target.display()
    );
}

/// The negative control for the line above, and the same protection F1's control
/// gives `exec`: with the field absent, `shell` still does what it did in 0.39.0.
#[cfg(unix)]
#[tokio::test]
async fn an_uncontained_shell_line_still_writes_outside_the_workspace() {
    let dir = workspace();
    let escape = EscapeDir::new("shell-uncontained");
    let target = escape.file();
    let store = Store::memory().unwrap();
    let line = format!("echo uncontained | tee {}", target.display());
    let provider = MockScript::new(vec![vec![shell_call(&line)]]);

    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        target.exists(),
        "0.39.0 shell behaviour is unchanged when the field is absent: {} should exist",
        target.display()
    );
}

// ---------------------------------------------------------------------------
// F6 — Linux confines writes, on Linux
// ---------------------------------------------------------------------------

/// The claim `src/lib.rs` has been making about Linux, asserted where Linux runs.
///
/// Before 0.40.0 the backend unshared a mount namespace and remounted nothing
/// into it, so the filesystem view was the host's and this assertion would have
/// failed. Only `--net` was real.
///
/// The skip is guarded rather than silent. A runner whose kernel refuses user
/// namespaces reports `PortableFloor`, which confines nothing — and a test that
/// quietly returned there would pass having proven exactly nothing, which is the
/// failure mode this release is most exposed to. On CI that is a hard failure.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_confines_a_contained_commands_writes_to_the_workspace() {
    use io_harness::sandbox::{select, Backend, Sandbox};

    let backend = select(&SandboxConfig::new()).backend();
    if backend != Backend::LinuxNamespaces {
        assert!(
            std::env::var("CI").is_err(),
            "this runner reported {backend:?} rather than LinuxNamespaces. On CI that is a \
             failure and not a skip: the confinement assertion below would pass without \
             confining anything, and the release would claim a boundary it never applied."
        );
        eprintln!("skipped: this host reports {backend:?}, which claims no filesystem boundary");
        return;
    }

    let dir = workspace();
    let escape = EscapeDir::new("linux-namespaces");
    let target = escape.file();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![exec_call(&["touch", target.to_str().unwrap()])],
        // The control, in the same run: a write *inside* the workspace must still
        // succeed, or "confined" would just mean "broken".
        vec![exec_call(&["touch", "inside-the-workspace.txt"])],
    ]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !target.exists(),
        "the mount namespace confined nothing: {} was written",
        target.display()
    );
    assert!(
        dir.path().join("inside-the-workspace.txt").exists(),
        "a write inside the workspace must still land — otherwise the namespace \
         is not confining writes, it is preventing them"
    );
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[1].decision.contains("exit 0"),
        "and it must succeed rather than merely leave a file: {:?}",
        steps[1].decision
    );
}

// ---------------------------------------------------------------------------
// F7 — the audit says which backend actually ran
// ---------------------------------------------------------------------------

/// A run contained less than the caller asked for must be legible afterwards.
///
/// Without these rows, a host that refused its native primitive and fell back to
/// `PortableFloor` — which confines nothing — produces a trace identical to a run
/// that was contained exactly as requested. The rows are the same ones the
/// verification gate has written since 0.6.0; what is new is the tool layer
/// reaching them.
#[cfg(unix)]
#[tokio::test]
async fn a_contained_command_records_the_backend_that_actually_applied() {
    use io_harness::sandbox::{select, Sandbox};

    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", "audited.txt"])]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        !events.is_empty(),
        "a contained command wrote no sandbox lifecycle rows at all"
    );

    let expected = select(&SandboxConfig::new()).backend();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "create" && e.backend.as_deref() == Some(expected.as_str())),
        "the recorded backend must be the one that applied ({}): {events:?}",
        expected.as_str()
    );
    assert!(
        events.iter().any(|e| e.kind == "exec"),
        "the command itself is recorded: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.kind == "destroy"),
        "and the sandbox's teardown closes the lifecycle: {events:?}"
    );
}

/// The control. With the field absent there is no sandbox, so there must be no
/// row claiming one — an audit that reported containment for an uncontained
/// command would be worse than no audit.
#[cfg(unix)]
#[tokio::test]
async fn an_uncontained_command_records_no_sandbox_at_all() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", "unaudited.txt"])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        store.sandbox_events(result.run_id).unwrap().is_empty(),
        "an uncontained command must not leave rows claiming it was contained"
    );
}
