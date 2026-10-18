//! Mouse-event encoding (DECSET 9 / 1000 / 1002 / 1003 tracking, with
//! the legacy X11, UTF-8 1005, SGR 1006 and urxvt 1015 encodings).
//!
//! Which events fire is decided by [`MouseProtocol`]; how they are
//! serialised by [`MouseEncoding`]. Both follow xterm's `button.c`
//! (`EditorButton` / `EmitButtonCode` / `EmitMousePosition`) and the
//! "Mouse Tracking" section of xterm ctlseqs.
//!
//! ## Button code (`Cb`)
//!
//! - Left=0, Middle=1, Right=2, "release / no button"=3
//! - Scroll-up=64, Scroll-down=65, Scroll-left=66, Scroll-right=67
//! - +4 if Shift, +8 if Alt (Meta), +16 if Ctrl
//! - +32 for motion reports (DECSET 1002/1003)
//!
//! DECSET 9 (X10 compatibility) reports presses only, with no modifier
//! bits and no wheel / release / motion events.
//!
//! ## Encodings
//!
//! ```text
//! Default (X11):  CSI M Cb+32 Cx+32 Cy+32        one byte each
//! UTF-8 (1005):   CSI M Cb+32 Cx+32 Cy+32        values >= 128 as 2-byte UTF-8
//! SGR (1006):     CSI < Cb ; Cx ; Cy M|m         decimal, m = release
//! urxvt (1015):   CSI Cb+32 ; Cx ; Cy M          decimal
//! ```
//!
//! In every encoding but SGR a release can't say *which* button went up,
//! so it is reported as button 3. `Cx` / `Cy` are 1-based.
//!
//! ## Coordinate limits
//!
//! The byte encodings can only carry so much: legacy X11 tops out at
//! column/row 223 (`255 - 32`), UTF-8 at 2015 (`2047 - 32`). Like xterm,
//! any position beyond the limit is sent as a `0x00` byte — xterm clamps
//! to one past the limit and emits that as zero, its documented
//! "past-end marker" (what 256 truncated to 8 bits used to produce). The
//! event itself is still reported so press/release stay paired. SGR and
//! urxvt are decimal and unlimited.

use toastty_term::{MouseEncoding, MouseMode, MouseProtocol};
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
///
/// `origin` is the content rect's top-left in physical px (`(pad_left,
/// pad_top)` — the window-padding inset). It is subtracted before dividing
/// by the cell size, so a click anywhere in the left/top padding gutter
/// produces a negative local coordinate that the `.max(0.0)` floor collapses
/// to cell 1, and a click in the right/bottom gutter produces a large
/// quotient that the existing `.min(cols/rows)` clamp folds onto the last
/// cell. No extra branches are needed for the padding clamp.
#[must_use]
pub fn pixel_to_cell(
    pixel: (f64, f64),
    origin: (f32, f32),
    cell_size: (f32, f32),
    grid: (u16, u16),
) -> (u16, u16) {
    let (cw, ch) = cell_size;
    if cw <= 0.0 || ch <= 0.0 {
        return (1, 1);
    }
    let local_x = pixel.0 - f64::from(origin.0);
    let local_y = pixel.1 - f64::from(origin.1);
    let col = ((local_x / f64::from(cw)).floor().max(0.0) as u32) + 1;
    let row = ((local_y / f64::from(ch)).floor().max(0.0) as u32) + 1;
    let (rows, cols) = grid;
    let col = col.min(u32::from(cols.max(1))) as u16;
    let row = row.min(u32::from(rows.max(1))) as u16;
    (col, row)
}

/// Encode a mouse event into bytes suitable for the PTY, or `None` if the
/// event should not be reported under the current mode.
///
/// `mode.protocol` controls whether events are emitted:
/// - `Off` — never emit.
/// - `X10Compat` (DECSET 9) — presses of buttons 1–3 only, no modifiers.
/// - `Normal` (DECSET 1000) — press/release + wheel.
/// - `ButtonMotion` (1002) — plus motion while a button is held.
/// - `AnyMotion` (1003) — plus any motion.
///
/// `mode.encoding` controls the wire format (see the module docs).
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
    let x10 = mode.protocol == MouseProtocol::X10Compat;
    // X10 compatibility mode never carries modifier bits (xterm emits
    // `' ' + button` verbatim in that mode).
    let mod_code = if x10 { 0 } else { mod_bits(modifiers) };
    let enc = mode.encoding;
    match kind {
        MouseEventKind::Press(b) => {
            let code = button_code(b)? + mod_code;
            Some(encode_report(enc, code, cell, true))
        }
        MouseEventKind::Release(b) => {
            if x10 {
                return None;
            }
            let base = button_code(b)?;
            // Only SGR can name the released button; the others send the
            // generic "release" code 3 (modifier bits still apply).
            let code = if enc == MouseEncoding::Sgr { base } else { 3 } + mod_code;
            Some(encode_report(enc, code, cell, false))
        }
        MouseEventKind::Motion { held } => {
            if !mode.report_drag() {
                return None;
            }
            if held.is_none() && !mode.report_any_motion() {
                return None;
            }
            let base = held.and_then(button_code).unwrap_or(3); // "no button" sentinel
            let code = base + mod_code + 32;
            Some(encode_report(enc, code, cell, true))
        }
        MouseEventKind::Scroll { dx, dy } => {
            // xterm drops wheel buttons (4+) in X10 compatibility mode.
            if x10 {
                return None;
            }
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
                let code = if dy > 0.0 { 65 } else { 64 } + mod_code;
                out.extend_from_slice(&encode_report(enc, code, cell, true));
            }
            if dx.abs() > f64::EPSILON {
                let code = if dx > 0.0 { 67 } else { 66 } + mod_code;
                out.extend_from_slice(&encode_report(enc, code, cell, true));
            }
            Some(out)
        }
    }
}

/// Largest 1-based coordinate the legacy X11 encoding can carry
/// (`255 - 32`). xterm `button.c` `MOUSE_LIMIT`.
const LEGACY_COORD_LIMIT: u16 = 255 - 32;

/// Largest 1-based coordinate the UTF-8 (1005) encoding can carry
/// (`2047 - 32`, the largest two-byte UTF-8 value minus the offset).
/// xterm `button.c` `EXT_MOUSE_LIMIT`.
const UTF8_COORD_LIMIT: u16 = 2047 - 32;

/// Serialise one report. `code` is the bare button code (no +32 offset);
/// `press` selects SGR's `M` vs `m` final and is ignored elsewhere.
fn encode_report(enc: MouseEncoding, code: u32, cell: (u16, u16), press: bool) -> Vec<u8> {
    let (col, row) = cell;
    match enc {
        MouseEncoding::Sgr => {
            let suffix = if press { 'M' } else { 'm' };
            format!("\x1b[<{code};{col};{row}{suffix}").into_bytes()
        }
        MouseEncoding::Urxvt => format!("\x1b[{};{col};{row}M", code + 32).into_bytes(),
        MouseEncoding::Default | MouseEncoding::Utf8 => {
            let utf8 = enc == MouseEncoding::Utf8;
            let limit = if utf8 {
                UTF8_COORD_LIMIT
            } else {
                LEGACY_COORD_LIMIT
            };
            let mut out = b"\x1b[M".to_vec();
            push_legacy_value(&mut out, code + 32, utf8);
            for v in [col, row] {
                if v > limit {
                    // Clamp, then emit xterm's past-end marker.
                    out.push(0);
                } else {
                    push_legacy_value(&mut out, u32::from(v) + 32, utf8);
                }
            }
            out
        }
    }
}

/// Append one already-offset value: a raw byte, or (under 1005) a
/// two-byte UTF-8 sequence for values ≥ 128 — xterm builds the bytes
/// by hand as `0xC0 + (v >> 6), 0x80 + (v & 0x3F)`.
#[allow(clippy::cast_possible_truncation)]
fn push_legacy_value(out: &mut Vec<u8>, v: u32, utf8: bool) {
    if utf8 && v >= 0x80 {
        out.push(0xC0 + (v >> 6) as u8);
        out.push(0x80 + (v & 0x3F) as u8);
    } else {
        out.push(v as u8);
    }
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

/// Auto-scroll direction for a selection drag whose pointer is at
/// window-relative y `pointer_y`, given the grid's content area
/// `[content_top, content_top + content_h)`. Returns `1` (scroll up
/// into history) when the pointer is above the grid, `-1` (scroll
/// toward the live bottom) when it's below, `0` inside. xterm, kitty
/// and Alacritty all keep scrolling while a drag is held past an edge
/// so a selection can grow beyond one screenful.
#[must_use]
pub fn drag_autoscroll_dir(pointer_y: f64, content_top: f64, content_h: f64) -> i32 {
    if pointer_y < content_top {
        1
    } else if pointer_y >= content_top + content_h {
        -1
    } else {
        0
    }
}

/// True when the protocol cares about this event at all. Cheap pre-filter
/// to avoid building an event object when we're going to drop it anyway.
#[must_use]
pub fn protocol_wants_event(mode: MouseMode, kind: &MouseEventKind) -> bool {
    if !mode.is_on() {
        return false;
    }
    match kind {
        // X10 compatibility (DECSET 9) reports presses only.
        MouseEventKind::Press(_) => true,
        MouseEventKind::Release(_) | MouseEventKind::Scroll { .. } => {
            mode.protocol != MouseProtocol::X10Compat
        }
        MouseEventKind::Motion { held } => match mode.protocol {
            MouseProtocol::Off | MouseProtocol::X10Compat | MouseProtocol::Normal => false,
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
            encoding: MouseEncoding::Sgr,
        }
    }

    #[test]
    fn pixel_to_cell_basic() {
        // 16x32 cells, pixel (40, 70) -> col 3 (1+floor(40/16)), row 3
        // (1+floor(70/32)). Zero origin.
        assert_eq!(
            pixel_to_cell((40.0, 70.0), (0.0, 0.0), (16.0, 32.0), (24, 80)),
            (3, 3)
        );
    }

    #[test]
    fn pixel_to_cell_clamps_to_grid() {
        assert_eq!(
            pixel_to_cell((9999.0, 9999.0), (0.0, 0.0), (16.0, 32.0), (10, 20)),
            (20, 10)
        );
    }

    #[test]
    fn pixel_to_cell_origin_is_one_one() {
        assert_eq!(
            pixel_to_cell((0.0, 0.0), (0.0, 0.0), (10.0, 20.0), (24, 80)),
            (1, 1)
        );
    }

    #[test]
    fn pixel_to_cell_zero_cell_size_returns_one_one() {
        assert_eq!(
            pixel_to_cell((40.0, 70.0), (0.0, 0.0), (0.0, 0.0), (24, 80)),
            (1, 1)
        );
    }

    #[test]
    fn pixel_to_cell_accounts_for_origin() {
        // origin (20, 10): a click at (60, 90) is local (40, 80) → col 3,
        // row 6 (1+floor(40/16), 1+floor(80/16)).
        assert_eq!(
            pixel_to_cell((60.0, 90.0), (20.0, 10.0), (16.0, 16.0), (24, 80)),
            (3, 6)
        );
    }

    #[test]
    fn click_in_left_top_padding_clamps_to_first_cell() {
        // Anywhere inside the left/top gutter (x < origin.x, y < origin.y) →
        // negative local → collapses to (1, 1).
        let origin = (20.0, 10.0);
        assert_eq!(
            pixel_to_cell((5.0, 3.0), origin, (16.0, 16.0), (24, 80)),
            (1, 1)
        );
        // Exactly on the origin is the first cell's top-left corner → (1, 1).
        assert_eq!(
            pixel_to_cell((20.0, 10.0), origin, (16.0, 16.0), (24, 80)),
            (1, 1)
        );
    }

    #[test]
    fn click_in_right_bottom_padding_clamps_to_last_cell() {
        // A click far past the content into the right/bottom gutter clamps
        // to the last cell (cols=24, rows=10).
        assert_eq!(
            pixel_to_cell((100_000.0, 100_000.0), (20.0, 10.0), (16.0, 16.0), (10, 24)),
            (24, 10)
        );
    }

    #[test]
    fn zero_origin_matches_legacy_behavior() {
        // With origin (0,0) the result is identical to the pre-padding
        // contract: pixel (40,70), 16x32 cells → (3, 3).
        assert_eq!(
            pixel_to_cell((40.0, 70.0), (0.0, 0.0), (16.0, 32.0), (24, 80)),
            (3, 3)
        );
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
    fn press_left_normal() {
        let got = encode_mouse(
            MouseEventKind::Press(MouseButton::Left),
            (10, 20),
            Modifiers::empty(),
            mode_on(MouseProtocol::Normal),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<0;10;20M".as_ref()));
    }

    #[test]
    fn release_left() {
        let got = encode_mouse(
            MouseEventKind::Release(MouseButton::Left),
            (10, 20),
            Modifiers::empty(),
            mode_on(MouseProtocol::Normal),
        );
        assert_eq!(got.as_deref(), Some(b"\x1b[<0;10;20m".as_ref()));
    }

    #[test]
    fn middle_right_buttons() {
        let m = encode_mouse(
            MouseEventKind::Press(MouseButton::Middle),
            (1, 1),
            Modifiers::empty(),
            mode_on(MouseProtocol::Normal),
        );
        assert_eq!(m.as_deref(), Some(b"\x1b[<1;1;1M".as_ref()));
        let r = encode_mouse(
            MouseEventKind::Press(MouseButton::Right),
            (2, 3),
            Modifiers::empty(),
            mode_on(MouseProtocol::Normal),
        );
        assert_eq!(r.as_deref(), Some(b"\x1b[<2;2;3M".as_ref()));
    }

    #[test]
    fn shift_alt_ctrl_modifiers_add_correctly() {
        let m = mode_on(MouseProtocol::Normal);
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
        let m = mode_on(MouseProtocol::Normal);
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
        let m = mode_on(MouseProtocol::Normal);
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
            mode_on(MouseProtocol::Normal),
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
            mode_on(MouseProtocol::Normal),
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
        let m = mode_on(MouseProtocol::Normal);
        assert!(protocol_wants_event(
            m,
            &MouseEventKind::Press(MouseButton::Left)
        ));
        assert!(!protocol_wants_event(
            m,
            &MouseEventKind::Motion {
                held: Some(MouseButton::Left)
            }
        ));
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
        assert!(protocol_wants_event(
            m,
            &MouseEventKind::Motion {
                held: Some(MouseButton::Left)
            }
        ));
        assert!(!protocol_wants_event(
            m,
            &MouseEventKind::Motion { held: None }
        ));
    }

    #[test]
    fn protocol_wants_event_any_motion() {
        let m = mode_on(MouseProtocol::AnyMotion);
        assert!(protocol_wants_event(
            m,
            &MouseEventKind::Motion { held: None }
        ));
    }

    // ---- drag_autoscroll_dir ----

    #[test]
    fn drag_autoscroll_dir_by_edge() {
        // Content area y in [10, 110).
        assert_eq!(drag_autoscroll_dir(9.9, 10.0, 100.0), 1);
        assert_eq!(drag_autoscroll_dir(-40.0, 10.0, 100.0), 1);
        assert_eq!(drag_autoscroll_dir(10.0, 10.0, 100.0), 0);
        assert_eq!(drag_autoscroll_dir(109.9, 10.0, 100.0), 0);
        assert_eq!(drag_autoscroll_dir(110.0, 10.0, 100.0), -1);
        assert_eq!(drag_autoscroll_dir(900.0, 10.0, 100.0), -1);
    }

    #[test]
    fn horizontal_scroll_emits_66_67() {
        let m = mode_on(MouseProtocol::Normal);
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

    // ---- legacy / UTF-8 / urxvt encodings and DECSET 9 ----

    fn mode(protocol: MouseProtocol, encoding: MouseEncoding) -> MouseMode {
        MouseMode { protocol, encoding }
    }

    fn enc(kind: MouseEventKind, cell: (u16, u16), mods: Modifiers, m: MouseMode) -> Vec<u8> {
        encode_mouse(kind, cell, mods, m).expect("event should be reported")
    }

    const PRESS_L: MouseEventKind = MouseEventKind::Press(MouseButton::Left);
    const RELEASE_L: MouseEventKind = MouseEventKind::Release(MouseButton::Left);

    #[test]
    fn legacy_press_release_and_drag() {
        let m = mode(MouseProtocol::ButtonMotion, MouseEncoding::Default);
        // Cb = 0+32 = ' ', Cx = 10+32 = '*', Cy = 20+32 = '4'.
        assert_eq!(enc(PRESS_L, (10, 20), Modifiers::empty(), m), b"\x1b[M *4");
        // Release is button 3 regardless of which button went up.
        assert_eq!(
            enc(RELEASE_L, (10, 20), Modifiers::empty(), m),
            b"\x1b[M#*4"
        );
        let rel_r = MouseEventKind::Release(MouseButton::Right);
        assert_eq!(enc(rel_r, (1, 1), Modifiers::empty(), m), b"\x1b[M#!!");
        // Drag: 0 + 32 (motion) + 32 = '@'.
        let drag = MouseEventKind::Motion {
            held: Some(MouseButton::Left),
        };
        assert_eq!(enc(drag, (2, 3), Modifiers::empty(), m), b"\x1b[M@\"#");
    }

    #[test]
    fn legacy_modifiers_apply_to_press_and_release() {
        let m = mode(MouseProtocol::Normal, MouseEncoding::Default);
        // Ctrl: 16 + 32 = '0'; release 3 + 16 + 32 = '3'.
        assert_eq!(enc(PRESS_L, (1, 1), Modifiers::CONTROL, m), b"\x1b[M0!!");
        assert_eq!(enc(RELEASE_L, (1, 1), Modifiers::CONTROL, m), b"\x1b[M3!!");
        // Shift+Alt+Ctrl on middle: 1 + 28 + 32 = '='.
        let mid = MouseEventKind::Press(MouseButton::Middle);
        let all = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL;
        assert_eq!(enc(mid, (1, 1), all, m), b"\x1b[M=!!");
    }

    #[test]
    fn legacy_wheel_and_any_motion() {
        let m = mode(MouseProtocol::AnyMotion, MouseEncoding::Default);
        let up = MouseEventKind::Scroll { dx: 0.0, dy: -1.0 };
        assert_eq!(enc(up, (5, 5), Modifiers::empty(), m), b"\x1b[M`%%");
        let dn = MouseEventKind::Scroll { dx: 0.0, dy: 1.0 };
        assert_eq!(enc(dn, (5, 5), Modifiers::empty(), m), b"\x1b[Ma%%");
        // Motion with no button: 3 + 32 + 32 = 'C'.
        let mv = MouseEventKind::Motion { held: None };
        assert_eq!(enc(mv, (1, 1), Modifiers::empty(), m), b"\x1b[MC!!");
    }

    #[test]
    fn legacy_coordinate_limit_uses_past_end_marker() {
        let m = mode(MouseProtocol::Normal, MouseEncoding::Default);
        // 223 is the last representable position (223 + 32 = 255).
        assert_eq!(
            enc(PRESS_L, (223, 223), Modifiers::empty(), m),
            b"\x1b[M \xff\xff"
        );
        // Beyond the limit xterm clamps and emits 0x00 for that value,
        // independently per axis; the event is still reported.
        assert_eq!(
            enc(PRESS_L, (224, 5), Modifiers::empty(), m),
            b"\x1b[M \x00%"
        );
        assert_eq!(
            enc(RELEASE_L, (5, 1000), Modifiers::empty(), m),
            b"\x1b[M#%\x00"
        );
    }

    #[test]
    fn utf8_encoding_small_values_match_legacy() {
        let m = mode(MouseProtocol::Normal, MouseEncoding::Utf8);
        assert_eq!(enc(PRESS_L, (10, 20), Modifiers::empty(), m), b"\x1b[M *4");
        // 95 + 32 = 127 still fits in one byte.
        assert_eq!(
            enc(PRESS_L, (95, 1), Modifiers::empty(), m),
            b"\x1b[M \x7f!"
        );
    }

    #[test]
    fn utf8_encoding_large_values_are_two_byte_utf8() {
        let m = mode(MouseProtocol::Normal, MouseEncoding::Utf8);
        // col 96 -> 128 -> U+0080 -> C2 80; row 300 -> 332 -> U+014C -> C5 8C.
        let got = enc(PRESS_L, (96, 300), Modifiers::empty(), m);
        assert_eq!(got, b"\x1b[M \xc2\x80\xc5\x8c");
        assert_eq!(std::str::from_utf8(&got[3..]).unwrap(), " \u{80}\u{14c}");
        // Upper limit: 2015 + 32 = 2047 = U+07FF -> DF BF. Past it: 0x00.
        assert_eq!(
            enc(PRESS_L, (2015, 2016), Modifiers::empty(), m),
            b"\x1b[M \xdf\xbf\x00"
        );
    }

    #[test]
    fn utf8_encoding_large_button_code_is_two_byte() {
        // Wheel-down + Ctrl + Alt: 65 + 24 + 32 = 121 (one byte);
        // any-motion with all modifiers: 3 + 28 + 32 + 32 = 95 (one byte).
        // Scroll-right + all modifiers: 67 + 28 + 32 = 127 (one byte).
        let m = mode(MouseProtocol::AnyMotion, MouseEncoding::Utf8);
        let all = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL;
        let right = MouseEventKind::Scroll { dx: 1.0, dy: 0.0 };
        assert_eq!(enc(right, (1, 1), all, m), b"\x1b[M\x7f!!");
        // The UTF-8 path for button codes is exercised directly.
        let mut out = Vec::new();
        push_legacy_value(&mut out, 128 + 32, true);
        assert_eq!(out, b"\xc2\xa0");
        out.clear();
        push_legacy_value(&mut out, 128 + 32, false);
        assert_eq!(out, b"\xa0");
    }

    #[test]
    fn urxvt_encoding() {
        let m = mode(MouseProtocol::ButtonMotion, MouseEncoding::Urxvt);
        // Cb keeps the +32 offset, decimal, always 'M'.
        assert_eq!(
            enc(PRESS_L, (10, 20), Modifiers::empty(), m),
            b"\x1b[32;10;20M"
        );
        assert_eq!(
            enc(RELEASE_L, (10, 20), Modifiers::empty(), m),
            b"\x1b[35;10;20M"
        );
        assert_eq!(
            enc(RELEASE_L, (10, 20), Modifiers::CONTROL, m),
            b"\x1b[51;10;20M"
        );
        let drag = MouseEventKind::Motion {
            held: Some(MouseButton::Left),
        };
        assert_eq!(
            enc(drag, (300, 400), Modifiers::empty(), m),
            b"\x1b[64;300;400M"
        );
        let up = MouseEventKind::Scroll { dx: 0.0, dy: -1.0 };
        assert_eq!(enc(up, (1, 1), Modifiers::empty(), m), b"\x1b[96;1;1M");
    }

    #[test]
    fn sgr_has_no_coordinate_limit_and_keeps_release_button() {
        let m = mode(MouseProtocol::Normal, MouseEncoding::Sgr);
        assert_eq!(
            enc(PRESS_L, (300, 500), Modifiers::empty(), m),
            b"\x1b[<0;300;500M"
        );
        let rel_r = MouseEventKind::Release(MouseButton::Right);
        assert_eq!(enc(rel_r, (1, 1), Modifiers::SHIFT, m), b"\x1b[<6;1;1m");
    }

    #[test]
    fn x10_compat_reports_presses_only_without_modifiers() {
        for encoding in [
            MouseEncoding::Default,
            MouseEncoding::Utf8,
            MouseEncoding::Sgr,
            MouseEncoding::Urxvt,
        ] {
            let m = mode(MouseProtocol::X10Compat, encoding);
            let all = Modifiers::SHIFT | Modifiers::ALT | Modifiers::CONTROL;
            let right = MouseEventKind::Press(MouseButton::Right);
            let got = enc(right, (3, 4), all, m);
            let want: &[u8] = match encoding {
                MouseEncoding::Default | MouseEncoding::Utf8 => b"\x1b[M\"#$",
                MouseEncoding::Sgr => b"\x1b[<2;3;4M",
                MouseEncoding::Urxvt => b"\x1b[34;3;4M",
            };
            assert_eq!(got, want, "{encoding:?}");
            assert_eq!(encode_mouse(RELEASE_L, (3, 4), all, m), None);
            let drag = MouseEventKind::Motion {
                held: Some(MouseButton::Left),
            };
            assert_eq!(encode_mouse(drag, (3, 4), all, m), None);
            let wheel = MouseEventKind::Scroll { dx: 0.0, dy: 1.0 };
            assert_eq!(encode_mouse(wheel, (3, 4), all, m), None);
        }
    }

    #[test]
    fn protocol_wants_event_x10_compat_press_only() {
        let m = mode(MouseProtocol::X10Compat, MouseEncoding::Default);
        assert!(protocol_wants_event(m, &PRESS_L));
        assert!(!protocol_wants_event(m, &RELEASE_L));
        assert!(!protocol_wants_event(
            m,
            &MouseEventKind::Scroll { dx: 0.0, dy: 1.0 }
        ));
        assert!(!protocol_wants_event(
            m,
            &MouseEventKind::Motion {
                held: Some(MouseButton::Left)
            }
        ));
    }
}
