//! Images on a conversational turn (0.43.0).
//!
//! `TaskContract::with_images` has taken images since the `media` feature
//! shipped, and every `Session` turn entry point builds its contract from a
//! `&str`. So the one path an operator would hand a screenshot to was the only
//! path that could not take one. `Session::attach` is that path — one staging
//! method rather than an images-carrying variant of each of the six turn shapes.
//!
//! Every assertion here is made against the `CompletionRequest` the provider
//! actually received, never against the contract that was built: what matters is
//! that the media reached the wire.

#![cfg(feature = "media")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, Media, Usage};
use io_harness::{
    ApproveAll, Ignore, Policy, Provider, Session, Store, TaskContract, Verification,
};

/// Records the media on every request it is handed.
struct Watcher {
    seen: Arc<Mutex<Vec<usize>>>,
    calls: Arc<AtomicUsize>,
    accepts: bool,
}

impl Watcher {
    fn new(accepts: bool) -> Self {
        Self {
            seen: Arc::new(Mutex::new(Vec::new())),
            calls: Arc::new(AtomicUsize::new(0)),
            accepts,
        }
    }
}

impl Provider for Watcher {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req.media.len());
        Ok(CompletionResponse {
            text: Some("looked".into()),
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                total_tokens: 7,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn accepts_images(&self) -> bool {
        self.accepts
    }
}

fn shot() -> Media {
    Media::image("image/png", b"pretend this is a png").unwrap()
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
}

// ---------------------------------------------------------------- F9

/// F9 — an image reaches the wire through a conversational turn, and rides one
/// turn only.
#[tokio::test]
async fn a_staged_image_reaches_the_request_and_only_the_next_turn() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Watcher::new(true);
    let mut session = Session::open(&store, dir.path()).unwrap();

    session.attach([shot()]);
    session
        .turn(
            "why is this misaligned?",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // No second attach.
    session
        .turn(
            "and the one below it?",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let seen = provider.seen.lock().unwrap().clone();
    assert_eq!(
        seen.first().copied(),
        Some(1),
        "the image never reached the wire"
    );
    assert_eq!(
        seen.get(1).copied(),
        Some(0),
        "the staging outlived the turn it was attached for"
    );
}

/// The same through the observed entry point: staging is orthogonal to how the
/// turn is driven, which is the whole argument for one method over six.
#[tokio::test]
async fn the_observed_entry_point_carries_it_too() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Watcher::new(true);
    let mut session = Session::open(&store, dir.path()).unwrap();

    session.attach([shot()]);
    session
        .turn_observed(
            "what is wrong here?",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &Ignore,
        )
        .await
        .unwrap();

    assert_eq!(provider.seen.lock().unwrap().first().copied(), Some(1));
}

/// A bounded turn whose contract carries its own images sends both: `attach` adds
/// to the contract rather than replacing what it had.
#[tokio::test]
async fn a_contracts_own_images_are_kept_beside_the_staged_one() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Watcher::new(true);
    let mut session = Session::open(&store, dir.path()).unwrap();

    let contract = TaskContract::workspace("compare these", dir.path())
        .with_verification(Verification::None)
        .with_max_steps(1)
        .with_images([shot()]);

    session.attach([shot()]);
    session
        .turn_bounded(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        provider.seen.lock().unwrap().first().copied(),
        Some(2),
        "the contract's own image and the staged one must both be sent"
    );
}

// ---------------------------------------------------------------- F10

/// F10 — a provider that does not accept images refuses, before anything is sent.
#[tokio::test]
async fn a_provider_that_takes_no_images_refuses_before_the_first_request() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Watcher::new(false);
    let mut session = Session::open(&store, dir.path()).unwrap();

    session.attach([shot()]);
    let refused = session
        .turn(
            "look at this",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await;

    assert!(refused.is_err(), "a text-only provider took an image");
    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        0,
        "the refusal cost a completion: it must happen before anything is sent"
    );
}
