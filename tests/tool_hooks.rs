//! A check attached to a point in the tool lifecycle, from `io.toml` (0.42.0).
//!
//! A `[[hook]]` has been an `Observer` since 0.28.0: it names events and the
//! strongest thing it can do is cancel at the next step boundary, which is after
//! the tool it objected to has run. So "never run this command in this
//! repository" was a Rust `Approver` an operator had to compile in.
//!
//! What is asserted here is what did **not** happen: no `Edit` row, a file
//! byte-identical on disk, a hook never spawned. A refusal that arrives after the
//! write has landed produces the same event stream and the same log line, and
//! only the absent write tells the two apart.

use std::path::Path;

use io_harness::Config;

// ---------------------------------------------------------------- scaffolding

/// Write `io.local.toml` — local scope, where a hook is permitted — and read the
/// configuration back.
fn local(dir: &Path, body: &str) -> io_harness::Result<Config> {
    std::fs::write(dir.join("io.local.toml"), body).unwrap();
    Config::discover(dir)
}

/// Write `io.toml` — project scope, the file a `git clone` delivers.
fn project(dir: &Path, body: &str) -> io_harness::Result<Config> {
    std::fs::write(dir.join("io.toml"), body).unwrap();
    Config::discover(dir)
}

const GATE: &str = r#"
[[hook]]
at = "before_tool"
tools = ["write_file"]
run = ["true"]
"#;

// ------------------------------------------------------------------------- F6

/// F6 — the trust rule is extended, never weakened.
///
/// A hook that can stop a tool is strictly more dangerous than one that appends a
/// log line, so `at` inherits the project-scope refusal rather than reopening the
/// question — including inside a `[profile]`, which is where the boundary has been
/// widened by accident before.
#[test]
fn a_lifecycle_hook_is_refused_in_a_project_scoped_file() {
    let dir = tempfile::tempdir().unwrap();
    let err = project(dir.path(), GATE).unwrap_err();
    assert!(err.to_string().contains("may not declare hooks"), "{err}");

    // The same table, one level down, reached by a different path.
    let dir = tempfile::tempdir().unwrap();
    let err = project(
        dir.path(),
        "[profile.ci]\n[[profile.ci.hook]]\nat = \"before_tool\"\nrun = [\"true\"]\n",
    )
    .unwrap_err();
    assert!(err.to_string().contains("hook"), "{err}");

    // And the identical file at local scope loads.
    let dir = tempfile::tempdir().unwrap();
    let config = local(dir.path(), GATE).expect("a local-scope lifecycle hook loads");
    assert!(!config.hooks().is_empty());
}

/// F6 — a table this crate cannot honour is refused when it is read, not when it
/// would have fired.
///
/// The failure mode a lifecycle hook can least afford is silence: a misspelled
/// `at` that loads, installs and never fires looks exactly like a check that
/// approved everything.
#[test]
fn a_lifecycle_table_that_cannot_fire_is_refused_at_load() {
    let cases = [
        // An `at` value this crate does not have.
        (
            "[[hook]]\nat = \"after_tool\"\nrun = [\"true\"]\n",
            "after_tool",
        ),
        // Both kinds at once. An event hook and a lifecycle hook are different
        // things and a table claiming both is a mistake worth naming.
        (
            "[[hook]]\non = [\"stalled\"]\nat = \"before_tool\"\nrun = [\"true\"]\n",
            "hook[0]",
        ),
        // A tool filter on an event hook filters nothing.
        (
            "[[hook]]\non = [\"stalled\"]\ntools = [\"exec\"]\nappend = \"a.jsonl\"\n",
            "tools",
        ),
        // Appending a log line cannot stop a tool call, so a lifecycle hook that
        // only appends is a check that always passes.
        (
            "[[hook]]\nat = \"before_tool\"\nappend = \"a.jsonl\"\n",
            "run",
        ),
    ];
    for (body, expect) in cases {
        let dir = tempfile::tempdir().unwrap();
        let err = local(dir.path(), body).unwrap_err();
        assert!(
            err.to_string().contains(expect),
            "`{body}` must be refused naming `{expect}`, got: {err}"
        );
    }
}
