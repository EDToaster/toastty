//! DECSET 2026 — synchronized output (BSU/ESU).
//!
//! Apps wrap a batch of cell updates in `CSI ? 2026 h` (Begin
//! Synchronized Update) / `CSI ? 2026 l` (End Synchronized Update). The
//! renderer must not display the partial state between them. If ESU
//! doesn't arrive within [`BSU_TIMEOUT`], the binary's watchdog
//! force-flushes the pause and the next post-flush frame issues a
//! corrective full redraw (decision #7).
//!
//! This module is intentionally pure — it owns no state. The active
//! flag and its wall-clock timestamp live on `toastty_term::Term`; the
//! helpers here just answer "did the timer expire?" and "what bytes do
//! we send as a DECRPM ($p) reply for mode 2026?".
//!
//! TODO(M9): wire `encode_decrpm_reply` into the CSI dispatcher's
//! DECRQM (`CSI ? Pm $ p`) query handler. It's tested locally here
//! (so it doesn't trip dead-code warnings), but no parser path
//! consumes the result yet.

use std::time::{Duration, Instant};

/// BSU → ESU watchdog timeout. One second matches tmux's default and
/// the value used by the toastty M2 prototype. After this elapses
/// without an ESU the binary force-flushes the pause.
pub const BSU_TIMEOUT: Duration = Duration::from_millis(1000);

/// True when at least [`BSU_TIMEOUT`] has elapsed since `started_at`.
/// The watchdog calls this once per PTY-bytes batch; when it returns
/// true the binary calls
/// `Term::force_flush_sync_output` and requests an immediate redraw.
#[must_use]
pub fn should_force_flush(started_at: Instant, now: Instant) -> bool {
    now.saturating_duration_since(started_at) >= BSU_TIMEOUT
}

/// Encode a DECRPM ("Report Mode" — `CSI ? Pm $ p`) reply for mode
/// 2026. Returns `"1"` (set) or `"2"` (reset). DEC defines mode replies
/// as `1 = set`, `2 = reset`, `3 = permanently set`, `4 = permanently
/// reset`. We never report 3 or 4 for 2026 — it's a runtime mode.
///
/// The returned bytes are JUST the digit; callers wrap them in the
/// full `CSI ? 2026 ; <digit> $ y` envelope. The binary doesn't ship a
/// DECRQM handler yet (see plan §gotchas), but this helper is in place
/// for when it does.
#[must_use]
pub fn encode_decrpm_reply(active: bool) -> Vec<u8> {
    if active { b"1".to_vec() } else { b"2".to_vec() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bsu_timeout_one_second() {
        assert_eq!(BSU_TIMEOUT, Duration::from_secs(1));
    }

    #[test]
    fn should_force_flush_at_exactly_one_second_is_true() {
        let start = Instant::now();
        // `now = start + BSU_TIMEOUT` is the boundary case; the
        // comparison is `>=` so this is true.
        let now = start + BSU_TIMEOUT;
        assert!(should_force_flush(start, now));
    }

    #[test]
    fn should_force_flush_below_timeout_is_false() {
        let start = Instant::now();
        let now = start + Duration::from_millis(500);
        assert!(!should_force_flush(start, now));
    }

    #[test]
    fn should_force_flush_handles_now_before_start() {
        // Pathological: clock readings reordered (shouldn't happen on
        // a monotonic clock, but the saturating sub keeps us safe).
        let start = Instant::now();
        assert!(!should_force_flush(start, start));
    }

    #[test]
    fn encode_decrpm_set_or_reset() {
        assert_eq!(encode_decrpm_reply(true), b"1");
        assert_eq!(encode_decrpm_reply(false), b"2");
    }

    /// Stuck-BSU regression (followup C1): when BSU goes high and then
    /// the app emits nothing for >= BSU_TIMEOUT, the watchdog predicate
    /// must report "force flush" so the idle-side caller
    /// (Toastty::event's Event::Redraw arm, fired by the
    /// `ControlSignal::RedrawIn(BSU_TIMEOUT)` wake-up) flips the pause.
    /// Pre-C1, the only callsite was handle_pty_bytes — which is never
    /// re-entered while the app is silent, so the watchdog never fired.
    #[test]
    fn should_force_flush_reports_true_after_timeout_with_no_further_input() {
        let started_at = Instant::now();
        // The watchdog wake-up is scheduled at exactly BSU_TIMEOUT; the
        // OS-level coalescing means we land at-or-after that point.
        let wake = started_at + BSU_TIMEOUT;
        assert!(should_force_flush(started_at, wake));
        // A few milliseconds late (typical OS jitter) is still true.
        let wake_late = started_at + BSU_TIMEOUT + Duration::from_millis(7);
        assert!(should_force_flush(started_at, wake_late));
        // And a few milliseconds early (timer fired ahead of schedule —
        // not expected, but be explicit about the >= boundary) is false.
        let wake_early = started_at + BSU_TIMEOUT - Duration::from_millis(1);
        assert!(!should_force_flush(started_at, wake_early));
    }
}
