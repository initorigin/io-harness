//! The P2 root pattern: a path reaching the filesystem without passing the gate
//! meant to cover it.
//!
//! Every check that decided containment by `canonicalize` decided nothing at all
//! for a path whose leaf does not exist — and the leaf of a write is exactly what
//! usually does not exist. These are the four findings that shared that shape
//! (H3, H4, M6) plus the missing ceiling on the one read every document parser
//! starts at (M15), each named for the finding it closes.
//!
//! The three tests without a finding ID are the other half of the job: this is
//! the change most able to refuse something legitimate, so a `..` that stays
//! inside the root, a symbolic link that stays inside the root, and a new file in
//! a new subdirectory each have a test saying they still work.

use io_harness::policy::{Act, Effect, Policy};
use io_harness::tools::workspace::MAX_DOCUMENT_BYTES;
use io_harness::tools::Workspace;

/// A workspace with a `src/` and a `docs/`, and a second directory standing in
/// for everything outside the root — `$HOME`, `/etc`, a `PATH` entry.
fn fixture() -> (tempfile::TempDir, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("src")).unwrap();
    std::fs::create_dir_all(root.path().join("docs")).unwrap();
    std::fs::write(root.path().join("src/a.rs"), "pub fn alpha() {}\n").unwrap();
    let outside = tempfile::tempdir().unwrap();
    (root, outside)
}

// ---------------------------------------------------------------------------
// H3 — a root-escaping path graded as though it named a file in the workspace
// ---------------------------------------------------------------------------

/// A path that climbs out of the root has no verdict to give, so it is denied.
///
/// It used to be *allowed*: `resolve` refused it, `check_path` fell through to
/// grading the lexical form, and that form popped `..` off an empty stack, so
/// a chain of them graded as the tail alone — an ordinary path in the workspace
/// as far as any rule could tell. A caller that asks the gate and then hands the
/// original string to a command, as `git_worktree` does, wrote a whole checkout
/// wherever the string pointed.
#[test]
fn h3_a_path_that_climbs_out_of_the_root_is_denied_rather_than_graded_as_an_inner_one() {
    let (root, _outside) = fixture();
    let ws = Workspace::new(root.path());

    for path in [
        "../elsewhere",
        "../../../../tmp/elsewhere",
        "src/../../elsewhere",
        "./../elsewhere",
    ] {
        for act in [Act::Read, Act::Write] {
            let verdict = ws.check_path(act, path);
            assert_eq!(
                verdict.effect,
                Effect::Deny,
                "{act:?} {path} must be denied, got {verdict:?}"
            );
            // No layer wrote this rule and no layer can lift it.
            assert_eq!(verdict.layer, None, "{path}");
            assert!(verdict.rule.is_some(), "{path} must say why");
        }
    }
}

/// The deny holds against a policy that allows everything, because containment
/// is not one of the things a layer gets to decide.
#[test]
fn h3_an_allow_everything_policy_does_not_lift_the_escape_deny() {
    let (root, _outside) = fixture();
    let allow_all = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*");
    let ws = Workspace::with_policy(root.path(), allow_all);

    assert_eq!(
        ws.check_path(Act::Write, "../../escaped").effect,
        Effect::Deny
    );
    assert!(ws.write_file("../../escaped", "x").is_err());
}

// ---------------------------------------------------------------------------
// H4 — a symlinked parent and a leaf that does not exist yet
// ---------------------------------------------------------------------------

/// A repository ships `docs/ext -> $HOME`; the agent writes `docs/ext/.bashrc`.
///
/// Lexically the path is inside the root, `canonicalize` fails because the leaf
/// is not there, and the escape test sat inside that `Ok`. So the check was
/// skipped and `fs::write` followed the link. `.bashrc`, `authorized_keys`,
/// `.config/autostart/*` and `.git/hooks/*` are all absent until something
/// creates them, which is why creation was the dangerous half.
#[cfg(unix)]
#[test]
fn h4_a_write_cannot_create_a_file_through_a_symlinked_parent_that_leaves_the_root() {
    let (root, outside) = fixture();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();
    let ws = Workspace::new(root.path());

    assert_eq!(
        ws.check_path(Act::Write, "docs/ext/.bashrc").effect,
        Effect::Deny,
        "a leaf that does not exist yet is still checked"
    );
    assert!(ws.write_file("docs/ext/.bashrc", "planted\n").is_err());
    assert!(ws.write_bytes("docs/ext/.bashrc", b"planted\n").is_err());
    assert!(
        !outside.path().join(".bashrc").exists(),
        "nothing may be created outside the canonical root"
    );
}

/// The same link with a leaf that *does* exist. This one was already refused in
/// 0.73.0 — it is here so the fix for the creation half cannot quietly cost the
/// half that already worked.
#[cfg(unix)]
#[test]
fn h4_an_existing_file_through_a_symlinked_parent_stays_denied() {
    let (root, outside) = fixture();
    std::fs::write(outside.path().join(".bashrc"), "the original\n").unwrap();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();
    let ws = Workspace::new(root.path());

    assert_eq!(
        ws.check_path(Act::Write, "docs/ext/.bashrc").effect,
        Effect::Deny
    );
    assert!(ws.write_file("docs/ext/.bashrc", "stolen\n").is_err());
    assert_eq!(
        std::fs::read_to_string(outside.path().join(".bashrc")).unwrap(),
        "the original\n"
    );
}

/// A directory that does not exist yet, under a link that leaves the root:
/// `create_dir_all` used to build the whole chain outside the workspace.
#[cfg(unix)]
#[test]
fn h4_a_write_cannot_create_a_directory_tree_outside_the_root() {
    let (root, outside) = fixture();
    std::os::unix::fs::symlink(outside.path(), root.path().join("docs/ext")).unwrap();
    let ws = Workspace::new(root.path());

    assert!(ws
        .write_file("docs/ext/.config/autostart/x.desktop", "Exec=sh\n")
        .is_err());
    assert!(!outside.path().join(".config").exists());
}

// ---------------------------------------------------------------------------
// M6 — the path that was checked is the path that is written
// ---------------------------------------------------------------------------

/// A symbolic link at the leaf pointing at a file outside the root that does not
/// exist yet.
///
/// Containment alone cannot see this one: the deepest *existing* ancestor is
/// `src/`, which is inside the root, so the path check says yes. It is the
/// `O_NOFOLLOW` open that refuses, which is the same mechanism that closes the
/// race where a live `shell_start` handle swaps a gated-allowed file for a link
/// between the check and the write. In 0.73.0 `fs::write` followed it and created
/// the file outside.
#[cfg(unix)]
#[test]
fn m6_a_symlink_at_the_leaf_cannot_redirect_a_write_out_of_the_root() {
    let (root, outside) = fixture();
    let target = outside.path().join("planted.txt");
    std::os::unix::fs::symlink(&target, root.path().join("src/b.rs")).unwrap();
    let ws = Workspace::new(root.path());

    assert!(ws.write_file("src/b.rs", "planted\n").is_err());
    assert!(ws.write_bytes("src/b.rs", b"planted\n").is_err());
    assert!(!target.exists(), "the write must not follow the link out");
}

/// The same for a link whose target exists: the write lands nowhere rather than
/// on the file outside.
#[cfg(unix)]
#[test]
fn m6_an_existing_file_behind_a_leaf_symlink_is_not_overwritten() {
    let (root, outside) = fixture();
    let target = outside.path().join("passwd");
    std::fs::write(&target, "root:x:0:0\n").unwrap();
    std::os::unix::fs::symlink(&target, root.path().join("src/b.rs")).unwrap();
    let ws = Workspace::new(root.path());

    assert!(ws.write_file("src/b.rs", "attacker:x:0:0\n").is_err());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "root:x:0:0\n");
}

// ---------------------------------------------------------------------------
// M15 — a ceiling on the read every document parser starts at
// ---------------------------------------------------------------------------

/// `read_bytes` used to be a bare `fs::read`: the whole file into memory, no
/// ceiling, and every parser — pdf, docx, xlsx, pptx, barcode — begins there.
#[test]
fn m15_a_document_read_over_the_size_cap_is_refused() {
    let (root, _outside) = fixture();
    // Sparse: this costs a length, not the bytes.
    std::fs::File::create(root.path().join("huge.pdf"))
        .unwrap()
        .set_len(MAX_DOCUMENT_BYTES + 1)
        .unwrap();
    let ws = Workspace::new(root.path());

    let err = ws.read_bytes("huge.pdf").unwrap_err();
    let said = err.to_string();
    assert!(
        said.contains("huge.pdf"),
        "the refusal names the path: {said}"
    );
    assert!(
        said.contains(&MAX_DOCUMENT_BYTES.to_string()),
        "the refusal names the limit: {said}"
    );
}

/// A file under the cap is unaffected, and a missing one still says so.
#[test]
fn m15_a_document_read_under_the_cap_is_unchanged() {
    let (root, _outside) = fixture();
    std::fs::write(root.path().join("small.bin"), [0u8, 1, 2, 3]).unwrap();
    let ws = Workspace::new(root.path());

    assert_eq!(ws.read_bytes("small.bin").unwrap(), vec![0u8, 1, 2, 3]);
    assert!(ws.read_bytes("absent.bin").is_err());
}

// ---------------------------------------------------------------------------
// The three things that must keep working
// ---------------------------------------------------------------------------

/// A `..` that stays inside the root is an ordinary path, and stayed one.
#[test]
fn a_parent_component_that_stays_inside_the_root_still_reads_and_writes() {
    let (root, _outside) = fixture();
    let ws = Workspace::new(root.path());

    assert_eq!(
        ws.check_path(Act::Write, "src/../docs/n.md").effect,
        Effect::Allow
    );
    assert!(ws.write_file("src/../docs/n.md", "note\n").is_ok());
    assert_eq!(ws.read_file("docs/n.md").unwrap(), "note\n");
    assert_eq!(
        ws.read_file("./src/../src/a.rs").unwrap(),
        "pub fn alpha() {}\n"
    );
}

/// A symbolic link that stays inside the root still resolves and is still
/// written through — the `O_NOFOLLOW` open retries against where it points, and
/// where it points is contained.
#[cfg(unix)]
#[test]
fn a_symlink_that_stays_inside_the_root_is_still_written_through() {
    let (root, _outside) = fixture();
    std::os::unix::fs::symlink(
        root.path().join("src/a.rs"),
        root.path().join("src/link.rs"),
    )
    .unwrap();
    let ws = Workspace::new(root.path());

    assert_eq!(
        ws.check_path(Act::Write, "src/link.rs").effect,
        Effect::Allow
    );
    assert!(ws.write_file("src/link.rs", "pub fn beta() {}\n").is_ok());
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/a.rs")).unwrap(),
        "pub fn beta() {}\n",
        "the link's target is what receives the bytes, as it always did"
    );
}

/// Creating a file in a directory that does not exist yet is the ordinary case
/// the new check has to keep allowing, since it is the case `canonicalize` could
/// never decide.
#[test]
fn a_new_file_in_a_new_subdirectory_is_still_created() {
    let (root, _outside) = fixture();
    let ws = Workspace::new(root.path());

    assert_eq!(
        ws.check_path(Act::Write, "src/deep/deeper/new.rs").effect,
        Effect::Allow
    );
    assert!(ws
        .write_file("src/deep/deeper/new.rs", "fn n() {}\n")
        .is_ok());
    assert_eq!(
        ws.read_file("src/deep/deeper/new.rs").unwrap(),
        "fn n() {}\n"
    );
}

/// A workspace whose root does not exist yet still behaves as it did: the same
/// resolution runs on the root, so nothing about "the directory is about to be
/// created" turns into a refusal.
#[test]
fn a_root_that_does_not_exist_yet_still_accepts_an_inner_write() {
    let (parent, _outside) = fixture();
    let root = parent.path().join("not-created-yet");
    let ws = Workspace::new(&root);

    assert_eq!(ws.check_path(Act::Write, "src/a.rs").effect, Effect::Allow);
    assert!(ws.write_file("src/a.rs", "fn n() {}\n").is_ok());
    assert_eq!(ws.check_path(Act::Write, "../escaped").effect, Effect::Deny);
}
