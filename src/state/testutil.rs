//! Fixtures the split state modules share (0.62.0).
//!
//! Two helpers outlived the single test module the split broke up: one reads the
//! crate's own source for a set of enum variants, the other fills a memory
//! workspace to its cap. Both are used from more than one subject, and a fixture
//! copied into two modules is a fixture that will disagree with itself.

use super::*;

/// The variants declared by `pub enum MemoryKind` in this file, lowercased the
/// way [`MemoryKind::as_str`] spells them.
///
/// A text parse, safe because of the enum's shape: a variant sits at four
/// spaces and starts with an uppercase letter, where a doc line starts with
/// `/` and an attribute with `#`. Line endings are normalised first — a
/// Windows checkout holds this file with CRLF, and a parse looking for `"\n}"`
/// would find nothing there and fail on one platform only.
pub(super) fn variants_in_source() -> Vec<&'static str> {
    let src = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/state.rs"),
    )
    .expect("this file is readable from its own test")
    .replace("\r\n", "\n");
    let body = src
        .split_once("pub enum MemoryKind {")
        .expect("the enum is declared in this file")
        .1;
    let body = body.split_once("\n}\n").expect("the enum is closed").0;

    let mut found = Vec::new();
    for line in body.lines() {
        let Some(rest) = line.strip_prefix("    ") else {
            continue;
        };
        if !rest.starts_with(|c: char| c.is_ascii_uppercase()) {
            continue;
        }
        let variant: String = rest
            .chars()
            .take_while(char::is_ascii_alphanumeric)
            .collect();
        // Leaked rather than returned as `String`, so the comparison above is
        // against `&'static str` like the constant it is checking. One leak
        // per variant per test process is nothing.
        found.push(&*Box::leak(variant.to_ascii_lowercase().into_boxed_str()));
    }
    assert!(
        !found.is_empty(),
        "the parse found nothing, so it is measuring itself rather than the enum"
    );
    found
}
/// Fill a workspace to the entry cap, oldest first. `k0` is the oldest.
pub(super) fn fill_to_the_cap(store: &Store, workspace: &str) {
    for i in 0..MEMORY_MAX_ENTRIES {
        store
            .memory_put(workspace, &format!("k{i}"), "v", 1, 1)
            .unwrap();
    }
}
