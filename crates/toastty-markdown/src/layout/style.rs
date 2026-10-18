//! Colours derived from the terminal theme.

use crate::{Rgba, Theme};

/// Mix `a` and `b`: `t = 1` is all `a`, `t = 0` all `b`. Opaque result.
pub(crate) fn mix(a: Rgba, b: Rgba, t: f32) -> Rgba {
    let t = t.clamp(0.0, 1.0);
    let ch = |i: usize| (f32::from(a[i]) * t + f32::from(b[i]) * (1.0 - t)).round() as u8;
    [ch(0), ch(1), ch(2), 0xff]
}

fn luminance(c: Rgba) -> f32 {
    let lin = |v: u8| {
        let v = f32::from(v) / 255.0;
        if v <= 0.040_45 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio between two colours.
pub(crate) fn contrast(a: Rgba, b: Rgba) -> f32 {
    let (la, lb) = (luminance(a), luminance(b));
    let (hi, lo) = if la > lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

/// `c`, pulled towards `fg` until it reaches `min` contrast against
/// `bg` (or becomes `fg`).
pub(crate) fn ensure_contrast(c: Rgba, fg: Rgba, bg: Rgba, min: f32) -> Rgba {
    let mut out = [c[0], c[1], c[2], 0xff];
    let mut t = 0.0;
    while contrast(out, bg) < min && t < 1.0 {
        t += 0.1;
        out = mix(fg, c, t);
    }
    out
}

pub(crate) fn is_dark(bg: Rgba) -> bool {
    luminance(bg) < 0.25
}

/// Every colour the layout draws with.
#[derive(Debug, Clone)]
pub(crate) struct Palette {
    pub fg: Rgba,
    pub bg: Rgba,
    pub heading: Rgba,
    pub muted: Rgba,
    pub link: Rgba,
    pub border: Rgba,
    pub rule: Rgba,
    pub code_bg: Rgba,
    pub inline_code_bg: Rgba,
    pub table_head_bg: Rgba,
    pub table_stripe_bg: Rgba,
    pub quote_bar: Rgba,
    pub checkbox_on: Rgba,
    pub ansi: [Rgba; 16],
    /// GitHub alert accents: note, tip, important, warning, caution.
    pub alerts: [Rgba; 5],
}

impl Palette {
    pub(crate) fn new(theme: &Theme) -> Self {
        let fg = [theme.fg[0], theme.fg[1], theme.fg[2], 0xff];
        let bg = [theme.bg[0], theme.bg[1], theme.bg[2], 0xff];
        let dark = is_dark(bg);
        let a = &theme.ansi;
        // Bright variants read better on dark backgrounds, normal ones
        // on light backgrounds.
        let pick = |n: usize| if dark { a[n + 8] } else { a[n] };
        let accent = |c: Rgba| ensure_contrast(c, fg, bg, 3.0);
        let link = ensure_contrast(pick(4), fg, bg, 4.0);
        let heading = if dark {
            mix(fg, [0xff; 4], 0.85)
        } else {
            mix(fg, [0, 0, 0, 0xff], 0.85)
        };
        Self {
            fg,
            bg,
            heading,
            muted: ensure_contrast(mix(fg, bg, 0.62), fg, bg, 3.5),
            link,
            border: mix(fg, bg, if dark { 0.22 } else { 0.18 }),
            rule: mix(fg, bg, if dark { 0.2 } else { 0.15 }),
            code_bg: mix(fg, bg, if dark { 0.065 } else { 0.035 }),
            inline_code_bg: mix(fg, bg, if dark { 0.14 } else { 0.08 }),
            table_head_bg: mix(fg, bg, if dark { 0.07 } else { 0.04 }),
            table_stripe_bg: mix(fg, bg, if dark { 0.035 } else { 0.02 }),
            quote_bar: mix(fg, bg, if dark { 0.28 } else { 0.22 }),
            checkbox_on: link,
            ansi: theme.ansi.map(|c| [c[0], c[1], c[2], 0xff]),
            alerts: [
                accent(pick(4)),
                accent(pick(2)),
                accent(pick(5)),
                accent(pick(3)),
                accent(pick(1)),
            ],
        }
    }

    /// An ANSI colour made readable on the code panel.
    pub(crate) fn syntax(&self, n: usize) -> Rgba {
        ensure_contrast(self.ansi[n.min(15)], self.fg, self.code_bg, 3.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixing_and_contrast() {
        assert_eq!(
            mix([255, 255, 255, 255], [0, 0, 0, 255], 0.5),
            [128, 128, 128, 255]
        );
        assert!((contrast([0, 0, 0, 255], [255, 255, 255, 255]) - 21.0).abs() < 0.01);
        let c = ensure_contrast(
            [250, 250, 250, 255],
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            4.5,
        );
        assert!(contrast(c, [255, 255, 255, 255]) >= 4.5);
    }

    #[test]
    fn palettes_are_readable_on_dark_and_light() {
        let dark = Palette::new(&Theme::default());
        assert!(is_dark(dark.bg));
        assert!(contrast(dark.link, dark.bg) >= 4.0);
        assert!(contrast(dark.muted, dark.bg) >= 3.5);
        let light = Palette::new(&Theme {
            fg: [0x1f, 0x23, 0x28, 0xff],
            bg: [0xff, 0xff, 0xff, 0xff],
            ansi: Theme::default().ansi,
        });
        assert!(!is_dark(light.bg));
        assert!(contrast(light.link, light.bg) >= 4.0);
        for i in 0..16 {
            assert!(contrast(light.syntax(i), light.code_bg) >= 3.0, "ansi {i}");
        }
    }
}
