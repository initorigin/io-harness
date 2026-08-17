//! The storage library stops being part of the published contract.
//!
//! `Error::State(#[from] rusqlite::Error)` put a third-party error type in this
//! crate's public API. A consumer who wanted to know whether a failure was a busy
//! database had to depend on `rusqlite` at a matching version to ask, and a
//! `rusqlite` major upgrade here became a breaking change for them — one they did
//! not ask for and could not avoid.
//!
//! `Error::Storage { kind, message }` is the replacement: an owned
//! classification a caller branches on and the message the storage layer
//! produced, neither of which requires naming `rusqlite`. `Error::State` keeps
//! existing for its deprecation cycle, and **F4's derived check is what makes the
//! claim structural** — the snapshot in `docs/public-api.txt` does not descend
//! into variants (`tests/public_api.rs`), so nothing there could have caught the
//! leak in the first place and nothing there would catch it coming back.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use io_harness::{Error, StorageErrorKind};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The one public item still permitted to name `rusqlite`, for the length of its
/// deprecation cycle. Removing the variant removes this entry with it.
const DEPRECATED_EXCEPTION: &str = "State";

// ---------------------------------------------------------------------------
// F4 — rusqlite is gone from the public contract
// ---------------------------------------------------------------------------

/// Every `.rs` file under `src/`.
fn sources() -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&PathBuf::from(ROOT).join("src"), &mut out);
    out.sort();
    assert!(
        out.len() >= 30,
        "only {} source files found under src/ — this walk has gone blind and every \
         assertion made over it is vacuous",
        out.len()
    );
    out
}

/// Every line of `text` whose public surface names `rusqlite`, as `(line, source)`.
///
/// A line is public surface if it declares a `pub` item or is a field or variant
/// inside one. Deliberately a parse of the declaration lines rather than of the
/// whole file: `rusqlite` is used everywhere inside function bodies in
/// `src/state/`, and that is exactly the difference between a dependency and a
/// published interface.
fn storage_leaks(text: &str) -> Vec<(usize, String)> {
    let text = text.replace("\r\n", "\n");
    let mut leaks = Vec::new();
    let mut in_public_type = false;
    // An attribute may span lines — `#[deprecated(since = "...", note = "...")]`
    // does — and its continuation lines are prose, not surface. Without this the
    // checker reads the word `rusqlite` inside a deprecation note as a leak,
    // which is how it first failed.
    let mut attribute_depth = 0i32;
    for (n, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();

        let opening_attribute = attribute_depth == 0 && trimmed.starts_with("#[");
        if opening_attribute || attribute_depth > 0 {
            attribute_depth += line.matches('[').count() as i32;
            attribute_depth -= line.matches(']').count() as i32;
            attribute_depth = attribute_depth.max(0);
            continue;
        }

        // A `pub struct` / `pub enum` at column zero opens a block whose fields
        // and variants are public surface; the closing `}` shuts it.
        if indent == 0 {
            if trimmed.starts_with("pub struct ")
                || trimmed.starts_with("pub enum ")
                || trimmed.starts_with("pub trait ")
            {
                in_public_type = true;
            } else if trimmed == "}" {
                in_public_type = false;
            }
        }

        let is_declaration = trimmed.starts_with("pub fn ")
            || trimmed.starts_with("pub async fn ")
            || trimmed.starts_with("pub const ")
            || trimmed.starts_with("pub type ")
            || trimmed.starts_with("pub use ");
        // Inside a public type, a field (`pub name: T` or `name: T` on a
        // variant) or a variant with a payload is surface too.
        let is_member = in_public_type && indent > 0 && !trimmed.starts_with("//");

        if !(is_declaration || is_member) || !line.contains("rusqlite") {
            continue;
        }
        if line.contains(DEPRECATED_EXCEPTION) {
            continue;
        }
        leaks.push((n + 1, trimmed.to_string()));
    }
    leaks
}

/// No `pub` item's surface names `rusqlite`, anywhere under `src/`.
#[test]
fn no_public_surface_names_the_storage_library() {
    let mut leaks = Vec::new();
    for path in sources() {
        let text = fs::read_to_string(&path).unwrap();
        for (line, source) in storage_leaks(&text) {
            leaks.push(format!(
                "{}:{line}: {source}",
                path.strip_prefix(ROOT).unwrap().display()
            ));
        }
    }
    assert!(
        leaks.is_empty(),
        "the storage library is back in the public surface:\n  {}\n\n\
         A consumer must not have to depend on this crate's storage library at a matching \
         version to read one of its errors. Wrap it — `Error::Storage` is the shape.",
        leaks.join("\n  ")
    );
}

/// The checker is proven capable of failing, on every shape of leak it claims to
/// cover — and proven not to fire on the shapes that are not leaks.
///
/// A green surface check whose parse cannot see a leak is worse than no check:
/// it is a claim about the public contract resting on nothing. This is the
/// control that makes the test above evidence.
#[test]
fn the_surface_checker_sees_a_leak_and_is_not_fooled_by_prose() {
    let leaking = "\
pub fn open(path: &str) -> rusqlite::Connection {
    todo!()
}

pub struct Handle {
    pub conn: rusqlite::Connection,
}

pub enum Failure {
    Bad(rusqlite::Error),
}
";
    let found = storage_leaks(leaking);
    assert_eq!(
        found.len(),
        3,
        "a public fn's return type, a public field and a variant payload are all leaks: {found:?}"
    );

    let clean = "\
// rusqlite is fine in a comment
/// and fine in a doc comment: rusqlite
fn private_helper() -> rusqlite::Result<()> {
    let _ = rusqlite::Error::QueryReturnedNoRows;
    Ok(())
}

pub struct Owned {
    pub message: String,
}
";
    assert!(
        storage_leaks(clean).is_empty(),
        "a private helper, a comment and a doc comment are not public surface: {:?}",
        storage_leaks(clean)
    );

    // A multi-line attribute whose prose mentions the library is not a leak —
    // the exact shape that made this checker fail the first time it ran.
    let attribute = "\
pub enum Failure {
    #[deprecated(
        since = \"0.63.0\",
        note = \"this note mentions rusqlite and is not public surface\"
    )]
    Old(String),
}
";
    assert!(
        storage_leaks(attribute).is_empty(),
        "a deprecation note is prose: {:?}",
        storage_leaks(attribute)
    );

    // And a CRLF checkout parses identically.
    assert_eq!(
        storage_leaks(&leaking.replace('\n', "\r\n")).len(),
        3,
        "a CRLF checkout must find the same three leaks"
    );
}

/// The exception itself is real: the deprecated variant exists and still names
/// `rusqlite`, so the check above is excluding something rather than nothing.
///
/// A control, and it is the reason the exclusion list is one name rather than a
/// silent `contains` that would also skip a real leak on the same line. When the
/// cycle ends and `Error::State` is removed, this test is what fails and says so.
#[test]
fn the_deprecated_variant_is_the_only_exception_and_it_is_still_there() {
    let text = fs::read_to_string(PathBuf::from(ROOT).join("src/error.rs"))
        .unwrap()
        .replace("\r\n", "\n");
    assert!(
        text.contains("State(rusqlite::Error)"),
        "Error::State no longer names rusqlite — if the deprecation cycle has ended, remove \
         DEPRECATED_EXCEPTION from this file rather than leaving a permanent hole in the check"
    );
    assert!(
        !text.contains("State(#[from] rusqlite::Error)"),
        "Error::State still carries #[from], so every `?` in src/state/ is still building the \
         deprecated variant and Error::Storage is unreachable"
    );
}

// ---------------------------------------------------------------------------
// F4 — the classification, and the message kept whole
// ---------------------------------------------------------------------------

/// A busy database is classified as busy, and the underlying message survives.
///
/// Asserted through the real conversion — `?` on a `rusqlite::Result` inside a
/// function returning this crate's `Result` — because that is the path all
/// roughly 360 storage call sites take, and a test that constructs the variant by
/// hand would prove nothing about them.
#[test]
fn a_storage_failure_is_classified_and_its_message_is_kept_whole() {
    let cases: &[(rusqlite::ffi::ErrorCode, StorageErrorKind)] = &[
        (
            rusqlite::ffi::ErrorCode::DatabaseBusy,
            StorageErrorKind::Busy,
        ),
        (
            rusqlite::ffi::ErrorCode::DatabaseLocked,
            StorageErrorKind::Busy,
        ),
        (
            rusqlite::ffi::ErrorCode::ConstraintViolation,
            StorageErrorKind::Constraint,
        ),
        (
            rusqlite::ffi::ErrorCode::DatabaseCorrupt,
            StorageErrorKind::Corrupt,
        ),
        (
            rusqlite::ffi::ErrorCode::NotADatabase,
            StorageErrorKind::Corrupt,
        ),
        (
            rusqlite::ffi::ErrorCode::OperationAborted,
            StorageErrorKind::Other,
        ),
    ];

    for (code, expected) in cases {
        let raw = rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error {
                code: *code,
                extended_code: 0,
            },
            Some("the storage layer said this".into()),
        );
        let underlying = raw.to_string();
        let failure: Error = raw.into();

        let Error::Storage { kind, message } = &failure else {
            panic!("a rusqlite failure must convert to Error::Storage, got {failure:?}");
        };
        assert_eq!(kind, expected, "{code:?} classified wrongly");
        assert_eq!(
            message, &underlying,
            "the storage layer's own message must be kept whole, not summarised away"
        );

        // Display carries both: the classification a caller branches on, and the
        // message an operator reads. An error type's Display is the part
        // consumers actually depend on and the part no compiler protects.
        let shown = failure.to_string();
        assert!(
            shown.contains(&kind.to_string()),
            "Display must name the kind: {shown}"
        );
        assert!(
            shown.contains("the storage layer said this"),
            "Display must carry the underlying message: {shown}"
        );
    }
}

/// A non-`SqliteFailure` storage error still converts, and lands in `Other`
/// rather than being classified as something it is not.
#[test]
fn an_unclassifiable_storage_failure_is_other_and_not_guessed_at() {
    let raw = rusqlite::Error::QueryReturnedNoRows;
    let underlying = raw.to_string();
    let failure: Error = raw.into();
    assert!(matches!(
        failure,
        Error::Storage {
            kind: StorageErrorKind::Other,
            ..
        }
    ));
    assert!(failure.to_string().contains(&underlying));
}

/// Only `Busy` says another attempt could work.
///
/// The whole point of the classification is that a caller can act on it, and the
/// only action a storage failure affords is "try again" or "do not". A
/// classification where everything is retryable is the same as no classification.
#[test]
fn only_a_busy_store_is_worth_another_attempt() {
    assert!(StorageErrorKind::Busy.is_retryable());
    for kind in [
        StorageErrorKind::Constraint,
        StorageErrorKind::Corrupt,
        StorageErrorKind::Other,
    ] {
        assert!(
            !kind.is_retryable(),
            "{kind} must not invite a retry that cannot work"
        );
    }
    let names: BTreeSet<String> = [
        StorageErrorKind::Busy,
        StorageErrorKind::Constraint,
        StorageErrorKind::Corrupt,
        StorageErrorKind::Other,
    ]
    .iter()
    .map(|k| k.to_string())
    .collect();
    assert_eq!(names.len(), 4, "every kind renders as a distinct word");
}

// ---------------------------------------------------------------------------
// F5 — the deprecation is a warning, not a break
// ---------------------------------------------------------------------------

/// The deprecation attribute names its replacement as code and names the version
/// that removes it, which is the house style the 0.17.0 `Verification` cycle set.
///
/// A `#[deprecated]` whose note says "use the new one" tells a caller nothing
/// they can paste, and one with no removal version tells them nothing about how
/// long they have.
#[test]
fn the_deprecation_names_its_replacement_and_its_removal() {
    let text = fs::read_to_string(PathBuf::from(ROOT).join("src/error.rs"))
        .unwrap()
        .replace("\r\n", "\n");
    let at = text
        .find("#[deprecated(")
        .expect("Error::State carries no #[deprecated] attribute at all");
    let end = at + text[at..].find(")]").expect("unterminated attribute");
    let attribute = &text[at..end];

    for needle in [
        "since = \"0.63.0\"",
        "Error::Storage",
        "kind",
        "message",
        "Removed in 0.65.0.",
    ] {
        assert!(
            attribute.contains(needle),
            "the deprecation note must contain {needle:?}, so a caller reading the warning can \
             paste the replacement and knows how long they have:\n{attribute}"
        );
    }
}

/// The deprecated variant still exists and still carries what it carried, so a
/// caller matching it keeps compiling for the whole cycle.
///
/// `#[allow(deprecated)]` here rather than at the crate level: this test is the
/// one place that is *supposed* to name it, and allowing it everywhere would hide
/// a real internal use.
#[test]
#[allow(deprecated)]
fn the_deprecated_variant_still_works_for_a_caller_who_matches_it() {
    let inner = rusqlite::Error::QueryReturnedNoRows;
    let shown = inner.to_string();
    let failure = Error::State(inner);
    assert!(
        failure.to_string().contains(&shown),
        "the deprecated variant must still render what it always rendered"
    );
    match failure {
        Error::State(_) => {}
        other => panic!("Error::State must still be matchable: {other:?}"),
    }
}

/// Nothing in `src/` constructs the deprecated variant, so the deprecation
/// warning cannot fire inside this crate on any build.
///
/// The audit that preceded this release found zero construction sites and zero
/// matches; this is that finding turned into a gate, because the cheapest way for
/// the cycle to go wrong is a new `Error::State(...)` written by someone who has
/// not read the attribute.
#[test]
fn nothing_in_the_crate_builds_the_deprecated_variant() {
    let mut sites = Vec::new();
    for path in sources() {
        let text = fs::read_to_string(&path).unwrap().replace("\r\n", "\n");
        for (n, line) in text.lines().enumerate() {
            let trimmed = line.trim_start();
            // The declaration itself, its doc comment and its attribute are not
            // construction sites.
            if trimmed.starts_with("//") || trimmed.starts_with("#[") {
                continue;
            }
            if line.contains("Error::State(") || line.contains("Self::State(") {
                sites.push(format!(
                    "{}:{}: {trimmed}",
                    path.strip_prefix(ROOT).unwrap().display(),
                    n + 1
                ));
            }
        }
    }
    assert!(
        sites.is_empty(),
        "the crate builds its own deprecated variant:\n  {}\n\n\
         Every storage failure converts to Error::Storage through `?`. A hand-built \
         Error::State hands a consumer the very type this release removed from the contract.",
        sites.join("\n  ")
    );
}
