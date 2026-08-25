use std::fmt;
use std::path::PathBuf;

#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Args {
    pub width: Option<u32>,
    pub quality: Option<u8>,
    pub info: bool,
    pub path: Option<PathBuf>,
}

#[derive(Debug)]
pub enum ParseError {
    Help,
    MissingValue(&'static str),
    InvalidNumber(&'static str, String),
    UnknownOption(String),
    TooManyArgs(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::Help => write!(f, "help"),
            ParseError::MissingValue(opt) => write!(f, "option {opt} requires a value"),
            ParseError::InvalidNumber(opt, v) => write!(f, "invalid value {v:?} for option {opt}"),
            ParseError::UnknownOption(o) => write!(f, "unknown option {o}"),
            ParseError::TooManyArgs(a) => write!(f, "unexpected argument {a:?}"),
        }
    }
}

pub const USAGE: &str = "\
Usage: isee [OPTIONS] [IMGPATH]

Preview an image in the terminal.

Options:
  -w WIDTH   Preview at the given pixel width
  -q QUALITY Preview quality (0-100)
  -i         Show image information
  -h, --help Print help

If IMGPATH is omitted, image data is read from stdin.
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
            out.quality = Some(parse_quality(v)?);
            continue;
        }
        match arg.as_str() {
            "-h" | "--help" => return Err(ParseError::Help),
            "-i" => out.info = true,
            "-w" => {
                let v = it.next().ok_or(ParseError::MissingValue("-w"))?;
                out.width = Some(parse_num("-w", &v)?);
            }
            "-q" => {
                let v = it.next().ok_or(ParseError::MissingValue("-q"))?;
                out.quality = Some(parse_quality(&v)?);
            }
            s if s.starts_with('-') && s.len() > 1 => return Err(ParseError::UnknownOption(s.to_string())),
            s => {
                if out.path.is_some() {
                    return Err(ParseError::TooManyArgs(s.to_string()));
                }
                out.path = Some(PathBuf::from(s));
            }
        }
    }
    Ok(out)
}

fn short_value<'a>(opt: &str, arg: &'a str) -> Option<&'a str> {
    arg.strip_prefix(opt)
        .filter(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

fn parse_num(opt: &'static str, v: &str) -> Result<u32, ParseError> {
    v.trim()
        .parse()
        .map_err(|_| ParseError::InvalidNumber(opt, v.to_string()))
}

fn parse_quality(v: &str) -> Result<u8, ParseError> {
    let n = parse_num("-q", v)?;
    if n > 100 {
        return Err(ParseError::InvalidNumber("-q", n.to_string()));
    }
    Ok(n as u8)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_basic_info() {
        let a = parse(["-i"]).unwrap();
        assert!(a.info);
        assert!(a.path.is_none());
        assert!(a.width.is_none());
    }

    #[test]
    fn parse_width_quality_path() {
        let a = parse(["-w", "800", "-q", "80", "img.png"]).unwrap();
        assert_eq!(a.width, Some(800));
        assert_eq!(a.quality, Some(80));
        assert_eq!(a.path, Some(PathBuf::from("img.png")));
    }

    #[test]
    fn parse_combined_short() {
        assert_eq!(parse(["-w800"]).unwrap().width, Some(800));
        assert_eq!(parse(["-q75"]).unwrap().quality, Some(75));
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
        assert!(matches!(parse(["-w", "abc"]), Err(ParseError::InvalidNumber(..))));
    }

    #[test]
    fn parse_quality_range() {
        assert!(matches!(parse(["-q", "101"]), Err(ParseError::InvalidNumber(..))));
        assert!(matches!(parse(["-q", "-1"]), Err(ParseError::InvalidNumber(..))));
    }

    #[test]
    fn parse_too_many_args() {
        assert!(matches!(parse(["a.png", "b.png"]), Err(ParseError::TooManyArgs(_))));
    }
}
