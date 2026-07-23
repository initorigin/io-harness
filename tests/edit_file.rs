//! End-to-end runs through the full loop with mock providers, so the tests are
//! deterministic and offline. The real OpenRouter path is exercised by
//! `examples/edit_file.rs`.
//!
//! Covers the 0.1.0 loop plus the 0.2.0 additions: execution-based verification
//! (the I01 stub is rejected in the loop), the time and cost budgets, retry with
//! escalation, and resuming an interrupted run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{resume, run, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

/// A provider that always returns a scripted `write_file` call.
struct MockWriter {
    content: String,
    tokens: u64,
}

impl MockWriter {
    fn new(content: impl Into<String>) -> Self {
        Self { content: content.into(), tokens: 0 }
    }
    fn with_tokens(content: impl Into<String>, tokens: u64) -> Self {
        Self { content: content.into(), tokens }
    }
}

impl Provider for MockWriter {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": self.content }),
            }],
            usage: (self.tokens > 0).then(|| Usage {
                total_tokens: self.tokens,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// A provider that never calls a tool, so the run must hit the step budget.
struct MockNoop {
    calls: AtomicU32,
}

impl Provider for MockNoop {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some("thinking...".into()),
            ..Default::default()
        })
    }
}

/// A writer that sleeps before responding, to exceed a time budget.
struct MockSlow {
    content: String,
    delay: Duration,
}

impl Provider for MockSlow {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        tokio::time::sleep(self.delay).await;
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": self.content }),
            }],
            ..Default::default()
        })
    }
}

/// A provider that errors a set number of times, then writes `content`.
struct MockFlaky {
    fails_remaining: AtomicU32,
    content: String,
}

impl Provider for MockFlaky {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if self.fails_remaining.fetch_sub(1, Ordering::SeqCst) > 0 {
            return Err(io_harness::Error::Provider("transient".into()));
        }
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": self.content }),
            }],
            ..Default::default()
        })
    }
}

/// A provider that always errors, to force escalation.
struct MockAlwaysErr;

impl Provider for MockAlwaysErr {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Err(io_harness::Error::Provider("down".into()))
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
    let provider = MockWriter::new("pub fn hello() -> u32 { 42 }\n");
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    let written = std::fs::read_to_string(&file).unwrap();
    assert!(written.contains("fn hello"));

    // The step is auditable in the store, with the trace populated.
    let steps = store.steps(result.run_id).unwrap();
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].decision, "wrote file");
    assert!(steps[0].prompt.contains("Goal: add a hello function"));
    assert!(steps[0].tool_call.contains("fn hello"));
}

#[tokio::test]
async fn stops_at_step_cap_when_never_verified() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    let contract = TaskContract::new("unreachable", &file, Verification::FileContains("NEVER".into()))
        .with_max_steps(3);
    let provider = MockNoop { calls: AtomicU32::new(0) };
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 3 });
    assert_eq!(provider.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn execution_verify_rejects_the_i01_stub_in_the_loop() {
    // The 0.1.0 failure: the model writes the literal substring "fn hello",
    // which does not compile. Under CompilesRust the loop can never pass it.
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hello.rs");

    let contract = TaskContract::new("compile a hello fn", &file, Verification::CompilesRust)
        .with_max_steps(2);
    let stub = MockWriter::new("fn hello");
    let store = Store::memory().unwrap();

    let result = run(&contract, &stub, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 2 });
}

#[tokio::test]
async fn execution_verify_accepts_a_compiling_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("hello.rs");

    let contract = TaskContract::new("compile a hello fn", &file, Verification::CompilesRust);
    let good = MockWriter::new("pub fn hello() -> u32 { 42 }\n");
    let store = Store::memory().unwrap();

    let result = run(&contract, &good, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
}

#[tokio::test]
async fn stops_at_time_budget() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    // Each call sleeps 30ms and writes content that never satisfies verify.
    let contract = TaskContract::new("slow", &file, Verification::FileContains("NEVER".into()))
        .with_max_steps(5)
        .with_time_budget(Duration::from_millis(20));
    let provider = MockSlow { content: "x".into(), delay: Duration::from_millis(30) };
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();
    // Step 1 runs (elapsed ~0), step 2's pre-check sees the budget blown.
    assert_eq!(result.outcome, RunOutcome::TimeBudgetExceeded { steps: 1 });
}

#[tokio::test]
async fn stops_at_cost_budget() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    // 100 tokens per call, budget 150, content never verifies.
    let contract = TaskContract::new("spendy", &file, Verification::FileContains("NEVER".into()))
        .with_max_steps(5)
        .with_token_budget(150);
    let provider = MockWriter::with_tokens("x", 100);
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();
    // 100 after step 1 (ok), 200 after step 2 (over).
    assert_eq!(result.outcome, RunOutcome::CostBudgetExceeded { steps: 2 });
}

#[tokio::test]
async fn retries_then_succeeds_recording_each_attempt() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    let contract = TaskContract::new("flaky", &file, Verification::FileContains("pass".into()))
        .with_max_steps(3)
        .with_max_retries(2);
    let provider = MockFlaky {
        fails_remaining: AtomicU32::new(2),
        content: "pass".into(),
    };
    let store = Store::memory().unwrap();

    let result = run(&contract, &provider, &store).await.unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });

    // Two retry attempts plus the successful write are all in the trace.
    let steps = store.steps(result.run_id).unwrap();
    assert_eq!(steps.len(), 3);
    assert_eq!(steps[0].decision, "retry 1 after error");
    assert_eq!(steps[1].decision, "retry 2 after error");
    assert_eq!(steps[2].decision, "wrote file");
}

#[tokio::test]
async fn escalates_after_exhausting_retries() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");

    let contract = TaskContract::new("down", &file, Verification::FileContains("pass".into()))
        .with_max_retries(1);
    let store = Store::memory().unwrap();

    let err = run(&contract, &MockAlwaysErr, &store).await;
    assert!(err.is_err());
}

#[tokio::test]
async fn resumes_an_interrupted_run_without_restarting() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("out.txt");
    let store = Store::memory().unwrap();

    // First run: two steps of wrong content, stops at the step budget.
    let first = TaskContract::new("finish", &file, Verification::FileContains("DONE".into()))
        .with_max_steps(2);
    let wrong = MockWriter::new("still working");
    let r1 = run(&first, &wrong, &store).await.unwrap();
    assert_eq!(r1.outcome, RunOutcome::StepCapReached { steps: 2 });

    // Resume the same run id with a provider that finishes the job.
    let second = TaskContract::new("finish", &file, Verification::FileContains("DONE".into()))
        .with_max_steps(5);
    let right = MockWriter::new("DONE now");
    let r2 = resume(&second, &right, &store, r1.run_id).await.unwrap();

    // Continued at step 3 — not restarted — and the whole trace is one run.
    assert_eq!(r2.run_id, r1.run_id);
    assert_eq!(r2.outcome, RunOutcome::Success { steps: 3 });
    assert_eq!(store.steps(r1.run_id).unwrap().len(), 3);
}
