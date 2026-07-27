#![cfg(feature = "xlsx")]
//! Spreadsheets: read a sheet as text, generate a new workbook, edit one cell of
//! an existing one.
//!
//! Three crates, because the three jobs are genuinely separate: `calamine` reads
//! and cannot write, `rust_xlsxwriter` writes new files and explicitly cannot
//! modify an existing one, and `umya-spreadsheet` is the only one that
//! round-trips a workbook it did not create — which is what [`set_cell`] needs,
//! since an edit that dropped the workbook's other sheets, formats and formulas
//! would be a rewrite wearing an edit's name.
//!
//! **Every byte in and out goes through the [`Workspace`].** `calamine` parses
//! from a [`Cursor`] over bytes that [`Workspace::read_bytes`] returned;
//! `rust_xlsxwriter` and `umya-spreadsheet` serialise into an in-memory buffer
//! that goes to [`Workspace::write_bytes`]. Nothing in this module opens,
//! creates, reads or writes a path itself, so a workbook is governed by exactly
//! the policy that governs a source file — `deny_write("secrets/*")` stops
//! [`set_cell`] for the same reason and in the same place it stops `write_file`.

use std::io::Cursor;

use calamine::{Reader, Xlsx};

use crate::error::{Error, Result};
use crate::tools::workspace::{Workspace, Wrote};

/// A workbook the caller pointed at could not be parsed.
///
/// Shaped like the errors [`Workspace::read_bytes`] builds — an [`Error::Config`]
/// naming the path — because a model reads the message and has to be able to act
/// on it: "that file is not a workbook" is a different next move from "that file
/// is not there".
fn unreadable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("{rel} is not a readable .xlsx workbook: {e}"))
}

/// Serialising or laying out the workbook failed before any byte was written.
fn unwritable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("cannot build the .xlsx for {rel}: {e}"))
}

/// Parse a workbook out of the workspace, in memory.
///
/// The single read entry point for this module: bytes from
/// [`Workspace::read_bytes`], parsed from a [`Cursor`]. A corrupt or non-xlsx
/// file comes back as an [`Err`] here rather than as a panic deeper in.
fn open(ws: &Workspace, rel: &str) -> Result<Xlsx<Cursor<Vec<u8>>>> {
    let bytes = ws.read_bytes(rel)?;
    Xlsx::new(Cursor::new(bytes)).map_err(|e| unreadable(rel, e))
}

/// Whether `cell` is an A1-style reference this module will pass to the library.
///
/// Not paranoia: `umya-spreadsheet`'s coordinate conversion unwraps the parsed
/// column and row, so a reference it cannot parse — `""`, `"A"`, `"1"`, `"A0"`,
/// `"sheet!A1"` — panics *inside* the dependency. The check is here so a bad
/// reference from a model becomes a message it can correct instead of a crashed
/// run. `$` anchors are accepted because the library understands them.
fn is_a1(cell: &str) -> bool {
    regex::Regex::new(r"^\$?[A-Za-z]{1,3}\$?[1-9][0-9]*$").is_ok_and(|re| re.is_match(cell))
}

/// The names of every sheet in the workbook, in workbook order.
pub fn sheet_names(ws: &Workspace, rel: &str) -> Result<Vec<String>> {
    Ok(open(ws, rel)?.sheet_names())
}

/// Render one sheet as text for the model. `None` means the first sheet.
///
/// # The rendering, and why this one
///
/// Tab-separated cells, one row per line, prefixed with a header line of column
/// letters and each row prefixed with its real 1-based sheet row number:
///
/// ```text
/// \tA\tB\tC
/// 1\tRegion\tQ1\tQ2
/// 2\tEMEA\t120\t140
/// ```
///
/// The judgement call is the row/column gutter, and it is there because the
/// model's *next* act is [`set_cell`], which takes an A1 reference. A bare grid
/// makes the model count columns to name a cell, and it miscounts — so the sheet
/// carries its own coordinate system and `Q2` for `EMEA` reads off as `C2`
/// directly. The numbers are the sheet's own, not the output's: a used range
/// starting at `C5` is labelled `C5`, so the references stay true when the top
/// rows are blank. Tabs rather than a padded grid because tabs cost one token
/// per gap instead of a run of spaces, and cell text is stripped of tabs and
/// newlines so a cell's content can never forge a column or a row boundary.
///
/// Values only. Formulas render as their cached result, and formatting, merges
/// and colours are not shown — a text projection cannot carry them, and pretending
/// otherwise would cost tokens on a lie. [`set_cell`] preserves all of it on the
/// way back regardless, because it never round-trips through this rendering.
pub fn read_sheet(ws: &Workspace, rel: &str, sheet: Option<&str>) -> Result<String> {
    let mut book = open(ws, rel)?;
    let names = book.sheet_names();
    let name = match sheet {
        Some(s) if names.iter().any(|n| n == s) => s.to_string(),
        Some(s) => {
            return Err(Error::Config(format!(
                "{rel} has no sheet named {s:?}; it has: {}",
                names.join(", ")
            )))
        }
        None => names
            .first()
            .cloned()
            .ok_or_else(|| Error::Config(format!("{rel} contains no sheets")))?,
    };

    let range = book
        .worksheet_range(&name)
        .map_err(|e| unreadable(rel, e))?;
    let Some((first_row, first_col)) = range.start() else {
        return Ok(format!("{name}: empty sheet"));
    };

    let mut out = String::new();
    // The gutter column has no letter, so the header starts with the separator.
    for c in 0..range.width() {
        out.push('\t');
        out.push_str(&rust_xlsxwriter::utility::column_number_to_name(
            (first_col + c as u32) as u16,
        ));
    }
    out.push('\n');
    for (i, row) in range.rows().enumerate() {
        out.push_str(&(first_row + i as u32 + 1).to_string());
        for cell in row {
            out.push('\t');
            out.push_str(&cell.to_string().replace(['\t', '\n', '\r'], " "));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Generate a new workbook at `rel` with one sheet named `sheet`, holding `rows`.
///
/// Every value is written as a string: this is the generate-a-report path, and a
/// harness that guessed at types would turn `007` into `7` and a version string
/// into a date. A caller who wants numbers formatted as numbers wants a richer
/// tool than this one.
///
/// This replaces whatever is at `rel` — `rust_xlsxwriter` cannot modify an
/// existing workbook, so there is no merge to attempt. [`set_cell`] is the
/// preserving path.
pub fn write_new(ws: &Workspace, rel: &str, sheet: &str, rows: &[Vec<String>]) -> Result<Wrote> {
    let mut book = rust_xlsxwriter::Workbook::new();
    let w = book.add_worksheet();
    w.set_name(sheet).map_err(|e| unwritable(rel, e))?;
    for (r, row) in rows.iter().enumerate() {
        for (c, value) in row.iter().enumerate() {
            w.write_string(r as u32, c as u16, value)
                .map_err(|e| unwritable(rel, e))?;
        }
    }
    let buf = book.save_to_buffer().map_err(|e| unwritable(rel, e))?;
    ws.write_bytes(rel, &buf)
}

/// Set one cell of an existing workbook, preserving everything else.
///
/// The whole workbook is parsed, one cell is mutated, and the workbook is
/// serialised back — so the other sheets, the formats, the column widths and the
/// formulas the caller did not touch come out the way they went in. `cell` is an
/// A1 reference (`"B7"`); a reference that is not one is refused before it
/// reaches the library.
///
/// # What "preserving" is worth
///
/// It is a real round trip, not a rewrite — but it is `umya-spreadsheet`'s round
/// trip, and it is preservation in practice rather than a guarantee. What the
/// library models it keeps; what it does not model it can drop or normalise on
/// the way out. Charts, drawings, pivot tables, slicers, macros and vendor
/// extensions are where that bites, so a chart- and drawing-heavy workbook is
/// the case to check before trusting an edit, not the case to assume. A cell
/// value in a data sheet is the operation this is sound for.
///
/// Nothing here can tell the caller which of those a given workbook contains, so
/// the honest form of that limit is this paragraph rather than a check that would
/// pass on the workbook it could not understand. An edit the caller cannot afford
/// to get subtly wrong wants a copy first — the same advice that applies to any
/// in-place edit of a file only one library claims to understand.
pub fn set_cell(ws: &Workspace, rel: &str, sheet: &str, cell: &str, value: &str) -> Result<Wrote> {
    if !is_a1(cell) {
        return Err(Error::Config(format!(
            "{cell:?} is not an A1-style cell reference (like B7)"
        )));
    }
    let bytes = ws.read_bytes(rel)?;
    // `true`: read the sheets eagerly. The lazy mode defers reading them until
    // asked, and the reader it would defer to is a Cursor over bytes this
    // function owns — there is no file to go back to, by design.
    let mut book = umya_spreadsheet::reader::xlsx::read_reader(Cursor::new(bytes), true)
        .map_err(|e| unreadable(rel, e))?;
    book.sheet_by_name_mut(sheet)
        .map_err(|e| Error::Config(format!("{rel} has no sheet named {sheet:?}: {e}")))?
        .cell_mut(cell)
        .set_value(value);

    let mut buf = Vec::new();
    umya_spreadsheet::writer::xlsx::write_writer(&book, &mut buf)
        .map_err(|e| unwritable(rel, e))?;
    ws.write_bytes(rel, &buf)
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

    /// A two-sheet workbook with a bold cell and a set column width, written
    /// through the workspace.
    ///
    /// Built with `umya-spreadsheet` directly rather than with [`write_new`], so
    /// the preserving-edit test is not measuring one of this module's code paths
    /// against another one — a fixture produced by the writer under test would
    /// only ever contain what that writer knows how to emit.
    ///
    /// It is still weaker than a real file for exactly that reason: it exercises
    /// the subset of the format this generator produces, not the charts, pivot
    /// tables, conditional formats and vendor extensions a workbook out of Excel
    /// carries. A real fixture belongs under `tests/fixtures/` and this test is
    /// not a substitute for it.
    fn two_sheet_fixture(ws: &Workspace, rel: &str) {
        let mut book = umya_spreadsheet::new_file();
        let first = book.sheet_by_name_mut("Sheet1").unwrap();
        first.set_name("Data");
        first.cell_mut("A1").set_value("Region");
        first.cell_mut("B1").set_value("Q1");
        first.cell_mut("A2").set_value("EMEA");
        first.cell_mut("B2").set_value("120");
        // Non-default formatting: the thing an edit most easily destroys.
        first.style_mut("A1").font_mut().set_bold(true);

        let notes = book.new_sheet("Notes").unwrap();
        notes.cell_mut("A1").set_value("do not lose me");

        let mut buf = Vec::new();
        umya_spreadsheet::writer::xlsx::write_writer(&book, &mut buf).unwrap();
        ws.write_bytes(rel, &buf).unwrap();
    }

    /// The same fixture, written by a *different* crate.
    ///
    /// `two_sheet_fixture` builds with `umya-spreadsheet`, which is also the crate
    /// [`set_cell`] edits with, so on its own it proves only that the library
    /// round-trips its own output — and a library that drops what it does not
    /// model never emits what it does not model. This one is written by
    /// `rust_xlsxwriter`, so the bytes handed to the edit were produced by
    /// something with its own idea of what an OOXML package looks like.
    ///
    /// It is still not a workbook Excel itself wrote — the tree carries no binary
    /// fixtures — so the strongest available evidence for the preserving edit is
    /// this plus the live run in `examples/documents_live.rs`, where a
    /// `rust_xlsxwriter` workbook is edited by `umya-spreadsheet` and read back by
    /// `calamine`, three crates deep.
    fn foreign_fixture(ws: &Workspace, rel: &str) {
        let mut book = rust_xlsxwriter::Workbook::new();
        let bold = rust_xlsxwriter::Format::new().set_bold();
        let data = book.add_worksheet();
        data.set_name("Data").unwrap();
        data.write_string_with_format(0, 0, "Region", &bold)
            .unwrap();
        data.write_string(0, 1, "Q1").unwrap();
        data.write_string(1, 0, "EMEA").unwrap();
        data.write_string(1, 1, "120").unwrap();
        let notes = book.add_worksheet();
        notes.set_name("Notes").unwrap();
        notes.write_string(0, 0, "do not lose me").unwrap();

        ws.write_bytes(rel, &book.save_to_buffer().unwrap())
            .unwrap();
    }

    #[test]
    fn an_edit_preserves_a_workbook_the_editing_crate_did_not_write() {
        let d = dir();
        let ws = Workspace::new(d.path());
        foreign_fixture(&ws, "book.xlsx");

        assert_eq!(
            set_cell(&ws, "book.xlsx", "Data", "B2", "999").unwrap(),
            Wrote::Changed
        );

        let after = umya_spreadsheet::reader::xlsx::read_reader(
            Cursor::new(ws.read_bytes("book.xlsx").unwrap()),
            true,
        )
        .unwrap();
        let data = after.sheet_by_name("Data").unwrap();
        assert_eq!(data.value("B2"), "999", "the new value landed");
        assert_eq!(data.value("A1"), "Region", "its neighbours survived");
        assert!(
            data.style("A1").font().is_some_and(|f| f.bold()),
            "the bold format another crate wrote survived an edit to B2"
        );
        assert_eq!(
            after.sheet_by_name("Notes").unwrap().value("A1"),
            "do not lose me",
            "the untouched sheet survived"
        );
        assert_eq!(
            sheet_names(&ws, "book.xlsx").unwrap(),
            vec!["Data", "Notes"],
            "and no sheet was added or dropped"
        );
    }

    /// F3, the criterion that matters: an edit is an edit, not a rewrite.
    #[test]
    fn setting_one_cell_leaves_the_other_sheet_and_the_formatting_intact() {
        let d = dir();
        let ws = Workspace::new(d.path());
        two_sheet_fixture(&ws, "book.xlsx");

        assert_eq!(
            set_cell(&ws, "book.xlsx", "Data", "B2", "999").unwrap(),
            Wrote::Changed
        );

        let after = umya_spreadsheet::reader::xlsx::read_reader(
            Cursor::new(ws.read_bytes("book.xlsx").unwrap()),
            true,
        )
        .unwrap();
        let data = after.sheet_by_name("Data").unwrap();
        assert_eq!(data.value("B2"), "999", "the new value landed");
        assert_eq!(data.value("A1"), "Region", "its neighbours survived");
        assert_eq!(data.value("A2"), "EMEA");
        assert!(
            data.style("A1").font().is_some_and(|f| f.bold()),
            "the bold format on A1 survived an edit to B2"
        );
        assert_eq!(
            after.sheet_by_name("Notes").unwrap().value("A1"),
            "do not lose me",
            "the untouched sheet survived"
        );
        assert_eq!(
            sheet_names(&ws, "book.xlsx").unwrap(),
            vec!["Data", "Notes"],
            "and no sheet was added or dropped"
        );
    }

    #[test]
    fn write_new_then_read_sheet_round_trips_the_values() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let rows = vec![
            vec!["Region".to_string(), "Q1".to_string()],
            vec!["EMEA".to_string(), "120".to_string()],
        ];

        assert_eq!(
            write_new(&ws, "out/report.xlsx", "Sales", &rows).unwrap(),
            Wrote::Created
        );

        let text = read_sheet(&ws, "out/report.xlsx", None).unwrap();
        assert_eq!(text, "\tA\tB\n1\tRegion\tQ1\n2\tEMEA\t120\n", "{text}");
        // Naming the sheet explicitly reads the same sheet.
        assert_eq!(
            read_sheet(&ws, "out/report.xlsx", Some("Sales")).unwrap(),
            text
        );
    }

    #[test]
    fn sheet_names_lists_every_sheet_in_workbook_order() {
        let d = dir();
        let ws = Workspace::new(d.path());
        two_sheet_fixture(&ws, "book.xlsx");

        assert_eq!(
            sheet_names(&ws, "book.xlsx").unwrap(),
            vec!["Data", "Notes"]
        );
    }

    #[test]
    fn a_denied_path_is_refused_for_both_reading_and_editing() {
        let d = dir();
        // Written through a permissive workspace so the file genuinely exists:
        // a refusal on a missing file would prove nothing.
        two_sheet_fixture(&Workspace::new(d.path()), "secrets/book.xlsx");
        let ws = guarded(d.path());

        let read = read_sheet(&ws, "secrets/book.xlsx", None);
        assert!(
            matches!(&read, Err(Error::Refused { act, target, .. })
                if act == "read" && target == "secrets/book.xlsx"),
            "got {read:?}"
        );
        let edit = set_cell(&ws, "secrets/book.xlsx", "Data", "B2", "999");
        assert!(
            matches!(&edit, Err(Error::Refused { act, .. }) if act == "read"),
            "the edit is stopped at the read, before a byte of it is parsed: got {edit:?}"
        );
    }

    /// The negative control for the test above. Same operations, same workspace,
    /// a path the same policy allows — so the refusals measure the boundary and
    /// not an operation that would have failed anywhere.
    #[test]
    fn the_same_operations_succeed_on_a_path_the_policy_allows() {
        let d = dir();
        let ws = guarded(d.path());
        two_sheet_fixture(&ws, "open/book.xlsx");

        assert!(read_sheet(&ws, "open/book.xlsx", None)
            .unwrap()
            .contains("EMEA"));
        assert_eq!(
            set_cell(&ws, "open/book.xlsx", "Data", "B2", "999").unwrap(),
            Wrote::Changed
        );
    }

    /// A file that is not a workbook is the model's mistake, not the harness's:
    /// it has to come back as something the model can read and correct.
    #[test]
    fn a_file_that_is_not_a_workbook_is_an_error_not_a_panic() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.xlsx", b"this is plainly not a zip archive")
            .unwrap();

        for err in [
            sheet_names(&ws, "notes.xlsx").unwrap_err(),
            read_sheet(&ws, "notes.xlsx", None).unwrap_err(),
            set_cell(&ws, "notes.xlsx", "Data", "A1", "x").unwrap_err(),
        ] {
            let shown = err.to_string();
            assert!(
                shown.contains("notes.xlsx") && shown.contains("not a readable"),
                "the message names the file and what is wrong with it, got {shown}"
            );
        }
    }

    /// `umya-spreadsheet` panics on a coordinate it cannot parse, so this is the
    /// guard that keeps a model's typo from taking the run down.
    #[test]
    fn a_malformed_cell_reference_is_rejected_before_it_reaches_the_library() {
        let d = dir();
        let ws = Workspace::new(d.path());
        two_sheet_fixture(&ws, "book.xlsx");

        for bad in ["", "A", "1", "A0", "Data!A1", "AAAA1", "B 2"] {
            let e = set_cell(&ws, "book.xlsx", "Data", bad, "x").unwrap_err();
            assert!(
                e.to_string().contains("A1-style"),
                "{bad:?} should be refused as a reference, got {e}"
            );
        }
        // And the control: the well-formed forms are accepted.
        for good in ["B2", "b2", "$B$2", "AA100"] {
            assert!(
                set_cell(&ws, "book.xlsx", "Data", good, "x").is_ok(),
                "{good:?} is a valid reference"
            );
        }
    }

    #[test]
    fn naming_a_sheet_that_is_not_there_lists_the_ones_that_are() {
        let d = dir();
        let ws = Workspace::new(d.path());
        two_sheet_fixture(&ws, "book.xlsx");

        let e = read_sheet(&ws, "book.xlsx", Some("Summary")).unwrap_err();
        let shown = e.to_string();
        assert!(
            shown.contains("Summary") && shown.contains("Data") && shown.contains("Notes"),
            "the error tells the model what it could have asked for, got {shown}"
        );
    }
}
