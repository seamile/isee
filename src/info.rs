use image::ImageFormat;

use crate::input::ImageInfo;

pub fn render(l: &ImageInfo) -> String {
    let dpi = match l.dpi {
        Some(d) => (d.round() as u64).to_string(),
        None => "-".to_string(),
    };
    let profile = l.profile.clone().unwrap_or_else(|| "-".to_string());
    let mut s = format!(
        "format: {}\nwidth: {}\nheight: {}\ndpi: {}\nalpha: {}\nprofile: {}\n",
        format_name(l.format),
        l.width,
        l.height,
        dpi,
        if l.alpha { "yes" } else { "no" },
        profile,
    );
    if l.frames > 1 {
        s.push_str(&format!("frames: {}\n", l.frames));
    }
    s
}

fn format_name(f: Option<ImageFormat>) -> &'static str {
    match f {
        Some(ImageFormat::Png) => "png",
        Some(ImageFormat::Jpeg) => "jpeg",
        Some(ImageFormat::Gif) => "gif",
        Some(ImageFormat::WebP) => "webp",
        _ => "unknown",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ImageInfo {
        ImageInfo {
            format: Some(ImageFormat::Jpeg),
            width: 1035,
            height: 818,
            frames: 1,
            dpi: Some(144.176),
            alpha: false,
            profile: Some("sRGB IEC61966-2.1".to_string()),
        }
    }

    #[test]
    fn output_matches_spec() {
        let out = render(&info());
        assert_eq!(
            out,
            "format: jpeg\nwidth: 1035\nheight: 818\ndpi: 144\nalpha: no\nprofile: sRGB IEC61966-2.1\n"
        );
    }

    #[test]
    fn unknown_values_rendered_as_dash_and_frames_appended() {
        let mut i = info();
        i.format = None;
        i.dpi = None;
        i.profile = None;
        i.alpha = true;
        i.frames = 12;
        let out = render(&i);
        assert_eq!(
            out,
            "format: unknown\nwidth: 1035\nheight: 818\ndpi: -\nalpha: yes\nprofile: -\nframes: 12\n"
        );
    }

    #[test]
    fn dpi_rounded_to_integer_for_display_only() {
        let mut i = info();
        i.dpi = Some(143.6);
        assert!(render(&i).contains("dpi: 144\n"));
        // internal precision untouched
        assert!((i.dpi.unwrap() - 143.6).abs() < f64::EPSILON);
    }
}
