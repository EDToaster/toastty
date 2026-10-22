//! Thin winit wrapper.
//!
//! Owns the platform realities winit doesn't surface cleanly:
//! Caps/Num Lock LED state, macOS dead-key routing through IME,
//! Wayland `RedrawRequested` cadence. See
//! [`docs/decisions/window-input.md`](../../docs/decisions/window-input.md).
//!
//! ## Why our own event enum
//!
//! Only this crate depends on `winit`. The renderer, dispatcher, and
//! binary consume [`Event`] which mirrors only the variants we use. This
//! keeps winit's API churn (0.30 → 0.31-beta is materially breaking) off
//! the rest of the workspace.
//!
//! ## Power friendliness
//!
//! [`run`] uses `ControlFlow::Wait` by default. Returning
//! [`ControlSignal::RedrawIn(Duration)`] from the app callback switches to
//! `ControlFlow::WaitUntil(now + d)` for that frame only. No constant
//! 60Hz redraw when nothing is happening.

// `deny` rather than `forbid` so the `led` module can opt back in to
// unsafe FFI for platform LED-state reads (a narrowly-scoped need —
// see `led/mod.rs`).
#![deny(unsafe_code)]

mod event;
pub mod led;

pub use event::{
    ControlSignal, Event, KeyState, LogicalKey, Modifiers, MouseButton, NamedKey, PhysicalKey,
    ScrollKind, ScrollPhase,
};
// Re-exported so downstream crates (toastty bin) can ship background
// threads that push events into the event loop without taking a direct
// winit dependency.
pub use winit::event_loop::EventLoopProxy;

use std::sync::Arc;
use std::time::Instant;

use raw_window_handle::{
    HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};
use thiserror::Error;
use tracing::{debug, trace, warn};
use winit::application::ApplicationHandler;
use winit::event::{
    ElementState, Ime, KeyEvent, MouseScrollDelta, StartCause, TouchPhase, WindowEvent,
};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey as WNamedKey, PhysicalKey as WPhysicalKey};
use winit::platform::modifier_supplement::KeyEventExtModifierSupplement;
use winit::window::{Window, WindowId};

/// Configuration for the window opened by [`run`].
#[derive(Debug, Clone)]
pub struct WindowOptions {
    pub title: String,
    pub size: (u32, u32),
    /// Whether IME composition is allowed. **Default is `true`** per the
    /// decision record — macOS dead keys route through IME, so disabling
    /// it silently breaks Option-key typing.
    pub ime: bool,
    /// Request a transparent window surface so a sub-1.0 background alpha
    /// composites over the desktop. Default false.
    pub transparent: bool,
}

impl Default for WindowOptions {
    fn default() -> Self {
        Self {
            title: "toastty".into(),
            size: (1024, 640),
            ime: true,
            transparent: false,
        }
    }
}

/// Errors from [`run`].
#[derive(Debug, Error)]
pub enum WindowError {
    #[error("winit event loop: {0}")]
    EventLoop(#[from] winit::error::EventLoopError),
    #[error("winit os: {0}")]
    Os(#[from] winit::error::OsError),
}

/// Custom user-event payload — sent via [`WindowHandle::wake`] from
/// background threads (e.g. the mio PTY thread).
///
/// `PtyReady` corresponds 1:1 to [`toastty_io::UserEvent`]; the `From`
/// impl below makes the `spawn_pty_reader<EventLoopProxy<UserEvent>>`
/// call site work without any per-call conversion.
#[derive(Debug)]
pub enum UserEvent {
    /// Wake the event loop. Surfaces as [`Event::User`].
    Wake,
    /// The PTY reader has bytes (or the close) for the main thread, from
    /// `toastty-io::spawn_pty_reader`. Surfaces as [`Event::PtyReady`].
    PtyReady,
}

impl From<toastty_io::UserEvent> for UserEvent {
    fn from(ev: toastty_io::UserEvent) -> Self {
        match ev {
            toastty_io::UserEvent::PtyReady => UserEvent::PtyReady,
        }
    }
}

/// Cloneable handle to the running event loop. Send across threads to wake
/// the loop without blocking on a window-bound mutex.
///
/// Construct one with [`run`] — it's passed to [`App::init`].
#[derive(Debug, Clone)]
pub struct WindowHandle {
    proxy: EventLoopProxy<UserEvent>,
}

/// The event loop has already exited; the wake was not delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
#[error("event loop closed")]
pub struct EventLoopClosed;

impl WindowHandle {
    /// Wake the event loop. The next event delivered to the app callback
    /// will be [`Event::User`].
    ///
    /// Returns `Err` if the event loop has already exited.
    pub fn wake(&self) -> Result<(), EventLoopClosed> {
        self.proxy
            .send_event(UserEvent::Wake)
            .map_err(|_| EventLoopClosed)
    }

    /// Borrow the underlying winit `EventLoopProxy<UserEvent>`. Pass
    /// this directly to `toastty_io::spawn_pty_reader` — the
    /// `From<toastty_io::UserEvent> for UserEvent` impl above satisfies
    /// the bound.
    pub fn event_loop_proxy(&self) -> EventLoopProxy<UserEvent> {
        self.proxy.clone()
    }
}

/// App trait — implement this and pass to [`run`].
///
/// [`App::init`] runs after the window is created, giving you a chance to
/// construct anything that needs a window handle (e.g. a wgpu surface).
/// [`App::event`] runs for every event.
pub trait App {
    /// Called once, after the window opens. Receives a cloneable window
    /// handle (for wgpu surface creation) and an event-loop wake handle
    /// (for the mio PTY thread).
    fn init(&mut self, _window: ToasttyWindow, _handle: WindowHandle) {}

    /// Called for every event. Returning [`ControlSignal::Exit`] stops
    /// the loop; [`ControlSignal::RedrawIn`] schedules a redraw deadline;
    /// [`ControlSignal::Continue`] waits for the next event.
    fn event(&mut self, event: Event) -> ControlSignal;
}

/// Cloneable, type-erased window handle.
///
/// Holds an `Arc<winit::window::Window>` internally so the renderer can
/// own a copy (for `wgpu::Instance::create_surface`) while the wrapper
/// keeps another for `request_redraw` etc.
///
/// Only this crate depends on `winit`; downstream code sees just the
/// `HasDisplayHandle + HasWindowHandle` impls.
#[derive(Debug, Clone)]
pub struct ToasttyWindow {
    inner: Arc<Window>,
}

impl ToasttyWindow {
    /// Physical (pixel) size of the window, suitable for sizing a wgpu
    /// surface.
    pub fn physical_size(&self) -> (u32, u32) {
        let s = self.inner.inner_size();
        (s.width, s.height)
    }

    /// Current scale factor, including fractional values delivered via
    /// `wp-fractional-scale-v1` on Wayland (winit 0.29.3+).
    pub fn scale_factor(&self) -> f64 {
        self.inner.scale_factor()
    }

    /// Toggle IME on the underlying window. Useful for the
    /// "IME on by default, with named inhibitors" pattern (decision §2).
    pub fn set_ime_allowed(&self, allowed: bool) {
        self.inner.set_ime_allowed(allowed);
    }

    /// Tell the IME where the text cursor is (physical pixels) so the
    /// candidate/preedit popup is positioned near it. Call this when preedit
    /// updates or the cursor moves while IME is active.
    pub fn set_ime_cursor_area(&self, x: f64, y: f64, width: f64, height: f64) {
        let pos = winit::dpi::PhysicalPosition::new(x, y);
        let size = winit::dpi::PhysicalSize::new(width, height);
        // Pass the physical position/size straight through: winit's
        // `set_ime_cursor_area<P: Into<Position>, S: Into<Size>>` infers the
        // concrete types from these, so an explicit `.into()` would be
        // ambiguous (the target `Position`/`Size` isn't otherwise pinned).
        self.inner.set_ime_cursor_area(pos, size);
    }

    /// Show the pointing-hand cursor (`true`, over a clickable link) or
    /// the default cursor (`false`). Callers should only call this on a
    /// change; some backends round-trip to the compositor.
    pub fn set_pointer_cursor(&self, pointer: bool) {
        let icon = if pointer {
            winit::window::CursorIcon::Pointer
        } else {
            winit::window::CursorIcon::Default
        };
        self.inner.set_cursor(icon);
    }

    /// Request a redraw. Honors winit's coalescing.
    pub fn request_redraw(&self) {
        self.inner.request_redraw();
    }

    /// Set the window title (OSC 0 / 2 target).
    ///
    /// `winit::Window::set_title` is moderately expensive on some
    /// backends (it round-trips through the compositor). Callers should
    /// only invoke this when the title actually changed.
    pub fn set_title(&self, title: &str) {
        self.inner.set_title(title);
    }
}

impl HasWindowHandle for ToasttyWindow {
    fn window_handle(&self) -> Result<raw_window_handle::WindowHandle<'_>, HandleError> {
        self.inner.window_handle()
    }
}

impl HasDisplayHandle for ToasttyWindow {
    fn display_handle(&self) -> Result<raw_window_handle::DisplayHandle<'_>, HandleError> {
        self.inner.display_handle()
    }
}

/// Run the event loop, blocking until the app signals exit or the user
/// closes the window. The window is opened with `options`; events are
/// translated and delivered to `app`.
pub fn run<A: App>(options: WindowOptions, mut app: A) -> Result<(), WindowError> {
    let event_loop = EventLoop::<UserEvent>::with_user_event().build()?;
    let proxy = event_loop.create_proxy();

    let mut runner = Runner {
        options,
        window: None,
        modifiers: ModifiersState::empty(),
        mouse_pos: (0.0, 0.0),
        proxy,
        app: &mut app,
        last_status: ControlSignal::Continue,
        redraw_at: None,
        resume_pty: false,
    };

    event_loop.set_control_flow(ControlFlow::Wait);
    event_loop.run_app(&mut runner)?;
    Ok(())
}

/// The event-loop control flow [`schedule`] picks (winit's
/// `ControlFlow`, plus exit).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Flow {
    Wait,
    WaitUntil(Instant),
    Poll,
    Exit,
}

/// What the loop does after an app callback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Plan {
    flow: Flow,
    /// Request a redraw now.
    redraw_now: bool,
    /// The scheduled redraw still pending afterwards.
    redraw_at: Option<Instant>,
}

/// Turn the app's answer to an event into a [`Plan`].
///
/// - `RedrawIn(0)` redraws at once.
/// - `RedrawIn(d)` sleeps until `now + d`; `new_events` requests the
///   redraw when the deadline passes. Requesting it now as well would
///   redraw at once and, since the redraw handler schedules the next
///   deadline, spin the loop for as long as a deadline is pending (a
///   blinking cursor cost ~50% of a core).
/// - The redraw handler's answer (`is_redraw`) sets the pending deadline,
///   later or earlier. Other events can only bring it forward, and a
///   `Continue` from them keeps it, so mouse motion or a key release
///   doesn't cancel a pending cursor blink.
fn schedule(sig: ControlSignal, is_redraw: bool, redraw_at: Option<Instant>, now: Instant) -> Plan {
    match sig {
        ControlSignal::Exit => Plan {
            flow: Flow::Exit,
            redraw_now: false,
            redraw_at,
        },
        ControlSignal::Continue => {
            let redraw_at = if is_redraw { None } else { redraw_at };
            Plan {
                flow: redraw_at.map_or(Flow::Wait, Flow::WaitUntil),
                redraw_now: false,
                redraw_at,
            }
        }
        // Yield redraws now exactly like RedrawIn(0), Defer polls
        // without a redraw; the resumed PtyReady is the Runner's business
        // (see `Runner::resume_pty`).
        // Duration::ZERO means "wake immediately" — under
        // ControlFlow::WaitUntil(now), winit's EventLoopWaker re-arms
        // the CFRunLoopTimer every iteration because Instant::now()
        // advances each call, defeating its deadline dedup. Profile
        // showed ~20% of main-thread time in
        // CFRunLoopTimerSetNextFireDate. ControlFlow::Poll dedups
        // against a fixed past instant inside EventLoopWaker::start, so
        // the arm happens once and subsequent iterations short-circuit.
        ControlSignal::Yield => Plan {
            flow: Flow::Poll,
            redraw_now: true,
            redraw_at,
        },
        ControlSignal::Defer => Plan {
            flow: Flow::Poll,
            redraw_now: false,
            redraw_at,
        },
        ControlSignal::RedrawIn(d) if d.is_zero() => Plan {
            flow: Flow::Poll,
            redraw_now: true,
            redraw_at,
        },
        ControlSignal::RedrawIn(d) => {
            let at = now.checked_add(d).unwrap_or(now);
            let at = match redraw_at {
                Some(prev) if !is_redraw => prev.min(at),
                _ => at,
            };
            Plan {
                flow: Flow::WaitUntil(at),
                redraw_now: false,
                redraw_at: Some(at),
            }
        }
    }
}

/// Internal driver — implements winit's `ApplicationHandler`.
struct Runner<'a, A: App> {
    options: WindowOptions,
    window: Option<Arc<Window>>,
    modifiers: ModifiersState,
    mouse_pos: (f64, f64),
    proxy: EventLoopProxy<UserEvent>,
    app: &'a mut A,
    last_status: ControlSignal,
    /// When the next scheduled redraw is due (from a
    /// `ControlSignal::RedrawIn`), if any.
    redraw_at: Option<Instant>,
    /// The app answered `ControlSignal::Yield` (or `Defer`): it stopped
    /// parsing PTY output so a frame could be drawn (or input handled).
    /// Until `about_to_wait` (which every platform runs after the
    /// iteration's `RedrawRequested`) delivers the resuming
    /// `Event::PtyReady`, reader wakes are dropped.
    resume_pty: bool,
}

impl<A: App> Runner<'_, A> {
    fn dispatch(&mut self, ev: Event, event_loop: &ActiveEventLoop) {
        let is_redraw = matches!(ev, Event::Redraw);
        let sig = self.app.event(ev);
        self.last_status = sig;
        if matches!(sig, ControlSignal::Yield | ControlSignal::Defer) {
            self.resume_pty = true;
        }
        self.apply_control(sig, is_redraw, event_loop);
    }

    /// Apply the app's answer to an event (see [`schedule`]).
    fn apply_control(&mut self, sig: ControlSignal, is_redraw: bool, event_loop: &ActiveEventLoop) {
        let plan = schedule(sig, is_redraw, self.redraw_at, Instant::now());
        self.redraw_at = plan.redraw_at;
        match plan.flow {
            Flow::Exit => event_loop.exit(),
            Flow::Wait => event_loop.set_control_flow(ControlFlow::Wait),
            Flow::WaitUntil(at) => event_loop.set_control_flow(ControlFlow::WaitUntil(at)),
            Flow::Poll => event_loop.set_control_flow(ControlFlow::Poll),
        }
        if plan.redraw_now
            && let Some(w) = &self.window
        {
            w.request_redraw();
        }
    }

    fn window_scale_factor(&self) -> f64 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }
}

impl<A: App> ApplicationHandler<UserEvent> for Runner<'_, A> {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            // Already created. (Mobile platforms can re-resume; desktop
            // shouldn't, but be defensive.)
            return;
        }

        let attrs = Window::default_attributes()
            .with_title(&self.options.title)
            .with_inner_size(winit::dpi::PhysicalSize::new(
                self.options.size.0,
                self.options.size.1,
            ))
            .with_transparent(self.options.transparent);

        let window = match event_loop.create_window(attrs) {
            Ok(w) => w,
            Err(e) => {
                warn!("failed to create window: {e}");
                event_loop.exit();
                return;
            }
        };

        window.set_ime_allowed(self.options.ime);

        debug!(
            size = ?window.inner_size(),
            scale = window.scale_factor(),
            "window created"
        );

        // Fire init.
        let window = Arc::new(window);
        let handle = WindowHandle {
            proxy: self.proxy.clone(),
        };
        self.app.init(
            ToasttyWindow {
                inner: window.clone(),
            },
            handle,
        );
        self.window = Some(window);
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.dispatch(Event::Close, event_loop);
            }
            WindowEvent::Resized(size) => {
                let scale = self.window_scale_factor();
                self.dispatch(
                    Event::Resize {
                        width: size.width,
                        height: size.height,
                        scale_factor: scale,
                    },
                    event_loop,
                );
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                let size = self.window.as_ref().map_or((0, 0), |w| {
                    let s = w.inner_size();
                    (s.width, s.height)
                });
                self.dispatch(
                    Event::Resize {
                        width: size.0,
                        height: size.1,
                        scale_factor,
                    },
                    event_loop,
                );
            }
            WindowEvent::RedrawRequested => {
                self.dispatch(Event::Redraw, event_loop);
            }
            WindowEvent::Focused(focused) => {
                self.dispatch(Event::Focus(focused), event_loop);
            }
            WindowEvent::ModifiersChanged(m) => {
                self.modifiers = m.state();
                trace!(?self.modifiers, "modifiers changed");
                let leds = led::read_led_state();
                let modifiers = translate_modifiers_with_leds(self.modifiers, leds);
                self.dispatch(Event::ModifiersChanged(modifiers), event_loop);
            }
            WindowEvent::CursorLeft { .. } => {
                self.dispatch(Event::PointerLeft, event_loop);
            }
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                let translated = translate_key(&event, self.modifiers, is_synthetic);
                self.dispatch(translated, event_loop);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let leds = led::read_led_state();
                let modifiers = translate_modifiers_with_leds(self.modifiers, leds);
                self.dispatch(
                    Event::Mouse {
                        button: translate_mouse_button(button),
                        state: translate_state(state),
                        position: self.mouse_pos,
                        modifiers,
                    },
                    event_loop,
                );
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.mouse_pos = (position.x, position.y);
                let leds = led::read_led_state();
                let modifiers = translate_modifiers_with_leds(self.modifiers, leds);
                self.dispatch(
                    Event::MouseMotion {
                        position: self.mouse_pos,
                        modifiers,
                    },
                    event_loop,
                );
            }
            WindowEvent::MouseWheel { delta, phase, .. } => {
                let kind = match delta {
                    MouseScrollDelta::LineDelta(..) => event::ScrollKind::Lines,
                    MouseScrollDelta::PixelDelta(..) => event::ScrollKind::Pixels,
                };
                // Map winit TouchPhase → our ScrollPhase. winit folds
                // macOS NSEvent.phase and NSEvent.momentumPhase into
                // one enum, so `Started` and `Ended` arrive twice per
                // swipe-with-inertia (once for the gesture, once for
                // the momentum tail). Cancelled is folded into Ended
                // to keep downstream handling symmetric.
                let phase = match phase {
                    TouchPhase::Started => event::ScrollPhase::Started,
                    TouchPhase::Ended | TouchPhase::Cancelled => event::ScrollPhase::Ended,
                    TouchPhase::Moved => event::ScrollPhase::Moved,
                };
                let (dx, dy) = translate_scroll(delta);
                self.dispatch(
                    Event::Scroll {
                        kind,
                        phase,
                        delta_x: dx,
                        delta_y: dy,
                    },
                    event_loop,
                );
            }
            // IME composition (dead-keys on macOS, fcitx5/Hangul/CJK on
            // Wayland). We surface the raw enabled/preedit/commit/disabled
            // transitions here; the preedit overlay rendering and the
            // commit-to-PTY write happen downstream in the binary.
            WindowEvent::Ime(ime) => {
                self.dispatch(translate_ime(ime), event_loop);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, ev: UserEvent) {
        let mapped = match ev {
            UserEvent::Wake => Event::User,
            // Covered by the resume in `about_to_wait`, after the frame.
            UserEvent::PtyReady if self.resume_pty => return,
            UserEvent::PtyReady => Event::PtyReady,
        };
        self.dispatch(mapped, event_loop);
    }

    fn new_events(&mut self, event_loop: &ActiveEventLoop, cause: StartCause) {
        // A `RedrawIn` deadline passed: redraw now. The redraw handler
        // schedules the next one.
        if matches!(cause, StartCause::ResumeTimeReached { .. }) {
            self.redraw_at = None;
            event_loop.set_control_flow(ControlFlow::Wait);
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // Resume PTY parsing after a `Yield`. winit runs this after the
        // iteration's user events and redraws on X11, Wayland and macOS
        // alike, so the frame the app yielded for is out by now. Calling
        // the app directly (rather than posting a wake) keeps it to one
        // parse slice per iteration without a channel round trip; the
        // `Poll` flow `Yield` set means the loop doesn't sleep before the
        // next frame. Otherwise we rely on the `ControlFlow` set in
        // `apply_control`.
        if std::mem::take(&mut self.resume_pty) {
            self.dispatch(Event::PtyReady, event_loop);
        }
    }
}

// ============================================================================
// Pure translation helpers — kept free of `Runner` state so they're testable.
// ============================================================================

pub(crate) fn translate_state(state: ElementState) -> KeyState {
    match state {
        ElementState::Pressed => KeyState::Pressed,
        ElementState::Released => KeyState::Released,
    }
}

pub(crate) fn translate_mouse_button(button: winit::event::MouseButton) -> MouseButton {
    use winit::event::MouseButton as W;
    match button {
        W::Left => MouseButton::Left,
        W::Right => MouseButton::Right,
        W::Middle => MouseButton::Middle,
        W::Back => MouseButton::Back,
        W::Forward => MouseButton::Forward,
        W::Other(n) => MouseButton::Other(n),
    }
}

/// Translate a `MouseScrollDelta` to `(dx, dy)` in our convention.
///
/// We keep both `LineDelta` and `PixelDelta` mapped through, but the sign
/// convention matches winit (positive y = content moves down). The
/// dispatcher applies the configured `scroll.multiplier` and quantization
/// per the decision record §7.
pub(crate) fn translate_scroll(delta: MouseScrollDelta) -> (f64, f64) {
    match delta {
        MouseScrollDelta::LineDelta(x, y) => (f64::from(x), f64::from(y)),
        MouseScrollDelta::PixelDelta(p) => (p.x, p.y),
    }
}

/// Translate a winit `Ime` event into our [`Event`]. Pure so the four
/// mappings are unit-testable without spinning up an event loop.
pub(crate) fn translate_ime(ime: Ime) -> Event {
    match ime {
        Ime::Enabled => Event::ImeEnabled,
        Ime::Preedit(text, cursor) => Event::ImePreedit { text, cursor },
        Ime::Commit(text) => Event::ImeCommit(text),
        Ime::Disabled => Event::ImeDisabled,
    }
}

#[cfg(test)]
pub(crate) fn translate_modifiers(m: ModifiersState) -> Modifiers {
    translate_modifiers_with_leds(m, led::LedState::default())
}

pub(crate) fn translate_modifiers_with_leds(m: ModifiersState, leds: led::LedState) -> Modifiers {
    let mut out = Modifiers::empty();
    if m.shift_key() {
        out |= Modifiers::SHIFT;
    }
    if m.control_key() {
        out |= Modifiers::CONTROL;
    }
    if m.alt_key() {
        out |= Modifiers::ALT;
    }
    if m.super_key() {
        out |= Modifiers::SUPER;
    }
    if leds.caps_lock {
        out |= Modifiers::CAPS_LOCK;
    }
    if leds.num_lock {
        out |= Modifiers::NUM_LOCK;
    }
    out
}

pub(crate) fn translate_logical_key(k: &Key) -> LogicalKey {
    match k {
        Key::Character(s) => LogicalKey::Character(s.as_str().to_owned()),
        Key::Named(n) => LogicalKey::Named(translate_named_key(*n)),
        Key::Unidentified(_) | Key::Dead(_) => LogicalKey::Unidentified,
    }
}

pub(crate) fn translate_named_key(n: WNamedKey) -> NamedKey {
    use NamedKey as O;
    use WNamedKey as W;
    match n {
        W::Enter => O::Enter,
        W::Escape => O::Escape,
        W::Backspace => O::Backspace,
        W::Tab => O::Tab,
        W::Space => O::Space,
        W::ArrowUp => O::ArrowUp,
        W::ArrowDown => O::ArrowDown,
        W::ArrowLeft => O::ArrowLeft,
        W::ArrowRight => O::ArrowRight,
        W::Home => O::Home,
        W::End => O::End,
        W::PageUp => O::PageUp,
        W::PageDown => O::PageDown,
        W::Insert => O::Insert,
        W::Delete => O::Delete,
        W::F1 => O::F(1),
        W::F2 => O::F(2),
        W::F3 => O::F(3),
        W::F4 => O::F(4),
        W::F5 => O::F(5),
        W::F6 => O::F(6),
        W::F7 => O::F(7),
        W::F8 => O::F(8),
        W::F9 => O::F(9),
        W::F10 => O::F(10),
        W::F11 => O::F(11),
        W::F12 => O::F(12),
        _ => O::Other,
    }
}

pub(crate) fn translate_physical_key(p: WPhysicalKey) -> PhysicalKey {
    match p {
        WPhysicalKey::Code(code) => PhysicalKey::Code(format!("{code:?}")),
        WPhysicalKey::Unidentified(_) => PhysicalKey::Unidentified,
    }
}

fn translate_key(event: &KeyEvent, mods: ModifiersState, is_synthetic: bool) -> Event {
    // text_with_all_modifiers() is the right field for terminal output:
    // KeyEvent::text would omit Ctrl, so Ctrl+A would be "a" instead of "\x01".
    // See docs/decisions/window-input.md §"Surprises" #2.
    let text = event
        .text_with_all_modifiers()
        .map(std::string::ToString::to_string);
    let leds = led::read_led_state();

    Event::Key {
        logical: translate_logical_key(&event.logical_key),
        unmodified: translate_logical_key(&event.key_without_modifiers()),
        physical: translate_physical_key(event.physical_key),
        text,
        modifiers: translate_modifiers_with_leds(mods, leds),
        state: translate_state(event.state),
        repeat: event.repeat,
        is_synthetic,
    }
}

/// Used to keep `RawDisplayHandle`/`RawWindowHandle` in the public surface
/// without forcing every consumer to pull in `raw-window-handle`.
///
/// Re-exported so the binary can pattern-match if it ever needs to.
pub use raw_window_handle as rwh;
#[doc(hidden)]
pub type RawDisplay = RawDisplayHandle;
#[doc(hidden)]
pub type RawWindow = RawWindowHandle;

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use winit::dpi::PhysicalPosition;

    #[test]
    fn window_options_default_has_ime_on() {
        let o = WindowOptions::default();
        assert!(
            o.ime,
            "IME must be on by default; macOS dead keys depend on it"
        );
        assert!(!o.title.is_empty());
        assert!(o.size.0 > 0 && o.size.1 > 0);
    }

    #[test]
    fn window_options_custom() {
        let o = WindowOptions {
            title: "toasted".to_string(),
            size: (800, 600),
            ime: false,
            transparent: false,
        };
        assert_eq!(o.title, "toasted");
        assert_eq!(o.size, (800, 600));
        assert!(!o.ime);
    }

    #[test]
    fn key_state_pressed() {
        assert!(KeyState::Pressed.is_pressed());
        assert!(!KeyState::Released.is_pressed());
    }

    #[test]
    fn translate_state_roundtrip() {
        assert_eq!(translate_state(ElementState::Pressed), KeyState::Pressed);
        assert_eq!(translate_state(ElementState::Released), KeyState::Released);
    }

    #[test]
    fn translate_mouse_button_all_variants() {
        use winit::event::MouseButton as W;
        assert_eq!(translate_mouse_button(W::Left), MouseButton::Left);
        assert_eq!(translate_mouse_button(W::Right), MouseButton::Right);
        assert_eq!(translate_mouse_button(W::Middle), MouseButton::Middle);
        assert_eq!(translate_mouse_button(W::Back), MouseButton::Back);
        assert_eq!(translate_mouse_button(W::Forward), MouseButton::Forward);
        assert_eq!(translate_mouse_button(W::Other(7)), MouseButton::Other(7));
    }

    #[test]
    fn translate_scroll_line() {
        let (dx, dy) = translate_scroll(MouseScrollDelta::LineDelta(1.0, -2.5));
        assert!((dx - 1.0).abs() < 1e-9);
        assert!((dy + 2.5).abs() < 1e-9);
    }

    #[test]
    fn translate_scroll_pixel() {
        let (dx, dy) = translate_scroll(MouseScrollDelta::PixelDelta(PhysicalPosition::new(
            3.5, -7.0,
        )));
        assert!((dx - 3.5).abs() < 1e-9);
        assert!((dy + 7.0).abs() < 1e-9);
    }

    #[test]
    fn translate_ime_enabled() {
        assert!(matches!(translate_ime(Ime::Enabled), Event::ImeEnabled));
    }

    #[test]
    fn translate_ime_disabled() {
        assert!(matches!(translate_ime(Ime::Disabled), Event::ImeDisabled));
    }

    #[test]
    fn translate_ime_commit() {
        match translate_ime(Ime::Commit("한".to_string())) {
            Event::ImeCommit(s) => assert_eq!(s, "한"),
            other => panic!("expected ImeCommit, got {other:?}"),
        }
    }

    #[test]
    fn translate_ime_preedit() {
        match translate_ime(Ime::Preedit("ㅎ".to_string(), Some((0, 3)))) {
            Event::ImePreedit { text, cursor } => {
                assert_eq!(text, "ㅎ");
                assert_eq!(cursor, Some((0, 3)));
            }
            other => panic!("expected ImePreedit, got {other:?}"),
        }
    }

    #[test]
    fn translate_ime_preedit_cleared() {
        // Empty text + None cursor is how winit signals "clear the preedit".
        match translate_ime(Ime::Preedit(String::new(), None)) {
            Event::ImePreedit { text, cursor } => {
                assert!(text.is_empty());
                assert_eq!(cursor, None);
            }
            other => panic!("expected ImePreedit, got {other:?}"),
        }
    }

    #[test]
    fn translate_modifiers_empty() {
        assert_eq!(
            translate_modifiers(ModifiersState::empty()),
            Modifiers::empty()
        );
    }

    #[test]
    fn translate_modifiers_all_set() {
        let all = ModifiersState::SHIFT
            | ModifiersState::CONTROL
            | ModifiersState::ALT
            | ModifiersState::SUPER;
        let out = translate_modifiers(all);
        assert!(out.contains(Modifiers::SHIFT));
        assert!(out.contains(Modifiers::CONTROL));
        assert!(out.contains(Modifiers::ALT));
        assert!(out.contains(Modifiers::SUPER));
        // CAPS_LOCK / NUM_LOCK come from platform LED state, not from winit.
        assert!(!out.contains(Modifiers::CAPS_LOCK));
        assert!(!out.contains(Modifiers::NUM_LOCK));
    }

    #[test]
    fn translate_modifiers_with_caps_lock_led() {
        let leds = led::LedState {
            caps_lock: true,
            num_lock: false,
        };
        let out = translate_modifiers_with_leds(ModifiersState::empty(), leds);
        assert!(out.contains(Modifiers::CAPS_LOCK));
        assert!(!out.contains(Modifiers::NUM_LOCK));
    }

    #[test]
    fn translate_modifiers_with_num_lock_led() {
        let leds = led::LedState {
            caps_lock: false,
            num_lock: true,
        };
        let out = translate_modifiers_with_leds(ModifiersState::empty(), leds);
        assert!(out.contains(Modifiers::NUM_LOCK));
        assert!(!out.contains(Modifiers::CAPS_LOCK));
    }

    #[test]
    fn translate_modifiers_combines_winit_and_leds() {
        let leds = led::LedState {
            caps_lock: true,
            num_lock: true,
        };
        let out = translate_modifiers_with_leds(ModifiersState::SHIFT, leds);
        assert!(out.contains(Modifiers::SHIFT));
        assert!(out.contains(Modifiers::CAPS_LOCK));
        assert!(out.contains(Modifiers::NUM_LOCK));
    }

    #[test]
    fn translate_logical_character() {
        let k = Key::Character("a".into());
        match translate_logical_key(&k) {
            LogicalKey::Character(s) => assert_eq!(s, "a"),
            other => panic!("expected Character, got {other:?}"),
        }
    }

    #[test]
    fn translate_logical_named() {
        let k = Key::Named(WNamedKey::Enter);
        match translate_logical_key(&k) {
            LogicalKey::Named(NamedKey::Enter) => {}
            other => panic!("expected Named(Enter), got {other:?}"),
        }
    }

    #[test]
    fn translate_logical_dead_is_unidentified() {
        let k = Key::Dead(None);
        assert!(matches!(
            translate_logical_key(&k),
            LogicalKey::Unidentified
        ));
    }

    #[test]
    fn translate_named_key_coverage() {
        let cases = [
            (WNamedKey::Enter, NamedKey::Enter),
            (WNamedKey::Escape, NamedKey::Escape),
            (WNamedKey::Backspace, NamedKey::Backspace),
            (WNamedKey::Tab, NamedKey::Tab),
            (WNamedKey::Space, NamedKey::Space),
            (WNamedKey::ArrowUp, NamedKey::ArrowUp),
            (WNamedKey::ArrowDown, NamedKey::ArrowDown),
            (WNamedKey::ArrowLeft, NamedKey::ArrowLeft),
            (WNamedKey::ArrowRight, NamedKey::ArrowRight),
            (WNamedKey::Home, NamedKey::Home),
            (WNamedKey::End, NamedKey::End),
            (WNamedKey::PageUp, NamedKey::PageUp),
            (WNamedKey::PageDown, NamedKey::PageDown),
            (WNamedKey::Insert, NamedKey::Insert),
            (WNamedKey::Delete, NamedKey::Delete),
            (WNamedKey::F1, NamedKey::F(1)),
            (WNamedKey::F12, NamedKey::F(12)),
            // Unmapped → Other.
            (WNamedKey::CapsLock, NamedKey::Other),
        ];
        for (input, expected) in cases {
            assert_eq!(translate_named_key(input), expected, "for {input:?}");
        }
    }

    #[test]
    fn translate_physical_unidentified() {
        use winit::keyboard::NativeKeyCode;
        let p = WPhysicalKey::Unidentified(NativeKeyCode::Unidentified);
        assert!(matches!(
            translate_physical_key(p),
            PhysicalKey::Unidentified
        ));
    }

    #[test]
    fn translate_physical_code() {
        use winit::keyboard::KeyCode;
        let p = WPhysicalKey::Code(KeyCode::KeyA);
        match translate_physical_key(p) {
            PhysicalKey::Code(s) => assert_eq!(s, "KeyA"),
            PhysicalKey::Unidentified => panic!("expected Code, got Unidentified"),
        }
    }

    // ---- PERF-1: redraw deadlines --------------------------------------

    fn ms(n: u64) -> std::time::Duration {
        std::time::Duration::from_millis(n)
    }

    #[test]
    fn redraw_in_sleeps_until_the_deadline_without_redrawing_now() {
        // The bug: RedrawIn(d) also redrew at once, so the redraw handler
        // (which answers RedrawIn(next blink)) ran back to back.
        let now = Instant::now();
        let plan = schedule(ControlSignal::RedrawIn(ms(500)), true, None, now);
        assert_eq!(
            plan,
            Plan {
                flow: Flow::WaitUntil(now + ms(500)),
                redraw_now: false,
                redraw_at: Some(now + ms(500)),
            }
        );
    }

    #[test]
    fn redraw_in_zero_redraws_at_once() {
        let now = Instant::now();
        let pending = Some(now + ms(300));
        let plan = schedule(ControlSignal::RedrawIn(ms(0)), false, pending, now);
        assert_eq!(plan.flow, Flow::Poll);
        assert!(plan.redraw_now);
        assert_eq!(plan.redraw_at, pending);
    }

    #[test]
    fn other_events_keep_a_pending_deadline() {
        // Mouse motion answers Continue: the blink deadline must survive.
        let now = Instant::now();
        let pending = Some(now + ms(300));
        let plan = schedule(ControlSignal::Continue, false, pending, now);
        assert_eq!(plan.flow, Flow::WaitUntil(now + ms(300)));
        assert_eq!(plan.redraw_at, pending);
        assert!(!plan.redraw_now);
        // They can bring it forward, not push it back.
        let plan = schedule(ControlSignal::RedrawIn(ms(100)), false, pending, now);
        assert_eq!(plan.redraw_at, Some(now + ms(100)));
        let plan = schedule(ControlSignal::RedrawIn(ms(900)), false, pending, now);
        assert_eq!(plan.redraw_at, pending);
    }

    #[test]
    fn the_redraw_handler_sets_or_clears_the_deadline() {
        let now = Instant::now();
        let pending = Some(now + ms(100));
        // A later deadline from the redraw handler replaces the old one.
        let plan = schedule(ControlSignal::RedrawIn(ms(500)), true, pending, now);
        assert_eq!(plan.redraw_at, Some(now + ms(500)));
        // Continue from the redraw handler: nothing scheduled, sleep.
        let plan = schedule(ControlSignal::Continue, true, pending, now);
        assert_eq!((plan.flow, plan.redraw_at), (Flow::Wait, None));
        assert_eq!(
            schedule(ControlSignal::Exit, false, pending, now).flow,
            Flow::Exit
        );
    }

    #[test]
    fn yield_redraws_at_once_and_keeps_the_deadline() {
        let now = Instant::now();
        let pending = Some(now + ms(300));
        assert_eq!(
            schedule(ControlSignal::Yield, false, pending, now),
            schedule(ControlSignal::RedrawIn(ms(0)), false, pending, now),
        );
        let plan = schedule(ControlSignal::Yield, false, pending, now);
        assert_eq!(plan.flow, Flow::Poll);
        assert!(plan.redraw_now);
        assert_eq!(plan.redraw_at, pending);
    }

    #[test]
    fn defer_polls_without_redrawing() {
        let now = Instant::now();
        let pending = Some(now + ms(300));
        assert_eq!(
            schedule(ControlSignal::Defer, false, pending, now),
            Plan {
                flow: Flow::Poll,
                redraw_now: false,
                redraw_at: pending,
            }
        );
    }

    #[test]
    fn control_signal_variants_are_distinct() {
        assert_ne!(ControlSignal::Continue, ControlSignal::Exit);
        assert_ne!(
            ControlSignal::RedrawIn(Duration::from_millis(16)),
            ControlSignal::Continue
        );
    }
}
