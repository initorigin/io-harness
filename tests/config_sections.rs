//! `Config::is_empty` accounts for every section the file format carries.
//!
//! It was a hand-written list of twelve, and the file format has twenty
//! sections: `[[lsp]]`, `[[hook]]`, `[[plugin]]`, `[browser]`, `[memory]`,
//! `[routing]`, `[otel]` and `[codeact]` were all missing, so a configuration
//! carrying only one of them reported that it set nothing at all.
//!
//! The point of this file is not that those eight are covered now — a list of
//! twenty is forgotten exactly the way a list of twelve was. The case table
//! below is checked against the field list of `struct File`, **read out of
//! `src/config.rs` at test time**, so a section added to the type with no case
//! here fails this test rather than being silently omitted from the answer.
//! `is_empty` carries the other half: it destructures `File` exhaustively, so a
//! new field is a compile error there before it is a failure here.
//!
//! Every case is written at the **user scope**. Half these sections are in
//! `REFUSED_SECTIONS` and cannot be written through `Config::from_toml`, which
//! is the project scope, so one loading path for all twenty is the only way the
//! table stays one table.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use io_harness::Config;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The floor on how many fields the parse must find in `struct File`.
///
/// Twenty when this was written. The floor exists because every assertion below
/// passes over an empty set: a parse that has gone blind — a rename, a
/// rustfmt-shape change, a CRLF checkout — would otherwise report agreement
/// between two empty lists.
const FIELD_FLOOR: usize = 18;

/// One case per section: the field of `struct File` it sets, and the smallest
/// user-scope file that sets it. Keep the values narrow and inert — this test
/// asks whether the section was *seen*, and a value with a validator behind it
/// would make a failure here mean two things.
const SECTIONS: &[(&str, &str)] = &[
    ("policy", "[policy]\n"),
    ("sandbox", "[sandbox]\nforce_floor = true\n"),
    ("run", "[run]\nmax_steps = 3\n"),
    ("memory", "[memory]\nmax_entries = 10\n"),
    // A routing rule is a threshold and a model; `require_primary` is neither,
    // so it sets the section without writing half a rule.
    ("routing", "[routing]\nrequire_primary = true\n"),
    ("otel", "[otel]\nservice_name = \"io-harness\"\n"),
    ("codeact", "[codeact]\nmax_callbacks = 4\n"),
    (
        "toolchain",
        "[toolchain.rust]\ntest = [\"cargo\", \"test\"]\n",
    ),
    ("prices", "[prices]\nas_of = \"2026-01-01\"\n"),
    (
        "mcp",
        "[[mcp]]\nid = \"probe\"\ntransport = \"stdio\"\ncommand = \"true\"\n",
    ),
    ("lsp", "[[lsp]]\nid = \"probe\"\ncommand = \"true\"\n"),
    ("browser", "[browser]\nheadless = true\n"),
    ("agent", "[[agent]]\nname = \"helper\"\n"),
    ("web", "[web]\nsearch = true\n"),
    (
        "provider",
        "[[provider]]\nkind = \"anthropic\"\nmodel = \"claude-sonnet-4\"\n",
    ),
    ("app", "[app]\nanything = 1\n"),
    ("profile", "[profile.dev.run]\nmax_steps = 3\n"),
    ("instructions", "[instructions]\nfiles = [\"AGENTS.md\"]\n"),
    // No `on`, so no event name to keep in step with `EVENT_NAMES`; an empty
    // `on` is every event, which is the shape that needs the least from the
    // validator.
    ("hook", "[[hook]]\nrun = [\"true\"]\n"),
    ("plugin", "[[plugin]]\npath = \"bundles/kit\"\n"),
];

static ENV: Mutex<()> = Mutex::new(());

/// Hold the environment and point the user scope at `user_dir`. The process has
/// one environment and `cargo test` runs these in parallel, so a test that set
/// the variable while another read it would fail unreproducibly. Same pattern,
/// same reason, as `tests/config.rs`.
fn env<'a>(user_dir: &Path) -> MutexGuard<'a, ()> {
    let guard = ENV.lock().unwrap_or_else(|e| e.into_inner());
    std::env::set_var("IO_CONFIG_HOME", user_dir);
    guard
}

/// The fields of `struct File`, read out of `src/config.rs`.
///
/// The item starts at `struct File {` and ends at the first line that is exactly
/// `}` at column zero — the file's own layout, held there by `cargo fmt`. A
/// field is a line of exactly one indent naming a key: attributes start with
/// `#`, doc comments with `/`, and nothing else in this item is indented once.
fn file_fields() -> BTreeSet<String> {
    let path = PathBuf::from(ROOT).join("src/config.rs");
    // Normalised, because a CRLF checkout would otherwise leave `\r` on the end
    // of the terminator and this parse would run to the end of the file.
    let source = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n");

    let marker = "\nstruct File {\n";
    let start = source.find(marker).unwrap_or_else(|| {
        panic!(
            "{}: `struct File` is not where this parse expects it — the shape it reads has \
             changed, and the case table below is checking nothing",
            path.display()
        )
    });
    let body = &source[start + marker.len()..];
    let end = body.find("\n}\n").unwrap_or_else(|| {
        panic!(
            "{}: `struct File` has no terminator at column zero",
            path.display()
        )
    });

    body[..end]
        .lines()
        .filter_map(|line| {
            let field = line.strip_prefix("    ")?;
            if field.starts_with([' ', '#', '/', '}']) {
                return None;
            }
            let (name, _) = field.split_once(':')?;
            Some(name.trim().to_string())
        })
        .collect()
}

/// The derivation. A section added to `File` with no case in `SECTIONS` fails
/// here, which is the property a hand-written list of twenty does not have.
#[test]
fn every_section_of_the_file_format_has_a_case() {
    let fields = file_fields();
    assert!(
        fields.len() >= FIELD_FLOOR,
        "only {} fields parsed out of `struct File`, under the floor of {FIELD_FLOOR}: the \
         parse has gone blind and every comparison below is between two empty sets — {fields:?}",
        fields.len()
    );

    let cases: BTreeSet<String> = SECTIONS.iter().map(|(name, _)| name.to_string()).collect();
    assert_eq!(
        cases.len(),
        SECTIONS.len(),
        "two cases name the same section"
    );

    let missing: Vec<_> = fields.difference(&cases).collect();
    assert!(
        missing.is_empty(),
        "these sections of `struct File` have no case here, so nothing checks that \
         `Config::is_empty` counts them: {missing:?}. Add a minimal file for each — and note \
         that `is_empty` destructures `File`, so it will already have refused to compile"
    );
    let stale: Vec<_> = cases.difference(&fields).collect();
    assert!(
        stale.is_empty(),
        "these cases name nothing in `struct File` any more: {stale:?}"
    );
}

/// The behaviour itself, one section at a time: a file that carries exactly one
/// section is not an empty configuration.
#[test]
fn a_configuration_carrying_only_one_section_is_not_empty() {
    for (name, text) in SECTIONS {
        // `[browser]` is behind the feature that compiles the field, and `File`
        // carries `deny_unknown_fields`, so on a default build the section is
        // not a section — it is an unknown key, and rejecting it is correct.
        if *name == "browser" && !cfg!(feature = "browser") {
            continue;
        }

        let user_dir = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let _guard = env(user_dir.path());
        fs::write(user_dir.path().join("io.toml"), text).unwrap();

        let config = Config::discover(project.path())
            .unwrap_or_else(|e| panic!("[{name}] must load: {e}\n{text}"));
        assert!(
            !config.is_empty(),
            "[{name}] is a section this file sets, and the configuration reported that it sets \
             nothing:\n{text}"
        );
    }
}

/// The control. Without it every assertion above is satisfied by an `is_empty`
/// that returns `false` unconditionally.
#[test]
fn a_configuration_with_no_file_behind_it_is_empty() {
    let user_dir = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let _guard = env(user_dir.path());

    assert!(
        Config::discover(project.path()).unwrap().is_empty(),
        "no file in any scope sets anything"
    );
    assert!(Config::from_toml("").unwrap().is_empty());
}
