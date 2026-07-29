//! Verification that is not about Rust — F8, F9 and F10.
//!
//! These three are the release's claim in one file: a criterion can be any
//! project's own test command (F8), a run can have no criterion at all (F9), and
//! the Rust-specific criteria a 0.16.2 caller wrote still work while they are
//! being deprecated out (F10).
//!
//! F8 runs against Node, and against `npm test` rather than `node` directly,
//! because the claim is that a *project's own* command can be the gate. `node`
//! and `npm` are preinstalled on all three GitHub runner images, so nothing here
//! installs a system package; a machine without them skips with a stated reason
//! rather than passing vacuously.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, ApproveAll, Provider, RunOutcome, Store, TaskContract, Verification};
use serde_json::json;

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

fn write(path: &str, content: &str) -> ToolCall {
    ToolCall {
        name: "write_file".into(),
        arguments: json!({ "path": path, "content": content }),
    }
}

/// A distinct, harmless call, so a run that is meant to reach its step cap does
/// not trip stall detection on the way — a stall needs a *repeated* signature,
/// and these differ.
fn look(n: usize) -> ToolCall {
    ToolCall {
        name: "grep".into(),
        arguments: json!({ "pattern": format!("marker-{n}") }),
    }
}

fn open() -> Policy {
    Policy::permissive()
}

// ---------------------------------------------------------------------------
// F8 — Verification::Command, against Node
// ---------------------------------------------------------------------------

/// A node package whose `npm test` runs one script. The script is what the agent
/// is asked to fix, so the gate is the project's own command and nothing about
/// the harness is language-aware.
///
/// No dependencies, so no install and no network: the execution sandbox denies
/// egress, and a fixture that needed the registry would be testing the network
/// rather than the gate.
fn node_project(passing: bool) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"name":"fixture","version":"1.0.0","private":true,"scripts":{"test":"node test.js"}}"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("test.js"),
        if passing {
            "process.exit(0)\n"
        } else {
            "console.error('FAIL: answer was 1, expected 42')\nprocess.exit(1)\n"
        },
    )
    .unwrap();
    dir
}

/// `npm` on this machine, or `None` — in which case the test skips and says why
/// rather than reporting a pass it did not earn.
fn npm_available() -> bool {
    std::process::Command::new("npm")
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn npm_test() -> Verification {
    Verification::Command {
        argv: vec!["npm".into(), "test".into()],
        expect_exit: 0,
    }
}

#[tokio::test]
async fn a_node_projects_own_test_command_is_a_passing_criterion() {
    if !npm_available() {
        eprintln!("skipped: no `npm` on this machine, and this release installs no system package");
        return;
    }
    let dir = node_project(true);
    let store = Store::memory().unwrap();
    // The agent does nothing of consequence; the gate is what is under test.
    let provider = MockScript::new(vec![vec![write("notes.md", "looked at it\n")]]);
    let contract =
        TaskContract::workspace("make the suite pass", dir.path(), npm_test()).with_max_steps(2);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "a project's own test command exiting zero is a pass: {:?}",
        result.outcome
    );
}

#[tokio::test]
async fn a_failing_node_suite_is_not_a_pass_and_its_output_reaches_the_trace() {
    if !npm_available() {
        eprintln!("skipped: no `npm` on this machine, and this release installs no system package");
        return;
    }
    let dir = node_project(false);
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![write("notes.md", "looked at it\n")]]);
    let contract =
        TaskContract::workspace("make the suite pass", dir.path(), npm_test()).with_max_steps(2);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        !matches!(result.outcome, RunOutcome::Success { .. }),
        "the suite failed, so the run did not succeed: {:?}",
        result.outcome
    );

    // Why it failed is in the trace. Without this a failing gate is a bare
    // discriminant, and "the agent's change is wrong" is indistinguishable from
    // "the test runner is not installed" — which need opposite responses.
    let events = store.sandbox_events(result.run_id).unwrap();
    assert!(
        events.iter().any(|e| e.kind == "gate_output"
            && e.detail
                .as_deref()
                .is_some_and(|d| d.contains("expected 42"))),
        "the command's own output was recorded: {events:?}"
    );
    assert!(
        events.iter().any(|e| e.kind == "gate_phase_failed"
            && e.detail.as_deref().is_some_and(|d| d.contains("exited 1"))),
        "and so was the exit status it was judged on: {events:?}"
    );
}

#[tokio::test]
async fn a_command_criterion_is_refused_rather_than_failed_when_the_policy_forbids_it() {
    let dir = node_project(true);
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![write("notes.md", "x\n")]]);
    let policy = Policy::permissive().layer("ops").deny_exec("npm");
    let contract =
        TaskContract::workspace("make the suite pass", dir.path(), npm_test()).with_max_steps(2);

    // Verification cannot prompt, so a criterion the policy will not allow is an
    // error to the caller rather than a quiet "it did not pass" — a criterion
    // that was refused is not one that ran and failed.
    let err = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .expect_err("a denied gate command must surface as a refusal");
    assert!(
        matches!(&err, io_harness::Error::Refused { act, target, .. }
            if act == "exec" && target == "npm"),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// F9 — Verification::None completes a run with no gate
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_run_with_no_gate_ends_when_the_agent_stops_calling_tools() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    // One step of work, then an assistant turn with no tool call.
    let provider = MockScript::new(vec![vec![write(
        "findings.md",
        "the deploy fails on DNS\n",
    )]]);
    let contract = TaskContract::workspace(
        "work out why the deploy fails and write it up",
        dir.path(),
        Verification::None,
    )
    .with_max_steps(10);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        result.outcome,
        RunOutcome::Finished { steps: 2 },
        "the run ended on the quiet turn, not on a ceiling"
    );
    assert!(
        dir.path().join("findings.md").exists(),
        "and the work it did do is on disk"
    );

    // Durable, and final: a resume reports the same outcome rather than driving
    // the loop again to watch the agent say nothing twice.
    let again = io_harness::resume(&contract, &provider, &store, result.run_id)
        .await
        .unwrap();
    assert_eq!(again.outcome, RunOutcome::Finished { steps: 2 });
}

/// F9's negative control. The same contract and the same variant, over an agent
/// that never stops — which must report the cap, not the finished outcome.
/// Without it, a `Finished` returned unconditionally would pass the test above.
#[tokio::test]
async fn a_run_that_hits_the_step_cap_reports_the_cap_and_not_the_finished_outcome() {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new((0..4).map(|n| vec![look(n)]).collect());
    let contract =
        TaskContract::workspace("keep looking", dir.path(), Verification::None).with_max_steps(4);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(result.outcome, RunOutcome::StepCapReached { steps: 4 });
}

/// The other half of the same boundary: an assistant turn with no tool call is
/// only terminal when there is no criterion. A contract that *has* one must keep
/// behaving exactly as it did in 0.16.2, or every existing caller would silently
/// find their runs ending at the agent's first quiet turn.
#[tokio::test]
async fn a_quiet_turn_does_not_end_a_run_that_has_a_criterion() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("out.txt"), "nothing yet\n").unwrap();
    let store = Store::memory().unwrap();
    // Nothing at all on the first turn; the real work on the second.
    let provider = MockScript::new(vec![vec![], vec![write("out.txt", "done\n")]]);
    let contract = TaskContract::workspace(
        "write done",
        dir.path(),
        Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "done".into(),
        },
    )
    .with_max_steps(4);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();

    assert_eq!(
        result.outcome,
        RunOutcome::Success { steps: 2 },
        "the quiet turn was an unproductive step, not the end of the run"
    );
}

// ---------------------------------------------------------------------------
// The removal (0.18.0): what the deprecated variants proved, proved through the
// gate that replaced them
// ---------------------------------------------------------------------------

/// The 0.17.0 deprecation note told a caller to write this criterion instead,
/// and named 0.18.0 as the release that would remove the old one. This is the
/// assertion behind that note: the replacement reaches the same outcome, on the
/// same fixture, through the same loop.
#[tokio::test]
async fn the_replacement_criterion_reaches_the_outcome_the_removed_variants_did() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub mod a;\n#[test] fn t() { assert_eq!(a::a(), 42); }\n",
    )
    .unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "pub fn a() -> u32 { 0 }\n").unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![write("src/a.rs", "pub fn a() -> u32 { 42 }\n")]]);

    let contract = TaskContract::workspace(
        "make a() return 42",
        dir.path(),
        Verification::Command {
            argv: vec!["cargo".into(), "test".into(), "--offline".into()],
            expect_exit: 0,
        },
    )
    .with_max_steps(3);

    let result = run_with(&contract, &provider, &store, &open(), &ApproveAll)
        .await
        .unwrap();
    assert_eq!(result.outcome, RunOutcome::Success { steps: 1 });
}

/// The deprecation was a promise with a date on it: three variants, replaced by
/// `Verification::Command`, removed in 0.18.0. A promise in an attribute rots as
/// quietly as one in prose, so the 0.17.0 suite checked the attribute existed.
/// This is the other end of it — the promise was kept, and the source is what
/// says so.
///
/// A pure function over the source text, with a negative control below, in the
/// style of this repository's other documentation checkers: a checker that
/// silently matches nothing passes every input and reports a green claim.
fn mentions_variant(src: &str, variant: &str) -> bool {
    src.lines().any(|line| {
        let t = line.trim();
        // A doc comment may still discuss the removal; a `Verification::X` in
        // code is what would mean the variant is back.
        !t.starts_with("//") && t.contains(&format!("Verification::{variant}"))
    })
}

#[test]
fn the_three_deprecated_variants_are_gone_from_the_source() {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/verify.rs"),
    )
    .unwrap();

    for variant in ["CompilesRust", "RustTestPasses", "WorkspaceTestPasses"] {
        assert!(
            !mentions_variant(&src, variant),
            "{variant} was promised removed in 0.18.0 and is still in src/verify.rs"
        );
    }

    // The negative control. Without it this test passes against a checker that
    // matches nothing at all — including against a file that never loaded.
    assert!(
        mentions_variant(&src, "Command"),
        "the checker matches nothing, so its three passes above mean nothing"
    );
    assert!(
        !src.contains("#[deprecated"),
        "the deprecation cycle this release closes was the only one open"
    );
}
