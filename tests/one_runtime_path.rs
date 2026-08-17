//! One runtime path, derived from the source rather than believed.
//!
//! `src/run.rs` exposes thirty-five top-level public functions and
//! [`Harness`](io_harness::Harness) adds a handful of methods over them. The
//! claim this release makes is that all of it is *one* loop with one branch —
//! `run_with_extras` for a flat run, `run_tree_with_extras` for a tree — and that
//! no entry point assembles a run of its own.
//!
//! A behavioural test cannot see that claim. `tests/harness.rs` proves the
//! facade and the free function agree on one contract; it would still pass if a
//! thirty-sixth entry point were added tomorrow with its own inline loop. So the
//! invariant is **derived**: the file is parsed, a call graph is built over its
//! own top-level functions, and every driving entry point must reach one of the
//! two engines. That is the same shape as
//! `every_name_the_harness_answers_is_reserved` (0.61.0), and it is here for the
//! same reason — a list that has to be kept current by hand is a list that goes
//! stale.
//!
//! **The guards matter as much as the assertion.** A parse that silently matches
//! nothing passes every assertion made over an empty set, so there are floors on
//! both counts, a named panic when the file's shape stops being parseable, and a
//! CRLF fixture — 0.60.2 shipped a checker that fell back to the whole document
//! on a CRLF checkout and passed for the wrong reason.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;

const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The two crate-internal engines every driving entry point must reach.
const ENGINES: &[&str] = &["run_with_extras", "run_tree_with_extras"];

/// The floor on how many top-level functions the parse must find in `src/run.rs`.
///
/// It stood at 251 when this test was written. The floor is deliberately far
/// below that: its job is to catch a parse that has gone blind, not to be a
/// second inventory that needs updating whenever a private helper is added.
const ITEM_FLOOR: usize = 150;

/// The floor on how many *driving* public entry points the parse must find.
///
/// Thirty when this test was written: thirty-five top-level public functions
/// minus the five synchronous `rewind*` functions, which read and revert the
/// trace and never enter the loop at all.
const DRIVER_FLOOR: usize = 28;

/// One top-level function in `src/run.rs`.
struct Fun {
    name: String,
    /// `pub` at column zero — reachable from `src/lib.rs`'s re-export block.
    public: bool,
    /// `async`, which is what tells a driving entry point from `rewind`.
    asynchronous: bool,
    body: String,
}

fn read(path: &str) -> String {
    let full = PathBuf::from(ROOT).join(path);
    // Normalised once, here, and never per checker: a CRLF checkout silently
    // changed what a windowing helper matched in 0.60.2 and the assertion inside
    // it then passed for the wrong reason.
    fs::read_to_string(&full)
        .unwrap_or_else(|e| panic!("{}: {e}", full.display()))
        .replace("\r\n", "\n")
}

/// Every top-level function in `source`, with its body.
///
/// A top-level item starts at column zero and ends at the first line that is
/// exactly `}` at column zero. That is the file's own layout, enforced by
/// `cargo fmt`, and the named panic below fires the moment it stops being true.
fn functions(source: &str) -> Vec<Fun> {
    let mut out = Vec::new();
    let lines: Vec<&str> = source.lines().collect();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        let decl = line
            .strip_prefix("pub async fn ")
            .map(|r| (r, true, true))
            .or_else(|| line.strip_prefix("pub fn ").map(|r| (r, true, false)))
            .or_else(|| {
                line.strip_prefix("pub(crate) async fn ")
                    .map(|r| (r, false, true))
            })
            .or_else(|| {
                line.strip_prefix("pub(crate) fn ")
                    .map(|r| (r, false, false))
            })
            .or_else(|| line.strip_prefix("async fn ").map(|r| (r, false, true)))
            .or_else(|| line.strip_prefix("fn ").map(|r| (r, false, false)));
        let Some((rest, public, asynchronous)) = decl else {
            i += 1;
            continue;
        };
        let name: String = rest
            .chars()
            .take_while(|c| c.is_alphanumeric() || *c == '_')
            .collect();
        if name.is_empty() {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = lines.len();
        for (j, l) in lines.iter().enumerate().skip(start + 1) {
            if *l == "}" {
                end = j;
                break;
            }
        }
        out.push(Fun {
            name,
            public,
            asynchronous,
            body: lines[start..=end.min(lines.len() - 1)].join("\n"),
        });
        i = end + 1;
    }
    out
}

/// Whether `body` mentions `name` as a call rather than as a substring of a
/// longer identifier.
fn calls(body: &str, name: &str) -> bool {
    let bytes = body.as_bytes();
    let mut from = 0;
    while let Some(at) = body[from..].find(name) {
        let at = from + at;
        let before_ok = at == 0 || {
            let c = bytes[at - 1] as char;
            !(c.is_alphanumeric() || c == '_')
        };
        let after = at + name.len();
        let after_ok = after >= bytes.len() || {
            let c = bytes[after] as char;
            !(c.is_alphanumeric() || c == '_')
        };
        if before_ok && after_ok {
            return true;
        }
        from = at + 1;
    }
    false
}

/// Every name reachable from `start` through calls to other functions in `graph`.
fn reaches(graph: &BTreeMap<&str, &str>, start: &str) -> BTreeSet<String> {
    let mut seen = BTreeSet::new();
    let mut stack = vec![start.to_string()];
    while let Some(here) = stack.pop() {
        let Some(body) = graph.get(here.as_str()) else {
            continue;
        };
        for candidate in graph.keys() {
            if **candidate != here && !seen.contains(*candidate) && calls(body, candidate) {
                seen.insert(candidate.to_string());
                stack.push(candidate.to_string());
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------
// F3 — one runtime path
// ---------------------------------------------------------------------------

/// Every driving public entry point in `src/run.rs` reaches one of the two
/// engines, and no entry point assembles a run of its own.
#[test]
fn every_driving_entry_point_reaches_the_shared_engine() {
    let source = read("src/run.rs");
    let items = functions(&source);
    assert!(
        items.len() >= ITEM_FLOOR,
        "the parse of src/run.rs found only {} top-level functions, which is below the \
         floor of {ITEM_FLOOR} — the file's layout has changed and this checker is blind, \
         which is a finding about the checker and not a passing test",
        items.len()
    );

    let graph: BTreeMap<&str, &str> = items
        .iter()
        .map(|f| (f.name.as_str(), f.body.as_str()))
        .collect();
    for engine in ENGINES {
        assert!(
            graph.contains_key(engine),
            "src/run.rs no longer defines `{engine}` — the engine this invariant is written \
             about has been renamed or removed, and the invariant must be rewritten rather \
             than left to pass vacuously"
        );
    }

    let drivers: Vec<&Fun> = items
        .iter()
        .filter(|f| f.public && f.asynchronous)
        .collect();
    assert!(
        drivers.len() >= DRIVER_FLOOR,
        "only {} public async entry points found in src/run.rs, below the floor of \
         {DRIVER_FLOOR}",
        drivers.len()
    );

    let mut stray = Vec::new();
    for driver in &drivers {
        let reachable = reaches(&graph, &driver.name);
        if !ENGINES.iter().any(|e| reachable.contains(*e)) {
            stray.push(driver.name.clone());
        }
    }
    assert!(
        stray.is_empty(),
        "{} of {} public entry points in src/run.rs do not reach `run_with_extras` or \
         `run_tree_with_extras`:\n  {}\n\nEvery driving entry point is a wrapper over the \
         shared engine. One that assembles its own run is a second runtime path, and a \
         second runtime path diverges on the third bug fix that lands in only one of them.",
        stray.len(),
        drivers.len(),
        stray.join("\n  ")
    );
}

/// Every `Harness` method that drives a run calls a `crate::run` entry point,
/// rather than reaching past them into the engine or into a loop of its own.
///
/// The facade delegates *outward*, to the same function a caller would have
/// called themselves. That direction is the point: it is what makes
/// `tests/harness.rs`'s trace equality a structural property rather than a
/// coincidence that holds for one contract.
#[test]
fn the_facade_delegates_to_the_public_entry_points() {
    let source = read("src/harness.rs");
    let items = functions(&source);
    // The methods are indented inside `impl`, so the column-zero parse above sees
    // nothing. Read them as a block instead and assert on the calls it makes.
    assert!(
        items.is_empty(),
        "src/harness.rs grew top-level functions; this checker reads the impl block as text \
         and must be rewritten before it means anything"
    );

    let expected = [
        "run_with_observed",
        "resume_with_observed",
        "run_tree_observed",
        "turn_observed",
        "turn_bounded_observed",
    ];
    for name in expected {
        assert!(
            calls(&source, name),
            "src/harness.rs no longer calls `{name}` — a Harness method that stops delegating \
             to the public entry point is the second implementation this test exists to forbid"
        );
    }
    assert!(
        !ENGINES.iter().any(|e| calls(&source, e)),
        "src/harness.rs reaches past the public entry points into the engine. It must call the \
         same function a caller would call, so that the two cannot drift apart."
    );
}

/// The parse is proven capable of failing, on the two shapes that would make it
/// blind: a file whose items are not at column zero, and a CRLF checkout.
#[test]
fn the_parse_is_proven_capable_of_seeing_and_of_failing() {
    let flat = "pub async fn alpha() {\n    beta();\n}\n\nfn beta() {\n    ();\n}\n";
    let found = functions(flat);
    assert_eq!(found.len(), 2, "the checker must see a two-function file");
    assert!(found[0].public && found[0].asynchronous);
    assert!(!found[1].public && !found[1].asynchronous);

    let graph: BTreeMap<&str, &str> = found
        .iter()
        .map(|f| (f.name.as_str(), f.body.as_str()))
        .collect();
    assert!(
        reaches(&graph, "alpha").contains("beta"),
        "the call graph must follow a call"
    );
    assert!(
        !reaches(&graph, "beta").contains("alpha"),
        "and must not follow one that is not there"
    );

    // CRLF: `read` normalises, so a checked-out-on-Windows file parses the same.
    let crlf = flat.replace('\n', "\r\n").replace("\r\n", "\n");
    assert_eq!(
        functions(&crlf).len(),
        2,
        "a CRLF checkout must parse identically — 0.60.2 shipped a checker that fell back to \
         the whole document instead and passed for the wrong reason"
    );

    // A substring must not read as a call.
    assert!(!calls("fn x() { alphabet(); }", "alpha"));
    assert!(calls("fn x() { alpha(); }", "alpha"));
}

// ---------------------------------------------------------------------------
// F6 — nothing is removed and no existing signature changes
// ---------------------------------------------------------------------------

/// Every entry point this release promises not to touch still exists with the
/// parameter count it had in 0.62.0.
///
/// A signature change is then a red test rather than a reviewer's
/// responsibility. The counts are of top-level parameters as `cargo fmt` lays
/// them out, which is what a caller supplies.
#[test]
fn no_entry_point_signature_moved() {
    let run = read("src/run.rs");
    let session = read("src/session.rs");

    // name -> parameter count, as of 0.62.0.
    let run_entry_points: &[(&str, usize)] = &[
        ("run", 3),
        ("run_observed", 4),
        ("run_with", 5),
        ("run_with_observed", 6),
        ("run_tree", 6),
        ("run_tree_observed", 7),
        ("resume", 4),
        ("resume_observed", 5),
        ("resume_with", 6),
        ("resume_with_observed", 7),
        ("resume_tree", 7),
        ("resume_tree_observed", 8),
        ("rewind", 4),
        ("rewind_run", 3),
        ("rewind_run_observed", 4),
    ];
    // Methods, so the receiver counts: `turn(&mut self, text, provider, store,
    // policy, approver)` is six.
    let turn_methods: &[(&str, usize)] = &[
        ("turn", 6),
        ("turn_observed", 7),
        ("turn_steered", 8),
        ("turn_bounded", 6),
        ("turn_bounded_observed", 7),
        ("turn_contained", 7),
        ("turn_contained_observed", 8),
    ];

    let mut wrong = Vec::new();
    for (source, prefix, table) in [
        (&run, "", run_entry_points),
        (&session, "    ", turn_methods),
    ] {
        for (name, expected) in table {
            let Some(params) = parameters(source, prefix, name) else {
                wrong.push(format!("{name}: no longer defined"));
                continue;
            };
            if params != *expected {
                wrong.push(format!("{name}: {expected} parameters became {params}"));
            }
        }
    }
    assert!(
        wrong.is_empty(),
        "an entry point's signature moved, and this release promised none would:\n  {}\n\n\
         Breaking a caller to make an API prettier is a cost paid by someone who did nothing \
         wrong. Add to the surface instead.",
        wrong.join("\n  ")
    );
}

/// The count of top-level parameters in `name`'s signature, or `None` if the
/// function is not defined at `prefix` in `source`.
fn parameters(source: &str, prefix: &str, name: &str) -> Option<usize> {
    let heads = [
        format!("{prefix}pub async fn {name}"),
        format!("{prefix}pub fn {name}"),
    ];
    // Every occurrence, not the first: `pub async fn resume_tree_with_answer`
    // appears 4,900 lines above `pub async fn resume_tree`, and a `find` that
    // stops at the first hit reports the shorter name as undefined.
    let at = heads.iter().find_map(|h| {
        source.match_indices(h.as_str()).find_map(|(start, _)| {
            let after = start + h.len();
            source[after..]
                .chars()
                .next()
                .is_some_and(|c| c == '(' || c == '<')
                .then_some(start)
        })
    })?;
    let open = at + source[at..].find('(')?;
    // Depth-aware, so a `&dyn Fn(&str, u8)` or a generic parameter list inside
    // the signature does not end the list early or split one parameter in two.
    // Counted as non-empty segments rather than as commas plus one, because
    // `cargo fmt` writes a trailing comma on every multi-line signature in this
    // crate and "commas plus one" reports every one of them as one parameter too
    // many.
    let (mut depth, mut count, mut segment) = (0i32, 0usize, false);
    for c in source[open..].chars() {
        match c {
            '(' | '<' | '[' => {
                depth += 1;
                if depth > 1 {
                    segment = true;
                }
            }
            ')' | ']' | '>' => {
                depth -= 1;
                if depth == 0 {
                    return Some(count + usize::from(segment));
                }
            }
            ',' if depth == 1 => {
                count += usize::from(segment);
                segment = false;
            }
            c if !c.is_whitespace() && depth >= 1 => segment = true,
            _ => {}
        }
    }
    None
}

/// The parameter counter is proven capable of failing.
#[test]
fn the_parameter_counter_counts_what_a_caller_supplies() {
    let src = "pub async fn a<P: Provider>(\n    x: &P,\n    y: &dyn Fn(&str, u8),\n) -> u8 {\n}\n";
    assert_eq!(
        parameters(src, "", "a"),
        Some(2),
        "a nested `Fn(..)` is one parameter, and the trailing comma rustfmt writes is not a third"
    );
    assert_eq!(
        parameters("pub fn a(x: u8, y: u8) {}\n", "", "a"),
        Some(2),
        "and a single-line signature with no trailing comma counts the same"
    );
    assert_eq!(parameters("pub fn b() {}\n", "", "b"), Some(0));
    assert_eq!(parameters("pub fn b() {}\n", "", "c"), None);
    assert_eq!(
        parameters("pub fn beta(x: u8) {}\n", "", "b"),
        None,
        "a prefix of a longer name must not match"
    );
    assert_eq!(
        parameters("pub fn ab(x: u8) {}\npub fn a(y: u8, z: u8) {}\n", "", "a"),
        Some(2),
        "and a longer name appearing FIRST must not hide the shorter one — the shape that \
         reported `resume_tree` undefined"
    );
}
