//! Smooth-scroll viewport state.
//!
//! Per architecture.md: the renderer holds `top_line: u64` plus a
//! `pixel_offset: f32`, then asks the ring for `top..top+visible+1` rows
//! every frame. This module owns just the lerp math — pure CPU, easy
//! to unit test.
//!
//! M4b does not wire scrolling into the demo; this is here so M5 has a
//! tested foundation.

/// Per-frame smoothing factor. Empirical sweet spot for ~60Hz updates;
/// users can override.
const DEFAULT_HALFLIFE_SECONDS: f32 = 0.08;

/// Viewport state — current position plus the target the user wants.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Viewport {
    /// Logical row at the top of the visible area.
    pub top_line: u64,
    /// Sub-row pixel offset; `0.0` = aligned, positive = pulled up.
    pub pixel_offset: f32,
    /// Where the scroll wants to settle. The current state lerps toward
    /// it on every `update`.
    pub target_top_line: u64,
    pub target_pixel_offset: f32,
    /// Cell height in pixels — needed to convert row deltas to pixel
    /// deltas during lerping.
    pub cell_height: f32,
    /// Half-life in seconds for the exponential decay (smaller = snappier).
    pub halflife: f32,
}

impl Viewport {
    /// New viewport pinned at `top_line` with no pending motion.
    #[must_use]
    pub fn new(top_line: u64, cell_height: f32) -> Self {
        Self {
            top_line,
            pixel_offset: 0.0,
            target_top_line: top_line,
            target_pixel_offset: 0.0,
            cell_height: cell_height.max(1.0),
            halflife: DEFAULT_HALFLIFE_SECONDS,
        }
    }

    /// Convert the `(top_line, pixel_offset)` pair into a single "scroll
    /// position in pixels" scalar. Useful for the lerp.
    #[must_use]
    #[allow(clippy::cast_precision_loss)] // 52 bits of mantissa is plenty for scrollback line indices
    pub fn position_px(&self) -> f64 {
        self.top_line as f64 * f64::from(self.cell_height) + f64::from(self.pixel_offset)
    }

    /// Same as `position_px` but for the target.
    #[must_use]
    #[allow(clippy::cast_precision_loss)]
    pub fn target_position_px(&self) -> f64 {
        self.target_top_line as f64 * f64::from(self.cell_height)
            + f64::from(self.target_pixel_offset)
    }

    /// Set a new target. The current position is unchanged; `update`
    /// will lerp toward this.
    pub fn scroll_to(&mut self, target_top_line: u64, target_pixel_offset: f32) {
        self.target_top_line = target_top_line;
        self.target_pixel_offset = target_pixel_offset;
    }

    /// Advance the viewport by `dt` seconds toward the target. Returns
    /// `true` if the position changed (caller can use this to decide
    /// whether to wake the renderer).
    pub fn update(&mut self, dt: f32) -> bool {
        if self.halflife <= 0.0 {
            // Instant snap.
            let snapped = self.target_position_px();
            self.set_position_px(snapped);
            return true;
        }

        let cur = self.position_px();
        let tgt = self.target_position_px();
        let delta = tgt - cur;
        if delta.abs() < 1e-3 {
            // Already at target — snap to remove drift, but report no
            // change so the renderer can park.
            self.set_position_px(tgt);
            return false;
        }

        // Exponential decay: pos += delta * (1 - 2^(-dt/halflife)).
        let factor = 1.0 - 2.0_f64.powf(-f64::from(dt) / f64::from(self.halflife));
        let new_pos = cur + delta * factor;
        self.set_position_px(new_pos);
        true
    }

    fn set_position_px(&mut self, p: f64) {
        // Decompose into integer row + fractional pixel.
        let cell = f64::from(self.cell_height);
        let row = (p / cell).floor();
        let offset = p - row * cell;

        // Clamp into u64 — negative position means "scrolled past the
        // top"; we let callers prevent that, but defensively clamp here.
        #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let row_u64 = if row < 0.0 { 0 } else { row as u64 };
        self.top_line = row_u64;
        #[allow(clippy::cast_possible_truncation)]
        let off = offset as f32;
        self.pixel_offset = off;
    }

    /// Row range to fetch for rendering: `[top..top + visible + 1)`. The
    /// `+1` is required so partial rows at the bottom are covered during
    /// fractional scrolling.
    #[must_use]
    pub fn row_range(&self, visible_rows: u64) -> (u64, u64) {
        (self.top_line, self.top_line + visible_rows + 1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn new_viewport_starts_at_target_with_no_offset() {
        let v = Viewport::new(10, 16.0);
        assert_eq!(v.top_line, 10);
        assert!((v.pixel_offset - 0.0).abs() < 1e-6);
        assert_eq!(v.target_top_line, 10);
    }

    #[test]
    fn position_px_combines_row_and_offset() {
        let mut v = Viewport::new(5, 16.0);
        v.pixel_offset = 4.0;
        assert!(approx(v.position_px(), 5.0 * 16.0 + 4.0));
    }

    #[test]
    fn scroll_to_sets_target_only() {
        let mut v = Viewport::new(0, 16.0);
        v.scroll_to(20, 4.0);
        assert_eq!(v.top_line, 0);
        assert_eq!(v.target_top_line, 20);
        assert!(approx(f64::from(v.target_pixel_offset), 4.0));
    }

    #[test]
    fn update_returns_true_when_distance_remains() {
        let mut v = Viewport::new(0, 16.0);
        v.scroll_to(100, 0.0);
        assert!(v.update(0.016));
    }

    #[test]
    fn update_returns_false_when_at_target() {
        let mut v = Viewport::new(7, 16.0);
        assert!(!v.update(0.016));
    }

    #[test]
    fn update_converges_to_target() {
        let mut v = Viewport::new(0, 16.0);
        v.scroll_to(10, 0.0);
        // Run for plenty of frames at 60Hz.
        for _ in 0..200 {
            v.update(1.0 / 60.0);
        }
        assert!(approx(v.position_px(), v.target_position_px()));
        assert_eq!(v.top_line, 10);
    }

    #[test]
    fn update_with_zero_halflife_snaps_immediately() {
        let mut v = Viewport::new(0, 16.0);
        v.halflife = 0.0;
        v.scroll_to(42, 0.0);
        assert!(v.update(0.001));
        assert_eq!(v.top_line, 42);
    }

    #[test]
    fn row_range_covers_visible_plus_one() {
        let v = Viewport::new(5, 16.0);
        let (lo, hi) = v.row_range(20);
        assert_eq!(lo, 5);
        assert_eq!(hi, 5 + 20 + 1);
    }

    #[test]
    fn cell_height_clamped_to_one() {
        // Zero cell height would cause div-by-zero in `set_position_px`.
        // Constructor must clamp.
        let v = Viewport::new(0, 0.0);
        assert!(v.cell_height >= 1.0);
    }

    #[test]
    fn negative_position_clamps_to_zero() {
        let mut v = Viewport::new(2, 16.0);
        v.scroll_to(0, 0.0);
        v.halflife = 0.0;
        v.update(0.001);
        assert_eq!(v.top_line, 0);
    }

    #[test]
    fn fractional_offset_survives_round_trip() {
        let mut v = Viewport::new(0, 16.0);
        v.scroll_to(3, 4.0);
        v.halflife = 0.0;
        v.update(0.001);
        assert_eq!(v.top_line, 3);
        assert!((v.pixel_offset - 4.0).abs() < 1e-3);
    }

    #[test]
    fn lerp_overshoot_clamps_to_target_eventually() {
        // Drive a small target swing; SSIM-style: it shouldn't oscillate.
        let mut v = Viewport::new(100, 16.0);
        v.scroll_to(50, 0.0);
        let mut prev_dist = f64::INFINITY;
        for _ in 0..100 {
            v.update(0.016);
            let dist = (v.position_px() - v.target_position_px()).abs();
            // Exponential decay — distance monotonically decreases.
            assert!(dist <= prev_dist + 1e-6);
            prev_dist = dist;
        }
    }
}
