//! toastty — lightweight GPU-accelerated terminal emulator.
//!
//! M5 wiring: PTY (`toastty-pty`) + mio reader (`toastty-io`) +
//! parser (`toastty-parser`) + term state (`toastty-term`) + renderer
//! (`toastty-render`) + window (`toastty-window`).
//!
//! The render thread (winit main thread on macOS) owns the parser,
//! `Term`, and `Renderer`. A background mio thread reads bytes from
//! the PTY master and posts them via `EventLoopProxy::send_event` as
//! `Event::PtyBytes(...)`; the binary feeds those to the parser and
//! requests a redraw.

#![deny(unsafe_op_in_unsafe_fn)]

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use pollster::block_on;
use toastty_config::{Config, ConfigError, ConfigSource};
use toastty_parser::Parser;
use toastty_protocols::resize_inband::encode_resize_report;
use toastty_protocols::synchronized::{BSU_TIMEOUT, should_force_flush};
use toastty_pty::{Pty, PtySpec, WinSize};
use toastty_render::{RenderOutcome, Renderer};
use toastty_term::{ClipboardRequest, Pos, SecurityFlags, Selection, SelectionMode, Smoothing, Term};
use toastty_window::{
    App, ControlSignal, Event, KeyState, LogicalKey, Modifiers, MouseButton, ToasttyWindow,
    WindowHandle, WindowOptions, run,
};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use toastty::cli;
use toastty::cli::CommandOverride;
use toastty::config_watcher::ConfigWatcher;
use toastty::focus::encode_focus;
use toastty::geometry::{effective_font_size_px, grid_dims_from_pixels};
use toastty::keyboard::encode_key;
use toastty::mouse::{
    MouseEventKind, classify_button_event, encode_mouse, pixel_to_cell, protocol_wants_event,
    word_bounds_in_row,
};
use toastty::paste::wrap_for_paste;
use toastty::pty_log::{Direction, PtyLogger};
use toastty::shell::resolve_command;
use toastty::theme_bridge::theme_from_config;

/// Target wake-up cadence while the scrollback viewport is animating.
/// 16 ms ≈ 60 Hz — fast enough to feel smooth on macOS trackpad
/// inertia frames without burning the event loop when idle.
const VIEWPORT_ANIM_TICK: Duration = Duration::from_millis(16);

/// Refresh cadence for the `TOASTTY_DEBUG` FPS overlay. Keeps the text
/// visibly updating when the grid is idle (otherwise the renderer would
/// short-circuit to skip-submit and the displayed FPS would freeze).
const DEBUG_OVERLAY_TICK: Duration = Duration::from_millis(250);

/// General lull threshold for the inertia-swallow heuristic. A
/// `ScrollKind::Pixels` event arriving more than this long after the
/// previous one is treated as the start of a fresh stream regardless
/// of phase — covers platforms that don't emit reliable phase
/// information for mouse-wheel events. See
/// [`Toastty::handle_scroll`].
const SCROLL_STREAM_LULL: Duration = Duration::from_millis(150);

/// Smaller lull threshold gating the `ScrollPhase::Started`-as-fresh-
/// gesture signal. macOS's momentum-begin `Started` follows the
/// gesture's `Ended` within one frame (~16 ms); a real user
/// re-engaging the trackpad takes much longer than 50 ms. So a
/// `Started` with at least this much gap from the previous event is
/// almost certainly a fresh gesture and the swallow flag can be
/// cleared faster than waiting out the full [`SCROLL_STREAM_LULL`].
const FRESH_STARTED_GAP: Duration = Duration::from_millis(50);

/// Maximum time between consecutive left-clicks for them to count as a
/// multi-click (double = 2, triple = 3). 350 ms is a sweet spot — fast
/// enough that an intentional double-click is reliable, slow enough to
/// be forgiving for a relaxed triple-click.
const MULTI_CLICK_WINDOW: Duration = Duration::from_millis(350);

/// In-progress drag-select. Persisted between `handle_mouse` press and
/// `handle_mouse_motion` so motion knows which mode/anchor to extend.
#[derive(Debug, Clone, Copy)]
struct DragState {
    /// Selection mode the drag is operating in.
    mode: SelectionMode,
    /// The original press position. For `Char`/`Block` this is the
    /// selection anchor directly. For `Word`/`Line` the visible
    /// selection extends from the word/line *containing* this point;
    /// motion expands by re-evaluating boundaries against the new cell.
    origin: Pos,
}

/// Convert a config `SmoothingFunction` into the per-frame easing
/// the term consumes. Tuning constants are local to the binary so
/// changes don't churn the term crate's public API.
fn smoothing_from_config(cfg: &Config) -> Smoothing {
    if !cfg.scrollback.smooth_scrolling {
        return Smoothing::Instant;
    }
    match cfg.scrollback.smoothing_function {
        toastty_config::SmoothingFunction::Instant => Smoothing::Instant,
        toastty_config::SmoothingFunction::Linear => Smoothing::Linear {
            pixels_per_sec: 600.0,
        },
        toastty_config::SmoothingFunction::EaseOut => Smoothing::EaseOut {
            duration_sec: 0.25,
        },
        toastty_config::SmoothingFunction::ExpDecay => Smoothing::ExpDecay {
            halflife_sec: 0.08,
        },
    }
}

/// Take the shorter of two optional deadlines. `None` means "no
/// deadline". Used when combining the cursor-blink wake with the
/// viewport-animation tick.
fn min_deadline(a: Option<Duration>, b: Option<Duration>) -> Option<Duration> {
    match (a, b) {
        (None, b) => b,
        (a, None) => a,
        (Some(x), Some(y)) => Some(x.min(y)),
    }
}

/// Initial working directory to hand the spawned shell.
///
/// macOS LaunchServices (Spotlight / Launchpad / Finder) starts apps with
/// CWD `/`, which leaves the shell stranded at root. Detect that case and
/// fall back to `$HOME`; when launched from another shell, inherit its CWD.
fn initial_cwd() -> std::path::PathBuf {
    let cwd = std::env::current_dir().ok();
    let launched_from_root = cwd.as_deref().is_some_and(|p| p == std::path::Path::new("/"));
    if !launched_from_root
        && let Some(p) = cwd
    {
        return p;
    }
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("/"))
}

fn main() -> Result<()> {
    // Parse args BEFORE tracing init — otherwise --print-default-config
    // would have log lines interleaved with the TOML we want to write
    // straight into a file.
    let action = match cli::parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\n\n{}", cli::help_text());
            std::process::exit(2);
        }
    };
    let command_override = match action {
        cli::Action::PrintHelp => {
            print!("{}", cli::help_text());
            return Ok(());
        }
        cli::Action::PrintVersion => {
            println!("{}", cli::version_text());
            return Ok(());
        }
        cli::Action::PrintDefaultConfig => {
            cli::write_default_config(&mut std::io::stdout())
                .context("write default config")?;
            return Ok(());
        }
        cli::Action::Run { command } => command,
    };

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("toastty {} starting", env!("CARGO_PKG_VERSION"));

    let (config, source, load_err) = Config::load_default_with_error();
    info!("config source: {source}");
    if matches!(source, ConfigSource::Defaults) {
        debug!("running with built-in defaults");
    }
    if let Some(ref e) = load_err {
        warn!("config load failed, using defaults: {e}");
    }

    let initial_size = (config.window.width.max(1), config.window.height.max(1));
    let transparent = config.theme.bg.as_array()[3] < 1.0;
    if transparent {
        info!("transparent background enabled (theme bg alpha < 1.0)");
    }
    let opts = WindowOptions {
        title: "toastty".into(),
        size: initial_size,
        ime: true,
        transparent,
    };

    let app = Toastty::new(config, command_override, initial_size, load_err);
    run(opts, app).context("window run")?;
    Ok(())
}

/// Running state of the terminal. All initialisation that needs a window
/// handle happens inside `App::init`.
struct Toastty {
    config: Config,
    /// CLI command override. Consumed by `init_impl` when spawning the
    /// PTY; `None` falls back to the configured shell.
    command_override: Option<CommandOverride>,
    window: Option<ToasttyWindow>,
    renderer: Option<Renderer>,
    parser: Parser,
    term: Term,
    pty: Option<Pty>,
    /// `JoinHandle` for the mio reader; held so we don't drop it. The
    /// thread exits on its own when the PTY closes or the proxy goes away.
    reader: Option<std::thread::JoinHandle<()>>,
    /// Most recent physical pixel size — kept so we can re-derive the
    /// cell grid on resize.
    physical_size: (u32, u32),
    /// Most recent window scale factor (winit `Window::scale_factor`,
    /// including fractional Wayland values). The configured `font.size_px`
    /// is logical; the rasterizer is fed `size_px × scale_factor` so text
    /// stays the same apparent size across monitors and is rasterized at
    /// each monitor's true resolution. Updated on `Event::Resize` (which
    /// also carries `ScaleFactorChanged`); a change triggers a font
    /// rebuild + grid resync.
    scale_factor: f64,
    /// Last title we pushed to `winit::Window::set_title`. Tracked so
    /// we don't churn the compositor on every `PtyBytes` batch — only
    /// change the title when it actually differs.
    last_title: Option<String>,
    /// Most recent mouse pixel position; tracked so motion events that
    /// arrive *without* a position field (e.g. wheel scrolls under SGR
    /// reporting) can be reported at the last known cell.
    mouse_pos: (f64, f64),
    /// Currently held mouse button (the most recent press without a
    /// matching release). Used to fill the "button held" slot of motion
    /// reports under DECSET 1002.
    mouse_held: Option<MouseButton>,
    /// Last (col, row) reported to the PTY via a motion event. Tracked
    /// so we only emit a motion report when the pointer crosses into a
    /// new cell — xterm convention, and avoids flooding the PTY with
    /// one event per pixel of sub-cell trackpad movement.
    last_reported_cell: Option<(u16, u16)>,
    /// Clipboard handle (lazily initialised on first paste). `arboard`
    /// keeps a connection to the OS clipboard server; constructing it
    /// failed on the user's box is recoverable — we just log + skip.
    clipboard: Option<arboard::Clipboard>,
    /// Optional bidirectional PTY-byte logger, enabled by
    /// `TOASTTY_PTY_LOG=<path>`. No-op when the env var is unset.
    pty_log: PtyLogger,
    /// Sub-row pixel accumulator for the alt-screen-arrow translation
    /// path. macOS trackpad inertial frames arrive as small per-frame
    /// pixel deltas; we accumulate them here and emit one ↑/↓ arrow
    /// once the magnitude crosses one cell height.
    alt_scroll_pixel_accum: f64,
    /// Sub-row pixel accumulators for the SGR mouse-mode forwarding
    /// path (apps like zellij, btop, neovim with `set mouse=a`).
    /// macOS delivers many small pixel events per gesture; the
    /// encoder converts each one to a discrete scroll notch
    /// regardless of magnitude, which made trackpad scrolling inside
    /// zellij feel runaway-sensitive. We accumulate here and emit
    /// one notch per ~2 cells of finger motion.
    mouse_scroll_accum_y: f64,
    mouse_scroll_accum_x: f64,
    /// Sub-row pixel accumulator for the primary-grid scrollback path
    /// when `smooth_scrolling = false`. Trackpad pixel deltas pile up
    /// here; we apply whole-row deltas only, leaving the residual
    /// behind for the next frame.
    scroll_pixel_residual: f64,
    /// Last render-time we ticked the viewport animation. Used to
    /// compute `dt` for [`Term::advance_viewport`].
    last_viewport_tick: Option<Instant>,
    /// When `TOASTTY_DEBUG` is set, we maintain a ring of recent frame
    /// timestamps and push a "<n> FPS" overlay string to the renderer
    /// every frame. Empty / disabled when the env var is unset.
    debug_enabled: bool,
    frame_times: VecDeque<Instant>,
    /// Reusable buffer for the FPS overlay text. Avoids a per-frame
    /// `format!()` allocation when `debug_enabled` is on.
    fps_buf: String,
    /// Resolved path of the config file we read at startup (if any).
    /// Used to install the hot-reload watcher in `init_impl`.
    config_path: Option<std::path::PathBuf>,
    /// File-watcher driving hot-reload. `None` if the parent directory
    /// is missing or the watcher couldn't be installed.
    config_watcher: Option<ConfigWatcher>,
    /// Currently-displayed error banner string. Tracked so we only call
    /// `Renderer::set_error_banner` when the contents actually change.
    banner_text: Option<String>,
    /// Most recent config-load error from startup (if any). Promoted to
    /// `banner_text` once the renderer is available in `init_impl`.
    pending_load_error: Option<ConfigError>,
    /// Timestamp of the most recent `ScrollKind::Pixels` event we
    /// observed, regardless of routing. Together with
    /// `swallow_pixel_stream` this drives the inertia-swallow
    /// heuristic: a Pixels event whose gap to its predecessor is
    /// shorter than [`SCROLL_STREAM_LULL`] is part of the *same*
    /// continuous gesture / inertia tail; a larger gap marks the
    /// boundary between streams. `None` before the first scroll.
    last_pixel_scroll_at: Option<Instant>,
    /// True while the *current* `Pixels` stream should be ignored.
    /// Set by [`Self::snap_view_after_input`] when typing snapped
    /// the viewport to the bottom, so the trailing inertia frames
    /// macOS keeps delivering can't undo the snap. Cleared
    /// automatically at the next `SCROLL_STREAM_LULL`-or-larger gap
    /// — i.e., when the stream genuinely ends — so the next
    /// deliberate gesture goes through.
    swallow_pixel_stream: bool,
    /// In-progress mouse drag-select state. `Some` between press and
    /// release of the left button while local selection is active.
    /// `None` otherwise, including while a mouse-tracking app owns
    /// the click stream.
    selecting: Option<DragState>,
    /// Timestamp of the most recent left-button press, for multi-click
    /// detection (double = word, triple = line).
    last_click_at: Option<Instant>,
    /// Cell of the most recent left-button press in selection
    /// coordinates `(line_id, col)`. A subsequent press lands the
    /// same cell only if both match.
    last_click_pos: Option<Pos>,
    /// Number of clicks in the current multi-click streak: 0 between
    /// streaks, 1/2/3 within one. Wrapping above 3 cycles back to 1.
    click_count: u8,
}

impl Toastty {
    fn new(
        config: Config,
        command_override: Option<CommandOverride>,
        initial_size: (u32, u32),
        load_err: Option<ConfigError>,
    ) -> Self {
        let scrollback = config.scrollback.lines.try_into().unwrap_or(u16::MAX);
        // Start at a tiny grid; init() resizes once we know cell dimensions.
        let mut term = Term::new(24, 80, scrollback);
        // Thread the `[cursor]` config table through to the runtime —
        // DECSCUSR can still override at runtime per app.
        term.set_cursor_default(config.cursor.shape, config.cursor.blink);
        // Thread the `[security]` flags through so OSC 52 dispatch can
        // gate before queueing a clipboard request.
        term.set_security(SecurityFlags {
            osc_52_read: config.security.osc_52_read,
            osc_52_write: config.security.osc_52_write,
        });
        Self {
            config,
            command_override,
            window: None,
            renderer: None,
            parser: Parser::new(),
            term,
            pty: None,
            reader: None,
            physical_size: initial_size,
            // Real value is read from the window in `init_impl`; 1.0 is a
            // safe pre-window default.
            scale_factor: 1.0,
            last_title: None,
            mouse_pos: (0.0, 0.0),
            mouse_held: None,
            last_reported_cell: None,
            clipboard: None,
            pty_log: PtyLogger::from_env(),
            alt_scroll_pixel_accum: 0.0,
            mouse_scroll_accum_y: 0.0,
            mouse_scroll_accum_x: 0.0,
            scroll_pixel_residual: 0.0,
            last_viewport_tick: None,
            debug_enabled: std::env::var_os("TOASTTY_DEBUG").is_some(),
            frame_times: VecDeque::with_capacity(128),
            fps_buf: String::with_capacity(16),
            config_path: Config::default_path(),
            config_watcher: None,
            banner_text: None,
            pending_load_error: load_err,
            last_pixel_scroll_at: None,
            swallow_pixel_stream: false,
            selecting: None,
            last_click_at: None,
            last_click_pos: None,
            click_count: 0,
        }
    }

    /// Sync the window title with whatever the PTY most recently set via
    /// OSC 0 / 2. Called once after every `parser.advance()` so the
    /// shell's `PROMPT_COMMAND` (or vim's `:set title`) lands on the
    /// window decoration. We cache `last_title` to avoid round-tripping
    /// through winit's `set_title` when nothing changed — it's
    /// moderately expensive on Wayland / macOS.
    fn sync_title(&mut self) {
        let Some(window) = self.window.as_ref() else {
            return;
        };
        let cur = self.term.title();
        if cur.is_empty() {
            return;
        }
        // Only call into winit if the title actually changed.
        let changed = match &self.last_title {
            Some(last) => last != cur,
            None => true,
        };
        if changed {
            window.set_title(cur);
            self.last_title = Some(cur.to_owned());
        }
    }

    /// Read the system clipboard and write it to the PTY, bracketed if the
    /// foreground app has opted into DECSET 2004.
    fn paste(&mut self) {
        if self.pty.is_none() {
            return;
        }
        // Lazy-init the clipboard handle. If init failed last time, retry —
        // a transient failure (e.g. Wayland server hadn't bound the data
        // device yet) shouldn't doom every paste for the rest of the
        // session.
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    warn!("clipboard init failed: {e}");
                    return;
                }
            }
        }
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        let text = match clipboard.get_text() {
            Ok(s) => s,
            Err(e) => {
                debug!("clipboard read failed: {e}");
                return;
            }
        };
        let bytes = wrap_for_paste(&text, self.term.bracketed_paste());
        self.write_pty(&bytes);
        // A paste is user-driven PTY output — snap the viewport to
        // the live bottom so the pasted text lands at the prompt.
        self.snap_view_after_input();
    }

    fn write_pty(&mut self, bytes: &[u8]) {
        self.pty_log.log(Direction::ToApp, bytes);
        if let Some(pty) = self.pty.as_ref()
            && let Err(e) = pty.write(bytes)
        {
            warn!("pty write failed: {e}");
        }
    }

    /// Service one OSC 52 clipboard request.
    ///
    /// `arboard` is synchronous; on Wayland in particular the read
    /// path can briefly stall while the data device negotiates. We
    /// accept that — clipboard requests are user-initiated by the app
    /// they're rare enough that a millisecond-scale stall on the
    /// render thread is invisible. (TODO if real workloads disagree:
    /// move clipboard I/O onto a dedicated worker thread.)
    fn service_clipboard_request(&mut self, req: &ClipboardRequest) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    warn!("clipboard init failed: {e}");
                    return;
                }
            }
        }
        let Some(clipboard) = self.clipboard.as_mut() else {
            return;
        };
        match req {
            ClipboardRequest::Set { data } => {
                // OSC 52 set must be UTF-8 text to round-trip through
                // arboard's typed API; lossy-decode if the app sent
                // garbage bytes.
                let text = String::from_utf8_lossy(data);
                if let Err(e) = clipboard.set_text(text.into_owned()) {
                    warn!("clipboard write failed: {e}");
                }
            }
            ClipboardRequest::Query { selection } => {
                let bytes = match clipboard.get_text() {
                    Ok(s) => s.into_bytes(),
                    Err(e) => {
                        debug!("clipboard read failed: {e}");
                        return;
                    }
                };
                let sel =
                    toastty_protocols::clipboard::SelectionChars(selection.clone());
                let reply = toastty_protocols::clipboard::encode_reply(&sel, &bytes);
                self.term.push_pty_reply(&reply);
            }
        }
    }

    fn current_cell(&self) -> (u16, u16) {
        let cell_size = self
            .renderer
            .as_ref()
            .map_or((1.0_f32, 1.0_f32), Renderer::cell_size);
        let (rows, cols) = self.term.size();
        pixel_to_cell(self.mouse_pos, cell_size, (rows, cols))
    }

    /// Translate the current pointer position into a stable selection
    /// position `(line_id, col)`. Uses `bottom_id` plus the current
    /// scrollback offset to pick the right primary-grid row regardless
    /// of where the user has scrolled.
    fn current_selection_pos(&self) -> Pos {
        // pixel_to_cell returns 1-based (col, row); convert to 0-based.
        let (col1, row1) = self.current_cell();
        let row = row1.saturating_sub(1);
        let col = col1.saturating_sub(1);
        let (rows, _) = self.term.size();
        let bottom = self.term.bottom_id();
        // visible_row → above_bottom delta.
        let above_bottom = u64::from(rows.saturating_sub(1).saturating_sub(row));
        let scroll = u64::from(self.term.view_offset_lines());
        let line_id = bottom.saturating_sub(above_bottom + scroll);
        Pos::new(line_id, col)
    }

    /// True when a fresh left-button press should be considered a
    /// continuation of the prior click for multi-click detection. The
    /// previous click must have occurred within
    /// [`MULTI_CLICK_WINDOW`] AND landed on the same cell.
    fn is_multiclick_continuation(&self, now: Instant, here: Pos) -> bool {
        let Some(at) = self.last_click_at else {
            return false;
        };
        if now.duration_since(at) > MULTI_CLICK_WINDOW {
            return false;
        }
        self.last_click_pos == Some(here)
    }

    /// Build the [`Selection`] for a fresh left-button press, given
    /// the current click count (1/2/3) and modifier set. Word/line
    /// boundaries are computed against the cell under the cursor; if
    /// the row is in scrollback, we walk via the scrollback-row
    /// accessor.
    fn selection_from_press(
        &self,
        origin: Pos,
        click_count: u8,
        modifiers: Modifiers,
    ) -> Selection {
        let (_, cols) = self.term.size();
        // Alt+drag → block selection (only meaningful for an initial
        // single click; double/triple-click + alt also drops back to
        // block, matching iTerm2 / kitty).
        if modifiers.contains(Modifiers::ALT) {
            return Selection::new(origin, SelectionMode::Block);
        }
        match click_count {
            2 => {
                // Word: expand origin to word boundaries.
                let (lo, hi) = self
                    .word_bounds_at(origin.line_id, origin.col)
                    .unwrap_or((origin.col, origin.col));
                let mut sel = Selection::new(
                    Pos::new(origin.line_id, lo),
                    SelectionMode::Word,
                );
                sel.set_active(Pos::new(origin.line_id, hi));
                sel
            }
            3 => {
                // Line: full row.
                let end = cols.saturating_sub(1);
                let mut sel = Selection::new(
                    Pos::new(origin.line_id, 0),
                    SelectionMode::Line,
                );
                sel.set_active(Pos::new(origin.line_id, end));
                sel
            }
            _ => Selection::new(origin, SelectionMode::Char),
        }
    }

    /// Look up the visible word-boundary range given a `line_id`,
    /// falling back to `None` for evicted rows. Used by both
    /// press-time word expansion and drag-time word extension.
    fn word_bounds_at(&self, line_id: u64, col: u16) -> Option<(u16, u16)> {
        let row = self.term.primary_row_by_line_id(line_id)?;
        Some(word_bounds_in_row(row, col))
    }

    /// Recompute the cell grid from the current pixel size and apply it
    /// to both [`Term`] and [`Pty`].
    fn resync_grid(&mut self) {
        let Some(renderer) = self.renderer.as_ref() else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_size();
        if cell_w <= 0.0 || cell_h <= 0.0 {
            return;
        }
        let (px_w, px_h) = self.physical_size;
        let (cols, rows) = grid_dims_from_pixels(px_w, px_h, cell_w, cell_h);
        self.term.resize(rows, cols);
        // Re-plumb cell pixel size: font swap (M4b reload) can change
        // it, and CSI 16 t queries arriving post-resize need the
        // current value.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (cpw, cph) = (cell_w as u16, cell_h as u16);
        self.term.set_cell_pixel_size(cpw, cph);
        if let Some(pty) = self.pty.as_mut() {
            let pixel_width = u16::try_from(px_w).unwrap_or(u16::MAX);
            let pixel_height = u16::try_from(px_h).unwrap_or(u16::MAX);
            if let Err(e) = pty.resize(WinSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            }) {
                warn!("pty resize failed: {e}");
            } else {
                debug!(rows, cols, "resized pty + term");
            }
        }
    }

    /// Physical font size to feed the rasterizer: the logical
    /// `font.size_px` scaled by the current monitor scale factor. See
    /// [`effective_font_size_px`] and the `scale_factor` field.
    fn scaled_font_px(&self) -> f32 {
        effective_font_size_px(self.config.font.size_px, self.scale_factor)
    }

    /// Feed a batch of PTY bytes through the parser. Returns whether a
    /// fresh BSU (mode 2026 begin-synchronized-update) just went high
    /// during this batch — the caller uses that signal to schedule the
    /// watchdog redraw via `ControlSignal::RedrawIn(BSU_TIMEOUT)`.
    fn handle_pty_bytes(&mut self, bytes: &[u8]) -> bool {
        self.pty_log.log(Direction::FromApp, bytes);
        let was_paused = self.term.pause_rendering();
        // Refresh the foreground-process CWD before parsing this
        // batch — if an RGP `r;path=...` arrives in these bytes,
        // `Term::register_asset_by_path` needs to know the current
        // shell-side CWD to resolve relative paths. macOS uses
        // `proc_pidinfo`; Linux uses `/proc/<pid>/cwd`.
        if let Some(pty) = self.pty.as_ref()
            && let Some(cwd) = toastty_pty::pty_foreground_cwd(pty.master_fd())
        {
            let cwd_str = cwd.to_string_lossy();
            if cwd_str != self.term.cwd() {
                self.term.set_cwd(cwd_str.into_owned());
            }
        }
        self.parser.advance(&mut self.term, bytes);
        // OSC 0/1/2 may have changed the title — sync to the window
        // decoration. `sync_title` is a no-op when the title is
        // unchanged, so calling on every batch is cheap.
        self.sync_title();

        // Service any OSC 52 clipboard requests. We have to do this
        // BEFORE draining `pty_replies` because the read path
        // (`ClipboardRequest::Query`) populates `pty_replies` with the
        // encoded reply via `push_pty_reply`.
        let requests = self.term.drain_clipboard_requests();
        for req in requests {
            self.service_clipboard_request(&req);
        }

        // Drain any OSC replies the parsed batch wants written back to
        // the PTY (OSC 4 palette query, OSC 52 clipboard read). The
        // queue is unconditional — handlers that need no reply just
        // don't enqueue. See `Term::drain_pty_replies`.
        let replies = self.term.drain_pty_replies();
        if !replies.is_empty() {
            self.write_pty(&replies);
        }

        // BSU watchdog: if a BSU is currently in flight and its timer
        // has already elapsed, force-flush so the next frame issues a
        // corrective full redraw. We re-check after the parser advance
        // because the same batch may have contained BSU+ESU in order
        // (in which case `pause_rendering` is already false).
        if self.term.pause_rendering()
            && let Some(started_at) = self.term.sync_output_started_at()
            && should_force_flush(started_at, Instant::now())
        {
            self.term.force_flush_sync_output();
            if let Some(w) = self.window.as_ref() {
                w.request_redraw();
            }
        }

        // A fresh BSU started in this batch — caller will schedule a
        // wake-up at BSU_TIMEOUT so the watchdog above eventually
        // fires even if the app sends no more bytes.
        !was_paused && self.term.pause_rendering()
    }
}

impl Toastty {
    fn init_impl(&mut self, window: ToasttyWindow, handle: &WindowHandle) {
        let size = window.physical_size();
        self.physical_size = size;
        // Capture the monitor scale factor before sizing the font so the
        // very first frame rasterizes at the correct physical resolution
        // (rather than scale 1.0, then re-rasterizing on the first
        // `Resize`). Fractional Wayland scales are included.
        self.scale_factor = window.scale_factor();

        // Build the renderer. Transparency is logged once at startup in
        // `run`; here we only need the flag for surface configuration.
        let transparent = self.config.theme.bg.as_array()[3] < 1.0;
        let mut renderer = match block_on(Renderer::new(
            window.clone(),
            size,
            self.config.window.vsync,
            transparent,
        )) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("renderer init failed: {e}");
                return;
            }
        };
        renderer.with_font_ex(
            Some(self.config.font.family.as_str()),
            self.scaled_font_px(),
            self.config.font.line_height,
        );
        if !renderer.font_family_available() {
            warn!(
                "configured font {:?} not found on this system; falling back to the bundled / system default",
                self.config.font.family,
            );
        }
        renderer.set_theme(theme_from_config(&self.config.theme));
        info!(
            "renderer ready: size={size:?} cell={:?} font={:?} {}px logical @ {}× = {}px physical",
            renderer.cell_size(),
            self.config.font.family,
            self.config.font.size_px,
            self.scale_factor,
            self.scaled_font_px(),
        );

        // Compute initial grid from cell size + physical size.
        let (cell_w, cell_h) = renderer.cell_size();
        let (cols, rows) = grid_dims_from_pixels(size.0, size.1, cell_w, cell_h);
        self.term.resize(rows, cols);

        // Plumb the renderer's cell pixel size + theme bg into Term so
        // CSI 16 t / OSC 11 queries reply with the right values. Apps
        // like yazi gate kitty-graphics rendering on these answers and
        // fall back to colored cells when the queries time out.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let (cpw, cph) = (cell_w as u16, cell_h as u16);
        self.term.set_cell_pixel_size(cpw, cph);
        let theme_bg = renderer.theme().bg;
        let bg_rgb = [
            linear_to_srgb_u8(theme_bg[0]),
            linear_to_srgb_u8(theme_bg[1]),
            linear_to_srgb_u8(theme_bg[2]),
        ];
        self.term.set_default_bg(bg_rgb);

        // Spawn the PTY. CLI command override (xterm-style `-e`,
        // `kitty <cmd>`, etc.) wins over the configured shell.
        let (program, args) = resolve_command(self.command_override.take(), &self.config.shell);
        info!(?program, ?args, rows, cols, "spawning shell");
        let pixel_width = u16::try_from(size.0).unwrap_or(u16::MAX);
        let pixel_height = u16::try_from(size.1).unwrap_or(u16::MAX);
        // TERM: until `terminfo/toastty.terminfo` is installed via
        // `tic -x`, advertise `xterm-256color`. That's a near-superset
        // of what we currently implement (256-color + truecolor + alt
        // screen + DECSCUSR + OSC 0/2 title). Users who install our
        // terminfo can override via `TERM=toastty` in their shell rc
        // — `env()` runs *after* `with_current_env()`, so the explicit
        // value wins regardless of what the host environment had.
        //
        // TODO(M6+): once `terminfo/toastty.terminfo` is documented as a
        // shipped install step, flip this default to "toastty" with a
        // `tput -T toastty colors` probe + fallback.
        let spec = PtySpec::program(program)
            .args(args)
            .with_current_env()
            // Strip multiplexer markers leaked from whatever launched
            // toastty. The shell running INSIDE toastty isn't inside
            // tmux / zellij / screen — toastty is the terminal it sees
            // — and apps that key off these vars degrade badly:
            // - yazi: `ZELLIJ_SESSION_NAME` is set → retains only
            //   `Sixel` in the adapter list → empty on macOS → falls
            //   all the way through to `Chafa` (colored-cell preview).
            //   Kitty graphics never gets a chance even when we
            //   correctly advertise support via KITTY_WINDOW_ID.
            // - many shells: `TMUX` / `TMUX_PANE` change prompt + reset
            //   wrappers that toastty doesn't speak.
            .env_remove("ZELLIJ")
            .env_remove("ZELLIJ_PANE_ID")
            .env_remove("ZELLIJ_SESSION_NAME")
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .env_remove("STY") // GNU screen
            .env("TERM", "xterm-256color")
            // M10 shell integration: snippets under
            // `share/shell-integration/` gate on this var so they only
            // activate inside a toastty session.
            .env("TOASTTY", "1")
            // M11a: signal kitty-graphics support to apps that gate on
            // env-var brand detection rather than probing. yazi's
            // `Brand::from_env` checks for `KITTY_WINDOW_ID` to route
            // image rendering through the modern KGP driver instead of
            // its legacy fallback (which exercises code paths we
            // haven't validated). The value is informational — apps
            // generally treat any non-empty string as "we're in kitty".
            .env("KITTY_WINDOW_ID", "1")
            .working_dir(initial_cwd())
            .size(WinSize {
                rows,
                cols,
                pixel_width,
                pixel_height,
            });
        let pty = match Pty::spawn(&spec) {
            Ok(p) => p,
            Err(e) => {
                tracing::error!("pty spawn failed: {e}");
                return;
            }
        };

        // Spawn the mio reader thread. It posts user-events via the
        // winit `EventLoopProxy`, which arrive in `App::event` as
        // `Event::PtyBytes` / `Event::PtyClosed`.
        let proxy = handle.event_loop_proxy();
        let reader = match toastty_io::spawn_pty_reader(pty.master_fd(), proxy) {
            Ok(j) => j,
            Err(e) => {
                tracing::error!("pty reader spawn failed: {e}");
                return;
            }
        };

        self.renderer = Some(renderer);
        self.window = Some(window);
        self.pty = Some(pty);
        self.reader = Some(reader);

        // Install hot-reload watcher on the resolved config path
        // (even if the file doesn't currently exist — the parent dir
        // watch still picks up a future CREATE event).
        if let Some(path) = self.config_path.clone() {
            self.config_watcher =
                ConfigWatcher::spawn(path, handle.event_loop_proxy());
        }

        // Promote any startup config load error to the in-window
        // banner now that the renderer exists.
        if let Some(err) = self.pending_load_error.take() {
            self.set_banner_from_error(&err);
        }

        // Kick off the first frame. macOS doesn't always fire an
        // initial RedrawRequested on first display — without this,
        // the window stays black until the user moves the mouse or
        // types a key.
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Apply currently-stored config fields to the live renderer and
    /// term. Skips knobs that aren't safely live-applicable (window
    /// size, shell program, scrollback ring capacity).
    fn apply_config_to_runtime(&mut self) {
        // Scale the (possibly reloaded) logical size before borrowing the
        // renderer — `scaled_font_px` takes `&self`.
        let font_px = self.scaled_font_px();
        if let Some(r) = self.renderer.as_mut() {
            r.with_font_ex(
                Some(self.config.font.family.as_str()),
                font_px,
                self.config.font.line_height,
            );
            if !r.font_family_available() {
                warn!(
                    "configured font {:?} not found on this system; falling back to the bundled / system default",
                    self.config.font.family,
                );
            }
            r.set_theme(theme_from_config(&self.config.theme));
            // Re-plumb the theme bg into Term so OSC 11 queries reply
            // with the new value.
            let theme_bg = r.theme().bg;
            let bg_rgb = [
                linear_to_srgb_u8(theme_bg[0]),
                linear_to_srgb_u8(theme_bg[1]),
                linear_to_srgb_u8(theme_bg[2]),
            ];
            self.term.set_default_bg(bg_rgb);
            r.invalidate_framebuffer();
        }
        self.term
            .set_cursor_default(self.config.cursor.shape, self.config.cursor.blink);
        self.term.set_security(SecurityFlags {
            osc_52_read: self.config.security.osc_52_read,
            osc_52_write: self.config.security.osc_52_write,
        });
        // Font swap may have changed the cell size; recompute the grid
        // so columns/rows still match the window pixels.
        self.resync_grid();
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    /// Re-parse the watched config file and either apply it (clearing
    /// any banner) or surface the error in the banner without
    /// touching live state.
    fn reload_config(&mut self) {
        let Some(path) = self.config_path.clone() else {
            return;
        };
        if !path.exists() {
            // File was deleted: keep running with current settings,
            // surface a banner so the user knows we're holding the
            // previous state.
            self.set_banner(Some(format!(
                "config file missing: {}\n(continuing with previous settings; recreate or restart)",
                path.display()
            )));
            return;
        }
        match Config::load_from_path(&path) {
            Ok(cfg) => {
                self.config = cfg;
                self.apply_config_to_runtime();
                self.set_banner(None);
                info!("config reloaded from {}", path.display());
            }
            Err(e) => {
                warn!("config reload failed: {e}");
                self.set_banner_from_error(&e);
            }
        }
    }

    fn set_banner_from_error(&mut self, err: &ConfigError) {
        let path_line = match self.config_path.as_ref() {
            Some(p) => format!("config error in {}", p.display()),
            None => "config error".to_string(),
        };
        // Wrap the typed error's Display output across multiple lines
        // if it contains newlines (the toml crate's parse errors
        // already carry a multi-line caret display).
        let msg = format!(
            "{path_line}\n{err}\n(fix the file and save — toastty will reload automatically)"
        );
        self.set_banner(Some(msg));
    }

    fn set_banner(&mut self, text: Option<String>) {
        if self.banner_text == text {
            return;
        }
        self.banner_text = text;
        if let Some(r) = self.renderer.as_mut() {
            r.set_error_banner(self.banner_text.as_deref());
        }
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
    }

    fn handle_key(
        &mut self,
        logical: &LogicalKey,
        text: Option<&str>,
        modifiers: Modifiers,
        state: KeyState,
        repeat: bool,
        is_synthetic: bool,
    ) -> ControlSignal {
        // Intercept paste binding *before* the regular encoder.
        if state == KeyState::Pressed && is_paste_binding(logical, modifiers) {
            self.paste();
            return ControlSignal::RedrawIn(Duration::ZERO);
        }
        // Copy current selection (if any) to the clipboard. Eats the
        // keypress so the foreground app doesn't also see it.
        if state == KeyState::Pressed && is_copy_binding(logical, modifiers) {
            if let Some(text) = self.term.extract_selection_text()
                && !text.is_empty()
            {
                self.write_clipboard(text);
            }
            return ControlSignal::RedrawIn(Duration::ZERO);
        }
        // Don't forward synthetic events — winit fires those on focus loss
        // to clear modifier state; reporting them to the PTY would desync
        // the app.
        if is_synthetic {
            return ControlSignal::Continue;
        }
        if let Some(bytes) = encode_key(
            logical,
            text,
            modifiers,
            self.term.kitty_flags(),
            state,
            repeat,
        ) {
            self.write_pty(&bytes);
            // Typing into the PTY clears any visible selection — the
            // user wouldn't expect old highlight to persist over
            // freshly-typed prompt characters.
            if state == KeyState::Pressed {
                self.term.clear_selection();
            }
            // Typing snaps the view back to the live bottom so the
            // user lands on the prompt, not on whatever scrollback
            // they were reading. Press-only — releases shouldn't
            // trigger an animation snap.
            if state == KeyState::Pressed {
                self.snap_view_after_input();
            }
        }
        ControlSignal::Continue
    }

    /// Insert IME-committed text into the PTY. This is the IME analogue of
    /// typing: an input method (fcitx5, CJK, Hangul, macOS dead keys, …)
    /// composes one or more characters and commits them as a single string
    /// rather than as per-keystroke `Event::Key`s. Treated like typed text
    /// — written raw (not bracketed-paste wrapped), clears the selection,
    /// and snaps the viewport to the prompt.
    fn handle_ime_commit(&mut self, text: &str) -> ControlSignal {
        // winit emits an empty preedit right before commit, but clear
        // defensively so the overlay never outlives the composition.
        if let Some(r) = self.renderer.as_mut() {
            r.set_preedit(None);
        }
        if text.is_empty() {
            return ControlSignal::RedrawIn(Duration::ZERO);
        }
        self.write_pty(text.as_bytes());
        self.term.clear_selection();
        self.snap_view_after_input();
        ControlSignal::RedrawIn(Duration::ZERO)
    }

    /// Update the inline preedit (in-progress composition) overlay drawn at
    /// the cursor. Empty text clears it. Also repositions the IME
    /// candidate popup at the cursor.
    fn handle_ime_preedit(&mut self, text: &str) -> ControlSignal {
        if let Some(r) = self.renderer.as_mut() {
            r.set_preedit(if text.is_empty() { None } else { Some(text) });
        }
        self.update_ime_cursor_area();
        ControlSignal::RedrawIn(Duration::ZERO)
    }

    /// Tell the compositor where the text cursor is (physical pixels) so the
    /// IME candidate / preedit popup is positioned beside it instead of at
    /// the window origin. No-op until the window and renderer exist.
    fn update_ime_cursor_area(&self) {
        let (Some(window), Some(renderer)) = (self.window.as_ref(), self.renderer.as_ref())
        else {
            return;
        };
        let (cell_w, cell_h) = renderer.cell_size();
        let cursor = self.term.cursor();
        let x = f64::from(cursor.col) * f64::from(cell_w);
        let y = f64::from(cursor.row) * f64::from(cell_h);
        window.set_ime_cursor_area(x, y, f64::from(cell_w), f64::from(cell_h));
    }

    /// Report cursor motion under DECSET 1002 (drag) / 1003 (any motion).
    /// xterm reports motion at cell granularity — emit one event per
    /// new cell crossed, not per pixel — so dragging within a cell stays
    /// silent and a slow drag across the grid produces one event per
    /// column/row boundary.
    fn handle_mouse_motion(
        &mut self,
        position: (f64, f64),
        modifiers: Modifiers,
    ) -> ControlSignal {
        self.mouse_pos = position;
        // Local drag-select first: when we've started a selection,
        // every motion event extends it, even if the foreground app
        // would otherwise want the motion. (The selection only starts
        // in the first place when `mouse_mode().is_on()` was false, so
        // there's no conflict.)
        if self.selecting.is_some() {
            self.extend_selection_to_cursor();
            return ControlSignal::RedrawIn(Duration::ZERO);
        }
        let mode = self.term.mouse_mode();
        let kind = MouseEventKind::Motion {
            held: self.mouse_held,
        };
        if !protocol_wants_event(mode, &kind) {
            // App hasn't opted into motion reporting; nothing to send.
            // Clear the de-dup cache so the next opt-in starts fresh.
            self.last_reported_cell = None;
            return ControlSignal::Continue;
        }
        let cell = self.current_cell();
        if self.last_reported_cell == Some(cell) {
            return ControlSignal::Continue;
        }
        self.last_reported_cell = Some(cell);
        if let Some(bytes) = encode_mouse(kind, cell, modifiers, mode) {
            self.write_pty(&bytes);
        }
        ControlSignal::Continue
    }

    fn handle_mouse(
        &mut self,
        button: MouseButton,
        state: KeyState,
        position: (f64, f64),
        modifiers: Modifiers,
    ) -> ControlSignal {
        self.mouse_pos = position;
        match state {
            KeyState::Pressed => self.mouse_held = Some(button),
            KeyState::Released => {
                if self.mouse_held == Some(button) {
                    self.mouse_held = None;
                }
            }
        }
        // OSC 8 click-to-open: Cmd-Left on macOS / Ctrl-Left elsewhere
        // on press only. Consume the event so the mouse encoder doesn't
        // also forward it to the foreground app.
        if state == KeyState::Pressed && is_open_link_binding(button, modifiers) {
            if let Some(url) = self.hyperlink_under_cursor(position)
                && let Err(e) = webbrowser::open(&url)
            {
                warn!("open URL failed: {e}");
            }
            return ControlSignal::Continue;
        }
        // Local text selection takes priority over PTY forwarding when
        // the foreground app hasn't opted into mouse tracking. Only
        // the left button drives selection; other buttons fall through
        // to the existing encoder.
        let mode = self.term.mouse_mode();
        if !mode.is_on() && button == MouseButton::Left {
            match state {
                KeyState::Pressed => self.begin_or_extend_selection(modifiers),
                KeyState::Released => self.finish_selection_drag(),
            }
            return ControlSignal::RedrawIn(Duration::ZERO);
        }
        let kind = classify_button_event(button, state);
        if protocol_wants_event(mode, &kind) {
            let cell = self.current_cell();
            if let Some(bytes) = encode_mouse(kind, cell, modifiers, mode) {
                self.write_pty(&bytes);
            }
        }
        ControlSignal::Continue
    }

    /// Start a fresh selection (or step the multi-click counter) at
    /// the current cursor position. Called from `handle_mouse` on a
    /// left-button press when the foreground app isn't tracking the
    /// mouse.
    fn begin_or_extend_selection(&mut self, modifiers: Modifiers) {
        let origin = self.current_selection_pos();
        let now = Instant::now();
        // Multi-click cycle: 1 → 2 → 3 → 1 within MULTI_CLICK_WINDOW
        // on the same cell. Otherwise restart at 1.
        let next_count = if self.is_multiclick_continuation(now, origin) {
            // Cycle 1→2→3 then back to 1 on the 4th — matches iTerm2.
            (self.click_count % 3) + 1
        } else {
            1
        };
        self.click_count = next_count;
        self.last_click_at = Some(now);
        self.last_click_pos = Some(origin);
        let sel = self.selection_from_press(origin, next_count, modifiers);
        // For Alt+drag, selection mode is Block — capture the chosen
        // mode and the original origin so motion can extend correctly.
        let mode = sel.mode;
        self.selecting = Some(DragState { mode, origin });
        self.term.set_selection(sel);
    }

    /// Extend the in-flight selection to whichever cell the pointer
    /// currently sits over. Called from `handle_mouse_motion` while a
    /// drag is active.
    fn extend_selection_to_cursor(&mut self) {
        let Some(drag) = self.selecting else {
            return;
        };
        let here = self.current_selection_pos();
        match drag.mode {
            SelectionMode::Char | SelectionMode::Block => {
                self.term.update_selection_active(here);
            }
            SelectionMode::Word => {
                // Snap `here` to the word containing it, then extend
                // anchor → active to the far edge so the selection
                // grows by whole words on each side.
                let (lo_here, hi_here) =
                    self.word_bounds_at(here.line_id, here.col).unwrap_or((here.col, here.col));
                let (lo_origin, hi_origin) = self
                    .word_bounds_at(drag.origin.line_id, drag.origin.col)
                    .unwrap_or((drag.origin.col, drag.origin.col));
                // Build a Selection with anchor at the further-from-here
                // edge of the origin word, active at the further-from-
                // origin edge of the here word.
                let dragging_forward = (here.line_id, here.col)
                    >= (drag.origin.line_id, drag.origin.col);
                let (anchor_col, active_col) = if dragging_forward {
                    (lo_origin, hi_here)
                } else {
                    (hi_origin, lo_here)
                };
                let mut sel = Selection::new(
                    Pos::new(drag.origin.line_id, anchor_col),
                    SelectionMode::Word,
                );
                sel.set_active(Pos::new(here.line_id, active_col));
                self.term.set_selection(sel);
            }
            SelectionMode::Line => {
                // Anchor at origin row col 0, active at here row col
                // cols-1 (or vice versa).
                let (_, cols) = self.term.size();
                let end = cols.saturating_sub(1);
                let dragging_forward = here.line_id >= drag.origin.line_id;
                let mut sel = Selection::new(
                    Pos::new(drag.origin.line_id, if dragging_forward { 0 } else { end }),
                    SelectionMode::Line,
                );
                sel.set_active(Pos::new(
                    here.line_id,
                    if dragging_forward { end } else { 0 },
                ));
                self.term.set_selection(sel);
            }
        }
    }

    /// Wrap up the drag on left-release: copy the selected text to the
    /// system clipboard if non-empty, then drop the visible highlight
    /// — the user signalled "I'm done picking" by releasing the
    /// button, and the highlight lingering past that is visual noise.
    fn finish_selection_drag(&mut self) {
        if self.selecting.take().is_none() {
            return;
        }
        if let Some(text) = self.term.extract_selection_text()
            && !text.is_empty()
        {
            self.write_clipboard(text);
        }
        self.term.clear_selection();
    }

    /// Lazy-init `self.clipboard` and write `text`. Mirrors the
    /// paste-path init in `Self::paste`.
    fn write_clipboard(&mut self, text: String) {
        if self.clipboard.is_none() {
            match arboard::Clipboard::new() {
                Ok(c) => self.clipboard = Some(c),
                Err(e) => {
                    warn!("clipboard init failed: {e}");
                    return;
                }
            }
        }
        if let Some(cb) = self.clipboard.as_mut()
            && let Err(e) = cb.set_text(text)
        {
            warn!("clipboard write failed: {e}");
        }
    }

    /// Look up the OSC 8 hyperlink URL at pixel `(x, y)`, if any.
    fn hyperlink_under_cursor(&self, position: (f64, f64)) -> Option<String> {
        let cell_size = self.renderer.as_ref()?.cell_size();
        let (rows, cols) = self.term.size();
        // `pixel_to_cell` returns 1-based (col, row). Convert to
        // 0-based with a saturating sub so column 1 / row 1 map to
        // (0, 0) instead of underflowing.
        let (col_1based, row_1based) = pixel_to_cell(position, cell_size, (rows, cols));
        let col = col_1based.checked_sub(1)?;
        let row = row_1based.checked_sub(1)?;
        let row_ref = self.term.row(row);
        let cell = row_ref.cells.get(col as usize)?;
        let id = cell.hyperlink_id?;
        self.term.hyperlink_url(id).map(str::to_string)
    }

    fn handle_scroll(
        &mut self,
        scroll_kind: toastty_window::ScrollKind,
        scroll_phase: toastty_window::ScrollPhase,
        delta_x: f64,
        delta_y: f64,
    ) -> ControlSignal {
        // winit's `MouseWheel` y sign is opposite to what every downstream
        // path here was assuming (primary scrollback, alt-screen arrow
        // translation, and the SGR mouse encoder). Negating once at the
        // entry point lines all three up — without it, scrolling moved in
        // the wrong direction across the board.
        let delta_y = -delta_y;
        let delta_x = -delta_x;

        // Maintain the inertia-swallow stream-tracking state up-front,
        // before any routing decision. We update on every `Pixels`
        // event (even ones we end up forwarding to a mouse-mode app)
        // so the state stays coherent if the app toggles mouse-tracking
        // mid-stream.
        //
        // Two ways to declare a fresh stream and clear the per-stream
        // swallow flag:
        //
        //   1. **Gap >= SCROLL_STREAM_LULL.** Works on every platform
        //      and for every phase value — the inertia tail can't
        //      survive a >150 ms silence, so anything arriving after
        //      it is a new stream.
        //   2. **`ScrollPhase::Started` with a gap >= FRESH_STARTED_GAP.**
        //      Started fires both for fingers-touching and for
        //      momentum-begin — but momentum-begin lands within ~16 ms
        //      of the preceding Ended, while a fresh user gesture
        //      takes much longer than 50 ms to set up. So a Started
        //      that didn't follow the prior event nearly-instantly is
        //      definitely user intent, and we can release the swallow
        //      faster than waiting out the full 150 ms lull.
        if scroll_kind == toastty_window::ScrollKind::Pixels {
            let now = Instant::now();
            let gap = self
                .last_pixel_scroll_at
                .map(|t| now.duration_since(t));
            let big_gap = gap.is_none_or(|g| g >= SCROLL_STREAM_LULL);
            let fresh_started = scroll_phase == toastty_window::ScrollPhase::Started
                && gap.is_none_or(|g| g >= FRESH_STARTED_GAP);
            if big_gap || fresh_started {
                self.swallow_pixel_stream = false;
            }
            self.last_pixel_scroll_at = Some(now);
        }

        // We need the cell height for the pixel→notch accumulator in
        // the mouse-mode forwarding path, the alt-screen path, and the
        // viewport (primary path). Without a renderer we can't size
        // anything, so drop the event.
        let Some(cell_h) = self.renderer.as_ref().map(|r| r.cell_size().1) else {
            return ControlSignal::Continue;
        };
        if cell_h <= 0.0 {
            return ControlSignal::Continue;
        }

        // Priority 1: app has mouse-tracking on. Forward the wheel
        // event — apps like zellij/btop/htop/neovim consume scroll
        // as input, and they always win over the local scrollback
        // view.
        //
        // For `Lines` (wheel notch) the event is intentional and
        // passes through 1:1. For `Pixels` (trackpad) the encoder
        // would otherwise emit one notch per pixel event, which on
        // macOS produces 60+ notches per gesture — apps feel
        // runaway-sensitive. Accumulate into the mouse-scroll
        // accumulators and emit one notch per ~2 cells of finger
        // motion. Two cells (rather than one) lands the perceived
        // speed roughly halfway between today's flood and a strict
        // one-notch-per-cell rate.
        let mode = self.term.mouse_mode();
        let pre_kind = MouseEventKind::Scroll {
            dx: delta_x,
            dy: delta_y,
        };
        if protocol_wants_event(mode, &pre_kind) {
            let cell = self.current_cell();
            let (notches_y, notches_x): (i32, i32) = match scroll_kind {
                toastty_window::ScrollKind::Lines => {
                    // Reset pixel accumulators when a wheel notch
                    // arrives — mixing notch + trackpad mid-stream
                    // shouldn't carry partial trackpad pixels into
                    // the notch path.
                    self.mouse_scroll_accum_y = 0.0;
                    self.mouse_scroll_accum_x = 0.0;
                    #[allow(clippy::cast_possible_truncation)]
                    let ny = delta_y.signum() as i32 * (delta_y.abs() > f64::EPSILON) as i32;
                    #[allow(clippy::cast_possible_truncation)]
                    let nx = delta_x.signum() as i32 * (delta_x.abs() > f64::EPSILON) as i32;
                    (ny, nx)
                }
                toastty_window::ScrollKind::Pixels => {
                    let threshold = self.pixel_scroll_threshold(2.0, f64::from(cell_h));
                    self.mouse_scroll_accum_y += delta_y;
                    self.mouse_scroll_accum_x += delta_x;
                    let cy = (self.mouse_scroll_accum_y / threshold).trunc();
                    let cx = (self.mouse_scroll_accum_x / threshold).trunc();
                    self.mouse_scroll_accum_y -= cy * threshold;
                    self.mouse_scroll_accum_x -= cx * threshold;
                    #[allow(clippy::cast_possible_truncation)]
                    let r = (cy as i32, cx as i32);
                    r
                }
            };
            if notches_y != 0 {
                let kind_y = MouseEventKind::Scroll {
                    dx: 0.0,
                    dy: f64::from(notches_y.signum()),
                };
                if let Some(bytes) = encode_mouse(kind_y, cell, Modifiers::empty(), mode) {
                    for _ in 0..notches_y.unsigned_abs() {
                        self.write_pty(&bytes);
                    }
                }
            }
            if notches_x != 0 {
                let kind_x = MouseEventKind::Scroll {
                    dx: f64::from(notches_x.signum()),
                    dy: 0.0,
                };
                if let Some(bytes) = encode_mouse(kind_x, cell, Modifiers::empty(), mode) {
                    for _ in 0..notches_x.unsigned_abs() {
                        self.write_pty(&bytes);
                    }
                }
            }
            return ControlSignal::Continue;
        }

        // Priority 2: alt screen + no mouse mode. Apps like `less`,
        // `man`, plain `vim` (without `set mouse=a`) sit on the alt
        // screen and expect arrow keys for navigation. Translate
        // wheel/trackpad into ↑/↓ sequences. The helper returns a
        // signed line count — positive == scroll DOWN, negative ==
        // scroll UP — folding in the running pixel accumulator so a
        // brief direction reversal mid-inertia doesn't emit an arrow
        // in the wrong direction.
        if self.term.is_alt_active() {
            // Same inertia-swallow guard as the primary-grid path:
            // if typing snapped state during a Pixels stream, drop
            // the trailing inertia frames so phantom arrow keys
            // don't get pushed into the alt-screen app afterwards.
            if scroll_kind == toastty_window::ScrollKind::Pixels && self.swallow_pixel_stream {
                return ControlSignal::Continue;
            }
            let signed = self.alt_screen_arrows_for_scroll(scroll_kind, delta_y, f64::from(cell_h));
            if signed != 0 {
                let bytes = if signed > 0 {
                    b"\x1b[B".as_slice()
                } else {
                    b"\x1b[A".as_slice()
                };
                let count = signed.unsigned_abs() as usize;
                let mut out: Vec<u8> = Vec::with_capacity(bytes.len() * count);
                for _ in 0..count {
                    out.extend_from_slice(bytes);
                }
                self.write_pty(&out);
            }
            return ControlSignal::Continue;
        }

        // Priority 3: primary grid scrollback view.
        //
        // Inertia-swallow: typing armed `swallow_pixel_stream`; while
        // the macOS inertia tail is still arriving (gap from the
        // previous Pixels event < SCROLL_STREAM_LULL) the flag stays
        // set and we drop every frame, so the snap-to-bottom isn't
        // undone. The first scroll after a real gap clears the flag
        // (handled at the top of this function), so the next
        // deliberate gesture lands cleanly. Lines/notch events are
        // intentional and pass through.
        if scroll_kind == toastty_window::ScrollKind::Pixels && self.swallow_pixel_stream {
            return ControlSignal::Continue;
        }
        let lines_per_notch = self.config.scrollback.lines_per_notch.max(1) as f64;
        match scroll_kind {
            toastty_window::ScrollKind::Lines => {
                // Discrete notch. Round to an integer line count and
                // animate via the configured smoothing function.
                let raw = -delta_y * lines_per_notch;
                #[allow(clippy::cast_possible_truncation)]
                let delta_lines = raw.round() as i32;
                if delta_lines != 0 {
                    self.term.scroll_view_by(delta_lines, 0.0, cell_h);
                }
            }
            toastty_window::ScrollKind::Pixels => {
                // Continuous pixel stream (incl. macOS inertia).
                // Positive delta_y == content moves down ==> view
                // target moves toward the bottom (decreases lines).
                let delta_pixel = -delta_y;
                if delta_pixel.abs() > f64::EPSILON {
                    if self.config.scrollback.smooth_scrolling {
                        #[allow(clippy::cast_possible_truncation)]
                        let delta_f32 = delta_pixel as f32;
                        self.term.scroll_view_by(0, delta_f32, cell_h);
                        // Pixel deltas can leave the *current* position
                        // behind by a fraction of a cell after the
                        // target moves. The lerp normally catches up,
                        // but for trackpad inertia where the user wants
                        // pixel-perfect tracking, snap current to
                        // target immediately. The animation tick will
                        // still smooth out wheel notches that arrive
                        // separately.
                        self.term.force_snap_view();
                    } else {
                        // Smooth scrolling disabled: accumulate pixels
                        // and only apply whole-row deltas. The residual
                        // stays in `scroll_pixel_residual` so a slow
                        // trackpad drag eventually advances by a row.
                        self.scroll_pixel_residual += delta_pixel;
                        let threshold = self.pixel_scroll_threshold(1.0, f64::from(cell_h));
                        let crossings = (self.scroll_pixel_residual / threshold).trunc();
                        self.scroll_pixel_residual -= crossings * threshold;
                        #[allow(clippy::cast_possible_truncation)]
                        let delta_lines = crossings as i32;
                        if delta_lines != 0 {
                            self.term.scroll_view_by(delta_lines, 0.0, cell_h);
                            self.term.force_snap_view();
                        }
                    }
                }
            }
        }
        // Reaching here means we actually applied a scroll
        // (scroll_view_by + optional force_snap_view above). The
        // term's damage / animation state has been updated, so we
        // must request a redraw — unconditionally. Returning
        // `Continue` when `viewport_animating()` happens to be false
        // (which is the case under the smooth-scroll-pixels +
        // force_snap_view path: current is snapped to target so
        // there's nothing to animate) silently skips the frame; the
        // visible state then drifts away from the internal state
        // until *something else* requests a redraw, at which point
        // the deferred scroll appears as a sudden jump. Historically
        // the cursor-blink cadence masked this, but apps like helix
        // disable blink via DECSCUSR and leave it off on exit, so
        // post-helix scrolling looked broken until the next
        // keystroke. `RedrawIn(ZERO)` is cheap — the renderer's
        // skip-submit gate will short-circuit when there really is
        // nothing dirty.
        ControlSignal::RedrawIn(Duration::ZERO)
    }

    /// Signed line count for an alt-screen scroll translation.
    /// Positive == press DOWN that many times, negative == press UP.
    /// For `Lines` kind, rounds to the nearest signed line count after
    /// multiplying by `lines_per_notch`. For `Pixels`, accumulates
    /// into [`Self::alt_scroll_pixel_accum`] and returns the signed
    /// number of whole-row thresholds crossed (positive == DOWN-net
    /// motion).
    fn alt_screen_arrows_for_scroll(
        &mut self,
        kind: toastty_window::ScrollKind,
        delta_y: f64,
        cell_h: f64,
    ) -> i32 {
        match kind {
            toastty_window::ScrollKind::Lines => {
                let raw = delta_y * f64::from(self.config.scrollback.lines_per_notch.max(1));
                #[allow(clippy::cast_possible_truncation)]
                let n = raw.round() as i32;
                // Reset pixel accumulator — switching back to a notch
                // wheel mid-stream shouldn't carry over a partial
                // trackpad value.
                self.alt_scroll_pixel_accum = 0.0;
                n
            }
            toastty_window::ScrollKind::Pixels => {
                // Accumulate signed pixel motion so direction reversal
                // cancels out before any arrow is emitted. Sign of
                // `crossings` is the net direction.
                let threshold = self.pixel_scroll_threshold(1.0, cell_h);
                self.alt_scroll_pixel_accum += delta_y;
                let crossings = (self.alt_scroll_pixel_accum / threshold).trunc();
                self.alt_scroll_pixel_accum -= crossings * threshold;
                #[allow(clippy::cast_possible_truncation)]
                let n = crossings as i32;
                n
            }
        }
    }

    /// Pixel travel (device px) that must accumulate before emitting
    /// one discrete scroll step, for a path whose baseline is `cells`
    /// cell-heights. Divided by the configured
    /// [`pixel_scroll_sensitivity`](toastty_config::ScrollbackConfig::pixel_scroll_sensitivity)
    /// so a higher value means more sensitive (less travel per step).
    /// The sensitivity is clamped to a small positive floor so a
    /// degenerate config value can't produce a zero/negative threshold
    /// and divide by zero in the accumulators.
    fn pixel_scroll_threshold(&self, cells: f64, cell_h: f64) -> f64 {
        let sens = self.config.scrollback.pixel_scroll_sensitivity.max(0.05);
        cells * cell_h / sens
    }

    /// Snap the viewport target to the live bottom and schedule a
    /// redraw. Called from the key handler whenever a press produces
    /// PTY output — matches the iTerm2/Kitty/Alacritty convention of
    /// "typing brings you back to the prompt".
    fn snap_view_after_input(&mut self) {
        if !self.term.at_view_bottom() || self.term.viewport_animating() {
            self.term.snap_view_to_bottom();
            // Under instant smoothing or smooth_scrolling=off, jump
            // right away so the first frame after the keystroke is
            // already at the bottom.
            if !self.config.scrollback.smooth_scrolling
                || self.config.scrollback.smoothing_function
                    == toastty_config::SmoothingFunction::Instant
            {
                self.term.force_snap_view();
            }
        }
        // macOS keeps delivering `ScrollKind::Pixels` inertia frames
        // for hundreds of milliseconds (sometimes >1s) after the
        // physical gesture ends. Marking the *current* Pixels stream
        // as swallowed means every remaining frame in that stream is
        // dropped, instead of guessing how long inertia will last.
        // [`handle_scroll`] clears the flag automatically at the
        // next inter-frame gap >= [`SCROLL_STREAM_LULL`], i.e. the
        // moment the stream genuinely ends — so the user's *next*
        // deliberate gesture still works. Wheel-notch (`Lines`)
        // events are intentional input and are never swallowed.
        self.swallow_pixel_stream = true;
        // Any pixel accumulators built up during the original gesture
        // would otherwise wait around and apply on the next scroll —
        // drop them so the post-typing state starts clean.
        self.scroll_pixel_residual = 0.0;
        self.alt_scroll_pixel_accum = 0.0;
    }
}

impl App for Toastty {
    fn init(&mut self, window: ToasttyWindow, handle: WindowHandle) {
        Toastty::init_impl(self, window, &handle);
    }

    fn event(&mut self, event: Event) -> ControlSignal {
        match event {
            Event::Close => {
                if let Some(pty) = self.pty.as_mut() {
                    let _ = pty.kill();
                    let _ = pty.wait();
                }
                ControlSignal::Exit
            }
            Event::Resize { width, height, scale_factor } => {
                self.physical_size = (width, height);
                // winit folds `ScaleFactorChanged` into this event (see
                // toastty-window), so this is the single place scale is
                // observed. A scale change — e.g. the window moved to a
                // monitor with different DPI scaling — means the logical
                // `font.size_px` now maps to a different physical size, so
                // re-rasterize the font at the new resolution. The grid is
                // re-derived from the new cell size by `resync_grid` below.
                // A real scale change is at least a few percent (the
                // smallest fractional-scaling step is ~0.05); a 1e-6
                // tolerance detects all of those while ignoring any
                // floating-point jitter, so we never rebuild the font
                // (atlas + pipeline) for a no-op scale.
                let scale_changed = (scale_factor - self.scale_factor).abs() > 1e-6;
                self.scale_factor = scale_factor;
                let font_px = self.scaled_font_px();
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(width, height);
                    if scale_changed {
                        r.with_font_ex(
                            Some(self.config.font.family.as_str()),
                            font_px,
                            self.config.font.line_height,
                        );
                        info!(
                            "scale changed to {}× → font {}px physical, cell={:?}",
                            scale_factor,
                            font_px,
                            r.cell_size(),
                        );
                    }
                    // The new back-buffer has undefined contents — the
                    // renderer must clear on the next frame regardless
                    // of damage state. `Renderer::resize` already sets
                    // this internally, but the explicit call documents
                    // the contract.
                    r.invalidate_framebuffer();
                }
                self.resync_grid();
                // DECSET 2048 — emit an in-band resize report so apps
                // that opted in see the new geometry in order with
                // everything else on the PTY (no SIGWINCH race). The
                // encoder returns None when 2048 is off, so we don't
                // touch the PTY in the default case.
                let (rows, cols) = self.term.size();
                let pixel_w = u16::try_from(width).unwrap_or(u16::MAX);
                let pixel_h = u16::try_from(height).unwrap_or(u16::MAX);
                if let Some(bytes) =
                    encode_resize_report(rows, cols, pixel_h, pixel_w, self.term.inband_resize_mode())
                {
                    self.write_pty(&bytes);
                }
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::Redraw => {
                // BSU watchdog (idle-fire path). If a BSU is still in
                // flight and its 1 s deadline has passed, force-flush
                // here — otherwise an app that emits BSU then goes
                // silent never gets its `ControlSignal::RedrawIn(BSU_TIMEOUT)`
                // wake-up converted into a corrective frame.
                // `handle_pty_bytes` covers the BSU-then-more-bytes
                // case; this branch covers BSU-then-silence.
                if self.term.pause_rendering()
                    && let Some(started_at) = self.term.sync_output_started_at()
                    && should_force_flush(started_at, Instant::now())
                {
                    self.term.force_flush_sync_output();
                }
                // Scrollback viewport animation tick. Runs before the
                // render so the frame we're about to draw reflects the
                // newly-lerped position. When the animation is at rest,
                // `advance_viewport` is a cheap no-op that returns false.
                let now = Instant::now();
                if let Some(r) = self.renderer.as_ref() {
                    let cell_h = r.cell_size().1;
                    if cell_h > 0.0 {
                        let dt = match self.last_viewport_tick {
                            Some(t) => now.duration_since(t).as_secs_f32().min(0.1),
                            None => 1.0 / 60.0,
                        };
                        let smoothing = smoothing_from_config(&self.config);
                        self.term.advance_viewport(dt, cell_h, smoothing);
                    }
                }
                self.last_viewport_tick = Some(now);

                // Refresh the debug overlay (FPS counter) BEFORE
                // rendering so it lands on this frame. We retain frame
                // timestamps from the last second and compute fps =
                // (n - 1) / (last - first). Empty/single-sample shows
                // "--" to avoid a misleading huge number on the first
                // tick.
                if self.debug_enabled
                    && let Some(r) = self.renderer.as_mut()
                {
                    use std::fmt::Write as _;
                    let cutoff = now - Duration::from_secs(1);
                    while self.frame_times.front().is_some_and(|t| *t < cutoff) {
                        self.frame_times.pop_front();
                    }
                    self.fps_buf.clear();
                    if self.frame_times.len() >= 2 {
                        let span = self
                            .frame_times
                            .back()
                            .unwrap()
                            .duration_since(*self.frame_times.front().unwrap())
                            .as_secs_f64();
                        let fps = if span > 0.0 {
                            (self.frame_times.len() - 1) as f64 / span
                        } else {
                            0.0
                        };
                        // write! into a String is infallible.
                        let _ = write!(self.fps_buf, " {fps:5.1} FPS ");
                    } else {
                        let _ = write!(self.fps_buf, " {:>5} FPS ", "--");
                    }
                    r.set_debug_overlay(Some(&self.fps_buf));
                }

                let mut next_blink: Option<Duration> = None;
                if let Some(r) = self.renderer.as_mut() {
                    match r.render_term(&mut self.term) {
                        Ok(RenderOutcome::Rendered) => {
                            // Consume the per-cell damage signal so the
                            // next render only re-emits cells that
                            // changed. Clear the BSU timeout-flush
                            // flag too so the renderer observes it for
                            // exactly one frame.
                            self.term.clear_damage();
                            self.term.clear_sync_output_force_flushed();
                            if self.debug_enabled {
                                self.frame_times.push_back(now);
                            }
                        }
                        Ok(RenderOutcome::Skipped) => {
                            // Frame skipped (pause-gated or surface
                            // hiccup): leave the damage signal and the
                            // BSU force-flushed flag alone so the next
                            // non-skipped frame still issues the
                            // corrective redraw.
                        }
                        Err(e) => warn!("render_term error: {e}"),
                    }
                    // Schedule a wake-up at the next cursor blink tick
                    // so the renderer can toggle visibility even when
                    // the PTY is silent. `next_redraw_deadline` returns
                    // None when the term has blink disabled (DECSCUSR
                    // Ps=2/4/6) so we don't pointlessly wake the event
                    // loop.
                    next_blink = r.next_redraw_deadline(&self.term);
                }
                // If the viewport is still animating, schedule the next
                // frame ~60 Hz. Combined with any blink deadline via
                // `min_deadline`. Once the animation settles, we stop
                // ticking and the event loop goes back to idle.
                let next_anim = if self.term.viewport_animating() {
                    Some(VIEWPORT_ANIM_TICK)
                } else {
                    self.last_viewport_tick = None;
                    None
                };
                let next_debug = if self.debug_enabled {
                    Some(DEBUG_OVERLAY_TICK)
                } else {
                    None
                };
                match min_deadline(min_deadline(next_blink, next_anim), next_debug) {
                    Some(d) => ControlSignal::RedrawIn(d),
                    None => ControlSignal::Continue,
                }
            }
            Event::Key {
                logical,
                text,
                modifiers,
                state,
                repeat,
                is_synthetic,
                ..
            } => self.handle_key(
                &logical,
                text.as_deref(),
                modifiers,
                state,
                repeat,
                is_synthetic,
            ),
            // IME (input-method) events. fcitx5 / CJK / Hangul composition
            // does NOT arrive as `Event::Key` — the composed text comes
            // through these instead, so they must be handled or typing via
            // an IME silently does nothing.
            Event::ImeCommit(text) => self.handle_ime_commit(&text),
            Event::ImePreedit { text, .. } => self.handle_ime_preedit(&text),
            Event::ImeEnabled => {
                // Composition session started; position the candidate popup
                // at the cursor. No preedit to draw yet.
                self.update_ime_cursor_area();
                ControlSignal::Continue
            }
            Event::ImeDisabled => {
                // Session ended — drop any half-formed preedit overlay.
                if let Some(r) = self.renderer.as_mut() {
                    r.set_preedit(None);
                }
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::Focus(focused) => {
                if let Some(bytes) = encode_focus(focused, self.term.report_focus()) {
                    self.write_pty(bytes);
                }
                ControlSignal::Continue
            }
            Event::Mouse {
                button,
                state,
                position,
                modifiers,
            } => self.handle_mouse(button, state, position, modifiers),
            Event::MouseMotion {
                position,
                modifiers,
            } => self.handle_mouse_motion(position, modifiers),
            Event::Scroll {
                kind,
                phase,
                delta_x,
                delta_y,
            } => self.handle_scroll(kind, phase, delta_x, delta_y),
            Event::PtyBytes(bytes) => {
                let fresh_bsu = self.handle_pty_bytes(&bytes);
                // If a BSU just went high we still want to wake the
                // event loop, but at the watchdog deadline rather than
                // immediately — `render_term` would just skip while the
                // pause is active. The watchdog inside
                // `handle_pty_bytes` already fires if the previous BSU
                // had already expired; this branch covers the
                // "BSU-then-silence" case where no further bytes
                // arrive to drive the watchdog.
                //
                // RedrawIn(ZERO) — not Continue — to force the event
                // loop to wake immediately. On macOS, plain
                // `request_redraw()` doesn't wake from Wait reliably;
                // RedrawIn sets ControlFlow::WaitUntil(now) which does.
                if fresh_bsu {
                    ControlSignal::RedrawIn(BSU_TIMEOUT)
                } else {
                    ControlSignal::RedrawIn(Duration::ZERO)
                }
            }
            Event::PtyClosed => {
                debug!("pty closed; exiting");
                if let Some(pty) = self.pty.as_mut() {
                    let _ = pty.wait();
                }
                ControlSignal::Exit
            }
            Event::User => {
                // The config watcher wakes the loop via Wake on file
                // changes. Drain its pending flag (atomic swap) and
                // reload if needed.
                let should_reload = self
                    .config_watcher
                    .as_ref()
                    .is_some_and(ConfigWatcher::take_pending);
                if should_reload {
                    self.reload_config();
                    ControlSignal::RedrawIn(Duration::ZERO)
                } else {
                    ControlSignal::Continue
                }
            }
        }
    }
}

/// True when `(button, modifiers)` is the "open hyperlink under cursor"
/// Convert a linear-light channel value (0.0..1.0) back to an 8-bit
/// sRGB byte. Inverse of `srgb_to_linear` in toastty-render. Used to
/// feed Term's OSC 10/11/12 query handlers a byte triplet apps can
/// understand.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn linear_to_srgb_u8(x: f32) -> u8 {
    let clamped = x.clamp(0.0, 1.0);
    let srgb = if clamped <= 0.003_130_8 {
        12.92 * clamped
    } else {
        1.055 * clamped.powf(1.0 / 2.4) - 0.055
    };
    (srgb * 255.0).round().clamp(0.0, 255.0) as u8
}

/// binding. On macOS it's `Cmd-Left`; on every other platform it's
/// `Ctrl-Left` (matching iTerm2 / Alacritty conventions).
fn is_open_link_binding(button: MouseButton, modifiers: Modifiers) -> bool {
    if button != MouseButton::Left {
        return false;
    }
    // M10-followup I2: require EXACT modifier set. `contains(SUPER)` was
    // true for `Cmd+Shift+Left`, `Cmd+Alt+Left`, etc., so hyperlink open
    // hijacked any combo that happened to include the platform's
    // primary modifier. Tighten to equality so only the bare combo
    // qualifies; users keep `Cmd+Shift+Left` / `Ctrl+Alt+Left` free for
    // selection-extend, window manager shortcuts, etc.
    if cfg!(target_os = "macos") {
        modifiers == Modifiers::SUPER
    } else {
        modifiers == Modifiers::CONTROL
    }
}

/// True when `(logical, modifiers)` is a "copy selection" binding.
///
/// Mirrors `is_paste_binding`: we accept both `Cmd+C` (macOS) and
/// `Ctrl+Shift+C` (Linux) regardless of host. Bare `Ctrl+C` is *not*
/// a copy — it's SIGINT and must always reach the foreground app.
fn is_copy_binding(logical: &LogicalKey, modifiers: Modifiers) -> bool {
    let LogicalKey::Character(s) = logical else {
        return false;
    };
    if !s.eq_ignore_ascii_case("c") {
        return false;
    }
    // Cmd+C — superset OK as long as Ctrl isn't also held (otherwise
    // a user with caps-lock + Ctrl+Cmd+C would be ambiguous). The
    // common cases (bare Cmd+C, Cmd+Shift+C) both pass.
    if modifiers.contains(Modifiers::SUPER) && !modifiers.contains(Modifiers::CONTROL) {
        return true;
    }
    // Ctrl+Shift+C (Linux). Both must be present.
    if modifiers.contains(Modifiers::CONTROL) && modifiers.contains(Modifiers::SHIFT) {
        return true;
    }
    false
}

/// True when `(logical, modifiers)` is a "paste from clipboard" binding.
///
/// We accept both the macOS-canonical `Cmd+V` and the Linux-canonical
/// `Ctrl+Shift+V` regardless of host platform, so users coming from
/// either world feel at home.
fn is_paste_binding(logical: &LogicalKey, modifiers: Modifiers) -> bool {
    let LogicalKey::Character(s) = logical else {
        return false;
    };
    // Accept either upper or lower case "v" — Shift+V comes through as
    // either depending on platform.
    if !s.eq_ignore_ascii_case("v") {
        return false;
    }
    // Cmd+V (macOS): SUPER, no other modifier required.
    if modifiers.contains(Modifiers::SUPER) {
        return true;
    }
    // Ctrl+Shift+V (Linux convention).
    if modifiers.contains(Modifiers::CONTROL) && modifiers.contains(Modifiers::SHIFT) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ch(s: &str) -> LogicalKey {
        LogicalKey::Character(s.to_string())
    }

    #[test]
    fn cmd_v_is_paste() {
        assert!(is_paste_binding(&ch("v"), Modifiers::SUPER));
        assert!(is_paste_binding(&ch("V"), Modifiers::SUPER));
    }

    #[test]
    fn ctrl_shift_v_is_paste() {
        assert!(is_paste_binding(
            &ch("v"),
            Modifiers::CONTROL | Modifiers::SHIFT
        ));
    }

    #[test]
    fn plain_v_is_not_paste() {
        assert!(!is_paste_binding(&ch("v"), Modifiers::empty()));
    }

    #[test]
    fn cmd_c_is_copy_on_any_platform() {
        assert!(is_copy_binding(&ch("c"), Modifiers::SUPER));
        assert!(is_copy_binding(&ch("C"), Modifiers::SUPER));
        // Cmd+Shift+C is also copy — the menu-bar accelerator some
        // users have muscle memory for.
        assert!(is_copy_binding(
            &ch("c"),
            Modifiers::SUPER | Modifiers::SHIFT
        ));
    }

    #[test]
    fn ctrl_shift_c_is_copy() {
        assert!(is_copy_binding(
            &ch("c"),
            Modifiers::CONTROL | Modifiers::SHIFT
        ));
    }

    #[test]
    fn bare_ctrl_c_is_not_copy() {
        // Ctrl+C must remain SIGINT.
        assert!(!is_copy_binding(&ch("c"), Modifiers::CONTROL));
    }

    #[test]
    fn plain_c_is_not_copy() {
        assert!(!is_copy_binding(&ch("c"), Modifiers::empty()));
    }

    #[test]
    fn ctrl_v_without_shift_is_not_paste() {
        // Bare Ctrl+V is the readline "yank" or vim insert-literal — leave
        // it alone.
        assert!(!is_paste_binding(&ch("v"), Modifiers::CONTROL));
    }

    #[test]
    fn cmd_other_key_is_not_paste() {
        assert!(!is_paste_binding(&ch("c"), Modifiers::SUPER));
    }

    #[test]
    fn named_keys_never_paste() {
        assert!(!is_paste_binding(
            &LogicalKey::Named(toastty_window::NamedKey::Enter),
            Modifiers::SUPER
        ));
    }

    // ----- is_open_link_binding truth table (M10.5) -----------------------

    #[test]
    fn left_click_without_modifier_is_not_open_link() {
        assert!(!is_open_link_binding(MouseButton::Left, Modifiers::empty()));
    }

    #[test]
    fn right_click_is_never_open_link() {
        assert!(!is_open_link_binding(MouseButton::Right, Modifiers::SUPER));
        assert!(!is_open_link_binding(
            MouseButton::Right,
            Modifiers::CONTROL
        ));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn cmd_left_is_open_link_on_macos() {
        assert!(is_open_link_binding(MouseButton::Left, Modifiers::SUPER));
        // Ctrl-Left is NOT open-link on macOS.
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL
        ));
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn ctrl_left_is_open_link_on_non_macos() {
        assert!(is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL
        ));
        assert!(!is_open_link_binding(MouseButton::Left, Modifiers::SUPER));
    }

    /// M10-followup I2: the exact-match guard must reject combos that
    /// happen to include the platform's primary modifier. Before the
    /// fix, `Cmd+Shift+Left` (selection-extend on macOS), `Cmd+Alt+Left`
    /// (word-jump), and so on, were all hijacked into hyperlink-open
    /// because `contains(SUPER)` is true for any superset.
    #[cfg(target_os = "macos")]
    #[test]
    fn cmd_with_extra_modifiers_is_not_open_link_on_macos() {
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::SUPER | Modifiers::SHIFT
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::SUPER | Modifiers::ALT
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::SUPER | Modifiers::CONTROL
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::SUPER | Modifiers::SHIFT | Modifiers::ALT
        ));
    }

    /// M10-followup I2: same as above, but for the non-macOS path —
    /// `Ctrl+Alt+Left`, `Ctrl+Shift+Left`, etc., must all reject.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn ctrl_with_extra_modifiers_is_not_open_link_on_non_macos() {
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL | Modifiers::SHIFT
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL | Modifiers::ALT
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL | Modifiers::SUPER
        ));
        assert!(!is_open_link_binding(
            MouseButton::Left,
            Modifiers::CONTROL | Modifiers::SHIFT | Modifiers::ALT
        ));
    }
}
