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
use io_harness::provider::ToolSpec;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume_tree_with_decision, ApproveAll, Approver, Compaction, Containment, ContextBudget,
    Policy, Provider, RunOutcome, Session, Store, TaskContract, Tool, ToolFuture, Toolbox,
    TurnKind, Verification,
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

/// A phrase out of `SUMMARY_SYSTEM`, which is the only prompt in the crate that
/// carries it: how a fold's own completion is told apart from a step's.
const SUMMARISER: &str = "compacting an agent's own working notes";

/// What the summariser answers with. Distinctive, so a ledger that carries it is
/// one that was folded rather than one that happens to read that way.
const SUMMARY: &str = "ZZ-FOLDED-ZZ the agent read some notes and decided nothing.";

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
        // 0.68.0 — a fold's summarising request is answered off-script, and is not
        // recorded. It is a completion the loop makes on its own behalf rather than
        // a step anyone wrote, so letting it take a script slot would shift every
        // later `Say` by one — and letting it take a `seen` slot would shift the
        // positional claims (`mock.user(1)` is the child's) that this whole file
        // rests on. No test that folds indexes by position; every test that indexes
        // by position never folds, because its ledger is a handful of entries under
        // a budget nobody shrank.
        if req.system.contains(SUMMARISER) {
            return Ok(CompletionResponse {
                text: Some(SUMMARY.to_string()),
                usage: Some(Usage {
                    total_tokens: 100,
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
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
            // 0.68.0 — and `kinds` already returns the pair a fold has to be
            // judged on: the run it happened in and the depth it happened at.
            EventKind::Compacted { .. } => "compacted",
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

    // 0.66.0 — and now on the path this file is actually about. Until this release
    // the arm above was the only one available: a contained turn built its own
    // contract, so a *judged* contained turn could not be expressed, and the test
    // for the tree loop's half of the 0.37.0 rule was driving the flat loop.
    let contained = Mock::new(vec![Say::Text("I would rather not")]);
    let turn = session
        .turn_contained_bounded(
            &contract,
            &contained,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert_eq!(turn.kind, TurnKind::Run, "a judged contained turn is work");
    assert!(
        !contained.system(0).contains("may not be work at all"),
        "no conversational opening on a judged contained turn"
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
    let src = run_subsystem_source();
    // Normalised, so a Windows checkout reads the same file this does.
    let src = src.replace("\r\n", "\n");

    for helper in [
        "seed_conversation",
        "open_turn_kind",
        "classify_first_completion",
        "drain_steer",
        "conversational_opening",
        // 0.68.0 — whether a step's fold is forced. A rule and not a value: it
        // decides that a caller's request is consumed once, that an overflow
        // recovery consumes it too, and that only the root honours it. Spelled
        // out at each call site instead, those three would drift apart the first
        // time one loop was edited without the other.
        "fold_forced",
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
        // 0.68.0 — the take that makes a requested fold happen once. A second
        // occurrence means one loop consumed the request beside the helper rather
        // than through it, which is the copy that would let the two disagree
        // about whether a fold has already been spent.
        "std::mem::take(asked)",
    ] {
        assert_eq!(
            src.matches(once_only).count(),
            1,
            "`{once_only}` is written once, in its helper"
        );
    }
}

/// `src/run.rs` and every `src/run/<subject>.rs`, concatenated.
///
/// 0.63.0 moved the run subsystem's private machinery into submodules, so a
/// source-reading checker pointed at the parent alone now sees a fraction of it —
/// and a count that comes back zero reads exactly like a rule that was deleted.
/// The floor below is what turns "the walk went blind" into a failure instead of
/// a silent pass.
fn run_subsystem_source() -> String {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = std::fs::read_to_string(root.join("src/run.rs"))
        .expect("src/run.rs")
        .replace("\r\n", "\n");
    let mut paths: Vec<std::path::PathBuf> = std::fs::read_dir(root.join("src/run"))
        .expect("src/run/")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "rs"))
        .collect();
    paths.sort();
    assert!(
        paths.len() >= 5,
        "src/run/ holds only {} modules — the split has been undone or this walk is blind, \
         and either way every count taken from it is meaningless",
        paths.len()
    );
    for path in paths {
        all.push('\n');
        all.push_str(
            &std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n"),
        );
    }
    all
}

// ------------------------------------------------- 0.66.0: a contained turn
// the caller shaped
//
// Until this release a contained turn built its own contract from the operator's
// text, so the file above had to reach for `turn_bounded` — the flat loop — every
// time it wanted to say something about a contract. These drive the contained
// loop with the caller's own contract, which is the thing that could not be done.

/// **F5** — the contract's root is replaced by the session's.
///
/// `turn_bounded` has made this promise since 0.36.0 and states it in its rustdoc:
/// a turn is about the conversation's workspace. If the contained pair did not
/// make the same one, a fan-out would be the one way to point a session's turn at
/// somebody else's directory — and every child inherits that root.
#[tokio::test]
async fn a_contained_bounded_turn_runs_in_the_sessions_workspace() {
    let session_dir = ws();
    let elsewhere = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, session_dir.path()).unwrap();

    // The contract names the other directory, and nothing else about it is wrong.
    let contract = TaskContract::workspace("write a.txt", elsewhere.path()).with_max_steps(2);
    let mock = Mock::new(vec![Say::Calls(vec![write("a.txt", "hi")])]);

    session
        .turn_contained_bounded(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert!(
        session_dir.path().join("a.txt").exists(),
        "the turn wrote outside the session's workspace"
    );
    assert!(
        !elsewhere.path().join("a.txt").exists(),
        "the contract's own root was used, so a turn escaped its conversation"
    );
}

/// **F5**, the observed twin — it reaches the observer, and it is one turn.
///
/// A fan-out is the shape that most needs an observer: children run at once and
/// their output interleaves, so `depth` and `run_id` are what make the stream
/// readable. A twin that delegated to the unobserved method would still fan out,
/// still pass every assertion about the workspace, and quietly report nothing.
#[tokio::test]
async fn the_observed_contained_bounded_turn_reports_its_children() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let contract = TaskContract::workspace("decompose it", dir.path()).with_max_steps(3);
    let log = Log::default();
    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("write a.txt saying A", "a.txt", "A")]),
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Text("done"),
    ]);

    let turn = session
        .turn_contained_bounded_observed(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
            &log,
        )
        .await
        .unwrap();

    let spawned = log.kinds("spawned");
    assert_eq!(
        spawned.len(),
        1,
        "the bound observer heard nothing about the fan-out: {spawned:?}"
    );
    assert_eq!(
        store.children(turn.run_id).unwrap().len(),
        1,
        "one child, under this turn's run"
    );
    assert_eq!(
        session.history(&store).unwrap().len(),
        1,
        "a fan-out is still one turn in the conversation"
    );
}

/// A tool that counts the times it was actually invoked.
///
/// The counter is the assertion: "the fan-out could call the caller's own tool" is
/// a measurement here, not an argument about what the model was offered.
struct Counted(std::sync::Arc<AtomicUsize>);

impl Tool for Counted {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "count_me".into(),
            description: "Record that this was called.".into(),
            parameters: json!({ "type": "object" }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move { Ok("counted".to_string()) })
    }
}

/// **F4a** — a verification gate on a contained turn decides the outcome.
///
/// Both directions, because a gate that never passes and a gate that never runs are
/// indistinguishable from one arm. The failing arm ends at the step cap; the passing
/// arm ends at `Success`, which on this loop is reached only through `evaluate_gate`.
#[tokio::test]
async fn a_contained_turns_verification_gate_decides_its_outcome() {
    for (content, expect_success) in [("wrong", false), ("A", true)] {
        let dir = ws();
        let store = Store::memory().unwrap();
        let mut session = Session::open(&store, dir.path()).unwrap();
        let contract = TaskContract::workspace("write a.txt", dir.path())
            .with_verification(Verification::WorkspaceFileContains {
                file: "a.txt".into(),
                needle: "A".into(),
            })
            .with_max_steps(2);
        let mock = Mock::new(vec![Say::Calls(vec![write("a.txt", content)])]);

        let turn = session
            .turn_contained_bounded(
                &contract,
                &mock,
                &store,
                &Policy::permissive(),
                &ApproveAll,
                &roomy(),
            )
            .await
            .unwrap();

        assert_eq!(
            matches!(turn.outcome, RunOutcome::Success { .. }),
            expect_success,
            "a gate the contained turn carried did not decide it: {:?} for {content:?}",
            turn.outcome
        );
        // The other half of the 0.37.0 rule, on this loop: a judged turn is work.
        assert_eq!(turn.kind, TurnKind::Run, "a judged contained turn is work");
    }
}

/// **F4b** — a tool the caller registered is reachable inside the fan-out.
///
/// Asserted on the tool's own counter and on `Store::observations`, never on the
/// model's reply: a scripted provider will happily say it called something it did
/// not, and the reply is the one piece of evidence that proves nothing.
#[tokio::test]
async fn a_contained_turn_can_call_the_contracts_own_tool() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let contract = TaskContract::workspace("use the tool", dir.path())
        .with_tools(Toolbox::new().with(Counted(calls.clone())))
        .with_max_steps(3);
    let mock = Mock::new(vec![
        Say::Calls(vec![call("count_me", json!({}))]),
        Say::Text("done"),
    ]);

    let turn = session
        .turn_contained_bounded(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert!(
        mock.tools(0).contains(&"count_me".to_string()),
        "the caller's tool was not offered to the root of the fan-out: {:?}",
        mock.tools(0)
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "the caller's tool was offered and then not actually invoked"
    );
    assert!(
        store
            .observations(turn.run_id)
            .unwrap()
            .iter()
            .any(|o| o.text.contains("counted")),
        "the tool's result never reached the run's observations"
    );
}

/// **F4c** — the contract's step cap stops the contained turn.
///
/// The provider never stops calling tools, so the only thing that can end this run
/// is the cap the caller set.
#[tokio::test]
async fn a_contained_turns_step_cap_is_the_contracts() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let contract = TaskContract::workspace("keep writing", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(2);
    let mock = Mock::new(vec![
        Say::Calls(vec![write("a.txt", "1")]),
        Say::Calls(vec![write("a.txt", "2")]),
        Say::Calls(vec![write("a.txt", "3")]),
        Say::Calls(vec![write("a.txt", "4")]),
    ]);

    let turn = session
        .turn_contained_bounded(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
        )
        .await
        .unwrap();

    assert!(
        matches!(turn.outcome, RunOutcome::StepCapReached { steps: 2 }),
        "the contract's cap did not bound the turn: {:?}",
        turn.outcome
    );
    assert_eq!(
        mock.calls(),
        2,
        "the model was asked for more completions than the cap allows"
    );
}

// ---------------------------------------------------- a steered fan-out (0.67.0)

/// What the operator says mid-turn. Distinctive, so its presence or absence in a
/// prompt is not a coincidence.
const OPERATOR: &str = "only the public modules";

/// The fan-out both 0.67.0 criteria are asserted against: the root spawns one
/// child, the child does its work, the root then stops on text.
///
/// The script order is what tells the root's completions from the child's — the
/// tree awaits a child inside the step that spawned it, so completion 0 is the
/// root's first step, completion 1 is the child's, and completion 2 is the root's
/// second. Every other test in this file relies on the same ordering.
fn one_child() -> Mock {
    Mock::new(vec![
        Say::Calls(vec![spawn("write a.txt saying A", "a.txt", "A")]),
        Say::Calls(vec![write("a.txt", "A")]),
        Say::Text("done"),
    ])
}

/// Says the operator's correction the moment a child is spawned — which is
/// emitted before the child's own events start arriving, so the message is
/// **pending in the inbox while the child runs**.
///
/// That timing is the whole point. A message queued before the turn would be
/// drained by the root's first boundary and gone before any child existed, and a
/// child that then failed to see it would prove nothing about whether children can
/// be steered.
struct SayOnSpawn(io_harness::Steer, AtomicUsize);

impl Observer for SayOnSpawn {
    fn event(&self, event: &RunEvent) -> Flow {
        if matches!(event.kind, EventKind::Spawned { .. })
            && self.1.fetch_add(1, Ordering::SeqCst) == 0
        {
            // Ignored on a closed channel: an observer must not panic.
            let _ = self.0.say(OPERATOR);
        }
        Flow::Continue
    }
}

/// **F2 (0.67.0)** — a contained turn is steerable at its root, and the tree loop's
/// drain executes.
///
/// `src/run/tree.rs` has called `drain_steer` at its own step boundary since the
/// loop was written, and until this release no contained entry point could pass an
/// inbox — so the call had never executed from a real caller. This is its first
/// end-to-end execution, and the fan-out still has to happen underneath it: a
/// steerable turn that stopped decomposing would be a different feature.
#[tokio::test]
async fn a_contained_bounded_turn_is_steerable_at_its_root() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let contract = TaskContract::workspace("decompose it", dir.path()).with_max_steps(3);
    let mock = one_child();
    let (steer, inbox) = io_harness::Steer::channel();

    let turn = session
        .turn_contained_bounded_steered(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
            &SayOnSpawn(steer, AtomicUsize::new(0)),
            &inbox,
        )
        .await
        .unwrap();

    // Completion 2 is the root's second step — its first boundary after the
    // message was sent. Completion 0 is the root's first step, before it existed.
    assert!(
        !mock.user(0).contains(OPERATOR),
        "the message was in the root's context before it was sent, so this says \
         nothing about a boundary"
    );
    assert!(
        mock.user(2).contains(OPERATOR),
        "the tree loop's drain never put the operator's message in the root's context: {}",
        mock.user(2)
    );
    // The same claim from the store rather than from the provider's recollection.
    let root_steps = store.steps(turn.run_id).unwrap();
    assert!(
        root_steps.iter().any(|s| s.prompt.contains(OPERATOR)),
        "the message is not in the root run's own trace"
    );

    // ...and the fan-out still happened underneath it.
    let children = store.children(turn.run_id).unwrap();
    assert_eq!(children.len(), 1, "one child, under this turn's run");
    assert!(
        !store.steps(children[0]).unwrap().is_empty(),
        "the child took no step of its own"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "A",
        "the child's work never reached the workspace"
    );
}

/// **F3 (0.67.0)** — a child is not steerable, and the root of the same turn is.
///
/// `Tree::extras` hands a child the empty set, so a sub-agent is never steerable by
/// an operator it has not spoken to. That is a deliberate boundary rather than an
/// oversight, so it is asserted rather than assumed — and asserted with the
/// positive control in the same test, because an absence that would also hold if
/// the fixture never ran is not evidence of anything.
#[tokio::test]
async fn the_operators_message_reaches_the_root_and_not_the_child() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    let contract = TaskContract::workspace("decompose it", dir.path()).with_max_steps(3);
    let mock = one_child();
    let (steer, inbox) = io_harness::Steer::channel();

    let turn = session
        .turn_contained_bounded_steered(
            &contract,
            &mock,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &roomy(),
            &SayOnSpawn(steer, AtomicUsize::new(0)),
            &inbox,
        )
        .await
        .unwrap();

    // The control: the root did read it, at its next boundary. Without this line
    // the assertion below would pass on a fixture that never delivered the message
    // to anyone.
    assert!(
        mock.user(2).contains(OPERATOR),
        "the root never read the message, so nothing here is evidence about the child"
    );
    // The child's own completion, which is the second in script order — and it ran
    // while the message was sitting unread in the inbox.
    assert!(
        !mock.user(1).contains(OPERATOR),
        "the operator's message reached a sub-agent they never spoke to: {}",
        mock.user(1)
    );

    // And from the store: nothing in the child's trace carries it, and the child
    // completed its work regardless.
    let children = store.children(turn.run_id).unwrap();
    assert_eq!(children.len(), 1);
    let child_steps = store.steps(children[0]).unwrap();
    assert!(!child_steps.is_empty(), "the child took no step of its own");
    assert!(
        !child_steps.iter().any(|s| s.prompt.contains(OPERATOR)),
        "the child's trace carries the operator's message"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "A",
        "the child did not complete its work"
    );
}

// ------------------------------------------- a requested fold, in a tree (0.68.0)
//
// `TaskContract::fold_now` is one contract laid over a whole tree, and a tree is
// the one shape where "the caller asked for a fold" and "this agent's own history
// needs one" are different sentences about different ledgers. `fold_forced` is
// where they are kept apart, and this is where that separation is measured.

/// A tree ceiling low enough that an agent's context budget lands on the
/// 2,000-token floor, and therefore its fold threshold on 1,600.
///
/// This is the only lever that reaches a **child's** threshold. `spawn_child`
/// builds every child a fresh `TaskContract::workspace(...)`, so a child inherits
/// none of the turn contract's `context`, `compaction` or `fold_now` — what it
/// does inherit is the tree's remaining tokens, through `effective_token_budget`.
/// Roomy enough that the ~1,000 tokens both arms of the test below actually spend
/// never reach the ceiling: this shrinks the prompt budget, it does not halt the
/// fan-out the way `the_shared_ledger_halts_a_turns_fan_out_on_spend` does.
fn tight() -> Containment {
    Containment::new(10, 4, 3, 4_000)
}

fn read(path: &str) -> ToolCall {
    call("read_file", json!({ "path": path }))
}

/// Nine files of 1,800 characters each, and the nine reads that pull them in.
///
/// Nine because a child keeps `Compaction::default().keep_recent` — eight —
/// observations whole, so a ninth is the first entry a fold has anything to fold.
///
/// 1,800 characters because the child's per-read ceiling is 2,000 — the entry cap
/// derived from what the tree's token ceiling leaves it — and **a file over that
/// ceiling is refused outright, not truncated**. Two earlier versions of this
/// fixture were written the other way round, at 3,000 and then 20,000 characters,
/// on the assumption that an oversized read arrives clipped to the cap. Both
/// measured thirteen durable observations totalling about 4,000 characters and no
/// fold at all: every read had come back as a one-line refusal saying the file was
/// over the ceiling, so the ledger the control needed to grow was made of error
/// notes. Just under the ceiling, each entry arrives whole and nine of them are
/// several times the 1,600-token threshold, which is a margin rather than a
/// coincidence.
///
/// Distinct paths so the stall detector sees nine different signatures and never
/// mistakes the fixture for an agent going in circles.
fn readable(dir: &std::path::Path) -> Vec<ToolCall> {
    (0..9)
        .map(|i| {
            let name = format!("note{i}.txt");
            std::fs::write(dir.join(&name), i.to_string().repeat(1_800)).unwrap();
            read(&name)
        })
        .collect()
}

/// Four conversational turns, so the next turn's seed is eight observations deep
/// — more than the `keep_recent` below, which is what gives a root fold anything
/// to fold. Conversational rather than working turns because the thing a root
/// folds *is* the conversation, and this is how a conversation gets into a
/// session.
async fn converse(session: &mut Session, store: &Store, policy: &Policy) {
    for i in 0..4 {
        let talker = Mock::new(vec![Say::Text("noted")]);
        session
            .turn(
                &format!("what happened at stage {i}?"),
                &talker,
                store,
                policy,
                &ApproveAll,
            )
            .await
            .unwrap();
    }
}

/// The turn contract both arms are driven with, differing only in `fold_now`.
///
/// Verification is set and unreachable, so the turn is work rather than a reply:
/// a classifying turn answers before the loop and never reaches a fold at all.
/// `keep_recent: 2` against an eight-entry seed, so the root has six observations
/// a fold can replace.
fn folding_contract(root: &std::path::Path, fold_now: bool) -> TaskContract {
    TaskContract::workspace("decompose it", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(4)
        .with_context_budget(ContextBudget::default())
        .with_compaction(Compaction {
            at_share: 0.8,
            keep_recent: 2,
        })
        .with_fold_now(fold_now)
}

/// **F5 (0.68.0)** — a spawned child does not fold on the root's request.
///
/// Two arms in one test, and the second is what makes the first mean anything.
/// Arm A asks a fan-out to fold and finds `Compacted` at depth 0 and nowhere
/// below it. On its own that assertion is also satisfied by a tree in which a
/// child could not have folded under any circumstances — which is 0.67.0's F3
/// lesson stated again: an absence is evidence only when the thing absent was
/// reachable at the moment it did not happen. Arm B is that reachability,
/// measured rather than argued: the same fixture with the request off and a child
/// given enough reads to cross its **own** threshold, which does emit `Compacted`
/// at depth 1. Delete the `depth == 0` term from `fold_forced` and arm A fails;
/// break child folding outright and arm B fails; write the test with arm A alone
/// and neither failure is distinguishable from a fixture that never ran.
///
/// The lever arm B pulls is the *tree's* token ceiling and not the turn
/// contract's `ContextBudget`, which is worth stating because it is not the
/// obvious knob. `spawn_child` builds each child a fresh `TaskContract`, so a
/// child inherits neither `compaction` nor `context` nor `fold_now` from the
/// contract the caller wrote; what reaches it is `Containment`'s remaining
/// tokens. Shrinking the tree ceiling is therefore the only way a caller can put
/// a child's fold threshold within reach at all — see [`tight`].
///
/// The same fact is why arm A's absence is, today, over-determined: at depth 1
/// `contract.fold_now` is already `false` before `fold_forced` is consulted, so
/// the depth gate is the second of two locks on one door rather than the only
/// one. It is still the lock that decides the question the day a child is handed
/// its parent's contract — inheriting a step cap or a compaction setting is a
/// plausible next release, and `fold_now` is the field that must not ride along —
/// and this test is what would fail on that day.
#[tokio::test]
async fn a_spawned_child_does_not_fold_on_the_roots_request() {
    // ---------------- arm A: the request is honoured, and stops at the root.
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive();
    let mut session = Session::open(&store, dir.path()).unwrap();
    converse(&mut session, &store, &policy).await;

    let log = Log::default();
    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("write a.txt saying A", "a.txt", "A")]),
        Say::Calls(vec![write("a.txt", "A")]),
    ]);
    let turn = session
        .turn_contained_bounded_observed(
            &folding_contract(dir.path(), true),
            &mock,
            &store,
            &policy,
            &ApproveAll,
            &tight(),
            &log,
        )
        .await
        .unwrap();

    // First, that there was a child at all and that it worked. Without these three
    // lines every assertion below is about a tree that never fanned out, and "no
    // child folded" would be true of a turn with no children.
    let children = store.children(turn.run_id).unwrap();
    assert_eq!(
        children.len(),
        1,
        "no child ran, so nothing here is about one"
    );
    assert!(
        !store.steps(children[0]).unwrap().is_empty(),
        "the child took no step of its own, so it had no ledger to fold"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "A",
        "the child's work never reached the workspace"
    );

    let folds = log.kinds("compacted");
    let root_folds: Vec<_> = folds.iter().filter(|(_, depth)| *depth == 0).collect();
    assert!(
        !root_folds.is_empty(),
        "the root did not honour `fold_now`, so the request never reached the tree loop: {folds:?}"
    );
    assert!(
        root_folds.iter().all(|(run, _)| *run == turn.run_id),
        "a depth-0 fold that is not this turn's own run: {folds:?}"
    );
    // The claim.
    let child_folds: Vec<_> = folds.iter().filter(|(_, depth)| *depth > 0).collect();
    assert!(
        child_folds.is_empty(),
        "a spawned child folded a ledger on a request that was never made of it, \
         and folded away work the operator never saw: {child_folds:?}"
    );

    // ---------------- arm B: the positive control — a child that folds its own.
    //
    // Same contract, same containment, same conversation. `fold_now` is off and
    // the child is given nine reads instead of one write, so the only thing that
    // can fold anything here is a ledger crossing its own threshold — and the
    // ledger that crosses it is the child's.
    let dir = ws();
    let store = Store::memory().unwrap();
    let mut session = Session::open(&store, dir.path()).unwrap();
    converse(&mut session, &store, &policy).await;

    let reads = readable(dir.path());
    let log = Log::default();
    let mock = Mock::new(vec![
        Say::Calls(vec![spawn("read every note", "unreachable.txt", "never")]),
        Say::Calls(reads[0..3].to_vec()),
        Say::Calls(reads[3..6].to_vec()),
        Say::Calls(reads[6..9].to_vec()),
    ]);
    let turn = session
        .turn_contained_bounded_observed(
            &folding_contract(dir.path(), false),
            &mock,
            &store,
            &policy,
            &ApproveAll,
            &tight(),
            &log,
        )
        .await
        .unwrap();

    let children = store.children(turn.run_id).unwrap();
    assert_eq!(children.len(), 1, "no child ran, so this controls nothing");
    let folds = log.kinds("compacted");
    let child_folds: Vec<_> = folds.iter().filter(|(_, depth)| *depth > 0).collect();
    // The child's own numbers, in the message rather than in a comment: when this
    // control fails, the question is always which of the three preconditions was
    // missed — enough durable entries to exceed `keep_recent`, enough estimated
    // tokens to cross the threshold, or enough steps for a fold to be attempted
    // after the entries existed. A count answers the first two immediately.
    let obs = store.observations(children[0]).unwrap();
    let child_obs = obs.len();
    let chars: usize = obs.iter().map(|o| o.text.chars().count()).sum();
    let child_steps = store.steps(children[0]).unwrap().len();
    assert!(
        !child_folds.is_empty(),
        "a child never folded: {child_obs} durable observations over {child_steps} steps \
         totalling {chars} characters (~{} estimated tokens), against `keep_recent` 8 — \
         so arm A's absence is an artefact of a fixture in which no child could ever fold, \
         and proves nothing about the depth gate: {folds:?}\nfirst entry: {:?}",
        chars / 4,
        obs.first()
            .map(|o| o.text.chars().take(160).collect::<String>())
            .unwrap_or_default()
    );
    assert!(
        child_folds.iter().all(|(run, _)| children.contains(run)),
        "a fold below depth 0 that belongs to no child of this turn: {folds:?}"
    );
}
