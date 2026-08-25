use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

/// Scaling bounds for the Kitty protocol, in DEVICE pixels. The graphics
/// protocol's s=/v= are physical pixels, so on HiDPI (DPR=2) screens a
/// logical-pixel bound would render the image at half size.
///
/// Sources are combined with max() so either one being physical wins even if
/// the other reports logical units (terminal-dependent):
///   - TIOCGWINSZ window pixels (`win.px`), already DPI-scaled on
///     kitty/Ghostty/iTerm2
///   - the probed/fallback cell size times the grid (cols*cell.w)
pub fn kitty_bounds(o: &RenderOpts) -> (u64, u64) {
    let cw = (o.win.cols as u64 * o.cell.w.max(1) as u64).max(1);
    let ch = (o.win.rows as u64 * o.cell.h.max(1) as u64).max(1);
    match o.win.px {
        Some((pw, ph)) => (cw.max(pw as u64), ch.max(ph as u64)),
        None => (cw, ch),
    }
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

pub struct RenderOpts {
    pub width: Option<u32>,
    pub quality: u8,
    pub cell: CellPx,
    pub win: WinSize,
}

pub fn filter(quality: u8) -> image::imageops::FilterType {
    if quality >= 50 {
        image::imageops::FilterType::Lanczos3
    } else {
        image::imageops::FilterType::Triangle
    }
}

pub fn target_px(img: &DynamicImage, o: &RenderOpts, bounds: (u64, u64)) -> (u32, u32) {
    let (iw, ih) = img.dimensions();
    let iw = iw.max(1) as u64;
    let ih = ih.max(1) as u64;
    let (tw, th) = match o.width {
        Some(w) => contain(w.max(1) as u64, (ih * w.max(1) as u64 / iw).max(1), bounds.0, bounds.1),
        // No explicit width: display at the image's native pixel size (ignore
        // DPI), shrinking only to fit the bounds.
        None => contain(iw, ih, bounds.0, bounds.1),
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
            quality: 50,
            cell: CellPx { w: 9, h: 18 },
            win: WinSize { cols: 80, rows: 24, px: None },
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
        assert_eq!(target_px(&img(2000, 1000), &o, kitty_bounds(&o)), (720, 360));
    }

    #[test]
    fn width_request_allows_upscale() {
        let mut o = opts();
        o.width = Some(800);
        o.win = WinSize { cols: 200, rows: 50, px: None }; // max 1800x900
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
        o.win = WinSize { cols: 200, rows: 50, px: None };
        assert_eq!(target_px(&img(100, 50), &o, kitty_bounds(&o)), (100, 50));
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (982, 548));
    }

    #[test]
    fn without_width_capped_to_window() {
        let o = opts();
        // max 720x432: 982x548 -> scale 720/982 -> 720x402; 2000x1000 -> 720x360.
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (720, 402));
        assert_eq!(target_px(&img(2000, 1000), &o, kitty_bounds(&o)), (720, 360));
    }

    #[test]
    fn hidpi_px_report_doubles_bounds() {
        // Retina: 80x24 grid, window reports 1440x864 device px. Bounds must
        // use the larger physical size, not the logical 720x432.
        let mut o = opts();
        o.win.px = Some((1440, 864));
        assert_eq!(kitty_bounds(&o), (1440, 864));
        // 982x548 now fits natively; a 2000x1000 image scales to 1440x720.
        assert_eq!(target_px(&img(982, 548), &o, kitty_bounds(&o)), (982, 548));
        assert_eq!(target_px(&img(2000, 1000), &o, kitty_bounds(&o)), (1440, 720));
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
    fn halfblock_bounds_are_logical() {
        let o = opts();
        assert_eq!(halfblock_bounds(&o), (720, 864));
    }
}
