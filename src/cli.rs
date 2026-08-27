use std::fmt;
use std::path::PathBuf;

use crate::size::Quality;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub width: Option<u32>,
    pub quality: Quality,
    pub info: bool,
    pub paths: Vec<PathBuf>,
}

impl Default for Args {
    fn default() -> Self {
        Args {
            width: None,
            quality: Quality::default(),
            info: false,
            paths: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub enum ParseError {
    Help,
    Version,
    MissingValue(&'static str),
    InvalidNumber(&'static str, String),
    InvalidQuality(String),
    UnknownOption(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Help => write!(f, "help"),
            ParseError::Version => write!(f, "version"),
            ParseError::MissingValue(opt) => write!(f, "option {opt} requires a value"),
            ParseError::InvalidNumber(opt, v) => write!(f, "invalid value {v:?} for option {opt}"),
            ParseError::InvalidQuality(v) => {
                write!(f, "invalid quality {v:?}: expected L, M or H")
            }
            ParseError::UnknownOption(o) => write!(f, "unknown option {o}"),
        }
    }
}

pub const USAGE: &str = "\
Usage: isee [OPTIONS] [IMGPATH ...]

Preview images in the terminal.
If IMGPATH is omitted, image data is read from stdin.

Options:
  -w WIDTH   Preview at the given pixel width.
             Without -w, previews are capped at 1920 px wide. A preview is
             never wider than the terminal window.
  -q QUALITY Preview scaling quality: L (nearest), M (triangle), H (lanczos);
             default M
  -i         Show image information
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
        if let Some(v) = short_value("-q", &arg) {
            out.quality = parse_quality(v)?;
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(ParseError::Help),
            "-v" => return Err(ParseError::Version),
            "-i" => out.info = true,
            "-w" => {
                let v = it.next().ok_or(ParseError::MissingValue("-w"))?;
                out.width = Some(parse_num("-w", &v)?);
            }
            "-q" => {
                let v = it.next().ok_or(ParseError::MissingValue("-q"))?;
                out.quality = parse_quality(&v)?;
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

fn parse_quality(v: &str) -> Result<Quality, ParseError> {
    Quality::parse(v).ok_or_else(|| ParseError::InvalidQuality(v.trim().to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn parse_combined_short() {
        assert_eq!(parse(["-w800"]).unwrap().width, Some(800));
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
