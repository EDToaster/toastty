//! Mouse-event encoding (SGR 1006 + DECSET 1000 / 1002 / 1003).
//!
//! We only implement the SGR (1006) encoding. The legacy X10 byte-packed
//! encoding caps at 223 cols and is functionally useless on a modern
//! terminal; nobody opts into 1000/1002 without also opting into 1006.
//!
//! When SGR is off but the protocol bit is on, we still emit SGR — it's
//! a strict superset of the X10 information. Real-world apps that care
//! enable both; the gap exists historically and we do not need to
//! preserve the broken legacy path.
//!
//! ## Encoding shape
//!
//! ```text
//! Press:    CSI < cb ; col ; row M
//! Release:  CSI < cb ; col ; row m
//! ```
//!
//! `cb` is the button code:
//! - Left=0, Middle=1, Right=2
//! - Scroll-up=64, Scroll-down=65, Scroll-left=66, Scroll-right=67
//! - +16 if Shift, +8 if Alt, +4 if Ctrl
//! - +32 if motion-while-button-held (drag report under DECSET 1002/1003)
//!
//! `col` and `row` are 1-based, clamped to the visible grid.

use toastty_term::{MouseMode, MouseProtocol, Row};
use toastty_window::{KeyState, Modifiers, MouseButton};

/// Logical mouse-event kind seen at the encoder boundary.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MouseEventKind {
    /// A button transitioned to pressed.
    Press(MouseButton),
    /// A button transitioned to released.
    Release(MouseButton),
    /// The cursor moved. `held` is the button currently held (if any) —
    /// used to compute the +32 motion bit and to know which button code to
    /// report.
    Motion { held: Option<MouseButton> },
    /// Wheel / trackpad scroll. Sign of `dy` decides up vs down.
    Scroll { dx: f64, dy: f64 },
}

/// Convert a button to its SGR code base (before modifier / motion bits).
///
/// Returns `None` for buttons we don't report (Back/Forward — those have
/// no canonical SGR code in xterm; some terminals invent 128/129 but
/// apps don't rely on them).
fn button_code(b: MouseButton) -> Option<u32> {
    match b {
        MouseButton::Left => Some(0),
        MouseButton::Middle => Some(1),
        MouseButton::Right => Some(2),
        MouseButton::Back | MouseButton::Forward | MouseButton::Other(_) => None,
    }
}

/// Modifier bits added to the SGR button code.
fn mod_bits(modifiers: Modifiers) -> u32 {
    let mut b = 0;
    if modifiers.contains(Modifiers::SHIFT) {
        b += 4; // Shift in xterm is the "report 4" bit (+4)
    }
    if modifiers.contains(Modifiers::ALT) {
        b += 8;
    }
    if modifiers.contains(Modifiers::CONTROL) {
        b += 16;
    }
    b
}

// Note: xterm's spec lists Shift=4, Meta(Alt)=8, Control=16, but several
// terminals (including kitty) swap Shift↔Control. We follow xterm's
// canonical assignment: Shift=4, Alt=8, Control=16 — same as Alacritty.

/// Convert pixel `(x, y)` to 1-based `(col, row)`, clamped to the grid.
#[must_use]
pub fn pixel_to_cell(
    pixel: (f64, f64),
    cell_size: (f32, f32),
    grid: (u16, u16),
) -> (u16, u16) {
    let (cw, ch) = cell_size;
    if cw <= 0.0 || ch <= 0.0 {
        return (1, 1);
    }
    let col = ((pixel.0 / f64::from(cw)).floor().max(0.0) as u32) + 1;
    let row = ((pixel.1 / f64::from(ch)).floor().max(0.0) as u32) + 1;
    let (rows, cols) = grid;
    let col = col.min(u32::from(cols.max(1))) as u16;
    let row = row.min(u32::from(rows.max(1))) as u16;
    (col, row)
}

/// Encode a mouse event into bytes suitable for the PTY, or `None` if the
/// event should not be reported under the current mode.
///
/// `mode` controls whether and how events are emitted:
/// - `Off` — never emit.
/// - `X10` — emit press/release only.
/// - `ButtonMotion` — emit press/release + motion when a button is held.
/// - `AnyMotion` — emit press/release + any motion (with or without a
///   held button).
///
/// `cell` is 1-based `(col, row)`.
#[must_use]
pub fn encode_mouse(
    kind: MouseEventKind,
    cell: (u16, u16),
    modifiers: Modifiers,
    mode: MouseMode,
) -> Option<Vec<u8>> {
    if !mode.is_on() {
        return None;
    }
    let (col, row) = cell;
    match kind {
        MouseEventKind::Press(b) => {
            let code = button_code(b)? + mod_bits(modifiers);
            Some(sgr_seq(code, col, row, true))
        }
        MouseEventKind::Release(b) => {
            let code = button_code(b)? + mod_bits(modifiers);
            Some(sgr_seq(code, col, row, false))
        }
        MouseEventKind::Motion { held } => {
            if !mode.report_drag() {
                return None;
            }
            if held.is_none() && !mode.report_any_motion() {
                return None;
            }
            let base = held.and_then(button_code).unwrap_or(3); // "no button" sentinel
            let code = base + mod_bits(modifiers) + 32;
            Some(sgr_seq(code, col, row, true))
        }
        MouseEventKind::Scroll { dx, dy } => {
            // Quantize: every event with a non-zero magnitude is one
            // notch; sign decides direction. Horizontal scroll uses
            // 66/67 (rarely used but cheap to encode).
            if dy.abs() < f64::EPSILON && dx.abs() < f64::EPSILON {
                return None;
            }
            let mut out = Vec::new();
            if dy.abs() > f64::EPSILON {
                // Convention: positive dy = "content moves down", which is
                // a scroll-down event from the app's point of view.
                let code = if dy > 0.0 { 65 } else { 64 } + mod_bits(modifiers);
                out.extend_from_slice(&sgr_seq(code, col, row, true));
            }
            if dx.abs() > f64::EPSILON {
                let code = if dx > 0.0 { 67 } else { 66 } + mod_bits(modifiers);
                out.extend_from_slice(&sgr_seq(code, col, row, true));
            }
            Some(out)
        }
    }
}

/// Build a single SGR mouse CSI sequence.
fn sgr_seq(code: u32, col: u16, row: u16, press: bool) -> Vec<u8> {
    let suffix = if press { 'M' } else { 'm' };
    format!("\x1b[<{code};{col};{row}{suffix}").into_bytes()
}

/// Translate the raw winit event-stream state into a [`MouseEventKind`].
///
/// `state` is the press/release transition delivered by winit. This is a
/// thin convenience for the dispatcher; the encoder above operates on
/// [`MouseEventKind`] directly.
#[must_use]
pub fn classify_button_event(button: MouseButton, state: KeyState) -> MouseEventKind {
    match state {
        KeyState::Pressed => MouseEventKind::Press(button),
        KeyState::Released => MouseEventKind::Release(button),
    }
}

/// True when `c` is a word character. We use a deliberately
/// conservative rule: ASCII alphanumerics plus `_`. Apps that
/// produce dotted/dashed identifiers (URLs, file paths) can still
/// be word-selected via drag — the heuristic just keeps double-click
/// from grabbing too much.
#[must_use]
pub fn is_word_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Word boundaries that contain column `col` on `row`. Scans left
/// and right while [`is_word_char`] holds. If the cell at `col` is
/// not a word character, returns `(col, col)` — caller is expected
/// to fall back to the single-char selection in that case.
#[must_use]
pub fn word_bounds_in_row(row: &Row, col: u16) -> (u16, u16) {
    let cells = row.cells.as_slice();
    let len: u16 = cells.len().try_into().unwrap_or(u16::MAX);
    if col >= len {
        return (col, col);
    }
    let at = cells.get(col as usize).map(|c| c.ch).unwrap_or(' ');
    if !is_word_char(at) {
        return (col, col);
    }
    let mut lo = col;
    while lo > 0
        && cells
            .get(lo as usize - 1)
            .is_some_and(|c| is_word_char(c.ch))
    {
        lo -= 1;
    }
    let mut hi = col;
    while hi + 1 < len
        && cells
            .get(hi as usize + 1)
            .is_some_and(|c| is_word_char(c.ch))
    {
        hi += 1;
    }
    (lo, hi)
}

/// True when the protocol cares about this event at all. Cheap pre-filter
/// to avoid building an event object when we're going to drop it anyway.
#[must_use]
pub fn protocol_wants_event(mode: MouseMode, kind: &MouseEventKind) -> bool {
    if !mode.is_on() {
        return false;
    }
    match kind {
        MouseEventKind::Press(_) | MouseEventKind::Release(_) | MouseEventKind::Scroll { .. } => {
            true
        }
        MouseEventKind::Motion { held } => match mode.protocol {
            MouseProtocol::Off | MouseProtocol::X10 => false,
            MouseProtocol::ButtonMotion => held.is_some(),
            MouseProtocol::AnyMotion => true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode_on(protocol: MouseProtocol) -> MouseMode {
        MouseMode {
            protocol,
            sgr_encoding: true,
        }
    }

    #[test]
    fn pixel_to_cell_basic() {
        // 16x32 cells, pixel (40, 70) -> col 3 (1+floor(40/16)), row 3
        // (1+floor(70/32)).
        assert_eq!(pixel_to_cell((40.0, 70.0), (16.0, 32.0), (24, 80)), (3, 3));
    }

    #[test]
    fn pixel_to_cell_clamps_to_grid() {
        assert_eq!(
            pixel_to_cell((9999.0, 9999.0), (16.0, 32.0), (10, 20)),
            (20, 10)
        );
    }

    #[test]
    fn pixel_to_cell_origin_is_one_one() {
        assert_eq!(pixel_to_cell((0.0, 0.0), (10.0, 20.0), (24, 80)), (1, 1));
    }

    #[test]
    fn pixel_to_cell_zero_cell_size_returns_one_one() {
        assert_eq!(pixel_to_cell((40.0, 70.0), (0.0, 0.0), (24, 80)), (1, 1));
    }

    #[test]
    fn off_mode_emits_nothing() {
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (5, 5),
            Modifiers::empty(),
            MouseMode::default(),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn press_left_x10() {
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (10, 20),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<0;10;20M".as_ref()));
    }

    #[test]
    fn release_left() {
        let got = encode_mouse(
            MouseEventKind::Release(MouseButton::Left),
            (10, 20),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<0;10;20m".as_ref()));
    }

    #[test]
    fn middle_right_buttons() {
        let m = encode_mouse(
            MouseEventKind::Press(MouseButton::Middle),
            (1, 1),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(m.as_deref(), Some(b"\x1b[<1;1;1M".as_ref()));
        let r = encode_mouse(
            MouseEventKind::Press(MouseButton::Right),
            (2, 3),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(r.as_deref(), Some(b"\x1b[<2;2;3M".as_ref()));
    }

    #[test]
    fn shift_alt_ctrl_modifiers_add_correctly() {
        let m = mode_on(MouseProtocol::X10);
        // Shift (+4)
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (1, 1),
            Modifiers::SHIFT,
            m,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<4;1;1M".as_ref()));
        // Alt (+8)
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (1, 1),
            Modifiers::ALT,
            m,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<8;1;1M".as_ref()));
        // Ctrl (+16)
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (1, 1),
            Modifiers::CONTROL,
            m,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<16;1;1M".as_ref()));
        // All three together (+28)
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (1, 1),
            Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL,
            m,
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<28;1;1M".as_ref()));
    }

    #[test]
    fn scroll_up_and_down() {
        let m = mode_on(MouseProtocol::X10);
        let up = encode_mouse(
            MouseEventKind::Scroll { dx: 0.0, dy: -1.0 },
            (5, 5),
            Modifiers::empty(),
            m,
        );
        assert_eq!(up.as_deref(), Some(b"\x1b[<64;5;5M".as_ref()));
        let dn = encode_mouse(
            MouseEventKind::Scroll { dx: 0.0, dy: 1.0 },
            (5, 5),
            Modifiers::empty(),
            m,
        );
        assert_eq!(dn.as_deref(), Some(b"\x1b[<65;5;5M".as_ref()));
    }

    #[test]
    fn scroll_zero_emits_nothing() {
        let m = mode_on(MouseProtocol::X10);
        let none = encode_mouse(
            MouseEventKind::Scroll { dx: 0.0, dy: 0.0 },
            (5, 5),
            Modifiers::empty(),
            m,
        );
        assert_eq!(none, None);
    }

    #[test]
    fn motion_without_button_in_x10_dropped() {
        let got = encode_mouse(
            MouseEventKind::Motion { held: None },
            (1, 1),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn motion_with_button_in_button_motion_mode_emits() {
        // +32 for motion, button code 0 for Left.
        let got = encode_mouse(
            MouseEventKind::Motion {
                held: Some(MouseButton::Left),
            },
            (3, 4),
            Modifiers::empty(),
            mode_on(MouseProtocol::ButtonMotion),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<32;3;4M".as_ref()));
    }

    #[test]
    fn motion_without_button_in_any_motion_mode_emits_with_no_button() {
        // base=3 (no button) + 32 (motion) = 35.
        let got = encode_mouse(
            MouseEventKind::Motion { held: None },
            (1, 1),
            Modifiers::empty(),
            mode_on(MouseProtocol::AnyMotion),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<35;1;1M".as_ref()));
    }

    #[test]
    fn unsupported_button_press_dropped() {
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Back),
            (1, 1),
            Modifiers::empty(),
            mode_on(MouseProtocol::X10),
        );
        assert_eq!(got, None);
    }

    #[test]
    fn classify_button_event_round_trip() {
        assert_eq!(
            classify_button_event(MouseButton::Left, KeyState::Pressed),
            MouseEventKind::Press(MouseButton::Left)
        );
        assert_eq!(
            classify_button_event(MouseButton::Right, KeyState::Released),
            MouseEventKind::Release(MouseButton::Right)
        );
    }

    #[test]
    fn protocol_wants_event_x10() {
        let m = mode_on(MouseProtocol::X10);
        assert!(protocol_wants_event(
            m,
            &MouseEventKind::Press(MouseButton::Left)
        ));
        assert!(!protocol_wants_event(m, &MouseEventKind::Motion {
            held: Some(MouseButton::Left)
        }));
    }

    #[test]
    fn protocol_wants_event_off() {
        assert!(!protocol_wants_event(
            MouseMode::default(),
            &MouseEventKind::Press(MouseButton::Left)
        ));
    }

    #[test]
    fn protocol_wants_event_button_motion_needs_held() {
        let m = mode_on(MouseProtocol::ButtonMotion);
        assert!(protocol_wants_event(m, &MouseEventKind::Motion {
            held: Some(MouseButton::Left)
        }));
        assert!(!protocol_wants_event(m, &MouseEventKind::Motion { held: None }));
    }

    #[test]
    fn protocol_wants_event_any_motion() {
        let m = mode_on(MouseProtocol::AnyMotion);
        assert!(protocol_wants_event(m, &MouseEventKind::Motion { held: None }));
    }

    // ---- word_bounds_in_row ----

    fn row_from_str(s: &str) -> Row {
        let mut row = Row::blank(s.chars().count() as u16);
        for (i, ch) in s.chars().enumerate() {
            row.cells[i].ch = ch;
        }
        row
    }

    #[test]
    fn word_bounds_picks_full_word() {
        let row = row_from_str("hello world");
        assert_eq!(word_bounds_in_row(&row, 0), (0, 4));
        assert_eq!(word_bounds_in_row(&row, 4), (0, 4));
        assert_eq!(word_bounds_in_row(&row, 6), (6, 10));
    }

    #[test]
    fn word_bounds_on_separator_returns_self() {
        let row = row_from_str("a b");
        assert_eq!(word_bounds_in_row(&row, 1), (1, 1));
    }

    #[test]
    fn word_bounds_handles_underscore() {
        let row = row_from_str("foo_bar baz");
        assert_eq!(word_bounds_in_row(&row, 3), (0, 6));
    }

    #[test]
    fn word_bounds_clamps_out_of_range() {
        let row = row_from_str("abc");
        assert_eq!(word_bounds_in_row(&row, 99), (99, 99));
    }

    #[test]
    fn horizontal_scroll_emits_66_67() {
        let m = mode_on(MouseProtocol::X10);
        let l = encode_mouse(
            MouseEventKind::Scroll { dx: -1.0, dy: 0.0 },
            (1, 1),
            Modifiers::empty(),
            m,
        );
        assert_eq!(l.as_deref(), Some(b"\x1b[<66;1;1M".as_ref()));
        let r = encode_mouse(
            MouseEventKind::Scroll { dx: 1.0, dy: 0.0 },
            (1, 1),
            Modifiers::empty(),
            m,
        );
        assert_eq!(r.as_deref(), Some(b"\x1b[<67;1;1M".as_ref()));
    }
}
