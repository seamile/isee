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
        img.resize(cells_w, px_h, size::filter(o.quality))
            .into_rgba8()
    };

    let mut out =
        String::with_capacity(cells_w as usize * cells_h as usize * 40 + cells_h as usize);
    use std::fmt::Write as _;
    for cy in 0..cells_h {
        for cx in 0..cells_w {
            let top = pixel(&rgba, cx, cy * 2);
            let bot = pixel(&rgba, cx, cy * 2 + 1);
            let (tr, tg, tb) = blend(top);
            let (br, bg, bb) = blend(bot);
            write!(
                out,
                "\x1b[38;2;{tr};{tg};{tb}m\x1b[48;2;{br};{bg};{bb}m\u{2580}"
            )
            .unwrap();
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

    // ---- render() output ----

    fn opts() -> RenderOpts {
        RenderOpts {
            width: None,
            quality: size::Quality::default(),
            cell: crate::detect::CellPx { w: 9, h: 18 },
            win: crate::detect::WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            dpy_scale: 1,
            tmux: false,
            transfer: crate::size::KgpTransfer::Stream,
        }
    }

    /// A 1x2 image maps to exactly one cell (cells_w=1 == width, px_h=2 ==
    /// height), so render() must take the no-resize shortcut and pass pixel
    /// values through untouched.
    fn img_1x2(top: [u8; 4], bot: [u8; 4]) -> DynamicImage {
        DynamicImage::ImageRgba8(RgbaImage::from_fn(1, 2, |_, y| {
            if y == 0 {
                image::Rgba(top)
            } else {
                image::Rgba(bot)
            }
        }))
    }

    #[test]
    fn render_native_grid_passes_colors_through_verbatim() {
        let out = render(&img_1x2([255, 0, 0, 255], [0, 0, 255, 255]), &opts());
        assert_eq!(
            out, "\x1b[38;2;255;0;0m\x1b[48;2;0;0;255m\u{2580}\x1b[0m\n",
            "top pixel drives fg, bottom drives bg, one half-block char"
        );
    }

    #[test]
    fn render_blends_alpha_into_fg_and_bg() {
        // Semi-transparent white darkens toward black on the foreground;
        // a fully transparent bottom becomes a black background.
        let out = render(&img_1x2([255, 255, 255, 128], [10, 20, 30, 0]), &opts());
        assert_eq!(
            out,
            "\x1b[38;2;128;128;128m\x1b[48;2;0;0;0m\u{2580}\x1b[0m\n"
        );
    }

    #[test]
    fn render_multicell_layout_and_row_terminators() {
        // 18x36 px at cell 9x18 targets cells_w=2, px_h=4 (resize branch:
        // grid != source dims). Output is 2 rows of 2 half-blocks, each row
        // reset and terminated with LF.
        let img = DynamicImage::new_rgba8(18, 36);
        let out = render(&img, &opts());
        let lines: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "two cell rows, got {out:?}");
        for line in &lines {
            assert!(
                line.starts_with("\x1b[38;2;"),
                "row must open SGR fg: {line:?}"
            );
            assert!(
                line.ends_with("\x1b[0m"),
                "row must end with reset: {line:?}"
            );
            assert_eq!(line.matches('\u{2580}').count(), 2, "row {line:?}");
        }
        assert_eq!(out.matches('\u{2580}').count(), 4);
        assert!(out.ends_with('\n'));
    }

    #[test]
    fn render_caps_grid_to_window_cells() {
        // 1440x1728 fits no terminal of 720x864 px bounds: it scales down so
        // the placeholder/character grid lands exactly on 80 cols x 48 rows
        // and can never wrap past the window edge.
        let o = opts(); // 80x24 cells, halfblock bounds 720x864
        let img = DynamicImage::new_rgba8(1440, 1728);
        let out = render(&img, &o);
        let lines: Vec<&str> = out.split('\n').filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 48, "cell rows");
        for line in &lines {
            assert_eq!(line.matches('\u{2580}').count(), 80, "row {line:?}");
        }
        assert!(out.ends_with("\x1b[0m\n"));
    }
}
