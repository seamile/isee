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

/// Exit-time repair for interrupted graphics output, chafa's `fast_exit`
/// re-worked in Rust. A graphics frame is one long escape sequence (kitty
/// APC, sixel DCS, OSC 1337) that only terminates at its trailing ST; a
/// SIGINT mid-write (e.g. Ctrl+C during the ~3.5 s pty stream of a large
/// image) leaves the terminal waiting for the rest, so the following shell
/// prompt is swallowed as payload. Before dying we write CAN (`0x18`) +
/// ST (`ESC \`), which closes any half-open APC/DCS/OSC immediately.
extern "C" fn fast_exit_handler(_: libc::c_int) {
    // write(2)/signal(2)/raise(2) are async-signal-safe; write into the raw
    // fd instead of the Rust stdout handle (non-reentrant, may hold a lock).
    let seq: [u8; 3] = [0x18, 0x1b, 0x5c];
    unsafe {
        libc::write(1, seq.as_ptr().cast(), seq.len());
        // Installing this handler replaced the default terminate action; die
        // the way SIGINT would have (shell sees 130) after the repair write.
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

fn install_fast_exit() {
    // Reroute SIGINT from default-die to the repair-then-die handler.
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = fast_exit_handler as *const () as usize;
        sa.sa_flags = libc::SA_RESTART;
        // sa_mask differs between libc targets; zero it via the union-free
        // fields both macOS and Linux expose.
        #[cfg(target_os = "macos")]
        {
            sa.sa_mask = std::mem::zeroed();
        }
        #[cfg(not(target_os = "macos"))]
        {
            libc::sigemptyset(&mut sa.sa_mask);
        }
        libc::sigaction(libc::SIGINT, &sa, std::ptr::null_mut());
    }
}

impl fmt::Display for AppErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AppErr::Usage(e) | AppErr::Fatal(e) => write!(f, "{e}"),
        }
    }
}

fn run(args: &cli::Args) -> Result<(), AppErr> {
    install_fast_exit();
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
    let term = detect::detect(stdout.as_raw_fd(), args.protocol);
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
        // Kitty renders through the placeholder grid inside tmux (the pane's
        // cursor model cannot track the outer terminal's placement moves).
        tmux: term.tmux,
        transfer: term.kgp_transfer,
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
        return preview_parallel(sources, &term, &opts, bounds, args.animate, &mut out);
    }

    let mut wrote_before = false;
    for source in sources {
        let path = display_source(source);
        match input::load(source, &opts, bounds, args.animate) {
            Ok(loaded) => {
                note_protocol_clamp(&loaded, &term, &opts);
                let block = render_loaded(&loaded, &term, &opts);
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

/// Warn (at most once per run) when the kitty placeholder cap — not the
/// terminal — forced a preview smaller than it would otherwise be shown.
/// Other protocols have no placeholder grid and are never capped by it.
fn note_protocol_clamp(
    loaded: &input::Loaded,
    term: &detect::TerminalInfo,
    opts: &size::RenderOpts,
) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    if !matches!(term.protocol, Protocol::Kitty) {
        return;
    }
    if let Some(msg) = size::kitty_protocol_clamp_notice(loaded.dims(), opts) {
        ONCE.call_once(|| eprintln!("{msg}"));
    }
}

/// Tmux passthrough for graphics escapes: wrap each transfer frame in its
/// own DCS passthrough while plain text and SGR colours flow through the
/// pane grid. Kitty APC chunks and Iip OSC frames always ride it; Sixel DCS
/// frames only when the user forced `-p sixel` (detection downgrades
/// unforced sixel to Half Blocks inside tmux).
fn wrap_tmux(block: Vec<u8>, term: &detect::TerminalInfo) -> Vec<u8> {
    if term.tmux
        && matches!(
            term.protocol,
            Protocol::Kitty | Protocol::Iip | Protocol::Sixel
        )
    {
        detect::wrap_graphics_passthrough(&block)
    } else {
        block
    }
}

/// Render a loaded source to a protocol block. An animation renders animated
/// on Kitty (native animation protocol) and, when it is a GIF, on
/// iTerm2/mintty (OSC 1337 passes the raw GIF through for the terminal to
/// play); animated WebPs and every other protocol show the first frame as a
/// static image. Pure function of (loaded, term, opts): safe to call from
/// worker threads.
fn render_loaded(
    loaded: &input::Loaded,
    term: &detect::TerminalInfo,
    opts: &size::RenderOpts,
) -> Vec<u8> {
    match loaded {
        input::Loaded::Static(img) => render_block(img, term, opts),
        input::Loaded::Anim(anim) => {
            let animated = match term.protocol {
                Protocol::Kitty => true,
                // Only the OSC 1337 brands that actually play GIF payloads
                // animate; the rest (Warp, VSCode, ...) show the first frame.
                Protocol::Iip => {
                    anim.kind == input::AnimKind::Gif
                        && matches!(
                            term.brand,
                            Some(brand::Brand::Iterm2) | Some(brand::Brand::Mintty)
                        )
                }
                _ => false,
            };
            let block = if animated {
                match term.protocol {
                    Protocol::Kitty => kitty::render_animation(
                        &anim.frames,
                        anim.loop_count,
                        opts,
                        kitty::new_image_id(),
                    ),
                    _ => iip::render_gif_raw(&anim.raw, opts, &anim.frames[0].img),
                }
            } else {
                render_block(&anim.frames[0].img, term, opts)
            };
            wrap_tmux(block, term)
        }
    }
}

/// Render a loaded image to a protocol frame (plus tmux DCS passthrough
/// wrapping when needed). Pure function of (img, term, opts): safe to call
/// from worker threads.
fn render_block(
    img: &image::DynamicImage,
    term: &detect::TerminalInfo,
    opts: &size::RenderOpts,
) -> Vec<u8> {
    let block = match term.protocol {
        Protocol::Kitty => kitty::render(img, opts, kitty::new_image_id()),
        Protocol::Iip => iip::render(img, opts),
        Protocol::Sixel => sixel::render(img, opts, sixel_advances_cursor(term)),
        Protocol::HalfBlocks => halfblock::render(img, opts).into_bytes(),
    };
    wrap_tmux(block, term)
}

/// Whether the terminal advances the cursor past the image on its own while
/// placing a Sixel, so only one trailing CRLF is needed to park the prompt
/// one line below. WezTerm does this (verified from its `assign_image_to_cells`
/// source: the cursor ends at the image's last row), and iTerm2 + VSCode
/// measured the same class of behavior (a bare CRLF-per-device-cell count
/// leaves 2x the displayed height of blank rows, i.e. the terminal already
/// moved the cursor and isee's rows stack on top). Recognized from env vars
/// or the XTVERSION probe.
fn sixel_advances_cursor(term: &detect::TerminalInfo) -> bool {
    matches!(
        term.brand,
        Some(brand::Brand::WezTerm) | Some(brand::Brand::Iterm2) | Some(brand::Brand::Vscode)
    )
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
    bounds: size::Bounds,
    animate: bool,
    out: &mut dyn Write,
) -> Result<(), AppErr> {
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;

    let next = AtomicUsize::new(0);
    let stop = AtomicUsize::new(0);
    let workers = sources.len().clamp(1, PREVIEW_WORKERS);
    let (tx, rx) = mpsc::channel::<(usize, String, Result<Vec<u8>, String>)>();

    std::thread::scope(|scope| -> Result<(), AppErr> {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    if stop.load(Ordering::Relaxed) != 0 {
                        break;
                    }
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    if i >= sources.len() {
                        break;
                    }
                    let path = display_source(&sources[i]);
                    let res = input::load(&sources[i], opts, bounds, animate);
                    if let Ok(loaded) = &res {
                        note_protocol_clamp(loaded, term, opts);
                    }
                    let res = res
                        .map_err(|e| e.to_string())
                        .map(|loaded| render_loaded(&loaded, term, opts));
                    if tx.send((i, path, res)).is_err() {
                        break;
                    }
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

    #[test]
    fn loaded_dims_static_and_anim_canvas() {
        let img = image::DynamicImage::new_rgba8(4, 4);
        assert_eq!(input::Loaded::Static(img).dims(), (4, 4));
        // Animations report the raw header canvas, not the resized frames.
        let mut raw = b"GIF89a".to_vec();
        raw.extend_from_slice(&1000u16.to_le_bytes());
        raw.extend_from_slice(&6000u16.to_le_bytes());
        let anim = input::Animation {
            kind: input::AnimKind::Gif,
            raw,
            frames: vec![
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(30, 30),
                    delay_ms: 50,
                },
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(30, 30),
                    delay_ms: 50,
                },
            ],
            loop_count: image::metadata::LoopCount::Infinite,
        };
        assert_eq!(input::Loaded::Anim(anim).dims(), (1000, 6000));
        // A WebP canvas comes from the VP8X chunk's u24 (width-1, height-1).
        let mut webp_raw = b"RIFF\x12\x00\x00\x00WEBP".to_vec();
        webp_raw.extend_from_slice(b"VP8X");
        webp_raw.extend_from_slice(&10u32.to_le_bytes());
        webp_raw.extend_from_slice(&[0, 0, 0, 0]); // flags + reserved
        webp_raw.extend_from_slice(&[0x87, 0x13, 0x00]); // width-1 = 4999
        webp_raw.extend_from_slice(&[0xbf, 0x2b, 0x00]); // height-1 = 11199
        let anim = input::Animation {
            kind: input::AnimKind::Webp,
            raw: webp_raw,
            frames: vec![
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(30, 30),
                    delay_ms: 50,
                },
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(30, 30),
                    delay_ms: 50,
                },
            ],
            loop_count: image::metadata::LoopCount::Infinite,
        };
        assert_eq!(input::Loaded::Anim(anim).dims(), (5000, 11200));
    }

    // ---- GIF animation dispatch ----

    fn opts() -> size::RenderOpts {
        size::RenderOpts {
            width: None,
            quality: size::Quality::default(),
            cell: detect::CellPx { w: 9, h: 18 },
            win: detect::WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            dpy_scale: 1,
            tmux: false,
            transfer: size::KgpTransfer::Stream,
        }
    }

    fn term_of(
        protocol: Protocol,
        brand: Option<brand::Brand>,
        tmux: bool,
    ) -> detect::TerminalInfo {
        detect::TerminalInfo {
            protocol,
            cell: detect::CellPx { w: 9, h: 18 },
            win: detect::WinSize {
                cols: 80,
                rows: 24,
                px: None,
            },
            tmux,
            dpy_scale: 1,
            probed_scale: None,
            brand,
            kgp_transfer: size::KgpTransfer::Stream,
        }
    }

    fn gif_loaded() -> input::Loaded {
        input::Loaded::Anim(input::Animation {
            kind: input::AnimKind::Gif,
            raw: b"GIF89a-fake-frames".to_vec(),
            frames: vec![
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(4, 4),
                    delay_ms: 50,
                },
                input::AnimFrame {
                    img: image::DynamicImage::new_rgba8(4, 4),
                    delay_ms: 50,
                },
            ],
            loop_count: image::metadata::LoopCount::Infinite,
        })
    }

    /// Whether an Iip block's OSC 1337 payload is a raw GIF ("R0lG" is
    /// base64 for "GIF8").
    fn osc_payload_is_gif(block: &[u8]) -> bool {
        let s = std::str::from_utf8(block).unwrap();
        let start = s.find(':').unwrap() + 1;
        let end = s.find('\u{7}').unwrap();
        s[start..end].starts_with("R0lG")
    }

    #[test]
    fn gif_animates_on_kitty_protocol() {
        let term = term_of(Protocol::Kitty, Some(brand::Brand::Ghostty), false);
        let block = render_loaded(&gif_loaded(), &term, &opts());
        let s = String::from_utf8_lossy(&block);
        assert!(s.contains("\x1b_Ga=f"), "frame transfers: {s}");
        assert!(s.contains("\x1b_Ga=a,i="), "animation controls: {s}");
    }

    #[test]
    fn gif_passes_raw_through_on_iterm2_and_mintty_only() {
        let o = opts();
        for brand in [Some(brand::Brand::Iterm2), Some(brand::Brand::Mintty)] {
            let term = term_of(Protocol::Iip, brand, false);
            assert!(
                osc_payload_is_gif(&render_loaded(&gif_loaded(), &term, &o)),
                "{brand:?} must pass the raw GIF through"
            );
        }
        for brand in [Some(brand::Brand::Warp), Some(brand::Brand::Vscode), None] {
            let term = term_of(Protocol::Iip, brand, false);
            assert!(
                !osc_payload_is_gif(&render_loaded(&gif_loaded(), &term, &o)),
                "{brand:?} must fall back to the first frame"
            );
        }
    }

    #[test]
    fn sixel_advance_brands_are_whitelisted() {
        // WezTerm/iTerm2/VSCode move the cursor past a placed Sixel
        // automatically; every other brand must use the full row count.
        for brand in [
            Some(brand::Brand::WezTerm),
            Some(brand::Brand::Iterm2),
            Some(brand::Brand::Vscode),
        ] {
            let term = term_of(Protocol::Sixel, brand, false);
            assert!(sixel_advances_cursor(&term), "{brand:?} advances");
        }
        for brand in [
            Some(brand::Brand::KittyFamily),
            Some(brand::Brand::Foot),
            Some(brand::Brand::Tabby),
            None,
        ] {
            let term = term_of(Protocol::Sixel, brand, false);
            assert!(!sixel_advances_cursor(&term), "{brand:?} does not advance");
        }
    }

    #[test]
    fn gif_tmux_wraps_animation_escapes() {
        let term = term_of(Protocol::Kitty, Some(brand::Brand::Ghostty), true);
        let mut o = opts();
        o.tmux = true;
        let block = render_loaded(&gif_loaded(), &term, &o);
        assert!(block.starts_with(b"\x1bPtmux;"), "{:?}", &block[..32]);
    }
}
