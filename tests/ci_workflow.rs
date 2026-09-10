//! The matrix builds every example a test spawns, and no others — F3 of 0.36.1.
//!
//! Until this release the CI matrix ran `cargo build --all-targets`, which links
//! all 35 examples on three operating systems for each of two feature
//! polarities. Thirty of them are demonstrations no test executes; linking them
//! proved only that they compile, which `cargo check --examples` proves once,
//! without producing a linked executable. The matrix now builds the library, the
//! test binaries, and the fixture examples a test spawns as a child process.
//!
//! That is safe only if the two sets cannot drift. `--lib --tests` does not build
//! `examples/`, and this repository has discovered that the hard way four
//! separate times — `crash_fixture` (0.12.0), `plan_gate_fixture` (0.31.0),
//! `fleet_fixture` (0.32.0), `attach_fixture` (0.33.0) — each time as a
//! confusing CI failure about a missing file. This test converts the fifth
//! occurrence into a named test failure that says what to add and where.
//!
//! Both sides are derived, neither is typed:
//!
//! * The spawned set is read out of `tests/`. A test locates an example by
//!   joining `examples` onto the directory holding the test binary, or by
//!   calling a local helper that does; the name is a string literal in the same
//!   few lines. Comments are stripped first, so a doc comment naming a fixture
//!   does not put it in the set.
//! * The built set is read out of `.github/workflows/ci.yml`, from the
//!   `--example <name>` arguments the matrix passes to `cargo build`.
//!
//! The name-must-be-a-real-example constraint is what keeps the scan honest in
//! the other direction: `edit_file` and `web_search` are tool names that appear
//! as string literals all over `tests/`, and both are also examples, so a scan
//! that took every literal would report them as spawned. Only literals near a
//! spawn marker count.
//!
//! The negative control is a fixture that spawns an example the workflow does
//! not build, which the checker must report. A checker that cannot be made to
//! fail is a green light wired to nothing — the phrase `tests/readme.rs` already
//! uses about its own controls.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use regex::Regex;

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// How many lines either side of a spawn marker a name may sit on.
///
/// Two, and the tightness is load-bearing. Every real case puts the literal on
/// the marker line or one line from it: `tests/checkpoint.rs` and its three
/// siblings write `.join("examples").join("crash_fixture")` on one line,
/// `tests/mcp.rs` builds the name into a `let` immediately above the `join`, and
/// `tests/context.rs` and `tests/observe.rs` put the `format!` inside the `join`
/// on the next line. Written first at eight, this scan pulled in `"edit_file"`
/// — a tool name that appears as a literal all over `tests/` and is also an
/// example — from five lines away. The window is what separates the two, so it
/// is sized to the widest real spawn and not a line further.
const WINDOW: usize = 2;

/// This file. Its negative controls are Rust source containing spawn markers and
/// example names, so a scan of `tests/` that included it would read its fixtures
/// as real spawns — which is exactly how `"edit_file"` first entered the set.
const SELF: &str = "ci_workflow.rs";

// ---------------------------------------------------------------------------
// The examples that exist
// ---------------------------------------------------------------------------

/// Every example target name, from `examples/*.rs`.
fn examples_on_disk(dir: &Path) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "rs") {
            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                out.insert(stem.to_string());
            }
        }
    }
    assert!(!out.is_empty(), "examples/ holds no .rs files");
    out
}

// ---------------------------------------------------------------------------
// The examples the tests spawn
// ---------------------------------------------------------------------------

/// Rust source with its comment lines removed.
///
/// `tests/attach.rs` names three sibling fixtures in a doc comment explaining
/// which examples `--lib --tests` cannot build. That prose must not decide what
/// CI builds, so it never reaches the scan.
fn without_comments(source: &str) -> Vec<&str> {
    source
        .lines()
        .map(|line| {
            let t = line.trim_start();
            if t.starts_with("//") {
                ""
            } else {
                line
            }
        })
        .collect()
}

/// True when this line marks the place a test resolves an example binary.
///
/// **Every spelling a test can reach `examples/` by has to be here, and 0.86.0
/// found that one was not.** A test that walked to the directory with
/// `dir.push("examples")` rather than `dir.join("examples")` matched no marker,
/// so the whole file was invisible to this checker: it spawned an example the CI
/// matrix did not build, and the matrix-versus-suite assertion passed while
/// saying nothing about it. The failure mode is silent by construction — a
/// spelling this list does not know about exempts a test rather than failing it
/// — which is why the list is stated as three cheap `contains` and not narrowed
/// to the one spelling that happens to be commonest.
fn is_spawn_marker(line: &str) -> bool {
    line.contains(r#"join("examples")"#)
        || line.contains(r#"push("examples")"#)
        || line.contains("example_binary(")
}

/// The example names `source` resolves as child processes.
///
/// A name counts when it is a string literal within [`WINDOW`] lines of a spawn
/// marker AND is the name of an example that exists. The literal may be the
/// whole name (`join("crash_fixture")`, `example_binary("tick")`) or the name
/// with the platform suffix appended (`format!("mcp_fixture_server{}", ..)`),
/// which is why the pattern anchors on the opening quote and the name and stops
/// there rather than requiring the closing quote.
fn spawned_examples(source: &str, known: &BTreeSet<String>) -> BTreeSet<String> {
    let literal = Regex::new(r#""([A-Za-z0-9_]+)"#).unwrap();
    let lines = without_comments(source);

    let mut out = BTreeSet::new();
    for (i, line) in lines.iter().enumerate() {
        if !is_spawn_marker(line) {
            continue;
        }
        let lo = i.saturating_sub(WINDOW);
        let hi = (i + WINDOW + 1).min(lines.len());
        for near in &lines[lo..hi] {
            for c in literal.captures_iter(near) {
                let name = &c[1];
                if known.contains(name) {
                    out.insert(name.to_string());
                }
            }
        }
    }
    out
}

/// Every example spawned anywhere under `tests/`.
fn spawned_across(dir: &Path, known: &BTreeSet<String>) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let entries =
        fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.file_name().is_some_and(|n| n == SELF) {
            continue;
        }
        if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).expect("readable test source");
            out.extend(spawned_examples(&text, known));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// The examples the workflow builds
// ---------------------------------------------------------------------------

/// The example names passed to `cargo build --example <name>` in a workflow.
///
/// `--examples` (plural, no name) is a different argument and is deliberately
/// not matched: it is how the thirty demonstrations are compile-checked, and
/// counting it here would make the built set meaningless.
fn workflow_examples(yaml: &str) -> BTreeSet<String> {
    Regex::new(r"--example\s+([A-Za-z0-9_]+)")
        .unwrap()
        .captures_iter(yaml)
        .map(|c| c[1].to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// The comparison
// ---------------------------------------------------------------------------

/// Both directions. An example a test spawns and the matrix does not build is a
/// missing-file failure on a runner; an example the matrix builds and nothing
/// spawns is link time this release exists to stop paying.
fn sets_match(spawned: &BTreeSet<String>, built: &BTreeSet<String>) -> Result<(), String> {
    let unbuilt: Vec<&String> = spawned.difference(built).collect();
    let unspawned: Vec<&String> = built.difference(spawned).collect();
    if unbuilt.is_empty() && unspawned.is_empty() {
        return Ok(());
    }

    let mut msg = String::new();
    if !unbuilt.is_empty() {
        msg.push_str(&format!(
            "spawned by a test and NOT built by the matrix: {unbuilt:?}\n  \
             add `--example <name>` to the build step in .github/workflows/ci.yml — \
             `--lib --tests` does not build examples/, and without this the test fails \
             on a runner with a missing file rather than with a reason\n"
        ));
    }
    if !unspawned.is_empty() {
        msg.push_str(&format!(
            "built by the matrix and spawned by no test: {unspawned:?}\n  \
             remove it from ci.yml — it is linked six times per run to prove it compiles, \
             which `cargo check --examples` already proves once\n"
        ));
    }
    Err(msg)
}

// ---------------------------------------------------------------------------
// Against the real files
// ---------------------------------------------------------------------------

#[test]
fn the_matrix_builds_exactly_the_examples_the_tests_spawn() {
    let root = repo_root();
    let known = examples_on_disk(&root.join("examples"));
    let spawned = spawned_across(&root.join("tests"), &known);
    let built = workflow_examples(&read(".github/workflows/ci.yml"));

    assert!(
        !spawned.is_empty(),
        "no test in tests/ resolves an example binary, which cannot be true — the scan \
         is looking for a line containing `join(\"examples\")`, `push(\"examples\")` or \
         `example_binary(` and found none. If the way a test locates a fixture has \
         changed, this checker has stopped checking anything."
    );

    if let Err(diff) = sets_match(&spawned, &built) {
        panic!(
            "the CI matrix and the test suite disagree about the fixture examples:\n\n{diff}\n\
             spawned ({}): {spawned:?}\n built   ({}): {built:?}\n",
            spawned.len(),
            built.len()
        );
    }
}

/// The fixture examples a workflow builds on the **default** feature polarity.
///
/// A name whose own line also carries `--all-features` is excluded: those are
/// the two fixtures behind `browser` and `mcp-server`, which cargo refuses to
/// build on the default polarity and which each workflow handles its own way.
/// What is left is the hand-written list that both workflows' default jobs need
/// to agree on.
fn default_polarity_examples(yaml: &str) -> BTreeSet<String> {
    let flag = Regex::new(r"--example\s+([A-Za-z0-9_]+)").unwrap();
    yaml.lines()
        .filter(|line| !line.contains("--all-features"))
        .flat_map(|line| {
            flag.captures_iter(line)
                .map(|c| c[1].to_string())
                .collect::<Vec<_>>()
        })
        .collect()
}

/// `ci.yml` and `release.yml` name the same default-polarity fixtures.
///
/// **0.86.0 paid for this not existing.** `release.yml` keeps its own copy of the
/// list, for the same reason `ci.yml` does — `--lib --tests` does not build
/// `examples/` — and the test above reads only `ci.yml`. A new fixture added to
/// `ci.yml` alone passed that test, passed all 25 checks on the feature PR and
/// all 47 on the release PR, and then failed the release workflow *after the tag
/// had been pushed*, which is the most expensive place in the process to find it.
///
/// A guard covers the file it reads and no other. Where a list is duplicated, the
/// guard has to name every copy — which is what this test is.
///
/// Set equality rather than a subset in either direction: a name in `release.yml`
/// that `ci.yml` does not have is the same drift facing the other way, and would
/// mean the release workflow builds a fixture nothing in CI proved is needed.
#[test]
fn both_workflows_name_the_same_default_polarity_fixtures() {
    let ci = default_polarity_examples(&read(".github/workflows/ci.yml"));
    let release = default_polarity_examples(&read(".github/workflows/release.yml"));

    assert!(
        ci.len() > 5,
        "ci.yml names only {} default-polarity fixtures, which cannot be right — this \
         comparison has stopped checking anything",
        ci.len()
    );
    if let Err(diff) = sets_match(&ci, &release) {
        panic!(
            "ci.yml and release.yml name different fixture examples on the default \
             polarity:\n\n{diff}\n ci.yml  ({}): {ci:?}\n release ({}): {release:?}\n\n\
             Both lists exist because `--lib --tests` does not build `examples/`. A name \
             in one and not the other fails the workflow that lacks it, on a runner, with \
             a missing file rather than a reason.",
            ci.len(),
            release.len()
        );
    }
}

/// Negative control: the comparison notices a name present in only one file.
#[test]
fn control_a_fixture_in_only_one_workflow_is_reported() {
    let ci = default_polarity_examples("run: cargo build --example a --example b\n");
    let release = default_polarity_examples("run: cargo build --example a\n");
    assert!(
        sets_match(&ci, &release).is_err(),
        "a fixture named in one workflow and not the other is reported"
    );
}

/// Negative control: an all-features line is not part of the default list.
#[test]
fn control_an_all_features_fixture_is_not_a_default_polarity_build() {
    let names = default_polarity_examples(
        "run: cargo build --all-features --example browser_fixture\nrun: cargo build --example tick\n",
    );
    assert!(!names.contains("browser_fixture"), "{names:?}");
    assert!(names.contains("tick"), "{names:?}");
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------

#[test]
fn control_a_spawn_the_workflow_does_not_build_is_reported() {
    let known: BTreeSet<String> = ["crash_fixture", "ghost_fixture"]
        .iter()
        .map(|s| s.to_string())
        .collect();

    // A test file shaped exactly like the real ones, spawning a fixture the
    // workflow below does not name. This is the failure the whole file exists
    // for: the fifth occurrence of trap 18, caught here instead of on a runner.
    let fixture = r#"
        fn ghost_bin() -> PathBuf {
            let mut dir = std::env::current_exe().unwrap();
            dir.pop();
            dir.join("examples").join("ghost_fixture")
        }
    "#;
    let workflow = "      - run: cargo build --tests --example crash_fixture\n";

    let spawned = spawned_examples(fixture, &known);
    assert!(
        spawned.contains("ghost_fixture"),
        "the scan must find a fixture spawned in the ordinary shape, got {spawned:?}"
    );

    let err = sets_match(&spawned, &workflow_examples(workflow))
        .expect_err("an unbuilt spawn must be reported");
    assert!(err.contains("NOT built"), "{err}");
    assert!(err.contains("ghost_fixture"), "{err}");
}

#[test]
fn control_an_example_built_for_nobody_is_reported() {
    let spawned: BTreeSet<String> = ["crash_fixture"].iter().map(|s| s.to_string()).collect();
    let built = workflow_examples("- run: cargo build --example crash_fixture --example spare\n");

    let err = sets_match(&spawned, &built).expect_err("an unspawned build must be reported");
    assert!(err.contains("spawned by no test"), "{err}");
    assert!(err.contains("spare"), "{err}");
}

#[test]
fn control_a_name_only_in_a_comment_is_not_spawned() {
    // `tests/attach.rs` really does carry a doc comment naming three sibling
    // fixtures. If prose could put an example in the set, this checker would
    // demand CI build whatever a comment happened to mention.
    let known: BTreeSet<String> = ["crash_fixture", "fleet_fixture"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let fixture = r#"
        /// It joins "crash_fixture" and "fleet_fixture" on the list of examples
        /// that `--lib --tests` cannot build.
        fn unrelated() -> usize {
            0
        }
    "#;
    assert!(
        spawned_examples(fixture, &known).is_empty(),
        "a fixture named only in a comment must not reach the set"
    );
}

#[test]
fn control_a_tool_name_far_from_a_spawn_is_not_spawned() {
    // `edit_file` is both an example and the name of a tool that appears as a
    // string literal throughout tests/. Distance from a spawn marker is the
    // only thing separating them.
    let known: BTreeSet<String> = ["edit_file", "crash_fixture"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let fixture = r#"
        fn a_run_edits_a_file() {
            assert_eq!(call.tool, "edit_file");
        }

        fn crash_bin() -> PathBuf {
            dir.join("examples").join("crash_fixture")
        }
    "#;
    assert_eq!(
        spawned_examples(fixture, &known),
        ["crash_fixture"]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>(),
        "a tool name outside the window must not be read as a spawn"
    );
}

#[test]
fn control_the_plural_examples_flag_is_not_a_named_build() {
    // `cargo check --examples` is how the thirty demonstrations keep their
    // compile check. If it were matched as a name, the built set would silently
    // absorb everything and the comparison would prove nothing.
    let built =
        workflow_examples("- run: cargo check --examples\n- run: cargo build --example tick\n");
    assert_eq!(
        built,
        ["tick"]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
    );
}

// --- 0.47.0: the leg that leaves the Ubuntu restriction alone ---------------
//
// Trap 87 is that a guard written for one workflow file does not cover its
// sibling. `tests/ci_workflow.rs` was written in 0.36.1 to stop exactly that and
// could not see `release.yml`; this pair names both files explicitly, because
// the leg that matters most is worth nothing if it exists in only one of them.

/// The restricted-userns leg exists in **both** workflow files and states which
/// rung it expects.
///
/// Without it the configuration a real user is in — a stock Ubuntu 24.04, which
/// is what `ubuntu-latest` is — is exercised nowhere, and every release up to
/// 0.46.0 silently took the portable floor there.
#[test]
fn both_workflows_have_a_leg_with_the_userns_restriction_left_in_place() {
    for file in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let yaml = read(file);
        assert!(
            yaml.contains("contained-linux:"),
            "{file} has no leg that leaves the userns restriction in place"
        );
        assert!(
            yaml.contains("IO_HARNESS_EXPECT_BACKEND: linux-landlock"),
            "{file}'s contained leg does not say which rung it expects, so it \
             would pass on the portable floor"
        );
    }
}

/// The legs that *do* restore the namespace keep doing so, in both files.
///
/// The new leg is an addition, not a replacement: without these the namespace
/// rung is exercised nowhere and the chain's lower rungs go untested.
#[test]
fn both_workflows_still_restore_unprivileged_user_namespaces_somewhere() {
    for file in [".github/workflows/ci.yml", ".github/workflows/release.yml"] {
        let yaml = read(file);
        assert!(
            yaml.contains("kernel.apparmor_restrict_unprivileged_userns=0"),
            "{file} no longer restores unprivileged user namespaces on any leg, \
             so the namespace rung is tested nowhere"
        );
    }
}

/// The release job names the new gate in its `needs:`.
///
/// 0.36.1's release gate caught a defect and refused to ship precisely because
/// `release` names every gate. A gate that runs and is not named is a gate the
/// release can ship past.
#[test]
fn the_release_job_will_not_ship_past_the_contained_leg() {
    let yaml = read(".github/workflows/release.yml");
    let needs = yaml
        .lines()
        .find(|l| l.trim_start().starts_with("needs: [checks, lint,"))
        .expect("the release job's needs: list");
    assert!(
        needs.contains("contained-linux"),
        "the release job does not name the contained leg, so a tree that failed \
         it could still be published: {needs}"
    );
}
