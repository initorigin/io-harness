//! Images through the full loop: who may look at one, and who may receive it.
//!
//! These prove the 0.15.0 image half at the run level rather than at the type
//! level. The unit tests in `src/provider/mod.rs` prove `Media` refuses what it
//! should; these prove the refusal is actually reached by an agent, on the real
//! path, and that the negative control passes — which is what makes the refusal
//! evidence rather than a coincidence.
#![cfg(feature = "media")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, Media, Provider, Store, TaskContract, Verification};
use serde_json::json;

/// A four-byte PNG header. Not a decodable image — nothing here decodes one —
/// but real enough that the bytes under test are bytes rather than a string.
const PNG: &[u8] = &[0x89, 0x50, 0x4e, 0x47];

/// Replays a script of tool calls and keeps every request it was handed, so a
/// test can assert on what the model would have seen.
struct Spy {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
    accepts: bool,
}

impl Spy {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
            accepts: true,
        }
    }

    /// A provider that takes text only — the shape every implementation written
    /// before 0.15.0 has, since `accepts_images` defaults to false.
    fn blind(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            accepts: false,
            ..Self::new(steps)
        }
    }

    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn images_per_request(&self) -> Vec<usize> {
        self.requests().iter().map(|r| r.media.len()).collect()
    }
}

impl Provider for Spy {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "spy"
    }

    fn accepts_images(&self) -> bool {
        self.accepts
    }
}

fn view(path: &str) -> ToolCall {
    ToolCall {
        name: "view_image".into(),
        arguments: json!({ "path": path }),
    }
}

/// A workspace with one image the agent may look at and one it may not.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("private")).unwrap();
    std::fs::write(dir.path().join("chart.png"), PNG).unwrap();
    std::fs::write(dir.path().join("private/badge.png"), PNG).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not an image").unwrap();
    dir
}

/// `private/` is denied for reading; everything else is readable.
fn guarded() -> Policy {
    Policy::default()
        .layer("base")
        .allow_read("*")
        .deny_read("private/*")
}

fn contract(dir: &tempfile::TempDir) -> TaskContract {
    TaskContract::workspace("look at the picture", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "notes.txt".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(2)
}

/// A store in a temp dir. Returned with its dir so the dir outlives the store.
fn store(dir: &tempfile::TempDir) -> Store {
    Store::open(dir.path().join("trace.db")).unwrap()
}

// ---------------------------------------------------------------- F1 and F2

#[tokio::test]
async fn an_image_the_policy_denies_is_refused_and_never_reaches_the_provider() {
    let dir = fixture();
    let provider = Spy::new(vec![vec![view("private/badge.png")], vec![]]);
    let store = store(&dir);

    let result = run_with(
        &contract(&dir),
        &provider,
        &store,
        &guarded(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        provider.images_per_request().iter().all(|n| *n == 0),
        "a denied image must not reach any request: {:?}",
        provider.images_per_request()
    );

    // The refusal names the real path, not the tool — which is the whole reason
    // this is a built-in rather than a registered `Tool`.
    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal")
        .expect("the refusal must be in the trace");
    assert_eq!(refusal.act, "read");
    assert_eq!(
        refusal.target, "private/badge.png",
        "the refusal names the file, not the tool"
    );
    assert_eq!(refusal.rule.as_deref(), Some("private/*"));
}

#[tokio::test]
async fn the_same_image_under_a_permissive_policy_does_reach_the_provider() {
    // The negative control for the test above. Without it, that test would pass
    // against a build where `view_image` never worked at all.
    let dir = fixture();
    let provider = Spy::new(vec![vec![view("private/badge.png")], vec![]]);

    run_with(
        &contract(&dir),
        &provider,
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        provider.images_per_request(),
        vec![0, 1],
        "the image is attached to the step after the one that looked at it"
    );
    let attached = &provider.requests()[1].media[0];
    assert_eq!(attached.media_type, "image/png");
    assert_eq!(attached.byte_len(), PNG.len());
}

// ------------------------------------------------------------------- F3

#[tokio::test]
async fn a_provider_that_does_not_accept_images_refuses_before_sending_anything() {
    let dir = fixture();
    let provider = Spy::blind(vec![vec![]]);
    let result = run_with(
        &contract(&dir).with_images([Media::image("image/png", PNG).unwrap()]),
        &provider,
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await;

    let err = result.expect_err("a text-only provider must refuse a request carrying an image");
    let message = err.to_string();
    assert!(message.contains("does not accept image input"), "{message}");
    assert!(
        provider.requests().is_empty(),
        "the refusal must happen before the provider is reached, so nothing is spent"
    );
}

#[tokio::test]
async fn a_provider_that_accepts_images_receives_the_contract_image_every_step() {
    // The control for the refusal above, and the caller-side half of the
    // release: the task is *about* these images, so they ride every step rather
    // than being attached once.
    let dir = fixture();
    let provider = Spy::new(vec![vec![], vec![]]);
    run_with(
        &contract(&dir).with_images([Media::image("image/png", PNG).unwrap()]),
        &provider,
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(provider.images_per_request(), vec![1, 1]);
}

// ------------------------------------------------------------------- F5

#[tokio::test]
async fn a_viewed_image_rides_one_request_and_is_then_dropped() {
    let dir = fixture();
    let provider = Spy::new(vec![vec![view("chart.png")], vec![], vec![]]);
    run_with(
        &contract(&dir).with_max_steps(3),
        &provider,
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    // Looked at on step 1, seen on step 2, gone by step 3. A viewed image is a
    // tool result, not a permanent part of the conversation.
    assert_eq!(provider.images_per_request(), vec![0, 1, 0]);
}

#[tokio::test]
async fn the_trace_records_the_digest_and_the_size_rather_than_the_bytes() {
    let dir = fixture();
    let provider = Spy::new(vec![vec![view("chart.png")], vec![]]);
    let store = store(&dir);
    let result = run_with(
        &contract(&dir),
        &provider,
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    let trace = steps
        .iter()
        .map(|s| format!("{} {} {}", s.decision, s.result, s.prompt))
        .collect::<String>();
    assert!(trace.contains("digest"), "{trace}");
    assert!(trace.contains("4 bytes"), "{trace}");
    // The encoded image itself must not be in the store: a trace that held the
    // bytes would grow by megabytes a step in the long unattended runs this
    // crate exists for.
    let encoded = Media::image("image/png", PNG).unwrap().base64;
    assert!(
        !trace.contains(&encoded),
        "the trace must not carry the image bytes"
    );
}

#[tokio::test]
async fn a_file_that_is_not_an_image_is_reported_rather_than_guessed_at() {
    let dir = fixture();
    let provider = Spy::new(vec![vec![view("notes.txt")], vec![]]);
    run_with(
        &contract(&dir),
        &provider,
        &store(&dir),
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    // An observation the model can act on, and no image on the wire — not an
    // error that ends the run.
    assert_eq!(provider.images_per_request(), vec![0, 0]);
    let second = &provider.requests()[1].user;
    assert!(second.contains("not an image"), "{second}");
}

// ---------------------------------------------------------------------------
// F5 (0.55.0) — the tool accepts what the door accepts, and says what it did
// ---------------------------------------------------------------------------

/// A 2×2 24-bit BMP, header and all. Written out rather than generated, because
/// this test crate has no image encoder — and a real file is what the claim is
/// about anyway.
const BMP: &[u8] = &[
    0x42, 0x4d, 0x46, 0, 0, 0, 0, 0, 0, 0, 0x36, 0, 0, 0, 0x28, 0, 0, 0, 2, 0, 0, 0, 2, 0, 0, 0, 1,
    0, 0x18, 0, 0, 0, 0, 0, 0x10, 0, 0, 0, 0x13, 0x0b, 0, 0, 0x13, 0x0b, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    0, 0xff, 0, 0, 0xff, 0, 0, 0, 0, 0, 0xff, 0, 0, 0xff, 0, 0, 0, 0,
];

#[tokio::test]
async fn a_bmp_reaches_the_provider_as_a_png_and_the_trace_says_it_was_converted() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("scan.bmp"), BMP).unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not an image").unwrap();
    let provider = Spy::new(vec![vec![view("scan.bmp")], vec![]]);
    let store = store(&dir);

    let result = run_with(
        &contract(&dir),
        &provider,
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    // Before 0.55.0 this was refused at the doorstep with the vendors' list of
    // four types, and nothing was attached.
    let attached: Vec<_> = provider
        .requests()
        .iter()
        .flat_map(|r| r.media.clone())
        .collect();
    assert_eq!(attached.len(), 1, "the BMP reached a request");
    assert_eq!(
        attached[0].media_type, "image/png",
        "and reached it as a PNG, because the wire set is still four"
    );

    let steps = store.steps(result.run_id).unwrap();
    let obs: String = steps.iter().map(|s| s.result.as_str()).collect();
    assert!(
        obs.contains("image/bmp converted to image/png"),
        "the trace shows the bytes on the wire are not the bytes on disk: {obs}"
    );
}

#[tokio::test]
async fn an_svg_is_refused_by_name_through_the_tool_and_attaches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("diagram.svg"), b"<svg/>").unwrap();
    std::fs::write(dir.path().join("notes.txt"), "not an image").unwrap();
    let provider = Spy::new(vec![vec![view("diagram.svg")], vec![]]);
    let store = store(&dir);

    let result = run_with(
        &contract(&dir),
        &provider,
        &store,
        &Policy::permissive(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        provider.images_per_request().iter().all(|n| *n == 0),
        "nothing was attached: {:?}",
        provider.images_per_request()
    );
    let obs: String = store
        .steps(result.run_id)
        .unwrap()
        .iter()
        .map(|s| s.result.as_str())
        .collect();
    assert!(
        obs.contains("SVG") && obs.contains("resvg"),
        "the refusal names the format and the fix, not the vendors' list: {obs}"
    );
}
