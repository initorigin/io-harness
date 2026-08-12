//! What a run's change is worth in the store (0.51.0).
//!
//! Until this release an edit was two integers, so a trace could say that step 7
//! added four lines to `src/parse.rs` and could not say which. These tests are
//! about the column that answers that, and about the two integers **not
//! moving** while it is added — which is the release's real risk, since
//! computing the counts from the same whole-file texts the hunk needs is the
//! natural tidy-up and would silently renumber every trace ever recorded.
//!
//! Driven end to end through the real loop with a scripted provider, so what is
//! asserted is what a run actually writes.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::Workspace;
use io_harness::{
    rewind_run, rewind_step, rewind_step_observed, run, Edit, EventKind, Flow, Observer, Provider,
    Reverted, RunEvent, Store, TaskContract,
};
use serde_json::json;

/// A provider that returns a fixed script of tool-call responses, one per step,
/// then stops.
struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl Provider for MockScript {
    async fn complete(&self, _req: CompletionRequest) -> io_harness::Result<CompletionResponse> {
        let i = self.at.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            tool_calls: self.steps.get(i).cloned().unwrap_or_default(),
            ..Default::default()
        })
    }
}

fn call(name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall {
        name: name.into(),
        arguments: args,
    }
}

/// Drive a scripted run over a workspace holding one file, and hand back the
/// store, the run id and the directory.
async fn drive(
    file: &str,
    contents: &str,
    steps: Vec<Vec<ToolCall>>,
) -> (Store, i64, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join(file), contents).unwrap();
    let (store, run_id) = drive_in(dir.path(), steps).await;
    (store, run_id, dir)
}

/// The same, over a directory the caller prepared.
async fn drive_in(dir: &std::path::Path, steps: Vec<Vec<ToolCall>>) -> (Store, i64) {
    let contract = TaskContract::workspace("change the file", dir);
    let script = MockScript {
        steps,
        at: AtomicUsize::new(0),
    };
    let store = Store::memory().unwrap();
    let result = run(&contract, &script, &store).await.unwrap();
    (store, result.run_id)
}

/// Apply a stored hunk in reverse, the way `rewind_step` does, using nothing but
/// the public store surface and a text edit — so this asserts the hunk's own
/// content rather than trusting the crate's applier to agree with itself.
///
/// Deliberately hand-rolled: `apply` living in the crate means a test that used
/// it would pass for a renderer and an applier that were wrong in the same
/// direction. This reads the `@@` header, walks to that line of the *after*
/// text, and swaps the `+` lines back for the `-` lines.
fn reverse_apply(after: &str, hunk: &str) -> String {
    let mut lines: Vec<&str> = after.split('\n').collect();
    let ends_nl = after.ends_with('\n');
    if ends_nl {
        lines.pop();
    }
    let mut header = hunk.lines().next().unwrap().split_whitespace();
    header.next().unwrap();
    header.next().unwrap();
    let new_range = header.next().unwrap().strip_prefix('+').unwrap();
    let new_start: usize = new_range.split(',').next().unwrap().parse().unwrap();

    let mut out: Vec<String> = lines[..new_start.saturating_sub(1)]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let mut at = new_start.saturating_sub(1);
    let mut bare = false;
    for line in hunk.split('\n').skip(1) {
        match line.chars().next() {
            Some('\\') => bare = true,
            Some('-') => out.push(line[1..].to_string()),
            Some('+') => {
                assert_eq!(lines[at], &line[1..], "the hunk's + line is what is there");
                at += 1;
            }
            Some(' ') => {
                assert_eq!(lines[at], &line[1..], "the hunk's context is what is there");
                out.push(line[1..].to_string());
                at += 1;
            }
            _ => {}
        }
    }
    out.extend(lines[at..].iter().map(|s| s.to_string()));
    let mut text = out.join("\n");
    // The marker sits after the last line of whichever side it describes; on the
    // way back that side is the old one, so a marker means the restored text has
    // no terminator.
    if ends_nl && !bare {
        text.push('\n');
    }
    text
}

/// F1 — a stored hunk is anchored to the file and reverse-applies byte for byte.
///
/// Four positions, because a hunk computed from the replaced fragment rather
/// than from the file is right by accident at the top of the file and wrong
/// everywhere else. The assertion is the byte-exact round trip, not the header's
/// shape: `@@ -1,1 +1,1 @@` looks like a diff whatever it is anchored to.
#[tokio::test]
async fn a_stored_hunk_is_anchored_to_the_file_and_reverse_applies() {
    let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
    for (what, search, replace) in [
        ("at the start", "one", "ONE"),
        ("in the middle", "five", "FIVE"),
        ("at the end", "eight", "EIGHT"),
    ] {
        let (store, run_id, dir) = drive(
            "f.txt",
            body,
            vec![vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": search, "replace": replace }),
            )]],
        )
        .await;
        let edits = store.edits(run_id).unwrap();
        assert_eq!(edits.len(), 1, "{what}");
        let hunk = edits[0]
            .hunk
            .as_deref()
            .unwrap_or_else(|| panic!("{what}: no hunk was stored"));
        let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
        assert_eq!(after, body.replace(search, replace), "{what}");
        assert_eq!(
            reverse_apply(&after, hunk),
            body,
            "{what}: reversing {hunk:?} must give the file back exactly"
        );
    }

    // The fourth position, and the one a fragment-anchored diff cannot express
    // at all: a file whose last line has no terminator.
    let bare = "alpha\nbeta\ngamma";
    let (store, run_id, dir) = drive(
        "f.txt",
        bare,
        vec![vec![call(
            "edit_file",
            json!({ "path": "f.txt", "search": "gamma", "replace": "GAMMA" }),
        )]],
    )
    .await;
    let hunk = store.edits(run_id).unwrap()[0].hunk.clone().unwrap();
    assert!(
        hunk.contains("\\ No newline at end of file"),
        "a file with no final newline must say so: {hunk:?}"
    );
    let after = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(after, "alpha\nbeta\nGAMMA");
    assert_eq!(reverse_apply(&after, &hunk), bare);
}

/// F1, second half — a run's whole change renders as a step-ordered patch series
/// with one header pair per edit.
#[tokio::test]
async fn a_runs_change_renders_as_a_step_ordered_patch_series() {
    let (store, run_id, _dir) = drive(
        "f.txt",
        "one\ntwo\nthree\n",
        vec![
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "one", "replace": "ONE" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "three", "replace": "THREE" }),
            )],
        ],
    )
    .await;
    let patch = store.patch(run_id).unwrap();
    assert_eq!(
        patch.matches("--- a/f.txt").count(),
        2,
        "one header pair per edit, because the second hunk's line numbers are \
         only right once the first has been applied: {patch}"
    );
    let one = patch.find("-one").unwrap();
    let three = patch.find("-three").unwrap();
    assert!(one < three, "step order, not path order: {patch}");
}

/// F2 — the line counts do not move, and this is the negative control the
/// release rests on.
///
/// The numbers below are this build's, and they are the numbers 0.18.0 through
/// 0.50.0 recorded. Measuring them from the whole-file texts the hunk is
/// computed from is the tidy-up that would change them, and it is this test's
/// sabotage.
///
/// **The last two cases are the ones that make this a control rather than a
/// decoration, and they were found by brute force rather than by reasoning.**
/// For most edits the two measures happen to agree — trimming the common head
/// and tail of the fragment finds the same changed span as trimming them over
/// the whole file — and a corpus of tidy whole-line edits therefore passes under
/// the sabotage. They diverge exactly when the replacement does not begin and
/// end on a line boundary: deleting a substring *inside* a line is nothing added
/// and one line removed to the fragment, and one line added and one removed to
/// the file, because the file still has a line there.
#[tokio::test]
async fn the_line_counts_are_what_they_have_been_since_0_18_0() {
    let plain = "one\ntwo\nthree\nfour\n";
    /// what it proves, the file it starts from, the script, and the counts this
    /// build records.
    type Case = (&'static str, &'static str, Vec<ToolCall>, (u64, u64));
    let cases: Vec<Case> = vec![
        (
            "a one-line replacement is one out and one in",
            plain,
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "two", "replace": "TWO" }),
            )],
            (1, 1),
        ),
        (
            "an insertion adds and removes nothing else",
            plain,
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "two\n", "replace": "two\nTWO AND A HALF\n" }),
            )],
            (1, 0),
        ),
        (
            "deleting a whole line removes and adds nothing",
            plain,
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "two\n", "replace": "" }),
            )],
            (0, 1),
        ),
        (
            "a whole-file write is measured over the whole file",
            plain,
            vec![call(
                "write_file",
                json!({ "path": "f.txt", "content": "one\nTWO\nthree\nfour\n" }),
            )],
            (1, 1),
        ),
        (
            "rewriting a file with what it already held is neither",
            plain,
            vec![call(
                "write_file",
                json!({ "path": "f.txt", "content": plain }),
            )],
            (0, 0),
        ),
        (
            "deleting inside a line removes one and adds none — the file still \
             has a line there, and measuring the file would say one and one",
            "hello world\n",
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": " world", "replace": "" }),
            )],
            (0, 1),
        ),
        (
            "splitting a line off-boundary is measured over the fragment's two \
             lines, not the file's two",
            "one\ntwo\n",
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "one\n", "replace": "1\none" }),
            )],
            (1, 0),
        ),
    ];
    for (what, body, script, expected) in cases {
        let (store, run_id, _dir) = drive("f.txt", body, vec![script]).await;
        let edits = store.edits(run_id).unwrap();
        assert_eq!(
            (edits[0].lines_added, edits[0].lines_removed),
            expected,
            "{what}"
        );
    }
}

/// F2, the other half — an unchanged write stores no hunk, because there is no
/// change to store, and that is distinct from a hunk that could not be rendered.
#[tokio::test]
async fn a_write_that_changes_nothing_stores_no_hunk() {
    let body = "unchanged\n";
    let (store, run_id, _dir) = drive(
        "f.txt",
        body,
        vec![vec![call(
            "write_file",
            json!({ "path": "f.txt", "content": body }),
        )]],
    )
    .await;
    let edits = store.edits(run_id).unwrap();
    assert_eq!(edits.len(), 1, "the write is still recorded");
    assert_eq!(edits[0].hunk, None);
    assert!(
        store.patch(run_id).unwrap().contains("no hunk stored"),
        "the series says the change is missing rather than omitting it silently"
    );
}

/// `Edit::measure` is unchanged and its `hunk` starts empty — the two
/// computations are separate, which is the whole of F2's design.
#[test]
fn measure_alone_carries_no_hunk() {
    let edit = Edit::measure(1, "edit_file", "a.rs", "fn one() {}\n", "fn two() {}\n");
    assert_eq!((edit.lines_added, edit.lines_removed), (1, 1));
    assert_eq!(edit.hunk, None);
}

// ---------------------------------------------------------------------------
// F7 / F8 / F9 / F10 — walking a run back a step at a time
// ---------------------------------------------------------------------------

/// F7 — a newest-first walk restores every intermediate state exactly.
///
/// Asserted at each intermediate state and not only at the end. A build that
/// reverse-applies in the wrong order can still arrive at the right final text
/// when the changes do not overlap, so the end alone proves nothing.
#[tokio::test]
async fn reverting_newest_first_restores_every_intermediate_state() {
    let start = "alpha\nbeta\ngamma\ndelta\n";
    let (store, run_id, dir) = drive(
        "f.txt",
        start,
        vec![
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "alpha", "replace": "ALPHA" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "beta", "replace": "BETA" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "delta", "replace": "DELTA" }),
            )],
        ],
    )
    .await;

    let read = || std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(read(), "ALPHA\nBETA\ngamma\nDELTA\n");

    let ws = Workspace::new(dir.path());
    let steps: Vec<u32> = store
        .edits(run_id)
        .unwrap()
        .iter()
        .map(|e| e.step)
        .collect();
    assert_eq!(steps.len(), 3, "three edits, one per step: {steps:?}");

    // Newest first, and the state after each revert is what the file held at the
    // end of the preceding step.
    let expected = [
        "ALPHA\nBETA\ngamma\ndelta\n",
        "ALPHA\nbeta\ngamma\ndelta\n",
        start,
    ];
    for (i, step) in steps.iter().rev().enumerate() {
        let done = rewind_step(&ws, &store, run_id, *step).unwrap();
        assert_eq!(done.len(), 1, "step {step} wrote one path");
        assert!(
            matches!(done[0].1, Reverted::Applied(_)),
            "step {step}: {:?}",
            done[0].1
        );
        assert_eq!(read(), expected[i], "after reverting step {step}");
    }

    // And the end state is what a whole-run rewind would have produced.
    assert_eq!(read(), start);
}

/// F8 — an out-of-order revert reports `Stale` and touches nothing.
///
/// The two edits overlap deliberately: the second rewrote the line the first
/// produced, so the first's hunk no longer has its context to find. A fuzzy
/// match would "succeed" here and corrupt the file, which is why an exact match
/// or nothing is the rule.
#[tokio::test]
async fn reverting_out_of_order_reports_stale_and_changes_nothing() {
    let (store, run_id, dir) = drive(
        "f.txt",
        "one\ntwo\n",
        vec![
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "one", "replace": "ONE" }),
            )],
            vec![call(
                "edit_file",
                json!({ "path": "f.txt", "search": "ONE", "replace": "UNO" }),
            )],
        ],
    )
    .await;
    let before = std::fs::read_to_string(dir.path().join("f.txt")).unwrap();
    assert_eq!(before, "UNO\ntwo\n");

    let ws = Workspace::new(dir.path());
    let first = store.edits(run_id).unwrap()[0].step;
    let done = rewind_step(&ws, &store, run_id, first).unwrap();
    assert!(
        matches!(done[0].1, Reverted::Stale(_)),
        "the older hunk's context is gone: {:?}",
        done[0].1
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).unwrap(),
        before,
        "a stale revert must leave the file byte-identical"
    );
}

/// F9 — an edit with no stored hunk is reported, not treated as empty.
///
/// The absence is produced the way production produces it: the file's previous
/// contents were not text, so there was nothing to diff against and no hunk was
/// stored. (The other cause — a row written before 0.51.0 — is a column that did
/// not exist, which only a store from an earlier release can demonstrate, and
/// `tests/cross_version.rs` is where that lives.)
///
/// Treating an absent hunk as an empty patch would report success having undone
/// nothing, which is the one way this feature can silently lose an operator's
/// work.
#[tokio::test]
async fn an_edit_with_no_stored_hunk_is_reported_rather_than_skipped() {
    let dir = tempfile::tempdir().unwrap();
    // Not valid UTF-8, so `read_before` keeps no restore text and there is
    // nothing for a diff to be against.
    std::fs::write(dir.path().join("f.dat"), [0xffu8, 0xfe, 0x00, 0x41]).unwrap();

    let (store, run_id) = drive_in(
        dir.path(),
        vec![vec![call(
            "write_file",
            json!({ "path": "f.dat", "content": "now it is text\n" }),
        )]],
    )
    .await;
    let edits = store.edits(run_id).unwrap();
    assert_eq!(edits.len(), 1);
    assert_eq!(edits[0].hunk, None, "nothing to diff against, so no hunk");

    let ws = Workspace::new(dir.path());
    let done = rewind_step(&ws, &store, run_id, edits[0].step).unwrap();
    assert!(
        matches!(done[0].1, Reverted::NoHunk(_)),
        "an absent hunk is absent, not empty: {:?}",
        done[0].1
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.dat")).unwrap(),
        "now it is text\n",
        "nothing was changed"
    );
    // And the series says the change is there and unrenderable, rather than
    // leaving it out.
    assert!(store.patch(run_id).unwrap().contains("no hunk stored"));
}

/// F10 — a step revert is distinguishable in the trace from a run rewind, and the
/// event round-trips with no duplicate key.
#[tokio::test]
async fn a_step_revert_and_a_run_rewind_are_different_rows() {
    let (store, run_id, dir) = drive(
        "f.txt",
        "one\ntwo\n",
        vec![vec![call(
            "edit_file",
            json!({ "path": "f.txt", "search": "one", "replace": "ONE" }),
        )]],
    )
    .await;
    let ws = Workspace::new(dir.path());
    let step = store.edits(run_id).unwrap()[0].step;

    let seen = Seen::default();
    rewind_step_observed(&ws, &store, run_id, step, &seen).unwrap();
    rewind_run(&ws, &store, run_id).unwrap();

    let rows = store.rewinds(run_id).unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].undid_step,
        Some(step),
        "the step revert names its step"
    );
    assert_eq!(rows[1].undid_step, None, "a whole-run rewind names none");

    // Exactly one event, its count taken from the value being returned.
    assert_eq!(*seen.0.lock().unwrap(), [(step, 1u32)]);
}

/// An observer that keeps every `Reverted` it is handed.
#[derive(Default)]
struct Seen(std::sync::Mutex<Vec<(u32, u32)>>);

impl Observer for Seen {
    fn event(&self, e: &RunEvent) -> Flow {
        if let EventKind::Reverted { undid_step, files } = &e.kind {
            self.0.lock().unwrap().push((*undid_step, *files));
        }
        Flow::Continue
    }
}
