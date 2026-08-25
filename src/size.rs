use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

/// Images without explicit DPI are assumed to be 72 dpi (1:1 pixels).
const REF_DPI: f64 = 72.0;

pub struct RenderOpts {
    pub width: Option<u32>,
    pub quality: u8,
    pub cell: CellPx,
    pub win: WinSize,
    pub dpi: Option<f64>,
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
    let max_w = (o.win.cols as u64 * o.cell.w as u64).max(1);
    let max_h = (o.win.rows as u64 * o.cell.h as u64).max(1);
    let (tw, th) = match o.width {
        Some(w) => contain(w.max(1) as u64, (ih * w.max(1) as u64 / iw).max(1), max_w, max_h),
        None => {
            // natural size, corrected for the image's physical density
            let s = match o.dpi {
                Some(d) if d > 0.0 => REF_DPI / d,
                _ => 1.0,
            };
            let tw = ((iw as f64 * s).round() as u64).max(1);
            let th = ((ih as f64 * s).round() as u64).max(1);
            contain(tw, th, max_w, max_h)
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
            dpi: None,
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
        // max 720x432, contain 2000x1000 -> scale 0.36 -> 720x360
        assert_eq!(target_px(&img(2000, 1000), &o), (720, 360));
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
    fn high_dpi_shrinks_natural_size() {
        let mut o = opts();
        o.dpi = Some(144.0);
        assert_eq!(target_px(&img(1000, 500), &o), (500, 250));
    }

    #[test]
    fn low_dpi_grows_natural_size() {
        let mut o = opts();
        o.dpi = Some(36.0);
        assert_eq!(target_px(&img(100, 50), &o), (200, 100));
    }

    #[test]
    fn explicit_width_overrides_dpi() {
        let mut o = opts();
        o.dpi = Some(144.0);
        o.width = Some(800);
        o.win = WinSize { cols: 200, rows: 50 };
        assert_eq!(target_px(&img(100, 50), &o), (800, 400));
    }

    #[test]
    fn dpi_scaled_still_capped_to_terminal() {
        let mut o = opts();
        o.dpi = Some(36.0); // x2 -> 2000x2000 from 1000x1000
        // max 720x432 -> scale 0.216 -> 432x432
        assert_eq!(target_px(&img(1000, 1000), &o), (432, 432));
    }

    #[test]
    fn zero_dpi_treated_as_identity() {
        let mut o = opts();
        o.dpi = Some(0.0);
        assert_eq!(target_px(&img(300, 200), &o), (300, 200));
    }
}
