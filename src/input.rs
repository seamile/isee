use std::fs;
use std::io::{self, BufRead, Cursor, Read, Seek};
use std::path::PathBuf;

use image::metadata::Orientation;
use image::{ColorType, DynamicImage, ImageDecoder, ImageFormat, ImageReader};

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
    pub dpi: Option<f64>,
}

pub fn load(source: &Source) -> Result<Loaded, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let m = meta::extract(&buf);
    let (img, _) = decode_bytes(&buf)?;
    Ok(Loaded { img, dpi: m.dpi })
}

pub struct ImageInfo {
    pub format: Option<ImageFormat>,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub dpi: Option<f64>,
    pub alpha: bool,
    pub profile: Option<String>,
}

pub fn load_info(source: &Source) -> Result<ImageInfo, Box<dyn std::error::Error>> {
    let buf = read_all(source)?;
    let m = meta::extract(&buf);
    let reader = ImageReader::new(Cursor::new(&buf[..])).with_guessed_format()?;
    let format = reader.format();
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    let color = decoder.color_type();
    let frames = match format {
        Some(ImageFormat::Gif) => gif_frames(Cursor::new(&buf[..]))?,
        _ => 1,
    };
    Ok(ImageInfo {
        format,
        width,
        height,
        frames,
        dpi: m.dpi,
        alpha: has_alpha(color) || m.alpha_hint,
        profile: m.profile,
    })
}

fn has_alpha(ct: ColorType) -> bool {
    matches!(
        ct,
        ColorType::La8 | ColorType::La16 | ColorType::Rgba8 | ColorType::Rgba16 | ColorType::Rgba32F
    )
}

fn gif_frames<R>(r: R) -> Result<u32, Box<dyn std::error::Error>>
where
    R: BufRead + Seek,
{
    use image::AnimationDecoder;
    use image::codecs::gif::GifDecoder;
    Ok(GifDecoder::new(r)?.into_frames().count() as u32)
}
