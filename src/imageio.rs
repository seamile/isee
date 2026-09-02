//! macOS HEIC/HEIF decoding through the system ImageIO + CoreGraphics
//! frameworks. No `libheif`, nothing dynamic to ship: the decoder lives in
//! the OS (10.13+), which keeps the release binary self-contained.
//!
//! Only the container's primary still image is decoded — HEIF sequences,
//! Live Photo auxiliary images, depth maps and animations are out of scope;
//! the static path always reports the primary frame.

use std::ffi::c_void;

use objc2_core_foundation::{
    CFBoolean, CFData, CFDictionary, CFNumber, CFNumberType, CFString, CFType, CGPoint, CGRect,
    CGSize,
};
use objc2_core_graphics::{CGBitmapContextCreate, CGColorSpace, CGContext, CGImage};
use objc2_image_io::{
    CGImageSource, kCGImagePropertyDPIWidth, kCGImagePropertyHasAlpha, kCGImagePropertyOrientation,
    kCGImagePropertyPixelHeight, kCGImagePropertyPixelWidth,
    kCGImageSourceCreateThumbnailFromImageAlways, kCGImageSourceCreateThumbnailWithTransform,
    kCGImageSourceThumbnailMaxPixelSize,
};

use image::metadata::Orientation;
use image::{ColorType, DynamicImage};

/// HEIC brands: ISOBMFF containers whose `ftyp` major/compatible brand set
/// intersects this list route to ImageIO. `mif1`/`msf1` are the generic HEIF
/// brands but are shared with AVIF, so a major-brand `avif` container is
/// rejected first — only `heic`/`heix`/`hevc`-family majors (or a HEIC major
/// with the generic brands merely listed as compatible) enter this path.
const HEIC_BRANDS: &[&[u8; 4]] = &[
    b"heic", b"heix", b"hevc", b"hevx", b"heim", b"heis", b"hevm", b"hevs", b"mif1", b"msf1",
];

/// Detect a HEIC/HEIF container by parsing the ISOBMFF `ftyp` box: the major
/// brand at offset 8 plus the compatible-brand list right after. Byte-matching
/// `ftyp` alone would also catch AVIF and MP4, which must not enter this path.
pub fn is_heif(buf: &[u8]) -> bool {
    // box: 4-byte big-endian size, 4-byte type ("ftyp"), 4-byte major brand,
    // 4-byte minor version, then the compatible-brand list.
    if buf.len() < 16 || &buf[4..8] != b"ftyp" {
        return false;
    }
    let major: &[u8] = &buf[8..12];
    if major == b"avif" || major == b"avis" {
        return false; // AVIF must stay an unsupported format here
    }
    if HEIC_BRANDS.iter().any(|b| major == *b) {
        return true;
    }
    // Compatible brands start at offset 16 and run in 4-byte steps.
    buf[16..]
        .as_chunks::<4>()
        .0
        .iter()
        .any(|c| HEIC_BRANDS.contains(&c))
}

fn err<T>(msg: impl Into<String>) -> Result<T, Box<dyn std::error::Error>> {
    Err(msg.into().into())
}

/// Read an i64-valued property out of the ImageIO properties dictionary.
/// ImageIO stores all of width/height/orientation/has-alpha as CFNumbers, and
/// a Float64 read is lossless for every integral value ImageIO emits.
///
/// # Safety
///
/// `key` must be one of the `kCGImageProperty*` extern statics defined by
/// ImageIO (accessing them is itself an unsafe operation).
fn prop_number(props: &CFDictionary, key: &CFString) -> Option<f64> {
    let raw: *const c_void = unsafe { props.value(key as *const CFString as *const c_void) };
    let number = unsafe { raw.cast::<CFNumber>().as_ref()? };
    let mut out: f64 = 0.0;
    unsafe {
        number
            .value(CFNumberType::Float64Type, (&mut out as *mut f64).cast())
            .then_some(out)
    }
}

/// Snapshot of the ImageIO property keys, captured once inside `unsafe` so
/// the rest of the module can read them safely.
fn prop_keys() -> PropKeys {
    unsafe {
        PropKeys {
            pixel_width: kCGImagePropertyPixelWidth,
            pixel_height: kCGImagePropertyPixelHeight,
            orientation: kCGImagePropertyOrientation,
            has_alpha: kCGImagePropertyHasAlpha,
            dpi_width: kCGImagePropertyDPIWidth,
        }
    }
}

struct PropKeys {
    pixel_width: &'static CFString,
    pixel_height: &'static CFString,
    orientation: &'static CFString,
    has_alpha: &'static CFString,
    dpi_width: &'static CFString,
}

struct HeifProps {
    width: u32,
    height: u32,
    orientation: Orientation,
    has_alpha: bool,
    dpi: Option<f64>,
}

/// Read the primary image's properties without decoding pixels. Returns the
/// ORIENTED (display) dimensions — orientation codes 5–8 swap width/height,
/// matching what a viewer shows.
fn heif_properties(buf: &[u8]) -> Result<HeifProps, Box<dyn std::error::Error>> {
    let data = CFData::from_bytes(buf);
    let source = unsafe { CGImageSource::with_data(&data, None) }
        .ok_or("ImageIO rejected this HEIF container")?;
    let index = unsafe { source.primary_image_index() };
    let props = unsafe { source.properties_at_index(index, None) }
        .ok_or("ImageIO returned no properties for this HEIF container")?;
    let keys = prop_keys();

    let raw_w = prop_number(&props, keys.pixel_width).unwrap_or(0.0);
    let raw_h = prop_number(&props, keys.pixel_height).unwrap_or(0.0);
    if raw_w < 1.0 || raw_h < 1.0 {
        return err("HEIF container reports no usable pixel dimensions");
    }
    let orientation = heif_orientation(prop_number(&props, keys.orientation).unwrap_or(1.0) as i64);
    let has_alpha = prop_number(&props, keys.has_alpha).unwrap_or(0.0) != 0.0;
    let dpi = prop_number(&props, keys.dpi_width).filter(|d| *d > 0.0 && *d <= f64::from(u16::MAX));

    let width = u32::try_from(raw_w as i64).map_err(|_| "HEIF dimensions exceed u32")?;
    let height = u32::try_from(raw_h as i64).map_err(|_| "HEIF dimensions exceed u32")?;
    let (width, height) = oriented_dims(width, height, orientation);
    Ok(HeifProps {
        width,
        height,
        orientation,
        has_alpha,
        dpi,
    })
}

/// `-i` info without a pixel decode: properties only.
pub fn load_info(buf: &[u8]) -> Result<HeifInfo, Box<dyn std::error::Error>> {
    let p = heif_properties(buf)?;
    Ok(HeifInfo {
        width: p.width,
        height: p.height,
        dpi: p.dpi,
        alpha: p.has_alpha,
        color: if p.has_alpha {
            ColorType::Rgba8
        } else {
            ColorType::Rgb8
        },
    })
}

pub struct HeifInfo {
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub alpha: bool,
    pub color: ColorType,
}

/// Full decode of the primary image into a straight-alpha RGBA8 / RGB8 image
/// with orientation applied. `check` receives the oriented dimensions before
/// any pixel buffer is allocated (the 12000 px / 384 MiB preview guards).
pub fn decode(
    buf: &[u8],
    check: impl Fn(u32, u32) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let p = heif_properties(buf)?;
    check(p.width, p.height)?;
    let (source, index) = image_source(buf)?;
    let cg_image = unsafe { source.image_at_index(index, None) }
        .ok_or("ImageIO could not decode this HEIF image")?;
    cg_image_to_dynamic(&cg_image, p.has_alpha, p.orientation, &check)
}

/// Decode a preview through ImageIO's native thumbnail path. The system
/// decoder applies orientation and performs HEVC downsampling before the
/// bitmap exists, avoiding a full 100–200 MiB raster for phone/camera images.
pub fn decode_thumbnail(
    buf: &[u8],
    max_pixel_size: u32,
    check: impl Fn(u32, u32) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let p = heif_properties(buf)?;
    check(p.width, p.height)?;
    let (source, index) = image_source(buf)?;
    let max_size = CFNumber::new_i64(i64::from(max_pixel_size.max(1)));
    let always = CFBoolean::new(true);
    let options = unsafe {
        let keys: [&CFType; 3] = [
            kCGImageSourceCreateThumbnailFromImageAlways.as_ref(),
            kCGImageSourceThumbnailMaxPixelSize.as_ref(),
            kCGImageSourceCreateThumbnailWithTransform.as_ref(),
        ];
        let values: [&CFType; 3] = [always.as_ref(), max_size.as_ref(), always.as_ref()];
        CFDictionary::<CFType, CFType>::from_slices(&keys, &values)
    };
    let options: &CFDictionary = options.as_ref();
    let cg_image = unsafe { source.thumbnail_at_index(index, Some(options)) }
        .ok_or("ImageIO could not decode a HEIF thumbnail")?;
    // CreateThumbnailWithTransform has already applied orientation.
    cg_image_to_dynamic(&cg_image, p.has_alpha, Orientation::NoTransforms, &check)
}

fn image_source(
    buf: &[u8],
) -> Result<(objc2_core_foundation::CFRetained<CGImageSource>, usize), Box<dyn std::error::Error>> {
    let data = CFData::from_bytes(buf);
    let source = unsafe { CGImageSource::with_data(&data, None) }
        .ok_or("ImageIO rejected this HEIF container")?;
    let index = unsafe { source.primary_image_index() };
    Ok((source, index))
}

fn cg_image_to_dynamic(
    cg_image: &CGImage,
    has_alpha: bool,
    orientation: Orientation,
    check: &impl Fn(u32, u32) -> Result<(), Box<dyn std::error::Error>>,
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let raw_w = u32::try_from(CGImage::width(Some(cg_image)))
        .map_err(|_| "decoded HEIF width exceeds u32")?;
    let raw_h = u32::try_from(CGImage::height(Some(cg_image)))
        .map_err(|_| "decoded HEIF height exceeds u32")?;
    let (display_w, display_h) = oriented_dims(raw_w, raw_h, orientation);
    check(display_w, display_h)?;

    // The bitmap must use the decoded image's RAW grid. Applying orientation
    // before drawing would stretch 90/270-degree images into swapped bounds;
    // rotate the DynamicImage only after CoreGraphics has rendered it.
    let bitmap_info = 1u32; // kCGImageAlphaPremultipliedLast
    let space = CGColorSpace::new_device_rgb().ok_or("no DeviceRGB color space")?;
    let mut pixels = vec![0u8; raw_w as usize * raw_h as usize * 4];
    let context = unsafe {
        CGBitmapContextCreate(
            pixels.as_mut_ptr().cast(),
            raw_w as usize,
            raw_h as usize,
            8,
            raw_w as usize * 4,
            Some(&space),
            bitmap_info,
        )
    }
    .ok_or("could not create the CoreGraphics bitmap context")?;
    let rect = CGRect::new(
        CGPoint::ZERO,
        CGSize::new(f64::from(raw_w), f64::from(raw_h)),
    );
    CGContext::draw_image(Some(&context), rect, Some(cg_image));

    let mut img = if has_alpha {
        demultiply(&mut pixels);
        let img =
            image::RgbaImage::from_raw(raw_w, raw_h, pixels).ok_or("HEIF buffer size mismatch")?;
        DynamicImage::ImageRgba8(img)
    } else {
        let mut rgb = Vec::with_capacity(raw_w as usize * raw_h as usize * 3);
        for px in pixels.as_chunks::<4>().0 {
            rgb.extend_from_slice(&px[..3]);
        }
        let img =
            image::RgbImage::from_raw(raw_w, raw_h, rgb).ok_or("HEIF buffer size mismatch")?;
        DynamicImage::ImageRgb8(img)
    };
    img.apply_orientation(orientation);
    Ok(img)
}

/// In-place premultiplied → straight alpha with rounding.
fn demultiply(rgba: &mut [u8]) {
    for px in rgba.as_chunks_mut::<4>().0 {
        let a = u32::from(px[3]);
        if a == 255 {
            continue;
        }
        if a == 0 {
            px[..3].fill(0);
            continue;
        }
        for c in &mut px[..3] {
            *c = ((u32::from(*c) * 255 + a / 2) / a).min(255) as u8;
        }
    }
}

fn oriented_dims(w: u32, h: u32, o: Orientation) -> (u32, u32) {
    match o {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (h, w),
        _ => (w, h),
    }
}

/// Map ImageIO's `kCGImagePropertyOrientation` code (1–8) onto `image`'s
/// orientation enum, which `apply_orientation` expects. EXIF 2 is a
/// horizontal mirror and 4 a vertical one; 5–8 are the mirrored rotations.
fn heif_orientation(code: i64) -> Orientation {
    match code {
        2 => Orientation::FlipHorizontal,
        3 => Orientation::Rotate180,
        4 => Orientation::FlipVertical,
        5 => Orientation::Rotate90FlipH,
        6 => Orientation::Rotate90,
        7 => Orientation::Rotate270FlipH,
        8 => Orientation::Rotate270,
        _ => Orientation::NoTransforms,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixture built by `sips` from a 32x24 PNG: top half opaque red, bottom
    /// half half-transparent blue. It pins brand detection, dimension reads,
    /// alpha handling and the demultiply conversion end to end.
    const FIXTURE: &[u8] = include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/small.heic"
    ));

    #[test]
    fn fixture_routes_to_heif() {
        assert!(is_heif(FIXTURE));
    }

    #[test]
    fn fixture_info_reads_properties() {
        let info = load_info(FIXTURE).expect("info decode");
        assert_eq!((info.width, info.height), (32, 24));
        assert!(info.alpha);
        assert_eq!(info.color, ColorType::Rgba8);
    }

    #[test]
    fn fixture_decodes_with_straight_alpha() {
        let img = decode(FIXTURE, |_, _| Ok(())).expect("full decode");
        assert_eq!((img.width(), img.height()), (32, 24));
        // The top half is opaque red; the bottom half is 50% blue, which only
        // reads as plain (0, 0, 255, 128) after demultiplication — a
        // premultiplied (0, 0, 128, 128) would be too dark.
        let rgba = img.to_rgba8();
        let top = rgba.get_pixel(16, 4);
        assert_eq!((top[0], top[1], top[2], top[3]), (255, 0, 0, 255));
        let bottom = rgba.get_pixel(16, 20);
        assert_eq!(
            (bottom[0], bottom[1], bottom[2], bottom[3]),
            (0, 0, 255, 128)
        );
    }

    #[test]
    fn fixture_thumbnail_preserves_aspect_and_alpha() {
        let img = decode_thumbnail(FIXTURE, 16, |_, _| Ok(())).expect("thumbnail decode");
        assert_eq!((img.width(), img.height()), (16, 12));
        assert_eq!(img.color(), ColorType::Rgba8);
        let rgba = img.to_rgba8();
        let bottom = rgba.get_pixel(8, 10);
        assert_eq!(bottom[3], 128);
        assert!(bottom[2] >= 250, "blue channel was {}", bottom[2]);
    }

    #[test]
    fn decode_respects_size_guard() {
        let err = decode(FIXTURE, |_, _| Err("size limit".into())).unwrap_err();
        assert!(err.to_string().contains("size limit"));
    }

    #[test]
    fn brand_detection_rejects_non_heif() {
        // AVIF is an ISOBMFF container too — it must NOT route to HEIC.
        assert!(!is_heif(b"\0\0\0\x20ftypavif\0\0\0\0avifmif1"));
        assert!(!is_heif(b"\0\0\0\x18ftypisom\0\0\0\0isommp42"));
        assert!(!is_heif(b"RIFF\0\0\0\0WEBP"));
        assert!(!is_heif(b""));
        assert!(!is_heif(b"\0\0\0\x10ftyp"));
    }

    #[test]
    fn brand_detection_accepts_heic_brands() {
        // Major brand heic, minor 0, no compatible list.
        assert!(is_heif(b"\0\0\0\x10ftypheic\0\0\0\0"));
        // Major brand mif1 (generic HEIF) with heic in the compatible list —
        // a common iPhone layout.
        assert!(is_heif(b"\0\0\0\x18ftypmif1\0\0\0\0heicmif1"));
        // An AVIF major brand must be rejected even when a HEIC brand is
        // merely listed as compatible.
        assert!(!is_heif(b"\0\0\0\x14ftypavif\0\0\0\0heic"));
    }
}
