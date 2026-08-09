//! A session's whole conversation, read back as one artifact (0.43.0).
//!
//! The discriminating assertion is `F7`'s: a branched session's export contains
//! the turn `Session::history` no longer returns. An export built from the path
//! passes every other assertion here and fails that one — and the turns a branch
//! took off the path are exactly the ones no other surface will show you, which is
//! the reason the export exists at all.
//!
//! `F8` is the other half and is about what an export must *not* do: it is a read.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, Usage};
use io_harness::{ApproveAll, Policy, Provider, Session, Store};

/// Answers every turn with a line of text and counts what it was asked.
struct Talker {
    said: Vec<String>,
    at: AtomicUsize,
    calls: AtomicUsize,
}

impl Talker {
    fn new(said: &[&str]) -> Self {
        Self {
            said: said.iter().map(|s| (*s).to_string()).collect(),
            at: AtomicUsize::new(0),
            calls: AtomicUsize::new(0),
        }
    }
}

impl Provider for Talker {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some(
                self.said
                    .get(i)
                    .cloned()
                    .unwrap_or_else(|| "nothing more".into()),
            ),
            usage: Some(Usage {
                prompt_tokens: 5,
                completion_tokens: 2,
                total_tokens: 7,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
}

// ---------------------------------------------------------------- F7

/// F7 — the export renders the whole tree, including what the model can no longer
/// see.
#[tokio::test]
async fn a_branched_session_exports_the_turn_history_no_longer_returns() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Talker::new(&[
        "a blue-green cutover would work",
        "here is the blue-green plan",
        "here is the read-only window plan",
    ]);
    let mut session = Session::open(&store, dir.path()).unwrap();

    let first = session
        .turn(
            "draft a migration plan",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    session
        .turn(
            "do it with a blue-green cutover",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // The wrong direction. Go back and take the other one.
    session.branch_from(&store, first.turn_id).unwrap();
    session
        .turn(
            "do it with a read-only window instead",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let path = session.history(&store).unwrap();
    assert_eq!(path.len(), 2, "the path is the plan and the second answer");

    let transcript = session.transcript(&store).unwrap();
    assert_eq!(
        transcript.turns.len(),
        3,
        "the export must hold the branched-away turn too"
    );
    assert_eq!(transcript.session_id, session.id());

    // Oldest first, and the one off the path is marked rather than dropped.
    let ids: Vec<i64> = transcript.turns.iter().map(|t| t.turn_id).collect();
    let mut sorted = ids.clone();
    sorted.sort_unstable();
    assert_eq!(ids, sorted, "turns must be in id order");

    let off: Vec<&str> = transcript
        .turns
        .iter()
        .filter(|t| !t.on_path)
        .map(|t| t.prompt.as_str())
        .collect();
    assert_eq!(
        off,
        vec!["do it with a blue-green cutover"],
        "exactly the branched-away turn is off the path"
    );

    // And the rendering carries both branches, with the abandoned one marked.
    let md = transcript.to_markdown();
    assert!(md.contains("do it with a blue-green cutover"), "{md}");
    assert!(md.contains("do it with a read-only window instead"), "{md}");
    assert!(md.contains("branched away from"), "{md}");
    assert!(md.contains("here is the blue-green plan"), "{md}");
}

#[test]
fn an_empty_session_renders_rather_than_failing() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let session = Session::open(&store, dir.path()).unwrap();

    let transcript = session.transcript(&store).unwrap();
    assert!(transcript.turns.is_empty());
    let md = transcript.to_markdown();
    assert!(md.contains("No turns"), "{md}");
    assert!(md.contains(&format!("Session {}", session.id())), "{md}");
}

/// A turn's folds are rendered where the steps behind them used to be — the half
/// that makes compaction honest rather than lossy.
#[tokio::test]
async fn a_summary_is_rendered_as_standing_in_for_what_it_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Talker::new(&["done"]);
    let mut session = Session::open(&store, dir.path()).unwrap();
    let turn = session
        .turn(
            "what changed?",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // Written directly: what is under test is the rendering, not the fold, and
    // the fold has its own suite.
    store
        .put_summary(
            turn.run_id,
            9,
            21,
            "Read the lexer; kept the token enum.",
            8,
        )
        .unwrap();

    let md = session.transcript(&store).unwrap().to_markdown();
    assert!(
        md.contains("21 earlier observations were summarised"),
        "{md}"
    );
    assert!(md.contains("kept the token enum"), "{md}");
    assert!(md.contains("At step 9"), "{md}");
}

// ---------------------------------------------------------------- F8

/// F8 — a transcript is a read.
#[tokio::test]
async fn exporting_calls_no_provider_and_writes_no_row() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = Talker::new(&["done"]);
    let mut session = Session::open(&store, dir.path()).unwrap();
    let turn = session
        .turn(
            "what changed?",
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let before = provider.calls.load(Ordering::SeqCst);
    let calls_before = store.provider_calls(turn.run_id).unwrap().len();

    // Three exports, so a lazy write on the first would still show up.
    for _ in 0..3 {
        let _ = session.transcript(&store).unwrap().to_markdown();
    }

    assert_eq!(
        provider.calls.load(Ordering::SeqCst),
        before,
        "exporting asked the model something"
    );
    assert_eq!(
        store.provider_calls(turn.run_id).unwrap().len(),
        calls_before,
        "exporting billed something"
    );
    assert!(
        store.summaries(turn.run_id).unwrap().is_empty(),
        "exporting wrote a summary"
    );
}
