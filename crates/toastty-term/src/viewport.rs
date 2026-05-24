//! Scrollback viewport state for the primary grid.
//!
//! Tracks the user's current scroll position (and a target the renderer
//! is animating toward). The position is decomposed as
//! `(lines, pixel_offset)` where:
//!
//! - `lines` is the number of grid rows above the live bottom that the
//!   *bottom* of the rendered viewport sits at; `lines == 0` means
//!   "pinned at the live bottom" and is the steady state for a
//!   non-scrolled-back terminal.
//! - `pixel_offset` is a sub-row offset in pixels, range `[0, cell_h)`.
//!   When it crosses a row boundary the lerp folds it into `lines`.
//!
//! `Smoothing` selects the easing applied while interpolating *current*
//! toward *target*; the binary picks it from the user's config.
//!
//! The viewport is alt-screen-agnostic: when the alt screen is active
//! the host should ignore (or snap) the viewport — the alt grid has no
//! scrollback.

/// Easing function for the lerp between `current` and `target`.
///
/// All variants carry their tuning parameter so the configured speed
/// can vary without changing the discriminant. The binary builds the
/// concrete `Smoothing` value from the user's `[scrollback]` config.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Smoothing {
    /// Snap immediately — no animation. Selected when
    /// `smooth_scrolling = false` or `smoothing_function = "instant"`.
    Instant,
    /// Constant pixel/sec velocity. `pixels_per_sec` is the speed of
    /// approach independent of distance.
    Linear { pixels_per_sec: f32 },
    /// Cubic ease-out over `duration_sec`. The lerp covers the full
    /// distance in roughly that many seconds, slowing as it nears
    /// the target.
    EaseOut { duration_sec: f32 },
    /// Exponential decay with the given half-life. Distance halves
    /// every `halflife_sec`; never overshoots.
    ExpDecay { halflife_sec: f32 },
}

impl Default for Smoothing {
    fn default() -> Self {
        Self::ExpDecay { halflife_sec: 0.08 }
    }
}

/// Viewport state: current position plus the target the user wants to
/// reach. The binary calls `advance` once per frame to drive the lerp.
#[derive(Debug, Clone, Copy, Default)]
pub struct Viewport {
    /// Current rendered offset (lines above the live bottom).
    pub current_lines: u32,
    /// Current sub-row pixel offset, in `[0, cell_h)`.
    pub current_pixel: f32,
    /// Target offset the lerp converges toward.
    pub target_lines: u32,
    pub target_pixel: f32,
}

impl Viewport {
    /// Fresh viewport pinned at the live bottom.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            current_lines: 0,
            current_pixel: 0.0,
            target_lines: 0,
            target_pixel: 0.0,
        }
    }

    /// True when the current position has reached the target.
    #[must_use]
    pub fn at_target(&self) -> bool {
        self.current_lines == self.target_lines
            && (self.current_pixel - self.target_pixel).abs() < 1e-3
    }

    /// True when the *current* (rendered) position is the live bottom.
    /// Used by the renderer to decide whether to draw the cursor.
    #[must_use]
    pub fn at_bottom(&self) -> bool {
        self.current_lines == 0 && self.current_pixel.abs() < 1e-3
    }

    /// True when the *target* is the live bottom — i.e. the user is
    /// either at the bottom or animating back to it.
    #[must_use]
    pub fn target_is_bottom(&self) -> bool {
        self.target_lines == 0 && self.target_pixel.abs() < 1e-3
    }

    /// Set the target to the live bottom (lines=0, pixel=0). The
    /// current position is unchanged; the next `advance` call animates
    /// toward it.
    pub fn snap_target_to_bottom(&mut self) {
        self.target_lines = 0;
        self.target_pixel = 0.0;
    }

    /// Force current = target. Used when `Smoothing::Instant` is in
    /// effect or to reset the viewport on alt-screen toggle.
    pub fn snap_to_target(&mut self) {
        self.current_lines = self.target_lines;
        self.current_pixel = self.target_pixel;
    }

    /// Adjust the *target* by a delta. `delta_lines` is positive when
    /// scrolling up (into history). `delta_pixel` is positive when
    /// scrolling up (pulls the viewport up). The result is folded so
    /// `current_pixel` stays in `[0, cell_h)` and excess pixels roll
    /// into `current_lines`. `max_lines` clamps the upper bound; the
    /// lower bound is always `0`.
    pub fn scroll_target_by(
        &mut self,
        delta_lines: i32,
        delta_pixel: f32,
        cell_h: f32,
        max_lines: u32,
    ) {
        let cell_h = cell_h.max(1.0);
        // Combine current target into a single pixel scalar for clean
        // arithmetic, then re-decompose.
        let cur_px = f64::from(self.target_lines) * f64::from(cell_h) + f64::from(self.target_pixel);
        let delta_px = f64::from(delta_lines) * f64::from(cell_h) + f64::from(delta_pixel);
        let mut new_px = cur_px + delta_px;
        let max_px = f64::from(max_lines) * f64::from(cell_h);
        if new_px < 0.0 {
            new_px = 0.0;
        } else if new_px > max_px {
            new_px = max_px;
        }
        let lines = (new_px / f64::from(cell_h)).floor();
        let pixel = new_px - lines * f64::from(cell_h);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let lines_u32 = lines as u32;
        #[allow(clippy::cast_possible_truncation)]
        let pixel_f32 = pixel as f32;
        self.target_lines = lines_u32;
        self.target_pixel = pixel_f32;
    }

    /// Clamp current + target to `max_lines`. Called by the host when
    /// the scrollback budget shrinks (resize, alt-screen toggle, etc.).
    pub fn clamp_to(&mut self, max_lines: u32) {
        if self.target_lines > max_lines {
            self.target_lines = max_lines;
            self.target_pixel = 0.0;
        }
        if self.current_lines > max_lines {
            self.current_lines = max_lines;
            self.current_pixel = 0.0;
        }
    }

    /// Translate the viewport when the underlying grid scrolls up by
    /// one line. When the user is in scrollback (current/target > 0),
    /// both shift up by one to keep the rendered content stable.
    /// Sticky-at-bottom (== 0) stays at 0. Both are clamped to
    /// `new_max_lines` (the grid's post-scroll history budget).
    pub fn on_grid_scroll_up(&mut self, new_max_lines: u32) {
        if self.target_lines > 0 {
            self.target_lines = (self.target_lines + 1).min(new_max_lines);
        }
        if self.current_lines > 0 {
            self.current_lines = (self.current_lines + 1).min(new_max_lines);
        }
        // Target_pixel stays — sub-row offset doesn't shift when whole
        // rows rotate. Same for current_pixel.
    }

    /// Advance the lerp by `dt` seconds. Returns `true` when the
    /// rendered position changed (the host should redraw).
    pub fn advance(&mut self, dt: f32, cell_h: f32, smoothing: Smoothing) -> bool {
        let cell_h = cell_h.max(1.0);
        let cur_px =
            f64::from(self.current_lines) * f64::from(cell_h) + f64::from(self.current_pixel);
        let tgt_px =
            f64::from(self.target_lines) * f64::from(cell_h) + f64::from(self.target_pixel);
        let delta = tgt_px - cur_px;
        // Already converged in scalar pixel space — snap any (lines,
        // pixel) decomposition drift and report no change.
        if delta.abs() < SNAP_EPSILON_PX {
            let was = (self.current_lines, self.current_pixel);
            self.snap_to_target();
            return was != (self.current_lines, self.current_pixel);
        }

        let new_px = match smoothing {
            Smoothing::Instant => tgt_px,
            Smoothing::Linear { pixels_per_sec } => {
                let step = f64::from(pixels_per_sec.max(0.0)) * f64::from(dt);
                if delta.abs() <= step {
                    tgt_px
                } else {
                    cur_px + step * delta.signum()
                }
            }
            Smoothing::EaseOut { duration_sec } => {
                // Cubic ease-out approximated by exponential approach
                // tuned so ~63% of the distance is covered per
                // `duration_sec / 3` slice. Smooth and never overshoots.
                let tau = f64::from(duration_sec.max(0.001)) / 3.0;
                let factor = 1.0 - (-f64::from(dt) / tau).exp();
                let step = delta * factor;
                if step.abs() >= delta.abs() {
                    tgt_px
                } else {
                    cur_px + step
                }
            }
            Smoothing::ExpDecay { halflife_sec } => {
                let factor = 1.0 - 2.0_f64.powf(-f64::from(dt) / f64::from(halflife_sec.max(1e-6)));
                cur_px + delta * factor
            }
        };

        // Snap to target if the lerp brought us within sub-pixel of it.
        // Without this the (lines, pixel) decomposition can stall just
        // below a row boundary due to float drift.
        if (new_px - tgt_px).abs() < SNAP_EPSILON_PX {
            let was = (self.current_lines, self.current_pixel);
            self.snap_to_target();
            return was != (self.current_lines, self.current_pixel);
        }

        // Decompose new_px back into (lines, pixel). new_px is always
        // >= 0 because both endpoints are non-negative and lerp stays
        // between them.
        let new_px = new_px.max(0.0);
        let lines = (new_px / f64::from(cell_h)).floor();
        let pixel = new_px - lines * f64::from(cell_h);
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let new_lines = lines as u32;
        #[allow(clippy::cast_possible_truncation)]
        let new_pixel = pixel as f32;
        let changed =
            new_lines != self.current_lines || (new_pixel - self.current_pixel).abs() > 1e-3;
        self.current_lines = new_lines;
        self.current_pixel = new_pixel;
        changed
    }
}

/// Snap-to-target threshold for the lerp, in pixels. Anything within
/// half a pixel of the target counts as "there" — without this, the
/// `(lines, pixel)` decomposition can stall just below a row boundary
/// (e.g. lines=99, pixel=15.9999 instead of lines=100, pixel=0).
const SNAP_EPSILON_PX: f64 = 0.5;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_is_at_bottom_and_at_target() {
        let v = Viewport::new();
        assert!(v.at_bottom());
        assert!(v.at_target());
        assert!(v.target_is_bottom());
    }

    #[test]
    fn scroll_target_clamps_to_max() {
        let mut v = Viewport::new();
        v.scroll_target_by(100, 0.0, 16.0, 10);
        assert_eq!(v.target_lines, 10);
        assert!((v.target_pixel - 0.0).abs() < 1e-3);
    }

    #[test]
    fn scroll_target_clamps_to_zero_at_bottom() {
        let mut v = Viewport::new();
        v.scroll_target_by(-5, 0.0, 16.0, 10);
        assert_eq!(v.target_lines, 0);
        assert!((v.target_pixel - 0.0).abs() < 1e-3);
    }

    #[test]
    fn scroll_target_folds_pixels_into_lines() {
        let mut v = Viewport::new();
        // 1 line + 20px @ 16px cell → 2 lines + 4px.
        v.scroll_target_by(1, 20.0, 16.0, 100);
        assert_eq!(v.target_lines, 2);
        assert!((v.target_pixel - 4.0).abs() < 1e-3);
    }

    #[test]
    fn snap_target_to_bottom_zeroes_target_only() {
        let mut v = Viewport::new();
        v.scroll_target_by(5, 0.0, 16.0, 100);
        // Pretend we've animated halfway:
        v.current_lines = 3;
        v.snap_target_to_bottom();
        assert_eq!(v.target_lines, 0);
        assert_eq!(v.current_lines, 3);
    }

    #[test]
    fn advance_instant_jumps_in_one_step() {
        let mut v = Viewport::new();
        v.target_lines = 42;
        assert!(v.advance(0.016, 16.0, Smoothing::Instant));
        assert_eq!(v.current_lines, 42);
        // Subsequent calls report no change.
        assert!(!v.advance(0.016, 16.0, Smoothing::Instant));
    }

    #[test]
    fn advance_exp_decay_converges() {
        let mut v = Viewport::new();
        v.target_lines = 100;
        for _ in 0..1000 {
            v.advance(1.0 / 60.0, 16.0, Smoothing::ExpDecay { halflife_sec: 0.08 });
        }
        assert!(v.at_target());
    }

    #[test]
    fn advance_linear_constant_speed() {
        let mut v = Viewport::new();
        v.target_lines = 10; // 160 px @ 16-px cell.
        // 320 px/sec for 0.25 sec = 80 px.
        v.advance(0.25, 16.0, Smoothing::Linear { pixels_per_sec: 320.0 });
        // Should be at 5 lines (80 / 16).
        assert_eq!(v.current_lines, 5);
    }

    #[test]
    fn advance_ease_out_never_overshoots() {
        let mut v = Viewport::new();
        v.target_lines = 50;
        let mut prev = 0u32;
        for _ in 0..200 {
            v.advance(1.0 / 60.0, 16.0, Smoothing::EaseOut { duration_sec: 0.3 });
            assert!(v.current_lines <= v.target_lines);
            assert!(v.current_lines >= prev);
            prev = v.current_lines;
        }
        assert!(v.at_target());
    }

    #[test]
    fn advance_returns_false_at_target() {
        let mut v = Viewport::new();
        assert!(!v.advance(0.016, 16.0, Smoothing::ExpDecay { halflife_sec: 0.08 }));
    }

    #[test]
    fn on_grid_scroll_up_keeps_view_stable() {
        let mut v = Viewport::new();
        v.target_lines = 5;
        v.current_lines = 5;
        v.on_grid_scroll_up(10);
        assert_eq!(v.target_lines, 6);
        assert_eq!(v.current_lines, 6);
    }

    #[test]
    fn on_grid_scroll_up_sticky_at_bottom() {
        let mut v = Viewport::new();
        v.on_grid_scroll_up(10);
        // Was at bottom (0), stays at bottom.
        assert_eq!(v.target_lines, 0);
        assert_eq!(v.current_lines, 0);
    }

    #[test]
    fn on_grid_scroll_up_saturates_at_max() {
        let mut v = Viewport::new();
        v.target_lines = 10;
        v.current_lines = 10;
        // History budget is already 10; scrolling up doesn't grow it
        // further (the user's already at the oldest line).
        v.on_grid_scroll_up(10);
        assert_eq!(v.target_lines, 10);
        assert_eq!(v.current_lines, 10);
    }

    #[test]
    fn clamp_to_drops_target_and_current() {
        let mut v = Viewport::new();
        v.target_lines = 50;
        v.current_lines = 30;
        v.clamp_to(20);
        assert_eq!(v.target_lines, 20);
        assert_eq!(v.current_lines, 20);
    }
}
