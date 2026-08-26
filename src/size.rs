use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

/// Scaling bounds for the Kitty protocol, in DEVICE pixels: the physical
/// cell times the grid. Deriving the bounds and the placeholder grid from
/// the SAME cell guarantees `ceil(w/cell) <= cols`, so an ultra-wide image
/// can never wrap its last placeholder column onto the next line.
///
/// The raw window-pixel report (`win.px`) is deliberately NOT used as a
/// bound: on Ghostty it includes window padding, which can exceed the grid
/// by a fraction of a cell and push the grid past the last column.
pub fn kitty_bounds(o: &RenderOpts) -> (u64, u64) {
    let (cw, ch) = kitty_cell(o);
    (
        (o.win.cols as u64 * cw as u64).max(1),
        (o.win.rows as u64 * ch as u64).max(1),
    )
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
            win: WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
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
}
