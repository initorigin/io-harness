//! Codebase navigation through the full loop, against a real server process.
//!
//! The server is `examples/lsp_fixture_server.rs`, spawned as a child over stdio
//! and speaking the real protocol — real framing, a real `initialize`, real
//! `didOpen` notifications. Nothing is mocked at the protocol level, which is the
//! only way these tests can fail for the reasons they exist to catch.
//!
//! It is a fixture rather than `rust-analyzer` because the paths that matter here
//! are the ones a real server will not perform on request: a handshake that never
//! finishes, a server that exits, a capability that is absent. The one live test
//! against a real server is `tests/lsp_live.rs`, which is opt-in and outside the
//! default gate.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::observe::{EventKind, Flow, Observer, RunEvent};
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, ApproveAll, Error, LspServer, Policy, Provider, RunOutcome, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

/// Where `cargo test` left the fixture example binary. See `tests/mcp.rs`, whose
/// reasoning this follows exactly.
fn fixture_server() -> PathBuf {
    let mut dir = std::env::current_exe().expect("the test binary has a path");
    dir.pop();
    if dir.ends_with("deps") {
        dir.pop();
    }
    let exe = format!("lsp_fixture_server{}", std::env::consts::EXE_SUFFIX);
    let path = dir.join("examples").join(&exe);
    assert!(
        path.exists(),
        "fixture server not built at {}. `cargo test` builds examples; \
         run `cargo build --example lsp_fixture_server` if invoking the test binary directly.",
        path.display()
    );
    path
}

/// A configured server whose answers are the script written beside the workspace.
fn fixture(dir: &Path, script: Value) -> LspServer {
    fixture_touching(dir, script, None)
}

/// The same, optionally leaving proof on disk that the process ever ran.
///
/// One builder rather than two `with_env` calls: `with_env` replaces the map the
/// way every other `with_*` builder replaces, so a second call would silently
/// drop the script and the server would answer nothing — which reads exactly like
/// a filter bug.
fn fixture_touching(dir: &Path, script: Value, marker: Option<&Path>) -> LspServer {
    let path = dir.join("lsp-script.json");
    std::fs::write(&path, script.to_string()).unwrap();
    let mut env = vec![(
        "IO_HARNESS_LSP_SCRIPT".to_string(),
        path.display().to_string(),
    )];
    if let Some(marker) = marker {
        env.push((
            "IO_HARNESS_LSP_TOUCH".to_string(),
            marker.display().to_string(),
        ));
    }
    LspServer::new("fix", fixture_server().display().to_string())
        .with_env(env)
        // Short, so a test that is going to fail on an unready server fails in
        // seconds. Nothing here asserts on the duration.
        .with_timeout(std::time::Duration::from_secs(5))
}

/// A provider that plays a fixed script of tool calls and records what it was
/// offered and what came back.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    offered: Mutex<Vec<String>>,
    seen: Mutex<Vec<String>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            offered: Mutex::new(Vec::new()),
            seen: Mutex::new(Vec::new()),
        }
    }

    fn tools_offered(&self) -> Vec<String> {
        self.offered.lock().unwrap().clone()
    }

    /// Everything the loop put in front of the model, joined. The observations a
    /// tool produced are in here, which is where a navigation answer shows up.
    fn transcript(&self) -> String {
        self.seen.lock().unwrap().join("\n")
    }
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        *self.offered.lock().unwrap() = request.tools.iter().map(|t| t.name.clone()).collect();
        self.seen.lock().unwrap().push(request.user.clone());
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// A workspace with one file whose lines are known, so a position means something.
fn workspace() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "struct Ledger;\n\nimpl Ledger {\n    fn draw(&self) {}\n}\n",
    )
    .unwrap();
    dir
}

fn permitted() -> Policy {
    Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
}

fn contract(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace("navigate the code", root)
        // Deliberately not satisfied by the workspace as it starts. The gate runs
        // after every step, so a criterion that is already true would end the run
        // before the tool's own observation ever reached a prompt — and every
        // assertion here is about what the model was shown.
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(steps)
}

/// The step that satisfies the gate, so the run ends deliberately rather than by
/// running out of script.
fn finish() -> Vec<ToolCall> {
    vec![call("write_file", json!({"path": "done.txt", "content": "ok"}))]
}

fn uri(dir: &Path, rel: &str) -> String {
    let text = dir.join(rel).to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// A `Location` at a wire position — zero-based, as a server speaks it.
fn location(dir: &Path, rel: &str, line: u64, character: u64) -> Value {
    json!({
        "uri": uri(dir, rel),
        "range": {
            "start": {"line": line, "character": character},
            "end": {"line": line, "character": character + 6},
        }
    })
}

/// Collect every event a run emitted.
#[derive(Default)]
struct Seen(Mutex<Vec<EventKind>>);

impl Observer for Seen {
    fn event(&self, event: &RunEvent) -> Flow {
        self.0.lock().unwrap().push(event.kind.clone());
        Flow::Continue
    }
}

// ---------------------------------------------------------------------------
// F3 — positions are 1-based in and out, 0-based on the wire
// ---------------------------------------------------------------------------

/// The model counts from one, the protocol counts from zero, and an off-by-one
/// here answers the neighbouring line — a wrong answer that reads like a right
/// one. Asserted on the wire body the server received (through the fixture's
/// `echo-position`) and on the line number the model was shown.
#[tokio::test]
async fn a_position_is_one_based_for_the_model_and_zero_based_on_the_wire() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![call(
        "lsp_hover",
        json!({"path": "src/lib.rs", "line": 12, "column": 5}),
    )], finish()]);
    let contract = contract(dir.path(), 4).with_lsp([fixture(
        dir.path(),
        json!({"responses": {"textDocument/hover": "echo-position"}}),
    )]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        transcript.contains("line=11 character=4"),
        "the wire carries the zero-based pair: {transcript}"
    );
}

/// Line 0 is not a line any file has. A model that sends one is off by one, and
/// clamping it would answer confidently about the wrong place.
#[tokio::test]
async fn a_zero_line_is_refused_by_name_rather_than_clamped() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![call(
        "lsp_hover",
        json!({"path": "src/lib.rs", "line": 0, "column": 1}),
    )], finish()]);
    let contract = contract(dir.path(), 4).with_lsp([fixture(
        dir.path(),
        json!({"responses": {"textDocument/hover": "echo-position"}}),
    )]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(transcript.contains("1-based"), "{transcript}");
    assert!(
        !transcript.contains("line=") && !transcript.contains("character="),
        "and the server was never asked: {transcript}"
    );
}

// ---------------------------------------------------------------------------
// F4 — a file edited this run is never answered from a stale buffer
// ---------------------------------------------------------------------------

/// The server's view of a file is whatever was last sent to it. A client that
/// opens each document once answers from the text as it was before the agent's
/// own edit — which is the defect this release is most likely to ship, because
/// opening once is the obvious spelling and every test that does not edit first
/// passes under it.
#[tokio::test]
async fn a_file_edited_this_run_is_re_sent_before_it_is_asked_about() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let server = fixture(
        dir.path(),
        json!({"responses": {"textDocument/hover": "echo-document"}}),
    );
    let provider = Script::new(vec![
        // Ask once, so the document is opened with the text as it is now.
        vec![call(
            "lsp_hover",
            json!({"path": "src/lib.rs", "line": 1, "column": 1}),
        )],
        // Then change it.
        vec![call(
            "edit_file",
            json!({"path": "src/lib.rs", "search": "struct Ledger;", "replace": "struct Tally;"}),
        )],
        // And ask again. The answer is the text the server holds.
        vec![call(
            "lsp_hover",
            json!({"path": "src/lib.rs", "line": 1, "column": 1}),
        )],
        finish(),
    ]);
    let contract = contract(dir.path(), 6).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        transcript.contains("struct Tally;"),
        "the second answer is the post-edit text: {transcript}"
    );
}

// ---------------------------------------------------------------------------
// F5 — the spawn is gated, and a refusal starts no process
// ---------------------------------------------------------------------------

/// A refusal is only worth asserting on the absence of the child: an error saying
/// "refused" while a server is already running reads identically from the message.
///
/// **The absent marker file is not, on its own, that assertion, and this is a
/// finding rather than a design.** The first version of this test asserted only
/// that the fixture had not written its marker — and a build that spawns first
/// and checks second still passed it, because the child is dropped on the way out
/// with `kill_on_drop` and can be killed before it is ever scheduled. An
/// assertion that depends on losing a race is not an assertion.
///
/// What is race-free is *which error comes back* for a command that does not
/// exist. If the gate runs first, the answer is `Refused` and no spawn is
/// attempted. If the spawn runs first, it fails on its own — a different, typed
/// error — and no scheduling decision can change which one it is.
#[tokio::test]
async fn a_denied_server_is_refused_before_any_spawn_is_attempted() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let missing = "io-harness-no-such-language-server";

    let denying = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .deny_exec(missing);

    let provider = Script::new(vec![vec![]]);
    let absent = contract(dir.path(), 4).with_lsp([LspServer::new("fix", missing)]);
    let err = run_with(&absent, &provider, &store, &denying, &ApproveAll)
        .await
        .unwrap_err();

    // `Refused`, not `Lsp { reason: "could not spawn ..." }`. The second is what a
    // build that spawns before it checks returns, whatever the timing.
    assert!(
        matches!(&err, Error::Refused { act, target, .. } if act == "exec" && target == missing),
        "{err:?}"
    );

    // And the same claim once more against a command that *does* exist, where the
    // marker is the belt to that braces: under a correct build no child is ever
    // created, so there is nothing to race.
    let marker = dir.path().join("the-server-ran");
    let server = fixture_touching(dir.path(), json!({}), Some(&marker));
    let denying = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .deny_exec(&fixture_server().display().to_string());
    let present = contract(dir.path(), 4).with_lsp([server]);
    let err = run_with(
        &present,
        &Script::new(vec![vec![]]),
        &Store::memory().unwrap(),
        &denying,
        &ApproveAll,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(&err, Error::Refused { act, .. } if act == "exec"),
        "{err:?}"
    );
    assert!(!marker.exists(), "and no process wrote its marker");
}

/// The other half of the same claim, in the same shape: allowed, the identical
/// configuration starts and answers.
#[tokio::test]
async fn an_allowed_server_starts_answers_and_reports_that_it_came_up() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let marker = dir.path().join("the-server-ran");
    let server = fixture_touching(
        dir.path(),
        json!({"responses": {"textDocument/definition": [location(dir.path(), "src/lib.rs", 0, 7)]}}),
        Some(&marker),
    );

    let provider = Script::new(vec![vec![call(
        "lsp_definition",
        json!({"path": "src/lib.rs", "line": 4, "column": 8}),
    )], finish()]);
    let contract = contract(dir.path(), 4).with_lsp([server]);
    let seen = Seen::default();

    io_harness::run_with_observed(
        &contract,
        &provider,
        &store,
        &permitted(),
        &ApproveAll,
        &seen,
    )
    .await
    .unwrap();

    assert!(marker.exists(), "the process was started");
    let transcript = provider.transcript();
    assert!(
        transcript.contains("src/lib.rs:1:8"),
        "the location comes back 1-based: {transcript}"
    );

    let events = seen.0.lock().unwrap();
    let started = events
        .iter()
        .filter(|k| matches!(k, EventKind::LspStarted { .. }))
        .count();
    assert_eq!(started, 1, "exactly one LspStarted per server per run");
    assert!(events.iter().any(
        |k| matches!(k, EventKind::LspStarted { server, root, .. } if server == "fix" && !root.is_empty())
    ));
}

// ---------------------------------------------------------------------------
// F6 — a location the policy will not let the run read is absent, and said so
// ---------------------------------------------------------------------------

/// A shorter list with nothing said is a wrong answer to "who calls this": the
/// model reads two call sites where there are three and concludes it has seen
/// them all. Asserted as an absence and a presence together.
#[tokio::test]
async fn a_denied_location_is_omitted_from_the_answer_and_the_omission_is_named() {
    let dir = workspace();
    std::fs::create_dir_all(dir.path().join("secret")).unwrap();
    std::fs::write(dir.path().join("secret/keys.rs"), "const KEY: &str = \"x\";\n").unwrap();
    let store = Store::memory().unwrap();

    let server = fixture(
        dir.path(),
        json!({"responses": {"textDocument/references": [
            location(dir.path(), "src/lib.rs", 3, 4),
            location(dir.path(), "secret/keys.rs", 0, 6),
            location(dir.path(), "src/lib.rs", 0, 7),
        ]}}),
    );
    let provider = Script::new(vec![vec![call(
        "lsp_references",
        json!({"path": "src/lib.rs", "line": 1, "column": 8}),
    )], finish()]);
    let contract = contract(dir.path(), 4).with_lsp([server]);
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
        .deny_read("secret/*");

    run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(transcript.contains("src/lib.rs:4:5"), "{transcript}");
    assert!(transcript.contains("src/lib.rs:1:8"), "{transcript}");
    assert!(
        !transcript.contains("keys.rs"),
        "the denied path is absent in any form: {transcript}"
    );
    assert!(
        transcript.contains("1 result omitted"),
        "and the omission is named: {transcript}"
    );
}

// ---------------------------------------------------------------------------
// F7 — rename answers with a patch and writes nothing
// ---------------------------------------------------------------------------

/// A rename that wrote its own files would satisfy the end state and violate the
/// release. Asserted as an absence first, then as an application: the patch this
/// returns is fed to `patch_file`, and the result is the renamed tree.
#[tokio::test]
async fn a_rename_returns_a_patch_series_and_writes_nothing() {
    let dir = workspace();
    std::fs::write(
        dir.path().join("src/other.rs"),
        "use crate::Ledger;\n\nfn use_it(l: &Ledger) {}\n",
    )
    .unwrap();
    let before_lib = std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap();
    let before_other = std::fs::read_to_string(dir.path().join("src/other.rs")).unwrap();
    let store = Store::memory().unwrap();

    // `Ledger` at src/lib.rs line 1 columns 8..14, and twice in src/other.rs.
    let edit = |line: u64, start: u64| {
        json!({
            "range": {
                "start": {"line": line, "character": start},
                "end": {"line": line, "character": start + 6},
            },
            "newText": "Tally"
        })
    };
    let server = fixture(
        dir.path(),
        json!({"responses": {"textDocument/rename": {"changes": {
            uri(dir.path(), "src/lib.rs"): [edit(0, 7)],
            uri(dir.path(), "src/other.rs"): [edit(0, 11), edit(2, 17)],
        }}}}),
    );

    let provider = Script::new(vec![vec![call(
        "lsp_rename",
        json!({"path": "src/lib.rs", "line": 1, "column": 8, "new_name": "Tally"}),
    )], finish()]);
    let contract = contract(dir.path(), 4).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    // Nothing was written.
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/lib.rs")).unwrap(),
        before_lib
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("src/other.rs")).unwrap(),
        before_other
    );
    // The run's only write is the step that ends it. Neither renamed path has an
    // edits row, which is the assertion that a build applying the edit inside the
    // tool would fail.
    let written: Vec<_> = store
        .edits(1)
        .unwrap()
        .into_iter()
        .map(|e| e.path)
        .collect();
    assert_eq!(written, ["done.txt"], "a rename writes nothing itself");

    let transcript = provider.transcript();
    assert!(transcript.contains("--- a/src/lib.rs"), "{transcript}");
    assert!(transcript.contains("--- a/src/other.rs"), "{transcript}");
    assert!(transcript.contains("+struct Tally;"), "{transcript}");
    assert!(
        transcript.contains("Nothing has been written"),
        "{transcript}"
    );
}

// ---------------------------------------------------------------------------
// F9 — an unready or dead server is unknown-and-why, never empty
// ---------------------------------------------------------------------------

/// Three ways of having no answer, and none of them is silence. An empty result
/// for "where is this defined" reads as "nowhere", which is the one thing this
/// surface must never say by accident.
#[tokio::test]
async fn a_server_that_cannot_answer_says_why_rather_than_answering_nothing() {
    for (name, script) in [
        ("hangs its handshake", json!({"hang_initialize": true})),
        ("exits after it", json!({"exit_after_initialize": true})),
        (
            "does not advertise the capability",
            json!({"capabilities": {"hoverProvider": true}}),
        ),
    ] {
        let dir = workspace();
        let store = Store::memory().unwrap();
        let server = fixture(dir.path(), script)
            // Short enough that the hanging arm is a test rather than a wait, and
            // asserted on structure rather than on how long it took.
            .with_timeout(std::time::Duration::from_secs(2));
        let provider = Script::new(vec![vec![call(
            "lsp_definition",
            json!({"path": "src/lib.rs", "line": 1, "column": 8}),
        )], finish()]);
        let contract = contract(dir.path(), 4).with_lsp([server]);

        run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
            .await
            .unwrap();

        let transcript = provider.transcript();
        assert!(
            transcript.contains("unavailable"),
            "{name}: the answer names the failure: {transcript}"
        );
        assert!(
            transcript.contains("language server fix"),
            "{name}: and names the server: {transcript}"
        );
        assert!(
            !transcript.contains("No locations."),
            "{name}: it must not read as 'there are none': {transcript}"
        );
    }
}

// ---------------------------------------------------------------------------
// F10 — a run with no server configured is 0.51.0's run
// ---------------------------------------------------------------------------

/// The release's negative control. "Free for a consumer who does not want it" is
/// either true on bytes or it is marketing: under 0.38.0's cacheable prefix every
/// schema is paid for on every request of every run.
#[tokio::test]
async fn a_run_with_no_server_is_offered_no_lsp_tool_at_all() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let provider = Script::new(vec![vec![]]);

    run_with(
        &contract(dir.path(), 2),
        &provider,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let offered = provider.tools_offered();
    assert!(
        offered.iter().any(|t| t == "grep"),
        "the built-ins are there: {offered:?}"
    );
    assert!(
        !offered.iter().any(|t| t.starts_with("lsp_")),
        "and no lsp tool is: {offered:?}"
    );
}

/// The other half: configured, exactly five schemas appear and nothing else moves.
#[tokio::test]
async fn a_configured_run_gains_exactly_five_schemas() {
    let dir = workspace();
    let store = Store::memory().unwrap();

    let without = Script::new(vec![vec![]]);
    run_with(
        &contract(dir.path(), 2),
        &without,
        &store,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let with = Script::new(vec![vec![]]);
    let store2 = Store::memory().unwrap();
    run_with(
        &contract(dir.path(), 2).with_lsp([fixture(dir.path(), json!({}))]),
        &with,
        &store2,
        &permitted(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let before = without.tools_offered();
    let after = with.tools_offered();
    let added: Vec<_> = after.iter().filter(|t| !before.contains(t)).collect();
    assert_eq!(
        added,
        [
            "lsp_definition",
            "lsp_references",
            "lsp_symbols",
            "lsp_hover",
            "lsp_rename"
        ]
        .iter()
        .collect::<Vec<_>>(),
        "exactly the five, in a stable order"
    );
    let removed: Vec<_> = before.iter().filter(|t| !after.contains(t)).collect();
    assert!(removed.is_empty(), "and nothing is taken away: {removed:?}");
}

// ---------------------------------------------------------------------------
// Symbols — one schema, two behaviours
// ---------------------------------------------------------------------------

/// No `query` is this file's symbols; a `query` is the workspace's. Two schemas
/// for one question would be prompt bytes on every request of every run.
#[tokio::test]
async fn symbols_answers_for_one_file_or_for_the_workspace() {
    let dir = workspace();
    let store = Store::memory().unwrap();
    let server = fixture(
        dir.path(),
        json!({"responses": {
            "textDocument/documentSymbol": [{
                "name": "Ledger", "kind": 23,
                "range": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 13}},
                "selectionRange": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}},
                "children": [{
                    "name": "draw", "kind": 6,
                    "range": {"start": {"line": 3, "character": 4}, "end": {"line": 3, "character": 20}},
                    "selectionRange": {"start": {"line": 3, "character": 7}, "end": {"line": 3, "character": 11}}
                }]
            }],
            "workspace/symbol": [{
                "name": "Ledger", "kind": 23,
                "location": {
                    "uri": uri(dir.path(), "src/lib.rs"),
                    "range": {"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 13}}
                }
            }]
        }}),
    );
    let provider = Script::new(vec![
        vec![call("lsp_symbols", json!({"path": "src/lib.rs"}))],
        vec![call("lsp_symbols", json!({"query": "Ledger"}))],
        finish(),
    ]);
    let contract = contract(dir.path(), 4).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        transcript.contains("Ledger (struct)"),
        "the kind is a word, not a number: {transcript}"
    );
    assert!(
        transcript.contains("draw (method)"),
        "and children are flattened under their parent: {transcript}"
    );
    assert!(
        transcript.contains("src/lib.rs:1:8"),
        "the workspace search reports where: {transcript}"
    );
}
