//! Demo: opens a real window via `toastty-window`, initializes the
//! `Renderer`, and animates the clear color through an HSV cycle.
//!
//! Esc or window-close exits cleanly. Resize is honored.
//!
//! Run with:
//!
//! ```sh
//! cargo run -p toastty-render --example clear_color
//! ```

use std::time::{Duration, Instant};

use pollster::block_on;
use toastty_render::{Renderer, color};
use toastty_window::{
    App, ControlSignal, Event, KeyState, LogicalKey, NamedKey, ToasttyWindow, WindowHandle,
    WindowOptions, run,
};

/// One full hue cycle per second feels right for a quick visual smoke test
/// without inducing seizures.
const HUE_CYCLE_SECS: f32 = 4.0;
/// Tick at ~60 Hz while animating.
const FRAME_INTERVAL: Duration = Duration::from_millis(16);

struct Demo {
    renderer: Option<Renderer>,
    start: Instant,
    window: Option<ToasttyWindow>,
}

impl Demo {
    fn new() -> Self {
        Self {
            renderer: None,
            start: Instant::now(),
            window: None,
        }
    }

    fn redraw(&mut self) {
        let Some(renderer) = self.renderer.as_mut() else {
            return;
        };
        let t = self.start.elapsed().as_secs_f32();
        let hue = (t / HUE_CYCLE_SECS).fract();
        let rgb = color::hsv_to_rgb(hue, 0.65, 0.8);
        renderer.set_clear_color([rgb[0], rgb[1], rgb[2], 1.0]);
        if let Err(e) = renderer.render() {
            tracing::warn!("render error: {e}");
        }
    }
}

impl App for Demo {
    fn init(&mut self, window: ToasttyWindow, _handle: WindowHandle) {
        let size = window.physical_size();
        let renderer =
            block_on(Renderer::new(window.clone(), size)).expect("failed to construct renderer");
        tracing::info!(
            "renderer ready: format={:?} size={size:?}",
            renderer.format()
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
                // Force a frame at the new size.
                ControlSignal::RedrawIn(Duration::ZERO)
            }
            Event::Redraw => {
                self.redraw();
                ControlSignal::RedrawIn(FRAME_INTERVAL)
            }
            // Everything else (focus changes, mouse, etc.) — keep the
            // animation loop ticking.
            _ => ControlSignal::RedrawIn(FRAME_INTERVAL),
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
        title: "toastty — clear-color demo".into(),
        size: (800, 500),
        ime: true,
    };

    if let Err(e) = run(opts, Demo::new()) {
        eprintln!("window error: {e}");
        std::process::exit(1);
    }
}
