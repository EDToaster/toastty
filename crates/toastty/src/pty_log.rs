//! Optional PTY byte-stream logger for debugging app↔toastty traffic.
//!
//! Enabled by setting `TOASTTY_PTY_LOG=<path>` in the environment
//! toastty is launched in. Both directions are recorded:
//!
//! - `→ app`: bytes toastty writes to the PTY master. Includes
//!   keyboard input forwarded to the running shell, plus replies
//!   synthesised by the term (DA1, OSC 4/11 queries, OSC 52
//!   clipboard, kitty graphics OK / error, …).
//! - `← app`: bytes toastty reads from the PTY master, i.e. the raw
//!   stream the parser ingests (shell output, app draw commands,
//!   query probes).
//!
//! Each chunk is logged on its own line with a relative timestamp:
//!
//! ```text
//! [+0.012s ← 38B] \x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\
//! [+0.014s → 13B] \x1b_Gi=31;OK\x1b\\
//! ```
//!
//! Non-printable bytes render as `\xNN`; printable ASCII passes
//! through. The format is grep-friendly: `grep '\\x1b_G' toastty.log`
//! pulls every kitty graphics APC seen in either direction.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::time::Instant;

/// Direction of a PTY chunk. The arrows mirror the convention the
/// rest of the codebase uses (`→` toward the app; `←` from the app).
#[derive(Debug, Clone, Copy)]
pub enum Direction {
    ToApp,
    FromApp,
}

impl Direction {
    fn arrow(self) -> &'static str {
        match self {
            Self::ToApp => "→",
            Self::FromApp => "←",
        }
    }
}

/// Opens a log file at the path in `TOASTTY_PTY_LOG` (if set) and
/// writes a header line. Returns `None` when the env var is absent or
/// the file can't be opened — disabling logging is silent and free.
pub struct PtyLogger {
    sink: Option<BufWriter<File>>,
    started_at: Instant,
}

impl std::fmt::Debug for PtyLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyLogger")
            .field("enabled", &self.sink.is_some())
            .finish()
    }
}

impl PtyLogger {
    pub fn from_env() -> Self {
        let path = std::env::var_os("TOASTTY_PTY_LOG");
        let sink = path
            .as_ref()
            .and_then(|p| File::create(Path::new(p)).ok())
            .map(BufWriter::new);
        let mut me = Self {
            sink,
            started_at: Instant::now(),
        };
        if let Some(p) = &path {
            tracing::info!(target: "pty_log", "writing PTY byte log to {p:?}");
        }
        // Write a header so the user knows which way the arrows point.
        if let Some(w) = me.sink.as_mut() {
            let _ = writeln!(
                w,
                "# toastty PTY log — `→` is bytes sent to the app, `←` is bytes received from the app"
            );
            let _ = w.flush();
        }
        me
    }

    /// Append a chunk to the log. Cheap when logging is off (one
    /// `Option::is_some` check).
    pub fn log(&mut self, dir: Direction, bytes: &[u8]) {
        let Some(sink) = self.sink.as_mut() else {
            return;
        };
        if bytes.is_empty() {
            return;
        }
        let elapsed = self.started_at.elapsed().as_secs_f64();
        // ASCII passthrough; ESC and other non-printables → \xNN. We
        // deliberately don't unwrap escape sequences across chunks —
        // the timing matters for debugging timeouts, so each PTY
        // read/write stays on its own line.
        let mut rendered = String::with_capacity(bytes.len() * 2);
        for &b in bytes {
            if (0x20..0x7f).contains(&b) || b == b'\n' || b == b'\t' {
                if b == b'\n' {
                    rendered.push_str("\\n");
                } else if b == b'\t' {
                    rendered.push_str("\\t");
                } else {
                    rendered.push(b as char);
                }
            } else {
                use std::fmt::Write as _;
                let _ = write!(rendered, "\\x{b:02x}");
            }
        }
        let _ = writeln!(
            sink,
            "[+{:.3}s {} {}B] {}",
            elapsed,
            dir.arrow(),
            bytes.len(),
            rendered,
        );
        // Flush eagerly so a crash or hang doesn't lose the tail of
        // the trace.
        let _ = sink.flush();
    }
}
