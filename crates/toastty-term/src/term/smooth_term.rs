use std::time::Instant;

use super::{MouseEncoding, Term};
use crate::SmoothRegion;

impl Term {
    pub fn smooth_regions(&self) -> &[SmoothRegion] {
        self.smooth_scroll.regions()
    }

    pub fn clear_smooth_regions(&mut self) {
        if self.smooth_scroll.clear() {
            self.mark_all_dirty();
        }
    }

    pub(super) fn handle_smooth_apc(&mut self, payload: &[u8]) {
        if self.smooth_scroll.command(
            payload,
            (self.rows, self.cols),
            self.alt_active,
            Instant::now(),
            &mut self.pty_replies,
        ) {
            self.mark_all_dirty();
        }
    }

    /// Input coordinates are fractional, zero-based grid cells. Only SGR
    /// tracking is intercepted: the protocol's acknowledgements assume one
    /// ordinary wheel event moves exactly one displayed content row.
    pub fn scroll_smooth_region(
        &mut self,
        cell: (f64, f64),
        delta_rows: f64,
        now: Instant,
    ) -> bool {
        if !self.alt_active
            || !self.mouse_mode.is_on()
            || self.mouse_mode.encoding != MouseEncoding::Sgr
        {
            return false;
        }
        self.expire_smooth_scroll(now);
        let consumed = self.smooth_scroll.scroll(cell, delta_rows, now).is_some();
        if consumed {
            self.mark_all_dirty();
        }
        consumed
    }

    pub fn expire_smooth_scroll(&mut self, now: Instant) {
        let changed = if self.mouse_mode.is_on() && self.mouse_mode.encoding == MouseEncoding::Sgr {
            self.smooth_scroll.expire(now)
        } else {
            self.smooth_scroll.cancel_motion()
        };
        if changed {
            self.mark_all_dirty();
        }
    }

    pub fn smooth_scroll_deadline(&self) -> Option<Instant> {
        self.smooth_scroll.deadline()
    }

    pub fn smooth_input_cell(&self, cell: (f64, f64)) -> (f64, f64) {
        if let Some(region) = self
            .smooth_regions()
            .iter()
            .find(|r| r.contains(cell.0, cell.1))
        {
            let [x, y] = region.shift();
            (cell.0 + f64::from(x), cell.1 + f64::from(y))
        } else {
            cell
        }
    }
}
