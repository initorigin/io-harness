//! Prompt templates (0.21.0): F10 — a template renders and nothing else.
//!
//! The failure this file records is a prompt that was quietly not the prompt the
//! operator wrote. Three ways that happens, all of them silent: a
//! `{{placeholder}}` nobody passed an argument for becomes an empty string and
//! the run pursues a goal with a hole in it; a typo in a template name resolves
//! to some other template; a rendered string is "helpfully" reprocessed on its
//! way into the contract. Each is an assertion below, and the last test drives a
//! real run so the rendered string is checked where it actually lands — inside
//! the prompt the provider is sent.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::template::{Template, Templates};
use io_harness::{
    run_with, ApproveAll, Policy, Provider, RunOutcome, Store, TaskContract, Verification,
};
use serde_json::json;

// ---------------------------------------------------------------- mock provider

/// Replays a fixed script of tool calls, one turn at a time, keeping every
/// request it was sent. The same shape `tests/skills.rs` uses: the kept requests
/// are what the prompt assertion reads.
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

fn write(path: &Path, body: &str) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, body).unwrap();
}

/// The checked-in fixture directory: two layouts, one non-markdown file, and a
/// template per rendering rule under test.
fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/templates-0.21.0")
}

fn discovered() -> Templates {
    Templates::discover(fixtures()).expect("the checked-in fixture directory must discover")
}

// ---------------------------------------------------------------- F10: discovery

/// F10 — discovery over a fixture directory finds the templates with their
/// descriptions. Both layouts, and nothing else in the directory mistaken for one.
#[test]
fn discovery_finds_both_layouts_with_their_descriptions() {
    let templates = discovered();

    assert_eq!(
        templates.names(),
        vec!["bugfix", "review", "ship"],
        "sorted by name, and notes.txt is not a template"
    );
    assert_eq!(templates.len(), 3);
    assert!(!templates.is_empty());
    assert_eq!(
        templates.get("bugfix").unwrap().description,
        "Fix one failing test and change nothing else.",
        "the frontmatter description"
    );
    assert_eq!(
        templates.get("review").unwrap().description,
        "Review the diff and say what would break",
        "no frontmatter: the first prose line, heading markers stripped"
    );
    assert!(
        templates.get("review").unwrap().body.contains("$ARGUMENTS"),
        "a Template holds its body — rendering needs the text"
    );
    assert!(templates.get("notes").is_none());
}

/// The catalogue is one line per template, name and description, no bodies — the
/// same shape `Skills::catalog` produces.
#[test]
fn the_catalog_carries_names_and_descriptions_but_no_bodies() {
    let catalog = discovered().catalog();

    assert!(
        catalog.contains("- ship: Write one file and stop."),
        "got: {catalog}"
    );
    assert!(
        !catalog.contains("{{needle}}"),
        "a template body must not be in the catalogue, got: {catalog}"
    );
    assert_eq!(Templates::none().catalog(), "");
    assert!(Templates::none().is_empty());
}

// ---------------------------------------------------------------- F10: rendering

/// F10 — `render` substitutes every `{{placeholder}}` and routes the remainder to
/// `$ARGUMENTS`, in the order the arguments were given.
#[test]
fn render_substitutes_placeholders_and_routes_the_remainder_to_arguments() {
    let rendered = discovered()
        .render(
            "bugfix",
            &[
                ("test", "parses_a_crlf_header"),
                ("note", "it only fails on CI"),
                ("file", "src/parse.rs"),
                ("also", "see the flake in #412"),
            ],
        )
        .unwrap();

    assert_eq!(
        rendered,
        "Fix the failing test parses_a_crlf_header in src/parse.rs. Change no other test.\n\n\
         Extra context: it only fails on CI see the flake in #412",
        "every placeholder substituted; the two unmatched arguments joined by one \
         space, in the order given"
    );
    assert!(
        !rendered.contains("{{") && !rendered.contains("$ARGUMENTS"),
        "no marker may survive rendering, got: {rendered}"
    );
}

/// `$ARGUMENTS` with nothing left over is empty, because a *remainder* is
/// legitimately empty. A named placeholder is not — that is the test below.
#[test]
fn arguments_with_no_remainder_expands_to_nothing() {
    let rendered = discovered()
        .render("bugfix", &[("test", "t"), ("file", "f")])
        .unwrap();

    assert!(rendered.ends_with("Extra context: "), "got: {rendered:?}");
}

/// F10 — an unknown template name is an error, and the error lists what does
/// exist so the caller does not have to go and read the directory.
#[test]
fn an_unknown_template_name_is_an_error() {
    let err = discovered()
        .render("bugfx", &[("test", "t"), ("file", "f")])
        .expect_err("a name that is not a template must not render");

    let message = err.to_string();
    assert!(
        matches!(err, io_harness::Error::Config(_)),
        "expected a Config error, got {err:?}"
    );
    assert!(
        message.contains("bugfx") && message.contains("bugfix"),
        "the error must name what was asked for and what exists, got: {message}"
    );
}

/// F10 — a placeholder with no argument is an error rather than an empty string:
/// the rule 0.19.0 set for `${env:}` in `config.rs`, applied here. An empty
/// string in a prompt is a silently different prompt.
#[test]
fn a_placeholder_with_no_argument_is_an_error_not_an_empty_string() {
    let err = discovered()
        .render("bugfix", &[("test", "parses_a_crlf_header")])
        .expect_err("a placeholder with no argument must not render");

    let message = err.to_string();
    assert!(
        matches!(err, io_harness::Error::Config(_)),
        "expected a Config error, got {err:?}"
    );
    assert!(
        message.contains("file"),
        "the error must name the placeholder that went unfilled, got: {message}"
    );
    assert!(
        message.contains("bugfix"),
        "and the template it is in, got: {message}"
    );
}

/// Substitution is single-pass: a value that itself contains `{{x}}` or
/// `$ARGUMENTS` is emitted literally, never re-read. Recursion is the whole
/// difference between a substitution and a template language.
#[test]
fn a_substituted_value_is_never_re_substituted() {
    let rendered = discovered()
        .render(
            "bugfix",
            &[
                ("test", "{{file}}"),
                ("file", "$ARGUMENTS"),
                ("rest", "tail"),
            ],
        )
        .unwrap();

    assert!(
        rendered.starts_with("Fix the failing test {{file}} in $ARGUMENTS."),
        "an inserted value must be emitted literally, got: {rendered}"
    );
    assert!(
        rendered.ends_with("Extra context: tail"),
        "and the real $ARGUMENTS still gets only the unmatched arguments, got: {rendered}"
    );
}

// ---------------------------------------------------------------- discovery failures

/// Same error behaviour as `Skills::discover`: a directory that is not there is a
/// `Config` error naming the path.
#[test]
fn a_missing_or_non_directory_path_is_a_config_error() {
    let root = tmp();
    let missing = root.path().join("no-such-templates");
    let err = Templates::discover(&missing).expect_err("a missing directory must fail");
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&missing.display().to_string())),
        "expected a Config error naming the path, got {err:?}"
    );

    let file = root.path().join("templates.md");
    write(&file, "# not a directory\n");
    let err = Templates::discover(&file).expect_err("a file must fail");
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&file.display().to_string())),
        "expected a Config error naming the path, got {err:?}"
    );
}

/// Two templates of the same name is an ambiguous set, and resolving it by
/// picking one silently is how an operator ends up debugging why their run got
/// the wrong prompt.
#[test]
fn two_templates_of_the_same_name_are_rejected() {
    let dir = tmp();
    write(&dir.path().join("a.md"), "---\nname: same\n---\nA\n");
    write(
        &dir.path().join("b/TEMPLATE.md"),
        "---\nname: same\n---\nB\n",
    );

    let err = Templates::discover(dir.path()).expect_err("a duplicate name must fail");
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains("same")),
        "expected a Config error naming the duplicate, got {err:?}"
    );
}

/// Over the cap the whole set is rejected rather than silently reduced.
#[test]
fn a_directory_over_the_template_cap_is_rejected() {
    let dir = tmp();
    for i in 0..=io_harness::template::MAX_TEMPLATES {
        write(&dir.path().join(format!("t-{i:03}.md")), "a line\n");
    }

    let err = Templates::discover(dir.path()).expect_err("over the cap must fail");
    assert!(
        matches!(err, io_harness::Error::Config(ref m) if m.contains(&dir.path().display().to_string())),
        "expected a Config error naming the directory, got {err:?}"
    );
}

// ---------------------------------------------------------------- F10: the rendered string drives a run

/// F10 — "the rendered string drives a real run through `TaskContract::workspace`
/// unchanged".
///
/// The rendered text is the contract's goal and nothing touches it on the way in:
/// it arrives in the prompt the provider is sent, byte for byte. Rendering is a
/// pure function of template plus arguments — it reads no file, consults no
/// policy, and draws on no budget — so the only thing this test can be failed by
/// is the string changing between `render` and the wire.
#[tokio::test]
async fn a_rendered_prompt_drives_a_real_run_unchanged() {
    let root = tmp();
    let rendered = discovered()
        .render(
            "ship",
            &[
                ("file", "out.txt"),
                ("needle", "SHIPPED"),
                ("aside", "and stop there"),
            ],
        )
        .unwrap();
    assert_eq!(
        rendered, "Write out.txt so that it contains SHIPPED. and stop there",
        "the string under test"
    );

    let contract = TaskContract::workspace(
        rendered.clone(),
        root.path(),
        Verification::WorkspaceFileContains {
            file: "out.txt".into(),
            needle: "SHIPPED".into(),
        },
    )
    .with_max_steps(2);
    let provider = MockScript::scripted(vec![vec![call(
        "write_file",
        json!({ "path": "out.txt", "content": "SHIPPED\n" }),
    )]]);

    let result = run_with(
        &contract,
        &provider,
        &Store::memory().unwrap(),
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .expect("a rendered prompt is an ordinary goal");

    assert!(
        provider.request(0).user.contains(&rendered),
        "the rendered string must reach the prompt unchanged, got: {}",
        provider.request(0).user
    );
    assert!(
        matches!(result.outcome, RunOutcome::Success { .. }),
        "the run driven by the rendered prompt must verify, got {:?}",
        result.outcome
    );
}

/// A `Template` is data: name, description, body, and no behaviour of its own.
/// Rendering lives on the registry, so a template that was never discovered
/// cannot be rendered by accident.
#[test]
fn a_template_is_plain_data() {
    let templates = discovered();
    let ship: &Template = templates.get("ship").unwrap();
    assert_eq!(ship.name, "ship");
    assert!(ship.body.contains("{{needle}}"));
}
