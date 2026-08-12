//! Unified diffs: rendering one, parsing one, applying one, and reversing one
//! (0.51.0).
//!
//! Crate-internal, and four functions rather than a type with a lifecycle,
//! because the four callers want four different halves of it: [`render`] is what
//! the store keeps for every edit, [`parse`] and [`apply`] are what the
//! `patch_file` tool does with a diff a model wrote, and [`reverse`] is what
//! turns a stored hunk back into an undo.
//!
//! **The differ is a common-head/common-tail walk, not a Myers diff, and that is
//! a decision rather than a shortcut.** It is the computation
//! [`crate::state::Edit::measure`] already performs to count the lines, so
//! storing the hunk costs the same walk a second time and no dependency. For an
//! `edit_file` — one contiguous replacement by construction — it *is* the
//! minimal diff. For a `write_file` that rewrote two distant regions of a file
//! it is one hunk spanning both: a valid unified diff that reverse-applies
//! exactly, and not the shortest one. A minimal diff is several hundred lines of
//! algorithm or a dependency, and it buys shorter output rather than a
//! capability this crate does not have.
//!
//! **Line endings are content, deliberately.** Text is split on `\n` and a `\r`
//! stays on the end of the line it terminated, so a CRLF file's diff round-trips
//! byte for byte on every platform and nothing here has to know which host it is
//! running on. The one case that needs its own machinery is the *absence* of a
//! final newline, which unified diff spells with a `\ No newline at end of file`
//! marker line, and which is carried per body line rather than per hunk because
//! the two sides of a hunk can disagree about it.

use crate::{Error, Result};

/// The number of unchanged lines shown either side of a change.
///
/// Three, as every unified diff since `diff -u` has used, so the output is
/// readable by a human and by `patch` without either being told anything.
const CONTEXT: usize = 3;

/// The marker unified diff uses for a file whose last line has no terminator.
const NO_NEWLINE: &str = "\\ No newline at end of file";

/// What a body line of a hunk is.
///
/// Three variants and not two, because a context line is neither an addition nor
/// a removal and [`reverse`] has to leave it exactly where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Present on both sides, shown for anchoring.
    Context,
    /// Present before and not after.
    Removed,
    /// Present after and not before.
    Added,
}

/// One line of a hunk's body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) kind: Kind,
    /// The line's text, without its terminator. A `\r` from a CRLF file is part
    /// of it.
    pub(crate) text: String,
    /// This line is the last line of the side(s) it belongs to, and that side's
    /// text does not end with a newline.
    pub(crate) no_newline: bool,
}

/// One hunk: where it sits in each side and what it does there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Hunk {
    /// 1-based first line of the range on the old side. `0` when the old range
    /// is empty, which is what `diff` emits for an insertion into an empty file.
    pub(crate) old_start: usize,
    pub(crate) old_count: usize,
    pub(crate) new_start: usize,
    pub(crate) new_count: usize,
    pub(crate) body: Vec<Line>,
}

/// Split text into lines, and say whether it ended with a newline.
///
/// The bool is not derivable from the `Vec` and is the whole of the
/// no-final-newline problem: `"a\n"` and `"a"` split to the same one line.
fn split(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        // No lines, and joining nothing must give back the empty string rather
        // than a lone newline — so "ends with a newline" is the honest flag for
        // a text that has no last line to be missing one.
        return (Vec::new(), true);
    }
    let ends = text.ends_with('\n');
    let mut lines: Vec<&str> = text.split('\n').collect();
    if ends {
        lines.pop();
    }
    (lines, ends)
}

/// Put lines back together. The inverse of [`split`] for every input.
fn join(lines: &[String], ends: bool) -> String {
    let mut out = lines.join("\n");
    if ends && !lines.is_empty() {
        out.push('\n');
    }
    out
}

/// Render the change `new` makes to `old` as a unified diff body, or `None` when
/// there is no change.
///
/// The header lines (`--- a/path`, `+++ b/path`) are the caller's, because the
/// caller is the only one that knows the path and whether it wants them at all —
/// [`crate::state::Store::patch`] writes them and the stored hunk does not carry
/// them twice.
pub(crate) fn render(old: &str, new: &str) -> Option<String> {
    if old == new {
        return None;
    }
    let (old_lines, old_nl) = split(old);
    let (new_lines, new_nl) = split(new);

    // **A line's identity includes whether it is terminated**, which is what
    // `diff` itself compares and what a naive text-only comparison gets wrong.
    // An unterminated last line is not the same line as the identically-spelled
    // terminated one: treating them as equal renders a hunk of pure context that
    // changes nothing, or drops the marker that says where the terminator went.
    let old_bare = |i: usize| !old_nl && i + 1 == old_lines.len();
    let new_bare = |j: usize| !new_nl && j + 1 == new_lines.len();
    let same = |i: usize, j: usize| old_lines[i] == new_lines[j] && old_bare(i) == new_bare(j);

    let mut head = 0;
    while head < old_lines.len() && head < new_lines.len() && same(head, head) {
        head += 1;
    }
    let mut tail = 0;
    while tail < old_lines.len() - head
        && tail < new_lines.len() - head
        && same(old_lines.len() - 1 - tail, new_lines.len() - 1 - tail)
    {
        tail += 1;
    }

    let before = head.saturating_sub(CONTEXT);
    let old_after = (old_lines.len() - tail + CONTEXT).min(old_lines.len());
    let new_after = (new_lines.len() - tail + CONTEXT).min(new_lines.len());

    let mut body = Vec::new();
    let mut push = |kind: Kind, text: &str, no_newline: bool| {
        body.push(Line {
            kind,
            text: text.to_string(),
            no_newline,
        })
    };

    // A context line exists only where `same` held, so the two sides agree about
    // the terminator and one flag answers for both.
    for i in before..head {
        push(Kind::Context, old_lines[i], old_bare(i));
    }
    for (i, line) in old_lines
        .iter()
        .enumerate()
        .take(old_lines.len() - tail)
        .skip(head)
    {
        push(Kind::Removed, line, old_bare(i));
    }
    for (j, line) in new_lines
        .iter()
        .enumerate()
        .take(new_lines.len() - tail)
        .skip(head)
    {
        push(Kind::Added, line, new_bare(j));
    }
    for i in (old_lines.len() - tail)..old_after {
        push(Kind::Context, old_lines[i], old_bare(i));
    }
    let old_count = old_after - before;
    let new_count = new_after - before;
    let hunk = Hunk {
        // A zero-length range is anchored at the line *before* the insertion
        // point, which is what `diff` emits and what `patch` reads.
        old_start: if old_count == 0 { before } else { before + 1 },
        old_count,
        new_start: if new_count == 0 { before } else { before + 1 },
        new_count,
        body,
    };
    Some(write(&[hunk]))
}

/// Render hunks back to unified-diff text. The inverse of [`parse`].
pub(crate) fn write(hunks: &[Hunk]) -> String {
    let mut out = String::new();
    for h in hunks {
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            h.old_start, h.old_count, h.new_start, h.new_count
        ));
        for line in &h.body {
            out.push(match line.kind {
                Kind::Context => ' ',
                Kind::Removed => '-',
                Kind::Added => '+',
            });
            out.push_str(&line.text);
            out.push('\n');
            if line.no_newline {
                out.push_str(NO_NEWLINE);
                out.push('\n');
            }
        }
    }
    out
}

/// Parse a unified diff body into hunks.
///
/// Refuses by name rather than skipping: a line this does not understand is an
/// error naming it, because a parser that ignores what it cannot read applies a
/// patch that is not the one it was given.
///
/// Everything before the first `@@` is ignored, which is what `patch(1)` does
/// and what lets a real `git diff` be pasted in whole. It cannot redirect
/// anything: the path is the tool's own argument, so a `+++` header naming a
/// different file is read by nobody.
///
/// Split on `\n` by hand rather than with [`str::lines`], which strips a
/// trailing `\r` — on a CRLF file that would silently delete the carriage return
/// from every line of the patch and write back a file with mixed endings.
pub(crate) fn parse(patch: &str) -> Result<Vec<Hunk>> {
    let mut hunks: Vec<Hunk> = Vec::new();
    for (n, raw) in patch
        .strip_suffix('\n')
        .unwrap_or(patch)
        .split('\n')
        .enumerate()
    {
        let at = n + 1;
        if let Some(rest) = raw.strip_prefix("@@ ") {
            hunks.push(header(rest, at)?);
            continue;
        }
        if raw == NO_NEWLINE || raw == "\\ No newline at end of file\r" {
            let Some(line) = hunks.last_mut().and_then(|h| h.body.last_mut()) else {
                return Err(bad(
                    at,
                    "a no-newline marker before any line it could apply to",
                ));
            };
            line.no_newline = true;
            continue;
        }
        let Some(h) = hunks.last_mut() else {
            // Preamble: `diff --git`, `index`, `---`, `+++`, mode lines, or a
            // model's sentence of explanation. Skipped rather than refused, so a
            // patch with no `@@` at all reaches the one error that says so.
            continue;
        };
        let kind = match raw.chars().next() {
            Some(' ') => Kind::Context,
            Some('-') => Kind::Removed,
            Some('+') => Kind::Added,
            // An empty line inside a hunk is a context line whose trailing space
            // an editor ate. Every real diff tool accepts it and refusing would
            // reject patches that are correct.
            None => Kind::Context,
            Some(_) => return Err(bad(at, &format!("{raw:?}, which is not a diff line"))),
        };
        h.body.push(Line {
            kind,
            text: raw.get(1..).unwrap_or_default().to_string(),
            no_newline: false,
        });
    }
    if hunks.is_empty() {
        return Err(Error::Config(
            "that patch has no @@ hunk header, so there is nothing to apply; a unified diff \
             looks like \"@@ -1,3 +1,4 @@\" followed by lines prefixed with a space, a minus \
             or a plus"
                .into(),
        ));
    }
    for (i, h) in hunks.iter().enumerate() {
        let old = h.body.iter().filter(|l| l.kind != Kind::Added).count();
        let new = h.body.iter().filter(|l| l.kind != Kind::Removed).count();
        if old != h.old_count || new != h.new_count {
            return Err(Error::Config(format!(
                "hunk {} says it covers {} old and {} new lines and its body has {old} and \
                 {new}; nothing was changed",
                i + 1,
                h.old_count,
                h.new_count
            )));
        }
    }
    Ok(hunks)
}

fn bad(line: usize, what: &str) -> Error {
    Error::Config(format!(
        "that patch could not be read: line {line} is {what}; nothing was changed"
    ))
}

/// `-a,b +c,d @@` — the part of a hunk header after the opening `@@ `.
fn header(rest: &str, at: usize) -> Result<Hunk> {
    let mut parts = rest.split_whitespace();
    let old = parts
        .next()
        .and_then(|s| s.strip_prefix('-'))
        .ok_or_else(|| bad(at, "a hunk header with no -old range"))?;
    let new = parts
        .next()
        .and_then(|s| s.strip_prefix('+'))
        .ok_or_else(|| bad(at, "a hunk header with no +new range"))?;
    // A range with no comma is one line, which is what `diff` emits for a
    // single-line range and what a model copying one will reproduce.
    let range = |s: &str| -> Result<(usize, usize)> {
        let (start, count) = match s.split_once(',') {
            Some((a, b)) => (a, b),
            None => (s, "1"),
        };
        Ok((
            start
                .parse()
                .map_err(|_| bad(at, "a hunk header whose start is not a number"))?,
            count
                .parse()
                .map_err(|_| bad(at, "a hunk header whose count is not a number"))?,
        ))
    };
    let (old_start, old_count) = range(old)?;
    let (new_start, new_count) = range(new)?;
    Ok(Hunk {
        old_start,
        old_count,
        new_start,
        new_count,
        body: Vec::new(),
    })
}

/// Turn hunks into the hunks that undo them.
///
/// Every field swaps sides and every body line swaps `-` for `+`; a context line
/// and its no-newline marker stay exactly where they are, because a line present
/// on both sides is present on both sides either way round.
pub(crate) fn reverse(hunks: &[Hunk]) -> Vec<Hunk> {
    hunks
        .iter()
        .map(|h| Hunk {
            old_start: h.new_start,
            old_count: h.new_count,
            new_start: h.old_start,
            new_count: h.old_count,
            body: h
                .body
                .iter()
                .map(|l| Line {
                    kind: match l.kind {
                        Kind::Context => Kind::Context,
                        Kind::Removed => Kind::Added,
                        Kind::Added => Kind::Removed,
                    },
                    text: l.text.clone(),
                    no_newline: l.no_newline,
                })
                .collect(),
        })
        .collect()
}

/// Apply hunks to `text`, or refuse and change nothing.
///
/// **Every hunk is matched against the original text at its own recorded
/// position, and the result is composed once at the end.** Applying them one at
/// a time to a text that is being rewritten underneath them is the obvious
/// spelling and it is wrong: the second hunk's line numbers are the original
/// file's, so a running rewrite lands it wherever the first hunk's size change
/// left it.
///
/// The match is exact — the context and removed lines must be the lines that are
/// there — because a fuzzy match on an undo is how a file gets quietly corrupted.
/// A patch that does not fit is an error naming the hunk and the line it
/// expected, which a model recovers from by reading the file again.
pub(crate) fn apply(text: &str, hunks: &[Hunk]) -> Result<String> {
    let (lines, ends_nl) = split(text);
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut cursor = 0usize;
    // The result's own final newline. Unchanged unless a hunk reaches the end of
    // the file and says otherwise.
    let mut out_nl = ends_nl;

    for (i, h) in hunks.iter().enumerate() {
        let n = i + 1;
        // A zero-length old range is anchored at the line before the insertion,
        // so the first line it touches is `old_start` itself rather than
        // `old_start - 1`.
        let start = if h.old_count == 0 {
            h.old_start
        } else {
            h.old_start
                .checked_sub(1)
                .ok_or_else(|| mismatch(n, "starts at line 0 but covers lines"))?
        };
        if start < cursor {
            return Err(mismatch(
                n,
                "overlaps the hunk before it, or the hunks are out of order",
            ));
        }
        if start > lines.len() {
            return Err(mismatch(
                n,
                &format!(
                    "starts at line {} and the file has {} lines",
                    h.old_start,
                    lines.len()
                ),
            ));
        }
        out.extend(lines[cursor..start].iter().map(|s| s.to_string()));

        let mut at = start;
        for line in &h.body {
            match line.kind {
                Kind::Added => out.push(line.text.clone()),
                Kind::Context | Kind::Removed => {
                    let found = lines.get(at).ok_or_else(|| {
                        mismatch(
                            n,
                            &format!("expects {:?} past the end of the file", line.text),
                        )
                    })?;
                    if *found != line.text {
                        return Err(mismatch(
                            n,
                            &format!(
                                "expects {:?} at line {} and the file has {:?}",
                                line.text,
                                at + 1,
                                found
                            ),
                        ));
                    }
                    // The removed side's no-newline marker describes the file
                    // being patched: if it says the last line has no terminator,
                    // it must be the last line and the text must indeed lack one.
                    if line.no_newline
                        && line.kind == Kind::Removed
                        && (at + 1 != lines.len() || ends_nl)
                    {
                        return Err(mismatch(
                            n,
                            "says the file has no final newline and it does",
                        ));
                    }
                    if line.kind == Kind::Context {
                        out.push(line.text.clone());
                    }
                    at += 1;
                }
            }
        }
        // Whether the *result* ends with a newline is decided by the last body
        // line that survives into it, and only when this hunk reaches the end.
        if at == lines.len() {
            out_nl = !h
                .body
                .iter()
                .rev()
                .find(|l| l.kind != Kind::Removed)
                .is_some_and(|l| l.no_newline);
        }
        cursor = at;
    }
    out.extend(lines[cursor..].iter().map(|s| s.to_string()));
    Ok(join(&out, out_nl))
}

fn mismatch(hunk: usize, what: &str) -> Error {
    Error::Config(format!(
        "hunk {hunk} does not fit: it {what}. Nothing was changed — read the file again and \
         write the patch against what is actually there"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render, apply, and get back what you started with. The property the whole
    /// module exists to have, and the one a diff that reads correctly can still
    /// fail.
    fn round_trip(old: &str, new: &str) {
        let patch = render(old, new).expect("a change renders a hunk");
        let hunks = parse(&patch).expect("what we rendered, we can read");
        assert_eq!(
            apply(old, &hunks).unwrap(),
            new,
            "forward apply of {patch:?}"
        );
        assert_eq!(
            apply(new, &reverse(&hunks)).unwrap(),
            old,
            "reverse apply of {patch:?}"
        );
    }

    #[test]
    fn a_change_round_trips_at_every_position_in_a_file() {
        let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        round_trip(body, &body.replace("one", "ONE"));
        round_trip(body, &body.replace("four", "FOUR"));
        round_trip(body, &body.replace("eight", "EIGHT"));
    }

    #[test]
    fn a_file_with_no_final_newline_round_trips_in_both_directions() {
        round_trip("one\ntwo\nthree", "one\nTWO\nthree");
        round_trip("one\ntwo\nthree", "one\ntwo\nTHREE");
        // Gaining and losing the terminator, which is a change even where every
        // line's text is identical.
        round_trip("one\ntwo", "one\ntwo\n");
        round_trip("one\ntwo\n", "one\ntwo");
    }

    #[test]
    fn a_crlf_file_round_trips_byte_for_byte() {
        let old = "one\r\ntwo\r\nthree\r\n";
        let new = "one\r\nTWO\r\nthree\r\n";
        round_trip(old, new);
        let patch = render(old, new).unwrap();
        assert!(
            patch.contains("-two\r\n"),
            "the carriage return is content, not a terminator: {patch:?}"
        );
    }

    #[test]
    fn creating_and_emptying_a_file_round_trip() {
        round_trip("", "one\ntwo\n");
        round_trip("one\ntwo\n", "");
    }

    #[test]
    fn an_unchanged_file_renders_no_hunk() {
        assert_eq!(render("same\n", "same\n"), None);
        assert_eq!(render("", ""), None);
    }

    #[test]
    fn the_header_names_the_files_own_line_numbers() {
        let body = "one\ntwo\nthree\nfour\nfive\nsix\nseven\neight\n";
        let patch = render(body, &body.replace("six", "SIX")).unwrap();
        // Three lines of context before line 6 puts the hunk at line 3.
        assert!(
            patch.starts_with("@@ -3,6 +3,6 @@\n"),
            "the hunk must be anchored where the change is: {patch:?}"
        );
    }

    #[test]
    fn a_multi_hunk_patch_applies_at_its_own_offsets() {
        let old = (1..=30).map(|n| format!("line {n}\n")).collect::<String>();
        let new = old
            .replace("line 5\n", "FIVE\n")
            .replace("line 25\n", "TWENTY-FIVE\nAND A HALF\n");
        // Two separate hunks, written as one patch, whose second range is only
        // correct if it is matched against the original file.
        let patch = format!(
            "{}{}",
            render(&old, &old.replace("line 5\n", "FIVE\n")).unwrap(),
            {
                let h = parse(
                    &render(&old, &old.replace("line 25\n", "TWENTY-FIVE\nAND A HALF\n")).unwrap(),
                )
                .unwrap();
                write(&h)
            }
        );
        let hunks = parse(&patch).unwrap();
        assert_eq!(hunks.len(), 2);
        assert_eq!(apply(&old, &hunks).unwrap(), new);
    }

    #[test]
    fn a_hunk_that_does_not_fit_refuses_and_names_what_it_expected() {
        let hunks = parse("@@ -1,1 +1,1 @@\n-nothing like this\n+something else\n").unwrap();
        let e = apply("one\ntwo\n", &hunks).unwrap_err().to_string();
        assert!(e.contains("hunk 1 does not fit"), "{e}");
        assert!(e.contains("nothing like this"), "{e}");
        assert!(e.contains("Nothing was changed"), "{e}");
    }

    #[test]
    fn out_of_order_hunks_are_refused_rather_than_applied() {
        let hunks = parse("@@ -5,1 +5,1 @@\n-five\n+FIVE\n@@ -1,1 +1,1 @@\n-one\n+ONE\n").unwrap();
        let text = "one\ntwo\nthree\nfour\nfive\n";
        let e = apply(text, &hunks).unwrap_err().to_string();
        assert!(e.contains("out of order"), "{e}");
    }

    #[test]
    fn a_body_that_disagrees_with_its_own_header_is_refused_at_parse_time() {
        let e = parse("@@ -1,5 +1,1 @@\n-one\n+ONE\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("says it covers 5 old"), "{e}");
    }

    #[test]
    fn a_patch_with_no_hunk_header_is_refused() {
        let e = parse("just some text\n").unwrap_err().to_string();
        assert!(e.contains("no @@ hunk header"), "{e}");
        let e = parse("").unwrap_err().to_string();
        assert!(e.contains("no @@ hunk header"), "{e}");
    }

    #[test]
    fn a_line_that_is_not_a_diff_line_is_named_rather_than_skipped() {
        let e = parse("@@ -1,1 +1,1 @@\n-one\n!what is this\n+ONE\n")
            .unwrap_err()
            .to_string();
        assert!(e.contains("line 3"), "{e}");
        assert!(e.contains("not a diff line"), "{e}");
    }

    #[test]
    fn file_headers_before_the_first_hunk_are_ignored_and_cannot_redirect_the_write() {
        let hunks = parse(
            "--- a/somewhere/else.rs\n+++ b/somewhere/else.rs\n@@ -1,1 +1,1 @@\n-one\n+ONE\n",
        )
        .unwrap();
        assert_eq!(hunks.len(), 1);
        assert_eq!(apply("one\n", &hunks).unwrap(), "ONE\n");
    }

    #[test]
    fn a_single_line_range_with_no_comma_is_one_line() {
        let hunks = parse("@@ -1 +1 @@\n-one\n+ONE\n").unwrap();
        assert_eq!((hunks[0].old_count, hunks[0].new_count), (1, 1));
        assert_eq!(apply("one\n", &hunks).unwrap(), "ONE\n");
    }

    #[test]
    fn a_no_newline_marker_that_disagrees_with_the_file_is_refused() {
        let hunks = parse("@@ -1,1 +1,1 @@\n-one\n\\ No newline at end of file\n+ONE\n").unwrap();
        let e = apply("one\n", &hunks).unwrap_err().to_string();
        assert!(e.contains("no final newline"), "{e}");
        assert_eq!(apply("one", &hunks).unwrap(), "ONE\n");
    }

    /// Two thousand generated pairs, every one of which must round-trip both
    /// ways.
    ///
    /// The hand-written cases above each encode a failure mode somebody thought
    /// of. This one exists for the ones nobody did: a diff is exactly the kind of
    /// code that is right on the four shapes its author pictured and wrong on the
    /// fifth. The generator is a plain LCG with a fixed seed, so a failure here is
    /// reproducible by running the test again — no dependency, no randomness the
    /// next run does not have.
    #[test]
    fn generated_pairs_round_trip_in_both_directions() {
        fn next(seed: &mut u64) -> usize {
            *seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (*seed >> 33) as usize
        }
        // A tiny alphabet, so lines repeat and the head/tail walk meets the
        // ambiguity a real file has.
        const WORDS: [&str; 6] = ["a", "b", "c", "a\r", "", "  d"];
        fn build(seed: &mut u64) -> String {
            let n = next(seed) % 9;
            let nl = next(seed) % 2 == 0;
            let mut s = String::new();
            for _ in 0..n {
                s.push_str(WORDS[next(seed) % WORDS.len()]);
                s.push('\n');
            }
            if !nl {
                s.pop();
            }
            s
        }
        let seed = &mut 0x5153_1959u64;
        for case in 0..2000 {
            let old = build(seed);
            let new = build(seed);
            if old == new {
                continue;
            }
            let patch = render(&old, &new)
                .unwrap_or_else(|| panic!("case {case}: {old:?} -> {new:?} rendered no hunk"));
            let hunks = parse(&patch)
                .unwrap_or_else(|e| panic!("case {case}: {patch:?} did not parse: {e}"));
            assert_eq!(
                apply(&old, &hunks).unwrap_or_else(|e| panic!("case {case}: forward: {e}")),
                new,
                "case {case}: forward apply of {patch:?} to {old:?}"
            );
            assert_eq!(
                apply(&new, &reverse(&hunks))
                    .unwrap_or_else(|e| panic!("case {case}: reverse: {e}")),
                old,
                "case {case}: reverse apply of {patch:?} to {new:?}"
            );
        }
    }

    #[test]
    fn reversing_twice_is_the_identity() {
        let hunks = parse("@@ -1,2 +1,2 @@\n one\n-two\n+TWO\n").unwrap();
        assert_eq!(reverse(&reverse(&hunks)), hunks);
    }
}
