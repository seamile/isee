use std::fmt;
use std::path::PathBuf;

use crate::detect::Protocol;
use crate::size::Quality;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub width: Option<u32>,
    pub crop: Option<[u32; 4]>,
    pub tile: Option<u32>,
    pub quality: Quality,
    pub info: bool,
    pub animate: bool,
    pub protocol: Option<Protocol>,
    pub paths: Vec<PathBuf>,
}

#[derive(Debug)]
pub enum ParseError {
    Help,
    Version,
    MissingValue(&'static str),
    InvalidNumber(&'static str, String),
    InvalidCrop(String),
    InvalidQuality(String),
    InvalidProtocol(String),
    UnknownOption(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Help => write!(f, "help"),
            ParseError::Version => write!(f, "version"),
            ParseError::MissingValue(opt) => write!(f, "option {opt} requires a value"),
            ParseError::InvalidNumber(opt, v) => write!(f, "invalid value {v:?} for option {opt}"),
            ParseError::InvalidCrop(v) => write!(
                f,
                "invalid crop {v:?}: expected x1,y1,x2,y2 with non-negative integers and a non-zero width and height"
            ),
            ParseError::InvalidQuality(v) => {
                write!(f, "invalid quality {v:?}: expected L, M or H")
            }
            ParseError::InvalidProtocol(v) => write!(
                f,
                "invalid protocol {v:?}: expected auto, kitty, iip, sixel or halfblock"
            ),
            ParseError::UnknownOption(o) => write!(f, "unknown option {o}"),
        }
    }
}

pub const USAGE: &str = "\
Usage: isee [OPTIONS] [IMGPATH ...]

Preview images in the terminal.
If IMGPATH is omitted, image data is read from stdin.

Options:
  -w WIDTH   Preview at the given pixel width; -w 0 uses the image's native width.
             Without -w, the terminal window width is the only cap. A
             preview is never wider than the terminal window; its height
             may exceed the window (the terminal scrolls vertically).
  -c X1,Y1,X2,Y2
             Show only the rectangle between two pixels; coordinates are
             top-left based and use the image before preview scaling. In
             tile mode, crop applies before tiling.
  -t COUNT   Tile COUNT images per row in square areas; scale to fit,
             keeping aspect ratio. Ignores -w and -a.
  -q QUALITY Preview scaling quality: L (nearest), M (triangle), H (lanczos);
             default M
  -p PROTO   Specify the preview protocol: auto (default),
             K[itty], I[term], S[ixel], H[alfblock]
  -i         Show image information
  -a         Animate GIFs where the terminal supports it (kitty; iTerm2,
             mintty), else fall back to the first frame
  -v         Print version
  -h, --help Print help
";

pub fn parse<I, S>(args: I) -> Result<Args, ParseError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut out = Args::default();
    let mut it = args.into_iter().map(Into::into);
    while let Some(arg) = it.next() {
        if let Some(v) = short_value("-w", &arg) {
            out.width = Some(parse_num("-w", v)?);
            continue;
        }
        if let Some(v) = arg.strip_prefix("-c").filter(|v| v.contains(',')) {
            out.crop = Some(parse_crop(v)?);
            continue;
        }
        if let Some(v) = short_value("-t", &arg) {
            out.tile = Some(parse_tile(v)?);
            continue;
        }
        if let Some(v) = short_value("-q", &arg) {
            out.quality = parse_quality(v)?;
            continue;
        }
        if let Some(v) = short_value("-p", &arg) {
            out.protocol = parse_protocol(v)?;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(ParseError::Help),
            "-v" => return Err(ParseError::Version),
            "-i" => out.info = true,
            "-a" | "--animate" => out.animate = true,
            "-w" => {
                let v = it.next().ok_or(ParseError::MissingValue("-w"))?;
                out.width = Some(parse_num("-w", &v)?);
            }
            "-c" => {
                let v = it.next().ok_or(ParseError::MissingValue("-c"))?;
                out.crop = Some(parse_crop(&v)?);
            }
            "-t" => {
                let v = it.next().ok_or(ParseError::MissingValue("-t"))?;
                out.tile = Some(parse_tile(&v)?);
            }
            "-q" => {
                let v = it.next().ok_or(ParseError::MissingValue("-q"))?;
                out.quality = parse_quality(&v)?;
            }
            "-p" => {
                let v = it.next().ok_or(ParseError::MissingValue("-p"))?;
                out.protocol = parse_protocol(&v)?;
            }
            s if s.starts_with('-') && s.len() > 1 => {
                return Err(ParseError::UnknownOption(s.to_string()));
            }
            s => out.paths.push(PathBuf::from(s)),
        }
    }
    Ok(out)
}

fn short_value<'a>(opt: &str, arg: &'a str) -> Option<&'a str> {
    arg.strip_prefix(opt)
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphanumeric()))
}

fn parse_num(opt: &'static str, v: &str) -> Result<u32, ParseError> {
    v.trim()
        .parse()
        .map_err(|_| ParseError::InvalidNumber(opt, v.to_string()))
}

fn parse_tile(v: &str) -> Result<u32, ParseError> {
    let count = parse_num("-t", v)?;
    if count == 0 {
        return Err(ParseError::InvalidNumber("-t", v.to_string()));
    }
    Ok(count)
}

fn parse_crop(v: &str) -> Result<[u32; 4], ParseError> {
    let parts: Vec<_> = v.split(',').map(str::trim).collect();
    let [x1, y1, x2, y2] = parts.as_slice() else {
        return Err(ParseError::InvalidCrop(v.to_string()));
    };
    let nums = [x1, y1, x2, y2]
        .map(|part| parse_num("-c", part).map_err(|_| ParseError::InvalidCrop(v.to_string())));
    let [x1, y1, x2, y2] = nums;
    let crop = [x1?, y1?, x2?, y2?];
    if crop[0] == crop[2] || crop[1] == crop[3] {
        return Err(ParseError::InvalidCrop(v.to_string()));
    }
    Ok(crop)
}

fn parse_quality(v: &str) -> Result<Quality, ParseError> {
    Quality::parse(v).ok_or_else(|| ParseError::InvalidQuality(v.trim().to_string()))
}

/// `-p` protocol choice: `auto` maps to None (let detection decide), the
/// rest force a concrete protocol; anything else is a hard usage error.
fn parse_protocol(v: &str) -> Result<Option<Protocol>, ParseError> {
    match v.trim().to_ascii_lowercase().as_str() {
        "auto" => Ok(None),
        "kitty" | "kgp" | "k" => Ok(Some(Protocol::Kitty)),
        "iterm" | "iip" | "i" => Ok(Some(Protocol::Iip)),
        "sixel" | "six" | "s" => Ok(Some(Protocol::Sixel)),
        "halfblock" | "hb" | "h" => Ok(Some(Protocol::HalfBlocks)),
        _ => Err(ParseError::InvalidProtocol(v.trim().to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_tile_count_and_ignored_options_in_either_order() {
        assert_eq!(parse(["-t", "3"]).unwrap().tile, Some(3));
        assert_eq!(parse(["-t3"]).unwrap().tile, Some(3));
        for args in [
            ["-t3", "-w800", "-a", "a.png"],
            ["-a", "-w800", "-t3", "a.png"],
        ] {
            let a = parse(args).unwrap();
            assert_eq!(a.tile, Some(3));
            assert_eq!(a.width, Some(800));
            assert!(a.animate);
            assert_eq!(a.paths, vec![PathBuf::from("a.png")]);
        }
    }

    #[test]
    fn parse_tile_rejects_invalid_counts_and_long_option() {
        assert!(matches!(parse(["-t"]), Err(ParseError::MissingValue("-t"))));
        for value in ["0", "-1", "abc", "4294967296"] {
            assert!(matches!(
                parse(["-t", value]),
                Err(ParseError::InvalidNumber("-t", _))
            ));
        }
        assert!(matches!(
            parse(["-t0"]),
            Err(ParseError::InvalidNumber("-t", _))
        ));
        assert!(matches!(
            parse(["--tile", "3"]),
            Err(ParseError::UnknownOption(_))
        ));
    }

    #[test]
    fn parse_basic_info() {
        let a = parse(["-i"]).unwrap();
        assert!(a.info);
        assert!(a.paths.is_empty());
        assert!(a.width.is_none());
    }

    #[test]
    fn parse_width_quality_path() {
        let a = parse(["-w", "800", "-q", "h", "img.png"]).unwrap();
        assert_eq!(a.width, Some(800));
        assert_eq!(a.quality, Quality::High);
        assert_eq!(a.paths, vec![PathBuf::from("img.png")]);
    }

    #[test]
    fn parse_crop_accepts_both_attached_and_separate_values() {
        let expected = Some([10, 20, 30, 40]);
        assert_eq!(parse(["-c", "10,20,30,40"]).unwrap().crop, expected);
        assert_eq!(parse(["-c10,20,30,40"]).unwrap().crop, expected);
        assert_eq!(parse(["-c", "10, 20, 30, 40"]).unwrap().crop, expected);
    }

    #[test]
    fn parse_crop_rejects_bad_shape_numbers_and_zero_area() {
        assert!(matches!(parse(["-c"]), Err(ParseError::MissingValue("-c"))));
        for value in [
            "1,2,3",
            "1,2,3,4,5",
            "-1,0,1,1",
            "0,0,1.5,1",
            "0,0,abc,1",
            "0,0,4294967296,1",
            "1,0,1,1",
            "0,1,1,1",
        ] {
            assert!(
                matches!(parse(["-c", value]), Err(ParseError::InvalidCrop(_))),
                "value: {value}"
            );
        }
    }

    #[test]
    fn parse_combined_short() {
        assert_eq!(parse(["-w800"]).unwrap().width, Some(800));
        assert_eq!(parse(["-w0"]).unwrap().width, Some(0));
        let l = parse(["-ql"]).unwrap();
        assert_eq!(l.quality, Quality::Low);
        let m = parse(["-qM", "-i"]).unwrap();
        assert_eq!(m.quality, Quality::Medium);
        assert!(m.info);
        assert_eq!(parse(["-q", "H"]).unwrap().quality, Quality::High);
    }

    #[test]
    fn parse_no_args() {
        let a = parse(std::iter::empty::<&str>()).unwrap();
        assert_eq!(a, Args::default());
    }

    #[test]
    fn parse_help() {
        assert!(matches!(parse(["--help"]), Err(ParseError::Help)));
        assert!(matches!(parse(["-h"]), Err(ParseError::Help)));
    }

    #[test]
    fn parse_version() {
        assert!(matches!(parse(["-v"]), Err(ParseError::Version)));
        assert!(matches!(
            parse(["--version"]),
            Err(ParseError::UnknownOption(o)) if o == "--version"
        ));
    }

    #[test]
    fn parse_unknown_option() {
        assert!(matches!(parse(["-x"]), Err(ParseError::UnknownOption(_))));
    }

    #[test]
    fn parse_missing_value() {
        assert!(matches!(parse(["-w"]), Err(ParseError::MissingValue("-w"))));
        assert!(matches!(parse(["-q"]), Err(ParseError::MissingValue("-q"))));
    }

    #[test]
    fn parse_invalid_number() {
        assert!(matches!(
            parse(["-w", "abc"]),
            Err(ParseError::InvalidNumber(..))
        ));
    }

    #[test]
    fn parse_quality_levels() {
        assert_eq!(parse(["-q", "l"]).unwrap().quality, Quality::Low);
        assert_eq!(parse(["-q", "m"]).unwrap().quality, Quality::Medium);
        assert_eq!(parse(["-q", "h"]).unwrap().quality, Quality::High);
        assert_eq!(parse(["-qL"]).unwrap().quality, Quality::Low);
        assert_eq!(parse(["-q", "M"]).unwrap().quality, Quality::Medium);
        assert_eq!(parse(["-qH"]).unwrap().quality, Quality::High);
        assert!(matches!(
            parse(["-q", "80"]),
            Err(ParseError::InvalidQuality(v)) if v == "80"
        ));
        assert!(matches!(
            parse(["-qx"]),
            Err(ParseError::InvalidQuality(..))
        ));
    }

    #[test]
    fn parse_defaults_to_medium_quality() {
        let a = parse(std::iter::empty::<&str>()).unwrap();
        assert_eq!(a.quality, Quality::Medium);
        assert_eq!(a.protocol, None);
    }

    #[test]
    fn parse_protocol_values() {
        assert_eq!(parse(["-p", "auto"]).unwrap().protocol, None);
        assert_eq!(parse(["-pkitty"]).unwrap().protocol, Some(Protocol::Kitty));
        assert_eq!(parse(["-p", "IIP"]).unwrap().protocol, Some(Protocol::Iip));
        assert_eq!(parse(["-psixel"]).unwrap().protocol, Some(Protocol::Sixel));
        assert_eq!(
            parse(["-p", "halfblock"]).unwrap().protocol,
            Some(Protocol::HalfBlocks)
        );
    }

    #[test]
    fn parse_protocol_aliases_case_insensitive() {
        assert_eq!(
            parse(["-p", "kgp"]).unwrap().protocol,
            Some(Protocol::Kitty)
        );
        assert_eq!(parse(["-pk"]).unwrap().protocol, Some(Protocol::Kitty));
        assert_eq!(
            parse(["-p", "KITTY"]).unwrap().protocol,
            Some(Protocol::Kitty)
        );
        assert_eq!(
            parse(["-p", "ITERM"]).unwrap().protocol,
            Some(Protocol::Iip)
        );
        assert_eq!(parse(["-pi"]).unwrap().protocol, Some(Protocol::Iip));
        assert_eq!(
            parse(["-p", "six"]).unwrap().protocol,
            Some(Protocol::Sixel)
        );
        assert_eq!(parse(["-pS"]).unwrap().protocol, Some(Protocol::Sixel));
        assert_eq!(
            parse(["-p", "hb"]).unwrap().protocol,
            Some(Protocol::HalfBlocks)
        );
        assert_eq!(parse(["-ph"]).unwrap().protocol, Some(Protocol::HalfBlocks));
        assert_eq!(
            parse(["-pHALFBLOCK"]).unwrap().protocol,
            Some(Protocol::HalfBlocks)
        );
    }

    #[test]
    fn parse_protocol_rejects_unknown_values() {
        assert!(matches!(
            parse(["-p", "asciart"]),
            Err(ParseError::InvalidProtocol(v)) if v == "asciart"
        ));
        assert!(matches!(
            parse(["-p", ""]),
            Err(ParseError::InvalidProtocol(..))
        ));
        assert!(matches!(parse(["-p"]), Err(ParseError::MissingValue("-p"))));
    }

    #[test]
    fn parse_multiple_paths_in_order() {
        let a = parse(["a.png", "b.png", "c.png"]).unwrap();
        assert_eq!(
            a.paths,
            vec![
                PathBuf::from("a.png"),
                PathBuf::from("b.png"),
                PathBuf::from("c.png")
            ]
        );
    }

    #[test]
    fn parse_paths_mixed_with_options() {
        let a = parse(["-w", "800", "a.png", "-i", "b.png"]).unwrap();
        assert_eq!(a.width, Some(800));
        assert!(a.info);
        assert_eq!(
            a.paths,
            vec![PathBuf::from("a.png"), PathBuf::from("b.png")]
        );
    }

    #[test]
    fn parse_no_path_is_stdin_mode() {
        let a = parse(["-i"]).unwrap();
        assert!(a.info);
        assert!(a.paths.is_empty());
    }
}
