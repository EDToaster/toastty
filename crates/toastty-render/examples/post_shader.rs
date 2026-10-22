//! Demo: user post-process shaders (M13) over a terminal frame.
//!
//! ```sh
//! cargo run -p toastty-render --example post_shader -- crt.glsl [more.wgsl ...]
//! ```
//!
//! The shaders run in order, as `[shader] paths` would. Keys:
//!
//! - `b`: ring the bell (BEL at the cursor)
//! - arrows: move the cursor
//! - `r`: reload the shader files (errors go to the error banner)
//! - `o`: toggle the debug overlay, `e`: toggle an error banner (both are
//!   UI, drawn over the shaded image)
//! - `a`: cycle `[shader] animation` (auto → always → never)
//! - Esc: exit
//!
//! With no shader arguments the frame renders exactly as without
//! post-processing.

use std::path::PathBuf;
use std::time::Duration;

use pollster::block_on;
use toastty_parser::Parser;
use toastty_render::{PostAnimation, PostShaderSource, RenderOutcome, Renderer};
use toastty_term::Term;
use toastty_window::{
    App, ControlSignal, Event, KeyState, LogicalKey, MouseButton, NamedKey, ToasttyWindow,
    WindowHandle, WindowOptions, run,
};

const COLS: u16 = 80;
const ROWS: u16 = 24;

struct Demo {
    renderer: Option<Renderer>,
    term: Term,
    parser: Parser,
    paths: Vec<PathBuf>,
    overlay: bool,
    banner: bool,
    button1: bool,
    animation: PostAnimation,
}

impl Demo {
    fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// Read and install the shader files; returns the banner text on
    /// failure.
    fn load_shaders(&mut self) -> Option<String> {
        let mut sources = Vec::new();
        for p in &self.paths {
            match std::fs::read_to_string(p) {
                Ok(source) => sources.push(PostShaderSource {
                    path: p.clone(),
                    source,
                }),
                Err(e) => return Some(format!("{}: {e}", p.display())),
            }
        }
        let r = self.renderer.as_mut()?;
        match r.set_post_shaders(&sources) {
            Ok(()) => None,
            Err(errors) => Some(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }

    fn next(&self) -> ControlSignal {
        match self
            .renderer
            .as_ref()
            .and_then(|r| r.next_redraw_deadline(&self.term))
        {
            Some(d) => ControlSignal::RedrawIn(d),
            None => ControlSignal::Continue,
        }
    }
}

impl App for Demo {
    fn init(&mut self, window: ToasttyWindow, _handle: WindowHandle) {
        let size = window.physical_size();
        let mut renderer =
            block_on(Renderer::new(window.clone(), size, true, false)).expect("renderer init");
        renderer.with_font(None, toastty_render::DEFAULT_FONT_SIZE_PX);
        renderer.set_padding(8, 8, 8, 8);
        self.renderer = Some(renderer);
        if let Some(err) = self.load_shaders() {
            eprintln!("{err}");
            if let Some(r) = self.renderer.as_mut() {
                r.set_error_banner(Some(&err));
            }
        }
    }

    fn event(&mut self, event: Event) -> ControlSignal {
        match event {
            Event::Close
            | Event::Key {
                logical: LogicalKey::Named(NamedKey::Escape),
                state: KeyState::Pressed,
                ..
            } => return ControlSignal::Exit,
            Event::Key {
                logical,
                state: KeyState::Pressed,
                ..
            } => match logical {
                LogicalKey::Named(NamedKey::ArrowLeft) => self.feed(b"\x1b[D"),
                LogicalKey::Named(NamedKey::ArrowRight) => self.feed(b"\x1b[C"),
                LogicalKey::Named(NamedKey::ArrowUp) => self.feed(b"\x1b[A"),
                LogicalKey::Named(NamedKey::ArrowDown) => self.feed(b"\x1b[B"),
                LogicalKey::Character(c) => match c.as_str() {
                    "b" => self.feed(b"\x07"),
                    "r" => {
                        let err = self.load_shaders();
                        if let Some(r) = self.renderer.as_mut() {
                            r.set_error_banner(err.as_deref());
                        }
                    }
                    "o" => {
                        self.overlay = !self.overlay;
                        if let Some(r) = self.renderer.as_mut() {
                            r.set_debug_overlay(self.overlay.then_some(" debug overlay "));
                        }
                    }
                    "e" => {
                        self.banner = !self.banner;
                        if let Some(r) = self.renderer.as_mut() {
                            r.set_error_banner(
                                self.banner
                                    .then_some("example.glsl:3:7: an error banner (UI, unshaded)"),
                            );
                        }
                    }
                    "a" => {
                        self.animation = match self.animation {
                            PostAnimation::Auto => PostAnimation::Always,
                            PostAnimation::Always => PostAnimation::Never,
                            PostAnimation::Never => PostAnimation::Auto,
                        };
                        eprintln!("animation: {:?}", self.animation);
                        if let Some(r) = self.renderer.as_mut() {
                            r.set_post_animation(self.animation);
                        }
                    }
                    _ => {}
                },
                _ => {}
            },
            Event::Mouse {
                button: MouseButton::Left,
                state,
                position,
                ..
            } => {
                self.button1 = state == KeyState::Pressed;
                if let Some(r) = self.renderer.as_mut() {
                    #[allow(clippy::cast_possible_truncation)]
                    r.set_pointer((position.0 as f32, position.1 as f32), self.button1);
                }
            }
            Event::MouseMotion { position, .. } => {
                if let Some(r) = self.renderer.as_mut() {
                    #[allow(clippy::cast_possible_truncation)]
                    r.set_pointer((position.0 as f32, position.1 as f32), self.button1);
                }
            }
            Event::Focus(focused) => {
                if let Some(r) = self.renderer.as_mut() {
                    r.set_focused(focused);
                }
            }
            Event::Resize { width, height, .. } => {
                if let Some(r) = self.renderer.as_mut() {
                    r.resize(width, height);
                }
            }
            Event::Redraw => {
                if let Some(r) = self.renderer.as_mut() {
                    match r.render_term(&mut self.term) {
                        Ok(RenderOutcome::Rendered) => self.term.clear_damage(),
                        Ok(RenderOutcome::Skipped) => {}
                        Err(e) => eprintln!("render_term error: {e}"),
                    }
                }
                return self.next();
            }
            _ => return self.next(),
        }
        // Anything handled above may have changed what's on screen.
        ControlSignal::RedrawIn(Duration::ZERO)
    }
}

fn main() {
    let mut paths = Vec::new();
    for a in std::env::args().skip(1) {
        paths.push(PathBuf::from(a));
    }

    let mut term = Term::new(ROWS, COLS, 0);
    let mut parser = Parser::new();
    parser.advance(
        &mut term,
        b"\x1b[1mtoastty post-process shader demo\x1b[0m\r\n\r\n",
    );
    for i in 0..16u8 {
        let s = format!("\x1b[48;5;{i}m  {i:>2}  \x1b[0m");
        parser.advance(&mut term, s.as_bytes());
        if i == 7 {
            parser.advance(&mut term, b"\r\n");
        }
    }
    parser.advance(&mut term, b"\r\n\r\n");
    parser.advance(
        &mut term,
        b"\x1b[31mred\x1b[0m \x1b[32mgreen\x1b[0m \x1b[33myellow\x1b[0m \x1b[34mblue\x1b[0m \x1b[35mmagenta\x1b[0m \x1b[36mcyan\x1b[0m\r\n",
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
    parser.advance(
        &mut term,
        b"b: bell   arrows: cursor   r: reload   o/e: overlays   a: animation\r\n\r\n$ ",
    );

    let demo = Demo {
        renderer: None,
        term,
        parser,
        paths,
        overlay: false,
        banner: false,
        button1: false,
        animation: PostAnimation::Auto,
    };
    let opts = WindowOptions {
        title: "toastty — post_shader demo".into(),
        size: (960, 560),
        ime: false,
        transparent: false,
    };
    if let Err(e) = run(opts, demo) {
        eprintln!("window error: {e}");
        std::process::exit(1);
    }
}
