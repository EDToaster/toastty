//! SGR text renditions on the render side: decoration geometry
//! (strikethrough, overline, double underline), faint colour math, and
//! glyph visibility for blink / conceal.
//!
//! Pure CPU, no GPU types, so it sits under the render crate's
//! coverage gate. [`crate::text::instance`] calls into it from both the
//! full and the partial-redraw instance builders, which keeps the two
//! paths in lock-step.

use toastty_term::{StyleFlags, UnderlineStyle};

/// Font line metrics in pixels at the configured size, in swash's
/// convention: offsets are measured from the baseline, positive = up,
/// and give the **top** edge of the stroke.
///
/// Comes from the primary face's `post` / `OS/2` tables via
/// `swash::Metrics::scale`; see
/// [`crate::text::glyph_rasterizer::GlyphRasterizer::decoration_metrics`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct FontLineMetrics {
    /// `OS/2.yStrikeoutPosition`, scaled.
    pub strikeout_offset: f32,
    /// `post.underlineThickness` (swash `stroke_size`), scaled.
    pub stroke_size: f32,
}

/// Pixel geometry of the per-cell decoration strips, relative to the
/// cell's top edge. All values are whole pixels so the strips land on
/// the pixel grid (the atlas glyphs are snapped the same way).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DecorationMetrics {
    /// Thickness of the strikethrough, overline and double-underline
    /// strokes. At least 1 px.
    pub thickness: f32,
    /// Top edge of the strikethrough stroke.
    pub strike_y: f32,
    /// Top edge of the overline stroke (the cell's top edge).
    pub overline_y: f32,
    /// Top edges of the two double-underline strokes (`[upper, lower]`).
    /// The lower stroke sits flush with the cell bottom, like the single
    /// underline; the gap between them equals `thickness`.
    pub double_underline_y: [f32; 2],
}

impl DecorationMetrics {
    /// Derive geometry for a `cell_h`-tall cell whose baseline sits
    /// `baseline_y` px below the cell top.
    ///
    /// `font` is the primary face's metrics when available. A missing
    /// or implausible strikeout position (zero, negative, or outside the
    /// cell) falls back to xterm's `baseline - 3/8 * ascent` (util.c
    /// `drawUnderline`), using `baseline_y` as the ascent.
    #[must_use]
    pub fn new(cell_h: f32, baseline_y: f32, font: Option<FontLineMetrics>) -> Self {
        let cell_h = cell_h.max(1.0);
        let baseline_y = baseline_y.clamp(0.0, cell_h);
        // Stroke thickness: the font's underline thickness, snapped to
        // whole px, never thinner than 1 px or thicker than a quarter
        // cell (keeps a double underline inside short cells).
        let max_t = (cell_h / 4.0).floor().max(1.0);
        let thickness = font
            .map(|f| f.stroke_size)
            .filter(|t| t.is_finite() && *t > 0.0)
            .map_or(1.0, f32::round)
            .clamp(1.0, max_t);

        let fallback_strike = baseline_y - 0.375 * baseline_y;
        let strike_top = font
            .map(|f| baseline_y - f.strikeout_offset)
            .filter(|y| {
                font.is_some_and(|f| f.strikeout_offset > 0.0) && *y >= 0.0 && *y < baseline_y
            })
            .unwrap_or(fallback_strike);
        let strike_y = strike_top.round().clamp(0.0, cell_h - thickness);

        let lower = cell_h - thickness;
        let upper = (lower - 2.0 * thickness).max(0.0);
        Self {
            thickness,
            strike_y,
            overline_y: 0.0,
            double_underline_y: [upper, lower],
        }
    }

    /// Geometry for a cell with no font information: baseline at 80%
    /// of the cell height (typical for monospace faces at 1.2 line
    /// height). Used by the GPU-less test paths.
    #[must_use]
    pub fn fallback(cell_h: f32) -> Self {
        Self::new(cell_h, (cell_h * 0.8).round(), None)
    }
}

/// One decoration strip to draw: `(y offset from cell top, height)`.
/// The width is the cell (or wide-cluster) width.
pub type Strip = (f32, f32);

/// Which decoration strips a cell needs, as `(kind, strip)` pairs in
/// draw order. `underline_strip` is the classic single-underline strip
/// (2 px, flush with the cell bottom) shared with the IME preedit.
///
/// `hyperlinked` forces at least a single underline (OSC 8 links are
/// drawn underlined).
pub fn decoration_strips(
    flags: StyleFlags,
    hyperlinked: bool,
    cell_h: f32,
    m: &DecorationMetrics,
    mut push: impl FnMut(DecorationKind, Strip),
) {
    match flags.underline {
        UnderlineStyle::Double => {
            for y in m.double_underline_y {
                push(DecorationKind::Underline, (y, m.thickness));
            }
        }
        UnderlineStyle::None if !hyperlinked => {}
        // Single, plus curly / dotted / dashed until they get their own
        // geometry, plus the OSC 8 hyperlink underline.
        _ => {
            let t = 2.0_f32.min(cell_h);
            push(DecorationKind::Underline, (cell_h - t, t));
        }
    }
    if flags.strikethrough() {
        push(DecorationKind::Strikethrough, (m.strike_y, m.thickness));
    }
    if flags.overline() {
        push(DecorationKind::Overline, (m.overline_y, m.thickness));
    }
}

/// Decoration strip kind; maps to an instance flag bit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecorationKind {
    Underline,
    Strikethrough,
    Overline,
}

/// Faint (SGR 2) blend factor: the glyph colour is moved this far from
/// the foreground toward the background. 0.5 matches xterm's
/// `faintIsRelative` dimming and Ghostty's default; kitty's
/// `dim_opacity` defaults to a similar 0.4–0.5 range.
pub const DIM_FACTOR: f32 = 0.5;

/// Faint foreground: blend `fg` toward `bg` by [`DIM_FACTOR`].
///
/// Blended in gamma-encoded sRGB rather than linear light so "50%"
/// looks like halfway to the eye (a linear-light mix of white on black
/// would read as ~74% grey). `fg`'s alpha is kept; `bg`'s alpha is
/// ignored (a translucent window must not make the text translucent).
#[must_use]
pub fn dim_color(fg: [f32; 4], bg: [f32; 4]) -> [f32; 4] {
    let mix = |f: f32, b: f32| {
        let f = linear_to_srgb(f);
        let b = linear_to_srgb(b);
        srgb_to_linear(f + (b - f) * DIM_FACTOR)
    };
    [
        mix(fg[0], bg[0]),
        mix(fg[1], bg[1]),
        mix(fg[2], bg[2]),
        fg[3],
    ]
}

/// True when a cell's glyph and decorations should be drawn this frame.
///
/// Concealed (SGR 8) cells never draw them; blinking (SGR 5/6) cells
/// draw them only in the on-phase. Mirrors xterm, which paints both by
/// setting the foreground pixel to the background (util.c
/// `updatedXtermGC`), so underline / strike vanish with the text while
/// the cell background and selection highlight stay.
#[inline]
#[must_use]
pub fn foreground_visible(flags: StyleFlags, blink_on: bool) -> bool {
    !flags.hidden() && (blink_on || !flags.blink())
}

fn linear_to_srgb(l: f32) -> f32 {
    let l = l.clamp(0.0, 1.0);
    if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    }
}

fn srgb_to_linear(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.040_45 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strips(
        flags: StyleFlags,
        link: bool,
        m: &DecorationMetrics,
    ) -> Vec<(DecorationKind, Strip)> {
        let mut v = Vec::new();
        decoration_strips(flags, link, 19.0, m, |k, s| v.push((k, s)));
        v
    }

    #[test]
    fn metrics_from_font_use_strikeout_and_stroke() {
        // DejaVu Sans Mono-ish at 16 px: strikeout ~4.7 px above the
        // baseline, stroke ~0.7 px.
        let m = DecorationMetrics::new(
            19.0,
            15.0,
            Some(FontLineMetrics {
                strikeout_offset: 4.7,
                stroke_size: 0.7,
            }),
        );
        assert_eq!(m.thickness, 1.0);
        assert_eq!(m.strike_y, 10.0);
        assert_eq!(m.overline_y, 0.0);
        assert_eq!(m.double_underline_y, [16.0, 18.0]);
    }

    #[test]
    fn thick_stroke_rounds_and_is_capped() {
        let f = |stroke| {
            DecorationMetrics::new(
                20.0,
                16.0,
                Some(FontLineMetrics {
                    strikeout_offset: 5.0,
                    stroke_size: stroke,
                }),
            )
        };
        assert_eq!(f(1.6).thickness, 2.0);
        assert_eq!(f(2.0).double_underline_y, [14.0, 18.0]);
        assert_eq!(f(30.0).thickness, 5.0, "capped at cell_h / 4");
        assert_eq!(f(f32::NAN).thickness, 1.0);
        assert_eq!(f(0.0).thickness, 1.0);
        assert_eq!(f(-1.0).thickness, 1.0);
    }

    #[test]
    fn implausible_strikeout_falls_back_to_xterm_three_eighths() {
        let want = (16.0_f32 - 0.375 * 16.0).round();
        for off in [0.0, -3.0, 40.0] {
            let m = DecorationMetrics::new(
                20.0,
                16.0,
                Some(FontLineMetrics {
                    strikeout_offset: off,
                    stroke_size: 1.0,
                }),
            );
            assert_eq!(m.strike_y, want, "offset {off}");
        }
        assert_eq!(DecorationMetrics::new(20.0, 16.0, None).strike_y, want);
    }

    #[test]
    fn degenerate_cells_stay_in_bounds() {
        let m = DecorationMetrics::new(0.0, 5.0, None);
        assert_eq!(m.thickness, 1.0);
        assert!(m.strike_y >= 0.0 && m.strike_y + m.thickness <= 1.0);
        assert_eq!(m.double_underline_y, [0.0, 0.0]);
        let m = DecorationMetrics::new(3.0, 2.0, None);
        assert_eq!(m.double_underline_y, [0.0, 2.0]);
    }

    #[test]
    fn fallback_uses_eighty_percent_baseline() {
        assert_eq!(
            DecorationMetrics::fallback(20.0),
            DecorationMetrics::new(20.0, 16.0, None)
        );
    }

    #[test]
    fn strips_for_each_rendition() {
        let m = DecorationMetrics::new(19.0, 15.0, None);
        let e = StyleFlags::EMPTY;
        assert!(strips(e, false, &m).is_empty());
        assert_eq!(
            strips(e.with_underline(UnderlineStyle::Single), false, &m),
            vec![(DecorationKind::Underline, (17.0, 2.0))]
        );
        // Curly / dotted / dashed still draw as a single underline.
        for u in [
            UnderlineStyle::Curly,
            UnderlineStyle::Dotted,
            UnderlineStyle::Dashed,
        ] {
            assert_eq!(strips(e.with_underline(u), false, &m).len(), 1);
        }
        // Hyperlink forces the single underline.
        assert_eq!(
            strips(e, true, &m),
            vec![(DecorationKind::Underline, (17.0, 2.0))]
        );
        let d = strips(e.with_underline(UnderlineStyle::Double), true, &m);
        assert_eq!(
            d,
            vec![
                (DecorationKind::Underline, (16.0, 1.0)),
                (DecorationKind::Underline, (18.0, 1.0)),
            ]
        );
        assert_eq!(
            strips(e.with(StyleFlags::STRIKETHROUGH), false, &m),
            vec![(DecorationKind::Strikethrough, (m.strike_y, 1.0))]
        );
        assert_eq!(
            strips(e.with(StyleFlags::OVERLINE), false, &m),
            vec![(DecorationKind::Overline, (0.0, 1.0))]
        );
        let all = e
            .with(StyleFlags::STRIKETHROUGH | StyleFlags::OVERLINE)
            .with_underline(UnderlineStyle::Single);
        let kinds: Vec<_> = strips(all, false, &m).into_iter().map(|(k, _)| k).collect();
        assert_eq!(
            kinds,
            [
                DecorationKind::Underline,
                DecorationKind::Strikethrough,
                DecorationKind::Overline
            ]
        );
    }

    #[test]
    fn dim_is_perceptual_half_way() {
        // White on black → sRGB 0.5 grey (linear ~0.214).
        let d = dim_color([1.0, 1.0, 1.0, 1.0], [0.0, 0.0, 0.0, 1.0]);
        for c in &d[..3] {
            assert!((c - 0.214).abs() < 0.002, "{c}");
        }
        assert_eq!(d[3], 1.0);
        // Identical fg/bg stays put; alpha comes from fg only.
        let same = dim_color([0.3, 0.3, 0.3, 0.8], [0.3, 0.3, 0.3, 0.0]);
        for c in &same[..3] {
            assert!((c - 0.3).abs() < 1e-4);
        }
        assert_eq!(same[3], 0.8);
        // Dark-on-light moves up toward the light bg.
        let up = dim_color([0.0, 0.0, 0.0, 1.0], [1.0, 1.0, 1.0, 1.0]);
        assert!((up[0] - 0.214).abs() < 0.002);
        // Out-of-range inputs are clamped, not propagated.
        let c = dim_color([2.0, -1.0, 0.0, 1.0], [0.0, 0.0, 0.0, 1.0]);
        assert!(c[0] <= 0.22 && c[1] == 0.0);
    }

    #[test]
    fn srgb_round_trip_low_and_high() {
        for v in [0.0, 0.001, 0.002, 0.2, 0.5, 1.0] {
            assert!((srgb_to_linear(linear_to_srgb(v)) - v).abs() < 1e-4, "{v}");
        }
    }

    #[test]
    fn foreground_visibility_rules() {
        let e = StyleFlags::EMPTY;
        assert!(foreground_visible(e, true));
        assert!(foreground_visible(e, false));
        let blink = e.with(StyleFlags::BLINK);
        assert!(foreground_visible(blink, true));
        assert!(!foreground_visible(blink, false));
        let hidden = e.with(StyleFlags::HIDDEN);
        assert!(!foreground_visible(hidden, true));
        assert!(!foreground_visible(
            e.with(StyleFlags::HIDDEN | StyleFlags::BLINK),
            true
        ));
    }
}
