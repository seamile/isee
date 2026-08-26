use std::io::Cursor;

use image::{DynamicImage, ImageFormat, ImageReader, Rgb, Rgba, RgbaImage};

const W: u32 = 4;
const H: u32 = 3;

/// New formats enabled in Cargo.toml whose magic bytes are discoverable by
/// `with_guessed_format()` (the same decode entry point production uses).
/// TGA and AVIF are absent: image 0.25's content sniffing cannot detect TGA, and
/// the stock `avif` feature ships only the ravif *encoder* (its pure-Rust decoder
/// does not exist; `avif-native`/libdav1d is out of scope).
const GUESSABLE: &[ImageFormat] = &[
    ImageFormat::Bmp,
    ImageFormat::Pnm,
    ImageFormat::Qoi,
    ImageFormat::Farbfeld,
    ImageFormat::Ico,
    ImageFormat::Tiff,
    ImageFormat::Hdr,
    ImageFormat::OpenExr,
];

fn source_image() -> RgbaImage {
    RgbaImage::from_fn(W, H, |x, y| {
        let r = (x * 255 / (W - 1)) as u8;
        let g = (y * 255 / (H - 1)) as u8;
        Rgba([r, g, 255 - r, 255])
    })
}

fn source_rgb32f() -> DynamicImage {
    DynamicImage::ImageRgb32F(image::Rgb32FImage::from_fn(W, H, |x, y| {
        let x = x as f32 / (W as f32 - 1.0);
        let y = y as f32 / (H as f32 - 1.0);
        Rgb([x, y, 1.0 - x])
    }))
}

// Pick a color type each encoder accepts: JPEG has no alpha, Farbfeld is RGBA16,
// and HDR/EXR only accept float RGB.
fn image_for(format: ImageFormat) -> DynamicImage {
    match format {
        ImageFormat::Jpeg => DynamicImage::ImageRgba8(source_image()).into_rgb8().into(),
        ImageFormat::Farbfeld => DynamicImage::ImageRgba8(source_image())
            .into_rgba16()
            .into(),
        ImageFormat::Hdr | ImageFormat::OpenExr => source_rgb32f(),
        _ => DynamicImage::ImageRgba8(source_image()),
    }
}

fn encode(format: ImageFormat) -> Vec<u8> {
    let mut cursor = Cursor::new(Vec::new());
    image_for(format)
        .write_to(&mut cursor, format)
        .expect("encode");
    cursor.into_inner()
}

#[test]
fn new_formats_decode_via_content_guess() {
    for &format in GUESSABLE {
        let bytes = encode(format);
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap_or_else(|e| panic!("guess failed for {format:?}: {e}"))
            .decode()
            .unwrap_or_else(|e| panic!("decode failed for {format:?}: {e}"));
        assert_eq!((img.width(), img.height()), (W, H), "dims for {format:?}");
    }
}

#[test]
fn original_formats_still_decode() {
    for &format in &[
        ImageFormat::Png,
        ImageFormat::Jpeg,
        ImageFormat::Gif,
        ImageFormat::WebP,
    ] {
        let bytes = encode(format);
        let img = ImageReader::new(Cursor::new(bytes))
            .with_guessed_format()
            .unwrap_or_else(|e| panic!("guess failed for {format:?}: {e}"))
            .decode()
            .unwrap_or_else(|e| panic!("decode failed for {format:?}: {e}"));
        assert_eq!((img.width(), img.height()), (W, H), "dims for {format:?}");
    }
}
