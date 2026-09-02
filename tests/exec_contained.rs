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
        &contract(dir.path()).with_full_access(),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        target.exists(),
        "0.45.0 behaviour survives `with_full_access()`, and only there: {} should exist",
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
    // 0.74.0, audit M10 — the egress proxy now refuses a loopback upstream by
    // default, and these tests route a contained `curl` through it to a listener
    // on 127.0.0.1. The widening is the documented opt-out an operator uses for
    // a local model; set once and never unset, because it is process-wide and
    // `cargo test` runs a binary's tests as threads. Opting in here is the point:
    // the floor is real, so a test that needs its own loopback has to say so.
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| std::env::set_var("IO_HARNESS_ALLOW_LOCAL_ADDRESSES", "1"));
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
    use io_harness::sandbox::{select, Sandbox};
    // One question, one answer, and it lives on `Backend` itself.
    //
    // This was a `matches!` list written out here and in three other places. When
    // 0.47.0's chain added three backends every one of them went silently wrong
    // at once: a host reporting `LinuxLandlock` took the branch meaning "this
    // backend confines nothing" and asserted that a write it had correctly
    // refused ought to have landed. `Backend::confines_writes` is an exhaustive
    // match, so the next backend added is a compile error there rather than a
    // passing test here that proves nothing.
    select(&SandboxConfig::new()).backend().confines_writes()
}

/// Does this host's backend claim a network boundary at all? A Job Object and the
/// portable floor do not, and asserting a denial they never promised is how a
/// suite starts lying about what it proved.
fn backend_claims_a_network_boundary() -> bool {
    use io_harness::sandbox::{select, Sandbox};
    select(&SandboxConfig::new()).backend().denies_egress()
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

/// **0.48.0 inverted this test, and that inversion is the release.**
///
/// It was written in 0.40.0 to assert the limitation rather than gloss it: the
/// backends take one boolean, so a policy naming a single host and a policy
/// opening everything produced the same sandbox, and a contained command under a
/// one-host allowance reached a *different* host. Its failure message said what to
/// do when it stopped being true — "if this assertion ever fails, per-host egress
/// has been implemented and `docs/CONTRACT.md` must stop saying it has not".
///
/// It failed on the first run of 0.48.0's proxy, with `curl` exiting 7. The
/// sandbox now permits the loopback proxy and nothing else, and the proxy asked
/// this run's own policy about the host and refused it. So the assertion is
/// reversed and the sentence explaining why is here rather than in a commit
/// message: the behaviour deliberately changed, which is the only legitimate
/// reason to change a test.
#[cfg(unix)]
#[tokio::test]
async fn egress_under_containment_reaches_the_named_host_and_no_other() {
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

    // The refusal is read off the trace rather than off `curl`'s exit code, and
    // that distinction is the release working: the dial reaches the proxy, the
    // proxy asks the policy and answers `403`, and `curl -s` exits 0 having
    // received it. What must be true is that the *policy* refused the host and
    // that the listener was never reached.
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.act == "net" && e.kind == "refusal")
        .collect();
    assert!(
        refusals
            .iter()
            .any(|e| e.target == format!("127.0.0.1:{}", addr.port())),
        "the host this run's policy never named was refused by it: {refusals:?}"
    );
    // A refusal row is written *before* the proxy would connect, so its presence
    // is also the proof that the listener was never dialed on this run's behalf.
    server.abort();
}

/// The other half, and the one that makes the release a capability rather than a
/// stricter denial: the host the policy *does* name is reached, through the proxy,
/// by an ordinary `curl` that knows nothing about any of this.
///
/// Both halves in one run would not distinguish "the proxy works" from "the
/// sandbox blocked everything", so they are two runs differing in one rule.
#[cfg(unix)]
#[tokio::test]
async fn a_host_the_policy_names_is_reached_through_the_proxy() {
    let dir = workspace();
    let (addr, server) = loopback_listener().await;
    let store = Store::memory().unwrap();
    let url = format!("http://{addr}/");
    let provider = MockScript::new(vec![vec![exec_call(&["curl", "-s", "-m", "5", &url])]]);

    // The listener's own host, named — and it is a per-host policy, so the run
    // starts a proxy rather than taking the boolean.
    let policy = Policy::default().allow_exec("curl").allow_net("127.0.0.1");

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
        "a named host is reachable under containment: {:?}",
        steps[0].decision
    );

    // And the dial is in the trace, decided by the policy rather than merely
    // permitted by the absence of a boundary.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "dial"
            && e.detail.as_deref() == Some(&format!("127.0.0.1:{}", addr.port())[..])),
        "every dial is recorded at command scope: {events:?}"
    );
    server.abort();
}

/// F7 — the proxy is the only route out, and this is the only assertion that can
/// tell a boundary from a convention.
///
/// Everything else about per-host egress would pass for an implementation that
/// merely sets `HTTP_PROXY` and hopes: the named host is reached, the unnamed one
/// is refused, and every dial is recorded — all true of a payload that chooses to
/// use the proxy. Here `curl` is told to ignore the proxy entirely (`--noproxy
/// '*'`) and dial the host **directly**. The host is one this run's policy names,
/// so the proxy would have permitted it; the sandbox must refuse it anyway,
/// because the sandbox permits the proxy's address and nothing else.
///
/// On a backend that can scope neither address nor port the run does not take
/// this path at all — the rung preference sends it to the boolean — so this is
/// asserted where it can be and reported where it cannot, never skipped.
#[cfg(unix)]
#[tokio::test]
async fn a_direct_dial_past_the_proxy_is_refused_by_the_sandbox() {
    let dir = workspace();
    let (addr, server) = loopback_listener().await;
    let store = Store::memory().unwrap();
    let url = format!("http://{addr}/");
    let provider = MockScript::new(vec![vec![exec_call(&[
        "curl",
        "-s",
        "-m",
        "5",
        "--noproxy",
        "*",
        &url,
    ])]]);

    // The very host the policy permits. If this succeeds, the proxy is advice.
    let policy = Policy::default().allow_exec("curl").allow_net("127.0.0.1");

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    let backend = {
        use io_harness::sandbox::{select, Sandbox};
        select(&SandboxConfig::new()).backend()
    };
    let steps = store.steps(result.run_id).unwrap();
    if !backend.denies_egress() {
        // The floor and a Job Object scope nothing. Assert that, rather than a
        // confinement they never promised: the dial succeeds and the crate says
        // the boundary is advisory on this backend.
        assert!(
            steps[0].decision.contains("exit 0"),
            "this backend scopes no egress, so a direct dial must succeed: {:?}",
            steps[0].decision
        );
        return;
    }
    assert!(
        !steps[0].decision.contains("exit 0"),
        "a contained command dialled past the proxy and reached the network. The \
         sandbox permits the proxy and nothing else, so this must be refused by \
         the kernel rather than by the payload's cooperation: {:?}",
        steps[0].decision
    );
    // And nothing was recorded as a dial, because nothing reached the proxy —
    // which is what makes this a *kernel* refusal rather than a policy one.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        !events.iter().any(|e| e.kind == "dial"),
        "the refusal was the sandbox's, not the proxy's: {events:?}"
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
        &contract(dir.path()).with_full_access(),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        target.exists(),
        "a full-access shell line still writes outside the workspace: {} should exist",
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
    use io_harness::sandbox::{select, Sandbox};

    // 0.47.0: any confining rung of the chain, not the namespace rung alone.
    // Which one a host takes is the chain's business; what this test asserts is
    // that a host claiming to confine writes actually does.
    let backend = select(&SandboxConfig::new()).backend();
    if !backend.confines_writes() {
        assert!(
            std::env::var("CI").is_err(),
            "this runner reported {backend:?}, which is not a confining rung of the Linux \
             chain. On CI that is a failure and not a skip: the confinement assertion below \
             would pass without confining anything, and the release would claim a boundary \
             it never applied."
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
        "the {backend:?} rung confined nothing: {} was written",
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
        &contract(dir.path()).with_full_access(),
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

// ===========================================================================
// 0.46.0 — the default is containment, and the exception is a sentence
// ===========================================================================

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::{run_with_observed, ExecMode};

/// Serialises the tests that set `CARGO_HOME`.
///
/// The environment is process-global and `cargo test` runs a binary's tests in
/// parallel, so two tests redirecting a toolchain's cache at once would read each
/// other's answer — trap 102's family, and the reason `tests/plugin.rs` needs
/// `XDG_CONFIG_HOME` redirected rather than `HOME`. Held across the whole run,
/// including its awaits, which is sound because `#[tokio::test]` is a
/// current-thread runtime.
/// A `tokio` mutex rather than a `std` one, and not as a style choice: the guard
/// is held across the run's awaits, which `clippy::await_holding_lock` refuses
/// for a blocking guard and permits for this one. It also cannot be poisoned, so
/// a test that panicked while holding it does not turn into a second failure
/// blamed on the first.
static ENV: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// A workspace that is **not** under the system temp directory.
///
/// `tempfile::tempdir()` puts a directory under `/private/var/folders`, which the
/// macOS profile allows writes to unconditionally — so a test asserting that a
/// mode refuses a write into its own workspace needs the workspace somewhere the
/// deny actually reaches. Reuses [`EscapeDir`]'s location and its drop.
struct OutsideTemp(EscapeDir);

impl OutsideTemp {
    fn path(&self) -> &Path {
        &self.0 .0
    }
}

/// Collects the `Contained` report a run makes at start.
#[derive(Default)]
struct Contained(Mutex<Vec<(String, String, u32)>>);

impl Observer for Contained {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Contained {
            mode,
            backend,
            roots,
        } = &event.kind
        {
            self.0
                .lock()
                .unwrap()
                .push((mode.clone(), backend.clone(), *roots));
        }
        Flow::Continue
    }
}

/// F1 — the default confines a write, with no builder call at all.
///
/// The whole release in one assertion. `contract()` is
/// `TaskContract::workspace(goal, root)` and nothing else — the shape every
/// embedder writes — and up to 0.45.0 this write landed.
#[cfg(unix)]
#[tokio::test]
async fn the_default_contract_confines_a_write_with_no_builder_call() {
    let dir = workspace();
    let escape = EscapeDir::new("default-confines");
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

    if !backend_confines_writes() {
        // The degraded host — a stock Ubuntu 24.04 or a Windows Job Object.
        // Asserted rather than skipped: 0.40.0's Linux defect survived three CI
        // runs precisely because the tests stepped over the fallback.
        assert!(
            target.exists(),
            "this host confines nothing, so the write must land: {}",
            target.display()
        );
        return;
    }
    assert!(
        !target.exists(),
        "the default contract let a command write outside the workspace: {}",
        target.display()
    );
}

/// F2 — and the write lands again the moment the caller says so, which is what
/// makes the containment a default rather than a policy.
#[cfg(unix)]
#[tokio::test]
async fn the_escape_hatch_is_one_call_and_it_is_complete() {
    let dir = workspace();
    let escape = EscapeDir::new("escape-hatch");
    let target = escape.file();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["touch", target.to_str().unwrap()])]]);
    let seen = Contained::default();

    let result = run_with_observed(
        &contract(dir.path()).with_full_access(),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    assert!(
        target.exists(),
        "with_full_access() must restore 0.45.0's behaviour exactly: {}",
        target.display()
    );
    // Complete, not partial: no backend was consulted and no row claims one.
    assert!(
        store.sandbox_events(result.run_id).unwrap().is_empty(),
        "a full-access run left rows claiming containment"
    );
    // But the run still says what it is. An absent event is not a statement.
    let reports = seen.0.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(reports[0].0, "full-access");
    assert_eq!(reports[0].1, "none");
    assert_eq!(reports[0].2, 0);
}

/// F3 — `ReadOnly` refuses a write into the workspace root itself, and still
/// permits the read.
///
/// The mode whose whole difference from the default is one directory, so the
/// assertion is on that directory.
///
/// **The workspace here is not a `tempfile::tempdir()`, and that is the module
/// header's trap read a second time.** The macOS profile blanket-allows
/// `/private/var/folders`, so a read-only workspace placed there would be
/// writable no matter what this release does, and the test would pass on the
/// development host while asserting nothing. The escape tests moved to `target/`
/// for that reason in 0.40.0; a read-only *workspace* has to move for the same
/// one.
#[cfg(unix)]
#[tokio::test]
async fn read_only_refuses_a_write_into_the_workspace_and_permits_the_read() {
    let dir = EscapeDir::new("read-only-workspace");
    let dir = OutsideTemp(dir);
    std::fs::write(dir.path().join("readable.txt"), "already here\n").unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![exec_call(&["touch", "written-under-read-only.txt"])],
        vec![exec_call(&["cat", "readable.txt"])],
    ]);

    let result = run_with(
        &contract(dir.path()).with_exec_mode(ExecMode::ReadOnly),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    if !backend_confines_writes() {
        assert!(
            dir.path().join("written-under-read-only.txt").exists(),
            "this host confines nothing, so the write must land"
        );
        return;
    }
    assert!(
        !dir.path().join("written-under-read-only.txt").exists(),
        "read-only let a command write into the workspace"
    );
    // The read is the other half, and it is what separates this from "nothing
    // runs at all under this mode".
    assert!(
        steps.iter().any(|s| s.prompt.contains("already here")),
        "read-only refused a read as well: {:?}",
        steps.iter().map(|s| s.prompt.len()).collect::<Vec<_>>()
    );
}

/// F6 — a granted root that does not exist never reaches a backend, and the
/// backend stays native.
///
/// 0.40.0's defect, reproduced deliberately: the Linux mount setup `fail`s on a
/// bind it cannot perform, and a failed setup degrades the whole backend to the
/// floor. The feature would appear to work while confining nothing.
#[cfg(unix)]
#[tokio::test]
async fn a_writable_root_that_does_not_exist_does_not_degrade_the_backend() {
    let _env = ENV.lock().await;
    use io_harness::sandbox::{select, Backend, Sandbox};

    let dir = workspace();
    let store = Store::memory().unwrap();
    // A cargo project whose registry cache is somewhere that is not there.
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let absent = dir.path().join("no-such-cargo-home");
    std::env::set_var("CARGO_HOME", &absent);

    let provider = MockScript::new(vec![vec![exec_call(&["touch", "landed.txt"])]]);
    let seen = Contained::default();
    let result = run_with_observed(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();
    std::env::remove_var("CARGO_HOME");

    // The command ran, inside the workspace, under this host's real backend.
    assert!(
        dir.path().join("landed.txt").exists(),
        "the run did not survive an absent writable root"
    );
    let expected = select(&SandboxConfig::new()).backend();
    let reports = seen.0.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert_eq!(
        reports[0].1,
        expected.as_str(),
        "an absent root degraded the backend"
    );
    assert_eq!(
        reports[0].2, 0,
        "a root that does not exist was granted anyway"
    );
    let _ = Backend::PortableFloor;
    assert!(!store.sandbox_events(result.run_id).unwrap().is_empty());
}

/// F9's other arm — the report names the backend that applied and counts the
/// roots that were granted, for an ordinary contained run.
#[cfg(unix)]
#[tokio::test]
async fn the_containment_report_names_what_actually_applied() {
    let _env = ENV.lock().await;
    use io_harness::sandbox::{select, Sandbox};

    let dir = workspace();
    // A cargo project whose registry cache exists, so there is a root to count.
    std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
    let home = dir.path().join("cargo-home");
    std::fs::create_dir_all(&home).unwrap();
    std::env::set_var("CARGO_HOME", &home);

    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["true"])]]);
    let seen = Contained::default();
    let _ = run_with_observed(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();
    std::env::remove_var("CARGO_HOME");

    let reports = seen.0.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "exactly one report per run: {reports:?}");
    assert_eq!(reports[0].0, "workspace-write");
    assert_eq!(
        reports[0].1,
        select(&SandboxConfig::new()).backend().as_str(),
        "the report must name the selection, not the request"
    );
    assert_eq!(
        reports[0].2, 1,
        "the toolchain's own cache is the granted root: {reports:?}"
    );
}

/// F4 — a real package manager completes under the default, and fails without
/// the cache roots.
///
/// `cargo generate-lockfile --offline` is the smallest real invocation that
/// **must** write outside the project it is building: measured on this host, it
/// creates `$CARGO_HOME/.package-cache` and `$CARGO_HOME/.global-cache` and
/// touches the network for nothing. That is the whole 0.40.0 limitation — "under
/// containment a toolchain writing `~/.cargo/registry` fails" — in one command.
///
/// The control differs in exactly one thing: whether the workspace root carries
/// the marker `toolchain::detect` reads. Without it there is no detection, so
/// there are no cache roots, so the same command is refused. A test asserting
/// only that `cache_dirs()` returned a plausible `PathBuf` would pass in exactly
/// the case that matters.
#[cfg(unix)]
#[tokio::test]
async fn a_real_package_manager_completes_under_the_default() {
    let _env = ENV.lock().await;
    let ws = OutsideTemp(EscapeDir::new("cargo-granted"));
    let home = OutsideTemp(EscapeDir::new("cargo-home"));
    std::fs::create_dir_all(ws.path().join("src")).unwrap();
    std::fs::write(
        ws.path().join("Cargo.toml"),
        "[package]\nname = \"p\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(ws.path().join("src/main.rs"), "fn main() {}\n").unwrap();

    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&[
        "cargo",
        "generate-lockfile",
        "--offline",
    ])]]);
    std::env::set_var("CARGO_HOME", home.path());
    let result = run_with(
        &contract(ws.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    std::env::remove_var("CARGO_HOME");

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        ws.path().join("Cargo.lock").exists(),
        "the granted run did not complete: {:?}",
        steps[0].decision
    );
    assert!(
        home.path().join(".package-cache").exists(),
        "the toolchain's own cache was not writable, which is the whole grant"
    );

    if !backend_confines_writes() {
        return; // nothing is confined here, so there is no control to run
    }

    // The control: the same command, the same cache, one marker file apart.
    let bare = OutsideTemp(EscapeDir::new("cargo-ungranted"));
    let home2 = OutsideTemp(EscapeDir::new("cargo-home-2"));
    std::fs::create_dir_all(bare.path().join("sub/src")).unwrap();
    std::fs::write(
        bare.path().join("sub/Cargo.toml"),
        "[package]\nname = \"q\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    std::fs::write(bare.path().join("sub/src/main.rs"), "fn main() {}\n").unwrap();

    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&[
        "cargo",
        "generate-lockfile",
        "--offline",
        "--manifest-path",
        "sub/Cargo.toml",
    ])]]);
    std::env::set_var("CARGO_HOME", home2.path());
    let result = run_with(
        &contract(bare.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    std::env::remove_var("CARGO_HOME");

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !home2.path().join(".package-cache").exists(),
        "an ungranted cache was written to anyway: {:?}",
        steps[0].decision
    );
}

/// F8 — the verification gate is contained under the same roots as the run.
///
/// The gate runs the project's *own* build command in an ephemeral workdir, so it
/// is the one place this crate runs a package manager on purpose. A gate that
/// could not populate a registry cache would fail for a reason that has nothing
/// to do with the code it is judging.
#[cfg(unix)]
#[tokio::test]
async fn the_verification_gate_gets_the_same_writable_roots() {
    let _env = ENV.lock().await;
    use io_harness::Verification;

    let ws = OutsideTemp(EscapeDir::new("gate-granted"));
    let home = OutsideTemp(EscapeDir::new("gate-home"));
    std::fs::write(ws.path().join("Cargo.toml"), "[package]\nname = \"g\"\n").unwrap();
    let marker = home.path().join("gate-wrote.txt");

    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![]]);
    std::env::set_var("CARGO_HOME", home.path());
    let _ = run_with(
        &contract(ws.path()).with_verification(Verification::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                format!("touch {}", marker.display()),
            ],
            expect_exit: 0,
        }),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    std::env::remove_var("CARGO_HOME");

    assert!(
        marker.exists(),
        "the gate could not write to the toolchain's own cache: {}",
        marker.display()
    );
}
