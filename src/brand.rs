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

/// The preferred bitmap protocol for a brand, following yazi's driver table
/// (drivers.rs) minus drivers this tool does not implement:
/// - Iip is the first pick for iTerm2/Warp/mintty/VSCode/Tabby/Hyper/Bobcat.
/// - Foot/Windows Terminal/BlackBox speak Sixel.
/// - Konsole only ships the legacy KGP snapshot here, which we do not
///   implement either, so it maps straight to Sixel (a deliberate deviation).
/// - Kitty-family/Ghostty/Rio stay None and ride the dedicated KGP probe in
///   detect(). WezTerm also stays on KGP although yazi prefers Iip: the KGP
///   path is already proven working there.
pub fn preferred_protocol(b: Brand) -> Option<Protocol> {
    match b {
        Brand::KittyFamily | Brand::Ghostty | Brand::WezTerm | Brand::Rio => None,
        Brand::Konsole | Brand::Foot | Brand::Microsoft | Brand::BlackBox => Some(Protocol::Sixel),
        Brand::Warp
        | Brand::Iterm2
        | Brand::Mintty
        | Brand::Vscode
        | Brand::Tabby
        | Brand::Hyper
        | Brand::Bobcat => Some(Protocol::Iip),
        Brand::Apple | Brand::Urxvt | Brand::Unknown => None,
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
    fn preferred_protocol_table() {
        use crate::detect::Protocol;
        // Kitty-family terminals ride the existing KGP probe instead.
        for b in [
            Brand::KittyFamily,
            Brand::Ghostty,
            Brand::WezTerm,
            Brand::Rio,
            Brand::Unknown,
            Brand::Apple,
            Brand::Urxvt,
        ] {
            assert_eq!(preferred_protocol(b), None, "{b:?}");
        }
        for b in [
            Brand::Konsole,
            Brand::Foot,
            Brand::Microsoft,
            Brand::BlackBox,
        ] {
            assert_eq!(preferred_protocol(b), Some(Protocol::Sixel), "{b:?}");
        }
        for b in [
            Brand::Warp,
            Brand::Iterm2,
            Brand::Mintty,
            Brand::Vscode,
            Brand::Tabby,
            Brand::Hyper,
            Brand::Bobcat,
        ] {
            assert_eq!(preferred_protocol(b), Some(Protocol::Iip), "{b:?}");
        }
    }
}
