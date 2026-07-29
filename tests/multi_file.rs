//! Multi-file workspace runs through the full loop with a scripted mock
//! provider, so the test is deterministic and offline. Proves 0.3's core: the
//! agent greps/finds across a repo, edits several files, and the run only
//! succeeds when the whole edited set meets its spec.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

/// A provider that returns a fixed script of tool-call responses, one per step.
/// After the script runs out it returns an empty response (no tool call).
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A fixture repo with two stub files that don't yet satisfy the spec.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn b() -> u32 { 0 }\n").unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub mod a;\npub mod b;\n#[test] fn t() { assert_eq!(a::a() + b::b(), 42); }\n",
    )
    .unwrap();
    dir
}

/// The success spec: the two files, together, must make `a() + b() == 42`, and
/// the project's own runner is what says so. Until 0.18.0 this was
/// `WorkspaceTestPasses`, which concatenated the files the caller listed and
/// compiled a criterion beside them; `cargo test` runs the crate's real suite,
/// which is what the migration note tells a caller to write.
fn verify() -> Verification {
    Verification::Command {
        argv: vec!["cargo".into(), "test".into(), "--offline".into()],
        expect_exit: 0,
    }
}

#[tokio::test]
async fn agent_finds_then_edits_several_files_and_the_set_verifies() {
    let dir = fixture();
    let contract = TaskContract::workspace(
        "Make a() + b() equal 42 by editing the two source files.",
        dir.path(),
        verify(),
    );

    // Scripted: locate files, inspect one, then edit both correctly.
    let script = MockScript::new(vec![
        vec![call("find", json!({ "name_glob": "*.rs" }))],
        vec![call(
            "grep",
            json!({ "pattern": "fn a", "path_glob": "src/*.rs" }),
        )],
        vec![call("read_file", json!({ "path": "src/a.rs" }))],
        vec![call(
            "write_file",
            json!({ "path": "src/a.rs", "content": "pub fn a() -> u32 { 40 }\n" }),
        )],
        vec![call(
            "write_file",
            json!({ "path": "src/b.rs", "content": "pub fn b() -> u32 { 2 }\n" }),
        )],
    ]);
    let store = Store::memory().unwrap();

    let result = run(&contract, &script, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 5 });

    // Both files were actually edited on disk.
    let a = std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap();
    let b = std::fs::read_to_string(dir.path().join("src/b.rs")).unwrap();
    assert!(a.contains("40"));
    assert!(b.contains("2 }"));

    // The trace records the per-file tool calls and edits of the run.
    let steps = store.steps(result.run_id).unwrap();
    let calls: String = steps.iter().map(|s| s.tool_call.clone()).collect();
    assert!(calls.contains("find"));
    assert!(calls.contains("grep"));
    assert!(calls.contains("write_file"));
    assert!(calls.contains("src/a.rs"));
    assert!(calls.contains("src/b.rs"));
}

#[tokio::test]
async fn run_fails_when_one_of_the_edited_files_is_wrong() {
    let dir = fixture();
    let contract = TaskContract::workspace(
        "Make a() + b() equal 42 by editing the two source files.",
        dir.path(),
        verify(),
    )
    .with_max_steps(4);

    // a is edited correctly, b is left wrong: the SET must never verify.
    let script = MockScript::new(vec![
        vec![call(
            "write_file",
            json!({ "path": "src/a.rs", "content": "pub fn a() -> u32 { 40 }\n" }),
        )],
        vec![call(
            "write_file",
            json!({ "path": "src/b.rs", "content": "pub fn b() -> u32 { 99 }\n" }),
        )],
    ]);
    let store = Store::memory().unwrap();

    let result = run(&contract, &script, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 4 });
}

#[tokio::test]
async fn write_outside_the_workspace_is_refused_not_crashing_the_run() {
    let dir = fixture();
    let contract = TaskContract::workspace("noop", dir.path(), verify()).with_max_steps(1);

    // The model tries to escape the workspace root: it must be refused as an
    // observation, and the run continues (here to the step cap).
    let script = MockScript::new(vec![vec![call(
        "write_file",
        json!({ "path": "../escaped.rs", "content": "pwned" }),
    )]]);
    let store = Store::memory().unwrap();

    let result = run(&contract, &script, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 1 });
    // Nothing was written outside the root.
    assert!(!dir.path().parent().unwrap().join("escaped.rs").exists());
}
