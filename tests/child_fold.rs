//! What a parent reads back from a child it spawned (0.86.0).
//!
//! 0.50.0 made a child's conclusion travel — the text of its last completion,
//! composed into the parent's ledger beside the outcome. This file asserts the
//! half that release did not: that the composed text reaches the parent's **next
//! request**, as the observation for the spawn call that produced it, in spawn
//! order, and that a child which said nothing says so there rather than being
//! absent.
//!
//! The provider is a scripted mock that records every request it is asked to
//! complete, because the claim is about what went out on the wire and not about
//! what the ledger holds. Asserting on the ledger would pass while the request
//! carried nothing, which is the failure this file exists to catch.
//!
//! Negative control: `a_child_that_said_nothing_is_reported_as_having_said
//! _nothing` fails if the fold is silently dropped, and
//! `the_second_childs_text_does_not_arrive_before_the_firsts` fails if the two
//! results are folded in completion order rather than spawn order.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_tree, ApproveAll, Containment, Policy, Provider, RunOutcome, Store, TaskContract,
    Verification,
};
use serde_json::json;

/// A fixed script of `(text, calls)` pairs, one per `complete` call, that keeps
/// every request it was given.
struct Capturing {
    steps: Vec<(Option<&'static str>, Vec<ToolCall>)>,
    at: AtomicUsize,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl Capturing {
    fn new(steps: Vec<(Option<&'static str>, Vec<ToolCall>)>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Every request, in the order the tree made them.
    fn requests(&self) -> Vec<CompletionRequest> {
        self.seen.lock().unwrap().clone()
    }
}

impl Provider for Capturing {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req);
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        let (text, calls) = self.steps.get(i).cloned().unwrap_or((None, Vec::new()));
        Ok(CompletionResponse {
            text: text.map(str::to_string),
            tool_calls: calls,
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

fn spawn(goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({ "goal": goal, "verify_file": file, "verify_contains": needle }),
    )
}

fn spawn_detached(goal: &str, file: &str, needle: &str) -> ToolCall {
    call(
        "spawn_agent",
        json!({
            "goal": goal,
            "verify_file": file,
            "verify_contains": needle,
            "wait": false,
        }),
    )
}

fn read(path: &str) -> ToolCall {
    call("read_file", json!({ "path": path }))
}

fn write(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// Everything one request carries that a model would read, as one string.
///
/// Both halves, because 0.49.0 sends `messages` when it has them and `user`
/// when it does not, and a claim about "the next request" that read only one of
/// them would pass on a request that carried the text in the other.
fn wire_text(req: &CompletionRequest) -> String {
    let messages = serde_json::to_string(&req.messages).unwrap();
    format!("{}\n{messages}", req.user)
}

/// The parent's requests, in order. The children's are dropped: a child's own
/// request is not where a parent reads anything.
fn parent_requests(provider: &Capturing, goal: &str) -> Vec<CompletionRequest> {
    provider
        .requests()
        .into_iter()
        .filter(|r| wire_text(r).contains(goal))
        .collect()
}

const PARENT_GOAL: &str = "Delegate both halves and combine what comes back.";

/// Two children, two conclusions, and the parent reads both of them.
#[tokio::test]
async fn both_childrens_texts_reach_the_parents_next_request_in_spawn_order() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace(PARENT_GOAL, dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "combined.txt".into(),
            needle: "ab".into(),
        })
        .with_max_steps(6);

    // Sequential: the parent's first step makes both spawn calls, child A runs to
    // completion, then child B, then the parent takes its second step.
    let provider = Capturing::new(vec![
        (
            None,
            vec![
                spawn("first half", "a.txt", "A"),
                spawn("second half", "b.txt", "B"),
            ],
        ),
        (Some("ALPHA-CONCLUSION"), vec![write("a.txt", "A")]),
        (Some("BETA-CONCLUSION"), vec![write("b.txt", "B")]),
        (None, vec![write("combined.txt", "ab")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the tree finished: {:?}",
        result.outcome
    );

    let parent = parent_requests(&provider, PARENT_GOAL);
    assert!(
        parent.len() >= 2,
        "the parent took a second step, so there is a next request to read: {} requests",
        parent.len()
    );
    let next = wire_text(&parent[1]);

    let alpha = next.find("ALPHA-CONCLUSION");
    let beta = next.find("BETA-CONCLUSION");
    assert!(
        alpha.is_some(),
        "the first child's conclusion reached the parent's next request. It carried:\n{next}"
    );
    assert!(
        beta.is_some(),
        "the second child's conclusion reached the parent's next request. It carried:\n{next}"
    );
    assert!(
        alpha < beta,
        "the children fold in spawn order, not completion order"
    );
}

/// A child that never said anything says so, rather than folding as an absence
/// the parent cannot tell from a child that was never spawned.
#[tokio::test]
async fn a_child_that_said_nothing_is_reported_as_having_said_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let contract = TaskContract::workspace(PARENT_GOAL, dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "combined.txt".into(),
            needle: "ab".into(),
        })
        .with_max_steps(6);

    let provider = Capturing::new(vec![
        (None, vec![spawn("first half", "a.txt", "A")]),
        // The child works and says nothing at all.
        (None, vec![write("a.txt", "A")]),
        (None, vec![write("combined.txt", "ab")]),
    ]);
    let store = Store::memory().unwrap();

    run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let parent = parent_requests(&provider, PARENT_GOAL);
    assert!(parent.len() >= 2, "the parent took a second step");
    let next = wire_text(&parent[1]);
    assert!(
        next.contains("returned nothing"),
        "a silent child is named as one in the parent's next request. It carried:\n{next}"
    );
}

/// A recording of a tree replays to the same folded child result.
///
/// The fold is composed from the child's own trace rather than carried out of
/// its loop, so a replayed run rebuilds it from rows a replayed child wrote. If
/// either half drifted, the parent's request would differ and this would show it
/// as a difference in the text the parent read.
#[tokio::test]
async fn a_recorded_tree_replays_to_the_same_folded_child_result() {
    let script = || {
        vec![
            (None, vec![spawn("first half", "a.txt", "A")]),
            (Some("ALPHA-CONCLUSION"), vec![write("a.txt", "A")]),
            (None, vec![write("combined.txt", "ab")]),
        ]
    };
    let contract = |root: &std::path::Path| {
        TaskContract::workspace(PARENT_GOAL, root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "combined.txt".into(),
                needle: "ab".into(),
            })
            .with_max_steps(6)
    };
    let policy = Policy::permissive();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("recording.json");
    let recorder = io_harness::provider::Record::new(Capturing::new(script()));
    let store = Store::memory().unwrap();
    run_tree(
        &contract(dir.path()),
        &recorder,
        &store,
        &policy,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    recorder.save(&path).unwrap();
    let recorded_fold = folded_child_text(&store);

    // Back to the workspace the recording was made against, or the replay
    // assembles a different observation section and misses on the first key.
    for f in ["a.txt", "combined.txt"] {
        let _ = std::fs::remove_file(dir.path().join(f));
    }

    let store = Store::memory().unwrap();
    run_tree(
        &contract(dir.path()),
        &io_harness::provider::Replay::load(&path).unwrap(),
        &store,
        &policy,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        recorded_fold.contains("ALPHA-CONCLUSION"),
        "the recorded run folded the child's conclusion: {recorded_fold:?}"
    );
    assert_eq!(
        recorded_fold,
        folded_child_text(&store),
        "the replayed run folds the same text"
    );
}

/// What every run in this store folded back from a child, as one string.
///
/// Read off the parent's own ledger rows rather than off a request, because a
/// replayed run answers from a recording and the point is what the run composed.
fn folded_child_text(store: &Store) -> String {
    let mut out = String::new();
    for run in store.runs().unwrap() {
        for obs in store.observations(run).unwrap() {
            if obs.text.contains("[child ") {
                out.push_str(&obs.text);
            }
        }
    }
    out
}

/// A detached child's report lands on a step the parent has already taken, and
/// the step it lands on keeps its assistant turn.
///
/// This is the ordinal hazard `Piece::of` documents from the other side. A
/// child's observation is a `Piece::Result`, so it takes the next tool-call
/// position on the step it is recorded against — and a child collected on a
/// *later* step takes a position that step's completion never called. The
/// transcript's bounds check then fails for the whole step, which drops its
/// assistant turn and its native tool-call blocks and sends it as flat prose.
/// The failure is a `tracing::warn!` and nothing else, so the request goes out
/// malformed and the run continues.
#[tokio::test]
async fn a_detached_childs_report_does_not_cost_its_step_the_assistant_turn() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("seed.txt"), "seed").unwrap();
    let contract = TaskContract::workspace(PARENT_GOAL, dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "combined.txt".into(),
            needle: "ab".into(),
        })
        .with_max_steps(8);

    // Step 1 detaches a child and does not wait. Step 2 makes exactly one call of
    // its own; the child's report is collected onto that same step, so the step
    // carries two results for one call.
    let provider = Capturing::new(vec![
        (None, vec![spawn_detached("first half", "a.txt", "A")]),
        (Some("DETACHED-CONCLUSION"), vec![write("a.txt", "A")]),
        (None, vec![read("seed.txt")]),
        (None, vec![write("combined.txt", "ab")]),
    ]);
    let store = Store::memory().unwrap();

    run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    // Every request the parent sent after the collection must still be a
    // role-tagged transcript: an assistant turn for each step that called
    // something, and one results batch answering it.
    let parent = parent_requests(&provider, PARENT_GOAL);
    let last = parent.last().expect("the parent took at least one step");
    let assistant_turns = last
        .messages
        .iter()
        .filter(|m| matches!(m, io_harness::Message::Assistant { .. }))
        .count();
    assert!(
        assistant_turns >= 2,
        "each step the parent took keeps its assistant turn once a detached child \
         has folded. The request carried {assistant_turns} assistant turn(s):\n{}",
        wire_text(last)
    );
}
