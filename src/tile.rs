use std::error::Error;
use std::io::Write;

use image::{DynamicImage, RgbaImage};
use unicode_width::UnicodeWidthStr;

use crate::detect::{Protocol, TerminalInfo};
use crate::input::{self, Loaded, Source};
use crate::size::{self, Bounds, RenderOpts};

const TILE_GAP_COLS: u32 = 1;

struct Layout {
    count: u32,
    cell_cols: u32,
    side: u32,
    gap_px: u32,
    width: u32,
}

impl Layout {
    fn new(opts: &RenderOpts, protocol: Protocol, count: u32) -> Result<Self, Box<dyn Error>> {
        if count == 0 {
            return Err("tile count must be positive".into());
        }
        let (cw, ch) = if protocol == Protocol::HalfBlocks {
            (opts.cell.w.max(1), opts.cell.h.max(1))
        } else {
            size::kitty_cell(opts)
        };
        let max_side = size::MAX_TARGET_DIMENSION as u32;
        let mut cols = opts.win.cols.max(1).min(max_side / cw);
        let mut height_cap = max_side;
        if protocol == Protocol::Kitty && opts.tmux {
            cols = cols.min(crate::kitty::MAX_PLACEHOLDER_CELLS as u32);
            height_cap = u64::from(height_cap).min(size::kitty_grid_cap_px(opts).1) as u32;
        }
        if cols == 0 || height_cap < cw {
            return Err("terminal cell exceeds the tile size limit".into());
        }
        let max_count = cols
            .saturating_add(TILE_GAP_COLS)
            .div_euclid(TILE_GAP_COLS + 1)
            .max(1);
        let count = count.min(max_count);
        let gaps = count.saturating_sub(1) * TILE_GAP_COLS;
        let cell_cols = (cols.saturating_sub(gaps) / count).min(height_cap / cw);
        let side = cell_cols * cw;
        let gap_px = TILE_GAP_COLS * cw;
        let width = (cell_cols * count + gaps) * cw;
        let rows = side.div_ceil(ch);
        input::check_preview_alloc(width, side, u64::from(width) * u64::from(side) * 4)?;
        if protocol == Protocol::HalfBlocks {
            let grid_width = cell_cols * count + gaps;
            input::check_preview_alloc(
                grid_width,
                rows * 2,
                u64::from(grid_width) * u64::from(rows) * 8,
            )?;
        }
        Ok(Self {
            count,
            cell_cols,
            side,
            gap_px,
            width,
        })
    }

    fn bounds(&self) -> Bounds {
        Bounds {
            w: u64::from(self.side),
            h: u64::from(self.side),
            h_cap: u64::from(self.side),
        }
    }
}

pub fn preview(
    sources: &[Source],
    term: &TerminalInfo,
    opts: &RenderOpts,
    count: u32,
    crop: Option<[u32; 4]>,
    out: &mut dyn Write,
) -> Result<(), Box<dyn Error>> {
    let layout = Layout::new(opts, term.protocol, count)?;
    if layout.count < count {
        eprintln!(
            "isee: tile count limited to {} by the terminal width or protocol limit",
            layout.count
        );
    }
    let load_opts = RenderOpts {
        width: None,
        dpy_scale: 1,
        ..opts.clone()
    };
    let render_opts = RenderOpts {
        width: Some(layout.width),
        ..load_opts.clone()
    };
    let mut failed = 0;
    let mut sources = sources.iter();
    loop {
        let mut canvas = None;
        let mut row_height = 0;
        let mut titles = String::new();
        let mut col = 0;
        while col < layout.count {
            let Some(source) = sources.next() else { break };
            let path = crate::display_source(source);
            let loaded = input::load(source, &load_opts, layout.bounds(), false, crop);
            if let Err(e) = &loaded
                && e.is::<input::NonImage>()
            {
                eprintln!("isee: {path}: {e}");
                continue;
            }
            let canvas = canvas.get_or_insert_with(|| RgbaImage::new(layout.width, layout.side));
            if col > 0 {
                titles.push_str(&" ".repeat(TILE_GAP_COLS as usize));
            }
            titles.push_str(&title(&path, layout.cell_cols as usize));
            match loaded {
                Ok(Loaded::Static(img)) => {
                    row_height = row_height.max(place(canvas, &img, col, &layout, opts));
                }
                Ok(Loaded::Anim(_)) => unreachable!("animation is disabled for tiles"),
                Err(e) => {
                    eprintln!("isee: {path}: {e}");
                    failed += 1;
                }
            }
            col += 1;
        }
        let Some(canvas) = canvas else { break };
        let block = if row_height == 0 {
            Vec::new()
        } else {
            let canvas = DynamicImage::ImageRgba8(canvas).crop_imm(0, 0, layout.width, row_height);
            render_row(&canvas, term, &render_opts, &layout)
        };
        out.write_all(titles.as_bytes())?;
        out.write_all(b"\r\n")?;
        out.write_all(&block)?;
        out.flush()?;
    }
    if failed > 0 {
        return Err(crate::failure_summary(failed).into());
    }
    Ok(())
}

fn place(
    canvas: &mut RgbaImage,
    img: &DynamicImage,
    col: u32,
    layout: &Layout,
    opts: &RenderOpts,
) -> u32 {
    let img = img.resize(layout.side, layout.side, size::filter(opts.quality));
    let height = img.height();
    let x = col * (layout.side + layout.gap_px);
    image::imageops::overlay(canvas, &img.to_rgba8(), i64::from(x), 0);
    height
}

fn render_row(
    img: &DynamicImage,
    term: &TerminalInfo,
    opts: &RenderOpts,
    layout: &Layout,
) -> Vec<u8> {
    if term.protocol == Protocol::HalfBlocks {
        let gaps = layout.count.saturating_sub(1) * TILE_GAP_COLS;
        let rows = img.height().div_ceil(opts.cell.h.max(1));
        let mut grid = RgbaImage::new(layout.cell_cols * layout.count + gaps, rows * 2);
        for col in 0..layout.count {
            // Sample tiles separately so the filter cannot bleed into an empty neighbor.
            let tile = img
                .crop_imm(
                    col * (layout.side + layout.gap_px),
                    0,
                    layout.side,
                    img.height(),
                )
                .resize_exact(layout.cell_cols, rows * 2, size::filter(opts.quality))
                .into_rgba8();
            image::imageops::replace(
                &mut grid,
                &tile,
                i64::from(col * (layout.cell_cols + TILE_GAP_COLS)),
                0,
            );
        }
        crate::halfblock::render_grid(&grid).into_bytes()
    } else {
        crate::render_block(img, term, opts)
    }
}

fn title(path: &str, cols: usize) -> String {
    let mut text = String::new();
    for ch in path.chars() {
        if ch.is_control() {
            text.extend(ch.escape_default());
        } else {
            text.push(ch);
        }
    }
    if text.width() > cols {
        while text.width() > cols.saturating_sub(1) {
            text.pop();
        }
        text.push('…');
    }
    text.push_str(&" ".repeat(cols.saturating_sub(text.width())));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{CellPx, WinSize};
    use crate::size::{KgpTransfer, Quality};

    fn opts() -> RenderOpts {
        RenderOpts {
            width: None,
            quality: Quality::Low,
            cell: CellPx { w: 10, h: 20 },
            win: WinSize {
                cols: 192,
                rows: 24,
                px: Some((1920, 480)),
            },
            dpy_scale: 1,
            tmux: false,
            transfer: KgpTransfer::Stream,
        }
    }

    fn term(protocol: Protocol, opts: &RenderOpts) -> TerminalInfo {
        TerminalInfo {
            protocol,
            cell: opts.cell,
            win: opts.win,
            tmux: opts.tmux,
            dpy_scale: opts.dpy_scale,
            probed_scale: None,
            brand: None,
            forced: true,
            kgp_transfer: opts.transfer,
        }
    }

    #[test]
    fn square_areas_use_terminal_width_not_height() {
        let layout = Layout::new(&opts(), Protocol::Kitty, 3).unwrap();
        assert_eq!((layout.count, layout.cell_cols), (3, 63));
        assert_eq!((layout.width, layout.side), (1910, 630));
        for protocol in [Protocol::Kitty, Protocol::Iip, Protocol::Sixel] {
            let o = RenderOpts {
                width: Some(layout.width),
                ..opts()
            };
            let bounds = if protocol == Protocol::Kitty {
                size::kitty_bounds(&o)
            } else {
                size::bitmap_bounds(&o)
            };
            assert_eq!(
                size::target_dims(layout.width, layout.side, &o, bounds),
                (layout.width, layout.side)
            );
        }
    }

    #[test]
    fn columns_align_to_cells_and_hidpi() {
        let mut o = opts();
        o.win.cols = 191;
        o.win.px = Some((3820, 960));
        let layout = Layout::new(&o, Protocol::Iip, 3).unwrap();
        assert_eq!(
            (layout.cell_cols, layout.width, layout.side),
            (63, 3820, 1260)
        );
        let layout = Layout::new(&o, Protocol::HalfBlocks, 3).unwrap();
        assert_eq!(
            (layout.cell_cols, layout.width, layout.side),
            (63, 1910, 630)
        );
    }

    #[test]
    fn excessive_counts_and_tmux_limits_are_bounded() {
        let o = opts();
        let layout = Layout::new(&o, Protocol::Kitty, u32::MAX).unwrap();
        assert_eq!((layout.count, layout.cell_cols, layout.side), (96, 1, 10));
        let mut o = o;
        o.tmux = true;
        o.win.cols = 1000;
        o.win.px = None;
        let layout = Layout::new(&o, Protocol::Kitty, 1).unwrap();
        assert_eq!((layout.width, layout.side), (2970, 2970));
        assert!(
            layout.side.div_ceil(size::kitty_cell(&o).1) as usize
                <= crate::kitty::MAX_PLACEHOLDER_CELLS
        );
        o.cell.h = 1;
        let layout = Layout::new(&o, Protocol::Kitty, 1).unwrap();
        assert_eq!(layout.side, 290);
    }

    #[test]
    fn invalid_and_oversized_layouts_fail_before_allocating() {
        assert!(Layout::new(&opts(), Protocol::Kitty, 0).is_err());
        let mut o = opts();
        o.cell.w = u32::MAX;
        assert!(Layout::new(&o, Protocol::Kitty, 1).is_err());
        o.cell.w = 10;
        o.win.cols = 1200;
        o.win.px = None;
        let err = Layout::new(&o, Protocol::Kitty, 1).err().unwrap();
        assert!(err.to_string().contains("memory limit"));
    }

    #[test]
    fn one_column_terminal_still_has_a_usable_tile() {
        let mut o = opts();
        o.win = WinSize {
            cols: 1,
            rows: 1,
            px: None,
        };
        let layout = Layout::new(&o, Protocol::HalfBlocks, 3).unwrap();
        assert_eq!((layout.count, layout.cell_cols, layout.side), (1, 1, 10));
    }

    #[test]
    fn output_failure_stops_preview_with_the_original_io_error() {
        struct Closed;
        impl Write for Closed {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                unreachable!("failed output must not be flushed")
            }
        }
        let source = Source::Path(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/anim.webp"),
        );
        let o = opts();
        let err = preview(
            &[source],
            &term(Protocol::HalfBlocks, &o),
            &o,
            3,
            None,
            &mut Closed,
        )
        .unwrap_err();
        assert_eq!(
            err.downcast_ref::<std::io::Error>().unwrap().kind(),
            std::io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn failed_images_keep_titles_without_a_square_image_row() {
        let source = Source::Path(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/missing-tile-row.png"),
        );
        let path = crate::display_source(&source);
        let o = opts();
        let layout = Layout::new(&o, Protocol::HalfBlocks, 3).unwrap();
        let mut out = Vec::new();
        assert!(
            preview(
                &[source],
                &term(Protocol::HalfBlocks, &o),
                &o,
                3,
                None,
                &mut out,
            )
            .is_err()
        );
        assert_eq!(
            String::from_utf8(out).unwrap(),
            format!("{}\r\n", title(&path, layout.cell_cols as usize))
        );
    }

    #[test]
    fn images_enlarge_and_align_to_the_top_left_without_cropping() {
        let o = RenderOpts {
            quality: Quality::Low,
            ..opts()
        };
        let layout = Layout {
            count: 3,
            cell_cols: 1,
            side: 8,
            gap_px: 1,
            width: 26,
        };
        let red = image::Rgba([255, 0, 0, 255]);
        let wide = DynamicImage::ImageRgba8(RgbaImage::from_pixel(4, 2, red));
        let tall = DynamicImage::ImageRgba8(RgbaImage::from_pixel(2, 4, red));
        let mut canvas = RgbaImage::new(26, 8);
        assert_eq!(place(&mut canvas, &wide, 0, &layout, &o), 4);
        assert_eq!(place(&mut canvas, &tall, 1, &layout, &o), 8);
        assert_eq!(canvas.get_pixel(0, 0), &red);
        assert_eq!(canvas.get_pixel(7, 3), &red);
        assert_eq!(canvas.get_pixel(0, 4)[3], 0);
        assert_eq!(canvas.get_pixel(8, 0)[3], 0);
        assert_eq!(canvas.get_pixel(9, 0), &red);
        assert_eq!(canvas.get_pixel(12, 7), &red);
        assert_eq!(canvas.get_pixel(13, 0)[3], 0);
        assert_eq!(canvas.get_pixel(25, 7)[3], 0);
    }

    #[test]
    fn titles_truncate_by_display_columns_and_escape_controls() {
        assert_eq!(title("a.png", 8), "a.png   ");
        assert_eq!(title("long-name.png", 5), "long…");
        assert_eq!(title("图片.png", 5), "图片…");
        assert_eq!(title("图片", 1), "…");
        assert_eq!(title("e\u{301}.png", 5), "e\u{301}.png");
        assert_eq!(title("👩‍💻.png", 6), "👩‍💻.png");
        assert_eq!(title("a\nb\t\x1b", 20), "a\\nb\\t\\u{1b}        ");
        for name in ["图片.png", "e\u{301}.png", "👩‍💻.png", "🇨🇳.png"] {
            for cols in 1..12 {
                assert_eq!(title(name, cols).width(), cols, "{name}: {cols}");
            }
        }
    }

    #[test]
    fn rendered_rows_preserve_dimensions_on_all_protocols() {
        let mut o = opts();
        o.win = WinSize {
            cols: 6,
            rows: 1,
            px: None,
        };
        for protocol in [
            Protocol::Kitty,
            Protocol::Iip,
            Protocol::Sixel,
            Protocol::HalfBlocks,
        ] {
            let layout = Layout::new(&o, protocol, 3).unwrap();
            let img = DynamicImage::ImageRgba8(RgbaImage::new(layout.width, 5));
            let tile = DynamicImage::ImageRgba8(RgbaImage::from_pixel(
                1,
                1,
                image::Rgba([255, 255, 255, 255]),
            ));
            let mut img_buf = img.to_rgba8();
            for col in 0..layout.count {
                place(&mut img_buf, &tile, col, &layout, &o);
            }
            let img = DynamicImage::ImageRgba8(img_buf);
            let row_opts = RenderOpts {
                width: Some(layout.width),
                ..o
            };
            let block = render_row(&img, &term(protocol, &row_opts), &row_opts, &layout);
            let text = String::from_utf8_lossy(&block);
            match protocol {
                Protocol::Kitty => assert!(text.contains("a=T,s=50,v=5,")),
                Protocol::Iip => assert!(text.contains("width=50px;height=5px:")),
                Protocol::Sixel => assert!(text.contains("\"1;1;50;5")),
                Protocol::HalfBlocks => {
                    assert_eq!(text.matches('▀').count(), 3);
                    assert_eq!(text.matches(' ').count(), 2);
                    assert_eq!(text.matches('\n').count(), 1);
                }
            }
        }
        o.tmux = true;
        let layout = Layout::new(&o, Protocol::Kitty, 3).unwrap();
        let img = DynamicImage::ImageRgba8(RgbaImage::new(layout.width, layout.side));
        let row_opts = RenderOpts {
            width: Some(layout.width),
            ..o
        };
        let block = render_row(&img, &term(Protocol::Kitty, &row_opts), &row_opts, &layout);
        assert!(block.starts_with(b"\x1bPtmux;"));
    }
}
