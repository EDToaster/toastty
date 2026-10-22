//! CPU-side state behind the post uniforms: the shader clock, pointer,
//! focus, bells and cursor history, and the animation schedule.
//!
//! No GPU and no `Term` here (the renderer gathers a [`FrameInputs`]
//! from those), so the timing rules are unit tested with synthetic
//! instants.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use super::compile::ShaderInputs;
use super::uniforms::{
    EMPTY_BELL, NEVER, PostUniforms, bell_mask, cursor_mask, focus_mask, mouse_mask, time_mask,
};

/// Interval between time-driven frames (~60 Hz).
pub(crate) const FRAME_INTERVAL: Duration = Duration::from_millis(16);

/// Interval between time-driven frames while the window is occluded: slow
/// polling, so animation resumes on its own once it's visible again.
pub(crate) const OCCLUDED_INTERVAL: Duration = Duration::from_millis(250);

/// Which events open a `TOASTTY_EVENT_DURATION` window for these inputs:
/// `(bell, cursor change, focus gain)`. Each event counts only if the
/// chain reads its uniforms; a chain that reads none of the three groups
/// (a generic "animate after anything" shader) gets all three.
pub(crate) fn window_events(inputs: &ShaderInputs) -> (bool, bool, bool) {
    let bell = inputs.reads(bell_mask());
    let cursor = inputs.reads(cursor_mask());
    let focus = inputs.reads(focus_mask());
    if bell || cursor || focus {
        (bell, cursor, focus)
    } else {
        (true, true, true)
    }
}

/// When post shaders get time-driven frames (`[shader] animation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PostAnimation {
    /// Decide from what the shaders read: shaders that read time animate
    /// while focused, or only for `TOASTTY_EVENT_DURATION` seconds after
    /// an event when they declare it; shaders that don't read time never
    /// get time-driven frames.
    #[default]
    Auto,
    /// Redraw at ~60 Hz all the time, even unfocused.
    Always,
    /// Redraw only on terminal updates and shader-input events.
    Never,
}

/// Cursor as the shaders see it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CursorSnap {
    /// `(left, bottom, width, height)` in surface px.
    pub rect: [f32; 4],
    /// sRGB RGBA.
    pub color: [f32; 4],
    /// `CURSORSTYLE_*`.
    pub style: i32,
}

/// Per-frame inputs the renderer gathers from the terminal and theme.
/// Colours are sRGB-encoded.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FrameInputs<'a> {
    pub resolution: [f32; 2],
    pub cursor: CursorSnap,
    pub cursor_visible: bool,
    pub cell_size: [f32; 2],
    pub grid_rect: [f32; 4],
    pub background: [f32; 3],
    pub foreground: [f32; 3],
    pub cursor_color: [f32; 3],
    pub cursor_text: [f32; 3],
    pub selection_background: [f32; 3],
    pub selection_foreground: [f32; 3],
    /// The palette, when it changed since the last frame.
    pub palette: Option<&'a [[f32; 3]; 256]>,
}

#[derive(Debug, Clone, Copy)]
struct Bell {
    at: Instant,
    /// Centre of the bell cell, surface px.
    center: [f32; 2],
}

/// `iMouse`, Shadertoy semantics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Mouse {
    /// Pointer position while button 1 is held; the last held position
    /// otherwise.
    pos: [f32; 2],
    /// Where button 1 was last pressed.
    click: [f32; 2],
    down: bool,
    /// The press hasn't been shown in a frame yet.
    pressed: bool,
}

impl Mouse {
    fn uniform(&self) -> [f32; 4] {
        [
            self.pos[0],
            self.pos[1],
            if self.down {
                self.click[0]
            } else {
                -self.click[0]
            },
            if self.pressed {
                self.click[1]
            } else {
                -self.click[1]
            },
        ]
    }
}

/// The local zone's offset from UTC for `iDate`, looked up off the
/// render thread: `date +%z` runs on a helper thread and publishes the
/// result through atomics, refreshed at the next local hour boundary (DST
/// changes happen on the hour). Until the first lookup lands, and when
/// `date` is missing, the offset is 0 (UTC).
///
/// `date` rather than `localtime_r` because the crate forbids `unsafe`,
/// and the `time` crate refuses to read the offset in a multi-threaded
/// process.
#[derive(Debug, Default, Clone)]
struct LocalOffset(Arc<OffsetCell>);

#[derive(Debug, Default)]
struct OffsetCell {
    seconds: AtomicI64,
    /// Unix time of the next lookup (0: now).
    refresh_at: AtomicU64,
    in_flight: AtomicBool,
}

impl LocalOffset {
    /// The offset in seconds east of UTC, starting a lookup when one is
    /// due. Never blocks.
    fn get(&self, unix_secs: u64) -> i64 {
        let cell = &self.0;
        if unix_secs >= cell.refresh_at.load(Ordering::Relaxed)
            && !cell.in_flight.swap(true, Ordering::AcqRel)
        {
            let shared = Arc::clone(cell);
            let spawned = std::thread::Builder::new()
                .name("toastty-utc-offset".into())
                .spawn(move || {
                    let seconds = local_utc_offset().unwrap_or_else(|| {
                        static LOGGED: AtomicBool = AtomicBool::new(false);
                        if !LOGGED.swap(true, Ordering::Relaxed) {
                            tracing::debug!("iDate: `date +%z` failed; using UTC");
                        }
                        0
                    });
                    shared.seconds.store(seconds, Ordering::Relaxed);
                    let now = unix_now().as_secs();
                    shared
                        .refresh_at
                        .store(next_local_hour(now, seconds), Ordering::Relaxed);
                    shared.in_flight.store(false, Ordering::Release);
                });
            if spawned.is_err() {
                cell.refresh_at
                    .store(next_local_hour(unix_secs, 0), Ordering::Relaxed);
                cell.in_flight.store(false, Ordering::Release);
            }
        }
        cell.seconds.load(Ordering::Relaxed)
    }
}

/// Unix time of the first local hour boundary after `unix_secs` in a zone
/// `offset` seconds east of UTC.
fn next_local_hour(unix_secs: u64, offset: i64) -> u64 {
    let local = i64::try_from(unix_secs).unwrap_or(i64::MAX - 7200) + offset;
    let next = (local.div_euclid(3600) + 1) * 3600 - offset;
    u64::try_from(next).unwrap_or(0)
}

fn unix_now() -> Duration {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
}

/// The post-process state that outlives a chain (the clock keeps running
/// across hot reloads).
#[derive(Debug)]
pub(crate) struct PostState {
    /// First post-processed frame: `iTime` zero.
    start: Option<Instant>,
    last_frame: Option<Instant>,
    frame: i32,
    frame_rate: f32,
    focused: bool,
    focus_gained_at: Option<Instant>,
    mouse: Mouse,
    /// A shader-input event the chain reads happened since the last frame.
    event_pending: bool,
    /// `Term::bell_revision` last seen; `None` until the first frame.
    bell_seen: Option<u32>,
    /// Newest first.
    bells: [Option<Bell>; 4],
    bell_count: u32,
    bell_cursor: [f32; 4],
    cursor: Option<CursorSnap>,
    previous_cursor: Option<CursorSnap>,
    cursor_changed_at: Option<Instant>,
    local_offset: LocalOffset,
    /// The last frame couldn't get a swapchain image because the window
    /// is occluded: time-driven frames slow to [`OCCLUDED_INTERVAL`].
    occluded: bool,
    uniforms: PostUniforms,
}

impl Default for PostState {
    fn default() -> Self {
        Self {
            start: None,
            last_frame: None,
            frame: 0,
            frame_rate: 0.0,
            focused: true,
            focus_gained_at: None,
            mouse: Mouse::default(),
            event_pending: false,
            bell_seen: None,
            bells: [None; 4],
            bell_count: 0,
            bell_cursor: [0.0; 4],
            cursor: None,
            previous_cursor: None,
            cursor_changed_at: None,
            local_offset: LocalOffset::default(),
            occluded: false,
            uniforms: PostUniforms::default(),
        }
    }
}

impl PostState {
    /// `iTime` at `t`.
    fn time_at(&self, t: Instant) -> f32 {
        self.start
            .map_or(0.0, |s| t.saturating_duration_since(s).as_secs_f32())
    }

    /// Window focus changed (or was re-reported).
    pub(crate) fn set_focused(
        &mut self,
        focused: bool,
        now: Instant,
        inputs: &ShaderInputs,
        mode: PostAnimation,
    ) {
        if focused == self.focused {
            return;
        }
        self.focused = focused;
        if focused {
            self.focus_gained_at = Some(now);
        }
        // Under `auto`, focus also switches time-driven frames on and off
        // (no duration), or opens a duration window (if focus counts).
        let schedule = mode == PostAnimation::Auto
            && inputs.reads(time_mask())
            && (inputs.event_duration.is_none() || (focused && window_events(inputs).2));
        if inputs.reads(focus_mask()) || schedule {
            self.event_pending = true;
        }
    }

    /// Pointer moved, or button 1 changed.
    pub(crate) fn set_pointer(&mut self, pos: (f32, f32), down: bool, inputs: &ShaderInputs) {
        let before = self.mouse.uniform();
        let p = [pos.0, pos.1];
        if down {
            if !self.mouse.down {
                self.mouse.click = p;
                self.mouse.pressed = true;
            }
            self.mouse.pos = p;
        }
        self.mouse.down = down;
        #[allow(clippy::float_cmp)] // Any change at all is a new value.
        let changed = self.mouse.uniform() != before;
        if inputs.reads(mouse_mask()) && changed {
            self.event_pending = true;
        }
    }

    /// Feed the terminal's bell counter. `cell` gives the bell cell as
    /// `(left, bottom, width, height)` px (only called when there is a
    /// new bell). Returns `true` on a new bell.
    pub(crate) fn observe_bell(
        &mut self,
        revision: u32,
        cell: impl FnOnce() -> [f32; 4],
        now: Instant,
        inputs: &ShaderInputs,
    ) -> bool {
        let Some(seen) = self.bell_seen.replace(revision) else {
            // First look: bells from before post-processing don't count.
            return false;
        };
        if seen == revision {
            return false;
        }
        self.bell_count = self.bell_count.wrapping_add(revision.wrapping_sub(seen));
        let rect = cell();
        self.bell_cursor = rect;
        self.bells.rotate_right(1);
        self.bells[0] = Some(Bell {
            at: now,
            center: [rect[0] + rect[2] * 0.5, rect[1] - rect[3] * 0.5],
        });
        if inputs.reads(super::uniforms::bell_mask()) {
            self.event_pending = true;
        }
        true
    }

    /// Forget the bell counter (the chain was switched off, so bells
    /// stop being observed).
    pub(crate) fn reset_bell_tracking(&mut self) {
        self.bell_seen = None;
    }

    /// Force the next frame through (a mode or chain change).
    pub(crate) fn request_frame(&mut self) {
        self.event_pending = true;
    }

    /// Latest event that starts an event-duration animation for a chain
    /// with these inputs (see [`window_events`]).
    fn last_event(&self, inputs: &ShaderInputs) -> Option<Instant> {
        let (bell, cursor, focus) = window_events(inputs);
        [
            self.bells[0].map(|b| b.at).filter(|_| bell),
            self.cursor_changed_at.filter(|_| cursor),
            self.focus_gained_at.filter(|_| focus),
        ]
        .into_iter()
        .flatten()
        .max()
    }

    /// Time until the chain wants its next frame, or `None` when it
    /// needs none (idle). Zero means now.
    pub(crate) fn next_frame_in(
        &self,
        now: Instant,
        mode: PostAnimation,
        inputs: &ShaderInputs,
    ) -> Option<Duration> {
        if self.event_pending {
            return Some(Duration::ZERO);
        }
        let interval = if self.occluded {
            OCCLUDED_INTERVAL
        } else {
            FRAME_INTERVAL
        };
        let tick = || {
            self.last_frame.map_or(Duration::ZERO, |l| {
                interval.saturating_sub(now.saturating_duration_since(l))
            })
        };
        match mode {
            PostAnimation::Never => None,
            PostAnimation::Always => Some(tick()),
            PostAnimation::Auto if !inputs.reads(time_mask()) => None,
            PostAnimation::Auto => match inputs.event_duration {
                None => self.focused.then(tick),
                Some(d) => {
                    let start = self.last_event(inputs)?;
                    let Some(end) = Duration::try_from_secs_f32(d)
                        .ok()
                        .and_then(|d| start.checked_add(d))
                    else {
                        // Past what `Instant` can hold: never ends.
                        return Some(tick());
                    };
                    if now < end {
                        Some(tick().min(end - now))
                    } else if self.last_frame.is_none_or(|l| l < end) {
                        // One frame at (or after) the end, so the shader
                        // shows its settled state.
                        Some(Duration::ZERO)
                    } else {
                        None
                    }
                }
            },
        }
    }

    /// True when the chain wants a frame now.
    pub(crate) fn frame_due(
        &self,
        now: Instant,
        mode: PostAnimation,
        inputs: &ShaderInputs,
    ) -> bool {
        self.next_frame_in(now, mode, inputs) == Some(Duration::ZERO)
    }

    /// Update the uniforms for a frame at `now` and return them.
    /// `needs_date` skips the wall-clock work when no shader reads
    /// `iDate`.
    pub(crate) fn begin_frame(
        &mut self,
        now: Instant,
        f: &FrameInputs<'_>,
        needs_date: bool,
    ) -> &PostUniforms {
        let start = *self.start.get_or_insert(now);
        let delta = self
            .last_frame
            .map_or(0.0, |l| now.saturating_duration_since(l).as_secs_f32());
        // Idle gaps aren't a frame rate: only smooth over short deltas.
        if delta > 0.0 && delta < 1.0 {
            let rate = 1.0 / delta;
            self.frame_rate = if self.frame_rate > 0.0 {
                self.frame_rate * 0.9 + rate * 0.1
            } else {
                rate
            };
        }

        // Cursor history.
        match self.cursor {
            Some(cur) if cur != f.cursor => {
                self.previous_cursor = Some(cur);
                self.cursor = Some(f.cursor);
                self.cursor_changed_at = Some(now);
            }
            None => self.cursor = Some(f.cursor),
            Some(_) => {}
        }
        let previous = self.previous_cursor.unwrap_or(f.cursor);

        let time = now.saturating_duration_since(start).as_secs_f32();
        let date = if needs_date { self.date() } else { [0.0; 4] };
        let mut bells = [EMPTY_BELL; 4];
        for (slot, bell) in bells.iter_mut().zip(&self.bells) {
            if let Some(b) = bell {
                #[allow(clippy::cast_possible_truncation)]
                let ago = now.saturating_duration_since(b.at).as_secs_f64() as f32;
                *slot = [b.center[0], b.center[1], self.time_at(b.at), ago];
            }
        }
        let time_bell = self.bells[0].map_or(NEVER, |b| self.time_at(b.at));
        let time_cursor_change = self.cursor_changed_at.map_or(NEVER, |t| self.time_at(t));
        let time_focus = self.focus_gained_at.map_or(0.0, |t| self.time_at(t));
        let mouse = self.mouse.uniform();
        let focus = i32::from(self.focused);
        let frame = self.frame;
        let rgb = |c: [f32; 3]| [c[0], c[1], c[2], 0.0];

        let u = &mut self.uniforms;
        u.i_resolution = [f.resolution[0], f.resolution[1], 1.0];
        u.i_time = time;
        u.i_time_delta = delta;
        u.i_frame_rate = self.frame_rate;
        u.i_frame = frame;
        u.i_channel_time = [0.0; 4];
        u.i_channel_resolution = [[0.0; 4]; 4];
        u.i_channel_resolution[0] = [f.resolution[0], f.resolution[1], 1.0, 0.0];
        u.i_mouse = mouse;
        u.i_date = date;
        u.i_sample_rate = 0.0;
        u.i_current_cursor = f.cursor.rect;
        u.i_previous_cursor = previous.rect;
        u.i_current_cursor_color = f.cursor.color;
        u.i_previous_cursor_color = previous.color;
        u.i_current_cursor_style = f.cursor.style;
        u.i_previous_cursor_style = previous.style;
        u.i_cursor_visible = i32::from(f.cursor_visible);
        u.i_time_cursor_change = time_cursor_change;
        u.i_time_focus = time_focus;
        u.i_focus = focus;
        u.i_time_bell = time_bell;
        u.i_bell_cursor = self.bell_cursor;
        u.i_bell_count = self.bell_count as i32;
        u.i_bells = bells;
        u.i_cell_size = f.cell_size;
        u.i_grid_rect = f.grid_rect;
        u.i_background_color = rgb(f.background);
        u.i_foreground_color = rgb(f.foreground);
        u.i_cursor_color = rgb(f.cursor_color);
        u.i_cursor_text = rgb(f.cursor_text);
        u.i_selection_background_color = rgb(f.selection_background);
        u.i_selection_foreground_color = rgb(f.selection_foreground);
        if let Some(p) = f.palette {
            for (dst, src) in u.i_palette.iter_mut().zip(p) {
                *dst = rgb(*src);
            }
        }
        &self.uniforms
    }

    /// A post-processed frame at `now` was submitted.
    pub(crate) fn end_frame(&mut self, now: Instant) {
        self.last_frame = Some(now);
        self.frame = self.frame.wrapping_add(1);
        self.event_pending = false;
        self.mouse.pressed = false;
        self.occluded = false;
    }

    /// A post frame at `now` was wanted but couldn't be drawn (no
    /// swapchain image: occluded, outdated, timed out). Counts as shown for
    /// the schedule, so an animating chain doesn't spin on a hidden window;
    /// the next frame that draws shows the current state anyway. While
    /// `occluded`, time-driven frames slow to [`OCCLUDED_INTERVAL`] and a
    /// pending event frame is dropped (the window is hidden). Otherwise
    /// (timeout, outdated surface) the event frame stays pending, to be
    /// drawn on the next try: those don't repeat without blocking, so
    /// this can't spin.
    pub(crate) fn frame_dropped(&mut self, now: Instant, occluded: bool) {
        self.last_frame = Some(now);
        if occluded {
            self.event_pending = false;
        }
        self.occluded = occluded;
    }

    /// Start the local UTC offset lookup now (a chain that reads `iDate`
    /// was installed), so the first frames aren't shown in UTC.
    pub(crate) fn prepare_date(&self) {
        self.local_offset.get(unix_now().as_secs());
    }

    /// `iDate`: `(year, month - 1, day, seconds since local midnight)`.
    fn date(&self) -> [f32; 4] {
        let since_epoch = unix_now();
        date_uniform(since_epoch, self.local_offset.get(since_epoch.as_secs()))
    }
}

/// `iDate` for a time `since_epoch` (UTC) in a zone `offset` seconds east
/// of UTC.
#[allow(clippy::cast_precision_loss)]
pub(crate) fn date_uniform(since_epoch: Duration, offset: i64) -> [f32; 4] {
    let local = since_epoch.as_secs_f64() + offset as f64;
    let days = (local / 86_400.0).floor();
    let secs = local - days * 86_400.0;
    #[allow(clippy::cast_possible_truncation)]
    let (y, m, d) = civil_from_days(days as i64);
    #[allow(clippy::cast_possible_truncation)]
    [y as f32, (m - 1) as f32, d as f32, secs as f32]
}

/// Proleptic Gregorian `(year, month 1..=12, day 1..=31)` for days since
/// 1970-01-01 (Howard Hinnant's `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The local zone's current offset from UTC in seconds, from `date +%z`
/// (blocks; runs on [`LocalOffset`]'s helper thread).
fn local_utc_offset() -> Option<i64> {
    let out = std::process::Command::new("date")
        .arg("+%z")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    parse_utc_offset(std::str::from_utf8(&out.stdout).ok()?)
}

/// Parse `+hhmm` / `-hhmm`.
fn parse_utc_offset(s: &str) -> Option<i64> {
    let s = s.trim();
    let (sign, digits) = match s.as_bytes().first()? {
        b'+' => (1, &s[1..]),
        b'-' => (-1, &s[1..]),
        _ => return None,
    };
    if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = digits[..2].parse().ok()?;
    let m: i64 = digits[2..].parse().ok()?;
    Some(sign * (h * 3600 + m * 60))
}

#[cfg(test)]
#[allow(clippy::float_cmp, clippy::cast_precision_loss)] // exact test values
mod tests {
    use super::*;
    use crate::post::uniforms::{bell_mask, mask_of};

    fn reads(names: &[&str], duration: Option<f32>) -> ShaderInputs {
        ShaderInputs {
            used: mask_of(names),
            event_duration: duration,
        }
    }

    fn frame_inputs(cursor_x: f32) -> FrameInputs<'static> {
        FrameInputs {
            resolution: [800.0, 600.0],
            cursor: CursorSnap {
                rect: [cursor_x, 20.0, 10.0, 20.0],
                color: [1.0, 1.0, 0.0, 1.0],
                style: 0,
            },
            cursor_visible: true,
            cell_size: [10.0, 20.0],
            grid_rect: [0.0, 0.0, 800.0, 600.0],
            background: [0.0; 3],
            foreground: [1.0; 3],
            cursor_color: [1.0, 1.0, 0.0],
            cursor_text: [0.0; 3],
            selection_background: [0.5; 3],
            selection_foreground: [1.0; 3],
            palette: None,
        }
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn static_shader_never_animates_in_auto() {
        let s = PostState::default();
        let now = Instant::now();
        let inputs = reads(&["iResolution", "iPalette"], None);
        assert_eq!(s.next_frame_in(now, PostAnimation::Auto, &inputs), None);
        assert_eq!(s.next_frame_in(now, PostAnimation::Never, &inputs), None);
        assert!(
            s.next_frame_in(now, PostAnimation::Always, &inputs)
                .is_some()
        );
    }

    #[test]
    fn time_shader_animates_while_focused() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iTime"], None);
        s.begin_frame(t0, &frame_inputs(0.0), false);
        s.end_frame(t0);
        assert_eq!(
            s.next_frame_in(t0 + ms(4), PostAnimation::Auto, &inputs),
            Some(ms(12))
        );
        assert!(s.frame_due(t0 + ms(16), PostAnimation::Auto, &inputs));
        s.set_focused(false, t0 + ms(5), &inputs, PostAnimation::Auto);
        // The focus change itself is one frame...
        assert!(s.frame_due(t0 + ms(5), PostAnimation::Auto, &inputs));
        s.end_frame(t0 + ms(5));
        // ...then idle while unfocused.
        assert_eq!(
            s.next_frame_in(t0 + ms(50), PostAnimation::Auto, &inputs),
            None
        );
        // `always` keeps going unfocused.
        assert!(s.frame_due(t0 + ms(50), PostAnimation::Always, &inputs));
    }

    #[test]
    fn duration_shader_animates_only_after_events() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iTime", "iBells"], Some(1.0));
        s.begin_frame(t0, &frame_inputs(0.0), false);
        s.end_frame(t0);
        // Nothing happened yet: idle.
        assert_eq!(
            s.next_frame_in(t0 + ms(100), PostAnimation::Auto, &inputs),
            None
        );
        // First observation only primes the counter.
        assert!(!s.observe_bell(0, || [0.0; 4], t0, &inputs));
        let bell_at = t0 + ms(200);
        assert!(s.observe_bell(1, || [10.0, 40.0, 10.0, 20.0], bell_at, &inputs));
        assert!(s.frame_due(bell_at, PostAnimation::Auto, &inputs));
        s.begin_frame(bell_at, &frame_inputs(0.0), false);
        s.end_frame(bell_at + ms(985));
        // Ticking inside the window, capped by its end.
        assert_eq!(
            s.next_frame_in(bell_at + ms(990), PostAnimation::Auto, &inputs),
            Some(ms(10))
        );
        // One settling frame past the end, then idle.
        assert!(s.frame_due(bell_at + ms(1000), PostAnimation::Auto, &inputs));
        s.end_frame(bell_at + ms(1001));
        assert_eq!(
            s.next_frame_in(bell_at + ms(1100), PostAnimation::Auto, &inputs),
            None
        );
        // `never` gives the bell frame but no animation.
        assert!(s.observe_bell(2, || [0.0; 4], bell_at + ms(2000), &inputs));
        assert!(s.frame_due(bell_at + ms(2000), PostAnimation::Never, &inputs));
        s.end_frame(bell_at + ms(2000));
        assert_eq!(
            s.next_frame_in(bell_at + ms(2010), PostAnimation::Never, &inputs),
            None
        );
    }

    #[test]
    fn cursor_change_opens_the_duration_window() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iTime", "iTimeCursorChange"], Some(0.5));
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_time_cursor_change, NEVER);
        s.end_frame(t0);
        assert_eq!(
            s.next_frame_in(t0 + ms(20), PostAnimation::Auto, &inputs),
            None
        );
        let t1 = t0 + ms(100);
        let u = s.begin_frame(t1, &frame_inputs(10.0), false);
        assert!((u.i_time_cursor_change - 0.1).abs() < 1e-4);
        assert_eq!(u.i_previous_cursor[0], 0.0);
        assert_eq!(u.i_current_cursor[0], 10.0);
        s.end_frame(t1);
        assert!(s.frame_due(t1 + ms(20), PostAnimation::Auto, &inputs));
    }

    #[test]
    fn events_only_wake_shaders_that_read_them() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let none = reads(&["iResolution"], None);
        s.set_pointer((5.0, 5.0), true, &none);
        s.set_focused(false, t0, &none, PostAnimation::Auto);
        s.observe_bell(0, || [0.0; 4], t0, &none);
        s.observe_bell(1, || [0.0; 4], t0, &none);
        assert!(!s.frame_due(t0, PostAnimation::Auto, &none));

        let mouse = reads(&["iMouse"], None);
        let mut s = PostState::default();
        // Moving without the button doesn't change iMouse.
        s.set_pointer((5.0, 5.0), false, &mouse);
        assert!(!s.frame_due(t0, PostAnimation::Auto, &mouse));
        s.set_pointer((5.0, 5.0), true, &mouse);
        assert!(s.frame_due(t0, PostAnimation::Auto, &mouse));

        let bell = ShaderInputs {
            used: bell_mask(),
            event_duration: None,
        };
        let mut s = PostState::default();
        s.observe_bell(7, || [0.0; 4], t0, &bell);
        s.observe_bell(9, || [0.0; 4], t0, &bell);
        assert!(s.frame_due(t0, PostAnimation::Never, &bell));
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_bell_count, 2);
    }

    #[test]
    fn mouse_follows_shadertoy_semantics() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iMouse"], None);
        s.set_pointer((10.0, 20.0), true, &inputs);
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_mouse, [10.0, 20.0, 10.0, 20.0]);
        s.end_frame(t0);
        s.set_pointer((30.0, 40.0), true, &inputs);
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_mouse, [30.0, 40.0, 10.0, -20.0]);
        s.end_frame(t0);
        s.set_pointer((50.0, 60.0), false, &inputs);
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_mouse, [30.0, 40.0, -10.0, -20.0]);
    }

    #[test]
    fn bells_are_newest_first_with_exact_age() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iBells"], None);
        s.begin_frame(t0, &frame_inputs(0.0), false);
        s.end_frame(t0);
        s.observe_bell(0, || [0.0; 4], t0, &inputs);
        for i in 1..=5u32 {
            let x = i as f32 * 10.0;
            s.observe_bell(
                i,
                || [x, 40.0, 10.0, 20.0],
                t0 + ms(u64::from(i) * 100),
                &inputs,
            );
        }
        let u = *s.begin_frame(t0 + ms(1000), &frame_inputs(0.0), false);
        // Newest (5th) first; centre of the cell.
        assert_eq!(u.i_bells[0][0], 55.0);
        assert_eq!(u.i_bells[0][1], 30.0);
        assert!((u.i_bells[0][2] - 0.5).abs() < 1e-5);
        assert!((u.i_bells[0][3] - 0.5).abs() < 1e-5);
        assert_eq!(u.i_bells[3][0], 25.0);
        assert!((u.i_time_bell - 0.5).abs() < 1e-5);
        assert_eq!(u.i_bell_cursor, [50.0, 40.0, 10.0, 20.0]);
        assert_eq!(u.i_bell_count, 5);
        // Fewer bells than slots: the rest are empty.
        let mut s = PostState::default();
        s.observe_bell(0, || [0.0; 4], t0, &inputs);
        s.observe_bell(1, || [0.0; 4], t0, &inputs);
        let u = s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!(u.i_bells[1], EMPTY_BELL);
    }

    #[test]
    fn clock_and_frame_counter() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let u = *s.begin_frame(t0, &frame_inputs(0.0), false);
        assert_eq!((u.i_time, u.i_time_delta, u.i_frame), (0.0, 0.0, 0));
        assert_eq!(u.i_resolution, [800.0, 600.0, 1.0]);
        assert_eq!(u.i_channel_resolution[0], [800.0, 600.0, 1.0, 0.0]);
        assert_eq!(u.i_time_bell, NEVER);
        s.end_frame(t0);
        let u = *s.begin_frame(t0 + ms(20), &frame_inputs(0.0), false);
        assert!((u.i_time - 0.02).abs() < 1e-6);
        assert!((u.i_time_delta - 0.02).abs() < 1e-6);
        assert!((u.i_frame_rate - 50.0).abs() < 0.01);
        assert_eq!(u.i_frame, 1);
        assert_eq!(u.i_focus, 1);
    }

    #[test]
    fn date_uniform_splits_civil_time() {
        // 2024-02-29 13:30:15 UTC.
        let t = Duration::from_secs(1_709_213_415);
        assert_eq!(date_uniform(t, 0), [2024.0, 1.0, 29.0, 48_615.0]);
        // Two hours east: 15:30:15 the same day.
        assert_eq!(date_uniform(t, 7200), [2024.0, 1.0, 29.0, 55_815.0]);
        // Fourteen hours west: the previous day.
        assert_eq!(
            date_uniform(t, -14 * 3600),
            [2024.0, 1.0, 28.0, 48_615.0 - 14.0 * 3600.0 + 86_400.0]
        );
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn huge_duration_does_not_overflow() {
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iTime", "iBells"], Some(f32::MAX));
        s.observe_bell(0, || [0.0; 4], t0, &inputs);
        s.observe_bell(1, || [0.0; 4], t0, &inputs);
        s.end_frame(t0);
        assert!(
            s.next_frame_in(t0 + ms(1), PostAnimation::Auto, &inputs)
                .is_some()
        );
    }

    #[test]
    fn events_open_windows_only_for_what_the_chain_reads() {
        let t0 = Instant::now();
        // Reads the bells only: a cursor move or focus gain doesn't
        // animate.
        let bells = reads(&["iTime", "iBells"], Some(1.0));
        let mut s = PostState::default();
        s.begin_frame(t0, &frame_inputs(0.0), false);
        s.end_frame(t0);
        s.begin_frame(t0 + ms(10), &frame_inputs(10.0), false); // cursor moved
        s.end_frame(t0 + ms(10));
        s.set_focused(false, t0 + ms(20), &bells, PostAnimation::Auto);
        s.set_focused(true, t0 + ms(30), &bells, PostAnimation::Auto);
        assert_eq!(
            s.next_frame_in(t0 + ms(100), PostAnimation::Auto, &bells),
            None
        );
        s.observe_bell(0, || [0.0; 4], t0, &bells);
        s.observe_bell(1, || [0.0; 4], t0 + ms(200), &bells);
        s.end_frame(t0 + ms(200));
        assert!(
            s.next_frame_in(t0 + ms(210), PostAnimation::Auto, &bells)
                .is_some()
        );

        // Reads the cursor only: a bell doesn't animate, a move does.
        let cursor = reads(&["iTime", "iTimeCursorChange"], Some(1.0));
        let mut s = PostState::default();
        s.begin_frame(t0, &frame_inputs(0.0), false);
        s.end_frame(t0);
        s.observe_bell(0, || [0.0; 4], t0, &cursor);
        s.observe_bell(1, || [0.0; 4], t0 + ms(10), &cursor);
        assert_eq!(
            s.next_frame_in(t0 + ms(20), PostAnimation::Auto, &cursor),
            None
        );
        s.begin_frame(t0 + ms(30), &frame_inputs(10.0), false);
        s.end_frame(t0 + ms(30));
        assert!(
            s.next_frame_in(t0 + ms(40), PostAnimation::Auto, &cursor)
                .is_some()
        );

        // Reads none of the event groups: every event counts.
        let any = reads(&["iTime"], Some(1.0));
        let mut s = PostState::default();
        s.observe_bell(0, || [0.0; 4], t0, &any);
        s.observe_bell(1, || [0.0; 4], t0, &any);
        s.end_frame(t0);
        assert!(
            s.next_frame_in(t0 + ms(10), PostAnimation::Auto, &any)
                .is_some()
        );
        let mut s = PostState::default();
        s.end_frame(t0);
        s.set_focused(false, t0, &any, PostAnimation::Auto);
        s.set_focused(true, t0 + ms(5), &any, PostAnimation::Auto);
        assert!(s.frame_due(t0 + ms(5), PostAnimation::Auto, &any));
    }

    #[test]
    fn dropped_event_frame_is_kept_unless_occluded() {
        let t0 = Instant::now();
        let inputs = reads(&["iMouse"], None);
        let mut s = PostState::default();
        s.set_pointer((1.0, 1.0), true, &inputs);
        s.frame_dropped(t0, false);
        assert!(s.frame_due(t0, PostAnimation::Auto, &inputs));
        s.frame_dropped(t0, true);
        assert!(!s.frame_due(t0, PostAnimation::Auto, &inputs));
    }

    #[test]
    fn prepare_date_starts_the_offset_lookup() {
        let s = PostState::default();
        s.prepare_date();
        // The helper thread schedules the next lookup when it's done
        // (whether or not `date` exists).
        let deadline = Instant::now() + Duration::from_secs(5);
        while s.local_offset.0.refresh_at.load(Ordering::Relaxed) == 0 {
            assert!(Instant::now() < deadline, "lookup never finished");
            std::thread::sleep(ms(5));
        }
    }

    #[test]
    fn occluded_window_polls_slowly() {
        assert_eq!(OCCLUDED_INTERVAL, ms(250));
        let mut s = PostState::default();
        let t0 = Instant::now();
        let inputs = reads(&["iTime"], None);
        s.frame_dropped(t0, true);
        assert_eq!(
            s.next_frame_in(t0 + ms(50), PostAnimation::Always, &inputs),
            Some(ms(200))
        );
        s.end_frame(t0 + ms(300));
        assert_eq!(
            s.next_frame_in(t0 + ms(304), PostAnimation::Always, &inputs),
            Some(ms(12))
        );
    }

    #[test]
    fn next_local_hour_follows_the_zone() {
        // 2024-02-29 13:30:15 UTC.
        let t = 1_709_213_415;
        assert_eq!(next_local_hour(t, 0), 1_709_215_200); // 14:00 UTC
        // +05:30: local 19:00:15, next local hour 20:00 = 14:30 UTC.
        assert_eq!(next_local_hour(t, 5 * 3600 + 1800), 1_709_217_000);
        assert_eq!(next_local_hour(1_709_215_200, 0), 1_709_215_200 + 3600);
    }

    #[test]
    fn utc_offset_parses() {
        assert_eq!(parse_utc_offset("+0530\n"), Some(5 * 3600 + 30 * 60));
        assert_eq!(parse_utc_offset("-0700"), Some(-7 * 3600));
        assert_eq!(parse_utc_offset("Z"), None);
        assert_eq!(parse_utc_offset("+5"), None);
    }
}
