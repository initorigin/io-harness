//! The boundary a program reaches, judged against the boundary a tool call
//! reaches — F4, F5, F6 and O4.
//!
//! This is the file that decides whether 0.79.0 is honest. Every functional
//! claim in the release can be satisfied by an implementation that runs a
//! program, produces the right output, and never asks the policy anything — so
//! nothing here asserts on what a program returned. It asserts on the rows the
//! policy wrote, and it compares them against the rows the *same acts* write when
//! the model makes them itself.
//!
//! The control is the point. "A program's acts are gated" proves nothing unless
//! the identical acts, made the ordinary way in the same fixture under the same
//! policy, produce the same rows — that comparison is the only thing that can
//! tell a real re-entry into `dispatch` apart from a second, shorter path that
//! happens to write plausible rows of its own.
#![cfg(feature = "codeact")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{
    run_with, run_with_observed, ApproveAll, CodeActConfig, DenyAll, EventKind, Flow, Observer,
    PolicyEvent, Provider, RunEvent, Store, TaskContract, CODEACT_UNCALLABLE,
};
use serde_json::json;

/// Plays a fixed script of tool calls and keeps the tool list it was offered, so
/// a test can assert on what the model was actually shown.
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

    fn offered_names(&self) -> Vec<String> {
        self.offered
            .lock()
            .unwrap()
            .iter()
            .map(|t| t.name.clone())
            .collect()
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

/// A workspace with two files to read and one to overwrite.
fn ws() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("one.txt"), "first\n").unwrap();
    std::fs::write(dir.path().join("two.txt"), "second\n").unwrap();
    std::fs::write(dir.path().join("out.txt"), "before\n").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("read both files and write a summary", root).with_max_steps(6)
}

/// Writes asked, execs refused, reads permitted.
///
/// Shaped by what actually leaves a row. A permitted-by-rule act writes no
/// `policy_events` row — only a refusal and a decision an approver was consulted
/// for do — and there is no `ask_read`, so a read is invisible to this comparison
/// however it is made. Asking on writes and denying execs gives the two kinds of
/// row that exist, which is what makes the equality below mean something rather
/// than being two one-element lists agreeing. The length assertion in the test
/// exists so this can never silently regress to the vacuous version.
fn asking() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .ask_write("*")
        .deny_exec("*")
}

/// Reads permitted, writes refused outright. Used where the point is a refusal
/// rather than a comparison.
fn no_writes() -> Policy {
    Policy::default()
        .layer("test")
        .allow_read("*")
        .deny_write("*")
        .allow_exec("*")
}

/// The part of a row that is a fact about the act rather than about the run:
/// its step is not compared, because a program does several acts inside one step
/// and the model does them across several, which is the whole point of the
/// feature and not a difference in what the policy saw.
fn shape(events: &[PolicyEvent]) -> Vec<(String, String, Option<String>)> {
    events
        .iter()
        .map(|e| (e.act.clone(), e.target.clone(), e.decision.clone()))
        .collect()
}

/// Skip rather than fail where this host has no interpreter: a machine without
/// Python is a supported machine, and a red suite there would be this crate
/// asserting a property of the host.
fn skip_without_python() -> bool {
    let found = io_harness::CODEACT_CANDIDATES
        .iter()
        .any(|c| which(c).is_some());
    if !found {
        eprintln!("no host interpreter; skipping");
    }
    !found
}

fn which(program: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

// ---------------------------------------------------------------------------
// F4 / O4 — the same acts, both ways, and the same rows
// ---------------------------------------------------------------------------

/// F4 and O4. One task, expressed twice.
///
/// The program reads two files and writes a third. The control makes the same
/// three calls as three ordinary tool calls. The assertion is that the policy
/// wrote the same rows — same acts, same targets, same decisions, in the same
/// order — because that is the only evidence that a program's act went through
/// the gate rather than around it.
#[tokio::test]
async fn a_program_and_a_chain_of_tool_calls_leave_the_same_policy_rows() {
    if skip_without_python() {
        return;
    }

    // --- the program ---
    let program_dir = ws();
    let program_store = Store::memory().unwrap();
    // Four acts, chosen to cover both kinds of row that exist: two writes an
    // approver is consulted about, and one exec the policy refuses outright. The
    // read is in there because it is what a program is for, even though a
    // permitted read leaves no row for either side to compare.
    let source = "a = read_file(path=\"one.txt\")\n\
                  write_file(path=\"out.txt\", content=str(a))\n\
                  write_file(path=\"two.txt\", content=\"rewritten\")\n\
                  e = exec(argv=[\"true\"])\n\
                  print(\"exec allowed:\", e.ok)\n";
    let program = MockScript::new(vec![vec![ToolCall {
        name: "run_program".into(),
        arguments: json!({ "source": source }),
    }]]);
    let by_program = run_with(
        &contract(program_dir.path()).with_codeact(CodeActConfig::default()),
        &program,
        &program_store,
        &asking(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // --- the control: the same three acts, made the ordinary way ---
    let chain_dir = ws();
    let chain_store = Store::memory().unwrap();
    let chain = MockScript::new(vec![
        vec![ToolCall {
            name: "read_file".into(),
            arguments: json!({ "path": "one.txt" }),
        }],
        vec![ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": "out.txt", "content": "first\n" }),
        }],
        vec![ToolCall {
            name: "write_file".into(),
            arguments: json!({ "path": "two.txt", "content": "rewritten" }),
        }],
        vec![ToolCall {
            name: "exec".into(),
            arguments: json!({ "argv": ["true"] }),
        }],
    ]);
    let by_chain = run_with(
        &contract(chain_dir.path()),
        &chain,
        &chain_store,
        &asking(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let from_program = shape(&program_store.events(by_program.run_id).unwrap());
    let from_chain = shape(&chain_store.events(by_chain.run_id).unwrap());

    // The control has to be non-empty, or the equality below is two empty lists
    // agreeing with each other — the failure mode this whole file exists to
    // avoid.
    assert!(
        from_chain.len() >= 3,
        "the control made three gated acts and should have left at least three rows; got {from_chain:?}"
    );
    assert_eq!(
        from_program, from_chain,
        "a program's acts must reach the policy exactly as a model's own calls do"
    );

    // And the acts actually happened rather than only being asked about. The
    // bytes are deliberately not compared: the program writes what `read_file`
    // returned, which is the observation text, and the control writes a literal —
    // the claim under test is what the policy saw, not that a model and a program
    // compose the same string.
    for dir in [&program_dir, &chain_dir] {
        assert_ne!(
            std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
            "before\n",
            "the approved write should have landed"
        );
        assert_eq!(
            std::fs::read_to_string(dir.path().join("two.txt")).unwrap(),
            "rewritten",
            "the second approved write should have landed"
        );
    }
}

// ---------------------------------------------------------------------------
// F5 — a refusal is a value, and it is on the record
// ---------------------------------------------------------------------------

/// F5. A denied write comes back to the program as something it can branch on,
/// and it leaves a row whether the program branches on it or ignores it.
///
/// The negative control is inside the same program: a read that IS permitted, so
/// a program that simply failed to run would fail this test rather than pass it
/// by producing no output at all.
#[tokio::test]
async fn a_denied_act_is_refused_to_the_program_and_leaves_a_row() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    let source = "r = read_file(path=\"one.txt\")\n\
                  print(\"read ok:\", r.ok)\n\
                  w = write_file(path=\"out.txt\", content=\"x\")\n\
                  print(\"write ok:\", w.ok)\n\
                  print(\"words:\", str(w))\n";
    let provider = MockScript::new(vec![vec![ToolCall {
        name: "run_program".into(),
        arguments: json!({ "source": source }),
    }]]);

    let seen = Collect::default();
    let result = run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &provider,
        &store,
        &no_writes(),
        &DenyAll,
        &seen,
    )
    .await
    .unwrap();

    let events = store.events(result.run_id).unwrap();
    let write_rows: Vec<&PolicyEvent> = events
        .iter()
        .filter(|e| e.act == "write" && e.target.contains("out.txt"))
        .collect();
    assert!(
        !write_rows.is_empty(),
        "the denied write must leave a row; rows were {:?}",
        shape(&events)
    );

    // The control: the permitted read did reach dispatch in the same program, so
    // the refusal above is the policy deciding rather than the program never
    // running. A permitted-by-rule read leaves no policy row, so the evidence for
    // it is the observer channel rather than the table.
    let kinds = seen.0.lock().unwrap().clone();
    assert!(
        kinds.iter().filter(|k| *k == "tool_call").count() >= 2,
        "both of the program's acts should have reached dispatch; events were {kinds:?}"
    );

    // And the file was not written.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("out.txt")).unwrap(),
        "before\n",
        "a refused write must not land"
    );
}

// ---------------------------------------------------------------------------
// F6 — the uncallable set, and that it is a list rather than a derivation
// ---------------------------------------------------------------------------

/// F6. A program that calls an uncallable tool is refused by name, and the tool
/// is not in the generated module either.
///
/// `remember` is the one under test because it is the sharpest case: it is
/// ungated inside `dispatch` on purpose, so if a program could call it the act
/// would reach the harness's own store with no `policy_events` row at all.
#[tokio::test]
async fn an_uncallable_tool_is_refused_to_a_program_by_name() {
    if skip_without_python() {
        return;
    }
    let dir = ws();
    let store = Store::memory().unwrap();
    // Built by hand rather than through a binding, because there is no binding:
    // the boundary must refuse the name even when nothing offered it, so the two
    // halves of F6 are checked separately and neither stands in for the other.
    //
    // The permitted read in the same program is the control. Without it, "no
    // memory was written" would also be true of a program that never ran.
    let source = "r = _act(\"remember\", {\"text\": \"a durable fact\"})\n\
                  print(\"remember ok:\", r.ok)\n\
                  print(\"has binding:\", \"remember\" in dir())\n\
                  c = read_file(path=\"one.txt\")\n\
                  print(\"control ok:\", c.ok)\n";
    let provider = MockScript::new(vec![vec![ToolCall {
        name: "run_program".into(),
        arguments: json!({ "source": source }),
    }]]);

    let seen = Collect::default();
    run_with_observed(
        &contract(dir.path()).with_codeact(CodeActConfig::default()),
        &provider,
        &store,
        &asking(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    let kinds = seen.0.lock().unwrap().clone();
    assert!(
        !kinds.iter().any(|k| k == "memory_wrote"),
        "a program must not reach the ungated writes; events were {kinds:?}"
    );
    // The control: the program did run and its permitted call did reach dispatch,
    // which is what makes the absence above meaningful.
    assert!(
        kinds.iter().any(|k| k == "tool_call"),
        "the program's permitted read should have reached dispatch; events were {kinds:?}"
    );
}

/// Records the wire name of every event, which is all these tests need and is
/// cheaper to assert on than a variant.
#[derive(Default)]
struct Collect(Mutex<Vec<String>>);

impl Observer for Collect {
    fn event(&self, event: &RunEvent) -> Flow {
        let name = match &event.kind {
            EventKind::MemoryWrote { .. } => "memory_wrote",
            EventKind::ToolCall { .. } => "tool_call",
            EventKind::Program { .. } => "program",
            _ => "other",
        };
        self.0.lock().unwrap().push(name.to_string());
        Flow::Continue
    }
}

/// F6's other half, checked without running anything: the set is a literal, and
/// it names the three ungated writes and the tools that need a conversation or a
/// tree. A built-in added later that belongs in neither group fails this.
#[test]
fn the_uncallable_set_names_the_ungated_writes_and_the_conversational_tools() {
    for name in [
        "remember",
        "forget",
        "todo_write",
        "ask_question",
        "ask_questions",
        "propose_plan",
        "spawn_agent",
        "send_message",
        "read_messages",
        "read_skill",
        "run_program",
    ] {
        assert!(
            CODEACT_UNCALLABLE.contains(&name),
            "{name} must not be callable from a program"
        );
    }
    // The negative control: the set is an exclusion list, not everything.
    for name in ["read_file", "write_file", "grep", "exec", "edit_file"] {
        assert!(
            !CODEACT_UNCALLABLE.contains(&name),
            "{name} is exactly what a program is for"
        );
    }
}

// ---------------------------------------------------------------------------
// F15 — a run that does not use it is unchanged
// ---------------------------------------------------------------------------

/// F15. Compiling the feature in changes nothing about a run that does not ask
/// for a program: the same acts, the same rows, the same file on disk.
#[tokio::test]
async fn a_run_that_asks_for_no_program_is_unchanged_by_the_feature() {
    let script = || {
        MockScript::new(vec![
            vec![ToolCall {
                name: "read_file".into(),
                arguments: json!({ "path": "one.txt" }),
            }],
            vec![ToolCall {
                name: "write_file".into(),
                arguments: json!({ "path": "out.txt", "content": "x" }),
            }],
        ])
    };

    let plain_dir = ws();
    let plain_store = Store::memory().unwrap();
    let plain = script();
    let plain_result = run_with(
        &contract(plain_dir.path()),
        &plain,
        &plain_store,
        &asking(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The same contract with the capability configured but never used. The tool
    // may be offered; nothing else may move.
    let armed_dir = ws();
    let armed_store = Store::memory().unwrap();
    let armed = script();
    let armed_result = run_with(
        &contract(armed_dir.path()).with_codeact(CodeActConfig::default()),
        &armed,
        &armed_store,
        &asking(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert_eq!(
        shape(&plain_store.events(plain_result.run_id).unwrap()),
        shape(&armed_store.events(armed_result.run_id).unwrap()),
        "configuring a program must not change a run that never writes one"
    );
    assert_eq!(
        std::fs::read_to_string(plain_dir.path().join("out.txt")).unwrap(),
        std::fs::read_to_string(armed_dir.path().join("out.txt")).unwrap(),
    );
    // And the control that this test is comparing something: the armed run was
    // actually offered the tool, so the equality above is not two identical
    // catalogues.
    assert!(
        armed.offered_names().iter().any(|n| n == "run_program") || skip_without_python(),
        "the armed run should have been offered the tool; it saw {:?}",
        armed.offered_names()
    );
    assert!(
        !plain.offered_names().iter().any(|n| n == "run_program"),
        "a contract that asks for no program must not be offered one"
    );
}
