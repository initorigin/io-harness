//! Sub-agents inside a session turn (0.39.0).
//!
//! The claim: a conversation can fan out. `Session::turn_contained` drives the
//! loop that owns [`SPAWN_TOOL`], so a turn decomposes into contained children
//! under the session's own policy, one shared per-turn ledger, and the observer
//! the operator is already reading — while staying **one** turn in the
//! conversation tree.
//!
//! Everything here is driven through the real loop with a scripted provider that
//! records the requests it was handed, because "the root was offered the spawn
//! tool and a plain turn was not" and "the child did not read the conversation"
//! are only observable as facts about what was actually sent.
//!
//! The negative controls are the point of the file: F1 checks all five
//! pre-0.39.0 turn shapes, and F7 checks the child's request as well as the
//! root's. An implementation that registered the tool one level up, or seeded
//! every agent with the transcript, passes every positive assertion here.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::approve::{Decision, DecisionFuture, Request};
use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_tree_with_decision, ApproveAll, Approver, Containment, Policy, Provider, RunOutcome,
    Session, Store, TaskContract, TurnKind, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one call.
#[derive(Clone)]
enum Say {
    /// Answer with text and no tool call, which ends a `Verification::None` turn.
    Text(&'static str),
    /// Make tool calls, so the turn takes a step and carries on.
    Calls(Vec<ToolCall>),
}

/// One completion this provider served, kept whole: a claim about what the model
/// was asked is a claim about this.
struct Seen {
    system: String,
    user: String,
    tools: Vec<String>,
}

/// Plays a script and keeps every request it was handed.
struct Mock {
    script: Vec<Say>,
    at: AtomicUsize,
    seen: Mutex<Vec<Seen>>,
}

impl Mock {
    fn new(script: Vec<Say>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The tool names offered on the nth completion.
    fn tools(&self, n: usize) -> Vec<String> {
        self.seen.lock().unwrap()[n].tools.clone()
    }

    fn user(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].user.clone()
    }

    fn system(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].system.clone()
    }

    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for Mock {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(Seen {
            system: req.system.clone(),
            user: req.user.clone(),
            tools: req.tools.iter().map(|t| t.name.clone()).collect(),
        });
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // Past the end of the script the model stops talking, which ends an
        // unbounded turn rather than hanging the test on a step cap.
        let say = self.script.get(i).cloned().unwrap_or(Say::Text("done"));
        Ok(CompletionResponse {
            text: match &say {
                Say::Text(t) => Some((*t).to_string()),
                Say::Calls(_) => None,
            },
            tool_calls: match &say {
                Say::Calls(c) => c.clone(),
                Say::Text(_) => Vec::new(),
            },
            usage: Some(Usage {
                total_tokens: 100,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// Collects every event, so a claim about depth and child run ids is checkable.
#[derive(Default)]
struct Log {
    events: Mutex<Vec<(i64, u32, String)>>,
}

impl Log {
    fn kinds(&self, kind: &str) -> Vec<(i64, u32)> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, _, k)| k == kind)
            .map(|(run, depth, _)| (*run, *depth))
            .collect()
    }
}

impl Observer for Log {
    fn event(&self, event: &RunEvent) -> Flow {
        let kind = match &event.kind {
            EventKind::Spawned { .. } => "spawned",
            EventKind::SpawnRefused { .. } => "spawn_refused",
            EventKind::Fleet { .. } => "fleet",
            EventKind::Answered { .. } => "answered",
            EventKind::Step { .. } => "step",
            _ => "other",
        };
        self.events
            .lock()
            .unwrap()
            .push((event.run_id, event.depth, kind.to_string()));
        Flow::Continue
    }
}

/// An approver that defers, so a turn stops mid-fan-out with a real pending
/// request rather than a simulated one.
struct Defers;

impl Approver for Defers {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Defer })
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

fn write(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn roomy() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

// ------------------------------------------------------------------------- F1

#[tokio::test]
async fn a_contained_turn_is_offered_the_spawn_tool() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let mock = Mock::new(vec![Say::Text("nothing to do")]);

    session
        .turn_contained(
            "review every file",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert!(
        mock.tools(0).contains(&"spawn_agent".to_string()),
        "the root of a contained turn may decompose: {:?}",
        mock.tools(0)
    );
}

#[tokio::test]
async fn no_other_turn_shape_is_offered_the_spawn_tool() {
    // The negative control, and the assertion that matters most in this file: an
    // implementation that registered the tool in `drive`, or in the shared prompt
    // builder, passes the test above and fails here — and that failure is every
    // embedding application's sessions silently gaining the ability to fan out
    // under a containment nobody passed.
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive();

    // 1. turn
    let mut session = Session::open(&store, dir.path()).unwrap();
    let plain = Mock::new(vec![Say::Text("hi")]);
    session
        .turn("hello", &plain, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    // 2. turn_observed
    let observed = Mock::new(vec![Say::Text("hi")]);
    session
        .turn_observed(
            "hello",
            &observed,
            &store,
            &policy,
            &ApproveAll,
            &Log::default(),
        )
        .await
        .unwrap();

    // 3. turn_steered
    let steered = Mock::new(vec![Say::Text("hi")]);
    let (_steer, inbox) = io_harness::Steer::channel();
    session
        .turn_steered(
            "hello",
            &steered,
            &store,
            &policy,
            &ApproveAll,
            &Log::default(),
            &inbox,
        )
        .await
        .unwrap();

    // 4. turn_bounded
    let contract = TaskContract::workspace("write a file", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "a.txt".into(),
            needle: "A".into(),
        })
        .with_max_steps(1);
    let bounded = Mock::new(vec![Say::Calls(vec![write("a.txt", "A")])]);
    session
        .turn_bounded(&contract, &bounded, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    // 5. turn_bounded_observed
    let bounded_observed = Mock::new(vec![Say::Calls(vec![write("b.txt", "A")])]);
    let contract_b = TaskContract::workspace("write another file", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "A".into(),
        })
        .with_max_steps(1);
    session
        .turn_bounded_observed(
            &contract_b,
            &bounded_observed,
            &store,
            &policy,
            &ApproveAll,
            &Log::default(),
        )
        .await
        .unwrap();

    for (name, mock) in [
        ("turn", &plain),
        ("turn_observed", &observed),
        ("turn_steered", &steered),
        ("turn_bounded", &bounded),
        ("turn_bounded_observed", &bounded_observed),
    ] {
        assert!(
            !mock.tools(0).contains(&"spawn_agent".to_string()),
            "{name} must not offer the spawn tool: {:?}",
            mock.tools(0)
        );
    }
}

// ------------------------------------------------------------------------- F2

#[tokio::test]
async fn a_child_spawned_inside_a_turn_belongs_to_that_turns_run() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let log = Log::default();
    let mut session = Session::open(&store, dir.path()).unwrap();

    // The root spawns two children in one step; each child writes its file; the
    // root then stops on text, which ends an unbounded turn.
    let mock = Mock::new(vec![
        Say::Calls(vec![
            spawn("write a", "a.txt", "A"),
            spawn("write b", "b.txt", "B"),
        ]),
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Calls(vec![write("b.txt", "B")]),
        Say::Text("both done"),
    ]);

    let turn = session
        .turn_contained_observed(
            "write a and b, one sub-agent each",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
            &log,
        )
        .await
        .unwrap();

    // Stored rows, not the parent's narration of its own children.
    let children = store.children(turn.run_id).unwrap();
    assert_eq!(children.len(), 2, "two child runs under the turn's run");
    let spawns = store
        .agent_events(turn.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "spawn")
        .count();
    assert_eq!(spawns, 2, "two spawn edges recorded under the turn");

    // Each child has a run of its own, and the observer saw it at depth 1.
    for child in &children {
        assert!(
            store.run_summary(*child).unwrap().is_some(),
            "child {child} has its own run row"
        );
        assert!(
            !store.steps(*child).unwrap().is_empty(),
            "child {child} took its own steps"
        );
    }
    let spawned = log.kinds("spawned");
    assert_eq!(spawned.len(), 2);
    let child_steps = log.kinds("step");
    assert!(
        child_steps
            .iter()
            .any(|(run, depth)| *depth == 1 && children.contains(run)),
        "the observer saw a child's own step at depth 1: {child_steps:?}"
    );

    // Both children's work reached the shared workspace.
    for (f, c) in [("a.txt", "A"), ("b.txt", "B")] {
        assert_eq!(std::fs::read_to_string(dir.path().join(f)).unwrap(), c);
    }
}

// ------------------------------------------------------------------------- F5

#[tokio::test]
async fn a_fan_out_is_still_one_turn() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();

    let mock = Mock::new(vec![
        Say::Calls(vec![
            spawn("write a", "a.txt", "A"),
            spawn("write b", "b.txt", "B"),
            spawn("write c", "c.txt", "C"),
        ]),
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Calls(vec![write("b.txt", "B")]),
        Say::Calls(vec![write("c.txt", "C")]),
        Say::Text("all done"),
    ]);

    let turn = session
        .turn_contained(
            "write a, b and c",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    let turns = store.session_turns(session.id()).unwrap();
    assert_eq!(turns.len(), 1, "three children, still one turn");
    assert_eq!(session.head(), Some(turn.turn_id));
    assert_eq!(session.history(&store).unwrap().len(), 1);
    assert_eq!(
        store.turn_for_run(turn.run_id).unwrap(),
        Some(turn.turn_id),
        "the turn's run names the turn"
    );
    for child in store.children(turn.run_id).unwrap() {
        assert_eq!(
            store.turn_for_run(child).unwrap(),
            None,
            "child run {child} is a run, never a second turn"
        );
    }
}

// ------------------------------------------------------------------------- F6

#[tokio::test]
async fn a_contained_turn_that_answers_spawns_nothing() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let log = Log::default();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let mock = Mock::new(vec![Say::Text("I can read and edit this repository.")]);

    let turn = session
        .turn_contained_observed(
            "what can you do?",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
            &log,
        )
        .await
        .unwrap();

    assert_eq!(turn.kind, TurnKind::Reply);
    assert_eq!(mock.calls(), 1, "an answer costs one completion");
    assert!(matches!(turn.outcome, RunOutcome::Finished { steps: 0 }));
    assert!(
        store.steps(turn.run_id).unwrap().is_empty(),
        "a reply opens no run"
    );
    assert!(
        store.children(turn.run_id).unwrap().is_empty(),
        "an answered turn spawned nothing"
    );
    assert_eq!(log.kinds("answered").len(), 1);
    // And the opening it was made with is the one that permits an answer while
    // still describing an agent that may spawn.
    let system = mock.system(0);
    assert!(
        system.contains("may not be work at all"),
        "conversational opening"
    );
    assert!(system.contains("spawn_agent"), "still the tree's own world");
}

#[tokio::test]
async fn a_contained_turn_with_a_criterion_does_not_classify() {
    // The second arm of the 0.37.0 rule, on the new path: a caller who said how
    // the turn is judged has said it is work.
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let contract = TaskContract::workspace("write a", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "a.txt".into(),
            needle: "A".into(),
        })
        .with_max_steps(2);
    let mock = Mock::new(vec![Say::Text("I would rather not")]);

    let turn = session
        .turn_bounded(&contract, &mock, &store, &Policy::permissive(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(turn.kind, TurnKind::Run, "a judged turn is work");
    assert!(
        !mock.system(0).contains("may not be work at all"),
        "no conversational opening on a judged turn"
    );
}

// ------------------------------------------------------------------------- F7

#[tokio::test]
async fn the_conversation_reaches_the_root_and_not_the_children() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();

    let first = Mock::new(vec![Say::Text("the retry policy retries 429 and 503.")]);
    session
        .turn(
            "what does the retry policy retry?",
            &first,
            &store,
            &Policy::permissive(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let second = Mock::new(vec![
        Say::Calls(vec![spawn("write the note", "note.txt", "N")]),
        Say::Calls(vec![write("note.txt", "N")]),
        Say::Text("done"),
    ]);
    session
        .turn_contained(
            "write that up, one sub-agent",
            &second,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    let root = second.user(0);
    assert!(
        root.contains("what does the retry policy retry?"),
        "the root continues the conversation"
    );
    assert!(root.contains("429 and 503"), "including what it answered");

    // The child is given its goal, not the transcript.
    let child = second.user(1);
    assert!(
        child.contains("write the note"),
        "the child has its own goal: {child}"
    );
    assert!(
        !child.contains("what does the retry policy retry?"),
        "the child must not read the conversation: {child}"
    );
    assert!(
        !child.contains("429 and 503"),
        "nor what an earlier turn answered: {child}"
    );
}

// ------------------------------------------------------------------------- F3

#[tokio::test]
async fn the_total_agent_cap_refuses_a_second_spawn_inside_a_turn() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let log = Log::default();
    let mut session = Session::open(&store, dir.path()).unwrap();

    // One agent in the whole tree: the root itself. Every spawn is refused.
    let containment = Containment::new(1, 4, 2, 1_000_000);
    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("write a", "a.txt", "A")]),
        Say::Text("could not delegate, so I stopped"),
    ]);

    let turn = session
        .turn_contained_observed(
            "delegate this",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &containment,
            &log,
        )
        .await
        .unwrap();

    assert!(
        !log.kinds("spawn_refused").is_empty(),
        "the tree's own cap refused the spawn"
    );
    assert!(
        store.children(turn.run_id).unwrap().is_empty(),
        "and no child run exists"
    );
    // The parent adapts rather than failing: the turn still ended normally.
    assert!(matches!(
        turn.outcome,
        RunOutcome::Finished { .. } | RunOutcome::Success { .. }
    ));
}

#[tokio::test]
async fn the_shared_ledger_halts_a_turns_fan_out_on_spend() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();

    // 100 tokens per completion against a 150-token tree ceiling: the root's own
    // first call fits, the fan-out does not.
    let containment = Containment::new(10, 4, 2, 150);
    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("write a", "a.txt", "A")]),
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Calls(vec![spawn("write b", "b.txt", "B")]),
        Say::Calls(vec![write("b.txt", "B")]),
        Say::Text("done"),
    ]);

    let turn = session
        .turn_contained(
            "delegate a and then b",
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &containment,
        )
        .await
        .unwrap();

    assert!(
        !std::path::Path::new(&dir.path().join("b.txt")).exists(),
        "the tree stopped before the second child's work"
    );
    assert!(
        matches!(
            turn.outcome,
            RunOutcome::BudgetCeilingReached { .. } | RunOutcome::CostBudgetExceeded { .. }
        ),
        "the turn ended on the shared ceiling, got {:?}",
        turn.outcome
    );
}

// ------------------------------------------------------------------------- F4

#[tokio::test]
async fn a_child_cannot_widen_the_sessions_boundary() {
    let dir = ws();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();

    // Writes under docs/ and nowhere else — the boundary the whole fan-out
    // inherits. `deny_write` rather than a bare `allow_write`: a denied action
    // never reaches an approver, so this is the wall and not the prompt, and
    // `ApproveAll` below cannot answer it away.
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("docs/*")
        .deny_write("src/*");

    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("write the source file", "src/x.rs", "x")]),
        // The child, asked for something the session never permitted.
        Say::Calls(vec![write("src/x.rs", "pub fn x() {}")]),
        Say::Text("the child could not write there"),
    ]);

    let turn = session
        .turn_contained(
            "document the modules",
            &mock,
            &store,
            &policy,
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert!(
        !dir.path().join("src/x.rs").exists(),
        "the child's write outside the session's boundary did not land"
    );
    let child = store.children(turn.run_id).unwrap();
    assert_eq!(
        child.len(),
        1,
        "the child ran; it was its write that was refused"
    );
    let refusals = store
        .events(child[0])
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .count();
    assert!(
        refusals >= 1,
        "the refusal is recorded under the CHILD's run id"
    );
}

// ------------------------------------------------------------------------- F8

#[tokio::test]
async fn a_paused_contained_turn_resumes_as_a_tree_under_the_same_turn() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    // Asks about every write, and the approver defers: the turn stops holding a
    // real pending request.
    let policy = Policy::default().layer("app").allow_read("*");
    let contract = TaskContract::workspace("write a", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "a.txt".into(),
            needle: "A".into(),
        })
        .with_max_steps(4);

    let mock = Mock::new(vec![
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Calls(vec![write("a.txt", "A")]),
    ]);

    let turn = session
        .turn_bounded(&contract, &mock, &store, &policy, &Defers)
        .await
        .unwrap();
    // A bounded turn is the shape that can pause; the contained one pauses the
    // same way, through the same run.
    let request_id = match turn.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    // The tree resume, on the turn's own run id.
    let resumed = resume_tree_with_decision(
        &contract,
        &mock,
        &store,
        turn.run_id,
        request_id,
        Decision::approve(),
        &policy,
        &ApproveAll,
        &roomy(),
    )
    .await
    .unwrap();

    assert_eq!(resumed.run_id, turn.run_id, "the same run continued");
    assert_eq!(
        store.turn_for_run(turn.run_id).unwrap(),
        Some(turn.turn_id),
        "and it is still the same turn"
    );
    let row = store
        .session_turn(turn.turn_id)
        .unwrap()
        .expect("the turn row");
    assert_eq!(row.session_id, session.id());
    assert_eq!(row.parent_turn_id, None);
    // Stated rather than glossed, and this is the measured fact the contract owes
    // an operator: the turn row is closed with what `run_summary` said at the
    // moment the turn returned, and a run parked on an approval has no summary
    // outcome yet — so the row reads `running`, and it still reads `running`
    // after a resume the session did not drive. `docs/CONTRACT.md` says so.
    assert_eq!(row.outcome.as_deref(), Some("running"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "A",
        "the resumed run did the work"
    );
}

// ------------------------------------------------------------------------- N4

/// The two loops must not drift: each session rule exists once and both loops
/// reach it through a call.
///
/// A grep alone proves nothing (0.33.0's fact 70), so this asserts the shape
/// that a copy would break: exactly one definition per helper, and at least two
/// call sites — one per loop — for each.
#[test]
fn each_session_rule_is_one_helper_that_both_loops_call() {
    let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/run.rs")).unwrap();
    // Normalised, so a Windows checkout reads the same file this does.
    let src = src.replace("\r\n", "\n");

    for helper in [
        "seed_conversation",
        "open_turn_kind",
        "classify_first_completion",
        "drain_steer",
        "conversational_opening",
    ] {
        let defs = src.matches(&format!("fn {helper}(")).count();
        assert_eq!(defs, 1, "{helper} is defined exactly once");
        let calls = src.matches(&format!("{helper}(")).count() - defs;
        assert!(
            calls >= 2,
            "{helper} is called by both loops, found {calls} call sites"
        );
    }

    // The rules the helpers own must not be re-implemented beside them. Each of
    // these literals belongs to exactly one place — the helper — and a second
    // occurrence is the copy this test exists to fail.
    for once_only in [
        "set_turn_kind(run_id, TURN_KIND_REPLY)",
        "set_turn_kind(run_id, TURN_KIND_RUN)",
        "turn interrupted by its operator",
        "turn answered without opening a run",
    ] {
        assert_eq!(
            src.matches(once_only).count(),
            1,
            "`{once_only}` is written once, in its helper"
        );
    }
}
