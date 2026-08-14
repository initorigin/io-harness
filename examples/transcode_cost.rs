//! What the image door costs (0.55.0).
//!
//! 0.55.0 widened what `view_image` and `Media` accept: the four types every
//! provider documents pass through byte-identically, and BMP, TIFF, ICO, TGA and
//! PNM are decoded and re-encoded to PNG. This measures the difference between
//! those two paths, on one machine, so an operator can decide whether a format
//! is worth converting before the run instead of during it.
//!
//! **Nothing here is a gate.** No test asserts any of it; a duration asserted on
//! a CI runner is a flake waiting to be written. The numbers go in
//! `docs/MEASUREMENTS.md` with the machine named.
//!
//! ```text
//! cargo run --release --features media --example transcode_cost
//! ```

use std::time::Instant;

use io_harness::Media;

/// A real image of the given size, encoded in `format` — the same shape of input
/// the door actually receives.
fn fixture(format: image::ImageFormat, side: u32) -> Vec<u8> {
    let mut img = image::RgbaImage::new(side, side);
    for (x, y, pixel) in img.enumerate_pixels_mut() {
        // Gradient rather than flat colour: a solid image compresses to nothing
        // and would measure the encoder's best case rather than an ordinary one.
        *pixel = image::Rgba([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]);
    }
    let source = image::DynamicImage::ImageRgba8(img);
    let source = match format {
        image::ImageFormat::Pnm | image::ImageFormat::Jpeg => {
            image::DynamicImage::ImageRgb8(source.to_rgb8())
        }
        _ => source,
    };
    let mut out = Vec::new();
    source
        .write_to(&mut std::io::Cursor::new(&mut out), format)
        .unwrap();
    out
}

fn time(rounds: u32, media_type: &str, bytes: &[u8]) -> (f64, usize) {
    // One untimed round first, so the measurement is not paying for whatever the
    // allocator does the first time.
    let first = Media::attach(media_type, bytes).expect("the door accepts it");
    let started = Instant::now();
    for _ in 0..rounds {
        let _ = Media::attach(media_type, bytes).unwrap();
    }
    (
        started.elapsed().as_secs_f64() * 1000.0 / f64::from(rounds),
        first.byte_len(),
    )
}

fn main() {
    const SIDE: u32 = 512;
    const ROUNDS: u32 = 20;

    println!("A {SIDE}×{SIDE} image, {ROUNDS} rounds, mean per call.\n");
    println!("| Source | In (bytes) | Out (bytes) | Path | ms |");
    println!("| --- | --- | --- | --- | --- |");

    for (media_type, format, path) in [
        ("image/png", image::ImageFormat::Png, "pass-through"),
        ("image/jpeg", image::ImageFormat::Jpeg, "pass-through"),
        ("image/bmp", image::ImageFormat::Bmp, "decode → PNG"),
        ("image/tiff", image::ImageFormat::Tiff, "decode → PNG"),
        ("image/x-tga", image::ImageFormat::Tga, "decode → PNG"),
        (
            "image/x-portable-anymap",
            image::ImageFormat::Pnm,
            "decode → PNG",
        ),
    ] {
        let bytes = fixture(format, SIDE);
        let (ms, out) = time(ROUNDS, media_type, &bytes);
        println!(
            "| `{media_type}` | {} | {out} | {path} | {ms:.2} |",
            bytes.len()
        );
    }

    println!(
        "\nA pass-through is a base64 encode and nothing else, which is what the \
         two rows at the top are for: they are the floor every row is measured against."
    );
}
