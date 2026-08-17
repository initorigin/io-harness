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

/// A checked-in text file, with its line endings normalised to `\n`.
///
/// `.gitattributes` pins `eol=lf` for `tests/fixtures/**` and for nothing else,
/// so a Windows checkout hands these pages back with CRLF. A checker that looks
/// for a blank line before a marker — `"\n\n**"` — matches nothing in
/// `"\r\n\r\n**"`, and the failure is silent: the window it was computing simply
/// runs to the end of the file and the assertion inside it passes for the wrong
/// reason. Normalising here rather than in each checker is what stops the next
/// one reintroducing it. Found by a control test on the Windows leg, green on
/// macOS throughout.
fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    let text =
        fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
    text.replace("\r\n", "\n")
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

// ---------------------------------------------------------------------------
// The landing page states the present, not the diff that produced it
// ---------------------------------------------------------------------------

/// Every release-version literal in `doc_text`, as `(line number, version)`.
///
/// The shape is this crate's own: `0.<minor>.<patch>`. Rust versions (`1.95`),
/// the `major.minor` install requirement (`"0.60"`) and a measured duration
/// (`0.965 ms`) all have too few components to match, which is deliberate —
/// each of those is a claim the landing page is supposed to make.
fn release_version_mentions(doc_text: &str) -> Vec<(usize, String)> {
    let version = Regex::new(r"\b0\.\d+\.\d+\b").unwrap();
    doc_text
        .lines()
        .enumerate()
        .flat_map(|(i, line)| {
            version
                .find_iter(line)
                .map(move |m| (i + 1, m.as_str().to_string()))
        })
        .collect()
}

/// Does `doc_text` state what the crate does, rather than which release changed
/// it?
///
/// The README grew one release at a time, and by 0.60.0 most of its capability
/// section was written as the release that added it — "Since 0.46.0 every
/// command `exec` ...", "**0.47.0 closed the Linux hole in this table**",
/// "Through 0.49.0 a child came back as ...". A reader who has never run this
/// crate was being handed the diff between two releases they have never used and
/// asked to reconstruct the present from it. CHANGELOG.md is the changelog, and
/// `docs/CAPABILITIES.md` records which release introduced what.
fn states_the_present(doc_text: &str) -> Result<(), String> {
    let found = release_version_mentions(doc_text);
    if found.is_empty() {
        return Ok(());
    }
    let listed: Vec<String> = found
        .iter()
        .map(|(line, v)| format!("line {line}: {v}"))
        .collect();
    Err(format!(
        "narrates its own release history:\n  {}",
        listed.join("\n  ")
    ))
}

#[test]
fn readme_states_no_release_version_outside_the_pinned_lines() {
    if let Err(e) = states_the_present(&read("README.md")) {
        panic!(
            "README.md {e}\n\n\
             A landing page states what the crate does now. Which release introduced a \
             capability belongs in docs/CAPABILITIES.md, and the full history belongs in \
             CHANGELOG.md. The two version claims this page *does* make — the install \
             snippet's `major.minor` and the MSRV — have their own gates above and are too \
             short to match this one.\n"
        );
    }
}

#[test]
fn present_tense_checker_rejects_a_reinstated_sentence() {
    // The real sentence, verbatim, as README.md carried it through 0.60.0.
    let fixture = "**0.47.0 closed the Linux hole in this table**, which was the easiest \
                   thing on this page to over-read.\n";
    let err = states_the_present(fixture).expect_err("the archaeology must be reported");
    assert!(
        err.contains("0.47.0"),
        "must quote the version it found: {err}"
    );
    assert!(err.contains("line 1"), "must say where: {err}");
}

#[test]
fn present_tense_checker_accepts_the_claims_the_page_is_meant_to_make() {
    // The install snippet, the MSRV, a measured duration, and a pre-1.0 note:
    // none of them is release archaeology, and a checker that flagged them would
    // be deleted within a release.
    let fixture = "io-harness = \"0.60\"\n\
                   **MSRV: Rust 1.95** or later.\n\
                   0.965 ms per capped write, and 303.8 ms saved.\n\
                   The crate is pre-1.0 and stays pre-1.0.\n";
    assert_eq!(states_the_present(fixture), Ok(()));
}

// ---------------------------------------------------------------------------
// The landing page points at the numbers
// ---------------------------------------------------------------------------

/// Does `doc_text` link `docs/MEASUREMENTS.md`?
///
/// Five measured benchmark sets sat in that file while the README linked it zero
/// times, so the one question a reader evaluating a runtime always has — what
/// does it cost — was answerable everywhere except the page they land on.
fn links_the_measurements(doc_text: &str) -> Result<(), String> {
    if doc_text.contains("docs/MEASUREMENTS.md") {
        Ok(())
    } else {
        Err("links docs/MEASUREMENTS.md nowhere".to_string())
    }
}

#[test]
fn readme_links_the_measurements() {
    if let Err(e) = links_the_measurements(&read("README.md")) {
        panic!(
            "README.md {e}\n\n\
             The measurements are the method behind every number the landing page quotes. \
             A number with no reachable method is a number nobody can reproduce or refute.\n"
        );
    }
}

#[test]
fn measurements_link_checker_reports_silence() {
    assert!(links_the_measurements("# io-harness\n\nFast. Trust me.\n").is_err());
    assert_eq!(
        links_the_measurements("See [the numbers](docs/MEASUREMENTS.md).\n"),
        Ok(())
    );
}

// ---------------------------------------------------------------------------
// The contract does not outlive a release
// ---------------------------------------------------------------------------

/// The claim `docs/CONTRACT.md` carried for thirty-three releases after it
/// stopped being true.
const RETIRED_APPCONTAINER_CLAIM: &str = "nothing selects it";

/// Does `doc_text` state the Windows access-confinement selector, and has it
/// stopped saying nothing selects it?
///
/// A contract that is wrong about the security boundary is worse than one that
/// is silent: a reader who checks is misinformed rather than uninformed.
fn states_the_appcontainer_selector(doc_text: &str) -> Result<(), String> {
    let mut wrong = Vec::new();
    if !doc_text.contains("with_access_confinement") {
        wrong.push(
            "names no selector for Windows access confinement — \
             `SandboxConfig::with_access_confinement()` is what a caller writes"
                .to_string(),
        );
    }
    if let Some(line) = doc_text
        .lines()
        .position(|l| l.contains(RETIRED_APPCONTAINER_CLAIM))
    {
        wrong.push(format!(
            "line {}: still says \"{RETIRED_APPCONTAINER_CLAIM}\", which a shipped selector \
             made false",
            line + 1
        ));
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

#[test]
fn contract_states_the_appcontainer_selector() {
    if let Err(e) = states_the_appcontainer_selector(&read("docs/CONTRACT.md")) {
        panic!(
            "docs/CONTRACT.md {e}\n\n\
             This file is what a caller may depend on. It kept describing the AppContainer as \
             built-but-unselectable after the release that selected it shipped, which is the \
             one kind of drift that leaves a careful reader worse off than a careless one.\n"
        );
    }
}

#[test]
fn appcontainer_checker_reports_the_sentence_that_was_there() {
    // The real sentence, verbatim.
    let fixture = "**The access half is `AppContainer`, 0.26.0 built it, and nothing selects \
                   it yet.**\n";
    let err = states_the_appcontainer_selector(fixture).expect_err("must be reported");
    assert!(err.contains("nothing selects it"), "{err}");
    assert!(err.contains("with_access_confinement"), "{err}");
    assert_eq!(
        states_the_appcontainer_selector(
            "`SandboxConfig::with_access_confinement()` selects it.\n"
        ),
        Ok(())
    );
}

// ---------------------------------------------------------------------------
// The storage library is out of the contract, and the page says so
// ---------------------------------------------------------------------------

/// The two claims `docs/CONTRACT.md` carried from 0.23.0 until 0.63.0 wrapped
/// the error, taken from the **real bytes of the page** rather than from the
/// sentences as a reader hears them.
///
/// 0.61.0's F7 was blind twice in a row for exactly this reason: its first needle
/// contained a `*` its regex could never match, and its second read `"not yet
/// hold"` while the page writes `**not**`. Both of these appear literally in the
/// pre-0.63.0 file, and the control below pastes them from it.
const RETIRED_STORAGE_CLAIMS: &[&str] = &[
    "**`rusqlite` is a public dependency of this crate.**",
    "the intent is to take it out",
];

/// Does `doc_text` describe the storage error as owned, and has it stopped
/// describing the wrap as an intention?
///
/// A contract that promises a wrap the crate has already shipped is drift in the
/// direction nobody complains about — a reader is under-informed rather than
/// misinformed — which is precisely why nothing forces it into the open. 0.60.2's
/// sweep found nineteen claims of that shape in this file.
fn states_the_storage_error_is_owned(doc_text: &str) -> Result<(), String> {
    let mut wrong = Vec::new();
    for claim in RETIRED_STORAGE_CLAIMS {
        if let Some(line) = doc_text.lines().position(|l| l.contains(claim)) {
            wrong.push(format!(
                "line {}: still says \"{claim}\", which 0.63.0's wrap made false",
                line + 1
            ));
        }
    }
    for needed in ["Error::Storage", "StorageErrorKind"] {
        if !doc_text.contains(needed) {
            wrong.push(format!(
                "names no `{needed}` — that is what a caller matches now, and a contract that \
                 does not name it leaves them reaching for the deprecated variant"
            ));
        }
    }
    // The wrap bought a type-level guarantee and not a graph-level one, and
    // saying only the first would be an overclaim in this crate's own favour.
    if !doc_text.contains("links = \"sqlite3\"") {
        wrong.push(
            "does not state the `links = \"sqlite3\"` constraint that survives the wrap — a \
             consumer still cannot hold a different rusqlite than this crate does"
                .to_string(),
        );
    }
    if wrong.is_empty() {
        Ok(())
    } else {
        Err(wrong.join("; "))
    }
}

#[test]
fn contract_states_the_storage_error_is_owned() {
    if let Err(e) = states_the_storage_error_is_owned(&read("docs/CONTRACT.md")) {
        panic!(
            "docs/CONTRACT.md {e}\n\n\
             This file is what a caller may depend on. It described wrapping the storage error \
             as an intention for forty releases; shipping the wrap and leaving the page saying \
             so would be the same drift in the opposite direction.\n"
        );
    }
}

#[test]
fn storage_checker_reports_the_sentences_that_were_there() {
    // Both sentences, pasted verbatim from the pre-0.63.0 page — including the
    // `**` bold markers, which is the character class that made 0.61.0's
    // equivalent checker blind.
    let fixture = "There is a third half the snapshot cannot show, because it enumerates \
                   re-exported\n*names* and this is a *type*: **`rusqlite` is a public \
                   dependency of this crate.**\n`Error::State(#[from] rusqlite::Error)` carries \
                   that crate's own error type.\n\n\
                   **`rusqlite::Error` is in the public API, and the intent is to take it out\n\
                   (0.23.0).**\n";
    let err = states_the_storage_error_is_owned(fixture).expect_err("must be reported");
    for claim in RETIRED_STORAGE_CLAIMS {
        assert!(err.contains(claim), "the checker missed {claim:?}: {err}");
    }
    assert!(err.contains("Error::Storage"), "{err}");

    // And the replacement passes.
    let fixed = "`Error::Storage { kind, message }` carries an owned `StorageErrorKind`.\n\
                 `libsqlite3-sys` declares `links = \"sqlite3\"`, so only one version can exist.\n";
    assert_eq!(states_the_storage_error_is_owned(fixed), Ok(()));

    // A CRLF checkout finds the same two claims — the shape that made a 0.60.2
    // checker pass while checking nothing.
    let crlf = fixture.replace('\n', "\r\n");
    let err = states_the_storage_error_is_owned(&crlf.replace("\r\n", "\n"))
        .expect_err("a normalised CRLF page must report the same claims");
    assert!(err.contains(RETIRED_STORAGE_CLAIMS[0]), "{err}");
}

// ---------------------------------------------------------------------------
// The release table is a list, and stays one
// ---------------------------------------------------------------------------

/// Every version CHANGELOG.md declares a section for.
fn changelog_versions(changelog: &str) -> BTreeSet<String> {
    let heading = Regex::new(r"^##\s*\[(\d+\.\d+\.\d+)\]").unwrap();
    changelog
        .lines()
        .filter_map(|line| heading.captures(line).map(|c| c[1].to_string()))
        .collect()
}

/// Versions the release table in `index` accounts for.
///
/// A table row, so a version merely mentioned in prose does not count as
/// recorded: the claim is that the table is the list.
fn release_table_versions(index: &str) -> BTreeSet<String> {
    let row = Regex::new(r"^\|\s*\[?(\d+\.\d+\.\d+)\]?").unwrap();
    index
        .lines()
        .filter_map(|line| row.captures(line.trim()).map(|c| c[1].to_string()))
        .collect()
}

/// Does the index's release table cover every released version?
fn release_table_covers(changelog: &str, index: &str) -> Result<(), String> {
    let declared = changelog_versions(changelog);
    if declared.is_empty() {
        return Err(
            "CHANGELOG.md declares no versions at all, so this check is vacuous \
                    and the parser is wrong"
                .to_string(),
        );
    }
    let recorded = release_table_versions(index);
    let missing: Vec<&String> = declared.difference(&recorded).collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "records {} of the {} released versions; missing: {:?}",
            recorded.len(),
            declared.len(),
            missing
        ))
    }
}

#[test]
fn the_capabilities_release_table_covers_every_released_version() {
    if let Err(e) = release_table_covers(&read("CHANGELOG.md"), &read("docs/CAPABILITIES.md")) {
        panic!(
            "docs/CAPABILITIES.md {e}\n\n\
             That table is where the release-anchored facts live now that the README states \
             the present. A table that silently stops being complete is worse than no table: \
             a reader cannot tell the difference between \"this capability arrived in no \
             release\" and \"nobody updated the row\".\n"
        );
    }
}

#[test]
fn release_table_checker_reports_a_dropped_row() {
    let changelog = "## [0.2.0] - 2026-01-02\n\nstuff\n\n## [0.1.0] - 2026-01-01\n\nstuff\n";
    let complete = "| Version | What |\n| --- | --- |\n| [0.2.0](x) | b |\n| [0.1.0](x) | a |\n";
    assert_eq!(release_table_covers(changelog, complete), Ok(()));

    let dropped = "| Version | What |\n| --- | --- |\n| [0.2.0](x) | b |\n";
    let err = release_table_covers(changelog, dropped).expect_err("must be reported");
    assert!(
        err.contains("0.1.0"),
        "must name the missing version: {err}"
    );
}

#[test]
fn release_table_checker_does_not_count_a_version_named_in_prose() {
    // The failure this guards: a table that lost a row while the version is
    // still mentioned somewhere on the page would otherwise read as covered.
    let changelog = "## [0.1.0] - 2026-01-01\n";
    let prose_only = "0.1.0 was the first release.\n";
    assert!(release_table_covers(changelog, prose_only).is_err());
}

// ---------------------------------------------------------------------------
// The format list is complete, and the source is what says so
// ---------------------------------------------------------------------------

/// The media types `src/provider/mod.rs` names, read out of the source rather
/// than retyped here.
///
/// Retyping them would make this test agree with itself: a format added to the
/// crate and to neither the README nor the fixture would pass. Everything the
/// crate can accept, convert, or refuse by name appears in one of the three
/// places this parses.
fn media_types_in_source(provider_rs: &str) -> BTreeSet<String> {
    let quoted = Regex::new(r#""(image/[a-z0-9.+-]+)""#).unwrap();
    quoted
        .captures_iter(provider_rs)
        .map(|c| c[1].to_string())
        .collect()
}

/// Media types the README does not account for.
fn formats_missing_from(readme: &str, types: &BTreeSet<String>) -> Vec<String> {
    types
        .iter()
        .filter(|t| !readme.contains(t.as_str()))
        .cloned()
        .collect()
}

#[test]
fn readme_lists_every_media_type_the_crate_names() {
    let types = media_types_in_source(&read("src/provider/mod.rs"));
    assert!(
        types.len() >= 12,
        "the source parse found only {} media types, so it has stopped matching what it \
         was written to read: {types:?}",
        types.len()
    );

    let missing = formats_missing_from(&read("README.md"), &types);
    assert!(
        missing.is_empty(),
        "the README's format tables do not account for {missing:?}. Every type the crate \
         accepts, converts, or refuses by name belongs on that page — a list that omits a \
         format reads as a claim that it does not exist, and a refusal a reader could not \
         have anticipated is the worst of the three outcomes."
    );
}

#[test]
fn readme_lists_every_document_feature() {
    let readme = read("README.md");
    let documented = documented_features(&readme);
    let missing: Vec<&str> = ["xlsx", "docx", "pptx", "pdf", "barcode", "media"]
        .into_iter()
        .filter(|f| !documented.contains(*f))
        .collect();
    assert!(
        missing.is_empty(),
        "the README does not name the format features {missing:?}, so a reader cannot tell \
         which cargo feature carries the file type they came for"
    );
}

#[test]
fn format_checker_reports_a_dropped_row() {
    // Exactly the drift this guards: TIFF leaves the table while the rest stays.
    let types: BTreeSet<String> = ["image/png", "image/tiff"]
        .into_iter()
        .map(str::to_string)
        .collect();
    let readme = "| `image/png` | `.png` | passed through |\n";
    assert_eq!(formats_missing_from(readme, &types), vec!["image/tiff"]);
}

#[test]
fn format_source_parse_finds_the_accepted_the_converted_and_the_refused() {
    // The parse must reach all three sets, not just the constant. If it ever
    // matched only `IMAGE_MEDIA_TYPES`, the README could drop every converted
    // and refused format and stay green.
    let types = media_types_in_source(&read("src/provider/mod.rs"));
    for expected in [
        "image/png",               // accepted
        "image/tiff",              // converted
        "image/x-portable-anymap", // converted, and the easiest to mistype
        "image/heic",              // refused by name
    ] {
        assert!(
            types.contains(expected),
            "the source parse missed {expected}, so it is no longer reading what it claims"
        );
    }
}

/// Every marker file `src/toolchain.rs` detects, read out of the source.
///
/// The `.NET` marker is a pattern rather than a filename, so it is added
/// explicitly — it is the one entry `MARKERS` does not hold.
fn markers_in_source(toolchain_rs: &str) -> BTreeSet<String> {
    let list = Regex::new(r"(?s)const MARKERS: &\[&str\] = &\[(.*?)\];").unwrap();
    let entry = Regex::new(r#""([^"]+)""#).unwrap();
    let body = list
        .captures(toolchain_rs)
        .map(|c| c[1].to_string())
        .unwrap_or_default();
    let mut out: BTreeSet<String> = entry
        .captures_iter(&body)
        .map(|c| c[1].to_string())
        .collect();
    out.insert(".csproj".to_string());
    out
}

#[test]
fn readme_lists_every_toolchain_marker() {
    let markers = markers_in_source(&read("src/toolchain.rs"));
    assert!(
        markers.len() >= 16,
        "the MARKERS parse found only {} entries, so it has stopped reading the table it \
         was written for: {markers:?}",
        markers.len()
    );

    let readme = read("README.md");
    let missing: Vec<&String> = markers.iter().filter(|m| !readme.contains(*m)).collect();
    assert!(
        missing.is_empty(),
        "the README's toolchain table does not name {missing:?}. That table is what tells a \
         reader whether their project is one the harness already knows how to build and \
         test, and a row missing from it reads as an ecosystem that is not supported."
    );
}

#[test]
fn marker_parse_reads_the_list_and_not_the_whole_file() {
    // The guard: a parse that matched every quoted string in the file would
    // "find" hundreds of markers and pass against any README at all.
    let fixture = "const MARKERS: &[&str] = &[\n    \"Cargo.toml\",\n    \"go.mod\",\n];\n\
                   const OTHER: &str = \"not-a-marker\";\n";
    let found = markers_in_source(fixture);
    assert!(found.contains("Cargo.toml") && found.contains("go.mod"));
    assert!(
        !found.contains("not-a-marker"),
        "parsed beyond the list: {found:?}"
    );
}

// ---------------------------------------------------------------------------
// One exec boundary, stated once — F1 of 0.60.2
// ---------------------------------------------------------------------------
//
// `docs/CONTRACT.md` carried two answers to "what is a command bounded by" for
// fifteen releases. One paragraph said a command runs outside the sandbox with
// the embedding program's privileges — true up to 0.44.0 — and another, 1,300
// lines earlier, said everything a run starts is contained. Nothing told a
// reader which superseded which, and the stale one was the reassuring one.

/// The `**What a command the agent runs is bounded by.**` block of the contract.
///
/// A bold lead opens each claim in that part of the file and the next one closes
/// this block, so the window is the marker up to the next blank line followed by
/// a bold lead. Scoping the assertion to the block is the point: the retired
/// phrasing is quoted elsewhere in the file *as* retired, and a whole-file
/// search could not tell a quotation from a claim.
fn command_execution_section(contract: &str) -> String {
    const LEAD: &str = "**What a command the agent runs is bounded by.**";
    let start = contract
        .find(LEAD)
        .unwrap_or_else(|| panic!("docs/CONTRACT.md no longer opens a block with {LEAD:?}"));
    let rest = &contract[start + LEAD.len()..];
    let end = rest.find("\n\n**").unwrap_or(rest.len());
    format!("{LEAD}{}", &rest[..end])
}

/// Backticks and line breaks removed, so an assertion is about the sentence and
/// not about where the paragraph happened to wrap.
fn flatten(text: &str) -> String {
    text.replace('`', "")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn states_one_exec_boundary(section: &str) -> Result<(), String> {
    let flat = flatten(section);
    if flat.contains("outside the sandbox") {
        return Err(
            "the command-execution block still says a command runs outside the sandbox. That \
             was true up to 0.44.0; ExecMode::WorkspaceWrite has been the default since 0.45.0 \
             and docs/CONTRACT.md states 1,300 lines earlier that everything a run starts is \
             contained."
                .to_string(),
        );
    }
    if !flat.contains("ExecMode::WorkspaceWrite is the default") {
        return Err(
            "the command-execution block does not name ExecMode::WorkspaceWrite as the default. \
             A reader who reaches this block must be told today's boundary here, not left to \
             find it in the containment section."
                .to_string(),
        );
    }
    Ok(())
}

#[test]
fn contract_states_one_exec_boundary() {
    if let Err(why) =
        states_one_exec_boundary(&command_execution_section(&read("docs/CONTRACT.md")))
    {
        panic!("{why}");
    }
}

#[test]
fn exec_boundary_checker_rejects_the_pre_0_45_0_sentence() {
    let fixture = "**What a command the agent runs is bounded by.** A command runs **in the \
                   workspace root with the embedding program's privileges, outside the \
                   sandbox**.\n\n**Toolchain detection is a default.**";
    let err = states_one_exec_boundary(&command_execution_section(fixture)).unwrap_err();
    assert!(err.contains("outside the sandbox"), "{err}");
}

#[test]
fn exec_boundary_checker_rejects_silence_about_the_default() {
    // Removing the false sentence without stating the true one is not a fix:
    // the block would then say nothing at all about what contains a command.
    let fixture = "**What a command the agent runs is bounded by.** Every call is an `Act::Exec` \
                   check on the program and on the whole argv.\n\n**Toolchain detection is a \
                   default.**";
    let err = states_one_exec_boundary(&command_execution_section(fixture)).unwrap_err();
    assert!(err.contains("ExecMode::WorkspaceWrite"), "{err}");
}

#[test]
fn exec_boundary_checker_accepts_the_corrected_paragraph() {
    let fixture = "**What a command the agent runs is bounded by.** Since 0.46.0 it runs\n\
                   contained by default: `ExecMode::WorkspaceWrite` is the default\n\
                   `exec_sandbox` mode.\n\n**Toolchain detection is a default.**";
    assert!(states_one_exec_boundary(&command_execution_section(fixture)).is_ok());
}

#[test]
fn command_execution_section_stops_at_the_next_claim() {
    // The window must not run on into the rest of the file. If it did, the
    // retired phrasing quoted elsewhere as retired would fail this test.
    let section = command_execution_section(&read("docs/CONTRACT.md"));
    assert!(
        !section.contains("Toolchain detection"),
        "the window ran past the block it is scoped to:\n{section}"
    );
    assert!(
        section.contains("Act::Exec"),
        "the window did not reach the block's own body:\n{section}"
    );
}

// ---------------------------------------------------------------------------
// A rustdoc block does not carry a retired sentence — F2 of 0.60.2
// ---------------------------------------------------------------------------
//
// The same boundary in the second place a caller reads it, and the only one
// that renders on docs.rs. `TaskContract::exec_sandbox` told a caller the
// `shell_start` / `shell_poll` / `shell_kill` handles "are not contained
// because a handle outlives the call that made it" — the exact sentence
// `docs/CONTRACT.md` names as the one 0.48.0 retired. This is the first check
// in this file that reads a doc comment inside `src/` rather than a page.

/// The `///` block immediately above `item` in a Rust source file.
///
/// Walks back from the item's own line and stops at the first line that is not
/// a doc comment, so an attribute, a blank line, or the previous item ends the
/// block. A block that swallowed its neighbours would pass any assertion that
/// only looks for an absent phrase.
fn doc_block_above(source: &str, item: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let at = lines
        .iter()
        .position(|l| l.trim_start().starts_with(item))
        .unwrap_or_else(|| panic!("no line in the source opens with {item:?}"));
    let mut start = at;
    while start > 0 && lines[start - 1].trim_start().starts_with("///") {
        start -= 1;
    }
    lines[start..at]
        .iter()
        .map(|l| l.trim_start().trim_start_matches("///").trim())
        .collect::<Vec<_>>()
        .join("\n")
}

fn carries_no_retired_containment_claim(block: &str) -> Result<(), String> {
    if flatten(block).contains("not contained") {
        return Err(
            "the exec_sandbox rustdoc still says the shell handles are not contained. 0.48.0 \
             retired that sentence — docs/CONTRACT.md names it as retired at line 1222 — and a \
             handle has taken the same containment every other spawn takes ever since."
                .to_string(),
        );
    }
    Ok(())
}

#[test]
fn exec_sandbox_rustdoc_does_not_carry_the_retired_sentence() {
    let block = doc_block_above(&read("src/contract.rs"), "pub exec_sandbox:");
    if let Err(why) = carries_no_retired_containment_claim(&block) {
        panic!("{why}");
    }
    assert!(
        flatten(&block).contains("take the same containment every other spawn takes"),
        "the block no longer states what a handle does take, which is the half a caller \
         needs:\n{block}"
    );
}

#[test]
fn retired_containment_checker_rejects_the_0_47_0_clause() {
    let fixture = "/// the `shell_start` / `shell_poll` / `shell_kill`\n\
                   /// handles, which are not contained because a handle outlives the call that\n\
                   /// made it;\n";
    let err = carries_no_retired_containment_claim(fixture).unwrap_err();
    assert!(err.contains("not contained"), "{err}");
}

#[test]
fn doc_block_extractor_stops_at_the_previous_item() {
    let fixture = "    /// The first field.\n    pub first: u8,\n    /// The second field.\n    \
                   /// Two lines of it.\n    pub second: u8,\n";
    let block = doc_block_above(fixture, "pub second:");
    assert!(block.contains("The second field."), "{block}");
    assert!(
        !block.contains("The first field."),
        "the extractor swallowed the previous item's block:\n{block}"
    );
}

// ---------------------------------------------------------------------------
// The guide's reserved-name claim matches the source — F3 of 0.60.2
// ---------------------------------------------------------------------------
//
// `docs/guide/tools-and-skills.md` was stale in both directions at once. It said
// the feature-gated built-ins are *not* in the reserved set, which 0.17.0 made
// false, and its hand-typed list of reserved names held `forget`, which is not
// reserved, while omitting seven that are. A list retyped into prose is a list
// that goes stale, so the page now defers to `RESERVED_TOOL_NAMES` and this
// check exists to keep it deferring.

/// Every `pub const NAME_TOOL: &str = "…"` in the crate, ident to tool name.
fn tool_name_consts(sources: &str) -> std::collections::BTreeMap<String, String> {
    let re = Regex::new(r#"const ([A-Z0-9_]+_TOOL): &str = "([a-z0-9_]+)""#).unwrap();
    re.captures_iter(sources)
        .map(|c| (c[1].to_string(), c[2].to_string()))
        .collect()
}

/// The tool names `RESERVED_TOOL_NAMES` actually holds, resolved through the
/// constants rather than read off a list in prose.
fn reserved_tool_names(custom_rs: &str, sources: &str) -> BTreeSet<String> {
    let block = Regex::new(r"(?s)const RESERVED_TOOL_NAMES: &\[&str\] = &\[(.*?)\];").unwrap();
    let body = block
        .captures(custom_rs)
        .map(|c| c[1].to_string())
        .unwrap_or_else(|| {
            panic!("RESERVED_TOOL_NAMES is no longer a slice literal in src/tools/custom.rs")
        });
    let consts = tool_name_consts(sources);
    Regex::new(r"([A-Z0-9_]+_TOOL)")
        .unwrap()
        .captures_iter(&body)
        .filter_map(|c| consts.get(&c[1]).cloned())
        .collect()
}

/// Every `.rs` file under `src/`, concatenated. The constants are spread across
/// the tool modules and `src/run.rs`, so resolving one file is not enough.
fn rust_sources() -> String {
    fn walk(dir: &Path, out: &mut String) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push_str(&fs::read_to_string(&path).expect("read source"));
                out.push('\n');
            }
        }
    }
    let mut out = String::new();
    walk(&repo_root().join("src"), &mut out);
    out
}

/// The `**Nothing may shadow anything**` bullet of the tools guide.
fn shadowing_bullet(guide: &str) -> String {
    const LEAD: &str = "- **Nothing may shadow anything**";
    let start = guide
        .find(LEAD)
        .unwrap_or_else(|| panic!("docs/guide/tools-and-skills.md no longer carries {LEAD:?}"));
    let rest = &guide[start + LEAD.len()..];
    let end = rest.find("\n- **").unwrap_or(rest.len());
    format!("{LEAD}{}", &rest[..end])
}

fn guide_defers_to_the_reserved_set(
    bullet: &str,
    reserved: &BTreeSet<String>,
) -> Result<(), String> {
    if bullet.contains("are *not* in the reserved set") {
        return Err(
            "the guide still says the feature-gated built-ins are not in the reserved set. 0.17.0 \
             put them in it, under `### Breaking changes`."
                .to_string(),
        );
    }
    // 0.61.0. The page carried "It does **not** yet hold every name dispatch
    // answers" for two releases, and every check below let it through: the
    // sentence is not 0.17.0's, and the names it cited — `browser_*`, `lsp_*` —
    // end in a `*` the identifier regex never matches. A claim that the set is
    // incomplete is the same class of stale as a hand-typed list of it, so it is
    // refused by shape rather than by the names it happens to cite.
    // The needles skip the word `not` on purpose: the page wrote it as `**not**`,
    // so a needle spanning it matches nothing — which is how the first version of
    // this check passed against the very paragraph it was written to catch.
    for claim in ["yet hold", "and not reserved", "dispatched and not"] {
        if bullet.contains(claim) {
            return Err(format!(
                "the guide still says the reserved set is incomplete ({claim:?}). 0.61.0 closed \
                 that gap and derives the set from the crate's own tool constants; a page that \
                 tells a reader to avoid naming a tool after a built-in *because the check does \
                 not catch it* is describing a release that has shipped."
            ));
        }
    }
    if !bullet.contains("RESERVED_TOOL_NAMES") {
        return Err(
            "the guide no longer defers to RESERVED_TOOL_NAMES. Naming the set is what keeps this \
             page from carrying a second copy of it that nothing checks."
                .to_string(),
        );
    }
    // A backticked lowercase identifier in this bullet reads as a reserved name.
    // A trailing underscore is a prefix (`mcp__`) and a `*` never matches, so
    // the two things the page names that are not tool names are not caught here.
    let named = Regex::new(r"`([a-z][a-z0-9_]*)`").unwrap();
    let wrong: Vec<String> = named
        .captures_iter(bullet)
        .map(|c| c[1].to_string())
        .filter(|n| !n.ends_with('_') && !reserved.contains(n))
        .collect();
    if !wrong.is_empty() {
        return Err(format!(
            "the guide names {wrong:?} as reserved and RESERVED_TOOL_NAMES does not hold them. \
             This page had `forget` in its list for four releases; do not restate the set here, \
             refer to it."
        ));
    }
    Ok(())
}

#[test]
fn the_guide_reserved_name_claim_matches_the_source() {
    let reserved = reserved_tool_names(&read("src/tools/custom.rs"), &rust_sources());
    assert!(
        reserved.len() >= 30,
        "the reserved-set parse resolved only {} names, so it has stopped reading what it was \
         written for: {reserved:?}",
        reserved.len()
    );
    let bullet = shadowing_bullet(&read("docs/guide/tools-and-skills.md"));
    if let Err(why) = guide_defers_to_the_reserved_set(&bullet, &reserved) {
        panic!("{why}");
    }
}

#[test]
fn reserved_set_parse_resolves_names_and_not_idents() {
    let reserved = reserved_tool_names(&read("src/tools/custom.rs"), &rust_sources());
    assert!(reserved.contains("write_file"), "{reserved:?}");
    assert!(reserved.contains("view_image"), "{reserved:?}");
    // 0.61.0 reserved `browser_click`, so this control can no longer be "the set
    // does not hold it". What it still has to prove is that the parse resolves
    // each entry through its constant rather than reporting the identifier it
    // read, which is what a `contains` on the slice's raw text would do.
    assert!(reserved.contains("browser_click"), "{reserved:?}");
    assert!(
        !reserved.contains("BROWSER_CLICK_TOOL"),
        "the parse is reporting identifiers rather than resolving them to tool names: {reserved:?}"
    );
}

#[test]
fn guide_checker_rejects_the_claim_0_17_0_made_false() {
    let fixture = "- **Nothing may shadow anything** — RESERVED_TOOL_NAMES holds them. The \
                   feature-gated built-ins are *not* in the reserved set.\n- **Next bullet**";
    let reserved = BTreeSet::from(["write_file".to_string()]);
    let err = guide_defers_to_the_reserved_set(&shadowing_bullet(fixture), &reserved).unwrap_err();
    assert!(err.contains("not in the reserved set"), "{err}");
}

/// The 0.61.0 sibling: the page may not tell a reader the set is incomplete.
///
/// This fixture is the paragraph the guide actually carried through 0.60.2, and
/// every other check in this file passes on it — which is why the check exists.
#[test]
fn guide_checker_rejects_a_claim_that_the_set_is_incomplete() {
    let fixture = "- **Nothing may shadow anything** — the reserved set is `RESERVED_TOOL_NAMES` \
                   in `src/tools/custom.rs`. It does **not** yet hold every name dispatch \
                   answers: the `browser_*` and `lsp_*` tools are among eighteen that are \
                   dispatched and not reserved.\n- **Next bullet**";
    let reserved = BTreeSet::from(["write_file".to_string()]);
    let err = guide_defers_to_the_reserved_set(&shadowing_bullet(fixture), &reserved).unwrap_err();
    assert!(err.contains("incomplete"), "{err}");
}

#[test]
fn guide_checker_rejects_a_reinstated_hand_list() {
    // The sabotage the contract names: `forget` back in the guide's list.
    let fixture = "- **Nothing may shadow anything** — see RESERVED_TOOL_NAMES: `write_file`, \
                   `forget`, `mcp__`.\n- **Next bullet**";
    let reserved = BTreeSet::from(["write_file".to_string()]);
    let err = guide_defers_to_the_reserved_set(&shadowing_bullet(fixture), &reserved).unwrap_err();
    assert!(err.contains("forget"), "{err}");
    assert!(!err.contains("mcp__"), "a prefix is not a name: {err}");
}

#[test]
fn guide_checker_accepts_a_bullet_that_defers() {
    let fixture = "- **Nothing may shadow anything** — the reserved set is `RESERVED_TOOL_NAMES` \
                   in `src/tools/custom.rs`, and the `browser_*` tools are not in it.\n\
                   - **Next bullet**";
    let reserved = BTreeSet::from(["write_file".to_string()]);
    assert!(guide_defers_to_the_reserved_set(&shadowing_bullet(fixture), &reserved).is_ok());
}

#[test]
fn the_section_window_survives_a_crlf_checkout() {
    // The regression the Windows leg found. Both windowing helpers look for a
    // blank line followed by a marker, which CRLF spells differently; `read`
    // normalises, and this asserts the helpers work on the shape it produces
    // even if someone hands them a raw CRLF string directly.
    let crlf = "**What a command the agent runs is bounded by.** `Act::Exec` on the argv.\r\n\
                \r\n**Toolchain detection is a default.**";
    let section = command_execution_section(&crlf.replace("\r\n", "\n"));
    assert!(
        !section.contains("Toolchain detection"),
        "the window ran past its block on a normalised CRLF page:\n{section}"
    );

    let bullet_crlf = "- **Nothing may shadow anything** — see `RESERVED_TOOL_NAMES`.\r\n\
                       - **A failing tool is an observation**";
    let bullet = shadowing_bullet(&bullet_crlf.replace("\r\n", "\n"));
    assert!(
        !bullet.contains("A failing tool"),
        "the bullet window ran past its own bullet:\n{bullet}"
    );
}

// ---------------------------------------------------------------------------
// The contract's ownership claims match the source — F7 of 0.62.0
// ---------------------------------------------------------------------------
//
// `docs/CONTRACT.md` stated the defect 0.62.0 closes as a standing property of
// the crate: "a run that is genuinely live is not detected either: `resume_*`
// will refuse a request that has already been decided, but it will not refuse
// one that is still being held by a process that is still running". That is now
// false, and a page that under-promises goes uncorrected for exactly as long as
// nobody is annoyed by it.
//
// **The needles below are copied from the real bytes of the page and of the
// source, never from the sentence as a reader hears it.** 0.61.0's equivalent
// gate was blind twice in a row: first because its identifier regex could not
// match the `*` in `browser_*`, then because its needle read `"not yet hold"`
// while the page writes `**not**` in the middle of the phrase. A checker that
// searches for a sentence nobody typed passes for the wrong reason.

/// The retired claim, exactly as `docs/CONTRACT.md` carried it before 0.62.0.
/// Split at the line break the page had, so a re-wrap cannot smuggle it back in
/// under a different shape — `flatten` collapses whitespace before the search.
const RETIRED_LIVE_OWNER_CLAIM: &str =
    "it will not refuse one that is still being held by a process that is still running";

/// Whether a page still tells a reader that a live owner goes undetected.
fn carries_no_undetected_live_owner_claim(page: &str) -> Result<(), String> {
    let flat = flatten(page);
    if flat.contains(RETIRED_LIVE_OWNER_CLAIM) {
        return Err(format!(
            "the page still says a live owner is not detected, which 0.62.0's lease made false: \
             {RETIRED_LIVE_OWNER_CLAIM}"
        ));
    }
    Ok(())
}

#[test]
fn the_contract_no_longer_says_a_live_owner_goes_undetected() {
    if let Err(why) = carries_no_undetected_live_owner_claim(&read("docs/CONTRACT.md")) {
        panic!("{why}");
    }
}

#[test]
fn the_undetected_live_owner_checker_rejects_the_retired_sentence() {
    // The control. Without it the test above passes against a page that says
    // nothing at all about ownership, which is how a prose gate goes quietly
    // blind — and this is the arm the sabotage pass restores.
    let fixture = "**The crate does not know whether the owner is alive.** A run that is\n\
                   genuinely live is not detected either: `resume_*` will refuse a request that\n\
                   has already been decided, but it will not refuse one that is still being held\n\
                   by a process that is still running.\n";
    let err = carries_no_undetected_live_owner_claim(fixture).unwrap_err();
    assert!(err.contains("still running"), "{err}");
}

#[test]
fn the_contracts_ownership_claims_match_the_source() {
    let page = flatten(&read("docs/CONTRACT.md"));
    // The step commit moved to `src/state/trace.rs` in 0.62.0's split. Named
    // explicitly rather than searched for: a checker that hunts for its subject
    // across files is a checker that will one day find nothing and say nothing.
    let commit_home = read("src/state/trace.rs");
    let run = read("src/run.rs");

    // Claim: every `run_*` / `resume_*` takes a lease. **Derived, not counted.** A
    // count of acquire sites is satisfied by six acquires in one function, and this
    // release has already had one deleted from a path no test covers while the
    // suite stayed green. The invariant is per function: every function that starts
    // a run or checks one is resumable — which is every place the crate begins
    // driving — takes a lease in the same body.
    let mut missing = Vec::new();
    for body in run
        .split("\npub async fn ")
        .skip(1)
        .chain(run.split("\npub(crate) async fn ").skip(1))
    {
        let name = body
            .split(['<', '('])
            .next()
            .unwrap_or("?")
            .trim()
            .to_string();
        let body = body.split("\npub").next().unwrap_or(body);
        let drives = body.contains("store.check_resumable(") || body.contains("store.start_run(");
        if drives && !body.contains("store.acquire_lease(") {
            missing.push(name);
        }
    }
    assert!(
        missing.is_empty(),
        "every entry point that starts or resumes a run takes a lease, and these do not: {missing:?}"
    );
    // The floor: if the parse stops finding entry points at all, the assertion
    // above passes by finding nothing, which is how a derived test goes blind.
    let drivers = run
        .split("\npub async fn ")
        .skip(1)
        .filter(|b| {
            let b = b.split("\npub").next().unwrap_or(b);
            b.contains("store.check_resumable(") || b.contains("store.start_run(")
        })
        .count();
    assert!(
        drivers >= 3,
        "the parse found only {drivers} driving entry points in src/run.rs, so it is \
         checking almost nothing"
    );
    assert!(
        page.contains("takes a lease on the run it is about to drive"),
        "the contract no longer states that a driver takes a lease"
    );

    // Claim: the generation is verified inside the transaction that writes the
    // step. Asserted against the order of the real statements: the lease check
    // must appear after the transaction is opened and before the step insert.
    let commit = commit_home
        .split_once("pub fn checkpoint_step(")
        .expect("checkpoint_step is defined in src/state/trace.rs")
        .1;
    let tx = commit
        .find("unchecked_transaction()")
        .expect("the transaction");
    let check = commit
        .find("SELECT generation FROM run_leases")
        .expect("the generation check");
    let insert = commit.find("INSERT INTO steps").expect("the step insert");
    assert!(
        tx < check && check < insert,
        "the generation check must sit inside the transaction and before the step insert \
         (tx {tx}, check {check}, insert {insert})"
    );
    assert!(
        page.contains("verified inside the transaction that would have written them")
            || page.contains("inside the transaction"),
        "the contract no longer states where the generation is verified"
    );

    // Claim: the session head advances by compare-and-swap. Both in-crate callers
    // go through it, and the unconditional writer is not one of them.
    let session = read("src/session.rs");
    assert_eq!(
        session.matches("set_session_head_if(").count(),
        2,
        "both session-head advances must go through the compare-and-swap"
    );
    assert!(
        !session.contains("store.set_session_head("),
        "a session-head advance is still using the unconditional write"
    );
    assert!(
        page.contains("compare-and-swap"),
        "the contract no longer states that a session head advances by compare-and-swap"
    );
}
