//! Sub-agent trees through the full loop with a scripted mock provider, so the
//! test is deterministic and offline. Proves 0.5.0's core: a parent decomposes a
//! task into contained children over a shared workspace, the children's edits
//! reach disk, and the parent composes their results back.
//!
//! Execution is sequential (a parent fully awaits a child before its next step),
//! so a single call counter in the mock yields the exact parent/child ordering.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    resume_tree_with_decision, run_tree, run_with, ApproveAll, Containment, DenyAll, Policy,
    Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

/// Returns a fixed script of tool-call responses, one per `complete` call, in
/// the order the tree makes them.
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

fn containment() -> Containment {
    Containment::new(10, 4, 3, 1_000_000)
}

#[tokio::test]
async fn a_parent_spawns_three_children_over_a_shared_workspace() {
    let dir = ws();
    let contract = TaskContract::workspace(
        "Split the work across three sub-agents, then combine.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "combined.txt".into(),
        needle: "abc".into(),
    })
    .with_max_steps(6);

    // parent#1 spawn A -> childA writes a.txt; parent#2 spawn B -> childB; parent#3
    // spawn C -> childC; parent#4 writes the combined file from the children's work.
    let script = MockScript::new(vec![
        vec![spawn("write a", "a.txt", "A")],
        vec![write("a.txt", "A")],
        vec![spawn("write b", "b.txt", "B")],
        vec![write("b.txt", "B")],
        vec![spawn("write c", "c.txt", "C")],
        vec![write("c.txt", "C")],
        vec![write("combined.txt", "abc")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 4 });

    // Each child's edit reached the shared workspace.
    for (f, c) in [
        ("a.txt", "A"),
        ("b.txt", "B"),
        ("c.txt", "C"),
        ("combined.txt", "abc"),
    ] {
        assert_eq!(std::fs::read_to_string(dir.path().join(f)).unwrap(), c);
    }

    // The trace holds the parent, all three children, and the parent/child edges.
    let children = store.children(result.run_id).unwrap();
    assert_eq!(children.len(), 3, "three child runs recorded");
    let spawns = store
        .agent_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "spawn")
        .count();
    assert_eq!(spawns, 3, "three spawn edges recorded");
}

#[tokio::test]
async fn spawn_agent_composes_the_childs_result_back() {
    let dir = ws();
    // The parent's success criterion is a value it never writes itself — only a
    // child produces result.txt, so the parent can only succeed via compose-back.
    let contract =
        TaskContract::workspace("Delegate producing the answer to a sub-agent.", dir.path())
            .with_verification(Verification::WorkspaceFileContains {
                file: "result.txt".into(),
                needle: "42".into(),
            });

    let script = MockScript::new(vec![
        vec![spawn("produce the answer", "result.txt", "42")],
        vec![write("result.txt", "42")], // the child
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    assert_eq!(
        std::fs::read_to_string(dir.path().join("result.txt")).unwrap(),
        "42"
    );
    // The value came from a spawned child, not the parent.
    assert_eq!(store.children(result.run_id).unwrap().len(), 1);
}

#[tokio::test]
async fn a_spawn_beyond_the_agent_cap_is_refused_but_the_rest_complete() {
    let dir = ws();
    // Root counts as agent 1, so a cap of 3 admits exactly two children; the
    // third spawn is refused while the first two still finish.
    let cap3 = Containment::new(3, 4, 3, 1_000_000);
    let contract = TaskContract::workspace(
        "Try to spawn three children under an agent cap of three.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "done.txt".into(),
        needle: "ok".into(),
    })
    .with_max_steps(6);

    let script = MockScript::new(vec![
        vec![spawn("write a", "a.txt", "A")],
        vec![write("a.txt", "A")],
        vec![spawn("write b", "b.txt", "B")],
        vec![write("b.txt", "B")],
        vec![spawn("write c", "c.txt", "C")], // refused: agent cap reached, no child runs
        vec![write("done.txt", "ok")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &cap3,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 4 });
    // Only the two in-cap children ran; c.txt was never created.
    assert_eq!(store.children(result.run_id).unwrap().len(), 2);
    assert!(!dir.path().join("c.txt").exists());
    // The refusal is in the trace, attributed to the agent cap.
    let refused: Vec<_> = store
        .agent_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "spawn_refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].detail.as_deref(), Some("agents"));
}

#[tokio::test]
async fn a_spawn_beyond_max_depth_is_refused_and_depth_counts_from_the_root() {
    let dir = ws();
    // max_depth 2: root(0) -> child(1) -> grandchild(2) is allowed; the
    // grandchild's own spawn would be depth 3 and is refused — a chain cannot
    // reset depth by nesting.
    let depth2 = Containment::new(10, 4, 2, 1_000_000);
    let contract = TaskContract::workspace(
        "Nest agents until the depth cap refuses the deepest spawn.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "top.txt".into(),
        needle: "T".into(),
    })
    .with_max_steps(4);

    let script = MockScript::new(vec![
        vec![spawn("child", "child.txt", "C")], // parent(0) -> child(1)
        vec![spawn("grandchild", "gc.txt", "G")], // child(1) -> grandchild(2)
        vec![spawn("great-grandchild", "ggc.txt", "X")], // grandchild(2) -> depth 3: REFUSED
        vec![write("gc.txt", "G")],             // grandchild finishes
        vec![write("child.txt", "C")],          // child finishes
        vec![write("top.txt", "T")],            // parent finishes
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &depth2,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    // Walk the chain to the grandchild and confirm its deepest spawn was refused.
    let child = store.children(result.run_id).unwrap()[0];
    let grandchild = store.children(child).unwrap()[0];
    assert_eq!(store.depth(grandchild).unwrap(), 2);
    assert!(
        store.children(grandchild).unwrap().is_empty(),
        "the refused spawn created no run"
    );
    assert!(!dir.path().join("ggc.txt").exists());
    let refused: Vec<_> = store
        .agent_events(grandchild)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "spawn_refused")
        .collect();
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].detail.as_deref(), Some("depth"));
}

#[tokio::test]
async fn one_approver_serves_the_whole_tree_approve() {
    let dir = ws();
    // Default policy gates every write on approval. A sensitive write inside a
    // child must reach the root's approver; ApproveAll lets it through.
    let contract = TaskContract::workspace("Delegate a sensitive write to a child.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "OK".into(),
        })
        .with_max_steps(2);

    let script = MockScript::new(vec![
        vec![spawn("write out", "out.txt", "OK")],
        vec![write("out.txt", "OK")], // child; write is Ask -> approver
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::default(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
    // The approve decision is recorded against the child's own run.
    let child = store.children(result.run_id).unwrap()[0];
    let approved = store
        .events(child)
        .unwrap()
        .into_iter()
        .filter(|e| e.decision.as_deref() == Some("approve"))
        .count();
    assert!(approved >= 1, "the child's sensitive write was approved");
}

#[tokio::test]
async fn one_approver_serves_the_whole_tree_deny() {
    let dir = ws();
    let contract = TaskContract::workspace(
        "Delegate a sensitive write the approver will refuse.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "out.txt".into(),
        needle: "OK".into(),
    })
    .with_max_steps(2);

    // The child keeps trying to write; DenyAll refuses every time.
    let script = MockScript::new(vec![
        vec![call(
            "spawn_agent",
            json!({ "goal": "write out", "verify_file": "out.txt", "verify_contains": "OK", "max_steps": 2 }),
        )],
        vec![write("out.txt", "OK")],
        vec![write("out.txt", "OK")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::default(),
        &DenyAll,
        &containment(),
    )
    .await
    .unwrap();

    // The denied child never produced the file, so the parent cannot verify.
    assert!(matches!(result.outcome, RunOutcome::StepCapReached { .. }));
    assert!(!dir.path().join("out.txt").exists());
    let child = store.children(result.run_id).unwrap()[0];
    let denied = store
        .events(child)
        .unwrap()
        .into_iter()
        .filter(|e| e.decision.as_deref() == Some("deny"))
        .count();
    assert!(
        denied >= 1,
        "the child's write was denied by the tree's approver"
    );
}

#[tokio::test]
async fn sub_agents_are_opt_in_a_plain_run_never_exposes_spawn() {
    let dir = ws();
    // A 0.4.0-style run_with (no Containment) must not expose spawn_agent: a
    // model that emits it gets an unknown-tool observation, no child is created,
    // and the run still reaches its verified success by normal means.
    let contract = TaskContract::workspace(
        "A plain workspace run that ignores a stray spawn call.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "f.txt".into(),
        needle: "V".into(),
    })
    .with_max_steps(3);

    let script = MockScript::new(vec![
        vec![spawn("should be inert", "f.txt", "V")], // not handled outside a tree
        vec![write("f.txt", "V")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_with(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });
    // No child run and no spawn event — spawning simply does not exist here.
    assert!(store.children(result.run_id).unwrap().is_empty());
    assert!(store.agent_events(result.run_id).unwrap().is_empty());
}

// --- T06: concurrent fan-out to 100+, bounded by max_concurrent. ---

use io_harness::provider::Usage;
use std::sync::atomic::AtomicUsize as Counter;

/// A provider that observes how many completions are in flight at once. The
/// parent (goal "ROOT-FANOUT") is told to spawn `fanout` children in one step;
/// every child does nothing and reaches its step cap. Each call yields once, so
/// concurrent children genuinely overlap and the peak is real.
struct ConcurrencyProbe {
    fanout: usize,
    active: Counter,
    peak: Counter,
}

impl ConcurrencyProbe {
    fn new(fanout: usize) -> Self {
        Self {
            fanout,
            active: Counter::new(0),
            peak: Counter::new(0),
        }
    }
}

impl Provider for ConcurrencyProbe {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::task::yield_now().await; // let siblings overlap
        self.active.fetch_sub(1, Ordering::SeqCst);

        let is_parent = req.user.contains("ROOT-FANOUT");
        let usage = Some(Usage {
            total_tokens: 1,
            ..Default::default()
        });
        if is_parent {
            let calls: Vec<ToolCall> = (0..self.fanout)
                .map(|i| {
                    call(
                        "spawn_agent",
                        json!({
                            "goal": format!("c{i}"),
                            "verify_file": format!("c{i}.txt"),
                            "verify_contains": "never-satisfied",
                            "max_steps": 1
                        }),
                    )
                })
                .collect();
            Ok(CompletionResponse {
                tool_calls: calls,
                usage,
                ..Default::default()
            })
        } else {
            // A child: do nothing, so it reaches its 1-step cap and returns.
            Ok(CompletionResponse {
                usage,
                ..Default::default()
            })
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_fanout_to_over_100_stays_bounded_and_completes() {
    let dir = ws();
    let fanout = 120usize;
    // Room for 120 children + the root, 16 at a time, plenty of budget.
    let containment = Containment::new(fanout as u32 + 1, 16, 3, 10_000_000);
    let contract = TaskContract::workspace(
        "ROOT-FANOUT: spawn a large fleet of sub-agents at once.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "never.txt".into(),
        needle: "never".into(),
    })
    .with_max_steps(1);

    let probe = ConcurrencyProbe::new(fanout);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &probe,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    // The whole tree completed without deadlock.
    assert!(matches!(result.outcome, RunOutcome::StepCapReached { .. }));
    // Every requested child ran.
    assert_eq!(store.children(result.run_id).unwrap().len(), fanout);
    // Concurrency was real but never exceeded max_concurrent.
    let peak = probe.peak.load(Ordering::SeqCst);
    assert!(peak > 1, "children actually overlapped (peak {peak})");
    assert!(
        peak <= 16,
        "never more than max_concurrent at once (peak {peak})"
    );
}

// --- T02 at the tree level: the aggregate ceiling halts the whole tree. ---

/// Reports a fixed token cost per call. The parent (goal "BUDGET-ROOT") spawns
/// `children` sub-agents, each a one-step no-op; their combined draw crosses the
/// tree ceiling.
struct TokenBurner {
    children: usize,
    per_call: u64,
}

impl Provider for TokenBurner {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let usage = Some(Usage {
            total_tokens: self.per_call,
            ..Default::default()
        });
        if req.user.contains("BUDGET-ROOT") {
            let calls = (0..self.children)
                .map(|i| {
                    call(
                        "spawn_agent",
                        json!({ "goal": format!("burn {i}"), "verify_file": format!("b{i}.txt"), "verify_contains": "nope", "max_steps": 1 }),
                    )
                })
                .collect();
            Ok(CompletionResponse {
                tool_calls: calls,
                usage,
                ..Default::default()
            })
        } else {
            Ok(CompletionResponse {
                usage,
                ..Default::default()
            })
        }
    }
}

#[tokio::test]
async fn the_aggregate_ceiling_halts_the_whole_tree() {
    let dir = ws();
    // Ceiling 100 tokens; four children at 30 each, run one at a time, cross it.
    // No child's own contract budget is exceeded — the halt is the tree's.
    let containment = Containment::new(10, 1, 3, 100);
    let contract = TaskContract::workspace(
        "BUDGET-ROOT: spawn children until the tree's spend ceiling stops it.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "x.txt".into(),
        needle: "x".into(),
    })
    .with_max_steps(2);

    let provider = TokenBurner {
        children: 4,
        per_call: 30,
    };
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    // The tree halts as a whole against the containment budget.
    assert!(
        matches!(result.outcome, RunOutcome::BudgetCeilingReached { .. }),
        "got {:?}",
        result.outcome
    );
    // The recorded spend never crossed the ceiling.
    let root_draws: u64 = store
        .agent_events(result.run_id)
        .unwrap()
        .into_iter()
        .filter_map(|e| {
            if e.kind == "budget_draw" {
                e.tokens
            } else {
                None
            }
        })
        .sum();
    assert!(root_draws <= 100);
}

// --- T07: a child's deferred action persists and resumes, unchanged from 0.4.0. ---

use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
use io_harness::resume_with_decision;

/// Defers the first decision it is asked, then approves — so a child's first
/// sensitive write is deferred to a human.
struct DeferOnce {
    asked: Counter,
}

impl Approver for DeferOnce {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        let first = self.asked.fetch_add(1, Ordering::SeqCst) == 0;
        Box::pin(async move {
            if first {
                Decision::Defer
            } else {
                Decision::approve()
            }
        })
    }
}

#[tokio::test]
async fn a_child_defer_persists_and_resumes_via_resume_with_decision() {
    let dir = ws();
    // Default policy makes the child's write a sensitive (Ask) action.
    let contract = TaskContract::workspace(
        "Delegate a sensitive write; the child will defer to a human.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "out.txt".into(),
        needle: "OK".into(),
    })
    .with_max_steps(3);

    let script = MockScript::new(vec![
        vec![call(
            "spawn_agent",
            json!({ "goal": "write out", "verify_file": "out.txt", "verify_contains": "OK" }),
        )],
        vec![write("out.txt", "OK")], // child: write is Ask -> approver defers
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::default(),
        &DeferOnce {
            asked: Counter::new(0),
        },
        &containment(),
    )
    .await
    .unwrap();

    // The tree paused, surfacing the child's request_id, and nothing was written.
    let request_id = match result.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };
    assert!(!dir.path().join("out.txt").exists());

    // The pending action is persisted against the child's own run.
    let pending = store
        .pending(request_id)
        .unwrap()
        .expect("pending survives");
    assert_eq!(pending.act, "write");
    assert_eq!(pending.target, "out.txt");
    let child_run = pending.run_id;
    assert_eq!(store.parent(child_run).unwrap(), Some(result.run_id));

    // A human approves; resume_with_decision performs the pending write and
    // finishes the child — the 0.4.0 resume path, reused unchanged.
    let child_contract = TaskContract::workspace("write out", dir.path()).with_verification(
        Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "OK".into(),
        },
    );
    let resumed = resume_with_decision(
        &child_contract,
        &script,
        &store,
        child_run,
        request_id,
        Decision::approve(),
        &Policy::default(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(matches!(resumed.outcome, RunOutcome::Success { .. }));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "OK"
    );
}

/// A provider that takes real wall-clock time per call and never finishes the
/// task, so only a limit can stop it.
struct Slow {
    per_call: std::time::Duration,
}

impl Provider for Slow {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        tokio::time::sleep(self.per_call).await;
        Ok(CompletionResponse::default())
    }
}

/// F16 — a tree halts on its duration ceiling.
///
/// `Containment::max_total_duration` was declared in 0.5.0 and never read, so a
/// caller could bound a 24-hour tree's wall-clock and have it silently ignored.
/// The step cap was the only thing that ever stopped this run.
///
/// The provider sleeps longer than the whole ceiling, so the crossing is not a
/// race: whatever the machine's speed, the first step overruns it. The
/// assertions are therefore a loose upper bound on steps and never a duration.
#[tokio::test]
async fn a_tree_halts_on_its_duration_ceiling_instead_of_ignoring_it() {
    let dir = ws();
    let mut containment = Containment::new(10, 1, 3, 1_000_000);
    containment.max_total_duration = Some(std::time::Duration::from_millis(200));

    let contract = TaskContract::workspace("Run until something stops you.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(20);

    let provider = Slow {
        per_call: std::time::Duration::from_millis(400),
    };
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    let steps = match result.outcome {
        RunOutcome::BudgetCeilingReached { steps } => steps,
        other => panic!("expected the duration ceiling to halt the tree, got {other:?}"),
    };
    assert!(
        steps < 20,
        "the ceiling must stop the tree well short of its step cap, stopped at {steps}"
    );
}

/// The ceiling is opt-in: with none set, the same run reaches its step cap as it
/// always did. A limit that changes an unbounded run's behaviour is a regression
/// however correct it is when asked for.
#[tokio::test]
async fn a_tree_with_no_duration_ceiling_is_unaffected() {
    let dir = ws();
    let containment = Containment::new(10, 1, 3, 1_000_000);
    assert!(containment.max_total_duration.is_none());

    let contract = TaskContract::workspace("Run until something stops you.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "never.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(3);

    let provider = Slow {
        per_call: std::time::Duration::from_millis(50),
    };
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { steps: 3 }),
        "got {:?}",
        result.outcome
    );
}

/// Spawns four children on its root turn, then makes each child take a different
/// amount of real time — the LAST child spawned finishes FIRST.
///
/// The stagger is the whole point: with results collected in completion order the
/// composition comes back reversed, so this provider distinguishes ordered
/// collection from unordered rather than merely exercising it.
struct Staggered {
    children: usize,
}

impl Provider for Staggered {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        if req.user.contains("ORDER-ROOT") {
            let calls = (0..self.children)
                .map(|i| {
                    call(
                        "spawn_agent",
                        json!({
                            "goal": format!("ordered-{i}"),
                            "verify_file": format!("o{i}.txt"),
                            "verify_contains": "nope",
                            "max_steps": 1
                        }),
                    )
                })
                .collect();
            return Ok(CompletionResponse {
                tool_calls: calls,
                ..Default::default()
            });
        }
        // A child: sleep longer the earlier it was spawned.
        for i in 0..self.children {
            if req.user.contains(&format!("ordered-{i}")) {
                let ms = (self.children - i) as u64 * 120;
                tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
                break;
            }
        }
        Ok(CompletionResponse::default())
    }
}

/// A tree composes its children in the order they were SPAWNED, not the order
/// they finished.
///
/// Until 0.12.0 the fan-out used `buffer_unordered`, so the composed child
/// observations and the `decisions` list — which become `steps.result` and
/// `steps.decision` — came back in completion order. The same task over the same
/// workspace therefore produced a different trace and a different next prompt
/// depending on which child won a race, which makes deterministic replay
/// impossible. This test fails against that: its children finish in exactly
/// reverse order.
#[tokio::test]
async fn children_are_composed_in_spawn_order_not_completion_order() {
    let dir = ws();
    let n = 4usize;
    // Concurrency 4: all four children genuinely overlap, so completion order is
    // decided by their sleeps and not by sequential execution.
    let containment = Containment::new(10, 4, 3, 1_000_000);
    let contract = TaskContract::workspace(
        "ORDER-ROOT: spawn several children that finish out of order.",
        dir.path(),
    )
    .with_verification(Verification::WorkspaceFileContains {
        file: "never.txt".into(),
        needle: "never".into(),
    })
    .with_max_steps(1);

    let store = Store::memory().unwrap();
    let result = run_tree(
        &contract,
        &Staggered { children: n },
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment,
    )
    .await
    .unwrap();

    let root_step = store
        .steps(result.run_id)
        .unwrap()
        .into_iter()
        .find(|s| s.step == 1)
        .expect("the root's spawning step");

    for text in [&root_step.result, &root_step.decision] {
        let mut positions = Vec::new();
        for i in 0..n {
            let needle = format!("ordered-{i}");
            match text.find(&needle) {
                Some(at) => positions.push((i, at)),
                // `decision` names children by run id, not goal; skip it if the
                // goal does not appear there rather than asserting on a shape the
                // crate does not promise.
                None => break,
            }
        }
        if positions.len() == n {
            let mut sorted = positions.clone();
            sorted.sort_by_key(|(_, at)| *at);
            assert_eq!(
                positions, sorted,
                "children must appear in spawn order; got {positions:?} in {text:?}"
            );
        }
    }

    // And the child run ids ascend with spawn order, so the tree graph reads the
    // same way twice.
    let children = store.children(result.run_id).unwrap();
    assert_eq!(children.len(), n, "all children were spawned");
    let mut ascending = children.clone();
    ascending.sort_unstable();
    assert_eq!(
        children, ascending,
        "child run ids must be allocated in spawn order"
    );
}

// ---------------------------------------------------------------------------
// 0.13.0 — the durable ledger reaches depth.
//
// The workspace loop and the sub-agent loop each build their own ledger. A fix
// that landed in one and not the other would leave the same defect at a depth
// where it is harder to see, so the sub-agent loop gets its own assertion.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn each_agent_keeps_its_own_durable_ledger_under_its_own_run_id() {
    let dir = ws();
    std::fs::write(dir.path().join("seed.txt"), "SEED-CONTENT").unwrap();
    let contract = TaskContract::workspace("delegate a read and a write", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "OK".into(),
        })
        .with_max_steps(3);

    let script = MockScript::new(vec![
        vec![call(
            "spawn_agent",
            json!({ "goal": "read the seed then write out", "verify_file": "out.txt", "verify_contains": "OK" }),
        )],
        // The child's turns: observe something, then satisfy its verification.
        vec![call("read_file", json!({ "path": "seed.txt" }))],
        vec![write("out.txt", "OK")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    let ids = store.tree_run_ids(result.run_id).unwrap();
    let children: Vec<i64> = ids.into_iter().filter(|id| *id != result.run_id).collect();
    assert!(!children.is_empty(), "the tree spawned at least one child");

    let child_rows = store.observations(children[0]).unwrap();
    assert!(
        child_rows.iter().any(|o| o.text.contains("SEED-CONTENT")),
        "the child's own observations are durable under the child's run id, got {child_rows:?}"
    );
    assert!(
        !store
            .observations(result.run_id)
            .unwrap()
            .iter()
            .any(|o| o.text.contains("SEED-CONTENT")),
        "a child's observations belong to the child, not to its parent's ledger"
    );
}

/// A provider whose script carries what the agent *said* beside what it called,
/// which the plain [`MockScript`] cannot express: it answers every call with tool
/// calls alone and never with text.
struct MockSaying {
    steps: Vec<(Option<&'static str>, Vec<ToolCall>)>,
    at: AtomicUsize,
}

impl MockSaying {
    fn new(steps: Vec<(Option<&'static str>, Vec<ToolCall>)>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
}

impl Provider for MockSaying {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        let (text, calls) = self.steps.get(i).cloned().unwrap_or((None, Vec::new()));
        Ok(CompletionResponse {
            text: text.map(str::to_string),
            tool_calls: calls,
            ..Default::default()
        })
    }
}

/// 0.50.0 — F1: a child hands its parent what it concluded, not a discriminant.
///
/// Until this release `compose_child` folded `[child 7 "goal" -> Success { steps:
/// 1 }]` and nothing else, because `RunOutcome::Success` carries no text. A parent
/// that fanned out to investigate learned that its children succeeded and nothing
/// they found. The marker below is produced by the child's completion and by
/// nothing else in the run, so its presence in the *parent's* ledger is the claim.
#[tokio::test]
async fn a_child_reports_what_it_concluded_to_its_parent() {
    const FINDING: &str = "the cache key omits the tenant id";

    let dir = ws();
    let contract = TaskContract::workspace("Investigate, then write it up.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "report.txt".into(),
            needle: "written".into(),
        })
        .with_max_steps(4);

    // parent#1 spawns; child#1 writes its verify file and says what it found;
    // parent#2 writes its own report.
    let script = MockSaying::new(vec![
        (None, vec![spawn("investigate", "found.txt", "FOUND")]),
        (Some(FINDING), vec![write("found.txt", "FOUND")]),
        (None, vec![write("report.txt", "written")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });

    let child = *store
        .children(result.run_id)
        .unwrap()
        .first()
        .expect("the parent spawned a child");

    let parent_rows = store.observations(result.run_id).unwrap();
    let composed = parent_rows
        .iter()
        .find(|o| o.text.contains(&format!("[child {child}")))
        .unwrap_or_else(|| panic!("the parent composed its child, got {parent_rows:?}"));

    // The release: the conclusion reaches the parent.
    assert!(
        composed.text.contains(FINDING),
        "the child's conclusion reaches its parent, got {:?}",
        composed.text
    );
    // And what the parent already learned is still there — this adds, it replaces
    // nothing.
    assert!(
        composed.text.contains("Success"),
        "the outcome is still reported, got {:?}",
        composed.text
    );
    assert!(
        composed.text.contains(&child.to_string()),
        "the child's run id is still reported, got {:?}",
        composed.text
    );
}

/// Defers every decision, so a child's write parks the whole tree on a human.
struct Defer;
impl Approver for Defer {
    fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async { Decision::Defer })
    }
}

/// 0.50.0 — F2: a child adopted after a pause reports the same words it would
/// have reported had its parent waited.
///
/// The parent's spawn step is deliberately left uncommitted when a child pauses
/// (0.7.0's fix for double execution), so the resume replays it — and the child
/// that had already finished is composed from the store rather than from a loop
/// that is no longer running. That is the path a restarted process takes, and it
/// must not render the conclusion differently: there is one rendering, reading one
/// set of `"said"` rows, which is why this is assertable at all.
#[tokio::test]
async fn an_adopted_child_reports_the_same_conclusion() {
    const FINDING: &str = "alpha holds the stale index";

    let dir = ws();
    let contract = TaskContract::workspace("Delegate both halves.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        })
        .with_max_steps(4);

    // a.txt is allowed outright; b.txt asks, and `Defer` parks it.
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("a.txt")
        .ask_write("b.txt");

    let script = MockSaying::new(vec![
        // parent#1 fans out to two children.
        (
            None,
            vec![spawn("do a", "a.txt", "ALPHA"), spawn("do b", "b.txt", "BETA")],
        ),
        // child A finishes and says what it found.
        (Some(FINDING), vec![write("a.txt", "ALPHA")]),
        // child B's write asks, and nothing in this process answers.
        (None, vec![write("b.txt", "BETA")]),
        // parent#1 replayed on resume: the same fan-out, one child already done.
        (
            None,
            vec![spawn("do a", "a.txt", "ALPHA"), spawn("do b", "b.txt", "BETA")],
        ),
        (None, vec![]),
        (None, vec![]),
    ]);
    let store = Store::memory().unwrap();

    let paused = run_tree(&contract, &script, &store, &policy, &Defer, &containment())
        .await
        .unwrap();
    let request_id = match paused.outcome {
        RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected the tree to pause on b.txt, got {other:?}"),
    };

    // Nothing was composed into the parent yet: its step never committed.
    assert!(
        !store
            .observations(paused.run_id)
            .unwrap()
            .iter()
            .any(|o| o.text.contains(FINDING)),
        "the paused step is uncommitted, so nothing about the finished child is durable yet"
    );

    let resumed = resume_tree_with_decision(
        &contract,
        &script,
        &store,
        paused.run_id,
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
        matches!(resumed.outcome, RunOutcome::Success { .. }),
        "the tree resumed to success, got {:?}",
        resumed.outcome
    );

    let rows = store.observations(paused.run_id).unwrap();
    assert!(
        rows.iter().any(|o| o.text.contains(FINDING)),
        "the adopted child's conclusion reaches the resumed parent, got {rows:?}"
    );
}

/// 0.50.0 — F9: a contradiction in the spawn arguments is a typed observation the
/// parent adapts to, and it costs nothing.
///
/// The two arguments this release adds can be combined into a request that has no
/// meaning: a child you are not waiting for has no wall clock to cross. Answered
/// where every other malformed spawn is answered — before a child is registered,
/// admitted or written — so a parent that gets it wrong leaves no run row, no
/// queue place and no `spawns` row behind, and carries on.
#[tokio::test]
async fn a_conflicting_spawn_costs_nothing_and_the_parent_carries_on() {
    let dir = ws();
    let contract = TaskContract::workspace("Delegate, then finish yourself.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(4);

    let script = MockScript::new(vec![
        vec![call(
            "spawn_agent",
            json!({
                "goal": "impossible", "verify_file": "x.txt", "verify_contains": "X",
                "wait": false, "background_after_secs": 30
            }),
        )],
        vec![write("done.txt", "ok")],
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();

    // The parent was not stopped by it.
    assert_eq!(result.outcome, RunOutcome::Success { steps: 2 });

    // It was told, in terms naming the conflict.
    let rows = store.observations(result.run_id).unwrap();
    assert!(
        rows.iter()
            .any(|o| o.text.contains("[spawn error]") && o.text.contains("background_after_secs")),
        "the parent is told which two arguments conflict, got {rows:?}"
    );

    // And it cost nothing: no child run, and no spawn recorded against the step.
    assert!(
        store.children(result.run_id).unwrap().is_empty(),
        "a conflicting spawn creates no child"
    );
    assert!(
        store
            .agent_events(result.run_id)
            .unwrap()
            .iter()
            .all(|e| e.kind != "spawn"),
        "a conflicting spawn records no spawn event"
    );
}

/// A provider that parks one scripted call, so a child can outlast its parent's
/// stated patience without the test depending on how busy the machine is.
struct MockParking {
    steps: Vec<(Option<&'static str>, Vec<ToolCall>)>,
    park: usize,
    delay: std::time::Duration,
    at: AtomicUsize,
}

impl Provider for MockParking {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        if i == self.park {
            tokio::time::sleep(self.delay).await;
        }
        let (text, calls) = self.steps.get(i).cloned().unwrap_or((None, Vec::new()));
        Ok(CompletionResponse {
            text: text.map(str::to_string),
            tool_calls: calls,
            ..Default::default()
        })
    }
}

/// 0.50.0 — F3: a detached child does not block its parent, and its report arrives
/// at a later step.
///
/// Three facts together, because no one of them is the claim. The report is absent
/// from the step that spawned the child (the parent did not wait), the parent went
/// on to take further steps (it was not merely reordered), and the report is there
/// at a strictly later step (nothing was lost by not waiting).
#[tokio::test]
async fn a_detached_child_reports_at_a_later_step() {
    const FINDING: &str = "nothing here blocks the parent";

    let dir = ws();
    let contract = TaskContract::workspace("Delegate and carry on.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(5);

    let detached = call(
        "spawn_agent",
        json!({
            "goal": "look into it", "verify_file": "found.txt", "verify_contains": "FOUND",
            "wait": false
        }),
    );
    let script = MockSaying::new(vec![
        (None, vec![detached]),
        (Some(FINDING), vec![write("found.txt", "FOUND")]),
        (None, vec![write("scratch.txt", "1")]),
        (None, vec![write("done.txt", "ok")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the parent finished its own work, got {:?}",
        result.outcome
    );

    let rows = store.observations(result.run_id).unwrap();
    let report = rows
        .iter()
        .find(|o| o.text.contains(FINDING))
        .unwrap_or_else(|| panic!("the detached child reported, got {rows:?}"));

    // It was not folded into the step that spawned it.
    assert!(
        report.step > 1,
        "the report arrives after the step that detached the child, got step {}",
        report.step
    );
    // And the parent was told, at the spawning step, that it was not waiting.
    let told = rows
        .iter()
        .find(|o| o.text.contains("detached"))
        .expect("the parent is told the child was detached");
    assert_eq!(told.step, 1, "it is told at the step it asked");
    // The parent kept working in between rather than idling.
    assert!(
        store.steps(result.run_id).unwrap().len() >= 3,
        "the parent took its own steps while the child ran"
    );
}

/// 0.50.0 — F4: a spawn that names neither argument is the spawn it always was.
///
/// The negative control the whole release rests on. Two children in one step, both
/// waited for, both folded into that step, and in the order the model asked for
/// them rather than the order they finished — which is 0.12.0's determinism claim
/// asserted again, because this release touches that stream.
#[tokio::test]
async fn a_blocking_spawn_folds_into_its_own_step_in_call_order() {
    let dir = ws();
    let contract = TaskContract::workspace("Delegate both, in order.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "b.txt".into(),
            needle: "BETA".into(),
        })
        .with_max_steps(4);

    let script = MockSaying::new(vec![
        (
            None,
            vec![spawn("do a", "a.txt", "ALPHA"), spawn("do b", "b.txt", "BETA")],
        ),
        (Some("A-SAID"), vec![write("a.txt", "ALPHA")]),
        (Some("B-SAID"), vec![write("b.txt", "BETA")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });

    let rows = store.observations(result.run_id).unwrap();
    let a = rows.iter().position(|o| o.text.contains("A-SAID"));
    let b = rows.iter().position(|o| o.text.contains("B-SAID"));
    let (a, b) = (a.expect("child A reported"), b.expect("child B reported"));
    assert!(a < b, "children fold in the order they were called for");
    assert_eq!(rows[a].step, 1, "a waited-for child folds into its own step");
    assert_eq!(rows[b].step, 1, "a waited-for child folds into its own step");
    // And nothing was left running.
    assert!(
        !rows.iter().any(|o| o.text.contains("detached")),
        "a spawn that named neither argument was never detached"
    );
}

/// 0.50.0 — F6: a tree never returns while a child it started is running.
///
/// The parent detaches a child and finishes its own work in the same step, so the
/// loop reaches its ending with a child still in flight. 0.48.0's rule is that
/// everything a run starts is inside the boundary; the drain is what keeps that
/// true for children, and it sits at the loop's one return rather than at each of
/// its endings.
#[tokio::test]
async fn a_tree_drains_the_children_it_stopped_waiting_for() {
    const FINDING: &str = "drained after the parent finished";

    let dir = ws();
    let contract = TaskContract::workspace("Delegate and finish at once.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(3);

    let detached = call(
        "spawn_agent",
        json!({
            "goal": "keep going", "verify_file": "found.txt", "verify_contains": "FOUND",
            "wait": false
        }),
    );
    // One parent step: detach a child AND satisfy the parent's own verification.
    let script = MockSaying::new(vec![
        (None, vec![detached, write("done.txt", "ok")]),
        (Some(FINDING), vec![write("found.txt", "FOUND")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });

    let child = *store
        .children(result.run_id)
        .unwrap()
        .first()
        .expect("the parent detached a child");

    // The child is finished, not abandoned mid-run.
    let summary = store
        .run_summary(child)
        .unwrap()
        .expect("the drained child has an ending");
    assert!(
        summary.success,
        "the drained child ran to completion, got {:?}",
        summary.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("found.txt")).unwrap(),
        "FOUND",
        "its work reached the workspace"
    );
    // And its report is durable under the parent, where an operator finds it.
    assert!(
        store
            .observations(result.run_id)
            .unwrap()
            .iter()
            .any(|o| o.text.contains(FINDING)),
        "a drained child's report is recorded against its parent"
    );
}

/// 0.50.0 — F5: a wall clock moves a child to the background and does not kill it.
///
/// Both halves, and the second is the one that matters. `tokio::time::timeout` is
/// the obvious way to write this and it is wrong: it drops the child's future,
/// cancelling it mid-step and leaving its run row `running` forever —
/// indistinguishable from a crashed process. Such an implementation passes the
/// first assertion here and fails the second.
#[tokio::test]
async fn a_wall_clock_backgrounds_a_child_without_killing_it() {
    const FINDING: &str = "slow but finished";

    let dir = ws();
    let contract = TaskContract::workspace("Delegate, but do not wait forever.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(5);

    let patient = call(
        "spawn_agent",
        json!({
            "goal": "take your time", "verify_file": "found.txt", "verify_contains": "FOUND",
            "background_after_secs": 1
        }),
    );
    let script = MockParking {
        steps: vec![
            (None, vec![patient]),
            (Some(FINDING), vec![write("found.txt", "FOUND")]),
            (None, vec![write("scratch.txt", "1")]),
            (None, vec![write("done.txt", "ok")]),
        ],
        // The child's own completion, parked past the parent's one-second patience.
        park: 1,
        delay: std::time::Duration::from_millis(1_500),
        at: AtomicUsize::new(0),
    };
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the parent finished its own work, got {:?}",
        result.outcome
    );

    let rows = store.observations(result.run_id).unwrap();
    // Half one: the parent stopped waiting, and was told so at the step it asked.
    let moved = rows
        .iter()
        .find(|o| o.text.contains("moved to the background"))
        .unwrap_or_else(|| panic!("the parent was told it stopped waiting, got {rows:?}"));
    assert_eq!(moved.step, 1);

    // Half two: the child was not dropped. It finished, its work landed, and it
    // reported.
    let child = *store.children(result.run_id).unwrap().first().unwrap();
    let summary = store.run_summary(child).unwrap().expect("the child ended");
    assert!(
        summary.success,
        "a backgrounded child is not cancelled, got {:?}",
        summary.outcome
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("found.txt")).unwrap(),
        "FOUND"
    );
    assert!(
        rows.iter().any(|o| o.text.contains(FINDING)),
        "the backgrounded child's report reaches its parent, got {rows:?}"
    );
}

/// 0.50.0 — F5's other arm: a clock the child finishes inside changes nothing.
///
/// Ten seconds against an instantly-answering child, so the only way this reports
/// a background move is if the race is decided by something other than the clock.
#[tokio::test]
async fn a_wall_clock_the_child_beats_is_an_ordinary_spawn() {
    let dir = ws();
    let contract = TaskContract::workspace("Delegate with a generous clock.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "found.txt".into(),
            needle: "FOUND".into(),
        })
        .with_max_steps(3);

    let patient = call(
        "spawn_agent",
        json!({
            "goal": "be quick", "verify_file": "found.txt", "verify_contains": "FOUND",
            "background_after_secs": 10
        }),
    );
    let script = MockSaying::new(vec![
        (None, vec![patient]),
        (Some("QUICK"), vec![write("found.txt", "FOUND")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &contract,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });

    let rows = store.observations(result.run_id).unwrap();
    assert!(
        !rows.iter().any(|o| o.text.contains("moved to the background")),
        "a child that finishes inside its clock is never backgrounded, got {rows:?}"
    );
    let report = rows.iter().find(|o| o.text.contains("QUICK")).unwrap();
    assert_eq!(report.step, 1, "it folds into the step that spawned it");
}

/// 0.50.0 — F8: the operator's settings narrow the model's, in both directions.
///
/// A contract clock reaches a spawn that named none and beats a spawn that named
/// a longer one; a contract that refuses detachment answers `"wait": false` with
/// an ordinary blocking spawn and a line saying so. Silence would be worse than
/// either: a model that believes it fanned out and did not is a model reasoning
/// about work that is not happening.
#[tokio::test]
async fn the_contract_narrows_what_a_spawn_may_ask_for() {
    let dir = ws();
    let strict = TaskContract::workspace("Delegate, but stay in step.", dir.path())
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(4)
        .without_detached_spawns();

    let detached = call(
        "spawn_agent",
        json!({
            "goal": "look into it", "verify_file": "found.txt", "verify_contains": "FOUND",
            "wait": false
        }),
    );
    let script = MockSaying::new(vec![
        (None, vec![detached]),
        (Some("SAID-IT"), vec![write("found.txt", "FOUND")]),
        (None, vec![write("done.txt", "ok")]),
    ]);
    let store = Store::memory().unwrap();

    let result = run_tree(
        &strict,
        &script,
        &store,
        &Policy::permissive(),
        &ApproveAll,
        &containment(),
    )
    .await
    .unwrap();
    assert!(matches!(result.outcome, RunOutcome::Success { .. }));

    let rows = store.observations(result.run_id).unwrap();
    let report = rows
        .iter()
        .find(|o| o.text.contains("SAID-IT"))
        .expect("the child still reported");
    // Waited for after all — folded into the step that asked, not a later one.
    assert_eq!(report.step, 1, "the refused detachment became a plain wait");
    // And the model was told, rather than left believing it had fanned out.
    assert!(
        report.text.contains("[spawn narrowed]"),
        "the parent is told its request was narrowed, got {:?}",
        report.text
    );
}

/// 0.50.0 — F8's other direction: a contract clock is not raised by a spawn that
/// asks for a longer one.
#[test]
fn a_contract_clock_is_a_ceiling_and_not_a_default_only() {
    use std::time::Duration;

    let none = TaskContract::workspace("g", "/repo");
    assert_eq!(none.spawn_background_after, None);
    assert!(none.detached_spawns);

    let capped = TaskContract::workspace("g", "/repo")
        .with_spawn_background_after(Duration::from_secs(60));
    assert_eq!(capped.spawn_background_after, Some(Duration::from_secs(60)));
    assert!(
        capped.detached_spawns,
        "a clock does not by itself forbid detaching"
    );

    let strict = TaskContract::workspace("g", "/repo").without_detached_spawns();
    assert!(!strict.detached_spawns);
}
