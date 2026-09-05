//! The walk under an accepted skills directory stays inside it.
//!
//! L13 confined the paths a workspace *declares* — `run.skills`,
//! `run.templates`, a plugin's `path` — and stopped at the declaration. Nothing
//! confined the walk beneath a root that had been accepted, so an entry inside
//! it that was a symbolic link to somewhere else was followed and the
//! `SKILL.md` found there became a name and a description in the system prompt.
//! That is the whole distance a planted skill has to travel: the catalogue is
//! prompt text, sent on every turn, before the model has done anything a policy
//! could refuse.
//!
//! Every refusal here is paired with the capability it must not take away. A
//! test that only asserts an absence passes just as well against a build where
//! discovery was deleted, so each one also names a skill that is still found.
#![cfg(unix)]

use std::os::unix::fs::symlink;
use std::path::Path;

use io_harness::Skills;

fn write(at: &Path, text: &str) {
    if let Some(parent) = at.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(at, text).unwrap();
}

/// A skill file with frontmatter, so the name under test is the one in the file
/// rather than whatever the directory happens to be called.
fn skill(at: &Path, name: &str) {
    write(
        at,
        &format!("---\nname: {name}\ndescription: {name} description\n---\n\n{name} body\n"),
    );
}

/// The finding itself: a subdirectory that is a link out of the root is not
/// descended, and an ordinary subdirectory beside it still is.
#[test]
fn a_symlinked_subdirectory_leaving_the_skills_root_yields_no_skill() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    // The real skill, in a real subdirectory: the control, and the reason this
    // test cannot pass against a build with no discovery in it.
    skill(&root.path().join("api-style/SKILL.md"), "api-style");
    // The planted one, reachable only by following the link.
    skill(&outside.path().join("SKILL.md"), "planted");
    symlink(outside.path(), root.path().join("borrowed")).unwrap();

    let skills = Skills::discover(root.path()).expect("a stray link is skipped, not fatal");

    assert_eq!(
        skills.names(),
        vec!["api-style"],
        "the real subdirectory is discovered and the linked one is not"
    );
    assert!(
        !skills.catalog().contains("planted"),
        "nothing from outside the root reaches the catalogue: {}",
        skills.catalog()
    );
}

/// The same hole with one character changed. A top-level `*.md` entry gets the
/// same test, and it is the worse half: `discover` canonicalises what it finds,
/// so a skill reached through a link would be rooted *outside* the accepted
/// directory and every companion file it named would resolve under that root.
#[test]
fn a_symlinked_skill_file_leaving_the_skills_root_yields_no_skill() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();

    skill(&root.path().join("migrations.md"), "migrations");
    skill(&outside.path().join("planted.md"), "planted");
    symlink(
        outside.path().join("planted.md"),
        root.path().join("planted.md"),
    )
    .unwrap();

    let skills = Skills::discover(root.path()).expect("a stray link is skipped, not fatal");

    assert_eq!(
        skills.names(),
        vec!["migrations"],
        "the file that is really there is discovered and the link is not"
    );
    assert!(
        skills.get("planted").is_none(),
        "a skill outside the root is not addressable either"
    );
}

/// A link that stays *inside* the root is a layout an operator is entitled to
/// use, and `resolve_under` has always allowed one for companion files. This is
/// the capability the refusal must not take with it.
#[test]
fn a_symlink_that_stays_inside_the_skills_root_is_still_discovered() {
    let root = tempfile::tempdir().unwrap();

    // `store/` holds no `SKILL.md`, so it contributes nothing of its own and the
    // link at the top level is the only way its file becomes a skill.
    skill(&root.path().join("store/real.md"), "aliased");
    symlink(
        root.path().join("store/real.md"),
        root.path().join("alias.md"),
    )
    .unwrap();

    let skills = Skills::discover(root.path()).expect("discover");

    assert_eq!(
        skills.names(),
        vec!["aliased"],
        "a link whose target canonicalises inside the root still resolves"
    );
}

/// A dangling link is skipped rather than failing the directory — an operator
/// removing a skill's target should not lose the rest of the catalogue with it.
#[test]
fn a_dangling_link_is_skipped_and_the_rest_of_the_directory_survives() {
    let root = tempfile::tempdir().unwrap();

    skill(&root.path().join("migrations.md"), "migrations");
    symlink(root.path().join("gone.md"), root.path().join("broken.md")).unwrap();

    let skills = Skills::discover(root.path()).expect("a dangling link is not fatal");

    assert_eq!(skills.names(), vec!["migrations"]);
}
