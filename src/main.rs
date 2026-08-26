mod cli;
mod detect;
mod halfblock;
mod info;
mod input;
mod kitty;
mod meta;
mod size;

use std::fmt;
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::process::ExitCode;

use detect::Protocol;

fn main() -> ExitCode {
    let args = match cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(cli::ParseError::Help) => {
            print!("{}", cli::USAGE);
            return ExitCode::SUCCESS;
        }
        Err(cli::ParseError::Version) => {
            println!("isee {}", env!("CARGO_PKG_VERSION"));
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("isee: {e}");
            eprint!("{}", cli::USAGE);
            return ExitCode::from(2);
        }
    };
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(AppErr::Usage(e)) => {
            eprintln!("isee: {e}");
            eprint!("{}", cli::USAGE);
            ExitCode::from(2)
        }
        Err(AppErr::Fatal(e)) => {
            eprintln!("isee: {e}");
            ExitCode::FAILURE
        }
    }
}

enum AppErr {
    Usage(String),
    Fatal(String),
}

impl fmt::Display for AppErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppErr::Usage(e) | AppErr::Fatal(e) => write!(f, "{e}"),
        }
    }
}

fn run(args: &cli::Args) -> Result<(), AppErr> {
    let source = match &args.path {
        Some(p) => input::Source::Path(p.clone()),
        None => {
            if detect::is_tty(0) {
                return Err(AppErr::Usage(
                    "no image path and stdin is a terminal".into(),
                ));
            }
            input::Source::Stdin
        }
    };
    let stdout = io::stdout();
    if args.info {
        let info = input::load_info(&source).map_err(|e| AppErr::Fatal(e.to_string()))?;
        let path = match &source {
            input::Source::Path(p) => std::fs::canonicalize(p)
                .map(|x| x.display().to_string())
                .unwrap_or_else(|_| p.display().to_string()),
            input::Source::Stdin => "-".to_string(),
        };
        print!("{}", info::render(&path, &info));
        io::stdout()
            .flush()
            .map_err(|e| AppErr::Fatal(e.to_string()))?;
        return Ok(());
    }

    let term = detect::detect(stdout.as_raw_fd());
    let opts = size::RenderOpts {
        width: args.width,
        quality: args.quality.unwrap_or(50),
        cell: term.cell,
        win: term.win,
    };
    let bounds = match term.protocol {
        Protocol::Kitty => size::kitty_bounds(&opts),
        Protocol::HalfBlocks => size::halfblock_bounds(&opts),
    };
    let loaded = input::load(&source, &opts, bounds).map_err(|e| AppErr::Fatal(e.to_string()))?;
    let mut bytes = match term.protocol {
        Protocol::Kitty => kitty::render(&loaded.img, &opts, kitty::new_image_id()),
        Protocol::HalfBlocks => halfblock::render(&loaded.img, &opts).into_bytes(),
    };
    // Kitty's placement already parks the cursor on the row below the image;
    // appending a newline here would push the prompt even further down.
    if !matches!(term.protocol, Protocol::Kitty) {
        bytes.push(b'\n');
    }
    let bytes = if term.tmux && matches!(term.protocol, Protocol::Kitty) {
        // In tmux only the KGP transfer chunks go through DCS passthrough;
        // placeholder cells must reach tmux's pane grid so the image lands in
        // the right pane and survives redraws. HalfBlocks are plain text and
        // need no passthrough at all.
        detect::wrap_kitty_passthrough(&bytes)
    } else {
        bytes
    };

    let mut out = stdout.lock();
    out.write_all(&bytes)
        .map_err(|e| AppErr::Fatal(e.to_string()))?;
    out.flush().map_err(|e| AppErr::Fatal(e.to_string()))?;
    Ok(())
}
