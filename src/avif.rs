//! AVIF still-image decoding, entirely in Rust: `avif-parse` demuxes the
//! ISOBMFF container and the AV1 sequence header, and `avif-decode` drives
//! rav1d (a Rust port of dav1d) for the pixels. Nothing dynamic to ship and
//! no `libdav1d` to find — the same self-contained stance as the macOS
//! ImageIO path in `imageio`.
//!
//! Only the primary still image is decoded. `avis` image sequences, `grid`
//! collages, HDR (PQ/HLG) and embedded ICC profiles are reported as
//! unsupported rather than silently mis-rendered, and the container's
//! `irot`/`imir` transforms are not parsed at all, so a rotated AVIF shows
//! unrotated — the same gap `image`'s dav1d decoder has.

use std::io::Cursor;

use image::{ColorType, DynamicImage, ImageBuffer};

/// Shape of an AVIF container, decided by the `ftyp` major brand.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `avif`: AV1 image items — the still-image path.
    Still,
    /// `avis`: AV1 image sequences, i.e. animated AVIF, which is not supported.
    Sequence,
}

/// The `ftyp` major brand: 4-byte big-endian box size, `ftyp`, 4-byte major
/// brand, 4-byte minor version, then the compatible-brand list.
fn major_brand(buf: &[u8]) -> Option<[u8; 4]> {
    if buf.len() < 16 || &buf[4..8] != b"ftyp" {
        return None;
    }
    buf[8..12].try_into().ok()
}

/// Classify an AVIF file by its `ftyp` major brand; `None` for every other
/// container (HEIC and MP4 are ISOBMFF too, and both belong to other paths).
///
/// Only the major brand counts. `avif-parse` accepts nothing else, so a
/// `mif1`-branded file that merely lists `avif` as a compatible brand is not
/// claimed here: it keeps its existing route (macOS ImageIO, which decodes
/// AVIF from 13 on, and the generic unsupported-format error elsewhere)
/// instead of being promised a decode this module would then refuse.
pub fn kind(buf: &[u8]) -> Option<Kind> {
    match &major_brand(buf)? {
        b"avif" => Some(Kind::Still),
        b"avis" => Some(Kind::Sequence),
        _ => None,
    }
}

/// True for an AVIF file of either shape, so the routing in `input` can hand
/// the file — and any clear "not supported" verdict — to this module.
pub fn is_avif(buf: &[u8]) -> bool {
    kind(buf).is_some()
}

/// `avif-parse` failures, with every unsupported corner (grid collages above
/// all) named as such; the rest speak for themselves.
fn avif_err(err: avif_parse::Error) -> Box<dyn std::error::Error> {
    match err {
        avif_parse::Error::Unsupported(msg) => format!("unsupported AVIF: {msg}").into(),
        other => other.to_string().into(),
    }
}

/// `avif-decode` failures; its embedded `avif-parse` errors keep their wording.
fn decode_err(err: avif_decode::Error) -> Box<dyn std::error::Error> {
    match err {
        avif_decode::Error::Avif(err) => avif_err(err),
        avif_decode::Error::Unsupported(msg) => format!("unsupported AVIF: {msg}").into(),
        other => other.to_string().into(),
    }
}

/// Parse the container without touching AV1 pixels.
fn parse(buf: &[u8]) -> Result<avif_parse::AvifData, Box<dyn std::error::Error>> {
    match kind(buf) {
        Some(Kind::Still) => avif_parse::read_avif(&mut Cursor::new(buf)).map_err(avif_err),
        Some(Kind::Sequence) => Err("animated AVIF (image sequence) is not supported".into()),
        None => Err("not an AVIF file".into()),
    }
}

/// What `-i` reports: container and sequence-header metadata, no pixel decode.
#[derive(Debug)]
pub struct AvifInfo {
    pub width: u32,
    pub height: u32,
    pub alpha: bool,
    pub color: ColorType,
}

pub fn load_info(buf: &[u8]) -> Result<AvifInfo, Box<dyn std::error::Error>> {
    let data = parse(buf)?;
    let meta = data.primary_item_metadata().map_err(avif_err)?;
    let alpha = data.alpha_item.is_some();
    let wide = meta.bit_depth > 8;
    // Mirror what `to_dynamic` produces: a monochrome image with alpha comes
    // out as RGBA (avif-decode has no grey+alpha variant), so `-i` must not
    // claim `L8`/`L16` there.
    let color = match (meta.monochrome, alpha, wide) {
        (_, true, false) => ColorType::Rgba8,
        (_, true, true) => ColorType::Rgba16,
        (true, false, false) => ColorType::L8,
        (true, false, true) => ColorType::L16,
        (false, false, false) => ColorType::Rgb8,
        (false, false, true) => ColorType::Rgb16,
    };
    Ok(AvifInfo {
        width: meta.max_frame_width.get(),
        height: meta.max_frame_height.get(),
        alpha,
        color,
    })
}

/// Decode the primary still image.
///
/// `check` receives the AV1 sequence header's dimensions and the estimated
/// decoded byte count (4 bytes per pixel at 8-bit, 8 at 10/12-bit) *before*
/// any frame buffer is allocated, so `input`'s regular preview guards cover
/// AVIF bombs too. AV1 has no scaled-decode mode, so the frame always decodes
/// at full resolution and the caller shrinks it afterwards.
pub fn decode(
    buf: &[u8],
    check: impl Fn(u32, u32, u64) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let meta = parse(buf)?.primary_item_metadata().map_err(avif_err)?;
    let (w, h) = (meta.max_frame_width.get(), meta.max_frame_height.get());
    let per_pixel = if meta.bit_depth > 8 { 8 } else { 4 };
    check(w, h, u64::from(w) * u64::from(h) * per_pixel)?;
    let decoded = avif_decode::Decoder::from_avif(buf)
        .map_err(decode_err)?
        .to_image()
        .map_err(decode_err)?;
    to_dynamic(decoded)
}

/// Rebuild avif-decode's row-major pixel buffers as a `DynamicImage`, keeping
/// the depth and the mono/RGB/RGBA mode the decoder chose. The pixels are
/// `rgb`'s structs (named fields; `Gray` behind `value()`), and `image` keeps
/// its 16-bit buffer aliases private, hence the generic `ImageBuffer::from_raw`.
fn to_dynamic(img: avif_decode::Image) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    fn buffer<P: image::Pixel>(
        pixels: Vec<P::Subpixel>,
        (w, h): (usize, usize),
    ) -> Result<ImageBuffer<P, Vec<P::Subpixel>>, &'static str> {
        ImageBuffer::from_raw(w as u32, h as u32, pixels).ok_or("AVIF buffer size mismatch")
    }

    Ok(match img {
        avif_decode::Image::Rgb8(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            DynamicImage::ImageRgb8(buffer(px, (w, h))?)
        }
        avif_decode::Image::Rgb16(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().flat_map(|p| [p.r, p.g, p.b]).collect();
            DynamicImage::ImageRgb16(buffer(px, (w, h))?)
        }
        avif_decode::Image::Rgba8(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            DynamicImage::ImageRgba8(buffer(px, (w, h))?)
        }
        avif_decode::Image::Rgba16(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().flat_map(|p| [p.r, p.g, p.b, p.a]).collect();
            DynamicImage::ImageRgba16(buffer(px, (w, h))?)
        }
        avif_decode::Image::Gray8(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().map(|p| p.value()).collect();
            DynamicImage::ImageLuma8(buffer(px, (w, h))?)
        }
        avif_decode::Image::Gray16(v) => {
            let (px, w, h) = v.into_contiguous_buf();
            let px = px.into_iter().map(|p| p.value()).collect();
            DynamicImage::ImageLuma16(buffer(px, (w, h))?)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures built with ImageMagick (6.9.12 + libheif 1.17.6) from a 32x24
    /// transparent canvas: the top half is opaque red, the bottom half 50%
    /// blue. `small` is 8-bit 4:4:4, `small-hbd` keeps libheif's default
    /// 12-bit output, and `small-opaque` is a flat 8-bit 4:2:0 colour with no
    /// alpha, so the three cover the RGB8/RGBA8/RGBA16 arms of `to_dynamic`.
    ///
    ///   convert -size 32x24 xc:none -fill red -draw 'rectangle 0,0 31,11' \
    ///     -fill 'rgba(0,0,255,0.5)' -draw 'rectangle 0,12 31,23' \
    ///     -depth 8 -define heic:chroma=444 -quality 100 AVIF:small.avif
    ///   convert -size 32x24 xc:none ... (the same, without `-depth 8`) \
    ///     -define heic:chroma=444 -quality 100 AVIF:small-hbd.avif
    ///   convert -size 32x24 xc:'rgb(10,200,30)' -depth 8 -quality 100 \
    ///     AVIF:small-opaque.avif
    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/small.avif"
    ));
    const HBD_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/small-hbd.avif"
    ));
    const OPAQUE_FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/small-opaque.avif"
    ));

    #[test]
    fn brand_detection_classifies_avif_only() {
        assert_eq!(kind(FIXTURE), Some(Kind::Still));
        assert!(is_avif(FIXTURE));
        // `avis` sequences are claimed so the failure can name the format.
        assert_eq!(
            kind(b"\0\0\0\x1cftypavis\0\0\0\0avifmsf1"),
            Some(Kind::Sequence)
        );
        // Another ISOBMFF container, a non-ISOBMFF one, and junk.
        assert_eq!(kind(b"\0\0\0\x18ftypheic\0\0\0\0mif1heic"), None);
        assert_eq!(kind(b"\0\0\0\x18ftypisom\0\0\0\0isommp42"), None);
        assert_eq!(kind(b"RIFF\0\0\0\0WEBP"), None);
        assert_eq!(kind(b"\0\0\0\x10ftyp"), None);
        assert_eq!(kind(b""), None);
        // A `mif1` major brand with an `avif` compatible brand is not claimed.
        assert_eq!(kind(b"\0\0\0\x14ftypmif1\0\0\0\0avifmiaf"), None);
    }

    #[test]
    fn info_reads_metadata_without_decoding() {
        let info = load_info(FIXTURE).expect("info");
        assert_eq!((info.width, info.height), (32, 24));
        assert!(info.alpha);
        assert_eq!(info.color, ColorType::Rgba8);
    }

    #[test]
    fn fixture_decodes_with_straight_alpha() {
        let img = decode(FIXTURE, |_, _, _| Ok(())).expect("decode");
        assert_eq!((img.width(), img.height()), (32, 24));
        assert_eq!(img.color(), ColorType::Rgba8);
        let rgba = img.to_rgba8();
        // The top half is opaque red. AV1 is lossy, so the flat areas are
        // compared with a small tolerance; the alpha plane is exact enough to
        // catch a premultiplied/unpremultiplied mix-up (128 vs 64).
        let top = rgba.get_pixel(16, 4);
        assert!(top[0] >= 250 && top[1] <= 5 && top[2] <= 5, "top {top:?}");
        assert_eq!(top[3], 255);
        let bottom = rgba.get_pixel(16, 20);
        assert!(
            bottom[2] >= 250 && bottom[0] <= 5 && bottom[1] <= 5,
            "bottom {bottom:?}"
        );
        assert!(
            (bottom[3] as i32 - 128).abs() <= 2,
            "alpha {} should be ~128 (straight alpha, not premultiplied)",
            bottom[3]
        );
    }

    #[test]
    fn high_bit_depth_stays_16_bit_with_an_8_byte_guard() {
        let info = load_info(HBD_FIXTURE).expect("info");
        assert_eq!(info.color, ColorType::Rgba16);
        assert!(info.alpha);
        // The pre-decode guard must budget eight bytes per pixel here, not four.
        let seen = std::cell::Cell::new(None);
        let err = decode(HBD_FIXTURE, |w, h, bytes| {
            seen.set(Some((w, h, bytes)));
            Err("size limit".into())
        })
        .unwrap_err();
        assert!(err.to_string().contains("size limit"));
        assert_eq!(seen.get(), Some((32, 24, 32 * 24 * 8)));
        let img = decode(HBD_FIXTURE, |_, _, _| Ok(())).expect("decode");
        assert_eq!(img.color(), ColorType::Rgba16);
        // 12-bit planes come out widened to the full 16-bit range.
        let rgba = img.to_rgba16();
        let top = rgba.get_pixel(16, 4);
        assert!(
            top[0] > 63_000 && top[1] < 1_000 && top[2] < 1_000,
            "top {top:?}"
        );
        assert_eq!(top[3], u16::MAX);
    }

    #[test]
    fn opaque_image_has_no_alpha_and_decodes_as_rgb() {
        let info = load_info(OPAQUE_FIXTURE).expect("info");
        assert!(!info.alpha);
        assert_eq!(info.color, ColorType::Rgb8);
        let img = decode(OPAQUE_FIXTURE, |_, _, _| Ok(())).expect("decode");
        assert_eq!(img.color(), ColorType::Rgb8);
        let rgb = img.to_rgb8();
        let px = rgb.get_pixel(16, 12);
        assert!(
            px[0].abs_diff(10) <= 2 && px[1].abs_diff(200) <= 2 && px[2].abs_diff(30) <= 2,
            "px {px:?}"
        );
    }

    #[test]
    fn dimensions_are_checked_before_allocating() {
        let seen = std::cell::Cell::new(None);
        let err = decode(FIXTURE, |w, h, bytes| {
            seen.set(Some((w, h, bytes)));
            Err("size limit".into())
        })
        .unwrap_err();
        assert!(err.to_string().contains("size limit"));
        assert_eq!(seen.get(), Some((32, 24, 32 * 24 * 4)));
    }

    #[test]
    fn sequences_are_rejected_with_a_clear_error() {
        let sequence = b"\0\0\0\x1cftypavis\0\0\0\0avifmsf1";
        let info_err = load_info(sequence).unwrap_err();
        assert!(info_err.to_string().contains("animated"), "{info_err}");
        let decode_err = decode(sequence, |_, _, _| Ok(())).unwrap_err();
        assert!(decode_err.to_string().contains("animated"), "{decode_err}");
    }

    #[test]
    fn non_avif_input_is_not_claimed() {
        assert!(load_info(b"\x89PNG\r\n\x1a\n").is_err());
        assert!(decode(b"not an image", |_, _, _| Ok(())).is_err());
    }
}
