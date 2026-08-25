use image::{DynamicImage, RgbaImage};

use crate::size::{self, RenderOpts};

pub fn render(img: &DynamicImage, o: &RenderOpts) -> String {
    let (tw, th) = size::target_px(img, o, size::halfblock_bounds(o));
    let cells_w = tw.div_ceil(o.cell.w.max(1)).max(1);
    let cells_h = th.div_ceil(o.cell.h.max(1)).max(1);
    let px_h = cells_h * 2;

    let rgba = if cells_w == img.width() && px_h == img.height() {
        img.to_rgba8()
    } else {
        img.resize(cells_w, px_h, size::filter(o.quality)).to_rgba8()
    };

    let mut out = String::with_capacity(cells_w as usize * cells_h as usize * 40 + cells_h as usize);
    use std::fmt::Write as _;
    for cy in 0..cells_h {
        for cx in 0..cells_w {
            let top = pixel(&rgba, cx, cy * 2);
            let bot = pixel(&rgba, cx, cy * 2 + 1);
            let (tr, tg, tb) = blend(top);
            let (br, bg, bb) = blend(bot);
            write!(out, "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}").unwrap();
        }
        out.push_str("\x1b[0m\n");
    }
    out
}

fn pixel(img: &RgbaImage, x: u32, y: u32) -> (u8, u8, u8, u8) {
    let p = img.get_pixel(x.min(img.width() - 1), y.min(img.height() - 1));
    (p[0], p[1], p[2], p[3])
}

fn blend(c: (u8, u8, u8, u8)) -> (u8, u8, u8) {
    let a = c.3 as u32;
    (
        ((c.0 as u32 * a + 127) / 255) as u8,
        ((c.1 as u32 * a + 127) / 255) as u8,
        ((c.2 as u32 * a + 127) / 255) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blend_opaque() {
        assert_eq!(blend((255, 0, 0, 255)), (255, 0, 0));
    }

    #[test]
    fn blend_half_alpha() {
        assert_eq!(blend((255, 255, 255, 128)), (128, 128, 128));
    }

    #[test]
    fn blend_transparent() {
        assert_eq!(blend((10, 20, 30, 0)), (0, 0, 0));
    }

    #[test]
    fn pixel_clamps() {
        let img = RgbaImage::new(2, 2);
        assert_eq!(pixel(&img, 99, 99), (0, 0, 0, 0));
    }
}
