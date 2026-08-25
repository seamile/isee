use image::ImageFormat;

use crate::input::ImageInfo;

pub fn render(l: &ImageInfo) -> String {
    let mut s = format!("Image: {}\n", l.desc);
    if let Some(f) = l.format {
        s.push_str(&format!("Format: {}\n", format_name(f)));
    }
    s.push_str(&format!("Size: {}x{} px\n", l.width, l.height));
    if l.frames > 1 {
        s.push_str(&format!("Frames: {}\n", l.frames));
    }
    s.push_str(&format!("Bytes: {}\n", l.bytes));
    s
}

fn format_name(f: ImageFormat) -> &'static str {
    match f {
        ImageFormat::Png => "PNG",
        ImageFormat::Jpeg => "JPEG",
        ImageFormat::Gif => "GIF",
        ImageFormat::WebP => "WebP",
        _ => "Unknown",
    }
}
