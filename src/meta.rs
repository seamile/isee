use std::io::Read;

const SRGB_NAME: &str = "sRGB IEC61966-2.1";

#[derive(Debug, Default, Clone)]
pub struct Meta {
    pub dpi: Option<f64>,
    pub profile: Option<String>,
    pub alpha_hint: bool,
}

/// Extract DPI / ICC profile description / alpha hints by scanning raw file bytes.
/// Unknown or malformed structures are silently skipped.
pub fn extract(data: &[u8]) -> Meta {
    let mut meta = Meta::default();
    if data.len() >= 8 && &data[..8] == b"\x89PNG\r\n\x1a\n" {
        png(data, &mut meta);
    } else if data.len() >= 3 && data[0] == 0xFF && data[1] == 0xD8 {
        jpeg(data, &mut meta);
    } else if data.len() >= 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        webp(data, &mut meta);
    } else if data.len() >= 6 && (&data[..6] == b"GIF89a" || &data[..6] == b"GIF87a") {
        gif(data, &mut meta);
    } else if data.len() >= 4 && (&data[..2] == b"II" || &data[..2] == b"MM") {
        tiff(data, &mut meta);
    } else if data.len() >= 2 && &data[..2] == b"BM" {
        bmp(data, &mut meta);
    }
    meta
}

// ---------- PNG ----------

fn png(data: &[u8], m: &mut Meta) {
    let mut srgb = false;
    let mut pos = 8usize;
    while pos + 8 <= data.len() {
        let len = be32(data, pos) as usize;
        if len.checked_add(12).is_none_or(|t| pos + t > data.len()) {
            break;
        }
        let kind = &data[pos + 4..pos + 8];
        let body = pos + 8;
        let payload = &data[body..body + len];
        match kind {
            b"pHYs" if len >= 9 => {
                if payload[8] == 1 {
                    m.dpi = Some(be32(data, body) as f64 * 0.0254);
                }
            }
            b"iCCP" => {
                if let Some(p) = parse_iccp(payload) {
                    m.profile = Some(p);
                }
            }
            b"sRGB" => srgb = true,
            b"tRNS" => m.alpha_hint = true,
            b"IEND" => break,
            _ => {}
        }
        pos = body + len + 4;
    }
    if m.profile.is_none() && srgb {
        m.profile = Some(SRGB_NAME.to_string());
    }
}

fn parse_iccp(payload: &[u8]) -> Option<String> {
    let nul = payload.iter().position(|&b| b == 0)?;
    let mut rest = &payload[nul + 1..];
    let (&method, tail) = rest.split_first()?;
    if method != 0 {
        return None;
    }
    rest = tail;
    let mut out = Vec::new();
    flate2::read::ZlibDecoder::new(rest)
        .read_to_end(&mut out)
        .ok()?;
    icc_desc(&out)
}

// ---------- JPEG ----------

fn jpeg(data: &[u8], m: &mut Meta) {
    let mut icc: Vec<(u8, Vec<u8>)> = Vec::new();
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            break;
        }
        let marker = data[pos + 1];
        pos += 2;
        if matches!(marker, 0xD8 | 0x01 | 0xD9) || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if marker == 0xDA {
            break;
        }
        let seg_len = be16(data, pos) as usize;
        if seg_len < 2 || pos + seg_len > data.len() {
            break;
        }
        let payload = &data[pos + 2..pos + seg_len];
        match marker {
            0xE0 => {
                if let Some(dpi) = jfif_dpi(payload) {
                    m.dpi = Some(dpi);
                }
            }
            0xE1 => {
                if let Some(dpi) = payload.strip_prefix(b"Exif\0\0").and_then(exif_xres) {
                    m.dpi = Some(dpi);
                }
            }
            0xE2 => {
                if let Some(rest) = payload.strip_prefix(b"ICC_PROFILE\0")
                    && rest.len() >= 2
                {
                    icc.push((rest[0], rest[2..].to_vec()));
                }
            }
            _ => {}
        }
        pos += seg_len;
    }
    if !icc.is_empty() {
        icc.sort_by_key(|(seq, _)| *seq);
        let joined: Vec<u8> = icc.into_iter().flat_map(|(_, part)| part).collect();
        if let Some(p) = icc_desc(&joined) {
            m.profile = Some(p);
        }
    }
}

fn jfif_dpi(payload: &[u8]) -> Option<f64> {
    if !payload.starts_with(b"JFIF\0") || payload.len() < 12 {
        return None;
    }
    let x = be16(payload, 8) as f64;
    match payload[7] {
        1 => Some(x),
        2 => Some(x * 2.54),
        // units=0 officially means aspect ratio only, but many encoders
        // (e.g. macOS sips) store the real DPI there; accept plausible values
        0 if x >= 36.0 && x == be16(payload, 10) as f64 => Some(x),
        _ => None,
    }
}

fn exif_xres(tiff: &[u8]) -> Option<f64> {
    if tiff.len() < 8 {
        return None;
    }
    let le = match &tiff[..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let u16_at = |o: usize| -> Option<u16> {
        let b = tiff.get(o..o + 2)?;
        Some(if le {
            u16::from_le_bytes(b.try_into().unwrap())
        } else {
            u16::from_be_bytes(b.try_into().unwrap())
        })
    };
    let u32_at = |o: usize| -> Option<u32> {
        let b = tiff.get(o..o + 4)?;
        Some(if le {
            u32::from_le_bytes(b.try_into().unwrap())
        } else {
            u32::from_be_bytes(b.try_into().unwrap())
        })
    };
    if u16_at(2)? != 42 {
        return None;
    }
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    let mut unit: u16 = 2;
    let mut xres: Option<f64> = None;
    let mut sub: Option<usize> = None;
    for i in 0..count {
        let e = ifd + 2 + i * 12;
        match u16_at(e)? {
            282 => {
                let off = u32_at(e + 8)? as usize;
                let num = u32_at(off)?;
                let den = u32_at(off + 4)?;
                if den != 0 {
                    xres = Some(num as f64 / den as f64);
                }
            }
            296 => unit = u16_at(e + 8)?,
            // Exif sub-IFD pointer: XResolution often lives there
            0x8769 => sub = Some(u32_at(e + 8)? as usize),
            _ => {}
        }
    }
    if xres.is_none()
        && let Some(s) = sub
    {
        let scount = u16_at(s)? as usize;
        for i in 0..scount {
            let e = s + 2 + i * 12;
            match u16_at(e)? {
                282 => {
                    let off = u32_at(e + 8)? as usize;
                    let num = u32_at(off)?;
                    let den = u32_at(off + 4)?;
                    if den != 0 {
                        xres = Some(num as f64 / den as f64);
                    }
                }
                296 => unit = u16_at(e + 8)?,
                _ => {}
            }
        }
    }
    let x = xres?;
    match unit {
        2 => Some(x),
        3 => Some(x * 2.54),
        _ => None,
    }
}

// ---------- WebP ----------

fn webp(data: &[u8], m: &mut Meta) {
    let mut pos = 12usize;
    while pos + 8 <= data.len() {
        let kind = &data[pos..pos + 4];
        let len = le32(data, pos + 4) as usize;
        let body = pos + 8;
        if body + len > data.len() {
            break;
        }
        match kind {
            b"ICCP" => {
                if let Some(p) = icc_desc(&data[body..body + len]) {
                    m.profile = Some(p);
                }
            }
            b"ALPH" => m.alpha_hint = true,
            _ => {}
        }
        pos = body + ((len + 1) & !1);
    }
}

// ---------- GIF ----------

fn gif(data: &[u8], m: &mut Meta) {
    let flags = data[10];
    let mut pos = 13usize;
    if flags & 0x80 != 0 {
        pos += 3 * (1usize << ((flags & 0x07) + 1));
    }
    while let Some(&intro) = data.get(pos) {
        pos += 1;
        match intro {
            0x3B => break,
            0x21 => {
                let Some(&label) = data.get(pos) else { break };
                pos += 1;
                if label == 0xF9
                    && let Some(&blen) = data.get(pos)
                    && (1..=4).contains(&blen)
                    && let Some(&packed) = data.get(pos + 1)
                    && packed & 0x01 != 0
                {
                    m.alpha_hint = true;
                }
                pos = skip_sub_blocks(data, pos);
            }
            0x2C => {
                let Some(desc) = data.get(pos..pos + 9) else {
                    break;
                };
                let lflags = desc[8];
                pos += 9;
                if lflags & 0x80 != 0 {
                    pos += 3 * (1usize << ((lflags & 0x07) + 1));
                }
                pos += 1; // LZW min code size
                pos = skip_sub_blocks(data, pos);
            }
            _ => break,
        }
    }
}

fn skip_sub_blocks(data: &[u8], mut pos: usize) -> usize {
    loop {
        let Some(&len) = data.get(pos) else {
            return pos;
        };
        pos += 1;
        if len == 0 {
            return pos;
        }
        pos += len as usize;
    }
}

// ---------- TIFF ----------

fn tiff(data: &[u8], m: &mut Meta) {
    m.dpi = exif_xres(data);
}

// ---------- BMP ----------

fn bmp(data: &[u8], m: &mut Meta) {
    if data.len() < 46 {
        return;
    }
    let hdr = le32(data, 14) as usize;
    if hdr < 40 {
        return;
    }
    let xppm = le32(data, 38);
    let yppm = le32(data, 42);
    let ppm = if xppm != 0 { xppm } else { yppm };
    if ppm != 0 {
        m.dpi = Some(ppm as f64 * 0.0254);
    }
}

// ---------- ICC ----------

/// Pull the human-readable description out of an ICC profile
/// ('desc' tag in v2 profiles, 'mluc' tag in v4).
fn icc_desc(data: &[u8]) -> Option<String> {
    if data.len() < 132 {
        return None;
    }
    let count = be32(data, 128) as usize;
    for i in 0..count {
        let e = 132 + i * 12;
        let sig = data.get(e..e + 4)?;
        let off = be32(data, e + 4) as usize;
        let size = be32(data, e + 8) as usize;
        let Some(tag) = data.get(off..off + size) else {
            continue;
        };
        match sig {
            b"desc" if size >= 12 && &tag[..4] == b"desc" => {
                let n = be32(tag, 8) as usize;
                if n == 0 || size < 12 + n {
                    continue;
                }
                let raw = &tag[12..12 + n];
                let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
                if let Ok(s) = std::str::from_utf8(&raw[..end]) {
                    return Some(s.to_string());
                }
            }
            b"mluc" if size >= 16 && &tag[..4] == b"mluc" => {
                let records = be32(tag, 8) as usize;
                if records == 0 {
                    continue;
                }
                let rec = 16; // sig(4) + reserved(4) + count(4) + record_size(4)
                let slen = be32(tag, rec + 4) as usize;
                let soff = be32(tag, rec + 8) as usize;
                let Some(raw) = tag.get(soff..soff + slen) else {
                    continue;
                };
                let units = raw
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|p| u16::from_be_bytes(*p))
                    .collect::<Vec<u16>>();
                let s = String::from_utf16_lossy(&units);
                let trimmed = s.trim_end_matches('\0');
                if !trimmed.is_empty() {
                    return Some(trimmed.to_string());
                }
            }
            _ => {}
        }
    }
    None
}

// ---------- primitives ----------

fn be16(d: &[u8], o: usize) -> u16 {
    u16::from_be_bytes([d[o], d[o + 1]])
}

fn be32(d: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn le32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

#[cfg(test)]
mod tests {
    use std::io::Write as _;

    use super::*;

    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_be_bytes());
    }

    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_be_bytes());
    }

    fn push_u16_le(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn push_u32_le(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    fn png_chunk(out: &mut Vec<u8>, kind: &[u8; 4], payload: &[u8]) {
        push_u32(out, payload.len() as u32);
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out.extend_from_slice(&[0, 0, 0, 0]); // crc placeholder, not verified
    }

    #[test]
    fn png_phys_iccp_srgb_trns() {
        let mut d = b"\x89PNG\r\n\x1a\n".to_vec();
        let phys: Vec<u8> = 5670u32
            .to_be_bytes()
            .into_iter()
            .chain(5670u32.to_be_bytes())
            .chain([1])
            .collect();
        png_chunk(&mut d, b"pHYs", &phys);

        // minimal v2 ICC with 'desc' tag; name differs from the sRGB fallback
        // so this really validates the zlib + ICC parsing path
        let text = b"Acme Custom RGB";
        let mut tag = b"desc".to_vec();
        tag.extend_from_slice(&[0; 4]);
        push_u32(&mut tag, text.len() as u32 + 1);
        tag.extend_from_slice(text);
        tag.push(0);
        let mut icc = vec![0u8; 128];
        push_u32(&mut icc, 1); // tag count
        push_u32(&mut icc, 0x64657363); // 'desc'
        push_u32(&mut icc, 144); // body offset: header + one 12-byte table entry
        push_u32(&mut icc, tag.len() as u32);
        icc.extend_from_slice(&tag);
        let mut iccp_payload = b"c\0".to_vec();
        iccp_payload.push(0); // compression method zlib
        let mut enc = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        enc.write_all(&icc).unwrap();
        iccp_payload.append(&mut enc.finish().unwrap());
        png_chunk(&mut d, b"iCCP", &iccp_payload);

        png_chunk(&mut d, b"sRGB", &[0]);
        png_chunk(&mut d, b"tRNS", &[0, 0]);
        png_chunk(&mut d, b"IEND", &[]);

        let m = extract(&d);
        assert!((m.dpi.unwrap() - 5670f64 * 0.0254).abs() < 1e-9);
        assert_eq!(m.profile.as_deref(), Some("Acme Custom RGB"));
        assert!(m.alpha_hint);
    }

    #[test]
    fn png_srgb_fallback_when_no_iccp() {
        let mut d = b"\x89PNG\r\n\x1a\n".to_vec();
        png_chunk(&mut d, b"sRGB", &[0]);
        png_chunk(&mut d, b"IEND", &[]);
        let m = extract(&d);
        assert_eq!(m.profile.as_deref(), Some(SRGB_NAME));
        assert!(m.dpi.is_none());
    }

    #[test]
    fn jpeg_jfif_dpi() {
        let mut d = b"\xff\xd8".to_vec();
        let mut app0 = b"JFIF\0".to_vec();
        app0.extend_from_slice(&[1, 2]); // version
        app0.push(1); // units = dpi
        push_u16(&mut app0, 300);
        push_u16(&mut app0, 300);
        d.extend_from_slice(&[0xFF, 0xE0]);
        push_u16(&mut d, app0.len() as u16 + 2);
        d.extend_from_slice(&app0);
        d.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
        let m = extract(&d);
        assert_eq!(m.dpi, Some(300.0));
    }

    #[test]
    fn jpeg_exif_dpi_inch_and_cm() {
        fn build_exif(le: bool, num: u32, den: u32, unit: u16) -> Vec<u8> {
            let w16 = |v: &mut Vec<u8>, x: u16| {
                v.extend_from_slice(&(if le { x.to_le_bytes() } else { x.to_be_bytes() }))
            };
            let w32 = |v: &mut Vec<u8>, x: u32| {
                v.extend_from_slice(&(if le { x.to_le_bytes() } else { x.to_be_bytes() }))
            };
            let mut ifd: Vec<u8> = Vec::new();
            w16(&mut ifd, 2); // entries

            w16(&mut ifd, 282); // XResolution
            w16(&mut ifd, 5); // RATIONAL
            w32(&mut ifd, 1);
            w32(&mut ifd, 38); // offset of rational data: 8(hdr) + 2(count) + 2*12(entries) + 4(next)

            w16(&mut ifd, 296); // ResolutionUnit
            w16(&mut ifd, 3); // SHORT
            w32(&mut ifd, 1);
            w16(&mut ifd, unit);
            w16(&mut ifd, 0);

            w32(&mut ifd, 0); // next IFD
            w32(&mut ifd, num);
            w32(&mut ifd, den);

            let mut tiff: Vec<u8> = Vec::new();
            tiff.extend_from_slice(if le { b"II" } else { b"MM" });
            w16(&mut tiff, 42);
            w32(&mut tiff, 8);
            tiff.extend_from_slice(&ifd);

            let mut d = b"\xff\xd8".to_vec();
            let mut app1 = b"Exif\0\0".to_vec();
            app1.extend_from_slice(&tiff);
            d.extend_from_slice(&[0xFF, 0xE1]);
            push_u16(&mut d, app1.len() as u16 + 2);
            d.extend_from_slice(&app1);
            d.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02]);
            d
        }
        assert_eq!(
            extract(&build_exif(true, 1_440_000, 10_000, 2)).dpi,
            Some(144.0)
        );
        let expect = (100.0f64 / 39.0 * 2.54 * 100.0).round();
        assert_eq!(
            extract(&build_exif(false, 100, 39, 3))
                .dpi
                .map(|v| (v * 100.0).round()),
            Some(expect)
        );
    }

    #[test]
    fn webp_iccp_alph() {
        let text = b"Custom Profile";
        let mut mluc = b"mluc".to_vec();
        mluc.extend_from_slice(&[0; 4]);
        push_u32(&mut mluc, 1); // record count
        push_u32(&mut mluc, 12); // record size
        mluc.extend_from_slice(b"enUS");
        push_u32(&mut mluc, text.len() as u32 * 2 + 2); // string length incl. NUL
        push_u32(&mut mluc, 28); // string offset: sig+res+count+recsize+record
        for &b in text {
            push_u16(&mut mluc, b as u16);
        }
        push_u16(&mut mluc, 0);

        let mut prof = vec![0u8; 128];
        push_u32(&mut prof, 1); // tag count
        prof.extend_from_slice(b"mluc");
        push_u32(&mut prof, 144); // body offset: header + one 12-byte table entry
        push_u32(&mut prof, mluc.len() as u32);
        prof.extend_from_slice(&mluc);

        let mut d = b"RIFF\x00\x00\x00\x00WEBP".to_vec();
        let mut chunk = b"ICCP".to_vec();
        chunk.extend_from_slice(&(prof.len() as u32).to_le_bytes());
        chunk.extend_from_slice(&prof);
        d.extend_from_slice(&chunk);

        let mut alph = b"ALPH".to_vec();
        alph.extend_from_slice(&1u32.to_le_bytes());
        alph.push(0);
        d.extend_from_slice(&alph);
        d.extend_from_slice(&[0]); // pad to even

        let m = extract(&d);
        assert_eq!(m.profile.as_deref(), Some("Custom Profile"));
        assert!(m.alpha_hint);
    }

    #[test]
    fn gif_transparent_flag() {
        let mut d = b"GIF89a".to_vec();
        d.extend_from_slice(&[1, 0, 1, 0, 0, 0, 0]); // LSD, no GCT
        d.extend_from_slice(&[0x21, 0xF9, 0x04, 0x01, 0x00, 0x00, 0x00, 0x00]); // GCE transparent=1
        d.extend_from_slice(&[
            0x2C, 0, 0, 0, 0, 1, 0, 1, 0, 0x00, 0x02, 0x02, 0x44, 0x01, 0x00,
        ]);
        d.push(0x3B);
        let m = extract(&d);
        assert!(m.alpha_hint);
    }

    #[test]
    fn gif_no_gct_offset_respected() {
        let mut d = b"GIF87a".to_vec();
        // LSD with GCT flag set: 2^2 colors * 3 bytes = 12 bytes of GCT
        d.extend_from_slice(&[1, 0, 1, 0, 0b1011_0000, 0, 0]);
        d.extend_from_slice(&[0u8; 12]);
        d.push(0x21);
        d.extend_from_slice(&[0xFE, 0x03, b'a', b'b', b'c', 0x00]); // comment ext
        d.push(0x3B);
        assert!(!extract(&d).alpha_hint); // walked to end without crash
    }

    #[test]
    fn tiff_resolution_dpi() {
        let mut ifd: Vec<u8> = Vec::new();
        push_u16(&mut ifd, 2); // entries

        push_u16(&mut ifd, 282); // XResolution
        push_u16(&mut ifd, 5); // RATIONAL
        push_u32(&mut ifd, 1);
        push_u32(&mut ifd, 38); // offset of rational data

        push_u16(&mut ifd, 296); // ResolutionUnit
        push_u16(&mut ifd, 3); // SHORT
        push_u32(&mut ifd, 1);
        push_u16(&mut ifd, 2); // inch
        push_u16(&mut ifd, 0);

        push_u32(&mut ifd, 0); // next IFD
        push_u32(&mut ifd, 300); // xres num
        push_u32(&mut ifd, 1); // xres den

        let mut d = b"MM".to_vec();
        push_u16(&mut d, 42);
        push_u32(&mut d, 8);
        d.extend_from_slice(&ifd);

        let m = extract(&d);
        assert_eq!(m.dpi, Some(300.0));
    }

    #[test]
    fn bmp_pixels_per_meter_dpi() {
        let mut d = b"BM".to_vec();
        push_u32_le(&mut d, 54); // file size
        push_u32_le(&mut d, 0); // reserved
        push_u32_le(&mut d, 54); // pixel data offset
        push_u32_le(&mut d, 40); // DIB header size
        push_u32_le(&mut d, 1); // width
        push_u32_le(&mut d, 1); // height
        push_u16_le(&mut d, 1); // planes
        push_u16_le(&mut d, 24); // bpp
        push_u32_le(&mut d, 0); // compression
        push_u32_le(&mut d, 3); // image size
        push_u32_le(&mut d, 2835); // x ppm (72 dpi)
        push_u32_le(&mut d, 2835); // y ppm
        push_u32_le(&mut d, 0); // colors
        push_u32_le(&mut d, 0); // important colors

        let m = extract(&d);
        assert!((m.dpi.unwrap() - 2835f64 * 0.0254).abs() < 0.01);
    }

    #[test]
    fn unknown_magic_yields_empty_meta() {
        let m = extract(b"not an image at all");
        assert!(m.dpi.is_none());
        assert!(m.profile.is_none());
        assert!(!m.alpha_hint);
    }
}
