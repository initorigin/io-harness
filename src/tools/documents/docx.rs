#![cfg(feature = "docx")]
//! Word documents: extract the text of an existing one, generate a new one.
//!
//! One crate, `docx-rs`, doing two jobs that share nothing — its reader builds a
//! tree from an existing `.docx`, its builder emits a fresh one.
//!
//! **Every byte in and out goes through the [`Workspace`].** [`read_text`] parses
//! the bytes [`Workspace::read_bytes`] returned, [`write_new`] packs into an
//! in-memory [`Cursor`] and hands the buffer to [`Workspace::write_bytes`].
//! Nothing here opens, creates, reads or writes a path itself, so a document is
//! governed by exactly the policy that governs a source file.
//!
//! # There is deliberately no edit
//!
//! [`write_new`] generates; it does not merge. The obvious missing third
//! function — change a paragraph of an existing document — is *not* here, and it
//! is not an oversight or a later task. `docx-rs`'s reader is the weaker half of
//! its round trip: it models the OOXML it knows and drops what it does not, so
//! read-then-write is not an edit but a lossy rewrite. On a document this harness
//! generated that costs nothing; on a user's real document — one with a tracked
//! comment thread, a content control, a field, a shape, a vendor extension — it
//! silently deletes the parts the reader could not name. An agent editing a
//! document the user cares about is exactly where that is unacceptable, so the
//! capability is not claimed rather than claimed and unsafe. The spreadsheet
//! module's [`set_cell`](super::xlsx::set_cell) exists because `umya-spreadsheet`
//! genuinely round-trips a workbook it did not create; nothing in the Word
//! ecosystem earns the same call yet.

use std::io::Cursor;

use docx_rs::{
    read_docx_with_options, DocumentChild, Docx, InsertChild, MoveToChild, Paragraph,
    ParagraphChild, ReadDocxOptions, Run, RunChild, StructuredDataTag, StructuredDataTagChild,
    Table, TableCellContent, TableChild, TableRowChild,
};

use crate::error::{Error, Result};
use crate::tools::workspace::{Workspace, Wrote};

/// A document the caller pointed at could not be parsed.
///
/// Shaped like the errors [`Workspace::read_bytes`] builds — an [`Error::Config`]
/// naming the path — because a model reads the message and has to act on it:
/// "that file is not a document" is a different next move from "that file is not
/// there".
fn unreadable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("{rel} is not a readable .docx document: {e}"))
}

/// Serialising the document failed before any byte was written.
fn unwritable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("cannot build the .docx for {rel}: {e}"))
}

/// Append the visible text of one run.
///
/// Tabs and breaks become their characters rather than being dropped: a run of
/// text with the tabs removed reads as a different document, and the model is
/// being asked what the document says.
fn push_run(out: &mut String, run: &Run) {
    for child in &run.children {
        match child {
            RunChild::Text(t) => out.push_str(&t.text),
            RunChild::Tab(_) | RunChild::PTab(_) => out.push('\t'),
            RunChild::Break(_) | RunChild::CarriageReturn(_) => out.push('\n'),
            // Drawings, comment anchors, field instructions and footnote marks
            // carry no text the reader is asking for.
            _ => {}
        }
    }
}

/// Append the inline text of a paragraph's children, in document order.
///
/// Recursive only through hyperlinks, which wrap the same [`ParagraphChild`]s —
/// and which `docx-rs`'s own `Paragraph::raw_text` skips entirely, so link text
/// would vanish from the extraction if this walked the same way it does.
///
/// A tracked insertion and a tracked move-in are text the document currently
/// says, so they are included. A deletion and a move-out are the *removed* half
/// of the same change — ghost text that is no longer part of the document as it
/// reads now — so they are not.
fn push_inline(out: &mut String, children: &[ParagraphChild]) {
    for child in children {
        match child {
            ParagraphChild::Run(r) => push_run(out, r),
            ParagraphChild::Hyperlink(h) => push_inline(out, &h.children),
            ParagraphChild::Insert(i) => {
                for c in &i.children {
                    if let InsertChild::Run(r) = c {
                        push_run(out, r);
                    }
                }
            }
            ParagraphChild::MoveTo(m) => {
                for c in &m.children {
                    if let MoveToChild::Run(r) = c {
                        push_run(out, r);
                    }
                }
            }
            _ => {}
        }
    }
}

/// One paragraph becomes one line.
fn push_paragraph(lines: &mut Vec<String>, p: &Paragraph) {
    let mut line = String::new();
    push_inline(&mut line, &p.children);
    lines.push(line);
}

/// One table row becomes one line, cells tab-separated.
///
/// Tabs for the same reason the spreadsheet reader uses them: one token per gap
/// instead of a run of padding, and the row structure survives into the text the
/// model reads. A cell holding several paragraphs joins them with a space, since
/// a newline inside a cell would forge a row boundary.
fn push_table(lines: &mut Vec<String>, table: &Table) {
    for TableChild::TableRow(row) in &table.rows {
        let mut cells = Vec::new();
        for TableRowChild::TableCell(cell) in &row.cells {
            let mut inner = Vec::new();
            for content in &cell.children {
                match content {
                    TableCellContent::Paragraph(p) => push_paragraph(&mut inner, p),
                    TableCellContent::Table(t) => push_table(&mut inner, t),
                    TableCellContent::StructuredDataTag(s) => push_sdt(&mut inner, s),
                    _ => {}
                }
            }
            cells.push(inner.join(" "));
        }
        lines.push(cells.join("\t"));
    }
}

/// A content control (`w:sdt`) holds ordinary body content behind a wrapper.
///
/// Worth walking rather than skipping: a document produced from a Word template
/// keeps most of its filled-in text inside these, and a reader that ignored them
/// would report a form as empty.
fn push_sdt(lines: &mut Vec<String>, tag: &StructuredDataTag) {
    for child in &tag.children {
        match child {
            StructuredDataTagChild::Run(r) => {
                let mut line = String::new();
                push_run(&mut line, r);
                lines.push(line);
            }
            StructuredDataTagChild::Paragraph(p) => push_paragraph(lines, p),
            StructuredDataTagChild::Table(t) => push_table(lines, t),
            StructuredDataTagChild::StructuredDataTag(s) => push_sdt(lines, s),
            _ => {}
        }
    }
}

/// Extract the document body as text, in reading order, one line per paragraph.
///
/// Body only. Headers, footers, footnotes and comments are not in the output:
/// they are not where the document's argument lives, and interleaving them would
/// put text in front of the model in an order the document does not have.
///
/// Formatting is not shown — a text projection cannot carry a style, and
/// spending tokens on a lie about one is worse than spending none.
pub fn read_text(ws: &Workspace, rel: &str) -> Result<String> {
    let bytes = ws.read_bytes(rel)?;
    // Image previews off: extracting text never needs a decoded picture, and
    // leaving them on would run a PNG re-encode over model-supplied bytes for a
    // result nothing here reads.
    let doc = read_docx_with_options(
        &bytes,
        ReadDocxOptions::default().with_image_previews(false),
    )
    .map_err(|e| unreadable(rel, e))?;

    let mut lines = Vec::new();
    for child in &doc.document.children {
        match child {
            DocumentChild::Paragraph(p) => push_paragraph(&mut lines, p),
            DocumentChild::Table(t) => push_table(&mut lines, t),
            DocumentChild::StructuredDataTag(s) => push_sdt(&mut lines, s),
            // Bookmarks, comment anchors, the section marker and a table of
            // contents field hold no body text of their own.
            _ => {}
        }
    }
    Ok(lines.join("\n"))
}

/// Generate a new document at `rel`, one paragraph per entry of `paragraphs`.
///
/// Plain body text, no styles: this is the write-a-report path, and a harness
/// that guessed at headings would guess wrong. A caller who wants structure
/// wants a richer tool than this one.
///
/// This replaces whatever is at `rel`. It is generate-only by design — see the
/// module documentation for why an in-place edit is not offered, and why
/// read-then-write is not an acceptable stand-in for one.
pub fn write_new(ws: &Workspace, rel: &str, paragraphs: &[String]) -> Result<Wrote> {
    let mut doc = Docx::new();
    for text in paragraphs {
        doc = doc.add_paragraph(Paragraph::new().add_run(Run::new().add_text(text)));
    }
    let mut buf = Cursor::new(Vec::new());
    doc.pack(&mut buf).map_err(|e| unwritable(rel, e))?;
    ws.write_bytes(rel, &buf.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The policy the boundary tests share: everything readable and writable
    /// except `secrets/`.
    fn guarded(root: &std::path::Path) -> Workspace {
        Workspace::with_policy(
            root,
            Policy::default()
                .layer("base")
                .allow_read("*")
                .allow_write("*")
                .deny_read("secrets/*")
                .deny_write("secrets/*"),
        )
    }

    fn sample() -> Vec<String> {
        vec![
            "Quarterly review".to_string(),
            // Characters that must survive XML escaping in both directions.
            "Revenue rose 4% — see <appendix> & the notes".to_string(),
            String::new(),
            "Signed, EMEA".to_string(),
        ]
    }

    #[test]
    fn write_new_then_read_text_round_trips_the_paragraphs() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let paragraphs = sample();

        assert_eq!(
            write_new(&ws, "out/report.docx", &paragraphs).unwrap(),
            Wrote::Created
        );

        let text = read_text(&ws, "out/report.docx").unwrap();
        assert_eq!(
            text,
            paragraphs.join("\n"),
            "one line per paragraph, in order, escaping intact"
        );
    }

    #[test]
    fn a_file_that_is_not_a_document_is_an_error_not_a_panic() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.docx", b"this is plainly not a zip archive")
            .unwrap();
        // A well-formed but empty zip — an end-of-central-directory record and
        // nothing else. It gets past the archive check and fails on the parts a
        // .docx is required to have, which is the other half of the failure
        // surface and the one more likely to panic.
        ws.write_bytes(
            "empty.docx",
            &[
                b'P', b'K', 5, 6, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
        )
        .unwrap();

        for rel in ["notes.docx", "empty.docx"] {
            let shown = read_text(&ws, rel).unwrap_err().to_string();
            assert!(
                shown.contains(rel) && shown.contains("not a readable"),
                "the message names the file and what is wrong with it, got {shown}"
            );
        }
    }

    #[test]
    fn a_denied_path_is_refused_for_both_reading_and_writing() {
        let d = dir();
        // Written through a permissive workspace so the file genuinely exists:
        // a refusal on a missing file would prove nothing.
        write_new(&Workspace::new(d.path()), "secrets/report.docx", &sample()).unwrap();
        let ws = guarded(d.path());

        let read = read_text(&ws, "secrets/report.docx");
        assert!(
            matches!(&read, Err(Error::Refused { act, target, .. })
                if act == "read" && target == "secrets/report.docx"),
            "got {read:?}"
        );
        let write = write_new(&ws, "secrets/report.docx", &sample());
        assert!(
            matches!(&write, Err(Error::Refused { act, target, .. })
                if act == "write" && target == "secrets/report.docx"),
            "got {write:?}"
        );
    }

    /// The negative control for the test above. Same operations, same workspace,
    /// a path the same policy allows — so the refusals measure the boundary and
    /// not an operation that would have failed anywhere.
    #[test]
    fn the_same_operations_succeed_on_a_path_the_policy_allows() {
        let d = dir();
        let ws = guarded(d.path());

        assert_eq!(
            write_new(&ws, "open/report.docx", &sample()).unwrap(),
            Wrote::Created
        );
        assert!(read_text(&ws, "open/report.docx")
            .unwrap()
            .contains("Quarterly review"));
    }
}
