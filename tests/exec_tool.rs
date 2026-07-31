//! The `exec` tool through the full loop — F1 to F5, and F12 — with a scripted
//! mock provider so the tests are deterministic and offline.
//!
//! `exec` is the widest capability this crate grants, so most of what is asserted
//! here is the boundary rather than the mechanism. Every refusal test carries a
//! negative control: an assertion that a denied command is refused proves nothing
//! unless the same command demonstrably runs when the policy allows it, and the
//! failure mode a control catches is a tool that never worked at all.
//!
//! `rustc` is the program under test throughout. It is the one binary guaranteed
//! present wherever `cargo test` runs, so these tests need no fixture toolchain,
//! no network, and no system package on any runner.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{run_with, ApproveAll, Provider, RunOutcome, Store, TaskContract};
use serde_json::json;

/// Plays a fixed script of tool calls, and keeps the tool list it was offered on
/// the first request so a test can assert on the schema the model actually saw.
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

    /// The spec the model was given for `name`, if it was offered one.
    fn spec(&self, name: &str) -> Option<ToolSpec> {
        self.offered
            .lock()
            .unwrap()
            .iter()
            .find(|t| t.name == name)
            .cloned()
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

fn exec_call(argv: &[&str]) -> ToolCall {
    ToolCall {
        name: "exec".into(),
        arguments: json!({ "argv": argv }),
    }
}

/// A one-file Rust project. `rustc` can build it, and the relative path in the
/// argv only resolves if the command ran in the workspace root.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();
    dir
}

/// A contract with no gate, so a script that runs out ends the run rather than
/// burning the step budget. The criterion is not what these tests are about.
fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("build the project", root).with_max_steps(6)
}

/// Every command allowed, nothing else changed. The base for the tests that are
/// about the mechanism rather than the boundary.
fn permissive() -> Policy {
    Policy::permissive()
}

// ---------------------------------------------------------------------------
// F1 — the agent can run a command
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_command_runs_in_the_workspace_root_and_its_result_reaches_the_next_turn() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    // A relative source path and a relative output directory: both only resolve
    // if the child's working directory is the workspace root.
    let provider = MockScript::new(vec![vec![exec_call(&[
        "rustc",
        "--crate-type",
        "lib",
        "src/lib.rs",
        "--out-dir",
        ".",
    ])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        dir.path().join("libsrc.rlib").exists()
            || std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".rlib")),
        "the build ran in the workspace root, so its relative output landed there"
    );

    let steps = store.steps(result.run_id).unwrap();
    let ran = &steps[0];
    assert!(
        ran.decision.contains("exec rustc") && ran.decision.contains("exit 0"),
        "the exit status is in the trace: {:?}",
        ran.decision
    );

    // "Reaches the agent's next turn" is the claim, so it is asserted against the
    // next turn's prompt rather than against the tool's return value.
    let next = &steps[1];
    assert!(
        next.prompt
            .contains("[exec `rustc --crate-type lib src/lib.rs --out-dir .` exit 0]"),
        "the command and its exit status reached the following request: {}",
        next.prompt
    );
}

#[tokio::test]
async fn a_command_that_fails_reports_its_status_and_its_stderr_to_the_agent() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["rustc", "src/nope.rs"])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        !steps[0].decision.contains("exit 0"),
        "a failing command is not reported as a success: {:?}",
        steps[0].decision
    );
    assert!(
        steps[1].prompt.contains("nope.rs"),
        "the compiler's own complaint reached the agent: {}",
        steps[1].prompt
    );
}

#[tokio::test]
async fn a_program_this_machine_does_not_have_is_an_observation_not_a_failed_run() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["io-harness-no-such-program"])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // The run continued and ended on its own terms rather than erroring out.
    assert!(matches!(result.outcome, RunOutcome::Finished { .. }));
    assert!(store.steps(result.run_id).unwrap()[1]
        .prompt
        .contains("[exec unavailable]"));
}

// ---------------------------------------------------------------------------
// F2 — exec is authorized, not merely available
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_denied_program_is_refused_before_it_is_spawned_and_the_refusal_is_attributed() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive().layer("ops").deny_exec("rustc");
    let provider = MockScript::new(vec![vec![exec_call(&[
        "rustc",
        "--crate-type",
        "lib",
        "src/lib.rs",
        "--out-dir",
        ".",
    ])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        !std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".rlib")),
        "nothing was spawned, so nothing was built"
    );

    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal" && e.act == "exec")
        .collect();
    assert!(
        refusals
            .iter()
            .any(|e| e.rule.as_deref() == Some("rustc") && e.layer.as_deref() == Some("ops")),
        "the refusal names the rule and the layer that produced it: {refusals:?}"
    );
}

/// The negative control for the test above. The same run, the same command, a
/// policy that allows it — so the refusal is measuring the boundary rather than a
/// tool that never worked.
#[tokio::test]
async fn the_same_command_under_a_policy_that_allows_it_executes() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive().layer("ops").allow_exec("rustc");
    let provider = MockScript::new(vec![vec![exec_call(&[
        "rustc",
        "--crate-type",
        "lib",
        "src/lib.rs",
        "--out-dir",
        ".",
    ])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    assert!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .flatten()
            .any(|e| e.file_name().to_string_lossy().ends_with(".rlib")),
        "the allowed command ran"
    );
    assert!(store.steps(result.run_id).unwrap()[0]
        .decision
        .contains("exit 0"));
}

// ---------------------------------------------------------------------------
// F3 — argv is matched whole, not just on the program name
// ---------------------------------------------------------------------------

/// The two argvs F3 is about, spelled with `rustc` so both actually run when the
/// policy lets them. `--version` stands for the harmless subcommand and
/// `--print` for the one an operator wants to forbid.
const HARMLESS: &[&str] = &["rustc", "--version"];
const FORBIDDEN: &[&str] = &["rustc", "--print", "sysroot"];

#[tokio::test]
async fn a_rule_can_allow_one_subcommand_and_deny_another_of_the_same_program() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive()
        .layer("ops")
        .allow_exec("rustc --version*")
        .deny_exec("rustc --print*");
    let provider = MockScript::new(vec![vec![exec_call(HARMLESS)], vec![exec_call(FORBIDDEN)]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("exit 0"),
        "the allowed argv ran: {:?}",
        steps[0].decision
    );
    assert!(
        steps[1].decision.contains("refused"),
        "the denied argv did not: {:?}",
        steps[1].decision
    );

    let refusals: Vec<_> = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal")
        .collect();
    assert!(
        refusals
            .iter()
            .any(|e| e.rule.as_deref() == Some("rustc --print*")
                && e.layer.as_deref() == Some("ops")),
        "attributed to the argv rule, not to the program: {refusals:?}"
    );
}

/// The negative control: a policy that allows both, under which both run. Without
/// it the test above would pass against an `exec` that refused everything after
/// the first call.
#[tokio::test]
async fn a_policy_allowing_both_argvs_runs_both() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let policy = Policy::permissive().layer("ops").allow_exec("rustc*");
    let provider = MockScript::new(vec![vec![exec_call(HARMLESS)], vec![exec_call(FORBIDDEN)]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();

    let steps = store.steps(result.run_id).unwrap();
    for (i, step) in steps.iter().take(2).enumerate() {
        assert!(
            step.decision.contains("exit 0"),
            "step {i} ran: {:?}",
            step.decision
        );
    }
}

// ---------------------------------------------------------------------------
// F4 — exec cannot reach a shell
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_tool_schema_accepts_only_an_array_of_strings() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![exec_call(&["rustc", "--version"])]]);

    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spec = provider.spec("exec").expect("exec is offered to the model");
    let argv = &spec.parameters["properties"]["argv"];
    assert_eq!(argv["type"], "array", "{:?}", spec.parameters);
    assert_eq!(argv["items"]["type"], "string", "{:?}", spec.parameters);
    assert_eq!(spec.parameters["required"], json!(["argv"]));
    // There is no second way in. A string parameter beside the array would be the
    // shell this tool exists not to have.
    let props = spec.parameters["properties"].as_object().unwrap();
    assert_eq!(
        props.keys().collect::<Vec<_>>(),
        vec!["argv"],
        "one parameter, and it is the array"
    );
}

/// Every metacharacter a shell would act on, inside ONE argument, observed in
/// what the child actually received.
///
/// `rustc` names the file it could not read, verbatim, so its own stderr is the
/// negative control: if anything had parsed the argument, the name in the error
/// would be a shorter or a substituted string rather than this one.
#[tokio::test]
async fn shell_metacharacters_reach_the_program_as_one_literal_argument() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let nasty = "a;b && c $(id) `whoami` | d > e.rs";
    let provider = MockScript::new(vec![vec![exec_call(&["rustc", nasty])]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let observed = &store.steps(result.run_id).unwrap()[1].prompt;
    assert!(
        observed.contains(nasty),
        "the child received the argument byte for byte: {observed}"
    );
    // Nothing was substituted or executed: no user name, no shell error, and the
    // file `e.rs` that a real redirection would have created does not exist.
    assert!(!dir.path().join("e.rs").exists(), "nothing was redirected");
}

// ---------------------------------------------------------------------------
// F5 — exec is bounded (the tool-level half; the runner's own bounds are unit
// tested in src/tools/exec.rs)
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_wedged_command_is_killed_and_reported_as_a_timeout_naming_itself() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    // A sleep, spelled the way each platform spells it; both are on every runner
    // image this crate builds on.
    let argv: Vec<&str> = if cfg!(windows) {
        vec!["ping", "-n", "30", "127.0.0.1"]
    } else {
        vec!["sleep", "30"]
    };
    let provider = MockScript::new(vec![vec![exec_call(&argv)]]);
    let contract = contract(dir.path()).with_exec_timeout(Duration::from_millis(300));

    let started = std::time::Instant::now();
    let result = run_with(&contract, &provider, &store, &permissive(), &ApproveAll)
        .await
        .unwrap();

    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the run got its turn back rather than waiting out the command"
    );
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].decision.contains("timed out"),
        "the trace says what happened: {:?}",
        steps[0].decision
    );
    assert!(
        steps[1].prompt.contains("[exec timed out]") && steps[1].prompt.contains(argv[0]),
        "the observation names the command that hung: {}",
        steps[1].prompt
    );
    // And the run itself is not a budget stop: the wedged command was the thing
    // that ended, not the contract.
    assert!(
        matches!(result.outcome, RunOutcome::Finished { .. }),
        "{:?}",
        result.outcome
    );
}

// ---------------------------------------------------------------------------
// F12 — the detection reaches the model
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_detected_project_puts_its_ecosystem_and_commands_in_the_recorded_prompt() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("package.json"), "{}").unwrap();
    std::fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    // Asserted against the prompt the trace recorded, not against the assembler.
    let prompt = &store.steps(result.run_id).unwrap()[0].prompt;
    assert!(prompt.contains("node"), "the ecosystem: {prompt}");
    assert!(prompt.contains("package.json"), "the marker: {prompt}");
    assert!(prompt.contains("test: pnpm test"), "the commands: {prompt}");
}

/// The negative control: a directory the table knows nothing about carries no
/// claim about itself, rather than a guess the agent would then act on.
#[tokio::test]
async fn an_undetected_project_says_nothing_about_its_toolchain() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("notes.txt"), "no build system here").unwrap();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let prompt = &store.steps(result.run_id).unwrap()[0].prompt;
    assert!(
        !prompt.contains("Project:"),
        "no detection, so no sentence about one: {prompt}"
    );
}
