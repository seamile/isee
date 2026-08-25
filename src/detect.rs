use std::env;
use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::io::AsRawFd;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Protocol {
    Kitty,
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
}

#[derive(Clone, Copy, Debug)]
pub struct TerminalInfo {
    pub protocol: Protocol,
    pub cell: CellPx,
    pub win: WinSize,
    pub tmux: bool,
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(500);
const KGP_PROBE: &[u8] = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\";
const CSI_CELL_PX: &[u8] = b"\x1b[16t";
const MAX_RESPONSE: usize = 4096;

pub fn is_tty(fd: i32) -> bool {
    unsafe { libc::isatty(fd) == 1 }
}

pub fn detect(stdout_fd: i32) -> TerminalInfo {
    let tmux = in_tmux();
    let stdout_tty = is_tty(stdout_fd);
    let override_p = isee_override();
    let env_kitty = env_kitty_hint();

    let mut cell = fallback_cell();
    let mut probed_kitty = false;

    if override_p.is_none()
        && stdout_tty
        && !tmux
        && let Some(mut tty) = RawTty::new()
    {
        probed_kitty = probe_kgp(&mut tty);
        if let Some(c) = probe_cell_px(&mut tty) {
            cell = c;
        }
        drain_input(tty.file.as_raw_fd());
    }

    let protocol = match override_p {
        Some(p) => p,
        None if probed_kitty || env_kitty => Protocol::Kitty,
        None => Protocol::HalfBlocks,
    };

    TerminalInfo {
        protocol,
        cell,
        win: win_size(stdout_fd),
        tmux,
    }
}

fn in_tmux() -> bool {
    env::var("TMUX").is_ok() || env::var("TERM_PROGRAM").is_ok_and(|v| v == "tmux")
}

fn isee_override() -> Option<Protocol> {
    let v = env::var("ISEE_PROTOCOL").ok()?.to_ascii_lowercase();
    if v == "kitty" {
        Some(Protocol::Kitty)
    } else {
        Some(Protocol::HalfBlocks)
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
    match env::var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase().as_str() {
        "ghostty" | "wezterm" | "rio" | "kitty" => return true,
        _ => {}
    }
    match env::var("TERM").unwrap_or_default().to_ascii_lowercase().as_str() {
        "xterm-kitty" | "xterm-ghostty" | "ghostty" | "wezterm" | "rio" | "kitty" => return true,
        _ => {}
    }
    false
}

fn fallback_cell() -> CellPx {
    match env::var("TERM_PROGRAM").unwrap_or_default().to_ascii_lowercase().as_str() {
        "ghostty" => CellPx { w: 9, h: 18 },
        "wezterm" | "rio" => CellPx { w: 8, h: 16 },
        _ => CellPx { w: 7, h: 14 },
    }
}

fn win_size(fd: i32) -> WinSize {
    unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) == 0 && ws.ws_col > 0 && ws.ws_row > 0 {
            WinSize {
                cols: ws.ws_col as u32,
                rows: ws.ws_row as u32,
            }
        } else {
            WinSize { cols: 80, rows: 24 }
        }
    }
}

fn probe_kgp(tty: &mut RawTty) -> bool {
    if tty.file.write_all(KGP_PROBE).is_err() {
        return false;
    }
    match read_until(&tty.file, b"\x1b\\", PROBE_TIMEOUT) {
        Some(r) => {
            let s = String::from_utf8_lossy(&r);
            s.contains("OK") && s.contains("i=31")
        }
        None => false,
    }
}

fn probe_cell_px(tty: &mut RawTty) -> Option<CellPx> {
    tty.file.write_all(CSI_CELL_PX).ok()?;
    let r = read_until(&tty.file, b"t", PROBE_TIMEOUT)?;
    parse_csi_t(&r)
}

fn parse_csi_t(raw: &[u8]) -> Option<CellPx> {
    let s = std::str::from_utf8(raw).ok()?;
    let idx = s.rfind('\x1b')?;
    let payload = &s[idx + 1..];
    let body = payload.strip_prefix('[')?.strip_suffix('t')?;
    let mut parts = body.split(';');
    let h: u32 = parts.next()?.trim().parse().ok()?;
    let w: u32 = parts.next()?.trim().parse().ok()?;
    if h == 0 || w == 0 {
        None
    } else {
        Some(CellPx { w, h })
    }
}

fn read_until(file: &File, term: &[u8], timeout: Duration) -> Option<Vec<u8>> {
    let fd = file.as_raw_fd();
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut tmp = [0u8; 256];
    loop {
        let elapsed = start.elapsed();
        if elapsed >= timeout {
            return None;
        }
        let remain = (timeout - elapsed).as_millis().min(i32::MAX as u128) as i32;
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        let rc = unsafe { libc::poll(&mut pfd, 1, remain) };
        if rc == 0 {
            return None;
        }
        if rc < 0 {
            if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return None;
        }
        if pfd.revents & libc::POLLIN == 0 {
            continue;
        }
        let n = unsafe { libc::read(fd, tmp.as_mut_ptr() as *mut libc::c_void, tmp.len()) };
        if n > 0 {
            buf.extend_from_slice(&tmp[..n as usize]);
            if buf.windows(term.len()).any(|w| w == term) || buf.len() >= MAX_RESPONSE {
                return Some(buf);
            }
        }
    }
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
}

impl RawTty {
    fn new() -> Option<Self> {
        let file = OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
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
        Some(RawTty { file, orig, raw: true })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csi_t_parse() {
        assert_eq!(parse_csi_t(b"\x1b[1080;1920t"), Some(CellPx { w: 1920, h: 1080 }));
        assert_eq!(parse_csi_t(b"junk\x1b[36;40t"), Some(CellPx { w: 40, h: 36 }));
        assert_eq!(parse_csi_t(b"\x1b[0;0t"), None);
        assert_eq!(parse_csi_t(b"nope"), None);
    }

    #[test]
    fn tmux_wrap_doubles_esc() {
        let out = wrap_passthrough(b"\x1b[0m\x1b_Gx\x1b\\");
        assert_eq!(out, b"\x1bPtmux;\x1b\x1b\x1b[0m\x1b\x1b_Gx\x1b\x1b\\\x1b\\");
    }
}
