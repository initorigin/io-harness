//! End-to-end file-edit run through the full loop, with a mock provider so the
//! test is deterministic and offline. The real OpenRouter path is exercised by
//! `examples/edit_file.rs`.

use std::sync::atomic::{AtomicU32, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

/// A provider that returns a scripted `write_file` call, then verification passes.
struct MockWriter {
    content: String,
}

impl Provider for MockWriter {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            text: None,
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": self.content }),
            }],
        })
    }
}

/// A provider that never calls a tool, so the run must hit the step cap.
struct MockNoop {
    calls: AtomicU32,
}

impl Provider for MockNoop {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some("thinking...".into()),
            tool_calls: vec![],
        })
    }
}

#[tokio::test]
async fn edits_file_and_verifies_success() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hello.rs");

    let contract = TaskContract::new(
        "add a hello function",
        &file,
        Verification::FileContains("fn hello".into()),
    );
    let provider = MockWriter {
        content: "pub fn hello() -> u32 { 42 }\n".into(),
    };
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.contains("fn hello"));

    // The step is auditable in the store.
    let steps = store.steps(result.run_id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].decision, "wrote file");
}

#[tokio::test]
async fn stops_at_step_cap_when_never_verified() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    let contract = TaskContract::new(
        "unreachable",
        &file,
        Verification::FileContains("NEVER".into()),
    )
    .with_max_steps(3);
    let provider = MockNoop { calls: AtomicU32::new(0) };
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 3 });
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}
