//! 0.7.0: durable checkpoint + whole-tree crash-resume, proven end to end with
//! deterministic offline providers. A "crash" is modelled two ways, both honest:
//! a REAL SIGKILL of a child process running the `crash_fixture` binary (the
//! headline `crash_resume` test), and, for the in-process tests, dropping the
//! run future mid-flight (`tokio::time::timeout`) and reopening the `Store` — a
//! new connection with a fresh in-memory ledger and step counters, exactly what
//! a restarted process sees. Nothing here touches the network.

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, Usage};
use io_harness::{
    resume, resume_tree, resume_tree_with_decision, run, run_tree, ApproveAll, Containment, Policy,
    Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------- providers ----------

/// Single-file provider: writes the scripted (content, tokens) for each step, in
/// order. Past the end it writes a fixed non-satisfying string, so a run only
/// ever succeeds on a step the script explicitly finishes.
struct Script {
    writes: Vec<(String, u64)>,
    at: AtomicUsize,
}
impl Script {
    fn new(writes: Vec<(&str, u64)>) -> Self {
        Self {
            writes: writes
                .into_iter()
                .map(|(c, t)| (c.to_string(), t))
                .collect(),
            at: AtomicUsize::new(0),
        }
    }
}
impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        let (content, tokens) = self
            .writes
            .get(i)
            .cloned()
            .unwrap_or(("WORKING\n".into(), 1));
        Ok(CompletionResponse {
            tool_calls: vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "content": content }),
            }],
            usage: Some(Usage {
                total_tokens: tokens,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}

/// A stateless tree provider that decides purely from the prompt, so it is
/// replay- and resume-safe (a crashed step that re-runs behaves identically):
/// a COORDINATOR agent spawns two children; a `FILE=x CONTENT=y` child writes x.
/// `child_delay` lets a test hold children mid-flight so a crash lands during
/// fan-out.
struct TreeProvider {
    child_delay: Duration,
}
impl Provider for TreeProvider {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("COORDINATOR") {
            return Ok(CompletionResponse {
                tool_calls: vec![
                    spawn("FILE=a.txt CONTENT=ALPHA", "a.txt", "ALPHA"),
                    spawn("FILE=b.txt CONTENT=BETA", "b.txt", "BETA"),
                ],
                ..Default::default()
            });
        }
        if let Some(idx) = req.user.find("FILE=") {
            if !self.child_delay.is_zero() {
                tokio::time::sleep(self.child_delay).await;
            }
            let rest = &req.user[idx + 5..];
            let file = rest
                .split_whitespace()
                .next()
                .unwrap_or("x.txt")
                .to_string();
            let content = rest
                .find("CONTENT=")
                .map(|c| {
                    rest[c + 8..]
                        .split_whitespace()
                        .next()
                        .unwrap_or("")
                        .to_string()
                })
                .unwrap_or_default();
            return Ok(CompletionResponse {
                tool_calls: vec![call(
                    "write_file",
                    json!({ "path": file, "content": content }),
                )],
                ..Default::default()
            });
        }
        Ok(CompletionResponse::default())
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

/// Approver that always defers (for the pause-across-restart test).
struct Defer;
impl Approver for Defer {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Defer })
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}
fn tree_contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace(
        "COORDINATOR: delegate to sub-agents; do not write files yourself.",
        root,
        Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        },
    )
}
fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// Locate the compiled `crash_fixture` example next to this test binary, for the
/// real-SIGKILL test. Standard cargo layout: target/<profile>/deps/<test> and
/// target/<profile>/examples/<name>.
fn crash_fixture_bin() -> PathBuf {
    let me = std::env::current_exe().unwrap();
    let profile_dir = me.parent().unwrap().parent().unwrap();
    let mut p = profile_dir.join("examples").join("crash_fixture");
    if cfg!(windows) {
        p.set_extension("exe");
    }
    p
}

// ---------- F1 / F3: a real SIGKILL, then resume ----------

#[tokio::test]
async fn crash_resume_after_a_real_sigkill_reaches_verified_success() {
    let bin = crash_fixture_bin();
    if !bin.exists() {
        // The example must be built for this test. `cargo test` builds examples,
        // but guard so a bare `cargo test --test checkpoint` fails loudly, not weirdly.
        panic!(
            "crash_fixture example not built at {bin:?} — run `cargo test` (which builds examples)"
        );
    }
    let dir = ws();
    let db = dir.path().join("runs.db");
    let file = dir.path().join("out.txt");

    let mut child = tokio::process::Command::new(&bin)
        .arg(&db)
        .arg(&file)
        .spawn()
        .expect("spawn crash_fixture");

    // Wait until it has durably committed a few steps, then SIGKILL it mid-run.
    let mut committed = 0u32;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(store) = Store::open(&db) {
            if let Ok(rows) = store.tree_run_ids(1) {
                if !rows.is_empty() {
                    committed = store.last_step(1).unwrap_or(0);
                    if committed >= 3 {
                        break;
                    }
                }
            }
        }
    }
    assert!(
        committed >= 3,
        "fixture did not commit steps before kill (got {committed})"
    );
    child.kill().await.expect("SIGKILL the fixture"); // tokio Child::kill is SIGKILL on unix

    // A fresh process (this one) resumes the same store to a verified result.
    let store = Store::open(&db).unwrap();
    let before = store.last_step(1).unwrap();
    let contract = TaskContract::new(
        "write SOLUTION-DONE",
        &file,
        Verification::FileContains("SOLUTION-DONE".into()),
    )
    .with_max_steps(1000);
    let finisher = Script::new(vec![("SOLUTION-DONE\n", 10)]);
    let r = resume(&contract, &finisher, &store, 1).await.unwrap();

    assert!(matches!(r.outcome, RunOutcome::Success { steps } if steps > before));
    // The edit is present exactly once (write overwrites; replay cannot double it).
    let out = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        out.matches("SOLUTION-DONE").count(),
        1,
        "edit applied exactly once"
    );
    // Committed steps were not re-run: the trace has no duplicate step numbers.
    let steps: Vec<u32> = store.steps(1).unwrap().iter().map(|s| s.step).collect();
    let mut sorted = steps.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        steps.len(),
        "no committed step was re-run on resume"
    );
}

// ---------- F2 / NF6: budget is continuous across a crash ----------

#[tokio::test]
async fn no_double_charge_across_a_crash_resume() {
    let file = ws().path().join("out.txt");
    // Uninterrupted baseline: two "wrong" steps then a finishing step, 10 tok each.
    let base_store = Store::memory().unwrap();
    let base = TaskContract::new("finish", &file, Verification::FileContains("DONE".into()))
        .with_max_steps(5);
    let baseline = run(
        &base,
        &Script::new(vec![("W\n", 10), ("W\n", 10), ("DONE\n", 10)]),
        &base_store,
    )
    .await
    .unwrap();
    let baseline_spent = base_store.spent_tokens(baseline.run_id).unwrap();
    assert_eq!(baseline_spent, 30);

    // Interrupted: crash (step cap) at step 2, then resume with a finisher.
    let store = Store::memory().unwrap();
    let capped = TaskContract::new("finish", &file, Verification::FileContains("DONE".into()))
        .with_max_steps(2);
    let crashed = run(
        &capped,
        &Script::new(vec![("W\n", 10), ("W\n", 10)]),
        &store,
    )
    .await
    .unwrap();
    assert!(matches!(crashed.outcome, RunOutcome::StepCapReached { .. }));
    assert_eq!(
        store.spent_tokens(crashed.run_id).unwrap(),
        20,
        "durable spend after crash"
    );

    let resumed = resume(
        &TaskContract::new("finish", &file, Verification::FileContains("DONE".into()))
            .with_max_steps(5),
        &Script::new(vec![("DONE\n", 10)]),
        &store,
        crashed.run_id,
    )
    .await
    .unwrap();
    assert!(matches!(resumed.outcome, RunOutcome::Success { .. }));

    // The total spend equals the uninterrupted run: no step charged twice, no reset.
    assert_eq!(store.spent_tokens(crashed.run_id).unwrap(), baseline_spent);
}

#[tokio::test]
async fn the_time_budget_is_wall_clock_and_durable_across_a_restart() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let file = dir.path().join("out.txt");
    let store = Store::open(&db).unwrap();
    let run_id = {
        // A run row exists with a start stamp.
        let c = TaskContract::new("x", &file, Verification::FileContains("DONE".into()))
            .with_max_steps(1);
        run(&c, &Script::new(vec![("W\n", 1)]), &store)
            .await
            .unwrap()
            .run_id
    };
    let before = store.elapsed_secs(run_id).unwrap();
    drop(store);
    tokio::time::sleep(Duration::from_millis(120)).await;
    // A restarted process sees MORE elapsed time, not a reset to zero.
    let reopened = Store::open(&db).unwrap();
    let after = reopened.elapsed_secs(run_id).unwrap();
    assert!(
        after >= before + 0.1,
        "elapsed must count wall-clock across a restart ({before} -> {after})"
    );
}

// ---------- F3 (in-process) / NF2: idempotent replay ----------

#[tokio::test]
async fn edit_applied_exactly_once_and_resume_is_idempotent() {
    let file = ws().path().join("out.txt");
    let store = Store::memory().unwrap();
    let contract = TaskContract::new(
        "write",
        &file,
        Verification::FileEquals("SOLUTION\n".into()),
    );
    let first = run(&contract, &Script::new(vec![("SOLUTION\n", 10)]), &store)
        .await
        .unwrap();
    assert!(matches!(first.outcome, RunOutcome::Success { .. }));
    let spent = store.spent_tokens(first.run_id).unwrap();
    let steps = store.steps(first.run_id).unwrap().len();

    // Re-running resume twice on a finished run is a faithful no-op: same outcome,
    // no extra steps, no extra spend, and the file unchanged (edit exactly once).
    for _ in 0..2 {
        let again = resume(
            &contract,
            &Script::new(vec![("SOLUTION\n", 10)]),
            &store,
            first.run_id,
        )
        .await
        .unwrap();
        assert_eq!(again.outcome, first.outcome);
        assert_eq!(store.steps(first.run_id).unwrap().len(), steps);
        assert_eq!(store.spent_tokens(first.run_id).unwrap(), spent);
    }
    assert_eq!(std::fs::read_to_string(&file).unwrap(), "SOLUTION\n");
}

// ---------- F8: corrupt / unresumable checkpoint is a typed error ----------

#[tokio::test]
async fn resuming_an_unknown_run_returns_a_typed_error_not_a_panic() {
    let store = Store::memory().unwrap();
    let file = ws().path().join("out.txt");
    let contract = TaskContract::new("x", &file, Verification::FileContains("DONE".into()));
    let err = resume(&contract, &Script::new(vec![("DONE\n", 1)]), &store, 424242).await;
    assert!(
        matches!(err, Err(io_harness::Error::Resume { .. })),
        "got {err:?}"
    );
}

// ---------- F9: checkpoint / resume / skipped events reconstruct history ----------

#[tokio::test]
async fn a_multi_crash_run_history_is_reconstructable_from_the_store() {
    let file = ws().path().join("out.txt");
    let store = Store::memory().unwrap();
    // Crash (step cap) at 2, resume and crash again at 3, then resume to success.
    let c2 =
        TaskContract::new("f", &file, Verification::FileContains("DONE".into())).with_max_steps(2);
    let r = run(&c2, &Script::new(vec![("W\n", 1), ("W\n", 1)]), &store)
        .await
        .unwrap();
    let id = r.run_id;
    let c3 =
        TaskContract::new("f", &file, Verification::FileContains("DONE".into())).with_max_steps(3);
    resume(&c3, &Script::new(vec![("W\n", 1)]), &store, id)
        .await
        .unwrap();
    let c5 =
        TaskContract::new("f", &file, Verification::FileContains("DONE".into())).with_max_steps(5);
    let done = resume(&c5, &Script::new(vec![("DONE\n", 1)]), &store, id)
        .await
        .unwrap();
    assert!(matches!(done.outcome, RunOutcome::Success { .. }));

    let events = store.checkpoint_events(id).unwrap();
    let kinds: Vec<&str> = events.iter().map(|e| e.kind.as_str()).collect();
    assert!(kinds.contains(&"checkpoint"), "steps are checkpointed");
    assert_eq!(
        kinds.iter().filter(|k| **k == "resume").count(),
        2,
        "two resumes recorded"
    );
    assert!(
        kinds.contains(&"skipped"),
        "already-committed steps are recorded as skipped"
    );
    // Every committed step is checkpointed exactly once.
    let checkpoints = kinds.iter().filter(|k| **k == "checkpoint").count();
    assert_eq!(checkpoints, store.steps(id).unwrap().len());
}

// ---------- F7: a long unattended run stands in for the 24h horizon ----------

#[tokio::test]
async fn a_long_unattended_run_sustains_the_loop_and_checkpoints_throughout() {
    // 250 steps, no user input, checkpointing every step, stopping only on
    // completion — the accelerated stand-in for a 24h+ unattended run.
    const N: usize = 250;
    let file = ws().path().join("out.txt");
    let store = Store::memory().unwrap();
    let mut writes: Vec<(&str, u64)> = vec![("W\n", 1); N - 1];
    writes.push(("DONE\n", 1));
    let contract = TaskContract::new("endure", &file, Verification::FileContains("DONE".into()))
        .with_max_steps(N as u32);
    let r = run(&contract, &Script::new(writes), &store).await.unwrap();
    assert_eq!(r.outcome, RunOutcome::Success { steps: N as u32 });
    let checkpoints = store
        .checkpoint_events(r.run_id)
        .unwrap()
        .iter()
        .filter(|e| e.kind == "checkpoint")
        .count();
    assert_eq!(
        checkpoints, N,
        "every one of the {N} steps was checkpointed"
    );
}

// ---------- F4: a tree crashed mid-fan-out resumes every agent ----------

#[tokio::test]
async fn a_tree_crash_resumes_every_agent_from_its_checkpoint() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let contract = tree_contract(dir.path());

    // Crash: drop the run mid-fan-out while children are still sleeping.
    let slow = TreeProvider {
        child_delay: Duration::from_millis(500),
    };
    let crashed = tokio::time::timeout(
        Duration::from_millis(150),
        run_tree(
            &contract,
            &slow,
            &store,
            &Policy::permissive(),
            &ApproveAll,
            &containment(),
        ),
    )
    .await;
    assert!(
        crashed.is_err(),
        "the run should have been cut off mid-fan-out"
    );
    // Children were spawned (rows + spawn records) but nothing finished.
    let agents_before = store.agent_count_tree(1).unwrap();
    assert!(
        agents_before >= 2,
        "children were spawned before the crash (got {agents_before})"
    );
    drop(store);

    // Restart: a fresh Store, fresh ledger. Resume the tree to completion.
    let store = Store::open(&db).unwrap();
    let fast = TreeProvider {
        child_delay: Duration::ZERO,
    };
    let r = resume_tree(
        &contract,
        &fast,
        &store,
        1,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        matches!(r.outcome, RunOutcome::Success { .. }),
        "tree completed on resume: {:?}",
        r.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt"))
            .unwrap()
            .trim(),
        "ALPHA"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt"))
            .unwrap()
            .trim(),
        "BETA"
    );
    // Adopted, not duplicated: the same children were resumed, no new agents spawned.
    assert_eq!(
        store.agent_count_tree(1).unwrap(),
        agents_before,
        "children adopted, not re-spawned"
    );
}

// ---------- F5: a tree approval survives a full process restart ----------

#[tokio::test]
async fn a_tree_approval_survives_a_full_restart() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let contract = tree_contract(dir.path());
    // Writing a.txt asks; writing b.txt is allowed outright.
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("b.txt")
        .ask_write("a.txt");

    let fast = TreeProvider {
        child_delay: Duration::ZERO,
    };
    let paused = run_tree(&contract, &fast, &store, &policy, &Defer, &containment())
        .await
        .unwrap();
    let request_id = match paused.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected the tree to pause, got {other:?}"),
    };
    // a.txt was not written while paused.
    assert!(!dir.path().join("a.txt").exists());
    drop(store);

    // A different process delivers the decision and resumes the whole tree.
    let store = Store::open(&db).unwrap();
    let r = resume_tree_with_decision(
        &contract,
        &fast,
        &store,
        1,
        request_id,
        Decision::Approve {
            modified: None,
            remember: vec![],
        },
        &policy,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        matches!(r.outcome, RunOutcome::Success { .. }),
        "tree resumed to success: {:?}",
        r.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt"))
            .unwrap()
            .trim(),
        "ALPHA"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt"))
            .unwrap()
            .trim(),
        "BETA"
    );
}

// ---------- F6: an in-flight sandboxed exec is re-created on resume ----------

#[tokio::test]
async fn a_sandboxed_verification_is_recreated_on_resume() {
    // The execution-based verify compiles model output inside an ephemeral 0.6.0
    // sandbox. A run that crashes (step cap) before it compiles resumes and the
    // verify runs again in a fresh sandbox, reaching success — the committed
    // steps are not re-run.
    let file = ws().path().join("lib.rs");
    let store = Store::memory().unwrap();
    let capped =
        TaskContract::new("make it compile", &file, Verification::CompilesRust).with_max_steps(1);
    let broken = run(&capped, &Script::new(vec![("fn main( {\n", 5)]), &store)
        .await
        .unwrap();
    assert!(matches!(broken.outcome, RunOutcome::StepCapReached { .. }));
    let before = store.last_step(broken.run_id).unwrap();

    let contract =
        TaskContract::new("make it compile", &file, Verification::CompilesRust).with_max_steps(4);
    let r = resume(
        &contract,
        &Script::new(vec![("pub fn f() -> u32 { 42 }\n", 5)]),
        &store,
        broken.run_id,
    )
    .await
    .unwrap();
    assert!(
        matches!(r.outcome, RunOutcome::Success { steps } if steps > before),
        "compiled on resume: {:?}",
        r.outcome
    );
}
