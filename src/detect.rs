use std::env;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

use crate::size::KgpTransfer;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Kitty,
    Iip,
    Sixel,
    HalfBlocks,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CellPx {
    pub w: u32,
    pub h: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct WinSize {
    pub cols: u32,
    pub rows: u32,
    /// Text-area size in device pixels from TIOCGWINSZ (ws_xpixel/ws_ypixel),
    /// as reported by kitty/Ghostty/iTerm2. None when the ioctl reports no
    /// pixel geometry (pipes, some terminals).
    pub px: Option<(u32, u32)>,
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalInfo {
    pub protocol: Protocol,
    pub cell: CellPx,
    pub win: WinSize,
    pub tmux: bool,
    /// Device pixels per logical point requested via `ISEE_DPI_SCALE`
    /// (1 when unset). Note the units of `win.px` are brand-dependent:
    /// Warp reports logical points, kitty/Ghostty/iTerm2 report device
    /// pixels — do not feed `win.px` straight into a bitmap-px bound.
    pub dpy_scale: u32,
    /// Device pixels per logical point as probed from the terminal itself
    /// (iTerm2's OSC 1337 ReportCellSize is the only known source).
    /// Informational for now: rendering still keys off `dpy_scale`.
    pub probed_scale: Option<f32>,
    /// Env-recognized terminal brand (see `brand::detect`); gates
    /// brand-specific behavior such as the Iip GIF-animation whitelist.
    pub brand: Option<crate::brand::Brand>,
    /// True only when `-p` chose the protocol (`auto` and `ISEE_PROTOCOL`
    /// are NOT forced): rendering never second-guesses a forced protocol —
    /// the iTerm2 KGP-animation downgrade keys off this.
    pub forced: bool,
    /// How kitty-protocol payloads should reach the terminal: probed
    /// tempfile support (`t=1`), or the `ISEE_KGP_TRANSFER` override.
    pub kgp_transfer: KgpTransfer,
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const KGP_PROBE: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const CSI_CELL_PX: &[u8] = b"\x1b[16t";
const OSC_CELL_SIZE: &[u8] = b"\x1b]1337;ReportCellSize\x1b\\";
const XTVERSION_Q: &[u8] = b"\x1b[>q";
const DA1_Q: &[u8] = b"\x1b[0c";
const MAX_RESPONSE: usize = 4096;

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn detect(stdout_fd: i32, forced: Option<Protocol>) -> TerminalInfo {
    let tmux = in_tmux();
    let stdout_tty = is_tty(stdout_fd);
    let override_p = isee_override();
    let env_kitty = env_kitty_hint();

    let mut brand = crate::brand::detect(|name| env::var(name).ok());
    let mut cell = fallback_cell();
    let mut probed_scale = None;

    // Probing runs on every tty, not just brand-recognized terminals: the
    // batch asks the terminal to identify itself (XTVERSION) and report its
    // capabilities (DA1), which covers unknown terminals whose env vars do
    // not survive ssh.
    let (probed_kitty, probed_file, _probed_sixel, protocol) = if stdout_tty
        && let Some(mut tty) = RawTty::new()
    {
        // Inside tmux the pane's own terminal is tmux's virtual terminal;
        // probe the OUTER terminal by wrapping the queries in tmux's DCS
        // passthrough (mirroring yazi), after making sure passthrough is on.
        if tmux {
            enable_tmux_passthrough();
        }
        // Forced kitty still needs the tempfile-transport confirmation, and
        // an UNKNOWN brand needs the full scan (XTVERSION identifies it,
        // DA1 reports sixel, KGP probes decide kitty). A brand-table hit, by
        // contrast, already carries the verdict from the measured matrix —
        // probing it again burns a full scan and leaks the KGP APC on the
        // very terminals KGP cannot render on. So the batch runs only when
        // `want_kgp` is true; otherwise we trust the env-detected brand.
        let want_kgp = forced == Some(Protocol::Kitty)
            || (forced.is_none() && brand.and_then(crate::brand::preferred_protocol).is_none());
        let answers = if want_kgp {
            probe_batch(&mut tty, tmux, true)
        } else {
            ProbeAnswers::default()
        };
        if let Some(b) = answers.brand {
            brand = Some(b);
        }

        let protocol = select_protocol(
            forced,
            override_p,
            answers.kgp,
            env_kitty,
            brand,
            answers.sixel,
            tmux,
        );
        // Leftover bytes were already scanned by the batch's classify loop;
        // drop them so they cannot be misread as the cell-size answer below.
        tty.pending.clear();

        if protocol != Protocol::HalfBlocks {
            let iterm2 = brand == Some(crate::brand::Brand::Iterm2);
            if iterm2 {
                // iTerm2 never answers `CSI 16 t` (that path would just burn
                // a full probe timeout), but it replies to the OSC 1337
                // ReportCellSize query with the cell size in logical points
                // PLUS the device-pixel scale. Fall back to the generic
                // probe if the answer does not parse.
                if let Some((c, s)) = probe_report_cell_size(&mut tty, tmux) {
                    cell = c;
                    probed_scale = Some(s);
                } else if let Some(c) = probe_cell_px(&mut tty, tmux) {
                    cell = c;
                }
            } else if let Some(c) = probe_cell_px(&mut tty, tmux) {
                cell = c;
            }
        }
        drain_input(tty.file.as_raw_fd());
        (answers.kgp, answers.kgp_file, answers.sixel, protocol)
    } else {
        // Non-tty stdout (or no /dev/tty): no probing possible, pick from
        // env alone. A forced protocol still applies.
        let protocol = select_protocol(forced, override_p, false, env_kitty, brand, false, tmux);
        (false, false, false, protocol)
    };

    let win = win_size(stdout_fd);
    // Default 1 = native-px semantics. How a declared `Npx` size renders is
    // BRAND-dependent (measured on a 2x Retina display, fullscreen): Warp
    // draws one declared px as one logical point (QuickLook-like), while
    // iTerm2 draws it as one DEVICE pixel — so the same bitmap+declaration
    // shows twice as wide on Warp. Kitty (device px) and Half Blocks (cell
    // units) always stay at 1. `ISEE_DPI_SCALE=2` shrinks the bitmap to
    // point size before encoding; that reaches the QuickLook intent on Warp
    // but halves it again on iTerm2 (there, `-w 2x` means QuickLook size).
    let dpy_scale = isee_dpi_scale().unwrap_or(1);
    // Brand-table selection skips the KGP probe for recognized terminals, so
    // its tempfile verdict is missing for every brand that prefers KGP
    // (kitty, Ghostty, WezTerm, iTerm2, VSCode — they all render kitty
    // graphics and accept a temp-file transfer by protocol design); default
    // them to tempfile so the skipped probe does not silently cost the
    // direct-placement speedup. `ISEE_KGP_TRANSFER=stream` still opts out.
    //
    // When the probe DID run (`probed_kitty`), its tempfile verdict is
    // authoritative and must win over the brand default. Warp is the one
    // brand whose current release rejects `t=t` (FileError
    // UnsupportedPlatform), so it must never default to Tempfile: when its
    // probe is skipped it streams, and when its probe runs it trusts the
    // (negative) verdict.
    let brand_kgp = brand
        .and_then(crate::brand::preferred_protocol)
        .is_some_and(|p| p == Protocol::Kitty);
    let brand_tempfile = brand_kgp && brand != Some(crate::brand::Brand::Warp);
    let probed_tempfile = if probed_kitty {
        probed_file
    } else {
        brand_tempfile
    };
    let kgp_transfer = kgp_transfer_choice(
        probed_tempfile,
        env::var("ISEE_KGP_TRANSFER").ok().as_deref(),
    );

    let info = TerminalInfo {
        protocol,
        cell,
        win,
        tmux,
        dpy_scale,
        probed_scale,
        brand,
        forced: forced.is_some(),
        kgp_transfer,
    };
    if env::var("ISEE_DEBUG").is_ok() {
        eprintln!(
            "isee: protocol={:?} cell={}x{} win={}x{} px={:?} scale={} probed_scale={:?} tmux={} transfer={:?} brand={:?} forced={}",
            info.protocol,
            info.cell.w,
            info.cell.h,
            info.win.cols,
            info.win.rows,
            info.win.px,
            info.dpy_scale,
            info.probed_scale,
            tmux,
            info.kgp_transfer,
            info.brand,
            info.forced
        );
    }
    info
}

/// Protocol priority: `-p` > `ISEE_PROTOCOL` > brand table (env- or
/// XTVERSION-derived; measured support matrix, KGP-first) > probed KGP /
/// kitty env hint > probed DA1 sixel > Half Blocks. The brand table wins over
/// probing so recognized terminals skip the KGP query in detect()
/// (`want_kgp`); unknown brands still ride the probe. Sixel inside tmux is
/// downgraded to Half Blocks unless it was forced with `-p sixel` — forced
/// sixel rides the DCS passthrough wrapper instead.
fn select_protocol(
    forced: Option<Protocol>,
    override_p: Option<Protocol>,
    probed_kitty: bool,
    env_kitty: bool,
    brand: Option<crate::brand::Brand>,
    probed_sixel: bool,
    tmux: bool,
) -> Protocol {
    let mut protocol = match forced {
        Some(p) => p,
        None => match override_p {
            Some(p) => p,
            None => brand
                .and_then(crate::brand::preferred_protocol)
                .or_else(|| (probed_kitty || env_kitty).then_some(Protocol::Kitty))
                .or_else(|| probed_sixel.then_some(Protocol::Sixel))
                .unwrap_or(Protocol::HalfBlocks),
        },
    };
    if tmux && protocol == Protocol::Sixel && forced != Some(Protocol::Sixel) {
        protocol = Protocol::HalfBlocks;
    }
    protocol
}

fn in_tmux() -> bool {
    env::var("TMUX").is_ok() || env::var("TERM_PROGRAM").is_ok_and(|v| v == "tmux")
}

fn isee_override() -> Option<Protocol> {
    let v = env::var("ISEE_PROTOCOL").ok()?.to_ascii_lowercase();
    match v.as_str() {
        "kitty" => Some(Protocol::Kitty),
        "iip" => Some(Protocol::Iip),
        "sixel" => Some(Protocol::Sixel),
        // "halfblocks" is accepted as an alias; unknown values fall back to
        // Half Blocks silently, matching the pre-existing behavior.
        _ => Some(Protocol::HalfBlocks),
    }
}

/// `ISEE_DPI_SCALE=1|2` forces the bitmap display scale; unset, "auto", or
/// any other value keeps the default 1 (native-px, imgcat-matching).
fn isee_dpi_scale() -> Option<u32> {
    match env::var("ISEE_DPI_SCALE").ok()?.trim() {
        "1" => Some(1),
        "2" => Some(2),
        _ => None,
    }
}

fn env_kitty_hint() -> bool {
    if env::var("KITTY_WINDOW_ID").is_ok() {
        return true;
    }
    if env::var("GHOSTTY_RESOURCES_DIR").is_ok() {
        return true;
    }
    if env::var("WEZTERM_EXECUTABLE").is_ok() {
        return true;
    }
    match env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "ghostty" | "wezterm" | "rio" | "kitty" => return true,
        _ => {}
    }
    match env::var("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "xterm-kitty" | "xterm-ghostty" | "ghostty" | "wezterm" | "rio" | "kitty" => return true,
        _ => {}
    }
    false
}

fn fallback_cell() -> CellPx {
    match env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "ghostty" => CellPx { w: 9, h: 18 },
        "wezterm" | "rio" => CellPx { w: 8, h: 16 },
        _ => CellPx { w: 7, h: 14 },
    }
}

fn win_size(fd: i32) -> WinSize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            let px = if ws.ws_xpixel > 0 && ws.ws_ypixel > 0 {
                Some((ws.ws_xpixel as u32, ws.ws_ypixel as u32))
            } else {
                None
            };
            WinSize {
                cols: ws.ws_col as u32,
                rows: ws.ws_row as u32,
                px,
            }
        } else {
            WinSize {
                cols: 80,
                rows: 24,
                px: None,
            }
        }
    }
}

/// Let this pane's DCS passthrough sequences reach the outer terminal and
/// survive the trip: tmux caps input buffers small by default, which
/// truncates large passthrough image transfers (mirrors yazi's tmux_setup()).
fn enable_tmux_passthrough() {
    use std::process::{Command, Stdio};
    let quiet = |cmd: &mut Command| {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok()
    };
    quiet(Command::new("tmux").args(["set", "-p", "allow-passthrough", "all"]));
    quiet(Command::new("tmux").args(["set", "-s", "input-buffer-size", "104857600"]));
}

/// Resolve the KGP payload transport: `ISEE_KGP_TRANSFER` forces a mode
/// (`tempfile`/`file`, or `stream`), otherwise the tempfile mode rides on the
/// probe result (the terminal answered OK to a `t=1` query); anything else
/// streams the payload through the pty.
fn kgp_transfer_choice(probed_tempfile: bool, forced: Option<&str>) -> KgpTransfer {
    let forced = forced.map(|v| v.trim().to_ascii_lowercase());
    match forced.as_deref() {
        Some("tempfile") | Some("file") => KgpTransfer::Tempfile,
        Some("stream") => KgpTransfer::Stream,
        _ if probed_tempfile => KgpTransfer::Tempfile,
        _ => KgpTransfer::Stream,
    }
}

fn write_probe(tty: &mut RawTty, seq: &[u8], passthrough: bool) -> bool {
    tty.file.write_all(&probe_bytes(seq, passthrough)).is_ok()
}

fn probe_bytes(seq: &[u8], passthrough: bool) -> Vec<u8> {
    if passthrough {
        wrap_passthrough(seq)
    } else {
        seq.to_vec()
    }
}

/// One batched capability probe, mirroring yazi's approach: wrap every
/// query between SaveCursor/RestoreCursor so the terminal's cursor survives,
/// and end with an EraseLine that scrubs the APC payload the KGP query leaks
/// as visible text on unsupporting terminals. Queries: XTVERSION (brand
/// self-report), DA1 (primary device attributes; attribute 4 = sixel), and
/// the two KGP queries (`t=d` streaming, `t=t` tempfile) when the kitty
/// protocol is still a candidate. All responses share one deadline; each
/// `read_until(ST)` chunk is classified as it arrives.
#[derive(Default)]
struct ProbeAnswers {
    kgp: bool,
    kgp_file: bool,
    sixel: bool,
    brand: Option<crate::brand::Brand>,
}

fn probe_batch(tty: &mut RawTty, passthrough: bool, want_kgp: bool) -> ProbeAnswers {
    let mut batch = Vec::with_capacity(256);
    batch.extend_from_slice(b"\x1b7"); // DECSC: save cursor
    batch.extend_from_slice(&probe_bytes(XTVERSION_Q, passthrough));
    batch.extend_from_slice(&probe_bytes(DA1_Q, passthrough));
    let mut probe_file = None;
    if want_kgp {
        let (queries, file) = kgp_queries(passthrough);
        batch.extend_from_slice(&queries);
        probe_file = file;
    }
    // Erase the leak line, then DECRC: restore the saved cursor.
    batch.extend_from_slice(b"\x1b[1G\x1b[2K\x1b8");

    let have_file = probe_file.is_some();
    if tty.file.write_all(&batch).is_err() {
        if let Some(f) = probe_file {
            let _ = std::fs::remove_file(f);
        }
        return ProbeAnswers::default();
    }

    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut chunks: Vec<String> = Vec::new();
    while !batch_resolved(&chunks, want_kgp, want_kgp && have_file) {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match read_until(tty, b"\x1b\\", deadline - now) {
            ProbeRead::Found(r) => {
                dbg_dump("batch", &r);
                chunks.push(String::from_utf8_lossy(&r).into_owned());
            }
            ProbeRead::Timeout(r) => {
                dbg_dump("batch-timeout", &r);
                // The leftover holds ST-less answers (DA1 replies in
                // particular) — classify it before giving up.
                if !r.is_empty() {
                    chunks.push(String::from_utf8_lossy(&r).into_owned());
                }
                break;
            }
        }
    }
    if let Some(f) = probe_file {
        let _ = std::fs::remove_file(f);
    }
    classify_batch(&chunks)
}

/// Have all expected answers been seen in `chunks`? The DA1 reply carries no
/// ST terminator, so it usually shows up either merged into an ST-terminated
/// chunk or in the Timeout leftover.
fn batch_resolved(chunks: &[String], want_kgp: bool, want_file: bool) -> bool {
    let version = chunks.iter().any(|s| s.contains(">|"));
    let da1 = chunks.iter().any(|s| da1_attrs(s).is_some());
    let want = usize::from(want_kgp) + usize::from(want_file);
    version && da1 && kgp_resolved_ids(chunks, want_file) >= want
}

fn classify_batch(chunks: &[String]) -> ProbeAnswers {
    let mut a = ProbeAnswers::default();
    for s in chunks {
        // One read can carry several responses merged together; split on the
        // ST terminator so each response's verdict stays independent (a
        // merged OK + EBADF pair must not inherit each other's verdict).
        let segs = s.split("\x1b\\");
        let (d, f) = kgp_probe_verdicts(&segs.collect::<Vec<_>>());
        a.kgp |= d;
        a.kgp_file |= f;
        if let Some(attrs) = da1_attrs(s) {
            a.sixel = attrs.contains(&4);
        }
        if let Some(name) = xtversion_name(s)
            && let Some(b) = crate::brand::from_version_str(name)
        {
            a.brand = Some(b);
        }
    }
    a
}

/// Build the two KGP probe queries (`t=d` streaming + `t=t` tempfile). The
/// second carries a REAL temp file path — kitty validates `t=t` queries by
/// opening the payload as a file, so an arbitrary payload answers EBADF even
/// on a tempfile-capable kitty (observed live: `EBADF: Failed to open file
/// for graphics transmission`). The probe file's name carries kitty's
/// `tty-graphics-protocol` marker so kitty deletes it after reading; we also
/// unlink it ourselves in case that cleanup does not run.
/// Returns the query bytes plus the temp file path when one was created.
fn kgp_queries(passthrough: bool) -> (Vec<u8>, Option<std::path::PathBuf>) {
    let probe_file = std::env::temp_dir().join(format!(
        "kitty-tty-graphics-protocol-isee-probe-{}",
        std::process::id()
    ));
    let have_file = std::fs::write(&probe_file, [0u8, 0, 0]).is_ok();
    let mut both = probe_bytes(KGP_PROBE, passthrough);
    if have_file {
        let q = format!(
            "\x1b_Gi=32,s=1,v=1,a=q,t=t,f=24;{}\x1b\\",
            base64_encode_path(&probe_file)
        );
        both.extend_from_slice(&probe_bytes(q.as_bytes(), passthrough));
    }
    (both, have_file.then_some(probe_file))
}

/// How many of the wanted probe ids (i=31 streaming, i=32 tempfile) have
/// been resolved by `chunks`? Unrecognized chunks are ignored and the read
/// loop keeps going until the shared deadline.
fn kgp_resolved_ids(chunks: &[String], want_file: bool) -> usize {
    let mut n = usize::from(chunks.iter().any(|s| s.contains("i=31")));
    if want_file && chunks.iter().any(|s| s.contains("i=32")) {
        n += 1;
    }
    n
}

fn base64_encode_path(path: &std::path::Path) -> String {
    crate::b64::base64_encode(path.to_string_lossy().as_bytes())
}

/// Classify the collected probe responses into `(streaming_ok, tempfile_ok)`.
/// Kitty echoes the request id on both OK and ERR responses, so one
/// contains() check per response segment resolves either outcome.
fn kgp_probe_verdicts(chunks: &[impl AsRef<str>]) -> (bool, bool) {
    chunks.iter().fold((false, false), |(d, f), s| {
        let s = s.as_ref();
        (
            if s.contains("i=31") {
                s.contains("OK")
            } else {
                d
            },
            if s.contains("i=32") {
                s.contains("OK")
            } else {
                f
            },
        )
    })
}

fn probe_cell_px(tty: &mut RawTty, passthrough: bool) -> Option<CellPx> {
    if !write_probe(tty, CSI_CELL_PX, passthrough) {
        return None;
    }
    let r = match read_until(tty, b"t", PROBE_TIMEOUT) {
        ProbeRead::Found(r) => {
            dbg_dump("cell", &r);
            r
        }
        ProbeRead::Timeout(r) => {
            dbg_dump("cell-timeout", &r);
            return None;
        }
    };
    parse_csi_t(&r)
}

/// iTerm2's OSC 1337 cell-size query. The reply carries the cell in logical
/// points plus the display scale, which also makes it a scale probe.
fn probe_report_cell_size(tty: &mut RawTty, passthrough: bool) -> Option<(CellPx, f32)> {
    if !write_probe(tty, OSC_CELL_SIZE, passthrough) {
        return None;
    }
    let r = match read_until(tty, b"\x1b\\", PROBE_TIMEOUT) {
        ProbeRead::Found(r) => {
            dbg_dump("cellsize", &r);
            r
        }
        ProbeRead::Timeout(r) => {
            dbg_dump("cellsize-timeout", &r);
            return None;
        }
    };
    parse_report_cell_size(&r)
}

/// Parse `ESC]1337;ReportCellSize=<height>;<width>;<scale>ESC\` (or a
/// BEL-terminated variant). Order is HEIGHT first, values are floats;
/// verified against iTerm2 on a 2x display: `=16.0;7.0;2.0` with the
/// window report cross-checking 7x16 pt * 2 = 14x32 device px.
fn parse_report_cell_size(raw: &[u8]) -> Option<(CellPx, f32)> {
    let s = std::str::from_utf8(raw).ok()?;
    let idx = s.find("ReportCellSize=")?;
    // Payload runs to the ST or BEL terminator.
    let body = s[idx + "ReportCellSize=".len()..]
        .split(['\x1b', '\x07'])
        .next()?;
    let mut parts = body.split(';');
    let h: f32 = parts.next()?.trim().parse().ok()?;
    let w: f32 = parts.next()?.trim().parse().ok()?;
    let scale: f32 = parts.next()?.trim().parse().ok()?;
    if h <= 0.0 || w <= 0.0 || scale <= 0.0 {
        return None;
    }
    Some((
        CellPx {
            w: w.round() as u32,
            h: h.round() as u32,
        },
        scale,
    ))
}

/// Parse a DA1 reply: `CSI ? Ps ; ... c` (e.g. `\x1b[?62;4;6c`). Attribute
/// 4 means Sixel graphics. Returns None when the chunk carries no DA1 reply
/// or a malformed one.
fn da1_attrs(chunk: &str) -> Option<Vec<u16>> {
    let start = chunk.find("\x1b[?")? + "\x1b[?".len();
    let end = chunk[start..].find('c')?;
    chunk[start..start + end]
        .split(';')
        .filter(|s| !s.is_empty())
        .map(|s| s.trim().parse().ok())
        .collect()
}

/// Extract the name from an XTVERSION reply: `DCS > | name ST` (e.g.
/// `\x1bP>|kitty(0.42.2)\x1b\\`), tolerating a BEL terminator and a reply
/// without terminator (Timeout leftover).
fn xtversion_name(chunk: &str) -> Option<&str> {
    let start = chunk.find(">|")? + 2;
    let rest = &chunk[start..];
    let end = rest.find(['\x1b', '\x07']).unwrap_or(rest.len());
    let name = &rest[..end];
    (!name.is_empty()).then_some(name)
}

fn parse_csi_t(raw: &[u8]) -> Option<CellPx> {
    let s = std::str::from_utf8(raw).ok()?;
    let idx = s.rfind('\x1b')?;
    let payload = &s[idx + 1..];
    let body = payload.strip_prefix('[')?.strip_suffix('t')?;
    let mut parts = body.split(';');
    // CSI 16 t responds with CSI 6 ; cell_height ; cell_width t.
    if parts.next()?.trim() != "6" {
        return None;
    }
    let h: u32 = parts.next()?.trim().parse().ok()?;
    let w: u32 = parts.next()?.trim().parse().ok()?;
    if h == 0 || w == 0 {
        None
    } else {
        Some(CellPx { w, h })
    }
}

enum ProbeRead {
    Found(Vec<u8>),
    Timeout(Vec<u8>),
}

fn dbg_dump(label: &str, buf: &[u8]) {
    if env::var("ISEE_DEBUG").is_ok() {
        eprintln!(
            "isee probe {label}: {} bytes: {}",
            buf.len(),
            buf.iter().map(|b| format!("{b:02x}")).collect::<String>()
        );
    }
}

/// Wait until `fd` is readable using select(2).
/// macOS poll(2) reports POLLNVAL for /dev/tty, so poll is unusable here.
fn wait_readable(fd: i32, timeout_ms: i32) -> bool {
    if fd < 0 || fd >= libc::FD_SETSIZE as i32 {
        return false;
    }
    unsafe {
        let mut rfds: libc::fd_set = std::mem::zeroed();
        libc::FD_ZERO(&mut rfds);
        libc::FD_SET(fd, &mut rfds);
        let mut tv = libc::timeval {
            tv_sec: (timeout_ms / 1000) as libc::time_t,
            tv_usec: ((timeout_ms % 1000) * 1000) as libc::suseconds_t,
        };
        let rc = libc::select(
            fd + 1,
            &mut rfds,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut tv,
        );
        rc > 0 && libc::FD_ISSET(fd, &rfds)
    }
}

/// Read from `tty` until `term` appears. Bytes read past `term` (e.g. the
/// response to the next probe arriving early) are kept in `tty.pending`.
fn read_until(tty: &mut RawTty, term: &[u8], timeout: Duration) -> ProbeRead {
    if let Some(pos) = find_sub(&tty.pending, term) {
        let rest = tty.pending.split_off(pos + term.len());
        let found = std::mem::replace(&mut tty.pending, rest);
        return ProbeRead::Found(found);
    }
    let fd = tty.file.as_raw_fd();
    let start = Instant::now();
    let mut tmp = [0u8; 256];
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return ProbeRead::Timeout(std::mem::take(&mut tty.pending));
        }
        let remain = (timeout - elapsed).as_millis().min(i32::MAX as u128) as i32;
        if !wait_readable(fd, remain) {
            return ProbeRead::Timeout(std::mem::take(&mut tty.pending));
        }
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n > 0 {
            tty.pending.extend_from_slice(&tmp[..n as usize]);
            if tty.pending.len() >= MAX_RESPONSE {
                return ProbeRead::Found(std::mem::take(&mut tty.pending));
            }
            if let Some(pos) = find_sub(&tty.pending, term) {
                let rest = tty.pending.split_off(pos + term.len());
                let found = std::mem::replace(&mut tty.pending, rest);
                return ProbeRead::Found(found);
            }
        }
    }
}

fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn drain_input(fd: i32) {
    let mut tmp = [0u8; 256];
    loop {
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n <= 0 {
            break;
        }
    }
}

struct RawTty {
    file: File,
    orig: libc::termios,
    raw: bool,
    pending: Vec<u8>,
}

impl RawTty {
    fn new() -> Option<Self> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .ok()?;
        let fd = file.as_raw_fd();
        let mut orig: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut orig) } != 0 {
            return None;
        }
        let mut raw = orig;
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
        Some(RawTty {
            file,
            orig,
            raw: true,
            pending: Vec::new(),
        })
    }
}

impl Drop for RawTty {
    fn drop(&mut self) {
        if self.raw {
            unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.orig) };
        }
    }
}

pub fn wrap_passthrough(output: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(output.len() + 16);
    out.extend_from_slice(b"\x1bPtmux;\x1b");
    for &b in output {
        if b == 0x1b {
            out.push(b);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

/// tmux-flavoured output for bitmap graphics: wrap each transfer frame in its
/// own DCS passthrough (mirroring yazi), while leaving plain text, SGR
/// colours and CRLF untouched so they flow through tmux's pane grid. The
/// grid is what lets tmux draw placeholders/text at the right pane position
/// and re-draw them on refresh — wrapping them in passthrough would render
/// the image at the client's stray cursor position and erase it on the next
/// redraw.
///
/// Frame kinds recognized (note their DIFFERENT terminators):
/// - Kitty APC chunk: `ESC _G ... ESC \`
/// - iTerm2 inline image: `ESC ]1337;... BEL` — terminated by BEL (0x07),
///   NOT by ST; matching on ST would swallow the whole block.
/// - Sixel: `ESC P <digit/;> q ... ESC \` (DCS header like `ESC Pq` or
///   `ESC P9;1q`).
pub fn wrap_graphics_passthrough(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + 4096);
    let mut i = 0;
    while i < data.len() {
        if data[i..].starts_with(b"\x1b_G")
            && let Some(end) = find_sub(&data[i + 2..], b"\x1b\\")
        {
            let seq_end = i + 2 + end + 2;
            out.extend_from_slice(&wrap_passthrough(&data[i..seq_end]));
            i = seq_end;
            continue;
        }
        if data[i..].starts_with(b"\x1b]1337;")
            && let Some(end) = find_sub(&data[i..], b"\x07")
        {
            // `end` is relative to `i`; BEL terminates the frame.
            let seq_end = i + end + 1;
            out.extend_from_slice(&wrap_passthrough(&data[i..seq_end]));
            i = seq_end;
            continue;
        }
        if data[i] == 0x1b && data.get(i + 1) == Some(&b'P') {
            let mut j = i + 2;
            while j < data.len().min(i + 32) && (data[j].is_ascii_digit() || data[j] == b';') {
                j += 1;
            }
            if data.get(j) == Some(&b'q')
                && let Some(end) = find_sub(&data[j + 1..], b"\x1b\\")
            {
                let seq_end = j + 1 + end + 2;
                out.extend_from_slice(&wrap_passthrough(&data[i..seq_end]));
                i = seq_end;
                continue;
            }
        }
        out.push(data[i]);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_t_parse() {
        assert_eq!(parse_csi_t(b"\x1b[1080;1920t"), None);
        assert_eq!(parse_csi_t(b"junk\x1b[36;40t"), None);
        assert_eq!(parse_csi_t(b"\x1b[0;0t"), None);
        assert_eq!(parse_csi_t(b"nope"), None);
    }

    #[test]
    fn csi_t_parse_cell_px_format() {
        assert_eq!(
            parse_csi_t(b"\x1b[6;1080;1920t"),
            Some(CellPx { w: 1920, h: 1080 })
        );
        assert_eq!(
            parse_csi_t(b"junk\x1b[6;36;40t"),
            Some(CellPx { w: 40, h: 36 })
        );
        assert_eq!(parse_csi_t(b"\x1b[6;0;0t"), None);
    }

    #[test]
    fn report_cell_size_st_terminated() {
        let (cell, scale) =
            parse_report_cell_size(b"junk\x1b]1337;ReportCellSize=16.0;7.0;2.0\x1b\\").unwrap();
        assert_eq!(cell, CellPx { w: 7, h: 16 });
        assert!((scale - 2.0).abs() < f32::EPSILON);
    }

    #[test]
    fn report_cell_size_bel_terminated() {
        let (cell, scale) =
            parse_report_cell_size(b"\x1b]1337;ReportCellSize=11.5;8.5;1.0\x07").unwrap();
        assert_eq!(cell, CellPx { w: 9, h: 12 });
        assert!((scale - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn report_cell_size_rejects_bad_payload() {
        assert_eq!(
            parse_report_cell_size(b"\x1b]1337;ReportCellSize=0;0;0\x1b\\"),
            None
        );
        assert_eq!(
            parse_report_cell_size(b"\x1b]1337;ReportCellSize=16.0;7.0\x1b\\"),
            None
        );
        assert_eq!(parse_report_cell_size(b"no answer here"), None);
    }

    // ---- KGP dual probe (streaming + tempfile) ----

    fn kgp_resp(id: u32, body: &str) -> String {
        format!("\x1b_Gi={id};{body}\x1b\\")
    }

    #[test]
    fn kgp_verdicts_both_ok() {
        let chunks = [kgp_resp(31, "OK"), kgp_resp(32, "OK")];
        assert_eq!(kgp_probe_verdicts(&chunks), (true, true));
        assert_eq!(kgp_resolved_ids(&chunks, true), 2);
    }

    #[test]
    fn kgp_verdicts_tempfile_rejected() {
        let chunks = [
            kgp_resp(31, "OK"),
            kgp_resp(32, "EBADF: Failed to open file for graphics transmission"),
        ];
        assert_eq!(kgp_probe_verdicts(&chunks), (true, false));
        assert_eq!(kgp_resolved_ids(&chunks, true), 2);
    }

    #[test]
    fn kgp_verdicts_streaming_only() {
        let chunks = [kgp_resp(31, "OK")];
        assert_eq!(kgp_probe_verdicts(&chunks), (true, false));
        assert_eq!(kgp_resolved_ids(&chunks, true), 1);
        assert_eq!(kgp_resolved_ids(&chunks, false), 1);
    }

    #[test]
    fn kgp_verdicts_ignore_unrelated_chunks() {
        // A stray response for another id (or noise) must not resolve
        // anything nor corrupt the verdicts that follow.
        let chunks = [
            kgp_resp(99, "OK"),
            kgp_resp(31, "OK"),
            "noise".to_string(),
            kgp_resp(32, "OK"),
        ];
        assert_eq!(kgp_probe_verdicts(&chunks), (true, true));
        assert_eq!(kgp_resolved_ids(&chunks, true), 2);
        assert_eq!(kgp_resolved_ids(&[kgp_resp(99, "OK")], true), 0);
    }

    #[test]
    fn kgp_verdicts_single_combined_chunk() {
        // Both responses can land in one read; the chunk carries both ids.
        let chunk = format!("{}{}", kgp_resp(31, "OK"), kgp_resp(32, "OK"));
        let chunks = [chunk];
        assert_eq!(kgp_probe_verdicts(&chunks), (true, true));
        assert_eq!(kgp_resolved_ids(&chunks, true), 2);
    }

    #[test]
    fn kgp_transfer_choice_prefers_probe_by_default() {
        assert_eq!(kgp_transfer_choice(true, None), KgpTransfer::Tempfile);
        assert_eq!(kgp_transfer_choice(false, None), KgpTransfer::Stream);
    }

    #[test]
    fn kgp_transfer_choice_env_overrides_probe() {
        assert_eq!(
            kgp_transfer_choice(false, Some("tempfile")),
            KgpTransfer::Tempfile
        );
        assert_eq!(
            kgp_transfer_choice(false, Some("FILE")),
            KgpTransfer::Tempfile
        );
        assert_eq!(
            kgp_transfer_choice(true, Some("stream")),
            KgpTransfer::Stream
        );
        // Unknown values fall through to the probe result.
        assert_eq!(
            kgp_transfer_choice(true, Some("???")),
            KgpTransfer::Tempfile
        );
        assert_eq!(kgp_transfer_choice(false, Some("???")), KgpTransfer::Stream);
        // Whitespace around the value is trimmed.
        assert_eq!(
            kgp_transfer_choice(false, Some(" tempfile ")),
            KgpTransfer::Tempfile
        );
    }

    #[test]
    fn tmux_wrap_doubles_esc() {
        let out = wrap_passthrough(b"\x1b[0m\x1b_Gx\x1b\\");
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b\x1b[0m\x1b\x1b_Gx\x1b\x1b\\\x1b\\");
    }

    #[test]
    fn kitty_wrap_only_passthroughs_apc_chunks() {
        // Two KGP chunks followed by placeholder text (SGR + cell + CRLF).
        let placeholder = "\u{10EEEE}";
        let mut data = b"\x1b_Ga=T,m=0;AA\x1b\\\x1b_Gm=0;BB\x1b\\\x1b[38;2;1;2;3m".to_vec();
        data.extend_from_slice(placeholder.as_bytes());
        data.extend_from_slice(b"\r\n");
        let out = wrap_graphics_passthrough(&data);
        // Each APC chunk gets its own passthrough wrapper...
        let chunk1 = wrap_passthrough(b"\x1b_Ga=T,m=0;AA\x1b\\");
        let chunk2 = wrap_passthrough(b"\x1b_Gm=0;BB\x1b\\");
        let mut expect = chunk1;
        expect.extend_from_slice(&chunk2);
        // ...while the placeholder text passes through byte-identical.
        expect.extend_from_slice(b"\x1b[38;2;1;2;3m");
        expect.extend_from_slice(placeholder.as_bytes());
        expect.extend_from_slice(b"\r\n");
        assert_eq!(out, expect);
    }

    #[test]
    fn kitty_wrap_handles_unterminated_apc() {
        // A trailing "\x1b_G" without terminator is copied verbatim.
        let data = b"\x1b_Gbroken";
        assert_eq!(wrap_graphics_passthrough(data), data);
    }

    #[test]
    fn osc1337_frame_wrapped_by_bel() {
        // The iTerm2 frame ends with BEL (0x07), NOT ST. The trailing CRLFs
        // that park the cursor must stay outside the wrapper.
        let mut data =
            b"\x1b]1337;File=inline=1;size=4;width=2px;height=2px;doNotMoveCursor=1:AAAA\x07"
                .to_vec();
        data.extend_from_slice(b"\r\n\r\n");
        let out = wrap_graphics_passthrough(&data);
        let wrapped = wrap_passthrough(
            b"\x1b]1337;File=inline=1;size=4;width=2px;height=2px;doNotMoveCursor=1:AAAA\x07",
        );
        let mut expect = wrapped;
        expect.extend_from_slice(b"\r\n\r\n");
        assert_eq!(out, expect);
    }

    #[test]
    fn sixel_dcs_frames_wrapped() {
        let heads: [&[u8]; 2] = [b"\x1bPq", b"\x1bP9;1q"];
        for head in heads {
            let mut data = head.to_vec();
            data.extend_from_slice(b"#0;2;100;0;0#0?-$\x1b\\\r\n");
            let out = wrap_graphics_passthrough(&data);
            let mut seq = head.to_vec();
            seq.extend_from_slice(b"#0;2;100;0;0#0?-$\x1b\\");
            let mut expect = wrap_passthrough(&seq);
            expect.extend_from_slice(b"\r\n");
            assert_eq!(out, expect, "head {head:?}");
        }
    }

    #[test]
    fn unterminated_sixel_copied_verbatim() {
        let data = b"\x1bP9;1qbroken";
        assert_eq!(wrap_graphics_passthrough(data), data);
    }

    #[test]
    fn osc1337_frame_wrapped_after_leading_text() {
        // Regression: the BEL terminator search returns an index relative to
        // the frame start, so a frame NOT at offset 0 used to truncate the
        // wrapper at the wrong place.
        let mut data = b"\r\n\x1b]1337;File=inline=1:BBBB\x07".to_vec();
        data.extend_from_slice(b"\r\n");
        let out = wrap_graphics_passthrough(&data);
        let mut expect = b"\r\n".to_vec();
        expect.extend_from_slice(&wrap_passthrough(b"\x1b]1337;File=inline=1:BBBB\x07"));
        expect.extend_from_slice(b"\r\n");
        assert_eq!(out, expect);
    }

    // ---- batched probe parsing ----

    #[test]
    fn da1_attrs_parse() {
        assert_eq!(
            da1_attrs("junk\x1b[?62;4;6;22c after"),
            Some(vec![62, 4, 6, 22])
        );
        // Minimal single-attribute reply.
        assert_eq!(da1_attrs("\x1b[?62c"), Some(vec![62]));
        // No DA1 reply in the chunk.
        assert_eq!(da1_attrs("nothing here"), None);
        // Malformed attributes reject the whole reply.
        assert_eq!(da1_attrs("\x1b[?62;x;6c"), None);
    }

    #[test]
    fn xtversion_name_parse() {
        assert_eq!(
            xtversion_name("noise\x1bP>|kitty(0.42.2)\x1b\\"),
            Some("kitty(0.42.2)")
        );
        assert_eq!(
            xtversion_name("\x1bP>|WezTerm 20240203\x07"),
            Some("WezTerm 20240203")
        );
        // No ST terminator (Timeout leftover): read to end of chunk.
        assert_eq!(
            xtversion_name("\x1bP>|foot(version 1.20)"),
            Some("foot(version 1.20)")
        );
        assert_eq!(xtversion_name("no version here"), None);
        assert_eq!(xtversion_name("\x1bP>|\x1b\\"), None);
    }

    #[test]
    fn batch_classify_full_answer_set() {
        let chunks = [
            "\x1bP>|WezTerm 20240203\x1b\\".to_string(),
            "\x1b[?62;4;6c".to_string(),
            kgp_resp(31, "OK"),
            kgp_resp(32, "OK"),
        ];
        let a = classify_batch(&chunks);
        assert!(a.kgp && a.kgp_file && a.sixel);
        assert_eq!(a.brand, Some(crate::brand::Brand::WezTerm));
        assert!(batch_resolved(&chunks, true, true));
    }

    #[test]
    fn batch_classify_merged_and_leftover_chunks() {
        // XTVERSION + DA1 merged into one chunk, KGP answers missing →
        // not resolved; sixel and brand still classified.
        let chunks = ["\x1bP>|foot(version 1.20)\x1b\\\x1b[?62;6;9c".to_string()];
        let a = classify_batch(&chunks);
        assert!(!a.kgp);
        assert!(!a.sixel); // DA1 attrs lack 4
        assert_eq!(a.brand, Some(crate::brand::Brand::Foot));
        assert!(!batch_resolved(&chunks, true, false));

        // Sixel-capable DA1 in the Timeout leftover flips the verdict.
        let leftover = ["\x1b[?62;4c".to_string()];
        let a = classify_batch(&leftover);
        assert!(a.sixel);
    }

    #[test]
    fn batch_classify_kgp_verdicts() {
        // Single combined chunk carrying both KGP verdicts.
        let chunk = format!("{}{}", kgp_resp(31, "OK"), kgp_resp(32, "EBADF: nope"));
        let a = classify_batch(&[chunk]);
        assert!(a.kgp);
        assert!(!a.kgp_file);
    }

    // ---- protocol priority ----

    use crate::brand::Brand;

    #[test]
    fn select_protocol_forced_beats_everything() {
        for p in [
            Protocol::Kitty,
            Protocol::Iip,
            Protocol::Sixel,
            Protocol::HalfBlocks,
        ] {
            assert_eq!(
                select_protocol(
                    Some(p),
                    Some(Protocol::Iip),
                    true,
                    true,
                    Some(Brand::Foot),
                    true,
                    false
                ),
                p,
                "{p:?}"
            );
        }
        // Forced values are never touched by the tmux downgrade either.
        assert_eq!(
            select_protocol(Some(Protocol::Sixel), None, false, false, None, false, true),
            Protocol::Sixel
        );
    }

    #[test]
    fn select_protocol_env_override_beats_probes() {
        assert_eq!(
            select_protocol(
                None,
                Some(Protocol::Sixel),
                true,
                false,
                Some(Brand::KittyFamily),
                true,
                false
            ),
            Protocol::Sixel
        );
        // An unknown ISEE_PROTOCOL value maps to Half Blocks (pre-existing
        // isee_override behavior) and still wins over probed answers.
        assert_eq!(
            select_protocol(
                None,
                Some(Protocol::HalfBlocks),
                true,
                false,
                Some(Brand::Foot),
                true,
                false
            ),
            Protocol::HalfBlocks
        );
    }

    #[test]
    fn select_protocol_brand_beats_probes_and_kitty_hint_kept() {
        // Brand table (measured matrix) beats probed KGP and probed DA1
        // sixel: Warp/Foot would otherwise flip to the probed protocol.
        assert_eq!(
            select_protocol(None, None, true, false, Some(Brand::Warp), true, false),
            Protocol::Kitty
        );
        assert_eq!(
            select_protocol(None, None, true, false, Some(Brand::Foot), true, false),
            Protocol::Sixel
        );
        // KGP brand with no probe result at all (probe skipped) still Kitty.
        assert_eq!(
            select_protocol(None, None, false, false, Some(Brand::Iterm2), false, false),
            Protocol::Kitty
        );
        // Hyper: no bitmap protocol at all.
        assert_eq!(
            select_protocol(None, None, false, false, Some(Brand::Hyper), false, false),
            Protocol::HalfBlocks
        );
        // Without a brand, the kitty env hint is kept even without a probe.
        assert_eq!(
            select_protocol(None, None, false, true, None, false, false),
            Protocol::Kitty
        );
        // DA1 sixel without any brand.
        assert_eq!(
            select_protocol(None, None, false, false, None, true, false),
            Protocol::Sixel
        );
        // Nothing known → Half Blocks.
        assert_eq!(
            select_protocol(None, None, false, false, None, false, false),
            Protocol::HalfBlocks
        );
    }

    #[test]
    fn select_protocol_tmux_downgrades_unforced_sixel() {
        // Probed / env-selected sixel inside tmux still falls back.
        assert_eq!(
            select_protocol(None, None, false, false, Some(Brand::Foot), true, true),
            Protocol::HalfBlocks
        );
        assert_eq!(
            select_protocol(None, Some(Protocol::Sixel), false, false, None, false, true),
            Protocol::HalfBlocks
        );
        // Other protocols are untouched inside tmux.
        assert_eq!(
            select_protocol(None, None, true, false, None, false, true),
            Protocol::Kitty
        );
    }
}
