//! TUI application state machine and event loop.
//!
//! ratatui event loop: an input box for the formula/name, a large 3D
//! viewport region (the molecule renders there via RGP), and a status/
//! help line. State machine: `Input` → `Loading` (worker thread) →
//! `Disambiguate` (candidate list) → `Viewing`.
//!
//! Network fetches run on a `std::thread` worker (mpsc channel) so the UI
//! never blocks. Mouse drag / scroll / keyboard events are handled inside
//! the ~30 ms poll loop.

use std::io::{Write, stdout};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Result;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    Event, KeyCode, KeyEventKind, KeyModifiers, MouseButton, MouseEventKind,
};
use ratatui::crossterm::execute;
use ratatui::layout::Rect;
use ratatui::widgets::ListState;

use crate::model::ColoredMesh;
use crate::pubchem::Candidate;
use crate::ui::{self, DrawMode, RgpAnchor};
use crate::{geometry, glb, pubchem, rgp, sdf};

// ─────────────────────────────────────────────────────────────────────────────
// Worker messages
// ─────────────────────────────────────────────────────────────────────────────

/// Messages that the worker thread sends back to the main loop.
enum WorkerMsg {
    /// `PubChem` candidate list for a query.
    Candidates(anyhow::Result<Vec<Candidate>>),
    /// Raw SDF text for a chosen CID.
    SdfText(anyhow::Result<String>),
}

// ─────────────────────────────────────────────────────────────────────────────
// Application state
// ─────────────────────────────────────────────────────────────────────────────

/// Which major mode the app is in.
enum Mode {
    /// Text-entry for query.
    Input,
    /// Waiting for a network response (search or fetch).
    Loading,
    /// Show candidates and let the user pick one.
    Disambiguate,
    /// A molecule is placed via RGP.
    Viewing,
}

/// Placed RGP objects: id + the original colored mesh (for re-placing on resize).
struct PlacedMesh {
    id: u32,
    cm: ColoredMesh,
}

struct App {
    mode: Mode,
    /// Current text in the input box.
    input: String,
    /// Pending status message for brief error display.
    status: Option<String>,

    // Disambiguation
    candidates: Vec<Candidate>,
    list_state: ListState,

    // Worker channel
    tx: mpsc::SyncSender<WorkerMsg>,
    rx: mpsc::Receiver<WorkerMsg>,

    // Viewing
    placed: Vec<PlacedMesh>,
    yaw: f32,
    pitch: f32,
    zoom: f32,
    animate: bool,

    // Last mouse drag position.
    last_drag: Option<(u16, u16)>,

    // Last known viewport inner rect (for re-place on resize).
    viewport_rect: Rect,
}

impl App {
    fn new() -> Self {
        let (tx, rx) = mpsc::sync_channel(8);
        App {
            mode: Mode::Input,
            input: String::new(),
            status: None,
            candidates: Vec::new(),
            list_state: ListState::default(),
            tx,
            rx,
            placed: Vec::new(),
            yaw: 0.0,
            pitch: 0.0,
            zoom: 1.0,
            animate: false,
            last_drag: None,
            viewport_rect: Rect::default(),
        }
    }

    // ── query submission ─────────────────────────────────────────────────

    fn submit_search(&mut self) {
        let query = self.input.trim().to_string();
        if query.is_empty() {
            return;
        }
        self.mode = Mode::Loading;
        self.status = None;

        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = pubchem::search(&query);
            let _ = tx.send(WorkerMsg::Candidates(result));
        });
    }

    fn fetch_cid(&mut self, cid: u32) {
        self.mode = Mode::Loading;
        let tx = self.tx.clone();
        std::thread::spawn(move || {
            let result = pubchem::fetch_sdf_3d(cid);
            let _ = tx.send(WorkerMsg::SdfText(result));
        });
    }

    // ── RGP helpers ──────────────────────────────────────────────────────

    fn anchor(&self) -> RgpAnchor {
        ui::viewport_anchor(self.viewport_rect)
    }

    fn register_and_place_meshes(&mut self, meshes: Vec<ColoredMesh>) -> std::io::Result<()> {
        // Delete any previous RGP objects.
        {
            let stdout = stdout();
            let mut out = stdout.lock();
            rgp::delete_all(&mut out)?;
            out.flush()?;
        }

        self.placed.clear();
        self.yaw = 0.0;
        self.pitch = 0.0;
        self.zoom = 1.0;
        self.animate = false;

        let anchor = self.anchor();
        let stdout = stdout();
        let mut out = stdout.lock();

        for (i, cm) in meshes.into_iter().enumerate() {
            let id = u32::try_from(i + 1).expect("mesh count fits in u32");
            let glb_bytes = glb::write(&cm.mesh);
            rgp::register_payload(&mut out, id, &glb_bytes)?;
            rgp::place(
                &mut out,
                id,
                &rgp::Placement {
                    row: anchor.row,
                    col: anchor.col,
                    w: anchor.w,
                    h: anchor.h,
                    depth: -5.0,
                    scale: self.zoom,
                    rx: self.pitch,
                    ry: self.yaw,
                    rz: 0.0,
                    color: cm.color,
                    animate: self.animate,
                },
            )?;
            self.placed.push(PlacedMesh { id, cm });
        }
        out.flush()?;
        Ok(())
    }

    fn update_transforms(&self) -> std::io::Result<()> {
        let stdout = stdout();
        let mut out = stdout.lock();
        for pm in &self.placed {
            rgp::update_transform(&mut out, pm.id, self.pitch, self.yaw, 0.0, self.zoom)?;
        }
        out.flush()?;
        Ok(())
    }

    fn set_animate_all(&self) -> std::io::Result<()> {
        let stdout = stdout();
        let mut out = stdout.lock();
        for pm in &self.placed {
            rgp::set_animate(&mut out, pm.id, self.animate)?;
        }
        out.flush()?;
        Ok(())
    }

    fn replace_all(&self) -> std::io::Result<()> {
        let anchor = self.anchor();
        let stdout = stdout();
        let mut out = stdout.lock();
        for pm in &self.placed {
            rgp::place(
                &mut out,
                pm.id,
                &rgp::Placement {
                    row: anchor.row,
                    col: anchor.col,
                    w: anchor.w,
                    h: anchor.h,
                    depth: -5.0,
                    scale: self.zoom,
                    rx: self.pitch,
                    ry: self.yaw,
                    rz: 0.0,
                    color: pm.cm.color,
                    animate: self.animate,
                },
            )?;
        }
        out.flush()?;
        Ok(())
    }

    fn cleanup_rgp() {
        let so = stdout();
        let mut out = so.lock();
        let _ = rgp::delete_all(&mut out);
        let _ = out.flush();
    }

    // ── event handlers ───────────────────────────────────────────────────

    /// Returns `true` if the app should quit.
    fn handle_key(&mut self, code: KeyCode, modifiers: KeyModifiers) -> bool {
        match &self.mode {
            Mode::Input => match code {
                KeyCode::Enter => {
                    self.submit_search();
                    false
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => true,
                // Esc quits; bare 'q' is a normal character so names like
                // "quinine" / "ubiquinone" can be typed or pasted.
                KeyCode::Esc => true,
                KeyCode::Backspace => {
                    self.input.pop();
                    false
                }
                KeyCode::Char(c) => {
                    self.input.push(c);
                    false
                }
                _ => false,
            },
            Mode::Loading => match code {
                KeyCode::Char('q') | KeyCode::Esc => true,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => true,
                _ => false,
            },
            Mode::Disambiguate => match code {
                KeyCode::Up => {
                    self.list_state.select_previous();
                    false
                }
                KeyCode::Down => {
                    self.list_state.select_next();
                    false
                }
                KeyCode::Enter => {
                    if let Some(idx) = self.list_state.selected()
                        && let Some(c) = self.candidates.get(idx)
                    {
                        let cid = c.cid;
                        self.fetch_cid(cid);
                    }
                    false
                }
                KeyCode::Char('q') | KeyCode::Esc => {
                    self.mode = Mode::Input;
                    false
                }
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => true,
                _ => false,
            },
            Mode::Viewing => match code {
                KeyCode::Char('q') | KeyCode::Esc => true,
                KeyCode::Char('c') if modifiers.contains(KeyModifiers::CONTROL) => true,
                KeyCode::Char('/') => {
                    // New search: delete RGP objects, go back to Input.
                    App::cleanup_rgp();
                    self.placed.clear();
                    self.input.clear();
                    // Drop any in-progress drag origin so re-entering
                    // Viewing later doesn't jump from a stale position.
                    self.last_drag = None;
                    self.mode = Mode::Input;
                    false
                }
                KeyCode::Char('a') => {
                    self.animate = !self.animate;
                    let _ = self.set_animate_all();
                    false
                }
                KeyCode::Char('r') => {
                    self.yaw = 0.0;
                    self.pitch = 0.0;
                    self.zoom = 1.0;
                    let _ = self.update_transforms();
                    false
                }
                _ => false,
            },
        }
    }

    /// Append pasted text to the input box (Input mode only). Takes the
    /// first line and drops control characters — the box is a single-line
    /// formula/name field, so a multi-line or newline-terminated paste
    /// shouldn't inject newlines or prematurely submit.
    fn handle_paste(&mut self, text: &str) {
        if !matches!(self.mode, Mode::Input) {
            return;
        }
        for c in text.chars() {
            if c == '\n' || c == '\r' {
                break;
            }
            if !c.is_control() {
                self.input.push(c);
            }
        }
    }

    fn handle_mouse(&mut self, kind: MouseEventKind, col: u16, row: u16) {
        match kind {
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some((lx, ly)) = self.last_drag {
                    let dx = col as f32 - lx as f32;
                    let dy = row as f32 - ly as f32;
                    self.yaw += dx * 4.0;
                    self.pitch += dy * 4.0;
                    if matches!(self.mode, Mode::Viewing) {
                        let _ = self.update_transforms();
                    }
                }
                self.last_drag = Some((col, row));
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.last_drag = None;
            }
            MouseEventKind::ScrollUp => {
                self.zoom = (self.zoom * 1.1).min(5.0);
                if matches!(self.mode, Mode::Viewing) {
                    let _ = self.update_transforms();
                }
            }
            MouseEventKind::ScrollDown => {
                self.zoom = (self.zoom / 1.1).max(0.2);
                if matches!(self.mode, Mode::Viewing) {
                    let _ = self.update_transforms();
                }
            }
            _ => {
                // Reset drag tracking on non-drag mouse events.
                if !matches!(kind, MouseEventKind::Drag(_)) {
                    self.last_drag = None;
                }
            }
        }
    }

    // ── worker channel drain ─────────────────────────────────────────────

    /// Drain all pending worker messages. Returns `Err` if we should abort.
    fn drain_worker(&mut self) -> Result<()> {
        loop {
            match self.rx.try_recv() {
                Ok(WorkerMsg::Candidates(res)) => match res {
                    Ok(candidates) if candidates.len() == 1 => {
                        // Skip disambiguation; go straight to fetch.
                        let cid = candidates[0].cid;
                        self.fetch_cid(cid);
                    }
                    Ok(candidates) if candidates.is_empty() => {
                        self.status = Some("No results found.".to_string());
                        self.mode = Mode::Input;
                    }
                    Ok(candidates) => {
                        self.candidates = candidates;
                        self.list_state = ListState::default();
                        self.list_state.select(Some(0));
                        self.mode = Mode::Disambiguate;
                    }
                    Err(e) => {
                        self.status = Some(format!("Search error: {e}"));
                        self.mode = Mode::Input;
                    }
                },
                Ok(WorkerMsg::SdfText(res)) => match res {
                    Ok(sdf_text) => match sdf::parse_sdf(&sdf_text) {
                        Ok(mol) => {
                            let meshes = geometry::build(&mol);
                            if let Err(e) = self.register_and_place_meshes(meshes) {
                                self.status = Some(format!("RGP error: {e}"));
                                self.mode = Mode::Input;
                            } else {
                                self.mode = Mode::Viewing;
                            }
                        }
                        Err(e) => {
                            self.status = Some(format!("Parse error: {e}"));
                            self.mode = Mode::Input;
                        }
                    },
                    Err(e) => {
                        self.status = Some(format!("Fetch error: {e}"));
                        self.mode = Mode::Input;
                    }
                },
                Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => break,
            }
        }
        Ok(())
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Entry point
// ─────────────────────────────────────────────────────────────────────────────

/// Run the interactive TUI. Returns when the user quits.
pub fn run() -> Result<()> {
    // ratatui::init() enables raw mode, enters the alternate screen, and
    // installs a panic hook that calls ratatui::restore() before panicking.
    // We then additionally enable mouse capture.
    let mut terminal = ratatui::init();
    execute!(stdout(), EnableMouseCapture, EnableBracketedPaste)?;

    // Override the panic hook installed by ratatui::init() so we also call
    // delete_all and DisableMouseCapture before panicking.
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Best-effort cleanup of RGP objects and mouse capture.
        {
            let so = stdout();
            let mut out = so.lock();
            let _ = rgp::delete_all(&mut out);
            let _ = out.flush();
        }
        let _ = execute!(stdout(), DisableMouseCapture, DisableBracketedPaste);
        // ratatui::restore() is called by the hook we replaced (prev_hook
        // captured ratatui's hook which calls restore).
        prev_hook(info);
    }));

    let result = run_loop(&mut terminal);

    // ── cleanup ──────────────────────────────────────────────────────────
    // Delete all RGP objects and disable mouse capture before restoring.
    {
        let stdout = stdout();
        let mut out = stdout.lock();
        let _ = rgp::delete_all(&mut out);
        let _ = out.flush();
    }
    let _ = execute!(stdout(), DisableMouseCapture);
    ratatui::restore();

    result
}

/// Inner event loop, separated so that cleanup in `run()` always executes.
fn run_loop(terminal: &mut ratatui::DefaultTerminal) -> Result<()> {
    let mut app = App::new();

    loop {
        // ── draw ─────────────────────────────────────────────────────────
        let draw_mode: DrawMode<'_> = match app.mode {
            Mode::Input => DrawMode::Input,
            Mode::Loading => DrawMode::Loading,
            Mode::Disambiguate => DrawMode::Disambiguate {
                candidates: &app.candidates,
                list_state: &mut app.list_state,
            },
            Mode::Viewing => DrawMode::Viewing,
        };

        // We need to capture the viewport rect from inside the draw closure.
        let mut viewport_inner = Rect::default();
        terminal.draw(|frame| {
            viewport_inner = ui::draw(frame, &app.input, draw_mode);
        })?;

        // If the viewport changed (or first frame), re-place all objects.
        if viewport_inner != app.viewport_rect {
            app.viewport_rect = viewport_inner;
            if matches!(app.mode, Mode::Viewing) {
                let _ = app.replace_all();
            }
        }

        // ── drain worker ─────────────────────────────────────────────────
        app.drain_worker()?;

        // ── events (non-blocking, ~33 ms timeout) ────────────────────────
        if event::poll(Duration::from_millis(33))? {
            match event::read()? {
                Event::Key(key)
                    if key.kind == KeyEventKind::Press
                        && app.handle_key(key.code, key.modifiers) =>
                {
                    break;
                }
                Event::Mouse(me) => {
                    app.handle_mouse(me.kind, me.column, me.row);
                }
                Event::Paste(text) => {
                    app.handle_paste(&text);
                }
                // Event::Resize: the next draw will recompute viewport_inner
                // and re-place if the rect changed.
                _ => {}
            }
        }

        // Drain worker again after event handling (avoids an extra frame
        // delay on fast networks).
        app.drain_worker()?;
    }

    Ok(())
}
