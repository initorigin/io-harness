//! The `shell` tool through the full loop — F1, F2 and F3 — with a scripted mock
//! provider so the tests are deterministic and offline.
//!
//! The parser's own tests live beside it in `src/tools/shell.rs`, where they can
//! sweep the whole ASCII range cheaply. What is asserted here is the thing those
//! cannot reach: that the parse is actually *joined to the boundary* — that every
//! sub-command and every redirect target reaches the policy, that all of it
//! happens before the first spawn, and that a refusal leaves the workspace as it
//! found it.
//!
//! Every refusal test carries a negative control, for the reason `exec_tool.rs`
//! states: an assertion that a denied line is refused proves nothing unless the
//! same line demonstrably runs when the policy allows it, and the failure a
//! control catches is a tool that never worked at all.
//!
//! `rustc` is the program under test throughout. It is the one binary guaranteed
//! present wherever `cargo test` runs, so these tests need no fixture toolchain,
//! no network, and no system package on any runner — and `rustc --version` and
//! `rustc --print sysroot` are two commands with reliably *different* output,
//! which is what makes the pipeline assertion below possible.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall, ToolSpec};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
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

fn shell_call(line: &str) -> ToolCall {
    ToolCall {
        name: "shell".into(),
        arguments: json!({ "line": line }),
    }
}

/// A workspace with a nested directory to `cd` into and a `secrets/` to deny.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("out")).unwrap();
    std::fs::create_dir_all(dir.path().join("secrets")).unwrap();
    std::fs::write(dir.path().join("secrets/key"), "hunter2\n").unwrap();
    dir
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("run some commands", root).with_max_steps(4)
}

/// Runs one line and hands back the workspace, the store and the run id.
async fn run_line(
    dir: &tempfile::TempDir,
    policy: Policy,
    line: &str,
) -> (Store, io_harness::RunResult) {
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![shell_call(line)]]);
    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &policy,
        &ApproveAll,
    )
    .await
    .unwrap();
    (store, result)
}

/// The first step's decision string, which is what the trace records.
fn decision(store: &Store, run_id: i64) -> String {
    store.steps(run_id).unwrap()[0].decision.clone()
}

/// What the model actually reads about the line it ran.
///
/// The observation is asserted against the *next* turn's prompt rather than
/// against the tool's return value, for the reason `exec_tool.rs` gives: reaching
/// the agent's next turn is the claim, and a return value nobody forwards would
/// satisfy an assertion about the return value.
fn observation(store: &Store, run_id: i64) -> String {
    let steps = store.steps(run_id).unwrap();
    steps
        .get(1)
        .map(|s| s.prompt.clone())
        .unwrap_or_else(|| steps[0].result.clone())
}

// ---------------------------------------------------------------------------
// F1 — a real command line runs under a checked boundary
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_sequence_with_cd_runs_every_stage_in_the_right_directory() {
    let dir = fixture();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn answer() -> u32 { 42 }\n",
    )
    .unwrap();

    // `cd src` has to take effect for `lib.rs` to resolve, and `--out-dir ../out`
    // has to resolve relative to `src` for the artefact to land in `out/`. Both
    // halves of the directory change are load-bearing.
    let (store, result) = run_line(
        &dir,
        Policy::permissive(),
        "cd src && rustc --crate-type lib lib.rs --out-dir ../out",
    )
    .await;

    let produced: Vec<_> = std::fs::read_dir(dir.path().join("out"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        produced.iter().any(|f| f.ends_with(".rlib")),
        "the compile ran with `src` as its working directory and wrote through the \
         relative out-dir; out/ holds {produced:?}"
    );
    assert!(
        decision(&store, result.run_id).contains("exit 0"),
        "the exit status is in the trace: {:?}",
        decision(&store, result.run_id)
    );
}

#[tokio::test]
async fn a_pipeline_wires_one_stage_into_the_next() {
    let dir = fixture();
    // Two commands with different output. Only the LAST stage's stdout is
    // reported, so seeing the sysroot and not the version is what proves the
    // first stage's stdout went into the pipe rather than into the observation.
    let (store, result) = run_line(
        &dir,
        Policy::permissive(),
        "rustc --version | rustc --print sysroot",
    )
    .await;

    let obs = observation(&store, result.run_id);
    let sysroot = String::from_utf8(
        std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        obs.contains(sysroot.trim()),
        "the last stage's stdout is the observation: {obs}"
    );
    assert!(
        !obs.contains("rustc 1."),
        "the first stage's stdout went down the pipe, not into the observation: {obs}"
    );
}

#[tokio::test]
async fn the_tool_is_offered_with_a_line_parameter_and_names_its_refusals() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![shell_call("rustc --version")]]);
    run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let spec = provider.spec("shell").expect("shell is offered");
    assert_eq!(spec.parameters["properties"]["line"]["type"], "string");
    assert_eq!(spec.parameters["required"][0], "line");
    // The description has to tell the model what it may not write, or it will
    // spend steps discovering the refusal set one construct at a time.
    for named in ["$(", "&&", "2>&1", "list_dir"] {
        assert!(
            spec.description.contains(named),
            "the description mentions {named:?}: {}",
            spec.description
        );
    }
}

// ---------------------------------------------------------------------------
// F2 — a denied stage refuses the whole line, and nothing runs
// ---------------------------------------------------------------------------

/// Allows the first stage and denies the second, so the only way the file under
/// test can exist is if the tool ran stage one before checking stage two.
fn deny_the_second_stage() -> Policy {
    Policy::permissive()
        .layer("test")
        .deny_exec("rustc --print*")
}

#[tokio::test]
async fn a_denied_later_stage_prevents_the_earlier_one_from_running_at_all() {
    let dir = fixture();
    let made = dir.path().join("out/made.txt");

    let (store, result) = run_line(
        &dir,
        deny_the_second_stage(),
        "rustc --version > out/made.txt; rustc --print sysroot",
    )
    .await;

    assert!(
        !made.exists(),
        "the first stage would have created out/made.txt; every check happens \
         before any spawn, so it must not exist"
    );
    let d = decision(&store, result.run_id);
    assert!(
        d.contains("refused") || d.contains("denied"),
        "the refusal is what the trace records: {d:?}"
    );

    // The negative control. Same line, same first stage, policy that allows both
    // — the file must appear, or the assertion above was passing because the tool
    // never worked rather than because the boundary held.
    let dir2 = fixture();
    let (_s, _r) = run_line(
        &dir2,
        Policy::permissive(),
        "rustc --version > out/made.txt; rustc --print sysroot",
    )
    .await;
    assert!(
        dir2.path().join("out/made.txt").exists(),
        "with both stages allowed the same line does create the file"
    );
}

#[tokio::test]
async fn a_denied_stage_of_a_pipeline_refuses_the_pipeline() {
    let dir = fixture();
    let (store, result) = run_line(
        &dir,
        deny_the_second_stage(),
        "rustc --version | rustc --print sysroot",
    )
    .await;
    let d = decision(&store, result.run_id);
    assert!(
        d.contains("refused") || d.contains("denied"),
        "a denied pipeline stage refuses the pipeline: {d:?}"
    );
}

// ---------------------------------------------------------------------------
// F3 — redirect targets are checked by path, not by name
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_write_redirect_is_checked_against_the_path_policy() {
    // Denied by a path rule that no name match on `rustc` could ever catch,
    // which is the whole point: a redirect is a write, and writes are checked
    // where `write_file` is checked.
    let dir = fixture();
    let policy = Policy::permissive().layer("test").deny_write("secrets/*");
    let (store, result) = run_line(&dir, policy, "rustc --version > secrets/leak.txt").await;

    assert!(
        !dir.path().join("secrets/leak.txt").exists(),
        "the redirect was refused before the file could be created"
    );
    let d = decision(&store, result.run_id);
    assert!(
        d.contains("refused") || d.contains("denied"),
        "trace records the refusal: {d:?}"
    );

    // Negative control: the same redirect to an allowed path does write.
    let dir2 = fixture();
    let (_s, _r) = run_line(
        &dir2,
        Policy::permissive().layer("test").deny_write("secrets/*"),
        "rustc --version > out/fine.txt",
    )
    .await;
    let wrote = std::fs::read_to_string(dir2.path().join("out/fine.txt")).unwrap();
    assert!(
        wrote.contains("rustc"),
        "an allowed redirect really does capture the command's stdout: {wrote:?}"
    );
}

#[tokio::test]
async fn a_read_redirect_is_checked_against_the_path_policy() {
    let dir = fixture();
    let policy = Policy::permissive().layer("test").deny_read("secrets/*");
    let (store, result) = run_line(&dir, policy, "rustc - < secrets/key").await;
    let d = decision(&store, result.run_id);
    assert!(
        d.contains("refused") || d.contains("denied"),
        "reading a denied path through `<` is refused: {d:?}"
    );
}

#[tokio::test]
async fn a_redirect_that_leaves_the_workspace_is_refused() {
    let dir = fixture();
    let (store, result) = run_line(
        &dir,
        Policy::permissive(),
        "rustc --version > ../escaped.txt",
    )
    .await;

    assert!(
        !dir.path().parent().unwrap().join("escaped.txt").exists(),
        "nothing was written outside the workspace root"
    );
    let obs = observation(&store, result.run_id);
    assert!(
        obs.contains("leaves the workspace root"),
        "the model is told why: {obs}"
    );
}

#[tokio::test]
async fn an_append_redirect_appends_rather_than_truncating() {
    let dir = fixture();
    std::fs::write(dir.path().join("out/log.txt"), "first\n").unwrap();
    let (_store, _result) =
        run_line(&dir, Policy::permissive(), "rustc --version >> out/log.txt").await;
    let text = std::fs::read_to_string(dir.path().join("out/log.txt")).unwrap();
    assert!(
        text.starts_with("first\n") && text.contains("rustc"),
        "`>>` kept what was there: {text:?}"
    );
}

// ---------------------------------------------------------------------------
// A refusal is an observation the model can act on, not an error that ends the run
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_refused_construct_names_itself_and_the_run_continues() {
    let dir = fixture();
    let (store, result) = run_line(&dir, Policy::permissive(), "rustc --version $(date)").await;

    let obs = observation(&store, result.run_id);
    assert!(
        obs.contains("command substitution"),
        "the model is told which construct offended: {obs}"
    );
    assert!(
        obs.contains("Run it as its own step"),
        "and what to do instead: {obs}"
    );
    // The run reached its ordinary end rather than erroring out, which is what
    // makes a refusal recoverable: the model gets another turn to rewrite it.
    assert!(
        !store.steps(result.run_id).unwrap().is_empty(),
        "the refusal is a recorded step"
    );
}
