//! Skills (0.9.0): discovery from a directory, and the run-start failure for a
//! directory the harness cannot use.
//!
//! F7's other half — the catalogue reaching the system prompt with names and
//! descriptions but no bodies — needs the prompt wiring and the `read_skill`
//! tool, and is asserted where that lands. What is asserted here is everything
//! that holds without it: what a directory yields, and what an unusable one does
//! to a run.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse};
use io_harness::skills::{Skills, MAX_SKILLS};
use io_harness::{run_with, ApproveAll, Policy, Provider, Store, TaskContract, Verification};

// ---------------------------------------------------------------- mock provider

/// Answers every turn with nothing to do, and counts how many times it was
/// asked. The count is what proves discovery ran *before* the provider was
/// reached.
#[derive(Default)]
struct MockScript {
    at: AtomicUsize,
}

impl MockScript {
    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse::default())
    }
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

fn write(path: &std::path::Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// A contract that can never be satisfied, so a run that gets past discovery
/// reaches the provider.
fn never_passes(root: &std::path::Path, skills: &std::path::Path) -> TaskContract {
    TaskContract::workspace(
        "exercise the skills directory",
        root,
        Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        },
    )
    .with_max_steps(1)
    .with_skills(skills)
}

async fn run_err(contract: &TaskContract, provider: &MockScript) -> io_harness::Error {
    run_with(
        contract,
        provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect_err("an unusable skills directory must fail the run")
}

// ---------------------------------------------------------------- F7: discovery

/// F7 — both layouts in common use are found, and nothing else in the directory
/// is mistaken for a skill.
#[test]
fn a_flat_file_and_a_skill_md_directory_are_both_discovered() {
    let dir = tmp();
    write(&dir.path().join("alpha.md"), "# Alpha\n\nthe body\n");
    write(
        &dir.path().join("beta/SKILL.md"),
        "beta's own line\n\nbody\n",
    );
    // Not skills: neither should reach the catalogue.
    write(&dir.path().join("notes.txt"), "not a skill\n");
    write(
        &dir.path().join("empty-dir/README.md"),
        "no SKILL.md here\n",
    );

    let skills = Skills::discover(dir.path()).unwrap();

    assert_eq!(
        skills.len(),
        2,
        "exactly two skills, got {:?}",
        skills.names()
    );
    assert_eq!(skills.names(), vec!["alpha", "beta"]);
    // No frontmatter: the name is the file stem (the directory, for a SKILL.md)
    // and the description is the first non-empty prose line.
    assert_eq!(skills.get("alpha").unwrap().description, "Alpha");
    assert_eq!(skills.get("beta").unwrap().description, "beta's own line");
    assert!(skills.get("alpha").unwrap().path.ends_with("alpha.md"));
    assert!(skills.get("beta").unwrap().path.ends_with("beta/SKILL.md"));
}

/// F7 — frontmatter wins over what the filesystem implies, for both layouts.
#[test]
fn frontmatter_overrides_the_derived_name_and_description() {
    let dir = tmp();
    write(
        &dir.path().join("gamma.md"),
        "---\nname: renamed\ndescription: what the frontmatter says\n---\n\n# Gamma\n\nbody\n",
    );
    write(
        &dir.path().join("delta/SKILL.md"),
        "---\nname: also-renamed\ndescription: likewise\n---\n\nbody\n",
    );

    let skills = Skills::discover(dir.path()).unwrap();

    assert_eq!(skills.names(), vec!["also-renamed", "renamed"]);
    assert!(
        skills.get("gamma").is_none() && skills.get("delta").is_none(),
        "the derived names must not survive the override"
    );
    assert_eq!(
        skills.get("renamed").unwrap().description,
        "what the frontmatter says"
    );
    assert_eq!(skills.get("also-renamed").unwrap().description, "likewise");
}

/// The catalogue is one line per skill — name and description, no bodies. What
/// it is *joined into* is the system prompt's business; that it never carries a
/// body is this file's.
#[test]
fn the_catalog_carries_names_and_descriptions_but_no_bodies() {
    let dir = tmp();
    write(
        &dir.path().join("alpha.md"),
        "---\nname: alpha\ndescription: one line\n---\n\nSECRET BODY LINE\n",
    );

    let catalog = Skills::discover(dir.path()).unwrap().catalog();

    assert_eq!(catalog, "- alpha: one line");
    assert!(!catalog.contains("SECRET BODY LINE"));
}

// ---------------------------------------------------------------- F9: run-start failure

/// F9 — a skills directory that is not there fails the run with a Config error
/// naming the path, before the provider is called even once.
#[tokio::test]
async fn a_missing_skills_directory_fails_at_run_start() {
    let root = tmp();
    let missing = root.path().join("no-such-skills");
    let contract = never_passes(root.path(), &missing);
    let provider = MockScript::default();

    let err = run_err(&contract, &provider).await;

    let named = missing.display().to_string();
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&named)),
        "expected a Config error naming {named}, got {err:?}"
    );
    assert_eq!(
        provider.calls(),
        0,
        "discovery must run before the provider is called"
    );
}

/// F9 — pointing `with_skills` at one file instead of a directory is the same
/// mistake, caught at the same point.
#[tokio::test]
async fn a_skills_path_that_is_not_a_directory_fails_at_run_start() {
    let root = tmp();
    let file = root.path().join("skills.md");
    write(&file, "# not a directory\n");
    let contract = never_passes(root.path(), &file);
    let provider = MockScript::default();

    let err = run_err(&contract, &provider).await;

    let named = file.display().to_string();
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&named)),
        "expected a Config error naming {named}, got {err:?}"
    );
    assert_eq!(provider.calls(), 0);
}

/// F9 — over the cap the whole set is rejected rather than silently reduced:
/// every name and description is sent on every turn, so the cost is real.
#[tokio::test]
async fn a_directory_over_the_skill_cap_fails_at_run_start() {
    let root = tmp();
    let skills = root.path().join("skills");
    for i in 0..=MAX_SKILLS {
        write(&skills.join(format!("skill-{i:03}.md")), "a line\n");
    }
    let contract = never_passes(root.path(), &skills);
    let provider = MockScript::default();

    let err = run_err(&contract, &provider).await;

    let named = skills.display().to_string();
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&named)),
        "expected a Config error naming {named}, got {err:?}"
    );
    assert_eq!(provider.calls(), 0);
}

/// Discovery is a gate, not a wall: a directory the harness can read lets the
/// run reach the loop.
#[tokio::test]
async fn a_usable_skills_directory_lets_the_run_start() {
    let root = tmp();
    let skills = root.path().join("skills");
    write(&skills.join("alpha.md"), "# Alpha\n\nbody\n");
    let contract = never_passes(root.path(), &skills);
    let provider = MockScript::default();

    run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("a readable skills directory must not fail the run");

    assert_eq!(provider.calls(), 1, "the loop must have been reached");
}
