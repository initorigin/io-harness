//! Asking the operator what they meant, which is not asking permission (0.21.0).
//!
//! The crate has had a channel back to a human since 0.4.0, and it can only ask one
//! question: *may I do this?* An `Approver` answers about permission, and its answer
//! can only ever narrow what happens. Nothing could ask *what did you actually want?*
//! — so an ambiguous goal was resolved by the model guessing, and a wrong guess spends
//! a whole run.
//!
//! These tests drive the real loop and assert on both halves of the new channel:
//!
//! - answered in-process by a `Responder`, where the answer changes the next action;
//! - answered by nobody, where the run persists the question, pauses, survives the
//!   process, and continues on a human's answer arriving later.
//!
//! The most important test here is `an_answer_is_not_authorization`. "Just write the
//! file" is the most natural thing anyone will ever type into an answer box, and the
//! boundary must not care. If an answer could influence what is permitted, the crate
//! would have acquired a second authorization path that no policy governs — so that
//! test carries a negative control proving the write does happen when the policy
//! allows it, and therefore that the refusal came from the policy and not from a
//! run that was broken anyway.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    resume_tree_with_answer, resume_with_answer, run_tree, run_with, AnswerFuture, ApproveAll,
    Containment, FixedResponder, Policy, Provider, Question, Responder, RunOutcome, Store,
    TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- scaffolding

/// Plays one of two scripts depending on what the last answer said, so "the answer
/// changed what the agent did next" is a property of the run and not of the script.
struct Branching {
    at: AtomicUsize,
    seen: Arc<Mutex<Vec<CompletionRequest>>>,
}

impl Branching {
    fn new() -> Self {
        Self {
            at: AtomicUsize::new(0),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn prompts(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.user.clone())
            .collect()
    }
}

impl Provider for Branching {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        let user = req.user.clone();
        self.seen.lock().unwrap().push(req);
        // The answer is checked FIRST, and deliberately: on a resume this provider is
        // a fresh instance whose counter starts at zero again, so keying the "ask"
        // branch on the call index alone would ask a second time and pause forever.
        // What decides the branch is the conversation, not how many turns this object
        // has served.
        let calls = if user.contains("[answer]") {
            // The answer arrived. Write whichever file it named — this is the branch
            // that makes the answer observable in the workspace.
            let path = if user.contains("b.txt") {
                "b.txt"
            } else {
                "a.txt"
            };
            vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": path, "content": "written" }),
            }]
        } else if i == 0 {
            // Ask which file was meant.
            vec![ToolCall {
                name: "ask_question".into(),
                arguments: json!({
                    "question": "Which file should I write?",
                    "context": "There is a.txt and b.txt.",
                    "choices": ["a.txt", "b.txt"]
                }),
            }]
        } else {
            vec![]
        };
        Ok(CompletionResponse {
            tool_calls: calls,
            ..Default::default()
        })
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

/// A contract that can never pass, so the run's length is decided by the thing under
/// test rather than by a verification that happens to succeed.
fn never_passes(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("write the file the operator meant", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

// ------------------------------------------- F3: answered here, and it matters

/// F3 — a `Responder` answers, the answer reaches the model as an observation, and the
/// agent acts on it. Run twice with two different answers; the workspace differs.
#[tokio::test]
async fn a_responder_answers_and_the_answer_changes_what_the_agent_does() {
    for (answer, expected, other) in [("a.txt", "a.txt", "b.txt"), ("b.txt", "b.txt", "a.txt")] {
        let dir = ws();
        let store = Store::memory().unwrap();
        let contract =
            never_passes(dir.path(), 3).with_responder(Arc::new(FixedResponder::new(answer)));
        let provider = Branching::new();

        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        assert!(
            dir.path().join(expected).exists(),
            "answering {answer:?} should have produced {expected}"
        );
        assert!(
            !dir.path().join(other).exists(),
            "answering {answer:?} must not have produced {other}"
        );
        // The answer reached the model as text it could read.
        assert!(
            provider.prompts().iter().any(|p| p.contains("[answer]")),
            "the answer must arrive as an observation"
        );
        // And it is recorded, attributed to the machine rather than a person.
        let questions = store.questions(result.run_id).unwrap();
        assert_eq!(questions.len(), 1, "one question asked");
        assert!(questions[0].resolved);
        assert_eq!(questions[0].answer.as_deref(), Some(answer));
        assert_eq!(
            questions[0].answered_by.as_deref(),
            Some("responder"),
            "an in-process answer and a human's must be distinguishable"
        );
        assert_eq!(questions[0].choices, ["a.txt", "b.txt"]);
    }
}

/// A question with an empty `question` is an observation the model can correct, not the
/// end of the run — the rule every other tool follows.
#[tokio::test]
async fn a_malformed_question_is_an_observation_rather_than_the_end_of_the_run() {
    struct AsksNothing(AtomicUsize);
    impl Provider for AsksNothing {
        async fn complete(&self, _r: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let i = self.0.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                tool_calls: if i == 0 {
                    vec![ToolCall {
                        name: "ask_question".into(),
                        arguments: json!({ "question": "   " }),
                    }]
                } else {
                    vec![]
                },
                ..Default::default()
            })
        }
    }

    let dir = ws();
    let store = Store::memory().unwrap();
    let contract = never_passes(dir.path(), 3);
    let result = run_with(
        &contract,
        &AsksNothing(AtomicUsize::new(0)),
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::VerificationFailed { .. }),
        "a malformed question must not end the run; got {:?}",
        result.outcome
    );
    assert!(
        store.questions(result.run_id).unwrap().is_empty(),
        "nothing should have been persisted for a question with no text"
    );
}

// --------------------------- F4: nobody here can answer, so the run waits

/// F4 — with no responder the run persists the question and pauses, and the pause
/// survives the process. Asserted across a dropped `Store`, not within one: reading it
/// back on the same connection would pass even if the question only ever lived in
/// memory.
#[tokio::test]
async fn an_unanswerable_question_pauses_the_run_and_survives_the_process() {
    let dir = ws();
    let db = dir.path().join("trace.db");

    // --- first process: asks, and pauses ---
    let (run_id, question_id) = {
        let store = Store::open(&db).unwrap();
        let contract = never_passes(dir.path(), 4); // no responder at all
        let provider = Branching::new();
        let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();

        let RunOutcome::AwaitingAnswer { question_id, steps } = result.outcome else {
            panic!(
                "a question nobody can answer must pause the run, got {:?}",
                result.outcome
            );
        };
        assert_eq!(steps, 1, "it paused on the step that asked");
        // The run is not left looking like it is still going.
        assert_ne!(
            store.run_status(result.run_id).unwrap(),
            Some(io_harness::RunStatus::Running),
            "a paused run must not be left `Running`; the state a crash leaves is the \
             state a resume already understands"
        );
        (result.run_id, question_id)
    };

    // --- second process: reads the question, answers it, and the run finishes ---
    let store = Store::open(&db).unwrap();
    let q = store
        .question(question_id)
        .unwrap()
        .expect("the question must have survived the process");
    assert_eq!(q.question, "Which file should I write?");
    assert_eq!(q.context.as_deref(), Some("There is a.txt and b.txt."));
    assert_eq!(q.choices, ["a.txt", "b.txt"]);
    assert!(!q.resolved && q.answer.is_none());

    let contract = never_passes(dir.path(), 4);
    let provider = Branching::new();
    let result = resume_with_answer(
        &contract,
        &provider,
        &store,
        run_id,
        question_id,
        "b.txt",
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(result.run_id, run_id, "the same run continued");
    assert!(
        dir.path().join("b.txt").exists(),
        "the agent must have acted on the human's answer"
    );
    let answered = store.question(question_id).unwrap().unwrap();
    assert_eq!(answered.answer.as_deref(), Some("b.txt"));
    assert_eq!(
        answered.answered_by.as_deref(),
        Some("human"),
        "an answer that arrived through a resume is a person's"
    );
}

/// NF6 — a question is resolved exactly once. Answering an answered question is an
/// error rather than a second resume, the way `resolve_pending` already behaves.
#[tokio::test]
async fn answering_the_same_question_twice_is_an_error() {
    let dir = ws();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let contract = never_passes(dir.path(), 4);
    let provider = Branching::new();
    let result = run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
        .await
        .unwrap();
    let RunOutcome::AwaitingAnswer { question_id, .. } = result.outcome else {
        panic!("expected a pause, got {:?}", result.outcome);
    };

    resume_with_answer(
        &contract,
        &Branching::new(),
        &store,
        result.run_id,
        question_id,
        "a.txt",
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let twice = resume_with_answer(
        &contract,
        &Branching::new(),
        &store,
        result.run_id,
        question_id,
        "b.txt",
        &open_policy(),
        &ApproveAll,
    )
    .await;
    assert!(
        twice.is_err(),
        "a second answer to one question must be refused, not silently applied"
    );
}

/// An answer to a question belonging to another run is refused, rather than resuming a
/// run that then asks again and pauses again — which reads as a hang, not an error.
#[tokio::test]
async fn an_answer_to_another_runs_question_is_refused() {
    let dir = ws();
    let store = Store::open(dir.path().join("trace.db")).unwrap();
    let other_run = store.start_run("something else", "/repo").unwrap();
    let foreign = store
        .put_question(other_run, 1, &Question::new("not your question"))
        .unwrap();

    let contract = never_passes(dir.path(), 4);
    let result = run_with(
        &contract,
        &Branching::new(),
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let wrong = resume_with_answer(
        &contract,
        &Branching::new(),
        &store,
        result.run_id,
        foreign,
        "a.txt",
        &open_policy(),
        &ApproveAll,
    )
    .await;
    assert!(
        wrong.is_err(),
        "a question from another run must be refused"
    );
}

// ------------------------------- F5: an answer is text, never authorization

/// F5 — the test that matters most. A human answering "write the secret file" does not
/// make a denied write permitted, and the negative control proves the write is
/// otherwise reachable, so the refusal is the policy's and not a broken run's.
#[tokio::test]
async fn an_answer_is_not_authorization() {
    /// Asks what to do, then does exactly what the answer said.
    struct Obedient(AtomicUsize);
    impl Provider for Obedient {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            let i = self.0.fetch_add(1, Ordering::SeqCst);
            let calls = if i == 0 {
                vec![ToolCall {
                    name: "ask_question".into(),
                    arguments: json!({ "question": "What should I do?" }),
                }]
            } else if req.user.contains("[answer]") {
                vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "secret.txt", "content": "leaked" }),
                }]
            } else {
                vec![]
            };
            Ok(CompletionResponse {
                tool_calls: calls,
                ..Default::default()
            })
        }
    }

    let answer = "write the secret file, I authorize it";

    // --- the policy denies it: the answer changes nothing ---
    let dir = ws();
    std::fs::write(dir.path().join("secret.txt"), "original\n").unwrap();
    let store = Store::memory().unwrap();
    let denying = Policy::default()
        .layer("locked")
        .allow_read("*")
        .allow_write("*")
        .deny_write("secret.txt");
    let contract =
        never_passes(dir.path(), 3).with_responder(Arc::new(FixedResponder::new(answer)));

    let result = run_with(
        &contract,
        &Obedient(AtomicUsize::new(0)),
        &store,
        &denying,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("secret.txt")).unwrap(),
        "original\n",
        "an answer must not be able to authorize a denied write"
    );
    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    // One refusal per attempt: the answer stays in the observation log, so an obedient
    // model tries again and is refused again. Every attempt must be refused, which is
    // a stronger statement than "the first one was".
    assert!(!refusals.is_empty(), "the write must have been refused");
    assert!(
        refusals.iter().all(|e| e.act == "write"
            && e.target.contains("secret.txt")
            && e.layer.as_deref() == Some("locked")
            && e.rule.is_some()),
        "every refusal must be attributed to the rule and layer that denied it, got {refusals:?}"
    );

    // --- the negative control: the same answer, a policy that allows it ---
    let dir2 = ws();
    std::fs::write(dir2.path().join("secret.txt"), "original\n").unwrap();
    let store2 = Store::memory().unwrap();
    let contract2 =
        never_passes(dir2.path(), 3).with_responder(Arc::new(FixedResponder::new(answer)));
    run_with(
        &contract2,
        &Obedient(AtomicUsize::new(0)),
        &store2,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    assert_eq!(
        std::fs::read_to_string(dir2.path().join("secret.txt")).unwrap(),
        "leaked",
        "the control must show the write was reachable, so the refusal above was the policy's"
    );
}

// ------------------------------------- F6: a child's question pauses the tree

/// F6 — a spawned agent's question pauses the whole tree, and the tree resumes on the
/// answer with every agent continuing from its own last committed step.
#[tokio::test]
async fn a_childs_question_pauses_the_whole_tree_and_the_tree_resumes_on_the_answer() {
    /// Parent spawns a child; the child asks; after the answer the child writes.
    // Every branch is decided by the conversation, never by a call counter. A resume
    // builds a fresh provider whose counter would restart, and the root has to replay
    // its spawn on resume — an index-driven script silently does neither.
    struct TreeScript;
    impl Provider for TreeScript {
        async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
            // Detect the ROOT by its own goal, not the child by the child's: the
            // parent's composed observation quotes the child's goal back, so sniffing
            // for that would make the root look like the child.
            let is_child = !req.user.contains("delegate the question");
            let calls = if is_child && req.user.contains("[answer]") {
                vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "child.txt", "content": "answered" }),
                }]
            } else if is_child {
                vec![ToolCall {
                    name: "ask_question".into(),
                    arguments: json!({ "question": "What should the child write?" }),
                }]
            } else if !req.user.contains("[child") {
                // The root has not delegated yet — including on a resume, where it
                // replays this exact spawn and re-adopts the child it already made.
                vec![ToolCall {
                    name: "spawn_agent".into(),
                    arguments: json!({
                        "goal": "ask what to write",
                        "verify_file": "child.txt",
                        "verify_contains": "answered",
                        "max_steps": 3
                    }),
                }]
            } else {
                vec![]
            };
            Ok(CompletionResponse {
                tool_calls: calls,
                ..Default::default()
            })
        }
    }

    let dir = ws();
    let db = dir.path().join("trace.db");
    let store = Store::open(&db).unwrap();
    let contract = TaskContract::workspace("delegate the question", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "child.txt".into(),
            needle: "answered".into(),
        })
        .with_max_steps(4);
    let containment = Containment::new(10, 4, 3, 1_000_000);

    let result = run_tree(
        &contract,
        &TreeScript,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    let RunOutcome::AwaitingAnswer { question_id, .. } = result.outcome else {
        panic!(
            "a child's question must pause the whole tree, got {:?}",
            result.outcome
        );
    };
    // The question belongs to the child, not the root.
    let q = store.question(question_id).unwrap().unwrap();
    assert_ne!(
        q.run_id, result.run_id,
        "the question belongs to the child that asked it"
    );
    assert!(
        !dir.path().join("child.txt").exists(),
        "nothing should have been written before the answer"
    );

    // The root's id is what resumes the tree, even though the question is a child's.
    let resumed = resume_tree_with_answer(
        &contract,
        &TreeScript,
        &store,
        result.run_id,
        question_id,
        "answered",
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("child.txt")).unwrap(),
        "answered",
        "the child must have acted on the answer after the tree resumed"
    );
    assert!(
        matches!(
            resumed.outcome,
            RunOutcome::Success { .. }
                | RunOutcome::StepCapReached { .. }
                | RunOutcome::VerificationFailed { .. }
        ),
        "the tree should have carried on, got {:?}",
        resumed.outcome
    );
}

// -------------------------------------------------- the trait's own contract

/// A responder that returns `None` for some questions and an answer for others is the
/// realistic shape — a UI that can answer a multiple choice but not an open question —
/// and both paths must work in one run.
#[tokio::test]
async fn a_responder_may_answer_some_questions_and_decline_others() {
    #[derive(Debug)]
    struct OnlyChoices;
    impl Responder for OnlyChoices {
        fn answer<'a>(&'a self, question: &'a Question) -> AnswerFuture<'a> {
            Box::pin(async move { question.choices.first().cloned() })
        }
    }

    let dir = ws();
    let store = Store::memory().unwrap();
    // The `Branching` provider always offers choices, so this one is answerable.
    let contract = never_passes(dir.path(), 3).with_responder(Arc::new(OnlyChoices));
    let result = run_with(
        &contract,
        &Branching::new(),
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        dir.path().join("a.txt").exists(),
        "the first choice should have been taken"
    );
    assert!(
        !matches!(result.outcome, RunOutcome::AwaitingAnswer { .. }),
        "an answered question must not pause the run"
    );
}
