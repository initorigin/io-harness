//! Skills (0.9.0): discovery from a directory, the catalogue reaching the system
//! prompt, `read_skill` loading one body on demand, and the policy governing
//! that read.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::skills::{Skills, MAX_SKILLS};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- mock provider

/// Replays a fixed script of tool calls, one turn at a time, keeping every
/// request it was sent. The count is what proves discovery ran *before* the
/// provider was reached; the kept requests are what the prompt assertions read.
#[derive(Default)]
struct MockScript {
    at: AtomicUsize,
    steps: Vec<Vec<ToolCall>>,
    seen: Mutex<Vec<CompletionRequest>>,
}

impl MockScript {
    fn scripted(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            ..Default::default()
        }
    }

    fn calls(&self) -> usize {
        self.at.load(Ordering::SeqCst)
    }

    /// The `i`th request the loop sent, copied out so no lock is held.
    fn request(&self, i: usize) -> CompletionRequest {
        self.seen.lock().unwrap()[i].clone()
    }
}

impl Provider for MockScript {
    async fn complete(&self, req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        self.seen.lock().unwrap().push(req);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
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

/// Two skills, one per layout, each with a body no prompt may ever carry.
fn two_skills(dir: &std::path::Path) {
    write(
        &dir.join("alpha.md"),
        "---\nname: alpha\ndescription: how to alpha\n---\n\nALPHA BODY LINE\n",
    );
    write(
        &dir.join("beta/SKILL.md"),
        "---\nname: beta\ndescription: how to beta\n---\n\nBETA BODY LINE\n",
    );
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

// ---------------------------------------------------------------- F7: the prompt half

/// F7 — every skill's name and description reaches the system prompt, no body
/// does, and `read_skill` is offered alongside.
#[tokio::test]
async fn the_catalog_reaches_the_system_prompt_and_no_body_does() {
    let root = tmp();
    let dir = root.path().join("skills");
    two_skills(&dir);
    let contract = never_passes(root.path(), &dir);
    let provider = MockScript::default();

    run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let req = provider.request(0);
    for expected in ["alpha", "how to alpha", "beta", "how to beta"] {
        assert!(
            req.system.contains(expected),
            "the system prompt must carry {expected:?}, got: {}",
            req.system
        );
    }
    for body in ["ALPHA BODY LINE", "BETA BODY LINE"] {
        assert!(
            !req.system.contains(body),
            "a skill body must never enter the prompt, got: {}",
            req.system
        );
    }
    assert!(
        req.tools.iter().any(|t| t.name == "read_skill"),
        "read_skill must be offered when skills are configured, got {:?}",
        req.tools.iter().map(|t| &t.name).collect::<Vec<_>>()
    );
}

/// A run with no skills is offered no `read_skill` — a tool that could only ever
/// fail would cost a slot in every request of every other run.
#[tokio::test]
async fn read_skill_is_not_offered_when_no_skills_are_configured() {
    let root = tmp();
    let contract = TaskContract::workspace(
        "no skills here",
        root.path(),
        Verification::WorkspaceFileContains {
            file: "unreachable.txt".into(),
            needle: "never".into(),
        },
    )
    .with_max_steps(1);
    let provider = MockScript::default();

    run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let req = provider.request(0);
    assert!(
        !req.tools.iter().any(|t| t.name == "read_skill"),
        "read_skill must not be offered without skills"
    );
    assert!(!req.system.contains("read_skill"));
}

// ---------------------------------------------------------------- F8: loading a body

/// F8 — the named body, and only it, lands in the observations. O1 (skill half)
/// — the trace records which skill was read.
#[tokio::test]
async fn read_skill_loads_exactly_the_named_body_into_the_observations() {
    let root = tmp();
    let dir = root.path().join("skills");
    two_skills(&dir);
    let contract = never_passes(root.path(), &dir).with_max_steps(2);
    let provider = MockScript::scripted(vec![vec![call("read_skill", json!({ "name": "beta" }))]]);
    let store = Store::memory().unwrap();

    let result = run_with(
        &contract,
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let second = provider.request(1);
    assert!(
        second.user.contains("BETA BODY LINE"),
        "the named skill's body must reach the next turn, got: {}",
        second.user
    );
    assert!(
        !second.user.contains("ALPHA BODY LINE"),
        "only the named skill's body may be loaded, got: {}",
        second.user
    );

    // O1 — name, call, and decision are all in the trace.
    let steps = store.steps(result.run_id).unwrap();
    assert!(
        steps[0].tool_call.contains("read_skill") && steps[0].tool_call.contains("beta"),
        "the trace must record the call, got {:?}",
        steps[0].tool_call
    );
    assert!(
        steps[0].decision.contains("beta"),
        "the trace must record which skill was read, got {:?}",
        steps[0].decision
    );
}

/// F8 — a name that is not a skill is an observation listing the ones that are,
/// not an error and not a failed run.
#[tokio::test]
async fn an_unknown_skill_name_is_an_observation_listing_what_exists() {
    let root = tmp();
    let dir = root.path().join("skills");
    two_skills(&dir);
    let contract = never_passes(root.path(), &dir).with_max_steps(2);
    let provider = MockScript::scripted(vec![vec![call("read_skill", json!({ "name": "gamma" }))]]);

    let result = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("an unknown skill must not fail the run");

    let second = provider.request(1);
    assert!(
        second.user.contains("alpha") && second.user.contains("beta"),
        "the observation must list the skills that do exist, got: {}",
        second.user
    );
    assert!(
        !second.user.contains("BETA BODY LINE"),
        "no body may be loaded for a name that does not exist"
    );
    assert!(
        matches!(result.outcome, RunOutcome::StepCapReached { .. }),
        "an unknown skill is not a failed run, got {:?}",
        result.outcome
    );
}

// ---------------------------------------------------------------- F10: policy-governed

/// F10 — reading a skill is a read like any other. A policy denying the skills
/// directory refuses it, attributably, and the body never reaches the model —
/// while the catalogue the prompt advertises is untouched.
#[tokio::test]
async fn a_denied_skills_directory_refuses_the_read_but_keeps_the_catalog() {
    let root = tmp();
    let dir = root.path().join("skills");
    two_skills(&dir);
    // Canonical: `Skill::path` is canonicalized, and on macOS a tempdir's
    // canonical form is not the one it was created under.
    let canon = std::fs::canonicalize(&dir).unwrap();
    let policy = Policy::default()
        .layer("base")
        .allow_read("*")
        .allow_write("*")
        .allow_exec("*")
        .deny_read(format!("{}/*", canon.display()));

    let contract = never_passes(root.path(), &dir).with_max_steps(2);
    let provider = MockScript::scripted(vec![vec![call("read_skill", json!({ "name": "beta" }))]]);
    let store = Store::memory().unwrap();

    let result = run_with(&contract, &provider, &store, &policy, &ApproveAll)
        .await
        .unwrap();

    let refusal = store
        .events(result.run_id)
        .unwrap()
        .into_iter()
        .find(|e| e.kind == "refusal" && e.act == "read")
        .expect("the refusal must be in the trace");
    assert!(
        refusal.target.contains("beta"),
        "the refusal must name the skill's path, got {:?}",
        refusal.target
    );
    assert_eq!(refusal.layer.as_deref(), Some("base"));

    let second = provider.request(1);
    assert!(
        !second.user.contains("BETA BODY LINE"),
        "a refused skill body must never enter the observations, got: {}",
        second.user
    );
    // Offering a skill is not granting it: the catalogue still advertises it.
    let first = provider.request(0);
    assert!(
        first.system.contains("beta") && first.system.contains("how to beta"),
        "the catalogue must be unaffected by the policy, got: {}",
        first.system
    );
}

/// The gate lets an ABSOLUTE target be decided by the policy directly, because a
/// skills directory normally sits outside the workspace root. This is the test
/// that says relaxing it opened nothing: `read_file` and `write_file` resolve
/// every path under the root, and an absolute one is refused there even when the
/// policy allows everything.
#[tokio::test]
async fn an_absolute_path_still_cannot_escape_the_workspace_root() {
    let root = tmp();
    let outside = tmp();
    let secret = outside.path().join("secret.txt");
    write(&secret, "SECRET OUTSIDE THE ROOT\n");
    let planted = outside.path().join("planted.txt");

    let dir = root.path().join("skills");
    two_skills(&dir);
    let contract = never_passes(root.path(), &dir).with_max_steps(3);
    let provider = MockScript::scripted(vec![
        vec![call(
            "read_file",
            json!({ "path": secret.display().to_string() }),
        )],
        vec![call(
            "write_file",
            json!({ "path": planted.display().to_string(), "content": "pwned" }),
        )],
    ]);

    run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        // Permissive: nothing but the workspace resolution stands in the way.
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let last = provider.request(2);
    assert!(
        !last.user.contains("SECRET OUTSIDE THE ROOT"),
        "an absolute read must not pull a file from outside the root, got: {}",
        last.user
    );
    assert!(
        !planted.exists(),
        "an absolute write must not land outside the root"
    );
}
