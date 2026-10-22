//! Performance counters for benchmarking, enabled by
//! `TOASTTY_STATS=<path>`.
//!
//! When the variable is unset every hook is a branch on `None` and
//! nothing is recorded. When it is set, the binary counts frames, PTY
//! bytes and the time spent parsing and rendering, plus two latencies:
//!
//! - **output → frame**: from the first PTY batch after a frame to the
//!   end of the frame that shows it;
//! - **key → frame**: from a key press to the end of the first frame
//!   that follows the app's reply (the echo).
//!
//! [`PerfStats::write`] dumps everything as one JSON object when the
//! PTY closes. `bench/run.py` reads it.

use std::fmt::Write as _;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[derive(Debug)]
struct Inner {
    path: std::path::PathBuf,
    start: Instant,
    start_unix: Duration,
    first_frame: Option<Duration>,
    first_bytes: Option<Duration>,
    frames: u64,
    skipped: u64,
    frame_us: Vec<u32>,
    pty_bytes: u64,
    pty_events: u64,
    parse: Duration,
    output_pending: Option<Instant>,
    output_latency_us: Vec<u32>,
    key_pending: Option<Instant>,
    key_echoed: bool,
    key_latency_us: Vec<u32>,
}

/// See the module docs. `Default` is the disabled state.
#[derive(Debug, Default)]
pub struct PerfStats(Option<Box<Inner>>);

impl PerfStats {
    /// Enabled when `TOASTTY_STATS` names an output path.
    pub fn from_env() -> Self {
        let Some(path) = std::env::var_os("TOASTTY_STATS") else {
            return Self(None);
        };
        Self(Some(Box::new(Inner {
            path: path.into(),
            start: Instant::now(),
            start_unix: SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default(),
            first_frame: None,
            first_bytes: None,
            frames: 0,
            skipped: 0,
            frame_us: Vec::new(),
            pty_bytes: 0,
            pty_events: 0,
            parse: Duration::ZERO,
            output_pending: None,
            output_latency_us: Vec::new(),
            key_pending: None,
            key_echoed: false,
            key_latency_us: Vec::new(),
        })))
    }

    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// A PTY batch of `len` bytes arrived at `at` and took `parse` to
    /// handle.
    pub fn pty_batch(&mut self, at: Instant, len: usize, parse: Duration) {
        let Some(s) = self.0.as_mut() else { return };
        s.first_bytes.get_or_insert(at - s.start);
        s.pty_bytes += len as u64;
        s.pty_events += 1;
        s.parse += parse;
        s.output_pending.get_or_insert(at);
        if s.key_pending.is_some() {
            s.key_echoed = true;
        }
    }

    pub fn key_press(&mut self, at: Instant) {
        let Some(s) = self.0.as_mut() else { return };
        if s.key_pending.is_none() {
            s.key_pending = Some(at);
            s.key_echoed = false;
        }
    }

    /// A frame was rendered between `start` and `end`.
    pub fn frame(&mut self, start: Instant, end: Instant) {
        let Some(s) = self.0.as_mut() else { return };
        s.first_frame.get_or_insert(end - s.start);
        s.frames += 1;
        s.frame_us.push(micros(end - start));
        if let Some(at) = s.output_pending.take() {
            s.output_latency_us.push(micros(end - at));
        }
        if s.key_echoed
            && let Some(at) = s.key_pending.take()
        {
            s.key_latency_us.push(micros(end - at));
            s.key_echoed = false;
        }
    }

    pub fn skipped(&mut self) {
        if let Some(s) = self.0.as_mut() {
            s.skipped += 1;
        }
    }

    /// Write the JSON report. Called once, when the PTY closes.
    pub fn write(&mut self) {
        let Some(s) = self.0.as_mut() else { return };
        let end = s.start.elapsed();
        let (utime, stime) = self_cpu_ticks();
        let mut o = String::new();
        let _ = write!(
            o,
            "{{\"start_unix_ns\":{},\"elapsed_us\":{},\"first_frame_us\":{},\"first_bytes_us\":{},\
             \"frames\":{},\"skipped\":{},\"pty_bytes\":{},\"pty_events\":{},\"parse_us\":{},\
             \"utime_ticks\":{},\"stime_ticks\":{},\"vm_hwm_kb\":{},\"vm_rss_kb\":{},",
            s.start_unix.as_nanos(),
            micros(end),
            s.first_frame.map_or(-1, |d| i64::from(micros(d))),
            s.first_bytes.map_or(-1, |d| i64::from(micros(d))),
            s.frames,
            s.skipped,
            s.pty_bytes,
            s.pty_events,
            s.parse.as_micros(),
            utime,
            stime,
            status_kb("VmHWM:"),
            status_kb("VmRSS:"),
        );
        push_list(&mut o, "frame_us", &s.frame_us);
        o.push(',');
        push_list(&mut o, "output_latency_us", &s.output_latency_us);
        o.push(',');
        push_list(&mut o, "key_latency_us", &s.key_latency_us);
        o.push('}');
        if let Err(e) = std::fs::write(&s.path, o) {
            tracing::warn!("TOASTTY_STATS: write {}: {e}", s.path.display());
        }
    }
}

fn micros(d: Duration) -> u32 {
    u32::try_from(d.as_micros()).unwrap_or(u32::MAX)
}

fn push_list(o: &mut String, name: &str, v: &[u32]) {
    let _ = write!(o, "\"{name}\":[");
    for (i, x) in v.iter().enumerate() {
        if i > 0 {
            o.push(',');
        }
        let _ = write!(o, "{x}");
    }
    o.push(']');
}

/// `utime` and `stime` of this process (all threads), in clock ticks.
fn self_cpu_ticks() -> (u64, u64) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0, 0);
    };
    // Fields after the `)` that closes `comm`: state is field 3, utime 14.
    let rest = stat.rsplit_once(')').map_or("", |(_, r)| r);
    let f: Vec<&str> = rest.split_whitespace().collect();
    let get = |i: usize| f.get(i).and_then(|x| x.parse().ok()).unwrap_or(0);
    (get(11), get(12))
}

fn status_kb(key: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with(key))
                .and_then(|l| l.split_whitespace().nth(1)?.parse().ok())
        })
        .unwrap_or(0)
}

#[cfg(feature = "pprof")]
thread_local! {
    static PROFILER: std::cell::RefCell<Option<(std::path::PathBuf, pprof::ProfilerGuard<'static>)>> =
        const { std::cell::RefCell::new(None) };
}

/// Start the in-process sampling profiler when built with
/// `--features pprof` and `TOASTTY_PPROF=<path>` is set. The folded
/// stacks (`thread;root;...;leaf count`) are written by
/// [`finish_profiler`].
pub fn start_profiler() {
    #[cfg(feature = "pprof")]
    if let Some(path) = std::env::var_os("TOASTTY_PPROF") {
        match pprof::ProfilerGuardBuilder::default().frequency(1999).build() {
            Ok(g) => PROFILER.with(|p| *p.borrow_mut() = Some((path.into(), g))),
            Err(e) => tracing::warn!("TOASTTY_PPROF: {e}"),
        }
    }
}

/// Write the profile started by [`start_profiler`], if any.
pub fn finish_profiler() {
    #[cfg(feature = "pprof")]
    PROFILER.with(|p| {
        let Some((path, guard)) = p.borrow_mut().take() else {
            return;
        };
        let Ok(report) = guard.report().build() else { return };
        let mut o = String::new();
        for (frames, count) in &report.data {
            o.push_str(&frames.thread_name);
            for frame in frames.frames.iter().rev() {
                for sym in frame.iter().rev() {
                    o.push(';');
                    o.push_str(&sym.name().replace(';', ":"));
                }
            }
            let _ = writeln!(o, " {count}");
        }
        let _ = std::fs::write(path, o);
    });
}
