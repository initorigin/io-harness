//! The review criterion reads the change, not the outcome (0.42.0).
//!
//! `Verification::Review` has handed the reviewer every file the run wrote, with
//! its contents **as they stand now**, since 0.34.0. That is the outcome. A
//! reviewer of a code change reads the change, and the difference is not
//! cosmetic: a rubric like "no public item lost its doc comment" cannot be
//! answered from a file that no longer contains the comment, because what was
//! deleted is not in the text.
//!
//! So F8's assertion is the one that only the change can satisfy — the reviewing
//! prompt contains **a line the run deleted**. A `changes` list built from
//! post-change contents passes every weaker assertion and fails that one.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    run_with, ApproveAll, ModelReviewer, Policy, Provider, Review, ReviewRequest, Reviewer,
    Reviewing, Store, TaskContract, Verification,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------- scaffolding

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
            usage: Some(Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// The reviewing model: passes everything, and keeps what it was shown.
#[derive(Debug)]
struct Judge {
    seen: Arc<Mutex<Vec<String>>>,
}

impl Judge {
    fn new() -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn seen(&self) -> Arc<Mutex<Vec<String>>> {
        Arc::clone(&self.seen)
    }
}

impl Provider for Judge {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req.user.clone());
        Ok(CompletionResponse {
            text: Some(r#"{"passed": true}"#.into()),
            usage: Some(Usage {
                prompt_tokens: 30,
                completion_tokens: 5,
                total_tokens: 35,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn model_hint(&self) -> Option<&str> {
        Some("judge-model")
    }
}

/// A reviewer written against 0.41.0: one method, and no idea the other exists.
#[derive(Debug)]
struct OnlyReview {
    seen: Mutex<Vec<ReviewRequest>>,
}

impl OnlyReview {
    fn new() -> Self {
        Self {
            seen: Mutex::new(Vec::new()),
        }
    }
}

impl Reviewer for OnlyReview {
    fn review<'a>(&'a self, request: ReviewRequest) -> Reviewing<'a> {
        self.seen.lock().unwrap().push(request);
        Box::pin(async { Ok(Review::passed()) })
    }

    fn model(&self) -> Option<&str> {
        None
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// The line the run deletes. Distinctive, and present nowhere else in this file's
/// fixtures, so finding it in the prompt can only mean one thing.
const DELETED: &str = "/// every public item here keeps its doc comment";
const ADDED: &str = "pub fn added() {}";

/// A workspace with one file the run will change and one it will not touch.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("kept.rs"),
        format!("{DELETED}\npub fn kept() {{}}\n"),
    )
    .unwrap();
    std::fs::write(dir.path().join("untouched.rs"), "pub fn other() {}\n").unwrap();
    dir
}

/// The run: rewrite `kept.rs` without the doc comment, and create a new file.
fn change_script() -> Vec<Vec<ToolCall>> {
    vec![vec![
        call(
            "write_file",
            json!({"path": "kept.rs", "content": format!("pub fn kept() {{}}\n{ADDED}\n")}),
        ),
        call(
            "write_file",
            json!({"path": "new.rs", "content": "pub fn brand_new() {}\n"}),
        ),
    ]]
}

fn reviewed(root: &std::path::Path, reviewer: Arc<dyn Reviewer>) -> TaskContract {
    TaskContract::workspace("tidy the module", root)
        .with_verification(Verification::Review {
            rubric: "no public item lost its doc comment".into(),
            allow_self_review: false,
        })
        .with_reviewer(reviewer)
}

// ------------------------------------------------------------------------- F8

/// F8 — the reviewer is handed the change, and the change contains what was
/// deleted.
///
/// Four assertions, and only the first is beyond a "what the run wrote" view: the
/// deleted line is in the prompt, and it is provably not in the file any more.
/// The others fix the shape — a created file has no before, an untouched file is
/// absent entirely.
#[tokio::test]
async fn the_reviewing_model_is_shown_what_the_run_removed() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let judge = Judge::new();
    let seen = judge.seen();
    let contract = reviewed(
        dir.path(),
        Arc::new(ModelReviewer::new(judge, "judge-model")),
    );

    run_with(
        &contract,
        &MockScript::new(change_script()),
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let prompts = seen.lock().unwrap();
    assert_eq!(prompts.len(), 1, "one gate, one review");
    let prompt = &prompts[0];

    let now = std::fs::read_to_string(dir.path().join("kept.rs")).unwrap();
    assert!(
        !now.contains(DELETED),
        "the file itself no longer holds the line, which is the whole point"
    );
    assert!(
        prompt.contains(DELETED),
        "the reviewer is shown what was removed: {prompt}"
    );
    assert!(prompt.contains(ADDED), "and what was added: {prompt}");
    assert!(
        prompt.contains("new.rs"),
        "a file the run created is part of the change: {prompt}"
    );
    assert!(
        !prompt.contains("untouched.rs"),
        "a file the run never touched is not: {prompt}"
    );
}

// ------------------------------------------------------------------------- F9

/// F9 — a reviewer that overrides nothing still receives the outcome.
///
/// The default forwards, so an implementation written against 0.41.0 keeps
/// getting exactly the `ReviewRequest` it got then: the files the run wrote, with
/// their contents as they stand. This is the price of zero breaks and it is
/// asserted rather than assumed.
#[tokio::test]
async fn a_reviewer_that_overrides_nothing_still_reads_the_outcome() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let reviewer = Arc::new(OnlyReview::new());
    let contract = reviewed(dir.path(), reviewer.clone());

    run_with(
        &contract,
        &MockScript::new(change_script()),
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let seen = reviewer.seen.lock().unwrap();
    assert_eq!(seen.len(), 1);
    let request = &seen[0];
    assert_eq!(request.rubric, "no public item lost its doc comment");
    let paths: Vec<String> = request
        .files
        .iter()
        .map(|(p, _)| p.to_string_lossy().into_owned())
        .collect();
    assert!(paths.iter().any(|p| p == "kept.rs"), "{paths:?}");
    assert!(paths.iter().any(|p| p == "new.rs"), "{paths:?}");
    assert!(!paths.iter().any(|p| p == "untouched.rs"), "{paths:?}");

    let kept = request
        .files
        .iter()
        .find(|(p, _)| p.to_string_lossy() == "kept.rs")
        .map(|(_, c)| c.clone())
        .unwrap();
    assert!(
        !kept.contains(DELETED) && kept.contains(ADDED),
        "the contents as they stand, which is what 0.34.0 promised: {kept}"
    );
}
