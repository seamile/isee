use image::ColorType;

use crate::input::ImageInfo;

pub fn render(path: &str, l: &ImageInfo) -> String {
    let mut s = format!("{path}:\n");
    s.push_str(&line("Size", &human_size(l.size)));
    s.push('\n');
    s.push_str(&line("Width", &format!("{} px", l.width)));
    s.push('\n');
    s.push_str(&line("Height", &format!("{} px", l.height)));
    s.push('\n');
    let dpi = match l.dpi {
        Some(d) => (d.round() as u64).to_string(),
        None => "-".to_string(),
    };
    s.push_str(&line("DPI", &dpi));
    s.push('\n');
    s.push_str(&line("Mode", mode_name(l.color)));
    s.push('\n');
    s.push_str(&line("Alpha", if l.alpha { "True" } else { "False" }));
    s.push('\n');
    s
}

fn line(key: &str, value: &str) -> String {
    let pad = " ".repeat(7usize.saturating_sub(key.len()));
    format!("- {key}:{pad}{value}")
}

fn mode_name(ct: ColorType) -> &'static str {
    match ct {
        ColorType::L8 | ColorType::L16 => "L",
        ColorType::La8 | ColorType::La16 => "LA",
        ColorType::Rgb8 | ColorType::Rgb16 | ColorType::Rgb32F => "RGB",
        ColorType::Rgba8 | ColorType::Rgba16 | ColorType::Rgba32F => "RGBA",
        _ => "?",
    }
}

fn human_size(bytes: u64) -> String {
    const KB: u64 = 1000;
    const MB: u64 = KB * 1000;
    const GB: u64 = MB * 1000;
    let b = bytes as f64;
    if bytes >= GB {
        format!("{:.1} GB", b / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", b / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", b / KB as f64)
    } else {
        format!("{:.0} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> ImageInfo {
        ImageInfo {
            size: 1_300_000,
            width: 800,
            height: 600,
            dpi: Some(144.176),
            alpha: true,
            color: ColorType::Rgb8,
        }
    }

    #[test]
    fn output_matches_spec() {
        let out = render("/foo/bar/image.png", &info());
        assert_eq!(
            out,
            "/foo/bar/image.png:\n\
             - Size:   1.3 MB\n\
             - Width:  800 px\n\
             - Height: 600 px\n\
             - DPI:    144\n\
             - Mode:   RGB\n\
             - Alpha:  True\n"
        );
    }

    #[test]
    fn stdin_path_rendered_as_dash() {
        assert!(render("-", &info()).starts_with("-:\n- Size:"));
    }

    #[test]
    fn dpi_rounded_to_integer_for_display_only() {
        let mut i = info();
        i.dpi = Some(143.6);
        assert!(render("/x.png", &i).contains("- DPI:    144\n"));
        assert!((i.dpi.unwrap() - 143.6).abs() < f64::EPSILON);
    }

    #[test]
    fn unknown_dpi_rendered_as_dash() {
        let mut i = info();
        i.dpi = None;
        assert!(render("/x.png", &i).contains("- DPI:    -\n"));
    }

    #[test]
    fn human_size_formatting() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(999), "999 B");
        assert_eq!(human_size(1000), "1.0 KB");
        assert_eq!(human_size(1_300_000), "1.3 MB");
        assert_eq!(human_size(2_500_000_000), "2.5 GB");
    }

    #[test]
    fn mode_name_mapping() {
        assert_eq!(mode_name(ColorType::L8), "L");
        assert_eq!(mode_name(ColorType::L16), "L");
        assert_eq!(mode_name(ColorType::La8), "LA");
        assert_eq!(mode_name(ColorType::Rgb8), "RGB");
        assert_eq!(mode_name(ColorType::Rgb32F), "RGB");
        assert_eq!(mode_name(ColorType::Rgba16), "RGBA");
    }
}
