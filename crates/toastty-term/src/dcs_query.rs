//! DCS status queries: XTGETTCAP (`DCS + q Pt ST`) and DECRQSS
//! (`DCS $ q Pt ST`).
//!
//! Both are request/response probes apps use to find out what the
//! terminal really supports, independent of `$TERM`. The headline user
//! is truecolor detection: neovim's `detect_termguicolors` (in
//! `runtime/lua/vim/_core/defaults.lua`) asks XTGETTCAP for `Tc` / `RGB`
//! / `setrgbf` + `setrgbb`, and separately sets `CSI 48;2;1;2;3 m` and
//! reads it back with DECRQSS `m`. Without replies, it falls back to
//! 256 colours even though toastty renders 24-bit SGR fine.
//!
//! This module is pure: [`Term`](crate::Term) buffers the DCS body in a
//! [`DcsQuery`] (bounded by [`DCS_QUERY_CAP`]) and calls
//! [`xtgettcap_reply`] / [`decrqss_reply`] on `unhook`.
//!
//! References:
//! - xterm ctlseqs, "DCS + q Pt ST" and "DCS $ q Pt ST":
//!   <https://invisible-island.net/xterm/ctlseqs/ctlseqs.html>
//! - kitty's `get_capabilities` (`kitty/terminfo.py`), whose
//!   one-reply-per-name shape we follow.

use std::fmt::Write as _;

use toastty_config::CursorShape;

use crate::cell::{Color, Style};

/// Upper bound on a buffered query body. Real queries are tiny (neovim's
/// four-cap XTGETTCAP is 27 bytes); the cap only exists so a hostile or
/// runaway stream can't grow memory without bound, and so the reply to
/// a single request stays bounded too (each name is at least one byte
/// plus a `;`, so at most ~2k replies of a few dozen bytes each).
pub(crate) const DCS_QUERY_CAP: usize = 4096;

/// Which status query a DCS introducer selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DcsQueryKind {
    /// `DCS + q` — XTGETTCAP (request termcap/terminfo string).
    Xtgettcap,
    /// `DCS $ q` — DECRQSS (request selection or setting).
    Decrqss,
}

impl DcsQueryKind {
    /// Classify a DCS from vte's `hook` arguments. Both queries use
    /// final byte `q` (like sixel) and are told apart by the single
    /// intermediate. Anything else isn't a query we answer.
    pub(crate) fn from_hook(intermediates: &[u8], action: char) -> Option<Self> {
        match (intermediates, action) {
            (b"+", 'q') => Some(Self::Xtgettcap),
            (b"$", 'q') => Some(Self::Decrqss),
            _ => None,
        }
    }
}

/// An in-progress DCS query body, accumulated byte-by-byte from vte's
/// `put` callbacks (so a query split across PTY reads reassembles).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DcsQuery {
    pub(crate) kind: DcsQueryKind,
    buf: Vec<u8>,
    /// Set once the body exceeded [`DCS_QUERY_CAP`]; the request is
    /// then answered with a single "invalid" reply.
    overflow: bool,
}

impl DcsQuery {
    pub(crate) fn new(kind: DcsQueryKind) -> Self {
        Self {
            kind,
            buf: Vec::new(),
            overflow: false,
        }
    }

    /// Append one body byte, flagging overflow past the cap.
    pub(crate) fn push(&mut self, byte: u8) {
        if self.buf.len() < DCS_QUERY_CAP {
            self.buf.push(byte);
        } else {
            self.overflow = true;
        }
    }

    /// Build the reply bytes for the completed query.
    pub(crate) fn reply(&self, state: &QueryState) -> Vec<u8> {
        match (self.kind, self.overflow) {
            (DcsQueryKind::Xtgettcap, true) => XTGETTCAP_INVALID.to_vec(),
            (DcsQueryKind::Decrqss, true) => DECRQSS_INVALID.to_vec(),
            (DcsQueryKind::Xtgettcap, false) => xtgettcap_reply(&self.buf, state),
            (DcsQueryKind::Decrqss, false) => decrqss_reply(&self.buf, state),
        }
    }
}

/// Snapshot of the terminal state the queries report on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct QueryState {
    /// Current SGR rendition (DECRQSS `m`).
    pub(crate) style: Style,
    /// DECSTBM margins, 0-indexed inclusive (DECRQSS `r`).
    pub(crate) scroll_top: u16,
    pub(crate) scroll_bot: u16,
    /// Cursor shape + blink (DECRQSS ` q`).
    pub(crate) cursor_shape: CursorShape,
    pub(crate) cursor_blink: bool,
    /// Screen size (DECRQSS `t` / `$|`).
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Whether OSC 52 writes are allowed by `[security]`; `Ms` is only
    /// advertised when they are, so tmux etc. don't route clipboard
    /// writes into a terminal that will drop them.
    pub(crate) osc52_write: bool,
}

/// `DCS 0 + r ST` — xterm's reply for an invalid XTGETTCAP request.
const XTGETTCAP_INVALID: &[u8] = b"\x1bP0+r\x1b\\";
/// `DCS 0 $ r ST` — xterm's reply for an invalid DECRQSS request.
const DECRQSS_INVALID: &[u8] = b"\x1bP0$r\x1b\\";

/// The terminfo name reported for `TN` / `name`. Matches the `TERM`
/// toastty exports to children (`xterm-256color`, see the spawn site in
/// the binary): the point of `TN` is to name a description the app can
/// load, and `toastty` isn't installed on most hosts.
pub(crate) const TERMINFO_NAME: &str = "xterm-256color";

/// A capability's value as reported by XTGETTCAP.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CapValue {
    /// Boolean capability: reply carries the name only (kitty).
    Bool,
    /// String / numeric capability, in terminfo source form (`\E` for
    /// ESC, `%p1%d` parameters) like kitty sends; neovim's
    /// `vim.tty.query` un-escapes `\E` itself.
    Str(&'static str),
}

/// Look up one capability name. Only capabilities toastty genuinely
/// implements are listed — notably NOT `Smulx` / `Setulc` (styled and
/// coloured underlines aren't rendered) and no kitty-keyboard cap (there
/// is no standard terminfo name; apps probe `CSI ? u` instead).
fn lookup_cap(name: &str, state: &QueryState) -> Option<CapValue> {
    let v = match name {
        "TN" | "name" => CapValue::Str(TERMINFO_NAME),
        // 256-colour palette (termcap `Co`, terminfo `colors`).
        "Co" | "colors" => CapValue::Str("256"),
        // Direct colour: ncurses `RGB` extension and tmux's `Tc`.
        "RGB" | "Tc" => CapValue::Bool,
        "setrgbf" => CapValue::Str("\\E[38;2;%p1%d;%p2%d;%p3%dm"),
        "setrgbb" => CapValue::Str("\\E[48;2;%p1%d;%p2%d;%p3%dm"),
        // DECSCUSR cursor shape (tmux / neovim extension caps). `Se`
        // matches `terminfo/toastty.terminfo`.
        "Ss" => CapValue::Str("\\E[%p1%d q"),
        "Se" => CapValue::Str("\\E[ q"),
        // OSC 52 clipboard set (tmux's `Ms`), only when allowed.
        "Ms" if state.osc52_write => CapValue::Str("\\E]52;%p1%s;%p2%s\\007"),
        // Synchronized output (DECSET 2026), tmux's `Sync` form.
        "Sync" => CapValue::Str("\\E[?2026%?%p1%{1}%-%tl%eh%;"),
        // Bracketed paste (DECSET 2004), ncurses extension names.
        "BE" => CapValue::Str("\\E[?2004h"),
        "BD" => CapValue::Str("\\E[?2004l"),
        "PS" => CapValue::Str("\\E[200~"),
        "PE" => CapValue::Str("\\E[201~"),
        // Focus reporting (DECSET 1004), ncurses extension names.
        "fe" => CapValue::Str("\\E[?1004h"),
        "fd" => CapValue::Str("\\E[?1004l"),
        "kxIN" => CapValue::Str("\\E[I"),
        "kxOUT" => CapValue::Str("\\E[O"),
        _ => return None,
    };
    Some(v)
}

/// Uppercase hex, two digits per byte (xterm's encoding).
pub(crate) fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02X}");
    }
    s
}

/// Decode two-digits-per-byte hex (either case). `None` for an empty,
/// odd-length or non-hex input.
pub(crate) fn hex_decode(hex: &[u8]) -> Option<Vec<u8>> {
    if hex.is_empty() || !hex.len().is_multiple_of(2) {
        return None;
    }
    let (pairs, _) = hex.as_chunks::<2>();
    pairs
        .iter()
        .map(|&[hi, lo]| {
            let hi = char::from(hi).to_digit(16)?;
            let lo = char::from(lo).to_digit(16)?;
            u8::try_from(hi * 16 + lo).ok()
        })
        .collect()
}

/// Answer an XTGETTCAP body (`<hex>;<hex>;...`).
///
/// One DCS reply per requested name, in request order, like kitty:
/// `DCS 1 + r <hex-name> ST` for a boolean, `DCS 1 + r <hex-name> =
/// <hex-value> ST` for a string/number, and `DCS 0 + r <hex-name> ST`
/// for an unknown name. xterm instead packs all pairs into one reply and
/// stops at the first unknown name; per-name replies are what neovim's
/// `vim.tty.query` matcher (`^\eP1\+r(%x+)(.*)$`) expects, and they let
/// a client tell exactly which name failed. A name that isn't valid hex
/// / UTF-8 gets xterm's bare `DCS 0 + r ST` (we don't echo arbitrary
/// bytes back) and processing continues.
pub(crate) fn xtgettcap_reply(body: &[u8], state: &QueryState) -> Vec<u8> {
    let mut out = Vec::new();
    for encoded in body.split(|&b| b == b';') {
        let Some(name) = hex_decode(encoded).and_then(|n| String::from_utf8(n).ok()) else {
            out.extend_from_slice(XTGETTCAP_INVALID);
            continue;
        };
        // Echo the name exactly as the client sent it, so string
        // matching on the client side works whatever case it used.
        let encoded = std::str::from_utf8(encoded).unwrap_or_default();
        match lookup_cap(&name, state) {
            Some(CapValue::Bool) => {
                let _ = write!(Raw(&mut out), "\x1bP1+r{encoded}\x1b\\");
            }
            Some(CapValue::Str(value)) => {
                let value = hex_encode(value.as_bytes());
                let _ = write!(Raw(&mut out), "\x1bP1+r{encoded}={value}\x1b\\");
            }
            None => {
                let _ = write!(Raw(&mut out), "\x1bP0+r{encoded}\x1b\\");
            }
        }
    }
    out
}

/// Answer a DECRQSS body. Valid requests reply `DCS 1 $ r <value><Pt>
/// ST` (xterm's polarity: `1` = valid, the reverse of the VT510 manual,
/// which every modern terminal follows); anything else replies `DCS 0 $
/// r ST`. Supported settings:
///
/// | Pt    | Setting  | Example reply value |
/// | ----- | -------- | ------------------- |
/// | `m`   | SGR      | `0;1;38:2::255:0:0` |
/// | `r`   | DECSTBM  | `1;25`              |
/// | ` q`  | DECSCUSR | `2`                 |
/// | `"p`  | DECSCL   | `62;1` (VT220, 7-bit, matching DA1) |
/// | `t`   | DECSLPP  | rows                |
/// | `$\|` | DECSCPP  | columns             |
///
/// DECSLRM (`s`) and DECSCA (`"q`) are reported invalid because toastty
/// doesn't implement left/right margins or character protection.
pub(crate) fn decrqss_reply(body: &[u8], state: &QueryState) -> Vec<u8> {
    let value = match body {
        b"m" => sgr_report(&state.style),
        b"r" => format!(
            "{};{}",
            u32::from(state.scroll_top) + 1,
            u32::from(state.scroll_bot) + 1
        ),
        b" q" => decscusr_value(state.cursor_shape, state.cursor_blink).to_string(),
        b"\"p" => "62;1".to_owned(),
        b"t" => state.rows.to_string(),
        b"$|" => state.cols.to_string(),
        _ => return DECRQSS_INVALID.to_vec(),
    };
    let pt = std::str::from_utf8(body).unwrap_or_default();
    format!("\x1bP1$r{value}{pt}\x1b\\").into_bytes()
}

/// DECSCUSR `Ps` for a shape + blink pair (the inverse of
/// `Term::apply_decscusr`; `0` is reported as its alias `1`).
fn decscusr_value(shape: CursorShape, blink: bool) -> u8 {
    let base = match shape {
        CursorShape::Block => 1,
        CursorShape::Underline => 3,
        CursorShape::Bar => 5,
    };
    if blink { base } else { base + 1 }
}

/// Render `style` as the SGR parameter string xterm's DECRQSS `m`
/// reports: always a leading `0`, then attributes, then colours, with
/// extended colours in the ITU T.416 colon form (`38:5:N`,
/// `38:2::R:G:B` with the empty colour-space slot). Neovim's truecolor
/// probe accepts exactly this shape (`0;48:2::1:2:3`).
///
/// Only renditions toastty stores are reported (bold, italic,
/// underline, reverse); the underline colour (SGR 58) is deliberately
/// omitted since it isn't rendered.
pub(crate) fn sgr_report(style: &Style) -> String {
    let mut s = String::from("0");
    let f = style.flags;
    for (on, code) in [
        (f.bold, "1"),
        (f.italic, "3"),
        (f.underline, "4"),
        (f.reverse, "7"),
    ] {
        if on {
            s.push(';');
            s.push_str(code);
        }
    }
    push_color(&mut s, style.fg, 30, 90, 38);
    push_color(&mut s, style.bg, 40, 100, 48);
    s
}

/// Append one colour: `base + n` for the 8 normal named colours,
/// `bright + n` for the 8 bright ones, `ext:5:N` / `ext:2::R:G:B` for
/// indexed / direct colour, nothing for the default.
fn push_color(s: &mut String, color: Color, base: u8, bright: u8, ext: u8) {
    let _ = match color {
        Color::Default => Ok(()),
        Color::Indexed256(n) => write!(s, ";{ext}:5:{n}"),
        Color::Rgb(r, g, b) => write!(s, ";{ext}:2::{r}:{g}:{b}"),
        named => {
            let n = named_index(named);
            if n < 8 {
                write!(s, ";{}", base + n)
            } else {
                write!(s, ";{}", bright + n - 8)
            }
        }
    };
}

/// Palette index (0..16) of a named colour. Callers only pass named
/// variants; `Default` / extended map to 0 defensively.
fn named_index(c: Color) -> u8 {
    match c {
        Color::Red => 1,
        Color::Green => 2,
        Color::Yellow => 3,
        Color::Blue => 4,
        Color::Magenta => 5,
        Color::Cyan => 6,
        Color::White => 7,
        Color::BrightBlack => 8,
        Color::BrightRed => 9,
        Color::BrightGreen => 10,
        Color::BrightYellow => 11,
        Color::BrightBlue => 12,
        Color::BrightMagenta => 13,
        Color::BrightCyan => 14,
        Color::BrightWhite => 15,
        _ => 0,
    }
}

/// `fmt::Write` over a byte buffer, so replies can be `write!`-ed
/// straight into the output without an intermediate `String`.
struct Raw<'a>(&'a mut Vec<u8>);

impl std::fmt::Write for Raw<'_> {
    fn write_str(&mut self, s: &str) -> std::fmt::Result {
        self.0.extend_from_slice(s.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::StyleFlags;

    fn state() -> QueryState {
        QueryState {
            style: Style::RESET,
            scroll_top: 0,
            scroll_bot: 24,
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            rows: 25,
            cols: 80,
            osc52_write: false,
        }
    }

    fn tcap(names: &[&str]) -> String {
        let body: Vec<String> = names.iter().map(|n| hex_encode(n.as_bytes())).collect();
        String::from_utf8(xtgettcap_reply(body.join(";").as_bytes(), &state())).unwrap()
    }

    fn rqss(pt: &str, st: &QueryState) -> String {
        String::from_utf8(decrqss_reply(pt.as_bytes(), st)).unwrap()
    }

    #[test]
    fn hex_roundtrip() {
        assert_eq!(hex_encode(b"Tc"), "5463");
        assert_eq!(hex_encode(b""), "");
        assert_eq!(hex_decode(b"5463").unwrap(), b"Tc");
        assert_eq!(hex_decode(b"6a6B").unwrap(), b"jk");
        assert_eq!(hex_decode(b"\xff\x00"), None);
        let all: Vec<u8> = (0..=255).collect();
        assert_eq!(hex_decode(hex_encode(&all).as_bytes()).unwrap(), all);
    }

    #[test]
    fn hex_decode_rejects_bad_input() {
        assert_eq!(hex_decode(b""), None);
        assert_eq!(hex_decode(b"546"), None);
        assert_eq!(hex_decode(b"5G"), None);
        assert_eq!(hex_decode(b"zz"), None);
        assert_eq!(hex_decode("é1".as_bytes()), None);
    }

    #[test]
    fn query_kind_from_hook() {
        assert_eq!(
            DcsQueryKind::from_hook(b"+", 'q'),
            Some(DcsQueryKind::Xtgettcap)
        );
        assert_eq!(
            DcsQueryKind::from_hook(b"$", 'q'),
            Some(DcsQueryKind::Decrqss)
        );
        assert_eq!(DcsQueryKind::from_hook(b"", 'q'), None);
        assert_eq!(DcsQueryKind::from_hook(b"+", 'p'), None);
        assert_eq!(DcsQueryKind::from_hook(b"$+", 'q'), None);
    }

    #[test]
    fn xtgettcap_boolean_caps() {
        assert_eq!(tcap(&["Tc"]), "\x1bP1+r5463\x1b\\");
        assert_eq!(tcap(&["RGB"]), "\x1bP1+r524742\x1b\\");
    }

    #[test]
    fn xtgettcap_string_caps() {
        let tn = hex_encode(TERMINFO_NAME.as_bytes());
        assert_eq!(tcap(&["TN"]), format!("\x1bP1+r544E={tn}\x1b\\"));
        assert_eq!(tcap(&["name"]), format!("\x1bP1+r6E616D65={tn}\x1b\\"));
        assert_eq!(tcap(&["Co"]), "\x1bP1+r436F=323536\x1b\\");
        assert_eq!(tcap(&["colors"]), "\x1bP1+r636F6C6F7273=323536\x1b\\");
        let f = hex_encode(b"\\E[38;2;%p1%d;%p2%d;%p3%dm");
        assert_eq!(
            tcap(&["setrgbf"]),
            format!("\x1bP1+r{}={f}\x1b\\", hex_encode(b"setrgbf"))
        );
        let b = hex_encode(b"\\E[48;2;%p1%d;%p2%d;%p3%dm");
        assert_eq!(
            tcap(&["setrgbb"]),
            format!("\x1bP1+r{}={b}\x1b\\", hex_encode(b"setrgbb"))
        );
        for name in [
            "Ss", "Se", "Sync", "BE", "BD", "PS", "PE", "fe", "fd", "kxIN", "kxOUT",
        ] {
            assert!(tcap(&[name]).starts_with("\x1bP1+r"), "{name}");
        }
    }

    #[test]
    fn xtgettcap_unsupported_caps_are_rejected() {
        // Styled / coloured underlines aren't rendered, so not advertised.
        for name in ["Smulx", "Setulc", "Smol", "bogus"] {
            let hex = hex_encode(name.as_bytes());
            assert_eq!(tcap(&[name]), format!("\x1bP0+r{hex}\x1b\\"));
        }
    }

    #[test]
    fn xtgettcap_ms_is_gated_on_osc52_write() {
        let body = hex_encode(b"Ms");
        let denied = xtgettcap_reply(body.as_bytes(), &state());
        assert_eq!(denied, b"\x1bP0+r4D73\x1b\\");
        let allowed = QueryState {
            osc52_write: true,
            ..state()
        };
        let reply = String::from_utf8(xtgettcap_reply(body.as_bytes(), &allowed)).unwrap();
        let value = hex_encode(b"\\E]52;%p1%s;%p2%s\\007");
        assert_eq!(reply, format!("\x1bP1+r4D73={value}\x1b\\"));
    }

    #[test]
    fn xtgettcap_multiple_names_reply_per_name_in_order() {
        assert_eq!(
            tcap(&["Tc", "bogus", "RGB"]),
            format!(
                "\x1bP1+r5463\x1b\\\x1bP0+r{}\x1b\\\x1bP1+r524742\x1b\\",
                hex_encode(b"bogus")
            )
        );
    }

    #[test]
    fn xtgettcap_echoes_client_hex_case() {
        let r = xtgettcap_reply(b"544e", &state());
        assert!(r.starts_with(b"\x1bP1+r544e="));
    }

    #[test]
    fn xtgettcap_invalid_hex_gets_bare_invalid_and_continues() {
        assert_eq!(xtgettcap_reply(b"", &state()), XTGETTCAP_INVALID);
        assert_eq!(xtgettcap_reply(b"54G3", &state()), XTGETTCAP_INVALID);
        assert_eq!(xtgettcap_reply(b"546", &state()), XTGETTCAP_INVALID);
        // Valid hex but not UTF-8.
        assert_eq!(xtgettcap_reply(b"FF", &state()), XTGETTCAP_INVALID);
        let mixed = xtgettcap_reply(b"zz;5463;", &state());
        assert_eq!(
            mixed,
            b"\x1bP0+r\x1b\\\x1bP1+r5463\x1b\\\x1bP0+r\x1b\\".to_vec()
        );
    }

    #[test]
    fn dcs_query_accumulates_and_caps() {
        let mut q = DcsQuery::new(DcsQueryKind::Xtgettcap);
        for b in b"5463" {
            q.push(*b);
        }
        assert_eq!(q.reply(&state()), b"\x1bP1+r5463\x1b\\");

        let mut big = DcsQuery::new(DcsQueryKind::Xtgettcap);
        for _ in 0..DCS_QUERY_CAP {
            big.push(b'5');
        }
        assert!(!big.overflow);
        big.push(b'4');
        assert!(big.overflow);
        assert_eq!(big.reply(&state()), XTGETTCAP_INVALID);

        let mut big = DcsQuery::new(DcsQueryKind::Decrqss);
        for _ in 0..=DCS_QUERY_CAP {
            big.push(b'm');
        }
        assert_eq!(big.reply(&state()), DECRQSS_INVALID);

        let mut m = DcsQuery::new(DcsQueryKind::Decrqss);
        m.push(b'm');
        assert_eq!(m.reply(&state()), b"\x1bP1$r0m\x1b\\");
    }

    #[test]
    fn decrqss_sgr_default() {
        assert_eq!(rqss("m", &state()), "\x1bP1$r0m\x1b\\");
    }

    #[test]
    fn decrqss_sgr_attrs_and_colors() {
        let st = QueryState {
            style: Style {
                fg: Color::Rgb(255, 0, 7),
                bg: Color::Indexed256(42),
                flags: StyleFlags {
                    bold: true,
                    italic: true,
                    underline: true,
                    reverse: true,
                },
            },
            ..state()
        };
        assert_eq!(
            rqss("m", &st),
            "\x1bP1$r0;1;3;4;7;38:2::255:0:7;48:5:42m\x1b\\"
        );
    }

    #[test]
    fn decrqss_sgr_matches_neovim_probe_shape() {
        let st = QueryState {
            style: Style {
                bg: Color::Rgb(1, 2, 3),
                ..Style::RESET
            },
            ..state()
        };
        assert_eq!(rqss("m", &st), "\x1bP1$r0;48:2::1:2:3m\x1b\\");
    }

    #[test]
    fn sgr_report_named_colors() {
        let named = [
            Color::Black,
            Color::Red,
            Color::Green,
            Color::Yellow,
            Color::Blue,
            Color::Magenta,
            Color::Cyan,
            Color::White,
            Color::BrightBlack,
            Color::BrightRed,
            Color::BrightGreen,
            Color::BrightYellow,
            Color::BrightBlue,
            Color::BrightMagenta,
            Color::BrightCyan,
            Color::BrightWhite,
        ];
        for (i, c) in named.into_iter().enumerate() {
            let i = u8::try_from(i).unwrap();
            let (fg, bg) = if i < 8 {
                (30 + i, 40 + i)
            } else {
                (90 + i - 8, 100 + i - 8)
            };
            let style = Style {
                fg: c,
                bg: c,
                ..Style::RESET
            };
            assert_eq!(sgr_report(&style), format!("0;{fg};{bg}"));
        }
        // Defensive fallbacks are unreachable from push_color.
        assert_eq!(named_index(Color::Default), 0);
        assert_eq!(named_index(Color::Rgb(1, 2, 3)), 0);
    }

    #[test]
    fn decrqss_decstbm() {
        assert_eq!(rqss("r", &state()), "\x1bP1$r1;25r\x1b\\");
        let st = QueryState {
            scroll_top: 4,
            scroll_bot: 9,
            ..state()
        };
        assert_eq!(rqss("r", &st), "\x1bP1$r5;10r\x1b\\");
    }

    #[test]
    fn decrqss_decscusr() {
        let cases = [
            (CursorShape::Block, true, 1),
            (CursorShape::Block, false, 2),
            (CursorShape::Underline, true, 3),
            (CursorShape::Underline, false, 4),
            (CursorShape::Bar, true, 5),
            (CursorShape::Bar, false, 6),
        ];
        for (shape, blink, ps) in cases {
            let st = QueryState {
                cursor_shape: shape,
                cursor_blink: blink,
                ..state()
            };
            assert_eq!(rqss(" q", &st), format!("\x1bP1$r{ps} q\x1b\\"));
        }
    }

    #[test]
    fn decrqss_cheap_settings() {
        assert_eq!(rqss("\"p", &state()), "\x1bP1$r62;1\"p\x1b\\");
        assert_eq!(rqss("t", &state()), "\x1bP1$r25t\x1b\\");
        assert_eq!(rqss("$|", &state()), "\x1bP1$r80$|\x1b\\");
    }

    #[test]
    fn decrqss_invalid_requests() {
        for pt in ["", "s", "\"q", "mm", "x", " m", "q"] {
            assert_eq!(rqss(pt, &state()), "\x1bP0$r\x1b\\", "{pt:?}");
        }
    }
}
