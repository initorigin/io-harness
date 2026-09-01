//! The document parsers against files written to break them.
//!
//! Every test here is named for the audit finding it closes and fails on the
//! behaviour that shipped before it. The shape they share: a parser is handed a
//! *small* file whose cost is not its size, and the harness has to answer with an
//! error a model can read instead of an allocation the process cannot survive.
//!
//! **None of these tests reaches the pathological size.** The crafted workbook
//! asks a library for a 550 GB layout and the crafted deck asks a zip for ten
//! gigabytes of slide; a test that proved the fix by letting either happen would
//! prove it by killing the runner. What is asserted instead is the bound: the
//! refusal, the numbers the refusal names, and the fact that the expensive call
//! was never reached. Each has a companion on a large-but-ordinary file, so the
//! refusals are evidence of a boundary rather than of a parser that stopped
//! working.

/// The one file whose source is read rather than run.
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

/// The *shipped* half of `src/tools/documents/pdf.rs`: everything above its
/// `#[cfg(test)]` module, with comment lines removed and line endings
/// normalised.
///
/// Each cut is load-bearing and each would make the checker read something it
/// is not asking about:
///
/// - The test module goes because the claim under test is about the parser's
///   entry points, and that module builds a `Document` from a file it just
///   wrote eleven times over to assert on the result. Those are not call sites
///   a panic guard is owed — nothing there parses input from outside the test —
///   and counting them makes "one parse, one guard" arithmetic about the test
///   suite instead of about the module.
/// - The comments go because the claim is about *calls*, and
///   `Document::load_mem` appears in the prose above `open` as well as in it.
/// - The normalisation is because a CRLF checkout would otherwise make every
///   anchor below miss and the checker pass over nothing.
///
/// The marker is required rather than optional, for the same reason [`body`]
/// panics on a signature it cannot find: a file that stopped carrying it would
/// otherwise silently widen the checker back to the whole thing.
fn pdf_code() -> String {
    let path = std::path::Path::new(ROOT).join("src/tools/documents/pdf.rs");
    let src = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
        .replace("\r\n", "\n");
    let end = src.find("\n#[cfg(test)]\n").unwrap_or_else(|| {
        panic!(
            "{} no longer separates its unit tests with a `#[cfg(test)]` module; \
             this checker is reading a file it does not understand",
            path.display()
        )
    });
    let mut kept = src[..end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    // The cut lands on the newline *before* `#[cfg(test)]`, so the shipped half
    // ends at the last function's closing brace with nothing after it — and
    // [`body`] looks for `"\n}\n"`. Without this the last function in the file
    // has no findable end and the checker panics on a file it understands
    // perfectly well.
    kept.push('\n');
    kept
}

/// The text of one top-level function, from its signature to the closing brace
/// in column zero.
///
/// Panics rather than returns when the signature is gone: a checker that quietly
/// matched nothing would pass every assertion made over it.
fn body<'a>(code: &'a str, signature: &str) -> &'a str {
    let start = code.find(signature).unwrap_or_else(|| {
        panic!("pdf.rs no longer contains `{signature}`; this checker is reading a file it does not understand")
    });
    let rest = &code[start..];
    let end = rest
        .find("\n}\n")
        .unwrap_or_else(|| panic!("no top-level close brace after `{signature}`"));
    &rest[..end]
}

/// M16 — the panic guard belongs to the parser, not to one of its callers.
///
/// `lopdf` panics on some malformed input rather than erroring, and it decodes
/// object streams behind a mutex a panicking worker poisons — so one bad object
/// becomes a cascade of unwraps and a dead run. Before 0.74.0 the guard sat in
/// `read_text`, which does not use `lopdf` at all, while `watermark` and
/// `fill_form` reached `Document::load_mem` unguarded.
///
/// This is derived from the source rather than driven through it. A behavioural
/// test needs a file that makes `lopdf` panic, and the panic is in a race between
/// decoding workers rather than in a structure a fixture can name; the
/// behavioural test below therefore measures that the three entry points *error*,
/// which is true either side of the fix, and this one measures the thing that
/// actually changed. It fails on 0.73.0's source, where the single `load_mem`
/// call sits outside any guard.
#[test]
fn m16_the_only_lopdf_parse_sits_inside_the_guarded_open() {
    let code = pdf_code();

    assert_eq!(
        code.matches("Document::load_mem").count(),
        1,
        "one parse, one guard: a second call site would need its own"
    );

    let open = body(
        &code,
        "fn open(ws: &Workspace, rel: &str) -> Result<Document> {",
    );
    assert!(
        open.contains("catch_unwind") && open.contains("Document::load_mem"),
        "the parse and the guard are in the same function, got:\n{open}"
    );

    for entry in ["pub fn watermark(", "pub fn fill_form("] {
        let reached = body(&code, entry);
        assert!(
            reached.contains("open(ws, rel)"),
            "{entry} reaches the parser through the guarded `open`, got:\n{reached}"
        );
    }

    // The other guard stayed where it was: `pdf-extract` is a second parser, not
    // a second call to the first one.
    let read_text = body(
        &code,
        "pub fn read_text(ws: &Workspace, rel: &str) -> Result<String> {",
    );
    assert!(
        read_text.contains("catch_unwind"),
        "the extractor keeps its own guard, got:\n{read_text}"
    );
}

#[cfg(feature = "xlsx")]
mod xlsx {
    use io_harness::tools::documents::xlsx::{read_sheet, set_cell, write_new};
    use io_harness::tools::Workspace;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A one-sheet workbook holding a value at each of `cells`, serialised the
    /// way the module's own fixtures are.
    ///
    /// `umya-spreadsheet` is the only one of the three crates here that will put
    /// a cell anywhere asked: `rust_xlsxwriter` takes a `u16` column and refuses
    /// past the format's grid, which is exactly the shape of file this has to
    /// produce. Coordinates are given as `(column, row)`, both 1-based, because
    /// the string form of a reference is parsed by a regex that stops at three
    /// letters.
    fn workbook_with(cells: &[(u32, u32)]) -> Vec<u8> {
        let mut book = umya_spreadsheet::new_file();
        let sheet = book.sheet_by_name_mut("Sheet1").unwrap();
        for &(col, row) in cells {
            sheet.cell_mut((col, row)).set_value("x");
        }
        let mut buf = Vec::new();
        umya_spreadsheet::writer::xlsx::write_writer(&book, &mut buf).unwrap();
        buf
    }

    /// H14 — a couple of kilobytes that ask for a couple of hundred gigabytes.
    ///
    /// `A1` and `XFD1048576` are both inside the format: the first cell of the
    /// sheet and the last. What they span is 16,384 columns by 1,048,576 rows,
    /// and the reader lays a used range out densely, so reading this sheet used
    /// to mean asking the allocator for about 550 GB — an out-of-memory kill or
    /// an abort, neither of which a caller can catch and neither of which any
    /// panic guard reaches.
    ///
    /// The test never lets that allocation be attempted. It asserts the refusal,
    /// that the refusal names the span and the limit, and that the file it came
    /// out of is a few kilobytes — which is the finding in one line.
    #[test]
    fn h14_a_sheet_spanning_the_grid_is_refused_before_it_is_laid_out() {
        let d = dir();
        let ws = Workspace::new(d.path());
        // (16384, 1048576) is XFD1048576, the last cell of a worksheet.
        let bytes = workbook_with(&[(1, 1), (16_384, 1_048_576)]);
        assert!(
            bytes.len() < 64 * 1024,
            "the crafted workbook is kilobytes, not a large file: {} bytes",
            bytes.len()
        );
        ws.write_bytes("bomb.xlsx", &bytes).unwrap();

        let shown = read_sheet(&ws, "bomb.xlsx", None).unwrap_err().to_string();
        assert!(
            shown.contains("bomb.xlsx") && shown.contains("is not a readable .xlsx workbook"),
            "the refusal is the shape every other unreadable workbook gets, got {shown}"
        );
        assert!(
            shown.contains("17179869184") && shown.contains("5000000"),
            "it names the span it declined to lay out and the limit that stopped it, got {shown}"
        );
    }

    /// The companion, and the reason the cap is a boundary rather than a wall: a
    /// spreadsheet larger than anything a person reads by hand still comes back
    /// as text. 24,000 cells is two hundred times under the cap.
    #[test]
    fn h14_an_ordinary_large_spreadsheet_still_reads() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let rows: Vec<Vec<String>> = (0..2_000)
            .map(|r| (0..12).map(|c| format!("r{r}c{c}")).collect())
            .collect();
        write_new(&ws, "big.xlsx", "Data", &rows).unwrap();

        let text = read_sheet(&ws, "big.xlsx", None).unwrap();
        assert_eq!(
            text.lines().count(),
            2_001,
            "every row plus the header of column letters"
        );
        assert!(
            text.contains("r0c0") && text.contains("r1999c11"),
            "the first cell and the last both survived the read"
        );
    }

    /// L14 — a column the header cannot name is labelled, not mislabelled.
    ///
    /// Cell references in a file are not held to the format's grid, and the
    /// helper that turns a column index into letters takes a `u16` and adds one
    /// to it inside itself. So column 65,535 used to panic in a debug build and
    /// wrap to `A` in a release one, and column 70,000 used to be truncated to a
    /// plausible, wrong `FOS` — a label a model would then build an A1 reference
    /// out of. This sheet is 70,001 columns wide and one row tall: a few
    /// kilobytes, well under the cell cap, and straight into the header loop.
    #[test]
    fn l14_a_column_the_header_cannot_name_is_labelled_rather_than_truncated() {
        let d = dir();
        let ws = Workspace::new(d.path());
        // Column 70,001 (1-based) is `CYNI`, past `u16` and past the grid.
        ws.write_bytes("wide.xlsx", &workbook_with(&[(1, 1), (70_001, 1)]))
            .unwrap();

        let text = read_sheet(&ws, "wide.xlsx", None).unwrap();
        let header = text.lines().next().unwrap();
        assert!(
            header.starts_with("\tA\tB\tC"),
            "the columns that have names still have them, got {}",
            &header[..header.len().min(40)]
        );
        assert!(
            header.ends_with("\t?"),
            "the column past the helper's ceiling is labelled `?`"
        );
        // `FOS` is column 4,465, and this sheet is 70,001 columns wide — so it
        // appears in the header as its own honest label, and asserting its
        // absence anywhere would be asserting that a real column has no name.
        // What the finding is about is the column at the *end*: 70,000 wrapped
        // into `u16` lands on 4,465 and would have been labelled `FOS` a second
        // time, in the last position, where a model reading the header would
        // take it for an A1 reference it could use.
        assert!(
            !header.ends_with("\tFOS"),
            "the last column is not labelled with the name it would truncate to"
        );
        assert_eq!(
            header.matches("\tFOS").count(),
            1,
            "`FOS` names exactly the one column that is really `FOS`"
        );
    }

    /// M14 — a row number the library cannot parse is refused before it reaches
    /// the library.
    ///
    /// The reference guard bounded the column, because four letters index past
    /// the end of a table inside the dependency, and left the row unbounded. The
    /// dependency parses a row as a `u32` and unwraps it, so `A5000000000` — a
    /// reference every other rule accepted — was a panic on `None` and a dead
    /// run rather than a message a model could correct.
    #[test]
    fn m14_a_row_number_too_large_to_parse_is_refused_before_the_library_unwraps() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("book.xlsx", &workbook_with(&[(1, 1)]))
            .unwrap();

        let shown = set_cell(&ws, "book.xlsx", "Sheet1", "A5000000000", "x")
            .unwrap_err()
            .to_string();
        assert!(
            shown.contains("A1-style"),
            "the model is told what a reference looks like, got {shown}"
        );
        // The control: the format's own last row is still a reference this takes,
        // so the bound refuses what does not fit rather than what is merely big.
        assert!(set_cell(&ws, "book.xlsx", "Sheet1", "A1048576", "x").is_ok());
    }
}

#[cfg(feature = "pptx")]
mod pptx {
    use std::io::Write;

    use io_harness::tools::documents::pptx::read_text;
    use io_harness::tools::Workspace;

    /// The limit the reader applies to one slide part, restated here so the test
    /// crosses it by one byte rather than by a guess.
    const MAX_SLIDE_BYTES: usize = 16 * 1024 * 1024;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// A deck built in memory from `(entry name, contents)` pairs.
    fn deck(entries: &[(&str, String)]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (name, content) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(content.as_bytes()).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    fn slide(text: &str) -> String {
        format!("<p:sld xmlns:a=\"x\"><a:p><a:r><a:t>{text}</a:t></a:r></a:p></p:sld>")
    }

    /// H15 — a slide part is read under a ceiling, and an overrun is an error.
    ///
    /// A zip bounds what an entry costs compressed and nothing bounds what it
    /// costs expanded; deflate reaches about a thousand to one on a run of one
    /// byte, so a ten-megabyte slide part inflates to something on the order of
    /// ten gigabytes, once per slide part in the archive.
    ///
    /// The fixture is the smallest file that crosses the same bound rather than
    /// the largest one that fits in the class: what is under test is the ceiling,
    /// not the ratio, and a test that actually inflated ten gigabytes would prove
    /// the fix by exhausting the machine. It still carries the property that
    /// makes the finding a finding — the file on disk is a few kilobytes.
    ///
    /// On 0.73.0 this deck reads back as a very long slide and the assertion on
    /// the error is what fails.
    #[test]
    fn h15_a_slide_part_that_expands_past_the_ceiling_is_refused_at_the_read() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let bytes = deck(&[("ppt/slides/slide1.xml", slide(&"x".repeat(MAX_SLIDE_BYTES)))]);
        assert!(
            bytes.len() < 1024 * 1024,
            "a compressed deck of a few kilobytes: {} bytes",
            bytes.len()
        );
        ws.write_bytes("bomb.pptx", &bytes).unwrap();

        let shown = read_text(&ws, "bomb.pptx").unwrap_err().to_string();
        assert!(
            shown.contains("bomb.pptx") && shown.contains("is not a readable .pptx deck"),
            "the refusal is the shape every other unreadable deck gets, got {shown}"
        );
        assert!(
            shown.contains("slide 1") && shown.contains(&MAX_SLIDE_BYTES.to_string()),
            "it names the slide that overran and the ceiling it overran, got {shown}"
        );
    }

    /// The companion. A deck of ordinary slides reads the way it always did —
    /// the ceiling is three orders of magnitude above a real slide part, and a
    /// truncation that reported a short deck as a whole one is what the refusal
    /// above exists to prevent.
    #[test]
    fn h15_an_ordinary_multi_slide_deck_still_reads() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let entries = [
            ("ppt/slides/slide1.xml", slide("Quarterly review")),
            ("ppt/slides/slide2.xml", slide("Revenue")),
            ("ppt/slides/slide3.xml", slide("Questions")),
        ];
        ws.write_bytes("deck.pptx", &deck(&entries)).unwrap();

        let text = read_text(&ws, "deck.pptx").unwrap();
        assert!(
            text.contains("Quarterly review") && text.contains("Questions"),
            "{text}"
        );
        assert_eq!(
            text.matches("# Slide").count(),
            3,
            "every slide is present, not just the ones before a cut: {text}"
        );
    }
}

#[cfg(feature = "pdf")]
mod pdf {
    use io_harness::tools::documents::pdf::{fill_form, read_text, watermark};
    use io_harness::tools::Workspace;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// M16, behaviourally: the three entry points agree on what a file they
    /// cannot parse is.
    ///
    /// This one passes either side of the fix — `lopdf` reports *this* input as
    /// an error rather than a panic — and it is here as the control for the
    /// derived test above. Together they say: the malformed file is an error at
    /// every door, and the door itself is now guarded.
    #[test]
    fn m16_a_malformed_pdf_is_an_error_at_every_entry_point() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.pdf", b"this is plainly not a PDF")
            .unwrap();
        // Past the header sniff, into the object parser, still unparseable.
        ws.write_bytes("torn.pdf", b"%PDF-1.7\n5 0 obj\n<< /Type /ObjStm /N 9")
            .unwrap();

        for rel in ["notes.pdf", "torn.pdf"] {
            for err in [
                read_text(&ws, rel).unwrap_err(),
                watermark(&ws, rel, "DRAFT").unwrap_err(),
                fill_form(&ws, rel, &[("a".to_string(), "b".to_string())]).unwrap_err(),
            ] {
                let shown = err.to_string();
                assert!(
                    shown.contains(rel) && shown.contains("not a readable PDF"),
                    "the message names the file and what is wrong with it, got {shown}"
                );
            }
        }
    }
}
