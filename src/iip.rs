use std::io::Write as _;

use image::codecs::{jpeg::JpegEncoder, png::PngEncoder};
use image::{DynamicImage, ExtendedColorType, GenericImageView, ImageEncoder};

use crate::size::{self, RenderOpts};

const JPEG_QUALITY: u8 = 85;

/// Render one image as an iTerm2 inline-image (OSC 1337) frame followed by
/// the CRLFs that park the cursor below it. Alpha images are sent as PNG,
/// opaque ones as JPEG q85 (mirroring yazi's iip driver); terminals sniff
/// only the PNG/JPEG magic so no other format can be smuggled through.
pub fn render(img: &DynamicImage, o: &RenderOpts) -> Vec<u8> {
    let (tw, th) = size::target_px(img, o, size::bitmap_bounds(o));
    let img = if tw == img.width() && th == img.height() {
        std::borrow::Cow::Borrowed(img)
    } else {
        std::borrow::Cow::Owned(img.resize(tw, th, size::filter(o.quality)))
    };
    let (w, h) = img.dimensions();

    let mut payload = Vec::new();
    if img.color().has_alpha() {
        PngEncoder::new(&mut payload)
            .write_image(&img.to_rgba8(), w, h, ExtendedColorType::Rgba8)
            .expect("png encode");
    } else {
        // Grayscale/floating-point inputs are normalized to RGB8 first so
        // every non-alpha path lands on a JPEG-encodable layout.
        let rgb = img.to_rgb8();
        JpegEncoder::new_with_quality(&mut payload, JPEG_QUALITY)
            .write_image(rgb.as_raw(), w, h, ExtendedColorType::Rgb8)
            .expect("jpeg encode");
    }

    let mut out = Vec::with_capacity(payload.len() * 2 + 128);
    write!(
        out,
        "\x1b]1337;File=inline=1;size={};width={w}px;height={h}px:",
        payload.len()
    )
    .unwrap();
    out.extend_from_slice(crate::b64::base64_encode(&payload).as_bytes());
    // The frame terminator is BEL (0x07), NOT ST.
    out.push(0x07);
    // No `doNotMoveCursor=1`: the terminal moves the cursor below the image
    // on its own and one newline parks the shell prompt on the next line —
    // exactly what imgcat does. yazi needs doNotMoveCursor because a TUI
    // re-draws its grid in place; a CLI preview does not.
    out.extend_from_slice(b"\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{CellPx, WinSize};

    fn opts() -> RenderOpts {
        RenderOpts {
            width: None,
            quality: size::Quality::default(),
            cell: CellPx { w: 9, h: 18 },
            win: WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            dpy_scale: 1,
        }
    }

    /// Extract the `size=` field value from an OSC 1337 header.
    fn declared_size(s: &str) -> usize {
        let start = s.find("size=").unwrap() + "size=".len();
        let end = s[start..].find(';').unwrap() + start;
        s[start..end].parse().unwrap()
    }

    #[test]
    fn alpha_image_encodes_png_frame() {
        let img = DynamicImage::new_rgba8(1, 1);
        let out = render(&img, &opts());
        assert!(out.starts_with(b"\x1b]1337;File=inline=1;size="), "{out:?}");
        let s = String::from_utf8_lossy(&out);
        let b64_start = s.find(':').unwrap() + 1;
        let payload_len = declared_size(&s);
        let hdr_end = s.find('\u{7}').unwrap();
        let b64_payload = &s[b64_start..hdr_end];
        // Header size field matches the real payload length.
        assert_eq!(
            b64_payload.len(),
            payload_len.div_ceil(3) * 4,
            "size field vs payload"
        );
        // First four payload bytes are 0x89 'P' 'N' 'G' -> base64 starts "iVBOR".
        assert!(b64_payload.starts_with("iVBOR"), "{b64_payload}");
        assert_eq!(out[hdr_end], 0x07);
        assert!(out.ends_with(b"\n"));
        assert!(!out.ends_with(b"\r\n"));
        // No doNotMoveCursor: the terminal advances the cursor itself, like
        // it does for imgcat.
        assert!(!s.contains("doNotMoveCursor"));
    }

    #[test]
    fn opaque_image_encodes_jpeg_frame() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::new(1, 1));
        let out = render(&img, &opts());
        let s = String::from_utf8_lossy(&out);
        let b64_start = s.find(':').unwrap() + 1;
        let hdr_end = s.find('\u{7}').unwrap();
        let b64_payload = &s[b64_start..hdr_end];
        // JPEG SOI (\xff\xd8\xff) -> base64 starts "/9j/".
        assert!(b64_payload.starts_with("/9j/"), "{b64_payload}");
    }

    #[test]
    fn trailing_newline_parks_prompt_below_image() {
        // The terminal advances the cursor past the image itself (imgcat
        // semantics); a single newline puts the prompt on the next line.
        let img = DynamicImage::new_rgb8(4, 36);
        let out = render(&img, &opts());
        let s = String::from_utf8_lossy(&out);
        let after = s.split_once('\u{7}').unwrap().1;
        assert_eq!(after, "\n");
    }

    #[test]
    fn explicit_width_upscales_into_header_dims() {
        let mut o = opts();
        o.width = Some(20);
        let img = DynamicImage::new_rgb8(5, 5);
        let out = render(&img, &o);
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains(";width=20px;height=20px:"), "{s}");
    }

    #[test]
    fn blocks_are_self_contained_for_multi_file_output() {
        // Pure function: two renders share no state and each block carries
        // its own terminator + trailing newlines, so concatenation stays
        // unambiguous.
        let a = render(
            &DynamicImage::ImageRgb8(image::RgbImage::new(3, 3)),
            &opts(),
        );
        let b = render(
            &DynamicImage::ImageRgba8(image::RgbaImage::new(3, 3)),
            &opts(),
        );
        assert!(!a.is_empty() && !b.is_empty());
        assert!(a.ends_with(b"\n") && b.ends_with(b"\n"));
        assert!(a.starts_with(b"\x1b]1337;") && b.starts_with(b"\x1b]1337;"));
    }
}
