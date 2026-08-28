mod b64;
mod brand;
mod cli;
mod detect;
mod halfblock;
mod iip;
mod info;
mod input;
mod kitty;
mod meta;
mod sixel;
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
    let sources = build_sources(args)?;
    let stdout = io::stdout();
    if args.info {
        return run_info(&sources, &stdout);
    }
    run_preview(&sources, args, &stdout)
}

/// Build the ordered list of input sources. When no path is supplied the single
/// source is stdin (after verifying stdin is not a terminal); paths are never
/// mixed with stdin.
fn build_sources(args: &cli::Args) -> Result<Vec<input::Source>, AppErr> {
    if args.paths.is_empty() {
        if detect::is_tty(0) {
            return Err(AppErr::Usage(
                "no image path and stdin is a terminal".into(),
            ));
        }
        Ok(vec![input::Source::Stdin])
    } else {
        Ok(args
            .paths
            .iter()
            .map(|p| input::Source::Path(p.clone()))
            .collect())
    }
}

/// The path shown to the user: the exact argument as passed in, so titles and
/// `-i` listings never rewrite the user's path.
fn display_source(source: &input::Source) -> String {
    match source {
        input::Source::Stdin => "-".to_string(),
        input::Source::Path(p) => p.display().to_string(),
    }
}

fn run_info(sources: &[input::Source], stdout: &io::Stdout) -> Result<(), AppErr> {
    let multi = sources.len() > 1;
    let mut out = stdout.lock();
    let mut wrote_before = false;
    let mut failed = 0usize;
    for source in sources {
        let path = display_source(source);
        match input::load_info(source) {
            Ok(info) => {
                emit_info_item(&mut out, multi, wrote_before, &path, &info)
                    .map_err(|e| AppErr::Fatal(e.to_string()))?;
                out.flush().map_err(|e| AppErr::Fatal(e.to_string()))?;
                wrote_before = true;
            }
            Err(e) => {
                if !multi {
                    return Err(AppErr::Fatal(e.to_string()));
                }
                eprintln!("isee: {path}: {e}");
                failed += 1;
            }
        }
    }
    if failed > 0 {
        Err(AppErr::Fatal(failure_summary(failed)))
    } else {
        Ok(())
    }
}

fn run_preview(
    sources: &[input::Source],
    args: &cli::Args,
    stdout: &io::Stdout,
) -> Result<(), AppErr> {
    let multi = sources.len() > 1;
    let term = detect::detect(stdout.as_raw_fd());
    let opts = size::RenderOpts {
        width: args.width,
        quality: args.quality,
        cell: term.cell,
        win: term.win,
        // Bitmap protocols declare sizes in logical points: feed them the
        // terminal's device-pixel scale so previews shrink to point size on
        // Retina. Kitty works in device pixels and Half Blocks in cell
        // units, so both stay at 1.
        dpy_scale: match term.protocol {
            Protocol::Iip | Protocol::Sixel => term.dpy_scale,
            Protocol::Kitty | Protocol::HalfBlocks => 1,
        },
    };
    let bounds = match term.protocol {
        Protocol::Kitty => size::kitty_bounds(&opts),
        Protocol::Iip | Protocol::Sixel => size::bitmap_bounds(&opts),
        Protocol::HalfBlocks => size::halfblock_bounds(&opts),
    };

    let mut out = stdout.lock();

    // Multiple files: decode on a small worker pool while the main thread
    // emits blocks strictly in input order. Single file (and stdin) keeps
    // the plain serial path below.
    if multi {
        return preview_parallel(sources, &term, &opts, bounds, &mut out);
    }

    let mut wrote_before = false;
    for source in sources {
        let path = display_source(source);
        match input::load(source, &opts, bounds) {
            Ok(loaded) => {
                let block = render_block(&loaded.img, &term, &opts);
                emit_preview_item(&mut out, multi, term.protocol, wrote_before, &path, &block)
                    .map_err(|e| AppErr::Fatal(e.to_string()))?;
                out.flush().map_err(|e| AppErr::Fatal(e.to_string()))?;
                wrote_before = true;
            }
            Err(e) => return Err(AppErr::Fatal(e.to_string())),
        }
    }
    Ok(())
}

/// Render a loaded image to a protocol frame (plus tmux DCS passthrough
/// wrapping when needed). Pure function of (img, term, opts): safe to call
/// from worker threads.
fn render_block(
    img: &image::DynamicImage,
    term: &detect::TerminalInfo,
    opts: &size::RenderOpts,
) -> Vec<u8> {
    let mut block = match term.protocol {
        Protocol::Kitty => kitty::render(img, opts, kitty::new_image_id()),
        Protocol::Iip => iip::render(img, opts),
        Protocol::Sixel => sixel::render(img, opts),
        Protocol::HalfBlocks => halfblock::render(img, opts).into_bytes(),
    };
    // Sixel is downgraded to Half Blocks at detect() time (nested
    // DCS escaping does not survive tmux), so only Kitty APC
    // chunks and Iip OSC frames get the passthrough treatment.
    if term.tmux && matches!(term.protocol, Protocol::Kitty | Protocol::Iip) {
        block = detect::wrap_graphics_passthrough(&block);
    }
    block
}

/// Worker count for multi-file previews. Each worker holds at most one
/// decoded block, so peak memory stays at ~4 preview blocks.
const PREVIEW_WORKERS: usize = 4;

/// Multi-file preview pipeline: `PREVIEW_WORKERS` threads decode + render
/// concurrently (JPEG/PNG entropy decoding dominates the runtime and does
/// not parallelize internally), while the main thread writes blocks strictly
/// in input order with a flush after each one, preserving the sequential
/// preview contract. A failed image is reported on stderr at its original
/// position without blocking the rest; write errors (e.g. EPIPE) stop the
/// pipeline promptly.
fn preview_parallel(
    sources: &[input::Source],
    term: &detect::TerminalInfo,
    opts: &size::RenderOpts,
    bounds: (u64, u64),
    out: &mut dyn Write,
) -> Result<(), AppErr> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let next = AtomicUsize::new(0);
    let stop = AtomicUsize::new(0);
    let workers = sources.len().min(PREVIEW_WORKERS).max(1);
    let (tx, rx) = mpsc::channel::<(usize, String, Result<Vec<u8>, String>)>();

    std::thread::scope(|scope| -> Result<(), AppErr> {
        for _ in 0..workers {
            scope.spawn(|| loop {
                if stop.load(Ordering::Relaxed) != 0 {
                    break;
                }
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= sources.len() {
                    break;
                }
                let path = display_source(&sources[i]);
                let res = input::load(&sources[i], opts, bounds)
                    .map_err(|e| e.to_string())
                    .map(|loaded| render_block(&loaded.img, term, opts));
                if tx.send((i, path, res)).is_err() {
                    break;
                }
            });
        }

        // Reorder worker output by input index: hold early finishers until
        // every predecessor has been emitted.
        let mut pending: BTreeMap<usize, (String, Result<Vec<u8>, String>)> = BTreeMap::new();
        let mut expect = 0usize;
        let mut wrote_before = false;
        let mut failed = 0usize;
        while expect < sources.len() {
            let Ok((i, path, res)) = rx.recv() else {
                break;
            };
            pending.insert(i, (path, res));
            while let Some((path, res)) = pending.remove(&expect) {
                match res {
                    Ok(block) => {
                        emit_preview_item(out, true, term.protocol, wrote_before, &path, &block)
                            .map_err(|e| AppErr::Fatal(e.to_string()))?;
                        out.flush().map_err(|e| AppErr::Fatal(e.to_string()))?;
                        wrote_before = true;
                    }
                    Err(e) => {
                        eprintln!("isee: {path}: {e}");
                        failed += 1;
                    }
                }
                expect += 1;
            }
        }
        // Stop idle workers promptly on early exit (e.g. EPIPE from `head`).
        stop.store(1, Ordering::Relaxed);
        if failed > 0 {
            return Err(AppErr::Fatal(failure_summary(failed)));
        }
        Ok(())
    })
}

/// Stream one preview image to `out`, flushing nothing itself. A single image
/// emits no path title; Kitty/Sixel blocks move the cursor off the image
/// themselves and Iip ends with its own newline, so only Half Blocks needs a
/// trailing newline added. Multiple images precede each image with its
/// original path and separate it from the previous one by exactly one blank
/// line. `wrote_before` reflects whether an earlier image was already
/// emitted, so a failed file never blocks later ones.
fn emit_preview_item(
    out: &mut dyn Write,
    multi: bool,
    protocol: Protocol,
    wrote_before: bool,
    path: &str,
    block: &[u8],
) -> io::Result<()> {
    if multi {
        if wrote_before {
            out.write_all(b"\n")?;
        }
        out.write_all(path.as_bytes())?;
        out.write_all(b"\n")?;
    }
    out.write_all(block)?;
    if !multi && matches!(protocol, Protocol::HalfBlocks) {
        out.write_all(b"\n")?;
    }
    Ok(())
}

/// Stream one `-i` listing to `out`. A single listing keeps the current output;
/// multiple listings are separated by one blank line.
fn emit_info_item(
    out: &mut dyn Write,
    multi: bool,
    wrote_before: bool,
    path: &str,
    info: &input::ImageInfo,
) -> io::Result<()> {
    if multi && wrote_before {
        out.write_all(b"\n")?;
    }
    out.write_all(info::render(path, info).as_bytes())?;
    Ok(())
}

fn failure_summary(n: usize) -> String {
    if n == 1 {
        "1 image failed".to_string()
    } else {
        format!("{n} images failed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn some_info() -> input::ImageInfo {
        input::ImageInfo {
            size: 100,
            width: 10,
            height: 5,
            dpi: None,
            alpha: false,
            color: image::ColorType::Rgb8,
        }
    }

    #[test]
    fn single_preview_has_no_title() {
        let mut out = Vec::new();
        emit_preview_item(&mut out, false, Protocol::Kitty, false, "a.png", b"BLOCK").unwrap();
        assert_eq!(out, b"BLOCK");
    }

    #[test]
    fn single_halfblock_gets_trailing_newline() {
        let mut out = Vec::new();
        emit_preview_item(
            &mut out,
            false,
            Protocol::HalfBlocks,
            false,
            "a.png",
            b"BLOCK",
        )
        .unwrap();
        assert_eq!(out, b"BLOCK\n");
    }

    #[test]
    fn single_iip_sixel_blocks_are_verbatim() {
        // Iip/Sixel blocks carry their own cursor-parking CRLFs; a bare
        // trailing newline would add an unwanted blank line.
        for p in [Protocol::Iip, Protocol::Sixel] {
            let mut out = Vec::new();
            emit_preview_item(&mut out, false, p, false, "a.png", b"FRAME\r\n").unwrap();
            assert_eq!(out, b"FRAME\r\n", "{p:?}");
        }
    }

    #[test]
    fn multi_preview_titles_and_blank_line() {
        let mut out = Vec::new();
        emit_preview_item(&mut out, true, Protocol::Kitty, false, "a.png", b"A\r\n").unwrap();
        emit_preview_item(&mut out, true, Protocol::Kitty, true, "b.png", b"B\r\n").unwrap();
        assert_eq!(out, b"a.png\nA\r\n\nb.png\nB\r\n");
    }

    #[test]
    fn multi_halfblock_no_extra_trailing_newline() {
        let mut out = Vec::new();
        emit_preview_item(&mut out, true, Protocol::HalfBlocks, false, "a.png", b"A\n").unwrap();
        emit_preview_item(&mut out, true, Protocol::HalfBlocks, true, "b.png", b"B\n").unwrap();
        assert_eq!(out, b"a.png\nA\n\nb.png\nB\n");
    }

    #[test]
    fn preview_failure_does_not_block_later_images() {
        let mut out = Vec::new();
        let mut wrote_before = false;
        emit_preview_item(
            &mut out,
            true,
            Protocol::Kitty,
            wrote_before,
            "a.png",
            b"A\r\n",
        )
        .unwrap();
        wrote_before = true;
        // a failed image emits nothing and leaves wrote_before untouched, so the
        // next success still gets exactly one separating blank line.
        emit_preview_item(
            &mut out,
            true,
            Protocol::Kitty,
            wrote_before,
            "c.png",
            b"C\r\n",
        )
        .unwrap();
        assert_eq!(out, b"a.png\nA\r\n\nc.png\nC\r\n");
        assert!(wrote_before);
    }

    #[test]
    fn failure_summary_singular_and_plural() {
        assert_eq!(failure_summary(1), "1 image failed");
        assert_eq!(failure_summary(2), "2 images failed");
    }

    #[test]
    fn info_single_matches_render() {
        let mut out = Vec::new();
        emit_info_item(&mut out, false, false, "a.png", &some_info()).unwrap();
        assert_eq!(out, info::render("a.png", &some_info()).as_bytes());
    }

    #[test]
    fn info_multi_separates_blocks() {
        let mut out = Vec::new();
        emit_info_item(&mut out, true, false, "a.png", &some_info()).unwrap();
        emit_info_item(&mut out, true, true, "b.png", &some_info()).unwrap();
        let expected = format!(
            "{}\n{}",
            info::render("a.png", &some_info()),
            info::render("b.png", &some_info())
        );
        assert_eq!(out, expected.as_bytes());
    }

    #[test]
    fn info_failure_leaves_gap_for_later() {
        let mut out = Vec::new();
        let mut wrote_before = false;
        emit_info_item(&mut out, true, wrote_before, "a.png", &some_info()).unwrap();
        wrote_before = true;
        emit_info_item(&mut out, true, wrote_before, "b.png", &some_info()).unwrap();
        assert_eq!(
            out,
            format!(
                "{}\n{}",
                info::render("a.png", &some_info()),
                info::render("b.png", &some_info())
            )
            .as_bytes()
        );
    }

    #[test]
    fn display_source_raw_and_stdin() {
        let src = input::Source::Path(PathBuf::from("foo//bar.png"));
        assert_eq!(display_source(&src), "foo//bar.png");
        assert_eq!(display_source(&input::Source::Stdin), "-");
    }
}
