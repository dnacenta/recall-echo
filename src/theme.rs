// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

//! Gruvbox Dark palette for every colored span recall-echo prints.
//!
//! Six semantic tokens ([`GOOD`], [`WARN`], [`BAD`], [`ACCENT`], [`DIM`],
//! [`BOLD`]) plus [`RESET`] are `static` [`Paint`] values. Inline format
//! args capture statics, so a call site is simply
//! `format!("{GOOD}HEALTHY{RESET}")`. Each [`Paint`] writes the escape for
//! the process-wide [`Mode`] — or nothing at all when color is unwanted —
//! at format time, so nothing has to be initialised in `main`.
//!
//! Only foreground colors are ever painted. The terminal owns the
//! background and the default text color; recall-echo is line-oriented
//! output inside the user's shell, not a full-screen app.
//!
//! This module is the only place in `src/` allowed to spell out an escape
//! (`\x1b[` or `\u{1b}[`) — a test enforces it. The one exception is
//! `agent_cli.rs`, which strips escapes from captured CLI output.

use std::ffi::OsString;
use std::fmt;
use std::io::IsTerminal;
use std::sync::OnceLock;

/// How much color the current process may emit.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// No escapes at all. Every token renders as the empty string.
    Plain,
    /// `\x1b[38;5;Nm` with Gruvbox's canonical 256-color indices.
    Ansi256,
    /// `\x1b[38;2;r;g;bm` with Gruvbox Dark's exact hex values.
    Truecolor,
}

/// Decide the color mode from environment and stream state.
///
/// Pure — `env` is injected so the precedence table can be unit tested
/// without touching the process environment or a real tty. It yields the
/// raw `OsString` so that a non-UTF-8 value still counts as *set*: an
/// unreadable `NO_COLOR` must fail closed (plain), not open. An empty
/// value counts as unset for every variable, per no-color.org ("present
/// and not an empty string"). Precedence, first match wins:
///
/// 1. `CLICOLOR_FORCE` non-empty and not `"0"` → color regardless of tty/NO_COLOR
/// 2. `NO_COLOR` non-empty (any bytes) → [`Mode::Plain`]
/// 3. `TERM` unset, empty or `"dumb"` → [`Mode::Plain`]
/// 4. stdout or stderr not a tty → [`Mode::Plain`]
/// 5. `COLORTERM` is `truecolor` / `24bit` → [`Mode::Truecolor`]
/// 6. otherwise → [`Mode::Ansi256`]
pub fn resolve(env: impl Fn(&str) -> Option<OsString>, both_streams_tty: bool) -> Mode {
    let env = |k: &str| env(k).filter(|v| !v.is_empty());
    let colored = || {
        if env("COLORTERM").is_some_and(|v| matches!(v.to_str(), Some("truecolor" | "24bit"))) {
            Mode::Truecolor
        } else {
            Mode::Ansi256
        }
    };

    if env("CLICOLOR_FORCE").is_some_and(|v| v != "0") {
        return colored();
    }
    if env("NO_COLOR").is_some() {
        return Mode::Plain;
    }
    match env("TERM") {
        None => return Mode::Plain,
        Some(t) if t == "dumb" => return Mode::Plain,
        Some(_) => {}
    }
    if !both_streams_tty {
        return Mode::Plain;
    }
    colored()
}

/// The process-wide mode, resolved once from the real environment on
/// first use. Color needs both stdout and stderr to be terminals: painted
/// text goes to either stream, and a redirected one must never receive
/// escapes.
pub fn mode() -> Mode {
    static MODE: OnceLock<Mode> = OnceLock::new();
    *MODE.get_or_init(|| {
        resolve(
            |k| std::env::var_os(k),
            std::io::stdout().is_terminal() && std::io::stderr().is_terminal(),
        )
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Token {
    Good,
    Warn,
    Bad,
    Accent,
    Dim,
    Bold,
    Reset,
}

/// A semantic color token. `Display` writes its escape for [`mode()`].
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Paint(Token);

/// Success marks, HEALTHY, running/on. Gruvbox green.
pub static GOOD: Paint = Paint(Token::Good);
/// Warnings, WATCH, "exists", not running/off. Gruvbox yellow.
pub static WARN: Paint = Paint(Token::Warn);
/// Errors, ALERT, failure marks. Gruvbox red.
pub static BAD: Paint = Paint(Token::Bad);
/// Filenames, entity names, relation edges, the logo. Gruvbox aqua.
pub static ACCENT: Paint = Paint(Token::Accent);
/// Metadata, scores, hints, separators. Gruvbox gray (a color, not SGR faint).
pub static DIM: Paint = Paint(Token::Dim);
/// Section headers and emphasised names.
pub static BOLD: Paint = Paint(Token::Bold);
/// Back to the terminal's defaults.
pub static RESET: Paint = Paint(Token::Reset);

impl Paint {
    /// The escape this token writes in `mode`. [`Mode::Plain`] is always `""`.
    #[must_use]
    pub const fn in_mode(self, mode: Mode) -> &'static str {
        match (mode, self.0) {
            (Mode::Plain, _) => "",
            (_, Token::Reset) => "\x1b[0m",
            (_, Token::Bold) => "\x1b[1m",
            (Mode::Truecolor, Token::Good) => "\x1b[38;2;184;187;38m",
            (Mode::Truecolor, Token::Warn) => "\x1b[38;2;250;189;47m",
            (Mode::Truecolor, Token::Bad) => "\x1b[38;2;251;73;52m",
            (Mode::Truecolor, Token::Accent) => "\x1b[38;2;142;192;124m",
            (Mode::Truecolor, Token::Dim) => "\x1b[38;2;146;131;116m",
            (Mode::Ansi256, Token::Good) => "\x1b[38;5;142m",
            (Mode::Ansi256, Token::Warn) => "\x1b[38;5;214m",
            (Mode::Ansi256, Token::Bad) => "\x1b[38;5;167m",
            (Mode::Ansi256, Token::Accent) => "\x1b[38;5;108m",
            (Mode::Ansi256, Token::Dim) => "\x1b[38;5;245m",
        }
    }
}

impl fmt::Display for Paint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.in_mode(mode()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Paint; 7] = [GOOD, WARN, BAD, ACCENT, DIM, BOLD, RESET];

    /// Gruvbox Dark hex values, byte-identical to pulse-null's `GRUVBOX_DARK`.
    const GRUVBOX_HEX: [(&str, u32); 5] = [
        ("GOOD", 0xb8bb26),
        ("WARN", 0xfabd2f),
        ("BAD", 0xfb4934),
        ("ACCENT", 0x8ec07c),
        ("DIM", 0x928374),
    ];
    const COLORS: [Paint; 5] = [GOOD, WARN, BAD, ACCENT, DIM];

    fn env_of(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let owned: Vec<(String, OsString)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), OsString::from(*v)))
            .collect();
        move |k| {
            owned
                .iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.clone())
        }
    }

    /// `NO_COLOR` holding bytes that are not UTF-8 is still *set*.
    #[test]
    fn resolve_no_color_non_utf8_is_plain() {
        use std::os::unix::ffi::OsStringExt;
        let env = |k: &str| match k {
            "NO_COLOR" => Some(OsString::from_vec(vec![0xff])),
            "TERM" => Some(OsString::from("xterm")),
            "COLORTERM" => Some(OsString::from("truecolor")),
            _ => None,
        };
        assert_eq!(resolve(env, true), Mode::Plain);
    }

    // ── resolve() precedence table ──────────────────────────────────

    /// no-color.org: an empty `NO_COLOR` is "not set".
    #[test]
    fn resolve_no_color_empty_is_unset() {
        let env = env_of(&[
            ("TERM", "xterm"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", ""),
        ]);
        assert_eq!(resolve(env, true), Mode::Truecolor);
    }

    #[test]
    fn resolve_clicolor_force_empty_is_unset() {
        let env = env_of(&[("CLICOLOR_FORCE", ""), ("TERM", "xterm")]);
        assert_eq!(resolve(env, false), Mode::Plain);
    }

    #[test]
    fn resolve_term_empty_is_plain() {
        let env = env_of(&[("TERM", ""), ("COLORTERM", "truecolor")]);
        assert_eq!(resolve(env, true), Mode::Plain);
    }

    #[test]
    fn resolve_no_color_set_is_plain() {
        let env = env_of(&[
            ("TERM", "xterm"),
            ("COLORTERM", "truecolor"),
            ("NO_COLOR", "1"),
        ]);
        assert_eq!(resolve(env, true), Mode::Plain);
    }

    #[test]
    fn resolve_term_dumb_is_plain_even_with_truecolor() {
        let env = env_of(&[("TERM", "dumb"), ("COLORTERM", "truecolor")]);
        assert_eq!(resolve(env, true), Mode::Plain);
    }

    #[test]
    fn resolve_term_unset_is_plain() {
        let env = env_of(&[("COLORTERM", "truecolor")]);
        assert_eq!(resolve(env, true), Mode::Plain);
    }

    #[test]
    fn resolve_non_tty_is_plain() {
        let env = env_of(&[("TERM", "xterm-256color"), ("COLORTERM", "truecolor")]);
        assert_eq!(resolve(env, false), Mode::Plain);
    }

    #[test]
    fn resolve_clicolor_force_overrides_non_tty_and_no_color() {
        let env = env_of(&[("CLICOLOR_FORCE", "1"), ("NO_COLOR", "1"), ("TERM", "dumb")]);
        assert_eq!(resolve(env, false), Mode::Ansi256);
        let env = env_of(&[("CLICOLOR_FORCE", "1"), ("COLORTERM", "24bit")]);
        assert_eq!(resolve(env, false), Mode::Truecolor);
    }

    #[test]
    fn resolve_clicolor_force_zero_is_not_force() {
        let env = env_of(&[("CLICOLOR_FORCE", "0"), ("TERM", "xterm")]);
        assert_eq!(resolve(env, false), Mode::Plain);
    }

    #[test]
    fn resolve_truecolor_from_colorterm() {
        for v in ["truecolor", "24bit"] {
            let env = env_of(&[("TERM", "xterm"), ("COLORTERM", v)]);
            assert_eq!(resolve(env, true), Mode::Truecolor, "COLORTERM={v}");
        }
    }

    #[test]
    fn resolve_tty_without_colorterm_is_ansi256() {
        let env = env_of(&[("TERM", "xterm-256color")]);
        assert_eq!(resolve(env, true), Mode::Ansi256);
        let env = env_of(&[("TERM", "xterm"), ("COLORTERM", "yes")]);
        assert_eq!(resolve(env, true), Mode::Ansi256);
    }

    // ── escapes ─────────────────────────────────────────────────────

    /// Parse `\x1b[38;2;r;g;bm` back into a hex triple.
    fn rgb_of(escape: &str) -> u32 {
        let body = escape
            .strip_prefix("\x1b[38;2;")
            .and_then(|s| s.strip_suffix('m'))
            .expect("truecolor escape shape");
        let parts: Vec<u32> = body.split(';').map(|p| p.parse().unwrap()).collect();
        assert_eq!(parts.len(), 3);
        (parts[0] << 16) | (parts[1] << 8) | parts[2]
    }

    #[test]
    fn truecolor_escapes_match_gruvbox_hex() {
        for (paint, (name, hex)) in COLORS.iter().zip(GRUVBOX_HEX) {
            assert_eq!(rgb_of(paint.in_mode(Mode::Truecolor)), hex, "{name}");
        }
    }

    #[test]
    fn ansi256_indices() {
        let expected = [142, 214, 167, 108, 245];
        for (paint, n) in COLORS.iter().zip(expected) {
            assert_eq!(paint.in_mode(Mode::Ansi256), format!("\x1b[38;5;{n}m"));
        }
    }

    #[test]
    fn plain_is_empty_for_every_token() {
        for paint in ALL {
            assert_eq!(paint.in_mode(Mode::Plain), "");
        }
    }

    #[test]
    fn bold_and_reset_are_mode_independent_attributes() {
        for mode in [Mode::Ansi256, Mode::Truecolor] {
            assert_eq!(BOLD.in_mode(mode), "\x1b[1m");
            assert_eq!(RESET.in_mode(mode), "\x1b[0m");
        }
    }

    #[test]
    fn dim_is_color_not_faint() {
        for mode in [Mode::Ansi256, Mode::Truecolor] {
            let esc = DIM.in_mode(mode);
            assert_ne!(esc, "\x1b[2m");
            assert!(esc.starts_with("\x1b[38;"), "{esc:?}");
        }
    }

    #[test]
    fn no_basic_16_color_escapes() {
        for paint in ALL {
            for mode in [Mode::Ansi256, Mode::Truecolor] {
                let esc = paint.in_mode(mode);
                assert!(
                    !matches!(esc, "\x1b[31m" | "\x1b[32m" | "\x1b[33m" | "\x1b[36m"),
                    "{esc:?}"
                );
            }
        }
    }
}

#[cfg(test)]
mod source_scan {
    use std::path::Path;

    fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).expect("read src dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rs_files(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `src/theme.rs` is the only file allowed to spell out an escape, in
    /// any Rust spelling or as a literal ESC byte. `agent_cli.rs` is exempt: it strips escapes
    /// from captured output and carries a fixture to prove it.
    #[test]
    fn no_raw_escapes_outside_theme() {
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rs_files(&src, &mut files);
        assert!(
            !files.is_empty(),
            "scan walked nothing — CARGO_MANIFEST_DIR wrong?"
        );
        let exempt = [src.join("theme.rs"), src.join("agent_cli.rs")];
        let needles = [
            "\\x1b[",
            "\\x1B[",
            "\\u{1b}[",
            "\\u{1B}[",
            "\\u{001b}[",
            "\u{1b}[",
        ];
        let offenders: Vec<_> = files
            .iter()
            .filter(|p| !exempt.contains(p))
            .filter(|p| {
                let src = std::fs::read_to_string(p).expect("read");
                needles.iter().any(|needle| src.contains(needle))
            })
            .collect();
        assert!(
            offenders.is_empty(),
            "raw ANSI escapes outside theme.rs: {offenders:?}"
        );
    }
}
