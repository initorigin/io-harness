//! 0.41.0 — read-only tool calls from one completion run concurrently, and every
//! recorded artifact still lands in the order the model asked for it.
//!
//! Nothing here measures a duration. A wall-clock assertion on a CI runner is a
//! flake waiting to be written, and this project has paid for that lesson more
//! than once. Concurrency is proven where it is either present or absent
//! instead: tools that rendezvous with each other complete only if they are in
//! flight together, and deadlock — bounded, so the test fails rather than hanging
//! the matrix — if they are run one after another.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, Store, TaskContract, ToolSpec, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- the tools

/// A read-only tool that cannot finish alone: it waits on a barrier shared with
/// its siblings. Three of these in one completion complete only if all three are
/// in flight at the same time.
struct Rendezvous {
    name: String,
    barrier: Arc<tokio::sync::Barrier>,
    effect: ToolEffect,
}

impl Rendezvous {
    fn new(name: &str, barrier: &Arc<tokio::sync::Barrier>, effect: ToolEffect) -> Self {
        Self {
            name: name.into(),
            barrier: Arc::clone(barrier),
            effect,
        }
    }
}

impl Tool for Rendezvous {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Waits for its siblings, then reports.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.barrier.wait().await;
            Ok(format!("{} met the others", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        self.effect
    }
}

/// A read-only tool that records how many of its kind were running when it
/// started, so the cap can be read off the peak rather than off a clock.
///
/// It also finishes out of order on purpose: `settle` is longer the earlier the
/// tool appears in the completion, so within each wave the model's first call is
/// the last to return. Nothing asserts that it worked — a runner that ran them in
/// order anyway would still pass — but where it does work, "in call order" and
/// "in completion order" are two different answers and the ordering claim has
/// something to be wrong about.
struct Counted {
    name: String,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
    settle: Duration,
}

impl Counted {
    fn new(name: &str, live: &Arc<AtomicUsize>, peak: &Arc<AtomicUsize>, settle: Duration) -> Self {
        Self {
            name: name.into(),
            live: Arc::clone(live),
            peak: Arc::clone(peak),
            settle,
        }
    }
}

impl Tool for Counted {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Counts itself in and out.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            let now = self.live.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            // Long enough that every task the bound admitted has counted itself
            // in before the first one counts itself out, and uneven so they do
            // not come back in the order they went out.
            tokio::time::sleep(self.settle).await;
            self.live.fetch_sub(1, Ordering::SeqCst);
            Ok(self.name.clone())
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

/// A read-only tool that keeps working after the run is expected to be gone, and
/// records the moment it gets there.
///
/// The sleep is what makes N5 falsifiable: a task that is aborted with the
/// `JoinSet` never reaches `finished`, and a task that was merely detached does —
/// after the run that started it has been dropped. It then parks forever, so the
/// run cannot complete on its own and the drop is what ends it.
struct Endless {
    name: String,
    entered: Arc<AtomicUsize>,
    finished: Arc<AtomicUsize>,
}

impl Tool for Endless {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Never returns.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.entered.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(400)).await;
            self.finished.fetch_add(1, Ordering::SeqCst);
            // Parked, so the run can only ever end by being dropped.
            std::future::pending::<()>().await;
            Ok(self.name.clone())
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

// ---------------------------------------------------------------- the provider

/// Returns a fixed script of tool calls, one completion per `complete`.
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

fn call(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}

fn ws() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// A contract that can never be satisfied, so the loop runs its whole step budget
/// and the scripted completion is reached.
fn never_passes(root: &std::path::Path, steps: u32) -> TaskContract {
    TaskContract::workspace("exercise the read batch", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        })
        .with_max_steps(steps)
}

fn open_policy() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

/// Generous, because it bounds a run that is expected to *finish* and the slowest
/// runner in the matrix is several times slower than a developer's machine.
const MUST_FINISH: Duration = Duration::from_secs(60);

/// Short, because it bounds a run that is expected never to finish. Nothing in
/// the serial path can complete a rendezvous, so waiting longer only makes a
/// failing matrix slower.
const MUST_NOT_FINISH: Duration = Duration::from_secs(5);

// ---------------------------------------------------------------- F1

/// F1 — read-only calls in one completion run concurrently, and with the cap at 1
/// they do not.
///
/// Three registered tools, each of which waits on a barrier of three. The
/// completion calls all three. Under the default cap the step completes, which
/// can only happen if all three are in flight together; on the same contract,
/// same policy, same store and same completion with `with_max_parallel_reads(1)`
/// the run cannot get past the first call and the bounded timeout fires.
///
/// This pair is the release in one assertion, and it contains no clock.
#[tokio::test]
async fn three_read_only_tools_rendezvous_and_at_cap_one_they_cannot() {
    let dir = ws();
    let barrier = Arc::new(tokio::sync::Barrier::new(3));
    let tools = Toolbox::new()
        .with(Rendezvous::new("meet_a", &barrier, ToolEffect::ReadOnly))
        .with(Rendezvous::new("meet_b", &barrier, ToolEffect::ReadOnly))
        .with(Rendezvous::new("meet_c", &barrier, ToolEffect::ReadOnly));
    let script = || MockScript::new(vec![vec![call("meet_a"), call("meet_b"), call("meet_c")]]);

    let concurrent = never_passes(dir.path(), 1).with_tools(tools.clone());
    let store = Store::memory().unwrap();
    let outcome = tokio::time::timeout(
        MUST_FINISH,
        run_with(&concurrent, &script(), &store, &open_policy(), &ApproveAll),
    )
    .await
    .expect("three read-only calls that rendezvous must complete when they run together");
    outcome.expect("the run itself must not error");

    let seen: Vec<String> = store
        .observations(store.last_run().unwrap().unwrap())
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    for name in ["meet_a", "meet_b", "meet_c"] {
        assert!(
            seen.iter().any(|t| t.contains(&format!("{name} met"))),
            "every tool in the batch must have reported; {name} did not: {seen:?}"
        );
    }

    // The negative control. Everything is the same but the cap.
    let serial = never_passes(dir.path(), 1)
        .with_tools(tools)
        .with_max_parallel_reads(1);
    let serial_store = Store::memory().unwrap();
    let ran = tokio::time::timeout(
        MUST_NOT_FINISH,
        run_with(
            &serial,
            &script(),
            &serial_store,
            &open_policy(),
            &ApproveAll,
        ),
    )
    .await;
    assert!(
        ran.is_err(),
        "at a cap of 1 the calls run one after another, so the rendezvous can \
         never be met and the run cannot finish — it finished, which means the \
         serial path is not serial"
    );
}

// ---------------------------------------------------------------- F6

/// F6 — the cap bounds calls in flight.
///
/// Twenty read-only calls in one completion under a cap of three. A counter
/// incremented on entry and decremented on exit never sees more than three, and
/// all twenty complete with their observations in the order the model asked.
#[tokio::test]
async fn the_cap_bounds_calls_in_flight_and_all_of_them_still_land_in_order() {
    let dir = ws();
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));
    let names: Vec<String> = (0..20).map(|i| format!("count_{i:02}")).collect();
    let mut tools = Toolbox::new();
    for (i, name) in names.iter().enumerate() {
        let settle = Duration::from_millis(20 - i as u64 % 20);
        tools = tools.with(Counted::new(name, &live, &peak, settle));
    }

    let contract = never_passes(dir.path(), 1)
        .with_tools(tools)
        .with_max_parallel_reads(3);
    let store = Store::memory().unwrap();
    let script = MockScript::new(vec![names.iter().map(|n| call(n)).collect()]);
    tokio::time::timeout(
        MUST_FINISH,
        run_with(&contract, &script, &store, &open_policy(), &ApproveAll),
    )
    .await
    .expect("twenty bounded read-only calls must complete")
    .expect("the run itself must not error");

    let peak = peak.load(Ordering::SeqCst);
    assert!(
        peak <= 3,
        "a cap of 3 must never have more than 3 calls in flight; peak was {peak}"
    );
    assert!(
        peak >= 2,
        "the bound must be a bound and not an accident of running them one at a \
         time; peak was {peak}"
    );

    let run_id = store.last_run().unwrap().unwrap();
    let landed: Vec<String> = store
        .observations(run_id)
        .unwrap()
        .into_iter()
        .filter_map(|o| o.target)
        .filter(|t| t.starts_with("count_"))
        .collect();
    assert_eq!(
        landed, names,
        "all twenty must land, in the order the model asked for them rather than \
         the order they finished"
    );
}

// ---------------------------------------------------------------- N5

/// N5 — a run that ends mid-batch leaves no task running behind it.
///
/// Three read-only calls that never return. The run is dropped while all of them
/// are in flight; afterwards none of them reaches its own completion, and the
/// store the run was writing to gains nothing.
#[tokio::test]
async fn dropping_a_run_mid_batch_leaves_nothing_running() {
    let dir = ws();
    let entered = Arc::new(AtomicUsize::new(0));
    let finished = Arc::new(AtomicUsize::new(0));
    let mut tools = Toolbox::new();
    for i in 0..3 {
        tools = tools.with(Endless {
            name: format!("endless_{i}"),
            entered: Arc::clone(&entered),
            finished: Arc::clone(&finished),
        });
    }

    let contract = never_passes(dir.path(), 1).with_tools(tools);
    let store = Store::memory().unwrap();
    let script = MockScript::new(vec![vec![
        call("endless_0"),
        call("endless_1"),
        call("endless_2"),
    ]]);

    let ran = tokio::time::timeout(
        Duration::from_millis(100),
        run_with(&contract, &script, &store, &open_policy(), &ApproveAll),
    )
    .await;
    assert!(ran.is_err(), "the batch cannot finish, so the run cannot");
    // The run future — and with it the `JoinSet` holding the batch — is dropped
    // here, at the end of the `timeout`, while every call is still sleeping.
    let steps_at_drop = store
        .last_run()
        .unwrap()
        .map(|r| store.steps(r).unwrap().len());

    // Longer than the sleep inside the tool, so a task that was detached rather
    // than aborted has had every chance to carry on.
    tokio::time::sleep(Duration::from_millis(900)).await;

    assert_eq!(
        finished.load(Ordering::SeqCst),
        0,
        "no aborted call may reach its own completion"
    );
    assert!(
        entered.load(Ordering::SeqCst) > 0,
        "the batch must actually have started, or this proves nothing"
    );
    assert_eq!(
        steps_at_drop,
        store
            .last_run()
            .unwrap()
            .map(|r| store.steps(r).unwrap().len()),
        "the store must be unchanged after the run went away"
    );
}

// ---------------------------------------------------------------- F5

/// F5 — a registered tool's `effect()` is respected in both directions.
///
/// Two registered tools, identical but for what `effect()` returns, and a
/// built-in `read_file` sitting between them in the completion. When they declare
/// `ReadOnly` the three calls are one batch and the rendezvous is met; when they
/// declare `Mutating` — same tools, same barrier, same completion — each runs on
/// its own and the rendezvous can never be met, so the bounded timeout fires.
///
/// The built-in read between them is load-bearing: if it were treated as
/// mutating it would split the run in two and the first arm would fail as well.
#[tokio::test]
async fn a_registered_tools_declared_effect_decides_whether_it_is_batched() {
    for (effect, must_meet) in [(ToolEffect::ReadOnly, true), (ToolEffect::Mutating, false)] {
        let dir = ws();
        std::fs::write(dir.path().join("between.txt"), "in the middle\n").unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(2));
        let tools = Toolbox::new()
            .with(Rendezvous::new("pair_a", &barrier, effect))
            .with(Rendezvous::new("pair_b", &barrier, effect));
        let script = MockScript::new(vec![vec![
            call("pair_a"),
            ToolCall {
                name: "read_file".into(),
                arguments: json!({ "path": "between.txt" }),
            },
            call("pair_b"),
        ]]);

        let contract = never_passes(dir.path(), 1).with_tools(tools);
        let store = Store::memory().unwrap();
        let bound = if must_meet {
            MUST_FINISH
        } else {
            MUST_NOT_FINISH
        };
        let ran = tokio::time::timeout(
            bound,
            run_with(&contract, &script, &store, &open_policy(), &ApproveAll),
        )
        .await;

        if must_meet {
            ran.expect(
                "two tools declaring ReadOnly, with a built-in read between them, \
                 are one batch and must meet",
            )
            .expect("the run itself must not error");
        } else {
            assert!(
                ran.is_err(),
                "tools declaring Mutating run one at a time, so the rendezvous \
                 cannot be met — it was, which means the declaration was ignored"
            );
        }
    }
}

// ---------------------------------------------------------------- F3

/// F3 — a mutating call is never overlapped, and never reordered against a read.
///
/// One completion of `[read a, read b, write a, read a]`. The fourth call's
/// observation contains what the third wrote, and the write does not begin until
/// the first two have folded — asserted from the stored observations rather than
/// from timing.
#[tokio::test]
async fn a_write_is_not_overlapped_and_a_read_after_it_sees_what_it_wrote() {
    let dir = ws();
    std::fs::write(dir.path().join("a.txt"), "before\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "beta\n").unwrap();

    let read = |path: &str| ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path }),
    };
    let script = MockScript::new(vec![vec![
        read("a.txt"),
        read("b.txt"),
        ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": "a.txt", "content": "after\n" }),
        },
        read("a.txt"),
    ]]);

    let store = Store::memory().unwrap();
    let result = run_with(
        &never_passes(dir.path(), 1),
        &script,
        &store,
        &open_policy(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let seen: Vec<String> = store
        .observations(result.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    assert_eq!(
        seen.len(),
        4,
        "one observation per call, in call order: {seen:?}"
    );
    assert!(
        seen[0].contains("before") && !seen[0].contains("after"),
        "the first read runs before the write: {:?}",
        seen[0]
    );
    assert!(seen[1].contains("beta"), "the second read: {:?}", seen[1]);
    assert!(
        seen[2].contains("a.txt"),
        "the write is the third artifact, between the two batches: {:?}",
        seen[2]
    );
    assert!(
        seen[3].contains("after") && !seen[3].contains("before"),
        "the fourth call must observe what the third wrote, which it can only do \
         if the write neither overlapped it nor was reordered past it: {:?}",
        seen[3]
    );
    assert_eq!(
        store.edits(result.run_id).unwrap().len(),
        1,
        "exactly one write landed"
    );
}

// ---------------------------------------------------------------- F4

/// A read-only tool that writes down the fact that it was entered at all.
///
/// F4 needs this rather than `read_file`, and the reason is a finding rather than
/// a preference: a batch that starts work past a pause and then folds only up to
/// the pause leaves *no* trace of the trailing built-in reads — a file read has no
/// side effect to catch it out. The sabotage that removes the collapse therefore
/// fails nothing against `read_file` and fails immediately here, where "was this
/// call started" is a question the fixture can answer.
struct Announcing {
    name: String,
    entered: Arc<std::sync::Mutex<Vec<String>>>,
}

impl Tool for Announcing {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Records that it ran.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.entered.lock().unwrap().push(self.name.clone());
            Ok(format!("{} ran", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

/// F4 — a pause inside a batch leaves no partial work behind.
///
/// Five read-only calls where the policy marks the third `Ask` and the approver
/// defers. The run stops `AwaitingApproval`; the store holds observations for the
/// two calls before it and none for the two after it, neither of which was even
/// entered; and resuming on approval proceeds from the deferred call.
#[tokio::test]
async fn a_pause_in_a_batch_records_nothing_for_the_calls_after_it() {
    use io_harness::approve::{Approver, Decision, DecisionFuture, Request};
    use io_harness::{Act, Effect};

    struct Defer;
    impl Approver for Defer {
        fn decide<'a>(&'a self, _r: &'a Request) -> DecisionFuture<'a> {
            Box::pin(async { Decision::Defer })
        }
    }

    let dir = ws();
    let names: Vec<String> = (1..=5).map(|i| format!("look_{i}")).collect();
    let entered = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut tools = Toolbox::new();
    for name in &names {
        tools = tools.with(Announcing {
            name: name.clone(),
            entered: Arc::clone(&entered),
        });
    }
    // `Ask` outranks `Allow`, so the third call is the one that stops.
    let asking = open_policy().rule(Act::Exec, Effect::Ask, "look_3");
    let script = || MockScript::new(vec![names.iter().map(|n| call(n)).collect()]);

    let path = dir.path().join("runs.db");
    let store = Store::open(&path).unwrap();
    let contract = never_passes(dir.path(), 2).with_tools(tools);
    let paused = run_with(&contract, &script(), &store, &asking, &Defer)
        .await
        .unwrap();
    let request_id = match paused.outcome {
        io_harness::RunOutcome::AwaitingApproval { request_id, .. } => request_id,
        other => panic!("expected AwaitingApproval, got {other:?}"),
    };

    let seen: Vec<String> = store
        .observations(paused.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    assert_eq!(
        seen.len(),
        2,
        "only the two calls before the deferred one may have been recorded: {seen:?}"
    );
    for early in &names[..2] {
        assert!(
            seen.iter().any(|t| t.contains(early.as_str())),
            "{early} ran before the pause and must be in the ledger: {seen:?}"
        );
    }
    for late in &names[2..] {
        assert!(
            !seen.iter().any(|t| t.contains(late.as_str())),
            "{late} is at or after the pause and must have left nothing behind: {seen:?}"
        );
    }
    assert_eq!(
        *entered.lock().unwrap(),
        names[..2].to_vec(),
        "a call at or after the pause must not have been started, let alone \
         recorded — the batch collapses from the first decision that is not an \
         outright allow"
    );
    assert_eq!(
        store.spent_tokens(paused.run_id).unwrap(),
        0,
        "the fixture provider reports no usage, so any draw here would be one the \
         trailing calls made"
    );

    // The human approves, and the run picks up from the call it stopped on.
    let resumed = io_harness::resume_with_decision(
        &contract,
        &script(),
        &store,
        paused.run_id,
        request_id,
        Decision::approve(),
        &asking,
        &ApproveAll,
    )
    .await
    .unwrap();
    let after: Vec<String> = store
        .observations(resumed.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    for released in &names[2..] {
        assert!(
            after.iter().any(|t| t.contains(released.as_str())),
            "{released} is at or after the approval and must run once it is \
             given: {after:?}"
        );
    }
}

// ---------------------------------------------------------------- F2 and N4

/// The eight files the recorded case reads.
const READS: &[&str] = &["a", "b", "c", "d", "e", "f", "g", "h"];

/// Put the workspace back exactly as it was, so every run of the recorded case
/// starts from the same tree and the requests it makes are identical — which is
/// what lets one recording answer all of them.
fn seed(root: &std::path::Path) {
    for name in READS {
        std::fs::write(
            root.join(format!("{name}.txt")),
            format!("contents of {name}\n"),
        )
        .unwrap();
    }
    for out in ["out_1.txt", "out_2.txt"] {
        let _ = std::fs::remove_file(root.join(out));
    }
}

/// Eight reads mixed with two writes, in one completion. The two writes split the
/// reads into two batches of four, so the case exercises the partition rather
/// than one long run of reads.
fn mixed_completion() -> Vec<ToolCall> {
    let read = |name: &str| ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": format!("{name}.txt") }),
    };
    let write = |path: &str, content: &str| ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    };
    let mut calls: Vec<ToolCall> = READS[..4].iter().map(|n| read(n)).collect();
    calls.push(write("out_1.txt", "first\n"));
    calls.extend(READS[4..].iter().map(|n| read(n)));
    calls.push(write("out_2.txt", "second\nthird\n"));
    calls
}

/// Everything the store holds about one run of the case, in one comparable value.
type Trace = (
    io_harness::RunOutcome,
    Vec<io_harness::StepRecord>,
    Vec<io_harness::context::Observation>,
    Vec<io_harness::Edit>,
    u64,
);

async fn drive(root: &std::path::Path, provider: &impl Provider, cap: usize) -> Trace {
    seed(root);
    let contract = never_passes(root, 1).with_max_parallel_reads(cap);
    let store = Store::memory().unwrap();
    let result = run_with(&contract, provider, &store, &open_policy(), &ApproveAll)
        .await
        .expect("the recorded case must replay");
    let id = result.run_id;
    (
        result.outcome,
        store.steps(id).unwrap(),
        store.observations(id).unwrap(),
        store.edits(id).unwrap(),
        store.spent_tokens(id).unwrap(),
    )
}

/// F2 — the trace is identical whether or not it ran concurrently, and N4 — it is
/// identical every time.
///
/// One recorded case, replayed into two stores: once at cap 10, once at cap 1.
/// The stored steps, the observations in the ledger, the decision strings, the
/// `Edit` rows, the ledger draw and the final `RunOutcome` compare equal row for
/// row. Not "equivalent" and not a normalisation — equality of what the store
/// holds.
///
/// Repeated twenty times, because concurrency that is deterministic nineteen
/// times out of twenty is a defect this release must not ship and one run cannot
/// see it.
#[tokio::test]
async fn a_batched_run_and_a_serial_run_leave_identical_traces_every_time() {
    let dir = ws();
    let path = dir.path().join("recording.json");

    // Record the case once, against a counter-keyed mock, then answer every
    // later run from the recording — which keys on the request's content, so a
    // run whose prompt differed at all would fail to be answered rather than
    // quietly diverge.
    seed(dir.path());
    let recorder = io_harness::provider::Record::new(MockScript::new(vec![mixed_completion()]));
    run_with(
        &never_passes(dir.path(), 1),
        &recorder,
        &Store::memory().unwrap(),
        &open_policy(),
        &ApproveAll,
    )
    .await
    .expect("the recording run must succeed");
    recorder.save(&path).unwrap();

    for round in 0..20 {
        let batched = drive(
            dir.path(),
            &io_harness::provider::Replay::load(&path).unwrap(),
            10,
        )
        .await;
        let serial = drive(
            dir.path(),
            &io_harness::provider::Replay::load(&path).unwrap(),
            1,
        )
        .await;
        assert_eq!(
            batched, serial,
            "round {round}: a run that batched its reads must leave exactly the \
             trace the serial run left — outcome, steps, observations, edits and \
             ledger draw"
        );
        assert_eq!(
            batched.3.len(),
            2,
            "round {round}: the case must actually have written, or the edit \
             comparison proves nothing"
        );
        assert_eq!(
            batched
                .2
                .iter()
                .filter(|o| o.kind == io_harness::context::ObsKind::Read)
                .count(),
            8,
            "round {round}: all eight reads must be in the ledger"
        );
    }
}

/// A registered tool is `Mutating` unless it says otherwise, so a toolbox
/// assembled before 0.41.0 keeps running one call at a time. Read here as well as
/// in the doctest because this is the property that makes the trait method
/// additive rather than a break in behaviour.
#[test]
fn a_tool_that_says_nothing_is_mutating() {
    struct Quiet;
    impl Tool for Quiet {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: "quiet".into(),
                description: "Says nothing about itself.".into(),
                parameters: json!({ "type": "object", "properties": {} }),
            }
        }
        fn invoke<'a>(&'a self, _a: &'a serde_json::Value) -> ToolFuture<'a> {
            Box::pin(async { Ok(String::new()) })
        }
    }
    assert_eq!(Quiet.effect(), ToolEffect::Mutating);
}
