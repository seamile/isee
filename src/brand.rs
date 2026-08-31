use crate::detect::Protocol;

/// Terminal brands recognized purely from the environment, mirroring
/// yazi-emulator's `Brand`. Detection never guesses: a brand is only mapped
/// when an env var hits exactly (case-insensitively for TERM/TERM_PROGRAM).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Brand {
    KittyFamily,
    Warp,
    Iterm2,
    WezTerm,
    Rio,
    Ghostty,
    Konsole,
    Foot,
    Microsoft,
    BlackBox,
    Mintty,
    Vscode,
    Tabby,
    Hyper,
    // Kept to mirror yazi's full table: only its CSI-response path can
    // produce these two brands, which this env-only detector never runs.
    #[allow(dead_code)]
    Bobcat,
    #[allow(dead_code)]
    Unknown,
    Apple,
    Urxvt,
}

/// Recognize the terminal brand from injected environment lookups, so tests
/// can run without touching real process state. Mapping table mirrors
/// `misc/yazi/yazi-emulator/src/brand.rs` (`Brand::from_env`).
pub fn detect(lookup: impl Fn(&str) -> Option<String>) -> Option<Brand> {
    match lookup("TERM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "xterm-kitty" => return Some(Brand::KittyFamily),
        "foot" | "foot-extra" => return Some(Brand::Foot),
        "xterm-ghostty" | "ghostty" => return Some(Brand::Ghostty),
        "rio" => return Some(Brand::Rio),
        "rxvt-unicode-256color" => return Some(Brand::Urxvt),
        _ => {}
    }

    match lookup("TERM_PROGRAM")
        .unwrap_or_default()
        .to_ascii_lowercase()
        .as_str()
    {
        "iterm.app" => return Some(Brand::Iterm2),
        "wezterm" => return Some(Brand::WezTerm),
        "ghostty" => return Some(Brand::Ghostty),
        "warpterminal" => return Some(Brand::Warp),
        "rio" => return Some(Brand::Rio),
        "blackbox" => return Some(Brand::BlackBox),
        "vscode" => return Some(Brand::Vscode),
        "tabby" => return Some(Brand::Tabby),
        "hyper" => return Some(Brand::Hyper),
        "mintty" => return Some(Brand::Mintty),
        "apple_terminal" => return Some(Brand::Apple),
        _ => {}
    }

    let vars: [(&str, Brand); 9] = [
        ("KITTY_WINDOW_ID", Brand::KittyFamily),
        ("KONSOLE_VERSION", Brand::Konsole),
        ("ITERM_SESSION_ID", Brand::Iterm2),
        ("WEZTERM_EXECUTABLE", Brand::WezTerm),
        ("GHOSTTY_RESOURCES_DIR", Brand::Ghostty),
        ("WT_Session", Brand::Microsoft),
        ("WARP_HONOR_PS1", Brand::Warp),
        ("VSCODE_INJECTION", Brand::Vscode),
        ("TABBY_CONFIG_DIRECTORY", Brand::Tabby),
    ];
    vars.into_iter()
        .find(|&(name, _)| lookup(name).is_some())
        .map(|(_, brand)| brand)
}

/// Recognize the terminal brand from an XTVERSION response body (`DCS > |
/// name ST`, e.g. `kitty(0.42.2)` or `WezTerm 20240203-110809-5046fc22`),
/// mirroring yazi's `Brand::from_csi` substring table. Multiplexer names
/// (tmux/Zellij/libvterm) deliberately map to None: they name no bitmap
/// terminal, and the env table keeps covering those.
pub fn from_version_str(resp: &str) -> Option<Brand> {
    let names = [
        ("kitty", Brand::KittyFamily),
        ("Konsole", Brand::Konsole),
        ("iTerm2", Brand::Iterm2),
        ("WezTerm", Brand::WezTerm),
        ("foot", Brand::Foot),
        ("ghostty", Brand::Ghostty),
        ("Warp", Brand::Warp),
        ("Rio ", Brand::Rio),
        ("Bobcat", Brand::Bobcat),
    ];
    names
        .into_iter()
        .find(|&(n, _)| resp.contains(n))
        .map(|(_, b)| b)
}

/// The preferred bitmap protocol for a brand: the first supported one in the
/// order KGP > IIP > Sixel > Half Blocks, per the measured support matrix
/// (user-tested 2026-08-31). isee's KGP path is the fastest (direct
/// placement, tempfile transport), so it has top priority.
/// - KGP brands: Kitty, Ghostty, iTerm2, WezTerm, VSCode, Warp.
/// - Konsole/Foot/Windows Terminal/BlackBox speak Sixel (Konsole ships only a
///   legacy KGP snapshot we do not implement, a deliberate deviation).
/// - Mintty/Tabby/Bobcat speak IIP (VSCode additionally needs
///   `terminal.integrated.enableImages: true`, off by default — until enabled
///   it just shows garbage-free text).
/// - Hyper supports no bitmap protocol at all (its xterm.js base has no OSC
///   1337 renderer; the official imgcat script fails too, tested 2026-08).
/// - Rio stays None and rides the KGP probe: its support was not measured,
///   and it answers the kitty env hint. Apple/Urxvt/Unknown are None too
///   (no known bitmap protocol).
pub fn preferred_protocol(b: Brand) -> Option<Protocol> {
    match b {
        Brand::KittyFamily
        | Brand::Ghostty
        | Brand::WezTerm
        | Brand::Iterm2
        | Brand::Vscode
        | Brand::Warp => Some(Protocol::Kitty),
        Brand::Konsole | Brand::Foot | Brand::Microsoft | Brand::BlackBox => Some(Protocol::Sixel),
        Brand::Mintty | Brand::Tabby | Brand::Bobcat => Some(Protocol::Iip),
        Brand::Hyper => Some(Protocol::HalfBlocks),
        Brand::Rio | Brand::Apple | Brand::Urxvt | Brand::Unknown => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| {
            vars.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn detects_by_term_program_case_insensitive() {
        let e = env_of(&[("TERM_PROGRAM", "WarpTerminal")]);
        assert_eq!(detect(e), Some(Brand::Warp));
        // Warp sometimes spells it differently; accept case-insensitively.
        let e = env_of(&[("TERM_PROGRAM", "warpterminal")]);
        assert_eq!(detect(e), Some(Brand::Warp));
        let e = env_of(&[("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(detect(e), Some(Brand::Iterm2));
        let e = env_of(&[("TERM_PROGRAM", "mintty")]);
        assert_eq!(detect(e), Some(Brand::Mintty));
        let e = env_of(&[("TERM_PROGRAM", "BlackBox")]);
        assert_eq!(detect(e), Some(Brand::BlackBox));
        let e = env_of(&[("TERM_PROGRAM", "Apple_Terminal")]);
        assert_eq!(detect(e), Some(Brand::Apple));
    }

    #[test]
    fn detects_by_term_value() {
        let e = env_of(&[("TERM", "xterm-kitty")]);
        assert_eq!(detect(e), Some(Brand::KittyFamily));
        let e = env_of(&[("TERM", "foot-extra")]);
        assert_eq!(detect(e), Some(Brand::Foot));
        let e = env_of(&[("TERM", "xterm-ghostty")]);
        assert_eq!(detect(e), Some(Brand::Ghostty));
        let e = env_of(&[("TERM", "rxvt-unicode-256color")]);
        assert_eq!(detect(e), Some(Brand::Urxvt));
    }

    #[test]
    fn detects_by_presence_variable() {
        let e = env_of(&[("KONSOLE_VERSION", "220803"), ("TERM", "xterm-256color")]);
        assert_eq!(detect(e), Some(Brand::Konsole));
        let e = env_of(&[("WARP_HONOR_PS1", "1")]);
        assert_eq!(detect(e), Some(Brand::Warp));
        let e = env_of(&[("WT_Session", "abc")]);
        assert_eq!(detect(e), Some(Brand::Microsoft));
        let e = env_of(&[("VSCODE_INJECTION", "1")]);
        assert_eq!(detect(e), Some(Brand::Vscode));
    }

    #[test]
    fn term_wins_over_term_program_and_vars_are_last() {
        // TERM is checked before TERM_PROGRAM in yazi's ordering.
        let e = env_of(&[("TERM", "foot"), ("TERM_PROGRAM", "iTerm.app")]);
        assert_eq!(detect(e), Some(Brand::Foot));
        let e = env_of(&[("KITTY_WINDOW_ID", "1"), ("ITERM_SESSION_ID", "w0t0p0")]);
        assert_eq!(detect(e), Some(Brand::KittyFamily));
    }

    #[test]
    fn unknown_terminal_maps_to_none() {
        let e = env_of(&[("TERM", "xterm-256color")]);
        assert_eq!(detect(e), None);
        let e = env_of(&[]);
        assert_eq!(detect(e), None);
    }

    #[test]
    fn version_str_recognizes_graphic_terminals() {
        assert_eq!(from_version_str("kitty(0.42.2)"), Some(Brand::KittyFamily));
        assert_eq!(from_version_str("Konsole 24.08.1"), Some(Brand::Konsole));
        assert_eq!(from_version_str("iTerm2 3.5.12"), Some(Brand::Iterm2));
        assert_eq!(
            from_version_str("WezTerm 20240203-110809-5046fc22"),
            Some(Brand::WezTerm)
        );
        assert_eq!(from_version_str("foot(version 1.20)"), Some(Brand::Foot));
        assert_eq!(from_version_str("ghostty 1.1.0"), Some(Brand::Ghostty));
        assert_eq!(from_version_str("Warp 0.2026.01"), Some(Brand::Warp));
        assert_eq!(from_version_str("Rio 0.2.17"), Some(Brand::Rio));
        assert_eq!(from_version_str("Bobcat 1.2"), Some(Brand::Bobcat));
    }

    #[test]
    fn version_str_rejects_multiplexers_and_unknown() {
        // Multiplexers name no bitmap terminal; keep the env table's verdict.
        assert_eq!(from_version_str("tmux 3.4"), None);
        assert_eq!(from_version_str("Zellij 0.41"), None);
        assert_eq!(from_version_str("libvterm 0.3"), None);
        assert_eq!(from_version_str("xterm(370)"), None);
        assert_eq!(from_version_str(""), None);
    }

    #[test]
    fn preferred_protocol_table() {
        use crate::detect::Protocol;
        // Measured support matrix, KGP first (the fastest path).
        for b in [
            Brand::KittyFamily,
            Brand::Ghostty,
            Brand::WezTerm,
            Brand::Iterm2,
            Brand::Vscode,
            Brand::Warp,
        ] {
            assert_eq!(preferred_protocol(b), Some(Protocol::Kitty), "{b:?}");
        }
        for b in [
            Brand::Konsole,
            Brand::Foot,
            Brand::Microsoft,
            Brand::BlackBox,
        ] {
            assert_eq!(preferred_protocol(b), Some(Protocol::Sixel), "{b:?}");
        }
        for b in [Brand::Mintty, Brand::Tabby, Brand::Bobcat] {
            assert_eq!(preferred_protocol(b), Some(Protocol::Iip), "{b:?}");
        }
        // Hyper supports no bitmap protocol at all (measured).
        assert_eq!(preferred_protocol(Brand::Hyper), Some(Protocol::HalfBlocks));
        // Rio/Apple/Urxvt/Unknown ride the probe instead.
        for b in [Brand::Rio, Brand::Apple, Brand::Urxvt, Brand::Unknown] {
            assert_eq!(preferred_protocol(b), None, "{b:?}");
        }
    }
}
