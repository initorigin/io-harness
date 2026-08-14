//! 0.54.0 — read-only tool calls start off the provider's stream, before the
//! completion carrying them has returned.
//!
//! **Nothing here measures a duration.** A "it was faster" assertion on a CI
//! runner is a flake waiting to be written, and this repository has paid for that
//! lesson more times than any other. Earliness is proven where it is either
//! present or absent instead, with the technique `tests/parallel_reads.rs`
//! established for 0.41.0: the provider and the tool wait on a barrier shared
//! between them, so the completion can only return if the tool was started while
//! the stream was still open, and the pair deadlocks — bounded, so the test fails
//! rather than hanging the matrix — if it was not.
//!
//! The other half is that nothing observable moved. A speculated read must
//! produce the same trace, the same observations in the same order and the same
//! events as the serial run of the same recorded case, and a result must never be
//! folded under a call the model did not make.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::approve::DecisionFuture;
use io_harness::policy::{Act, Effect};
use io_harness::provider::Fallback;
use io_harness::{
    ApproveAll, Approver, Decision, Error, EventKind, Flow, Observer, Policy, Provider,
    ProviderErrorKind, Request, RetryPolicy, RunEvent, Session, Store, TaskContract, ToolSpec,
};
use serde_json::json;

// ---------------------------------------------------------------- the provider

/// One scripted completion: what it reports while streaming, and what it settles
/// on. The two are deliberately separate — a provider whose settled answer always
/// matches what it streamed cannot exercise the rule that decides whether a
/// speculated result may be used at all.
#[derive(Clone, Default)]
struct Turn {
    /// Handed to `on_call`, in order, before the completion returns.
    report: Vec<ToolCall>,
    /// What the finished `CompletionResponse` carries.
    settle: Vec<ToolCall>,
    /// Closing text. A turn with no calls and some text ends the conversation.
    text: Option<String>,
    /// Fail this turn's first `fails` attempts with a retryable error.
    fails: u32,
    /// Waited on after reporting and before returning. The tool waits on the same
    /// barrier, so the completion returns only if the tool was started early.
    gate: Option<Arc<tokio::sync::Barrier>>,
}

impl Turn {
    /// The common case: report exactly what will settle.
    fn calls(calls: Vec<ToolCall>) -> Self {
        Self {
            report: calls.clone(),
            settle: calls,
            ..Default::default()
        }
    }

    fn done() -> Self {
        Self {
            text: Some("done".into()),
            ..Default::default()
        }
    }

    fn gated(mut self, gate: &Arc<tokio::sync::Barrier>) -> Self {
        self.gate = Some(Arc::clone(gate));
        self
    }

    fn failing(mut self, times: u32) -> Self {
        self.fails = times;
        self
    }

    /// Report one thing and settle on another, which is the case F5 exists for.
    fn settling_on(mut self, settle: Vec<ToolCall>) -> Self {
        self.settle = settle;
        self
    }
}

/// Serves a fixed script, one turn per successful completion.
///
/// It implements all three completion methods on purpose. `complete` and
/// `complete_streaming` report no call at all, which is what every `Provider`
/// written before 0.54.0 does and therefore what the negative controls need;
/// `complete_streaming_calls` is the new one.
struct Script {
    turns: Vec<Turn>,
    /// Advanced only by a completion that succeeded, so a retry re-serves the
    /// same turn rather than skipping to the next.
    served: AtomicUsize,
    failures: Mutex<HashMap<usize, u32>>,
    /// Set once a completion has returned, for the "was anything run before the
    /// model finished speaking" probes.
    finished: Arc<AtomicBool>,
    /// How many times the new method was the one that served.
    reported_through: AtomicUsize,
}

impl Script {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns,
            served: AtomicUsize::new(0),
            failures: Mutex::new(HashMap::new()),
            finished: Arc::new(AtomicBool::new(false)),
            reported_through: AtomicUsize::new(0),
        }
    }

    async fn answer(
        &self,
        on_call: Option<&(dyn Fn(usize, &ToolCall) + Send + Sync)>,
    ) -> io_harness::Result<CompletionResponse> {
        let i = self.served.load(Ordering::SeqCst);
        let turn = self.turns.get(i).cloned().unwrap_or_else(Turn::done);

        if let Some(sink) = on_call {
            for (at, call) in turn.report.iter().enumerate() {
                sink(at, call);
                // As a real SSE read does between packets: it is what gives the
                // loop's select a chance to act on the call before this
                // completion returns, and therefore what makes every earliness
                // assertion in this file mean anything.
                tokio::task::yield_now().await;
            }
        }

        if let Some(gate) = &turn.gate {
            gate.wait().await;
        }

        if turn.fails > 0 {
            let mut failures = self.failures.lock().unwrap();
            let seen = failures.entry(i).or_insert(0);
            if *seen < turn.fails {
                *seen += 1;
                return Err(Error::Provider {
                    kind: ProviderErrorKind::Server,
                    status: Some(503),
                    retry_after: None,
                    message: "scripted mid-stream failure".into(),
                });
            }
        }

        self.served.fetch_add(1, Ordering::SeqCst);
        self.finished.store(true, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: turn.text.clone(),
            tool_calls: turn.settle.clone(),
            ..Default::default()
        })
    }
}

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.answer(None).await
    }

    async fn complete_streaming(
        &self,
        _req: CompletionRequest,
        _on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.answer(None).await
    }

    async fn complete_streaming_calls(
        &self,
        _req: CompletionRequest,
        _on_token: &(dyn Fn(&str) + Send + Sync),
        on_call: &(dyn Fn(usize, &ToolCall) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.reported_through.fetch_add(1, Ordering::SeqCst);
        self.answer(Some(on_call)).await
    }

    fn name(&self) -> &str {
        "script"
    }
}

/// A provider that never heard of 0.54.0: `complete` and `complete_streaming`
/// only, so the trait's default is what serves the call sink — and the default
/// reports nothing. This is what `Record`, `Replay` and every out-of-tree
/// implementation are.
struct Deaf {
    turns: Vec<Turn>,
    served: AtomicUsize,
}

impl Deaf {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns,
            served: AtomicUsize::new(0),
        }
    }

    fn answer(&self) -> io_harness::Result<CompletionResponse> {
        let i = self.served.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.get(i).cloned().unwrap_or_else(Turn::done);
        Ok(CompletionResponse {
            text: turn.text.clone(),
            tool_calls: turn.settle.clone(),
            ..Default::default()
        })
    }
}

impl Provider for Deaf {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.answer()
    }

    async fn complete_streaming(
        &self,
        _req: CompletionRequest,
        _on_token: &(dyn Fn(&str) + Send + Sync),
    ) -> io_harness::Result<CompletionResponse> {
        self.answer()
    }

    fn name(&self) -> &str {
        "deaf"
    }
}

// ------------------------------------------------------------------- the tools

/// A read-only tool that cannot finish alone: it waits on a barrier shared with
/// the provider's own stream. It completes only if it was started while the
/// completion was still open.
struct Rendezvous {
    name: String,
    barrier: Arc<tokio::sync::Barrier>,
}

impl Tool for Rendezvous {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Waits for the stream it was called from.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.barrier.wait().await;
            Ok(format!("{} met the stream", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

/// A read-only tool that counts how many times it actually ran.
struct Counted {
    name: String,
    runs: Arc<AtomicUsize>,
}

impl Tool for Counted {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.name.clone(),
            description: "Counts its own invocations.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok(format!("{} ran", self.name))
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

// -------------------------------------------------------------- the scaffolding

/// Records the ordered event stream, and the one new event's numbers.
#[derive(Default)]
struct Listener {
    kinds: Mutex<Vec<String>>,
    tool_calls: Mutex<Vec<String>>,
    speculated: Mutex<Vec<(usize, usize, usize)>>,
}

impl Observer for Listener {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::ToolCall { name, target } => {
                self.kinds.lock().unwrap().push("tool_call".into());
                self.tool_calls
                    .lock()
                    .unwrap()
                    .push(format!("{name}:{target}"));
            }
            EventKind::Speculated {
                started,
                used,
                discarded,
            } => {
                self.kinds.lock().unwrap().push("speculated".into());
                self.speculated
                    .lock()
                    .unwrap()
                    .push((*started, *used, *discarded));
            }
            other => {
                let tag = serde_json::to_value(RunEvent::new(1, 1, other.clone())).unwrap()["event"]
                    .as_str()
                    .unwrap()
                    .to_string();
                self.kinds.lock().unwrap().push(tag);
            }
        }
        Flow::Continue
    }
}

impl Listener {
    /// The totals from the one `Speculated` event, or zeroes when none was
    /// emitted. A step that speculated nothing must emit nothing, so "no event"
    /// and "an event of zeroes" are different facts and only the first is right.
    fn counts(&self) -> Option<(usize, usize, usize)> {
        self.speculated.lock().unwrap().first().copied()
    }
}

fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "ALPHA").unwrap();
    std::fs::write(dir.path().join("b.txt"), "BRAVO").unwrap();
    dir
}

fn policy() -> Policy {
    Policy::default()
        .layer("speculation-test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn read(path: &str) -> ToolCall {
    ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path }),
    }
}

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

fn tool(name: &str) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: json!({}),
    }
}

/// A contract that streams — which is what a session turn does — with a fast
/// retry so the one test that retries does not spend its budget asleep.
fn contract(root: &std::path::Path, tools: Toolbox) -> TaskContract {
    TaskContract::workspace("exercise speculation", root)
        .with_tools(tools)
        .with_max_steps(4)
        .with_retry_policy(RetryPolicy {
            base: Duration::from_millis(1),
            max: Duration::from_millis(1),
        })
}

/// Generous, because it bounds a turn that is expected to *finish* and the
/// slowest runner in the matrix is several times slower than a developer's
/// machine.
const MUST_FINISH: Duration = Duration::from_secs(60);

/// Short, because it bounds a turn that is expected never to finish: nothing in
/// the serial path can complete a rendezvous with the stream that called it, so
/// waiting longer only makes a failing matrix slower.
const MUST_NOT_FINISH: Duration = Duration::from_secs(5);

// ------------------------------------------------------------------------- F1

/// F1 — a read-only call runs while the stream is still open.
///
/// The provider reports one call and then waits on a barrier of two. The tool
/// waits on the same barrier. Neither can proceed alone, so the turn completes
/// only if the tool was started before the completion returned. This is the whole
/// release, asserted without a clock.
#[tokio::test]
async fn a_read_only_call_runs_before_the_completion_returns() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let gate = Arc::new(tokio::sync::Barrier::new(2));

    let provider = Script::new(vec![
        Turn::calls(vec![tool("peek")]).gated(&gate),
        Turn::done(),
    ]);
    let tools = Toolbox::new().with(Rendezvous {
        name: "peek".into(),
        barrier: Arc::clone(&gate),
    });
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), tools),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the tool never met the stream, so the read did not start early")
    .unwrap();

    assert_eq!(
        listener.counts(),
        Some((1, 1, 0)),
        "one call should have been started early and used"
    );
    assert_eq!(
        provider.reported_through.load(Ordering::SeqCst),
        2,
        "the new method should have served every completion"
    );
    assert!(turn.run_id > 0);
}

// ------------------------------------------------------------------------- F9

/// F9 — `with_max_parallel_reads(1)` turns starting early off with the batching.
///
/// The same script and the same barrier. With the cap at one, nothing is
/// speculated, so the tool is only reached after the completion returns — and the
/// completion is waiting on the tool. The turn cannot finish, which is the
/// negative control F1 needs to mean anything.
#[tokio::test]
async fn a_cap_of_one_starts_nothing_early() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let gate = Arc::new(tokio::sync::Barrier::new(2));

    let provider = Script::new(vec![
        Turn::calls(vec![tool("peek")]).gated(&gate),
        Turn::done(),
    ]);
    let tools = Toolbox::new().with(Rendezvous {
        name: "peek".into(),
        barrier: Arc::clone(&gate),
    });
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let outcome = tokio::time::timeout(
        MUST_NOT_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), tools).with_max_parallel_reads(1),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await;

    assert!(
        outcome.is_err(),
        "the turn finished at a cap of 1, so something was started early after all"
    );
    assert_eq!(
        listener.counts(),
        None,
        "a run that speculates nothing must emit no Speculated event at all"
    );
}

// ------------------------------------------------------------------------- F2

/// F2 — speculation stops at the first call that is not read-only, and the read
/// after a write sees what the write wrote.
///
/// The completion is `[read a, write a, read a]` and the provider reports all
/// three, including the write: an eager provider is allowed to report anything
/// and the harness is what must be narrow. Only the leading read may start early.
/// The sabotage is 0.41.0's maximal-run rule, under which the trailing read also
/// starts early and returns the bytes from before the write — a wrong value, not
/// merely a wrong order.
#[tokio::test]
async fn speculation_stops_at_the_first_call_that_is_not_read_only() {
    let ws = workspace();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        Turn::calls(vec![read("a.txt"), write("a.txt", "OMEGA"), read("a.txt")]),
        Turn::done(),
    ]);
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), Toolbox::new()),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    assert_eq!(
        listener.counts(),
        Some((1, 1, 0)),
        "only the leading read may be started early"
    );

    let reads: Vec<String> = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .filter(|o| o.text.contains("[read a.txt]"))
        .map(|o| o.text)
        .collect();
    assert_eq!(reads.len(), 2, "both reads should have produced an observation");
    assert!(
        reads[0].contains("ALPHA"),
        "the first read happens before the write: {}",
        reads[0]
    );
    assert!(
        reads[1].contains("OMEGA"),
        "the read AFTER the write must see what the write wrote, not the bytes from before it: {}",
        reads[1]
    );
}

// ------------------------------------------------------------------------- F5

/// F5 — a settled call whose arguments differ from the streamed ones discards the
/// speculation.
///
/// The provider streams `read_file{a.txt}` and settles on `read_file{b.txt}`. The
/// observation must carry b's contents. The sabotage is keying a speculated
/// result by its position alone, under which a's bytes are folded under b's call —
/// the one defect this release can ship that a model cannot detect, because the
/// observation has the shape of a successful read either way.
#[tokio::test]
async fn a_settled_call_with_different_arguments_discards_the_speculation() {
    let ws = workspace();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        Turn::calls(vec![read("a.txt")]).settling_on(vec![read("b.txt")]),
        Turn::done(),
    ]);
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), Toolbox::new()),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    assert_eq!(
        listener.counts(),
        Some((1, 0, 1)),
        "the speculated call should have been started and then thrown away"
    );

    let texts: Vec<String> = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect();
    let all = texts.join("\n");
    assert!(
        all.contains("BRAVO"),
        "the settled call's own file must be the one read: {all}"
    );
    assert!(
        !all.contains("ALPHA"),
        "the speculated file's bytes were folded under a call the model did not make: {all}"
    );
}

/// F5, second arm — a settled call with a different *name* at the same position is
/// discarded too, and so is a completion shorter than what was streamed.
#[tokio::test]
async fn a_settled_call_with_a_different_name_or_a_shorter_completion_discards_it() {
    for (label, turn) in [
        (
            "a different name at the same position",
            Turn::calls(vec![read("a.txt")]).settling_on(vec![tool("counter")]),
        ),
        (
            "a completion shorter than what was streamed",
            Turn::calls(vec![read("a.txt"), read("b.txt")]).settling_on(vec![]),
        ),
    ] {
        let ws = workspace();
        let store = Store::memory().unwrap();
        let runs = Arc::new(AtomicUsize::new(0));
        let provider = Script::new(vec![turn, Turn::done()]);
        let tools = Toolbox::new().with(Counted {
            name: "counter".into(),
            runs: Arc::clone(&runs),
        });
        let listener = Listener::default();
        let mut session = Session::open(&store, ws.path()).unwrap();

        let turn = tokio::time::timeout(
            MUST_FINISH,
            session.turn_bounded_observed(
                &contract(ws.path(), tools),
                &provider,
                &store,
                &policy(),
                &ApproveAll,
                &listener,
            ),
        )
        .await
        .unwrap_or_else(|_| panic!("{label}: the turn should finish"))
        .unwrap();

        let (_, used, discarded) = listener
            .counts()
            .unwrap_or_else(|| panic!("{label}: something should have been speculated"));
        assert_eq!(used, 0, "{label}: nothing speculated may be used");
        assert!(discarded > 0, "{label}: the speculation should be discarded");

        let all = store
            .observations(turn.run_id)
            .unwrap()
            .into_iter()
            .map(|o| o.text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains("ALPHA"),
            "{label}: a discarded speculation reached the run anyway: {all}"
        );
    }
}

// ------------------------------------------------------------------------- F4

/// F4 — a completion that never settles leaves no trace of the work it caused.
///
/// The first attempt reports two reads and then fails with a retryable error; the
/// retry reports the same two and succeeds. The reads therefore happen four times
/// and the run records two observations — and the event says so, which is the
/// point of having it: work paid for and thrown away is exactly what an operator
/// cannot otherwise see.
#[tokio::test]
async fn a_failed_attempt_leaves_no_observation_behind() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));

    let provider = Script::new(vec![
        Turn::calls(vec![tool("counter"), tool("counter")]).failing(1),
        Turn::done(),
    ]);
    let tools = Toolbox::new().with(Counted {
        name: "counter".into(),
        runs: Arc::clone(&runs),
    });
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), tools),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    let (started, used, discarded) = listener.counts().expect("something was speculated");
    assert_eq!(
        (started, used, discarded),
        (4, 2, 2),
        "the abandoned attempt's reads must be counted as started and discarded"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        4,
        "the tool really did run four times — twice for an attempt that was thrown away"
    );

    let folded = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .filter(|o| o.text.contains("counter ran"))
        .count();
    assert_eq!(
        folded, 2,
        "the run must record exactly the completion that settled, not the one that failed"
    );
}

// ------------------------------------------------------------------------- F7

/// F7 — a denied call is never executed early.
///
/// The policy denies the tool by name. Nothing may run it, and the assertion is on
/// the tool's own counter rather than on the refusal text: an implementation that
/// reported a refusal it never enforced satisfies the second and fails the first.
#[tokio::test]
async fn a_denied_call_is_never_started_early() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));

    let provider = Script::new(vec![Turn::calls(vec![tool("secret")]), Turn::done()]);
    let tools = Toolbox::new().with(Counted {
        name: "secret".into(),
        runs: Arc::clone(&runs),
    });
    let denied = Policy::default()
        .layer("speculation-test")
        .allow_read("*")
        .deny_exec("secret");
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), tools),
            &provider,
            &store,
            &denied,
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a call the policy denies was executed anyway, and merely not recorded"
    );
    assert_eq!(
        listener.counts(),
        None,
        "nothing should have been speculated, so no event should have been emitted"
    );
}

// ------------------------------------------------------------------------- F3

/// F3 — the trace is the trace it would have been, exactly.
///
/// The same recorded case is run twice at the **same** cap: once with a provider
/// that reports its finished calls and once with one that does not. That is the
/// control the claim needs. Comparing a speculating run against
/// `with_max_parallel_reads(1)` instead would compare 0.41.0's batch path against
/// the serial one, which already differ — `dispatch` emits an
/// `EventKind::Contained` per call where `read_batch` does not — and would fail
/// for a reason this release did not cause.
///
/// The step rows, the observations and the ordered `ToolCall` events must be
/// identical, and the only permitted difference in the event stream is the
/// presence of `Speculated`.
#[tokio::test]
async fn the_trace_is_identical_whether_or_not_a_read_started_early() {
    async fn once(speculating: bool) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
        let ws = workspace();
        let store = Store::memory().unwrap();
        let turns = vec![
            Turn::calls(vec![read("a.txt"), read("b.txt"), write("c.txt", "GAMMA")]),
            Turn::done(),
        ];
        let listener = Listener::default();
        let mut session = Session::open(&store, ws.path()).unwrap();
        let contract = contract(ws.path(), Toolbox::new());

        let turn = if speculating {
            session
                .turn_bounded_observed(
                    &contract,
                    &Script::new(turns),
                    &store,
                    &policy(),
                    &ApproveAll,
                    &listener,
                )
                .await
                .unwrap()
        } else {
            session
                .turn_bounded_observed(
                    &contract,
                    &Deaf::new(turns),
                    &store,
                    &policy(),
                    &ApproveAll,
                    &listener,
                )
                .await
                .unwrap()
        };

        let steps = store
            .steps(turn.run_id)
            .unwrap()
            .into_iter()
            .map(|s| format!("{}|{}|{}", s.step, s.decision, s.tool_call))
            .collect();
        let observations = store
            .observations(turn.run_id)
            .unwrap()
            .into_iter()
            .map(|o| format!("{}|{:?}|{}", o.step, o.target, o.text))
            .collect();
        let calls = listener.tool_calls.lock().unwrap().clone();
        let kinds = listener.kinds.lock().unwrap().clone();
        (steps, observations, calls, kinds)
    }

    let (fast_steps, fast_obs, fast_calls, fast_kinds) = once(true).await;
    let (slow_steps, slow_obs, slow_calls, slow_kinds) = once(false).await;

    assert_eq!(fast_steps, slow_steps, "the step rows differ");
    assert_eq!(fast_obs, slow_obs, "the observations differ");
    assert_eq!(
        fast_calls, slow_calls,
        "the tool calls were announced in a different order"
    );

    let stripped: Vec<String> = fast_kinds
        .iter()
        .filter(|k| *k != "speculated")
        .cloned()
        .collect();
    assert_eq!(
        stripped, slow_kinds,
        "the event streams differ by something other than the Speculated event"
    );
    assert!(
        fast_kinds.iter().any(|k| k == "speculated"),
        "the run that could speculate should have said so"
    );
}

// ------------------------------------------------------------------------- F8

/// F8 — a provider that does not implement the new method is 0.53.0's run.
///
/// `Deaf` overrides `complete` and `complete_streaming` and nothing else, so the
/// trait default serves the call sink and the default reports no call. This is the
/// shape `Record` and `Replay` have, which is why a replayed run speculates
/// nothing by construction rather than by anyone remembering to suppress it.
#[tokio::test]
async fn a_provider_without_the_new_method_speculates_nothing() {
    let ws = workspace();
    let store = Store::memory().unwrap();
    let provider = Deaf::new(vec![
        Turn::calls(vec![read("a.txt"), read("b.txt")]),
        Turn::done(),
    ]);
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = session
        .turn_bounded_observed(
            &contract(ws.path(), Toolbox::new()),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        )
        .await
        .unwrap();

    assert_eq!(
        listener.counts(),
        None,
        "the trait default reported a call, so speculation reached a provider that never opted in"
    );
    assert!(
        !listener.kinds.lock().unwrap().iter().any(|k| k == "speculated"),
        "no Speculated event may appear for a provider that reports no calls"
    );
    // The reads still happened, through the ordinary path.
    let all = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("ALPHA") && all.contains("BRAVO"));
}

// ------------------------------------------------------------------------- F6

/// Approves everything, and records whether the completion had already returned
/// by the time it was asked.
struct Watchful {
    consulted: Arc<AtomicUsize>,
    asked_after_the_completion: Arc<AtomicUsize>,
    finished: Arc<AtomicBool>,
}

impl Approver for Watchful {
    fn decide<'a>(&'a self, _request: &'a Request) -> DecisionFuture<'a> {
        Box::pin(async move {
            self.consulted.fetch_add(1, Ordering::SeqCst);
            if self.finished.load(Ordering::SeqCst) {
                self.asked_after_the_completion
                    .fetch_add(1, Ordering::SeqCst);
            }
            Decision::Approve {
                modified: None,
                remember: Vec::new(),
            }
        })
    }
}

/// F6 — no approver is asked about an unsettled completion, and a grey-tier call
/// is not speculated.
///
/// The completion reads a permitted path and then one the policy asks about. The
/// first is started early; the second is not, and the approver is consulted once,
/// after the completion returned. Both halves in one test, since the claim is the
/// distinction — a build that speculated neither would satisfy the second alone.
#[tokio::test]
async fn a_grey_tier_call_is_not_speculated_and_its_approver_is_asked_late() {
    let ws = workspace();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        Turn::calls(vec![read("a.txt"), read("b.txt")]),
        Turn::done(),
    ]);
    let consulted = Arc::new(AtomicUsize::new(0));
    let late = Arc::new(AtomicUsize::new(0));
    let approver = Watchful {
        consulted: Arc::clone(&consulted),
        asked_after_the_completion: Arc::clone(&late),
        finished: Arc::clone(&provider.finished),
    };
    let asking = Policy::default()
        .layer("speculation-test")
        .allow_read("a.txt")
        .rule(Act::Read, Effect::Ask, "b.txt");
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), Toolbox::new()),
            &provider,
            &store,
            &asking,
            &approver,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    assert_eq!(
        listener.counts(),
        Some((1, 1, 0)),
        "only the outright-allowed read may be started early"
    );
    assert_eq!(
        consulted.load(Ordering::SeqCst),
        1,
        "the approver should have been consulted exactly once"
    );
    assert_eq!(
        late.load(Ordering::SeqCst),
        1,
        "the approver was asked about a completion that had not settled yet"
    );

    // Both reads still happened, in order, through the paths each belongs to.
    let all = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(all.contains("ALPHA") && all.contains("BRAVO"));
}

// ------------------------------------------------------------------------ F11

/// F11 — a `Fallback` discards what the primary speculated when it falls over.
///
/// The primary reports a read and then fails; the secondary answers with a
/// different call. Only the secondary's call may be folded — the settled
/// completion is the secondary's, and the primary's speculated result matches
/// nothing in it.
#[tokio::test]
async fn a_fallover_folds_nothing_the_primary_speculated() {
    let ws = workspace();
    let store = Store::memory().unwrap();

    // Always fails, so every completion in this turn is served by the secondary
    // and the primary's report is only ever work thrown away.
    let primary = Script::new(vec![Turn::calls(vec![read("a.txt")]).failing(u32::MAX)]);
    let secondary = Script::new(vec![Turn::calls(vec![read("b.txt")]), Turn::done()]);
    let provider = Fallback::new(primary, secondary);
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), Toolbox::new()),
            &provider,
            &store,
            &policy(),
            &ApproveAll,
            &listener,
        ),
    )
    .await
    .expect("the turn should finish")
    .unwrap();

    assert_eq!(
        listener.counts(),
        Some((1, 0, 1)),
        "the primary's speculation should have been started and then discarded"
    );

    let all = store
        .observations(turn.run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        all.contains("BRAVO"),
        "the secondary's own call must be the one that ran: {all}"
    );
    assert!(
        !all.contains("ALPHA"),
        "a result speculated off the failed primary was folded under the secondary's call: {all}"
    );
}
