use std::io::Write as _;

use image::{DynamicImage, GenericImageView};
use palette::{Srgb, cast::ComponentsAs};
use quantette::{
    PaletteSize,
    color_map::IndexedColorMap,
    wu::{BinnerU8x3, WuU8x3},
};

use crate::size::{self, RenderOpts};

/// Render one image as a raw Sixel DCS frame (`ESC P9;1q ... ESC \`) followed
/// by the CRLFs that park the cursor below it. Encoding mirrors yazi's sixel
/// driver: Wu-based quantization to at most 256 colors (255 on alpha images,
/// where palette entry 0 is reserved for fully transparent pixels), one
/// band per 6 pixel rows with `$`/`-` cursor moves and `!`-run RLE.
pub fn render(img: &DynamicImage, o: &RenderOpts) -> Vec<u8> {
    let (tw, th) = size::target_px(img, o, size::bitmap_bounds(o));
    let img = if tw == img.width() && th == img.height() {
        std::borrow::Cow::Borrowed(img)
    } else {
        std::borrow::Cow::Owned(img.resize(tw, th, size::filter(o.quality)))
    };
    let alpha = img.color().has_alpha();

    let qo = match &*img {
        DynamicImage::ImageRgb8(rgb) => quantify(rgb, false),
        _ => quantify(&img.to_rgb8(), alpha),
    }
    .expect("sixel quantize");

    let mut out = Vec::new();
    write!(out, "\x1bP9;1q\"1;1;{};{}", img.width(), img.height()).unwrap();

    // Palette: scale each component to Sixel's 0..100 range. On alpha images
    // entries are numbered from 1; index 0 stays black and absorbs fully
    // transparent pixels.
    for (i, c) in qo.palette.iter().enumerate() {
        write!(
            out,
            "#{};2;{};{};{}",
            i + alpha as usize,
            c.red as u16 * 100 / 255,
            c.green as u16 * 100 / 255,
            c.blue as u16 * 100 / 255
        )
        .unwrap();
    }

    let (w, h) = (img.width() as usize, img.height() as usize);
    for y in 0..h {
        // One Sixel band per pixel row; the bit plane bit rotates every 6 rows.
        let c = (b'?' + (1 << (y % 6))) as char;

        let mut last = 0u8;
        let mut repeat = 0usize;
        for x in 0..w {
            let idx = if img.get_pixel(x as u32, y as u32)[3] == 0 {
                0
            } else {
                qo.indices[y * w + x] + alpha as u8
            };

            if idx == last || repeat == 0 {
                (last, repeat) = (idx, repeat + 1);
                continue;
            }

            if repeat > 1 {
                write!(out, "#{last}!{repeat}{c}").unwrap();
            } else {
                write!(out, "#{last}{c}").unwrap();
            }

            (last, repeat) = (idx, 1);
        }

        if repeat > 1 {
            write!(out, "#{last}!{repeat}{c}").unwrap();
        } else {
            write!(out, "#{last}{c}").unwrap();
        }

        // Carriage return within the band...
        out.push(b'$');
        // ...and a band break every 6 rows.
        if y % 6 == 5 {
            out.push(b'-');
        }
    }
    out.extend_from_slice(b"\x1b\\");

    // Some terminals move below the image on their own; emitting one CRLF per
    // LOGICAL cell row (same rule as Iip — Sixel pixels are rendered one per
    // logical point) may add one blank line there, but is the only way to
    // keep the prompt clear on the terminals that do not.
    let ch = o.cell.h;
    let rows = (h as f64 / ch.max(1) as f64).ceil().max(1.0) as u32;
    for _ in 0..rows {
        out.extend_from_slice(b"\r\n");
    }
    out
}

struct QuantizeOutput {
    indices: Vec<u8>,
    palette: Vec<Srgb<u8>>,
}

fn quantify(rgb: &image::RgbImage, alpha: bool) -> Result<QuantizeOutput, String> {
    let buf = &rgb.as_raw()[..(rgb.pixels().len() * 3)];
    let colors: &[Srgb<u8>] = buf.components_as();

    let wu = WuU8x3::run_slice(colors, BinnerU8x3::rgb()).map_err(|e| e.to_string())?;
    let color_map =
        wu.color_map(PaletteSize::try_from(256u16 - alpha as u16).map_err(|e| e.to_string())?);

    Ok(QuantizeOutput {
        indices: color_map.map_to_indices(colors),
        palette: color_map.into_palette().into_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::detect::{CellPx, WinSize};
    use image::{Rgb, Rgba};

    fn opts() -> RenderOpts {
        RenderOpts {
            width: None,
            quality: size::Quality::default(),
            cell: CellPx { w: 9, h: 18 },
            win: WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            dpy_scale: 1,
        }
    }

    #[test]
    fn opaque_red_frame_structure() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 6, Rgb([255, 0, 0])));
        let out = render(&img, &opts());
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("\x1bP9;1q\"1;1;1;6"), "got {s}");
        assert!(s.contains("#0;2;100;0;0"), "pure red must hit 100: {s}");
        assert!(s.contains('$'), "band carriage return missing: {s}");
        assert!(s.contains("\x1b\\"), "ST terminator missing: {s}");
        assert!(s.ends_with("\r\n"));
    }

    #[test]
    fn alpha_image_reserves_index_zero() {
        let mut rgba = image::RgbaImage::new(2, 1);
        rgba.put_pixel(0, 0, Rgba([255, 0, 0, 255]));
        rgba.put_pixel(1, 0, Rgba([0, 0, 255, 0]));
        let out = render(&DynamicImage::ImageRgba8(rgba), &opts());
        let s = String::from_utf8_lossy(&out);
        // Palette numbering starts at 1; #0 has no definition entry.
        assert!(s.contains("#1;2;"), "palette must start at #1: {s}");
        assert!(!s.contains("#0;2;"), "index 0 must stay undefined: {s}");
    }

    #[test]
    fn bands_split_every_six_rows() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(1, 13, Rgb([10, 200, 30])));
        let out = render(&img, &opts());
        let s = String::from_utf8_lossy(&out);
        assert!(s.matches('-').count() >= 2, "expected >=2 band breaks: {s}");
    }

    #[test]
    fn repeated_colors_rle_compressed() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(20, 1, Rgb([7, 7, 250])));
        let out = render(&img, &opts());
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("!20"), "run of 20 must use an RLE marker: {s}");
    }

    #[test]
    fn transparent_pixels_use_reserved_index() {
        let mut rgba = image::RgbaImage::new(4, 1);
        rgba.put_pixel(0, 0, Rgba([50, 60, 70, 0]));
        rgba.put_pixel(1, 0, Rgba([50, 60, 70, 0]));
        rgba.put_pixel(2, 0, Rgba([50, 60, 70, 0]));
        rgba.put_pixel(3, 0, Rgba([50, 60, 70, 0]));
        let out = render(&DynamicImage::ImageRgba8(rgba), &opts());
        let s = String::from_utf8_lossy(&out);
        // All-transparent run maps to palette index 0 regardless of quantized
        // colors; row-0 plane char is '?' + (1 << 0) = '@'.
        assert!(s.contains("#0!4@"), "got {s}");
    }

    #[test]
    fn trailing_newline_parks_cursor_below() {
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(3, 3, Rgb([1, 2, 3])));
        let out = render(&img, &opts());
        assert!(out.ends_with(b"\r\n"));
        // HiDPI: the row count must divide by the logical cell (10), not
        // kitty_cell's doubled height (36) — a 20px image spans 2 rows.
        let mut o = opts();
        o.cell = CellPx { w: 5, h: 10 };
        o.win.px = Some((720, 864));
        let img = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(4, 20, Rgb([1, 2, 3])));
        let out = render(&img, &o);
        assert_eq!(
            String::from_utf8_lossy(&out).matches("\r\n").count(),
            2,
            "2 logical cell rows expected"
        );
    }
}
