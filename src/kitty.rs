use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

use flate2::Compression;
use flate2::write::ZlibEncoder;
use image::DynamicImage;
use image::metadata::LoopCount;

use crate::b64::base64_encode;
use crate::input::AnimFrame;
use crate::size::{self, KgpTransfer, RenderOpts};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

/// zlib-compressed KGP transfer, ON by default, opt out via
/// `ISEE_KGP_COMPRESS=0` (also `false`/`no`/`off`, case-insensitive): raw
/// RGBA payloads (~2.9 MB for a 900x600 image) dominate the wall time a
/// terminal spends consuming frames — on Ghostty, compression cut a
/// 103-image batch from ~31 s to ~18 s for ~3 s extra CPU. `o=z` asks the
/// terminal to zlib-decompress the payload after transfer; the format stays
/// `f=32`, so `s`/`v` and the placeholder grid are untouched. Stream
/// transport only — tempfile transfers cross the pty as a path, with
/// nothing to compress.
fn kgp_compress_enabled(v: Option<&str>) -> bool {
    !matches!(
        v,
        Some(s)
            if s == "0"
                || s.eq_ignore_ascii_case("false")
                || s.eq_ignore_ascii_case("no")
                || s.eq_ignore_ascii_case("off")
    )
}

/// Kitty's Unicode-placeholder mechanism matches a cell to an image by the
/// cell's foreground color, which encodes only 24 bits. The id must therefore
/// stay strictly below `0xffffff` (mirroring yazi's `% (0xffffff + 1)`).
pub fn new_image_id() -> u32 {
    let pid = std::process::id() & 0xffffff;
    let n = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    (pid + n) % 0xffffff
}

pub fn render(img: &DynamicImage, o: &RenderOpts, id: u32) -> Vec<u8> {
    let compress = kgp_compress_enabled(std::env::var("ISEE_KGP_COMPRESS").ok().as_deref());
    render_with(img, o, id, compress)
}

fn render_with(img: &DynamicImage, o: &RenderOpts, id: u32, compress: bool) -> Vec<u8> {
    let (tw, th) = size::target_px(img, o, size::kitty_bounds(o));
    let rgba = if tw == img.width() && th == img.height() {
        img.to_rgba8()
    } else {
        img.resize(tw, th, size::filter(o.quality)).into_rgba8()
    };
    if o.tmux {
        // Inside tmux the outer terminal's placement moves are invisible to
        // the pane's cursor model (a following prompt would be painted on
        // top of the image), so anchor the bitmap to a placeholder grid
        // instead; icat does the same (it force-enables placeholders in
        // tmux). Byte-for-byte the pre-direct-placement output.
        let (w, h) = rgba.dimensions();
        // Grid of cells that will hold the placeholder; only anchors
        // placement, it does NOT re-scale the image (no c= / r= in the
        // control sequence). The cell here is the DEVICE-pixel cell so the
        // grid matches the image's physical size on HiDPI screens (logical
        // cells would double the grid).
        let (cw, ch) = size::kitty_cell(o);
        let cols = (w as f64 / cw as f64).ceil().max(1.0) as u32;
        let rows = (h as f64 / ch as f64).ceil().max(1.0) as u32;
        let mut out = encode(&rgba, w, h, id, compress);
        place(&mut out, cols, rows, id);
        out
    } else {
        // Direct placement, icat-style: transmit with `a=T` and let the
        // terminal draw the bitmap at its declared device-pixel size (auto-
        // fitting oversize images). A leading CR guards against a mid-line
        // cursor, and a trailing CR LF parks the cursor at column 0 of the
        // line BELOW the image — after a C=0 placement kitty leaves the
        // cursor on the image's last row just right of the bitmap
        // (`cursor.x += num_cols; cursor.y += num_rows - 1`, verified in
        // kitty's graphics.c and with a live `get-text --add-cursor` probe),
        // so without the CRLF the next output would overwrite the image's
        // bottom line.
        let mut out = Vec::with_capacity(rgba.as_raw().len() / 3 * 4 + 128);
        out.push(b'\r');
        let (w, h) = rgba.dimensions();
        emit_frame(
            &mut out,
            &format!("a=T,s={w},v={h},i={id},q=2"),
            "",
            &rgba,
            id,
            o,
            compress,
        );
        out.extend_from_slice(b"\r\n");
        out
    }
}

/// Render an animated GIF for kitty. Outside tmux: the first composited
/// frame is transmitted as the animation's root image (`a=T`, direct
/// placement), every remaining frame follows as an `a=f` full-canvas frame,
/// playback is armed with `a=a` controls mirroring icat (gap for the root,
/// `s=2` loading mode once the first extra frame has arrived, `s=3` start
/// after the last). Inside tmux the root is anchored with the placeholder
/// grid and the sequence stays byte-for-byte the pre-direct-placement one.
/// The root frame's gap defaults to zero, so it is set explicitly (`r=1`,
/// the root is frame 1) before the start control.
pub fn render_animation(
    frames: &[AnimFrame],
    loop_count: LoopCount,
    o: &RenderOpts,
    id: u32,
) -> Vec<u8> {
    let compress = kgp_compress_enabled(std::env::var("ISEE_KGP_COMPRESS").ok().as_deref());
    render_animation_with(frames, loop_count, o, id, compress)
}

fn render_animation_with(
    frames: &[AnimFrame],
    loop_count: LoopCount,
    o: &RenderOpts,
    id: u32,
    compress: bool,
) -> Vec<u8> {
    // decode_gif already resized every frame to the shared preview target,
    // so target_px on the first frame is idempotent: no per-frame resize.
    let first = frames[0].img.to_rgba8();
    let (w, h) = first.dimensions();
    // s=3 runs the animation normally; v=1 loops infinitely and v=N plays
    // N-1 times, so a GIF asking for n loops maps to n+1.
    let v = match loop_count {
        LoopCount::Infinite => 1,
        LoopCount::Finite(n) => n.get().saturating_add(1),
    };
    if o.tmux {
        let (cw, ch) = size::kitty_cell(o);
        let cols = (w as f64 / cw as f64).ceil().max(1.0) as u32;
        let rows = (h as f64 / ch as f64).ceil().max(1.0) as u32;
        let mut out = encode(&first, w, h, id, compress);
        place(&mut out, cols, rows, id);
        write!(
            out,
            "\x1b_Ga=a,i={id},q=2,r=1,z={}\x1b\\",
            frames[0].delay_ms
        )
        .unwrap();
        write!(out, "\x1b_Ga=a,i={id},q=2,s=3,v={v}\x1b\\").unwrap();
        // The canvas of every frame is fully composited (GIF frames arrive
        // as complete pictures), so `X=1` (simple overwrite) reproduces it
        // exactly; the default alpha blend would double-blend semi-
        // transparent pixels.
        let opts = if compress { ",o=z" } else { "" };
        for frame in &frames[1..] {
            let b64 = z64(&frame.img.to_rgba8(), compress);
            write_chunked(
                &mut out,
                &b64,
                &format!(
                    "a=f,f=32,s={w},v={h},i={id},q=2,X=1{opts},z={}",
                    frame.delay_ms
                ),
                "a=f",
            );
        }
        return out;
    }
    let mut out = Vec::new();
    out.push(b'\r');
    emit_frame(
        &mut out,
        &format!("a=T,s={w},v={h},i={id},q=2"),
        "",
        &first,
        id,
        o,
        compress,
    );
    write!(
        out,
        "\x1b_Ga=a,i={id},q=2,r=1,z={}\x1b\\",
        frames[0].delay_ms
    )
    .unwrap();
    // The canvas of every frame is fully composited (GIF frames arrive as
    // complete pictures), so `X=1` (simple overwrite) reproduces it exactly;
    // the default alpha blend would double-blend semi-transparent pixels.
    for (n, frame) in frames[1..].iter().enumerate() {
        let f = frame.img.to_rgba8();
        emit_frame(
            &mut out,
            &format!("a=f,s={w},v={h},i={id},q=2,X=1,z={}", frame.delay_ms),
            "a=f",
            &f,
            id,
            o,
            compress,
        );
        if n == 0 {
            // icat order: once the first extra frame has arrived, switch the
            // animation to loading mode (s=2) so playback starts only when
            // every frame is in.
            write!(out, "\x1b_Ga=a,i={id},q=2,s=2\x1b\\").unwrap();
        }
    }
    write!(out, "\x1b_Ga=a,i={id},q=2,s=3,v={v}\x1b\\").unwrap();
    // Park the cursor below the root image (same C=0 placement semantics as
    // the static path: the a=a / a=f controls never move the cursor).
    out.extend_from_slice(b"\r\n");
    out
}

fn encode(rgba: &image::RgbaImage, w: u32, h: u32, id: u32, compress: bool) -> Vec<u8> {
    let b64 = z64(rgba, compress);
    let opts = if compress { ",o=z" } else { "" };
    let mut out = Vec::with_capacity(b64.len() + 64 * b64.len().div_ceil(CHUNK_BYTES));
    write_chunked(
        &mut out,
        &b64,
        &format!("a=T,C=1,U=1,f=32,s={w},v={h},i={id},q=2{opts}"),
        "",
    );
    out
}

/// The wire payload for one image/frame: fully opaque bitmaps go as `f=24`
/// RGB (a third smaller, verified against icat's `IsOpaque` branch), anything
/// with alpha as `f=32` RGBA. Returns the format code and raw bytes.
fn payload(rgba: &image::RgbaImage) -> (u32, Vec<u8>) {
    if rgba.pixels().all(|p| p.0[3] == 255) {
        let mut rgb = Vec::with_capacity(rgba.width() as usize * rgba.height() as usize * 3);
        for px in rgba.pixels() {
            rgb.extend_from_slice(&px.0[..3]);
        }
        (24, rgb)
    } else {
        (32, rgba.as_raw().clone())
    }
}

/// Transmit one image/frame payload for the direct-placement path. `keys`/
/// `cont` are the control keys of the opening and continuation blocks (e.g.
/// `a=T,s=800,v=600,i=42,q=2` with empty `cont`). The format comes from
/// `payload` (opaque → f=24 RGB, alpha → f=32 RGBA) and the transport from
/// `o.transfer`: a temp file whose PATH alone crosses the pty (`t=t`, kitty
/// reads the pixels and deletes the file), or the chunked pty stream,
/// optionally zlib-compressed (`o=z`). A temp file that cannot be created
/// falls back to the stream.
fn emit_frame(
    out: &mut Vec<u8>,
    keys: &str,
    cont: &str,
    rgba: &image::RgbaImage,
    id: u32,
    o: &RenderOpts,
    compress: bool,
) {
    let (fmt, bytes) = payload(rgba);
    if o.transfer == KgpTransfer::Tempfile {
        // The name must carry kitty's `tty-graphics-protocol` marker or kitty
        // will not delete the file after reading it (graphics.c gates the
        // unlink on that substring), leaking one file per image. The letter
        // form `t=t` is mandatory: kitty silently drops the numeric `t=1`
        // (verified live — no response, file never read), while `t=t`
        // answers OK and deletes the file after reading it.
        let path = std::env::temp_dir().join(format!("kitty-tty-graphics-protocol-isee-{id}.rgb"));
        if std::fs::write(&path, &bytes).is_ok() {
            let b64 = base64_encode(path.to_string_lossy().as_bytes());
            write!(out, "\x1b_G{keys},f={fmt},t=t;{b64}\x1b\\").unwrap();
            return;
        }
    }
    stream_frame(out, keys, cont, fmt, &bytes, compress);
}

fn stream_frame(out: &mut Vec<u8>, keys: &str, cont: &str, fmt: u32, bytes: &[u8], compress: bool) {
    let b64 = if compress {
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(bytes).unwrap();
        base64_encode(&z.finish().unwrap())
    } else {
        base64_encode(bytes)
    };
    let opts = if compress { ",o=z" } else { "" };
    write_chunked(out, &b64, &format!("{keys},f={fmt}{opts}"), cont);
}

const CHUNK_BYTES: usize = 4096;

/// zlib-compressed (or raw) RGBA payload as base64, shared by the static and
/// animation transfer paths.
fn z64(rgba: &image::RgbaImage, compress: bool) -> String {
    if compress {
        // Speed-first: the point is fewer pty bytes, not a minimal archive.
        let mut z = ZlibEncoder::new(Vec::new(), Compression::fast());
        z.write_all(rgba.as_raw()).unwrap();
        base64_encode(&z.finish().unwrap())
    } else {
        base64_encode(rgba.as_raw())
    }
}

/// Split `b64` into escape-code payload blocks of at most 4096 bytes, writing
/// `first` as the control keys of the opening block and re-issuing the keys
/// in `cont` on every later block (empty means the bare `m=` continuation).
fn write_chunked(out: &mut Vec<u8>, b64: &str, first: &str, cont: &str) {
    let total = b64.len().div_ceil(CHUNK_BYTES);
    for (i, chunk) in b64.as_bytes().chunks(CHUNK_BYTES).enumerate() {
        let more = if i + 1 < total { 1 } else { 0 };
        if i == 0 {
            write!(out, "\x1b_G{first},m={more};").unwrap();
        } else if cont.is_empty() {
            write!(out, "\x1b_Gm={more};").unwrap();
        } else {
            write!(out, "\x1b_G{cont},m={more};").unwrap();
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
}

/// Anchor the transmitted image to a grid of terminal cells. The foreground
/// color's 24-bit RGB value is set to the image id so the terminal associates
/// these cells with the image.
///
/// `cols`/`rows` must stay within `MAX_PLACEHOLDER_CELLS` (enforced by
/// `size::kitty_bounds`): the diacritics table cannot address larger offsets,
/// and a past-the-end fallback to diacritic 0 used to render everything right
/// of column 296 as garbage. Indexing directly panics on violation instead.
fn place(out: &mut Vec<u8>, cols: u32, rows: u32, id: u32) {
    let (r, g, b) = ((id >> 16) & 0xff, (id >> 8) & 0xff, id & 0xff);
    write!(out, "\x1b[38;2;{r};{g};{b}m").unwrap();
    // Emit the grid as plain text lines separated by CRLF. CR guarantees each
    // row restarts at column 1 (a bare LF keeps the end column and shifts
    // subsequent rows right, fragmenting the image into horizontal bands),
    // while letting the terminal scroll naturally like any CLI output keeps
    // the shell prompt exactly one line below the image with no blank gap.
    // yazi instead positions every row with an absolute MoveTo because a TUI
    // must never scroll.
    for y in 0..rows {
        if y > 0 {
            out.extend_from_slice(b"\r\n");
        }
        let dy = DIACRITICS[y as usize];
        for x in 0..cols {
            let dx = DIACRITICS[x as usize];
            for ch in std::iter::once('\u{10EEEE}')
                .chain(std::iter::once(dy))
                .chain(std::iter::once(dx))
            {
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
        }
    }
    // Reset attributes, then park the cursor on the line below the image so
    // the shell prompt never overwrites the last placeholder row.
    out.extend_from_slice(b"\x1b[0m");
    out.extend_from_slice(b"\r\n");
}

/// Placeholder cells addressable per axis: a cell's row/column offset is
/// encoded as one combining diacritic drawn from kitty's
/// `rowcolumn-diacritics.txt` (297 entries, values 0..296), so no placeholder
/// can express an offset past the table's end. `size::kitty_bounds` clamps
/// the grid to this; emitting more cells would need an out-of-table mark.
pub(crate) const MAX_PLACEHOLDER_CELLS: usize = DIACRITICS.len();

/// Combining marks that vary each placeholder cell so the terminal does not
/// merge adjacent private-use placeholder characters into a single glyph.
/// Mirrors the DIACRITICS table from yazi's kgp driver, which is kitty's
/// `rowcolumn-diacritics.txt`.
static DIACRITICS: [char; 297] = [
    '\u{0305}',
    '\u{030D}',
    '\u{030E}',
    '\u{0310}',
    '\u{0312}',
    '\u{033D}',
    '\u{033E}',
    '\u{033F}',
    '\u{0346}',
    '\u{034A}',
    '\u{034B}',
    '\u{034C}',
    '\u{0350}',
    '\u{0351}',
    '\u{0352}',
    '\u{0357}',
    '\u{035B}',
    '\u{0363}',
    '\u{0364}',
    '\u{0365}',
    '\u{0366}',
    '\u{0367}',
    '\u{0368}',
    '\u{0369}',
    '\u{036A}',
    '\u{036B}',
    '\u{036C}',
    '\u{036D}',
    '\u{036E}',
    '\u{036F}',
    '\u{0483}',
    '\u{0484}',
    '\u{0485}',
    '\u{0486}',
    '\u{0487}',
    '\u{0592}',
    '\u{0593}',
    '\u{0594}',
    '\u{0595}',
    '\u{0597}',
    '\u{0598}',
    '\u{0599}',
    '\u{059C}',
    '\u{059D}',
    '\u{059E}',
    '\u{059F}',
    '\u{05A0}',
    '\u{05A1}',
    '\u{05A8}',
    '\u{05A9}',
    '\u{05AB}',
    '\u{05AC}',
    '\u{05AF}',
    '\u{05C4}',
    '\u{0610}',
    '\u{0611}',
    '\u{0612}',
    '\u{0613}',
    '\u{0614}',
    '\u{0615}',
    '\u{0616}',
    '\u{0617}',
    '\u{0657}',
    '\u{0658}',
    '\u{0659}',
    '\u{065A}',
    '\u{065B}',
    '\u{065D}',
    '\u{065E}',
    '\u{06D6}',
    '\u{06D7}',
    '\u{06D8}',
    '\u{06D9}',
    '\u{06DA}',
    '\u{06DB}',
    '\u{06DC}',
    '\u{06DF}',
    '\u{06E0}',
    '\u{06E1}',
    '\u{06E2}',
    '\u{06E4}',
    '\u{06E7}',
    '\u{06E8}',
    '\u{06EB}',
    '\u{06EC}',
    '\u{0730}',
    '\u{0732}',
    '\u{0733}',
    '\u{0735}',
    '\u{0736}',
    '\u{073A}',
    '\u{073D}',
    '\u{073F}',
    '\u{0740}',
    '\u{0741}',
    '\u{0743}',
    '\u{0745}',
    '\u{0747}',
    '\u{0749}',
    '\u{074A}',
    '\u{07EB}',
    '\u{07EC}',
    '\u{07ED}',
    '\u{07EE}',
    '\u{07EF}',
    '\u{07F0}',
    '\u{07F1}',
    '\u{07F3}',
    '\u{0816}',
    '\u{0817}',
    '\u{0818}',
    '\u{0819}',
    '\u{081B}',
    '\u{081C}',
    '\u{081D}',
    '\u{081E}',
    '\u{081F}',
    '\u{0820}',
    '\u{0821}',
    '\u{0822}',
    '\u{0823}',
    '\u{0825}',
    '\u{0826}',
    '\u{0827}',
    '\u{0829}',
    '\u{082A}',
    '\u{082B}',
    '\u{082C}',
    '\u{082D}',
    '\u{0951}',
    '\u{0953}',
    '\u{0954}',
    '\u{0F82}',
    '\u{0F83}',
    '\u{0F86}',
    '\u{0F87}',
    '\u{135D}',
    '\u{135E}',
    '\u{135F}',
    '\u{17DD}',
    '\u{193A}',
    '\u{1A17}',
    '\u{1A75}',
    '\u{1A76}',
    '\u{1A77}',
    '\u{1A78}',
    '\u{1A79}',
    '\u{1A7A}',
    '\u{1A7B}',
    '\u{1A7C}',
    '\u{1B6B}',
    '\u{1B6D}',
    '\u{1B6E}',
    '\u{1B6F}',
    '\u{1B70}',
    '\u{1B71}',
    '\u{1B72}',
    '\u{1B73}',
    '\u{1CD0}',
    '\u{1CD1}',
    '\u{1CD2}',
    '\u{1CDA}',
    '\u{1CDB}',
    '\u{1CE0}',
    '\u{1DC0}',
    '\u{1DC1}',
    '\u{1DC3}',
    '\u{1DC4}',
    '\u{1DC5}',
    '\u{1DC6}',
    '\u{1DC7}',
    '\u{1DC8}',
    '\u{1DC9}',
    '\u{1DCB}',
    '\u{1DCC}',
    '\u{1DD1}',
    '\u{1DD2}',
    '\u{1DD3}',
    '\u{1DD4}',
    '\u{1DD5}',
    '\u{1DD6}',
    '\u{1DD7}',
    '\u{1DD8}',
    '\u{1DD9}',
    '\u{1DDA}',
    '\u{1DDB}',
    '\u{1DDC}',
    '\u{1DDD}',
    '\u{1DDE}',
    '\u{1DDF}',
    '\u{1DE0}',
    '\u{1DE1}',
    '\u{1DE2}',
    '\u{1DE3}',
    '\u{1DE4}',
    '\u{1DE5}',
    '\u{1DE6}',
    '\u{1DFE}',
    '\u{20D0}',
    '\u{20D1}',
    '\u{20D4}',
    '\u{20D5}',
    '\u{20D6}',
    '\u{20D7}',
    '\u{20DB}',
    '\u{20DC}',
    '\u{20E1}',
    '\u{20E7}',
    '\u{20E9}',
    '\u{20F0}',
    '\u{2CEF}',
    '\u{2CF0}',
    '\u{2CF1}',
    '\u{2DE0}',
    '\u{2DE1}',
    '\u{2DE2}',
    '\u{2DE3}',
    '\u{2DE4}',
    '\u{2DE5}',
    '\u{2DE6}',
    '\u{2DE7}',
    '\u{2DE8}',
    '\u{2DE9}',
    '\u{2DEA}',
    '\u{2DEB}',
    '\u{2DEC}',
    '\u{2DED}',
    '\u{2DEE}',
    '\u{2DEF}',
    '\u{2DF0}',
    '\u{2DF1}',
    '\u{2DF2}',
    '\u{2DF3}',
    '\u{2DF4}',
    '\u{2DF5}',
    '\u{2DF6}',
    '\u{2DF7}',
    '\u{2DF8}',
    '\u{2DF9}',
    '\u{2DFA}',
    '\u{2DFB}',
    '\u{2DFC}',
    '\u{2DFD}',
    '\u{2DFE}',
    '\u{2DFF}',
    '\u{A66F}',
    '\u{A67C}',
    '\u{A67D}',
    '\u{A6F0}',
    '\u{A6F1}',
    '\u{A8E0}',
    '\u{A8E1}',
    '\u{A8E2}',
    '\u{A8E3}',
    '\u{A8E4}',
    '\u{A8E5}',
    '\u{A8E6}',
    '\u{A8E7}',
    '\u{A8E8}',
    '\u{A8E9}',
    '\u{A8EA}',
    '\u{A8EB}',
    '\u{A8EC}',
    '\u{A8ED}',
    '\u{A8EE}',
    '\u{A8EF}',
    '\u{A8F0}',
    '\u{A8F1}',
    '\u{AAB0}',
    '\u{AAB2}',
    '\u{AAB3}',
    '\u{AAB7}',
    '\u{AAB8}',
    '\u{AABE}',
    '\u{AABF}',
    '\u{AAC1}',
    '\u{FE20}',
    '\u{FE21}',
    '\u{FE22}',
    '\u{FE23}',
    '\u{FE24}',
    '\u{FE25}',
    '\u{FE26}',
    '\u{10A0F}',
    '\u{10A38}',
    '\u{1D185}',
    '\u{1D186}',
    '\u{1D187}',
    '\u{1D188}',
    '\u{1D189}',
    '\u{1D1AA}',
    '\u{1D1AB}',
    '\u{1D1AC}',
    '\u{1D1AD}',
    '\u{1D242}',
    '\u{1D243}',
    '\u{1D244}',
];

#[allow(dead_code)]
pub fn clear_all() -> Vec<u8> {
    b"\x1b_Ga=d,d=A\x1b\\".to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

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
            transfer: KgpTransfer::Stream,
        }
    }

    #[test]
    fn image_id_fits_24_bits() {
        for _ in 0..100 {
            let id = new_image_id();
            assert!(id < 0xffffff, "id {id} exceeds 24 bits");
        }
    }

    #[test]
    fn tmux_wide_grid_never_exceeds_diacritics_table() {
        // A 400-column grid of 10 px cells would need 400 column diacritics,
        // but the table only holds 297 (kitty's rowcolumn-diacritics.txt).
        // The tmux bounds clamp must keep the grid within it; the old
        // fallback to diacritic 0 rendered everything right of column 296 as
        // garbage.
        let mut o = opts();
        o.tmux = true;
        o.win = crate::detect::WinSize {
            cols: 400,
            rows: 40,
            px: None,
        };
        o.cell = crate::detect::CellPx { w: 10, h: 20 };
        o.width = Some(3000);
        // 3000x100 contains exactly to the clamped 2970x800 bounds:
        // scale 0.99 -> 2970x99 -> 297 cols x ceil(99/20)=5 rows.
        let img = image::DynamicImage::new_rgba8(3000, 100);
        let out = render(&img, &o, 42);
        let mut marker = [0u8; 4];
        '\u{10EEEE}'.encode_utf8(&mut marker);
        let placeholders = out.windows(4).filter(|w| *w == marker).count();
        assert_eq!(placeholders, 297 * 5);
    }

    #[test]
    fn direct_wide_image_is_not_clamped() {
        // Non-tmux direct placement has no placeholder grid, so a 3000 px
        // wide request on a 400-col terminal is NOT shrunk to 2970 px.
        let mut o = opts();
        o.tmux = false;
        o.win = crate::detect::WinSize {
            cols: 400,
            rows: 40,
            px: None,
        };
        o.cell = crate::detect::CellPx { w: 10, h: 20 };
        o.width = Some(3000);
        let img = image::DynamicImage::new_rgba8(3000, 100);
        let out = render(&img, &o, 42);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.contains("a=T,s=3000,v=100,i=42,q=2,f=32,"),
            "native size kept, got {s}"
        );
        assert!(!s.contains('\u{10EEEE}'), "no placeholders: {s}");
    }

    #[test]
    fn compress_flag_parsing() {
        // Default is ON; only explicit falsy values opt out.
        assert!(kgp_compress_enabled(None));
        assert!(kgp_compress_enabled(Some("")));
        assert!(kgp_compress_enabled(Some("1")));
        assert!(kgp_compress_enabled(Some("true")));
        assert!(kgp_compress_enabled(Some("TRUE")));
        assert!(!kgp_compress_enabled(Some("0")));
        assert!(!kgp_compress_enabled(Some("false")));
        assert!(!kgp_compress_enabled(Some("False")));
        assert!(!kgp_compress_enabled(Some("no")));
        assert!(!kgp_compress_enabled(Some("off")));
    }

    #[test]
    fn compressed_transfer_declares_o_z_and_keeps_f32() {
        let img = DynamicImage::new_rgba8(2, 1);
        let out = encode(&img.to_rgba8(), 2, 1, 42, true);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=2,v=1,i=42,q=2,o=z,m=0;"),
            "got {s}"
        );
    }

    #[test]
    fn uncompressed_transfer_has_no_o_z() {
        let img = DynamicImage::new_rgba8(2, 1);
        let out = encode(&img.to_rgba8(), 2, 1, 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=2,v=1,i=42,q=2,m=0;"),
            "got {s}"
        );
    }

    #[test]
    fn tmux_control_uses_placeholder_without_cr() {
        let img = DynamicImage::new_rgba8(2, 1);
        let mut o = opts();
        o.tmux = true;
        let out = render_with(&img, &o, 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=2,v=1,i=42,q=2,m=0;"),
            "got {s}"
        );
        assert!(!s.contains("c="), "must not send c=, got {s}");
        assert!(!s.contains("r="), "must not send r=, got {s}");
        assert!(s.contains('\u{10EEEE}'), "must emit placeholder, got {s}");
        assert!(s.ends_with("\x1b[0m\r\n"));
    }

    #[test]
    fn direct_control_places_without_placeholder() {
        // icat-style direct placement: no C=1 (the terminal moves the cursor
        // itself), no U=1/placeholder grid, and a trailing CR LF that parks
        // the cursor below the image.
        let img = DynamicImage::new_rgba8(2, 1);
        let out = render_with(&img, &opts(), 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=2,v=1,i=42,q=2,f=32,m=0;"),
            "got {s}"
        );
        let control = &s[..s.find(';').unwrap()];
        assert!(!control.contains("C="), "{s}");
        assert!(!control.contains("U="), "{s}");
        assert!(!control.contains("c="), "{s}");
        assert!(!control.contains("r="), "{s}");
        assert!(!s.contains('\u{10EEEE}'), "no placeholders: {s}");
        assert!(s.ends_with("\x1b\\\r\n"), "trailing CRLF: {s}");
    }

    #[test]
    fn tmux_hidpi_grid_uses_physical_cell() {
        // Retina: 80x24 grid, window px 1440x864 (device), probed cell 9x18
        // (logical). The placeholder grid must use the physical 18x36 cell so
        // it matches the image's rendered size instead of doubling it.
        let mut o = opts();
        o.tmux = true;
        o.win.px = Some((1440, 864));
        let img = DynamicImage::new_rgba8(982, 548);
        let out = render_with(&img, &o, 42, false);
        let s = String::from_utf8_lossy(&out);
        // Native size fits the 1440x864 bounds; grid ceil(982/18) x ceil(548/36).
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=982,v=548,i=42,q=2,m=1;"),
            "got {s}"
        );
        assert_eq!(s.matches('\u{10EEEE}').count(), 55 * 16);
    }

    #[test]
    fn direct_hidpi_sends_native_pixels_without_grid() {
        let mut o = opts();
        o.win.px = Some((1440, 864));
        let img = DynamicImage::new_rgba8(982, 548);
        let out = render_with(&img, &o, 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=982,v=548,i=42,q=2,f=32,m=1;"),
            "got {s}"
        );
        assert!(!s.contains('\u{10EEEE}'), "{s}");
    }

    #[test]
    fn tmux_placeholder_grid_tracks_image_size() {
        // 2x1 px with 9x18 cell => 1x1 placeholder grid, fg encodes id (0,0,42).
        let mut o = opts();
        o.tmux = true;
        let img = DynamicImage::new_rgba8(2, 1);
        let out = render(&img, &o, 42);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s.matches('\u{10EEEE}').count(), 1);
        assert!(s.contains("\x1b[38;2;0;0;42m"), "got {s}");

        // 36px tall with 18px cell.height => 2 rows => 2 placeholders.
        let img = DynamicImage::new_rgba8(2, 36);
        let out = render(&img, &o, 42);
        let s = String::from_utf8_lossy(&out);
        assert_eq!(s.matches('\u{10EEEE}').count(), 2);
    }

    #[test]
    fn direct_non_whole_cell_image_sends_native_pixel_size() {
        // 400x300 px with 9x18 cell must NOT be snapped to a whole-cell size
        // (396x306 or 405x306): s/v report the real RGBA dimensions.
        let img = DynamicImage::new_rgba8(400, 300);
        let out = render_with(&img, &opts(), 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=400,v=300,i=42,q=2,f=32,m=1;"),
            "got {s}"
        );
        assert!(!s.contains('\u{10EEEE}'), "{s}");
    }

    #[test]
    fn tmux_multiline_placeholder_separates_rows_with_crlf() {
        let mut o = opts();
        o.tmux = true;
        let img = DynamicImage::new_rgba8(2, 36);
        let out = render(&img, &o, 42);
        let s = std::str::from_utf8(&out).unwrap();
        // Rows are plain text lines separated by CRLF, plus one trailing CRLF
        // that parks the cursor below the image; no bare LF and no cursor
        // save/restore positioning.
        assert!(!s.contains("\x1b[s"), "save cursor must not be used: {s}");
        assert!(
            !s.contains("\x1b[u"),
            "restore cursor must not be used: {s}"
        );
        assert_eq!(
            s.matches('\n').count(),
            2,
            "expected 1 separator + 1 trailing"
        );
        for (i, b) in s.bytes().enumerate() {
            if b == b'\n' {
                assert_eq!(s.as_bytes()[i - 1], b'\r', "LF at byte {i} lacks CR");
            }
        }
        assert!(s.ends_with("\x1b[0m\r\n"));
    }

    // ---- direct placement: f=24 for opaque bitmaps ----

    #[test]
    fn direct_opaque_image_uses_rgb_f24() {
        let mut raw = image::RgbaImage::new(2, 1);
        for px in raw.pixels_mut() {
            *px = image::Rgba([10, 20, 30, 255]);
        }
        let img = DynamicImage::ImageRgba8(raw);
        let out = render_with(&img, &opts(), 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=2,v=1,i=42,q=2,f=24,m=0;"),
            "got {s}"
        );
        // Payload is 2*1*3 = 6 RGB bytes -> 8 base64 chars.
        let start = s.find(';').unwrap() + 1;
        let end = s.find("\x1b\\").unwrap();
        assert_eq!(end - start, 8, "{s}");
    }

    #[test]
    fn direct_image_with_alpha_uses_rgba_f32() {
        // new_rgba8 pixels have alpha 0: not opaque, stays f=32.
        let img = DynamicImage::new_rgba8(2, 1);
        let out = render_with(&img, &opts(), 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=2,v=1,i=42,q=2,f=32,m=0;"),
            "got {s}"
        );
    }

    // ---- direct placement: tempfile transport ----

    #[test]
    fn direct_tempfile_transfer_sends_path_only() {
        let mut raw = image::RgbaImage::new(2, 1);
        for px in raw.pixels_mut() {
            *px = image::Rgba([1, 2, 3, 255]);
        }
        let img = DynamicImage::ImageRgba8(raw);
        let mut o = opts();
        o.transfer = KgpTransfer::Tempfile;
        let out = render_with(&img, &o, 42, false);
        let s = String::from_utf8_lossy(&out);
        let path = std::env::temp_dir().join("kitty-tty-graphics-protocol-isee-42.rgb");
        let want = format!(
            "\r\x1b_Ga=T,s=2,v=1,i=42,q=2,f=24,t=t;{}\x1b\\\r\n",
            base64_encode(path.to_string_lossy().as_bytes())
        );
        assert_eq!(s, want, "single block carrying the file path: {s}");
        // The file holds the raw RGB payload; nothing else crossed the pty.
        assert_eq!(std::fs::read(&path).unwrap(), vec![1, 2, 3, 1, 2, 3]);
        let _ = std::fs::remove_file(&path);
    }

    // ---- GIF animation ----

    fn anim_frame(w: u32, h: u32, delay_ms: u32) -> AnimFrame {
        AnimFrame {
            img: DynamicImage::new_rgba8(w, h),
            delay_ms,
        }
    }

    #[test]
    fn tmux_animation_transmits_root_then_frames_and_starts() {
        let frames = [
            anim_frame(8, 4, 70),
            anim_frame(8, 4, 30),
            anim_frame(8, 4, 10),
        ];
        let mut o = opts();
        o.tmux = true;
        let out = render_animation_with(&frames, LoopCount::Infinite, &o, 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=8,v=4,i=42,q=2,m="),
            "root frame first: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=a,i=42,q=2,r=1,z=70\x1b\\"),
            "root frame gap: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=a,i=42,q=2,s=3,v=1\x1b\\"),
            "start control: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,f=32,s=8,v=4,i=42,q=2,X=1,z=30,m=0;"),
            "second frame: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,f=32,s=8,v=4,i=42,q=2,X=1,z=10,m=0;"),
            "third frame: {s}"
        );
        assert_eq!(s.matches("\x1b_Ga=f,f=32").count(), 2);
        // 1 root a=T + 2 a=a controls + 2 a=f frames.
        assert_eq!(s.matches("\x1b_G").count(), 5);
        // 8x4 with a 9x18 cell fits in a single placeholder cell.
        assert_eq!(s.matches('\u{10EEEE}').count(), 1);
        // No loading mode inside tmux: the legacy sequence is byte-identical.
        assert!(!s.contains("s=2"), "{s}");
    }

    #[test]
    fn direct_animation_root_frames_loading_then_start() {
        // icat order: root a=T, gap control, frames with an s=2 loading-mode
        // control right after the first extra frame, s=3 start at the end,
        // and a trailing CR LF parking the cursor below the root image.
        let frames = [
            anim_frame(8, 4, 70),
            anim_frame(8, 4, 30),
            anim_frame(8, 4, 10),
        ];
        let out = render_animation_with(&frames, LoopCount::Infinite, &opts(), 42, false);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=8,v=4,i=42,q=2,f=32,m="),
            "root frame first: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=a,i=42,q=2,r=1,z=70\x1b\\"),
            "root frame gap: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,s=8,v=4,i=42,q=2,X=1,z=30,f=32,m=0;"),
            "second frame: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,s=8,v=4,i=42,q=2,X=1,z=10,f=32,m=0;"),
            "third frame: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=a,i=42,q=2,s=2\x1b\\"),
            "loading mode: {s}"
        );
        assert!(
            s.contains("\x1b_Ga=a,i=42,q=2,s=3,v=1\x1b\\"),
            "start control: {s}"
        );
        assert_eq!(s.matches("\x1b_G").count(), 6);
        // s=2 sits between the first and second a=f frame.
        let first_f = s.find("\x1b_Ga=f").unwrap();
        let s2 = s.find("s=2\x1b\\").unwrap();
        let second_f = s[first_f + 1..].find("\x1b_Ga=f").unwrap() + first_f + 1;
        assert!(first_f < s2 && s2 < second_f, "s=2 after frame 1: {s}");
        assert!(s.ends_with("\x1b\\\r\n"), "{s}");
    }

    #[test]
    fn animation_loop_count_maps_to_kitty_v() {
        // Finite(n) plays n times; kitty v=N plays N-1 times, so n -> n+1.
        let frames = [anim_frame(8, 4, 50), anim_frame(8, 4, 50)];
        let out = render_animation_with(
            &frames,
            LoopCount::Finite(std::num::NonZeroU32::new(2).unwrap()),
            &opts(),
            7,
            false,
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b_Ga=a,i=7,q=2,s=3,v=3\x1b\\"), "{s}");
    }

    #[test]
    fn tmux_animation_loop_count_maps_to_kitty_v() {
        let frames = [anim_frame(8, 4, 50), anim_frame(8, 4, 50)];
        let mut o = opts();
        o.tmux = true;
        let out = render_animation_with(
            &frames,
            LoopCount::Finite(std::num::NonZeroU32::new(2).unwrap()),
            &o,
            7,
            false,
        );
        let s = String::from_utf8_lossy(&out);
        assert!(s.contains("\x1b_Ga=a,i=7,q=2,s=3,v=3\x1b\\"), "{s}");
    }

    #[test]
    fn animation_frame_chunks_carry_a_f_on_continuation() {
        let frames = [anim_frame(40, 20, 50), anim_frame(40, 20, 50)];
        let out = render_animation_with(&frames, LoopCount::Infinite, &opts(), 9, false);
        let s = String::from_utf8_lossy(&out);
        // 40x20 RGBA = 3200 B -> 4268 base64 chars -> two blocks under the
        // 4096-byte cap, and every later block must re-declare a=f.
        assert!(
            s.contains("a=f,s=40,v=20,i=9,q=2,X=1,z=50,f=32,m=1;"),
            "first block: {s}"
        );
        assert!(s.contains("\x1b_Ga=f,m=0;"), "continuation block: {s}");
    }

    #[test]
    fn tmux_animation_compressed_frames_declare_o_z() {
        let frames = [anim_frame(8, 4, 50), anim_frame(8, 4, 50)];
        let mut o = opts();
        o.tmux = true;
        let out = render_animation_with(&frames, LoopCount::Infinite, &o, 11, true);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\x1b_Ga=T,C=1,U=1,f=32,s=8,v=4,i=11,q=2,o=z,m="),
            "{s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,f=32,s=8,v=4,i=11,q=2,X=1,o=z,z=50,m=0;"),
            "{s}"
        );
    }

    #[test]
    fn direct_animation_compressed_frames_declare_o_z() {
        let frames = [anim_frame(8, 4, 50), anim_frame(8, 4, 50)];
        let out = render_animation_with(&frames, LoopCount::Infinite, &opts(), 11, true);
        let s = String::from_utf8_lossy(&out);
        assert!(
            s.starts_with("\r\x1b_Ga=T,s=8,v=4,i=11,q=2,f=32,o=z,m="),
            "{s}"
        );
        assert!(
            s.contains("\x1b_Ga=f,s=8,v=4,i=11,q=2,X=1,z=50,f=32,o=z,m=0;"),
            "{s}"
        );
    }
}
