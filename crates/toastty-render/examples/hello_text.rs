//! Demo: opens a window, builds a 60×20 `Term`, feeds it a few lines of
//! text + colors, places the cursor in the middle, and renders it with
//! the M4b text pipeline.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p toastty-render --example hello_text
//! ```
//!
//! Esc or window-close exits. Resize is honored (full-frame redraw).

use std::time::Duration;

use pollster::block_on;
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
const FONT_SIZE_PX: f32 = 16.0;

struct Demo {
    renderer: Option<Renderer>,
    term: Term,
    window: Option<ToasttyWindow>,
}

impl Demo {
    fn new() -> Self {
        let mut term = Term::new(ROWS, COLS, 0);
        let mut parser = Parser::new();
        // Greeting + color demo + cursor in the middle.
        parser.advance(&mut term, b"toastty M4b text demo\r\n\r\n");
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
        }
    }
}

impl App for Demo {
    fn init(&mut self, window: ToasttyWindow, _handle: WindowHandle) {
        let size = window.physical_size();
        let mut renderer =
            block_on(Renderer::new(window.clone(), size)).expect("renderer init");
        renderer.with_font(Some("Fira Mono"), FONT_SIZE_PX);
        renderer.set_theme(Theme::default_dark());
        tracing::info!(
            "renderer ready: size={size:?} cell={:?}",
            renderer.cell_size(),
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

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let opts = WindowOptions {
        title: "toastty — hello_text demo".into(),
        size: (960, 480),
        ime: true,
    };

    if let Err(e) = run(opts, Demo::new()) {
        eprintln!("window error: {e}");
        std::process::exit(1);
    }
}
