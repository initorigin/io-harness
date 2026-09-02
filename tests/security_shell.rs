//! The two path seams the `shell` tool owns, through the full loop (0.74.0).
//!
//! `tests/security_p2.rs` closes H4 at `Workspace`, which is the door `write_file`
//! goes through. A redirect goes through a different one: `shell::resolve` builds
//! the path and `shell::apply_redirects` opens it, and neither ever called
//! `Workspace`. So the finding needed closing twice, and this file is the second
//! half — asserted through `run_with` rather than against the function, because
//! what is claimed is that the *tool* refuses, not that a helper returns `Err`.
//!
//! C1 is here for the same reason. Its fix at the profile lives in
//! `src/sandbox/macos.rs` and is tested there, on a string. The second line is
//! taken at the door where a model chooses a directory name — a `cd` target — and
//! that door is this tool.
//!
//! F2b is the half of H4 that fix left open. It closed the symlinked *parent* and
//! said so in as many words; the leaf was still followed, because a dangling link
//! reads as a file about to be created and because the redirect's open carried no
//! `O_NOFOLLOW` while `write_file`'s did. The two doors are claimed to answer
//! alike about the same path, so the tests here are the ones that say they do.
//!
//! Every refusal carries a companion showing the legitimate case still works. For
//! C1 the companions are most of the point: the set that would have been easy to
//! write is one that also refuses `My Project`, and a workspace named that is not
//! an attack. For F2b the companion is a leaf link pointing *inside* the root,
//! which a repository is allowed to contain and which must still be written and
//! read through.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

/// One `shell` call, then nothing — the loop finishes on the empty completion.
struct Script {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for Script {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

/// A workspace with a `docs/` and a `src/`, and a second directory standing in
/// for everything outside the root.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    (root, tempfile::tempdir().unwrap())
}

/// Run one `shell` line and hand back what the model reads about it.
///
/// The observation is taken from the *next* turn's prompt where there is one, for
/// the reason `tests/shell.rs` gives: reaching the agent's next turn is the
/// claim, and a return value nobody forwards would satisfy an assertion about the
/// return value.
async fn run_line(root: &std::path::Path, line: &str) -> String {
    let store = Store::memory().unwrap();
    let provider = Script {
        steps: vec![vec![ToolCall {
            name: "shell".into(),
            arguments: json!({ "line": line }),
        }]],
        at: AtomicUsize::new(0),
    };
    let result = run_with(
        &TaskContract::workspace("run some commands", root).with_max_steps(4),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();
    let steps = store.steps(result.run_id).unwrap();
    steps
        .get(1)
        .map(|s| s.prompt.clone())
        .unwrap_or_else(|| steps[0].result.clone())
}

// ---------------------------------------------------------------------------
// H4 — a redirect through a symlinked parent, with a leaf that does not exist
// ---------------------------------------------------------------------------

/// The audit's own line: `docs/ext -> $HOME`, and `echo x > docs/ext/.bashrc`.
///
/// `resolve` was purely lexical, so the target was "inside the root" as far as
/// anything could tell, `open_for_write` followed the link, and the file appeared
/// in the home directory. Nothing about the redirect was different from the
/// `write_file` half of H4 except which code built the path.
#[cfg(unix)]
#[tokio::test]
async fn h4_a_shell_redirect_cannot_create_a_file_through_a_symlinked_parent() {
    let (root, outside) = fixture();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();

    let obs = run_line(root.path(), "rustc --version > docs/ext/.bashrc").await;

    assert!(
        !outside.path().join(".bashrc").exists(),
        "nothing may be created outside the canonical root"
    );
    assert!(
        obs.contains("outside the workspace root"),
        "the model is told why: {obs}"
    );
}

/// The same link with a leaf that *does* exist. Already refused in 0.73.0 — here
/// so the fix for the creation half cannot quietly cost the half that worked.
#[cfg(unix)]
#[tokio::test]
async fn h4_a_shell_redirect_onto_an_existing_file_through_a_symlinked_parent_stays_denied() {
    let (root, outside) = fixture();
    std::fs::write(outside.path().join(".bashrc"), "the original\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();

    let obs = run_line(root.path(), "rustc --version > docs/ext/.bashrc").await;

    assert_eq!(
        std::fs::read_to_string(outside.path().join(".bashrc")).unwrap(),
        "the original\n",
        "the file outside the root was not touched: {obs}"
    );
}

/// Reading is the same seam. `< docs/ext/secret` never creates anything, so the
/// lexical check was the only thing between the link and `File::open`.
#[cfg(unix)]
#[tokio::test]
async fn h4_a_shell_input_redirect_cannot_read_through_a_symlinked_parent() {
    let (root, outside) = fixture();
    std::fs::write(outside.path().join("secret"), "hunter2\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();

    let obs = run_line(root.path(), "rustc --version < docs/ext/secret").await;

    assert!(
        obs.contains("outside the workspace root"),
        "a read through the link is refused too: {obs}"
    );
    assert!(!obs.contains("hunter2"), "{obs}");
}

/// `cd` through the same link chooses where every later stage runs, and that
/// directory is also what the sandbox is told to confine writes to.
#[cfg(unix)]
#[tokio::test]
async fn h4_a_cd_through_a_symlinked_parent_that_leaves_the_root_is_refused() {
    let (root, outside) = fixture();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();

    let obs = run_line(root.path(), "cd docs/ext && rustc --version").await;

    assert!(
        obs.contains("outside the workspace root"),
        "the model is told why: {obs}"
    );
}

// ---------------------------------------------------------------------------
// F2b — a redirect through a dangling link at the leaf
// ---------------------------------------------------------------------------

/// `docs/ext -> $HOME/.bashrc`, dangling, and `rustc --version > docs/ext`.
///
/// H4's fix asserted that the escape is the *parent*, not the leaf, and that
/// sentence is what left this open. `contain_under_root` could not tell a
/// dangling link from a file about to be created, so `resolve` graded the link's
/// own name; `open_for_write` had no `O_NOFOLLOW`, so `open(2)` followed the link
/// out of the root and created the file. Two things answer it: the containment
/// walk resolves a dangling link now, and the open refuses to follow one.
#[cfg(unix)]
#[tokio::test]
async fn f2b_a_redirect_through_a_dangling_leaf_symlink_cannot_leave_the_root() {
    let (root, outside) = fixture();
    let target = outside.path().join(".bashrc");
    std::os::unix::fs::symlink(&target, root.path().join("docs/ext")).unwrap();

    let obs = run_line(root.path(), "rustc --version > docs/ext").await;

    assert!(
        !target.exists(),
        "nothing may be created outside the canonical root: {obs}"
    );
    assert!(
        obs.contains("outside the workspace root"),
        "the model is told why: {obs}"
    );
}

/// Appending is the same open with a different flag, and had the same hole.
#[cfg(unix)]
#[tokio::test]
async fn f2b_an_appending_redirect_through_a_dangling_leaf_symlink_is_refused_too() {
    let (root, outside) = fixture();
    let target = outside.path().join("authorized_keys");
    std::os::unix::fs::symlink(&target, root.path().join("docs/keys")).unwrap();

    let obs = run_line(root.path(), "rustc --version >> docs/keys").await;

    assert!(!target.exists(), "{obs}");
}

/// The companion to F2b, and the capability the `O_NOFOLLOW` open must not cost.
/// A link at the leaf pointing *inside* the root still receives the redirect —
/// the open is retried once against where it points — and that holds whether the
/// file it names is already there or not.
#[cfg(unix)]
#[tokio::test]
async fn a_redirect_onto_a_leaf_symlink_that_stays_inside_the_root_still_writes_through() {
    let (root, _outside) = fixture();
    std::fs::write(root.path().join("src/log.txt"), "").unwrap();
    std::os::unix::fs::symlink("../src/log.txt", root.path().join("docs/here")).unwrap();
    std::os::unix::fs::symlink("../src/later.txt", root.path().join("docs/dangling")).unwrap();

    let obs = run_line(root.path(), "rustc --version > docs/here").await;
    let written = std::fs::read_to_string(root.path().join("src/log.txt"))
        .unwrap_or_else(|e| panic!("the redirect did not write through the leaf link: {e}: {obs}"));
    assert!(written.contains("rustc"), "{written}");

    let obs = run_line(root.path(), "rustc --version > docs/dangling").await;
    let written = std::fs::read_to_string(root.path().join("src/later.txt")).unwrap_or_else(|e| {
        panic!("a dangling link inside the root did not create what it names: {e}: {obs}")
    });
    assert!(written.contains("rustc"), "{written}");
}

/// The input redirect took the same opener in the same change, so it gets the
/// same companion: `< link` still reads the file a link inside the root names.
#[cfg(unix)]
#[tokio::test]
async fn an_input_redirect_through_a_leaf_symlink_inside_the_root_still_reads() {
    let (root, _outside) = fixture();
    std::fs::write(root.path().join("src/in.txt"), "hello from inside\n").unwrap();
    std::os::unix::fs::symlink("../src/in.txt", root.path().join("docs/src.txt")).unwrap();

    let obs = run_line(root.path(), "cat < docs/src.txt").await;

    assert!(
        obs.contains("hello from inside"),
        "the read follows a link that stays inside the root: {obs}"
    );
}

/// The companion. A link that stays *inside* the root is an ordinary part of a
/// repository and still resolves — containment is about where a path lands, not
/// about whether a link was involved.
#[cfg(unix)]
#[tokio::test]
async fn a_redirect_through_a_symbolic_link_that_stays_inside_the_root_still_works() {
    let (root, _outside) = fixture();
    std::os::unix::fs::symlink(root.path().join("src"), root.path().join("docs/inner")).unwrap();

    let obs = run_line(root.path(), "rustc --version > docs/inner/out.txt").await;

    let written = std::fs::read_to_string(root.path().join("src/out.txt")).unwrap_or_else(|e| {
        panic!("the redirect did not write through the inner link: {e}: {obs}")
    });
    assert!(written.contains("rustc"), "{written}");
}

/// The other companion. A redirect to a file that does not exist yet, with no
/// link anywhere, is the ordinary case and the one a containment check written
/// with `canonicalize` alone would have refused.
#[tokio::test]
async fn a_redirect_creating_a_new_file_in_the_workspace_still_works() {
    let (root, _outside) = fixture();

    let obs = run_line(root.path(), "rustc --version > docs/fresh.txt").await;

    let written = std::fs::read_to_string(root.path().join("docs/fresh.txt"))
        .unwrap_or_else(|e| panic!("a new file was not created: {e}: {obs}"));
    assert!(written.contains("rustc"), "{written}");
}

// ---------------------------------------------------------------------------
// C1 — a `cd` target whose *name* would become structure in a sandbox profile
// ---------------------------------------------------------------------------

/// The audit's payload, as a directory that really exists in the workspace.
///
/// `planned.cwd` is handed to the sandbox as the stage's workdir, and on macOS
/// that workdir is rendered into an SBPL string literal where the last matching
/// rule wins. A name carrying `")) (allow file-write* (subpath "/` appended its
/// own rules to the profile while the backend went on reporting a confining rung.
/// The profile refuses this now too; this is the door, and it refuses first and
/// with a reason the model can act on.
#[cfg(unix)]
#[tokio::test]
async fn c1_a_cd_into_a_directory_whose_name_can_close_a_profile_literal_is_refused() {
    let (root, _outside) = fixture();
    let hostile = r#"p")) (allow network*) (allow file-write* (subpath "/"#;
    std::fs::create_dir(root.path().join(hostile)).unwrap();

    let obs = run_line(root.path(), &format!("cd '{hostile}' && rustc --version")).await;

    assert!(
        obs.contains("double quote"),
        "the refusal names the character and the reason: {obs}"
    );
    assert!(
        !obs.contains("rustc 1."),
        "the line did not run in that directory: {obs}"
    );
}

/// The rest of the refused set, each one a character that can end a line or a
/// literal in generated text. A backslash and a newline are both expressible
/// through the single quotes the lexer passes through verbatim.
#[cfg(unix)]
#[tokio::test]
async fn c1_a_backslash_or_a_control_character_in_a_cd_target_is_refused_too() {
    let (root, _outside) = fixture();
    for name in ["back\\slash", "two\nlines"] {
        std::fs::create_dir(root.path().join(name)).unwrap();
        let obs = run_line(root.path(), &format!("cd '{name}' && rustc --version")).await;
        assert!(
            obs.contains("[shell refused]"),
            "`cd {name:?}` must be refused: {obs}"
        );
    }
}

/// The companions, and the reason the set is not a literal bare word.
///
/// Every one of these is an ordinary directory name somebody really has. A space
/// cannot end a string literal, and neither can a hyphen, a dot, an underscore, a
/// parenthesis, an apostrophe or a non-ASCII letter — so refusing them would cost
/// real workspaces and buy nothing. `Project (old)` is in the list deliberately:
/// parentheses are structure in SBPL *outside* a literal and characters inside
/// one, and this is the boundary that distinction draws.
#[tokio::test]
async fn c1_ordinary_directory_names_are_still_valid_cd_targets() {
    let (root, _outside) = fixture();
    for ordinary in [
        "My Project",
        "my-project",
        "my.project",
        "my_project",
        "Project (old)",
        "проект",
    ] {
        std::fs::create_dir(root.path().join(ordinary)).unwrap();
        std::fs::write(root.path().join(ordinary).join("marker"), "here\n").unwrap();

        let obs = run_line(
            root.path(),
            &format!("cd '{ordinary}' && rustc --version > out.txt"),
        )
        .await;

        let written = std::fs::read_to_string(root.path().join(ordinary).join("out.txt"))
            .unwrap_or_else(|e| panic!("`cd {ordinary}` did not work: {e}: {obs}"));
        assert!(written.contains("rustc"), "{ordinary}: {written}");
    }
}
