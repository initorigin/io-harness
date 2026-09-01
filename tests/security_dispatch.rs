//! The dispatch seam, driven the way an attacker would drive it.
//!
//! Three findings meet in `src/run/dispatch.rs`, and each is the same shape: a
//! path or an act that reaches the OS through an arm that did not ask the gate
//! covering it.
//!
//! * **H3** — `git_worktree` is the one git built-in that *creates* the path the
//!   model named, and the only check under it refused a leading `-`.
//! * **L8** — a policy naming `git` has to mean it for every built-in, not only
//!   for the ones whose paths happen to be denied.
//! * **H7** — a navigation the policy refuses has to leave a row. A refusal
//!   nobody can read afterwards is the audit's whole complaint about this
//!   surface, and the fix that only *stopped* the read closed half of it.
//!
//! The git tests skip cleanly when the machine has no `git`, the way
//! `tests/git.rs` does — git is a runtime capability here, not a build
//! dependency. The LSP tests need `examples/lsp_fixture_server`; `cargo test`
//! builds it, `--lib --tests` does not.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{
    run_with, Act, ApproveAll, Effect, LspServer, PolicyEvent, Provider, Store, TaskContract,
    Verification,
};
use serde_json::{json, Value};

// --------------------------------------------------------------- the harness

struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
    seen: std::sync::Mutex<Vec<String>>,
}

impl Script {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
            seen: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// Everything the loop put in front of the model, joined.
    fn transcript(&self) -> String {
        self.seen.lock().unwrap().join("\n")
    }
}

impl Provider for Script {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.seen.lock().unwrap().push(req.user.clone());
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

/// A contract whose verification is never satisfied, so the run ends on the
/// step the script runs out rather than before a tool's observation was read.
fn contract(root: &Path, steps: u32) -> TaskContract {
    TaskContract::workspace("work the repository", root)
        .with_verification(Verification::WorkspaceFileContains {
            file: "README.md".into(),
            needle: "never satisfied".into(),
        })
        .with_max_steps(steps)
}

/// Every refusal row of one act, in order.
fn refusals(store: &Store, run_id: i64, act: &str) -> Vec<PolicyEvent> {
    store
        .events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| e.kind == "refusal" && e.act == act)
        .collect()
}

// --------------------------------------------------------------- git fixture

fn have_git() -> bool {
    std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
}

fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    std::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid")
        .output()
        .expect("git should be runnable once `have_git` said so")
}

/// A repository rooted at `ws` *inside* a tempdir.
///
/// The nesting is what makes the escape assertable: `..` from the workspace
/// root reaches a directory this test owns, so a build that still writes
/// outside the root writes somewhere the test can look — and somewhere
/// `TempDir` takes away afterwards. A test pointed at the machine's real `/tmp`
/// would either litter it or pass because something else had already made the
/// path.
struct Repo(tempfile::TempDir);

impl Repo {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("ws");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("README.md"), "hello\n").unwrap();
        git(&root, &["init", "--initial-branch=main"]);
        git(&root, &["add", "README.md"]);
        git(&root, &["commit", "-m", "first"]);
        Self(dir)
    }

    fn root(&self) -> PathBuf {
        self.0.path().join("ws")
    }

    /// A path beside the root, which nothing in a run may create.
    fn beside(&self, name: &str) -> PathBuf {
        self.0.path().join(name)
    }

    fn branches(&self) -> String {
        String::from_utf8_lossy(&git(&self.root(), &["branch", "--list"]).stdout).into_owned()
    }

    fn log(&self) -> String {
        String::from_utf8_lossy(&git(&self.root(), &["log", "--oneline"]).stdout).into_owned()
    }
}

async fn drive(root: &Path, steps: Vec<Vec<ToolCall>>, policy: &Policy, store: &Store) -> i64 {
    let n = steps.len() as u32 + 1;
    run_with(
        &contract(root, n),
        &Script::new(steps),
        store,
        policy,
        &ApproveAll,
    )
    .await
    .unwrap()
    .run_id
}

// ---------------------------------------------------------------------- H3
// `git_worktree` writes outside the workspace
// ---------------------------------------------------------------------------

/// H3 — a worktree path that leaves the root is denied, and no checkout appears
/// where it pointed.
///
/// Two spellings, because they were open for two different reasons and a fix
/// that closed one left the other.
///
/// * `../escaped` is the audit's own exploit, one directory shorter.
///   `Workspace::check_path` graded the *collapsed* form — `normalize` popped a
///   `..` off an empty vector and called the result `escaped` — so the gate
///   allowed it while `Git::argv` pushed the original string and `git worktree
///   add` wrote a full checkout a directory above the root.
/// * An absolute path was open a second way, and still was after `check_path`
///   was repaired: `gate` hands an absolute read or write target to the policy
///   directly rather than to `check_path`, which is the relaxation `read_skill`
///   needs and exactly wrong for the one built-in that creates the path it is
///   given. Under a policy whose writes are broad it wrote the checkout and an
///   *allow* row to match.
///
/// The refusal string is not the assertion. The absence on disk is: a build that
/// refuses in the transcript and creates the directory anyway has not fixed
/// anything.
#[tokio::test]
async fn h3_a_worktree_path_that_leaves_the_root_is_denied_and_writes_no_checkout() {
    if !have_git() {
        return;
    }
    let repo = Repo::new();
    let root = repo.root();
    let relative = repo.beside("escaped");
    let absolute = repo.beside("absolute-escape");
    let store = Store::memory().unwrap();

    // Permissive on purpose: containment is not a rule an operator writes, so it
    // has to hold with no rule behind it. A policy that denied the path would
    // prove the policy works, which was never in doubt.
    let run_id = drive(
        &root,
        vec![
            vec![call(
                "git_worktree",
                json!({"name": "agent/relative", "path": "../escaped"}),
            )],
            vec![call(
                "git_worktree",
                json!({"name": "agent/absolute", "path": absolute.display().to_string()}),
            )],
        ],
        &Policy::permissive(),
        &store,
    )
    .await;

    assert!(
        !relative.exists(),
        "a checkout was written above the root at {}",
        relative.display()
    );
    assert!(
        !absolute.exists(),
        "a checkout was written at an absolute path outside the root at {}",
        absolute.display()
    );

    // Neither branch was created either: `git worktree add -b` makes the branch
    // and the directory in one command, so a surviving branch means the command
    // ran.
    let branches = repo.branches();
    assert!(!branches.contains("agent/relative"), "{branches}");
    assert!(!branches.contains("agent/absolute"), "{branches}");

    // And both refusals are rows, attributed to no layer, because no layer can
    // permit a path with no meaning inside the workspace.
    let rows = refusals(&store, run_id, "write");
    assert_eq!(rows.len(), 2, "one row per refused worktree: {rows:?}");
    for row in &rows {
        assert!(
            row.rule.as_deref().is_some_and(|r| r.contains("escapes")),
            "the row says why it was refused: {row:?}"
        );
        assert_eq!(row.layer, None, "{row:?}");
    }
}

/// H3's companion — a worktree inside the root is still created, on its own
/// branch, under the same permissive policy.
///
/// The fix is a containment test and not a ban on the tool. A build that refused
/// every worktree would pass the test above and fail this one.
#[tokio::test]
async fn h3_companion_a_worktree_inside_the_root_is_still_created() {
    if !have_git() {
        return;
    }
    let repo = Repo::new();
    let root = repo.root();
    let store = Store::memory().unwrap();

    let run_id = drive(
        &root,
        vec![vec![call(
            "git_worktree",
            json!({"name": "agent/side", "path": ".worktrees/side"}),
        )]],
        &Policy::permissive(),
        &store,
    )
    .await;

    assert!(
        root.join(".worktrees/side/README.md").is_file(),
        "the allowed worktree is a real checkout"
    );
    assert!(repo.branches().contains("agent/side"));
    assert!(
        refusals(&store, run_id, "write").is_empty(),
        "nothing was refused"
    );
}

// ---------------------------------------------------------------------- L8
// `deny_exec("git")` reaches every git built-in
// ---------------------------------------------------------------------------

/// L8 — a policy denying the `git` program refuses `git_commit`, not only the
/// built-ins whose paths it also denies.
///
/// The audit read this arm as checking `Act::Write` on `.git` and nothing else.
/// It checks the program first and it has since 0.70.0 — this locks that in on a
/// built-in that names no path at all, which is where a check derived from the
/// paths would have nothing to derive from. The approver count is not asserted
/// here because `ApproveAll` would approve; the refusal row is the evidence, and
/// a `Deny` never reaches an approver (`tests/ask_is_not_deny.rs` asserts that
/// half).
#[tokio::test]
async fn l8_a_policy_denying_the_git_program_refuses_a_git_commit() {
    if !have_git() {
        return;
    }
    let repo = Repo::new();
    let root = repo.root();
    let store = Store::memory().unwrap();
    let before = repo.log().lines().count();

    let run_id = drive(
        &root,
        vec![
            vec![call(
                "write_file",
                json!({"path": "NOTES.md", "content": "written by the agent\n"}),
            )],
            vec![call("git_add", json!({"paths": ["NOTES.md"]}))],
            vec![call("git_commit", json!({"message": "should never land"}))],
        ],
        &Policy::permissive().layer("app").deny_exec("git"),
        &store,
    )
    .await;

    assert_eq!(
        repo.log().lines().count(),
        before,
        "no commit was made: {}",
        repo.log()
    );
    let rows = refusals(&store, run_id, "exec");
    assert_eq!(
        rows.len(),
        2,
        "one row for `git_add` and one for `git_commit`: {rows:?}"
    );
    for row in &rows {
        assert_eq!(
            row.target, "git",
            "the program is what was refused: {row:?}"
        );
        assert_eq!(row.layer.as_deref(), Some("app"), "{row:?}");
    }
}

/// L8's companion — the same three steps under a policy that allows `git` still
/// produce the commit.
#[tokio::test]
async fn l8_companion_a_policy_that_allows_git_still_commits() {
    if !have_git() {
        return;
    }
    let repo = Repo::new();
    let root = repo.root();
    let store = Store::memory().unwrap();
    let before = repo.log().lines().count();

    let run_id = drive(
        &root,
        vec![
            vec![call(
                "write_file",
                json!({"path": "NOTES.md", "content": "written by the agent\n"}),
            )],
            vec![call("git_add", json!({"paths": ["NOTES.md"]}))],
            vec![call("git_commit", json!({"message": "add notes"}))],
        ],
        &Policy::permissive(),
        &store,
    )
    .await;

    let log = repo.log();
    assert_eq!(log.lines().count(), before + 1, "{log}");
    assert!(log.contains("add notes"), "{log}");
    assert!(refusals(&store, run_id, "exec").is_empty());
}

// ---------------------------------------------------------------------- H7
// a refused navigation is a row, not only a silence
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

/// A configured server whose answers are the script written beside the
/// workspace.
fn fixture(root: &Path) -> LspServer {
    let path = root.join("lsp-script.json");
    std::fs::write(
        &path,
        json!({"responses": {"textDocument/hover": "echo-document"}}).to_string(),
    )
    .unwrap();
    LspServer::new("fix", fixture_server().display().to_string())
        .with_env([(
            "IO_HARNESS_LSP_SCRIPT".to_string(),
            path.display().to_string(),
        )])
        .with_timeout(std::time::Duration::from_secs(5))
}

/// A workspace with one ordinary file and one the policy will speak about.
fn code() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("secret")).unwrap();
    std::fs::write(dir.path().join("README.md"), "hello\n").unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "struct Ledger;\n\nimpl Ledger {\n    fn draw(&self) {}\n}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("secret/keys.rs"),
        "const KEY: &str = \"x\";\n",
    )
    .unwrap();
    dir
}

/// H7 — a navigation the policy denies leaves a `policy_events` row naming the
/// rule that stopped it.
///
/// `LspSession::navigate` learned to refuse the path this release, and that
/// stopped the read. It could not record it: `navigate` has no `Store`, no step
/// and no depth, and every persisted row in this crate is written by `gate`. So
/// the refusal was invisible — an operator auditing the trace for what the agent
/// tried to read saw nothing at all, which is the same silence H7 is about in
/// the direction that leaks. The gate call in the three LSP arms is what writes
/// it.
#[tokio::test]
async fn h7_a_navigation_the_policy_denies_is_recorded_as_a_refusal_row() {
    let dir = code();
    let root = dir.path();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        vec![call(
            "lsp_hover",
            json!({"path": "secret/keys.rs", "line": 1, "column": 1}),
        )],
        vec![],
    ]);
    let policy = Policy::default()
        .layer("app")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
        .deny_read("secret/*");
    let contract = contract(root, 3).with_lsp([fixture(root)]);

    let run_id = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap()
        .run_id;

    let rows = refusals(&store, run_id, "read");
    assert_eq!(rows.len(), 1, "one refusal row for the question: {rows:?}");
    assert_eq!(rows[0].target, "secret/keys.rs");
    assert_eq!(rows[0].rule.as_deref(), Some("secret/*"));
    assert_eq!(rows[0].layer.as_deref(), Some("app"));
    assert!(
        provider.transcript().contains("read refused"),
        "and the model was told: {}",
        provider.transcript()
    );
}

/// H7's companion — a navigation the policy asks about reaches the approver and,
/// approved, still answers.
///
/// This is the release's one user-visible change on this surface: a policy whose
/// read tier is `Ask` now prompts on a navigation where it passed silently
/// before. It is the treatment `read_file` has always had, and the row it leaves
/// is a `decision`, not a `refusal` — a build that turned the new gate into a
/// blanket refusal fails here rather than in a bug report.
#[tokio::test]
async fn h7_companion_an_asked_navigation_is_approved_answered_and_recorded() {
    let dir = code();
    let root = dir.path();
    let store = Store::memory().unwrap();

    let provider = Script::new(vec![
        vec![call(
            "lsp_hover",
            json!({"path": "src/lib.rs", "line": 1, "column": 8}),
        )],
        vec![],
    ]);
    let policy = Policy::default()
        .layer("app")
        .allow_write("*")
        .allow_exec("*")
        .rule(Act::Read, Effect::Ask, "src/*");
    let contract = contract(root, 3).with_lsp([fixture(root)]);

    let run_id = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap()
        .run_id;

    assert!(
        refusals(&store, run_id, "read").is_empty(),
        "an approved navigation is not a refusal"
    );
    let approved: Vec<_> = store
        .events(run_id)
        .unwrap()
        .into_iter()
        .filter(|e| {
            e.kind == "decision"
                && e.act == "read"
                && e.target == "src/lib.rs"
                && e.decision.as_deref() == Some("approve")
        })
        .collect();
    assert_eq!(approved.len(), 1, "the approval is a row: {approved:?}");
    // The server answered, so the gate let the question through rather than
    // swallowing it.
    let transcript = provider.transcript();
    assert!(
        transcript.contains("[lsp_hover]") && !transcript.contains("lsp_hover unavailable"),
        "the navigation still answered: {transcript}"
    );
}
