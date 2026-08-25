use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};

use image::DynamicImage;

use crate::size::{self, RenderOpts};

static NEXT_ID: AtomicU32 = AtomicU32::new(0);

pub fn new_image_id() -> u32 {
    let pid = std::process::id() & 0xffff;
    (pid << 16) | NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

pub fn render(img: &DynamicImage, o: &RenderOpts, id: u32) -> Vec<u8> {
    let (tw, th) = size::target_px(img, o);
    let resized = img.resize(tw, th, size::filter(o.quality));
    let rgba = resized.to_rgba8();
    let (w, h) = rgba.dimensions();
    let cols = w.div_ceil(o.cell.w.max(1)).max(1);
    let rows = h.div_ceil(o.cell.h.max(1)).max(1);

    let b64 = base64_encode(rgba.as_raw());
    const CHUNK: usize = 4096;
    let total = b64.len().div_ceil(CHUNK);
    let mut out: Vec<u8> = Vec::with_capacity(b64.len() + 64 * total);
    for (i, chunk) in b64.as_bytes().chunks(CHUNK).enumerate() {
        let more = i + 1 < total;
        if i == 0 {
            write!(
                out,
                "\x1b_Ga=T,f=32,s={w},v={h},c={cols},r={rows},i={id},q=2,m={m};",
                m = if more { 1 } else { 0 }
            )
            .unwrap();
        } else {
            out.extend_from_slice(b"\x1b_Gm=");
            out.push(if more { b'1' } else { b'0' });
            out.push(b';');
        }
        out.extend_from_slice(chunk);
        out.extend_from_slice(b"\x1b\\");
    }
    out
}

#[allow(dead_code)]
pub fn clear_all() -> Vec<u8> {
    b"\x1b_Ga=d,d=A\x1b\\".to_vec()
}

fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[n as usize & 63] as char } else { '=' });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_vectors() {
        assert_eq!(base64_encode(b""), "");
        assert_eq!(base64_encode(b"f"), "Zg==");
        assert_eq!(base64_encode(b"fo"), "Zm8=");
        assert_eq!(base64_encode(b"foo"), "Zm9v");
        assert_eq!(base64_encode(b"foob"), "Zm9vYg==");
    }

    #[test]
    fn single_chunk_control() {
        let img = DynamicImage::new_rgba8(2, 1);
        let o = RenderOpts {
            width: None,
            quality: 50,
            cell: crate::detect::CellPx { w: 9, h: 18 },
            win: crate::detect::WinSize { cols: 80, rows: 24 },
            dpi: None,
        };
        let out = render(&img, &o, 42);
        let s = String::from_utf8_lossy(&out);
        assert!(s.starts_with("\x1b_Ga=T,f=32,s=2,v=1,c=1,r=1,i=42,q=2,m=0;"), "got {s}");
        assert!(s.ends_with("\x1b\\"));
    }
}
