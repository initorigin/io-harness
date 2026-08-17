//! Whether a session turn may answer instead of working, said outright.
//!
//! Since 0.37.0 a turn classifies exactly when the caller declared no criterion:
//! a caller who said how the turn is judged has said it is work. That inference
//! is right for the callers it was written for and wrong for one real shape — an
//! embedder building a chat surface attaches a criterion to *every* turn, and
//! therefore loses greeting handling entirely, with no way to ask for it back.
//!
//! Four cells, written as a table because two of them are the release and two of
//! them are the promise that nothing moved:
//!
//! | `conversational` | criterion | classifies? | what it is |
//! |---|---|---|---|
//! | `None`       | none    | yes | 0.37.0's inference, untouched |
//! | `None`       | present | no  | 0.37.0's inference, untouched |
//! | `Some(true)` | present | yes | **the release** — unreachable before it |
//! | `Some(false)`| none    | no  | the opposite request, also unreachable |
//!
//! The third cell is the one that must have been impossible on the previous
//! tree. If it were reachable before, the release would be adding a spelling
//! rather than a capability.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    ApproveAll, Ignore, Policy, Provider, Session, Store, TaskContract, TurnKind, Verification,
};
use serde_json::json;

/// Answers in words on the first completion and reaches for a tool afterwards.
///
/// The first completion is what decides the question: a turn allowed to classify
/// ends there as a reply, and a turn that is not goes on to call the tool.
#[derive(Default)]
struct Greeter {
    at: AtomicUsize,
}

impl Provider for Greeter {
    fn name(&self) -> &str {
        "greeter"
    }

    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        if i == 0 {
            return Ok(CompletionResponse {
                text: Some("Hello — I am a harness.".into()),
                ..Default::default()
            });
        }
        Ok(CompletionResponse {
            text: Some(format!("working, step {i}")),
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "src/done.rs", "content": "fn done() {}\n" }),
            }],
            ..Default::default()
        })
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    dir
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Take one bounded turn under `contract` and report whether it was recorded as
/// a reply — which is what "the turn decided it was conversation" means in the
/// trace, rather than in the return value alone.
async fn classified(contract: &TaskContract, root: &std::path::Path) -> bool {
    let store = Store::memory().unwrap();
    let provider = Greeter::default();
    let mut session = Session::open(&store, root).unwrap();
    let result = session
        .turn_bounded_observed(
            contract,
            &provider,
            &store,
            &open_policy(),
            &ApproveAll,
            &Ignore,
        )
        .await
        .unwrap();

    // `kind` is what the classification decides, and it is the assertion.
    let by_kind = matches!(result.kind, TurnKind::Reply);

    // The store must agree. A turn that answered staged nothing — no step row —
    // and a turn that worked has steps, so this is the classification read back
    // out of the trace rather than out of the value the call returned.
    let steps = store.steps(result.run_id).unwrap();
    assert_eq!(
        by_kind,
        steps.is_empty(),
        "the returned turn kind and the trace must not disagree about whether the turn \
         answered: kind says reply={by_kind}, the store holds {} step(s), outcome {:?}",
        steps.len(),
        result.outcome
    );
    by_kind
}

fn gated(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("hello", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "src/done.rs".into(),
            needle: "fn done".into(),
        })
        .with_max_steps(3)
}

fn ungated(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("hello", root).with_max_steps(3)
}

/// All four cells, in one test, so a change that fixes one by breaking another
/// cannot pass.
#[tokio::test]
async fn the_turns_framing_is_inferred_by_default_and_settable_when_it_is_wrong() {
    let dir = workspace();

    // Cell 1 — no override, no criterion: 0.37.0's inference, untouched.
    assert!(
        classified(&ungated(dir.path()), dir.path()).await,
        "an unverified turn must still be allowed to answer"
    );

    // Cell 2 — no override, a criterion: 0.37.0's inference, untouched.
    assert!(
        !classified(&gated(dir.path()), dir.path()).await,
        "a judged turn must still be work by default"
    );

    // Cell 3 — THE RELEASE. A criterion and an explicit yes. Unreachable before
    // this release at any call site: the inference read `verify` and nothing
    // else, so this combination could not be expressed.
    assert!(
        classified(
            &gated(dir.path()).with_conversational_turns(true),
            dir.path()
        )
        .await,
        "a chat surface that judges every turn must be able to ask for greeting handling back"
    );

    // Cell 4 — the opposite request, equally unreachable before. An unverified
    // turn that should do the work anyway rather than be allowed to answer.
    assert!(
        !classified(
            &ungated(dir.path()).with_conversational_turns(false),
            dir.path()
        )
        .await,
        "a caller must be able to refuse classification for an unverified turn"
    );
}

/// The default is `None`, and `None` is not `Some(false)`.
///
/// Three states, and the middle one is what makes the field an `Option` rather
/// than a `bool`: "unset, infer" is a real answer and differs from both explicit
/// ones. A `bool` with `false` as its default would have made cell 1 unreachable.
#[test]
fn the_unset_state_is_a_third_state_and_not_a_spelling_of_false() {
    let dir = workspace();
    assert_eq!(ungated(dir.path()).conversational, None);
    assert_eq!(
        ungated(dir.path())
            .with_conversational_turns(false)
            .conversational,
        Some(false)
    );
    assert_eq!(
        ungated(dir.path())
            .with_conversational_turns(true)
            .conversational,
        Some(true)
    );
}

/// The override reaches a session turn and does not reach a one-shot run.
///
/// A one-shot `run_*` drives the loop with `TurnExtras::default()` and never
/// classifies at all, so this field is inert there. Stated in the rustdoc and
/// asserted here, because "it has no effect" is the kind of claim that quietly
/// stops being true.
#[tokio::test]
async fn a_one_shot_run_is_untouched_by_the_override() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Greeter::default();

    // A GATED contract, so the run cannot end on the first completion for the
    // ordinary reason — `finished` requires `Verification::None` — plus an
    // explicit `Some(true)`. If the override leaked into the one-shot path the
    // run would answer and stop; it must go past the wordy first completion and
    // call the tool instead.
    //
    // The ungated shape would prove nothing here: an unverified one-shot run ends
    // on a completion with no tool call whatever this field says, which is
    // 0.17.0's `RunOutcome::Finished` and not classification.
    let contract = gated(dir.path()).with_conversational_turns(true);
    let result = io_harness::run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.len() > 1,
        "a one-shot run does not classify, so an explicit `conversational` must not stop it \
         on the first completion: {} step(s), outcome {:?}",
        steps.len(),
        result.outcome
    );
}
