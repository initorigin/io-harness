//! A program through the full loop — F1, F2, F9, F11, F13 — with a scripted
//! mock provider so the tests are deterministic and offline.
//!
//! What is asserted here is the shape of the capability rather than its
//! boundary: that a program is one tool call inside one step, that a host with
//! no interpreter is a supported host rather than a broken one, that the two
//! harness-side bounds actually stop a program, and that a program which raises
//! comes back as something the model can act on. The boundary itself is
//! `tests/codeact_boundary.rs`, which is the file that decides whether this
//! release is honest.
#![cfg(feature = "codeact")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{
    run_with, run_with_observed, ApproveAll, CodeActConfig, EventKind, Flow, Observer, Provider,
    RunEvent, Store, TaskContract,
};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<ToolSpec>>,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
        }
    }

    fn spec(&self, name: &str) -> Option<ToolSpec> {
        self.offered
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.name == name)
            .cloned()
    }

    /// How many completions were asked for — one per step.
    fn turns(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = req.tools.clone();
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// One `Program` event, kept as a value rather than a tuple so the tests below
/// read by name.
#[derive(Clone)]
struct Prog {
    interpreter: Option<String>,
    detail: String,
    calls: u32,
    outcome: String,
}

/// Collects the `Program` events and the name of every tool call, which is what
/// most of these tests read their evidence out of.
#[derive(Default)]
struct Collect {
    programs: Mutex<Vec<Prog>>,
    tool_calls: Mutex<Vec<String>>,
}

impl Collect {
    fn outcomes(&self) -> Vec<String> {
        self.programs
            .lock()
            .unwrap()
            .iter()
            .map(|p| p.outcome.clone())
            .collect()
    }

    fn called(&self, name: &str) -> usize {
        self.tool_calls
            .lock()
            .unwrap()
            .iter()
            .filter(|n| *n == name)
            .count()
    }
}

impl Observer for Collect {
    fn event(&self, event: &RunEvent) -> Flow {
        match &event.kind {
            EventKind::Program {
                interpreter,
                detail,
                calls,
                outcome,
            } => self.programs.lock().unwrap().push(Prog {
                interpreter: interpreter.clone(),
                detail: detail.clone(),
                calls: *calls,
                outcome: outcome.clone(),
            }),
            EventKind::ToolCall { name, .. } => self.tool_calls.lock().unwrap().push(name.clone()),
            _ => {}
        }
        Flow::Continue
    }
}

fn ws() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("one.txt"), "first\n").unwrap();
    std::fs::write(dir.path().join("two.txt"), "second\n").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("summarise the workspace", root).with_max_steps(6)
}

fn permissive() -> Policy {
    Policy::permissive()
}

fn program(source: &str) -> ToolCall {
    ToolCall {
        name: "run_program".into(),
        arguments: json!({ "source": source }),
    }
}

/// A machine without Python is a supported machine, so these skip rather than
/// fail there — a red suite on such a host would be this crate asserting a
/// property of the host it explicitly says it does not require.
fn skip_without_python() -> bool {
    let found = io_harness::CODEACT_CANDIDATES.iter().any(|c| {
        std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).any(|d| d.join(c).is_file()))
            .unwrap_or(false)
    });
    if !found {
        eprintln!("no host interpreter; skipping");
    }
    !found
}

// ---------------------------------------------------------------------------
// F1 / F2 — a program is a tool call, and it is one step
// ---------------------------------------------------------------------------

/// F2, and the release's own value claim measured as structure rather than as
/// tokens: three acts inside one program take **one** provider turn, and the
/// same three acts as ordinary tool calls take three.
///
/// The control is the whole test. "A program did three things in one step" is
/// only interesting beside the run that needed three steps for it, and the pair
/// is what the token comparison in `docs/MEASUREMENTS.md` rests on.
#[tokio::test]
async fn three_acts_in_one_program_take_one_turn_where_three_calls_take_three() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let by_program = MockScript::new(vec![vec![program(
        "a = read_file(path=\"one.txt\")\n\
         b = read_file(path=\"two.txt\")\n\
         c = list_dir(path=\".\")\n\
         print(len(str(a)) + len(str(b)) + len(str(c)))\n",
    )]]);
    run_with(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &by_program,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let chain_dir = ws();
    let chain_store = Store::memory().unwrap();
    let by_chain = MockScript::new(vec![
        vec![ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": "one.txt" }),
        }],
        vec![ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": "two.txt" }),
        }],
        vec![ToolCall {
            name: "list_dir".into(),
            arguments: json!({ "path": "." }),
        }],
    ]);
    run_with(
        &contract(chain_dir.path()),
        &by_chain,
        &chain_store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        by_program.turns() < by_chain.turns(),
        "a program should collapse turns: program took {}, the chain took {}",
        by_program.turns(),
        by_chain.turns()
    );
}

/// F1. The tool is advertised with a schema whose single required argument is the
/// program's source, and the callable names are written into the description
/// rather than left for the model to guess.
#[tokio::test]
async fn the_tool_names_what_a_program_may_call() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![]]);
    run_with(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spec = provider
        .spec("run_program")
        .expect("the tool is offered when an interpreter was found");
    assert_eq!(spec.parameters["required"], json!(["source"]));
    assert_eq!(spec.parameters["properties"]["source"]["type"], "string");
    // The names a program can call are in the description, and `exec` is the one
    // worth pinning: it was silently missing from an earlier version of the
    // generated surface while everything else still worked.
    for name in ["read_file", "grep", "write_file", "exec"] {
        assert!(
            spec.description.contains(name),
            "the description should name {name}; it was {:?}",
            spec.description
        );
    }
    // And the uncallable set is not advertised as callable.
    assert!(
        !spec.description.contains("todo_write"),
        "an uncallable tool must not be named as one a program may call"
    );
}

// ---------------------------------------------------------------------------
// F9 — a host with no interpreter is a supported host
// ---------------------------------------------------------------------------

/// F9. Discovery fails, the tool is never offered, and the run proceeds exactly
/// as it would have with the feature off — with the decision on the record.
///
/// The interpreter is named rather than absent, so the test does not depend on
/// what this machine happens to have on `PATH`: a named interpreter that does
/// not answer is not silently replaced by a candidate, which is itself the
/// behaviour under test.
#[tokio::test]
async fn a_host_with_no_usable_interpreter_is_offered_no_program_and_runs_anyway() {
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": "one.txt" }),
    }]]);
    let seen = Collect::default();

    let missing = dir.path().join("definitely-not-an-interpreter");
    let result = run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default().with_interpreter(&missing)),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    assert!(
        provider.spec("run_program").is_none(),
        "a host with no usable interpreter must not be offered the tool"
    );
    // The run did its ordinary work regardless — the fallback is the turn running
    // as it always did, not a degraded one.
    assert_eq!(
        seen.called("read_file"),
        1,
        "the run should have gone on doing its work"
    );
    assert!(result.run_id > 0);

    // And the decision is readable rather than inferred.
    let first = seen
        .programs
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("discovery emits an event either way");
    assert_eq!(first.outcome, "withheld");
    assert_eq!(first.interpreter, None);
    assert_eq!(first.calls, 0);
    assert!(
        first.detail.contains("definitely-not-an-interpreter"),
        "the event should name what was tried; it said {:?}",
        first.detail
    );
}

/// F9's other half, and the control for the test above: with discovery allowed to
/// find the host's own interpreter, the tool IS offered and the event says so.
/// Without this, "the tool was withheld" would also be true of a build where the
/// tool was never wired up at all.
#[tokio::test]
async fn a_host_with_an_interpreter_is_offered_the_tool_and_says_which() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![]]);
    let seen = Collect::default();
    run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    assert!(provider.spec("run_program").is_some());
    let first = seen
        .programs
        .lock()
        .unwrap()
        .first()
        .cloned()
        .expect("an event");
    assert_eq!(first.outcome, "available");
    assert!(first.interpreter.is_some(), "the resolved path is recorded");
    assert!(
        first.detail.contains("Python 3."),
        "the probed version is recorded; it said {:?}",
        first.detail
    );
}

// ---------------------------------------------------------------------------
// F11 — the callback bound
// ---------------------------------------------------------------------------

/// F11. A program that calls in a tight loop is stopped at the bound, and the
/// model is told which bound it hit rather than being handed a silence.
#[tokio::test]
async fn a_program_that_loops_is_stopped_at_the_callback_bound() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![program(
        "for i in range(500):\n    read_file(path=\"one.txt\")\nprint(\"never reached\")\n",
    )]]);
    let seen = Collect::default();

    run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default().with_max_callbacks(3)),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    let ran = seen
        .programs
        .lock()
        .unwrap()
        .iter()
        .find(|p| p.outcome != "available" && p.outcome != "withheld")
        .cloned()
        .expect("the program that ran emits its own event");
    assert_eq!(ran.outcome, "bound", "the run should end at the bound");
    assert_eq!(ran.calls, 3, "exactly the bound's worth of calls were made");
    // The control: the acts it did make really were made, so the bound stopped a
    // working program rather than a broken one.
    assert_eq!(
        seen.called("read_file"),
        3,
        "the calls under the bound reached dispatch"
    );
}

/// F11's other half, and the one a callback bound cannot cover: a program that
/// spins without ever calling back.
///
/// There is no frame to check a deadline between, and nothing underneath would
/// stop it — `SandboxLimits` is `none()` on a default `TaskContract`, so a
/// contained program has no wall cap and an uncontained one has no rlimits at
/// all. The wait itself is therefore what is bounded. Without that this test
/// hangs for ever rather than failing, which is why it is here.
#[tokio::test]
async fn a_program_that_never_calls_back_is_stopped_by_the_clock() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![program("while True:\n    pass\n")]]);
    let seen = Collect::default();

    let ran = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        run_with_observed(
            &contract(dir.path()).with_codeact(
                CodeActConfig::default().with_timeout(std::time::Duration::from_secs(2)),
            ),
            &provider,
            &store,
            &permissive(),
            &ApproveAll,
            &seen,
        ),
    )
    .await;

    assert!(
        ran.is_ok(),
        "a program that never calls back must be stopped by its own deadline, not hang the run"
    );
    ran.unwrap().unwrap();

    let outcomes = seen.outcomes();
    assert!(
        outcomes.contains(&"timeout".to_string()),
        "the program should have ended on the clock; outcomes were {outcomes:?}"
    );
}

// ---------------------------------------------------------------------------
// F13 — a program that raises is feedback, not a dead step
// ---------------------------------------------------------------------------

/// F13. The traceback reaches the model, and the model may send a corrected
/// program in the next step, which then runs.
#[tokio::test]
async fn a_program_that_raises_can_be_corrected_on_the_next_step() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![
        vec![program(
            "raise ValueError(\"the first attempt is wrong\")\n",
        )],
        vec![program(
            "r = read_file(path=\"one.txt\")\nprint(\"second attempt:\", str(r))\n",
        )],
    ]);
    let seen = Collect::default();

    run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    let outcomes = seen.outcomes();
    assert!(
        outcomes.contains(&"failed".to_string()),
        "the first program raised; outcomes were {outcomes:?}"
    );
    assert!(
        outcomes.contains(&"finished".to_string()),
        "the corrected program ran; outcomes were {outcomes:?}"
    );
    // The corrected program's act reached dispatch, so the run really did carry on.
    assert_eq!(
        seen.called("read_file"),
        1,
        "the second attempt's read should have happened"
    );
}
