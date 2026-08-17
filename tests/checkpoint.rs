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
    resume, resume_tree, resume_tree_with_decision, run, run_tree, run_with, ApproveAll,
    Containment, Policy, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

/// The compile gate these tests use, now that the Rust-specific criteria are
/// gone: `rustc` invoked by argv, in the edited file's own directory, exactly as
/// a caller following the 0.17.0 migration note would write it.
fn compiles(file: &str) -> Verification {
    Verification::Command {
        argv: vec![
            "rustc".into(),
            "--edition".into(),
            "2021".into(),
            "--crate-type".into(),
            "lib".into(),
            file.into(),
        ],
        expect_exit: 0,
    }
}

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

/// A workspace provider that writes one fixed (path, content) on every turn.
///
/// Stateless, so a replayed or resumed step behaves identically. Pointed at a
/// contract whose verification the content does NOT satisfy it produces a run
/// that reaches its step cap with real committed state — the interrupted run the
/// back-compatibility tests below resume from; pointed at content that does
/// satisfy it, it finishes one.
struct WriteOnce {
    path: &'static str,
    content: &'static str,
}
impl Provider for WriteOnce {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        Ok(CompletionResponse {
            tool_calls: vec![call(
                "write_file",
                json!({ "path": self.path, "content": self.content }),
            )],
            ..Default::default()
        })
    }
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
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "b.txt".into(),
        needle: "BETA".into(),
    })
}
fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

/// A workspace contract over `root` satisfied by `needle` landing in `out.txt`.
/// Workspace mode on purpose: the observation ledger and the policy gate only
/// exist on that path, so a single-file contract would prove nothing about
/// either.
fn out_contract(root: &std::path::Path, needle: &str, max_steps: u32) -> TaskContract {
    TaskContract::workspace("write out.txt", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: needle.into(),
        })
        .with_max_steps(max_steps)
}

/// The store as a plain SQLite file, for reaching past the public API to make a
/// 0.13.0 database look like a 0.12.0 one. `rusqlite` is the crate's own storage
/// dependency and other tests in this suite (`store_open`, `outcome_record`)
/// already open the file this way.
fn sqlite(db: &std::path::Path) -> rusqlite::Connection {
    rusqlite::Connection::open(db).expect("open the store as a plain SQLite file")
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

// ---------- 0.13.0: the back-compatibility promises, proven not asserted ----------

/// The checkpoint format is still 7, and that is a promise, not an accident.
///
/// [`Store::check_resumable`] refuses any store whose `PRAGMA user_version` is
/// *higher* than this constant, so a bump makes every 0.12.0 database written by
/// an older binary unresumable by the newer one — an unattended run that crashed
/// before the upgrade could never be finished. 0.13.0 added `run_policies` and
/// `ledger_observations` and changed nothing about the layout of a checkpoint:
/// both are additive tables an older binary never queries. If this assertion
/// ever fails, the release notes have to say so and a migration has to exist.
#[test]
fn the_checkpoint_format_is_still_7_because_0_13_0_only_added_tables() {
    assert_eq!(
        io_harness::CHECKPOINT_FORMAT,
        7,
        "0.13.0's two new tables are additive; bumping the format would make \
         check_resumable refuse every 0.12.0 store"
    );
}

/// A store written by 0.12.0 — one with neither 0.13.0 table in it — opens and
/// resumes to a verified result.
///
/// The 0.12.0 shape is produced honestly: let `Store::open` build the schema,
/// then `DROP` the two tables 0.13.0 introduced, which is byte-for-byte what an
/// older binary leaves on disk. Reopening recreates them empty (`CREATE TABLE IF
/// NOT EXISTS` is the whole migration) and the interrupted run continues from its
/// last committed step. If this regressed, every checkpoint written before the
/// upgrade would be dead on arrival.
#[tokio::test]
async fn a_store_written_without_the_0_13_0_tables_opens_and_resumes() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let crashed = run(
        &out_contract(dir.path(), "DONE", 1),
        &WriteOnce {
            path: "out.txt",
            content: "WORKING\n",
        },
        &store,
    )
    .await
    .unwrap();
    assert!(
        matches!(crashed.outcome, RunOutcome::StepCapReached { .. }),
        "an interrupted run to resume: {:?}",
        crashed.outcome
    );
    let id = crashed.run_id;
    let before = store.last_step(id).unwrap();
    drop(store);

    // Rewind the file to 0.12.0: the two tables simply are not there.
    let c = sqlite(&db);
    c.execute_batch("DROP TABLE run_policies; DROP TABLE ledger_observations;")
        .unwrap();
    for table in ["run_policies", "ledger_observations"] {
        assert!(
            c.query_row(&format!("SELECT 1 FROM {table}"), [], |_| Ok(()))
                .is_err(),
            "{table} is really gone before the reopen"
        );
    }
    drop(c);

    // A 0.13.0 binary opens that file: the tables come back empty, and the run
    // recorded in it resumes and completes.
    let store = Store::open(&db).unwrap();
    assert!(
        store.run_policy(id).unwrap().is_none(),
        "a 0.12.0 store records no policy for its runs"
    );
    assert!(
        store.observations(id).unwrap().is_empty(),
        "a 0.12.0 store has no durable ledger"
    );
    let r = resume(
        &out_contract(dir.path(), "DONE", 5),
        &WriteOnce {
            path: "out.txt",
            content: "DONE\n",
        },
        &store,
        id,
    )
    .await
    .unwrap();
    assert!(
        matches!(r.outcome, RunOutcome::Success { steps } if steps > before),
        "the 0.12.0 checkpoint resumed to success: {:?}",
        r.outcome
    );
}

/// A run with no recorded policy resumes through the bare [`resume`], because
/// absence is not a policy.
///
/// Every run started by 0.12.0 or earlier has no `run_policies` row. The 0.13.0
/// gate refuses a run whose *recorded* policy is not permissive; a missing row is
/// deliberately a third case, resumed exactly as 0.7.0's contract promised. Read
/// as "the caller chose permissive" it would be the same behaviour by the wrong
/// reasoning — and read as "unknown, therefore refuse" it would strand every
/// pre-0.13.0 checkpoint.
#[tokio::test]
async fn a_run_with_no_recorded_policy_resumes_through_the_bare_resume() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let crashed = run(
        &out_contract(dir.path(), "DONE", 1),
        &WriteOnce {
            path: "out.txt",
            content: "WORKING\n",
        },
        &store,
    )
    .await
    .unwrap();
    let id = crashed.run_id;
    assert!(
        store.run_policy(id).unwrap().is_some(),
        "0.13.0 does record one, so deleting it below is a real difference"
    );
    drop(store);

    // The run as 0.12.0 would have left it: a run row, no policy row.
    sqlite(&db)
        .execute("DELETE FROM run_policies WHERE run_id = ?1", [id])
        .unwrap();

    let store = Store::open(&db).unwrap();
    assert!(store.run_policy(id).unwrap().is_none());
    let r = resume(
        &out_contract(dir.path(), "DONE", 5),
        &WriteOnce {
            path: "out.txt",
            content: "DONE\n",
        },
        &store,
        id,
    )
    .await
    .unwrap();
    assert!(
        matches!(r.outcome, RunOutcome::Success { .. }),
        "a policy-less run is driven, not refused: {:?}",
        r.outcome
    );
}

/// A run with no durable observations resumes and re-derives its context.
///
/// 0.13.0 restores the observation ledger from `ledger_observations` at the top
/// of the resumed loop. A pre-0.13.0 checkpoint has no rows there, so the restore
/// yields an empty ledger — which must mean "re-derive from the workspace, as
/// 0.12.0 always did", not an error and not a run that assembles context from
/// nothing and stalls.
#[tokio::test]
async fn a_run_with_no_durable_observations_resumes_and_re_derives() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let crashed = run(
        &out_contract(dir.path(), "DONE", 1),
        &WriteOnce {
            path: "out.txt",
            content: "WORKING\n",
        },
        &store,
    )
    .await
    .unwrap();
    let id = crashed.run_id;
    assert!(
        !store.observations(id).unwrap().is_empty(),
        "0.13.0 does write a ledger, so emptying it below is a real difference"
    );
    drop(store);

    // The run as 0.12.0 would have left it: committed steps, no ledger.
    sqlite(&db)
        .execute("DELETE FROM ledger_observations WHERE run_id = ?1", [id])
        .unwrap();

    let store = Store::open(&db).unwrap();
    assert!(store.observations(id).unwrap().is_empty());
    let r = resume(
        &out_contract(dir.path(), "DONE", 5),
        &WriteOnce {
            path: "out.txt",
            content: "DONE\n",
        },
        &store,
        id,
    )
    .await
    .unwrap();
    assert!(
        matches!(r.outcome, RunOutcome::Success { .. }),
        "an empty restore re-derives rather than failing: {:?}",
        r.outcome
    );
    assert!(
        !store.observations(id).unwrap().is_empty(),
        "the resumed run wrote its own observations from there on"
    );
}

/// A finished run reports its outcome through the bare [`resume`] even though it
/// was started under a policy.
///
/// The policy gate sits *after* the finished-run short-circuit, and that ordering
/// is the whole contract: a run that is already over drives no loop, asks no
/// provider and performs no action, so there is no boundary to drop by reading
/// it. Gating it first would turn every terminal outcome — the refused and
/// escalated ones `tests/net.rs` depends on reporting — into an error for any
/// run that had a policy, which is most of them.
#[tokio::test]
async fn a_finished_policy_bearing_run_still_reports_its_outcome_through_the_bare_resume() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*");
    assert!(
        !policy.is_permissive(),
        "the gate only has something to refuse if the policy is a real boundary"
    );
    let contract = out_contract(dir.path(), "DONE", 5);
    let finisher = || WriteOnce {
        path: "out.txt",
        content: "DONE\n",
    };
    let done = run_with(&contract, &finisher(), &store, &policy, &ApproveAll)
        .await
        .unwrap();
    assert!(
        matches!(done.outcome, RunOutcome::Success { .. }),
        "{:?}",
        done.outcome
    );
    let recorded = store
        .run_policy(done.run_id)
        .unwrap()
        .expect("the run carries a recorded policy");
    assert!(
        !recorded.is_permissive(),
        "the recorded policy is the kind the gate refuses, so the short-circuit \
         below is what lets this through — not a permissive row"
    );

    let again = resume(&contract, &finisher(), &store, done.run_id)
        .await
        .unwrap();
    assert_eq!(
        again.outcome, done.outcome,
        "a finished run is reported, not refused and not re-driven"
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
    let capped = TaskContract::new("make it compile", &file, compiles("lib.rs")).with_max_steps(1);
    let broken = run(&capped, &Script::new(vec![("fn main( {\n", 5)]), &store)
        .await
        .unwrap();
    assert!(matches!(broken.outcome, RunOutcome::StepCapReached { .. }));
    let before = store.last_step(broken.run_id).unwrap();

    let contract =
        TaskContract::new("make it compile", &file, compiles("lib.rs")).with_max_steps(4);
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

// ---------- 0.16.0 F12/F13: the tree resumes under the policy it started with ----------
//
// 0.13.0 gave the single-file and workspace loops `resume_from_stored_policy` so
// a crashed run could come back under its own boundary without the caller
// reconstructing it. The tree loop never got one, so the three resume paths
// disagreed about whether a restart preserves the boundary. These cover the pair
// that closes that gap.

/// Every policy refusal in the whole tree, as `(target, rule, layer)`.
///
/// Read straight out of `policy_events` rather than through `Store::events`,
/// because a tree's refusals land on the *child* agent's run id and the caller
/// only holds the root's — the one thing a per-run reader cannot answer.
fn tree_refusals(db: &std::path::Path) -> Vec<(String, Option<String>, Option<String>)> {
    let c = sqlite(db);
    let mut stmt = c
        .prepare("SELECT target, rule, layer FROM policy_events WHERE kind = 'refusal' ORDER BY id")
        .unwrap();
    let rows: Vec<_> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    rows
}

/// Start a tree under `policy` and cut it off mid-fan-out — F4's crash, with the
/// policy as a parameter. Leaves the store closed, as a killed process would.
async fn crash_a_tree_mid_fan_out(dir: &std::path::Path, db: &std::path::Path, policy: &Policy) {
    let store = Store::open(db).unwrap();
    let slow = TreeProvider {
        child_delay: Duration::from_millis(500),
    };
    let crashed = tokio::time::timeout(
        Duration::from_millis(150),
        run_tree(
            &tree_contract(dir),
            &slow,
            &store,
            policy,
            &ApproveAll,
            &containment(),
        ),
    )
    .await;
    assert!(
        crashed.is_err(),
        "the run should have been cut off mid-fan-out"
    );
    assert!(
        store.agent_count_tree(1).unwrap() >= 2,
        "children were spawned before the crash, so the resume has a tree to restore"
    );
    assert!(
        store.run_policy(1).unwrap().is_some(),
        "the policy the tree started under is durable — that is what the resume reads"
    );
}

/// F12 — a tree started under a policy denying a path, crashed, and resumed
/// through the new entry point *without a policy argument* still refuses that
/// path, and the refusal is attributable to the original rule and layer.
#[tokio::test]
async fn a_tree_resumed_from_its_stored_policy_is_still_bounded_by_it() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    // The coordinator fans out to a.txt and b.txt; the boundary allows one and
    // denies the other, by an explicit rule so the trace has something to name.
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*")
        .deny_write("a.txt");
    crash_a_tree_mid_fan_out(dir.path(), &db, &policy).await;

    // A different process: the run id and nothing else. No policy passed.
    let store = Store::open(&db).unwrap();
    let r = io_harness::resume_tree_from_stored_policy(
        &tree_contract(dir.path()),
        &TreeProvider {
            child_delay: Duration::ZERO,
        },
        &store,
        1,
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert!(
        matches!(r.outcome, RunOutcome::Success { .. }),
        "the tree was driven, not stopped — the refusal below is a boundary, not a dead run: {:?}",
        r.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("b.txt"))
            .unwrap()
            .trim(),
        "BETA",
        "the allowed half of the fan-out completed"
    );
    assert!(
        !dir.path().join("a.txt").exists(),
        "the denied path stayed denied across the restart"
    );
    let refusals = tree_refusals(&db);
    assert!(
        refusals
            .iter()
            .any(|(target, rule, layer)| target == "a.txt"
                && rule.as_deref() == Some("a.txt")
                && layer.as_deref() == Some("base")),
        "the refusal is in the trace, attributed to the rule and layer of the policy the tree \
         was STARTED under: {refusals:?}"
    );
}

/// F13(a) — the negative control. The same tree, the same call, started
/// permissively: the write happens. Without this, F12 would pass just as well if
/// the resumed tree refused everything.
#[tokio::test]
async fn the_same_tree_resume_on_a_permissive_run_performs_the_write() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    crash_a_tree_mid_fan_out(dir.path(), &db, &Policy::permissive()).await;

    let store = Store::open(&db).unwrap();
    let r = io_harness::resume_tree_from_stored_policy(
        &tree_contract(dir.path()),
        &TreeProvider {
            child_delay: Duration::ZERO,
        },
        &store,
        1,
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
        "ALPHA",
        "the path F12 sees refused is written here, so F12 measures a restored boundary"
    );
    assert!(
        tree_refusals(&db).is_empty(),
        "a permissive tree refuses nothing: {:?}",
        tree_refusals(&db)
    );
}

/// F13(b) — a tree with no recorded policy is a typed error, never a silent
/// [`Policy::permissive`]. An unknown run id has no recorded policy, which is the
/// same state every pre-0.13.0 run row is in.
#[tokio::test]
async fn a_tree_with_no_recorded_policy_is_refused_rather_than_resumed_permissively() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let err = io_harness::resume_tree_from_stored_policy(
        &tree_contract(dir.path()),
        &TreeProvider {
            child_delay: Duration::ZERO,
        },
        &store,
        424_242,
        &ApproveAll,
        &containment(),
    )
    .await
    .expect_err("a tree whose boundary cannot be recovered must not be resumed");
    assert!(matches!(&err, io_harness::Error::Resume { .. }), "{err:?}");
}

// ---------- 0.50.0 F7: a detached child is taken back after a restart ----------

/// A coordinator that detaches one child and then finishes its own work, and a
/// child that can be made slower than the process it is running in.
///
/// Stateless, like [`TreeProvider`]: which reply it gives is decided by what is in
/// the request, so the same provider serves the run and the resume.
struct DetachProvider {
    child_delay: Duration,
    /// Park the coordinator's *second* completion for ever, which is the honest
    /// way to interrupt a run mid-step: the step is left uncommitted and the run
    /// row stays `running`, exactly as a killed process leaves it.
    park_parent: bool,
}
impl Provider for DetachProvider {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("COORDINATOR") {
            // Once it has been told the child is detached, it gets on with its own
            // work — so the spawn happens exactly once however often this is asked.
            if req.user.contains("detached") {
                if self.park_parent {
                    std::future::pending::<()>().await;
                }
                return Ok(CompletionResponse {
                    tool_calls: vec![call(
                        "write_file",
                        json!({ "path": "done.txt", "content": "ok" }),
                    )],
                    ..Default::default()
                });
            }
            return Ok(CompletionResponse {
                tool_calls: vec![call(
                    "spawn_agent",
                    json!({
                        "goal": "FILE=a.txt CONTENT=ALPHA",
                        "verify_file": "a.txt",
                        "verify_contains": "ALPHA",
                        "wait": false
                    }),
                )],
                ..Default::default()
            });
        }
        if req.user.contains("FILE=a.txt") {
            if !self.child_delay.is_zero() {
                tokio::time::sleep(self.child_delay).await;
            }
            return Ok(CompletionResponse {
                text: Some("CHILD-CONCLUSION".into()),
                tool_calls: vec![call(
                    "write_file",
                    json!({ "path": "a.txt", "content": "ALPHA" }),
                )],
                ..Default::default()
            });
        }
        Ok(CompletionResponse::default())
    }
}

/// 0.50.0 — F7: a child its parent stopped waiting for survives a process death.
///
/// A detached child's step **commits**, so the resume starts after it and the spawn
/// call is never replayed. Without the re-adoption walk the child would be left
/// with a `running` run row nobody drives — an orphan, and exactly what 0.48.0's
/// "everything a run starts is inside the boundary" forbids. The child is resumed
/// from its own checkpoint through the ordinary spawn path, so it is the same run
/// row rather than a second child.
#[tokio::test]
async fn a_detached_child_is_readopted_after_a_restart() {
    let dir = ws();
    let db = dir.path().join("runs.db");
    let store = Store::open(&db).unwrap();
    let contract = TaskContract::workspace(
        "COORDINATOR: delegate the file, do not wait, then finish.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "done.txt".into(),
        needle: "ok".into(),
    })
    .with_max_steps(6);

    // Cut the process off while the detached child is still working.
    let slow = DetachProvider {
        child_delay: Duration::from_secs(5),
        park_parent: true,
    };
    let crashed = tokio::time::timeout(
        Duration::from_millis(300),
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
    assert!(crashed.is_err(), "the run was cut off mid-flight");
    let children_before = store.children(1).unwrap();
    assert_eq!(children_before.len(), 1, "one child was detached");
    assert!(
        !dir.path().join("a.txt").exists(),
        "the child had not finished when the process died"
    );
    drop(store);

    // Restart: a fresh Store, a fresh in-memory ledger, nothing in flight.
    let store = Store::open(&db).unwrap();
    let fast = DetachProvider {
        child_delay: Duration::ZERO,
        park_parent: false,
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
        "the tree resumed to success, got {:?}",
        r.outcome
    );

    // The same child, taken back rather than spawned again.
    assert_eq!(
        store.children(1).unwrap(),
        children_before,
        "the detached child was re-adopted, not duplicated"
    );
    let child = children_before[0];
    let summary = store.run_summary(child).unwrap().expect("the child ended");
    assert!(
        summary.success,
        "the re-adopted child finished, got {:?}",
        summary.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "ALPHA",
        "its work reached the workspace"
    );
    // And what it concluded is durable under its parent.
    assert!(
        store
            .observations(1)
            .unwrap()
            .iter()
            .any(|o| o.text.contains("CHILD-CONCLUSION")),
        "the re-adopted child's report reaches its parent"
    );
}

// ---------- 0.64.0: the assistant turn is durable ----------

/// F2 — the turn round-trips through the store without loss, including the two
/// shapes that break a string join.
///
/// `steps.tool_call` writes `name:args` joined with ` | `, so a tool name
/// carrying a `:` and an argument carrying a ` | ` cannot be split back apart
/// from it. That is why this table exists rather than the existing column being
/// reused, and asserting it on those exact shapes is what makes the claim
/// checkable instead of stated.
#[test]
fn an_assistant_turn_round_trips_through_the_store_whole() {
    use io_harness::AssistantTurn;

    let store = Store::memory().unwrap();
    let run_id = store.start_run("goal", "model").unwrap();

    let calls = vec![
        ToolCall {
            name: "ns:read_file".into(),
            arguments: json!({ "path": "a.txt" }),
        },
        ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": "b.txt", "content": "left | right" }),
        },
        ToolCall {
            name: "run_command".into(),
            arguments: json!({ "argv": ["sh", "-c", "echo a | wc -l"], "n": 3, "deep": { "k": [1, 2] } }),
        },
    ];
    store
        .record_step_turn(
            run_id,
            &AssistantTurn::new(1, Some("I will read, then write."), calls.clone()),
        )
        .unwrap();

    let back = store.step_turns(run_id).unwrap();
    assert_eq!(back.len(), 1, "one step, one turn");
    assert_eq!(back[0].step, 1);
    assert_eq!(back[0].text.as_deref(), Some("I will read, then write."));
    assert_eq!(
        back[0].calls.len(),
        3,
        "every call survives, including the two a string join would fuse"
    );
    for (i, (want, got)) in calls.iter().zip(back[0].calls.iter()).enumerate() {
        assert_eq!(&got.name, &want.name, "call {i} keeps its name and its order");
        // Compared as JSON, not as a string: two renderings of one object are
        // equal here and unequal as text, and the stored fact is the object.
        assert_eq!(&got.arguments, &want.arguments, "call {i} keeps its arguments whole");
    }
}

/// F2 — an absent text is not an empty one.
///
/// A model that wrote nothing beside its calls and a model that wrote `""` are
/// different facts. A resumed run that cannot tell them apart emits an assistant
/// turn the live run did not.
#[test]
fn an_absent_assistant_text_is_not_an_empty_one() {
    use io_harness::AssistantTurn;

    let store = Store::memory().unwrap();
    let run_id = store.start_run("goal", "model").unwrap();

    store
        .record_step_turn(run_id, &AssistantTurn::new(1, None::<String>, Vec::new()))
        .unwrap();
    store
        .record_step_turn(run_id, &AssistantTurn::new(2, Some(""), Vec::new()))
        .unwrap();

    let back = store.step_turns(run_id).unwrap();
    assert_eq!(back.len(), 2, "oldest step first");
    assert_eq!(back[0].step, 1);
    assert_eq!(back[1].step, 2);
    assert_eq!(back[0].text, None, "wrote nothing");
    assert_eq!(back[1].text.as_deref(), Some(""), "wrote an empty string");
    assert_ne!(back[0].text, back[1].text, "and the two are not the same row");
}

/// A step committed twice keeps one turn — the last one it actually took.
///
/// A retry after a tool error writes another `steps` row for the same step. The
/// primary key is `(run_id, step)`, so this table replaces rather than growing a
/// second trace of one step.
#[test]
fn a_step_recorded_twice_keeps_the_turn_it_last_took() {
    use io_harness::AssistantTurn;

    let store = Store::memory().unwrap();
    let run_id = store.start_run("goal", "model").unwrap();

    store
        .record_step_turn(run_id, &AssistantTurn::new(1, Some("first"), Vec::new()))
        .unwrap();
    store
        .record_step_turn(
            run_id,
            &AssistantTurn::new(
                1,
                Some("second"),
                vec![ToolCall {
                    name: "read_file".into(),
                    arguments: json!({ "path": "a.txt" }),
                }],
            ),
        )
        .unwrap();

    let back = store.step_turns(run_id).unwrap();
    assert_eq!(back.len(), 1, "one row per step, not two");
    assert_eq!(back[0].text.as_deref(), Some("second"));
    assert_eq!(back[0].calls.len(), 1);
}
