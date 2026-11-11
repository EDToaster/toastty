//! Experimental `APC smooth` regions, compatible with Ghostty Smooth protocol 3.
//!
//! The grid stays authoritative. A bounded visual offset bridges precision
//! input and the app's next synchronized redraw; at most one wheel request per
//! region is in flight, so reversals cannot misidentify acknowledgements.

use std::time::{Duration, Instant};

const MAX_REGIONS: usize = 8;
const MAX_BACKLOG: f64 = 32.0;
const ACK_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmoothRect {
    pub top: u16,
    pub bottom: u16,
    pub left: u16,
    pub right: u16,
}

impl SmoothRect {
    fn overlaps(self, other: Self) -> bool {
        self.left <= other.right
            && other.left <= self.right
            && self.top <= other.bottom
            && other.top <= self.bottom
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SmoothRegion {
    pub id: u32,
    pub rect: SmoothRect,
    pub position: u32,
    pub max_position: u32,
    /// The extended declaration reserves the rightmost column as well.
    pub horizontal_overscan: bool,
    app_shift: [u16; 2],
    offset: f64,
    pending: i8,
    requested_at: Option<Instant>,
    mouse_cell: (u16, u16),
    queued: Option<WheelRequest>,
}

impl SmoothRegion {
    /// Positive values move content left/up, in fractions of a cell.
    pub fn shift(&self) -> [f32; 2] {
        [
            f32::from(self.app_shift[0]) / 1000.0,
            (self.offset + f64::from(self.app_shift[1]) / 1000.0).clamp(0.0, 0.999) as f32,
        ]
    }

    pub fn contains(&self, col: f64, row: f64) -> bool {
        col >= f64::from(self.rect.left)
            && col < f64::from(self.rect.right) + if self.horizontal_overscan { 0.0 } else { 1.0 }
            && row >= f64::from(self.rect.top)
            && row < f64::from(self.rect.bottom)
    }

    fn reset_motion(&mut self) {
        self.offset = 0.0;
        self.pending = 0;
        self.requested_at = None;
        self.queued = None;
    }

    fn request(&mut self, now: Instant) -> Option<WheelRequest> {
        if self.pending != 0 {
            return None;
        }
        let direction = if self.offset >= 1.0 && self.position < self.max_position {
            1
        } else if self.offset < 0.0 && self.position > 0 {
            -1
        } else {
            return None;
        };
        self.pending = direction;
        self.requested_at = Some(now);
        let request = WheelRequest {
            down: direction > 0,
            cell: self.mouse_cell,
        };
        self.queued = Some(request);
        Some(request)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WheelRequest {
    pub down: bool,
    /// SGR coordinates are one-based.
    pub cell: (u16, u16),
}

#[derive(Debug, PartialEq)]
enum Command {
    Query,
    Clear(Option<u32>),
    Declare(SmoothRegion),
}

fn parse(payload: &[u8], rows: u16, cols: u16) -> Option<Command> {
    // Fixed-field ASCII protocol, not an arbitrary APC payload.
    if payload.len() > 192 {
        return None;
    }
    let fields: Vec<_> = std::str::from_utf8(payload).ok()?.split(';').collect();
    match fields.as_slice() {
        ["smooth", "?"] => return Some(Command::Query),
        ["smooth", "off"] => return Some(Command::Clear(None)),
        ["smooth", "off", id] if !id.is_empty() && id.bytes().all(|b| b.is_ascii_digit()) => {
            return Some(Command::Clear(Some(id.parse().ok()?)));
        }
        _ => {}
    }
    if fields.first() != Some(&"smooth") || !matches!(fields.len(), 5 | 8 | 10) {
        return None;
    }
    let numbers: Vec<u32> = fields[1..]
        .iter()
        .map(|s| {
            if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                None
            } else {
                s.parse().ok()
            }
        })
        .collect::<Option<_>>()?;
    let (id, top, bottom, left, right, position, max_position) = if fields.len() == 5 {
        (
            0,
            numbers[0],
            numbers[1],
            0,
            u32::from(cols.checked_sub(1)?),
            numbers[2],
            numbers[3],
        )
    } else {
        (
            numbers[0], numbers[1], numbers[2], numbers[3], numbers[4], numbers[5], numbers[6],
        )
    };
    let horizontal_overscan = fields.len() == 10;
    let shift = if horizontal_overscan {
        [numbers[7], numbers[8]]
    } else {
        [0, 0]
    };
    if top >= bottom
        || bottom >= u32::from(rows)
        || left > right
        || right >= u32::from(cols)
        || (horizontal_overscan && left == right)
        || position > max_position
        || shift.iter().any(|&v| v > 999)
    {
        return None;
    }
    Some(Command::Declare(SmoothRegion {
        id,
        rect: SmoothRect {
            top: top as u16,
            bottom: bottom as u16,
            left: left as u16,
            right: right as u16,
        },
        position,
        max_position,
        horizontal_overscan,
        app_shift: [shift[0] as u16, shift[1] as u16],
        offset: 0.0,
        pending: 0,
        requested_at: None,
        mouse_cell: (left as u16 + 1, top as u16 + 1),
        queued: None,
    }))
}

#[derive(Debug, Default)]
pub(crate) struct SmoothScroll {
    regions: Vec<SmoothRegion>,
}

impl SmoothScroll {
    pub(crate) fn regions(&self) -> &[SmoothRegion] {
        &self.regions
    }

    pub(crate) fn clear(&mut self) -> bool {
        let changed = !self.regions.is_empty();
        self.regions.clear();
        changed
    }

    pub(crate) fn command(
        &mut self,
        payload: &[u8],
        size: (u16, u16),
        alt: bool,
        now: Instant,
        replies: &mut Vec<u8>,
    ) -> bool {
        match parse(payload, size.0, size.1) {
            Some(Command::Query) => {
                replies.extend_from_slice(b"\x1b_smooth;ok;3\x1b\\");
                false
            }
            Some(Command::Clear(id)) => {
                let before = self.regions.len();
                self.regions.retain(|r| id.is_some_and(|id| id != r.id));
                self.regions.len() != before
            }
            Some(Command::Declare(mut new)) if alt => {
                if self
                    .regions
                    .iter()
                    .any(|r| r.id != new.id && r.rect.overlaps(new.rect))
                {
                    return false;
                }
                if let Some(old) = self.regions.iter_mut().find(|r| r.id == new.id) {
                    if old.rect == new.rect && old.horizontal_overscan == new.horizontal_overscan {
                        let delta = i64::from(new.position) - i64::from(old.position);
                        // A duplicate declaration is not an acknowledgement and
                        // must not refresh the timeout. Other moves are app jumps.
                        if delta == 0 || delta == i64::from(old.pending) {
                            new.offset = (old.offset - f64::from(delta as i8)).clamp(
                                -f64::from(new.position),
                                f64::from(new.max_position - new.position),
                            );
                            new.mouse_cell = old.mouse_cell;
                            if delta == 0 {
                                new.pending = old.pending;
                                new.requested_at = old.requested_at;
                                new.queued = old.queued;
                            } else {
                                new.request(now);
                            }
                        }
                    }
                    *old = new;
                } else if self.regions.len() < MAX_REGIONS {
                    self.regions.push(new);
                } else {
                    return false;
                }
                true
            }
            _ => false,
        }
    }

    /// `None` leaves normal wheel handling in charge. `Some` consumes the
    /// vertical delta, even when no whole row is ready to send.
    pub(crate) fn scroll(&mut self, cell: (f64, f64), delta_rows: f64, now: Instant) -> Option<()> {
        if !delta_rows.is_finite() {
            return None;
        }
        let region = self
            .regions
            .iter_mut()
            .find(|r| r.max_position > 0 && r.contains(cell.0, cell.1))?;
        region.offset = (region.offset + delta_rows).clamp(
            -f64::from(region.position).min(MAX_BACKLOG),
            f64::from(region.max_position - region.position).min(MAX_BACKLOG),
        );
        region.mouse_cell = (cell.0 as u16 + 1, cell.1 as u16 + 1);
        region.request(now);
        Some(())
    }

    pub(crate) fn drain_requests(&mut self, replies: &mut Vec<u8>) {
        for region in &mut self.regions {
            if let Some(request) = region.queued.take() {
                request.write(replies);
            }
        }
    }

    pub(crate) fn cancel_motion(&mut self) -> bool {
        let mut changed = false;
        for region in &mut self.regions {
            changed |= region.offset != 0.0 || region.pending != 0;
            region.reset_motion();
        }
        changed
    }

    pub(crate) fn expire(&mut self, now: Instant) -> bool {
        let mut changed = false;
        for region in &mut self.regions {
            if region
                .requested_at
                .is_some_and(|at| now.duration_since(at) >= ACK_TIMEOUT)
            {
                region.reset_motion();
                changed = true;
            }
        }
        changed
    }

    pub(crate) fn deadline(&self) -> Option<Instant> {
        self.regions
            .iter()
            .filter_map(|r| r.requested_at.map(|t| t + ACK_TIMEOUT))
            .min()
    }
}

impl WheelRequest {
    fn write(self, replies: &mut Vec<u8>) {
        use std::io::Write as _;
        let button = if self.down { 65 } else { 64 };
        let _ = write!(replies, "\x1b[<{button};{};{}M", self.cell.0, self.cell.1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declare(s: &mut SmoothScroll, pos: u32, now: Instant, replies: &mut Vec<u8>) {
        assert!(s.command(
            format!("smooth;1;1;7;1;10;{pos};100").as_bytes(),
            (10, 20),
            true,
            now,
            replies,
        ));
        s.drain_requests(replies);
    }

    fn scroll(
        s: &mut SmoothScroll,
        cell: (f64, f64),
        delta: f64,
        now: Instant,
        replies: &mut Vec<u8>,
    ) -> Option<()> {
        let result = s.scroll(cell, delta, now);
        s.drain_requests(replies);
        result
    }

    #[test]
    fn fractional_input_ack_and_reversal_preserve_the_visual_position() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 10, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), 0.25, now, &mut replies);
        assert_eq!(replies, b"");
        assert_eq!(s.regions[0].shift()[1], 0.25);
        scroll(&mut s, (2.0, 2.0), 1.0, now, &mut replies);
        assert_eq!(replies, b"\x1b[<65;3;3M");
        replies.clear();
        declare(&mut s, 11, now, &mut replies);
        assert_eq!(s.regions[0].shift()[1], 0.25);
        scroll(&mut s, (2.0, 2.0), -0.5, now, &mut replies);
        assert_eq!(replies, b"\x1b[<64;3;3M");
        replies.clear();
        declare(&mut s, 10, now, &mut replies);
        assert_eq!(s.regions[0].shift()[1], 0.75);
    }

    #[test]
    fn slow_apps_get_one_bounded_request_and_duplicates_do_not_extend_timeout() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 10, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), f64::MAX, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), 10.0, now, &mut replies);
        assert_eq!(replies, b"\x1b[<65;3;3M");
        assert_eq!(s.regions[0].offset, MAX_BACKLOG);
        declare(&mut s, 10, now + Duration::from_millis(400), &mut replies);
        assert!(s.expire(now + ACK_TIMEOUT));
        assert_eq!(s.regions[0].shift(), [0.0, 0.0]);
        assert!(s.deadline().is_none());
    }

    #[test]
    fn backlog_drains_on_ack_but_keyboard_jumps_cancel_it() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 10, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), 3.5, now, &mut replies);
        replies.clear();
        declare(&mut s, 11, now, &mut replies);
        assert_eq!(replies, b"\x1b[<65;3;3M");
        replies.clear();
        declare(&mut s, 80, now, &mut replies);
        assert_eq!(replies, b"");
        assert_eq!(s.regions[0].shift(), [0.0, 0.0]);
        assert!(s.deadline().is_none());
    }

    #[test]
    fn invalid_declarations_leave_existing_regions_untouched() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 10, now, &mut replies);
        let before = s.regions.clone();
        for payload in [
            "smooth;1;1;10;1;10;0;100",
            "smooth;1;7;1;1;10;0;100",
            "smooth;1;1;7;1;10;101;100",
            "smooth;1;1;7;1;10;0;100;0;1000",
            "smooth;1;1;7;1;10;0;100;-1;0",
            "smooth;1;1;7;1;10;0;100;NaN;0",
            "smooth;2;2;6;2;9;0;100",
            "smooth;off;+1",
        ] {
            assert!(!s.command(payload.as_bytes(), (10, 20), true, now, &mut replies));
            assert_eq!(s.regions, before);
        }
    }

    #[test]
    fn reversing_an_outstanding_request_does_not_move_the_other_pane() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 10, now, &mut replies);
        assert!(s.command(
            b"smooth;2;1;7;12;19;50;100",
            (10, 20),
            true,
            now,
            &mut replies
        ));
        scroll(&mut s, (2.0, 2.0), 1.5, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), -2.0, now, &mut replies);
        assert_eq!(
            replies, b"\x1b[<65;3;3M",
            "reversal must wait for the original ack"
        );
        replies.clear();
        declare(&mut s, 11, now, &mut replies);
        assert_eq!(replies, b"\x1b[<64;3;3M");
        replies.clear();
        declare(&mut s, 10, now, &mut replies);
        assert_eq!(replies, b"\x1b[<64;3;3M");
        replies.clear();
        declare(&mut s, 9, now, &mut replies);
        assert_eq!(replies, b"");
        assert_eq!(s.regions[0].shift()[1], 0.5);
        assert_eq!(s.regions[1].position, 50);
        assert_eq!(s.regions[1].shift(), [0.0; 2]);
    }

    #[test]
    fn bounds_overscan_and_zero_range_do_not_send_wheel_events() {
        let now = Instant::now();
        let mut s = SmoothScroll::default();
        let mut replies = Vec::new();
        declare(&mut s, 0, now, &mut replies);
        scroll(&mut s, (2.0, 2.0), -5.0, now, &mut replies);
        assert_eq!(replies, b"");
        assert_eq!(scroll(&mut s, (2.0, 7.0), 1.0, now, &mut replies), None);
        assert!(s.command(
            b"smooth;1;1;7;1;10;0;0;500;250",
            (10, 20),
            true,
            now,
            &mut replies
        ));
        assert_eq!(s.regions[0].shift(), [0.5, 0.25]);
        assert_eq!(scroll(&mut s, (2.0, 2.0), 1.0, now, &mut replies), None);
        assert!(!s.regions[0].contains(10.0, 2.0));
    }
}
