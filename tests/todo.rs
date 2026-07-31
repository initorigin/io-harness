//! The agent's plan, held where an operator can watch it (0.21.0).
//!
//! The failure this file exists to prevent is recorded, not imagined. The 0.20.0
//! live session
//! (`.ultraship/products/io-harness/evidence/0.20.0/live-run-session.txt`) shows a
//! six-step turn in which the model looped over `find` and `read_file` until the
//! stall detector stopped it. Watching that run, an operator could see every step
//! the agent *took* and nothing about what it *intended*, so there was no moment
//! before the end at which the run could be recognised as going the wrong way.
//!
//! A plan fixes that only if it is durable and visible **while the run is still
//! going**. So the central test here does not read the plan afterwards: it opens a
//! second connection to the same database from inside an `Observer`, mid-run, and
//! reads the plan the way a UI in another process would. Reading it after the fact
//! would pass even if the write happened at the very end, which is exactly the bug
//! worth catching.
//!
//! The second half matters as much. A plan is the agent's stated intent and nothing
//! more: the harness never enforces it, no outcome depends on it, and writing one is
//! not an act the policy gates. `a_plan_is_inert` pins all three, because a plan
//! that quietly acquired teeth would be a permission system nobody declared.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, run_with_observed, ApproveAll, Policy, Provider, RunOutcome, Store, TaskContract,
    TodoState, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays a fixed script of tool calls, one turn at a time.
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

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// A contract that can never be satisfied, so the loop runs its whole step budget
/// and the plan's state at the end is the agent's, not the verifier's.
fn never_passes(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace("exercise the todo tool", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

/// One `todo_write` argument list, as the model would send it.
fn plan(items: &[(&str, &str)]) -> serde_json::Value {
    json!({
        "items": items
            .iter()
            .map(|(text, state)| json!({ "text": text, "state": state }))
            .collect::<Vec<_>>()
    })
}

// ------------------------------------------- F1: durable and readable mid-run

/// F1, the part that only a second connection can prove: the plan is in the store
/// **before the run that wrote it has finished**.
///
/// The observer fires on the `Step` event for step 1, opens its own `Store` over the
/// same file — which is what a UI in another process has — and reads the plan back.
/// A write that landed at the end of the run instead would leave this empty.
#[tokio::test]
async fn a_plan_is_readable_from_a_second_connection_while_the_run_is_still_going() {
    let dir = ws();
    let db = dir.path().join("trace.db");
    let store = Store::open(&db).unwrap();

    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call(
            "todo_write",
            plan(&[
                ("read the config", "active"),
                ("write the patch", "pending"),
            ]),
        )],
        vec![call("read_file", json!({ "path": "nothing.txt" }))],
        vec![],
    ]);

    /// Reads the plan from its own connection the moment step 1 commits.
    struct MidRun {
        db: std::path::PathBuf,
        seen: Mutex<Vec<(String, TodoState)>>,
        run_id: Mutex<Option<i64>>,
    }
    impl Observer for MidRun {
        fn event(&self, event: &RunEvent) -> Flow {
            if let EventKind::Step { .. } = &event.kind {
                if event.step == 1 {
                    *self.run_id.lock().unwrap() = Some(event.run_id);
                    let other = Store::open(&self.db).expect("a second connection opens");
                    let items = other.todos(event.run_id).expect("the plan reads back");
                    *self.seen.lock().unwrap() = items
                        .into_iter()
                        .map(|i| (i.text.to_string(), i.state))
                        .collect();
                }
            }
            Flow::Continue
        }
    }

    let watcher = Arc::new(MidRun {
        db: db.clone(),
        seen: Mutex::new(Vec::new()),
        run_id: Mutex::new(None),
    });

    let result = run_with_observed(
        &contract,
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
        watcher.as_ref(),
    )
    .await
    .unwrap();

    let seen = watcher.seen.lock().unwrap().clone();
    assert_eq!(
        seen,
        vec![
            ("read the config".to_string(), TodoState::Active),
            ("write the patch".to_string(), TodoState::Pending),
        ],
        "the plan must be readable from another connection at step 1, in order, \
         with each item's state — it was {seen:?}"
    );
    // And the run went on afterwards, so this was genuinely mid-run.
    let steps = store.steps(result.run_id).unwrap().len();
    assert!(
        steps > 1,
        "the run should have continued past the step that wrote the plan; it took {steps}"
    );
}

/// F1, the wholesale-replace half: a second write is not a merge. There are no item
/// ids for a model to get wrong precisely because the list is replaced, and that
/// promise is only true if the old rows go.
#[tokio::test]
async fn a_second_write_replaces_the_whole_list_rather_than_merging_it() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call(
            "todo_write",
            plan(&[
                ("first", "done"),
                ("second", "active"),
                ("third", "pending"),
            ]),
        )],
        vec![call("todo_write", plan(&[("only this one", "active")]))],
        vec![],
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let items = store.todos(result.run_id).unwrap();
    assert_eq!(
        items.len(),
        1,
        "the second write replaces the list; found {items:?}"
    );
    assert_eq!(items[0].text, "only this one");
    assert_eq!(items[0].state, TodoState::Active);
}

/// F1, the outlives-its-process half. The same shape 0.20.0's session test uses: the
/// `Store` that wrote the plan is dropped, and a fresh one over the same file reads
/// it. A plan held in memory would vanish here.
#[tokio::test]
async fn a_plan_outlives_the_store_that_wrote_it() {
    let dir = ws();
    let db = dir.path().join("trace.db");
    let contract = never_passes(dir.path(), 2);
    let provider = MockScript::new(vec![
        vec![call(
            "todo_write",
            plan(&[("survive a restart", "pending")]),
        )],
        vec![],
    ]);

    let run_id = {
        let store = Store::open(&db).unwrap();
        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();
        result.run_id
    };

    let reopened = Store::open(&db).unwrap();
    let items = reopened.todos(run_id).unwrap();
    assert_eq!(
        items.len(),
        1,
        "the plan should have survived; got {items:?}"
    );
    assert_eq!(items[0].text, "survive a restart");
    assert_eq!(items[0].state, TodoState::Pending);
}

/// F1's ordering promise, stated separately because it is the one an operator reads
/// most: the items come back in the order the agent wrote them, not in id order,
/// alphabetical order, or state order.
#[tokio::test]
async fn a_plan_reads_back_in_the_order_the_agent_wrote_it() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 2);
    let provider = MockScript::new(vec![
        vec![call(
            "todo_write",
            plan(&[("zebra", "done"), ("apple", "pending"), ("mango", "active")]),
        )],
        vec![],
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    let texts: Vec<_> = store
        .todos(result.run_id)
        .unwrap()
        .into_iter()
        .map(|i| i.text.to_string())
        .collect();
    assert_eq!(texts, vec!["zebra", "apple", "mango"]);
}

// ------------------------------------------------------------ F2: a plan is inert

/// F2 — all three halves of inertness in one run, because they are one claim.
///
/// The agent writes a plan it never finishes, and:
///
/// 1. the outcome is the one the verification and the step budget dictate
///    (`StepCapReached`), not one derived from an unfinished plan;
/// 2. `todo_write` produced no `policy_events` row, because it is not gated;
/// 3. the negative control — a real workspace write in the same run — *did* produce
///    one, so the absence above is the todo tool being ungated rather than the
///    policy machinery being asleep.
#[tokio::test]
async fn a_plan_is_inert() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call(
            "todo_write",
            plan(&[("never finished", "active"), ("also never", "pending")]),
        )],
        // The negative control, in the same run under the same policy.
        vec![call(
            "write_file",
            json!({ "path": "out.txt", "content": "written" }),
        )],
        vec![],
    ]);

    // A policy that gates writes by asking, so a gated act is guaranteed to leave a
    // row. `ApproveAll` answers, so the write still happens.
    let policy = Policy::default()
        .layer("test")
        .allow_read("*")
        .ask_write("*")
        .allow_exec("*");

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    // 1 — the outcome is the contract's, not the plan's.
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "an unfinished plan must not change the outcome; got {:?}",
        result.outcome
    );
    let items = store.todos(result.run_id).unwrap();
    assert_eq!(items.len(), 2, "the unfinished plan is still recorded");
    assert!(
        items.iter().any(|i| i.state != TodoState::Done),
        "the plan was deliberately left unfinished"
    );

    // 2 and 3 — the policy saw the write and never saw the plan.
    let targets: Vec<String> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .map(|e| e.target)
        .collect();
    assert!(
        targets.iter().any(|t| t.contains("out.txt")),
        "the control write should have been gated; policy rows were {targets:?}"
    );
    assert!(
        !targets.iter().any(|t| t.contains("never finished")),
        "a todo write is not an act and must leave no policy row; rows were {targets:?}"
    );
    assert!(
        !targets.iter().any(|t| t == "todo_write"),
        "a todo write must not be gated as a tool name either; rows were {targets:?}"
    );
}

/// F2's other edge: an empty list is a legal plan, and it clears the plan rather than
/// being rejected. An agent that finishes its work and clears its list must not get
/// an error for it.
#[tokio::test]
async fn an_empty_plan_clears_the_list_and_is_not_an_error() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        vec![call("todo_write", plan(&[("something", "active")]))],
        vec![call("todo_write", json!({ "items": [] }))],
        vec![],
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        store.todos(result.run_id).unwrap().is_empty(),
        "an empty write clears the plan"
    );
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "clearing a plan is not a failure; got {:?}",
        result.outcome
    );
}

/// An unparseable plan is an observation the model can correct, not an error that
/// ends the run — the rule every other tool in the crate follows.
#[tokio::test]
async fn a_malformed_plan_is_an_observation_rather_than_the_end_of_the_run() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3);
    let provider = MockScript::new(vec![
        // `items` is not a list at all.
        vec![call("todo_write", json!({ "items": "read the config" }))],
        vec![call("todo_write", plan(&[("recovered", "active")]))],
        vec![],
    ]);

    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "a malformed plan must not end the run; got {:?}",
        result.outcome
    );
    let items = store.todos(result.run_id).unwrap();
    assert_eq!(
        items.len(),
        1,
        "the corrected plan is what stands; got {items:?}"
    );
    assert_eq!(items[0].text, "recovered");
}
