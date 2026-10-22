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
/// progress. The longer of this and twice the last frame's render time
/// is used (capped at [`MAX_MIN_PARSE`]): when frames are slow (a huge
/// window, or a frame full of glyphs new to the atlas) the frame rate
/// drops rather than rendering eating more than a third of the main
/// thread. Frames of up to ~5 ms still go out at 60 Hz.
const MIN_PARSE: Duration = Duration::from_millis(1);

/// Upper bound on the render-scaled parse time, so even very slow frames
/// come at least every ~33 ms of parsing.
const MAX_MIN_PARSE: Duration = Duration::from_micros(33_333);

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
    /// The last slice ended in ESC, so a `_` opening the next one starts
    /// an APC.
    trailing_esc: bool,
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

    /// What the `PtyReady` loop does next, at `now`, in a pass that began
    /// at `parse_start` and has (`any_parsed`) or hasn't parsed a slice yet,
    /// while mode 2026 does (`sync_paused`) or doesn't pause rendering.
    ///
    /// Never stops before the first slice. Besides guaranteeing progress
    /// when the thread was descheduled between the pass starting and this
    /// check, that first slice is what takes the reader's buffer and
    /// re-arms its wake; see [`outcome`] for the other half.
    #[must_use]
    pub fn step(
        &self,
        now: Instant,
        parse_start: Instant,
        any_parsed: bool,
        sync_paused: bool,
    ) -> Step {
        let frame_due = self.should_yield(now, parse_start);
        if !frame_due {
            Step::Parse
        } else if any_parsed && !sync_paused {
            Step::Stop(Stop::Yield)
        } else {
            // Due, but a synchronized update holds the frame back (or
            // nothing is parsed yet): parse to the end of the update.
            Step::ParseToEsu
        }
    }

    /// Whether a frame is due and the pass begun at `parse_start` has
    /// parsed long enough to let it through.
    fn should_yield(&self, now: Instant, parse_start: Instant) -> bool {
        let min_parse = (self.last_render * 2).clamp(MIN_PARSE, MAX_MIN_PARSE);
        self.frame_due(now) && now.saturating_duration_since(parse_start) >= min_parse
    }

    /// Whether `bytes` (the next slice, in stream order) opens an APC:
    /// `ESC _`, possibly split across slices. RGP's `r;path=` registration
    /// is an APC and resolves relative paths against the foreground
    /// process's CWD; probing that CWD costs an ioctl plus a `readlink`,
    /// so `handle_pty_bytes` does it only for slices where this is true.
    pub fn opens_apc(&mut self, bytes: &[u8]) -> bool {
        let Some(&last) = bytes.last() else {
            return false;
        };
        let split = std::mem::replace(&mut self.trailing_esc, last == 0x1B) && bytes[0] == b'_';
        split || memmem::find(bytes, b"\x1b_").is_some()
    }
}

/// The next move of the `PtyReady` parse loop ([`Ingest::step`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step {
    /// Parse the next slice, up to [`SLICE`] bytes.
    Parse,
    /// Parse the next slice only up to the end of its first ESU
    /// ([`slice_to_esu`]).
    ParseToEsu,
    /// Leave the loop with bytes still buffered.
    Stop(Stop),
}

/// Why the parse loop left with bytes still buffered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// To draw a frame: answer `ControlSignal::Yield`.
    Yield,
}

/// What a finished `PtyReady` pass answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The PTY closed and every byte is parsed: reap and exit.
    Closed,
    /// Nothing new to parse (a wake for bytes an earlier pass took).
    Idle,
    /// Redraw, then resume parsing (`ControlSignal::Yield`).
    Yield,
    /// Drained mid synchronized update: wake at the BSU watchdog deadline
    /// rather than redraw a frame `render_term` would skip.
    AwaitEsu,
    /// Drained: redraw now.
    Redraw,
}

/// The answer to a `PtyReady` pass that ended with `stop` (or by draining
/// the reader when `None`).
///
/// A stop must always be answered with one that schedules a resume, even
/// if nothing was parsed: the stop left bytes in the reader, whose wake
/// stays armed until they are taken, so no further `PtyReady` would
/// arrive and output would freeze.
#[must_use]
pub fn outcome(finished: bool, any_parsed: bool, stop: Option<Stop>, sync_paused: bool) -> Outcome {
    match stop {
        _ if finished => Outcome::Closed,
        Some(Stop::Yield) => Outcome::Yield,
        None if !any_parsed => Outcome::Idle,
        None if sync_paused => Outcome::AwaitEsu,
        None => Outcome::Redraw,
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
    fn slow_frames_get_twice_their_time_in_parsing() {
        let mut ing = Ingest::default();
        let t0 = Instant::now();
        // A 10 ms frame: one is due 6.7 ms after it ends, but parse for
        // twice as long as it took first.
        ing.frame_drawn(t0, t0 + ms(10));
        let parse_start = t0 + ms(10);
        assert!(!ing.should_yield(parse_start + ms(19), parse_start));
        assert!(ing.should_yield(parse_start + ms(20), parse_start));
        // Fast frames don't change the 60 Hz cadence.
        ing.frame_drawn(t0, t0 + ms(5));
        assert!(ing.should_yield(t0 + ms(17), t0 + ms(5)));
        // Capped: a 30 ms frame gets 33 ms, not 60.
        ing.frame_drawn(t0, t0 + ms(30));
        let parse_start = t0 + ms(30);
        assert!(!ing.should_yield(parse_start + ms(33), parse_start));
        assert!(ing.should_yield(parse_start + MAX_MIN_PARSE, parse_start));
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
    fn a_frame_due_on_entry_still_parses_a_slice_first() {
        // Regression: the thread was descheduled between the pass
        // starting and the first check, so a frame was already due. The
        // loop stopped without peeking, the pass answered "idle", and the
        // reader, whose wake was still armed, never woke us again.
        let ing = Ingest::default();
        let parse_start = Instant::now();
        let late = parse_start + ms(40);
        assert_eq!(ing.step(late, parse_start, false, false), Step::ParseToEsu);
        assert_eq!(
            ing.step(late, parse_start, true, false),
            Step::Stop(Stop::Yield)
        );
        // And a stop is never answered as idle, parsed or not.
        assert_eq!(
            outcome(false, false, Some(Stop::Yield), false),
            Outcome::Yield
        );
    }

    #[test]
    fn step_parses_to_the_esu_while_the_frame_is_held() {
        let ing = Ingest::default();
        let t0 = Instant::now();
        assert_eq!(ing.step(t0, t0, true, true), Step::Parse);
        assert_eq!(ing.step(t0 + ms(5), t0, true, true), Step::ParseToEsu);
    }

    #[test]
    fn outcome_after_draining() {
        assert_eq!(outcome(true, true, None, false), Outcome::Closed);
        assert_eq!(outcome(true, false, None, true), Outcome::Closed);
        assert_eq!(outcome(false, false, None, false), Outcome::Idle);
        assert_eq!(outcome(false, true, None, true), Outcome::AwaitEsu);
        assert_eq!(outcome(false, true, None, false), Outcome::Redraw);
    }

    #[test]
    fn slice_to_esu_ends_after_the_first_esu() {
        let s = b"\x1b[?2026hAAA\x1b[?2026l\x1b[?2026hBBB\x1b[?2026l";
        let n = slice_to_esu(s);
        assert_eq!(&s[..n], b"\x1b[?2026hAAA\x1b[?2026l");
        assert_eq!(slice_to_esu(b"no esu here"), 11);
        assert_eq!(slice_to_esu(b""), 0);
    }

    #[test]
    fn opens_apc_finds_esc_underscore_even_across_slices() {
        let mut ing = Ingest::default();
        assert!(!ing.opens_apc(b"plain output\r\n"));
        assert!(ing.opens_apc(b"x\x1b_Gr;path=a.glb\x1b\\"));
        // Split between ESC and `_`.
        assert!(!ing.opens_apc(b"abc\x1b"));
        assert!(ing.opens_apc(b"_r;path=a\x1b\\"));
        // ESC at the end of one slice, something else next.
        assert!(!ing.opens_apc(b"\x1b"));
        assert!(!ing.opens_apc(b"[0m"));
        // tmux passthrough doubles the ESC; still contains `ESC _`.
        assert!(ing.opens_apc(b"\x1bPtmux;\x1b\x1b_Ga=q\x1b\x1b\\\x1b\\"));
        assert!(!ing.opens_apc(b""));
    }
}
