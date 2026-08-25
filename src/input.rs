use std::fs;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::PathBuf;

use image::metadata::Orientation;
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader};

pub enum Source {
    Path(PathBuf),
    Stdin,
}

enum SourceReader {
    File(BufReader<fs::File>),
    Bytes(Cursor<Vec<u8>>),
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            SourceReader::File(r) => r.read(buf),
            SourceReader::Bytes(r) => r.read(buf),
        }
    }
}

impl BufRead for SourceReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            SourceReader::File(r) => r.fill_buf(),
            SourceReader::Bytes(r) => r.fill_buf(),
        }
    }

    fn consume(&mut self, amt: usize) {
        match self {
            SourceReader::File(r) => r.consume(amt),
            SourceReader::Bytes(r) => r.consume(amt),
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            SourceReader::File(r) => r.seek(pos),
            SourceReader::Bytes(r) => r.seek(pos),
        }
    }
}

pub struct Loaded {
    pub img: DynamicImage,
}

pub struct ImageInfo {
    pub format: Option<ImageFormat>,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub bytes: u64,
    pub desc: String,
}

pub fn load(source: &Source) -> Result<Loaded, Box<dyn std::error::Error>> {
    let (reader, _desc, _bytes) = match source {
        Source::Path(p) => {
            let file = fs::File::open(p)?;
            let r = SourceReader::File(BufReader::new(file));
            (r, p.display().to_string(), fs::metadata(p)?.len())
        }
        Source::Stdin => {
            let mut buf = Vec::new();
            io::stdin().lock().read_to_end(&mut buf)?;
            let bytes = buf.len() as u64;
            (SourceReader::Bytes(Cursor::new(buf)), "<stdin>".to_string(), bytes)
        }
    };
    let mut reader = ImageReader::new(reader);
    reader = reader.with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(Loaded { img })
}

pub fn load_info(source: &Source) -> Result<ImageInfo, Box<dyn std::error::Error>> {
    let (buf, desc) = match source {
        Source::Path(p) => (fs::read(p)?, p.display().to_string()),
        Source::Stdin => {
            let mut buf = Vec::new();
            io::stdin().lock().read_to_end(&mut buf)?;
            (buf, "<stdin>".to_string())
        }
    };
    let bytes = buf.len() as u64;
    let format = ImageReader::new(Cursor::new(&buf))
        .with_guessed_format()?
        .format();
    let frames = match format {
        Some(ImageFormat::Gif) => gif_frames(BufReader::new(Cursor::new(&buf)))?,
        _ => 1,
    };
    let (width, height) = ImageReader::new(Cursor::new(&buf))
        .with_guessed_format()?
        .into_decoder()?
        .dimensions();
    Ok(ImageInfo {
        format,
        width,
        height,
        frames,
        bytes,
        desc,
    })
}

fn gif_frames<R>(r: R) -> Result<u32, Box<dyn std::error::Error>>
where
    R: BufRead + Seek,
{
    use image::codecs::gif::GifDecoder;
    use image::AnimationDecoder;
    Ok(GifDecoder::new(r)?.into_frames().count() as u32)
}
