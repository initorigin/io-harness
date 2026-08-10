//! `shell_start` handles under containment — F1, F2 and F3 of 0.48.0, through the
//! full loop with a scripted mock provider so the tests are deterministic and
//! offline.
//!
//! Until this release a handle was the one execution path still running at full
//! privilege. An agent whose `exec` could not write outside the workspace could
//! start the same line with `shell_start` and write wherever it liked, so the
//! boundary depended on which tool the model happened to pick.
//!
//! Every assertion here comes in a pair, for the reason `tests/exec_contained.rs`
//! states at length: a single-armed test passes an implementation that contains
//! every handle unconditionally, and that implementation changes where every
//! existing embedder's dev server may write.
//!
//! The escape target is a directory under the crate's own `target/` rather than a
//! second temp directory, because the macOS profile re-allows the whole of
//! `/private/var/folders` and a `touch` there proves nothing on the platform this
//! was developed on. Same reasoning, same shape, as `tests/exec_contained.rs`.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::sandbox::{SandboxConfig, SandboxLimits};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

/// Plays a fixed script of tool calls. The same shape `tests/exec_contained.rs`
/// uses.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    #[allow(dead_code)]
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

fn start(line: &str) -> ToolCall {
    ToolCall {
        name: "shell_start".into(),
        arguments: json!({ "line": line }),
    }
}

fn poll(id: u64) -> ToolCall {
    ToolCall {
        name: "shell_poll".into(),
        arguments: json!({ "handle": id }),
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

/// Does this host's backend confine writes at all? A Job Object and the portable
/// floor do not, and asserting a confinement they never promised is how a test
/// proves nothing while passing.
fn backend_confines_writes() -> bool {
    use io_harness::sandbox::{select, Sandbox};
    select(&SandboxConfig::new()).backend().confines_writes()
}

/// A directory outside the workspace that the macOS profile does **not** blanket
/// allow. Removed on drop, including when the assertion that reads it fails.
struct EscapeDir(PathBuf);

impl EscapeDir {
    fn new(tag: &str) -> Self {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("contained-handles-escape")
            .join(tag);
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for EscapeDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Wait for a path to appear, up to a generous ceiling.
///
/// A handle's work happens in a detached task, so "did it write the file" is a
/// question about a process this thread never awaits. The loop is bounded so a
/// failure is a failed assertion rather than a hung suite — and the assertion it
/// serves is always about the file's **presence**, never about how quickly it
/// appeared, so a slow runner delays this and cannot fail it.
fn appears(path: &Path) -> bool {
    for _ in 0..200 {
        if path.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    false
}

/// A step that costs the run two seconds, so the handle beside it has a life
/// inside the run rather than only after it.
///
/// **This is not decoration and it is the trap this file exists to remember.**
/// `Handles` kills every live handle when the registry drops, which is at the end
/// of the run — correctly, because a process outliving the run that started it is
/// the leak the whole module prevents. A script of two instant steps therefore
/// ends, and kills, in a few milliseconds. An *uncontained* `touch` wins that
/// race; the same `touch` behind `sandbox-exec` pays the wrapper's startup and
/// loses it. Written without this step, the confinement assertions in this file
/// passed against a handle that had simply been killed before it could act —
/// which is a test proving nothing while reporting success, and is why every
/// confinement assertion below is paired with a write that must *land*.
fn settle() -> ToolCall {
    exec_call(&["sleep", "2"])
}

// ---------------------------------------------------------------------------
// F1 — a handle cannot write outside its granted roots, and an uncontained one
//      still can
// ---------------------------------------------------------------------------

#[cfg(unix)]
#[tokio::test]
async fn a_contained_handle_cannot_write_outside_the_workspace() {
    let dir = workspace();
    let escape = EscapeDir::new("contained");
    let target = escape.file("escaped.txt");
    let store = Store::memory().unwrap();
    // Inside first, outside second. The first write is the control that the
    // handle really ran under containment; without it, "the outside file is
    // absent" is equally satisfied by a handle that never started.
    let inside = dir.path().join("landed.txt");
    let line = format!(
        "touch {} && touch {}",
        inside.to_str().unwrap(),
        target.to_str().unwrap()
    );
    let provider = MockScript::new(vec![vec![start(&line)], vec![settle()], vec![poll(1)]]);

    let result = run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    if !backend_confines_writes() {
        assert!(
            appears(&target),
            "the floor confines nothing, so the write must land: {}",
            target.display()
        );
        return;
    }

    assert!(
        appears(&inside),
        "the contained handle ran and wrote inside its granted roots: {}",
        inside.display()
    );
    // Give the handle every chance to write before concluding it could not: the
    // failing direction of this assertion must never be "the test was too quick".
    assert!(
        !appears(&target),
        "a contained handle wrote outside the workspace: {}",
        target.display()
    );

    // And the containment is in the trace, at handle scope, not merely implied by
    // an absent file.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "exec" && e.detail.as_deref() == Some(line.as_str())),
        "the handle's line was recorded as a contained exec: {events:?}"
    );
    assert!(
        events
            .iter()
            .any(|e| e.kind == "destroy" && e.detail.as_deref() == Some("shell_start handle 1")),
        "the handle's containment was closed in the trace: {events:?}"
    );
}

/// The negative control, and the half that protects every existing caller.
///
/// An implementation that contains every handle unconditionally passes the test
/// above and fails this one.
#[cfg(unix)]
#[tokio::test]
async fn an_uncontained_handle_still_writes_outside_the_workspace() {
    let dir = workspace();
    let escape = EscapeDir::new("uncontained");
    let target = escape.file("escaped.txt");
    let store = Store::memory().unwrap();
    let line = format!("touch {}", target.to_str().unwrap());
    let provider = MockScript::new(vec![vec![start(&line)], vec![settle()], vec![poll(1)]]);

    run_with(
        &contract(dir.path()).with_full_access(),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        appears(&target),
        "`with_full_access()` still starts an uncontained handle: {} should exist",
        target.display()
    );
}

// ---------------------------------------------------------------------------
// F2 — every stage of a handle is contained, not only the first
// ---------------------------------------------------------------------------

/// A one-stage test passes an implementation that wraps only the head of the
/// line. The escape this closes is a *later* stage: `true | tee /somewhere/else`
/// is two commands and the second one is as much a command as the first.
#[cfg(unix)]
#[tokio::test]
async fn every_stage_of_a_handle_is_contained_not_only_the_first() {
    let dir = workspace();
    let escape = EscapeDir::new("second-stage");
    let target = escape.file("second-stage.txt");
    let inside = dir.path().join("first-stage.txt");
    let store = Store::memory().unwrap();
    // The first stage writes inside the workspace, so a failure to reach the
    // second stage at all is distinguishable from the second stage being
    // confined.
    let line = format!(
        "echo hello | tee {} {}",
        inside.to_str().unwrap(),
        target.to_str().unwrap()
    );
    let provider = MockScript::new(vec![vec![start(&line)], vec![settle()], vec![poll(1)]]);

    run_with(
        &contract(dir.path()).with_contained_exec(SandboxConfig::new()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    if !backend_confines_writes() {
        assert!(appears(&target), "the floor confines nothing");
        return;
    }

    assert!(
        appears(&inside),
        "the pipe is intact: the first stage ran and reached the second"
    );
    assert!(
        !appears(&target),
        "the second stage of a handle's line wrote outside the workspace: {}",
        target.display()
    );
}

// ---------------------------------------------------------------------------
// F3 — a handle takes the resource caps a foreground stage takes, and the wall
//      clock reaches neither
// ---------------------------------------------------------------------------

/// `max_wall_secs` is enforced inside the sandbox's own runner
/// (`src/sandbox.rs`), which `exec` reaches and no `shell` path does — a line is
/// several piped processes, so the `shell` tool owns every child and spawns them
/// itself. A handle therefore cannot be killed by the wall clock, which is also
/// the right answer: a dev server killed at the sandbox's ceiling would be a
/// containment feature deleting the tool's purpose.
///
/// `US-IO-HARNESS-0.48.0-I02`: the control here was a foreground `shell` line
/// until implementation showed no `shell` line has ever been wall-capped.
#[cfg(unix)]
#[tokio::test]
async fn the_wall_clock_kills_an_exec_and_never_a_handle() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let marker = dir.path().join("still-running.txt");
    // Starts, writes its marker, then outlives the cap by a wide margin.
    let line = format!("touch {} && sleep 30", marker.to_str().unwrap());
    // The `exec` in the middle is both the control and the clock: it is killed by
    // the cap it outlives, and the seconds it spends being killed are seconds the
    // handle beside it survives.
    let provider = MockScript::new(vec![
        vec![start(&line)],
        vec![exec_call(&["sleep", "30"])],
        vec![poll(1)],
    ]);

    let capped = SandboxConfig {
        limits: SandboxLimits {
            max_wall_secs: Some(1),
            ..SandboxLimits::none()
        },
        ..SandboxConfig::new()
    };

    let result = run_with(
        &contract(dir.path()).with_contained_exec(capped),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The `exec` arm: killed by the cap, and named as the cap rather than as a
    // command that merely failed.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "cap_hit" && e.detail.as_deref() == Some("wall")),
        "`exec` outlived max_wall_secs and was killed by it: {events:?}"
    );

    // The handle arm: it started before the cap, the `exec` beside it was killed
    // by that cap, and the handle is still running. Asserted as "never reached a
    // terminal state", so a slow runner delays this and cannot fail it.
    assert!(appears(&marker), "the handle started");
    let handle = store
        .process_handles(result.run_id)
        .unwrap()
        .into_iter()
        .find(|h| h.handle == 1)
        .expect("the handle is in the trace");
    assert_eq!(
        handle.state, "running",
        "the wall cap killed the `exec` beside it and never the handle: {handle:?}"
    );
}

/// The other half: the caps a handle *does* take are really applied. Without
/// this, "the wall clock does not reach a handle" is indistinguishable from
/// "nothing reaches a handle".
///
/// `RLIMIT_CPU` is counted in CPU seconds by the kernel, so the assertion is
/// about a process being killed and not about elapsed wall time — a loaded
/// runner makes this slower and cannot make it flaky.
#[cfg(unix)]
#[tokio::test]
async fn a_handle_takes_the_cpu_cap_a_foreground_stage_takes() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let started = dir.path().join("spinning.txt");
    // Marks that it ran, then burns CPU with no syscall to sleep on.
    let line = format!(
        "touch {} && awk 'BEGIN{{while(1){{x+=1}}}}'",
        started.to_str().unwrap()
    );
    // The `sleep` step is the clock: it costs almost no CPU itself, and while it
    // runs the spinning stage beside it burns the second the cap allows it.
    let provider = MockScript::new(vec![
        vec![start(&line)],
        vec![exec_call(&["sleep", "3"])],
        vec![poll(1)],
    ]);

    let capped = SandboxConfig {
        limits: SandboxLimits {
            max_cpu_secs: Some(1),
            ..SandboxLimits::none()
        },
        ..SandboxConfig::new()
    };

    let result = run_with(
        &contract(dir.path()).with_contained_exec(capped),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(appears(&started), "the spinning stage started");

    // It burned its CPU second under `RLIMIT_CPU` while the `sleep` step ran, so
    // by the time the run's last step boundary swept the registry the handle was
    // over. Without `apply_rlimits` on the handle's spawn it is still spinning
    // and this row still says `running`.
    let handle = store
        .process_handles(result.run_id)
        .unwrap()
        .into_iter()
        .find(|h| h.handle == 1)
        .expect("the handle is in the trace");
    assert_ne!(
        handle.state, "running",
        "the CPU cap reached the handle's stage and ended it: {handle:?}"
    );
}

