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
use io_harness::sandbox::SandboxConfig;
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
