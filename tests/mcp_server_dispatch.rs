//! The 0.78.0 visibility promotions, checked from outside the crate.
//!
//! Serving `tools/call` routes it through the run loop's own `dispatch`, and
//! `src/mcp_server.rs` is a *sibling* of `src/run.rs` rather than a descendant of
//! it — so three modules and four items under `src/run/` had to widen from
//! `pub(super)` to `pub(crate)` for the call to reach the gate at all.
//!
//! Nothing here drives the server. A served `tools/call` needs a live session,
//! and a session's parts are crate-private by construction, so the behaviour is
//! asserted from inside `src/mcp_server.rs` where those parts are nameable. What
//! is left is the half that can only be checked from out here: that each
//! promotion stopped at `pub(crate)`.
//!
//! **A source-text gate, and it has to be.** `mod run;` is private in
//! `src/lib.rs`, so an item inside it that accidentally became `pub` is still
//! exported by nothing: `tests/public_api.rs` would not notice, `cargo doc` would
//! not show it, and the mistake would ship as a wider promise than the release
//! made. Reading the declarations is the only thing that sees it.

use std::path::Path;

/// Read one of this crate's own source files, with line endings normalised.
///
/// A Windows checkout holds these with CRLF, and a gate comparing against
/// `\n`-terminated text would pass on one platform and fail on the other.
fn source(relative: &str) -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(relative))
        .unwrap_or_else(|e| panic!("{relative} is readable from the crate it belongs to: {e}"))
        .replace("\r\n", "\n")
}

/// Every module opened and every item widened so that `src/mcp_server.rs` can
/// call `dispatch`, as the declaration each one must carry.
///
/// Written out rather than pattern-matched: the point of the gate is that a
/// declaration cannot change without this list changing with it, and a regex
/// loose enough to survive an edit would be loose enough to accept a `pub`.
const PROMOTED: &[(&str, &str)] = &[
    ("src/run.rs", "pub(crate) mod dispatch;"),
    ("src/run.rs", "pub(crate) mod gate;"),
    ("src/run.rs", "pub(crate) mod memory;"),
    ("src/run/dispatch.rs", "pub(crate) async fn dispatch("),
    ("src/run/gate.rs", "pub(crate) enum Dispatched {"),
    ("src/run/gate.rs", "pub(crate) struct PlanPhase<'a> {"),
    ("src/run/memory.rs", "pub(crate) fn memory_key(root: &Path)"),
];

/// The same declarations spelled `pub`. None may appear anywhere.
const NEVER: &[(&str, &str)] = &[
    ("src/run.rs", "pub mod dispatch;"),
    ("src/run.rs", "pub mod gate;"),
    ("src/run.rs", "pub mod memory;"),
    ("src/run/dispatch.rs", "pub async fn dispatch("),
    ("src/run/gate.rs", "pub enum Dispatched"),
    ("src/run/gate.rs", "pub struct PlanPhase"),
    ("src/run/memory.rs", "pub fn memory_key("),
];

#[test]
fn nf5_every_promotion_stopped_at_pub_crate() {
    for (file, declaration) in PROMOTED {
        assert!(
            source(file).contains(declaration),
            "{file} no longer declares `{declaration}` — a promotion this release \
             made was changed without this gate changing with it"
        );
    }
    for (file, wider) in NEVER {
        assert!(
            !source(file).contains(wider),
            "{file} declares `{wider}`. A promotion made for `src/mcp_server.rs` \
             widened past `pub(crate)`, which nothing else catches: `mod run;` is \
             private, so a `pub` item inside it is exported by neither the public \
             API snapshot nor rustdoc"
        );
    }
}

#[test]
fn nf5_the_run_module_stays_private_so_no_promotion_can_escape_the_crate() {
    let lib = source("src/lib.rs");
    assert!(
        lib.lines().any(|line| line == "mod run;"),
        "`mod run;` is what bounds every `pub(crate)` promotion this release made; \
         exporting the module would publish all of them at once"
    );
}

#[test]
fn nf5_no_promoted_name_is_in_the_public_api_snapshot() {
    // The snapshot records `<kind> <name> <file>` per line. Compared against the
    // name column rather than by substring, so an unrelated line that happens to
    // mention one of these words cannot make the gate pass or fail by accident.
    let surface = source("docs/public-api.txt");
    for promoted in ["dispatch", "Dispatched", "PlanPhase", "memory_key"] {
        assert!(
            !surface
                .lines()
                .filter(|line| !line.starts_with('#'))
                .any(|line| line.split_whitespace().nth(1) == Some(promoted)),
            "`{promoted}` is crate-private machinery and must not be in the public surface"
        );
    }
}
