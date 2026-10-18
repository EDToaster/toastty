//! Scroll-to-bottom chip: a small round, low-profile button with a
//! chevron, shown in a corner of the content area while the view is
//! scrolled back into the scrollback.
//!
//! Unlike the rest of the overlays it is not built from terminal cells:
//! it is drawn by its own tiny SDF pipeline ([`pipeline::ChipPipeline`],
//! `shaders/chip.wgsl`) so the disc, hairline ring and chevron are
//! anti-aliased at device resolution and sit at a fixed pixel margin
//! from the corner, independent of the cell grid and font.
//!
//! This module holds the pure, unit-tested parts: geometry
//! ([`chip_geometry`]), theme-derived colours ([`chip_colors`]) and the
//! fade animation ([`ChipFade`]).

pub mod pipeline;

use std::time::{Duration, Instant};

use crate::ScrollButtonCorner;

/// Chip diameter in logical px.
pub const CHIP_DIAMETER: f32 = 28.0;
/// Gap between the chip and the content edges, in logical px.
pub const CHIP_MARGIN: f32 = 10.0;
/// Extra hit-test slop around the drawn disc, in logical px, so the
/// small chip stays easy to click.
pub const CHIP_HIT_SLOP: f32 = 4.0;
/// Fade-in / fade-out duration.
pub const CHIP_FADE: Duration = Duration::from_millis(120);

/// Placement of the chip, all in physical framebuffer px (content
/// origin already applied).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipGeometry {
    /// Disc centre.
    pub center: [f32; 2],
    /// Disc radius.
    pub radius: f32,
    /// Hit-test rect `[x0, y0, x1, y1]` (the disc's bounding box plus
    /// [`CHIP_HIT_SLOP`]).
    pub hit_rect: [f32; 4],
}

/// Where the chip goes for a content area at `origin` (physical px)
/// of size `dims` (physical px) at window scale `scale`, anchored to
/// `corner`. `None` when the content area is too small to fit the
/// chip plus its margins (or the inputs are degenerate).
#[must_use]
pub fn chip_geometry(
    origin: [f32; 2],
    dims: (f32, f32),
    scale: f32,
    corner: ScrollButtonCorner,
) -> Option<ChipGeometry> {
    if !(scale.is_finite() && scale > 0.0) {
        return None;
    }
    let radius = CHIP_DIAMETER * 0.5 * scale;
    let margin = CHIP_MARGIN * scale;
    let need = 2.0 * (radius + margin);
    if !(dims.0 >= need && dims.1 >= need) {
        return None;
    }
    let cx = match corner {
        ScrollButtonCorner::BottomRight => origin[0] + dims.0 - margin - radius,
        ScrollButtonCorner::BottomLeft => origin[0] + margin + radius,
    };
    let cy = origin[1] + dims.1 - margin - radius;
    let reach = radius + CHIP_HIT_SLOP * scale;
    Some(ChipGeometry {
        center: [cx, cy],
        radius,
        hit_rect: [cx - reach, cy - reach, cx + reach, cy + reach],
    })
}

/// Gap between the "Copy Markdown" chip and the block's top / the
/// content's right edge, in logical px.
pub const COPY_CHIP_MARGIN: f32 = 6.0;

/// Where the rich block "Copy Markdown" chip goes: the top-right corner
/// of the block, whose first visible row starts `block_top` px below
/// the content top (negative when it begins above the viewport, in
/// which case the chip sticks to the content top). `None` when the
/// content area can't fit it.
#[must_use]
pub fn copy_chip_geometry(
    origin: [f32; 2],
    dims: (f32, f32),
    scale: f32,
    block_top: f32,
) -> Option<ChipGeometry> {
    if !(scale.is_finite() && scale > 0.0 && block_top.is_finite()) {
        return None;
    }
    let radius = CHIP_DIAMETER * 0.5 * scale;
    let margin = COPY_CHIP_MARGIN * scale;
    let need = 2.0 * (radius + margin);
    if !(dims.0 >= need && dims.1 >= need) {
        return None;
    }
    let cx = origin[0] + dims.0 - margin - radius;
    let cy = origin[1] + block_top.max(0.0) + margin + radius;
    let reach = radius + CHIP_HIT_SLOP * scale;
    Some(ChipGeometry {
        center: [cx, cy],
        radius,
        hit_rect: [cx - reach, cy - reach, cx + reach, cy + reach],
    })
}

/// Straight-alpha colours for the chip's fill, ring and chevron.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipColors {
    pub fill: [f32; 4],
    pub border: [f32; 4],
    pub icon: [f32; 4],
}

/// Derive the chip colours from the (linear-light) theme: a fill just
/// off the background, opaque so scrollback text never bleeds
/// through (it reads as a raised surface without shouting), a faint hairline ring in the foreground colour and a muted
/// chevron. Hovering lifts all three. The fill mix is done in sRGB so
/// "10% toward fg" looks like 10% rather than the much brighter result
/// a linear-light mix gives on dark themes.
#[must_use]
pub fn chip_colors(bg: [f32; 4], fg: [f32; 4], hovered: bool) -> ChipColors {
    let mix = |k: f32| -> [f32; 3] {
        std::array::from_fn(|i| {
            let b = linear_to_srgb(bg[i]);
            let f = linear_to_srgb(fg[i]);
            srgb_to_linear(b + (f - b) * k)
        })
    };
    let (fill_k, fill_a, border_a, icon_a) = if hovered {
        (0.22, 1.0, 0.22, 1.0)
    } else {
        (0.12, 1.0, 0.12, 0.72)
    };
    let f = mix(fill_k);
    ChipColors {
        fill: [f[0], f[1], f[2], fill_a],
        border: [fg[0], fg[1], fg[2], border_a],
        icon: [fg[0], fg[1], fg[2], icon_a],
    }
}

fn linear_to_srgb(c: f32) -> f32 {
    let c = c.clamp(0.0, 1.0);
    if c <= 0.003_130_8 {
        c * 12.92
    } else {
        1.055 * c.powf(1.0 / 2.4) - 0.055
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

/// Linear-in-time, ease-out fade between hidden (0) and shown (1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChipFade {
    from: f32,
    to: f32,
    start: Option<Instant>,
}

impl Default for ChipFade {
    fn default() -> Self {
        Self {
            from: 0.0,
            to: 0.0,
            start: None,
        }
    }
}

impl ChipFade {
    /// Retarget the fade. A no-op when already heading to the same
    /// target; otherwise the fade restarts from the current value so a
    /// reversal mid-fade doesn't jump.
    pub fn set_visible(&mut self, visible: bool, now: Instant) {
        let to = if visible { 1.0 } else { 0.0 };
        if (to - self.to).abs() < f32::EPSILON {
            return;
        }
        self.from = self.value(now);
        self.to = to;
        self.start = Some(now);
    }

    /// Jump straight to the target state with no animation (used when
    /// the chip is disabled).
    pub fn snap(&mut self, visible: bool) {
        let v = if visible { 1.0 } else { 0.0 };
        self.from = v;
        self.to = v;
        self.start = None;
    }

    /// Current opacity in `0..=1`.
    #[must_use]
    pub fn value(&self, now: Instant) -> f32 {
        let Some(start) = self.start else {
            return self.to;
        };
        let t = now.saturating_duration_since(start).as_secs_f32() / CHIP_FADE.as_secs_f32();
        if t >= 1.0 {
            return self.to;
        }
        // Ease-out cubic.
        let e = 1.0 - (1.0 - t).powi(3);
        self.from + (self.to - self.from) * e
    }

    /// True while a fade is still in progress at `now`.
    #[must_use]
    pub fn animating(&self, now: Instant) -> bool {
        self.start
            .is_some_and(|s| now.saturating_duration_since(s) < CHIP_FADE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn geometry_anchors_bottom_right_with_pixel_margin() {
        let g = chip_geometry(
            [0.0, 0.0],
            (800.0, 480.0),
            1.0,
            ScrollButtonCorner::BottomRight,
        )
        .unwrap();
        assert_eq!(g.radius, 14.0);
        assert_eq!(g.center, [800.0 - 10.0 - 14.0, 480.0 - 10.0 - 14.0]);
        assert_eq!(g.hit_rect, [758.0, 438.0, 794.0, 474.0]);
    }

    #[test]
    fn geometry_anchors_bottom_left_and_applies_origin() {
        let g = chip_geometry(
            [5.0, 7.0],
            (300.0, 200.0),
            1.0,
            ScrollButtonCorner::BottomLeft,
        )
        .unwrap();
        assert_eq!(g.center, [5.0 + 10.0 + 14.0, 7.0 + 200.0 - 24.0]);
    }

    #[test]
    fn copy_chip_sits_at_block_top_right() {
        let g = copy_chip_geometry([4.0, 2.0], (800.0, 480.0), 1.0, 32.0).unwrap();
        assert_eq!(
            g.center,
            [4.0 + 800.0 - 6.0 - 14.0, 2.0 + 32.0 + 6.0 + 14.0]
        );
        // A block starting above the viewport pins the chip to the top.
        let g = copy_chip_geometry([0.0, 0.0], (800.0, 480.0), 2.0, -50.0).unwrap();
        assert_eq!(g.center, [800.0 - 12.0 - 28.0, 12.0 + 28.0]);
        assert!(copy_chip_geometry([0.0, 0.0], (30.0, 480.0), 1.0, 0.0).is_none());
        assert!(copy_chip_geometry([0.0, 0.0], (800.0, 480.0), 1.0, f32::NAN).is_none());
    }

    #[test]
    fn geometry_scales_with_dpi() {
        let g = chip_geometry(
            [0.0, 0.0],
            (1600.0, 960.0),
            2.0,
            ScrollButtonCorner::BottomRight,
        )
        .unwrap();
        assert_eq!(g.radius, 28.0);
        assert_eq!(g.center, [1600.0 - 20.0 - 28.0, 960.0 - 48.0]);
        assert_eq!(g.hit_rect[2] - g.hit_rect[0], 2.0 * (28.0 + 8.0));
    }

    #[test]
    fn geometry_none_when_too_small_or_degenerate() {
        let br = ScrollButtonCorner::BottomRight;
        assert!(chip_geometry([0.0, 0.0], (47.0, 480.0), 1.0, br).is_none());
        assert!(chip_geometry([0.0, 0.0], (800.0, 47.0), 1.0, br).is_none());
        assert!(chip_geometry([0.0, 0.0], (48.0, 48.0), 1.0, br).is_some());
        assert!(chip_geometry([0.0, 0.0], (800.0, 480.0), 0.0, br).is_none());
        assert!(chip_geometry([0.0, 0.0], (800.0, 480.0), f32::NAN, br).is_none());
        assert!(chip_geometry([0.0, 0.0], (f32::NAN, 480.0), 1.0, br).is_none());
    }

    #[test]
    fn colors_are_subtle_and_hover_lifts_them() {
        let bg = [0.0, 0.0, 0.0, 1.0];
        let fg = [1.0, 1.0, 1.0, 1.0];
        let n = chip_colors(bg, fg, false);
        let h = chip_colors(bg, fg, true);
        // 12% toward white in sRGB, back to linear.
        assert!((linear_to_srgb(n.fill[0]) - 0.12).abs() < 1e-4);
        assert!(h.fill[0] > n.fill[0]);
        assert_eq!(n.fill[3], 1.0, "opaque so text never bleeds through");
        assert!(h.border[3] > n.border[3]);
        assert!(h.icon[3] > n.icon[3]);
        assert_eq!(&n.icon[..3], &fg[..3]);
        assert!(n.border[3] < 0.2, "ring stays a hairline");
    }

    #[test]
    fn srgb_round_trips() {
        for v in [0.0, 0.002, 0.04, 0.2, 0.5, 1.0] {
            assert!((srgb_to_linear(linear_to_srgb(v)) - v).abs() < 1e-4, "{v}");
        }
        assert!((linear_to_srgb(2.0) - 1.0).abs() < 1e-6);
        assert_eq!(srgb_to_linear(-1.0), 0.0);
    }

    #[test]
    fn fade_runs_in_and_out() {
        let t0 = Instant::now();
        let mut f = ChipFade::default();
        assert_eq!(f.value(t0), 0.0);
        assert!(!f.animating(t0));
        f.set_visible(true, t0);
        assert!(f.animating(t0));
        assert_eq!(f.value(t0), 0.0);
        let mid = f.value(t0 + CHIP_FADE / 2);
        assert!(
            mid > 0.5 && mid < 1.0,
            "ease-out is past halfway at t=0.5: {mid}"
        );
        assert_eq!(f.value(t0 + CHIP_FADE), 1.0);
        assert!(!f.animating(t0 + CHIP_FADE));
        // Same target again: no restart.
        f.set_visible(true, t0 + CHIP_FADE * 2);
        assert!(!f.animating(t0 + CHIP_FADE * 2));
        // Reverse mid-fade starts from the current value (no jump).
        let t1 = t0 + CHIP_FADE * 3;
        f.set_visible(false, t1);
        let quarter = t1 + CHIP_FADE / 4;
        let v = f.value(quarter);
        f.set_visible(true, quarter);
        assert!((f.value(quarter) - v).abs() < 1e-6);
        assert_eq!(f.value(quarter + CHIP_FADE), 1.0);
    }

    #[test]
    fn snap_skips_the_animation() {
        let t0 = Instant::now();
        let mut f = ChipFade::default();
        f.set_visible(true, t0);
        f.snap(false);
        assert_eq!(f.value(t0), 0.0);
        assert!(!f.animating(t0));
        f.snap(true);
        assert_eq!(f.value(t0), 1.0);
    }
}
