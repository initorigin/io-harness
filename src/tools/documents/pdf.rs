#![cfg(feature = "pdf")]
//! PDF: generate one, read its text, stamp a watermark on it, fill its form.
//!
//! Two crates, for the two halves of the job. `lopdf` is the object model — it
//! parses a file into its dictionaries and streams, lets them be edited, and
//! writes them back, which is what [`write_new`], [`watermark`] and [`fill_form`]
//! need. `pdf-extract` is the text extractor [`read_text`] uses, and it is a
//! renderer in miniature rather than an object reader: it walks content streams,
//! resolves fonts and encodings and reconstructs the string a human would see.
//! Neither does the other's job.
//!
//! They pin different `lopdf` versions, so two of them compile. That is known
//! and left alone: the versions never meet in a signature here, because
//! `pdf-extract` is only ever handed `&[u8]` and only ever hands back a
//! `String`.
//!
//! **Every byte in and out goes through the [`Workspace`].** `lopdf` parses via
//! [`Document::load_mem`] and serialises via [`Document::save_to`] into a
//! `Vec<u8>`; `pdf-extract` parses via `extract_text_from_mem`. Nothing in this
//! module opens, creates, reads or writes a path itself — `Document::load(path)`
//! and `Document::save(path)` exist and are deliberately not used. A document
//! capability that opened files itself would sit outside the permission policy,
//! and a `deny_write("secrets/*")` that stops `write_file` but not [`watermark`]
//! is not a policy, it is a suggestion.

use lopdf::content::{Content, Operation};
use lopdf::{dictionary, Dictionary, Document, Object, ObjectId, Stream};

use crate::error::{Error, Result};
use crate::tools::workspace::{Workspace, Wrote};

/// US Letter, the page [`write_new`] emits. Points, the PDF unit.
const PAGE: (f32, f32) = (612.0, 792.0);
/// Margin on all four sides of a generated page.
const MARGIN: f32 = 72.0;
/// Body size and baseline-to-baseline distance for a generated page.
const BODY: (f32, f32) = (12.0, 14.0);
/// Characters per line before [`wrap`] breaks. Helvetica averages about half its
/// point size per character, so 12pt across a 468pt column is roughly 78.
const WRAP: usize = 78;
/// sin 45 degrees, the rotation [`watermark`] stamps at.
const DIAGONAL: f32 = std::f32::consts::FRAC_1_SQRT_2;
/// How far a `/Parent` chain is followed before giving up. A malformed file can
/// point a page at its own ancestor; this is what stops that being a hang.
const PARENT_LIMIT: usize = 32;

/// A file the caller pointed at could not be parsed as a PDF.
///
/// Shaped like the errors [`Workspace::read_bytes`] builds — an [`Error::Config`]
/// naming the path — because a model reads the message and has to be able to act
/// on it: "that file is not a PDF" is a different next move from "that file is
/// not there".
fn unreadable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("{rel} is not a readable PDF: {e}"))
}

/// Serialising or laying out the document failed before any byte was written.
fn unwritable(rel: &str, e: impl std::fmt::Display) -> Error {
    Error::Config(format!("cannot build the PDF for {rel}: {e}"))
}

/// Parse a document out of the workspace, in memory.
///
/// The single read entry point for the `lopdf` side of this module: bytes from
/// [`Workspace::read_bytes`], parsed with [`Document::load_mem`]. A truncated or
/// non-PDF file comes back as an [`Err`] here rather than as a panic deeper in.
fn open(ws: &Workspace, rel: &str) -> Result<Document> {
    let bytes = ws.read_bytes(rel)?;
    Document::load_mem(&bytes).map_err(|e| unreadable(rel, e))
}

/// Serialise a document and hand the bytes to the workspace.
fn save(ws: &Workspace, rel: &str, doc: &mut Document) -> Result<Wrote> {
    let mut buf = Vec::new();
    doc.save_to(&mut buf).map_err(|e| unwritable(rel, e))?;
    ws.write_bytes(rel, &buf)
}

/// Text as bytes a base-14 font with `/WinAnsiEncoding` will render.
///
/// One byte per character, which covers ASCII exactly and Latin-1 closely.
/// Anything above U+00FF has no single-byte code in this encoding at all, so it
/// becomes `?` rather than a byte that would render as some unrelated glyph —
/// visibly wrong beats silently wrong. Embedding a real font to carry the rest
/// is a different tool than this one.
fn encode(s: &str) -> Vec<u8> {
    s.chars()
        .map(|c| if (c as u32) < 0x100 { c as u8 } else { b'?' })
        .collect()
}

/// Break `text` into lines of at most [`WRAP`] characters, honouring the newlines
/// already in it and breaking on whitespace where it can.
///
/// A single word longer than the column is hard-split rather than left to run
/// off the right margin — a URL is the usual case and it is common enough to be
/// worth the three lines.
fn wrap(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for para in text.split('\n') {
        let mut line = String::new();
        for word in para.split_whitespace() {
            let chars: Vec<char> = word.chars().collect();
            for piece in chars.chunks(WRAP) {
                let piece: String = piece.iter().collect();
                if line.is_empty() {
                    line = piece;
                } else if line.chars().count() + 1 + piece.chars().count() <= WRAP {
                    line.push(' ');
                    line.push_str(&piece);
                } else {
                    out.push(std::mem::replace(&mut line, piece));
                }
            }
        }
        out.push(line);
    }
    out
}

/// The `[llx lly urx ury]` box a page renders into, in points.
///
/// `/MediaBox` is an inheritable attribute, so a page that does not carry one
/// takes its parent's; this walks that chain. A file that names no box anywhere
/// gets US Letter, which is what a viewer assumes too.
fn media_box(doc: &Document, page_id: ObjectId) -> [f32; 4] {
    let mut id = page_id;
    for _ in 0..PARENT_LIMIT {
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        if let Ok(arr) = dict.get_deref(b"MediaBox", doc).and_then(Object::as_array) {
            let nums: Vec<f32> = arr.iter().filter_map(|o| o.as_float().ok()).collect();
            if let [llx, lly, urx, ury] = nums[..] {
                return [llx, lly, urx, ury];
            }
        }
        let Ok(parent) = dict.get(b"Parent").and_then(Object::as_reference) else {
            break;
        };
        id = parent;
    }
    [0.0, 0.0, PAGE.0, PAGE.1]
}

/// Give the page a `/Resources` of its own that resolves to exactly what it
/// resolved to before, so that adding an entry to it cannot shadow an inherited
/// one.
///
/// This is the trap under `lopdf`'s `add_xobject`: it calls
/// `get_or_create_resources`, and *create* means an empty dictionary on the page.
/// `/Resources` is inheritable and a page's own copy **replaces** its parent's
/// rather than merging with it — so on the very common page that carries no
/// `/Resources` and inherits the document's, creating an empty one detaches every
/// font and image the page's content stream refers to. The stamp would land and
/// the page underneath it would render blank.
///
/// Copying the inherited value first makes the addition additive. A `/Resources`
/// held by reference stays a reference — the pages that shared it still share it,
/// and each page's content only ever names its own stamp.
fn own_resources(doc: &mut Document, page_id: ObjectId) -> Result<()> {
    if doc
        .get_dictionary(page_id)
        .is_ok_and(|p| p.has(b"Resources"))
    {
        return Ok(());
    }
    let mut inherited = None;
    let mut id = page_id;
    for _ in 0..PARENT_LIMIT {
        let Ok(dict) = doc.get_dictionary(id) else {
            break;
        };
        if let Ok(res) = dict.get(b"Resources") {
            inherited = Some(res.clone());
            break;
        }
        let Ok(parent) = dict.get(b"Parent").and_then(Object::as_reference) else {
            break;
        };
        id = parent;
    }
    let res = inherited.unwrap_or_else(|| Object::Dictionary(Dictionary::new()));
    doc.get_object_mut(page_id)
        .and_then(Object::as_dict_mut)
        .map_err(|e| Error::Config(format!("page {page_id:?} is not a dictionary: {e}")))?
        .set("Resources", res);
    Ok(())
}

/// Generate a PDF at `rel`, one page per string in `pages`.
///
/// Helvetica at 12pt inside a one-inch margin on US Letter, text wrapped at the
/// column and newlines in the input honoured. This is a "put this text in a PDF"
/// tool, not a typesetting engine: there are no styles, no images and no tables,
/// and the caller who wants them wants a different tool.
///
/// The one ceiling worth naming: a string is one page, so a string longer than
/// about forty-six wrapped lines runs off the bottom of its page. Text that long
/// belongs in more than one element of `pages`.
///
/// This replaces whatever is at `rel`. [`watermark`] and [`fill_form`] are the
/// paths that preserve an existing document.
// ponytail: base-14 Helvetica, no embedded font — see `encode` for what that
// costs. Embed a font when a caller needs a script Latin-1 cannot spell.
pub fn write_new(ws: &Workspace, rel: &str, pages: &[String]) -> Result<Wrote> {
    let mut doc = Document::with_version("1.5");
    let pages_id = doc.new_object_id();
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let resources_id = doc.add_object(dictionary! {
        "Font" => dictionary! { "F1" => font_id },
    });

    let mut kids = Vec::with_capacity(pages.len());
    for text in pages {
        let mut ops = vec![
            Operation::new("BT", vec![]),
            Operation::new(
                "Tf",
                vec![Object::Name(b"F1".to_vec()), Object::Real(BODY.0)],
            ),
            Operation::new("TL", vec![Object::Real(BODY.1)]),
            Operation::new(
                "Td",
                vec![Object::Real(MARGIN), Object::Real(PAGE.1 - MARGIN)],
            ),
        ];
        for line in wrap(text) {
            ops.push(Operation::new(
                "Tj",
                vec![Object::string_literal(encode(&line))],
            ));
            ops.push(Operation::new("T*", vec![]));
        }
        ops.push(Operation::new("ET", vec![]));

        let stream = Content { operations: ops }
            .encode()
            .map_err(|e| unwritable(rel, e))?;
        let content_id = doc.add_object(Stream::new(Dictionary::new(), stream));
        kids.push(Object::Reference(doc.add_object(dictionary! {
            "Type" => "Page",
            "Parent" => pages_id,
            "Contents" => content_id,
        })));
    }

    let count = kids.len() as i64;
    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => kids,
            "Count" => count,
            "Resources" => resources_id,
            "MediaBox" => vec![
                Object::Real(0.0), Object::Real(0.0),
                Object::Real(PAGE.0), Object::Real(PAGE.1),
            ],
        }),
    );
    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => pages_id,
    });
    doc.trailer.set("Root", catalog_id);

    save(ws, rel, &mut doc)
}

/// Extract the text of the PDF at `rel`.
///
/// # What this is worth
///
/// A PDF stores placed glyphs, not a document: there is no paragraph, no reading
/// order, and often no space character — a gap between two words is a coordinate,
/// not a byte. `pdf-extract` reconstructs text from those placements and does it
/// well enough to be useful, but the result is an approximation and this function
/// does not promise otherwise. Columns can interleave, tables lose their shape,
/// ligatures and unusual encodings can come back wrong, and a scanned page
/// contains no text at all and returns an empty string rather than an error.
/// Treat the output as evidence, not as the document.
///
/// # Why the panic guard
///
/// `pdf-extract` is written to panic on input it does not understand — an
/// unexpected encoding, a font it cannot map, a missing width — rather than to
/// return an error. Since a model routinely points this at whatever file it
/// found, an unguarded call turns a bad PDF into a dead run. The extraction is a
/// pure function of a byte slice, so catching the unwind is safe: there is no
/// half-mutated state left behind, only a `String` that was never produced.
///
/// The guard depends on unwinding, so it does nothing under a `panic = "abort"`
/// profile; the panic message is also still printed by the default hook before
/// this converts it into an [`Error::Config`].
pub fn read_text(ws: &Workspace, rel: &str) -> Result<String> {
    let bytes = ws.read_bytes(rel)?;
    match std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes)) {
        Ok(Ok(text)) => Ok(text),
        Ok(Err(e)) => Err(unreadable(rel, e)),
        Err(panic) => {
            let what = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "the extractor panicked".to_string());
            Err(unreadable(rel, format!("text extraction failed: {what}")))
        }
    }
}

/// Stamp `text` diagonally across every page of the PDF at `rel`, keeping what
/// is already there.
///
/// # How it composes
///
/// The stamp is a form XObject carrying its own font, invoked from a content
/// stream **appended** to the page. The page's existing content streams are not
/// decoded, re-encoded or replaced — their bytes are the same bytes afterwards,
/// which is the only way to be sure an inline image or an operator this crate
/// has never heard of survives the round trip.
///
/// Two details make the composition safe rather than merely additive:
///
/// - A `q` is **prepended** as its own stream and the matching `Q` opens the
///   appended one. Content streams for a page are concatenated before they are
///   interpreted, so an original that left the graphics state modified — an
///   unbalanced `q`, or a bare `cm` — would otherwise drag the stamp along with
///   it. The bracket puts the stamp back in the page's default space wherever
///   the original left off.
/// - The page is given a `/Resources` of its own before the stamp is registered
///   in it (see `own_resources`), because creating an empty one on a page that
///   inherits its resources detaches every font the page already used.
///
/// The stamp is drawn last, so it sits over the page rather than under it, and it
/// is light grey and opaque: where the two overlap the stamp wins, and the grey
/// is what keeps that from making the page unreadable.
// ponytail: opaque light grey, no /ExtGState alpha — a real transparent stamp
// needs a graphics-state resource per page. Add it when a caller says the stamp
// is too heavy over dense text.
pub fn watermark(ws: &Workspace, rel: &str, text: &str) -> Result<Wrote> {
    if text.is_empty() {
        return Err(Error::Config(format!(
            "cannot watermark {rel} with an empty string"
        )));
    }
    let mut doc = open(ws, rel)?;
    let font_id = doc.add_object(dictionary! {
        "Type" => "Font",
        "Subtype" => "Type1",
        "BaseFont" => "Helvetica",
        "Encoding" => "WinAnsiEncoding",
    });
    let glyphs = encode(text);

    for (_, page_id) in doc.get_pages() {
        let [llx, lly, urx, ury] = media_box(&doc, page_id);
        let (w, h) = (urx - llx, ury - lly);
        let (cx, cy) = (llx + w / 2.0, lly + h / 2.0);
        // Fit the string to the page diagonal, which is the length a 45-degree
        // stamp actually has to cross. 0.55em is Helvetica's rough average
        // advance; the clamp keeps a one-word stamp from filling the page and a
        // sentence from becoming unreadable.
        let diagonal = (w * w + h * h).sqrt();
        let size = (diagonal * 0.8 / (0.55 * glyphs.len().max(1) as f32)).clamp(8.0, 96.0);
        let half = 0.55 * size * glyphs.len() as f32 / 2.0;

        own_resources(&mut doc, page_id)?;
        let form = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Form",
                "BBox" => vec![
                    Object::Real(llx), Object::Real(lly),
                    Object::Real(urx), Object::Real(ury),
                ],
                "Resources" => dictionary! { "Font" => dictionary! { "F1" => font_id } },
            },
            Content {
                operations: vec![
                    Operation::new("q", vec![]),
                    // 0.75 grey: legible as a stamp, still letting the page read
                    // through it.
                    Operation::new(
                        "rg",
                        vec![Object::Real(0.75), Object::Real(0.75), Object::Real(0.75)],
                    ),
                    // Rotate 45 degrees about the page centre: the `cm` matrix
                    // [cos sin -sin cos tx ty], and sin 45 = cos 45.
                    Operation::new(
                        "cm",
                        vec![
                            Object::Real(DIAGONAL),
                            Object::Real(DIAGONAL),
                            Object::Real(-DIAGONAL),
                            Object::Real(DIAGONAL),
                            Object::Real(cx),
                            Object::Real(cy),
                        ],
                    ),
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Real(size)]),
                    Operation::new("Td", vec![Object::Real(-half), Object::Real(0.0)]),
                    Operation::new("Tj", vec![Object::string_literal(glyphs.clone())]),
                    Operation::new("ET", vec![]),
                    Operation::new("Q", vec![]),
                ],
            }
            .encode()
            .map_err(|e| unwritable(rel, e))?,
        );
        let form_id = doc.add_object(form);
        let name = format!("IoWm{}_{}", form_id.0, form_id.1);
        doc.add_xobject(page_id, name.as_bytes(), form_id)
            .map_err(|e| unwritable(rel, e))?;

        let mut contents: Vec<Object> = doc
            .get_page_contents(page_id)
            .into_iter()
            .map(Object::Reference)
            .collect();
        let opener = doc.add_object(Stream::new(Dictionary::new(), b"q\n".to_vec()));
        let stamp = doc.add_object(Stream::new(
            Dictionary::new(),
            format!("Q\nq\n/{name} Do\nQ\n").into_bytes(),
        ));
        contents.insert(0, Object::Reference(opener));
        contents.push(Object::Reference(stamp));
        doc.get_object_mut(page_id)
            .and_then(Object::as_dict_mut)
            .map_err(|e| unwritable(rel, e))?
            .set("Contents", contents);
    }

    save(ws, rel, &mut doc)
}

/// Every terminal AcroForm field, as `(fully qualified name, leaf name, id)`.
///
/// A field's real name is the `/T` of every node from the form root down to it,
/// joined with `.` — so a field is addressable both as `address.city` and, when
/// the leaf is unambiguous, as `city`. Nodes under `/Kids` that carry no `/T` are
/// widgets rather than fields and are not descended into.
fn form_fields(doc: &Document) -> Vec<(String, String, ObjectId)> {
    fn walk(
        doc: &Document,
        ids: &[ObjectId],
        prefix: &str,
        depth: usize,
        out: &mut Vec<(String, String, ObjectId)>,
    ) {
        if depth > PARENT_LIMIT {
            return;
        }
        for id in ids {
            let Ok(dict) = doc.get_dictionary(*id) else {
                continue;
            };
            let leaf = dict
                .get(b"T")
                .and_then(Object::as_str)
                .map(|s| String::from_utf8_lossy(s).to_string())
                .unwrap_or_default();
            if leaf.is_empty() {
                continue;
            }
            let qualified = if prefix.is_empty() {
                leaf.clone()
            } else {
                format!("{prefix}.{leaf}")
            };
            let kids: Vec<ObjectId> = dict
                .get_deref(b"Kids", doc)
                .and_then(Object::as_array)
                .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
                .unwrap_or_default();
            // A kid with its own /T is a child field; a kid without one is this
            // field's widget, and this node is still the terminal field.
            let named_kids: Vec<ObjectId> = kids
                .iter()
                .copied()
                .filter(|k| {
                    doc.get_dictionary(*k)
                        .is_ok_and(|d| d.get(b"T").and_then(Object::as_str).is_ok())
                })
                .collect();
            if named_kids.is_empty() {
                out.push((qualified, leaf, *id));
            } else {
                walk(doc, &named_kids, &qualified, depth + 1, out);
            }
        }
    }

    let Ok(acro) = acroform(doc) else {
        return Vec::new();
    };
    let roots: Vec<ObjectId> = acro
        .get_deref(b"Fields", doc)
        .and_then(Object::as_array)
        .map(|a| a.iter().filter_map(|o| o.as_reference().ok()).collect())
        .unwrap_or_default();
    let mut out = Vec::new();
    walk(doc, &roots, "", 0, &mut out);
    out
}

/// The document's `/AcroForm` dictionary, whether the catalog holds it inline or
/// by reference.
fn acroform(doc: &Document) -> Result<&Dictionary> {
    let catalog = doc
        .catalog()
        .map_err(|e| Error::Config(format!("the PDF has no catalog: {e}")))?;
    catalog
        .get_deref(b"AcroForm", doc)
        .and_then(Object::as_dict)
        .map_err(|_| Error::Config("the PDF has no AcroForm (it contains no form fields)".into()))
}

/// Set AcroForm field values in the PDF at `rel`.
///
/// `fields` is `(name, value)`; a name is either a field's fully qualified name
/// (`address.city`) or its leaf name when that is unambiguous in the document.
/// A name that matches nothing — or that matches more than one field — is an
/// error naming the fields the document does have, because a fill that silently
/// dropped a value would be reported as a success.
///
/// # The appearance problem, and what this does about it
///
/// A form field holds its value in `/V` and holds what a viewer *draws* in a
/// separate appearance stream, `/AP`. Setting `/V` alone changes the data and not
/// the picture: the field keeps rendering whatever `/AP` says, which for a blank
/// form is blank. A tool that stopped at `/V` would produce a file that passes
/// every programmatic check and shows an empty form to every human.
///
/// The two fixes are to regenerate `/AP` per field, or to set
/// `/NeedAppearances true` on the AcroForm dictionary and let the viewer
/// regenerate it. **This sets `/NeedAppearances`**, because generating a faithful
/// appearance stream means re-implementing the viewer's text layout — font
/// metrics from `/DA`, the field's quadding, multiline and comb behaviour,
/// auto-sizing when the size is `0` — and an appearance stream that is *wrong* is
/// worse than none, since it is what gets drawn and printed.
///
/// It also **removes the stale `/AP`** from every field it fills. A viewer that
/// honours `/NeedAppearances` was going to rebuild it anyway; a viewer that does
/// not would otherwise draw the value the field had *before* the fill. Blank is a
/// visible failure; the old value silently presented as the new one is not.
///
/// Checkbox and radio fields (`/FT /Btn`) are the exception on both counts: their
/// value is a name selecting a state out of `/AP`, not a string drawn into one,
/// so this sets `/V` and `/AS` as names and leaves `/AP` alone.
///
/// # What it cannot promise
///
/// `/NeedAppearances` is honoured by Acrobat, by pdf.js and by the common desktop
/// viewers, but it is a request to the viewer and not a rendered result. A viewer
/// that ignores it will show the filled text fields empty. If the output has to
/// render identically everywhere without cooperation, it has to be flattened by
/// something that lays out glyphs, and that is not this function.
// ponytail: /AS is set on the field dictionary, which is correct for the common
// merged field-and-widget node and for a radio group (whose /T and /FT sit on
// the group, which is the node this treats as terminal). A group that puts /T on
// its widget kids, or a parent that declares /FT for kids that do not, needs the
// /Parent chain walked for both; add that when a caller has one.
pub fn fill_form(ws: &Workspace, rel: &str, fields: &[(String, String)]) -> Result<Wrote> {
    let mut doc = open(ws, rel)?;
    let known = form_fields(&doc);
    if known.is_empty() {
        // Distinguishes "no AcroForm" from "an AcroForm with no fields" only in
        // as much as the message can; either way there is nothing to fill.
        acroform(&doc)?;
        return Err(Error::Config(format!("{rel} has no fillable form fields")));
    }

    // Resolve every name before mutating anything, so a typo in the third field
    // does not leave the first two written.
    let mut targets: Vec<(ObjectId, &str)> = Vec::with_capacity(fields.len());
    for (name, value) in fields {
        let hits: Vec<ObjectId> = known
            .iter()
            .filter(|(q, leaf, _)| q == name || leaf == name)
            .map(|(_, _, id)| *id)
            .collect();
        match hits[..] {
            [id] => targets.push((id, value.as_str())),
            [] => {
                let names: Vec<&str> = known.iter().map(|(q, _, _)| q.as_str()).collect();
                return Err(Error::Config(format!(
                    "{rel} has no form field named {name:?}; it has: {}",
                    names.join(", ")
                )));
            }
            _ => {
                return Err(Error::Config(format!(
                    "{name:?} names more than one field in {rel}; use the fully qualified name"
                )))
            }
        }
    }

    for (id, value) in targets {
        let is_button = doc
            .get_dictionary(id)
            .and_then(|d| d.get_deref(b"FT", &doc).and_then(Object::as_name))
            .is_ok_and(|ft| ft == b"Btn");
        let field = doc
            .get_object_mut(id)
            .and_then(Object::as_dict_mut)
            .map_err(|e| unwritable(rel, e))?;
        if is_button {
            field.set("V", Object::Name(encode(value)));
            field.set("AS", Object::Name(encode(value)));
        } else {
            field.set("V", Object::string_literal(encode(value)));
            field.remove(b"AP");
        }
    }

    // The AcroForm dictionary itself, inline in the catalog or by reference.
    let acro_ref = doc
        .catalog()
        .ok()
        .and_then(|c| c.get(b"AcroForm").ok())
        .and_then(|o| o.as_reference().ok());
    match acro_ref {
        Some(id) => doc
            .get_dictionary_mut(id)
            .map_err(|e| unwritable(rel, e))?
            .set("NeedAppearances", true),
        None => doc
            .catalog_mut()
            .and_then(|c| c.get_mut(b"AcroForm"))
            .and_then(Object::as_dict_mut)
            .map_err(|e| unwritable(rel, e))?
            .set("NeedAppearances", true),
    }

    save(ws, rel, &mut doc)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

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

    /// A PDF with an AcroForm, one text field and one checkbox, built in memory.
    ///
    /// Built with `lopdf` directly rather than with [`write_new`], because
    /// [`write_new`] emits no form at all — a fixture from the module's own writer
    /// could only ever contain what that writer knows how to emit. The text field
    /// carries a deliberately stale `/AP`, which is what [`fill_form`] has to
    /// remove.
    ///
    /// It is still weaker than a real form: no `/DA` inheritance games, no radio
    /// group with separate widget kids, no XFA. A real fixture belongs under
    /// `tests/fixtures/` and this is not a substitute for it.
    fn form_fixture(ws: &Workspace, rel: &str) {
        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let page_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1",
            "BaseFont" => "Helvetica", "Encoding" => "WinAnsiEncoding",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "Helv" => font_id },
        });
        let content_id = doc.add_object(Stream::new(
            Dictionary::new(),
            Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new(
                        "Tf",
                        vec![Object::Name(b"Helv".to_vec()), Object::Real(12.0)],
                    ),
                    Operation::new("Td", vec![Object::Real(72.0), Object::Real(700.0)]),
                    Operation::new("Tj", vec![Object::string_literal("Membership application")]),
                    Operation::new("ET", vec![]),
                ],
            }
            .encode()
            .unwrap(),
        ));
        // The stale appearance: what a viewer would draw if /V alone changed.
        let stale_ap = doc.add_object(Stream::new(
            dictionary! {
                "Type" => "XObject", "Subtype" => "Form",
                "BBox" => vec![Object::Real(0.0), Object::Real(0.0),
                               Object::Real(300.0), Object::Real(20.0)],
            },
            b"/Tx BMC EMC".to_vec(),
        ));
        let text_field = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Tx",
            "T" => Object::string_literal("full_name"),
            "V" => Object::string_literal(""),
            "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
            "Rect" => vec![Object::Real(100.0), Object::Real(600.0),
                           Object::Real(400.0), Object::Real(620.0)],
            "P" => page_id,
            "AP" => dictionary! { "N" => stale_ap },
        });
        let check_field = doc.add_object(dictionary! {
            "Type" => "Annot", "Subtype" => "Widget", "FT" => "Btn",
            "T" => Object::string_literal("subscribe"),
            "V" => Object::Name(b"Off".to_vec()),
            "AS" => Object::Name(b"Off".to_vec()),
            "Rect" => vec![Object::Real(100.0), Object::Real(560.0),
                           Object::Real(120.0), Object::Real(580.0)],
            "P" => page_id,
        });
        doc.objects.insert(
            page_id,
            Object::Dictionary(dictionary! {
                "Type" => "Page",
                "Parent" => pages_id,
                "Contents" => content_id,
                "Annots" => vec![Object::Reference(text_field), Object::Reference(check_field)],
            }),
        );
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1_i64,
                "Resources" => resources_id,
                "MediaBox" => vec![Object::Real(0.0), Object::Real(0.0),
                                   Object::Real(612.0), Object::Real(792.0)],
            }),
        );
        let acro_id = doc.add_object(dictionary! {
            "Fields" => vec![Object::Reference(text_field), Object::Reference(check_field)],
            "DA" => Object::string_literal("/Helv 12 Tf 0 g"),
            "DR" => resources_id,
        });
        let catalog_id = doc.add_object(dictionary! {
            "Type" => "Catalog",
            "Pages" => pages_id,
            "AcroForm" => acro_id,
        });
        doc.trailer.set("Root", catalog_id);

        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        ws.write_bytes(rel, &buf).unwrap();
    }

    #[test]
    fn write_new_then_read_text_round_trips_recognisable_text() {
        let d = dir();
        let ws = Workspace::new(d.path());

        assert_eq!(
            write_new(
                &ws,
                "out/report.pdf",
                &[
                    String::from("Quarterly revenue rose"),
                    String::from("Headcount held flat")
                ],
            )
            .unwrap(),
            Wrote::Created
        );

        let text = read_text(&ws, "out/report.pdf").unwrap();
        assert!(
            text.contains("Quarterly revenue rose"),
            "the first page's text came back: {text:?}"
        );
        assert!(
            text.contains("Headcount held flat"),
            "and the second page's: {text:?}"
        );
    }

    #[test]
    fn write_new_wraps_a_long_line_without_losing_any_of_its_words() {
        let d = dir();
        let ws = Workspace::new(d.path());
        let long = "alpha ".repeat(60) + "omega";

        write_new(&ws, "long.pdf", &[long]).unwrap();

        let text = read_text(&ws, "long.pdf").unwrap();
        assert_eq!(
            text.matches("alpha").count(),
            60,
            "every word survived the wrap: {text:?}"
        );
        assert!(text.contains("omega"), "including the last: {text:?}");
    }

    /// Both halves matter. The first catches a watermark that never landed; the
    /// second catches one that landed by flattening the page it landed on.
    #[test]
    fn watermarking_stamps_every_page_and_leaves_the_original_text_extractable() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(
            &ws,
            "deck.pdf",
            &[
                String::from("First page body"),
                String::from("Second page body"),
                String::from("Third page body"),
            ],
        )
        .unwrap();

        assert_eq!(watermark(&ws, "deck.pdf", "DRAFT").unwrap(), Wrote::Changed);

        let bytes = ws.read_bytes("deck.pdf").unwrap();
        let per_page = pdf_extract::extract_text_from_mem_by_pages(&bytes).unwrap();
        assert_eq!(per_page.len(), 3, "no page was added or dropped");
        for (i, text) in per_page.iter().enumerate() {
            assert!(
                text.contains("DRAFT"),
                "page {} carries the stamp: {text:?}",
                i + 1
            );
        }
        assert!(per_page[0].contains("First page body"), "{:?}", per_page[0]);
        assert!(
            per_page[1].contains("Second page body"),
            "{:?}",
            per_page[1]
        );
        assert!(per_page[2].contains("Third page body"), "{:?}", per_page[2]);
    }

    /// The structural half of the same claim: the original content streams are
    /// still there, byte for byte, rather than re-encoded into something that
    /// happens to still extract.
    #[test]
    fn watermarking_appends_to_the_page_rather_than_replacing_its_content() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(&ws, "one.pdf", &[String::from("Original body text")]).unwrap();

        let before = {
            let doc = Document::load_mem(&ws.read_bytes("one.pdf").unwrap()).unwrap();
            let page_id = *doc.get_pages().values().next().unwrap();
            doc.get_page_content(page_id)
        };
        watermark(&ws, "one.pdf", "CONFIDENTIAL").unwrap();

        let doc = Document::load_mem(&ws.read_bytes("one.pdf").unwrap()).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let after = doc.get_page_content(page_id);
        assert!(
            after.windows(before.len()).any(|w| w == before.as_slice()),
            "the original content stream is still present verbatim inside the new one"
        );
        assert!(
            after.len() > before.len(),
            "and something was added around it"
        );
    }

    /// Stamping twice must not eat the first stamp — the same property as
    /// preserving the original content, applied to this module's own output.
    #[test]
    fn watermarking_twice_leaves_both_stamps_and_the_body() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(&ws, "twice.pdf", &[String::from("Body survives")]).unwrap();

        watermark(&ws, "twice.pdf", "DRAFT").unwrap();
        watermark(&ws, "twice.pdf", "COPY").unwrap();

        let text = read_text(&ws, "twice.pdf").unwrap();
        for expected in ["Body survives", "DRAFT", "COPY"] {
            assert!(text.contains(expected), "{expected} is missing: {text:?}");
        }
    }

    /// A page that inherits its `/Resources` is the case where a careless stamp
    /// detaches the page's own font. `write_new` produces exactly that shape, so
    /// this asserts the page ends up with resources that still name `F1`.
    #[test]
    fn watermarking_a_page_with_inherited_resources_keeps_its_own_font_reachable() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(&ws, "inherit.pdf", &[String::from("Needs its font")]).unwrap();
        watermark(&ws, "inherit.pdf", "DRAFT").unwrap();

        let doc = Document::load_mem(&ws.read_bytes("inherit.pdf").unwrap()).unwrap();
        let page_id = *doc.get_pages().values().next().unwrap();
        let fonts = doc.get_page_fonts(page_id).unwrap();
        assert!(
            fonts.contains_key(b"F1".as_slice()),
            "the inherited font is still resolvable from the page, got {:?}",
            fonts.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn filling_a_form_sets_the_value_and_asks_the_viewer_to_redraw_it() {
        let d = dir();
        let ws = Workspace::new(d.path());
        form_fixture(&ws, "form.pdf");

        assert_eq!(
            fill_form(
                &ws,
                "form.pdf",
                &[
                    ("full_name".to_string(), "Ada Lovelace".to_string()),
                    ("subscribe".to_string(), "Yes".to_string()),
                ],
            )
            .unwrap(),
            Wrote::Changed
        );

        let doc = Document::load_mem(&ws.read_bytes("form.pdf").unwrap()).unwrap();
        let by_name: BTreeMap<String, ObjectId> = form_fields(&doc)
            .into_iter()
            .map(|(q, _, id)| (q, id))
            .collect();

        let text = doc.get_dictionary(by_name["full_name"]).unwrap();
        assert_eq!(
            text.get(b"V").unwrap().as_str().unwrap(),
            b"Ada Lovelace",
            "the value landed"
        );
        // The half that a /V-only implementation fails: the value has to be
        // drawable, not merely stored.
        assert!(
            !text.has(b"AP"),
            "the stale appearance stream is gone, so no viewer can draw the old value"
        );
        let acro = acroform(&doc).unwrap();
        assert!(
            acro.get(b"NeedAppearances")
                .and_then(Object::as_bool)
                .unwrap(),
            "and the AcroForm asks the viewer to build the new appearance"
        );

        // Checkboxes take a name, not a string, and keep their /AP.
        let check = doc.get_dictionary(by_name["subscribe"]).unwrap();
        assert_eq!(check.get(b"V").unwrap().as_name().unwrap(), b"Yes");
        assert_eq!(check.get(b"AS").unwrap().as_name().unwrap(), b"Yes");
    }

    #[test]
    fn naming_a_field_that_is_not_there_lists_the_ones_that_are() {
        let d = dir();
        let ws = Workspace::new(d.path());
        form_fixture(&ws, "form.pdf");

        let e = fill_form(
            &ws,
            "form.pdf",
            &[("surname".to_string(), "Lovelace".to_string())],
        )
        .unwrap_err();
        let shown = e.to_string();
        assert!(
            shown.contains("surname") && shown.contains("full_name") && shown.contains("subscribe"),
            "the error tells the model what it could have asked for, got {shown}"
        );
    }

    /// A bad name in the batch must not half-fill the document.
    #[test]
    fn a_batch_with_one_unknown_field_writes_none_of_it() {
        let d = dir();
        let ws = Workspace::new(d.path());
        form_fixture(&ws, "form.pdf");
        let before = ws.read_bytes("form.pdf").unwrap();

        assert!(fill_form(
            &ws,
            "form.pdf",
            &[
                ("full_name".to_string(), "Ada Lovelace".to_string()),
                ("nope".to_string(), "x".to_string()),
            ],
        )
        .is_err());

        assert_eq!(
            ws.read_bytes("form.pdf").unwrap(),
            before,
            "the file on disk is untouched"
        );
    }

    #[test]
    fn a_pdf_without_a_form_is_an_error_not_a_silent_success() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(&ws, "plain.pdf", &[String::from("no fields here")]).unwrap();

        let e = fill_form(&ws, "plain.pdf", &[("a".to_string(), "b".to_string())]).unwrap_err();
        assert!(
            e.to_string().contains("AcroForm"),
            "the message says what the file is missing, got {e}"
        );
    }

    /// A file that is not a PDF is the model's mistake, not the harness's: it has
    /// to come back as something the model can read and correct. `pdf-extract`
    /// panics rather than errors on plenty of input, which is why [`read_text`]
    /// catches the unwind — this is the test that keeps that guard honest.
    #[test]
    fn a_file_that_is_not_a_pdf_is_an_error_not_a_panic() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.pdf", b"this is plainly not a PDF")
            .unwrap();
        // A file with the right header and nothing behind it: past the cheapest
        // sniff, still unparseable.
        ws.write_bytes("truncated.pdf", b"%PDF-1.7\n1 0 obj\n<< /Type /Cat")
            .unwrap();

        for rel in ["notes.pdf", "truncated.pdf"] {
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

    /// The guard in [`read_text`] exists because `pdf-extract` panics rather than
    /// erroring on structures it does not handle. This is a PDF that parses fine
    /// and then hits one of those panics — an `/Encoding` name the extractor has
    /// no table for — so the guard is measured rather than assumed.
    #[test]
    fn a_pdf_that_makes_the_extractor_panic_comes_back_as_an_error() {
        let d = dir();
        let ws = Workspace::new(d.path());

        let mut doc = Document::with_version("1.5");
        let pages_id = doc.new_object_id();
        let font_id = doc.add_object(dictionary! {
            "Type" => "Font", "Subtype" => "Type1",
            "BaseFont" => "Helvetica", "Encoding" => "NoSuchEncoding",
        });
        let resources_id = doc.add_object(dictionary! {
            "Font" => dictionary! { "F1" => font_id },
        });
        let content_id = doc.add_object(Stream::new(
            Dictionary::new(),
            Content {
                operations: vec![
                    Operation::new("BT", vec![]),
                    Operation::new("Tf", vec![Object::Name(b"F1".to_vec()), Object::Real(12.0)]),
                    Operation::new("Td", vec![Object::Real(72.0), Object::Real(700.0)]),
                    Operation::new("Tj", vec![Object::string_literal("boom")]),
                    Operation::new("ET", vec![]),
                ],
            }
            .encode()
            .unwrap(),
        ));
        let page_id = doc.add_object(dictionary! {
            "Type" => "Page", "Parent" => pages_id, "Contents" => content_id,
        });
        doc.objects.insert(
            pages_id,
            Object::Dictionary(dictionary! {
                "Type" => "Pages",
                "Kids" => vec![Object::Reference(page_id)],
                "Count" => 1_i64,
                "Resources" => resources_id,
                "MediaBox" => vec![Object::Real(0.0), Object::Real(0.0),
                                   Object::Real(612.0), Object::Real(792.0)],
            }),
        );
        let catalog_id = doc.add_object(dictionary! { "Type" => "Catalog", "Pages" => pages_id });
        doc.trailer.set("Root", catalog_id);
        let mut buf = Vec::new();
        doc.save_to(&mut buf).unwrap();
        ws.write_bytes("hostile.pdf", &buf).unwrap();

        let err = read_text(&ws, "hostile.pdf").unwrap_err();
        let shown = err.to_string();
        assert!(
            shown.contains("hostile.pdf") && shown.contains("text extraction failed"),
            "the run survives and the model is told which file did it, got {shown}"
        );
        // The control: the very same code path on a file the extractor handles.
        write_new(&ws, "fine.pdf", &[String::from("readable")]).unwrap();
        assert!(read_text(&ws, "fine.pdf").unwrap().contains("readable"));
    }

    #[test]
    fn a_denied_path_is_refused_for_reading_and_for_writing() {
        let d = dir();
        // Written through a permissive workspace so the files genuinely exist: a
        // refusal on a missing file would prove nothing.
        let open = Workspace::new(d.path());
        write_new(&open, "secrets/doc.pdf", &[String::from("classified")]).unwrap();
        form_fixture(&open, "secrets/form.pdf");
        let ws = guarded(d.path());

        let read = read_text(&ws, "secrets/doc.pdf");
        assert!(
            matches!(&read, Err(Error::Refused { act, target, .. })
                if act == "read" && target == "secrets/doc.pdf"),
            "got {read:?}"
        );
        let written = write_new(&ws, "secrets/new.pdf", &[String::from("x")]);
        assert!(
            matches!(&written, Err(Error::Refused { act, target, .. })
                if act == "write" && target == "secrets/new.pdf"),
            "got {written:?}"
        );
        for edit in [
            watermark(&ws, "secrets/doc.pdf", "DRAFT"),
            fill_form(
                &ws,
                "secrets/form.pdf",
                &[("full_name".to_string(), "x".to_string())],
            ),
        ] {
            assert!(
                matches!(&edit, Err(Error::Refused { act, .. }) if act == "read"),
                "the edit is stopped at the read, before a byte of it is parsed: got {edit:?}"
            );
        }
    }

    /// The negative control for the test above. Same operations, same workspace,
    /// paths the same policy allows — so the refusals measure the boundary and not
    /// operations that would have failed anywhere.
    #[test]
    fn the_same_operations_succeed_on_a_path_the_policy_allows() {
        let d = dir();
        let ws = guarded(d.path());

        assert_eq!(
            write_new(&ws, "open/doc.pdf", &[String::from("classified")]).unwrap(),
            Wrote::Created
        );
        assert!(read_text(&ws, "open/doc.pdf")
            .unwrap()
            .contains("classified"));
        assert_eq!(
            watermark(&ws, "open/doc.pdf", "DRAFT").unwrap(),
            Wrote::Changed
        );

        form_fixture(&ws, "open/form.pdf");
        assert_eq!(
            fill_form(
                &ws,
                "open/form.pdf",
                &[("full_name".to_string(), "Ada".to_string())]
            )
            .unwrap(),
            Wrote::Changed
        );
    }

    #[test]
    fn an_empty_watermark_is_refused_rather_than_stamping_nothing() {
        let d = dir();
        let ws = Workspace::new(d.path());
        write_new(&ws, "doc.pdf", &[String::from("body")]).unwrap();

        assert!(watermark(&ws, "doc.pdf", "").is_err());
    }

    #[test]
    fn text_outside_latin_1_becomes_a_visible_substitute_rather_than_a_wrong_glyph() {
        assert_eq!(encode("caf\u{e9}"), b"caf\xe9");
        assert_eq!(encode("\u{65e5}\u{672c}"), b"??");
    }

    #[test]
    fn wrapping_keeps_every_line_within_the_column() {
        let lines = wrap(&format!("{} tail", "x".repeat(200)));
        assert!(lines.iter().all(|l| l.chars().count() <= WRAP), "{lines:?}");
        let joined: String = lines.concat();
        assert!(joined.contains("tail"));
        assert_eq!(joined.matches('x').count(), 200, "no character was dropped");
    }
}
