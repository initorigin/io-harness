//! Documentation drift checkers — F4, F9, F10 of 0.16.0; F7 of 0.36.1.
//!
//! Four facts are stated in `Cargo.toml` and then retyped into prose, which is
//! the shape that rots: the MSRV, the feature list, every relative link, and
//! the version a reader is told to depend on. Each checker here is a pure
//! function over text plus one test that runs it against the real files, and
//! each carries a negative control — a fixture that must fail — because a
//! checker that has never failed is a checker nobody has shown to work.
//!
//! The fourth was added by 0.36.1 and it is the one that rotted furthest. The
//! README's `[dependencies]` snippet said `io-harness = "0.25"` for eleven
//! releases: a reader copying the single highest-traffic line in the repository
//! got a crate without `Session`'s current surface, without plugins, without the
//! git built-ins, without `rewind_run` — and got no error, because `0.25`
//! resolves. Eleven releases of drift in a file that already had a drift
//! checker is the argument for the fourth checker, not against the three.
//!
//! Two conventions this file fixes, so the prose tasks know what to write:
//!
//! * MSRV — any line of the README, of the crate-root docs, or of
//!   `docs/CONTRACT.md` that says "MSRV" or "minimum supported Rust" must state
//!   `Cargo.toml`'s `rust-version` and no other version, and at least one such
//!   line must exist in each of the three.
//! * Feature list — `docs/CONTRACT.md` carries the canonical list, under a
//!   heading containing the word "feature", as a markdown list or table whose
//!   rows open with the feature name in backticks. That list is compared with
//!   the `[features]` keys in *both* directions.
//!
//! `Cargo.toml` is read by line scanning rather than by a toml parser: the two
//! things needed from it are a `key = "value"` line and one flat table, and a
//! parser dependency to read a file this crate already owns is not worth it.

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

// ---------------------------------------------------------------------------
// Cargo.toml — line scanning
// ---------------------------------------------------------------------------

/// The `rust-version` value from a `Cargo.toml`'s `[package]` table.
fn declared_msrv(cargo_toml: &str) -> String {
    cargo_toml
        .lines()
        .find_map(|line| {
            let rest = line.trim().strip_prefix("rust-version")?;
            let value = rest.trim_start().strip_prefix('=')?.trim();
            Some(value.trim_matches('"').to_string())
        })
        .expect("Cargo.toml declares no rust-version")
}

/// The `version` value from a `Cargo.toml`'s `[package]` table.
///
/// The first `version = ` line in the file: `[package]` opens it, and the
/// `[dependencies]` entries below use inline tables or their own `version` keys
/// inside a nested table, never a bare top-level one. The same assumption
/// `release.yml` already makes with `sed -n 's/^version = "\(.*\)"/\1/p' | head -1`.
fn declared_version(cargo_toml: &str) -> String {
    cargo_toml
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix("version")?;
            let value = rest.trim_start().strip_prefix('=')?.trim();
            Some(value.trim_matches('"').to_string())
        })
        .expect("Cargo.toml declares no version")
}

/// The keys of the `[features]` table.
fn declared_features(cargo_toml: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut inside = false;
    for line in cargo_toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            inside = line == "[features]";
            continue;
        }
        if !inside || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, _)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                out.insert(key.to_string());
            }
        }
    }
    assert!(!out.is_empty(), "Cargo.toml has no [features] table");
    out
}

/// The crate-root documentation of `src/lib.rs` — its leading `//!` block.
fn crate_root_docs(lib_rs: &str) -> String {
    lib_rs
        .lines()
        .filter_map(|l| l.trim_start().strip_prefix("//!"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// F9 — MSRV drift
// ---------------------------------------------------------------------------

fn version_re() -> Regex {
    Regex::new(r"\b\d+\.\d+(?:\.\d+)?\b").unwrap()
}

/// Does `doc_text` state `declared` as its MSRV, and nothing else?
///
/// Lines are the unit: a line that names the MSRV must carry the declared
/// version. A line that mentions some other version without naming the MSRV
/// (the README's note about what happens on 1.87) is prose, not a claim.
fn msrv_matches(declared: &str, doc_text: &str) -> Result<(), String> {
    let versions = version_re();
    let mut stated = false;
    let mut contradictions = Vec::new();

    for (i, line) in doc_text.lines().enumerate() {
        let lower = line.to_lowercase();
        if !(lower.contains("msrv") || lower.contains("minimum supported rust")) {
            continue;
        }
        let found: Vec<&str> = versions.find_iter(line).map(|m| m.as_str()).collect();
        if found.is_empty() {
            continue;
        }
        if found.iter().all(|v| *v == declared) {
            stated = true;
        } else {
            contradictions.push(format!("line {}: {}", i + 1, line.trim()));
        }
    }

    if !contradictions.is_empty() {
        return Err(format!(
            "states an MSRV other than Cargo.toml's {declared}:\n  {}",
            contradictions.join("\n  ")
        ));
    }
    if !stated {
        return Err(format!(
            "states no MSRV — Cargo.toml declares rust-version = \"{declared}\", \
             so this file needs a line naming it (\"MSRV\" or \"minimum supported Rust\")"
        ));
    }
    Ok(())
}

#[test]
fn msrv_is_stated_and_matches_cargo_toml() {
    let declared = declared_msrv(&read("Cargo.toml"));
    let sources: [(&str, String); 3] = [
        ("README.md", read("README.md")),
        (
            "src/lib.rs (crate-root docs)",
            crate_root_docs(&read("src/lib.rs")),
        ),
        ("docs/CONTRACT.md", read("docs/CONTRACT.md")),
    ];

    let failures: Vec<String> = sources
        .iter()
        .filter_map(|(name, text)| {
            msrv_matches(&declared, text)
                .err()
                .map(|e| format!("{name} {e}"))
        })
        .collect();

    assert!(
        failures.is_empty(),
        "MSRV drift (Cargo.toml rust-version = \"{declared}\"):\n\n{}\n",
        failures.join("\n\n")
    );
}

#[test]
fn msrv_checker_rejects_a_different_version() {
    let fixture = "# Fixture\n\n- **MSRV:** Rust **1.75**, which is not what Cargo.toml says.\n";
    let err = msrv_matches("1.88", fixture).expect_err("a stale MSRV must be reported");
    assert!(
        err.contains("1.88"),
        "message should name the declared version: {err}"
    );
    assert!(
        err.contains("1.75"),
        "message should quote the offending line: {err}"
    );
}

#[test]
fn msrv_checker_rejects_silence() {
    let err = msrv_matches("1.88", "# Fixture\n\nNo mention of a floor at all.\n")
        .expect_err("a doc that states no MSRV must be reported");
    assert!(err.contains("states no MSRV"), "{err}");
}

#[test]
fn msrv_checker_accepts_the_real_declaration() {
    let fixture = "[![MSRV 1.88](https://img.shields.io/badge/MSRV-1.88-blue.svg)](Cargo.toml)\n\
                   - **MSRV:** Rust **1.88**. The floor comes from `rmcp`, which publishes no\n\
                     `rust-version` of its own — on 1.87 the build fails inside it.\n";
    assert_eq!(msrv_matches("1.88", fixture), Ok(()));
}

// ---------------------------------------------------------------------------
// F7 of 0.36.1 — the version a reader is told to depend on
// ---------------------------------------------------------------------------

/// `major.minor` of a full version — what a reader writes in `[dependencies]`.
///
/// The snippet carries two components on purpose. `io-harness = "0.36"` is the
/// requirement a caller wants (any 0.36.x), and it is also what a patch release
/// must not churn: bumping the README on every patch would put a line in the
/// diff of every release that says nothing.
fn major_minor(version: &str) -> String {
    version.split('.').take(2).collect::<Vec<_>>().join(".")
}

/// Every `io-harness = "<version>"` dependency line in `doc_text`, as
/// `(line number, version)`.
///
/// Inside a fence, deliberately: this is the snippet a reader copies out of a
/// ```toml block, and a mention of the crate name in prose is not a claim about
/// which version to depend on.
fn readme_dependency_versions(doc_text: &str) -> Vec<(usize, String)> {
    let dep = Regex::new(r#"^\s*io-harness\s*=\s*"([^"]+)""#).unwrap();
    doc_text
        .lines()
        .enumerate()
        .filter_map(|(i, line)| dep.captures(line).map(|c| (i + 1, c[1].to_string())))
        .collect()
}

/// Does every `[dependencies]` snippet name the current `major.minor`?
///
/// Silence is a failure for the same reason it is for the MSRV: a landing page
/// with no install line is not a landing page, and a checker that accepts an
/// absent claim stops noticing when the claim is deleted rather than corrected.
fn dependency_version_matches(declared: &str, doc_text: &str) -> Result<(), String> {
    let want = major_minor(declared);
    let found = readme_dependency_versions(doc_text);

    if found.is_empty() {
        return Err(format!(
            "states no dependency version — Cargo.toml declares {declared}, so this file needs \
             an `io-harness = \"{want}\"` line in its install snippet"
        ));
    }

    let stale: Vec<String> = found
        .iter()
        .filter(|(_, v)| *v != want)
        .map(|(line, v)| format!("line {line}: io-harness = \"{v}\""))
        .collect();

    if stale.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "tells a reader to depend on a version that is not this one \
             (Cargo.toml declares {declared}, so the snippet must say \"{want}\"):\n  {}",
            stale.join("\n  ")
        ))
    }
}

#[test]
fn readme_dependency_version_matches_cargo_toml() {
    let declared = declared_version(&read("Cargo.toml"));
    if let Err(e) = dependency_version_matches(&declared, &read("README.md")) {
        panic!(
            "README.md {e}\n\n\
             This is the single highest-traffic line in the repository: it is what a reader \
             copies from the crates.io landing page. A stale one produces no error — an old \
             version resolves — so nobody reports it, which is how it went eleven releases \
             without being noticed.\n"
        );
    }
}

#[test]
fn dependency_version_checker_rejects_the_line_that_was_there_for_eleven_releases() {
    // The real line, verbatim, as `README.md:28` carried it from 0.25.0 to
    // 0.36.0. The control is the defect itself rather than an invented one.
    let fixture = "```toml\n[dependencies]\nio-harness = \"0.25\"\n\
                   tokio = { version = \"1\", features = [\"rt-multi-thread\"] }\n```\n";
    let err = dependency_version_matches("0.36.1", fixture)
        .expect_err("the stale install line must be reported");
    assert!(err.contains("0.25"), "must quote the offending line: {err}");
    assert!(err.contains("0.36"), "must name what it should say: {err}");
}

#[test]
fn dependency_version_checker_accepts_the_corrected_line() {
    let fixture = "```toml\n[dependencies]\nio-harness = \"0.36\"\n```\n";
    assert_eq!(dependency_version_matches("0.36.1", fixture), Ok(()));
}

#[test]
fn dependency_version_checker_rejects_silence() {
    let err = dependency_version_matches("0.36.1", "# io-harness\n\nNo install line at all.\n")
        .expect_err("a README with no install snippet must be reported");
    assert!(err.contains("states no dependency version"), "{err}");
}

#[test]
fn dependency_version_is_major_minor_so_a_patch_does_not_churn_the_readme() {
    // The whole point of two components: 0.36.0 and 0.36.1 want the same line,
    // so a patch release does not put a no-op edit in its own diff.
    assert_eq!(major_minor("0.36.1"), "0.36");
    assert_eq!(major_minor("0.36.0"), "0.36");
    let fixture = "io-harness = \"0.36\"\n";
    assert_eq!(dependency_version_matches("0.36.0", fixture), Ok(()));
    assert_eq!(dependency_version_matches("0.36.1", fixture), Ok(()));
    assert!(dependency_version_matches("0.37.0", fixture).is_err());
}

// ---------------------------------------------------------------------------
// F10 — feature-list drift, both directions
// ---------------------------------------------------------------------------

/// Feature names documented in `doc_text`: list items or table rows under a
/// heading that contains the word "feature", each opening with the name in
/// backticks.
fn documented_features(doc_text: &str) -> BTreeSet<String> {
    let heading = Regex::new(r"(?i)^#{1,6}\s+.*\bfeature").unwrap();
    let entry = Regex::new(r"^\s*(?:[-*+]|\|)\s*\**`([A-Za-z0-9_-]+)`").unwrap();

    let mut out = BTreeSet::new();
    let mut inside = false;
    for line in doc_text.lines() {
        if line.trim_start().starts_with('#') {
            inside = heading.is_match(line.trim_start());
            continue;
        }
        if inside {
            if let Some(c) = entry.captures(line) {
                out.insert(c[1].to_string());
            }
        }
    }
    out
}

/// Both directions: a feature Cargo.toml has and the docs do not, and a feature
/// the docs have and Cargo.toml does not. One direction alone passes forever
/// after the first stale entry.
fn feature_list_matches(
    declared: &BTreeSet<String>,
    documented: &BTreeSet<String>,
) -> Result<(), String> {
    let undocumented: Vec<&String> = declared.difference(documented).collect();
    let unknown: Vec<&String> = documented.difference(declared).collect();
    if undocumented.is_empty() && unknown.is_empty() {
        return Ok(());
    }

    let mut msg = String::new();
    if !undocumented.is_empty() {
        msg.push_str(&format!(
            "in Cargo.toml's [features] but not documented: {undocumented:?}\n"
        ));
    }
    if !unknown.is_empty() {
        msg.push_str(&format!(
            "documented but absent from Cargo.toml's [features]: {unknown:?}\n"
        ));
    }
    Err(msg)
}

#[test]
fn documented_feature_list_matches_cargo_toml() {
    let declared = declared_features(&read("Cargo.toml"));
    let contract = read("docs/CONTRACT.md");
    let documented = documented_features(&contract);

    assert!(
        !documented.is_empty(),
        "docs/CONTRACT.md documents no feature flags. It is the canonical list: it needs a \
         heading containing \"feature\" followed by one list item or table row per feature, \
         each opening with the feature name in backticks. Cargo.toml declares {declared:?}"
    );

    if let Err(diff) = feature_list_matches(&declared, &documented) {
        panic!("feature-list drift between Cargo.toml and docs/CONTRACT.md:\n{diff}");
    }
}

#[test]
fn feature_checker_rejects_an_undocumented_feature() {
    let declared: BTreeSet<String> = ["default", "media", "documents"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let documented: BTreeSet<String> = ["default", "documents"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let err = feature_list_matches(&declared, &documented).expect_err("must report the gap");
    assert!(err.contains("not documented"), "{err}");
    assert!(err.contains("media"), "{err}");
}

#[test]
fn feature_checker_rejects_a_documented_feature_that_does_not_exist() {
    let declared: BTreeSet<String> = ["default", "media"].iter().map(|s| s.to_string()).collect();
    let documented: BTreeSet<String> = ["default", "media", "audio"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let err =
        feature_list_matches(&declared, &documented).expect_err("must report the stale entry");
    assert!(err.contains("absent from Cargo.toml"), "{err}");
    assert!(err.contains("audio"), "{err}");
}

#[test]
fn feature_extractor_reads_lists_and_tables() {
    let fixture = "## Feature flags\n\n\
                   - `default` — nothing.\n\
                   - **`media`** — images.\n\n\
                   ## Something else\n\n\
                   - `not-a-feature` — outside the section.\n\n\
                   ### Per-format features\n\n\
                   | `docx` | Word |\n";
    let found = documented_features(fixture);
    assert_eq!(
        found,
        ["default", "media", "docx"]
            .iter()
            .map(|s| s.to_string())
            .collect::<BTreeSet<_>>()
    );
}

// ---------------------------------------------------------------------------
// F4 — relative links resolve
// ---------------------------------------------------------------------------

/// Every relative link target in `text`, as `(line number, target)`. Inline
/// links and reference definitions both count; fenced code is skipped, and so
/// are absolute URLs and bare `#anchor` fragments.
fn relative_links(text: &str) -> Vec<(usize, String)> {
    let inline = Regex::new(r"\]\(([^)]+)\)").unwrap();
    let reference = Regex::new(r"^\s{0,3}\[[^\]]+\]:\s*(\S+)").unwrap();

    let mut out = Vec::new();
    let mut fenced = false;
    for (i, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }

        let targets = inline
            .captures_iter(line)
            .map(|c| c[1].to_string())
            .chain(reference.captures(line).map(|c| c[1].to_string()));

        for target in targets {
            // Drop any link title: `(path "title")`.
            let target = target.split_whitespace().next().unwrap_or_default();
            if target.is_empty()
                || target.starts_with('#')
                || target.starts_with('<')
                || target.contains("://")
                || target.starts_with("mailto:")
            {
                continue;
            }
            // Strip the anchor; an anchor-only remainder is same-page.
            let path = target.split('#').next().unwrap_or_default();
            if path.is_empty() {
                continue;
            }
            out.push((i + 1, path.to_string()));
        }
    }
    out
}

/// Relative link targets in `text` that do not resolve to something on disk,
/// relative to the directory holding the document.
fn dangling_links(dir: &Path, text: &str) -> Vec<(usize, String)> {
    relative_links(text)
        .into_iter()
        .filter(|(_, target)| !dir.join(target).exists())
        .collect()
}

fn markdown_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            markdown_files(&path, out);
        } else if path.extension().is_some_and(|e| e == "md") {
            out.push(path);
        }
    }
}

#[test]
fn relative_links_in_readme_and_docs_resolve() {
    let root = repo_root();
    let mut files = vec![root.join("README.md")];
    markdown_files(&root.join("docs"), &mut files);
    files.sort();

    let mut failures = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("readable markdown");
        let dir = file.parent().expect("file has a parent");
        for (line, target) in dangling_links(dir, &text) {
            let shown = file.strip_prefix(&root).unwrap_or(file);
            failures.push(format!("{}:{line}: {target}", shown.display()));
        }
    }

    assert!(
        failures.is_empty(),
        "dangling relative links ({} in {} files):\n  {}\n",
        failures.len(),
        files.len(),
        failures.join("\n  ")
    );
}

#[test]
fn link_checker_reports_an_absent_target() {
    let dir = tempfile::tempdir().expect("tempdir");
    fs::write(dir.path().join("present.md"), "# present\n").unwrap();
    let doc = "See [the one that exists](present.md) and [the one that does not](missing.md).\n\
               Also [an anchor into a real file](present.md#heading), which is fine,\n\
               [an absolute URL](https://example.com/nope.md), which is not ours,\n\
               and [a fragment](#somewhere) on this page.\n\
               \n\
               ```\n\
               [not a link](also-missing.md)\n\
               ```\n";

    let dangling = dangling_links(dir.path(), doc);
    assert_eq!(
        dangling,
        vec![(1, "missing.md".to_string())],
        "exactly the absent relative target should be reported"
    );
}
