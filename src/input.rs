use std::fs;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;

use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageReader};

use crate::meta;

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

fn decode_bytes(buf: &[u8]) -> Result<(DynamicImage, ColorType), Box<dyn std::error::Error>> {
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
    let db = opt.fontdb_mut();
    for dir in core_font_dirs() {
        db.load_fonts_dir(dir);
    }
    opt
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
    let mut dirs = vec![
        "/usr/share/fonts".into(),
        "/usr/local/share/fonts".into(),
    ];
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

pub fn load(source: &Source) -> Result<Loaded, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let (img, _) = decode_bytes(&buf)?;
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
        ColorType::La8 | ColorType::La16 | ColorType::Rgba8 | ColorType::Rgba16 | ColorType::Rgba32F
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
}
