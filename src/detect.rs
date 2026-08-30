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
    /// How kitty-protocol payloads should reach the terminal: probed
    /// tempfile support (`t=1`), or the `ISEE_KGP_TRANSFER` override.
    pub kgp_transfer: KgpTransfer,
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const KGP_PROBE: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const CSI_CELL_PX: &[u8] = b"\x1b[16t";
const OSC_CELL_SIZE: &[u8] = b"\x1b]1337;ReportCellSize\x1b\\";
const MAX_RESPONSE: usize = 4096;

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn detect(stdout_fd: i32) -> TerminalInfo {
    let tmux = in_tmux();
    let stdout_tty = is_tty(stdout_fd);
    let override_p = isee_override();
    let env_kitty = env_kitty_hint();

    // Brand-based selection (env table, mirroring yazi): covers bitmap
    // terminals the KGP hint does not reach. Kitty-family brands map to None
    // and keep riding the dedicated KGP probe below.
    let brand = crate::brand::detect(|name| env::var(name).ok());
    let brand_proto = brand.and_then(crate::brand::preferred_protocol);

    let mut cell = fallback_cell();
    let mut probed_kitty = false;
    let mut probed_file = false;
    let mut probed_scale = None;

    if override_p.is_none()
        && stdout_tty
        && (env_kitty || brand_proto.is_some())
        && let Some(mut tty) = RawTty::new()
    {
        // Cell probing (`CSI 16 t`) is harmless and broadly supported, so it
        // runs for every graphics-capable terminal. The KGP query however
        // leaks its APC payload as visible text on unsupporting terminals,
        // so it stays gated on the strict kitty-environment hints.
        // Inside tmux the pane's own terminal is tmux's virtual terminal;
        // probe the OUTER terminal by wrapping the queries in tmux's DCS
        // passthrough (mirroring yazi), after making sure passthrough is on.
        if tmux {
            enable_tmux_passthrough();
        }
        if env_kitty {
            (probed_kitty, probed_file) = probe_kgp(&mut tty, tmux);
        }
        let iterm2 = brand == Some(crate::brand::Brand::Iterm2);
        if iterm2 {
            // iTerm2 never answers `CSI 16 t` (that path would just burn a
            // full probe timeout), but it replies to the OSC 1337
            // ReportCellSize query with the cell size in logical points PLUS
            // the device-pixel scale. Fall back to the generic probe if the
            // answer does not parse.
            if let Some((c, s)) = probe_report_cell_size(&mut tty, tmux) {
                cell = c;
                probed_scale = Some(s);
            } else if let Some(c) = probe_cell_px(&mut tty, tmux) {
                cell = c;
            }
        } else if let Some(c) = probe_cell_px(&mut tty, tmux) {
            cell = c;
        }
        drain_input(tty.file.as_raw_fd());
    }

    let mut protocol = match override_p {
        Some(p) => p,
        None if probed_kitty || env_kitty => Protocol::Kitty,
        None => brand_proto.unwrap_or(Protocol::HalfBlocks),
    };
    // Sixel inside tmux would need a sixel DCS frame nested inside tmux's own
    // DCS passthrough, which does not survive reliably; downgrade to Half
    // Blocks (a forced ISEE_PROTOCOL=sixel still works for experiments).
    if tmux && protocol == Protocol::Sixel {
        protocol = Protocol::HalfBlocks;
    }

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
    let kgp_transfer = kgp_transfer_choice(
        probed_kitty && probed_file,
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
        kgp_transfer,
    };
    if env::var("ISEE_DEBUG").is_ok() {
        eprintln!(
            "isee: protocol={:?} cell={}x{} win={}x{} px={:?} scale={} probed_scale={:?} tmux={} transfer={:?}",
            info.protocol,
            info.cell.w,
            info.cell.h,
            info.win.cols,
            info.win.rows,
            info.win.px,
            info.dpy_scale,
            info.probed_scale,
            tmux,
            info.kgp_transfer
        );
    }
    info
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

/// Probe KGP support with two queries in one shot: `t=d` (streaming
/// transfer) and `t=t` (tempfile transfer). Only terminals that speak KGP
/// answer the first; the second carries a REAL temp file path — kitty
/// validates `t=t` queries by opening the payload as a file, so an arbitrary
/// payload answers EBADF even on a tempfile-capable kitty (observed live:
/// `EBADF: Failed to open file for graphics transmission`). The probe file's
/// name carries kitty's `tty-graphics-protocol` marker so kitty deletes it
/// after reading; we also unlink it ourselves in case that cleanup does not
/// run. Both responses share one deadline, and leftover bytes from an early
/// combined read stay in `tty.pending` for the next `read_until`.
/// Returns `(streaming_ok, tempfile_ok)`.
fn probe_kgp(tty: &mut RawTty, passthrough: bool) -> (bool, bool) {
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
    if tty.file.write_all(&both).is_err() {
        if have_file {
            let _ = std::fs::remove_file(&probe_file);
        }
        return (false, false);
    }
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let mut chunks: Vec<String> = Vec::new();
    let need = if have_file { 2 } else { 1 };
    while kgp_resolved_ids(&chunks, have_file) < need {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        match read_until(tty, b"\x1b\\", deadline - now) {
            ProbeRead::Found(r) => {
                dbg_dump("kgp", &r);
                chunks.push(String::from_utf8_lossy(&r).into_owned());
            }
            ProbeRead::Timeout(r) => {
                dbg_dump("kgp-timeout", &r);
                break;
            }
        }
    }
    if have_file {
        let _ = std::fs::remove_file(&probe_file);
    }
    let (direct, file) = kgp_probe_verdicts(&chunks);
    (direct, file && have_file)
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
/// contains() check per id resolves either outcome.
fn kgp_probe_verdicts(chunks: &[String]) -> (bool, bool) {
    chunks.iter().fold((false, false), |(d, f), s| {
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
            let seq_end = end + 1;
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
}
