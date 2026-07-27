#![cfg(feature = "barcode")]
//! Barcodes: read every 1D and 2D symbol out of an image the agent can already
//! see.
//!
//! `rxing` is the zxing port, so the symbologies come as a set rather than one
//! at a time — QR, Data Matrix, Aztec, PDF417, Code 128, Code 39/93, EAN-8/13,
//! UPC-A/E, ITF, Codabar. This module does not pick between them; it hands the
//! decoder a greyscale image and reports what came back, because a model that
//! had to name the symbology before it could read the label would need to
//! already know the answer.
//!
//! **The image is read through [`Workspace::read_bytes`] and decoded in
//! memory.** `rxing` ships `detect_in_file`/`detect_multiple_in_file` helpers
//! that take a path and call `image::open` themselves — they are not used here,
//! and that is the whole point: a path handed to a library is a read that never
//! passes the policy gate, so `deny_read("secrets/*")` would stop `read_file`
//! and not stop this.
//!
//! # Found nothing is not an error
//!
//! An image with no code in it returns `Ok(vec![])`. "I looked and there was
//! nothing there" is an observation the model can act on — crop, rotate, try the
//! other attachment — while an `Err` reads as a malfunction and invites a retry
//! of the identical call. An image that cannot be decoded *as an image* is the
//! other thing entirely, and that is an [`Err`].

use rxing::RXingResult;

use crate::error::{Error, Result};
use crate::tools::workspace::Workspace;

/// One decoded symbol: what it says and which symbology it was.
///
/// `format` is `rxing`'s own variant name for the symbology (`"QR_CODE"`,
/// `"CODE_128"`, `"EAN_13"`) rather than a prettier one of ours, so the string a
/// model reads back is the string it can search for. Taken from the enum's
/// `Debug` and not its `Display`, which renders lowercase prose (`"code 128"`) —
/// fine for a human, a poor token for a model to match on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decoded {
    /// The payload the symbol carries.
    pub text: String,
    /// The symbology it was encoded in.
    pub format: String,
}

impl From<&RXingResult> for Decoded {
    fn from(r: &RXingResult) -> Self {
        Self {
            text: r.getText().to_string(),
            format: format!("{:?}", r.getBarcodeFormat()),
        }
    }
}

/// Decode every barcode in the image at `rel`.
///
/// PNG and JPEG are the declared formats; whatever else the `image` build
/// supports is decoded too rather than refused, since a caller who managed to
/// put a GIF in the workspace is better served by an answer than by a lecture.
///
/// # Cost
///
/// Two readers, in sequence, and the order is the point. `rxing`'s multi-symbol
/// reader is slow — it re-runs the single reader over sub-regions of the image
/// hunting for more — and the case a real run hits most is the image with
/// nothing in it, which is exactly the case that makes it search hardest before
/// giving up. So the single-symbol reader runs first as a gate: if it finds
/// nothing, this returns empty at single-reader cost and the expensive one is
/// never constructed. Only an image that provably contains at least one symbol
/// pays for the sweep that looks for its neighbours, and if that sweep fails or
/// comes back empty the gate's own result stands.
///
/// # Errors
///
/// [`Error::Refused`] if the policy denies reading `rel`, and [`Error::Config`]
/// if the file is missing or is not an image. Never an error for "no barcode
/// here" — see the module docs.
pub fn decode(ws: &Workspace, rel: &str) -> Result<Vec<Decoded>> {
    let bytes = ws.read_bytes(rel)?;
    // Decoded here, not inside rxing, so that "these bytes are not an image"
    // and "this image has no barcode" stay distinguishable: every rxing failure
    // below is the second thing, because the first one already returned.
    let img = image::load_from_memory(&bytes)
        .map_err(|e| Error::Config(format!("{rel} is not a readable image: {e}")))?;
    let (width, height) = (img.width(), img.height());
    let luma = img.to_luma8().into_raw();

    // Every failure from here down is "no symbol found", including the
    // zero-dimension and malformed-symbol cases the readers report as errors.
    let Ok(first) = rxing::helpers::detect_in_luma(luma.clone(), width, height, None) else {
        return Ok(Vec::new());
    };
    match rxing::helpers::detect_multiple_in_luma(luma, width, height) {
        Ok(all) if !all.is_empty() => Ok(all.iter().map(Decoded::from).collect()),
        _ => Ok(vec![Decoded::from(&first)]),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::Policy;
    use rxing::{BarcodeFormat, Writer};

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
                .deny_read("secrets/*"),
        )
    }

    /// Encode `text` as `format` and write it into the workspace as a PNG.
    ///
    /// Generated rather than committed as a fixture: a checked-in binary is a
    /// thing nobody can review, and `rxing` writes the symbologies it reads. The
    /// weakness is the same one that buys the convenience — this exercises the
    /// decoder against its own encoder's output, which is cleaner than a phone
    /// photo of a crumpled label ever is. A scanned fixture belongs under
    /// `tests/fixtures/` and this is not a substitute for it.
    fn symbol(ws: &Workspace, rel: &str, text: &str, format: BarcodeFormat, w: i32, h: i32) {
        let matrix = rxing::MultiFormatWriter
            .encode(text, &format, w, h)
            .unwrap();
        let img: image::DynamicImage = (&matrix).into();
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        ws.write_bytes(rel, &png.into_inner()).unwrap();
    }

    /// A plain white PNG: an image, valid, with nothing in it.
    fn blank(ws: &Workspace, rel: &str) {
        let img = image::DynamicImage::ImageLuma8(image::GrayImage::from_pixel(
            240,
            240,
            image::Luma([255u8]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        img.write_to(&mut png, image::ImageFormat::Png).unwrap();
        ws.write_bytes(rel, &png.into_inner()).unwrap();
    }

    #[test]
    fn a_qr_code_decodes_to_the_payload_it_was_written_with() {
        let d = dir();
        let ws = Workspace::new(d.path());
        symbol(
            &ws,
            "qr.png",
            "io-harness 0.14.0",
            BarcodeFormat::QR_CODE,
            300,
            300,
        );

        assert_eq!(
            decode(&ws, "qr.png").unwrap(),
            vec![Decoded {
                text: "io-harness 0.14.0".to_string(),
                format: "QR_CODE".to_string(),
            }]
        );
    }

    /// The 1D half: a different reader entirely inside `rxing`, so QR passing
    /// says nothing about this one.
    #[test]
    fn a_one_dimensional_symbology_decodes_to_the_payload_it_was_written_with() {
        let d = dir();
        let ws = Workspace::new(d.path());
        symbol(
            &ws,
            "c128.png",
            "IO-HARNESS-0140",
            BarcodeFormat::CODE_128,
            400,
            120,
        );

        let found = decode(&ws, "c128.png").unwrap();
        assert_eq!(
            found,
            vec![Decoded {
                text: "IO-HARNESS-0140".to_string(),
                format: "CODE_128".to_string(),
            }],
            "got {found:?}"
        );
    }

    /// The acceptance criterion, and the case a real run hits most: nothing
    /// found is a result, not a failure.
    #[test]
    fn an_image_with_no_barcode_in_it_returns_an_empty_vec_not_an_error() {
        let d = dir();
        let ws = Workspace::new(d.path());
        blank(&ws, "blank.png");

        let found = decode(&ws, "blank.png");
        assert!(
            matches!(&found, Ok(v) if v.is_empty()),
            "an empty image is an observation the model can act on, \
             so this is Ok(vec![]) and not an Err: got {found:?}"
        );
    }

    /// The other side of that line: bytes that are not an image at all are the
    /// model's mistake and have to come back as something it can correct —
    /// without taking the run down on the way.
    #[test]
    fn a_file_that_is_not_an_image_is_an_error_not_a_panic() {
        let d = dir();
        let ws = Workspace::new(d.path());
        ws.write_bytes("notes.png", b"this is plainly not a PNG")
            .unwrap();

        let shown = decode(&ws, "notes.png").unwrap_err().to_string();
        assert!(
            shown.contains("notes.png") && shown.contains("not a readable image"),
            "the message names the file and what is wrong with it, got {shown}"
        );
        // A missing file is the third case, and it is not "corrupt" either.
        assert!(decode(&ws, "gone.png")
            .unwrap_err()
            .to_string()
            .contains("no such file"));
    }

    #[test]
    fn a_denied_path_is_refused_before_a_pixel_is_decoded() {
        let d = dir();
        // Written through a permissive workspace so the file genuinely exists:
        // a refusal on a missing file would prove nothing.
        symbol(
            &Workspace::new(d.path()),
            "secrets/badge.png",
            "employee-4471",
            BarcodeFormat::QR_CODE,
            300,
            300,
        );
        let ws = guarded(d.path());

        let refused = decode(&ws, "secrets/badge.png");
        assert!(
            matches!(&refused, Err(Error::Refused { act, target, .. })
                if act == "read" && target == "secrets/badge.png"),
            "got {refused:?}"
        );
    }

    /// The negative control for the test above. Same image, same payload, same
    /// workspace, a path the same policy allows — so the refusal measures the
    /// boundary and not a decode that would have failed anywhere.
    #[test]
    fn the_same_decode_succeeds_on_a_path_the_policy_allows() {
        let d = dir();
        let ws = guarded(d.path());
        symbol(
            &ws,
            "open/badge.png",
            "employee-4471",
            BarcodeFormat::QR_CODE,
            300,
            300,
        );

        assert_eq!(
            decode(&ws, "open/badge.png").unwrap(),
            vec![Decoded {
                text: "employee-4471".to_string(),
                format: "QR_CODE".to_string(),
            }]
        );
    }
}
