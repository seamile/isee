use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

/// Scaling bounds for bitmap protocols drawn by the terminal itself
/// (Iip/Sixel), in LOGICAL pixels: the probed/logical cell times the grid.
/// How a declared `Npx` size actually renders is brand-dependent (measured
/// on a 2x Retina display): Warp & co draw one image pixel per logical
/// point, but iTerm2 draws one image pixel per DEVICE pixel — these bounds
/// follow yazi's logical-cell convention for both drivers, so they are
/// exact on Warp and conservative (2x) on iTerm2, whose auto-fit still
/// clamps the result. KGP instead maps an image pixel to a device pixel.
pub fn bitmap_bounds(o: &RenderOpts) -> (u64, u64) {
    (
        (o.win.cols as u64 * o.cell.w.max(1) as u64).max(1),
        (o.win.rows as u64 * o.cell.h.max(1) as u64).max(1),
    )
}

/// The grid kitty placeholders can address, in DEVICE pixels: a placeholder's
/// row/column offset is one diacritic from a 297-entry table
/// (`kitty::MAX_PLACEHOLDER_CELLS`), so both axes hard-cap there no matter
/// how large the terminal is.
pub fn kitty_grid_cap_px(o: &RenderOpts) -> (u64, u64) {
    let (cw, ch) = kitty_cell(o);
    let cap = crate::kitty::MAX_PLACEHOLDER_CELLS as u64;
    (cap * cw as u64, cap * ch as u64)
}

/// Terminal-grid bounds for the Kitty protocol, in DEVICE pixels: the
/// physical cell times the grid. The placeholder grid derives from this same
/// cell so `ceil(w/cell) <= cols`, and an ultra-wide image can never wrap its
/// last placeholder column onto the next line. No protocol cap applied (see
/// `kitty_bounds`).
///
/// The raw window-pixel report (`win.px`) is deliberately NOT used as a
/// bound: on Ghostty it includes window padding, which can exceed the grid
/// by a fraction of a cell.
fn kitty_terminal_bounds(o: &RenderOpts) -> (u64, u64) {
    let (cw, ch) = kitty_cell(o);
    (
        (o.win.cols as u64 * cw as u64).max(1),
        (o.win.rows as u64 * ch as u64).max(1),
    )
}

/// Scaling bounds for the Kitty protocol, in DEVICE pixels: the terminal
/// grid. Direct placement (`a=T` without `C=1`) renders the bitmap at its
/// declared device-pixel size and auto-fits oversize images, so the only
/// bound is what the window can show. Inside tmux, though, rendering goes
/// through the placeholder grid (tmux's cursor model cannot track the outer
/// terminal's placement moves), and a placeholder cell's row/column offset is
/// one diacritic from a 297-entry table — offsets past 296 cannot be
/// expressed and fall back to diacritic 0, garbling everything right of
/// row/column 296 (visible from `-w` ~2970 on a 10 px-cell terminal). So the
/// tmux path clamps to the addressable grid; images beyond it shrink to fit.
pub fn kitty_bounds(o: &RenderOpts) -> (u64, u64) {
    let (w, h) = kitty_terminal_bounds(o);
    if o.tmux {
        let cap = kitty_grid_cap_px(o);
        (w.min(cap.0), h.min(cap.1))
    } else {
        (w, h)
    }
}

/// One-line stderr notice for the kitty placeholder limit (tmux only): `Some`
/// when this image's preview had to shrink below what the terminal grid alone
/// would allow — i.e. the 297-entry diacritics table, not the window, is the
/// binding constraint. Direct placement (non-tmux) has no such cap, so this
/// is naturally `None` there. `img` is the RAW image size (for animations the
/// GIF canvas, not the resized preview frames).
pub fn kitty_protocol_clamp_notice(img: (u32, u32), o: &RenderOpts) -> Option<String> {
    let uncapped = target_dims(img.0, img.1, o, kitty_terminal_bounds(o));
    let capped = target_dims(img.0, img.1, o, kitty_bounds(o));
    if uncapped == capped {
        return None;
    }
    Some(format!(
        "isee: limited to {}x{} px by kitty's placeholder cell table (tmux)",
        capped.0, capped.1,
    ))
}

/// Physical cell size for the Kitty placeholder grid: max() of the probed
/// cell and window-pixels/grid, so a logical-cell probe on a HiDPI terminal
/// cannot halve the grid (and a logical px report cannot either).
pub fn kitty_cell(o: &RenderOpts) -> (u32, u32) {
    let w = o.win.px.map_or(o.cell.w, |(pw, _)| {
        o.cell.w.max((pw / o.win.cols.max(1)).max(1))
    });
    let h = o.win.px.map_or(o.cell.h, |(_, ph)| {
        o.cell.h.max((ph / o.win.rows.max(1)).max(1))
    });
    (w.max(1), h.max(1))
}

/// Scaling bounds for half-block rendering, in cell units (logical): the
/// output is plain text, one character per cell column and two half-block
/// pixel rows per cell row.
pub fn halfblock_bounds(o: &RenderOpts) -> (u64, u64) {
    (
        (o.win.cols as u64 * o.cell.w.max(1) as u64).max(1),
        (o.win.rows as u64 * o.cell.h.max(1) as u64 * 2).max(1),
    )
}

/// How KGP payloads reach the terminal: streamed through the pty in
/// escape-sequence chunks (`Stream`), or handed over as a temp file whose
/// path alone crosses the pty (`Tempfile`). Tempfile is only usable when a
/// probe confirmed the terminal accepts `t=1` transfers (kitty deletes the
/// file after reading); anything else falls back to `Stream`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum KgpTransfer {
    Stream,
    Tempfile,
}

pub struct RenderOpts {
    pub width: Option<u32>,
    pub quality: Quality,
    pub cell: CellPx,
    pub win: WinSize,
    /// Device pixels per logical point for the bitmap protocols (Iip/Sixel),
    /// from `ISEE_DPI_SCALE` only (1 when unset). When set, the bitmap is
    /// shrunk to point size first (`input::shrink_to_points`), which assumes
    /// the terminal renders one declared px as one logical point — true on
    /// Warp, FALSE on iTerm2 (device pixels there), and unused by Kitty
    /// (device pixels) and Half Blocks (cell units).
    pub dpy_scale: u32,
    /// True inside tmux: kitty renders through the placeholder grid (tmux's
    /// cursor model cannot track the outer terminal's placement moves), so
    /// the 297-cell diacritics clamp still applies.
    pub tmux: bool,
    /// KGP payload transport chosen at detect() time (probe result or
    /// `ISEE_KGP_TRANSFER` override).
    pub transfer: KgpTransfer,
}

/// Preview quality tier mapped to a resize filter:
/// - Low: Nearest (1 sample per pixel) — fastest, but aliasing when shrinking
/// - Medium: Triangle (2x2 bilinear) — smooth, slight softening
/// - High: Lanczos3 (6x6 windowed sinc) — sharpest, may ring on hard edges
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Quality {
    Low,
    #[default]
    Medium,
    High,
}

impl Quality {
    pub fn parse(v: &str) -> Option<Quality> {
        match v.trim().to_ascii_lowercase().as_str() {
            "l" => Some(Quality::Low),
            "m" => Some(Quality::Medium),
            "h" => Some(Quality::High),
            _ => None,
        }
    }
}

pub fn filter(quality: Quality) -> image::imageops::FilterType {
    match quality {
        Quality::Low => image::imageops::FilterType::Nearest,
        Quality::Medium => image::imageops::FilterType::Triangle,
        Quality::High => image::imageops::FilterType::Lanczos3,
    }
}

/// Cap for previews without an explicit `-w`: wide images are downscaled to at
/// most this pixel width (bounded in turn by the terminal window size).
pub const DEFAULT_MAX_WIDTH: u64 = 1920;

pub fn target_px(img: &DynamicImage, o: &RenderOpts, bounds: (u64, u64)) -> (u32, u32) {
    let (iw, ih) = img.dimensions();
    target_dims(iw, ih, o, bounds)
}

/// Compute the preview target pixel size from raw dimensions `(iw, ih)` rather
/// than a `DynamicImage`, so callers can drive scaling from an image header
/// (plus EXIF orientation) before any full decode. `target_px` is a thin
/// wrapper over this, so kitty/half-block rendering is unchanged.
pub fn target_dims(iw: u32, ih: u32, o: &RenderOpts, bounds: (u64, u64)) -> (u32, u32) {
    let iw = iw.max(1) as u64;
    let ih = ih.max(1) as u64;
    let (tw, th) = match o.width {
        Some(w) => contain(
            w.max(1) as u64,
            (ih * w.max(1) as u64 / iw).max(1),
            bounds.0,
            bounds.1,
        ),
        // No explicit width: display at the image's native pixel size (ignore
        // DPI), capped at DEFAULT_MAX_WIDTH and then shrunk only as needed to
        // fit the bounds. The window width always wins over any requested or
        // default cap.
        None => contain(iw, ih, bounds.0.min(DEFAULT_MAX_WIDTH), bounds.1),
    };
    (tw as u32, th as u32)
}

fn contain(tw: u64, th: u64, max_w: u64, max_h: u64) -> (u64, u64) {
    if tw <= max_w && th <= max_h {
        return (tw, th);
    }
    let s = (max_w as f64 / tw as f64).min(max_h as f64 / th as f64);
    (
        ((tw as f64 * s).round() as u64).max(1),
        ((th as f64 * s).round() as u64).max(1),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{CellPx, WinSize};

    fn opts() -> RenderOpts {
        RenderOpts {
            width: None,
            quality: Quality::default(),
            cell: CellPx { w: 9, h: 18 },
            win: WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            dpy_scale: 1,
            tmux: false,
            transfer: KgpTransfer::Stream,
        }
    }

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::new_rgba8(w, h)
    }

    #[test]
    fn fits_within_terminal_without_upscale() {
        let o = opts();
        assert_eq!(target_px(&img(100, 50), &o, kitty_bounds(&o)), (100, 50));
    }

    #[test]
    fn scales_down_to_fit() {
        let o = opts();
        // max 720x432 (80 cols x 9 px, 24 rows x 18 px), contain 2000x1000
        // -> scale 0.36 -> 720x360.
        assert_eq!(
            target_px(&img(2000, 1000), &o, kitty_bounds(&o)),
            (720, 360)
        );
    }

    #[test]
    fn width_request_allows_upscale() {
        let mut o = opts();
        o.width = Some(800);
        o.win = WinSize {
            cols: 200,
            rows: 50,
            px: None,
        }; // max 1800x900
        assert_eq!(target_px(&img(100, 50), &o, kitty_bounds(&o)), (800, 400));
    }

    #[test]
    fn width_request_capped_to_terminal() {
        let mut o = opts();
        o.width = Some(5000);
        // max 720x432, image 100x100 -> uniform scale 432/5000 -> 432x432
        assert_eq!(target_px(&img(100, 100), &o, kitty_bounds(&o)), (432, 432));
    }

    #[test]
    fn without_width_keeps_natural_pixel_size() {
        // Window of 200 cols x 50 rows = 1800x900 px: images that fit are
        // shown at native size, never upscaled or shrunk by DPI or the grid.
        let mut o = opts();
        o.win = WinSize {
            cols: 200,
            rows: 50,
            px: None,
        };
        assert_eq!(target_px(&img(100, 50), &o, kitty_bounds(&o)), (100, 50));
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (982, 548));
    }

    #[test]
    fn without_width_capped_to_window() {
        let o = opts();
        // max 720x432: 982x548 -> scale 720/982 -> 720x402; 2000x1000 -> 720x360.
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (720, 402));
        assert_eq!(
            target_px(&img(2000, 1000), &o, kitty_bounds(&o)),
            (720, 360)
        );
    }

    #[test]
    fn hidpi_px_report_doubles_bounds() {
        // Retina: 80x24 grid, window reports 1440x864 device px. The physical
        // cell is 18x36, so bounds = 80*18 x 24*36 = 1440x864 (not the
        // logical 720x432).
        let mut o = opts();
        o.win.px = Some((1440, 864));
        assert_eq!(kitty_cell(&o), (18, 36));
        assert_eq!(kitty_bounds(&o), (1440, 864));
        // 982x548 now fits natively; a 2000x1000 image scales to 1440x720.
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (982, 548));
        assert_eq!(
            target_px(&img(2000, 1000), &o, kitty_bounds(&o)),
            (1440, 720)
        );
    }

    #[test]
    fn padded_px_report_cannot_overflow_grid() {
        // Ghostty includes window padding in ws_xpixel: 1450x870 for the same
        // 80x24 grid of 18x36 cells. The bounds must stay at the exact grid
        // (1440x864) so the placeholder grid never exceeds the terminal
        // columns/rows and the last column cannot wrap to the next line.
        let mut o = opts();
        o.win.px = Some((1450, 870));
        assert_eq!(kitty_cell(&o), (18, 36));
        assert_eq!(kitty_bounds(&o), (1440, 864));
        // Ultra-wide image caps at 1440 wide -> exactly 80 placeholder cols.
        let (tw, _) = target_px(&img(3000, 1000), &o, kitty_bounds(&o));
        assert_eq!(tw.div_ceil(kitty_cell(&o).0), 80);
    }

    #[test]
    fn kitty_bounds_clamped_to_addressable_grid() {
        // A placeholder cell's row/column offset is a single diacritic from
        // kitty's 297-entry rowcolumn-diacritics table, so grids beyond
        // 297x297 cells cannot be addressed. 400 cols x 10 px cells would be
        // 4000 px; the bounds must cap it at 2970 so `-w 3500` shrinks to
        // fit instead of garbling everything right of column 296.
        let mut o = opts();
        o.tmux = true;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        assert_eq!(kitty_bounds(&o), (2970, 5940));
    }

    #[test]
    fn direct_placement_bounds_have_no_protocol_cap() {
        // Outside tmux the image is placed directly at its declared size and
        // the terminal auto-fits oversize bitmaps, so a 400-col window stays
        // 4000 px wide — no 297-cell clamp applies.
        let mut o = opts();
        o.tmux = false;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        assert_eq!(kitty_bounds(&o), (4000, 8000));
    }

    #[test]
    fn kitty_grid_cap_px_tracks_diacritics_table() {
        let mut o = opts();
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        assert_eq!(kitty_grid_cap_px(&o), (2970, 5940));
    }

    #[test]
    fn clamp_notice_fires_for_wide_request() {
        // -w 3500 on a 10 px-cell terminal (tmux): the protocol (2970 px),
        // not the 400-col window (4000 px), is what shrinks the image.
        let mut o = opts();
        o.tmux = true;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        o.width = Some(3500);
        let msg = kitty_protocol_clamp_notice((3000, 100), &o).unwrap();
        assert!(msg.contains("2970x98"), "{msg}");
        assert!(msg.contains("placeholder cell table"), "{msg}");
    }

    #[test]
    fn clamp_notice_silent_for_direct_placement() {
        // Non-tmux direct placement has no protocol cap: the same wide
        // request fits the window and no notice fires.
        let mut o = opts();
        o.tmux = false;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        o.width = Some(3500);
        assert_eq!(kitty_protocol_clamp_notice((3000, 100), &o), None);
    }

    #[test]
    fn clamp_notice_fires_for_tall_image() {
        // A portrait image without -w: the 297-row cap (5940 px) binds
        // before the 400-row window (8000 px).
        let mut o = opts();
        o.tmux = true;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 400,
            rows: 400,
            px: None,
        };
        let msg = kitty_protocol_clamp_notice((1500, 6000), &o).unwrap();
        assert!(msg.contains("1485x5940"), "{msg}");
        assert!(msg.contains("placeholder cell table"), "{msg}");
    }

    #[test]
    fn clamp_notice_silent_when_terminal_is_the_constraint() {
        // A 200-col window (2000 px) is narrower than the 2970 px protocol
        // cap: the same bounds apply either way, so no protocol notice.
        let mut o = opts();
        o.tmux = true;
        o.cell = CellPx { w: 10, h: 20 };
        o.win = WinSize {
            cols: 200,
            rows: 400,
            px: None,
        };
        o.width = Some(3500);
        assert_eq!(kitty_protocol_clamp_notice((3000, 100), &o), None);
    }

    #[test]
    fn clamp_notice_silent_within_grid() {
        let o = opts();
        assert_eq!(kitty_protocol_clamp_notice((100, 50), &o), None);
    }

    #[test]
    fn hidpi_px_report_doubles_grid_cell() {
        let mut o = opts();
        o.win.px = Some((1440, 864));
        // cell probe said logical 9x18, window px says 18x36 physical: the
        // physical one must win so the placeholder grid matches the image.
        assert_eq!(kitty_cell(&o), (18, 36));
        // A physically-probed cell wins over a logical px report too.
        o.cell = CellPx { w: 19, h: 38 };
        assert_eq!(kitty_cell(&o), (19, 38));
    }

    #[test]
    fn bitmap_bounds_stay_logical_on_hidpi() {
        // Without a px report both bounds formulas degenerate to the same
        // logical grid...
        let mut o = opts();
        assert_eq!(bitmap_bounds(&o), kitty_bounds(&o));
        assert_eq!(bitmap_bounds(&o), (720, 432));
        // ...but on HiDPI only the KITTY bounds double: OSC 1337 and Sixel
        // render one image pixel per logical point, so their bounds must
        // stay at the logical cell (9x18), not the physical one (18x36).
        o.win.px = Some((1440, 864));
        assert_eq!(kitty_bounds(&o), (1440, 864));
        assert_eq!(bitmap_bounds(&o), (720, 432));
    }

    #[test]
    fn halfblock_bounds_are_logical() {
        let o = opts();
        assert_eq!(halfblock_bounds(&o), (720, 864));
    }

    #[test]
    fn target_dims_matches_target_px() {
        // The header-driven path must agree with the decoded-image wrapper.
        let mut o = opts();
        o.width = Some(800);
        o.win = WinSize {
            cols: 200,
            rows: 50,
            px: None,
        }; // max 1800x900
        let bounds = kitty_bounds(&o);
        assert_eq!(
            target_px(&img(100, 50), &o, bounds),
            target_dims(100, 50, &o, bounds)
        );
        assert_eq!(target_dims(100, 50, &o, bounds), (800, 400));
    }

    #[test]
    fn target_dims_width_upscales_and_caps() {
        let mut o = opts();
        o.width = Some(800);
        o.win = WinSize {
            cols: 200,
            rows: 50,
            px: None,
        };
        assert_eq!(target_dims(100, 50, &o, kitty_bounds(&o)), (800, 400));
        // A huge width request is capped by the terminal bounds (width-bound here).
        o.width = Some(5000);
        assert_eq!(target_dims(2000, 1000, &o, kitty_bounds(&o)), (1800, 900));
    }

    #[test]
    fn target_dims_natural_size_and_bounds() {
        let o = opts();
        let b = kitty_bounds(&o); // 720x432
        assert_eq!(target_dims(982, 548, &o, b), (720, 402));
        assert_eq!(target_dims(2000, 1000, &o, b), (720, 360));
        // Small images keep their native size (never upscaled by the bounds).
        assert_eq!(target_dims(100, 50, &o, b), (100, 50));
    }

    #[test]
    fn target_dims_swapped_for_90_rotation() {
        // A 4000x3000 raw JPEG displayed with Rotate90 becomes 3000x4000; the
        // target must be computed from the ORIENTED dims (portrait), not the
        // raw landscape grid.
        let mut o = opts();
        o.width = Some(720);
        let b = (100_000u64, 100_000u64); // unbounded so width dominates
        let (tw, th) = target_dims(3000, 4000, &o, b); // oriented dims
        assert_eq!((tw, th), (720, 960)); // portrait aspect preserved
        assert_eq!(target_dims(4000, 3000, &o, b), (720, 540)); // landscape
    }

    #[test]
    fn without_width_capped_at_default_max_width() {
        // Even with a huge terminal, previews without -w never exceed 1920 px
        // wide; smaller images keep their native size.
        let o = opts();
        let b = (100_000u64, 100_000u64);
        assert_eq!(target_dims(4000, 2000, &o, b), (1920, 960));
        assert_eq!(target_dims(1920, 1080, &o, b), (1920, 1080));
        assert_eq!(target_dims(800, 400, &o, b), (800, 400));
    }

    #[test]
    fn default_width_cap_yields_to_window() {
        // The window width always wins: min(window, 1920).
        let mut o = opts(); // window bounds 720x432
        let b = kitty_bounds(&o);
        assert_eq!(target_dims(4000, 2000, &o, b), (720, 360));
        // Explicit -w is also still capped by the window.
        o.width = Some(10_000);
        assert_eq!(target_dims(1000, 500, &o, b), (720, 360));
        // ...and window height matters for tall images under the cap.
        o.width = None;
        assert_eq!(target_dims(1500, 4000, &o, b), (162, 432));
    }

    #[test]
    fn quality_tiers_map_to_filters() {
        use image::imageops::FilterType as F;
        assert_eq!(filter(Quality::Low), F::Nearest);
        assert_eq!(filter(Quality::Medium), F::Triangle);
        assert_eq!(filter(Quality::High), F::Lanczos3);
        assert_eq!(Quality::default(), Quality::Medium);
        assert_eq!(Quality::parse("l"), Some(Quality::Low));
        assert_eq!(Quality::parse("H"), Some(Quality::High));
        assert_eq!(Quality::parse(" x "), None);
        assert_eq!(Quality::parse("80"), None);
    }
}
