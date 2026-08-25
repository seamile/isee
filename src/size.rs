use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

/// No explicit width: only shrink when the image exceeds this many pixels on
/// either edge, so images are shown at their native size like the system
/// Preview app and don't get scaled down to the terminal's cell grid.
const MAX_PIXEL: u64 = 1200;

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

pub fn target_px(img: &DynamicImage, o: &RenderOpts) -> (u32, u32) {
    let (iw, ih) = img.dimensions();
    let iw = iw.max(1) as u64;
    let ih = ih.max(1) as u64;
    let (tw, th) = match o.width {
        Some(w) => {
            let max_w = (o.win.cols as u64 * o.cell.w as u64).max(1);
            let max_h = (o.win.rows as u64 * o.cell.h as u64).max(1);
            contain(w.max(1) as u64, (ih * w.max(1) as u64 / iw).max(1), max_w, max_h)
        }
        None => {
            // Display at the image's native pixel size (ignore DPI); shrink
            // only when it exceeds the generous MAX_PIXEL ceiling.
            contain(iw, ih, MAX_PIXEL, MAX_PIXEL)
        }
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
            win: WinSize { cols: 80, rows: 24 },
        }
    }

    fn img(w: u32, h: u32) -> DynamicImage {
        DynamicImage::new_rgba8(w, h)
    }

    #[test]
    fn fits_within_terminal_without_upscale() {
        let o = opts();
        assert_eq!(target_px(&img(100, 50), &o), (100, 50));
    }

    #[test]
    fn scales_down_to_fit() {
        let o = opts();
        // max 1200x1200, contain 2000x1000 -> scale 0.6 -> 1200x600
        assert_eq!(target_px(&img(2000, 1000), &o), (1200, 600));
    }

    #[test]
    fn width_request_allows_upscale() {
        let mut o = opts();
        o.width = Some(800);
        o.win = WinSize { cols: 200, rows: 50 }; // max 1800x900
        assert_eq!(target_px(&img(100, 50), &o), (800, 400));
    }

    #[test]
    fn width_request_capped_to_terminal() {
        let mut o = opts();
        o.width = Some(5000);
        // max 720x432, image 100x100 -> uniform scale 432/5000 -> 432x432
        assert_eq!(target_px(&img(100, 100), &o), (432, 432));
    }

    #[test]
    fn without_width_keeps_natural_pixel_size() {
        let o = opts();
        // Fits within MAX_PIXEL ceiling: shown at native size, never upscaled
        // or shrunk by DPI or the terminal cell grid.
        assert_eq!(target_px(&img(100, 50), &o), (100, 50));
        assert_eq!(target_px(&img(982, 548), &o), (982, 548));
    }

    #[test]
    fn natural_size_capped_to_ceiling() {
        let o = opts();
        // MAX_PIXEL 1200x1200, image 1000x1000 fits -> native size.
        assert_eq!(target_px(&img(1000, 1000), &o), (1000, 1000));
        // Exceeds one edge (2000x1000) -> uniform scale 0.6 -> 1200x600.
        assert_eq!(target_px(&img(2000, 1000), &o), (1200, 600));
    }
}
