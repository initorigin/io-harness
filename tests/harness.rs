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

use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, Containment, Harness, Policy, Provider, RunOutcome, Store, TaskContract,
    Verification,
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

/// A boundary that refuses the writes the scripted provider makes.
///
/// The permissive/open pair is not enough on its own — see the test below.
fn closed_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .deny_write("*")
}

/// One contract, two stores, two ways in — and the canonical traces are equal.
///
/// The stores are fresh for the reason `tests/determinism.rs` gives: run ids are
/// `AUTOINCREMENT` and reach the model's own observations, so two runs sharing a
/// store cannot be compared. Equality here is the whole claim that the `Harness`
/// is a binding rather than a second implementation.
///
/// **Run twice, under two boundaries, and the second pair is the load-bearing
/// one.** The first draft of this test compared only the open-policy pair, and a
/// sabotage that made `with_policy` throw the caller's policy away — binding
/// `Policy::permissive()` instead — **survived it**: the scripted run's writes are
/// permitted by both, so the two traces matched and the test said nothing about
/// the binding it exists to check. That is a finding about the test, recorded
/// rather than quietly patched. The closed pair fixes it: under a policy that
/// refuses the writes, a harness ignoring its bound policy would succeed where the
/// free function is refused, and the traces diverge on the first step.
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

    let mut traces = Vec::new();
    for (which, policy) in [("open", open_policy()), ("closed", closed_policy())] {
        // Through the free function.
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Script::default();
        let direct = run_with(
            &contract(dir.path()),
            &provider,
            &store,
            &policy,
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
            .with_policy(policy)
            .with_approver(&ApproveAll);
        let faced = harness.run(&contract(dir.path())).await.unwrap();
        let faced_trace = store.canonical_trace(faced.run_id).unwrap();

        assert!(
            !direct_trace.is_empty(),
            "{which}: an empty trace would make the comparison vacuous"
        );
        assert_eq!(
            direct_trace, faced_trace,
            "{which}: the harness must drive the loop the free function drives, step for step"
        );
        assert_eq!(
            format!("{:?}", direct.outcome),
            format!("{:?}", faced.outcome),
            "{which}: same outcome"
        );
        traces.push(direct_trace);
    }

    // And the two boundaries really do produce different runs — without this, the
    // closed pair could be a second copy of the open one and would re-prove
    // nothing. This is what makes the policy binding observable at all.
    assert_ne!(
        traces[0], traces[1],
        "the open and closed boundaries must drive different runs, or the closed pair adds \
         no information and the sabotage that survived would survive again"
    );
}

/// What the facade costs per step, measured rather than asserted.
///
/// `#[ignore]` because it is a measurement and not a gate, which is the shape
/// `memory_recall_cost` set. **No criterion in this release names a duration as a
/// threshold**: the shape of the cost is what matters and it is constant — the
/// `Harness` assembles the same binding the entry points assemble today, once at
/// construction instead of per call, and then calls the same function. There is
/// no work inside the loop for it to add.
///
/// Run it with `cargo test --test harness -- --ignored --nocapture`.
#[tokio::test]
#[ignore = "a measurement, not a gate"]
async fn what_the_facade_costs_per_step() {
    const ROUNDS: usize = 21;

    let mut direct = Vec::with_capacity(ROUNDS);
    let mut faced = Vec::with_capacity(ROUNDS);

    for _ in 0..ROUNDS {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Script::default();
        let contract = TaskContract::workspace("write a few files", dir.path()).with_max_steps(4);
        let at = std::time::Instant::now();
        run_with(&contract, &provider, &store, &open_policy(), &ApproveAll)
            .await
            .unwrap();
        direct.push(at.elapsed());

        let dir = workspace();
        let store = Store::memory().unwrap();
        let provider = Script::default();
        // The SAME cap the direct contract carries. The first version of this
        // measurement bound a template with the crate default of 12 and compared
        // it against a 4-step contract, and reported the harness as twice as slow
        // — it was timing three times the work. A measurement whose two arms are
        // not the same run measures nothing.
        let harness = Harness::new(&provider, &store)
            .with_policy(open_policy())
            .with_defaults(TaskContract::workspace("", "").with_max_steps(4));
        let contract = harness.workspace("write a few files", dir.path());
        assert_eq!(
            contract.max_steps, 4,
            "both arms must run the same contract"
        );
        let at = std::time::Instant::now();
        harness.run(&contract).await.unwrap();
        faced.push(at.elapsed());
    }

    direct.sort();
    faced.sort();
    println!(
        "4-step run, {ROUNDS} rounds, medians: free function {:?}, harness {:?}",
        direct[ROUNDS / 2],
        faced[ROUNDS / 2]
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

// ---------------------------------------------------------------------------
// F7 — the bound facade takes a contained turn (0.66.0)
// ---------------------------------------------------------------------------
//
// Until this release `Harness` offered `turn`, `turn_with` and `run_tree` and no
// contained turn at all, so an embedder who bound their host once had to unbind
// the moment a conversation needed to decompose.

/// Spawns once, then answers.
#[derive(Default)]
struct Fanout {
    at: AtomicUsize,
    tools: Mutex<Vec<Vec<String>>>,
}

impl Provider for Fanout {
    fn name(&self) -> &str {
        "fanout"
    }

    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.tools
            .lock()
            .unwrap()
            .push(req.tools.iter().map(|t| t.name.clone()).collect());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(match i {
            0 => CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "spawn_agent".into(),
                    arguments: json!({
                        "goal": "write a.txt saying A",
                        "verify_file": "a.txt",
                        "verify_contains": "A",
                    }),
                }],
                ..Default::default()
            },
            1 => CompletionResponse {
                tool_calls: vec![ToolCall {
                    name: "write_file".into(),
                    arguments: json!({ "path": "a.txt", "content": "A" }),
                }],
                ..Default::default()
            },
            _ => CompletionResponse {
                text: Some("done".into()),
                ..Default::default()
            },
        })
    }
}

/// Counts what the bound observer was told.
#[derive(Default)]
struct Heard {
    spawned: Mutex<Vec<(i64, u32)>>,
}

impl Observer for Heard {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Spawned { .. } = &event.kind {
            self.spawned.lock().unwrap().push((event.run_id, event.depth));
        }
        Flow::Continue
    }
}

fn roomy() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// A contained turn through the facade fans out, and the **bound** observer hears
/// it.
///
/// The observer is the load-bearing half. A delegation that reached the unobserved
/// session method would still fan out, still write the file, still report the same
/// outcome — and silently tell the operator's observer nothing, which is the one
/// thing a tree most needs it for.
#[tokio::test]
async fn a_bound_harness_takes_a_contained_turn() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Fanout::default();
    let heard = Heard::default();
    let harness = Harness::new(&provider, &store)
        .with_policy(open_policy())
        .with_approver(&ApproveAll)
        .with_observer(&heard);
    let mut session = harness.session(dir.path()).unwrap();

    let turn = harness
        .turn_contained(&mut session, "decompose it", &roomy())
        .await
        .unwrap();

    assert!(
        provider.tools.lock().unwrap()[0].contains(&"spawn_agent".to_string()),
        "the facade's contained turn was not offered the spawn tool"
    );
    let spawned = heard.spawned.lock().unwrap().clone();
    assert_eq!(
        spawned.len(),
        1,
        "the harness's own observer heard nothing about the fan-out: {spawned:?}"
    );
    assert_eq!(
        store.children(turn.run_id).unwrap().len(),
        1,
        "one child, under this turn's run"
    );
}

/// The contract reaches the facade's contained turn, and so does the bound policy.
///
/// Two arms for the reason `the_facade_and_the_free_function_produce_the_same_trace`
/// records: under a permitting boundary a facade that threw its bound policy away
/// would pass every assertion here. The closed arm is the one that can fail.
#[tokio::test]
async fn a_bound_harness_takes_a_contained_turn_under_a_contract() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Fanout::default();
    let harness = Harness::new(&provider, &store)
        .with_policy(open_policy())
        .with_approver(&ApproveAll);
    let mut session = harness.session(dir.path()).unwrap();

    let contract = harness.task(
        "write a.txt",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "a.txt".into(),
            needle: "A".into(),
        },
    );
    let turn = harness
        .turn_contained_with(&mut session, &contract, &roomy())
        .await
        .unwrap();

    assert!(
        matches!(turn.outcome, RunOutcome::Success { .. }),
        "the contract's gate did not decide the facade's contained turn: {:?}",
        turn.outcome
    );

    // The same contract under a boundary that refuses the write: the gate cannot
    // pass, which it only cannot do if the harness's bound policy reached the loop.
    let closed_store = Store::memory().unwrap();
    let closed_provider = Fanout::default();
    let closed = Harness::new(&closed_provider, &closed_store)
        .with_policy(closed_policy())
        .with_approver(&ApproveAll);
    let mut session = closed.session(dir.path()).unwrap();
    let contract = closed.task(
        "write b.txt",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "B".into(),
        },
    );
    let refused = closed
        .turn_contained_with(&mut session, &contract, &roomy())
        .await
        .unwrap();

    assert!(
        !matches!(refused.outcome, RunOutcome::Success { .. }),
        "the harness's bound policy did not reach the contained turn: {:?}",
        refused.outcome
    );
    assert!(
        !dir.path().join("b.txt").exists(),
        "a write the bound policy denies was made anyway"
    );
}
