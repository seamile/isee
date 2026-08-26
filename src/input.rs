use std::fs;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;

use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageReader};
use jpeg_decoder::PixelFormat;

use crate::meta;
use crate::size::{self, RenderOpts};

pub enum Source {
    Path(PathBuf),
    Stdin,
}

fn read_all(source: &Source) -> io::Result<Vec<u8>> {
    match source {
        Source::Path(p) => fs::read(p),
        Source::Stdin => {
            let mut buf = Vec::new();
            io::stdin().lock().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn decode_full(buf: &[u8]) -> Result<(DynamicImage, ColorType), Box<dyn std::error::Error>> {
    if is_svg(buf) {
        let img = decode_svg(buf)?;
        let color = img.color();
        return Ok((img, color));
    }
    let reader = ImageReader::new(Cursor::new(buf)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let color = decoder.color_type();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok((img, color))
}

/// Decode for the interactive preview. SVG is rasterized as-is; JPEGs whose
/// source is much larger than the preview target are downsampled during DCT
/// decoding (`jpeg-decoder`), avoiding a full-resolution raster and the
/// resulting peak memory. Everything else, and any JPEG the DCT path declines,
/// falls through to the regular `image` reader.
fn decode_for_preview(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: (u64, u64),
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    if is_svg(buf) {
        return decode_svg(buf);
    }
    if is_jpeg(buf)
        && let Some(img) = decode_jpeg_scaled(buf, opts, bounds)?
    {
        return Ok(img);
    }
    let (img, _) = decode_full(buf)?;
    Ok(img)
}

fn is_jpeg(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0xFF && buf[1] == 0xD8
}

/// Decode a JPEG with DCT downscaling when it is much larger than the preview
/// target. Returns `None` (so the caller falls back to the regular decoder)
/// for unsupported pixel formats, small / already-fitting images, or any
/// parse/scale/decode failure.
fn decode_jpeg_scaled(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: (u64, u64),
) -> Result<Option<DynamicImage>, Box<dyn std::error::Error>> {
    let mut decoder = jpeg_decoder::Decoder::new(Cursor::new(buf));
    if decoder.read_info().is_err() {
        return Ok(None);
    }
    let Some(info) = decoder.info() else {
        return Ok(None);
    };
    if !matches!(info.pixel_format, PixelFormat::RGB24 | PixelFormat::L8) {
        return Ok(None);
    }
    let raw_w = info.width as u32;
    let raw_h = info.height as u32;
    let orientation = read_orientation(buf);
    let (ow, oh) = oriented_dims(raw_w, raw_h, orientation);
    let (tw, th) = size::target_dims(ow, oh, opts, bounds);
    // Only bother when the preview is meaningfully smaller than the source.
    if tw >= ow && th >= oh {
        return Ok(None);
    }
    // Request is expressed against the RAW (stored) grid, preserving its aspect
    // ratio so jpeg-decoder's `||` scale selection is equivalent to `&&`.
    let (req_w, req_h) = oriented_request(tw, th, orientation);
    let req_w = req_w.clamp(1, u16::MAX as u32) as u16;
    let req_h = req_h.clamp(1, u16::MAX as u32) as u16;
    let (aw, ah) = match decoder.scale(req_w, req_h) {
        Ok(dims) => dims,
        Err(_) => return Ok(None),
    };
    if aw as u32 >= raw_w && ah as u32 >= raw_h {
        // No real DCT reduction (the target is close to the source size);
        // skip so the regular decoder does the exact work.
        return Ok(None);
    }
    let pixels = match decoder.decode() {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };
    Ok(
        match build_scaled_image(pixels, info.pixel_format, aw, ah) {
            Some(mut img) => {
                img.apply_orientation(orientation);
                Some(img)
            }
            None => None,
        },
    )
}

/// Read the EXIF orientation from the JPEG header only (no full decode) so the
/// target can be computed from the displayed dimensions before scaling.
fn read_orientation(buf: &[u8]) -> Orientation {
    let Ok(reader) = ImageReader::new(Cursor::new(buf)).with_guessed_format() else {
        return Orientation::NoTransforms;
    };
    let Ok(mut decoder) = reader.into_decoder() else {
        return Orientation::NoTransforms;
    };
    decoder.orientation().unwrap_or(Orientation::NoTransforms)
}

/// Displayed (oriented) dimensions given the stored raw size and orientation.
fn oriented_dims(w: u32, h: u32, o: Orientation) -> (u32, u32) {
    match o {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (h, w),
        _ => (w, h),
    }
}

/// Express an oriented-space target request back on the raw (stored) grid by
/// swapping width/height for the 90/270 rotations, preserving raw aspect.
fn oriented_request(tw: u32, th: u32, o: Orientation) -> (u32, u32) {
    match o {
        Orientation::Rotate90
        | Orientation::Rotate270
        | Orientation::Rotate90FlipH
        | Orientation::Rotate270FlipH => (th, tw),
        _ => (tw, th),
    }
}

fn build_scaled_image(pixels: Vec<u8>, fmt: PixelFormat, w: u16, h: u16) -> Option<DynamicImage> {
    match fmt {
        PixelFormat::RGB24 => {
            let buf = image::RgbImage::from_raw(w as u32, h as u32, pixels)?;
            Some(DynamicImage::ImageRgb8(buf))
        }
        PixelFormat::L8 => {
            let buf = image::GrayImage::from_raw(w as u32, h as u32, pixels)?;
            Some(DynamicImage::ImageLuma8(buf))
        }
        _ => None,
    }
}

/// Detect SVG (and gzipped `.svgz`) content before handing bytes to `image`,
/// whose `MAGIC_BYTES` have no SVG signature.
fn is_svg(buf: &[u8]) -> bool {
    let mut i = 0;
    if buf.starts_with(b"\xef\xbb\xbf") {
        i = 3;
    }
    let start = buf[i..]
        .iter()
        .position(|&b| !b.is_ascii_whitespace())
        .map(|p| i + p)
        .unwrap_or(buf.len());
    // gzip magic => likely `.svgz`; resvg's `svgz` feature unpacks it.
    if buf.get(start..start + 2) == Some(&b"\x1f\x8b"[..]) {
        return true;
    }
    let head = String::from_utf8_lossy(&buf[start..buf.len().min(start + 64)]);
    let head = head.trim().to_ascii_lowercase();
    head.starts_with("<?xml") || head.starts_with("<svg") || head.starts_with("<!doctype svg")
}

/// `usvg::Options::default()` leaves the font database empty, so resvg finds no
/// matching face and silently drops every `<text>`. Load a curated set of system
/// font directories so SVG text renders instead of vanishing.
fn svg_options() -> resvg::usvg::Options<'static> {
    let mut opt = resvg::usvg::Options::default();
    let fallback = {
        let db = opt.fontdb_mut();
        for dir in core_font_dirs() {
            db.load_fonts_dir(dir);
        }
        // usvg's default `font_family` is "Times New Roman" and fontdb's generic
        // families resolve to MS core fonts, neither of which exist on many Linux
        // distros. A `<text>` with no `font-family` is parsed as a literal
        // `Family::Named` (that default), so if it can't be resolved resvg silently
        // drops every glyph. Substitute a real family present in the loaded set.
        let family = resolve_default_family(db);
        if let Some(name) = &family {
            db.set_sans_serif_family(name.clone());
            db.set_serif_family(name.clone());
        }
        family
    };
    if let Some(name) = fallback {
        opt.font_family = name;
    }
    opt
}

/// Returns `None` when the built-in default family (`"Times New Roman"`) actually
/// resolves to a loaded face, preserving out-of-the-box behavior on systems that
/// ship it. Otherwise picks a real family that does, so an unqualified `<text>`
/// never silently vanishes.
fn resolve_default_family(db: &resvg::usvg::fontdb::Database) -> Option<String> {
    if has_family(db, "Times New Roman") {
        return None;
    }
    // Common Latin sans faces found on headless Linux/CI images.
    const CANDIDATES: &[&str] = &[
        "DejaVu Sans",
        "Noto Sans",
        "Liberation Sans",
        "FreeSans",
        "Arial",
        "Helvetica",
        "Verdana",
        "Tahoma",
    ];
    for name in CANDIDATES {
        if has_family(db, name) {
            return Some((*name).to_string());
        }
    }
    // Last resort: any regular, non-monospace face the database actually loaded.
    db.faces()
        .find(|f| f.style == resvg::usvg::fontdb::Style::Normal && !f.monospaced)
        .and_then(|f| f.families.first().map(|(family, _)| family.clone()))
}

fn has_family(db: &resvg::usvg::fontdb::Database, name: &str) -> bool {
    db.query(&resvg::usvg::fontdb::Query {
        families: &[resvg::usvg::fontdb::Family::Name(name)],
        weight: resvg::usvg::fontdb::Weight::NORMAL,
        stretch: resvg::usvg::fontdb::Stretch::Normal,
        style: resvg::usvg::fontdb::Style::Normal,
    })
    .is_some()
}

/// `fontdb::load_system_fonts()` scans every font the OS knows about — on macOS
/// that includes a huge pool of downloadable fonts (the `AssetsV2` tree), which
/// is the single biggest cost of rendering SVG text. Loading only the core
/// system directories keeps common fonts working while shaving a large chunk off
/// startup, at the price of not covering some less-common faces.
#[cfg(target_os = "macos")]
fn core_font_dirs() -> &'static [&'static str] {
    &[
        "/System/Library/Fonts",
        "/System/Library/Fonts/Supplemental",
        "/Library/Fonts",
    ]
}

/// Linux has no font-flattened system dir like macOS; mirror `fontdb`'s
/// non-fontconfig fallback set (system dirs + the user's `~/.fonts` and
/// `~/.local/share/fonts`) so user-installed fonts still resolve. Missing
/// directories are silently skipped by `load_fonts_dir`.
#[cfg(target_os = "linux")]
fn core_font_dirs() -> Vec<std::path::PathBuf> {
    let mut dirs = vec!["/usr/share/fonts".into(), "/usr/local/share/fonts".into()];
    if let Ok(home) = std::env::var("HOME") {
        let home = std::path::Path::new(&home);
        dirs.push(home.join(".fonts"));
        dirs.push(home.join(".local/share/fonts"));
    }
    dirs
}

fn decode_svg(buf: &[u8]) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let tree = resvg::usvg::Tree::from_data(buf, &svg_options())?;
    let size = tree.size();
    let width = size.width().ceil().max(1.0) as u32;
    let height = size.height().ceil().max(1.0) as u32;
    let mut pixmap = resvg::tiny_skia::Pixmap::new(width, height)
        .ok_or_else(|| "could not allocate an SVG pixmap".to_string())?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::default(),
        &mut pixmap.as_mut(),
    );
    // tiny-skia stores premultiplied RGBA; convert back to the non-premultiplied
    // layout `image` expects, otherwise semi-transparent pixels render too dark.
    let mut img = image::RgbaImage::new(width, height);
    for (px, p) in img.pixels_mut().zip(pixmap.pixels()) {
        let c = p.demultiply();
        *px = image::Rgba([c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Ok(DynamicImage::ImageRgba8(img))
}

pub struct Loaded {
    pub img: DynamicImage,
}

pub fn load(
    source: &Source,
    opts: &RenderOpts,
    bounds: (u64, u64),
) -> Result<Loaded, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let img = decode_for_preview(&buf, opts, bounds)?;
    Ok(Loaded { img })
}

pub struct ImageInfo {
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub alpha: bool,
    pub color: ColorType,
}

pub fn load_info(source: &Source) -> Result<ImageInfo, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    if is_svg(&buf) {
        let tree = resvg::usvg::Tree::from_data(&buf, &svg_options())?;
        let size = tree.size();
        return Ok(ImageInfo {
            size: buf.len() as u64,
            width: size.width().ceil().max(1.0) as u32,
            height: size.height().ceil().max(1.0) as u32,
            dpi: None,
            alpha: true,
            color: ColorType::Rgba8,
        });
    }
    let m = meta::extract(&buf);
    let reader = ImageReader::new(Cursor::new(&buf[..])).with_guessed_format()?;
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    Ok(ImageInfo {
        size: buf.len() as u64,
        width,
        height,
        dpi: m.dpi,
        alpha: has_alpha(color) || m.alpha_hint,
        color,
    })
}

fn has_alpha(ct: ColorType) -> bool {
    matches!(
        ct,
        ColorType::La8
            | ColorType::La16
            | ColorType::Rgba8
            | ColorType::Rgba16
            | ColorType::Rgba32F
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // A full-bleed red rect at 0.5 opacity over a transparent background, so a
    // non-demultiplied pixel would read (128, 0, 0, 128) and look too dark.
    const SVG_HALF_RED: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" width="10" height="8"><rect width="10" height="8" fill="rgb(255,0,0)" fill-opacity="0.5"/></svg>"#;

    #[test]
    fn detects_svg_variants() {
        assert!(is_svg(br#"<?xml version="1.0"?><svg/>"#));
        assert!(is_svg(
            br#"<!DOCTYPE svg PUBLIC "-//W3C//DTD SVG 1.1//EN"><svg/>"#
        ));
        assert!(is_svg(b"<svg width='1' height='1'>"));
        assert!(is_svg(b"\xef\xbb\xbf\n  <svg/>"));
        assert!(is_svg(b"\x1f\x8b\x08\x00"));
        assert!(!is_svg(b"\x89PNG\r\n\x1a\n"));
        assert!(!is_svg(b"not an image"));
        assert!(!is_svg(b""));
    }

    #[test]
    fn svg_decode_demultiplies_alpha() {
        let img = decode_svg(SVG_HALF_RED).unwrap();
        assert_eq!((img.width(), img.height()), (10, 8));
        let bound = img.to_rgba8();
        let px = bound.get_pixel(5, 4);
        assert_eq!(px[3], 128, "expected 0.5 alpha on the rect");
        assert_eq!(px[0], 255, "red must be restored after demultiply");
        assert_eq!(px[1], 0);
        assert_eq!(px[2], 0);
    }

    #[test]
    fn opaque_svg_keeps_full_alpha() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="4" height="4"><rect width="4" height="4" fill="rgb(10,20,30)"/></svg>"#;
        let img = decode_svg(svg).unwrap();
        let bound = img.to_rgba8();
        let px = bound.get_pixel(2, 2);
        assert_eq!(px, &image::Rgba([10, 20, 30, 255]));
    }

    // Regression guard: if `svg_options()` forgets to load the system fonts,
    // resvg silently drops every `<text>` and the image has no lit pixels.
    #[test]
    fn svg_text_renders_system_fonts() {
        let svg = br#"<svg xmlns="http://www.w3.org/2000/svg" width="80" height="60"><rect width="80" height="60" fill="black"/><text x="40" y="32" fill="white">Hello</text></svg>"#;
        let img = decode_svg(svg).unwrap();
        let bound = img.to_rgba8();
        let lit = bound
            .pixels()
            .filter(|p| p[0] != 0 || p[1] != 0 || p[2] != 0)
            .count();
        assert!(lit > 0, "expected some text pixels to be lit, got {lit}");
    }

    // ---- JPEG DCT downscale path ----

    fn opts_width(width: Option<u32>) -> RenderOpts {
        RenderOpts {
            width,
            quality: 50,
            cell: crate::detect::CellPx { w: 9, h: 18 },
            win: crate::detect::WinSize {
                cols: 200,
                rows: 50,
                px: None,
            },
        }
    }

    fn encode_jpeg(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::RgbImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Rgb([(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    fn encode_jpeg_gray(w: u32, h: u32) -> Vec<u8> {
        let mut img = image::GrayImage::new(w, h);
        for (x, y, px) in img.enumerate_pixels_mut() {
            *px = image::Luma([((x + y) % 256) as u8]);
        }
        let mut out = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    /// Inject a minimal EXIF APP1 segment (Orientation tag) right after SOI, so
    /// the `image` JPEG decoder reports the given orientation.
    fn with_exif_orientation(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        assert!(jpeg.starts_with(b"\xff\xd8"), "expected an SOI marker");
        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD offset
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one directory entry
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation tag
        tiff.extend_from_slice(&3u16.to_le_bytes()); // type SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count 1
        tiff.extend_from_slice(&(orientation as u32).to_le_bytes()); // value
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD offset
        let mut app1 = Vec::new();
        app1.extend_from_slice(b"Exif\x00\x00");
        app1.extend_from_slice(&tiff);
        let mut out = jpeg[..2].to_vec();
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&(app1.len() as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    #[test]
    fn jpeg_scale_large_source_small_target() {
        // 1200x600 RGB source, -w 240 (portrait of the 2:1 grid) => target
        // 240x120; the DCT decoder returns the 1/4 output => 300x150, which is
        // below the source and above the target. The render resize then fits.
        let jpeg = encode_jpeg(1200, 600);
        let o = opts_width(Some(240));
        let bounds = (100_000u64, 100_000u64);
        assert_eq!(size::target_dims(1200, 600, &o, bounds), (240, 120));
        let img = decode_jpeg_scaled(&jpeg, &o, bounds)
            .unwrap()
            .expect("scaled");
        assert_eq!((img.width(), img.height()), (300, 150));
        assert!(img.width() >= 240 && img.height() >= 120);
        assert_eq!(img.color(), ColorType::Rgb8);
    }

    #[test]
    fn jpeg_gray_scaled_path() {
        let jpeg = encode_jpeg_gray(1200, 600);
        let o = opts_width(Some(240));
        let bounds = (100_000u64, 100_000u64);
        let img = decode_jpeg_scaled(&jpeg, &o, bounds)
            .unwrap()
            .expect("scaled");
        assert_eq!((img.width(), img.height()), (300, 150));
        assert_eq!(img.color(), ColorType::L8);
    }

    #[test]
    fn jpeg_small_image_falls_back() {
        // A JPEG that already fits its target must not be sent through DCT.
        let jpeg = encode_jpeg(100, 50);
        let o = opts_width(None);
        let bounds = (100_000u64, 100_000u64);
        assert!(decode_jpeg_scaled(&jpeg, &o, bounds).unwrap().is_none());
    }

    #[test]
    fn jpeg_exif_orientation_is_preserved() {
        // Rotate90 (orientation 6) on a 1200x600 raw grid displays as 600x1200,
        // so the target is a portrait 240x480. The raw DCT request is (480,240),
        // yielding 600x300, then rotated to a 300x600 portrait.
        let jpeg = with_exif_orientation(&encode_jpeg(1200, 600), 6);
        let o = opts_width(Some(240));
        let bounds = (100_000u64, 100_000u64);
        assert_eq!(size::target_dims(600, 1200, &o, bounds), (240, 480));
        let img = decode_jpeg_scaled(&jpeg, &o, bounds)
            .unwrap()
            .expect("scaled");
        assert_eq!(
            (img.width(), img.height()),
            (300, 600),
            "orientation must swap dims"
        );
        assert!(img.width() >= 240 && img.height() >= 480);
    }

    #[test]
    fn decode_for_preview_scales_rgb_and_leaves_png_full() {
        let o = opts_width(Some(240));
        let bounds = (100_000u64, 100_000u64);
        // JPEG goes through DCT scaling.
        let jpeg = encode_jpeg(1200, 600);
        let scaled = decode_for_preview(&jpeg, &o, bounds).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (300, 150));
        // PNG never uses DCT scaling: a full decode, exact resize is done later.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(1200, 600))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let full = decode_for_preview(&png, &o, bounds).unwrap();
        assert_eq!((full.width(), full.height()), (1200, 600));
    }
}
