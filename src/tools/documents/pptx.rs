#![cfg(feature = "pptx")]
//! PowerPoint decks: extract the slide text. Read-only, by design.
//!
//! No `.pptx` crate, deliberately. A deck is a zip of XML, and the slide text is
//! the content of the `<a:t>` elements in `ppt/slides/slideN.xml` — that is a
//! short walk over `zip` and `quick-xml`, two dependencies this tree can already
//! justify, against a presentation-parsing crate that was days old. The vetting
//! rule that kept OCR out applies to convenience too.
//!
//! **There is no write path here and there should not be one.** Generating a
//! deck means writing slide layouts, masters, theme parts and the relationship
//! graph that ties them together; hand-rolling that on top of a zip writer
//! produces a file PowerPoint may or may not open, and "may or may not" is not a
//! capability. Reading is a projection and cannot corrupt anything; writing
//! would be a format implementation.
//!
//! **Every byte comes through the [`Workspace`].** The archive is opened over a
//! [`Cursor`] on the bytes [`Workspace::read_bytes`] returned. Nothing here opens
//! a path, and nothing here writes at all — so a deck is governed by exactly the
//! policy that governs a source file.

use std::io::{Cursor, Read};

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::error::{Error, Result};
use crate::tools::workspace::Workspace;

/// The one directory slides live in. Layouts (`ppt/slideLayouts/`), masters
/// (`ppt/slideMasters/`) and speaker notes (`ppt/notesSlides/`) sit elsewhere and
/// are deliberately not read: boilerplate placeholder text and private notes are
/// not what "the text of this deck" means.
const SLIDES: &str = "ppt/slides/slide";

/// The most of one slide part this will read, uncompressed.
///
/// `zip` bounds the compressed side of an entry and nothing bounds the other one:
/// deflate reaches about a thousand to one on a run of a single byte, so ten
/// megabytes of slide part inflate to something on the order of ten gigabytes,
/// once per slide part in the archive. The read is what has to be bounded, since
/// by the time a length could be checked the bytes are already in memory — and
/// the length an entry declares for itself is written by whoever wrote the file.
///
/// Sixteen megabytes is far past any real slide part, which run under one, and
/// small enough that a deck full of them stays an amount of memory a process can
/// spare.
const MAX_SLIDE_BYTES: u64 = 16 * 1024 * 1024;

/// A deck the caller pointed at could not be parsed.
///
/// Shaped like the errors [`Workspace::read_bytes`] builds — an [`Error::Config`]
/// naming the path — because a model reads the message and has to act on it.
fn unreadable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("{rel} is not a readable .pptx deck: {e}"))
}

/// The slide number in an entry name, if the name is exactly a slide part.
///
/// Both halves of the match are exact and the middle must parse as a number, so
/// `ppt/slides/_rels/slide1.xml.rels`, `ppt/notesSlides/notesSlide1.xml` and a
/// hostile `../../ppt/slides/slide1.xml` all fail it. Nothing here is ever used
/// as a path — the archive is read by index — so a crafted entry name is a
/// classification question, not a zip-slip one.
fn slide_number(name: &str) -> Option<u32> {
    name.strip_prefix(SLIDES)?
        .strip_suffix(".xml")?
        .parse()
        .ok()
}

/// Pull the text out of one slide's XML.
///
/// `<a:t>` holds every visible character on a slide: title, body, table cell,
/// chart label, the text inside a shape. Matched on the local name rather than
/// the literal `a:t`, because the DrawingML namespace prefix is a choice the
/// file makes and a generator is free to bind it to something else.
///
/// A newline is emitted at the end of each `<a:p>` paragraph and for each
/// `<a:br/>`, so a title and the bullet under it do not run together into one
/// word.
fn slide_text(xml: &[u8]) -> std::result::Result<String, quick_xml::Error> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(false);

    let mut out = String::new();
    let mut in_text = false;
    loop {
        match reader.read_event()? {
            Event::Start(e) if e.local_name().as_ref() == "t" => in_text = true,
            Event::End(e) => match e.local_name().as_ref() {
                "t" => in_text = false,
                "p" => out.push('\n'),
                _ => {}
            },
            Event::Empty(e) if e.local_name().as_ref() == "br" => out.push('\n'),
            // quick-xml 0.41 split what `BytesText::unescape` used to do. A text
            // event no longer carries entities at all — the reader emits each one
            // as its own `GeneralRef` — so the text arm only normalises
            // end-of-lines, which is what `xml10_content` is for. A slide is
            // XML 1.0.
            Event::Text(t) if in_text => out.push_str(&t.xml10_content()),
            // `&amp;` arrives here as the bare name `amp`. Rebuilding the
            // reference and handing it to `unescape` resolves the five predefined
            // entities and a numeric character reference alike, and an entity
            // this crate cannot resolve stays an error rather than silently
            // deleting a character from someone's slide.
            Event::GeneralRef(r) if in_text => {
                let name = r.into_inner();
                out.push_str(&quick_xml::escape::unescape(&format!("&{name};"))?);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(out)
}

/// Extract the deck's text, slide by slide, in slide order.
///
/// Each slide is labelled with its number, so a model that finds a sentence it
/// wants to quote or correct knows which slide it is on — the extraction is the
/// only coordinate system the deck has once it is text:
///
/// ```text
/// # Slide 1
/// Quarterly review
/// EMEA
///
/// # Slide 2
/// Revenue
/// ```
///
/// Slides come back in **numeric** order, not the archive's order and not
/// lexicographic order — both of which put `slide10.xml` before `slide2.xml` and
/// hand the model a deck that reads out of sequence. A deck with no slide parts
/// is an error naming the file rather than an empty string, for the same reason
/// the spreadsheet reader refuses a workbook with no sheets: silence reads as
/// "this deck is blank", which is a different and wrong thing to tell the model.
///
/// # The one refusal
///
/// Each slide part is read under a sixteen-megabyte ceiling, because a zip bounds
/// what an entry costs compressed and not what it costs expanded. A part that
/// runs past it fails the whole extraction rather than contributing the prefix
/// that fit: a partial slide is indistinguishable from a short one once it is
/// text, and a deck quietly reported as shorter than it is would be worse than a
/// deck reported as unreadable.
pub fn read_text(ws: &Workspace, rel: &str) -> Result<String> {
    let bytes = ws.read_bytes(rel)?;
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| unreadable(rel, e))?;

    // Collected as (number, index) and read back by index: entry names in a zip
    // are not guaranteed unique, and looking a slide up by name would leave
    // which duplicate answers up to the archive.
    let mut slides: Vec<(u32, usize)> = (0..archive.len())
        .filter_map(|i| {
            let name = archive.by_index(i).ok()?.name().to_string();
            slide_number(&name).map(|n| (n, i))
        })
        .collect();
    if slides.is_empty() {
        return Err(Error::Config(format!(
            "{rel} contains no slides; is it a .pptx?"
        )));
    }
    slides.sort_unstable();

    let mut out = String::new();
    for (number, index) in slides {
        let entry = archive.by_index(index).map_err(|e| unreadable(rel, e))?;
        let mut xml = Vec::new();
        // One byte past the limit, deliberately. Reading exactly the limit leaves
        // "the part ended" and "the read stopped" as the same observation, and the
        // difference between them is the whole point: a slide the reader saw only
        // the first sixteen megabytes of extracts as a short slide, and nothing in
        // the output would tell a model that from a slide that is short. Asking
        // for one more byte than may be kept makes the overrun a fact rather than
        // an inference, and it is refused.
        std::io::copy(&mut entry.take(MAX_SLIDE_BYTES + 1), &mut xml)
            .map_err(|e| unreadable(rel, e))?;
        if xml.len() as u64 > MAX_SLIDE_BYTES {
            return Err(unreadable(
                rel,
                format!(
                    "slide {number} expands past the {MAX_SLIDE_BYTES}-byte limit this reads a \
                     slide part as. A part that large is a compression bomb rather than a slide, \
                     and reading what fits would report the deck as shorter than it is"
                ),
            ));
        }
        let text = slide_text(&xml).map_err(|e| unreadable(rel, e))?;

        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("# Slide {number}\n"));
        out.push_str(text.trim_matches('\n'));
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use std::io::Write;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    /// The policy the boundary tests share: everything readable except
    /// `secrets/`.
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

    /// One slide part's XML, with the shape/paragraph/run nesting a real deck
    /// has, so the walk is exercised against the structure and not a flat list
    /// of `<a:t>`.
    fn slide_xml(lines: &[&str]) -> String {
        let body: String = lines
            .iter()
            .map(|l| format!("<a:p><a:r><a:t>{l}</a:t></a:r></a:p>"))
            .collect();
        format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<p:sld xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main"
       xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <p:cSld><p:spTree><p:sp><p:txBody>{body}</p:txBody></p:sp></p:spTree></p:cSld>
</p:sld>"#
        )
    }

    /// Build a deck in memory from `(entry name, contents)` pairs. Nothing here
    /// touches the filesystem, and the entries go in in the order given — which
    /// is how the ordering test proves the archive's own order is not used.
    fn deck(entries: &[(&str, String)]) -> Vec<u8> {
        let mut z = zip::ZipWriter::new(Cursor::new(Vec::new()));
        for (name, content) in entries {
            z.start_file(*name, zip::write::SimpleFileOptions::default())
                .unwrap();
            z.write_all(content.as_bytes()).unwrap();
        }
        z.finish().unwrap().into_inner()
    }

    /// A twelve-slide deck written into the archive in lexicographic order, so
    /// `slide10` physically precedes `slide2`.
    fn twelve_slides() -> Vec<u8> {
        let mut names: Vec<u32> = (1..=12).collect();
        names.sort_by_key(|n| n.to_string());
        let entries: Vec<(String, String)> = names
            .iter()
            .map(|n| {
                (
                    format!("ppt/slides/slide{n}.xml"),
                    slide_xml(&[&format!("Slide {n} title"), &format!("body {n}")]),
                )
            })
            .collect();
        let borrowed: Vec<(&str, String)> = entries
            .iter()
            .map(|(n, c)| (n.as_str(), c.clone()))
            .collect();
        deck(&borrowed)
    }

    #[test]
    fn slide_text_extracts_in_numeric_slide_order_past_nine() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("deck.pptx", &twelve_slides()).unwrap();

        let text = read_text(&ws, "deck.pptx").unwrap();

        let headings: Vec<&str> = text.lines().filter(|l| l.starts_with("# Slide")).collect();
        assert_eq!(
            headings,
            (1..=12).map(|n| format!("# Slide {n}")).collect::<Vec<_>>(),
            "numeric order, not the archive's and not lexicographic: {text}"
        );
        assert!(
            text.contains("Slide 2 title") && text.contains("Slide 10 title"),
            "every slide's text is present: {text}"
        );
        // Slide 2's body precedes slide 10's heading, so the labels are not just
        // sorted over content that stayed in archive order.
        assert!(
            text.find("body 2").unwrap() < text.find("# Slide 10").unwrap(),
            "{text}"
        );
    }

    #[test]
    fn paragraphs_and_breaks_become_line_boundaries_and_entities_are_unescaped() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let xml =
            slide_xml(&["Q1 &amp; Q2", "up 4%"]).replace("</a:r></a:p>", "</a:r><a:br/></a:p>");
        ws.write_bytes("deck.pptx", &deck(&[("ppt/slides/slide1.xml", xml)]))
            .unwrap();

        assert_eq!(
            read_text(&ws, "deck.pptx").unwrap(),
            "# Slide 1\nQ1 & Q2\n\nup 4%\n",
            "a break adds its own line; the entity comes back as the character"
        );
    }

    #[test]
    fn only_slide_parts_are_read_not_layouts_masters_or_notes() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let bytes = deck(&[
            ("ppt/slides/slide1.xml", slide_xml(&["real slide text"])),
            (
                "ppt/slideLayouts/slideLayout1.xml",
                slide_xml(&["click to edit master title"]),
            ),
            (
                "ppt/slideMasters/slideMaster1.xml",
                slide_xml(&["master placeholder"]),
            ),
            (
                "ppt/notesSlides/notesSlide1.xml",
                slide_xml(&["private speaker note"]),
            ),
            // Named to look like a slide without being one.
            (
                "ppt/slides/_rels/slide1.xml.rels",
                slide_xml(&["relationship part"]),
            ),
            ("ppt/slides/slideX.xml", slide_xml(&["not a number"])),
        ]);
        ws.write_bytes("deck.pptx", &bytes).unwrap();

        let text = read_text(&ws, "deck.pptx").unwrap();
        assert_eq!(text, "# Slide 1\nreal slide text\n", "{text}");
    }

    #[test]
    fn a_file_that_is_not_a_deck_is_an_error_not_a_panic() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.pptx", b"this is plainly not a zip archive")
            .unwrap();
        // A real zip with no slide parts in it — past the archive check, which
        // is where a name-based lookup would have panicked or read nothing.
        ws.write_bytes(
            "jar.pptx",
            &deck(&[("META-INF/MANIFEST.MF", "Manifest-Version: 1.0".to_string())]),
        )
        .unwrap();
        // A slide part whose XML is malformed: the parse error must surface as an
        // error, not abort the run.
        ws.write_bytes(
            "torn.pptx",
            &deck(&[(
                "ppt/slides/slide1.xml",
                "<a:p><a:r><a:t>x</a:t></a:p></a:r>".to_string(),
            )]),
        )
        .unwrap();

        for rel in ["notes.pptx", "jar.pptx", "torn.pptx"] {
            let shown = read_text(&ws, rel).unwrap_err().to_string();
            assert!(
                shown.contains(rel),
                "the message names the file, got {shown}"
            );
        }
    }

    /// The other malformed shape, and the one that does *not* error: XML that
    /// simply stops. `quick-xml` reports end-of-input rather than a failure, so
    /// the reader returns the text it did get. Best-effort is the right answer
    /// for an extraction — a truncated deck still has content worth showing the
    /// model — but it is a behaviour worth pinning rather than discovering.
    #[test]
    fn a_truncated_slide_returns_what_could_be_read_rather_than_failing() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes(
            "cut.pptx",
            &deck(&[(
                "ppt/slides/slide1.xml",
                "<a:p><a:r><a:t>kept</a:t></a:r></a:p><a:p><a:r><a:t>cut".to_string(),
            )]),
        )
        .unwrap();

        assert_eq!(
            read_text(&ws, "cut.pptx").unwrap(),
            "# Slide 1\nkept\ncut\n"
        );
    }

    #[test]
    fn a_denied_path_is_refused_for_reading() {
        let d = dir();
        // Written through a permissive workspace so the file genuinely exists:
        // a refusal on a missing file would prove nothing.
        Workspace::new(d.path())
            .write_bytes("secrets/deck.pptx", &twelve_slides())
            .unwrap();
        let ws = guarded(d.path());

        let read = read_text(&ws, "secrets/deck.pptx");
        assert!(
            matches!(&read, Err(Error::Refused { act, target, .. })
                if act == "read" && target == "secrets/deck.pptx"),
            "got {read:?}"
        );
    }

    /// The negative control for the test above. Same deck, same operation, same
    /// workspace, a path the same policy allows — so the refusal measures the
    /// boundary and not an operation that would have failed anywhere.
    #[test]
    fn the_same_read_succeeds_on_a_path_the_policy_allows() {
        let d = dir();
        let ws = guarded(d.path());
        ws.write_bytes("open/deck.pptx", &twelve_slides()).unwrap();

        assert!(read_text(&ws, "open/deck.pptx")
            .unwrap()
            .contains("Slide 2 title"));
    }
}
