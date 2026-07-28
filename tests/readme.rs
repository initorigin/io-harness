//! The README and the crate root are a landing page, not a release history.
//!
//! Two properties, both of which the 0.15.0 files failed:
//!
//! F1 — the first screen answers "what is this and how do I start". Within the
//! first 60 lines the README says what the crate is, shows a code fence a reader
//! can run, and names the MSRV. The 0.15.0 README's first code fence was on line
//! 107, below a capability list and a status paragraph.
//!
//! F2 — no heading is named after the release that introduced it, and the crate
//! root carries no `v0.N adds` narrative. The 0.15.0 files were organised
//! entirely that way: `## Usage (v0.4)`, `## MCP and network egress (v0.8)`, and
//! 230 lines of `//! v0.2 adds`, with v0.3 filed after v0.14.
//!
//! Both checks are pure functions over text so the negative controls can feed
//! them the 0.15.0 shape and watch them fail. A checker that cannot be made to
//! fail is a green light wired to nothing.

use std::fs;
use std::path::Path;

/// How much of the README counts as the first screen.
const FIRST_SCREEN: usize = 60;

fn repo_root() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// F2 — no heading names a release
// ---------------------------------------------------------------------------

/// Every markdown heading in `text` that names a version, e.g. `## Usage (v0.4)`
/// or `## Documents (0.14)`.
///
/// Fenced code blocks are skipped: a `#` inside a fence is a comment or a shell
/// prompt, not a heading, and a quickstart that shows `# cargo add io-harness`
/// must not be read as document structure.
fn version_named_headings(text: &str) -> Vec<String> {
    let mut found = Vec::new();
    let mut in_fence = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence || !trimmed.starts_with('#') {
            continue;
        }
        if heading_names_a_version(trimmed) {
            found.push(trimmed.to_string());
        }
    }
    found
}

/// True when a heading carries a `(v0.4)` / `(0.14)` style version tag.
fn heading_names_a_version(heading: &str) -> bool {
    let bytes: Vec<char> = heading.chars().collect();
    for (i, c) in bytes.iter().enumerate() {
        if *c != '(' {
            continue;
        }
        let mut j = i + 1;
        if bytes.get(j) == Some(&'v') || bytes.get(j) == Some(&'V') {
            j += 1;
        }
        let digits_start = j;
        while bytes.get(j).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
        }
        if j == digits_start || bytes.get(j) != Some(&'.') {
            continue;
        }
        j += 1;
        let minor_start = j;
        while bytes.get(j).is_some_and(|c| c.is_ascii_digit()) {
            j += 1;
        }
        if j > minor_start {
            return true;
        }
    }
    false
}

/// Crate-root doc lines that narrate a release: `//! v0.9 adds ...`.
///
/// The changelog is where release history belongs and it is already better at
/// it. The crate root is what docs.rs opens on, and a reader landing there is
/// not asking which version added what.
fn release_narration(lib_rs: &str) -> Vec<String> {
    lib_rs
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            let Some(doc) = t.strip_prefix("//!") else {
                return false;
            };
            let doc = doc.trim_start();
            let Some(rest) = doc.strip_prefix('v') else {
                return false;
            };
            let mut chars = rest.chars();
            if !chars.next().is_some_and(|c| c.is_ascii_digit()) {
                return false;
            }
            // `v0.9 adds`, `v0.14 makes`, `v0.8 is` — a version opening a sentence
            // about itself.
            rest.split_whitespace().next().is_some_and(|first| {
                first
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ',')
            })
        })
        .map(|line| line.trim().to_string())
        .collect()
}

#[test]
fn readme_has_no_version_named_heading() {
    let found = version_named_headings(&read("README.md"));
    assert!(
        found.is_empty(),
        "README headings are named after the release that introduced them. \
         The README is a landing page; the changelog is the release history.\n{}",
        found.join("\n")
    );
}

#[test]
fn crate_root_has_no_version_named_heading() {
    let found = version_named_headings(&read("src/lib.rs"));
    assert!(
        found.is_empty(),
        "crate-root headings are named after a release:\n{}",
        found.join("\n")
    );
}

#[test]
fn crate_root_does_not_narrate_releases() {
    let found = release_narration(&read("src/lib.rs"));
    assert!(
        found.is_empty(),
        "the crate root narrates its own release history. docs.rs opens on this \
         page and a reader arriving there is not asking which version added \
         what — describe what the crate does, and leave the history to \
         CHANGELOG.md.\n{}",
        found.join("\n")
    );
}

// ---------------------------------------------------------------------------
// F1 — the first screen
// ---------------------------------------------------------------------------

/// What the first screen of the README must contain. Returns the missing parts.
fn first_screen_gaps(readme: &str, declared_msrv: &str) -> Vec<&'static str> {
    let head: Vec<&str> = readme.lines().take(FIRST_SCREEN).collect();
    let head_text = head.join("\n");
    let mut gaps = Vec::new();

    // What the crate is: prose outside a fence, outside the badge block, before
    // the reader is asked to do anything.
    let says_what_it_is = head.iter().any(|line| {
        let t = line.trim();
        t.len() > 40
            && !t.starts_with('#')
            && !t.starts_with("[!")
            && !t.starts_with('|')
            && !t.starts_with("```")
    });
    if !says_what_it_is {
        gaps.push("a sentence saying what the crate is");
    }

    if !head_text.contains("```") {
        gaps.push("a code fence a reader can run");
    }

    if !head_text.contains(declared_msrv) {
        gaps.push("the MSRV");
    }

    gaps
}

fn declared_msrv() -> String {
    read("Cargo.toml")
        .lines()
        .find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("rust-version")?;
            let value = rest.split('=').nth(1)?;
            Some(value.trim().trim_matches('"').to_string())
        })
        .expect("Cargo.toml declares no rust-version")
}

#[test]
fn readme_opens_as_a_landing_page() {
    let msrv = declared_msrv();
    let gaps = first_screen_gaps(&read("README.md"), &msrv);
    assert!(
        gaps.is_empty(),
        "the README's first {FIRST_SCREEN} lines are missing: {}. \
         A reader arriving from crates.io asks what this is and how to start, \
         and must not have to scroll past prose to find out.",
        gaps.join(", ")
    );
}

// ---------------------------------------------------------------------------
// Negative controls
//
// Each checker is fed the 0.15.0 shape and must report it. Without these the
// tests above would pass just as well if the checkers matched nothing at all.
// ---------------------------------------------------------------------------

#[test]
fn control_version_named_headings_are_reported() {
    // The real headings of the 0.15.0 README, extracted at the commit before the
    // rewrite. Kept as a fixture rather than read through git so the control
    // survives a shallow clone.
    let fixture = read("tests/fixtures/readme-0.15.0-headings.md");
    let found = version_named_headings(&fixture);

    assert!(
        found.len() >= 10,
        "the 0.15.0 README had a version-named heading for nearly every \
         capability; the checker found only {}: {found:?}",
        found.len()
    );
    assert!(
        found.iter().any(|h| h.contains("Usage (v0.4)")),
        "expected `## Usage (v0.4)` among the reported headings, got {found:?}"
    );
    assert!(
        found.iter().any(|h| h.contains("Documents (v0.14)")),
        "expected `## Documents (v0.14)` among the reported headings, got {found:?}"
    );
}

#[test]
fn control_a_clean_heading_is_not_reported() {
    // The other half of the control: the checker must not simply flag every
    // heading. A page of clean headings reports nothing.
    let clean = "# IO Harness\n## Quickstart\n## What it does\n### Feature flags\n";
    assert!(version_named_headings(clean).is_empty());
}

#[test]
fn control_release_narration_is_reported() {
    let fixture = "//! # io-harness\n\
                   //!\n\
                   //! v0.2 bounds the run with step, time, and cost budgets.\n\
                   //! v0.14 adds documents behind an opt-in feature.\n\
                   //! The loop observes, reasons, acts, and verifies.\n";
    let found = release_narration(fixture);
    assert_eq!(
        found.len(),
        2,
        "expected both narrating lines to be reported, got {found:?}"
    );
    assert!(found[0].contains("v0.2 bounds"));
    assert!(found[1].contains("v0.14 adds"));
}

#[test]
fn control_first_screen_gaps_are_reported() {
    // A README shaped like 0.15.0's: prose and a capability list first, the code
    // fence far below the fold, no MSRV in sight.
    let mut buried = String::from("# IO Harness\n\n");
    for i in 0..FIRST_SCREEN {
        buried.push_str(&format!("- capability number {i}\n"));
    }
    buried.push_str("```rust\nfn main() {}\n```\n");

    let gaps = first_screen_gaps(&buried, "1.88");
    assert!(
        gaps.contains(&"a code fence a reader can run"),
        "a quickstart below the fold must be reported, got {gaps:?}"
    );
    assert!(
        gaps.contains(&"the MSRV"),
        "a missing MSRV must be reported, got {gaps:?}"
    );

    // And the positive half: a first screen that has all three reports nothing.
    let good = "# IO Harness\n\nRun an AI agent from a typed task contract to a \
                verified result, in your own process.\n\n```toml\nio-harness = \
                \"0.16\"\n```\n\nRequires Rust 1.88 or later.\n";
    assert!(first_screen_gaps(good, "1.88").is_empty());
}
