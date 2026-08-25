use image::{DynamicImage, GenericImageView};

use crate::detect::{CellPx, WinSize};

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
    let max_w = (o.win.cols as u64 * o.cell.w as u64).max(1);
    let max_h = (o.win.rows as u64 * o.cell.h as u64).max(1);
    let (tw, th) = match o.width {
        Some(w) => {
            let mut tw = (w.max(1)) as u64;
            let mut th = (ih * tw / iw).max(1);
            if tw > max_w || th > max_h {
                let s = (max_w as f64 / tw as f64).min(max_h as f64 / th as f64);
                tw = ((tw as f64 * s).round() as u64).max(1);
                th = ((th as f64 * s).round() as u64).max(1);
            }
            (tw, th)
        }
        None => {
            if iw <= max_w && ih <= max_h {
                (iw, ih)
            } else {
                let s = (max_w as f64 / iw as f64).min(max_h as f64 / ih as f64);
                (((iw as f64 * s).round() as u64).max(1), ((ih as f64 * s).round() as u64).max(1))
            }
        }
    };
    (tw as u32, th as u32)
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
}
