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

use std::time::Duration;

use anyhow::{Context, Result};
use pollster::block_on;
use toastty_config::{Config, ConfigSource};
use toastty_parser::Parser;
use toastty_pty::{Pty, PtySpec, WinSize};
use toastty_render::Renderer;
use toastty_term::Term;
use toastty_window::{
    App, ControlSignal, Event, KeyState, ToasttyWindow, WindowHandle, WindowOptions, run,
};
use tracing::{debug, info, warn};
use tracing_subscriber::EnvFilter;

use toastty::cli;
use toastty::geometry::grid_dims_from_pixels;
use toastty::keyboard::encode_key;
use toastty::shell::resolve_shell;
use toastty::theme_bridge::theme_from_config;

/// Default initial window size in pixels. M5 does not yet read this from
/// a `[window]` config section — that lands in M6.
const DEFAULT_WINDOW_SIZE: (u32, u32) = (1280, 800);

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
    match action {
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
        cli::Action::Run => {}
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    info!("toastty {} starting", env!("CARGO_PKG_VERSION"));

    let (config, source) = Config::load_default();
    info!("config source: {source}");
    if matches!(source, ConfigSource::Defaults) {
        debug!("running with built-in defaults");
    }

    let opts = WindowOptions {
        title: "toastty".into(),
        size: DEFAULT_WINDOW_SIZE,
        ime: true,
    };

    let app = Toastty::new(config);
    run(opts, app).context("window run")?;
    Ok(())
}

/// Running state of the terminal. All initialisation that needs a window
/// handle happens inside `App::init`.
struct Toastty {
    config: Config,
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
}

impl Toastty {
    fn new(config: Config) -> Self {
        let scrollback = config.scrollback.lines.try_into().unwrap_or(u16::MAX);
        // Start at a tiny grid; init() resizes once we know cell dimensions.
        let term = Term::new(24, 80, scrollback);
        Self {
            config,
            window: None,
            renderer: None,
            parser: Parser::new(),
            term,
            pty: None,
            reader: None,
            physical_size: DEFAULT_WINDOW_SIZE,
        }
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

    fn handle_pty_bytes(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
        // Redraw is requested via ControlSignal::RedrawIn(ZERO) from the
        // PtyBytes handler — that path wakes the event loop reliably on
        // macOS. Bare `request_redraw()` + ControlSignal::Continue queues
        // a redraw via setNeedsDisplay but doesn't wake the loop until
        // the next external event, leaving the window stale until a
        // keystroke. See `Event::PtyBytes` below.
    }
}

impl App for Toastty {
    fn init(&mut self, window: ToasttyWindow, handle: WindowHandle) {
        let size = window.physical_size();
        self.physical_size = size;

        // Build the renderer.
        let mut renderer = match block_on(Renderer::new(window.clone(), size)) {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("renderer init failed: {e}");
                return;
            }
        };
        renderer.with_font_ex(
            Some(self.config.font.family.as_str()),
            self.config.font.size_px,
            self.config.font.line_height,
        );
        renderer.set_theme(theme_from_config(&self.config.theme));
        info!(
            "renderer ready: size={size:?} cell={:?} font={:?} {}px",
            renderer.cell_size(),
            self.config.font.family,
            self.config.font.size_px,
        );

        // Compute initial grid from cell size + physical size.
        let (cell_w, cell_h) = renderer.cell_size();
        let (cols, rows) = grid_dims_from_pixels(size.0, size.1, cell_w, cell_h);
        self.term.resize(rows, cols);

        // Spawn the PTY.
        let (program, args) = resolve_shell(&self.config.shell);
        info!(?program, ?args, rows, cols, "spawning shell");
        let pixel_width = u16::try_from(size.0).unwrap_or(u16::MAX);
        let pixel_height = u16::try_from(size.1).unwrap_or(u16::MAX);
        let spec = PtySpec::program(program)
            .args(args)
            .with_current_env()
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

        // Kick off the first frame. macOS doesn't always fire an
        // initial RedrawRequested on first display — without this,
        // the window stays black until the user moves the mouse or
        // types a key.
        if let Some(w) = self.window.as_ref() {
            w.request_redraw();
        }
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
            Event::Resize { width, height, .. } => {
                self.physical_size = (width, height);
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(width, height);
                }
                self.resync_grid();
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::Redraw => {
                if let Some(r) = self.renderer.as_mut() {
                    if let Err(e) = r.render_term(&self.term) {
                        warn!("render_term error: {e}");
                    }
                    // Consume the per-row damage signal so the next
                    // render only re-shapes rows that changed.
                    self.term.clear_dirty();
                }
                ControlSignal::Continue
            }
            Event::Key {
                logical,
                text,
                modifiers,
                state: KeyState::Pressed,
                ..
            } => {
                if let Some(bytes) = encode_key(&logical, text.as_deref(), modifiers)
                    && let Some(pty) = self.pty.as_ref()
                    && let Err(e) = pty.write(&bytes)
                {
                    warn!("pty write failed: {e}");
                }
                ControlSignal::Continue
            }
            Event::PtyBytes(bytes) => {
                self.handle_pty_bytes(&bytes);
                // RedrawIn(ZERO) — not Continue — to force the event
                // loop to wake immediately. On macOS, plain
                // `request_redraw()` doesn't wake from Wait reliably;
                // RedrawIn sets ControlFlow::WaitUntil(now) which does.
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::PtyClosed => {
                debug!("pty closed; exiting");
                if let Some(pty) = self.pty.as_mut() {
                    let _ = pty.wait();
                }
                ControlSignal::Exit
            }
            // Mouse / scroll / focus / synthetic-release: nothing wired yet.
            // TODO(mouse-forwarding): SGR mouse → PTY (M6).
            // TODO(selection): drag-to-select + copy/paste (M7).
            _ => ControlSignal::Continue,
        }
    }
}
