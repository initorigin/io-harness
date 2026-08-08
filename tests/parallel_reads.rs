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
struct Counted {
    name: String,
    live: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

impl Counted {
    fn new(name: &str, live: &Arc<AtomicUsize>, peak: &Arc<AtomicUsize>) -> Self {
        Self {
            name: name.into(),
            live: Arc::clone(live),
            peak: Arc::clone(peak),
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
            // Two yields, so every task the bound admitted has a turn to count
            // itself in before the first one counts itself out.
            tokio::task::yield_now().await;
            tokio::task::yield_now().await;
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
    for name in &names {
        tools = tools.with(Counted::new(name, &live, &peak));
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
