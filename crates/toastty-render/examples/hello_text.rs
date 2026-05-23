//! Demo: opens a window, builds a 60×20 `Term`, feeds it a few lines of
//! text + colors, places the cursor in the middle, and renders it with
//! the M4b text pipeline.
//!
//! Accepts an optional `--config <path>` arg. With no arg, it tries the
//! XDG-style path (`$XDG_CONFIG_HOME/toastty/config.toml` or
//! `~/.config/toastty/config.toml`) and falls back to
//! [`toastty_config::Config::defaults`] if nothing is there. The source
//! of the loaded config is logged at startup.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p toastty-render --example hello_text
//! cargo run -p toastty-render --example hello_text -- --config path/to.toml
//! ```
//!
//! Esc or window-close exits. Resize is honored (full-frame redraw).

use std::path::PathBuf;
use std::time::Duration;

use pollster::block_on;
use toastty_config::{Config, ConfigSource, ThemeConfig};
use toastty_parser::Parser;
use toastty_render::Renderer;
use toastty_render::text::instance::Theme;
use toastty_term::Term;
use toastty_window::{
    App, ControlSignal, Event, KeyState, LogicalKey, NamedKey, ToasttyWindow, WindowHandle,
    WindowOptions, run,
};

const COLS: u16 = 60;
const ROWS: u16 = 20;

/// Convert a `toastty_config::ThemeConfig` into the renderer's `Theme`.
///
/// This bridge lives in the demo (not in `toastty-config` or
/// `toastty-render`) so neither crate has to depend on the other —
/// keeping `toastty-config` a leaf crate and `toastty-render` GPU-only.
/// In the real `toastty` binary the same function will live next to
/// `main.rs`.
fn theme_from_config(cfg: &ThemeConfig) -> Theme {
    let mut palette = [[0.0; 4]; 16];
    for (i, c) in cfg.palette.iter().enumerate() {
        palette[i] = c.as_array();
    }
    Theme {
        fg: cfg.fg.as_array(),
        bg: cfg.bg.as_array(),
        cursor: cfg.cursor.as_array(),
        palette,
    }
}

struct Demo {
    renderer: Option<Renderer>,
    term: Term,
    window: Option<ToasttyWindow>,
    config: Config,
}

impl Demo {
    fn new(config: Config) -> Self {
        let mut term = Term::new(ROWS, COLS, 0);
        let mut parser = Parser::new();
        // Greeting + color demo + cursor in the middle.
        parser.advance(&mut term, b"toastty M4.5 config demo\r\n\r\n");
        parser.advance(
            &mut term,
            b"\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[33myellow\x1b[0m ",
        );
        parser.advance(
            &mut term,
            b"\x1b[34mblue\x1b[0m \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m\r\n",
        );
        parser.advance(
            &mut term,
            b"\x1b[1;91mbold bright red\x1b[0m \x1b[7minverted\x1b[0m\r\n\r\n",
        );
        parser.advance(
            &mut term,
            b"the quick brown fox jumps over the lazy dog\r\n",
        );
        parser.advance(
            &mut term,
            b"THE QUICK BROWN FOX JUMPS OVER THE LAZY DOG\r\n",
        );
        parser.advance(&mut term, b"0123456789 !@#$%^&*() {}[]<>/?\r\n\r\n");
        // Cursor in the middle: row 12, col 28 (1-based).
        parser.advance(&mut term, b"\x1b[12;28HHello, toastty!");
        // Park the cursor a few columns to the left so it sits over the !.
        parser.advance(&mut term, b"\x1b[12;42H");

        Self {
            renderer: None,
            term,
            window: None,
            config,
        }
    }
}

impl App for Demo {
    fn init(&mut self, window: ToasttyWindow, _handle: WindowHandle) {
        let size = window.physical_size();
        let mut renderer = block_on(Renderer::new(window.clone(), size)).expect("renderer init");

        // Plumb config → renderer:
        //   font.family + font.size_px + font.line_height → with_font_ex
        //   theme (fg/bg/cursor/palette)                  → set_theme
        renderer.with_font_ex(
            Some(self.config.font.family.as_str()),
            self.config.font.size_px,
            self.config.font.line_height,
        );
        renderer.set_theme(theme_from_config(&self.config.theme));

        tracing::info!(
            "renderer ready: size={size:?} cell={:?} font={:?} {}px line_height={}",
            renderer.cell_size(),
            self.config.font.family,
            self.config.font.size_px,
            self.config.font.line_height,
        );
        self.renderer = Some(renderer);
        self.window = Some(window);
    }

    fn event(&mut self, event: Event) -> ControlSignal {
        match event {
            Event::Close
            | Event::Key {
                logical: LogicalKey::Named(NamedKey::Escape),
                state: KeyState::Pressed,
                ..
            } => ControlSignal::Exit,
            Event::Resize { width, height, .. } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(width, height);
                }
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::Redraw => {
                if let Some(r) = self.renderer.as_mut()
                    && let Err(e) = r.render_term(&self.term)
                {
                    tracing::warn!("render_term error: {e}");
                }
                // M4b: full-frame redraw, but no animation — wait for
                // events rather than polling at 60 Hz.
                ControlSignal::Continue
            }
            _ => ControlSignal::Continue,
        }
    }
}

fn parse_args() -> Option<PathBuf> {
    // Single optional arg: `--config <path>`. We deliberately avoid
    // pulling in `clap` for the demo.
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == "--config" {
            return args.next().map(PathBuf::from);
        }
        if let Some(rest) = a.strip_prefix("--config=") {
            return Some(PathBuf::from(rest));
        }
    }
    None
}

fn load_config(explicit: Option<&std::path::Path>) -> (Config, ConfigSource) {
    if let Some(p) = explicit {
        match Config::load_from_path(p) {
            Ok(cfg) => (cfg, ConfigSource::File(p.to_path_buf())),
            Err(e) => {
                eprintln!(
                    "config: failed to load {}: {e}; falling back to defaults",
                    p.display()
                );
                (Config::defaults(), ConfigSource::Defaults)
            }
        }
    } else {
        Config::load_default()
    }
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let explicit = parse_args();
    let (config, source) = load_config(explicit.as_deref());
    tracing::info!("config source: {source}");

    let opts = WindowOptions {
        title: "toastty — hello_text demo".into(),
        size: (960, 480),
        ime: true,
    };

    if let Err(e) = run(opts, Demo::new(config)) {
        eprintln!("window error: {e}");
        std::process::exit(1);
    }
}
