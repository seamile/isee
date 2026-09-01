use std::fs;
use std::io::{self, Cursor, Read};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::time::Duration;

use image::codecs::gif::GifDecoder;
use image::metadata::{LoopCount, Orientation};
use image::{
    AnimationDecoder, ColorType, DynamicImage, GenericImageView, ImageDecoder, ImageReader,
};
use image_webp::WebPDecoder;
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

/// Maximum single edge (width or height) allowed for a regular preview decode.
const PREVIEW_MAX_DIMENSION: u32 = 12_000;
/// Maximum bytes a single preview allocation may reserve. The budget is a
/// decompression-bomb guard, not steady-state usage: decoded pixels are
/// resized down to the terminal window right after. 384 MiB covers ~100 MP
/// RGBA8 (e.g. 12000x8300), which full-decode formats like PNG need in full.
const PREVIEW_MAX_ALLOC: u64 = 384 * 1024 * 1024;
/// Maximum pixel count allowed when rasterizing an SVG preview.
const SVG_MAX_PIXELS: u64 = 16 * 1024 * 1024;
/// Maximum cumulative retained-frame budget for an animation decode. Frames
/// live for the whole preview (a static bitmap is freed after resizing), so
/// the guard is tighter than `PREVIEW_MAX_ALLOC` and counts the resized
/// canvases actually kept. Exceeding it truncates the clip at a frame
/// boundary rather than failing the preview.
const ANIM_MAX_ALLOC: u64 = 192 * 1024 * 1024;
/// Hard cap on retained animation frames, guarding pathological clips whose
/// byte budget stays small (tiny canvases with a huge frame count).
const ANIM_MAX_FRAMES: usize = 4096;
/// Minimum gap advertised to the terminal: the kitty animation protocol
/// ignores a zero gap (substituting its 40 ms default), so a genuinely fast
/// GIF reports 1 ms instead.
const ANIM_MIN_DELAY_MS: u32 = 1;

/// `image` decode limits guarding the interactive preview. The `-i` info path
/// (`load_info`) and the JPEG DCT downscale path are intentionally excluded:
/// the DCT path reads only the header before shrinking, and `-i` must still
/// report the metadata of enormous images.
fn preview_limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(PREVIEW_MAX_DIMENSION);
    limits.max_image_height = Some(PREVIEW_MAX_DIMENSION);
    limits.max_alloc = Some(PREVIEW_MAX_ALLOC);
    limits
}

fn preview_size_error(w: u32, h: u32) -> Box<dyn std::error::Error> {
    format!("image exceeds the preview size limit ({w}x{h}, max {PREVIEW_MAX_DIMENSION}px a side)")
        .into()
}

fn preview_alloc_error(w: u32, h: u32) -> Box<dyn std::error::Error> {
    format!(
        "image exceeds the preview memory limit ({w}x{h} needs more than {PREVIEW_MAX_ALLOC} bytes)"
    )
    .into()
}

/// Reject a regular full decode whose dimensions exceed `PREVIEW_MAX_DIMENSION`.
fn check_preview_size(w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
    if w > PREVIEW_MAX_DIMENSION || h > PREVIEW_MAX_DIMENSION {
        return Err(preview_size_error(w, h));
    }
    Ok(())
}

/// Reject a regular full decode whose decoded buffer would exceed
/// `PREVIEW_MAX_ALLOC`. `image::Limits::max_alloc` is enforced by some decoders
/// but not the top-level output buffer allocation, so this is the reliable cap.
fn check_preview_alloc(w: u32, h: u32, bytes: u64) -> Result<(), Box<dyn std::error::Error>> {
    if bytes > PREVIEW_MAX_ALLOC {
        return Err(preview_alloc_error(w, h));
    }
    Ok(())
}

fn decode_full(buf: &[u8]) -> Result<(DynamicImage, ColorType), Box<dyn std::error::Error>> {
    if is_svg(buf) {
        let img = decode_svg(buf)?;
        let color = img.color();
        return Ok((img, color));
    }
    #[cfg(target_os = "macos")]
    if crate::imageio::is_heif(buf) {
        let img = crate::imageio::decode(buf, |w, h| {
            check_preview_size(w, h)?;
            check_preview_alloc(w, h, u64::from(w) * u64::from(h) * 4)?;
            Ok(())
        })?;
        let color = img.color();
        return Ok((img, color));
    }
    let mut reader = ImageReader::new(Cursor::new(buf)).with_guessed_format()?;
    reader.limits(preview_limits());
    let mut decoder = reader.into_decoder()?;
    let color = decoder.color_type();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let (w, h) = decoder.dimensions();
    check_preview_size(w, h)?;
    check_preview_alloc(w, h, decoder.total_bytes())?;
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok((img, color))
}

/// Decode for the interactive preview. SVG is rasterized as-is; JPEGs whose
/// source is much larger than the preview target are downsampled during DCT
/// decoding (`jpeg-decoder`), avoiding a full-resolution raster and the
/// resulting peak memory. Everything else, and any JPEG the DCT path declines,
/// falls through to the regular `image` reader.
///
/// `dpy_scale` is the bitmap display scale (device px per logical point):
/// when > 1 (Retina Iip/Sixel), every target is computed in point space and
/// the decoded bitmap shrinks to point size so the declared `height=Npx`
/// renders at the image's natural visual size (see `shrink_to_points`).
fn decode_for_preview(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: size::Bounds,
    dpy_scale: u32,
) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    if is_svg(buf) {
        let img = decode_svg(buf)?;
        return Ok(shrink_to_points(img, dpy_scale, opts, bounds));
    }
    if is_jpeg(buf)
        && let Some(img) = decode_jpeg_scaled(buf, opts, bounds, dpy_scale)?
    {
        return Ok(img);
    }
    #[cfg(target_os = "macos")]
    if crate::imageio::is_heif(buf) {
        // decode_full applies the preview size/alloc guards itself.
        let (img, _) = decode_full(buf)?;
        return Ok(shrink_to_points(img, dpy_scale, opts, bounds));
    }
    let (img, _) = decode_full(buf)?;
    Ok(shrink_to_points(img, dpy_scale, opts, bounds))
}

/// Shrink a decoded bitmap to its point size for scaled displays, opting in
/// via `ISEE_DPI_SCALE=2`. Default (`dpy_scale` 1) declares the native pixel
/// size, which iTerm2/Warp render at 1px = 1pt and auto-fit to the window —
/// exactly imgcat's behavior. With scale 2 the bitmap halves first so a
/// Retina screenshot shows at QuickLook size (one source pixel per device
/// pixel after the terminal's 1pt = 2 device-px render). `-w` applies in
/// point space afterwards, then the bounds cap.
fn shrink_to_points(
    img: DynamicImage,
    dpy_scale: u32,
    opts: &RenderOpts,
    bounds: size::Bounds,
) -> DynamicImage {
    if dpy_scale <= 1 {
        return img;
    }
    let (iw, ih) = (img.width(), img.height());
    let (tw, th) = size::target_dims(iw.div_ceil(dpy_scale), ih.div_ceil(dpy_scale), opts, bounds);
    if (tw, th) == (iw, ih) {
        return img;
    }
    img.resize(tw, th, size::filter(opts.quality))
}

fn is_jpeg(buf: &[u8]) -> bool {
    buf.len() >= 2 && buf[0] == 0xFF && buf[1] == 0xD8
}

/// Detect a GIF by its 6-byte signature (`GIF87a` / `GIF89a`), before any
/// decode, so the animation path can be gated on `-a`.
fn is_gif(buf: &[u8]) -> bool {
    buf.starts_with(b"GIF87a") || buf.starts_with(b"GIF89a")
}

/// Decode a JPEG with DCT downscaling when it is much larger than the preview
/// target. Returns `None` (so the caller falls back to the regular decoder)
/// for unsupported pixel formats, small / already-fitting images, or any
/// parse/scale/decode failure.
fn decode_jpeg_scaled(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: size::Bounds,
    dpy_scale: u32,
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
    // Targets live in point space on scaled displays: divide first so the DCT
    // pre-scale matches the point-space preview (the render resize finishes
    // the job when DCT's discrete ratios overshoot).
    let (ow, oh) = (ow.div_ceil(dpy_scale), oh.div_ceil(dpy_scale));
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

/// Reject an SVG whose rasterized pixel area exceeds `SVG_MAX_PIXELS`, using
/// overflow-safe multiplication so a pathological declared size cannot wrap.
/// The `resvg` pixmap allocation is outside `image::Limits`, so this must run
/// before `Pixmap::new` rather than relying on the decode reader limits.
fn check_svg_size(w: u32, h: u32) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(pixels) = u64::from(w).checked_mul(u64::from(h))
        && pixels <= SVG_MAX_PIXELS
    {
        return Ok(());
    }
    Err(
        format!("SVG exceeds the preview pixel limit ({w}x{h}, max {SVG_MAX_PIXELS} pixels)")
            .into(),
    )
}

fn decode_svg(buf: &[u8]) -> Result<DynamicImage, Box<dyn std::error::Error>> {
    let tree = resvg::usvg::Tree::from_data(buf, &svg_options())?;
    let size = tree.size();
    let width = size.width().ceil().max(1.0) as u32;
    let height = size.height().ceil().max(1.0) as u32;
    check_svg_size(width, height)?;
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

/// A loaded preview source: one static image, or an animation with every
/// frame decoded (opted in via `-a`).
pub enum Loaded {
    Static(DynamicImage),
    Anim(Animation),
}

impl Loaded {
    /// Pixel dimensions to size a preview from: the static image itself, or
    /// the animation's raw canvas from the file header — frames are already
    /// resized to the shared preview target, so the header is the only place
    /// the original dimensions survive.
    pub fn dims(&self) -> (u32, u32) {
        match self {
            Loaded::Static(img) => img.dimensions(),
            Loaded::Anim(anim) => match anim.kind {
                AnimKind::Gif => gif_canvas(&anim.raw),
                AnimKind::Webp => webp_canvas(&anim.raw),
            },
        }
    }
}

/// Which container an `Animation` was decoded from. The kind decides the
/// raw-passthrough behavior: OSC 1337 terminals replay raw GIF bytes, but
/// none of them renders an animated WebP payload.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum AnimKind {
    Gif,
    Webp,
}

/// An animation: composited full-canvas frames plus the loop count from the
/// container header. The original bytes are kept so terminals that animate
/// GIFs themselves (OSC 1337) can pass the file through unmodified.
pub struct Animation {
    pub kind: AnimKind,
    pub raw: Vec<u8>,
    pub frames: Vec<AnimFrame>,
    pub loop_count: LoopCount,
}

/// Logical screen size from a GIF header: little-endian u16 canvas width at
/// offset 6 and height at offset 8, right after the 6-byte signature.
fn gif_canvas(raw: &[u8]) -> (u32, u32) {
    if raw.len() < 10 {
        return (1, 1);
    }
    let le = |b: &[u8]| u16::from_le_bytes([b[0], b[1]]) as u32;
    (le(&raw[6..8]).max(1), le(&raw[8..10]).max(1))
}

/// Canvas size from a WebP VP8X chunk: three-byte little-endian (width-1,
/// height-1) after the 4-byte flags/reserved prefix. Animated WebPs are
/// always extended-format files, so the first VP8X carries the canvas; a
/// missing or truncated chunk falls back to 1x1.
fn webp_canvas(raw: &[u8]) -> (u32, u32) {
    if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WEBP" {
        return (1, 1);
    }
    let mut off = 12usize;
    while off + 8 <= raw.len() {
        let size =
            u32::from_le_bytes([raw[off + 4], raw[off + 5], raw[off + 6], raw[off + 7]]) as usize;
        let data = off + 8;
        if &raw[off..off + 4] == b"VP8X" {
            if raw.len() >= data + 10 {
                let u24 = |b: &[u8]| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16;
                return (
                    u24(&raw[data + 4..data + 7]) + 1,
                    u24(&raw[data + 7..data + 10]) + 1,
                );
            }
            return (1, 1);
        }
        // RIFF chunks pad their payload out to an even size.
        off = data + size + (size & 1);
    }
    (1, 1)
}

pub fn load(
    source: &Source,
    opts: &RenderOpts,
    bounds: size::Bounds,
    animate: bool,
) -> Result<Loaded, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    if animate
        && is_gif(&buf)
        && let Ok(anim) = decode_gif(&buf, opts, bounds, opts.dpy_scale)
    {
        return Ok(Loaded::Anim(anim));
    }
    if animate
        && is_webp(&buf)
        && let Ok(anim) = decode_webp(&buf, opts, bounds, opts.dpy_scale)
    {
        return Ok(Loaded::Anim(anim));
    }
    let img = decode_for_preview(&buf, opts, bounds, opts.dpy_scale)?;
    Ok(Loaded::Static(img))
}

/// A single composited animation frame, resized to the shared preview target.
pub struct AnimFrame {
    pub img: DynamicImage,
    /// Gap to the next frame in whole milliseconds (at least 1).
    pub delay_ms: u32,
}

/// Decode an animated GIF into composited full-canvas frames (`GifDecoder`
/// blends each frame onto the logical screen itself) resized to one shared
/// preview target, so every frame reports identical dimensions. The first
/// frame must pass the regular preview size/alloc checks; retained frames
/// must fit `ANIM_MAX_ALLOC` / `ANIM_MAX_FRAMES`, otherwise the clip is
/// truncated at a frame boundary. Fewer than two frames — or a mid-stream
/// failure before reaching two — is an error so the caller falls back to the
/// static path.
fn decode_gif(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: size::Bounds,
    dpy_scale: u32,
) -> Result<Animation, Box<dyn std::error::Error>> {
    // The GIF signature was verified by the caller (`is_gif`), so no format
    // guessing is needed; `GifDecoder::new` takes the reader directly.
    let decoder = GifDecoder::new(Cursor::new(buf))?;
    let loop_count = decoder.loop_count();
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut target = (0, 0);
    let mut budget: u64 = 0;
    for res in decoder.into_frames() {
        let frame = match res {
            Ok(frame) => frame,
            Err(err) => {
                if frames.len() >= 2 {
                    break; // keep whatever decoded cleanly so far
                }
                return Err(err.into());
            }
        };
        let (w, h) = (frame.buffer().width(), frame.buffer().height());
        if frames.is_empty() {
            check_preview_size(w, h)?;
            check_preview_alloc(w, h, frame.buffer().len() as u64)?;
            // One shared point-space target on scaled displays (the same rule
            // as `shrink_to_points`), keeping all frames mutually consistent.
            let (pw, ph) = (w.div_ceil(dpy_scale), h.div_ceil(dpy_scale));
            target = size::target_dims(pw, ph, opts, bounds);
        }
        let cost = u64::from(target.0) * u64::from(target.1) * 4;
        if frames.len() >= ANIM_MAX_FRAMES || (!frames.is_empty() && budget + cost > ANIM_MAX_ALLOC)
        {
            break; // truncation at a frame boundary
        }
        let delay_ms = u32::try_from(Duration::from(frame.delay()).as_millis())
            .unwrap_or(u32::MAX)
            .max(ANIM_MIN_DELAY_MS);
        let mut img = DynamicImage::ImageRgba8(frame.into_buffer());
        if (w, h) != target {
            img = img.resize_exact(target.0, target.1, size::filter(opts.quality));
        }
        budget += cost;
        frames.push(AnimFrame { img, delay_ms });
    }
    if frames.len() < 2 {
        return Err("GIF has fewer than two frames".into());
    }
    Ok(Animation {
        kind: AnimKind::Gif,
        raw: buf.to_vec(),
        frames,
        loop_count,
    })
}

/// Detect a WebP by its RIFF container signature, before any decode, so the
/// animation path can be gated on `-a`.
fn is_webp(buf: &[u8]) -> bool {
    buf.len() >= 12 && &buf[0..4] == b"RIFF" && &buf[8..12] == b"WEBP"
}

/// Decode an animated WebP into composited full-canvas frames. `image`'s
/// top-level API exposes only static WebP decoding, so this drives
/// `image-webp`'s decoder directly: `reset_animation` positions the reader at
/// the first ANMF chunk (the chunk table is built during construction), and
/// each `read_frame` blends one frame onto the canvas and reports its
/// duration. The same guardrails as `decode_gif` apply; fewer than two
/// frames — or a mid-stream failure before reaching two — is an error so the
/// caller falls back to the static path.
fn decode_webp(
    buf: &[u8],
    opts: &RenderOpts,
    bounds: size::Bounds,
    dpy_scale: u32,
) -> Result<Animation, Box<dyn std::error::Error>> {
    // The RIFF/WEBP signature was verified by the caller (`is_webp`).
    let mut decoder = WebPDecoder::new(Cursor::new(buf))?;
    if !decoder.is_animated() {
        return Err("WebP has fewer than two frames".into());
    }
    let (cw, ch) = decoder.dimensions();
    check_preview_size(cw, ch)?;
    let Some(canvas_bytes) = decoder.output_buffer_size() else {
        return Err("WebP animation has no output buffer".into());
    };
    check_preview_alloc(cw, ch, canvas_bytes as u64)?;
    let loop_count = match decoder.loop_count() {
        image_webp::LoopCount::Forever => LoopCount::Infinite,
        image_webp::LoopCount::Times(n) => LoopCount::Finite(NonZeroU32::from(n)),
    };
    decoder.reset_animation();

    let has_alpha = decoder.has_alpha();
    let target = size::target_dims(cw.div_ceil(dpy_scale), ch.div_ceil(dpy_scale), opts, bounds);
    let mut canvas = vec![0u8; canvas_bytes];
    let mut frames: Vec<AnimFrame> = Vec::new();
    let mut budget: u64 = 0;
    loop {
        let cost = u64::from(target.0) * u64::from(target.1) * 4;
        if frames.len() >= ANIM_MAX_FRAMES || (!frames.is_empty() && budget + cost > ANIM_MAX_ALLOC)
        {
            break; // truncation at a frame boundary
        }
        match decoder.read_frame(&mut canvas) {
            Ok(raw_delay) => {
                let delay_ms = raw_delay.max(ANIM_MIN_DELAY_MS);
                let base = if has_alpha {
                    DynamicImage::ImageRgba8(
                        image::RgbaImage::from_raw(cw, ch, canvas.clone())
                            .ok_or("WebP frame buffer size mismatch")?,
                    )
                } else {
                    DynamicImage::ImageRgb8(
                        image::RgbImage::from_raw(cw, ch, canvas.clone())
                            .ok_or("WebP frame buffer size mismatch")?,
                    )
                };
                let img = if (cw, ch) == target {
                    base
                } else {
                    base.resize_exact(target.0, target.1, size::filter(opts.quality))
                };
                budget += cost;
                frames.push(AnimFrame { img, delay_ms });
            }
            Err(image_webp::DecodingError::NoMoreFrames) => break,
            Err(err) => {
                if frames.len() >= 2 {
                    break; // keep whatever decoded cleanly so far
                }
                return Err(err.into());
            }
        }
    }
    if frames.len() < 2 {
        return Err("WebP has fewer than two frames".into());
    }
    Ok(Animation {
        kind: AnimKind::Webp,
        raw: buf.to_vec(),
        frames,
        loop_count,
    })
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
    #[cfg(target_os = "macos")]
    if crate::imageio::is_heif(&buf) {
        // Properties-only read: no pixel decode on the `-i` path.
        let h = crate::imageio::load_info(&buf)?;
        return Ok(ImageInfo {
            size: buf.len() as u64,
            width: h.width,
            height: h.height,
            dpi: h.dpi,
            alpha: h.alpha,
            color: h.color,
        });
    }
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
            quality: size::Quality::default(),
            cell: crate::detect::CellPx { w: 9, h: 18 },
            win: crate::detect::WinSize {
                cols: 200,
                rows: 50,
                px: None,
            },
            dpy_scale: 1,
            tmux: false,
            transfer: crate::size::KgpTransfer::Stream,
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
        let bounds = size::Bounds::window(100_000, 100_000);
        assert_eq!(size::target_dims(1200, 600, &o, bounds), (240, 120));
        let img = decode_jpeg_scaled(&jpeg, &o, bounds, 1)
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
        let bounds = size::Bounds::window(100_000, 100_000);
        let img = decode_jpeg_scaled(&jpeg, &o, bounds, 1)
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
        let bounds = size::Bounds::window(100_000, 100_000);
        assert!(decode_jpeg_scaled(&jpeg, &o, bounds, 1).unwrap().is_none());
    }

    #[test]
    fn jpeg_exif_orientation_is_preserved() {
        // Rotate90 (orientation 6) on a 1200x600 raw grid displays as 600x1200,
        // so the target is a portrait 240x480. The raw DCT request is (480,240),
        // yielding 600x300, then rotated to a 300x600 portrait.
        let jpeg = with_exif_orientation(&encode_jpeg(1200, 600), 6);
        let o = opts_width(Some(240));
        let bounds = size::Bounds::window(100_000, 100_000);
        assert_eq!(size::target_dims(600, 1200, &o, bounds), (240, 480));
        let img = decode_jpeg_scaled(&jpeg, &o, bounds, 1)
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
        let bounds = size::Bounds::window(100_000, 100_000);
        // JPEG goes through DCT scaling.
        let jpeg = encode_jpeg(1200, 600);
        let scaled = decode_for_preview(&jpeg, &o, bounds, 1).unwrap();
        assert_eq!((scaled.width(), scaled.height()), (300, 150));
        // PNG never uses DCT scaling: a full decode, exact resize is done later.
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(1200, 600))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let full = decode_for_preview(&png, &o, bounds, 1).unwrap();
        assert_eq!((full.width(), full.height()), (1200, 600));
    }

    // ---- bitmap display scale (Retina Iip/Sixel point sizing) ----

    fn encode_png(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    #[test]
    fn dpy_scale_shrinks_bitmap_to_point_size() {
        // A 400x300 image on a scale-2 terminal must become a 200x150 bitmap:
        // declared as 200x150px it renders 200x150pt = 400x300 device px,
        // the natural imgcat-sized result, instead of the doubled 300pt.
        let png = encode_png(400, 300);
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let img = decode_for_preview(&png, &o, bounds, 2).unwrap();
        assert_eq!((img.width(), img.height()), (200, 150));
    }

    #[test]
    fn dpy_scale_one_keeps_native_size() {
        let png = encode_png(400, 300);
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let img = decode_for_preview(&png, &o, bounds, 1).unwrap();
        assert_eq!((img.width(), img.height()), (400, 300));
    }

    #[test]
    fn dpy_scale_width_request_applies_in_point_space() {
        // -w 800 means 800 logical points on a scale-2 display: the bitmap is
        // upscaled from the 200pt natural size to an 800pt-wide one.
        let png = encode_png(400, 300);
        let mut o = opts_width(Some(800));
        o.win = crate::detect::WinSize {
            cols: 400,
            rows: 100,
            px: None,
        }; // point bounds 3600x1800 leave room for the upscale
        let bounds = size::Bounds::window(3_600, 1_800);
        let img = decode_for_preview(&png, &o, bounds, 2).unwrap();
        assert_eq!((img.width(), img.height()), (800, 600));
    }

    #[test]
    fn dpy_scale_bounds_cap_survives_scaling() {
        // The point bounds still cap: a 400x300 image at scale 2 with a
        // 100x50-point window shrinks further to fit (100x75).
        let png = encode_png(400, 300);
        let o = opts_width(None);
        let bounds = size::Bounds::window(100, 50);
        let img = decode_for_preview(&png, &o, bounds, 2).unwrap();
        assert_eq!((img.width(), img.height()), (67, 50));
    }

    #[test]
    fn jpeg_dct_target_uses_point_space() {
        // 1200x600 at scale 2 behaves like a 600x300 source: no -w keeps it
        // at its point size (600x300), via DCT pre-scaling.
        let jpeg = encode_jpeg(1200, 600);
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let img = decode_for_preview(&jpeg, &o, bounds, 2).unwrap();
        assert_eq!((img.width(), img.height()), (600, 300));
    }

    #[test]
    fn check_preview_size_accepts_within_limit() {
        assert!(check_preview_size(12000, 9000).is_ok());
        assert!(check_preview_size(1, 1).is_ok());
        assert!(check_preview_size(12000, 12000).is_ok());
    }

    #[test]
    fn check_preview_size_rejects_over_dimension() {
        assert!(check_preview_size(12001, 10).is_err());
        assert!(check_preview_size(10, 12001).is_err());
        let err = check_preview_size(12001, 10).unwrap_err().to_string();
        assert!(err.contains("preview size limit"), "got {err}");
    }

    #[test]
    fn check_preview_alloc_rejects_over_memory() {
        assert!(check_preview_alloc(100, 100, 100 * 100 * 4).is_ok());
        assert!(check_preview_alloc(12000, 12000, PREVIEW_MAX_ALLOC).is_ok());
        let err = check_preview_alloc(12000, 12000, PREVIEW_MAX_ALLOC + 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("preview memory limit"), "got {err}");
    }

    #[test]
    fn check_svg_size_accepts_within_pixel_limit() {
        assert!(check_svg_size(4000, 4000).is_ok());
        assert!(check_svg_size(4096, 4096).is_ok());
    }

    #[test]
    fn check_svg_size_rejects_over_pixel_limit() {
        assert!(check_svg_size(4097, 4097).is_err());
        assert!(check_svg_size(u32::MAX, u32::MAX).is_err());
        let err = check_svg_size(4097, 4097).unwrap_err().to_string();
        assert!(err.contains("preview pixel limit"), "got {err}");
    }

    fn svg_of_size(w: u32, h: u32) -> Vec<u8> {
        format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}"><rect width="{w}" height="{h}" fill="red"/></svg>"#
        )
        .into_bytes()
    }

    #[test]
    fn svg_below_pixel_limit_still_decodes() {
        let img = decode_svg(&svg_of_size(200, 200)).unwrap();
        assert_eq!((img.width(), img.height()), (200, 200));
    }

    #[test]
    fn svg_over_pixel_limit_rejected_before_raster() {
        let err = decode_svg(&svg_of_size(5000, 5000))
            .unwrap_err()
            .to_string();
        assert!(err.contains("preview pixel limit"), "got {err}");
    }

    #[test]
    fn decode_full_rejects_png_over_dimension_limit() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(12001, 10))
            .write_to(&mut Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let err = decode_full(&png).unwrap_err().to_string();
        assert!(err.contains("limit"), "got {err}");
    }

    #[test]
    fn decode_full_rejects_over_dimension_jpeg() {
        let jpeg = encode_jpeg(12001, 10);
        let err = decode_full(&jpeg).unwrap_err().to_string();
        assert!(err.contains("limit"), "got {err}");
    }

    #[test]
    fn jpeg_dct_scales_raw_source_over_dimension_limit() {
        let jpeg = encode_jpeg(12001, 60);
        let o = opts_width(Some(240));
        let bounds = size::Bounds::window(100_000, 100_000);
        let img = decode_jpeg_scaled(&jpeg, &o, bounds, 1)
            .unwrap()
            .expect("DCT must scale an oversized JPEG");
        assert!(img.width() < 12001, "DCT must not keep full width");
        assert!(img.width() >= 240 && img.height() >= 1);
        assert!(decode_for_preview(&jpeg, &o, bounds, 1).is_ok());
    }

    #[test]
    fn load_info_ignores_preview_dimension_limit() {
        let jpeg = encode_jpeg(12001, 60);
        let path = std::env::temp_dir().join(format!("isee_load_info_{}.jpg", std::process::id()));
        std::fs::write(&path, &jpeg).unwrap();
        let info = load_info(&Source::Path(path.clone())).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(info.width, 12001);
        assert_eq!(info.height, 60);
        assert_eq!(info.size, jpeg.len() as u64);
    }

    // ---- GIF animation ----

    use image::codecs::gif::{GifEncoder, Repeat};

    fn anim_frame(w: u32, h: u32, rgb: [u8; 3], delay_cs: u16) -> image::Frame {
        let mut img = image::RgbaImage::new(w, h);
        for px in img.pixels_mut() {
            *px = image::Rgba([rgb[0], rgb[1], rgb[2], 255]);
        }
        image::Frame::from_parts(
            img,
            0,
            0,
            // numer/denom is in milliseconds (cs*10 over 1 = the centisecond
            // GIF delay), NOT numerator-over-1000.
            image::Delay::from_numer_denom_ms(u32::from(delay_cs) * 10, 1),
        )
    }

    fn encode_gif(frames: &[image::Frame], repeat: Repeat) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut out);
            enc.set_repeat(repeat).unwrap();
            for f in frames {
                enc.encode_frame(f.clone()).unwrap();
            }
        }
        out
    }

    #[test]
    fn detects_gif_signature() {
        assert!(is_gif(b"GIF89a\x01\x00\x01\x00"));
        assert!(is_gif(b"GIF87a\x01\x00\x01\x00"));
        assert!(!is_gif(b"GIF98a\x01\x00\x01\x00"));
        assert!(!is_gif(b""));
        assert!(!is_gif(b"\x89PNG\r\n\x1a\n"));
    }

    #[test]
    fn decode_gif_extracts_composited_frames_and_loop_count() {
        let frames = [
            anim_frame(4, 4, [255, 0, 0], 5),
            anim_frame(4, 4, [0, 255, 0], 20),
            anim_frame(4, 4, [0, 0, 255], 100),
        ];
        let buf = encode_gif(&frames, Repeat::Finite(2));
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let anim = decode_gif(&buf, &o, bounds, 1).unwrap();
        assert_eq!(anim.frames.len(), 3);
        let delays: Vec<u32> = anim.frames.iter().map(|f| f.delay_ms).collect();
        assert_eq!(delays, vec![50, 200, 1000]);
        for f in &anim.frames {
            assert_eq!(f.img.color(), ColorType::Rgba8);
            assert_eq!((f.img.width(), f.img.height()), (4, 4));
        }
        if let LoopCount::Finite(n) = anim.loop_count {
            assert_eq!(n.get(), 2);
        } else {
            panic!("expected a finite loop count");
        }
        assert_eq!(anim.raw, buf);
    }

    #[test]
    fn decode_gif_resizes_all_frames_to_shared_target() {
        let frames = [
            anim_frame(40, 20, [255, 0, 0], 5),
            anim_frame(40, 20, [0, 255, 0], 10),
        ];
        let buf = encode_gif(&frames, Repeat::Infinite);
        let o = opts_width(Some(20));
        let bounds = size::Bounds::window(100_000, 100_000);
        let anim = decode_gif(&buf, &o, bounds, 1).unwrap();
        assert_eq!(anim.frames.len(), 2);
        for f in &anim.frames {
            assert_eq!((f.img.width(), f.img.height()), (20, 10));
        }
    }

    #[test]
    fn decode_gif_with_single_frame_errors_for_static_fallback() {
        let buf = encode_gif(&[anim_frame(4, 4, [1, 2, 3], 5)], Repeat::Infinite);
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let Err(err) = decode_gif(&buf, &o, bounds, 1) else {
            panic!("expected an error for a single-frame GIF");
        };
        assert!(
            err.to_string().contains("fewer than two frames"),
            "got {err}"
        );
    }

    #[test]
    fn decode_gif_clamps_zero_delay_to_one_ms() {
        // A zero gap must never hit the wire: kitty ignores z=0 and would
        // fall back to its 40 ms default.
        let frames = [
            anim_frame(4, 4, [255, 0, 0], 0),
            anim_frame(4, 4, [0, 255, 0], 0),
        ];
        let buf = encode_gif(&frames, Repeat::Infinite);
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let anim = decode_gif(&buf, &o, bounds, 1).unwrap();
        for f in &anim.frames {
            assert_eq!(f.delay_ms, 1);
        }
    }

    #[test]
    fn load_returns_gif_only_when_animate_requested() {
        let frames = [
            anim_frame(4, 4, [255, 0, 0], 5),
            anim_frame(4, 4, [0, 255, 0], 5),
        ];
        let buf = encode_gif(&frames, Repeat::Infinite);
        let path = std::env::temp_dir().join(format!("isee_gif_{}.gif", std::process::id()));
        std::fs::write(&path, &buf).unwrap();
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let without_flag = load(&Source::Path(path.clone()), &o, bounds, false).unwrap();
        assert!(matches!(without_flag, Loaded::Static(_)));
        let with_flag = load(&Source::Path(path.clone()), &o, bounds, true).unwrap();
        let Loaded::Anim(anim) = with_flag else {
            panic!("expected an animated load with -a");
        };
        assert_eq!(anim.frames.len(), 2);
        let _ = std::fs::remove_file(&path);
    }

    // ---- WebP animation ----

    /// A tiny 3-frame 8x6 animation generated once with img2webp
    /// (`img2webp -o anim.webp -loop 5 -d 80`): loop count 5, 80 ms gaps.
    const WEBP_ANIM: &[u8] = include_bytes!("../tests/fixtures/anim.webp");

    fn encode_webp_static(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::new();
        image::DynamicImage::ImageRgb8(image::RgbImage::new(w, h))
            .write_to(&mut Cursor::new(&mut out), image::ImageFormat::WebP)
            .unwrap();
        out
    }

    #[test]
    fn detects_webp_signature() {
        assert!(is_webp(b"RIFF\x12\x00\x00\x00WEBPVP8 "));
        assert!(!is_webp(b"RIFF\x12\x00\x00\x00WAV "));
        assert!(!is_webp(b"RIFF"));
        assert!(!is_webp(b""));
    }

    #[test]
    fn webp_canvas_parses_vp8x_and_falls_back() {
        let u24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
        let mut raw = b"RIFF\x18\x00\x00\x00WEBP".to_vec();
        raw.extend_from_slice(b"VP8X");
        raw.extend_from_slice(&10u32.to_le_bytes());
        raw.extend_from_slice(&[0, 0, 0, 0]); // flags + reserved
        raw.extend_from_slice(&u24(4999)); // width-1
        raw.extend_from_slice(&u24(11199)); // height-1
        assert_eq!(webp_canvas(&raw), (5000, 11200));
        // No VP8X (simple lossy file): the caller never asks, but stay safe.
        assert_eq!(webp_canvas(&encode_webp_static(8, 6)), (1, 1));
        assert_eq!(webp_canvas(b"RIFF\x12\x00\x00\x00WEBP"), (1, 1));
        assert_eq!(webp_canvas(b""), (1, 1));
    }

    #[test]
    fn decode_webp_extracts_composited_frames_and_loop_count() {
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let anim = decode_webp(WEBP_ANIM, &o, bounds, 1).unwrap();
        assert_eq!(anim.kind, AnimKind::Webp);
        assert_eq!(anim.frames.len(), 3);
        for f in &anim.frames {
            assert_eq!((f.img.width(), f.img.height()), (8, 6));
            assert_eq!(f.delay_ms, 80);
        }
        if let LoopCount::Finite(n) = anim.loop_count {
            assert_eq!(n.get(), 5);
        } else {
            panic!("expected a finite loop count");
        }
        assert_eq!(anim.raw, WEBP_ANIM);
    }

    #[test]
    fn decode_webp_composites_each_frame_onto_canvas() {
        // img2webp encodes the red/lime/blue fixture lossily; allow for that.
        let expected = [[255u8, 0, 0], [0, 255, 0], [0, 0, 255]];
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let anim = decode_webp(WEBP_ANIM, &o, bounds, 1).unwrap();
        for (f, rgb) in anim.frames.iter().zip(expected) {
            let rgba = f.img.to_rgba8();
            let px = rgba.get_pixel(4, 3);
            for ch in 0..3 {
                assert!(
                    (i16::from(px[ch]) - i16::from(rgb[ch])).abs() <= 16,
                    "frame {ch} pixel {px:?} vs {rgb:?}"
                );
            }
        }
    }

    #[test]
    fn decode_webp_resizes_all_frames_to_shared_target() {
        let o = opts_width(Some(4));
        let bounds = size::Bounds::window(100_000, 100_000);
        let anim = decode_webp(WEBP_ANIM, &o, bounds, 1).unwrap();
        assert_eq!(anim.frames.len(), 3);
        for f in &anim.frames {
            assert_eq!((f.img.width(), f.img.height()), (4, 3));
        }
    }

    #[test]
    fn load_returns_webp_only_when_animate_requested() {
        let path = std::env::temp_dir().join(format!("isee_webp_{}.webp", std::process::id()));
        std::fs::write(&path, WEBP_ANIM).unwrap();
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let without_flag = load(&Source::Path(path.clone()), &o, bounds, false).unwrap();
        assert!(matches!(without_flag, Loaded::Static(_)));
        let with_flag = load(&Source::Path(path.clone()), &o, bounds, true).unwrap();
        let Loaded::Anim(anim) = with_flag else {
            panic!("expected an animated load with -a");
        };
        assert_eq!(anim.frames.len(), 3);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn load_keeps_static_webp_static_even_with_animate() {
        let buf = encode_webp_static(8, 6);
        assert!(is_webp(&buf));
        let o = opts_width(None);
        let bounds = size::Bounds::window(10_000, 10_000);
        let path =
            std::env::temp_dir().join(format!("isee_webp_static_{}.webp", std::process::id()));
        std::fs::write(&path, &buf).unwrap();
        let loaded = load(&Source::Path(path.clone()), &o, bounds, true).unwrap();
        assert!(matches!(loaded, Loaded::Static(_)));
        let _ = std::fs::remove_file(&path);
    }
}
