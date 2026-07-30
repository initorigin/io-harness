//! Sessions: a durable, branchable conversation (0.20.0).
//!
//! The claim this file has to make true is that a session is durable *for the
//! reason a run is*, not for a reason of its own — so the assertions are about
//! the run tables as much as about the session ones: a turn has a run id, a
//! refusal inside a turn is in `policy_events` attributed to its rule and layer,
//! and a conversation reopened in a second `Store` says what the first one said.
//!
//! Everything is driven through the real loop with a scripted provider that
//! records the requests it was handed, because "the second turn read the first" is
//! only observable as a fact about the prompt the provider actually received.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    ApproveAll, Policy, Provider, RunOutcome, Session, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// What the provider does on one call.
#[derive(Clone)]
enum Say {
    /// Answer with text and no tool call, which ends a `Verification::None` turn.
    Text(&'static str),
    /// Make one tool call, so the turn takes a step and carries on.
    Call(ToolCall),
}

/// Plays a script and keeps every request it was handed, so a test can assert
/// what the model was actually asked.
struct Mock {
    script: Vec<Say>,
    at: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl Mock {
    fn new(script: Vec<Say>) -> Self {
        Self {
            script,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// The prompt of the nth completion this provider served.
    fn prompt(&self, n: usize) -> String {
        self.seen.lock().unwrap()[n].clone()
    }

    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for Mock {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req.user.clone());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        // Past the end of the script, the model stops talking — which ends an
        // unbounded turn rather than hanging the test on a step cap.
        let say = self.script.get(i).cloned().unwrap_or(Say::Text("done"));
        Ok(CompletionResponse {
            text: match &say {
                Say::Text(t) => Some((*t).to_string()),
                Say::Call(_) => None,
            },
            tool_calls: match &say {
                Say::Call(c) => vec![c.clone()],
                Say::Text(_) => Vec::new(),
            },
            usage: Some(Usage {
                total_tokens: 7,
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.md"), "# notes\n").unwrap();
    dir
}

/// A boundary that allows the workspace and refuses one directory outright, so a
/// turn's refusal is attributable to a named rule in a named layer.
fn policy() -> Policy {
    Policy::default()
        .layer("session-test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("cargo*")
        .deny_write("secrets/*")
}

// ---------------------------------------------------------------------- F1

/// F1 — a session outlives its process.
///
/// The second `Store` is the point: it is a different connection over the same
/// file, which is as close to "a later process" as a test gets without spawning
/// one. The negative control is a *second* session over the same root, which must
/// see none of the first's conversation — otherwise the test would pass on a
/// workspace-keyed memory rather than on the session tree.
#[tokio::test]
async fn a_session_outlives_the_process_that_opened_it() {
    let ws = workspace();
    let db = ws.path().join("runs.db");
    let provider = Mock::new(vec![
        Say::Text("the retry policy retries a 503"),
        Say::Text("and a 429"),
        Say::Text("yes, both"),
        Say::Text("nothing to go on"),
    ]);

    let id = {
        let store = Store::open(&db).unwrap();
        let mut session = Session::open(&store, ws.path()).unwrap();
        session
            .turn(
                "what does the retry policy retry?",
                &provider,
                &store,
                &policy(),
                &ApproveAll,
            )
            .await
            .unwrap();
        session
            .turn("what else?", &provider, &store, &policy(), &ApproveAll)
            .await
            .unwrap();
        session.id()
    };

    // A fresh connection, a fresh `Session`, nothing carried in memory but the id.
    let store = Store::open(&db).unwrap();
    let mut session = Session::reopen(&store, id).unwrap();
    assert_eq!(session.root(), ws.path());
    assert_eq!(session.history(&store).unwrap().len(), 2);

    session
        .turn(
            "did you cover both?",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let third = provider.prompt(2);
    assert!(
        third.contains("what does the retry policy retry?")
            && third.contains("the retry policy retries a 503"),
        "the third turn did not carry the first turn across the process boundary:\n{third}"
    );

    // The negative control: a different session over the same workspace starts
    // with nothing, so what the third turn read came from the tree and not from
    // anything keyed to the directory.
    let mut other = Session::open(&store, ws.path()).unwrap();
    other
        .turn("who are you?", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    let fresh = provider.prompt(3);
    assert!(
        !fresh.contains("the retry policy retries a 503"),
        "a new session inherited another session's conversation:\n{fresh}"
    );
}

// ---------------------------------------------------------------------- F2

/// F2 — a turn is a run, with everything a run has.
#[tokio::test]
async fn a_turn_is_a_run_and_carries_a_runs_guarantees() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();
    let provider = Mock::new(vec![
        // A write the policy refuses outright, then the model gives up and speaks.
        Say::Call(ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": "secrets/key.txt", "content": "stolen" }),
        }),
        Say::Text("I cannot write there"),
    ]);

    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn(
            "put the key in secrets/key.txt",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // The turn row points at a real run, and the run is a finished run.
    let recorded = store.session_turn(turn.turn_id).unwrap().unwrap();
    assert_eq!(recorded.run_id, turn.run_id);
    assert_eq!(recorded.session_id, session.id());
    assert!(store.run_status(turn.run_id).unwrap().is_some());
    let summary = store
        .run_summary(turn.run_id)
        .unwrap()
        .expect("a finished turn has a summary");
    assert_eq!(summary.run_id, turn.run_id);

    // The refusal is in the run's own trace, attributed exactly as a one-shot
    // run's refusal is — same table, same rule, same layer.
    let refusals: Vec<_> = store
        .events(turn.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        refusals
            .iter()
            .any(|e| e.layer.as_deref() == Some("session-test")
                && e.rule.as_deref() == Some("secrets/*")),
        "a turn's refusal is not attributed to the rule and layer that made it: {refusals:?}"
    );

    // The secret was not written, which is the only assertion that matters about a
    // boundary.
    assert!(!ws.path().join("secrets/key.txt").exists());

    // And the reply the session reports is what the agent actually said last.
    assert_eq!(turn.reply.as_deref(), Some("I cannot write there"));
}

// ---------------------------------------------------------------------- F3

/// F3 — the tree branches and nothing is lost.
#[tokio::test]
async fn a_branch_forgets_the_turns_after_it_and_deletes_none_of_them() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();
    let provider = Mock::new(vec![
        Say::Text("plan A: blue-green"),
        Say::Text("plan B: read-only window"),
        Say::Text("plan C: dual write"),
    ]);
    let mut session = Session::open(&store, ws.path()).unwrap();

    let first = session
        .turn(
            "draft a migration plan",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    let second = session
        .turn(
            "use a blue-green cutover",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // Back to the plan, and take the other road.
    session.branch_from(&store, first.turn_id).unwrap();
    assert_eq!(session.head(), Some(first.turn_id));
    let third = session
        .turn(
            "use a read-only window instead",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    let branched = provider.prompt(2);
    assert!(
        branched.contains("draft a migration plan"),
        "the branch lost the turn it branched from:\n{branched}"
    );
    assert!(
        !branched.contains("use a blue-green cutover"),
        "the branch still carried the turn it was taken to avoid:\n{branched}"
    );

    // Nothing was rewritten: the abandoned turn is still there, unchanged, with
    // its own reply and its own run.
    let abandoned = store.session_turn(second.turn_id).unwrap().unwrap();
    assert_eq!(abandoned.prompt, "use a blue-green cutover");
    assert_eq!(abandoned.reply.as_deref(), Some("plan B: read-only window"));
    assert_eq!(store.session_turns(session.id()).unwrap().len(), 3);

    // Each head has its own path, and both paths start at the same root turn.
    let path = session.history(&store).unwrap();
    assert_eq!(
        path.iter().map(|t| t.id).collect::<Vec<_>>(),
        vec![first.turn_id, third.turn_id]
    );
    session.branch_from(&store, second.turn_id).unwrap();
    assert_eq!(
        session
            .history(&store)
            .unwrap()
            .iter()
            .map(|t| t.id)
            .collect::<Vec<_>>(),
        vec![first.turn_id, second.turn_id]
    );

    // A turn from another session cannot be grafted on.
    let other = Session::open(&store, ws.path()).unwrap();
    assert!(session.branch_from(&store, other.id() + 9_999).is_err());
}

// ---------------------------------------------------------------------- F4

/// F4 — a turn may be bounded or unbounded, and a bound is per turn.
#[tokio::test]
async fn a_turn_is_unbounded_unless_the_caller_bounds_that_one_turn() {
    let ws = workspace();
    let store = Store::open(ws.path().join("runs.db")).unwrap();
    let provider = Mock::new(vec![Say::Text("nothing to do"), Say::Text("had a look")]);
    let mut session = Session::open(&store, ws.path()).unwrap();

    // Unbounded: no criterion, so an assistant turn that calls no tool ends it.
    let chat = session
        .turn(
            "what is in this repo?",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();
    assert!(
        matches!(chat.outcome, RunOutcome::Finished { .. }),
        "an unbounded turn should end as Finished, got {:?}",
        chat.outcome
    );

    // Bounded: the criterion decides, and `cargo --version` is a command that
    // passes on every platform this crate is tested on without a network.
    let contract = TaskContract::workspace(
        "check the toolchain",
        // Deliberately the wrong root: the session's own root must win, because a
        // turn is about the conversation's workspace.
        "/nonexistent",
        Verification::Command {
            argv: vec!["cargo".into(), "--version".into()],
            expect_exit: 0,
        },
    )
    .with_max_steps(3);
    let bounded = session
        .turn_bounded(&contract, &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(bounded.outcome, RunOutcome::Success { .. }),
        "a bounded turn whose criterion passes should report Success, got {:?}",
        bounded.outcome
    );

    // The bound was this turn's, not the session's: the next turn has no gate and
    // ends by talking again.
    let after = session
        .turn("anything else?", &provider, &store, &policy(), &ApproveAll)
        .await
        .unwrap();
    assert!(matches!(after.outcome, RunOutcome::Finished { .. }));
    assert_eq!(provider.calls(), 3);
}

// ---------------------------------------------------------------------- NF1

/// NF1 — a session shares the database with the runs, and adding one changes
/// nothing about them.
///
/// The other half of NF1 — that a database written *without* the session tables
/// gains them on open — is a unit test in `src/state.rs`, where a legacy schema
/// can be created directly. Here the assertion is the pairing: an old run and a
/// new session in one file, and the checkpoint format untouched.
#[tokio::test]
async fn a_session_shares_the_store_with_the_runs_and_changes_none_of_them() {
    let ws = workspace();
    let db = ws.path().join("runs.db");

    let run = {
        let store = Store::open(&db).unwrap();
        let run = store.start_run("an older run", "notes.md").unwrap();
        store.finish_run(run, "success").unwrap();
        run
    };

    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.run_summary(run).unwrap().map(|s| s.outcome),
        Some("success".to_string())
    );

    let provider = Mock::new(vec![Say::Text("read it")]);
    let mut session = Session::open(&store, ws.path()).unwrap();
    let turn = session
        .turn(
            "summarise the notes",
            &provider,
            &store,
            &policy(),
            &ApproveAll,
        )
        .await
        .unwrap();

    // The turn's run is a new row beside the old one; the old one is untouched.
    assert_ne!(turn.run_id, run);
    assert_eq!(
        store.run_summary(run).unwrap().map(|s| s.outcome),
        Some("success".to_string())
    );
    // A conversation is not a reason to bump the checkpoint format.
    assert_eq!(io_harness::CHECKPOINT_FORMAT, 7);
}
