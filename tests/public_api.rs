//! The public-surface checkers.
//!
//! Two claims in the 0.16.0 contract are promises rather than pages: that no
//! public item vanishes without a deprecation cycle and a migration note, and
//! that every public item carries a worked example. Both decay silently. These
//! tests are what converts them into build failures.
//!
//! The enumeration is a text parse of `src/`, deliberately. `rustdoc --output-format
//! json` would be more precise and needs a nightly toolchain, and a checker that
//! needs a nightly toolchain is a checker that stops running the first time the
//! pinned nightly breaks. Everything here is `std`, so it runs wherever `cargo
//! test` runs.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

const ROOT: &str = env!("CARGO_MANIFEST_DIR");
const SNAPSHOT: &str = "docs/public-api.txt";

/// The kinds of item a `pub use` in `lib.rs` can name, longest keyword first so
/// `async fn` is not mistaken for a name beginning with `async`.
const KINDS: &[&str] = &["async fn", "struct", "enum", "trait", "const", "type", "fn"];

// ---------------------------------------------------------------------------
// The enumeration
// ---------------------------------------------------------------------------

/// One publicly re-exported item: what it is, what it is called, where it lives,
/// what feature gates it, and the doc block written above its definition.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Item {
    kind: String,
    name: String,
    file: String,
    gate: Option<String>,
    docs: String,
}

impl Item {
    /// The snapshot line. `<kind> <name> <file>`, plus the gate when there is one
    /// — a re-export that becomes conditional is a change to the surface as much
    /// as one that disappears, so the gate has to be part of what is compared.
    fn line(&self) -> String {
        match &self.gate {
            Some(g) => format!("{} {} {} ({})", self.kind, self.name, self.file, g),
            None => format!("{} {} {}", self.kind, self.name, self.file),
        }
    }

    /// How an item is named in a report: enough to find it, not the whole line.
    fn label(&self) -> String {
        format!("{} {} ({})", self.kind, self.name, self.file)
    }

    fn parse_line(line: &str) -> Option<Item> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }
        let mut parts = line.splitn(4, ' ');
        let kind = parts.next()?.to_string();
        let name = parts.next()?.to_string();
        let file = parts.next()?.to_string();
        let gate = parts
            .next()
            .map(|g| g.trim().trim_start_matches('(').trim_end_matches(')').to_string());
        Some(Item { kind, name, file, gate, docs: String::new() })
    }
}

/// A `pub struct` / `pub fn` / … found at column zero of a source file.
///
/// Column zero is the whole of the "is it a free item" test: an inherent method
/// or a trait method is indented, and so is anything inside `mod tests`. It also
/// means `pub(crate)` needs no special case — the line starts `pub(`, not `pub `.
#[derive(Clone, Debug)]
struct Def {
    kind: String,
    name: String,
    docs: String,
}

/// Every `.rs` file under `src/`, as (repo-relative path, contents), sorted so a
/// resolution that falls back to a directory scan is deterministic.
fn source_index() -> Vec<(String, String)> {
    let root = PathBuf::from(ROOT);
    let mut out = Vec::new();
    collect(&root.join("src"), &root, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn collect(dir: &Path, root: &Path, out: &mut Vec<(String, String)>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => panic!("cannot read {}: {e}", dir.display()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, root, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).unwrap_or_default();
            out.push((rel, text));
        }
    }
}

/// Bracket balance of a line, used to carry a multi-line `#[derive(...)]` without
/// losing the doc block above it.
///
// ponytail: counts brackets textually, so an attribute containing an unbalanced
// bracket inside a string literal would confuse it. No such attribute exists in
// this crate; if one appears, the fix is to skip bracket runs inside quotes.
fn bracket_delta(line: &str) -> i32 {
    line.chars().filter(|c| *c == '[').count() as i32
        - line.chars().filter(|c| *c == ']').count() as i32
}

/// Split `pub struct Foo<T> {` into its kind and name. `None` for a `pub` line
/// that is not one of the kinds a re-export can name (a `pub mod`, a `pub use`).
fn kind_and_name(rest: &str) -> Option<(String, String)> {
    for kw in KINDS {
        let Some(tail) = rest.strip_prefix(kw) else {
            continue;
        };
        if !tail.starts_with(' ') {
            continue;
        }
        let name: String = tail
            .trim_start()
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            continue;
        }
        return Some((kw.replace(' ', "_"), name));
    }
    None
}

/// Every column-zero public definition in one file, with the `///` block above it.
///
/// Walking forwards rather than backwards from the item is what makes the
/// attribute case cheap: attributes and ordinary comments carry the pending doc
/// block along, and anything else — a blank line, a closing brace, a statement —
/// clears it, which is exactly rustdoc's own rule about what a doc block attaches
/// to.
fn defs(src: &str) -> Vec<Def> {
    let mut out = Vec::new();
    let mut docs: Vec<&str> = Vec::new();
    let mut attr_depth = 0i32;
    for line in src.lines() {
        if attr_depth > 0 {
            attr_depth += bracket_delta(line);
            continue;
        }
        if let Some(d) = line.strip_prefix("///") {
            docs.push(d);
            continue;
        }
        if line.starts_with("#[") || line.starts_with("#!") {
            attr_depth += bracket_delta(line);
            continue;
        }
        if line.starts_with("//") {
            continue;
        }
        if let Some(rest) = line.strip_prefix("pub ") {
            if let Some((kind, name)) = kind_and_name(rest) {
                out.push(Def { kind, name, docs: docs.join("\n") });
                docs.clear();
                continue;
            }
        }
        docs.clear();
    }
    out
}

/// Where to look for `module::Name`, most specific first.
///
/// `pub use provider::{Anthropic, Media}` names one module and two items defined
/// in different files: `Media` in `provider/mod.rs`, `Anthropic` re-exported by it
/// from `provider/anthropic.rs`. Rather than follow re-export chains, the search
/// widens — the module's own file, then its directory, then its parent's — which
/// resolves both and keeps `containment::Ledger` from colliding with the unrelated
/// `context::Ledger`.
fn candidates(module: &str) -> Vec<(bool, String)> {
    let segs: Vec<&str> = module.split("::").filter(|s| !s.is_empty()).collect();
    let mut out = Vec::new();
    for i in (1..=segs.len()).rev() {
        let base = format!("src/{}", segs[..i].join("/"));
        out.push((false, format!("{base}.rs")));
        out.push((false, format!("{base}/mod.rs")));
        out.push((true, format!("{base}/")));
    }
    out.push((true, "src/".to_string()));
    out
}

fn resolve(index: &[(String, Vec<Def>)], module: &str, name: &str) -> Option<(String, Def)> {
    for (is_dir, path) in candidates(module) {
        for (file, defs) in index {
            let hit = if is_dir { file.starts_with(&path) } else { *file == path };
            if !hit {
                continue;
            }
            if let Some(d) = defs.iter().find(|d| d.name == name) {
                return Some((file.clone(), d.clone()));
            }
        }
    }
    None
}

/// `feature = "media"` out of `#[cfg(feature = "media")]`.
fn cfg_gate(line: &str) -> Option<String> {
    let inner = line.trim().strip_prefix("#[cfg(")?.strip_suffix(")]")?;
    Some(inner.trim().to_string())
}

/// Every name re-exported from `lib.rs`, as (module path, name, feature gate).
fn exports(lib: &str) -> Vec<(String, String, Option<String>)> {
    let mut out = Vec::new();
    let mut gate: Option<String> = None;
    let mut buf = String::new();
    for line in lib.lines() {
        let t = line.trim_end();
        if buf.is_empty() {
            if let Some(g) = cfg_gate(t) {
                gate = Some(g);
                continue;
            }
            if t.starts_with("//") {
                continue;
            }
            match t.strip_prefix("pub use ") {
                Some(rest) => buf.push_str(rest.trim()),
                None => {
                    gate = None;
                    continue;
                }
            }
        } else {
            buf.push(' ');
            buf.push_str(t.trim());
        }
        if !buf.ends_with(';') {
            continue;
        }
        let stmt = buf.trim_end_matches(';');
        match stmt.find('{') {
            Some(i) => {
                let module = stmt[..i].trim().trim_end_matches("::").to_string();
                let inner = stmt[i + 1..].trim_end_matches('}');
                for name in inner.split(',') {
                    let name = name.trim();
                    if !name.is_empty() {
                        out.push((module.clone(), name.to_string(), gate.clone()));
                    }
                }
            }
            None => {
                if let Some(i) = stmt.rfind("::") {
                    out.push((stmt[..i].to_string(), stmt[i + 2..].trim().to_string(), gate.clone()));
                }
            }
        }
        buf.clear();
        gate = None;
    }
    out
}

/// The live public surface, sorted the way the snapshot is sorted.
///
/// An exported name that cannot be resolved to a definition is an error, not a
/// skip. A checker that silently drops what it cannot parse reports an empty
/// difference for every input, which is the one failure mode that makes the whole
/// mechanism worthless.
fn enumerate() -> Result<Vec<Item>, String> {
    let files = source_index();
    let index: Vec<(String, Vec<Def>)> =
        files.iter().map(|(p, s)| (p.clone(), defs(s))).collect();
    let lib = files
        .iter()
        .find(|(p, _)| p == "src/lib.rs")
        .map(|(_, s)| s.clone())
        .ok_or_else(|| "src/lib.rs not found".to_string())?;

    let mut items = Vec::new();
    let mut unresolved = Vec::new();
    for (module, name, gate) in exports(&lib) {
        match resolve(&index, &module, &name) {
            Some((file, def)) => items.push(Item {
                kind: def.kind,
                name,
                file,
                gate,
                docs: def.docs,
            }),
            None => unresolved.push(format!("  {module}::{name}")),
        }
    }
    if !unresolved.is_empty() {
        return Err(format!(
            "{} name(s) re-exported from src/lib.rs could not be resolved to a definition:\n{}\n\n\
             Either the item moved and the enumerator's search needs widening, or the re-export \
             is stale. It cannot be skipped: an unresolvable name is an unchecked name.",
            unresolved.len(),
            unresolved.join("\n")
        ));
    }
    items.sort_by_key(|i| i.line());
    Ok(items)
}

// ---------------------------------------------------------------------------
// T01 — the snapshot comparison (F8)
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Drift {
    added: Vec<Item>,
    removed: Vec<Item>,
    renamed: Vec<(Item, Item)>,
}

impl Drift {
    fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.renamed.is_empty()
    }
}

/// Compare a live surface against a snapshot.
///
/// Pure, over two lists, so the negative controls can feed it synthetic vectors
/// instead of mutating a checked-in file. A rename is a removal and an addition
/// that agree about kind and defining file — reporting it as such matters because
/// a rename and a removal need the same deprecation cycle, and a diff that shows
/// them as unrelated add/remove pairs reads like a wash.
fn drift(current: &[Item], snapshot: &[Item]) -> Drift {
    let cur: BTreeSet<String> = current.iter().map(Item::line).collect();
    let snap: BTreeSet<String> = snapshot.iter().map(Item::line).collect();

    let mut added: Vec<Item> = current.iter().filter(|i| !snap.contains(&i.line())).cloned().collect();
    let mut removed: Vec<Item> = snapshot.iter().filter(|i| !cur.contains(&i.line())).cloned().collect();

    let mut renamed = Vec::new();
    let mut kept_removed = Vec::new();
    for r in removed.drain(..) {
        match added
            .iter()
            .position(|a| a.kind == r.kind && a.file == r.file && a.name != r.name)
        {
            Some(i) => renamed.push((r, added.remove(i))),
            None => kept_removed.push(r),
        }
    }
    Drift { added, removed: kept_removed, renamed }
}

/// The failure message. `None` when the surface matches.
fn drift_report(d: &Drift) -> Option<String> {
    if d.is_empty() {
        return None;
    }
    let mut m = String::from("the public surface no longer matches docs/public-api.txt\n\n");
    if !d.renamed.is_empty() {
        m.push_str(&format!("RENAMED ({}):\n", d.renamed.len()));
        for (old, new) in &d.renamed {
            m.push_str(&format!("  {} -> {}\n", old.label(), new.name));
        }
    }
    if !d.removed.is_empty() {
        m.push_str(&format!("REMOVED ({}):\n", d.removed.len()));
        for i in &d.removed {
            m.push_str(&format!("  {}\n", i.label()));
        }
    }
    if !d.added.is_empty() {
        m.push_str(&format!("ADDED ({}):\n", d.added.len()));
        for i in &d.added {
            m.push_str(&format!("  {}\n", i.label()));
        }
    }
    m.push_str(
        "\nWhat to do:\n\
         - REMOVED or RENAMED: this is a break. The removed name keeps existing with a\n  \
           #[deprecated] attribute naming its replacement, and CHANGELOG.md gets a migration\n  \
           note under Removed or Changed saying what to write instead.\n\
         - ADDED: the new item needs a doc comment with a worked example (see the F5 test)\n  \
           before it is a documented part of the surface.\n\
         - Then edit docs/public-api.txt by hand to match. There is deliberately no --bless\n  \
           flag: a one-command regenerate turns this test into a rubber stamp the first time\n  \
           someone is in a hurry, which is precisely the moment it exists to catch.\n",
    );
    Some(m)
}

#[test]
fn public_surface_matches_snapshot() {
    let current = enumerate().unwrap_or_else(|e| panic!("{e}"));
    let path = PathBuf::from(ROOT).join(SNAPSHOT);
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} is the checked-in record of the public surface and could not be read ({e}). \
             It is written by hand, one `<kind> <name> <file>` line per item.",
            path.display()
        )
    });
    let snapshot: Vec<Item> = text.lines().filter_map(Item::parse_line).collect();
    assert!(
        !snapshot.is_empty(),
        "{} parsed to zero items — the comparison below would pass for any surface",
        path.display()
    );
    if let Some(report) = drift_report(&drift(&current, &snapshot)) {
        panic!("{report}");
    }
}

// ---------------------------------------------------------------------------
// T02 — the worked-example checker (F5)
// ---------------------------------------------------------------------------

/// Public items whose doc comment carries no fenced code block.
fn without_examples(items: &[Item]) -> Vec<&Item> {
    items.iter().filter(|i| !i.docs.contains("```")).collect()
}

#[test]
fn every_public_item_has_a_worked_example() {
    let items = enumerate().unwrap_or_else(|e| panic!("{e}"));
    let missing = without_examples(&items);
    if missing.is_empty() {
        return;
    }
    let list: Vec<String> = missing.iter().map(|i| format!("  {}", i.label())).collect();
    panic!(
        "{} of {} public items have no worked example (no ``` fence in their doc comment):\n{}\n\n\
         Every item re-exported from src/lib.rs carries an example that compiles as a doctest. \
         Add one per item above; the list shrinks as the sweep proceeds.",
        missing.len(),
        items.len(),
        list.join("\n")
    );
}

// ---------------------------------------------------------------------------
// Negative controls
// ---------------------------------------------------------------------------
//
// Each checker above answers a question about a list. Fed a list, each one can
// return "nothing wrong" — which is what it would do if it were broken. These
// prove otherwise, and assert on the reported text rather than on a length,
// because a report that finds the right count and names the wrong item is still
// a report nobody can act on.

fn fixture(kind: &str, name: &str, file: &str, docs: &str) -> Item {
    Item {
        kind: kind.into(),
        name: name.into(),
        file: file.into(),
        gate: None,
        docs: docs.into(),
    }
}

#[test]
fn drift_reports_a_removal() {
    let snapshot = vec![
        fixture("struct", "Kept", "src/a.rs", ""),
        fixture("fn", "gone", "src/b.rs", ""),
    ];
    let current = vec![fixture("struct", "Kept", "src/a.rs", "")];

    let d = drift(&current, &snapshot);
    assert_eq!(d.removed.len(), 1, "expected exactly one removal, got {d:?}");
    assert_eq!(d.removed[0].name, "gone");
    assert!(d.added.is_empty() && d.renamed.is_empty(), "{d:?}");

    let report = drift_report(&d).expect("a removal must produce a report");
    assert!(report.contains("REMOVED (1):"), "{report}");
    assert!(report.contains("fn gone (src/b.rs)"), "{report}");
    assert!(report.contains("#[deprecated]"), "{report}");
    assert!(!report.contains("Kept"), "{report}");
}

#[test]
fn drift_reports_a_rename() {
    let snapshot = vec![fixture("struct", "OldName", "src/a.rs", "")];
    let current = vec![fixture("struct", "NewName", "src/a.rs", "")];

    let d = drift(&current, &snapshot);
    assert_eq!(d.renamed.len(), 1, "expected exactly one rename, got {d:?}");
    assert_eq!(d.renamed[0].0.name, "OldName");
    assert_eq!(d.renamed[0].1.name, "NewName");
    assert!(
        d.added.is_empty() && d.removed.is_empty(),
        "a rename must not also be reported as a bare add/remove pair: {d:?}"
    );

    let report = drift_report(&d).expect("a rename must produce a report");
    assert!(report.contains("RENAMED (1):"), "{report}");
    assert!(report.contains("struct OldName (src/a.rs) -> NewName"), "{report}");
}

#[test]
fn drift_reports_an_addition_and_stays_quiet_when_nothing_changed() {
    let snapshot = vec![fixture("struct", "Kept", "src/a.rs", "")];
    let current = vec![
        fixture("struct", "Kept", "src/a.rs", ""),
        fixture("const", "NEW", "src/b.rs", ""),
    ];

    let d = drift(&current, &snapshot);
    assert_eq!(d.added.len(), 1, "{d:?}");
    assert!(d.removed.is_empty() && d.renamed.is_empty(), "{d:?}");
    let report = drift_report(&d).expect("an addition must produce a report");
    assert!(report.contains("ADDED (1):"), "{report}");
    assert!(report.contains("const NEW (src/b.rs)"), "{report}");

    assert!(
        drift_report(&drift(&snapshot, &snapshot)).is_none(),
        "an unchanged surface must report nothing"
    );
}

#[test]
fn a_feature_gate_is_part_of_the_compared_surface() {
    let snapshot = vec![fixture("struct", "Media", "src/provider/mod.rs", "")];
    let mut gated = snapshot[0].clone();
    gated.gate = Some("feature = \"media\"".into());

    let report = drift_report(&drift(&[gated], &snapshot))
        .expect("moving an item behind a feature gate must be reported");
    assert!(report.contains("feature") || report.contains("Media"), "{report}");
}

#[test]
fn example_checker_reports_a_documented_but_exampleless_item() {
    let items = vec![
        fixture(
            "struct",
            "Documented",
            "src/a.rs",
            " A thorough paragraph of prose.\n\n Another one. No example anywhere in it.",
        ),
        fixture(
            "fn",
            "worked",
            "src/b.rs",
            " Prose, then an example.\n\n ```\n let x = 1;\n ```",
        ),
    ];

    let missing = without_examples(&items);
    assert_eq!(
        missing.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
        vec!["Documented"],
        "the checker must report the documented-but-exampleless item and only that one"
    );
}

#[test]
fn an_unresolvable_export_is_reported_rather_than_skipped() {
    let index = vec![(
        "src/a.rs".to_string(),
        defs("/// Docs.\npub struct Present {}\n"),
    )];
    assert!(resolve(&index, "a", "Present").is_some());
    assert!(
        resolve(&index, "a", "Absent").is_none(),
        "a name with no definition must not resolve to some other item"
    );
}

#[test]
fn the_parser_reads_the_shapes_this_crate_actually_uses() {
    let src = "\
/// Gated docs.
///
/// ```
/// let x = 1;
/// ```
#[cfg(feature = \"media\")]
#[derive(
    Debug,
    Clone,
)]
pub struct Gated {
    /// A field, whose docs belong to the field.
    pub f: String,
}

pub(crate) struct NotPublic;

/// Undocumented by example.
pub async fn go() {}

impl Gated {
    /// A method, not a free item.
    pub fn method(&self) {}
}
";
    let found = defs(src);
    let names: Vec<&str> = found.iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, vec!["Gated", "go"], "got {found:?}");
    assert_eq!(found[0].kind, "struct");
    assert!(found[0].docs.contains("```"), "attributes must not detach the doc block");
    assert_eq!(found[1].kind, "async_fn");
    assert!(!found[1].docs.contains("```"));

    let uses = exports(
        "#[cfg(feature = \"media\")]\npub use provider::{Media, IMAGE_MEDIA_TYPES};\n\
         pub use net::REQUEST_TIMEOUT;\n\
         pub use run::{\n    run,\n    run_observed,\n};\n",
    );
    assert_eq!(
        uses,
        vec![
            ("provider".into(), "Media".into(), Some("feature = \"media\"".into())),
            ("provider".into(), "IMAGE_MEDIA_TYPES".into(), Some("feature = \"media\"".into())),
            ("net".into(), "REQUEST_TIMEOUT".into(), None),
            ("run".into(), "run".into(), None),
            ("run".into(), "run_observed".into(), None),
        ]
    );
}
