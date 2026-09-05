//! The language-server boundary, driven the way an attacker would drive it.
//!
//! Two adversaries, and neither is the operator. The first is the model: an
//! agent following instructions planted in a hostile repository, choosing the
//! `path` every navigation tool takes. The second is the language server itself,
//! because a repository decides which one a run spawns and a server answers
//! `textDocument/rename` with whatever files it likes.
//!
//! The harness is `tests/lsp.rs`'s — the same fixture server over the same real
//! framing, a scripted provider, a real run through the loop — so a test here
//! fails for a reason the whole path produced rather than for a reason a unit
//! constructed. What is different is the workspace: the root has a *sibling*
//! holding a file the run must never read, so `..` reaches something real and an
//! absolute path names something that exists. A test that pointed at a path with
//! nothing behind it would pass on a build that leaked, because there would be
//! nothing to leak.
//!
//! M12's regression lives in `src/lsp.rs`'s own test module instead. The exploit
//! is a server that lies in its frame *header*, and the fixture cannot be asked
//! to do that — it lengths every frame from the body it is about to write.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, Act, ApproveAll, Effect, LspServer, Policy, Provider, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

/// The string that must never reach the model. Distinctive enough that a match
/// anywhere in a transcript is proof rather than a coincidence.
const SECRET: &str = "AKIA-SHIBBOLETH-DO-NOT-LEAK";

// ---------------------------------------------------------------------------
// Harness — `tests/lsp.rs`'s, with a sibling directory outside the root
// ---------------------------------------------------------------------------

/// Where `cargo test` left the fixture example binary. See `tests/lsp.rs`.
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
fn fixture(root: &Path, script: Value) -> LspServer {
    let path = root.join("lsp-script.json");
    std::fs::write(&path, script.to_string()).unwrap();
    LspServer::new("fix", fixture_server().display().to_string())
        .with_env([(
            "IO_HARNESS_LSP_SCRIPT".to_string(),
            path.display().to_string(),
        )])
        // Short, so a test that is going to fail on an unready server fails in
        // seconds. Nothing here asserts on the duration.
        .with_timeout(std::time::Duration::from_secs(5))
}

/// A provider that plays a fixed script of tool calls and records what it saw.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: Mutex<Vec<String>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: Mutex::new(Vec::new()),
        }
    }

    /// Everything the loop put in front of the model, joined. Every tool
    /// observation is in here, which is where a leak would show up.
    fn transcript(&self) -> String {
        self.seen.lock().unwrap().join("\n")
    }
}

impl Provider for Script {
    fn name(&self) -> &str {
        "script"
    }

    async fn complete(&self, request: CompletionRequest) -> io_harness::Result<CompletionResponse> {
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

/// A workspace root with a secret sitting *beside* it rather than inside it.
///
/// The tempdir is the parent, so `../secret.txt` from the root reaches a real
/// file and the absolute form names one too. Returned as the whole tempdir so it
/// outlives the run; `root()` and `secret()` name the two paths in it.
struct Outside(tempfile::TempDir);

impl Outside {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "struct Ledger;\n\nimpl Ledger {\n    fn draw(&self) {}\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("secret.txt"),
            format!("aws_secret_access_key = {SECRET}\n"),
        )
        .unwrap();
        Self(dir)
    }

    fn root(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    fn secret(&self) -> PathBuf {
        self.0.path().join("secret.txt")
    }
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
        // Deliberately not satisfied by the workspace as it starts, so the run
        // ends on the step that writes `done.txt` rather than before the tool's
        // observation ever reached a prompt.
        .with_verification(Verification::WorkspaceFileContains {
            file: "done.txt".into(),
            needle: "ok".into(),
        })
        .with_max_steps(steps)
}

/// The step that satisfies the gate, so the run ends deliberately.
fn finish() -> Vec<ToolCall> {
    vec![call(
        "write_file",
        json!({"path": "done.txt", "content": "ok"}),
    )]
}

fn uri_of(path: &Path) -> String {
    let text = path.to_string_lossy().replace('\\', "/");
    if text.starts_with('/') {
        format!("file://{text}")
    } else {
        format!("file:///{text}")
    }
}

/// A `TextEdit` at a wire position — zero-based, as a server speaks it.
fn edit(line: u64, start: u64, len: u64, new_text: &str) -> Value {
    json!({
        "range": {
            "start": {"line": line, "character": start},
            "end": {"line": line, "character": start + len},
        },
        "newText": new_text
    })
}

/// A `Location` at a wire position.
fn location(root: &Path, rel: &str, line: u64, character: u64) -> Value {
    json!({
        "uri": uri_of(&root.join(rel)),
        "range": {
            "start": {"line": line, "character": character},
            "end": {"line": line, "character": character + 6},
        }
    })
}

// ---------------------------------------------------------------------------
// H7 — the model's `path` is checked for `Act::Read` before anything reads it
// ---------------------------------------------------------------------------

/// H7 — `lsp_hover` on a `..` that climbs out of the root reads nothing.
///
/// The exploit shape from the audit, one directory shorter because the sibling
/// is where the fixture can put a real file. Until 0.74.0 the path went model →
/// dispatch → `ws.root().join(path)` → `read_to_string` → `didOpen`, with no
/// `Act::Read` check anywhere on it, so the file's whole text crossed the
/// boundary into a third-party process and — through this script's
/// `echo-document` — straight back into the model's context.
///
/// `echo-document` is what makes the assertion mean something: the fixture
/// answers a hover with the text it was last *sent* for that document, so a
/// build that still syncs the file quotes the secret back and this fails. A
/// build that refuses first never sends a `didOpen`, and the server has nothing
/// to echo.
#[tokio::test]
async fn h7_a_dot_dot_path_out_of_the_root_is_refused_before_the_server_is_told_anything() {
    let outside = Outside::new();
    let root = outside.root();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        vec![call(
            "lsp_hover",
            json!({"path": "../secret.txt", "line": 1, "column": 1}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([fixture(
        &root,
        json!({"responses": {"textDocument/hover": "echo-document"}}),
    )]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains(SECRET),
        "a file outside the root reached the model: {transcript}"
    );
    assert!(
        // The refusal now comes from the gate rather than from `navigate`'s own
        // floor, so it is the crate's standard `[read refused]` observation and,
        // more to the point, a `policy_events` row — which is the half of H7 the
        // floor alone could not close. The floor is still there underneath for a
        // caller that reaches the session without the run loop.
        transcript.contains("[read refused]") && transcript.contains("../secret.txt"),
        "and the refusal names the act and the path it refused: {transcript}"
    );
}

/// H7 — an absolute `path` discards the root entirely, and is refused for it.
///
/// `Path::join` replaces rather than appends when its argument is absolute, so
/// `ws.root().join("/etc/shadow")` *is* `/etc/shadow`. Asserted separately from
/// the `..` case because they fail differently: a `..` climbs a root that is
/// still there, an absolute path means the root was never consulted.
#[tokio::test]
async fn h7_an_absolute_path_discards_the_root_and_is_refused() {
    let outside = Outside::new();
    let root = outside.root();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        vec![call(
            "lsp_hover",
            json!({"path": outside.secret().display().to_string(), "line": 1, "column": 1}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([fixture(
        &root,
        json!({"responses": {"textDocument/hover": "echo-document"}}),
    )]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains(SECRET),
        "an absolute path reached outside the root: {transcript}"
    );
    // 0.80.0 — the refusal moved and got more specific, which is F7 working.
    // An absolute read used to reach `policy().check()` with the containment
    // check skipped, so it was refused by whatever rule the policy happened to
    // carry and the message said only "refused by policy". `policy_verdict` asks
    // `check_path` for every read now, so the refusal names the reason: the path
    // escaped the workspace root. The property under test is unchanged — the
    // model is told, and the secret never reaches the transcript, which the
    // assertion above still checks.
    assert!(
        transcript.contains("read refused") && transcript.contains("escapes workspace root"),
        "and the refusal is named, with the reason it was refused: {transcript}"
    );
}

/// H7 — a `deny_read` rule now reaches the navigation tools' own argument.
///
/// The filter that existed before 0.74.0 was on the *answer*: a denied path was
/// dropped from a list of locations. Nothing looked at the path the question was
/// asked about, so `deny_read("secret/*")` filtered the file out of a
/// `lsp_references` answer while `lsp_hover` on that same file read it and
/// shipped it to the server.
#[tokio::test]
async fn h7_a_denied_in_root_path_is_refused_as_the_question_not_just_in_the_answer() {
    let outside = Outside::new();
    let root = outside.root();
    std::fs::create_dir_all(root.join("secret")).unwrap();
    std::fs::write(
        root.join("secret/keys.rs"),
        format!("const KEY: &str = \"{SECRET}\";\n"),
    )
    .unwrap();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        vec![call(
            "lsp_hover",
            json!({"path": "secret/keys.rs", "line": 1, "column": 1}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([fixture(
        &root,
        json!({"responses": {"textDocument/hover": "echo-document"}}),
    )]);
    let policy = permitted().layer("deny").deny_read("secret/*");

    run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains(SECRET),
        "a denied path was read and echoed back: {transcript}"
    );
    assert!(
        // Gate wording, as above — and the rule still names itself, which is what
        // this assertion is really about.
        transcript.contains("[read refused]") && transcript.contains("secret/*"),
        "and the rule that refused it is attributed: {transcript}"
    );
}

/// The companion H7 must not break: every tool still answers for an in-root
/// path, and a `..` that stays inside the root still resolves.
///
/// Five tools in one step, because the check is one guard at the top of
/// `navigate` and a guard that refused any of them would be a feature removed
/// rather than a boundary drawn. The last call spells the same file
/// `src/../src/lib.rs` — a `..` is not an escape by itself, and a fix that
/// rejected the character would break every path a model composes from a
/// directory and a name.
#[tokio::test]
async fn h7_companion_an_in_root_path_still_answers_through_every_tool() {
    let outside = Outside::new();
    let root = outside.root();
    let store = Store::memory().unwrap();

    let server = fixture(
        &root,
        json!({"responses": {
            "textDocument/definition": [location(&root, "src/lib.rs", 0, 7)],
            "textDocument/references": [location(&root, "src/lib.rs", 3, 7)],
            "textDocument/documentSymbol": [{
                "name": "Ledger",
                "kind": 23,
                "range": {"start": {"line": 0, "character": 7},
                          "end": {"line": 0, "character": 13}},
            }],
            "textDocument/rename": {"changes": {
                uri_of(&root.join("src/lib.rs")): [edit(0, 7, 6, "Tally")],
            }},
            "textDocument/hover": "echo-document",
        }}),
    );

    let provider = Script::new(vec![
        vec![
            call(
                "lsp_definition",
                json!({"path": "src/lib.rs", "line": 4, "column": 8}),
            ),
            call(
                "lsp_references",
                json!({"path": "src/lib.rs", "line": 1, "column": 8}),
            ),
            call("lsp_symbols", json!({"path": "src/lib.rs"})),
            call(
                "lsp_rename",
                json!({"path": "src/lib.rs", "line": 1, "column": 8, "new_name": "Tally"}),
            ),
            // The `..` that stays inside. Same file, spelled the way a model
            // composing a path from a directory does.
            call(
                "lsp_hover",
                json!({"path": "src/../src/lib.rs", "line": 1, "column": 8}),
            ),
        ],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains("refused by policy"),
        "an in-root path was refused: {transcript}"
    );
    // definition and references
    assert!(transcript.contains("src/lib.rs:1:8"), "{transcript}");
    assert!(transcript.contains("src/lib.rs:4:8"), "{transcript}");
    // symbols
    assert!(transcript.contains("Ledger (struct)"), "{transcript}");
    // rename
    assert!(transcript.contains("--- a/src/lib.rs"), "{transcript}");
    assert!(transcript.contains("+struct Tally;"), "{transcript}");
    // hover through the in-root `..`: the server echoes the document it was
    // sent, so the file was read and synced rather than refused. Asserted
    // against the tool's own observation header, so the file's own text sitting
    // somewhere else in the prompt could not satisfy it.
    assert!(
        transcript.contains("[lsp_hover]\nstruct Ledger;"),
        "{transcript}"
    );
}

// ---------------------------------------------------------------------------
// M11 — a server's `WorkspaceEdit` cannot name a file the run may not read
// ---------------------------------------------------------------------------

/// M11 — a rename whose edit names a file outside the workspace renders nothing.
///
/// The server is untrusted: the repository chooses which one runs. Before
/// 0.74.0 `rename_patch` accepted any URI whose effect was merely `!= Deny`,
/// read the file at that absolute path, and handed `diff::render` the before and
/// after — so every removed line of `~/.aws/credentials` arrived in the model's
/// context, once per file, as often as the model asked.
///
/// Under `permitted()` the old bar passes twice over: `allow_read("*")` does not
/// span a path with separators in it, and `Policy::default()`'s read default is
/// `Allow` anyway — so on 0.73.0's behaviour the effect check never refused this
/// at all and the secret is in the transcript. Containment is what closes it.
/// The in-root half of the same answer still renders, which is the assertion
/// that this filters rather than fails.
#[tokio::test]
async fn m11_a_server_naming_a_file_outside_the_root_has_it_omitted_not_read() {
    let outside = Outside::new();
    let root = outside.root();
    let store = Store::memory().unwrap();

    let server = fixture(
        &root,
        json!({"responses": {"textDocument/rename": {"changes": {
            uri_of(&root.join("src/lib.rs")): [edit(0, 7, 6, "Tally")],
            uri_of(&outside.secret()): [edit(0, 0, 3, "xxx")],
        }}}}),
    );
    let provider = Script::new(vec![
        vec![call(
            "lsp_rename",
            json!({"path": "src/lib.rs", "line": 1, "column": 8, "new_name": "Tally"}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains(SECRET),
        "the server read a file outside the root into the model's context: {transcript}"
    );
    assert!(
        !transcript.contains("secret.txt"),
        "the outside path is absent in any form: {transcript}"
    );
    assert!(
        transcript.contains("--- a/src/lib.rs") && transcript.contains("+struct Tally;"),
        "and the in-root half of the same answer still renders: {transcript}"
    );
    assert!(
        transcript.contains("1 file omitted"),
        "and the omission is stated rather than silent: {transcript}"
    );
}

/// M11 — `Ask` is not permission when the contents are what gets read.
///
/// The distinction the comment on `readable` draws is real and is kept: naming a
/// path is not reading it, so a location list still shows a path the policy only
/// asks about. Rendering a patch *is* reading it, and there is no approver on
/// this path to answer the question, so an unanswered `Ask` is a refusal here.
/// On 0.73.0's behaviour `!= Deny` passed and the whole file's diff was
/// rendered.
#[tokio::test]
async fn m11_an_ask_read_is_not_permission_to_render_a_file_as_a_patch() {
    let outside = Outside::new();
    let root = outside.root();
    let store = Store::memory().unwrap();

    let server = fixture(
        &root,
        json!({"responses": {"textDocument/rename": {"changes": {
            uri_of(&root.join("src/lib.rs")): [edit(0, 7, 6, "Tally")],
        }}}}),
    );
    let provider = Script::new(vec![
        vec![call(
            "lsp_rename",
            json!({"path": "src/lib.rs", "line": 1, "column": 8, "new_name": "Tally"}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([server]);
    // Reads under `src/` are asked about, never allowed. Writes stay open so the
    // step that ends the run still lands.
    let policy = Policy::default()
        .layer("app")
        .allow_write("*")
        .allow_exec("*")
        .rule(Act::Read, Effect::Ask, "src/*");

    run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    assert!(
        !transcript.contains("--- a/src/lib.rs"),
        "a file the policy only asks about was rendered as a patch: {transcript}"
    );
    assert!(
        !transcript.contains("-struct Ledger;"),
        "and none of its lines reached the model as a removal: {transcript}"
    );
    assert!(
        transcript.contains("1 file omitted"),
        "and the omission says an ask is not an allow: {transcript}"
    );
}

/// The companion M11 must not break: a legitimate rename across several in-root
/// files still produces its whole patch series.
///
/// Three files, because the failure a stricter bar invites is a filter that
/// keeps the file the question was asked about and drops its siblings — which
/// would leave a patch that renames a symbol in one place and breaks the build
/// everywhere else, silently.
#[tokio::test]
async fn m11_companion_a_rename_across_several_in_root_files_still_patches_them_all() {
    let outside = Outside::new();
    let root = outside.root();
    std::fs::write(
        root.join("src/other.rs"),
        "use crate::Ledger;\n\nfn use_it(l: &Ledger) {}\n",
    )
    .unwrap();
    std::fs::create_dir_all(root.join("src/deep")).unwrap();
    std::fs::write(root.join("src/deep/more.rs"), "fn f(l: &Ledger) {}\n").unwrap();
    let store = Store::memory().unwrap();

    let server = fixture(
        &root,
        json!({"responses": {"textDocument/rename": {"changes": {
            uri_of(&root.join("src/lib.rs")): [edit(0, 7, 6, "Tally")],
            uri_of(&root.join("src/other.rs")): [edit(0, 11, 6, "Tally"), edit(2, 14, 6, "Tally")],
            uri_of(&root.join("src/deep/more.rs")): [edit(0, 9, 6, "Tally")],
        }}}}),
    );
    let provider = Script::new(vec![
        vec![call(
            "lsp_rename",
            json!({"path": "src/lib.rs", "line": 1, "column": 8, "new_name": "Tally"}),
        )],
        finish(),
    ]);
    let contract = contract(&root, 4).with_lsp([server]);

    run_with(&contract, &provider, &store, &permitted(), &ApproveAll)
        .await
        .unwrap();

    let transcript = provider.transcript();
    for rel in ["src/lib.rs", "src/other.rs", "src/deep/more.rs"] {
        assert!(
            transcript.contains(&format!("--- a/{rel}")),
            "{rel} lost its section: {transcript}"
        );
    }
    assert!(
        !transcript.contains("file omitted"),
        "nothing legitimate was dropped: {transcript}"
    );
    assert!(
        transcript.contains("+struct Tally;") && transcript.contains("+use crate::Tally;"),
        "and the patch is the rename, not an empty hull: {transcript}"
    );
}
