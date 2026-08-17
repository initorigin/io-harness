//! The host bound once, and the proof that binding it changed nothing.
//!
//! Two claims, and the second is the one that matters. **F1** is that a caller
//! builds the provider, the store, the boundary and the host's own configuration
//! once and drives more than one task through them without restating any of it.
//! **F2** is that a run driven through the [`Harness`] and the same run driven
//! through the free function produce the *same trace* — not merely the same
//! [`RunOutcome`].
//!
//! F2 is written the way it is because outcome equality would pass against a
//! facade that quietly ran a different loop, which is a worse outcome than no
//! facade at all. `Store::canonical_trace` is the comparison `tests/determinism.rs`
//! already trusts for exactly this question, and it is proven capable of failing
//! there.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, Harness, Policy, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

/// Writes one file per turn, so the run makes real progress and its trace has
/// something in it to compare.
#[derive(Default)]
struct Script {
    at: AtomicUsize,
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: Some(format!("turn {i}")),
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({
                    "path": format!("src/f{i}.rs"),
                    "content": format!("fn hello{i}() -> u32 {{ {i} }}\n"),
                }),
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

// ---------------------------------------------------------------------------
// F1 — the host is bound once and two tasks run through it
// ---------------------------------------------------------------------------

/// Three host settings, bound once, reaching **both** of two different tasks.
///
/// The assertion is on what each contract carries rather than on the run's
/// return value, because that is what a caller who forgot to restate a setting
/// would lose. A sabotage that drops a bound setting on the path from the harness
/// to the second contract fails here on the second task and not the first, which
/// is what tells "bound once" apart from "bound per call".
#[tokio::test]
async fn the_host_is_bound_once_and_two_tasks_run_through_it() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::default();

    // Three settings that are properties of the host, not of either task.
    let host = TaskContract::workspace("", "")
        .with_max_steps(3)
        .with_max_retries(5)
        .with_instruction("never touch Cargo.lock");

    let harness = Harness::new(&provider, &store)
        .with_policy(open_policy())
        .with_approver(&ApproveAll)
        .with_defaults(host);

    let first = harness.workspace("write the first file", dir.path());
    let second = harness.task(
        "write the second file",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "src/f1.rs".into(),
            needle: "fn hello1".into(),
        },
    );

    // Bound once, present in both — and neither call restated any of them.
    for (which, contract) in [("first", &first), ("second", &second)] {
        assert_eq!(contract.max_steps, 3, "{which}: the bound step cap");
        assert_eq!(contract.max_retries, 5, "{which}: the bound retry cap");
        assert_eq!(
            contract.instructions,
            vec!["never touch Cargo.lock".to_string()],
            "{which}: the bound repository instruction"
        );
        assert_eq!(
            contract.root.as_deref(),
            Some(dir.path()),
            "{which}: the root each call gave"
        );
    }

    // The goals are the two callers' own, and the verification is the second
    // caller's alone — the template supplies the host, never the task.
    assert_eq!(first.goal, "write the first file");
    assert_eq!(second.goal, "write the second file");
    assert!(matches!(first.verify, Verification::None));
    assert!(matches!(
        second.verify,
        Verification::WorkspaceFileContains { .. }
    ));

    // And both actually run through the bound provider, store and boundary.
    let ran = harness.run(&first).await.unwrap();
    assert!(store.run_summary(ran.run_id).unwrap().is_some());
    let ran = harness.run(&second).await.unwrap();
    assert!(
        matches!(ran.outcome, RunOutcome::Success { .. }),
        "the second task's verification passed: {:?}",
        ran.outcome
    );
}

/// A contract handed to [`Harness::run`] is used verbatim: the template is a
/// source for [`Harness::workspace`] and [`Harness::task`] and for nothing else.
///
/// This is the recorded design decision made checkable. The rejected rule — fill
/// in whatever a contract still holds at its default — cannot tell a caller who
/// set a field to its default value from one who never set it, and this test is
/// where that rule would show up as a surprise.
#[tokio::test]
async fn a_contract_handed_to_the_harness_is_used_verbatim() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::default();

    let harness = Harness::new(&provider, &store)
        .with_policy(open_policy())
        .with_defaults(TaskContract::workspace("", "").with_max_steps(99));

    // Built without the harness, so it carries the crate's own default cap.
    let mine = TaskContract::workspace("write a file", dir.path()).with_max_steps(2);
    let default_cap = TaskContract::workspace("x", "y").max_steps;

    let result = harness.run(&mine).await.unwrap();
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps.len() <= 2,
        "the caller's own cap of 2 governed, not the harness's 99: {} steps",
        steps.len()
    );
    assert_ne!(
        default_cap, 99,
        "the template's cap must differ from the crate default, or this test proves nothing"
    );
}

// ---------------------------------------------------------------------------
// F2 — the facade and the free function produce the same trace
// ---------------------------------------------------------------------------

/// One contract, two stores, two ways in — and the canonical traces are equal.
///
/// The stores are fresh for the reason `tests/determinism.rs` gives: run ids are
/// `AUTOINCREMENT` and reach the model's own observations, so two runs sharing a
/// store cannot be compared. Equality here is the whole claim that the `Harness`
/// is a binding rather than a second implementation.
#[tokio::test]
async fn the_facade_and_the_free_function_produce_the_same_trace() {
    let contract = |root: &std::path::Path| {
        TaskContract::workspace("write a few files", root)
            .with_verification(Verification::WorkspaceFileContains {
                file: "src/f2.rs".into(),
                needle: "fn hello2".into(),
            })
            .with_max_steps(4)
    };

    // Through the free function.
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::default();
    let direct = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();
    let direct_trace = store.canonical_trace(direct.run_id).unwrap();

    // Through the harness, with the same boundary bound instead of passed.
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::default();
    let harness = Harness::new(&provider, &store)
        .with_policy(open_policy())
        .with_approver(&ApproveAll);
    let faced = harness.run(&contract(dir.path())).await.unwrap();
    let faced_trace = store.canonical_trace(faced.run_id).unwrap();

    assert!(
        !direct_trace.is_empty(),
        "an empty trace would make the comparison below vacuous"
    );
    assert_eq!(
        direct_trace, faced_trace,
        "the harness must drive the loop the free function drives, step for step"
    );
    assert_eq!(
        format!("{:?}", direct.outcome),
        format!("{:?}", faced.outcome)
    );
}

/// The harness's defaults are the free function's defaults.
///
/// `Harness::new` binds `Policy::permissive`, `ApproveAll` and `Ignore` — which is
/// exactly what `run` and `run_observed` use — so an unconfigured harness and the
/// unpoliced entry point are the same run. Without this, "the defaults match" is
/// a comment rather than a claim.
#[tokio::test]
async fn an_unconfigured_harness_is_the_unpoliced_entry_point() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::default();
    let harness = Harness::new(&provider, &store);

    assert!(harness.policy().is_permissive());
    assert!(std::ptr::eq(harness.store(), &store));

    // A single-file contract is the shape that refuses under a policy-bearing
    // caller, so it is the sharpest test that the bound default really is
    // permissive: this returns a result rather than `Error::Config`.
    let contract = harness.workspace("write a file", dir.path());
    assert!(harness.run(&contract).await.is_ok());
}
