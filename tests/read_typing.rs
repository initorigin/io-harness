//! `read_file` has a type — F6, F7, F8, F11 — at the workspace level and
//! through the full loop.
//!
//! The capability under test is not "it reads a file". It is that a file which
//! is **not** text is named rather than decoded. Until 0.55.0 the whole body of
//! [`Workspace::read_file`] was
//! `std::fs::read_to_string(abs).unwrap_or_default()`, so an executable, a JPEG
//! and a UTF-16 log all arrived at the model as `Ok("")` — the same answer a
//! file that does not exist gives, and a model told a file is empty writes over
//! it. That is the defect these tests pin, which is why F6 writes the old
//! behaviour out explicitly: a silent revert to `unwrap_or_default()` has to
//! fail something.
//!
//! Each clause is asserted twice — once against `Workspace` directly, where the
//! behaviour lives, and once through a run, where the model actually sees it.

use std::sync::atomic::{AtomicUsize, Ordering};

use io_harness::policy::Policy;
use io_harness::provider::{CompletionRequest, CompletionResponse, ToolCall};
use io_harness::tools::{FileContent, TextEncoding, Workspace};
use io_harness::{run_with, ApproveAll, Provider, Store, TaskContract};
use serde_json::json;

struct MockScript {
    steps: Vec<Vec<ToolCall>>,
    at: AtomicUsize,
}

impl MockScript {
    fn new(steps: Vec<Vec<ToolCall>>) -> Self {
        Self {
            steps,
            at: AtomicUsize::new(0),
        }
    }
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

fn read(path: &str) -> ToolCall {
    ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path }),
    }
}

fn read_range(path: &str, offset: u64, limit: u64) -> ToolCall {
    ToolCall {
        name: "read_file".into(),
        arguments: json!({ "path": path, "offset": offset, "limit": limit }),
    }
}

fn contract(root: &std::path::Path) -> TaskContract {
    TaskContract::workspace("read what is there", root).with_max_steps(6)
}

/// The observation the agent was handed at `step`.
fn observation(store: &Store, run_id: i64, step: usize) -> String {
    store.steps(run_id).unwrap()[step].prompt.clone()
}

/// A tree carrying one of each shape the classification has to tell apart.
fn fixture() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(root.join("notes.md"), "# alpha\nplain text\n").unwrap();
    // A lone 0x80 continuation byte with no lead: not valid UTF-8 in any
    // position, and the leading bytes say ELF so the sniff has something to name.
    std::fs::write(
        root.join("agent"),
        [0x7f, b'E', b'L', b'F', 0x02, 0x01, 0x80, 0x00],
    )
    .unwrap();
    std::fs::write(root.join("logo.png"), [0x89, b'P', b'N', b'G']).unwrap();
    std::fs::write(root.join("books.xlsx"), [b'P', b'K', 0x03, 0x04]).unwrap();
    std::fs::write(root.join("utf16le.log"), utf16(true, "línea uno\n")).unwrap();
    std::fs::write(root.join("utf16be.log"), utf16(false, "línea uno\n")).unwrap();
    // The same content with the mark removed: two bytes are a weak signal, and
    // without them this is bytes.
    std::fs::write(root.join("nobom.log"), &utf16(true, "línea uno\n")[2..]).unwrap();
    dir
}

/// UTF-16 bytes with the byte-order mark in front, in the given endianness.
fn utf16(little_endian: bool, text: &str) -> Vec<u8> {
    let mut out = if little_endian {
        vec![0xff, 0xfe]
    } else {
        vec![0xfe, 0xff]
    };
    for unit in text.encode_utf16() {
        out.extend_from_slice(&if little_endian {
            unit.to_le_bytes()
        } else {
            unit.to_be_bytes()
        });
    }
    out
}

// ---------------------------------------------------------------------------
// F6 — a binary file is named as binary, not read as empty
// ---------------------------------------------------------------------------

#[test]
fn a_binary_file_is_named_with_its_size_and_kind() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    match ws.read_typed("agent").unwrap() {
        FileContent::Binary { bytes, kind } => {
            assert_eq!(bytes, 8, "the size is the file's own");
            assert_eq!(kind, "an ELF executable", "the leading bytes are named");
        }
        other => panic!("a binary file classified as {other:?}"),
    }
}

#[test]
fn the_text_reader_now_fails_where_it_used_to_return_an_empty_string() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    // This is the defect, written out. Before 0.55.0 the assertion below was
    // `assert_eq!(ws.read_file("agent").unwrap(), "")` and it passed — the whole
    // method was `read_to_string(abs).unwrap_or_default()`. A revert to that
    // body fails here and nowhere else.
    let err = ws.read_file("agent").unwrap_err().to_string();
    assert!(
        err.contains("agent") && err.contains("an ELF executable") && err.contains("8 bytes"),
        "the error names the file, what it is and how big it is, got {err}"
    );

    // And the one case where nothing really is the answer is untouched: a
    // missing file still reads as empty, which is what lets an agent create one.
    assert_eq!(ws.read_file("brand-new.rs").unwrap(), "");
}

#[tokio::test]
async fn through_the_loop_a_binary_read_carries_no_content() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read("agent")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[read agent error]") && next.contains("an ELF executable"),
        "the model is told what the file is: {next}"
    );
    assert!(
        !next.contains("[read agent]\n\n"),
        "and is not handed an empty document: {next}"
    );
}

// ---------------------------------------------------------------------------
// F7 — UTF-16 is decoded and named; UTF-16 without a mark is binary
// ---------------------------------------------------------------------------

#[test]
fn utf16_is_decoded_and_the_encoding_is_named() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    for (path, expected) in [
        ("utf16le.log", TextEncoding::Utf16Le),
        ("utf16be.log", TextEncoding::Utf16Be),
    ] {
        match ws.read_typed(path).unwrap() {
            FileContent::Text { text, encoding } => {
                assert_eq!(text, "línea uno\n", "{path} decoded to its own text");
                assert_eq!(encoding, expected, "{path} named its encoding");
            }
            other => panic!("{path} classified as {other:?}"),
        }
    }
}

#[test]
fn utf16_without_a_byte_order_mark_is_binary_rather_than_guessed() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    match ws.read_typed("nobom.log").unwrap() {
        FileContent::Binary { .. } => {}
        other => panic!("a BOM-less UTF-16 file was decoded anyway, as {other:?}"),
    }
}

#[tokio::test]
async fn through_the_loop_a_utf16_read_says_which_encoding_it_was() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read("utf16le.log")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[read utf16le.log (UTF-16LE)]") && next.contains("línea uno"),
        "the header names the encoding and the text is there: {next}"
    );
}

#[tokio::test]
async fn an_ordinary_utf8_read_says_nothing_new() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read("notes.md")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[read notes.md]\n# alpha\nplain text\n"),
        "the common case is unchanged, header included: {next}"
    );
}

// ---------------------------------------------------------------------------
// F8 — an image or a document is routed, not decoded
// ---------------------------------------------------------------------------

#[test]
fn an_image_is_routed_to_the_tool_that_can_look_at_it() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    match ws.read_typed("logo.png").unwrap() {
        FileContent::Image { format } => assert_eq!(format, "a PNG image"),
        other => panic!("an image classified as {other:?}"),
    }
    let why = ws.read_typed("logo.png").unwrap().refusal("logo.png").unwrap();
    assert!(why.contains("logo.png") && why.contains("a PNG image"), "{why}");
    // Whether `view_image` is compiled in decides which sentence, and both name
    // the tool or say plainly that this build has no image support.
    if cfg!(feature = "media") {
        assert!(why.contains("view_image"), "{why}");
    } else {
        assert!(why.contains("media"), "{why}");
    }
}

#[test]
fn a_document_names_the_tool_that_reads_it() {
    let dir = fixture();
    let ws = Workspace::new(dir.path());

    match ws.read_typed("books.xlsx").unwrap() {
        FileContent::Document { format, tool } => {
            assert_eq!(format, "a spreadsheet");
            assert_eq!(tool, "xlsx_read");
        }
        other => panic!("a workbook classified as {other:?}"),
    }
    let why = ws
        .read_typed("books.xlsx")
        .unwrap()
        .refusal("books.xlsx")
        .unwrap();
    assert!(
        why.contains("xlsx_read"),
        "the tool is named whether or not it is compiled in: {why}"
    );
    if !cfg!(feature = "xlsx") {
        assert!(
            why.contains("not compiled into this build"),
            "a model told nothing calls the same tool again: {why}"
        );
    }
}

#[test]
fn an_svg_is_text_because_it_is_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("diagram.svg"),
        "<svg xmlns=\"http://www.w3.org/2000/svg\"/>\n",
    )
    .unwrap();
    let ws = Workspace::new(dir.path());

    // The deliberate hole in the image table: a model reading an SVG wants the
    // markup, and calling it an image would make a readable file unreadable.
    match ws.read_typed("diagram.svg").unwrap() {
        FileContent::Text { text, .. } => assert!(text.contains("<svg")),
        other => panic!("an SVG classified as {other:?}"),
    }
}

#[tokio::test]
async fn through_the_loop_an_image_read_carries_no_content_and_names_the_route() {
    let dir = fixture();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read("logo.png")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[read logo.png error]") && next.contains("a PNG image"),
        "the model is told what it asked for: {next}"
    );
    assert!(
        !next.contains("\u{89}PNG"),
        "and none of the bytes came with it: {next}"
    );
}

// ---------------------------------------------------------------------------
// F9 — a read that will not fit returns no content
// ---------------------------------------------------------------------------

/// A file over the default ceiling — `entry_cap_chars(24_000)` is 12,000 chars —
/// with a sentinel at its head, its middle and its tail, so "none of the file's
/// bytes" is an assertion rather than a claim about the first line.
fn too_big() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let filler = "x".repeat(7_000);
    let body = format!("HEAD-SENTINEL\n{filler}\nMIDDLE-SENTINEL\n{filler}\nTAIL-SENTINEL\n");
    std::fs::write(dir.path().join("huge.txt"), body).unwrap();
    dir
}

#[tokio::test]
async fn a_read_that_will_not_fit_returns_none_of_the_file() {
    let dir = too_big();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read("huge.txt")]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    for sentinel in ["HEAD-SENTINEL", "MIDDLE-SENTINEL", "TAIL-SENTINEL"] {
        assert!(
            !next.contains(sentinel),
            "a refused read carries no content, and {sentinel} is in it: {next}"
        );
    }
    assert!(
        next.contains("[read huge.txt error]")
            && next.contains("huge.txt is 14046 chars")
            && next.contains("12000-char ceiling"),
        "the refusal names the file, its size and the ceiling: {next}"
    );
    assert!(
        next.contains("offset") && next.contains("limit") && next.contains("max_read_chars"),
        "and both ways to proceed: {next}"
    );
}

#[tokio::test]
async fn the_offered_range_actually_works_on_the_file_that_was_refused() {
    let dir = too_big();
    let store = Store::memory().unwrap();
    // The refusal is only usable if the remedy it names succeeds, so the two
    // halves are one test: refused whole, served by range.
    let provider = MockScript::new(vec![
        vec![read("huge.txt")],
        vec![read_range("huge.txt", 1, 1)],
    ]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let after_range = observation(&store, result.run_id, 2);
    assert!(
        after_range.contains("HEAD-SENTINEL") && after_range.contains("lines 1-1 of 5"),
        "the range the refusal offered returns the line asked for: {after_range}"
    );
    assert!(
        !after_range.contains("MIDDLE-SENTINEL"),
        "and only that line: {after_range}"
    );
}

// ---------------------------------------------------------------------------
// F11 — a range read is served and legible as a range
// ---------------------------------------------------------------------------

/// A hundred numbered lines, so an off-by-one is visible rather than plausible.
fn hundred_lines() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let body: String = (1..=100).map(|n| format!("line {n}\n")).collect();
    std::fs::write(dir.path().join("long.txt"), body).unwrap();
    dir
}

#[tokio::test]
async fn a_range_read_returns_exactly_the_lines_asked_for() {
    let dir = hundred_lines();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read_range("long.txt", 10, 5)]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("line 10\nline 11\nline 12\nline 13\nline 14\n"),
        "lines 10 to 14, one-based: {next}"
    );
    assert!(
        !next.contains("line 9\n") && !next.contains("line 15\n"),
        "and nothing on either side of them: {next}"
    );
    assert!(
        next.contains("lines 10-14 of 100"),
        "the header states the range and the total, so a slice reads as a slice: {next}"
    );
}

#[tokio::test]
async fn a_range_beyond_the_end_is_an_error_naming_the_total() {
    let dir = hundred_lines();
    let store = Store::memory().unwrap();
    let provider = MockScript::new(vec![vec![read_range("long.txt", 500, 5)]]);

    let result = run_with(
        &contract(dir.path()),
        &provider,
        &store,
        &Policy::permissive(),
        &ApproveAll,
    )
    .await
    .unwrap();

    let next = observation(&store, result.run_id, 1);
    assert!(
        next.contains("[read long.txt error]") && next.contains("100"),
        "an empty success would read as an empty file: {next}"
    );
}
