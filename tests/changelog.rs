//! The changelog's breaking-change contract, enforced.
//!
//! This crate is pre-1.0 and stays pre-1.0, so a break ships as a minor bump.
//! What a consumer can rely on is therefore not "a minor never breaks" but "when
//! it breaks, the changelog says so and says what to write instead". That claim
//! decays the moment someone marks an entry `**BREAKING` and forgets the note,
//! so it is checked rather than promised.
//!
//! The rule: every entry in `CHANGELOG.md` carrying the marker must also carry a
//! migration note. See `docs/CHANGELOG_STRUCTURE.md` for the format the marker
//! and the note take.
//!
//! The parse is a pure function over the file's text, so the negative control —
//! a fixture entry that is marked and has no note — is an ordinary unit test
//! rather than a temporary edit to the real file. A checker that silently matches
//! nothing passes every input and is worse than no checker, because it reports a
//! green claim; `the_marker_is_actually_found_in_the_real_file` is the guard
//! against that.

use std::path::PathBuf;

/// The breaking-change marker. Matched as a prefix, so the qualified forms —
/// `**BREAKING (behaviour)**`, `**BREAKING (MSRV)**`, `**BREAKING (trace)**` —
/// are found by the same token as the bare one.
const MARKER: &str = "**BREAKING";

/// What a marked entry must also carry. Rendered `*Migration:*` in the file; the
/// emphasis is cosmetic and deliberately not part of the token.
const NOTE: &str = "Migration:";

/// One changelog entry: the version whose section it sits in, and its text.
///
/// An entry runs from a top-level bullet or a heading to the next one. Lines
/// inside a fenced code block are never boundaries, so a `- ` or `#` in a Rust or
/// TOML snippet does not split the entry that contains it.
fn entries(changelog: &str) -> Vec<(String, String)> {
    let lines: Vec<&str> = changelog.lines().collect();

    let mut fenced = vec![false; lines.len()];
    let mut open = false;
    for (i, line) in lines.iter().enumerate() {
        if line.trim_start().starts_with("```") {
            open = !open;
            fenced[i] = true; // a fence line is never a boundary either
        } else {
            fenced[i] = open;
        }
    }

    let starts: Vec<usize> = (0..lines.len())
        .filter(|&i| !fenced[i] && (lines[i].starts_with("- ") || lines[i].starts_with('#')))
        .collect();

    let mut out = Vec::new();
    let mut version = "(before the first version heading)".to_string();
    for (n, &start) in starts.iter().enumerate() {
        if let Some(v) = version_of(lines[start]) {
            version = v;
        }
        let end = starts.get(n + 1).copied().unwrap_or(lines.len());
        out.push((version.clone(), lines[start..end].join("\n")));
    }
    out
}

/// `## [0.9.1] - 2026-07-26` -> `0.9.1`.
fn version_of(line: &str) -> Option<String> {
    let rest = line.strip_prefix("## [")?;
    let v = rest.split(']').next()?;
    v.chars()
        .next()
        .filter(char::is_ascii_digit)
        .map(|_| v.to_string())
}

/// Every entry that claims a break and does not say what to write instead,
/// reported as `version — first line`.
///
/// This is the whole check, as a pure function over the file's text: the
/// integration test feeds it `CHANGELOG.md` and the unit tests feed it fixtures.
fn breaks_without_migration(changelog: &str) -> Vec<String> {
    entries(changelog)
        .into_iter()
        .filter(|(_, text)| text.contains(MARKER) && !text.contains(NOTE))
        .map(|(version, text)| {
            let first = text.lines().next().unwrap_or_default().trim();
            format!("{version} — {first}")
        })
        .collect()
}

/// How many entries claim a break at all. Only used to prove the parser sees the
/// real file's markers.
fn marked(changelog: &str) -> usize {
    entries(changelog)
        .iter()
        .filter(|(_, text)| text.contains(MARKER))
        .count()
}

fn changelog() -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("CHANGELOG.md");
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn every_breaking_entry_carries_a_migration_note() {
    let offenders = breaks_without_migration(&changelog());
    assert!(
        offenders.is_empty(),
        "{} changelog entr{} marked `{MARKER}` with no `{NOTE}` note. \
         A break without a migration note is a promise this crate does not keep — \
         say what to write instead, old call on one side and new call on the other:\n  {}",
        offenders.len(),
        if offenders.len() == 1 { "y is" } else { "ies are" },
        offenders.join("\n  ")
    );
}

/// The negative control on the *file* half: a parser that matched nothing would
/// pass the test above on any input at all, including an empty file.
#[test]
fn the_marker_is_actually_found_in_the_real_file() {
    let found = marked(&changelog());
    assert!(
        found >= 20,
        "the parser found only {found} marked entries in CHANGELOG.md; \
         the 0.2.0-0.15.0 audit marked far more than that, so the parse is broken \
         rather than the file being clean"
    );
}

/// The negative control the release contract names: an entry that is marked
/// breaking and carries no note must be reported, by version.
#[test]
fn a_marked_entry_with_no_note_is_reported() {
    let fixture = "\
## [9.9.9] - 2026-01-01

### Breaking changes

- **BREAKING** — `Thing::old` is removed. The API changed.

- **BREAKING (behaviour)** — the default flipped. *Migration:* pass
  `Thing::with_old_default()` to keep 9.9.8's behaviour.

### Added

- Something harmless.
";

    let offenders = breaks_without_migration(fixture);
    assert_eq!(
        offenders,
        vec!["9.9.9 — - **BREAKING** — `Thing::old` is removed. The API changed.".to_string()],
        "the unmigrated entry must be reported, and the migrated one must not"
    );
}

/// The other half of the control: nothing marked, nothing reported — the checker
/// must not fire on an ordinary entry that happens to discuss a change.
#[test]
fn an_unmarked_entry_is_never_reported() {
    let fixture = "\
## [9.9.9] - 2026-01-01

### Changed

- `Thing::new` now takes a `&str`. This is a breaking change in every sense
  except the marker, which is not present.
";

    assert!(breaks_without_migration(fixture).is_empty());
    assert_eq!(marked(fixture), 0);
}

/// A `- ` inside a fenced snippet is part of the entry that opened the fence, not
/// a new entry — otherwise a marked entry could be split away from its own note.
#[test]
fn a_fenced_snippet_does_not_split_an_entry() {
    let fixture = "\
## [9.9.9] - 2026-01-01

### Breaking changes

- **BREAKING** — `Thing::old` is removed.

  ```toml
  # before
  - old = 1
  ```

  *Migration:* write `Thing::new` instead.
";

    assert_eq!(marked(fixture), 1);
    assert!(breaks_without_migration(fixture).is_empty());
}
