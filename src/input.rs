use std::fs;
use std::io::{self, Cursor, Read};
use std::path::PathBuf;

use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageReader};

use crate::meta;

pub enum Source {
    Path(PathBuf),
    Stdin,
}

fn read_all(source: &Source) -> io::Result<Vec<u8>> {
    match source {
        Source::Path(p) => fs::read(p),
        Source::Stdin => {
            let mut buf = Vec::new();
            io::stdin().lock().read_to_end(&mut buf)?;
            Ok(buf)
        }
    }
}

fn decode_bytes(buf: &[u8]) -> Result<(DynamicImage, ColorType), Box<dyn std::error::Error>> {
    let reader = ImageReader::new(Cursor::new(buf)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let color = decoder.color_type();
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok((img, color))
}

pub struct Loaded {
    pub img: DynamicImage,
}

pub fn load(source: &Source) -> Result<Loaded, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let (img, _) = decode_bytes(&buf)?;
    Ok(Loaded { img })
}

pub struct ImageInfo {
    pub size: u64,
    pub width: u32,
    pub height: u32,
    pub dpi: Option<f64>,
    pub alpha: bool,
    pub color: ColorType,
}

pub fn load_info(source: &Source) -> Result<ImageInfo, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let m = meta::extract(&buf);
    let reader = ImageReader::new(Cursor::new(&buf[..])).with_guessed_format()?;
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    Ok(ImageInfo {
        size: buf.len() as u64,
        width,
        height,
        dpi: m.dpi,
        alpha: has_alpha(color) || m.alpha_hint,
        color,
    })
}

fn has_alpha(ct: ColorType) -> bool {
    matches!(
        ct,
        ColorType::La8 | ColorType::La16 | ColorType::Rgba8 | ColorType::Rgba16 | ColorType::Rgba32F
    )
}
