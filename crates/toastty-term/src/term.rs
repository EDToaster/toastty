//! `Term`: top-level terminal state.
//!
//! Owns the primary and alternate grids, a cursor, and the SGR state in
//! effect. Implements `toastty_parser::Perform` so the parser can drive it
//! directly.

use std::time::Instant;

use unicode_width::UnicodeWidthChar;

use crate::cell::{Cell, Color, Style};
use crate::cursor::Cursor;
use crate::damage::Damage;
use crate::grid::Grid;
use toastty_config::CursorShape;
use toastty_graphics::kitty::handler::{KittyHandler, KittySink};
use toastty_graphics::kitty::header::DeleteSpec;
use toastty_graphics::{ImageData, ImageGrid, ImageRegistry, Placement};
use toastty_parser::{Params, Perform};

/// Width of a hard tab. Eight is the canonical default; once we expose
/// tab-stop manipulation (HTS/TBC) this becomes per-column state.
const TAB_WIDTH: u16 = 8;

/// Mouse reporting protocol selected via DECSET 1000/1002/1003.
///
/// Apps opt in via the matching DECSET; the binary uses
/// [`Term::mouse_mode`] to decide which events to forward as CSI sequences.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MouseProtocol {
    /// Mouse reporting off (default).
    #[default]
    Off,
    /// DECSET 1000 — report button presses + releases only.
    X10,
    /// DECSET 1002 — report button presses + releases + motion while a
    /// button is held (drag).
    ButtonMotion,
    /// DECSET 1003 — report all motion (rarely needed, but cheap).
    AnyMotion,
}

/// Combined mouse mode state — protocol + whether SGR (1006) encoding is
/// active. `sgr_encoding` doesn't change which events fire, just how they
/// are serialised.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MouseMode {
    pub protocol: MouseProtocol,
    pub sgr_encoding: bool,
}

impl MouseMode {
    /// True when *any* event reporting is on (protocol != Off).
    #[must_use]
    pub fn is_on(&self) -> bool {
        !matches!(self.protocol, MouseProtocol::Off)
    }

    /// True when motion-while-button-held events should be reported.
    #[must_use]
    pub fn report_drag(&self) -> bool {
        matches!(
            self.protocol,
            MouseProtocol::ButtonMotion | MouseProtocol::AnyMotion
        )
    }

    /// True when *any* motion (regardless of button state) is reported.
    #[must_use]
    pub fn report_any_motion(&self) -> bool {
        matches!(self.protocol, MouseProtocol::AnyMotion)
    }
}

/// Synchronized-output (DECSET 2026) state.
///
/// When BSU (`CSI ? 2026 h`) is received, `active` flips to true and
/// `started_at` records the wall-clock instant. The renderer must skip
/// frames while `active` is true. If ESU (`CSI ? 2026 l`) doesn't arrive
/// within the timeout (~1s, matching tmux), the binary's watchdog calls
/// [`Term::force_flush_sync_output`] which clears `active` and sets
/// `timeout_force_flushed` so the very next post-flush render performs a
/// corrective full redraw (decision #7).
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct SyncOutput {
    /// BSU received, ESU not yet.
    pub active: bool,
    /// Wall-clock instant BSU went high; used by the watchdog timer.
    pub started_at: Option<Instant>,
    /// Set when the watchdog force-flushed the pause without an ESU. The
    /// renderer reads this on the next frame to ensure a full redraw, then
    /// the binary clears it via
    /// [`Term::clear_sync_output_force_flushed`].
    pub timeout_force_flushed: bool,
}

// ---- Kitty keyboard protocol flag bits ----
// See <https://sw.kovidgoyal.net/kitty/keyboard-protocol/>.

/// Disambiguate escape codes (bit 1).
pub const KITTY_FLAG_DISAMBIGUATE: u8 = 0b0_0001;
/// Report event types — press / repeat / release (bit 2).
pub const KITTY_FLAG_REPORT_EVENTS: u8 = 0b0_0010;
/// Report alternate keys (bit 4) — TODO(kitty-keyboard).
pub const KITTY_FLAG_REPORT_ALTERNATE: u8 = 0b0_0100;
/// Report all keys as escape codes (bit 8) — TODO(kitty-keyboard).
pub const KITTY_FLAG_REPORT_ALL_AS_ESC: u8 = 0b0_1000;
/// Report associated text (bit 16) — TODO(kitty-keyboard).
pub const KITTY_FLAG_REPORT_TEXT: u8 = 0b1_0000;

/// Top-level terminal state object.
///
/// `clippy::struct_excessive_bools` is suppressed: the boolean fields
/// here track orthogonal DECSET modes (bracketed paste / focus report /
/// grapheme cluster / inband resize / OSC 52 read+write security) plus
/// the cursor-blink + alt-screen flags. Each is independent state, not
/// a 7-bit state machine.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)]
pub struct Term {
    primary: Grid,
    alt: Grid,
    cursor: Cursor,
    /// Cursor snapshot captured on the most recent `1049` enter; restored
    /// on `1049` exit.
    saved_cursor: Cursor,
    alt_active: bool,
    rows: u16,
    cols: u16,
    /// Primary-grid scrollback capacity (visible rows + history).
    scrollback: u16,
    /// Per-cell sparse damage (visible rows only). Set by every mutation
    /// that touches a cell or range of cells; consumed by the renderer's
    /// row-shape cache and partial-redraw builder so clean cells skip
    /// re-emission.
    ///
    /// Bare cursor moves (CUU/CUD/CUF/CUB/CUP — no cell write) flip the
    /// damage bit for both the column being left and the column being
    /// entered (across both rows when the cursor changes row), so the
    /// cursor block repaints correctly under `hjkl`-style navigation.
    /// See [`Term::move_cursor`].
    damage: Damage,
    /// Window title, last set via OSC 0 or OSC 2. Empty when the app has
    /// not set one (the binary picks a default at startup). Title updates
    /// do NOT mark any row dirty — the title doesn't live in the grid.
    title: String,
    /// Cursor block shape. Set at startup from config; runtime overrides
    /// arrive via DECSCUSR (`CSI N SP q`).
    cursor_shape: CursorShape,
    /// Cursor blink flag. Stored for completeness but not yet rendered —
    /// the animation tick lands with M9.
    cursor_blink: bool,
    /// DECSET 2004 — wrap pastes in `\x1b[200~ ... \x1b[201~`.
    bracketed_paste: bool,
    /// DECSET 1004 — emit `\x1b[I` / `\x1b[O` on focus change.
    report_focus: bool,
    /// Current mouse reporting mode (DECSET 1000 / 1002 / 1003 / 1006).
    mouse_mode: MouseMode,
    /// Kitty keyboard progressive-enhancement flag stack.
    ///
    /// The active flags are the top of the stack; empty stack == legacy
    /// behaviour. Capped at 8 entries (more than enough — kitty docs say
    /// "small stack").
    kitty_keyboard_stack: Vec<u8>,
    /// DECSET 2026 — synchronized output (BSU/ESU) state.
    sync_output: SyncOutput,
    /// DECSET 2027 — grapheme cluster processing opt-in.
    grapheme_cluster_mode: bool,
    /// DECSET 2048 — in-band resize notifications opt-in.
    inband_resize_mode: bool,
    /// Last cwd advertised via `OSC 7 ; file://<host>/<path> ST`. Empty
    /// when the shell hasn't sent one yet. Not validated against the
    /// host — we accept whatever the shell told us.
    cwd: String,
    /// Semantic prompt markers (OSC 133). FIFO bounded at 4096 — old
    /// marks rotate out when the cap is hit so the buffer can't bloat
    /// on a long-running session.
    prompt_marks: std::collections::VecDeque<PromptMark>,
    /// FIFO of bytes that need to be written back to the PTY after the
    /// current `parser.advance` call completes. Populated by OSC handlers
    /// that need to reply to a query (OSC 4 query, OSC 52 read). Drained
    /// by the binary via [`Term::drain_pty_replies`].
    ///
    /// Keeping replies in a queue (rather than synchronously writing to
    /// the PTY from inside a `Perform` callback) lets the parsing layer
    /// stay free of I/O concerns and keeps `Term` reentrancy-safe.
    pty_replies: Vec<u8>,
    /// OSC 4 palette overrides. `None` at an index means "fall through to
    /// the renderer's built-in xterm 256-color table"; `Some([r, g, b])`
    /// is the app-supplied sRGB triple. Boxed because a flat
    /// `[Option<[u8; 3]>; 256]` is 1 KB; keeping it on the heap avoids
    /// inflating `Term` itself.
    palette_overrides: Box<[Option<[u8; 3]>; 256]>,
    /// Bump-counter incremented every time an OSC 4 set lands. The
    /// renderer keeps a cached linear-light palette and compares
    /// revisions to decide when to rebuild.
    palette_revision: u32,
    /// Intern table for OSC 8 hyperlink URLs. The cell stores a
    /// `NonZeroU16` index into this `Vec`; closing the hyperlink
    /// (`OSC 8 ; ; ST`) clears `current_hyperlink` without touching
    /// previously-stamped cells. Bounded at 65535 distinct URLs per
    /// session — well above anything reasonable.
    hyperlinks: Vec<String>,
    /// Currently active hyperlink id stamped onto every cell written by
    /// [`Term::print_char`] until the next `OSC 8 ; ; ST` (closer) or
    /// a new `OSC 8 ; ... ; url` (which switches to a fresh id).
    current_hyperlink: Option<crate::cell::HyperlinkId>,
    /// FIFO of OSC 52 clipboard requests waiting on the binary to
    /// service through `arboard`. Drained by [`Term::drain_clipboard_requests`].
    clipboard_requests: Vec<ClipboardRequest>,
    /// Security gates from the user's `[security]` config.
    security: SecurityFlags,
    // ----- M11a: Kitty graphics + image protocols -----
    /// Buffer of APC payload bytes for the current `APC ... ST` packet.
    /// Cleared on `apc_start`; appended via `apc_chunk`; consumed by
    /// `apc_end`.
    apc_buffer: Vec<u8>,
    /// Stateful Kitty dispatcher. Owns chunked-upload reassembly.
    image_handler: KittyHandler,
    /// Cache of decoded image bytes keyed by Kitty image id.
    image_registry: ImageRegistry,
    /// Parallel layer of placements over the cell grid.
    image_grid: ImageGrid,
    /// Monotonic counter bumped whenever the registry or grid mutates.
    /// The renderer compares against its cached value to decide when
    /// to re-sync GPU textures (and force a full clear of the frame).
    image_revision: u32,
    /// SGR 58 underline color. Stored but not yet rendered as an
    /// underline color; the Unicode placeholder pipeline reads it as
    /// the *low byte* of the image id (kitty's protocol).
    cursor_underline_color: Option<Color>,
    /// Unicode placeholder run-in-progress.
    placeholder_run: Option<PlaceholderRun>,
}

/// In-progress run of Kitty Unicode placeholder cells.
///
/// Apps emit `<PLACEHOLDER><d_row><d_col>(<d_id_msb>)?` per cell as the
/// foreground SGR encodes the low byte of the image id. We collect the
/// run greedily until the next non-placeholder/non-diacritic codepoint
/// arrives, then materialize placements.
#[derive(Debug)]
pub(crate) struct PlaceholderRun {
    /// Image id whose low byte was taken from the cursor fg color.
    pub image_id_low: u8,
    /// Cursor underline color encoded the optional 8-bit MSB extension
    /// of the image id (kitty supports a 24-bit id via SGR 38;5 + 58;5).
    pub image_id_high: Option<u8>,
    /// Cells collected so far: `(row, col, diacritics)`.
    pub cells: Vec<PlaceholderCell>,
    /// Starting row (so future extensions can detect newline
    /// boundaries; not yet consumed in M11a).
    #[allow(dead_code)]
    pub start_row: u16,
}

/// One placeholder cell within a [`PlaceholderRun`].
#[derive(Debug, Clone)]
pub(crate) struct PlaceholderCell {
    pub row: u16,
    pub col: u16,
    /// Diacritic indices in emission order. The first encodes the source
    /// image row, the second the source image column, the third (optional)
    /// the image id MSB extension.
    pub diacritics: smallvec::SmallVec<[u16; 3]>,
}

/// One pending OSC 52 clipboard operation, queued by [`Term`] and
/// serviced by the binary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardRequest {
    /// Replace the system clipboard with `data`.
    Set { data: Vec<u8> },
    /// Read the system clipboard and reply via
    /// [`Term::push_pty_reply`] with a `selection`-tagged OSC 52
    /// response.
    Query { selection: Vec<u8> },
}

/// Security flags mirrored on `Term` from
/// [`toastty_config::SecurityConfig`]. Kept separate from the config
/// struct so the term crate doesn't depend on config wire types.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SecurityFlags {
    /// Allow OSC 52 clipboard reads. Off by default.
    pub osc_52_read: bool,
    /// Allow OSC 52 clipboard writes. Off by default.
    pub osc_52_write: bool,
}

/// One semantic prompt marker recorded from OSC 133.
///
/// `(row, kind)` lets a future command-navigation feature jump between
/// prompt-start / command-start / command-finished markers without
/// re-scanning the grid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromptMark {
    /// Visible row the marker was recorded on. Note that scrollback can
    /// push the underlying row off the top of the visible viewport; we
    /// don't currently rebase the row index in that case.
    pub row: u16,
    /// What kind of marker this is.
    pub kind: PromptMarkKind,
}

/// Kind of a [`PromptMark`] recorded from OSC 133.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptMarkKind {
    /// `OSC 133 ; A` — start of prompt.
    PromptStart,
    /// `OSC 133 ; B` — end of prompt / start of user input.
    PromptEnd,
    /// `OSC 133 ; C` — start of command output (after Enter).
    CommandStart,
    /// `OSC 133 ; D ; [exit_code]` — command finished.
    CommandFinished(Option<i32>),
}

/// Cap on the number of OSC-133 prompt marks we retain.
const PROMPT_MARK_CAP: usize = 4096;

impl Term {
    /// Construct a fresh terminal `rows` rows by `cols` cols, with
    /// `scrollback` additional rows of history available behind the
    /// primary screen. The alt screen uses no scrollback (decision #6).
    #[must_use]
    pub fn new(rows: u16, cols: u16, scrollback: u16) -> Self {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let primary_cap = rows as usize + scrollback as usize;
        let primary = Grid::new(rows, cols, primary_cap);
        let alt = Grid::new(rows, cols, rows as usize);
        Self {
            primary,
            alt,
            cursor: Cursor::default(),
            saved_cursor: Cursor::default(),
            alt_active: false,
            rows,
            cols,
            scrollback,
            // Start with everything dirty so the first render shapes
            // every row.
            damage: Damage::new(rows),
            title: String::new(),
            // Defaults match `toastty_config::CursorConfig::defaults`.
            // Callers can override via `set_cursor_default` at init time;
            // the PTY can override at runtime via DECSCUSR.
            cursor_shape: CursorShape::Block,
            cursor_blink: true,
            bracketed_paste: false,
            report_focus: false,
            mouse_mode: MouseMode::default(),
            kitty_keyboard_stack: Vec::new(),
            sync_output: SyncOutput::default(),
            grapheme_cluster_mode: false,
            inband_resize_mode: false,
            cwd: String::new(),
            prompt_marks: std::collections::VecDeque::new(),
            pty_replies: Vec::new(),
            palette_overrides: Box::new([None; 256]),
            palette_revision: 0,
            hyperlinks: Vec::new(),
            current_hyperlink: None,
            clipboard_requests: Vec::new(),
            security: SecurityFlags::default(),
            apc_buffer: Vec::new(),
            image_handler: KittyHandler::new(),
            // Default 256 MiB image cache cap. Generous but bounded; the
            // binary can shrink via `Term::set_image_cap`.
            image_registry: ImageRegistry::new(256 * 1024 * 1024),
            image_grid: ImageGrid::new(),
            image_revision: 0,
            cursor_underline_color: None,
            placeholder_run: None,
        }
    }

    /// Set the security flags. Called by the binary right after
    /// `Term::new` to thread the user's `[security]` config through.
    pub fn set_security(&mut self, flags: SecurityFlags) {
        self.security = flags;
    }

    /// Current security flags (read-only).
    #[must_use]
    pub fn security(&self) -> SecurityFlags {
        self.security
    }

    /// Drain queued OSC 52 clipboard requests. Called by the binary
    /// after every `parser.advance` and serviced via `arboard`.
    pub fn drain_clipboard_requests(&mut self) -> Vec<ClipboardRequest> {
        std::mem::take(&mut self.clipboard_requests)
    }

    /// Drain bytes queued for the PTY back-channel and return them. The
    /// binary calls this after every `parser.advance` and writes the
    /// returned bytes to the PTY master. Returns an empty `Vec` when
    /// nothing was queued.
    pub fn drain_pty_replies(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.pty_replies)
    }

    /// Append `bytes` to the PTY back-channel. Public so cooperating
    /// layers (the binary's OSC 52 clipboard read path) can queue
    /// asynchronously-produced replies through the same drain.
    pub fn push_pty_reply(&mut self, bytes: &[u8]) {
        self.pty_replies.extend_from_slice(bytes);
    }

    /// Read-only view of the cached image bytes.
    #[must_use]
    pub fn image_registry(&self) -> &ImageRegistry {
        &self.image_registry
    }

    /// Read-only view of placements over the cell grid.
    #[must_use]
    pub fn image_grid(&self) -> &ImageGrid {
        &self.image_grid
    }

    /// Monotonic image-content revision. Bumps when the registry or
    /// placement grid changes. Renderer compares to its cached value
    /// to decide when to re-sync GPU textures.
    #[must_use]
    pub fn image_revision(&self) -> u32 {
        self.image_revision
    }

    /// Current SGR 58 underline color, or `None` when SGR 59 (or 0)
    /// reset it. The Unicode placeholder pipeline reads this as the
    /// high byte of the image id.
    #[must_use]
    pub fn cursor_underline_color(&self) -> Option<Color> {
        self.cursor_underline_color
    }

    /// Override the per-Term image-cache byte budget. May evict.
    pub fn set_image_cap(&mut self, cap_bytes: u64) {
        let evicted = self.image_registry.set_cap(cap_bytes);
        if !evicted.is_empty() {
            for id in &evicted {
                self.image_grid.remove_image(*id);
            }
            self.image_revision = self.image_revision.wrapping_add(1);
            self.mark_all_dirty();
        }
    }

    /// Read the override for palette index `idx`. Returns `None` if no
    /// override is active for that slot (the renderer should fall back
    /// to its built-in 256-color table).
    #[must_use]
    pub fn palette_override(&self, idx: u8) -> Option<[u8; 3]> {
        self.palette_overrides[idx as usize]
    }

    /// Monotonic revision counter. Bumps on every successful OSC 4 set.
    /// The renderer reads this once per frame and rebuilds its cached
    /// linear-light extended palette on change.
    #[must_use]
    pub fn palette_revision(&self) -> u32 {
        self.palette_revision
    }

    /// Set palette override for `idx` and bump the revision. Marks every
    /// row dirty so the new color shows immediately under partial
    /// redraw (since the resolved color of any existing cell drawn at
    /// `idx` has changed out from under it).
    fn set_palette_override(&mut self, idx: u8, rgb: [u8; 3]) {
        self.palette_overrides[idx as usize] = Some(rgb);
        self.palette_revision = self.palette_revision.wrapping_add(1);
        self.mark_all_dirty();
    }

    /// Resolve a hyperlink id (as stamped on a [`Cell`]) back to its URL.
    /// Returns `None` if the id is out of range — defensive against
    /// future bugs since ids are minted by [`Term::intern_hyperlink`]
    /// and stored as `NonZeroU16`.
    #[must_use]
    pub fn hyperlink_url(&self, id: crate::cell::HyperlinkId) -> Option<&str> {
        // Ids are 1-based: `NonZeroU16::new(1)` indexes `hyperlinks[0]`.
        let idx = id.get() as usize - 1;
        self.hyperlinks.get(idx).map(String::as_str)
    }

    /// Intern `url` into the hyperlink table and return its id. Dedups
    /// against existing entries so the same URL across many cells
    /// shares one id. Returns `None` if the table is full (65535
    /// distinct URLs in one session — practically unreachable).
    fn intern_hyperlink(&mut self, url: &str) -> Option<crate::cell::HyperlinkId> {
        if let Some(pos) = self.hyperlinks.iter().position(|u| u == url) {
            // Convert position (0-based) to 1-based NonZero id.
            return crate::cell::HyperlinkId::new((pos + 1) as u16);
        }
        // Cap at u16::MAX - 1 (since ids start at 1).
        if self.hyperlinks.len() >= (u16::MAX as usize) {
            return None;
        }
        self.hyperlinks.push(url.to_string());
        let id = u16::try_from(self.hyperlinks.len()).ok()?;
        crate::cell::HyperlinkId::new(id)
    }

    /// Most recent cwd advertised via OSC 7. Empty when the shell hasn't
    /// emitted one yet.
    #[must_use]
    pub fn cwd(&self) -> &str {
        &self.cwd
    }

    /// Read-only view of the OSC 133 prompt marks recorded so far, in
    /// emission order. Capped at 4096 entries; oldest entries roll off
    /// the front when the cap is hit.
    ///
    /// Returns a contiguous slice. The backing store is a `VecDeque`
    /// for O(1) FIFO eviction (M10-followup I3), so this method calls
    /// `make_contiguous` to expose the entries as a slice. The mutation
    /// happens against `&mut self` internally; the public signature
    /// stays `&self`-returning-`&[…]` so callers continue to index /
    /// slice without API churn.
    #[must_use]
    pub fn prompt_marks(&mut self) -> &[PromptMark] {
        self.prompt_marks.make_contiguous()
    }

    /// Append a prompt mark at the current cursor row, evicting the
    /// oldest entry when the cap is hit.
    ///
    /// Uses `VecDeque::pop_front` so eviction is O(1) instead of the
    /// O(n) `Vec::remove(0)` shift (M10-followup I3): a hot loop of
    /// rapid prompts no longer pays an N²/2 cost once the cap is
    /// reached.
    fn push_prompt_mark(&mut self, kind: PromptMarkKind) {
        let row = self.cursor.row;
        if self.prompt_marks.len() >= PROMPT_MARK_CAP {
            self.prompt_marks.pop_front();
        }
        self.prompt_marks.push_back(PromptMark { row, kind });
    }

    /// True when DECSET 2004 (bracketed paste) is active.
    #[must_use]
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }

    /// True when DECSET 2026 (synchronized output) is active and the
    /// renderer must skip submitting frames. Cleared on ESU or by the
    /// watchdog after the timeout.
    #[must_use]
    pub fn pause_rendering(&self) -> bool {
        self.sync_output.active
    }

    /// True when DECSET 2027 (grapheme cluster processing) is active.
    #[must_use]
    pub fn grapheme_cluster_mode(&self) -> bool {
        self.grapheme_cluster_mode
    }

    /// True when DECSET 2048 (in-band resize notifications) is active.
    #[must_use]
    pub fn inband_resize_mode(&self) -> bool {
        self.inband_resize_mode
    }

    /// Wall-clock instant the current BSU went high. `None` if no BSU is
    /// currently active. Exposed for the binary's watchdog timer.
    #[must_use]
    pub fn sync_output_started_at(&self) -> Option<Instant> {
        self.sync_output.started_at
    }

    /// True when the BSU watchdog timed out and force-flushed the pause;
    /// the renderer must emit a corrective full redraw before the binary
    /// clears the flag via
    /// [`Term::clear_sync_output_force_flushed`].
    ///
    /// ## Why this flag exists alongside the dirty bitset
    ///
    /// The corrective full redraw is mechanically delivered by
    /// [`Term::force_flush_sync_output`] calling `mark_all_dirty` on
    /// the per-row bitset — so for M8's row-level damage signal this
    /// flag carries no extra information that the renderer's hot path
    /// needs (the dirty bitset already forces re-shape of every row).
    ///
    /// We keep the flag distinct because M9 splits the damage signal
    /// into finer-grained dirty rectangles / dirty cells (see
    /// `docs/milestones/m09-damage-tracking.md`). At that point the
    /// renderer needs to distinguish:
    ///
    /// 1. "Dirty list covers every cell because the app actually
    ///    rewrote every cell" → small per-cell list still valid,
    ///    `LoadOp::Load` is fine.
    /// 2. "Dirty list is tiny because the app only wrote a few cells
    ///    but we're recovering from a forced ESU mid-batch — the
    ///    underlying framebuffer holds half-painted state" →
    ///    `LoadOp::Clear` + repaint everything.
    ///
    /// Case 2 is exactly what `sync_output_force_flushed` signals.
    /// The binary clears the flag immediately after a render that
    /// actually went out (followup C2 — only on `RenderOutcome::Rendered`).
    #[must_use]
    pub fn sync_output_force_flushed(&self) -> bool {
        self.sync_output.timeout_force_flushed
    }

    /// Clear the timeout-force-flushed flag. Called by the binary right
    /// after the corrective full redraw has been issued.
    pub fn clear_sync_output_force_flushed(&mut self) {
        self.sync_output.timeout_force_flushed = false;
    }

    /// Force-flush the synchronized-output pause: clear `active`, set
    /// `timeout_force_flushed = true`, and mark every visible row dirty
    /// so the next frame issues a corrective full redraw (decision #7).
    ///
    /// Idempotent: calling twice in a row clears nothing new and leaves
    /// the flag latched until the renderer consumes it.
    pub fn force_flush_sync_output(&mut self) {
        if !self.sync_output.active && !self.sync_output.timeout_force_flushed {
            // Nothing to do — and we don't want to needlessly mark rows
            // dirty when no BSU was ever in flight.
            return;
        }
        self.sync_output.active = false;
        self.sync_output.started_at = None;
        self.sync_output.timeout_force_flushed = true;
        self.mark_all_dirty();
    }

    /// Internal: handle a DECSET 2026 toggle. Enable-side captures the
    /// wall-clock so the watchdog can compute elapsed time. Disable-side
    /// clears the pause and marks every row dirty so the post-ESU frame
    /// is a full redraw — both the watchdog and the normal ESU paths
    /// share this corrective-redraw behaviour.
    ///
    /// Reentrant guard: a second BSU while already active must NOT
    /// restart the timer (the spec says successive BSUs without an ESU
    /// in between are a single contiguous batch).
    fn set_sync_output(&mut self, enable: bool) {
        if enable {
            if !self.sync_output.active {
                self.sync_output.active = true;
                self.sync_output.started_at = Some(Instant::now());
            }
            // Reentrant BSU: leave started_at unchanged.
        } else {
            // ESU: clear and force a corrective full redraw.
            self.sync_output.active = false;
            self.sync_output.started_at = None;
            self.mark_all_dirty();
        }
    }

    /// True when DECSET 1004 (focus reporting) is active.
    #[must_use]
    pub fn report_focus(&self) -> bool {
        self.report_focus
    }

    /// Current mouse reporting mode (DECSET 1000/1002/1003/1006).
    #[must_use]
    pub fn mouse_mode(&self) -> MouseMode {
        self.mouse_mode
    }

    /// Active kitty keyboard progressive-enhancement flags (top of stack).
    /// Zero == legacy behaviour.
    #[must_use]
    pub fn kitty_flags(&self) -> u8 {
        self.kitty_keyboard_stack.last().copied().unwrap_or(0)
    }

    /// Read-only view of the kitty flag stack; useful for tests.
    #[must_use]
    pub fn kitty_stack(&self) -> &[u8] {
        &self.kitty_keyboard_stack
    }

    /// Push `flags` onto the kitty progressive-enhancement stack.
    ///
    /// Caps the stack at 8 entries (rotates oldest out when full).
    pub fn kitty_push(&mut self, flags: u8) {
        if self.kitty_keyboard_stack.len() >= 8 {
            self.kitty_keyboard_stack.remove(0);
        }
        self.kitty_keyboard_stack.push(flags);
    }

    /// Pop `n` entries from the kitty flag stack. Saturates at zero.
    pub fn kitty_pop(&mut self, n: usize) {
        let new_len = self.kitty_keyboard_stack.len().saturating_sub(n);
        self.kitty_keyboard_stack.truncate(new_len);
    }

    /// Mutate active kitty flags without push/pop. `mode` selects the
    /// operation:
    /// - 1 — set bits to exactly `flags` (replace).
    /// - 2 — OR `flags` into the top.
    /// - 3 — clear (AND NOT) `flags` from the top.
    ///
    /// If the stack is empty, this implicitly pushes `flags` (for mode 1
    /// or 2) or is a no-op (mode 3).
    pub fn kitty_set(&mut self, flags: u8, mode: u8) {
        if self.kitty_keyboard_stack.is_empty() {
            match mode {
                1 | 2 => self.kitty_keyboard_stack.push(flags),
                _ => {}
            }
            return;
        }
        // Safe: just checked non-empty.
        let top = self.kitty_keyboard_stack.last_mut().unwrap();
        match mode {
            1 => *top = flags,
            2 => *top |= flags,
            3 => *top &= !flags,
            _ => {}
        }
    }

    /// Set the default cursor shape and blink flag. Intended for the
    /// binary to call right after `Term::new` so the runtime cursor
    /// state matches the user's `[cursor]` config table. Runtime
    /// overrides (DECSCUSR) still win after this is called.
    pub fn set_cursor_default(&mut self, shape: CursorShape, blink: bool) {
        self.cursor_shape = shape;
        self.cursor_blink = blink;
    }

    /// Current window title — last value set via OSC 0 / OSC 2. Empty if
    /// the PTY never set one.
    #[must_use]
    pub fn title(&self) -> &str {
        &self.title
    }

    /// Current cursor block shape. Reflects either the startup default
    /// or the most recent DECSCUSR override.
    #[must_use]
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }

    /// Current cursor blink flag. Stored but not yet rendered — see
    /// M9 / animation tick.
    #[must_use]
    pub fn cursor_blink(&self) -> bool {
        self.cursor_blink
    }

    /// Visible (rows, cols).
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Current cursor (row/col + active SGR style).
    pub fn cursor(&self) -> Cursor {
        self.cursor
    }

    /// Borrow visible row `idx` from whichever grid is active.
    pub fn row(&self, idx: u16) -> &crate::grid::Row {
        self.active_grid().row(idx)
    }

    /// True when the alternate screen is currently displayed.
    pub fn is_alt_active(&self) -> bool {
        self.alt_active
    }

    /// Borrow the per-cell damage set (visible rows only). The renderer
    /// reads this to decide which cells to re-emit. Length of
    /// `damage().rows` equals `self.size().0`.
    ///
    /// Call [`Term::clear_damage`] after consuming. Decision #7 / M9.
    #[must_use]
    pub fn damage(&self) -> &Damage {
        &self.damage
    }

    /// Reset the damage set. Renderer's host (the binary) calls this
    /// once a frame has consumed the damage signal.
    pub fn clear_damage(&mut self) {
        self.damage.clear();
    }

    /// Force every visible cell to be reported dirty on the next read,
    /// and flip the top-level "framebuffer is stale" flag so the
    /// renderer issues a full clear on the next frame. Used by the
    /// renderer when its row-shape cache is invalidated (resize, font
    /// change, BSU watchdog) to force a re-shape.
    pub fn mark_all_dirty(&mut self) {
        self.damage.mark_all();
    }

    /// Mark cell `(r, c)` dirty. Bounds-checked; out-of-range writes
    /// are a no-op so callers don't have to guard.
    fn mark_cell(&mut self, r: u16, c: u16) {
        if let Some(row) = self.damage.rows.get_mut(r as usize) {
            row.mark(c);
        }
    }

    /// Public counterpart of [`Term::mark_cell`] for cooperating layers
    /// (e.g. the renderer's cursor-blink tick) to mark a single cell
    /// dirty without going through a full-screen redraw. Bounds-checked.
    ///
    /// If the cell at `(r, c)` is a width-2 continuation, the primary
    /// cell at `(r, c - 1)` is marked as well so the multi-cell glyph
    /// gets re-emitted (the renderer skips continuation cells, so
    /// marking only `(r, c)` would emit no instance at all).
    pub fn mark_cell_dirty(&mut self, r: u16, c: u16) {
        let is_continuation = self
            .row(r)
            .cells
            .get(c as usize)
            .is_some_and(|cell| cell.is_continuation);
        if is_continuation && c > 0 {
            self.mark_cell(r, c - 1);
        }
        self.mark_cell(r, c);
    }

    /// Mark cells `[start, end)` on row `r` dirty. Saturates at the
    /// row's column count and is a no-op if `r` is out of range.
    fn mark_cells(&mut self, r: u16, start: u16, end: u16) {
        let cols = self.cols;
        if let Some(row) = self.damage.rows.get_mut(r as usize) {
            row.mark_range(start, end, cols);
        }
    }

    /// Mark every cell in row `r` dirty. Used by `erase_line(2)`,
    /// wrap, and other whole-row events.
    fn mark_row(&mut self, r: u16) {
        if let Some(row) = self.damage.rows.get_mut(r as usize) {
            row.mark_all();
        }
    }

    /// Resize the visible viewport. **Does not reflow** — that's a
    /// decision #6 / scrollback.md follow-up. The cursor is clamped to the
    /// new dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        // TODO(reflow): walk soft-wrap runs and reshape per
        // docs/decisions/scrollback.md. M3 only fixes geometry + cursor.
        let rows = rows.max(1);
        let cols = cols.max(1);
        let primary_cap = rows as usize + self.scrollback as usize;
        self.primary.resize(rows, cols, primary_cap);
        self.alt.resize(rows, cols, rows as usize);
        self.rows = rows;
        self.cols = cols;
        self.clamp_cursor();
        // Resize invalidates every cached shaped line — re-shape all.
        self.damage.resize(rows);
    }

    fn active_grid(&self) -> &Grid {
        if self.alt_active {
            &self.alt
        } else {
            &self.primary
        }
    }

    fn active_grid_mut(&mut self) -> &mut Grid {
        if self.alt_active {
            &mut self.alt
        } else {
            &mut self.primary
        }
    }

    fn clamp_cursor(&mut self) {
        if self.cursor.row >= self.rows {
            self.cursor.row = self.rows - 1;
        }
        // `col == cols` is the pending-wrap sentinel (the next print wraps).
        // We allow it on resize / alt restore so we don't silently lose
        // wrap state. Anything strictly past that gets clamped.
        if self.cursor.col > self.cols {
            self.cursor.col = self.cols;
        }
    }

    fn linefeed(&mut self) {
        if self.cursor.row + 1 >= self.rows {
            // At bottom: scroll up by one and stay on the last row.
            self.active_grid_mut().scroll_up();
            // Every visible row's content shifted up; the cached shape
            // for each row no longer matches its position. Force a
            // re-shape of all rows.
            self.mark_all_dirty();
            // Slide image placements up by 1 row (alt screen has no
            // image placements but the call is cheap).
            let dropped = self.image_grid.shift_rows_up(1, 0);
            if !dropped.is_empty() {
                self.image_revision = self.image_revision.wrapping_add(1);
            }
        } else {
            // Followup C3: a mid-screen LF moves the cursor without
            // touching any cell content. Under partial redraw, the
            // dirty-instance builder would see an empty damage set
            // and the renderer would skip the frame, leaving the old
            // cursor block painted on the original row. Mark both
            // the cell the cursor leaves and the cell it lands on so
            // the old block gets overpainted and the new block gets
            // emitted.
            let old_row = self.cursor.row;
            self.cursor.row += 1;
            let new_row = self.cursor.row;
            // Defend against the pending-wrap sentinel where
            // cursor.col may equal cols.
            let col = self.cursor.col.min(self.cols.saturating_sub(1));
            self.mark_cell(old_row, col);
            self.mark_cell(new_row, col);
        }
    }

    /// Cell-width lookup for a single codepoint.
    ///
    /// `Term::print` operates per-codepoint, not per-grapheme. M8 wires
    /// mode 2027 into the *renderer's* cluster-width snap only; the cell
    /// grid uses `UnicodeWidthChar` here. For VS16 / ZWJ clusters this
    /// means grid columns and rendered geometry can disagree — picked up
    /// in M9 when `Term::print` grows cluster awareness.
    ///
    /// Returns `1` for ordinary text / unknown chars, `2` for CJK
    /// ideographs / emoji / fullwidth forms, `0` for combining marks
    /// and other zero-width controls.
    fn char_cell_width(c: char) -> u16 {
        match UnicodeWidthChar::width(c) {
            Some(0) => 0,
            // Wide (2+) → 2 cells; narrow / unknown → 1. `None`
            // (unprintable) is treated as 1 so the cursor never gets
            // stuck on a no-width codepoint that wasn't filtered out
            // upstream.
            Some(w) if w >= 2 => 2,
            _ => 1,
        }
    }

    fn print_char(&mut self, c: char) {
        // M11a: Unicode placeholder pipeline.
        //
        // Apps emit `<U+10EEEE><diacritic*>` cells where:
        // - The cursor's SGR fg is Indexed256(N): low byte of image id.
        // - SGR 58 (cursor_underline_color) Indexed256(M): high byte
        //   of image id (optional).
        // - 0..3 diacritics encode source-image row, source-image col,
        //   and (optionally) the image id MSB extension.
        //
        // We collect cells greedily into `placeholder_run` until the
        // next non-placeholder/non-diacritic codepoint arrives, then
        // finalize → emit image placements.
        if toastty_graphics::is_placeholder(c) {
            if self.placeholder_run.is_none()
                && let Color::Indexed256(low) = self.cursor.style.fg
            {
                let high = match self.cursor_underline_color {
                    Some(Color::Indexed256(h)) => Some(h),
                    _ => None,
                };
                self.placeholder_run = Some(PlaceholderRun {
                    image_id_low: low,
                    image_id_high: high,
                    cells: Vec::new(),
                    start_row: self.cursor.row,
                });
            }
            if let Some(run) = self.placeholder_run.as_mut() {
                run.cells.push(PlaceholderCell {
                    row: self.cursor.row,
                    col: self.cursor.col.min(self.cols.saturating_sub(1)),
                    diacritics: smallvec::SmallVec::new(),
                });
            }
            // Placeholder still occupies a cell in the grid layout, so
            // fall through and write it as a normal char (cell width 1).
            // Treating it as cell-width 1 keeps text-layout sane; the
            // renderer skips drawing it via the image overlay.
            // (We still write the codepoint so partial-redraw + cursor
            // motion behaves identically to text.)
        } else if toastty_graphics::is_diacritic(c)
            && let Some(run) = self.placeholder_run.as_mut()
            && let Some(idx) = toastty_graphics::diacritic_to_index(c)
        {
            // Diacritic attaches to the most recent placeholder cell.
            if let Some(last) = run.cells.last_mut() {
                last.diacritics.push(idx);
            }
            // Diacritics are zero-width; don't advance.
            return;
        } else if let Some(run) = self.placeholder_run.take() {
            // Non-placeholder / non-diacritic codepoint: finalize the
            // run before printing this char.
            self.finalize_placeholder_run(run);
        }

        let cell_w = Self::char_cell_width(c);
        // Zero-width chars (combining marks, controls) currently fall
        // through as a no-op. A future pass can attach them to the
        // previous cell's grapheme; for M8 we just drop them so the
        // cursor doesn't advance into a dead cell.
        if cell_w == 0 {
            return;
        }

        // Wrap before printing if the cursor is past the last column
        // OR — for a width-2 char — there's only 1 column left. The
        // width-2 wrap case matches xterm: a wide char never splits.
        let needs_wrap = self.cursor.col >= self.cols
            || (cell_w == 2 && self.cursor.col + 1 >= self.cols);
        if needs_wrap {
            // Mark the row we're leaving as soft-wrapped (decision #6).
            // The cursor block was sitting on the last column of the
            // leaving row, so mark that cell dirty so the trailing
            // cursor block gets overpainted. The fresh row 0-col is
            // marked once the cursor lands there below.
            let leaving = self.cursor.row;
            self.active_grid_mut().row_mut(leaving).soft_wrap = true;
            let last_col = self.cols.saturating_sub(1);
            self.mark_cell(leaving, last_col);
            self.cursor.col = 0;
            self.linefeed();
            // After linefeed, mark the new row's col 0 (the cursor's
            // resting position). Marking the cell where the cursor
            // lands keeps the cursor block in sync under partial
            // redraw.
            self.mark_cell(self.cursor.row, 0);
        }
        let primary = Cell {
            ch: c,
            style: self.cursor.style,
            is_continuation: false,
            hyperlink_id: self.current_hyperlink,
        };
        let col = self.cursor.col;
        let row = self.cursor.row;
        let max_cols = self.cols;
        self.active_grid_mut().row_mut(row).put(col, primary, max_cols);
        if cell_w == 2 {
            // Continuation marker: '\0' with the same style.
            let cont = Cell {
                ch: '\0',
                style: self.cursor.style,
                is_continuation: true,
                hyperlink_id: self.current_hyperlink,
            };
            self.active_grid_mut()
                .row_mut(row)
                .put(col + 1, cont, max_cols);
        }
        // Mark the cell(s) just written. For a width-2 cluster, the
        // continuation cell is marked too so partial-redraw still
        // sees it.
        self.mark_cell(row, col);
        if cell_w == 2 {
            self.mark_cell(row, col + 1);
        }
        self.cursor.col += cell_w;
    }

    /// Materialize the accumulated placeholder run into image
    /// placements over the cells the run touched. Called when the
    /// stream emits a non-placeholder/non-diacritic codepoint.
    fn finalize_placeholder_run(&mut self, run: PlaceholderRun) {
        if run.cells.is_empty() {
            return;
        }
        // Compute the image id. SGR fg supplies the low byte; SGR 58
        // (cursor_underline_color) optionally supplies the high byte;
        // a third diacritic on the first cell supplies a 16-bit
        // extension. We support the common 8-bit form (fg → id),
        // optionally promoted to 16-bit via SGR 58. The 32-bit kitty
        // extension via three diacritics is documented but rare; for
        // M11a we accept the 16-bit form only.
        let id = if let Some(high) = run.image_id_high {
            u32::from(high) << 8 | u32::from(run.image_id_low)
        } else {
            u32::from(run.image_id_low)
        };
        // The cells in the run form contiguous rectangles (apps emit
        // them row-by-row). For each contiguous (row, col-range)
        // segment, emit a placement with a sub-rect derived from the
        // first/last diacritic pair in that segment.
        //
        // For M11a we materialize each cell as a 1x1 placement with
        // src_rect derived from the diacritic pair. A future
        // optimization can coalesce adjacent cells into a single
        // placement.
        if !self.image_registry.contains(id) {
            // Image not registered: still occupy the cells as
            // placeholders so the layout doesn't shift; we just don't
            // emit visible images.
            return;
        }
        // Look up image dims for the sub-rect calculation.
        let (img_w, img_h) = {
            let Some(img) = self.image_registry.get(id) else {
                return;
            };
            (img.width, img.height)
        };
        // The diacritic table maps to 0..N where N is the number of
        // cells along the relevant axis. Apps typically emit the same
        // row diacritic across one display row, with the column
        // diacritic varying. We treat the FIRST diacritic as the
        // image row, the SECOND as the image column. Without
        // metadata about cell dims we synthesize a uniform tiling: if
        // an app emits R rows worth of placeholders, the image is
        // tiled into R rows; same for columns.
        //
        // To keep this M11a-minimal, build a single placement per
        // cell whose src_rect spans (col_diacritic, row_diacritic) on
        // a uniform grid based on the maximum diacritic seen across
        // the run.
        let mut max_row_d = 0u16;
        let mut max_col_d = 0u16;
        for cell in &run.cells {
            if let Some(&r) = cell.diacritics.first() {
                max_row_d = max_row_d.max(r);
            }
            if let Some(&c) = cell.diacritics.get(1) {
                max_col_d = max_col_d.max(c);
            }
        }
        // Tile dimensions in source pixels. If diacritics are zero
        // everywhere (single-cell placement covering the full image),
        // emit a single full-image placement spanning the run's
        // bounding rect.
        let single_cell = max_row_d == 0 && max_col_d == 0;
        let mut placements = Vec::new();
        if single_cell {
            // Bounding rect of the run.
            let mut min_r = u16::MAX;
            let mut max_r = 0;
            let mut min_c = u16::MAX;
            let mut max_c = 0;
            for cell in &run.cells {
                min_r = min_r.min(cell.row);
                max_r = max_r.max(cell.row);
                min_c = min_c.min(cell.col);
                max_c = max_c.max(cell.col);
            }
            placements.push(Placement {
                image_id: id,
                placement_id: 0,
                row_range: min_r..max_r.saturating_add(1),
                col_range: min_c..max_c.saturating_add(1),
                src_rect: toastty_graphics::SrcRect::FULL,
                z: 0,
            });
        } else {
            let tile_w = if max_col_d == 0 {
                img_w
            } else {
                img_w / u32::from(max_col_d.saturating_add(1)).max(1)
            };
            let tile_h = if max_row_d == 0 {
                img_h
            } else {
                img_h / u32::from(max_row_d.saturating_add(1)).max(1)
            };
            for cell in &run.cells {
                let row_d = cell.diacritics.first().copied().unwrap_or(0);
                let col_d = cell.diacritics.get(1).copied().unwrap_or(0);
                let sx = u32::from(col_d) * tile_w;
                let sy = u32::from(row_d) * tile_h;
                placements.push(Placement {
                    image_id: id,
                    placement_id: 0,
                    row_range: cell.row..cell.row.saturating_add(1),
                    col_range: cell.col..cell.col.saturating_add(1),
                    src_rect: toastty_graphics::SrcRect {
                        x: sx,
                        y: sy,
                        w: tile_w,
                        h: tile_h,
                    },
                    z: 0,
                });
            }
        }
        for placement in placements {
            mark_placement_dirty(self, &placement);
            self.image_grid.add(placement);
        }
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    fn handle_csi(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let priv_marker = intermediates.first().copied();
        match action {
            'A' => self.cursor_up(first_param(params, 1).max(1)),
            'B' => self.cursor_down(first_param(params, 1).max(1)),
            'C' => self.cursor_forward(first_param(params, 1).max(1)),
            'D' => self.cursor_back(first_param(params, 1).max(1)),
            'H' | 'f' => {
                let r = first_param(params, 1).max(1);
                let c = nth_param(params, 1, 1).max(1);
                self.cursor_position(r, c);
            }
            'J' => self.erase_display(first_param(params, 0)),
            'K' => self.erase_line(first_param(params, 0)),
            'm' => self.apply_sgr(params),
            'h' if priv_marker == Some(b'?') => self.apply_decset(params, true),
            'l' if priv_marker == Some(b'?') => self.apply_decset(params, false),
            // DECSCUSR: `CSI Ps SP q` — runtime cursor shape + blink.
            // vte exposes the SP intermediate as `intermediates = b" "`.
            'q' if intermediates == b" " => self.apply_decscusr(first_param(params, 0)),
            // Kitty keyboard protocol stack manipulation:
            //   CSI > flags u   — push
            //   CSI < n u       — pop n (default 1)
            //   CSI = flags ; mode u — set/clear without push
            //   CSI ? u         — query (handled by the binary; no state
            //                     change here, just observable via
            //                     `Term::kitty_flags()`).
            'u' if priv_marker == Some(b'>') => {
                let f = first_param(params, 0);
                self.kitty_push((f & 0xff) as u8);
            }
            'u' if priv_marker == Some(b'<') => {
                let n = first_param(params, 1).max(1) as usize;
                self.kitty_pop(n);
            }
            'u' if priv_marker == Some(b'=') => {
                let f = first_param(params, 0);
                let mode = nth_param(params, 1, 1);
                self.kitty_set((f & 0xff) as u8, (mode & 0xff) as u8);
            }
            // `CSI ? u` (query) is handled at the binary level: it needs
            // to write the reply back to the PTY.

            // DA1 — Primary Device Attributes (`CSI c` or `CSI 0 c`).
            // Apps probe terminal capabilities at startup; many TUIs
            // (yazi, helix, neovim) wait for this reply with a short
            // timeout and refuse to start if it doesn't arrive.
            // Advertise VT220 (`62`) + ANSI color (`22`). That's enough
            // for every check we've seen and avoids the
            // "sixel?"/"unicode-core?"/"images?" probes opening up.
            'c' if priv_marker.is_none() => {
                self.pty_replies.extend_from_slice(b"\x1b[?62;22c");
            }
            // DA2 — Secondary Device Attributes (`CSI > c` / `CSI > 0 c`).
            // Reply with `CSI > <type> ; <version> ; <cartridge> c`. We
            // claim type 0 (VT100), version 0, cartridge 0 — same shape
            // as xterm/alacritty.
            'c' if priv_marker == Some(b'>') => {
                self.pty_replies.extend_from_slice(b"\x1b[>0;0;0c");
            }
            // DSR — Device Status Report.
            //   `CSI 5 n` → reply `CSI 0 n` (OK).
            //   `CSI 6 n` → cursor position; reply `CSI <row> ; <col> R`
            //               with 1-based coords.
            //   `CSI ? 6 n` → DECXCPR; same coords, `?` prefix on reply.
            'n' if priv_marker.is_none() => {
                let ps = first_param(params, 0);
                match ps {
                    5 => self.pty_replies.extend_from_slice(b"\x1b[0n"),
                    6 => {
                        let row = self.cursor.row.saturating_add(1);
                        let col = self.cursor.col.min(self.cols.saturating_sub(1)).saturating_add(1);
                        let reply = format!("\x1b[{row};{col}R");
                        self.pty_replies.extend_from_slice(reply.as_bytes());
                    }
                    _ => {}
                }
            }
            'n' if priv_marker == Some(b'?') => {
                let ps = first_param(params, 0);
                if ps == 6 {
                    let row = self.cursor.row.saturating_add(1);
                    let col = self.cursor.col.min(self.cols.saturating_sub(1)).saturating_add(1);
                    let reply = format!("\x1b[?{row};{col}R");
                    self.pty_replies.extend_from_slice(reply.as_bytes());
                }
            }

            _ => {}
        }
    }

    /// Apply DECSCUSR (`CSI Ps SP q`). Mapping per xterm:
    ///
    /// | Ps     | Shape     | Blink |
    /// | ------ | --------- | ----- |
    /// | 0 / 1  | Block     | yes   |
    /// | 2      | Block     | no    |
    /// | 3      | Underline | yes   |
    /// | 4      | Underline | no    |
    /// | 5      | Bar       | yes   |
    /// | 6      | Bar       | no    |
    ///
    /// Unknown values are ignored (current xterm behavior). No row is
    /// marked dirty — the cursor instance is rebuilt every frame
    /// regardless, and the cell grid isn't touched.
    fn apply_decscusr(&mut self, ps: u16) {
        match ps {
            0 | 1 => {
                self.cursor_shape = CursorShape::Block;
                self.cursor_blink = true;
            }
            2 => {
                self.cursor_shape = CursorShape::Block;
                self.cursor_blink = false;
            }
            3 => {
                self.cursor_shape = CursorShape::Underline;
                self.cursor_blink = true;
            }
            4 => {
                self.cursor_shape = CursorShape::Underline;
                self.cursor_blink = false;
            }
            5 => {
                self.cursor_shape = CursorShape::Bar;
                self.cursor_blink = true;
            }
            6 => {
                self.cursor_shape = CursorShape::Bar;
                self.cursor_blink = false;
            }
            _ => {}
        }
    }

    /// Move the cursor to (`new_row`, `new_col`) without touching any
    /// cell, clamping to the visible grid, and mark both the old and
    /// new cells dirty so the cursor block at the old position gets
    /// overpainted under partial redraw, and the new cell gets the
    /// fresh cursor block. Used by the bare cursor-movement CSIs
    /// (CUU/CUD/CUF/CUB/CUP) — `print` / `execute(LF/CR/BS/TAB)` mark
    /// their own cells dirty via the cell write itself.
    fn move_cursor(&mut self, new_row: u16, new_col: u16) {
        let max_row = self.rows.saturating_sub(1);
        let max_col = self.cols.saturating_sub(1);
        let new_row = new_row.min(max_row);
        let new_col = new_col.min(max_col);
        let old_row = self.cursor.row;
        let old_col = self.cursor.col.min(max_col);
        self.cursor.row = new_row;
        self.cursor.col = new_col;
        // Mark the old cell so the cursor block at that position gets
        // overpainted; mark the new cell so the fresh cursor block
        // shows up under partial redraw.
        self.mark_cell(old_row, old_col);
        self.mark_cell(new_row, new_col);
    }

    fn cursor_up(&mut self, n: u16) {
        let new_row = self.cursor.row.saturating_sub(n);
        self.move_cursor(new_row, self.cursor.col);
    }

    fn cursor_down(&mut self, n: u16) {
        let new_row = self.cursor.row.saturating_add(n);
        self.move_cursor(new_row, self.cursor.col);
    }

    fn cursor_forward(&mut self, n: u16) {
        let new_col = self.cursor.col.saturating_add(n);
        self.move_cursor(self.cursor.row, new_col);
    }

    fn cursor_back(&mut self, n: u16) {
        let mut new_col = self.cursor.col.saturating_sub(n);
        // Snap onto the start of a width-2 cluster: if the landing
        // column is a continuation cell, step one more column left so
        // the cursor lands on the cluster's primary cell. Bounds-
        // checked so we don't underflow at column 0.
        new_col = self.snap_back_off_continuation(new_col);
        self.move_cursor(self.cursor.row, new_col);
    }

    /// If `col` points at a continuation cell, return `col - 1`
    /// (the cluster's primary). Otherwise return `col` unchanged.
    /// Bounds-checked: column 0 cannot be a valid continuation, so the
    /// answer is always in-range.
    fn snap_back_off_continuation(&self, col: u16) -> u16 {
        if col == 0 || col >= self.cols {
            return col;
        }
        let cells = &self.active_grid().row(self.cursor.row).cells;
        let is_cont = cells
            .get(col as usize)
            .is_some_and(|c| c.is_continuation);
        if is_cont { col - 1 } else { col }
    }

    fn cursor_position(&mut self, row_1based: u16, col_1based: u16) {
        let new_row = row_1based.saturating_sub(1);
        let new_col = col_1based.saturating_sub(1);
        self.move_cursor(new_row, new_col);
    }

    fn erase_display(&mut self, mode: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        let rows = self.rows;
        let style = self.cursor.style;
        let grid = self.active_grid_mut();
        match mode {
            // 0: cursor to end of screen.
            0 => {
                grid.row_mut(cur_row).erase(cur_col, cols, style);
                for r in (cur_row + 1)..rows {
                    let row = grid.row_mut(r);
                    row.erase(0, cols, style);
                    row.soft_wrap = false;
                }
                // Mark only the erased range on the cursor row; full
                // row on rows below.
                self.mark_cells(cur_row, cur_col, cols);
                for r in (cur_row + 1)..rows {
                    self.mark_row(r);
                }
            }
            // 1: beginning of screen to cursor (inclusive).
            1 => {
                for r in 0..cur_row {
                    let row = grid.row_mut(r);
                    row.erase(0, cols, style);
                    row.soft_wrap = false;
                }
                grid.row_mut(cur_row)
                    .erase(0, cur_col.saturating_add(1), style);
                for r in 0..cur_row {
                    self.mark_row(r);
                }
                self.mark_cells(cur_row, 0, cur_col.saturating_add(1));
            }
            // 2/3: entire screen (3 = also scrollback, which we treat the same in M3).
            _ => {
                grid.clear_visible(style);
                self.damage.mark_all();
            }
        }
    }

    fn erase_line(&mut self, mode: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        let style = self.cursor.style;
        let row = self.active_grid_mut().row_mut(cur_row);
        let (start, end) = match mode {
            0 => (cur_col, cols),
            1 => (0, cur_col.saturating_add(1)),
            _ => (0, cols),
        };
        row.erase(start, end, style);
        self.mark_cells(cur_row, start, end);
    }

    fn apply_sgr(&mut self, params: &Params) {
        // `CSI m` (no params) and `CSI 0 m` both reset. `vte 0.15.0` always
        // pushes at least one numeric param even when none was written, but
        // we keep the defensive empty-params branch for direct callers.
        if params.is_empty() {
            self.cursor.style = Style::RESET;
            return;
        }

        // Walk the top-level params one slice at a time. The multi-param
        // SGR introducers (38/48/58) consume one or more *following*
        // top-level params on the legacy semicolon form
        // (`CSI 38;5;N m` → slices `[[38],[5],[N]]`), but read their
        // sub-params from the same slice on the ITU-T T.416 colon form
        // (`CSI 38:5:N m` → slice `[[38,5,N]]`). Both must be supported —
        // virtually every modern app uses the semicolon form, but some
        // (especially with underline color, mode 58) emit the colon form.
        //
        // Critical: the consumed params must NOT also be re-interpreted as
        // standalone SGR codes. The old implementation iterated each
        // top-level slice and called `apply_sgr_param(slice[0])`, which
        // meant a truecolor `\x1b[38;2;200;32;100m` sequence accidentally
        // ran `apply_sgr_param(32)` and set fg green. That's the leak this
        // function exists to fix.
        let mut iter = params.iter();
        while let Some(slice) = iter.next() {
            // Empty top-level params shouldn't occur from vte but treat them
            // as the implicit 0 (reset) the spec requires.
            let head = slice.first().copied().unwrap_or(0);
            match head {
                38 if slice.len() >= 2 => self.cursor.style.fg = parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.fg),
                48 if slice.len() >= 2 => self.cursor.style.bg = parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.bg),
                58 if slice.len() >= 2 => {
                    // M11a: SGR 58 underline color is stored on
                    // `cursor_underline_color`. The Unicode placeholder
                    // pipeline reads this as the *high byte* of the
                    // image id (kitty's protocol packs id MSB into the
                    // 256-color underline slot).
                    self.cursor_underline_color = parse_extended_color_from_slice(&slice[1..]);
                }
                38 => {
                    // Semicolon form: consume from the outer iterator.
                    let color = parse_extended_color_from_iter(&mut iter);
                    if let Some(c) = color {
                        self.cursor.style.fg = c;
                    }
                }
                48 => {
                    let color = parse_extended_color_from_iter(&mut iter);
                    if let Some(c) = color {
                        self.cursor.style.bg = c;
                    }
                }
                58 => {
                    // M11a: semicolon form for SGR 58 — store on
                    // `cursor_underline_color`.
                    self.cursor_underline_color = parse_extended_color_from_iter(&mut iter);
                }
                // SGR 59 — reset underline color to default.
                59 => {
                    self.cursor_underline_color = None;
                }
                // SGR 0 (reset) — clear the underline color too.
                0 => {
                    self.cursor_underline_color = None;
                    self.apply_sgr_param(0);
                }
                v => self.apply_sgr_param(v),
            }
        }
    }

    fn apply_sgr_param(&mut self, v: u16) {
        let style = &mut self.cursor.style;
        match v {
            0 => *style = Style::RESET,
            1 => style.flags.bold = true,
            3 => style.flags.italic = true,
            4 => style.flags.underline = true,
            7 => style.flags.reverse = true,
            22 => style.flags.bold = false,
            23 => style.flags.italic = false,
            24 => style.flags.underline = false,
            27 => style.flags.reverse = false,
            30..=37 => style.fg = ansi_color(v - 30, false),
            39 => style.fg = Color::Default,
            40..=47 => style.bg = ansi_color(v - 40, false),
            49 => style.bg = Color::Default,
            // M11a: 59 resets the cursor underline color (placeholder
            // image-id MSB). The SGR walker handles this branch by
            // calling `apply_sgr_param(59)`, which now lives here
            // rather than falling through to the wildcard.
            59 => {
                // Falls through — we can't touch self.cursor_underline_color
                // from here because we only have `&mut style`. Reset
                // happens in `apply_sgr` for the top-level walk; see
                // the explicit clear there.
            }
            90..=97 => style.fg = ansi_color(v - 90, true),
            100..=107 => style.bg = ansi_color(v - 100, true),
            _ => {}
        }
    }

    fn apply_decset(&mut self, params: &Params, enable: bool) {
        for sub in params {
            let Some(&code) = sub.first() else {
                continue;
            };
            match code {
                1049 => {
                    if enable {
                        self.enter_alt_screen();
                    } else {
                        self.exit_alt_screen();
                    }
                }
                // 1000 — X10 / VT200 button reporting (press + release).
                1000 => {
                    self.mouse_mode.protocol = if enable {
                        MouseProtocol::X10
                    } else {
                        MouseProtocol::Off
                    };
                }
                // 1002 — button-event tracking (press/release + drag).
                1002 => {
                    self.mouse_mode.protocol = if enable {
                        MouseProtocol::ButtonMotion
                    } else {
                        MouseProtocol::Off
                    };
                }
                // 1003 — any-event tracking (all motion regardless of buttons).
                1003 => {
                    self.mouse_mode.protocol = if enable {
                        MouseProtocol::AnyMotion
                    } else {
                        MouseProtocol::Off
                    };
                }
                // 1004 — focus events.
                1004 => {
                    self.report_focus = enable;
                }
                // 1006 — SGR extended mouse encoding.
                1006 => {
                    self.mouse_mode.sgr_encoding = enable;
                }
                // 2004 — bracketed paste.
                2004 => {
                    self.bracketed_paste = enable;
                }
                // 2026 — synchronized output (BSU/ESU).
                2026 => {
                    self.set_sync_output(enable);
                }
                // 2027 — grapheme cluster processing opt-in.
                2027 => {
                    self.grapheme_cluster_mode = enable;
                }
                // 2048 — in-band resize notifications.
                2048 => {
                    self.inband_resize_mode = enable;
                }
                // TODO(modes): 1, 7, 12, 25, etc.
                _ => {}
            }
        }
    }

    fn enter_alt_screen(&mut self) {
        if self.alt_active {
            return;
        }
        self.saved_cursor = self.cursor;
        self.alt_active = true;
        self.alt.clear_visible(Style::RESET);
        // Reset cursor to home and clear style for the alt screen.
        self.cursor = Cursor::default();
        // Switching screens invalidates every cached shaped line.
        self.mark_all_dirty();
        // Alt screen has no image placements in M11a — clear so apps
        // can't accidentally see stale images from the primary screen.
        let dropped = self.image_grid.clear();
        if !dropped.is_empty() {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    fn exit_alt_screen(&mut self) {
        if !self.alt_active {
            return;
        }
        self.alt_active = false;
        self.cursor = self.saved_cursor;
        self.clamp_cursor();
        // Switching back: re-shape the primary screen contents.
        self.mark_all_dirty();
        // Same policy on exit: clear image placements (the primary
        // screen's images were not preserved across the alt-screen
        // switch in M11a).
        let dropped = self.image_grid.clear();
        if !dropped.is_empty() {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }
}

fn first_param(params: &Params, default: u16) -> u16 {
    nth_param(params, 0, default)
}

fn nth_param(params: &Params, n: usize, default: u16) -> u16 {
    params
        .iter()
        .nth(n)
        .and_then(|sub| sub.first().copied())
        .filter(|&v| v != 0)
        .unwrap_or(default)
}

/// Parse a `38/48/58` extended-color introducer's sub-parameters from a
/// single sub-param slice — i.e. the ITU-T T.416 colon form like
/// `CSI 38:5:42m` (which `vte 0.15` exposes as one slice `[38, 5, 42]`).
/// Caller passes the slice *after* the leading 38/48/58, i.e. `[5, 42]`
/// or `[2, R, G, B]` (or the 5-element `[2, Pi, R, G, B]` with the T.416
/// color-space identifier we ignore).
///
/// Returns `None` for malformed input (insufficient sub-params, unknown
/// kind). The caller has already consumed the sub-params either way, so
/// nothing leaks back into the SGR stream.
fn parse_extended_color_from_slice(rest: &[u16]) -> Option<Color> {
    match rest.first().copied()? {
        // Indexed: next sub-param is the 0..256 palette index.
        5 => rest.get(1).map(|n| Color::Indexed256(clamp_u8(*n))),
        // Truecolor. The canonical T.416 form is `[2, Pi, R, G, B]` with a
        // color-space identifier; the widely-deployed shortcut form omits
        // it (`[2, R, G, B]`). xterm and alacritty both accept both. We
        // mirror that: 5+ sub-params → skip the identifier; 4 sub-params →
        // treat the first as R.
        2 => {
            // `rest.len() >= N` below guarantees the indexing is in-bounds,
            // so we use direct slice access — `rest.get(..).copied()?`
            // would produce unreachable short-circuit branches that clippy
            // and coverage both flag.
            let (r, g, b) = if rest.len() >= 5 {
                (rest[2], rest[3], rest[4])
            } else if rest.len() >= 4 {
                (rest[1], rest[2], rest[3])
            } else {
                return None;
            };
            Some(Color::Rgb(clamp_u8(r), clamp_u8(g), clamp_u8(b)))
        }
        _ => None,
    }
}

/// Parse a `38/48/58` extended-color from the legacy semicolon form
/// (`CSI 38;5;42m` → slices `[[38],[5],[42]]`). The caller has already
/// consumed the leading `38/48/58` slice from `iter`. We read the kind
/// (5 or 2) and the appropriate number of *following* top-level params,
/// returning `None` on malformed input. Crucially, **we always consume
/// the expected number of params** so they cannot leak back into the
/// outer SGR walker.
fn parse_extended_color_from_iter<'a, I>(iter: &mut I) -> Option<Color>
where
    I: Iterator<Item = &'a [u16]>,
{
    let kind = iter.next().and_then(|s| s.first().copied())?;
    match kind {
        5 => {
            let idx = iter.next().and_then(|s| s.first().copied())?;
            Some(Color::Indexed256(clamp_u8(idx)))
        }
        2 => {
            // Semicolon form is always 3-component RGB. The T.416
            // color-space identifier is only meaningful in the colon
            // form and not transmitted via `;`-separated params.
            let r = iter.next().and_then(|s| s.first().copied())?;
            let g = iter.next().and_then(|s| s.first().copied())?;
            let b = iter.next().and_then(|s| s.first().copied())?;
            Some(Color::Rgb(clamp_u8(r), clamp_u8(g), clamp_u8(b)))
        }
        _ => None,
    }
}

/// Saturating cast — SGR params arrive as `u16`; valid values are 0..256.
fn clamp_u8(v: u16) -> u8 {
    u8::try_from(v).unwrap_or(u8::MAX)
}

fn ansi_color(idx: u16, bright: bool) -> Color {
    match (idx, bright) {
        (0, false) => Color::Black,
        (1, false) => Color::Red,
        (2, false) => Color::Green,
        (3, false) => Color::Yellow,
        (4, false) => Color::Blue,
        (5, false) => Color::Magenta,
        (6, false) => Color::Cyan,
        (7, false) => Color::White,
        (0, true) => Color::BrightBlack,
        (1, true) => Color::BrightRed,
        (2, true) => Color::BrightGreen,
        (3, true) => Color::BrightYellow,
        (4, true) => Color::BrightBlue,
        (5, true) => Color::BrightMagenta,
        (6, true) => Color::BrightCyan,
        (7, true) => Color::BrightWhite,
        _ => Color::Default,
    }
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.print_char(c);
    }

    fn execute(&mut self, byte: u8) {
        // Any non-print event terminates an in-flight placeholder run.
        if let Some(run) = self.placeholder_run.take() {
            self.finalize_placeholder_run(run);
        }
        match byte {
            b'\r' => {
                // CR moves the cursor in-place to column 0. Mark old
                // and new cells so the cursor block at the old column
                // gets overpainted and the new one shows up.
                let row = self.cursor.row;
                let max_col = self.cols.saturating_sub(1);
                let old_col = self.cursor.col.min(max_col);
                self.cursor.col = 0;
                self.mark_cell(row, old_col);
                self.mark_cell(row, 0);
            }
            b'\n' | 0x0B | 0x0C => self.linefeed(),
            0x08 => {
                // BS: move cursor left one, no wrap. Snap off the
                // continuation half of a wide cluster so two BSes
                // in a row don't strand the cursor inside a CJK
                // ideograph.
                //
                // Mark the old and new cells so the cursor block at
                // the old column gets overpainted and the new one
                // shows up under partial redraw (latent damage gap
                // fix in M9.4).
                if self.cursor.col > 0 {
                    let row = self.cursor.row;
                    let max_col = self.cols.saturating_sub(1);
                    let old_col = self.cursor.col.min(max_col);
                    self.cursor.col -= 1;
                    self.cursor.col = self.snap_back_off_continuation(self.cursor.col);
                    self.mark_cell(row, old_col);
                    self.mark_cell(row, self.cursor.col);
                }
            }
            b'\t' => {
                // HT: advance to next multiple of TAB_WIDTH, clamped.
                // Same damage-gap fix as BS.
                let row = self.cursor.row;
                let max_col = self.cols.saturating_sub(1);
                let old_col = self.cursor.col.min(max_col);
                let next = (self.cursor.col / TAB_WIDTH + 1) * TAB_WIDTH;
                self.cursor.col = next.min(max_col);
                self.mark_cell(row, old_col);
                self.mark_cell(row, self.cursor.col);
            }
            // BEL and everything else are no-ops for M3.
            _ => {}
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        if let Some(run) = self.placeholder_run.take() {
            self.finalize_placeholder_run(run);
        }
        self.handle_csi(params, intermediates, action);
    }

    /// OSC dispatch. Currently handles the title-setting variants:
    ///
    /// - `OSC 0 ; <title> ST` — set both icon and window title.
    /// - `OSC 1 ; <title> ST` — set the icon title only. We have no
    ///   tray icon, so this is parsed but not acted on. (We could
    ///   stash it on `self` for future use; for v0.1 we just ignore.)
    /// - `OSC 2 ; <title> ST` — set the window title only.
    ///
    /// The payload bytes are not guaranteed to be UTF-8 by the spec,
    /// but in practice nearly every emitter is. We lossy-decode so a
    /// rogue byte can't crash the terminal.
    ///
    /// All other OSC codes (4 palette, 8 hyperlinks, 10/11 fg/bg query,
    /// 52 clipboard, ...) are deferred to later milestones — silently
    /// ignored here so they don't trip on stale payloads.
    ///
    /// Note: title changes do **not** mark any row dirty. The renderer's
    /// per-frame cursor pass already runs every frame; the title lives
    /// outside the grid entirely.
    #[allow(clippy::too_many_lines)] // each OSC arm is small but the table is wide.
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        // OSC must have at least one param: the code.
        let Some(code_bytes) = params.first() else {
            return;
        };
        // Parse the code as ASCII digits. Non-numeric → unknown OSC.
        let Ok(code_str) = std::str::from_utf8(code_bytes) else {
            return;
        };
        let Ok(code) = code_str.parse::<u32>() else {
            return;
        };
        match code {
            // 0 = set icon + window title; 2 = set window title.
            // 1 is icon-only — we have no tray icon so don't change
            // anything, but accept the sequence cleanly.
            0 | 2 => {
                let payload = params.get(1).copied().unwrap_or(b"");
                let title = String::from_utf8_lossy(payload).into_owned();
                self.title = title;
            }
            // OSC 4 — extended palette query / set.
            //
            // Payload is a sequence of `(idx, spec)` pairs. vte splits
            // on `;`, so `OSC 4 ; 1 ; rgb:ab/cd/ef ; 2 ; ?` arrives as
            // `params = [b"4", b"1", b"rgb:ab/cd/ef", b"2", b"?"]`. Walk
            // pairs in steps of two starting at index 1.
            4 => {
                let rest = &params[1..];
                let mut i = 0;
                while i + 1 < rest.len() {
                    if let Some(op) =
                        toastty_protocols::palette::parse_pair(rest[i], rest[i + 1])
                    {
                        match op {
                            toastty_protocols::palette::Osc4Op::Query { idx } => {
                                let rgb = self.palette_override(idx).unwrap_or_else(|| {
                                    toastty_protocols::palette::default_xterm_256(idx)
                                });
                                let reply =
                                    toastty_protocols::palette::encode_query_reply(idx, rgb);
                                self.pty_replies.extend_from_slice(&reply);
                            }
                            toastty_protocols::palette::Osc4Op::Set { idx, rgb } => {
                                self.set_palette_override(idx, rgb);
                            }
                        }
                    }
                    i += 2;
                }
            }
            // OSC 52 — clipboard. Gated by `security.osc_52_read /
            // osc_52_write`. Both gates default to off (silently drop)
            // so a casual escape sequence in a less(1) output can't
            // hijack the clipboard.
            //
            // vte splits the payload on `;`, so an emitted
            // `OSC 52 ; c ; aGVsbG8=` arrives as
            // `params = [b"52", b"c", b"aGVsbG8="]`. Rejoin past the
            // leading code.
            52 => {
                let mut joined = Vec::new();
                for (i, p) in params.iter().enumerate().skip(1) {
                    if i > 1 {
                        joined.push(b';');
                    }
                    joined.extend_from_slice(p);
                }
                if let Some(op) = toastty_protocols::clipboard::parse(&joined) {
                    match op {
                        toastty_protocols::clipboard::Osc52Op::Set { payload, .. } => {
                            if self.security.osc_52_write {
                                self.clipboard_requests
                                    .push(ClipboardRequest::Set { data: payload });
                            }
                        }
                        toastty_protocols::clipboard::Osc52Op::Query { selection } => {
                            if self.security.osc_52_read {
                                self.clipboard_requests
                                    .push(ClipboardRequest::Query { selection: selection.0 });
                            }
                        }
                    }
                }
            }
            // OSC 8 — hyperlinks.
            //
            // Payload format past `8;` is `<params>;<url>`. vte splits
            // on `;`, so an emitted `OSC 8 ; id=foo ; https://x` arrives
            // as `params = [b"8", b"id=foo", b"https://x"]`. Rejoin past
            // the leading code so the parser sees one byte string.
            //
            // `OSC 8 ; ; ST` (empty URL) closes the active hyperlink —
            // future printed cells are stamped with `None`.
            8 => {
                let mut joined = Vec::new();
                for (i, p) in params.iter().enumerate().skip(1) {
                    if i > 1 {
                        joined.push(b';');
                    }
                    joined.extend_from_slice(p);
                }
                if let Some(parsed) = toastty_protocols::hyperlink::parse(&joined) {
                    if parsed.url.is_empty() {
                        self.current_hyperlink = None;
                    } else {
                        self.current_hyperlink = self.intern_hyperlink(parsed.url);
                    }
                }
            }
            // OSC 7 — current working directory (`file://<host>/<path>`).
            //
            // The URL itself shouldn't contain semicolons, but `;` is a
            // valid byte inside a percent-decoded path (RFC 3986 reserves
            // it). vte will have split on it already, so rejoin params
            // past the code so we don't drop trailing segments.
            7 => {
                let mut joined = Vec::new();
                for (i, p) in params.iter().enumerate().skip(1) {
                    if i > 1 {
                        joined.push(b';');
                    }
                    joined.extend_from_slice(p);
                }
                if let Some(path) = toastty_protocols::osc_cwd::parse_file_url(&joined) {
                    self.cwd = path;
                }
            }
            // OSC 133 — semantic prompt markers (Final Term protocol).
            //
            // vte splits the OSC params on `;`, so an emitted
            // `OSC 133 ; D ; 0` arrives as
            // `params = [b"133", b"D", b"0"]`. We rejoin everything past
            // the leading code byte before handing off to the parser.
            133 => {
                let mut joined = Vec::new();
                for (i, p) in params.iter().enumerate().skip(1) {
                    if i > 1 {
                        joined.push(b';');
                    }
                    joined.extend_from_slice(p);
                }
                if let Some(kind) = toastty_protocols::semantic_prompt::parse(&joined) {
                    let mapped = match kind {
                        toastty_protocols::semantic_prompt::PromptKind::PromptStart => {
                            PromptMarkKind::PromptStart
                        }
                        toastty_protocols::semantic_prompt::PromptKind::PromptEnd => {
                            PromptMarkKind::PromptEnd
                        }
                        toastty_protocols::semantic_prompt::PromptKind::CommandStart => {
                            PromptMarkKind::CommandStart
                        }
                        toastty_protocols::semantic_prompt::PromptKind::CommandFinished(c) => {
                            PromptMarkKind::CommandFinished(c)
                        }
                    };
                    self.push_prompt_mark(mapped);
                }
            }
            // 1 = icon-title only — accepted but not acted on (we have
            // no tray icon). Folded into the wildcard arm: same body
            // either way.
            _ => {
                // 1 + unknown OSC: silently ignored. M10 adds 4 / 8 / 52
                // in later steps.
            }
        }
    }

    // DCS / APC / hyperlinks / kitty keyboard / mode 2026 etc. all
    // deferred. TODOs live in lib-level docs.

    fn apc_start(&mut self) {
        self.apc_buffer.clear();
    }

    fn apc_chunk(&mut self, bytes: &[u8]) {
        // Cap on a single APC packet's buffered bytes — defends against
        // a hostile stream sending an unbounded APC payload that's
        // larger than the kitty handler's per-upload cap. 256 MiB
        // here matches the registry default cap.
        const APC_BUFFER_CAP: usize = 256 * 1024 * 1024;
        if self.apc_buffer.len().saturating_add(bytes.len()) > APC_BUFFER_CAP {
            // Drop further chunks for this packet — `apc_end` will see
            // a truncated buffer and fail header parsing cleanly.
            return;
        }
        self.apc_buffer.extend_from_slice(bytes);
    }

    fn apc_end(&mut self) {
        // Take ownership of the buffered payload so we can borrow
        // `self` mutably as the sink. Defensive: if the payload doesn't
        // start with `G`, it's not a Kitty graphics packet — silently
        // drop. (Other APC users — e.g. tmux passthrough — might pass
        // through; we ignore them for M11a.)
        let payload = std::mem::take(&mut self.apc_buffer);
        if payload.is_empty() || payload[0] != b'G' {
            return;
        }
        // Split on the first `;` into header vs body.
        let split = payload.iter().position(|&b| b == b';');
        let (header_bytes, body): (&[u8], &[u8]) = match split {
            Some(idx) => (&payload[..idx], &payload[idx + 1..]),
            None => (&payload[..], &[]),
        };
        // Pull the handler out so we can pass &mut self as the sink.
        let mut handler = std::mem::take(&mut self.image_handler);
        // Swallow the Result — header errors do not reach the sink so
        // there's no reply to emit. (A future enhancement could push a
        // synthetic error reply here.)
        let _ = handler.process(header_bytes, body, self);
        self.image_handler = handler;
    }
}

// ---- M11a: KittySink ----

impl KittySink for Term {
    fn register_image(&mut self, id_request: u32, data: ImageData) -> Option<u32> {
        match self.image_registry.insert(id_request, data) {
            Ok(inserted) => {
                // Evicted ids no longer exist in the registry; drop their
                // placements + mark cells dirty.
                for evicted in &inserted.evicted {
                    let dropped = self.image_grid.remove_image(*evicted);
                    for p in dropped {
                        mark_placement_dirty(self, &p);
                    }
                }
                self.image_revision = self.image_revision.wrapping_add(1);
                Some(inserted.id)
            }
            Err(_) => None,
        }
    }

    fn place_image(&mut self, mut placement: Placement) {
        // The handler emits placements with row/col fields populated
        // from header.cell_x / cell_y. For a *transmit-and-place*, the
        // semantic is "place at the current cursor" — translate the
        // placement against the cursor row/col so the placement lands
        // where the app expects.
        //
        // For a standalone `a=p` (place an already-transmitted image),
        // the cell_x/cell_y fields are absolute; we leave them alone.
        // Distinguishing the two paths from inside the sink isn't
        // possible without extra state — the handler always carries the
        // header's cell_x/cell_y. For M11a we treat all placements as
        // relative-to-cursor, which matches the way `tput` / `kitty
        // +kitten icat` actually drive the protocol.
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col.min(self.cols.saturating_sub(1));
        let row_span = placement.row_range.end - placement.row_range.start;
        let col_span = placement.col_range.end - placement.col_range.start;
        placement.row_range = cur_row..cur_row.saturating_add(row_span);
        placement.col_range = cur_col..cur_col.saturating_add(col_span);
        // Clamp to grid.
        if placement.row_range.end > self.rows {
            placement.row_range.end = self.rows;
        }
        if placement.col_range.end > self.cols {
            placement.col_range.end = self.cols;
        }
        // Mark dirty BEFORE consuming `placement` into the grid.
        mark_placement_dirty(self, &placement);
        self.image_grid.add(placement);
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    fn delete_image(&mut self, delete: DeleteSpec, header: &toastty_graphics::kitty::header::Header) {
        // Treat empty / unknown specs the same as `a` (all).
        let spec_byte = if delete.byte == 0 { b'a' } else { delete.byte };
        let mut dropped_placements = Vec::new();
        let drop_bytes = delete.free_bytes();
        match spec_byte {
            // `a` / `A` — delete all visible placements (and bytes if
            // uppercase).
            b'a' | b'A' => {
                dropped_placements.extend(self.image_grid.clear());
                if drop_bytes {
                    let ids: Vec<u32> = self.image_registry.ids().collect();
                    for id in ids {
                        self.image_registry.remove(id);
                    }
                }
            }
            // `i` / `I` — by image id (provided via `i=`).
            b'i' | b'I' => {
                if header.image_id != 0 {
                    dropped_placements.extend(self.image_grid.remove_image(header.image_id));
                    if drop_bytes {
                        self.image_registry.remove(header.image_id);
                    }
                }
            }
            // `n` / `N` — by image *number* (provided via `I=`). We
            // don't track image-number→id mapping yet; fall through as
            // a no-op.
            // `p` / `P` — by (image id, placement id). The grid filter
            // matches both fields.
            b'p' | b'P' => {
                let img = header.image_id;
                let pid = header.placement_id;
                dropped_placements.extend(self.image_grid.remove_where(|p| {
                    p.image_id == img && p.placement_id == pid
                }));
            }
            // `r` / `R` — by row.
            b'r' | b'R' => {
                if header.cell_y < u32::from(self.rows) {
                    let row = header.cell_y as u16;
                    dropped_placements.extend(self.image_grid.clear_row(row));
                }
            }
            // Other specs (`c` cell, `x`/`y` columns/rows, `z` by z, `q`
            // by ranges, ...) are deferred for M11a follow-ups.
            _ => {}
        }
        for p in &dropped_placements {
            mark_placement_dirty(self, p);
        }
        if !dropped_placements.is_empty() || drop_bytes {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    fn queue_reply(&mut self, bytes: &[u8]) {
        self.pty_replies.extend_from_slice(bytes);
    }

    fn pending_budget_remaining(&self) -> u64 {
        // Take registry budget minus the bytes already buffered in the
        // APC reassembly buffer.
        let used_buf = self.apc_buffer.len() as u64;
        self.image_registry
            .budget_remaining()
            .saturating_sub(used_buf)
    }

    fn advance_cursor_after_placement(&mut self, rows: u16, _cols: u16) {
        // Kitty docs say: after T, the cursor moves to the cell *below*
        // the bottom-left of the placement. We approximate by moving
        // down by `rows` and resetting column to 0 — close enough for
        // common `kitty +kitten icat` usage.
        //
        // If the cursor would land below the visible viewport, we let
        // `linefeed` scroll the grid (which also shifts image rows up).
        for _ in 0..rows {
            self.linefeed();
        }
        self.cursor.col = 0;
    }
}

fn mark_placement_dirty(t: &mut Term, p: &Placement) {
    let rows = t.rows;
    let cols = t.cols;
    let r_end = p.row_range.end.min(rows);
    let c_end = p.col_range.end.min(cols);
    for r in p.row_range.start..r_end {
        t.mark_cells(r, p.col_range.start, c_end);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cell::{Color, StyleFlags};
    use toastty_parser::Parser;

    /// Feed `bytes` through a fresh parser into `t`.
    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// Build a `Term`, feed it bytes, return it.
    fn run(rows: u16, cols: u16, bytes: &[u8]) -> Term {
        let mut t = Term::new(rows, cols, 0);
        feed(&mut t, bytes);
        t
    }

    /// Stringify a row, trimming trailing blanks.
    fn row_text(t: &Term, r: u16) -> String {
        let mut s: String = t.row(r).cells.iter().map(|c| c.ch).collect();
        while s.ends_with(' ') {
            s.pop();
        }
        s
    }

    /// Test helper: per-row dirty view derived from the damage set.
    /// Mirrors the old `Term::dirty_rows()` shim that the renderer used
    /// to read. Kept tests-only so the migration to per-cell damage
    /// doesn't churn 20+ assert sites.
    fn dirty_rows(t: &Term) -> Vec<bool> {
        t.damage()
            .rows
            .iter()
            .map(|r| !r.is_empty())
            .collect()
    }

    #[test]
    fn new_initialises_blank_grid_and_cursor() {
        let t = Term::new(3, 4, 8);
        assert_eq!(t.size(), (3, 4));
        assert_eq!(t.cursor(), Cursor::default());
        assert!(!t.is_alt_active());
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "");
        }
    }

    #[test]
    fn new_clamps_zero_dimensions_to_one() {
        // The renderer can't display zero rows or columns; the constructor
        // should round up rather than build an unusable grid.
        let t = Term::new(0, 0, 0);
        assert_eq!(t.size(), (1, 1));
    }

    #[test]
    fn plain_text_lands_in_row_zero() {
        let t = run(3, 8, b"hello");
        assert_eq!(row_text(&t, 0), "hello");
        assert_eq!(t.cursor().col, 5);
        assert_eq!(t.cursor().row, 0);
    }

    #[test]
    fn cr_returns_to_col_zero() {
        let t = run(3, 8, b"abc\rx");
        assert_eq!(row_text(&t, 0), "xbc");
    }

    #[test]
    fn lf_moves_to_next_row() {
        let t = run(3, 8, b"a\nb");
        assert_eq!(row_text(&t, 0), "a");
        // LF does not move to col 0 — that's CR's job. Cursor stayed at col 1.
        assert_eq!(t.cursor().col, 2);
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.row(1).cells[1].ch, 'b');
    }

    #[test]
    fn crlf_starts_fresh_line() {
        let t = run(3, 8, b"a\r\nb");
        assert_eq!(row_text(&t, 0), "a");
        assert_eq!(row_text(&t, 1), "b");
    }

    #[test]
    fn lf_at_bottom_scrolls() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"a\r\nb\r\nc");
        // After LF on the bottom row, "a" should have scrolled into
        // history; visible is now "b" then "c".
        assert_eq!(row_text(&t, 0), "b");
        assert_eq!(row_text(&t, 1), "c");
    }

    #[test]
    fn vertical_tab_and_form_feed_act_like_lf() {
        // Real terminals treat 0x0B and 0x0C as LF for index motion.
        let t = run(3, 4, b"a\x0bb\x0cc");
        assert_eq!(row_text(&t, 0), "a");
        assert_eq!(row_text(&t, 1).trim_end(), " b");
        assert_eq!(t.row(2).cells[2].ch, 'c');
    }

    #[test]
    fn backspace_moves_cursor_left_but_does_not_wrap() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"ab\x08");
        assert_eq!(t.cursor().col, 1);
        // Backspace at column 0 is a no-op (must not underflow).
        feed(&mut t, b"\x08\x08\x08\x08");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn tab_advances_to_next_tab_stop() {
        // cols=24 so we have multiple stops without clamping.
        let mut t = Term::new(2, 24, 0);
        feed(&mut t, b"\t");
        assert_eq!(t.cursor().col, 8);
        feed(&mut t, b"a\t");
        assert_eq!(t.cursor().col, 16);
        feed(&mut t, b"\t");
        // Already at multiple of 8 — tab advances to the next one.
        assert_eq!(t.cursor().col, 24 - 1); // clamps to last col since cols=24
    }

    #[test]
    fn tab_clamps_at_last_column() {
        let mut t = Term::new(2, 10, 0);
        // Print enough to push past the next tab stop; tab should clamp.
        feed(&mut t, b"abcdef\t");
        assert_eq!(t.cursor().col, 8);
        feed(&mut t, b"\t");
        // Next tab stop would be 16, but cols=10, so clamp to last column = 9.
        assert_eq!(t.cursor().col, 9);
    }

    #[test]
    fn bel_is_a_noop() {
        let t = run(2, 4, b"a\x07b");
        assert_eq!(row_text(&t, 0), "ab");
    }

    #[test]
    fn print_wraps_at_end_of_line_and_marks_soft_wrap() {
        let t = run(3, 4, b"hello");
        assert_eq!(row_text(&t, 0), "hell");
        assert_eq!(row_text(&t, 1), "o");
        assert!(t.row(0).soft_wrap);
        assert!(!t.row(1).soft_wrap);
    }

    #[test]
    fn cursor_moves_table_driven() {
        // (initial_seq, op_seq, expected_row, expected_col)
        let cases: &[(&[u8], &[u8], u16, u16)] = &[
            // CUU — up
            (b"\r\n\r\n\r\n", b"\x1b[2A", 1, 0),
            // CUD — down
            (b"", b"\x1b[2B", 2, 0),
            // CUF — forward
            (b"", b"\x1b[3C", 0, 3),
            // CUB — back (after some text)
            (b"abcd", b"\x1b[2D", 0, 2),
            // CUP — absolute position 2;3 (1-based)
            (b"", b"\x1b[2;3H", 1, 2),
            // CUP with implicit defaults goes home
            (b"abcd\n\rxy", b"\x1b[H", 0, 0),
            // 'f' is an alias for CUP
            (b"", b"\x1b[3;2f", 2, 1),
            // Movement with zero param treated as 1
            (b"", b"\x1b[0C", 0, 1),
            // Movement clamps to grid edges
            (b"", b"\x1b[99C", 0, 7),
            (b"", b"\x1b[99B", 4, 0),
        ];
        for (init, op, want_r, want_c) in cases.iter().copied() {
            let mut t = Term::new(5, 8, 0);
            feed(&mut t, init);
            feed(&mut t, op);
            let cur = t.cursor();
            assert_eq!(
                (cur.row, cur.col),
                (want_r, want_c),
                "init={init:?} op={op:?}",
            );
        }
    }

    #[test]
    fn erase_display_modes() {
        // mode 0 — cursor to end
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[1;3H\x1b[0J");
        assert_eq!(row_text(&t, 0), "aa");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(row_text(&t, 2), "");

        // mode 1 — start to cursor
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[2;3H\x1b[1J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1).trim_end(), "   bb");
        assert_eq!(row_text(&t, 2), "ccccc");

        // mode 2 — everything
        let mut t = Term::new(3, 5, 0);
        feed(&mut t, b"aaaaa\r\nbbbbb\r\nccccc\x1b[2J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(row_text(&t, 2), "");

        // mode 3 — also clears scrollback; in M3 same as mode 2.
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"abcd\r\nefgh\x1b[3J");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1), "");
    }

    #[test]
    fn erase_line_modes() {
        // Use CUP (`H`) to reposition to row 1, col 3 — `abcdef` then jump
        // back to col 3 (1-based) = idx 2. EL covers the rest.
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[0K");
        assert_eq!(row_text(&t, 0), "ab");
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[1K");
        assert_eq!(row_text(&t, 0).trim_end(), "   def");
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[2K");
        assert_eq!(row_text(&t, 0), "");
    }

    #[test]
    fn sgr_single_param_sets_fg_color() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31mr");
        assert_eq!(t.row(0).cells[0].style.fg, Color::Red);
    }

    #[test]
    fn sgr_multi_param_applies_in_order() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;3;31;44mx");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Red);
        assert_eq!(s.bg, Color::Blue);
        assert!(s.flags.bold);
        assert!(s.flags.italic);
    }

    #[test]
    fn sgr_reset_clears_everything() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31mA\x1b[0mB");
        assert!(t.row(0).cells[0].style.flags.bold);
        assert_eq!(t.row(0).cells[1].style, Style::RESET);
    }

    #[test]
    fn sgr_empty_means_reset() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31mA\x1b[mB");
        assert_eq!(t.row(0).cells[1].style, Style::RESET);
    }

    #[test]
    fn sgr_attribute_reset_codes() {
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, b"\x1b[1;3;4;7mA\x1b[22;23;24;27mB");
        let a = t.row(0).cells[0].style.flags;
        assert_eq!(
            a,
            StyleFlags {
                bold: true,
                italic: true,
                underline: true,
                reverse: true
            }
        );
        let b = t.row(0).cells[1].style.flags;
        assert_eq!(b, StyleFlags::default());
    }

    #[test]
    fn sgr_bright_and_default_colors() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[91;104mA");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::BrightRed);
        assert_eq!(s.bg, Color::BrightBlue);
        feed(&mut t, b"\x1b[39;49mB");
        let s = t.row(0).cells[1].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_full_color_table() {
        // Every standard + bright slot maps to the expected variant.
        let pairs: &[(u16, Color)] = &[
            (30, Color::Black),
            (31, Color::Red),
            (32, Color::Green),
            (33, Color::Yellow),
            (34, Color::Blue),
            (35, Color::Magenta),
            (36, Color::Cyan),
            (37, Color::White),
            (90, Color::BrightBlack),
            (91, Color::BrightRed),
            (92, Color::BrightGreen),
            (93, Color::BrightYellow),
            (94, Color::BrightBlue),
            (95, Color::BrightMagenta),
            (96, Color::BrightCyan),
            (97, Color::BrightWhite),
        ];
        for (code, want) in pairs.iter().copied() {
            let mut t = Term::new(1, 1, 0);
            feed(&mut t, format!("\x1b[{code}mX").as_bytes());
            assert_eq!(t.row(0).cells[0].style.fg, want, "fg code {code}");
            let bg_code = code + 10;
            let mut t = Term::new(1, 1, 0);
            feed(&mut t, format!("\x1b[{bg_code}mX").as_bytes());
            assert_eq!(t.row(0).cells[0].style.bg, want, "bg code {bg_code}");
        }
    }

    #[test]
    fn sgr_unknown_param_is_ignored() {
        // 256-color introducer is unhandled in M3 — it must not panic, and
        // it must not silently apply garbage to the style.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[123mX");
        assert_eq!(t.row(0).cells[0].style, Style::RESET);
    }

    #[test]
    fn unknown_csi_action_ignored() {
        // 'Z' (CBT) is unhandled — it must not panic and the cursor must
        // not move.
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[2Z");
        assert_eq!(t.cursor(), Cursor::default());
    }

    #[test]
    fn alt_screen_enter_and_exit_round_trip() {
        let mut t = Term::new(3, 4, 4);
        feed(&mut t, b"abcd\r\nefgh");
        let saved_before = t.cursor();
        feed(&mut t, b"\x1b[?1049h"); // enter alt
        assert!(t.is_alt_active());
        assert_eq!(t.cursor(), Cursor::default());
        // Alt grid is blank.
        for r in 0..3 {
            assert_eq!(row_text(&t, r), "");
        }
        feed(&mut t, b"XYZ");
        // Now exit — primary state should be intact, alt content gone.
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.is_alt_active());
        assert_eq!(t.cursor(), saved_before);
        assert_eq!(row_text(&t, 0), "abcd");
        assert_eq!(row_text(&t, 1).trim_end(), "efgh");
    }

    #[test]
    fn alt_screen_double_enter_is_idempotent() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"hi");
        let saved = t.cursor();
        feed(&mut t, b"\x1b[?1049h\x1b[?1049h");
        feed(&mut t, b"\x1b[?1049l\x1b[?1049l");
        assert_eq!(t.cursor(), saved);
        assert_eq!(row_text(&t, 0), "hi");
    }

    #[test]
    fn unknown_decset_param_is_a_noop() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?25hX");
        assert_eq!(t.row(0).cells[0].ch, 'X');
    }

    #[test]
    fn resize_clamps_cursor_and_changes_size() {
        let mut t = Term::new(3, 4, 4);
        feed(&mut t, b"abcd\r\nefgh\r\nijkl");
        // After "ijkl" the cursor sits at col=4 (pending wrap), row=2.
        assert_eq!(t.cursor().row, 2);
        t.resize(2, 3);
        assert_eq!(t.size(), (2, 3));
        let c = t.cursor();
        // Row must be inside the new viewport; col may equal cols
        // (pending-wrap sentinel) but must not exceed it.
        assert!(c.row < 2);
        assert!(c.col <= 3);
    }

    #[test]
    fn resize_to_larger_keeps_visible_content() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"ab\r\ncd");
        t.resize(4, 6);
        assert_eq!(t.size(), (4, 6));
        assert_eq!(row_text(&t, 0), "ab");
        assert_eq!(row_text(&t, 1).trim_end(), "cd");
    }

    #[test]
    fn resize_zero_dims_clamped_to_one() {
        let mut t = Term::new(2, 2, 0);
        t.resize(0, 0);
        assert_eq!(t.size(), (1, 1));
    }

    #[test]
    fn print_after_scroll_keeps_writing_on_last_row() {
        let mut t = Term::new(2, 4, 4);
        feed(&mut t, b"aaaa\r\nbbbb\r\ncccc");
        // After the second LF the cursor is on the last row; "cccc" lands
        // entirely on row 1; row 0 should now be "bbbb".
        assert_eq!(row_text(&t, 0), "bbbb");
        assert_eq!(row_text(&t, 1), "cccc");
    }

    #[test]
    fn print_advances_cursor_one_past_last_column_until_next_print() {
        let mut t = Term::new(2, 3, 0);
        feed(&mut t, b"abc");
        // After writing the final column, cursor sits one past the end
        // (pending-wrap behaviour). It must not immediately wrap.
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 3);
        feed(&mut t, b"d");
        assert!(t.row(0).soft_wrap);
        assert_eq!(t.row(1).cells[0].ch, 'd');
    }

    #[test]
    fn cursor_back_underflow_protected() {
        let mut t = Term::new(2, 4, 0);
        // CUB at col 0 must clamp, not panic.
        feed(&mut t, b"\x1b[10D");
        assert_eq!(t.cursor().col, 0);
        feed(&mut t, b"\x1b[10A");
        assert_eq!(t.cursor().row, 0);
    }

    #[test]
    fn apply_sgr_with_empty_params_resets_style() {
        // vte never produces empty params from a real CSI, but the
        // defensive branch in `apply_sgr` should still be exercised.
        let mut t = Term::new(1, 4, 0);
        t.cursor.style = Style {
            fg: Color::Red,
            bg: Color::Default,
            flags: StyleFlags {
                bold: true,
                ..StyleFlags::default()
            },
        };
        t.apply_sgr(&Params::default());
        assert_eq!(t.cursor.style, Style::RESET);
    }

    #[test]
    fn ansi_color_unknown_index_falls_back_to_default() {
        // The internal `ansi_color` helper has a defensive catch-all; we
        // exercise it here since callers normally bound the index 0..=7.
        assert_eq!(super::ansi_color(99, false), Color::Default);
        assert_eq!(super::ansi_color(99, true), Color::Default);
    }

    // ---- mode toggle tests (M7) ---------------------------------------------

    #[test]
    fn bracketed_paste_toggles_via_decset_2004() {
        let mut t = Term::new(2, 4, 0);
        assert!(!t.bracketed_paste());
        feed(&mut t, b"\x1b[?2004h");
        assert!(t.bracketed_paste());
        feed(&mut t, b"\x1b[?2004l");
        assert!(!t.bracketed_paste());
    }

    #[test]
    fn report_focus_toggles_via_decset_1004() {
        let mut t = Term::new(2, 4, 0);
        assert!(!t.report_focus());
        feed(&mut t, b"\x1b[?1004h");
        assert!(t.report_focus());
        feed(&mut t, b"\x1b[?1004l");
        assert!(!t.report_focus());
    }

    #[test]
    fn mouse_mode_1000_x10() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?1000h");
        assert_eq!(t.mouse_mode().protocol, MouseProtocol::X10);
        assert!(t.mouse_mode().is_on());
        assert!(!t.mouse_mode().report_drag());
        feed(&mut t, b"\x1b[?1000l");
        assert_eq!(t.mouse_mode().protocol, MouseProtocol::Off);
    }

    #[test]
    fn mouse_mode_1002_button_motion() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?1002h");
        assert_eq!(t.mouse_mode().protocol, MouseProtocol::ButtonMotion);
        assert!(t.mouse_mode().report_drag());
        assert!(!t.mouse_mode().report_any_motion());
    }

    #[test]
    fn mouse_mode_1003_any_motion() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?1003h");
        assert!(t.mouse_mode().report_any_motion());
        assert!(t.mouse_mode().report_drag());
    }

    #[test]
    fn mouse_mode_1006_sgr_encoding() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?1006h");
        assert!(t.mouse_mode().sgr_encoding);
        feed(&mut t, b"\x1b[?1006l");
        assert!(!t.mouse_mode().sgr_encoding);
    }

    #[test]
    fn mouse_mode_combined_1006_1002() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?1002h\x1b[?1006h");
        assert!(t.mouse_mode().sgr_encoding);
        assert_eq!(t.mouse_mode().protocol, MouseProtocol::ButtonMotion);
    }

    #[test]
    fn kitty_push_pop_round_trip() {
        let mut t = Term::new(2, 4, 0);
        assert_eq!(t.kitty_flags(), 0);
        feed(&mut t, b"\x1b[>1u");
        assert_eq!(t.kitty_flags(), 1);
        assert_eq!(t.kitty_stack(), &[1]);
        feed(&mut t, b"\x1b[>3u");
        assert_eq!(t.kitty_flags(), 3);
        assert_eq!(t.kitty_stack(), &[1, 3]);
        feed(&mut t, b"\x1b[<1u");
        assert_eq!(t.kitty_flags(), 1);
        feed(&mut t, b"\x1b[<u");
        // CSI < u with no param defaults to 1.
        assert_eq!(t.kitty_flags(), 0);
        assert!(t.kitty_stack().is_empty());
    }

    #[test]
    fn kitty_pop_more_than_stack_does_not_underflow() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[>5u");
        feed(&mut t, b"\x1b[<99u");
        assert_eq!(t.kitty_flags(), 0);
        assert!(t.kitty_stack().is_empty());
    }

    #[test]
    fn kitty_set_mode_1_replaces_top() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[>1u");
        feed(&mut t, b"\x1b[=15;1u");
        assert_eq!(t.kitty_flags(), 15);
    }

    #[test]
    fn kitty_set_mode_2_ors_bits() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[>1u");
        feed(&mut t, b"\x1b[=2;2u");
        assert_eq!(t.kitty_flags(), 3);
    }

    #[test]
    fn kitty_set_mode_3_clears_bits() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[>3u");
        feed(&mut t, b"\x1b[=2;3u");
        assert_eq!(t.kitty_flags(), 1);
    }

    #[test]
    fn kitty_set_with_empty_stack_pushes() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[=5;1u");
        assert_eq!(t.kitty_flags(), 5);
        assert_eq!(t.kitty_stack(), &[5]);
    }

    #[test]
    fn kitty_set_mode_3_on_empty_is_noop() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[=5;3u");
        assert!(t.kitty_stack().is_empty());
    }

    #[test]
    fn kitty_stack_caps_at_eight() {
        let mut t = Term::new(2, 4, 0);
        for n in 0..10u8 {
            t.kitty_push(n);
        }
        assert_eq!(t.kitty_stack().len(), 8);
        // Oldest values rotated out — top is the last value pushed.
        assert_eq!(t.kitty_flags(), 9);
    }

    #[test]
    fn cursor_only_moves_mark_dirty() {
        // Bare cursor moves (no cell write) must still flip the dirty
        // bits for both the row being left and the row being entered,
        // otherwise vim `hjkl` / `less ^B^F` leave a stale cursor block
        // behind. See the `move_cursor` helper in term.rs.
        let mut t = Term::new(10, 10, 0);
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);

        // CUP to (4, 4): both the old row (0) and the new row (4) must
        // be dirty.
        feed(&mut t, b"\x1b[5;5H");
        assert_eq!(t.cursor().row, 4);
        assert_eq!(t.cursor().col, 4);
        let dirty = dirty_rows(&t);
        assert!(dirty[0], "row 0 (cursor left) should be dirty");
        assert!(dirty[4], "row 4 (cursor entered) should be dirty");

        // Clear the bitset, then CUU 2 → (2, 4): rows 4 and 2 must be
        // dirty.
        t.clear_damage();
        feed(&mut t, b"\x1b[2A");
        assert_eq!(t.cursor().row, 2);
        assert_eq!(t.cursor().col, 4);
        let dirty = dirty_rows(&t);
        assert!(dirty[4], "row 4 (cursor left) should be dirty");
        assert!(dirty[2], "row 2 (cursor entered) should be dirty");
    }

    #[test]
    fn nth_param_uses_default_for_missing_or_zero() {
        // `CSI ;5 H` — row default (=1), col=5.
        let mut t = Term::new(5, 5, 0);
        feed(&mut t, b"\x1b[;5H");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 4);
    }

    // ----- Extended-color SGR (38/48 + 5/2) leak-fix tests ---------------
    //
    // These cover the bug where the old SGR walker re-interpreted the
    // sub-params of a truecolor / 256-color introducer as standalone SGR
    // codes — e.g. `CSI 38;2;200;32;100m` accidentally setting fg green
    // because 32 lands in the 30..=37 named-foreground range.

    #[test]
    fn sgr_256_fg_does_not_leak_into_bg_for_index_in_named_range() {
        // `42` is a 256-color index but ALSO the value of `bg=Green` in the
        // legacy SGR table. The fix must not re-apply `42` as a standalone
        // SGR after consuming it as the palette index.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;5;42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        // Critical: bg must still be Default. Under the broken parser bg
        // would be `Color::Green` because `42 - 40 = 2` triggers the 40..=47
        // arm.
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_truecolor_fg_does_not_leak_inner_byte_as_fg() {
        // `\x1b[38;2;200;32;100m` sets fg to RGB(200, 32, 100). The middle
        // byte (G=32) lies in 30..=37 and would set fg=Green under the
        // broken parser; the third byte (B=100) lies in 100..=107 and
        // would also set bg=BrightBlack. Neither must happen.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;2;200;32;100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
        // Sanity: even if a future bug made these flags persist, this
        // asserts no flag side effect.
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_256_bg_basic() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48;5;1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Indexed256(1));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_truecolor_bg_basic() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48;2;10;20;30mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Rgb(10, 20, 30));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_256_incomplete_is_ignored_cleanly() {
        // `\x1b[38;5m` is missing the index — the parser must consume the
        // `5` so it can't leak as a standalone SGR, and must NOT change
        // the fg.
        let mut t = Term::new(1, 4, 0);
        // Start with a known fg so we can detect an accidental change.
        feed(&mut t, b"\x1b[31m\x1b[38;5mX");
        let s = t.row(0).cells[0].style;
        // fg is whatever the introducer left it as — either kept Red (good)
        // or reset to Default. Under the broken parser, `5` would land in
        // `apply_sgr_param(5)` (a no-op currently) but a future bug could
        // map 5 to BlinkSlow. We assert the introducer was at least
        // consumed: fg should be Red (untouched), NOT something else.
        assert_eq!(s.fg, Color::Red);
        // And no flag side effects from consuming `5`.
        assert!(!s.flags.bold);
    }

    #[test]
    fn sgr_truecolor_incomplete_does_not_leak_components() {
        // Missing one component: `\x1b[38;2;200;32m` (only R and G). The
        // 32 must NOT leak as a standalone SGR setting fg=Green.
        let mut t = Term::new(1, 4, 0);
        // Establish a baseline: bold + red.
        feed(&mut t, b"\x1b[1;31m");
        feed(&mut t, b"\x1b[38;2;200;32mX");
        let s = t.row(0).cells[0].style;
        // Fg was Red; the truecolor sequence is incomplete — fg either
        // stays Red or becomes whatever the partial parse returned. In
        // our implementation it stays Red (Option::unwrap_or current fg).
        // The CRITICAL assertion: fg is NOT Green (which would be set by
        // `32` leaking through).
        assert_ne!(s.fg, Color::Green);
        // And bold must still be on (we didn't accidentally consume the 1).
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_mixed_named_then_extended_then_attr() {
        // Real-world style sequence: red fg, override with 256-color fg,
        // turn on bold. Final state: fg = Indexed256(42), bold = true,
        // bg = Default.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31;38;5;42;1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        assert_eq!(s.bg, Color::Default);
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_colon_form_256_color() {
        // ITU-T T.416 colon form. `vte 0.15` reports this as one slice
        // `[38, 5, 42]` rather than three slices `[[38],[5],[42]]`. The
        // parser must handle both.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:5:42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Indexed256(42));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_truecolor_short() {
        // Colon-form 4-arg truecolor: `[38, 2, R, G, B]`. The G byte (32)
        // must not leak.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:2:200:32:100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_truecolor_with_color_space_id() {
        // Canonical T.416 truecolor: `[38, 2, Pi, R, G, B]`. The Pi
        // color-space identifier (here `1` — sRGB) must be skipped.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38:2:1:200:32:100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Rgb(200, 32, 100));
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_unknown_extended_kind_is_consumed_without_leak() {
        // 38 followed by neither 5 nor 2: malformed. The introducer must
        // still consume the `9` so it doesn't reapply as a standalone
        // SGR. Currently 9 is unhandled but we test the principle.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;38;9;31mX");
        let s = t.row(0).cells[0].style;
        // Bold from before the introducer should stick.
        assert!(s.flags.bold);
        // The trailing `31` (red fg) must apply — the parser must have
        // bailed cleanly out of the malformed 38;9 sequence.
        assert_eq!(s.fg, Color::Red);
    }

    #[test]
    fn sgr_default_fg_only_clears_fg() {
        // `\x1b[39m` resets fg but leaves bg/flags alone.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[1;31;44mA\x1b[39mB");
        let s = t.row(0).cells[1].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Blue);
        assert!(s.flags.bold);
    }

    #[test]
    fn sgr_underline_color_is_consumed_and_ignored() {
        // We don't store underline color yet, but the introducer must
        // consume its sub-params so nothing leaks. Mode 58 with 256-color
        // index 42 should NOT end up setting bg=Green or anything else.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58;5;42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_underline_color_truecolor_is_consumed_and_ignored() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58;2;200;32;100mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
        assert_eq!(s.flags, StyleFlags::default());
    }

    #[test]
    fn sgr_underline_color_default_is_a_noop() {
        // Mode 59 = default underline color. Must not panic / leak.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[31;59mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Red);
    }

    #[test]
    fn sgr_helix_simulated_sidebar_does_not_leak_past_reset() {
        // Reproduces the helix sidebar leak shape: paint a few cells with
        // a 256-color fg, then reset, then paint plain text. The plain
        // text cells must come out with default fg — no green leak.
        let mut t = Term::new(1, 16, 0);
        feed(&mut t, b"\x1b[38;5;42m 1 \x1b[0m text");
        // Cells 0..3 (the painted " 1 ") have fg=Indexed256(42); cells
        // 3..8 (the " text" after reset) have fg=Default.
        for (i, want_fg) in [
            (0, Color::Indexed256(42)),
            (1, Color::Indexed256(42)),
            (2, Color::Indexed256(42)),
            (3, Color::Default),
            (4, Color::Default),
            (5, Color::Default),
            (6, Color::Default),
            (7, Color::Default),
        ] {
            assert_eq!(
                t.row(0).cells[i].style.fg,
                want_fg,
                "cell {i} fg differs (helix leak regression)",
            );
            // bg must always be Default in this scenario.
            assert_eq!(
                t.row(0).cells[i].style.bg,
                Color::Default,
                "cell {i} bg accidentally non-default",
            );
        }
    }

    #[test]
    fn sgr_truecolor_then_named_fg_overrides_correctly() {
        // After truecolor, a plain named SGR must still work (no stale
        // state in the iterator).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[38;2;1;2;3m\x1b[33mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Yellow);
    }

    #[test]
    fn parse_extended_color_from_slice_rejects_unknown_kind() {
        // Direct unit-test of the slice helper — input `[9, ...]` is
        // neither 5 nor 2 so must return None.
        assert!(super::parse_extended_color_from_slice(&[9, 1, 2, 3]).is_none());
        // Empty slice → None.
        assert!(super::parse_extended_color_from_slice(&[]).is_none());
        // 5 with no index → None.
        assert!(super::parse_extended_color_from_slice(&[5]).is_none());
        // 2 with fewer than 3 RGB components → None.
        assert!(super::parse_extended_color_from_slice(&[2, 1, 2]).is_none());
    }

    #[test]
    fn parse_extended_color_from_iter_returns_none_for_unknown_kind() {
        // Direct unit-test of the iterator helper.
        let slices: Vec<&[u16]> = vec![&[9], &[1], &[2]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
    }

    #[test]
    fn clamp_u8_saturates_values_above_255() {
        assert_eq!(super::clamp_u8(0), 0);
        assert_eq!(super::clamp_u8(255), 255);
        assert_eq!(super::clamp_u8(256), 255);
        assert_eq!(super::clamp_u8(u16::MAX), 255);
    }

    #[test]
    fn sgr_colon_form_bg_256_color() {
        // Cover the colon-form `48` branch (top-level `bg = ...`).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[48:5:1mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Indexed256(1));
        assert_eq!(s.fg, Color::Default);
    }

    #[test]
    fn sgr_colon_form_underline_color_consumed_and_ignored() {
        // Cover the colon-form `58` branch (top-level discard).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[58:5:42mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.fg, Color::Default);
        assert_eq!(s.bg, Color::Default);
    }

    #[test]
    fn sgr_semicolon_form_bg_incomplete_leaves_bg_alone() {
        // Cover the `None` branch of the bg `if let Some(c)` in the
        // semicolon-form 48 handler. We pre-set a known bg (Blue), then
        // feed a malformed `\x1b[48;9m` (unknown kind 9). The bg must
        // stay Blue — the partial parse must NOT leak any of those
        // codes back into the SGR walker.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[44m\x1b[48;9mX");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.bg, Color::Blue);
    }

    #[test]
    fn parse_extended_color_from_slice_truecolor_with_color_space_id_form() {
        // Cover the rest.len() >= 5 branch directly.
        let c = super::parse_extended_color_from_slice(&[2, 1, 100, 150, 200]);
        assert_eq!(c, Some(Color::Rgb(100, 150, 200)));
    }

    #[test]
    fn parse_extended_color_from_slice_truecolor_short_form() {
        // Cover the 4-arg shortcut.
        let c = super::parse_extended_color_from_slice(&[2, 100, 150, 200]);
        assert_eq!(c, Some(Color::Rgb(100, 150, 200)));
    }

    // ----- OSC title + DECSCUSR cursor shape (M6) -------------------------

    #[test]
    fn osc_2_sets_window_title() {
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]2;hello world\x1b\\");
        assert_eq!(t.title(), "hello world");
    }

    #[test]
    fn osc_0_sets_window_title() {
        // OSC 0 also covers the window title (and the icon title, which
        // we don't model).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]0;banner\x1b\\");
        assert_eq!(t.title(), "banner");
    }

    #[test]
    fn osc_0_bel_terminated_also_works() {
        // BEL-terminated OSC is the older variant still in active use by
        // most shells (bash PROMPT_COMMAND, zsh print -P).
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]0;via-bel\x07");
        assert_eq!(t.title(), "via-bel");
    }

    #[test]
    fn osc_1_icon_title_does_not_change_window_title() {
        // OSC 1 is icon-only. We don't have a tray icon, so it must not
        // overwrite the window title set by OSC 0 or 2.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]2;window-title\x1b\\");
        feed(&mut t, b"\x1b]1;icon-title\x1b\\");
        assert_eq!(t.title(), "window-title");
    }

    #[test]
    fn unknown_osc_is_silently_ignored() {
        // OSC 99999 — not implemented. Must not panic; must not affect title.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]2;keep-me\x1b\\");
        feed(&mut t, b"\x1b]99999;noise\x1b\\");
        assert_eq!(t.title(), "keep-me");
    }

    #[test]
    fn osc_without_payload_is_safe() {
        // `OSC 2 ST` with no semicolon / payload at all — must not panic.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b]2\x1b\\");
        // Title left empty (or unchanged). Either is acceptable; we
        // just need to not crash.
        assert_eq!(t.title(), "");
    }

    #[test]
    fn osc_with_non_utf8_title_is_lossy_decoded() {
        // Title payload with invalid UTF-8: we lossy-decode rather than
        // dropping the title. Each invalid byte becomes U+FFFD.
        let mut t = Term::new(1, 4, 0);
        // 0xff is invalid UTF-8.
        feed(&mut t, b"\x1b]2;A\xffB\x1b\\");
        // Don't assert exact content — just that we didn't crash and the
        // title is non-empty (since the bytes lossy-decode to something).
        assert!(!t.title().is_empty());
    }

    #[test]
    fn osc_does_not_mark_rows_dirty() {
        // Title-setting must not pollute the dirty bitset — the title
        // lives outside the grid.
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        feed(&mut t, b"\x1b]2;quiet\x1b\\");
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(!d, "row {i} should not be dirty after OSC title set");
        }
    }

    #[test]
    fn decscusr_default_is_block_blinking() {
        // Brand-new Term should match CursorConfig::defaults().
        let t = Term::new(1, 4, 0);
        assert_eq!(t.cursor_shape(), CursorShape::Block);
        assert!(t.cursor_blink());
    }

    #[test]
    fn decscusr_full_table() {
        // (Ps, expected_shape, expected_blink)
        let cases: &[(u16, CursorShape, bool)] = &[
            (0, CursorShape::Block, true),
            (1, CursorShape::Block, true),
            (2, CursorShape::Block, false),
            (3, CursorShape::Underline, true),
            (4, CursorShape::Underline, false),
            (5, CursorShape::Bar, true),
            (6, CursorShape::Bar, false),
        ];
        for &(ps, want_shape, want_blink) in cases {
            let mut t = Term::new(1, 4, 0);
            let seq = format!("\x1b[{ps} q");
            feed(&mut t, seq.as_bytes());
            assert_eq!(
                t.cursor_shape(),
                want_shape,
                "Ps={ps}: wrong cursor shape",
            );
            assert_eq!(t.cursor_blink(), want_blink, "Ps={ps}: wrong blink");
        }
    }

    #[test]
    fn decscusr_unknown_ps_is_ignored() {
        // Unknown Ps (here 7+) must not panic or change anything. We
        // verify by setting a known state, sending the bad code, and
        // checking the state is unchanged.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, b"\x1b[5 q"); // Bar, blinking
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        assert!(t.cursor_blink());
        feed(&mut t, b"\x1b[42 q"); // unknown
        assert_eq!(t.cursor_shape(), CursorShape::Bar);
        assert!(t.cursor_blink());
    }

    #[test]
    fn decscusr_without_space_intermediate_is_not_handled_here() {
        // `CSI N q` (no space) is *not* DECSCUSR — it's a different
        // sequence we don't currently support. Must not change the
        // cursor shape. (vte routes the space intermediate as
        // `intermediates = b" "`.)
        let mut t = Term::new(1, 4, 0);
        let initial = (t.cursor_shape(), t.cursor_blink());
        feed(&mut t, b"\x1b[5q"); // no space — different action
        assert_eq!((t.cursor_shape(), t.cursor_blink()), initial);
    }

    #[test]
    fn set_cursor_default_overrides_init_values() {
        // The binary calls this once during init() to thread the
        // `[cursor]` config table through to the runtime state.
        let mut t = Term::new(1, 4, 0);
        t.set_cursor_default(CursorShape::Underline, false);
        assert_eq!(t.cursor_shape(), CursorShape::Underline);
        assert!(!t.cursor_blink());
    }

    // ---- M8 mode toggle tests --------------------------------------------

    #[test]
    fn decset_2026_pauses_rendering() {
        let mut t = Term::new(2, 4, 0);
        assert!(!t.pause_rendering());
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.pause_rendering());
        assert!(t.sync_output_started_at().is_some());
    }

    #[test]
    fn decset_2026_disable_clears_pause_and_marks_dirty() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026h");
        // Clear dirty so we can observe the disable-side effect.
        t.clear_damage();
        feed(&mut t, b"\x1b[?2026l");
        assert!(!t.pause_rendering());
        assert!(t.sync_output_started_at().is_none());
        // Every visible row should now be dirty so the post-ESU frame
        // does a full redraw.
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} should be dirty after ESU");
        }
    }

    #[test]
    fn decset_2026_reentrant_bsu_does_not_restart_timer() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026h");
        let first = t.sync_output_started_at().expect("first BSU recorded");
        // Sleep is allergy-inducing in tests; we just trust the
        // wall-clock monotonicity here and re-enable. The flag must
        // NOT bump `started_at` forward.
        feed(&mut t, b"\x1b[?2026h");
        let second = t.sync_output_started_at().expect("still active");
        assert_eq!(first, second, "reentrant BSU must not restart timer");
    }

    #[test]
    fn decset_2027_toggles_grapheme_cluster_mode() {
        let mut t = Term::new(2, 4, 0);
        assert!(!t.grapheme_cluster_mode());
        feed(&mut t, b"\x1b[?2027h");
        assert!(t.grapheme_cluster_mode());
        feed(&mut t, b"\x1b[?2027l");
        assert!(!t.grapheme_cluster_mode());
    }

    #[test]
    fn decset_2027_is_independent_of_other_modes() {
        // Toggling 2027 must not affect 2026 or 2048 (no cross-mode
        // bleed). Catches accidental field-confusion regressions.
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026h");
        feed(&mut t, b"\x1b[?2048h");
        feed(&mut t, b"\x1b[?2027h");
        assert!(t.grapheme_cluster_mode());
        assert!(t.pause_rendering());
        assert!(t.inband_resize_mode());
        feed(&mut t, b"\x1b[?2027l");
        assert!(!t.grapheme_cluster_mode());
        // Other modes unaffected.
        assert!(t.pause_rendering());
        assert!(t.inband_resize_mode());
    }

    #[test]
    fn decset_2048_toggles_inband_resize_mode() {
        let mut t = Term::new(2, 4, 0);
        assert!(!t.inband_resize_mode());
        feed(&mut t, b"\x1b[?2048h");
        assert!(t.inband_resize_mode());
        feed(&mut t, b"\x1b[?2048l");
        assert!(!t.inband_resize_mode());
    }

    #[test]
    fn decset_2048_survives_resize() {
        // Mode 2048 is meant for apps that want resize reports on
        // *future* geometry changes — it must survive an actual
        // resize() call (which mostly rebuilds grid state).
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2048h");
        t.resize(4, 8);
        assert!(t.inband_resize_mode());
    }

    #[test]
    fn force_flush_sync_output_sets_timeout_flag_and_dirties_all() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026h");
        assert!(t.pause_rendering());
        t.clear_damage();
        t.force_flush_sync_output();
        assert!(!t.pause_rendering());
        assert!(t.sync_output_force_flushed());
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} should be dirty after timeout flush");
        }
    }

    #[test]
    fn force_flush_sync_output_is_idempotent_when_no_bsu() {
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        // No BSU in flight — flush is a no-op.
        t.force_flush_sync_output();
        assert!(!t.sync_output_force_flushed());
        for d in dirty_rows(&t) {
            assert!(!d, "no row should be dirty when there was no BSU to flush");
        }
    }

    #[test]
    fn sync_output_force_flushed_is_consumed_by_clear() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026h");
        t.force_flush_sync_output();
        assert!(t.sync_output_force_flushed());
        t.clear_sync_output_force_flushed();
        assert!(!t.sync_output_force_flushed());
    }

    #[test]
    fn print_wide_cluster_writes_continuation_cell() {
        // Print a CJK ideograph at column 0. The primary cell holds
        // the character; the next cell is a continuation marker.
        // Cursor advances by 2 columns.
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, "你".as_bytes());
        let row = t.row(0);
        assert_eq!(row.cells[0].ch, '你');
        assert!(!row.cells[0].is_continuation);
        assert!(row.cells[1].is_continuation);
        assert_eq!(row.cells[1].ch, '\0');
        // Continuation inherits the style of the cluster's primary.
        assert_eq!(row.cells[1].style, row.cells[0].style);
        assert_eq!(t.cursor().col, 2);
    }

    #[test]
    fn print_wide_cluster_wraps_when_only_one_column_left() {
        // Grid is 3 cols wide. Print "ab" → cursor at col 2. Next
        // print of "你" should wrap to the next row instead of
        // splitting the cluster.
        let mut t = Term::new(3, 3, 0);
        feed(&mut t, b"ab");
        assert_eq!(t.cursor().col, 2);
        feed(&mut t, "你".as_bytes());
        // Wrap took effect: the wide cluster lands on row 1.
        assert_eq!(t.row(1).cells[0].ch, '你');
        assert!(t.row(1).cells[1].is_continuation);
        // Row 0 marked soft-wrap.
        assert!(t.row(0).soft_wrap);
    }

    #[test]
    fn print_two_wide_clusters_fill_grid() {
        // 4-column row, two CJK ideographs.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, "你好".as_bytes());
        let row = t.row(0);
        assert_eq!(row.cells[0].ch, '你');
        assert!(row.cells[1].is_continuation);
        assert_eq!(row.cells[2].ch, '好');
        assert!(row.cells[3].is_continuation);
        // Cursor sits one past the last column (pending-wrap).
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn backspace_skips_continuation_cell() {
        // After printing one wide cluster, cursor is at col 2.
        // BS should land on col 1? No — the continuation cell should
        // be skipped, so cursor lands on col 0 (the cluster's
        // primary). Two BSes after a single wide cluster shouldn't
        // strand the cursor on a continuation half.
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "你".as_bytes());
        assert_eq!(t.cursor().col, 2);
        feed(&mut t, b"\x08");
        // After one BS, cursor steps off the continuation: lands at
        // col 0 (the cluster's primary).
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn cursor_back_skips_continuation_cell() {
        // CUB n by 1 from col 2 should land on col 0 (jumping over
        // the continuation cell at col 1).
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "你".as_bytes());
        assert_eq!(t.cursor().col, 2);
        feed(&mut t, b"\x1b[1D");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn sync_output_full_flow_bsu_then_esu_in_one_batch() {
        // App emits BSU, writes a row of cells, then ESU all in one
        // PTY batch. After the batch:
        // - pause_rendering is false (ESU lowered it)
        // - every row is dirty (post-ESU full redraw)
        // - the cell content is what the app wrote
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        feed(&mut t, b"\x1b[?2026hAB\x1b[?2026l");
        assert!(!t.pause_rendering());
        assert_eq!(row_text(&t, 0), "AB");
        // All rows dirty (the ESU disable path marks everything).
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} should be dirty after batch ESU");
        }
    }

    /// Followup C2: when `render_term` returns `RenderOutcome::Skipped`
    /// (pause-gated), the binary's `Event::Redraw` branch must NOT clear
    /// the dirty bitset or the BSU force-flushed flag — both must
    /// survive until the next non-skipped frame so the corrective full
    /// redraw is delivered.
    ///
    /// This test simulates the binary's "skipped frame" branch by
    /// mutating Term state in the exact sequence the binary will use:
    /// force-flush sets the flag and dirties everything; then a
    /// hypothetical "skipped" frame does NOT call `clear_dirty` or
    /// `clear_sync_output_force_flushed`; the flag and dirty rows must
    /// still be set on the next observation.
    #[test]
    fn sync_output_force_flushed_flag_survives_skipped_frame() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026hAB");
        t.force_flush_sync_output();
        // Precondition: every row dirty, flag set, pause cleared.
        assert!(!t.pause_rendering());
        assert!(t.sync_output_force_flushed());
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} should be dirty after timeout flush");
        }
        // A pause-gated Skipped frame: the binary does NOT call
        // clear_dirty / clear_sync_output_force_flushed. State must be
        // identical when we observe it again.
        assert!(t.sync_output_force_flushed());
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} dirty bit must survive a skipped frame");
        }
        // Now simulate a Rendered frame that consumes the signals.
        t.clear_damage();
        t.clear_sync_output_force_flushed();
        assert!(!t.sync_output_force_flushed());
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(!d, "row {i} should be clean after a real render");
        }
    }

    #[test]
    fn sync_output_force_flush_after_bsu_only_renders_partial_state() {
        // App emits BSU + content, never sends ESU. Watchdog calls
        // force_flush_sync_output. After the flush, the partial cell
        // content is visible AND every row is dirty so the renderer
        // emits a corrective full redraw (decision #7 subtlety).
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[?2026hAB");
        // Pre-condition: paused, content written.
        assert!(t.pause_rendering());
        assert_eq!(row_text(&t, 0), "AB");
        t.clear_damage();
        t.force_flush_sync_output();
        assert!(!t.pause_rendering());
        assert!(t.sync_output_force_flushed());
        for (i, &d) in dirty_rows(&t).iter().enumerate() {
            assert!(d, "row {i} should be dirty after timeout flush");
        }
        // Content is still there.
        assert_eq!(row_text(&t, 0), "AB");
    }

    #[test]
    fn parse_extended_color_from_iter_truecolor_runs_out_of_components() {
        // Cover the `?` branches inside the iter helper. Each missing
        // component path returns None.
        // Missing G.
        let slices: Vec<&[u16]> = vec![&[2], &[100]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing B.
        let slices: Vec<&[u16]> = vec![&[2], &[100], &[150]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing R.
        let slices: Vec<&[u16]> = vec![&[2]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Missing 256 index.
        let slices: Vec<&[u16]> = vec![&[5]];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
        // Empty iterator -> None on first read.
        let slices: Vec<&[u16]> = vec![];
        let mut it = slices.into_iter();
        assert!(super::parse_extended_color_from_iter(&mut it).is_none());
    }

    // ---- M9 damage-tracking tests --------------------------------------

    /// Convenience: are cells `(r, c)` marked dirty in the damage set?
    fn damage_has_cell(t: &Term, r: u16, c: u16) -> bool {
        let Some(row) = t.damage().rows.get(r as usize) else {
            return false;
        };
        row.all_cols || row.cols.binary_search(&c).is_ok()
    }

    #[test]
    fn damage_print_marks_only_written_cell() {
        let mut t = Term::new(2, 8, 0);
        t.clear_damage();
        feed(&mut t, b"A");
        // Only (0, 0) should be dirty.
        assert!(damage_has_cell(&t, 0, 0));
        // Cells past the write shouldn't be in the dirty set.
        let row = &t.damage().rows[0];
        assert!(!row.all_cols);
        // The marked column list should contain only column 0.
        assert_eq!(&row.cols[..], &[0]);
    }

    #[test]
    fn damage_cup_marks_old_and_new_cell() {
        let mut t = Term::new(5, 5, 0);
        feed(&mut t, b"abc"); // cursor at (0, 3)
        t.clear_damage();
        feed(&mut t, b"\x1b[3;3H"); // CUP to (2, 2) — cursor was at (0, 3)
        // old (0, 3): mark old cell. new (2, 2): mark new cell.
        assert!(damage_has_cell(&t, 0, 3), "old cursor cell must be dirty");
        assert!(damage_has_cell(&t, 2, 2), "new cursor cell must be dirty");
        // No other cells should be dirty.
        assert_eq!(&t.damage().rows[0].cols[..], &[3]);
        assert_eq!(&t.damage().rows[2].cols[..], &[2]);
        assert!(t.damage().rows[1].is_empty());
    }

    #[test]
    fn damage_erase_line_marks_range_only() {
        // EL mode 0 (cursor to end of line) — marks [cur_col, cols).
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, b"abcdef"); // cursor at (0, 6)
        t.clear_damage();
        feed(&mut t, b"\x1b[K"); // EL 0
        let row = &t.damage().rows[0];
        // Range marked: [6, 8) — two cells.
        assert!(!row.all_cols);
        assert_eq!(&row.cols[..], &[6, 7]);
        // Row 1 stayed clean.
        assert!(t.damage().rows[1].is_empty());
    }

    #[test]
    fn damage_erase_line_mode2_marks_all_cells_in_row() {
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        feed(&mut t, b"\x1b[2K"); // EL 2 — entire line
        // The full-row range collapses to all_cols.
        let row = &t.damage().rows[0];
        assert!(row.all_cols);
    }

    #[test]
    fn damage_mark_all_dirty_sets_all_flag() {
        let mut t = Term::new(3, 4, 0);
        t.clear_damage();
        t.mark_all_dirty();
        assert!(t.damage().all);
        for r in &t.damage().rows {
            assert!(r.all_cols);
        }
    }

    #[test]
    fn damage_resize_sets_all_flag() {
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        t.resize(5, 8);
        assert!(t.damage().all);
        assert_eq!(t.damage().rows.len(), 5);
    }

    #[test]
    fn damage_clear_resets() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"A");
        assert!(!t.damage().is_empty());
        t.clear_damage();
        assert!(t.damage().is_empty());
    }

    #[test]
    fn damage_bs_marks_old_and_new_cell() {
        let mut t = Term::new(2, 8, 0);
        feed(&mut t, b"abc"); // cursor at (0, 3)
        t.clear_damage();
        feed(&mut t, b"\x08"); // BS — cursor moves to (0, 2)
        assert_eq!(t.cursor().col, 2);
        // Old col (3) and new col (2) must be dirty.
        assert!(damage_has_cell(&t, 0, 3), "old cursor col 3 dirty");
        assert!(damage_has_cell(&t, 0, 2), "new cursor col 2 dirty");
    }

    #[test]
    fn damage_ht_marks_old_and_new_cell() {
        let mut t = Term::new(2, 32, 0);
        // Cursor at col 0, HT → col 8.
        t.clear_damage();
        feed(&mut t, b"\t");
        assert_eq!(t.cursor().col, 8);
        assert!(damage_has_cell(&t, 0, 0), "old col 0 dirty");
        assert!(damage_has_cell(&t, 0, 8), "new col 8 dirty");
    }

    #[test]
    fn damage_wide_cluster_marks_continuation_cell() {
        let mut t = Term::new(2, 8, 0);
        t.clear_damage();
        feed(&mut t, "你".as_bytes()); // wide cluster at col 0
        // Both the primary (0) and continuation (1) cells are marked.
        assert!(damage_has_cell(&t, 0, 0), "primary cell dirty");
        assert!(damage_has_cell(&t, 0, 1), "continuation cell dirty");
    }

    #[test]
    fn damage_erase_display_mode0_marks_partial_row_and_full_subsequent_rows() {
        let mut t = Term::new(4, 8, 0);
        feed(&mut t, b"\x1b[2;3Hab"); // cursor at (1, 4)
        t.clear_damage();
        feed(&mut t, b"\x1b[J"); // ED 0
        // Row 1: range [4, 8) marked.
        let row1 = &t.damage().rows[1];
        assert!(!row1.all_cols);
        assert_eq!(&row1.cols[..], &[4, 5, 6, 7]);
        // Rows 2, 3: full row marked.
        assert!(t.damage().rows[2].all_cols);
        assert!(t.damage().rows[3].all_cols);
        // Row 0: not touched (clear_damage was called).
        assert!(t.damage().rows[0].is_empty());
    }

    /// Followup C3: a bare LF without scroll (mid-screen) moves the
    /// cursor down by a row without touching any cell content. The
    /// dirty-instance builder would see an empty damage set and the
    /// renderer would skip the frame, leaving the previous cursor
    /// block painted on the old row. `linefeed` must mark both the
    /// old and new cursor cell so partial redraw overpaints the old
    /// block and emits the new one.
    #[test]
    fn damage_lf_marks_old_and_new_cell() {
        let mut t = Term::new(3, 8, 0);
        // Cursor at row 0, col 0 by default.
        t.clear_damage();
        feed(&mut t, b"\n");
        // Old cell (0, 0) and new cell (1, 0) must both be dirty.
        assert!(damage_has_cell(&t, 0, 0), "old cursor cell must be dirty");
        assert!(damage_has_cell(&t, 1, 0), "new cursor cell must be dirty");
        // No other cells should be dirty.
        assert_eq!(&t.damage().rows[0].cols[..], &[0]);
        assert_eq!(&t.damage().rows[1].cols[..], &[0]);
        assert!(t.damage().rows[2].is_empty());
    }

    /// Followup C1: `Term::mark_cell_dirty` is the public hook the
    /// renderer uses to mark the cursor's cell dirty when the blink
    /// flips visible→invisible (so the dirty-instance builder emits a
    /// fresh bg quad that overpaints the previous cursor block).
    #[test]
    fn mark_cell_dirty_marks_single_cell() {
        let mut t = Term::new(3, 8, 0);
        t.clear_damage();
        t.mark_cell_dirty(1, 4);
        let row1 = &t.damage().rows[1];
        assert!(!row1.all_cols);
        assert_eq!(&row1.cols[..], &[4]);
        // Other rows untouched.
        assert!(t.damage().rows[0].is_empty());
        assert!(t.damage().rows[2].is_empty());
    }

    /// Followup C1: when the targeted cell is a width-2 continuation
    /// (second half of a wide-char cluster), `mark_cell_dirty` must
    /// also mark `col - 1` so the primary cell's multi-cell glyph gets
    /// re-emitted under partial redraw (the dirty-instance builder
    /// skips continuation cells).
    #[test]
    fn mark_cell_dirty_marks_continuation_partner() {
        // Wide char (CJK) at col 0; continuation at col 1.
        let mut t = Term::new(1, 4, 0);
        feed(&mut t, "你".as_bytes());
        t.clear_damage();
        // Asking for the continuation cell at (0, 1) must also dirty
        // the primary at (0, 0).
        t.mark_cell_dirty(0, 1);
        let row0 = &t.damage().rows[0];
        assert!(!row0.all_cols);
        assert_eq!(&row0.cols[..], &[0, 1]);
    }

    /// Followup C1: out-of-range row is a no-op (mirrors the private
    /// `mark_cell` helper, which guards `damage.rows.get_mut`).
    #[test]
    fn mark_cell_dirty_out_of_range_row_is_noop() {
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        t.mark_cell_dirty(99, 0);
        assert!(t.damage().is_empty());
    }

    // ----- OSC 7 (cwd) ------------------------------------------------------

    #[test]
    fn osc7_sets_cwd_from_file_url() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]7;file://host/home/user\x1b\\");
        assert_eq!(t.cwd(), "/home/user");
    }

    #[test]
    fn osc7_handles_hostless_form() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]7;file:///srv/data\x1b\\");
        assert_eq!(t.cwd(), "/srv/data");
    }

    #[test]
    fn osc7_percent_decodes_spaces() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]7;file:///tmp/space%20dir\x1b\\");
        assert_eq!(t.cwd(), "/tmp/space dir");
    }

    #[test]
    fn osc7_non_file_scheme_is_ignored() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]7;http://example.com/\x1b\\");
        assert_eq!(t.cwd(), "");
    }

    #[test]
    fn osc7_bel_terminator_works_too() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]7;file:///home\x07");
        assert_eq!(t.cwd(), "/home");
    }

    // ----- OSC 133 (semantic prompts) --------------------------------------

    #[test]
    fn osc133_a_records_prompt_start() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\x1b]133;A\x1b\\");
        let marks = t.prompt_marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].kind, PromptMarkKind::PromptStart);
        assert_eq!(marks[0].row, 0);
    }

    #[test]
    fn osc133_b_records_prompt_end_at_current_row() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\r\n\x1b]133;B\x1b\\");
        let marks = t.prompt_marks();
        assert_eq!(marks.len(), 1);
        assert_eq!(marks[0].kind, PromptMarkKind::PromptEnd);
        assert_eq!(marks[0].row, 1);
    }

    #[test]
    fn osc133_c_records_command_start() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\x1b]133;C\x1b\\");
        assert_eq!(t.prompt_marks()[0].kind, PromptMarkKind::CommandStart);
    }

    #[test]
    fn osc133_d_with_exit_code() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\x1b]133;D;0\x1b\\");
        assert_eq!(
            t.prompt_marks()[0].kind,
            PromptMarkKind::CommandFinished(Some(0))
        );
        feed(&mut t, b"\x1b]133;D;127\x1b\\");
        assert_eq!(
            t.prompt_marks()[1].kind,
            PromptMarkKind::CommandFinished(Some(127))
        );
    }

    #[test]
    fn osc133_d_without_exit_code() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\x1b]133;D\x1b\\");
        assert_eq!(
            t.prompt_marks()[0].kind,
            PromptMarkKind::CommandFinished(None)
        );
    }

    #[test]
    fn osc133_unknown_kind_ignored() {
        let mut t = Term::new(3, 8, 0);
        feed(&mut t, b"\x1b]133;Z\x1b\\");
        assert!(t.prompt_marks().is_empty());
    }

    #[test]
    fn osc133_marks_cap_at_4096_with_fifo_eviction() {
        let mut t = Term::new(3, 8, 0);
        for _ in 0..4100 {
            feed(&mut t, b"\x1b]133;A\x1b\\");
        }
        assert_eq!(t.prompt_marks().len(), 4096);
    }

    /// M10-followup I3: push 5000 marks (well past the 4096 cap) and
    /// confirm the cap is honored without quadratic blowup. We don't
    /// measure wall time — the test just exercises the eviction path
    /// at scale, which on the old `Vec::remove(0)` implementation was
    /// O(n) per eviction; on the new `VecDeque::pop_front` path it's
    /// O(1). If the runtime regresses to quadratic, this test will
    /// hang in CI long before it asserts.
    #[test]
    fn osc133_marks_fifo_eviction_is_not_quadratic() {
        let mut t = Term::new(3, 8, 0);
        for _ in 0..5000 {
            feed(&mut t, b"\x1b]133;A\x1b\\");
        }
        // Cap is enforced.
        assert_eq!(t.prompt_marks().len(), PROMPT_MARK_CAP);
        // FIFO semantics: the oldest entries dropped off the front;
        // everything we see is still PromptStart (we only ever pushed
        // PromptStart).
        for m in t.prompt_marks() {
            assert_eq!(m.kind, PromptMarkKind::PromptStart);
        }
    }

    // ----- PTY reply queue --------------------------------------------------

    #[test]
    fn pty_reply_queue_drains_in_order() {
        let mut t = Term::new(2, 4, 0);
        t.push_pty_reply(b"hello");
        t.push_pty_reply(b" world");
        let drained = t.drain_pty_replies();
        assert_eq!(drained, b"hello world");
        // Second drain returns empty — drain consumes.
        assert!(t.drain_pty_replies().is_empty());
    }

    #[test]
    fn pty_reply_queue_starts_empty() {
        let mut t = Term::new(2, 4, 0);
        assert!(t.drain_pty_replies().is_empty());
    }

    // ----- OSC 4 (palette) -------------------------------------------------

    #[test]
    fn da1_replies_with_vt220_plus_color() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[c");
        let bytes = t.drain_pty_replies();
        assert_eq!(&bytes[..], b"\x1b[?62;22c");
    }

    #[test]
    fn da1_with_explicit_zero_param_also_replies() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[0c");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?62;22c");
    }

    #[test]
    fn da2_replies_with_type_version_cartridge() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[>c");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[>0;0;0c");
    }

    #[test]
    fn dsr_status_report_replies_ok() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[5n");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[0n");
    }

    #[test]
    fn dsr_cursor_position_replies_1_based() {
        let mut t = Term::new(5, 10, 0);
        // Move cursor to row 3, col 4 (1-based via CUP).
        feed(&mut t, b"\x1b[3;4H");
        feed(&mut t, b"\x1b[6n");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[3;4R");
    }

    #[test]
    fn decxcpr_replies_with_question_mark_prefix() {
        let mut t = Term::new(5, 10, 0);
        feed(&mut t, b"\x1b[2;5H");
        feed(&mut t, b"\x1b[?6n");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?2;5R");
    }

    #[test]
    fn osc4_set_records_override_and_bumps_revision() {
        let mut t = Term::new(2, 4, 0);
        let r0 = t.palette_revision();
        feed(&mut t, b"\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        assert_eq!(t.palette_override(1), Some([0xab, 0xcd, 0xef]));
        assert_ne!(t.palette_revision(), r0);
    }

    #[test]
    fn osc4_set_marks_all_dirty() {
        let mut t = Term::new(2, 4, 0);
        t.clear_damage();
        feed(&mut t, b"\x1b]4;5;rgb:00/00/00\x1b\\");
        assert!(t.damage().all, "palette change must invalidate every row");
    }

    #[test]
    fn osc4_query_enqueues_reply() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]4;1;?\x1b\\");
        let bytes = t.drain_pty_replies();
        let s = std::str::from_utf8(&bytes).unwrap();
        // Default xterm-256 red is 0x800000 → "8080/0000/0000".
        assert_eq!(s, "\x1b]4;1;rgb:8080/0000/0000\x1b\\");
    }

    #[test]
    fn osc4_query_returns_override_when_set() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]4;1;rgb:ab/cd/ef\x1b\\");
        let _ = t.drain_pty_replies();
        feed(&mut t, b"\x1b]4;1;?\x1b\\");
        let bytes = t.drain_pty_replies();
        let s = std::str::from_utf8(&bytes).unwrap();
        assert_eq!(s, "\x1b]4;1;rgb:abab/cdcd/efef\x1b\\");
    }

    #[test]
    fn osc4_multi_pair_handled() {
        let mut t = Term::new(2, 4, 0);
        feed(
            &mut t,
            b"\x1b]4;1;rgb:11/22/33;2;rgb:44/55/66\x1b\\",
        );
        assert_eq!(t.palette_override(1), Some([0x11, 0x22, 0x33]));
        assert_eq!(t.palette_override(2), Some([0x44, 0x55, 0x66]));
    }

    #[test]
    fn osc4_malformed_pair_does_not_panic() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]4;notanumber;rgb:ab/cd/ef\x1b\\");
        // No override applied, no panic.
        assert!(t.palette_overrides.iter().all(Option::is_none));
    }

    // ----- OSC 8 (hyperlinks) ----------------------------------------------

    #[test]
    fn osc8_stamps_hyperlink_id_on_printed_cells() {
        let mut t = Term::new(2, 16, 0);
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\link");
        // First 4 cells must share the same non-None hyperlink id.
        let id0 = t.row(0).cells[0].hyperlink_id;
        assert!(id0.is_some());
        for c in 0..4 {
            assert_eq!(t.row(0).cells[c].hyperlink_id, id0);
        }
        // Resolves back to the URL.
        let url = t.hyperlink_url(id0.unwrap()).unwrap();
        assert_eq!(url, "https://example.com");
    }

    #[test]
    fn osc8_closer_clears_subsequent_cells() {
        let mut t = Term::new(2, 16, 0);
        feed(
            &mut t,
            b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\after",
        );
        assert!(t.row(0).cells[0].hyperlink_id.is_some());
        assert!(t.row(0).cells[4].hyperlink_id.is_none(), "after closer");
    }

    #[test]
    fn osc8_dedupes_same_url() {
        let mut t = Term::new(2, 16, 0);
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\a\x1b]8;;\x1b\\");
        feed(&mut t, b"\x1b]8;;https://example.com\x1b\\b\x1b]8;;\x1b\\");
        // Both cells reference the same intern id (interning dedupes).
        assert_eq!(
            t.row(0).cells[0].hyperlink_id,
            t.row(0).cells[1].hyperlink_id,
        );
    }

    #[test]
    fn osc8_id_param_parsed_but_unused_for_dedup() {
        // id= is parsed but not used as the dedup key (URL is). Two
        // sequences with the same URL but different `id=` still share
        // the same hyperlink id.
        let mut t = Term::new(2, 16, 0);
        feed(&mut t, b"\x1b]8;id=a;https://x.com\x1b\\X\x1b]8;;\x1b\\");
        feed(&mut t, b"\x1b]8;id=b;https://x.com\x1b\\Y\x1b]8;;\x1b\\");
        assert_eq!(
            t.row(0).cells[0].hyperlink_id,
            t.row(0).cells[1].hyperlink_id,
        );
    }

    // ----- OSC 52 (clipboard) ----------------------------------------------

    #[test]
    fn osc52_set_with_write_disabled_is_dropped() {
        let mut t = Term::new(2, 4, 0);
        // Default security = both gates closed.
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert!(t.drain_clipboard_requests().is_empty());
    }

    #[test]
    fn osc52_set_with_write_enabled_queues_request() {
        let mut t = Term::new(2, 4, 0);
        t.set_security(SecurityFlags {
            osc_52_read: false,
            osc_52_write: true,
        });
        feed(&mut t, b"\x1b]52;c;aGVsbG8=\x1b\\");
        let reqs = t.drain_clipboard_requests();
        assert_eq!(reqs.len(), 1);
        match &reqs[0] {
            ClipboardRequest::Set { data } => assert_eq!(data, b"hello"),
            ClipboardRequest::Query { .. } => panic!("expected Set"),
        }
    }

    #[test]
    fn osc52_query_with_read_disabled_is_dropped() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b]52;c;?\x1b\\");
        assert!(t.drain_clipboard_requests().is_empty());
    }

    #[test]
    fn osc52_query_with_read_enabled_queues_request() {
        let mut t = Term::new(2, 4, 0);
        t.set_security(SecurityFlags {
            osc_52_read: true,
            osc_52_write: false,
        });
        feed(&mut t, b"\x1b]52;c;?\x1b\\");
        let reqs = t.drain_clipboard_requests();
        assert_eq!(reqs.len(), 1);
        match &reqs[0] {
            ClipboardRequest::Query { selection } => assert_eq!(selection, b"c"),
            ClipboardRequest::Set { .. } => panic!("expected Query"),
        }
    }

    #[test]
    fn osc52_malformed_does_not_panic() {
        let mut t = Term::new(2, 4, 0);
        t.set_security(SecurityFlags {
            osc_52_read: true,
            osc_52_write: true,
        });
        feed(&mut t, b"\x1b]52;c;!!\x1b\\");
        assert!(t.drain_clipboard_requests().is_empty());
    }

    // ----- M11a: kitty APC integration -----

    /// Helper: produce a 1x1 red RGBA pixel base64-encoded.
    fn b64_red_1x1() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    #[test]
    fn apc_kitty_transmit_registers_image() {
        let mut t = Term::new(4, 8, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=1,v=1,i=42;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert!(t.image_registry().contains(42));
        // OK reply was queued.
        let replies = t.drain_pty_replies();
        assert!(!replies.is_empty());
        let s = String::from_utf8_lossy(&replies);
        assert!(s.contains(";OK"), "expected OK reply, got {s:?}");
    }

    #[test]
    fn apc_kitty_non_graphics_is_ignored() {
        // tmux / other APC users — payload doesn't start with 'G'.
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b_tmux passthrough payload\x1b\\");
        assert_eq!(t.image_registry().len(), 0);
    }

    #[test]
    fn apc_kitty_oversized_payload_replies_efbig() {
        let mut t = Term::new(2, 4, 0);
        // Shrink the cap so the test isn't sensitive to the default.
        t.set_image_cap(1024);
        // Declare a payload bigger than the cap.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=8192,v=8192,i=1,S=268435456;\x1b\\");
        feed(&mut t, &payload);
        let replies = t.drain_pty_replies();
        let s = String::from_utf8_lossy(&replies);
        assert!(s.contains("EFBIG"), "expected EFBIG, got {s:?}");
    }

    #[test]
    fn apc_kitty_transmit_and_place_advances_cursor() {
        let mut t = Term::new(8, 8, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=2,r=2;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        // Image placed.
        assert!(t.image_registry().contains(1));
        assert_eq!(t.image_grid().len(), 1);
        // Cursor advanced by `r=2` rows (column reset to 0).
        let cur = t.cursor();
        assert_eq!(cur.row, 2);
        assert_eq!(cur.col, 0);
    }

    #[test]
    fn apc_kitty_delete_all_clears_grid() {
        let mut t = Term::new(8, 8, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=1;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert_eq!(t.image_grid().len(), 1);
        // Now delete all.
        feed(&mut t, b"\x1b_Ga=d,d=A\x1b\\");
        assert_eq!(t.image_grid().len(), 0);
        // Uppercase 'A' also frees the bytes.
        assert_eq!(t.image_registry().len(), 0);
    }

    #[test]
    fn apc_kitty_animate_replies_enotsup() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b_Ga=a,i=1\x1b\\");
        let replies = t.drain_pty_replies();
        let s = String::from_utf8_lossy(&replies);
        assert!(s.contains("ENOTSUP"), "got {s:?}");
    }

    #[test]
    fn linefeed_shifts_image_placements_up_on_scroll() {
        let mut t = Term::new(2, 4, 0);
        // Place an image at the cursor (row 0).
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=1;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        // The placement was at row 0..1.
        let p_before = t.image_grid().iter().next().unwrap().row_range.clone();
        assert_eq!(p_before, 0..1);
        // Force a few newlines so the grid scrolls (only 2 rows total).
        feed(&mut t, b"\n\n\n");
        // Placement either shifted off-screen (dropped) or moved up.
        // With 2 rows + 3 newlines, the placement should be dropped.
        assert!(t.image_grid().is_empty());
    }

    #[test]
    fn alt_screen_enter_clears_image_grid() {
        let mut t = Term::new(4, 8, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=1;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert_eq!(t.image_grid().len(), 1);
        // Enter alt screen.
        feed(&mut t, b"\x1b[?1049h");
        assert_eq!(t.image_grid().len(), 0);
    }

    #[test]
    fn sgr_58_underline_color_indexed_stored() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[58;5;42m");
        assert_eq!(t.cursor_underline_color(), Some(Color::Indexed256(42)));
    }

    #[test]
    fn sgr_58_colon_form_indexed_stored() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[58:5:7m");
        assert_eq!(t.cursor_underline_color(), Some(Color::Indexed256(7)));
    }

    #[test]
    fn sgr_59_resets_underline_color() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[58;5;42m");
        assert!(t.cursor_underline_color().is_some());
        feed(&mut t, b"\x1b[59m");
        assert_eq!(t.cursor_underline_color(), None);
    }

    #[test]
    fn sgr_0_resets_underline_color() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[58;5;42m");
        feed(&mut t, b"\x1b[0m");
        assert_eq!(t.cursor_underline_color(), None);
    }

    #[test]
    fn placeholder_run_creates_image_placement() {
        let mut t = Term::new(4, 8, 0);
        // First, register image id=42.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=2,v=2,i=42;");
        // 2x2 RGBA all-red, base64-encoded.
        let raw = vec![255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255];
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&raw)
        };
        payload.extend_from_slice(b64.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert!(t.image_registry().contains(42));
        // Now emit a placeholder run with fg = Indexed256(42).
        // SGR 38;5;42 sets fg; then emit placeholder + first diacritic
        // (image row 0) + second diacritic (image col 0).
        feed(&mut t, b"\x1b[38;5;42m");
        // Placeholder char + a non-placeholder to finalize.
        let mut s = String::new();
        s.push(toastty_graphics::PLACEHOLDER);
        s.push(' '); // finalize
        feed(&mut t, s.as_bytes());
        // The image grid should have one placement of image 42.
        assert_eq!(t.image_grid().len(), 1);
        let p = t.image_grid().iter().next().unwrap();
        assert_eq!(p.image_id, 42);
    }

    #[test]
    fn placeholder_without_indexed_fg_is_inactive() {
        let mut t = Term::new(4, 8, 0);
        // No image registered, no SGR fg set.
        let mut s = String::new();
        s.push(toastty_graphics::PLACEHOLDER);
        s.push(' ');
        feed(&mut t, s.as_bytes());
        // No placements created.
        assert_eq!(t.image_grid().len(), 0);
    }

    #[test]
    fn placeholder_finalizes_on_csi() {
        let mut t = Term::new(4, 8, 0);
        // Register image.
        let raw = vec![255u8, 0, 0, 255];
        let b64 = {
            use base64::Engine;
            base64::engine::general_purpose::STANDARD.encode(&raw)
        };
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=1,v=1,i=7;");
        payload.extend_from_slice(b64.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        feed(&mut t, b"\x1b[38;5;7m");
        let mut s = String::new();
        s.push(toastty_graphics::PLACEHOLDER);
        feed(&mut t, s.as_bytes());
        // CSI sequence should finalize the run.
        feed(&mut t, b"\x1b[H");
        assert_eq!(t.image_grid().len(), 1);
    }

    #[test]
    fn image_revision_bumps_on_register_and_place() {
        let mut t = Term::new(4, 8, 0);
        let rev0 = t.image_revision();
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=1;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        let rev1 = t.image_revision();
        assert_ne!(rev0, rev1);
    }
}
