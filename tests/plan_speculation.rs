//! 0.75.0 — what the plan phase leaves speculable, and what it does not.
//!
//! Two features meet here and neither was written with the other in mind.
//! `plan_lock` denies `Act::Exec` and `Act::Write` while a plan is unreviewed and
//! leaves `Act::Read` alone; speculation starts a completion's leading run of
//! read-only calls before the completion has settled, and asks the policy about
//! each one on exactly the terms `dispatch` would. So during planning a git
//! reader and a registered tool are not started early — their check is an
//! `Act::Exec` check — while `grep`, `find`, `read_file` and `list_dir` still
//! are.
//!
//! **The claim is the split, not the absence.** A test that only asserted
//! "nothing is speculated while a plan is outstanding" would pass against a
//! build that turned speculation off in a plan-gated run, which is a different
//! and worse behaviour: the reads a plan is written from are exactly the calls
//! that should still overlap. So the plan-gated run is asserted against the same
//! script with no gate at all, and the two differ in the four calls the lock
//! moves and in nothing else.
//!
//! Nothing here measures a duration.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{Tool, ToolEffect, ToolFuture, Toolbox};
use io_harness::{
    AcceptPlan, ApproveAll, EventKind, Flow, Observer, Policy, Provider, RunEvent, Session, Store,
    TaskContract, ToolSpec, PROPOSE_PLAN_TOOL,
};
use serde_json::json;

/// The sink `complete_streaming_calls` reports finished tool calls to. Named for
/// the same reason `tests/speculative_reads.rs` names it: written out, the type
/// trips `clippy::type_complexity` under `-D warnings`.
type CallSink<'a> = &'a (dyn Fn(usize, &ToolCall) + Send + Sync);

// ---------------------------------------------------------------- the provider

/// One scripted completion: what it reports while streaming, and what it settles
/// on. Both are the same here — this file is about which calls are *offered*,
/// not about what happens when a completion changes its mind.
#[derive(Clone, Default)]
struct Turn {
    calls: Vec<ToolCall>,
    text: Option<String>,
}

impl Turn {
    fn calls(calls: Vec<ToolCall>) -> Self {
        Self { calls, text: None }
    }

    fn done() -> Self {
        Self {
            calls: Vec::new(),
            text: Some("done".into()),
        }
    }
}

/// Serves a fixed script and reports every finished call as it goes, which is
/// what makes a run speculate at all.
struct Script {
    turns: Vec<Turn>,
    served: AtomicUsize,
}

impl Script {
    fn new(turns: Vec<Turn>) -> Self {
        Self {
            turns,
            served: AtomicUsize::new(0),
        }
    }

    async fn answer(
        &self,
        on_call: Option<CallSink<'_>>,
    ) -> io_harness::Result<CompletionResponse> {
        let i = self.served.fetch_add(1, Ordering::SeqCst);
        let turn = self.turns.get(i).cloned().unwrap_or_else(Turn::done);
        if let Some(sink) = on_call {
            for (at, call) in turn.calls.iter().enumerate() {
                sink(at, call);
                // As a real SSE read does between packets: it is what gives the
                // loop's select a chance to act on the call before this
                // completion returns.
                tokio::task::yield_now().await;
            }
        }
        Ok(CompletionResponse {
            text: turn.text.clone(),
            tool_calls: turn.calls.clone(),
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
        self.answer(Some(on_call)).await
    }

    fn name(&self) -> &str {
        "plan-script"
    }
}

// ------------------------------------------------------------------- the tool

/// A read-only registered tool that counts its own invocations. Registered
/// rather than built in on purpose: `speculable`'s registered arm is the other
/// half of the plan lock's reach, because it asks `Act::Exec` on the tool's own
/// name.
struct Probe {
    runs: Arc<AtomicUsize>,
}

impl Tool for Probe {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "probe".into(),
            description: "Observes nothing and counts itself.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
        }
    }

    fn invoke<'a>(&'a self, _arguments: &'a serde_json::Value) -> ToolFuture<'a> {
        Box::pin(async move {
            self.runs.fetch_add(1, Ordering::SeqCst);
            Ok("probe ran".to_string())
        })
    }

    fn effect(&self) -> ToolEffect {
        ToolEffect::ReadOnly
    }
}

// -------------------------------------------------------------- the scaffolding

#[derive(Default)]
struct Listener {
    speculated: Mutex<Vec<(usize, usize, usize)>>,
}

impl Observer for Listener {
    fn event(&self, event: &RunEvent) -> Flow {
        if let EventKind::Speculated {
            started,
            used,
            discarded,
        } = &event.kind
        {
            self.speculated
                .lock()
                .unwrap()
                .push((*started, *used, *discarded));
        }
        Flow::Continue
    }
}

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

/// A workspace that is also a repository, so `git_status` has a repository to
/// read and something to say about it.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "ALPHA").unwrap();
    std::fs::write(dir.path().join("b.txt"), "BRAVO").unwrap();
    let git = |args: &[&str]| {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git is on PATH");
        assert!(out.status.success(), "git {args:?}: {out:?}");
    };
    git(&["init", "--initial-branch=main"]);
    git(&["config", "user.email", "t@example.invalid"]);
    git(&["config", "user.name", "test"]);
    git(&["add", "a.txt"]);
    git(&["commit", "-m", "first"]);
    dir
}

fn policy() -> Policy {
    Policy::default()
        .layer("plan-speculation-test")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn call(name: &str, arguments: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments,
    }
}

/// The completion both tests serve: the four calls the plan lock leaves alone,
/// then the two whose check is an `Act::Exec` check. In that order, because
/// speculation is the completion's leading run and the split is only visible
/// where the boundary between the two falls inside one completion.
fn mixed_completion() -> Vec<ToolCall> {
    vec![
        call("grep", json!({ "pattern": "ALPHA" })),
        call("find", json!({ "name_glob": "*.txt" })),
        call("read_file", json!({ "path": "a.txt" })),
        call("list_dir", json!({ "path": "" })),
        call("git_status", json!({})),
        call("probe", json!({})),
    ]
}

const MUST_FINISH: Duration = Duration::from_secs(60);

fn contract(root: &std::path::Path, tools: Toolbox) -> TaskContract {
    TaskContract::workspace("exercise the plan phase", root)
        .with_tools(tools)
        .with_max_steps(4)
}

/// Every observation of a run, joined.
fn observations(store: &Store, run_id: i64) -> String {
    store
        .observations(run_id)
        .unwrap()
        .into_iter()
        .map(|o| o.text)
        .collect::<Vec<_>>()
        .join("\n")
}

// ------------------------------------------------------------------------ F14

/// F14 — while a plan is unreviewed the reads still start early and the calls
/// that reach the world do not.
///
/// `plan_lock` denies `Act::Exec` and leaves `Act::Read` untouched, so the four
/// leading calls are speculated and `git_status` — whose first question is
/// `Act::Exec` on `git` — closes speculation where it stands. The registered
/// tool behind it is refused on the same act and never runs.
///
/// The assertion is on the count and on both sides of the split. A build that
/// answered "speculate nothing while planning" produces `None` here, and a build
/// that speculated the git reader produces six.
#[tokio::test]
async fn f14_a_plan_gated_run_speculates_the_reads_and_stops_at_the_first_exec_check() {
    if !have_git() {
        return;
    }
    let ws = workspace();
    let store = Store::memory().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));

    let provider = Script::new(vec![
        Turn::calls(mixed_completion()),
        Turn::calls(vec![call(
            PROPOSE_PLAN_TOOL,
            json!({ "steps": [{ "intent": "read the repository" }] }),
        )]),
        Turn::done(),
    ]);
    let tools = Toolbox::new().with(Probe {
        runs: Arc::clone(&runs),
    });
    let listener = Listener::default();
    let mut session = Session::open(&store, ws.path()).unwrap();

    let turn = tokio::time::timeout(
        MUST_FINISH,
        session.turn_bounded_observed(
            &contract(ws.path(), tools).with_plan_gate(Arc::new(AcceptPlan)),
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
        listener.speculated.lock().unwrap().first().copied(),
        Some((4, 4, 0)),
        "the four calls the plan lock leaves as reads should have started early, and the \
         `Act::Exec` check behind them should have stopped there"
    );

    let all = observations(&store, turn.run_id);
    for marker in ["[grep", "[find", "[read a.txt]", "[list_dir "] {
        assert!(
            all.contains(marker),
            "a read the plan phase permits produced no observation ({marker}): {all}"
        );
    }
    assert!(
        all.contains("exec refused"),
        "the plan lock must refuse the calls that reach the world: {all}"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "a registered tool ran while its run was still waiting for an approved plan"
    );
}

/// F14's control: the same script with no plan gate speculates the whole
/// completion.
///
/// Without this the test above passes against a build where a git reader and a
/// registered tool are never speculated at all, which is the reading of F12 this
/// release exists to disprove. The only difference between the two runs is the
/// gate.
#[tokio::test]
async fn f14_the_same_completion_with_no_plan_gate_speculates_every_call_in_it() {
    if !have_git() {
        return;
    }
    let ws = workspace();
    let store = Store::memory().unwrap();
    let runs = Arc::new(AtomicUsize::new(0));

    let provider = Script::new(vec![Turn::calls(mixed_completion()), Turn::done()]);
    let tools = Toolbox::new().with(Probe {
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

    assert_eq!(
        listener.speculated.lock().unwrap().first().copied(),
        Some((6, 6, 0)),
        "with no plan outstanding every call in this completion is in the overlappable set"
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "the registered tool ran exactly once"
    );

    let all = observations(&store, turn.run_id);
    assert!(
        all.contains("[git_status]") && all.contains("probe ran"),
        "both of the calls the plan lock would have stopped ran here: {all}"
    );
    assert!(
        !all.contains("refused"),
        "nothing is refused without a gate: {all}"
    );
}
