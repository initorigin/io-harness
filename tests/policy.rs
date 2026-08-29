//! Policy and approval through the full loop, with a scripted mock provider so
//! the tests are deterministic and offline.
//!
//! These prove the 0.4.0 outcome at the run level: an out-of-policy action is
//! refused and the run carries on, a sensitive action stops for a human, and
//! every refusal and decision lands in the trace.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::policy::{Act, Effect, Policy, Rule};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run, run_with, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

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

fn write(path: &str, content: &str) -> ToolCall {
    call("write_file", json!({ "path": path, "content": content }))
}

/// A fixture repo with two stubs and a secret the agent must not touch.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(src.join("a.rs"), "pub fn a() -> u32 { 0 }\n").unwrap();
    std::fs::write(src.join("b.rs"), "pub fn b() -> u32 { 0 }\n").unwrap();
    std::fs::write(dir.path().join("secrets/key.txt"), "original-secret").unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        src.join("lib.rs"),
        "pub mod a;\npub mod b;\n#[test] fn t() { assert_eq!(a::a() + b::b(), 42); }\n",
    )
    .unwrap();
    dir
}

/// The success spec: the two files, together, must make `a() + b() == 42`, and
/// the project's own runner is what says so. Until 0.18.0 this was
/// `WorkspaceTestPasses`, which concatenated the files the caller listed and
/// compiled a criterion beside them; `cargo test` runs the crate's real suite,
/// which is what the migration note tells a caller to write.
fn verify() -> Verification {
    Verification::Command {
        argv: vec!["cargo".into(), "test".into(), "--offline".into()],
        expect_exit: 0,
    }
}

/// src/ is writable, secrets/ is denied outright.
///
/// `cargo` is allowed because the verification gate is a command since 0.18.0
/// and verification cannot prompt: a gate spawns only what a rule allows
/// outright. It is an allow on the criterion's runner, not on the agent's reach
/// — the read and write rules below are what these tests are about.
fn guarded() -> Policy {
    Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_exec("cargo")
        .deny_read("secrets/*")
        .deny_write("secrets/*")
}

/// Counts how many times it was consulted, so a test can assert it was *not*.
struct Counting {
    calls: AtomicUsize,
    decision: Mutex<Decision>,
}

impl Counting {
    fn new(decision: Decision) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            decision: Mutex::new(decision),
        }
    }
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl Approver for Counting {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let d = self.decision.lock().unwrap().clone();
        Box::pin(async move { d })
    }
}

const GOOD_A: &str = "pub fn a() -> u32 { 20 }\n";
const GOOD_B: &str = "pub fn b() -> u32 { 22 }\n";

#[tokio::test]
async fn a_denied_write_is_refused_and_the_run_still_reaches_its_goal() {
    let dir = fixture();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    // The model tries the secret first, then does the real work.
    let script = MockScript::new(vec![
        vec![write("secrets/key.txt", "exfiltrated")],
        vec![write("src/a.rs", GOOD_A), write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    // The secret was never modified.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret"
    );
    // The refusal is in the trace, attributable to the rule and layer.
    let events = store.events(result.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal")
        .expect("refusal");
    assert_eq!(refusal.act, "write");
    assert_eq!(refusal.target, "secrets/key.txt");
    assert_eq!(refusal.rule.as_deref(), Some("secrets/*"));
    assert_eq!(refusal.layer.as_deref(), Some("base"));
}

#[tokio::test]
async fn a_denied_action_never_reaches_the_approver() {
    let dir = fixture();
    let contract = TaskContract::workspace("touch the secret", dir.path())
        .with_verification(verify())
        .with_max_steps(1);
    let script = MockScript::new(vec![vec![write("secrets/key.txt", "x")]]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    // Refusal and approval are different things: a deny is not a prompt.
    assert_eq!(
        approver.count(),
        0,
        "a denied action must not be offered for approval"
    );
}

#[tokio::test]
async fn approve_all_proceeds_where_deny_all_does_not() {
    for (decision, expect_written) in [(Decision::approve(), true), (Decision::deny("nope"), false)]
    {
        let dir = fixture();
        let contract = TaskContract::workspace("edit a", dir.path())
            .with_verification(verify())
            .with_max_steps(1);
        let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
        let store = Store::memory().unwrap();
        let approver = Counting::new(decision);

        run_with(&contract, &script, &store, &guarded(), &approver)
            .await
            .unwrap();

        let written = std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap() == GOOD_A;
        assert_eq!(written, expect_written);
        assert_eq!(approver.count(), 1, "the write should have been offered");
    }
}

#[tokio::test]
async fn reads_do_not_prompt_but_every_write_does_including_an_allowed_overwrite() {
    let dir = fixture();
    let contract = TaskContract::workspace("read then write", dir.path())
        .with_verification(verify())
        .with_max_steps(2);
    let script = MockScript::new(vec![
        vec![call("read_file", json!({ "path": "src/a.rs" }))],
        // src/a.rs already exists and the path rules allow it — it still asks.
        vec![write("src/a.rs", GOOD_A)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    assert_eq!(
        approver.count(),
        1,
        "the read must not prompt and the overwrite must"
    );
}

#[tokio::test]
async fn an_approver_can_redirect_the_write_and_the_trace_shows_both_forms() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit a", dir.path())
        .with_verification(verify())
        .with_max_steps(1);
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::Approve {
        modified: Some(Request::new(Act::Write, "src/redirected.rs").with_content(GOOD_A)),
        remember: Vec::new(),
    });

    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    // The rewritten action ran, the requested one did not.
    assert!(dir.path().join("src/redirected.rs").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
        "pub fn a() -> u32 { 0 }\n"
    );
    let events = store.events(result.run_id).unwrap();
    let d = events
        .iter()
        .find(|e| e.kind == "decision")
        .expect("decision");
    assert_eq!(d.target, "src/a.rs");
    assert_eq!(d.performed.as_deref(), Some("src/redirected.rs"));
}

#[tokio::test]
async fn an_approved_rewrite_cannot_move_an_action_across_a_deny() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit a", dir.path())
        .with_verification(verify())
        .with_max_steps(1);
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let store = Store::memory().unwrap();
    // A rogue approver tries to redirect an allowed write onto a denied path.
    let approver = Counting::new(Decision::Approve {
        modified: Some(Request::new(Act::Write, "secrets/key.txt").with_content("stolen")),
        remember: Vec::new(),
    });

    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    // Deny is absolute — approval does not unlock it.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret"
    );
    let events = store.events(result.run_id).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "refusal" && e.target == "secrets/key.txt"),
        "the rewritten action must be refused and recorded"
    );
}

#[tokio::test]
async fn remembering_a_rule_stops_the_prompting_and_is_returned_to_the_caller() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit both", dir.path())
        .with_verification(verify())
        .with_max_steps(3);
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::Approve {
        modified: None,
        remember: vec![Rule {
            act: Act::Write,
            effect: Effect::Allow,
            pattern: "src/*".into(),
        }],
    });

    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    // Asked once; the second write matched the remembered rule.
    assert_eq!(
        approver.count(),
        1,
        "a remembered rule must stop the second prompt"
    );
    // and the caller gets the rules back to persist if it wants to.
    assert_eq!(result.remembered.len(), 1);
    assert_eq!(result.remembered[0].pattern, "src/*");
}

#[tokio::test]
async fn a_remembered_allow_cannot_override_a_base_deny() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit then exfiltrate", dir.path())
        .with_verification(verify())
        .with_max_steps(2);
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "stolen")],
    ]);
    let store = Store::memory().unwrap();
    // Remembers an allow covering everything — including the denied tree.
    let approver = Counting::new(Decision::Approve {
        modified: None,
        remember: vec![Rule {
            act: Act::Write,
            effect: Effect::Allow,
            pattern: "*".into(),
        }],
    });

    run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret",
        "a remembered allow must not defeat a deny beneath it"
    );
}

#[tokio::test]
async fn a_refused_action_consumes_a_step_so_retrying_it_hits_the_cap() {
    let dir = fixture();
    let contract = TaskContract::workspace("keep trying the secret", dir.path())
        .with_verification(verify())
        .with_max_steps(3);
    // The model requests the same denied write every step.
    let script = MockScript::new(vec![
        vec![write("secrets/key.txt", "x")],
        vec![write("secrets/key.txt", "x")],
        vec![write("secrets/key.txt", "x")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &io_harness::approve::ApproveAll,
    )
    .await
    .unwrap();

    // Bounded by the step cap rather than looping forever. The contract carries a
    // criterion that never passed, so the capped run reports the 0.70.0 variant.
    assert_eq!(result.outcome, RunOutcome::VerificationFailed { steps: 3 });
    let refusals = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .count();
    assert_eq!(refusals, 3, "one refusal record per consumed step");
}

#[tokio::test]
async fn deferring_pauses_the_run_and_persists_the_pending_action() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit a", dir.path())
        .with_verification(verify())
        .with_max_steps(2);
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let approver = Counting::new(Decision::Defer);

    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    let request_id = match result.outcome {
        RunOutcome::AwaitingApproval { request_id, steps } => {
            assert_eq!(steps, 1);
            request_id
        }
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };
    // Nothing was written while waiting.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
        "pub fn a() -> u32 { 0 }\n"
    );

    // The pending action outlives this Store, as a different process would see it.
    drop(store);
    let reopened = Store::open(&path).unwrap();
    let pending = reopened
        .pending(request_id)
        .unwrap()
        .expect("still pending");
    assert_eq!(pending.act, "write");
    assert_eq!(pending.target, "src/a.rs");
    assert_eq!(pending.content.as_deref(), Some(GOOD_A));
    assert_eq!(pending.resolved, None);
}

#[tokio::test]
async fn a_run_with_no_policy_behaves_exactly_as_0_3_0_did() {
    let dir = fixture();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    let script = MockScript::new(vec![vec![
        write("src/a.rs", GOOD_A),
        write("src/b.rs", GOOD_B),
        // 0.3.0 had no notion of a protected path, and still does not by default.
        write("secrets/key.txt", "whatever"),
    ]]);
    let store = Store::memory().unwrap();

    let result = run(&contract, &script, &store).await.unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "whatever",
        "the boundary is opt-in: no policy means no enforcement"
    );
    // Nothing was refused and nothing was put to an approver. Verification
    // spawns are still traced — recording argv is unconditional, so 0.6.0 can
    // add argument-level rules against a real baseline.
    let events = store.events(result.run_id).unwrap();
    assert!(!events.iter().any(|e| e.kind == "refusal"));
    assert!(!events
        .iter()
        .any(|e| e.source.as_deref() == Some("approver")));
    assert!(
        events
            .iter()
            .any(|e| e.act == "exec" && e.target.starts_with("cargo test")),
        "every spawn is recorded with its full argv"
    );
}

#[tokio::test]
async fn resuming_an_approval_performs_the_pending_action_and_finishes_the_run() {
    let dir = fixture();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    // Step 1 defers on the write to a.rs; after resuming, step 2 writes b.rs.
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();

    let paused = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::Defer),
    )
    .await
    .unwrap();
    let (run_id, request_id) = match paused.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => (paused.run_id, request_id),
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    // The human decides later, through a different Store over the same file.
    drop(store);
    let store = Store::open(&path).unwrap();
    let resumed = io_harness::resume_with_decision(
        &contract,
        &script,
        &store,
        run_id,
        request_id,
        Decision::approve(),
        &guarded(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();

    assert_eq!(
        resumed.run_id, run_id,
        "continues under the original run id"
    );
    assert_eq!(resumed.outcome, RunOutcome::Success { steps: 2 });
    // The approved action performed exactly what the human was shown.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
        GOOD_A
    );
    assert_eq!(
        store
            .pending(request_id)
            .unwrap()
            .unwrap()
            .resolved
            .as_deref(),
        Some("approve")
    );
    let events = store.events(run_id).unwrap();
    assert!(events
        .iter()
        .any(|e| e.source.as_deref() == Some(&format!("resumed:{request_id}")[..])));
}

#[tokio::test]
async fn resuming_a_denial_does_not_perform_the_action_and_closes_the_run() {
    let dir = fixture();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let store = Store::open(dir.path().join("runs.db")).unwrap();

    let paused = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::Defer),
    )
    .await
    .unwrap();
    let request_id = match paused.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    let resumed = io_harness::resume_with_decision(
        &contract,
        &script,
        &store,
        paused.run_id,
        request_id,
        Decision::deny("not this one"),
        &guarded(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();

    assert_eq!(resumed.outcome, RunOutcome::Denied { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
        "pub fn a() -> u32 { 0 }\n",
        "a denied action must not be performed on resume"
    );
    assert_eq!(
        store
            .pending(request_id)
            .unwrap()
            .unwrap()
            .resolved
            .as_deref(),
        Some("deny")
    );
}

#[tokio::test]
async fn a_deny_that_lands_while_paused_still_holds_on_resume() {
    let dir = fixture();
    let contract = TaskContract::workspace("edit a", dir.path()).with_verification(verify());
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let store = Store::open(dir.path().join("runs.db")).unwrap();

    let paused = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::Defer),
    )
    .await
    .unwrap();
    let request_id = match paused.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    // The policy tightened while the human was deciding.
    let tightened = guarded().layer("tightened").deny_write("src/a.rs");
    let resumed = io_harness::resume_with_decision(
        &contract,
        &script,
        &store,
        paused.run_id,
        request_id,
        Decision::approve(),
        &tightened,
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();

    assert_eq!(resumed.outcome, RunOutcome::Denied { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/a.rs")).unwrap(),
        "pub fn a() -> u32 { 0 }\n",
        "the pause must not grant immunity from a deny"
    );
}

#[tokio::test]
async fn a_slow_approver_keeps_the_whole_run_waiting_and_it_then_completes() {
    use tokio::sync::Mutex as AsyncMutex;

    /// Answers only after the decision arrives from elsewhere, mid-run.
    struct Awaited {
        rx: AsyncMutex<Option<tokio::sync::oneshot::Receiver<Decision>>>,
    }
    impl Approver for Awaited {
        fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
            Box::pin(async move {
                let taken = self.rx.lock().await.take();
                match taken {
                    Some(rx) => rx.await.unwrap_or(Decision::deny("closed")),
                    None => Decision::approve(),
                }
            })
        }
    }

    let dir = fixture();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    let script = MockScript::new(vec![vec![
        write("src/a.rs", GOOD_A),
        write("src/b.rs", GOOD_B),
    ]]);
    let store = Store::memory().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let approver = Awaited {
        rx: AsyncMutex::new(Some(rx)),
    };

    // The decision lands well after the run is already blocked on it.
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(80)).await;
        let _ = tx.send(Decision::approve());
    });

    let started = std::time::Instant::now();
    let result = run_with(&contract, &script, &store, &guarded(), &approver)
        .await
        .unwrap();

    assert!(
        started.elapsed() >= std::time::Duration::from_millis(70),
        "the run must wait for the human rather than time out"
    );
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
}

#[tokio::test]
async fn refusal_and_decision_records_never_carry_file_contents() {
    let dir = fixture();
    let secret_body = "SUPER-SECRET-VALUE";
    std::fs::write(dir.path().join("secrets/key.txt"), secret_body).unwrap();
    let contract = TaskContract::workspace("make a+b 42", dir.path()).with_verification(verify());
    let script = MockScript::new(vec![
        vec![write("secrets/key.txt", "PAYLOAD-THAT-MUST-NOT-BE-LOGGED")],
        vec![write("src/a.rs", GOOD_A), write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();

    let result = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();

    for e in store.events(result.run_id).unwrap() {
        let row = format!("{e:?}");
        assert!(
            !row.contains("PAYLOAD-THAT-MUST-NOT-BE-LOGGED"),
            "a policy record must not carry the write payload: {row}"
        );
        assert!(
            !row.contains(secret_body),
            "a policy record must not carry file contents: {row}"
        );
        assert!(
            !row.contains(GOOD_A),
            "a policy record must not carry the written body: {row}"
        );
    }
}

#[tokio::test]
async fn a_policy_on_a_single_file_contract_is_refused_not_silently_ignored() {
    // Single-file mode has no policy-aware tool layer in 0.4.0. The dangerous
    // outcome would be accepting the policy and enforcing nothing, so the run
    // must refuse rather than hand back a false sense of a boundary.
    let dir = fixture();
    let contract = TaskContract::new(
        "edit it",
        dir.path().join("src/a.rs"),
        Verification::FileContains("fn a".into()),
    );
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    let store = Store::memory().unwrap();

    let err = run_with(
        &contract,
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap_err();
    assert!(
        format!("{err}").contains("requires workspace mode"),
        "expected a loud refusal, got {err}"
    );

    // A permissive policy is the no-enforcement case and must still work.
    let script = MockScript::new(vec![vec![write("src/a.rs", GOOD_A)]]);
    assert!(run(&contract, &script, &store).await.is_ok());
}

// ---------------------------------------------------------------------------
// 0.13.0 — a resumed run is the run it was.
//
// `resume` took no policy and substituted `Policy::permissive()` with
// `ApproveAll` for every workspace run, so a caller who ran under a boundary and
// crashed resumed without one. Nothing warned; the trace showed no refusals
// because nothing refused. These are the tests for the boundary half.
// ---------------------------------------------------------------------------

/// A step budget of one is how a test interrupts a run: the run stops with work
/// left, exactly as a crashed process leaves it, and the resume continues under
/// the original run id.
fn capped(dir: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("make a+b 42", dir)
        .with_verification(verify())
        .with_max_steps(steps)
}

#[tokio::test]
async fn a_resumed_run_still_enforces_the_policy_it_was_started_under() {
    let dir = fixture();
    // Step 1 does real work and stops at the budget. Step 2 — the one the resume
    // drives — goes for the secret.
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "exfiltrated")],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &guarded(),
        &approver,
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, RunOutcome::VerificationFailed { steps: 1 });

    let resumed = io_harness::resume_with(
        &capped(dir.path(), 5),
        &script,
        &store,
        first.run_id,
        &guarded(),
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(resumed.run_id, first.run_id, "one run, not two");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret",
        "the denied write was refused after the resume, not performed"
    );
    let events = store.events(first.run_id).unwrap();
    let refusal = events
        .iter()
        .find(|e| e.kind == "refusal")
        .expect("the refusal is in the trace under the original run id");
    assert_eq!(refusal.act, "write");
    assert_eq!(refusal.target, "secrets/key.txt");
    assert_eq!(refusal.rule.as_deref(), Some("secrets/*"));
}

/// The negative control for the test above. Same fixture, same script, same
/// resume — but resumed permissively on purpose. The secret IS overwritten,
/// which proves the assertion above detects an enforced boundary rather than
/// passing because the write would have failed anyway.
#[tokio::test]
async fn the_same_resume_performs_the_write_when_no_policy_is_supplied() {
    let dir = fixture();
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "exfiltrated")],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &Policy::permissive(),
        &approver,
    )
    .await
    .unwrap();

    io_harness::resume_with(
        &capped(dir.path(), 5),
        &script,
        &store,
        first.run_id,
        &Policy::permissive(),
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "exfiltrated",
        "with no boundary the write lands — so the test above is measuring the boundary"
    );
}

#[tokio::test]
async fn a_bare_resume_refuses_a_run_that_was_started_under_a_policy() {
    let dir = fixture();
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "exfiltrated")],
    ]);
    let store = Store::memory().unwrap();

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &guarded(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();
    let steps_before = store.steps(first.run_id).unwrap().len();

    let err = io_harness::resume(&capped(dir.path(), 5), &script, &store, first.run_id)
        .await
        .unwrap_err();

    let message = format!("{err}");
    assert!(
        message.contains(&first.run_id.to_string()) && message.contains("resume_with"),
        "the error names the run and the alternative, got {message}"
    );
    assert_eq!(
        store.steps(first.run_id).unwrap().len(),
        steps_before,
        "refused before driving the loop, so no step was taken"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret"
    );
}

#[tokio::test]
async fn a_bare_resume_still_works_for_a_run_that_never_had_a_boundary() {
    let dir = fixture();
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &Policy::permissive(),
        &Counting::new(Decision::approve()),
    )
    .await
    .unwrap();
    assert_eq!(first.outcome, RunOutcome::VerificationFailed { steps: 1 });

    let resumed = io_harness::resume(&capped(dir.path(), 5), &script, &store, first.run_id)
        .await
        .unwrap();

    assert_eq!(resumed.run_id, first.run_id);
    assert_eq!(resumed.outcome, RunOutcome::Success { steps: 2 });
}

// ---------------------------------------------------------------------------
// 0.15.0 — resuming from the policy the store already holds.
//
// The policy has been durable since 0.13.0, but a caller still had to
// reconstruct one to resume with it. A caller resuming after a crash in another
// process may have nothing to reconstruct it from — and from 0.15.0 a crashed
// run may already have taken an irreversible action under it.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_resumed_from_its_stored_policy_is_still_bounded_by_it() {
    let dir = fixture();
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "exfiltrated")],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &guarded(),
        &approver,
    )
    .await
    .unwrap();

    // No policy passed. The caller has the run id and nothing else, which is the
    // situation this entry point exists for.
    let resumed = io_harness::resume_from_stored_policy(
        &capped(dir.path(), 5),
        &script,
        &store,
        first.run_id,
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(resumed.run_id, first.run_id, "one run, not two");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "original-secret",
        "the boundary was recovered from the store, not lost with the process"
    );
    let events = store.events(first.run_id).unwrap();
    assert!(
        events
            .iter()
            .any(|e| e.kind == "refusal" && e.target == "secrets/key.txt"),
        "the refusal is in the trace: {events:?}"
    );
}

/// The negative control. The same call on a run started permissively performs
/// the write — so the test above is measuring a recovered boundary rather than a
/// resume that denies everything.
#[tokio::test]
async fn the_same_call_on_a_permissive_run_performs_the_write() {
    let dir = fixture();
    let script = MockScript::new(vec![
        vec![write("src/a.rs", GOOD_A)],
        vec![write("secrets/key.txt", "exfiltrated")],
        vec![write("src/b.rs", GOOD_B)],
    ]);
    let store = Store::memory().unwrap();
    let approver = Counting::new(Decision::approve());

    let first = run_with(
        &capped(dir.path(), 1),
        &script,
        &store,
        &Policy::permissive(),
        &approver,
    )
    .await
    .unwrap();

    io_harness::resume_from_stored_policy(
        &capped(dir.path(), 5),
        &script,
        &store,
        first.run_id,
        &approver,
    )
    .await
    .unwrap();

    assert_eq!(
        std::fs::read_to_string(dir.path().join("secrets/key.txt")).unwrap(),
        "exfiltrated"
    );
}

#[tokio::test]
async fn a_run_with_no_recorded_policy_is_refused_rather_than_resumed_permissively() {
    // Substituting a permissive policy for one that cannot be found is the exact
    // defect 0.13.0 closed. An unknown run id has no recorded policy, which is
    // the same state a pre-0.13.0 run row is in.
    let dir = fixture();
    let store = Store::memory().unwrap();
    let err = io_harness::resume_from_stored_policy(
        &capped(dir.path(), 5),
        &MockScript::new(vec![]),
        &store,
        424_242,
        &Counting::new(Decision::approve()),
    )
    .await
    .expect_err("a run whose boundary cannot be recovered must not be resumed");
    assert!(matches!(&err, io_harness::Error::Resume { .. }), "{err:?}");
}
