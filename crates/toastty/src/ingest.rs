//! PTY ingest pacing: how much buffered output to parse before letting a
//! frame through.
//!
//! The parser, `Term` and renderer all live on the main thread, so under a
//! flood (`cat` of a big file) every millisecond spent parsing is one not
//! spent drawing. Parsing everything the reader has buffered before each
//! frame drew nothing at all until the flood ended. Instead the
//! `Event::PtyReady` handler parses in slices and, once a frame is due,
//! stops and answers `ControlSignal::Yield`: the loop draws, then resumes
//! parsing. Small writes (typing, prompts) are parsed in one go and drawn
//! at once, exactly as before; only output that outlasts a frame interval
//! is paced.
//!
//! See `docs/decisions/pty-event-loop.md` (2026-10-22 section).

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use memchr::memmem;

/// Most bytes parsed between two "is a frame due?" checks. At the
/// slowest parse rates seen (~13 MB/s, `seq`-style short lines) this is
/// ~1.3 ms, fine enough to keep frames on time; big enough that the
/// per-slice work in `handle_pty_bytes` stays noise.
pub const SLICE: usize = 16 * 1024;

/// Target frame period while output floods in (~60 Hz). The display rate
/// isn't known here (winit only reports it per monitor, and it can change
/// as the window moves); 60 Hz is the common floor and keeps the render
/// share of a flood small.
pub const FRAME_INTERVAL: Duration = Duration::from_micros(16_667);

/// Least time to parse before yielding again, so a flood always makes
/// progress. The longer of this and the last frame's render time is
/// used, which keeps parsing at >= half the main thread when frames are
/// slow (e.g. a huge window).
const MIN_PARSE: Duration = Duration::from_millis(1);

/// How long a requested frame may hold up parsing. Redraws arrive within
/// the same loop iteration on every platform winit supports, so this only
/// guards against one that never comes: parsing then carries on as if no
/// frame were pending.
const MAX_FRAME_WAIT: Duration = Duration::from_millis(100);

/// Mode 2026 end-synchronized-update, `CSI ? 2026 l`.
const ESU: &[u8] = b"\x1b[?2026l";

static ESU_FINDER: LazyLock<memmem::Finder<'static>> = LazyLock::new(|| memmem::Finder::new(ESU));

/// Pacing state for the PTY ingest. One per window, owned by the app.
#[derive(Debug, Default)]
pub struct Ingest {
    /// Start of the last frame that was actually drawn.
    last_frame: Option<Instant>,
    /// How long that frame took to render.
    last_render: Duration,
    /// When the ingest asked for a redraw that hasn't run yet.
    frame_requested: Option<Instant>,
}

impl Ingest {
    /// A frame was drawn between `start` and `end`.
    pub fn frame_drawn(&mut self, start: Instant, end: Instant) {
        self.last_frame = Some(start);
        self.last_render = end.saturating_duration_since(start);
    }

    /// The redraw handler ran (drawn or skipped).
    pub fn redraw_handled(&mut self) {
        self.frame_requested = None;
    }

    /// The ingest answered with a redraw request at `now`.
    pub fn frame_requested(&mut self, now: Instant) {
        self.frame_requested.get_or_insert(now);
    }

    fn frame_due(&self, now: Instant) -> bool {
        self.last_frame
            .is_none_or(|t| now.saturating_duration_since(t) >= FRAME_INTERVAL)
    }

    /// At the top of `PtyReady`: a redraw the ingest asked for is still
    /// queued and a frame is due, so yield without parsing. Without this,
    /// output arriving about as fast as it parses would deliver a fresh
    /// wake each time the handler drained the buffer, and winit hands out
    /// every queued user event before `RedrawRequested`.
    #[must_use]
    pub fn should_wait_for_frame(&self, now: Instant) -> bool {
        self.frame_requested
            .is_some_and(|at| now.saturating_duration_since(at) < MAX_FRAME_WAIT)
            && self.frame_due(now)
    }

    /// Between slices: stop parsing (begun at `parse_start`) to draw a
    /// frame. The caller also holds off while mode 2026 pauses rendering,
    /// since that frame would be skipped.
    #[must_use]
    pub fn should_yield(&self, now: Instant, parse_start: Instant) -> bool {
        let min_parse = self.last_render.clamp(MIN_PARSE, FRAME_INTERVAL);
        self.frame_due(now) && now.saturating_duration_since(parse_start) >= min_parse
    }
}

/// How much of `avail` to parse next once a frame is due but mode 2026
/// is holding it back: up to and including the first ESU, if there is
/// one. Ending the slice right after the ESU lets the frame go out before
/// the next synchronized update begins (apps often send ESU and the next
/// BSU back to back, so a slice boundary anywhere else would almost
/// always land inside an update).
#[must_use]
pub fn slice_to_esu(avail: &[u8]) -> usize {
    ESU_FINDER
        .find(avail)
        .map_or(avail.len(), |i| i + ESU.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn the_first_frame_is_due_at_once() {
        let ing = Ingest::default();
        let now = Instant::now();
        // Still parses for MIN_PARSE first.
        assert!(!ing.should_yield(now, now));
        assert!(ing.should_yield(now + ms(1), now));
    }

    #[test]
    fn yields_a_frame_interval_after_the_last_frame() {
        let mut ing = Ingest::default();
        let t0 = Instant::now();
        ing.frame_drawn(t0, t0 + ms(2));
        let parse_start = t0 + ms(2);
        assert!(!ing.should_yield(t0 + ms(10), parse_start));
        assert!(!ing.should_yield(t0 + ms(16), parse_start));
        assert!(ing.should_yield(t0 + ms(17), parse_start));
    }

    #[test]
    fn slow_frames_get_at_least_as_much_parse_time() {
        let mut ing = Ingest::default();
        let t0 = Instant::now();
        // A 30 ms frame: one is due as soon as it ends, but parse for as
        // long as it took first.
        ing.frame_drawn(t0, t0 + ms(30));
        let parse_start = t0 + ms(30);
        assert!(!ing.should_yield(t0 + ms(40), parse_start));
        // Capped at one frame interval.
        assert!(ing.should_yield(parse_start + FRAME_INTERVAL, parse_start));
    }

    #[test]
    fn waits_for_a_requested_frame_only_while_one_is_due_and_pending() {
        let mut ing = Ingest::default();
        let t0 = Instant::now();
        ing.frame_drawn(t0, t0 + ms(1));
        assert!(!ing.should_wait_for_frame(t0 + ms(20)), "nothing requested");
        ing.frame_requested(t0 + ms(5));
        assert!(!ing.should_wait_for_frame(t0 + ms(6)), "not due yet");
        assert!(ing.should_wait_for_frame(t0 + ms(20)));
        // A redraw that never comes stops holding parsing up.
        assert!(!ing.should_wait_for_frame(t0 + ms(5) + MAX_FRAME_WAIT));
        ing.redraw_handled();
        assert!(!ing.should_wait_for_frame(t0 + ms(20)));
    }

    #[test]
    fn frame_requested_keeps_the_earliest_request() {
        let mut ing = Ingest::default();
        let t0 = Instant::now();
        ing.frame_requested(t0);
        ing.frame_requested(t0 + ms(50));
        assert!(!ing.should_wait_for_frame(t0 + MAX_FRAME_WAIT));
    }

    #[test]
    fn slice_to_esu_ends_after_the_first_esu() {
        let s = b"\x1b[?2026hAAA\x1b[?2026l\x1b[?2026hBBB\x1b[?2026l";
        let n = slice_to_esu(s);
        assert_eq!(&s[..n], b"\x1b[?2026hAAA\x1b[?2026l");
        assert_eq!(slice_to_esu(b"no esu here"), 11);
        assert_eq!(slice_to_esu(b""), 0);
    }
}
