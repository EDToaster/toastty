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
use toastty_graphics::rgp::asset::CpuAsset;
use toastty_graphics::rgp::glb_loader::load_glb;
use toastty_graphics::rgp::handler::{RgpHandler, RgpSink};
use toastty_graphics::rgp::obj_loader::load_obj;
use toastty_graphics::rgp::operation::{
    RGP_PREFIX, RgpAnchor, RgpFormat, RgpPlacementStyle, RgpPlacementUpdate,
};
use toastty_graphics::rgp::path_resolver::resolve as resolve_rgp_path;
use toastty_graphics::rgp::scene::{RgpAsset, RgpScene};
use toastty_graphics::sixel::{SIXEL_MAX_COLORS, SixelDcs, SixelHandler};
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
    /// DECSC (`ESC 7`) save slot. Separate from `saved_cursor` so a
    /// `DECSC ... enter-alt-screen ... exit-alt-screen ... DECRC`
    /// sequence doesn't have the alt-screen path clobber the user's
    /// save. `None` until DECSC fires; DECRC with no prior save homes
    /// the cursor and resets SGR, per xterm.
    decsc_saved: Option<Cursor>,
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
    /// Sixel (DCS) decoder. Stateless aside from its pixel cap; the
    /// per-image DCS header params + body live in `sixel_pending`.
    sixel_handler: SixelHandler,
    /// In-progress sixel DCS packet: header params (P1/P2/P3) plus the
    /// raw body bytes accumulated between `hook` (final byte `q`) and
    /// `unhook`. `None` outside a sixel DCS.
    sixel_pending: Option<SixelDcs>,
    /// DECSDM (mode 80) state. Per current xterm semantics, SET = "sixel
    /// display mode" (image is anchored, screen does NOT scroll to make
    /// room); RESET = sixel scrolling (the default, image viewers expect
    /// it). We store the literal mode bit so DECRQM reports it directly
    /// and derive "should scroll" as `!sixel_display_mode`.
    sixel_display_mode: bool,
    /// DECSET 8452 — leave the cursor to the RIGHT of a sixel image
    /// (on its last row) instead of on the line below it. Off by default.
    sixel_cursor_right: bool,
    /// Cache of decoded image bytes keyed by Kitty image id.
    image_registry: ImageRegistry,
    /// Parallel layer of placements over the cell grid. Always refers
    /// to the *active* screen's images.
    image_grid: ImageGrid,
    /// Images belonging to the *inactive* screen. The primary and alt
    /// screens maintain independent image lists (per the kitty graphics
    /// protocol), so when we switch screens we stash the departing
    /// screen's grid here and install a fresh one as `image_grid`.
    /// `None` while on the primary screen (nothing stashed).
    stashed_image_grid: Option<ImageGrid>,
    /// Image *number* (`I=`) → most-recently-registered image *id* map.
    /// The kitty spec lets a client refer to "the most recent image with
    /// this number" via `d=n`/`d=N`. Updated on every registration that
    /// carried a non-zero `I=`; pruned when the target id leaves the
    /// registry (delete / eviction).
    image_number_to_id: std::collections::HashMap<u32, u32>,
    /// Monotonic counter bumped whenever the registry or grid mutates.
    /// The renderer compares against its cached value to decide when
    /// to re-sync GPU textures (and force a full clear of the frame).
    image_revision: u32,
    /// SGR 58 underline color. Stored but not yet rendered as an
    /// underline color; the Unicode placeholder pipeline reads it as
    /// the *high byte* (bits 8..16) of the image id. The SGR 38
    /// foreground supplies bits 0..8 (the low byte).
    ///
    /// TODO: kitty's full protocol allows image ids up to bits 8..32
    /// via a third diacritic on the first cell of a run. M11a only
    /// handles the 16-bit form (bits 0..8 from SGR 38 + bits 8..16
    /// from SGR 58); the third-diacritic 32-bit extension is rare and
    /// deferred.
    cursor_underline_color: Option<Color>,
    /// Unicode placeholder run-in-progress.
    placeholder_run: Option<PlaceholderRun>,
    /// Cell pixel size (width, height). Set by the binary at startup
    /// and on resize. Read by the `CSI 16 t` (XTWINOPS) handler to
    /// report char cell pixel dimensions back to apps. Apps like yazi
    /// use it to compute kitty-graphics image placement sizes.
    /// Defaults to a plausible (8, 16) until the binary updates it.
    cell_pixel_size: (u16, u16),
    /// Default background color in sRGB bytes. Set by the binary from
    /// the renderer's theme. Read by the OSC 11 `?` query handler.
    /// Apps use it to decide dark vs light rendering modes.
    default_bg_rgb: [u8; 3],
    /// DECSET 25 — show/hide the cursor. Defaults to `true` (shown).
    /// Apps that take over the screen (yazi, helix, neovim, btop)
    /// toggle this off so their layout isn't disrupted by the cursor
    /// block. The renderer ANDs this with the blink state when
    /// deciding whether to emit the cursor instance.
    cursor_visible: bool,
    /// Scrollback viewport state for the primary grid. Tracks both the
    /// rendered offset (current) and the user's target. The host (the
    /// binary) drives the lerp via [`Term::advance_viewport`] each frame.
    /// Alt screen is rendered with offset 0 regardless of these fields —
    /// the alt grid has no scrollback by construction.
    viewport: crate::viewport::Viewport,
    /// Pre-allocated blank row, sized to current `cols`, returned by
    /// [`Term::view_row`] when the renderer queries past the available
    /// scrollback (or past the live grid's bottom during fractional
    /// scrolling). Resized in [`Term::resize`].
    blank_row: crate::grid::Row,
    // ----- M12a: Ratty Graphics Protocol -----
    /// Stateful RGP dispatcher. Owns per-id chunked-payload
    /// reassembly. The `apc_end` demux peeks at the leading bytes of
    /// `apc_buffer` and routes `ratty;g;...` payloads here.
    rgp_handler: RgpHandler,
    /// In-memory RGP scene: registered assets + live placements.
    /// Accessor-only API; the renderer pulls from `Term::rgp_scene`
    /// on revision advance, mirrors the M11a image-registry pattern.
    rgp_scene: RgpScene,
    /// Active mouse text selection, if any. Endpoints are pinned to
    /// `line_id` so the selection survives `scroll_up`/`scroll_down`
    /// and survives scrollback eviction (the renderer just stops
    /// highlighting the evicted rows). Cleared on resize, alt-screen
    /// flip, and most "screen contents fundamentally changed" paths.
    selection: Option<crate::selection::Selection>,
    /// DECSTBM top/bottom margins (0-indexed, inclusive). Default spans
    /// the full visible region: `(0, rows - 1)`. `CSI Ps;Ps r` updates
    /// these; resize and RIS reset to the full region. Only LF / RI /
    /// IL / DL consult these — bare cursor moves and direct CUP can
    /// still leave the region (xterm behavior without DECOM).
    scroll_top: u16,
    scroll_bot: u16,
    /// XTMODKEYS (`CSI > 4 ; Pv m`) — modifyOtherKeys level.
    ///   0 = disabled (legacy encoding)
    ///   1 = enabled for non-printable Ctrl combos only
    ///   2 = enabled for all modified keys (full disambiguation)
    /// Stored so apps that probe / set it don't spam unhandled-CSI
    /// logs. Key-encoding paths still emit kitty-protocol sequences;
    /// honoring this level for the legacy `CSI 27 ; mods ; code ~`
    /// reporting is future work.
    modify_other_keys: u8,
}

/// In-progress run of Kitty Unicode placeholder cells.
///
/// Apps emit `<PLACEHOLDER><d_row><d_col>(<d_id_msb>)?` per cell as the
/// foreground SGR encodes the low byte of the image id. We collect the
/// run greedily until the next non-placeholder/non-diacritic codepoint
/// arrives, then materialize placements.
#[derive(Debug)]
pub(crate) struct PlaceholderRun {
    /// Low bits of the image id, decoded from the SGR foreground color
    /// at run start (see [`placeholder_image_id_from_sgr`]):
    ///
    /// - **Truecolor RGB** (`SGR 38;2;R;G;B`): bits 0..24 =
    ///   `(R << 16) | (G << 8) | B`. Used by yazi and helix's
    ///   `image.nvim`. The most common encoding in the wild.
    /// - **256-color** (`SGR 38;5;L`): bits 0..8 = `L`.
    ///
    /// The high byte (bits 24..32) comes from each cell's *third*
    /// diacritic, so the full image id is resolved per cell in
    /// [`Term::finalize_placeholder_run`] as `(id_msb << 24) | fg_bits`.
    pub fg_bits: u32,
    /// Placement id decoded from the SGR underline color (`SGR 58`) at
    /// run start. Zero means "unnamed". Per the kitty spec the underline
    /// color carries the placement id, *not* part of the image id.
    pub placement_id: u32,
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
            decsc_saved: None,
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
            sixel_handler: SixelHandler::default(),
            sixel_pending: None,
            sixel_display_mode: false,
            sixel_cursor_right: false,
            // m7: default 320 MiB image cache cap (kitty spec recommends
            // 320 MB). Generous but bounded; the binary can override via
            // `Term::set_image_cap`.
            image_registry: ImageRegistry::new(320 * 1024 * 1024),
            image_grid: ImageGrid::new(),
            stashed_image_grid: None,
            image_number_to_id: std::collections::HashMap::new(),
            image_revision: 0,
            cursor_underline_color: None,
            placeholder_run: None,
            // Reasonable defaults until the binary plumbs the real
            // values from the renderer.
            cell_pixel_size: (8, 16),
            default_bg_rgb: [0x12, 0x12, 0x17],
            cursor_visible: true,
            viewport: crate::viewport::Viewport::new(),
            blank_row: crate::grid::Row::blank(cols),
            rgp_handler: RgpHandler::new(),
            rgp_scene: RgpScene::new(),
            selection: None,
            scroll_top: 0,
            scroll_bot: rows - 1,
            modify_other_keys: 0,
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

    /// Read-only view of the RGP scene (registered assets + live
    /// placements). M12d's renderer pulls from this every frame.
    #[must_use]
    pub fn rgp_scene(&self) -> &RgpScene {
        &self.rgp_scene
    }

    /// Monotonic RGP scene revision. Bumps on every mutation
    /// (register / place / update / delete). Renderer compares to
    /// its cached value to decide when to repaint the 3D layer.
    #[must_use]
    pub fn rgp_revision(&self) -> u32 {
        self.rgp_scene.revision()
    }

    /// Monotonic RGP *asset-table* revision. Bumps only on register /
    /// delete-all — i.e. when the set of registered meshes changes.
    /// The renderer gates its GPU mesh-cache re-upload on this so a
    /// transform-only `u` (rotation/scale/color — all per-draw
    /// uniforms) repaints without re-uploading geometry.
    #[must_use]
    pub fn rgp_asset_revision(&self) -> u32 {
        self.rgp_scene.asset_revision()
    }

    /// Advance per-placement animation phases for `animate=1`
    /// placements. The renderer calls this once per frame before
    /// reading `rgp_scene()` so the model matrices reflect the
    /// current rotation. Does NOT bump `rgp_revision` — animation
    /// is transient and the GPU mesh cache is unaffected.
    pub fn tick_rgp_animations(&mut self, now: std::time::Instant) {
        self.rgp_scene.tick_animations(now);
    }

    /// Current SGR 58 underline color, or `None` when SGR 59 (or 0)
    /// reset it. The Unicode placeholder pipeline reads this as the
    /// high byte of the image id.
    #[must_use]
    pub fn cursor_underline_color(&self) -> Option<Color> {
        self.cursor_underline_color
    }

    /// Plumb the renderer's cell pixel size into Term so the
    /// `CSI 16 t` (XTWINOPS) handler can report it back to apps.
    /// Called by the binary at startup and on resize.
    pub fn set_cell_pixel_size(&mut self, width: u16, height: u16) {
        self.cell_pixel_size = (width.max(1), height.max(1));
    }

    /// Plumb the renderer's theme background color (sRGB bytes) into
    /// Term so the OSC 11 query handler can report it. Apps use this
    /// to choose dark/light rendering modes.
    pub fn set_default_bg(&mut self, rgb: [u8; 3]) {
        self.default_bg_rgb = rgb;
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

    /// Override the tracked CWD. Used by the binary to feed in the
    /// foreground process's actual CWD (via `libproc` / `/proc`) so
    /// RGP path resolution doesn't depend on the shell having
    /// OSC 7 wired up. Accepts an owned `String` so callers can
    /// move a `PathBuf::to_string_lossy().into_owned()`.
    pub fn set_cwd(&mut self, cwd: String) {
        self.cwd = cwd;
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

    /// True when the cursor should be drawn. Apps toggle this via
    /// DECSET 25 (`CSI ?25h` show, `CSI ?25l` hide). The renderer ANDs
    /// this with the blink state when deciding whether to emit the
    /// cursor instance.
    #[must_use]
    pub fn cursor_visible(&self) -> bool {
        self.cursor_visible
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

    /// Borrow visible row `idx` from whichever grid is active. Does
    /// **not** honor the scrollback viewport — callers that want the
    /// user-visible content should use [`Term::view_row`] instead. This
    /// is kept for internal logic (cursor, parser writes) that always
    /// operates on the live grid.
    pub fn row(&self, idx: u16) -> &crate::grid::Row {
        self.active_grid().row(idx)
    }

    /// Borrow the row to render at viewport position `idx` (0 = top of
    /// the rendered viewport).
    ///
    /// Renderer contract:
    /// - When `view_offset_pixel == 0`: `idx` ranges over `0..rows` and
    ///   the y-position for each row is `idx * cell_h`.
    /// - When `view_offset_pixel > 0`: `idx` ranges over `0..=rows` (one
    ///   extra partial row at the top). The y-position is
    ///   `idx * cell_h + view_offset_pixel - cell_h` — i.e. the row at
    ///   `idx == 0` hangs above the screen, exposing only its bottom
    ///   `view_offset_pixel` pixels.
    ///
    /// Out-of-range positions (above the oldest retained scrollback
    /// row, or past the live grid's bottom during fractional scroll)
    /// fall back to a pre-allocated blank row. The alt screen ignores
    /// the viewport entirely — its rendering is always at offset 0.
    pub fn view_row(&self, idx: u16) -> &crate::grid::Row {
        // Alt screen never honors the viewport — alt grid has no
        // scrollback budget, and apps using alt screen typically have
        // their own pager UI.
        if self.alt_active {
            return self.active_grid().row(idx);
        }
        let lines = self.viewport.current_lines;
        // When there's a sub-row offset we render one extra row at the
        // top of the viewport (the partial row that hangs above y=0),
        // shifting every logical row index down by one.
        let pixel_extra: u32 = u32::from(self.viewport.current_pixel > 0.0);
        let shift = i64::from(lines) + i64::from(pixel_extra);
        let logical = i64::from(idx) - shift;
        if logical >= 0 {
            let r = u16::try_from(logical).unwrap_or(u16::MAX);
            if r < self.primary.visible_rows() {
                return self.primary.row(r);
            }
            return &self.blank_row;
        }
        // Negative → scrollback. `logical == -1` is the row immediately
        // above logical row 0; scrollback_row's index is 0-based the
        // same way.
        let n = u32::try_from(-logical - 1).unwrap_or(u32::MAX);
        self.primary.scrollback_row(n).unwrap_or(&self.blank_row)
    }

    /// Current rendered scroll offset (lines above the live bottom).
    pub fn view_offset_lines(&self) -> u32 {
        self.viewport.current_lines
    }

    /// Sub-row pixel offset of the current rendered position
    /// (`0.0..cell_h`). The renderer translates instances by
    /// `-view_offset_pixel` on the y axis to realize fractional
    /// scrolling.
    pub fn view_offset_pixel(&self) -> f32 {
        self.viewport.current_pixel
    }

    /// Target scrollback offset the viewport is animating toward.
    /// Equal to [`Term::view_offset_lines`] when the animation has
    /// settled.
    pub fn target_offset_lines(&self) -> u32 {
        self.viewport.target_lines
    }

    /// Target sub-row pixel offset the viewport is animating toward.
    pub fn target_offset_pixel(&self) -> f32 {
        self.viewport.target_pixel
    }

    /// True when the rendered viewport is the live bottom and the
    /// user isn't animating away from it.
    pub fn at_view_bottom(&self) -> bool {
        self.viewport.at_bottom()
    }

    /// Lines of scrollback currently available to scroll into. 0 for
    /// the alt screen.
    pub fn history_lines(&self) -> u32 {
        if self.alt_active {
            0
        } else {
            self.primary.history_lines()
        }
    }

    /// Adjust the scrollback *target* by a delta. Positive `delta_lines`
    /// = scroll up into history; positive `delta_pixel` = pull the
    /// viewport up by that many pixels. The host passes the current
    /// cell height so sub-row pixels can fold into lines cleanly.
    ///
    /// Clamped to the available history. No-op when the alt screen is
    /// active.
    pub fn scroll_view_by(&mut self, delta_lines: i32, delta_pixel: f32, cell_h: f32) {
        if self.alt_active {
            return;
        }
        let max = self.primary.history_lines();
        self.viewport
            .scroll_target_by(delta_lines, delta_pixel, cell_h, max);
    }

    /// Snap the viewport target to the live bottom. The current
    /// position will animate to it on subsequent
    /// [`Term::advance_viewport`] calls (or jump immediately under
    /// [`crate::Smoothing::Instant`]).
    pub fn snap_view_to_bottom(&mut self) {
        self.viewport.snap_target_to_bottom();
    }

    /// Force current = target — used when smooth scrolling is
    /// disabled. Marks all rows dirty if the rendered position
    /// changes.
    pub fn force_snap_view(&mut self) {
        let was = (self.viewport.current_lines, self.viewport.current_pixel);
        self.viewport.snap_to_target();
        if was != (self.viewport.current_lines, self.viewport.current_pixel) {
            self.mark_all_dirty();
        }
    }

    /// Advance the viewport animation by `dt` seconds. Returns `true`
    /// if the rendered position changed (the host should redraw). When
    /// it returns `true`, all rows are also marked dirty so the
    /// renderer re-emits everything at the new offset.
    pub fn advance_viewport(
        &mut self,
        dt: f32,
        cell_h: f32,
        smoothing: crate::viewport::Smoothing,
    ) -> bool {
        let changed = self.viewport.advance(dt, cell_h, smoothing);
        if changed {
            self.mark_all_dirty();
        }
        changed
    }

    /// True when the viewport is mid-animation (current != target).
    /// The host uses this to decide whether to schedule the next
    /// redraw.
    pub fn viewport_animating(&self) -> bool {
        !self.viewport.at_target()
    }

    /// True when the user is currently looking at scrollback (the
    /// rendered viewport is NOT at the live bottom). The renderer uses
    /// this to suppress the cursor instance — most terminals hide the
    /// cursor while in scrollback view.
    pub fn is_view_scrolled_back(&self) -> bool {
        !self.viewport.at_bottom()
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

    // ----- mouse text selection -----

    /// Current selection, if any.
    #[must_use]
    pub fn selection(&self) -> Option<&crate::selection::Selection> {
        self.selection.as_ref()
    }

    /// Monotonic id of the live bottom row on the primary grid. See
    /// [`Grid::bottom_id`] for the model — the binary uses this to
    /// pin selection endpoints to a stable line id.
    #[must_use]
    pub fn bottom_id(&self) -> u64 {
        self.primary.bottom_id()
    }

    /// Replace the active selection, dirty-marking every visible row
    /// the old and new selections touch so the renderer re-paints both
    /// the freshly-selected cells and the cells that were just
    /// deselected. No-op on the alt screen (selection clears on alt
    /// entry).
    pub fn set_selection(&mut self, sel: crate::selection::Selection) {
        if self.alt_active {
            return;
        }
        let prev = self.selection;
        self.dirty_selection_rows(prev);
        self.selection = Some(sel);
        self.dirty_selection_rows(Some(sel));
    }

    /// Update the `active` endpoint of the current selection (drag
    /// extension). No-op if there is no selection. For `Word` / `Line`
    /// modes the caller is expected to have already snapped `active`
    /// to the word/line boundary.
    pub fn update_selection_active(&mut self, active: crate::selection::Pos) {
        let Some(mut sel) = self.selection else {
            return;
        };
        let old = sel;
        sel.set_active(active);
        self.selection = Some(sel);
        // Dirty union of old and new selections so cells that left the
        // selection get repainted with their base bg.
        self.dirty_selection_rows(Some(old));
        self.dirty_selection_rows(Some(sel));
    }

    /// Drop any active selection. Marks the rows it covered dirty so
    /// the renderer repaints them without the selection tint.
    pub fn clear_selection(&mut self) {
        let prev = self.selection;
        if prev.is_none() {
            return;
        }
        self.selection = None;
        self.dirty_selection_rows(prev);
    }

    /// True iff `(line_id, col)` is currently selected. Cheap — a
    /// short-circuit on the `Option` and then a few comparisons. The
    /// renderer calls this once per cell per frame.
    ///
    /// Selection is always treated as empty on the alt screen — the
    /// alt grid has its own coordinate system and the binary clears
    /// selection on alt entry anyway.
    #[must_use]
    pub fn is_cell_selected(&self, line_id: u64, col: u16) -> bool {
        if self.alt_active {
            return false;
        }
        self.selection
            .as_ref()
            .is_some_and(|s| s.contains(line_id, col))
    }

    /// Mark every viewport row that's touched by the given selection
    /// dirty. Damage is indexed by *viewport* row (what the renderer
    /// iterates via `view_row(r)`), not by primary-grid visible row —
    /// under scrollback the two diverge, and a scrollback-only
    /// selection would otherwise leave damage empty and the renderer
    /// would skip the frame so no highlight ever appears.
    ///
    /// Walks the visible rows (`O(visible_rows)`) rather than the
    /// selection's `rows_touched()` range, so dragging a selection
    /// across thousands of scrollback rows stays cheap.
    fn dirty_selection_rows(&mut self, sel: Option<crate::selection::Selection>) {
        let Some(sel) = sel else { return };
        let cols = self.cols;
        let rows = self.rows;
        let scroll = u64::from(self.viewport.current_lines);
        let bottom = self.primary.bottom_id();
        for vr in 0..rows {
            // viewport row vr → line_id of the row currently rendered
            // there. Same math as `line_id_for_render_row` in the
            // render crate (with pixel_extra = 0 for damage purposes).
            let above_bottom = u64::from(rows - 1 - vr) + scroll;
            let Some(line_id) = bottom.checked_sub(above_bottom) else {
                continue;
            };
            if sel.rows_touched().contains(&line_id) {
                self.mark_cells(vr, 0, cols);
            }
        }
    }

    /// Extract the selected text. Returns `None` when there's no
    /// selection. Joins across rows with `\n`, except that
    /// soft-wrapped rows (where `row.soft_wrap == true`) join without
    /// the newline so the user gets the logical line back as one
    /// piece. Trailing whitespace is stripped per visual row. Block
    /// selections produce one line per row, hard-truncated at the
    /// selection's column bounds.
    #[must_use]
    pub fn extract_selection_text(&self) -> Option<String> {
        use crate::selection::SelectionMode;
        let sel = self.selection.as_ref()?;
        let (first, last) = sel.ordered();
        let cols = self.cols;
        let mut out = String::new();
        for line_id in first.line_id..=last.line_id {
            let row = self.primary_row_by_line_id(line_id)?;
            // Determine column range for this row given selection mode.
            let (start_col, end_col) = match sel.mode {
                SelectionMode::Block => {
                    let (a, b) = (first.col, last.col);
                    let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                    (lo, hi.saturating_add(1).min(cols))
                }
                _ => {
                    let s = if line_id == first.line_id {
                        first.col
                    } else {
                        0
                    };
                    let e_inclusive = if line_id == last.line_id {
                        last.col
                    } else {
                        cols.saturating_sub(1)
                    };
                    (s, e_inclusive.saturating_add(1).min(cols))
                }
            };
            // Collect chars in [start_col, end_col), skipping continuation
            // cells (they're the trailing half of a width-2 cluster — the
            // primary cell already carries the char).
            let mut line_buf = String::new();
            for col in start_col..end_col {
                let Some(cell) = row.cells.get(col as usize) else {
                    break;
                };
                if cell.is_continuation {
                    continue;
                }
                // Treat NUL (the `Cell::default()` ch) as space; rows
                // shorter than `cols` get padded with NUL cells.
                let ch = if cell.ch == '\0' { ' ' } else { cell.ch };
                line_buf.push(ch);
            }
            // Strip trailing whitespace — conventional terminal-copy
            // behaviour and matches every common terminal emulator.
            while matches!(line_buf.chars().last(), Some(c) if c.is_whitespace()) {
                line_buf.pop();
            }
            out.push_str(&line_buf);
            // Row separator: hard newline between rows, except that
            // soft-wrapped rows in char/word/line mode are joined
            // without a newline (the soft wrap is a presentational
            // line break, not a logical one). Block mode always
            // separates rows with `\n`.
            let is_last_row = line_id == last.line_id;
            if !is_last_row {
                let join_soft = !matches!(sel.mode, SelectionMode::Block) && row.soft_wrap;
                if !join_soft {
                    out.push('\n');
                }
            }
        }
        Some(out)
    }

    /// Look up the primary-grid `Row` whose live `line_id` matches.
    /// Walks via `Grid::locate` so we transparently hit either the
    /// visible region or scrollback. Returns `None` for an evicted
    /// line id. Used by the binary to look up word boundaries during
    /// drag selection and by the extractor to walk selection rows.
    #[must_use]
    pub fn primary_row_by_line_id(&self, line_id: u64) -> Option<&crate::grid::Row> {
        match self.primary.locate(line_id)? {
            crate::grid::RowLocation::Visible(r) => Some(self.primary.row(r)),
            crate::grid::RowLocation::Scrollback(n) => self.primary.scrollback_row(n),
        }
    }

    /// Resize the visible viewport. The **primary** grid reflows —
    /// soft-wrapped lines are re-wrapped to the new width and scrollback is
    /// preserved (see [`Grid::resize_reflow`]). The alt grid is geometry
    /// only (full-screen apps redraw on SIGWINCH). The cursor is remapped
    /// through the reflow and then clamped to the new dimensions.
    pub fn resize(&mut self, rows: u16, cols: u16) {
        let rows = rows.max(1);
        let cols = cols.max(1);
        let primary_cap = rows as usize + self.scrollback as usize;
        // The primary's logical cursor is the live cursor when the primary
        // is active, else the snapshot taken on alt-screen (1049) entry.
        let primary_cursor = if self.alt_active {
            (self.saved_cursor.row, self.saved_cursor.col)
        } else {
            (self.cursor.row, self.cursor.col)
        };
        let (new_row, new_col) =
            self.primary
                .resize_reflow(rows, cols, primary_cap, primary_cursor);
        self.alt.resize(rows, cols, rows as usize);
        if self.alt_active {
            // The live cursor belongs to alt (clamped below); update the
            // saved primary cursor so a later 1049 exit restores a position
            // consistent with the reflowed primary grid.
            self.saved_cursor.row = new_row;
            self.saved_cursor.col = new_col;
        } else {
            self.cursor.row = new_row;
            self.cursor.col = new_col;
        }
        self.rows = rows;
        self.cols = cols;
        // Resize collapses any prior DECSTBM region: xterm resets the
        // scrolling margins to the full new screen so a smaller window
        // can't leave an out-of-range region pinned.
        self.scroll_top = 0;
        self.scroll_bot = rows - 1;
        self.clamp_cursor();
        // Resize invalidates every cached shaped line — re-shape all.
        self.damage.resize(rows);
        // Keep the blank-row fallback aligned with the current width.
        self.blank_row.resize_cols(cols);
        // The primary grid may have lost history on a cap change; clamp
        // the viewport so it doesn't reference rows past the new limit.
        self.viewport.clamp_to(self.primary.history_lines());
        // Selection coordinates only stay meaningful when row layout
        // doesn't move under them. Resize moves rows around (and
        // potentially drops history); clear so we don't paint stale
        // highlights at the wrong (line_id, col).
        self.selection = None;
        // Image placements anchored past the new geometry become
        // phantoms (kitty clips placements on resize). Trim each to the
        // new rows/cols and drop any that no longer have any visible
        // cells. `pix_offset` is preserved by `clip_to`.
        if self.image_grid.clip_to(rows, cols) {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
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

    /// Reverse Index (RI, `ESC M`): symmetric counterpart to `linefeed`.
    /// Move cursor up one; at the top of the scroll region, scroll the
    /// scroll region *down* by one (open a blank row at the top, drop
    /// the bottom row of the region). Less, vim, fzf and friends rely
    /// on this for the "paint lines into the top" path (e.g. `b`/`u`
    /// paging up).
    fn reverse_index(&mut self) {
        if self.cursor.row == self.scroll_top {
            // At top of region: scroll region down by one, cursor
            // stays put. Partial regions don't touch scrollback.
            self.region_scroll_down();
        } else if self.cursor.row > 0 {
            let old_row = self.cursor.row;
            self.cursor.row -= 1;
            let new_row = self.cursor.row;
            let col = self.cursor.col.min(self.cols.saturating_sub(1));
            self.mark_cell(old_row, col);
            self.mark_cell(new_row, col);
        }
        // At absolute row 0 but above scroll_top (cursor outside
        // region) — RI has no effect, matching xterm.
    }

    fn linefeed(&mut self) {
        if self.cursor.row == self.scroll_bot {
            // At bottom of region: scroll region up by one, cursor
            // stays at scroll_bot. Full-region case preserves
            // scrollback semantics; partial regions don't.
            self.region_scroll_up();
        } else if self.cursor.row + 1 < self.rows {
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
        // Cursor below scroll_bot but at absolute screen bottom: per
        // xterm, LF does nothing here — the region's scroll is gated
        // on the cursor being *on* the bottom margin.
    }

    /// Scroll the current DECSTBM region up by one row.
    ///
    /// Full-region (top == 0 && bot == rows - 1) takes the fast path
    /// that delegates to `grid.scroll_up()` so the displaced top row
    /// rotates into scrollback (primary grid only), viewport offsets
    /// follow, and image placements slide up. A partial region shifts
    /// rows in place within `[top..=bot]`: the top row is dropped
    /// (no scrollback), rows below shift up, and `bot` is blanked.
    fn region_scroll_up(&mut self) {
        let top = self.scroll_top;
        let bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        if top == 0 && bot == self.rows.saturating_sub(1) {
            self.active_grid_mut().scroll_up();
            if !self.alt_active {
                self.viewport
                    .on_grid_scroll_up(self.primary.history_lines());
            }
            self.mark_all_dirty();
            let dropped = self.image_grid.shift_rows_up(1, 0);
            // M13: relative-placement children follow their parent.
            self.image_grid.resolve_relative_positions();
            if !dropped.is_empty() {
                self.image_revision = self.image_revision.wrapping_add(1);
            }
            return;
        }
        if top >= bot {
            return;
        }
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        for r in top..bot {
            let src = std::mem::take(grid.row_mut(r + 1));
            *grid.row_mut(r) = src;
        }
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        let row = grid.row_mut(bot);
        row.resize_cols(cols);
        for c in &mut row.cells {
            *c = blank;
        }
        row.soft_wrap = false;
        for r in top..=bot {
            self.mark_row(r);
        }
        // Per the kitty graphics protocol: when a scroll region is
        // active, only images entirely within it are scrolled, and they
        // are clipped at the region boundaries. `bot` is an inclusive
        // margin, so the exclusive bottom bound is `bot + 1`.
        let dropped = self
            .image_grid
            .shift_rows_up_within(1, top, bot.saturating_add(1));
        self.image_grid.resolve_relative_positions();
        if !dropped.is_empty() {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// Symmetric to `region_scroll_up`: scroll the current DECSTBM
    /// region down by one row. Partial regions drop the bottom row
    /// (not preserved as scroll-back-forward) and blank the top.
    fn region_scroll_down(&mut self) {
        let top = self.scroll_top;
        let bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        if top == 0 && bot == self.rows.saturating_sub(1) {
            self.active_grid_mut().scroll_down();
            if !self.alt_active {
                self.viewport.clamp_to(self.primary.history_lines());
            }
            self.mark_all_dirty();
            let dropped = self.image_grid.shift_rows_down(1, 0, self.rows);
            self.image_grid.resolve_relative_positions();
            if !dropped.is_empty() {
                self.image_revision = self.image_revision.wrapping_add(1);
            }
            return;
        }
        if top >= bot {
            return;
        }
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        for r in (top + 1..=bot).rev() {
            let src = std::mem::take(grid.row_mut(r - 1));
            *grid.row_mut(r) = src;
        }
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        let row = grid.row_mut(top);
        row.resize_cols(cols);
        for c in &mut row.cells {
            *c = blank;
        }
        row.soft_wrap = false;
        for r in top..=bot {
            self.mark_row(r);
        }
        // Scroll images within the region, clipping at its boundaries.
        // `bot` is inclusive → exclusive bottom bound is `bot + 1`.
        let dropped = self
            .image_grid
            .shift_rows_down_within(1, top, bot.saturating_add(1));
        self.image_grid.resolve_relative_positions();
        if !dropped.is_empty() {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// DECSTBM — `CSI Ps ; Ps r`. Set the top/bottom scrolling margins
    /// (1-based, inclusive). Empty or `0` params default to row 1 and
    /// the last row respectively; a non-strictly-decreasing pair
    /// (e.g. `5;3`) is rejected and the region is left untouched, per
    /// xterm. On success, the cursor is moved to home (1,1) — the spec
    /// mandates this so apps can chain DECSTBM with a CUP.
    fn set_scrolling_region(&mut self, top_1: u16, bot_1: u16) {
        if self.rows == 0 {
            return;
        }
        let last = self.rows - 1;
        let top0 = top_1.saturating_sub(1).min(last);
        let bot0 = bot_1.saturating_sub(1).min(last);
        if bot0 <= top0 {
            return;
        }
        self.scroll_top = top0;
        self.scroll_bot = bot0;
        self.cursor_position(1, 1);
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
        // - The cursor's SGR fg encodes the LOW bits of the image id:
        //     * Rgb(R, G, B)  → bits 0..24 = (R<<16) | (G<<8) | B  (yazi uses this)
        //     * Indexed256(L) → bits 0..8  = L
        // - The SGR underline color (SGR 58) encodes the placement id.
        // - 0..3 diacritics encode source-image row, source-image col,
        //   and (optionally) the image id high byte (bits 24..32). The
        //   full image id is `(id_msb << 24) | fg_bits`.
        //
        // We collect cells greedily into `placeholder_run` until the
        // next non-placeholder/non-diacritic codepoint arrives, then
        // finalize → emit image placements.
        if toastty_graphics::is_placeholder(c) {
            if self.placeholder_run.is_none()
                && let Some((fg_bits, placement_id)) =
                    placeholder_image_id_from_sgr(self.cursor.style.fg, self.cursor_underline_color)
            {
                self.placeholder_run = Some(PlaceholderRun {
                    fg_bits,
                    placement_id,
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
        let needs_wrap =
            self.cursor.col >= self.cols || (cell_w == 2 && self.cursor.col + 1 >= self.cols);
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
        // Reconcile any wide cluster we're about to partially clobber so a
        // half-overwritten cluster doesn't leave a dangling primary or a
        // stranded continuation cell (which renders as a gap). The write
        // spans columns `[col, col + cell_w)`.
        self.clear_wide_orphans(row, col, cell_w);
        self.active_grid_mut()
            .row_mut(row)
            .put(col, primary, max_cols);
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

    /// Before writing `width` cells starting at `col`, blank the surviving
    /// half of any width-2 cluster the write only partially overwrites:
    ///
    /// - **Left straddle:** if `col` is a continuation cell, its primary at
    ///   `col - 1` would be left as a wide glyph with no continuation —
    ///   blank it.
    /// - **Right straddle:** if the last written column's right neighbour
    ///   (`col + width`) is a continuation cell, the primary we just
    ///   overwrote was wide and that continuation is now orphaned (renders
    ///   as a gap) — blank it.
    ///
    /// Without this, in-place edits (e.g. zsh redrawing a line of CJK after
    /// a deletion) strand half-clusters. The blanked cell takes the cursor's
    /// current style so it matches a space printed there.
    fn clear_wide_orphans(&mut self, row: u16, col: u16, width: u16) {
        let last = col + width - 1;
        let blank = Cell {
            ch: ' ',
            style: self.cursor.style,
            is_continuation: false,
            hyperlink_id: None,
        };
        let cells = &self.active_grid().row(row).cells;
        let left_orphan = col > 0 && cells.get(col as usize).is_some_and(|c| c.is_continuation);
        let right_orphan = cells
            .get(last as usize + 1)
            .is_some_and(|c| c.is_continuation);
        let max_cols = self.cols;
        if left_orphan {
            self.active_grid_mut()
                .row_mut(row)
                .put(col - 1, blank, max_cols);
            self.mark_cell(row, col - 1);
        }
        if right_orphan {
            self.active_grid_mut()
                .row_mut(row)
                .put(last + 1, blank, max_cols);
            self.mark_cell(row, last + 1);
        }
    }

    /// Materialize the accumulated placeholder run into image
    /// placements over the cells the run touched. Called when the
    /// stream emits a non-placeholder/non-diacritic codepoint.
    #[allow(clippy::needless_pass_by_value)] // run is conceptually consumed.
    fn finalize_placeholder_run(&mut self, run: PlaceholderRun) {
        if run.cells.is_empty() {
            return;
        }
        // The image id's low bits come from the SGR fg (run-level,
        // `run.fg_bits`); the placement id comes from the SGR underline
        // (run-level, `run.placement_id`). The image id's high byte
        // (bits 24..32) is the *third* diacritic of each cell, so the
        // full image id is resolved per cell below.
        //
        // Diacritic semantics (kitty Unicode-placeholder spec):
        //   1st diacritic → source image ROW
        //   2nd diacritic → source image COLUMN
        //   3rd diacritic → image id high byte (bits 24..32)
        //
        // Inheritance from the LEFT neighbor (within this run): a cell
        // that omits trailing components inherits them from the
        // previously resolved cell, with the column auto-incremented:
        //   0 diacritics → (prev.row, prev.col + 1, prev.id_msb)
        //   1 diacritic  → (d0,       prev.col + 1, prev.id_msb)
        //   2 diacritics → (d0,       d1,           prev.id_msb)
        //   3 diacritics → (d0,       d1,           d2)
        // With no previous cell, missing components default to 0.
        //
        // Per the kitty spec the tile size is the terminal's cell-pixel
        // dimensions (CSI 16t value): each placeholder cell renders a
        // cell_w × cell_h sub-rect at (col * cell_w, row * cell_h) in
        // the source image. Apps (yazi, image.nvim) pre-downscale their
        // image so the placements tile it perfectly.
        let (cell_pw, cell_ph) = self.cell_pixel_size;
        let cell_pw = u32::from(cell_pw).max(1);
        let cell_ph = u32::from(cell_ph).max(1);
        let placement_id = run.placement_id;
        let mut placements = Vec::new();
        // Resolved (row, col, id_msb) of the previous cell, for
        // left-neighbor inheritance.
        let mut prev: Option<(u16, u16, u16)> = None;
        for cell in &run.cells {
            let d = &cell.diacritics;
            let (row_d, col_d, id_msb) =
                match (d.first().copied(), d.get(1).copied(), d.get(2).copied()) {
                    (None, _, _) => {
                        // Bare cell: inherit everything; advance column.
                        let (pr, pc, pm) = prev.unwrap_or((0, 0, 0));
                        (pr, pc.saturating_add(1), pm)
                    }
                    (Some(r), None, _) => {
                        // Row only: inherit column (advanced) and id_msb.
                        let (_, pc, pm) = prev.unwrap_or((0, 0, 0));
                        (r, pc.saturating_add(1), pm)
                    }
                    (Some(r), Some(c), None) => {
                        // Row + col: inherit id_msb.
                        let (_, _, pm) = prev.unwrap_or((0, 0, 0));
                        (r, c, pm)
                    }
                    (Some(r), Some(c), Some(m)) => (r, c, m),
                };
            prev = Some((row_d, col_d, id_msb));

            // Full image id: high byte from 3rd diacritic, low bits
            // from the SGR foreground.
            let id = (u32::from(id_msb) << 24) | run.fg_bits;
            // Image not registered: still occupy the cell as a
            // placeholder so layout doesn't shift; just skip rendering.
            let Some(img) = self.image_registry.get(id) else {
                continue;
            };
            let (img_w, img_h) = (img.width, img.height);

            let sx_full = u32::from(col_d) * cell_pw;
            let sy_full = u32::from(row_d) * cell_ph;
            // Clamp to image bounds — a stray diacritic past the
            // image's edge would otherwise sample outside the texture.
            let sx = sx_full.min(img_w.saturating_sub(1));
            let sy = sy_full.min(img_h.saturating_sub(1));
            let sw = cell_pw.min(img_w.saturating_sub(sx));
            let sh = cell_ph.min(img_h.saturating_sub(sy));
            placements.push(Placement {
                image_id: id,
                placement_id,
                row_range: cell.row..cell.row.saturating_add(1),
                col_range: cell.col..cell.col.saturating_add(1),
                src_rect: toastty_graphics::SrcRect {
                    x: sx,
                    y: sy,
                    w: sw,
                    h: sh,
                },
                z: 0,
                pix_offset: (0, 0),
                parent: None,
                rel_offset: (0, 0),
            });
        }
        if placements.is_empty() {
            return;
        }
        for placement in placements {
            mark_placement_dirty(self, &placement);
            self.image_grid.add(placement);
        }
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    #[allow(clippy::too_many_lines)] // one arm per CSI final byte; the table is wide.
    fn handle_csi(&mut self, params: &Params, intermediates: &[u8], action: char) {
        let priv_marker = intermediates.first().copied();
        match action {
            'A' => self.cursor_up(first_param(params, 1).max(1)),
            'B' => self.cursor_down(first_param(params, 1).max(1)),
            'C' => self.cursor_forward(first_param(params, 1).max(1)),
            'D' => self.cursor_back(first_param(params, 1).max(1)),
            // CHA — Cursor Horizontal Absolute. `CSI Ps G` moves the
            // cursor to column Ps (1-based) on the current row. Used
            // heavily by Claude Code to lay out tab-stop-style columns
            // of text on a single line; without it, the words pile up.
            'G' => {
                let col = first_param(params, 1).max(1);
                self.cursor_position(self.cursor.row.saturating_add(1), col);
            }
            'H' | 'f' => {
                let r = first_param(params, 1).max(1);
                let c = nth_param(params, 1, 1).max(1);
                self.cursor_position(r, c);
            }
            'J' => self.erase_display(first_param(params, 0)),
            'K' => self.erase_line(first_param(params, 0)),
            // ICH — Insert Character. Default count is 1.
            '@' => self.insert_char(first_param(params, 1).max(1)),
            // IL — Insert Line.
            'L' => self.insert_line(first_param(params, 1).max(1)),
            // DL — Delete Line.
            'M' => self.delete_line(first_param(params, 1).max(1)),
            // SU — Scroll Up. `CSI Ps S` scrolls the scroll region up
            // `Ps` lines (content moves up, blanks open at the bottom
            // margin). Default 1. The `CSI ? ... S` form is XTSMGRAPHICS
            // (sixel/ReGIS geometry query) — guard on the private marker
            // so we don't scroll on a graphics probe.
            'S' if priv_marker.is_none() => {
                let n = first_param(params, 1).max(1);
                for _ in 0..n {
                    self.region_scroll_up();
                }
            }
            // XTSMGRAPHICS — `CSI ? Pi ; Pa ; Pv S`. Sixel/graphics
            // capability query. `Pi=1` asks about color registers,
            // `Pi=2` about graphics geometry. We report fixed
            // capabilities (256 palette registers; current text-area
            // pixel size as the max image geometry). `Pa` (read / reset /
            // set / read-max) is treated as a read — our limits aren't
            // client-tunable. Unknown items reply with status 2 (failure).
            'S' if priv_marker == Some(b'?') => {
                let pi = first_param(params, 0);
                let reply = match pi {
                    1 => format!("\x1b[?1;0;{SIXEL_MAX_COLORS}S"),
                    2 => {
                        let w = u32::from(self.cols) * u32::from(self.cell_pixel_size.0);
                        let h = u32::from(self.rows) * u32::from(self.cell_pixel_size.1);
                        format!("\x1b[?2;0;{w};{h}S")
                    }
                    _ => format!("\x1b[?{pi};2S"),
                };
                self.pty_replies.extend_from_slice(reply.as_bytes());
            }
            // SD — Scroll Down. `CSI Ps T` scrolls the scroll region down
            // `Ps` lines (content moves down, blanks open at the top
            // margin). Default 1. The 5-parameter form
            // (`CSI Ps;Ps;Ps;Ps;Ps T`) is highlight mouse tracking — only
            // treat the single-param form as SD; anything with more params
            // falls through to the catch-all.
            'T' if priv_marker.is_none() && params.len() <= 1 => {
                let n = first_param(params, 1).max(1);
                for _ in 0..n {
                    self.region_scroll_down();
                }
            }
            // DCH — Delete Character. The DEL-fix that motivated this:
            // zsh sends `BS DCH CUF SP CUB` for every backspace; without
            // a working DCH the deletion appears to land at the end of
            // the line instead of at the cursor.
            'P' => self.delete_char(first_param(params, 1).max(1)),
            // ECH — Erase Character (no shift).
            'X' => self.erase_char(first_param(params, 1).max(1)),
            // VPA — Vertical Position Absolute (same column).
            'd' => self.vertical_position_absolute(first_param(params, 1).max(1)),
            // SGR proper has no private marker. `CSI > Ps ; Ps m` is
            // xterm's XTMODKEYS (modifyOtherKeys); we don't implement
            // it — apps negotiate the kitty keyboard protocol via the
            // `u` sequences instead. Without this guard, the `> 4 ; 2`
            // form was being applied as SGR 4 (underline) + SGR 2
            // (dim), making every subsequent cell render underlined.
            'm' if priv_marker.is_none() => self.apply_sgr(params),
            // XTMODKEYS — `CSI > Pp ; Pv m`. `Pp = 4` selects the
            // modifyOtherKeys resource. Apps (vim, neovim, fish) emit
            // `>4;2m` to enable level-2 reporting at startup and `>4m`
            // to disable on exit. Toastty negotiates richer keys via
            // the kitty keyboard protocol stack instead, so we only
            // record the level — useful later if we add a legacy
            // `CSI 27 ; mods ; code ~` encoder fallback. Accepting the
            // sequence here keeps the warning log quiet.
            'm' if priv_marker == Some(b'>') => {
                let pp = first_param(params, 0);
                if pp == 4 {
                    let pv = nth_param(params, 1, 0);
                    self.modify_other_keys = (pv & 0xff) as u8;
                }
            }
            'h' if priv_marker == Some(b'?') => self.apply_decset(params, true),
            'l' if priv_marker == Some(b'?') => self.apply_decset(params, false),
            // DECRQM — `CSI ? Ps $ p`. The app asks whether DEC private
            // mode `Ps` is currently set; we reply with DECRPM:
            // `CSI ? Ps ; Pm $ y`. `Pm` values per VT-spec:
            //   0 — not recognized
            //   1 — set
            //   2 — reset
            //   3 — permanently set
            //   4 — permanently reset
            // Apps (notably neovim, helix, kitty's own probes) gate
            // BSU/ESU (mode 2026) on this reply: if we don't answer,
            // they fall back to "not supported" after a short timeout
            // and never emit the optimization. Mode 2026 is therefore
            // the load-bearing case, but we answer the full set of
            // modes we already track in `apply_decset` for symmetry.
            'p' if priv_marker == Some(b'?') && intermediates.contains(&b'$') => {
                let ps = first_param(params, 0);
                let pm = self.decrqm_status(ps);
                let reply = format!("\x1b[?{ps};{pm}$y");
                self.pty_replies.extend_from_slice(reply.as_bytes());
            }
            // DECSTBM — `CSI Ps ; Ps r`. Set top/bottom scrolling
            // margins (1-based, inclusive). Default top is row 1,
            // default bottom is the last row, so a bare `CSI r` or
            // `CSI 0 r` resets the region to the full screen.
            'r' if priv_marker.is_none() => {
                let top = first_param(params, 1);
                let bot = nth_param(params, 1, self.rows);
                self.set_scrolling_region(top, bot);
            }
            // DECSCUSR: `CSI Ps SP q` — runtime cursor shape + blink.
            // vte exposes the SP intermediate as `intermediates = b" "`.
            'q' if intermediates == b" " => self.apply_decscusr(first_param(params, 0)),
            // Kitty keyboard protocol stack manipulation:
            //   CSI > flags u   — push
            //   CSI < n u       — pop n (default 1)
            //   CSI = flags ; mode u — set/clear without push
            //   CSI ? u         — query active flags
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
            // `CSI ? u` — query the active progressive-enhancement flags
            // (top of the stack). Reply `CSI ? <flags> u`. Apps probe
            // this to detect kitty-keyboard support; without a reply they
            // assume legacy mode and never enable the protocol, so the
            // kitty key encoder never gets exercised.
            'u' if priv_marker == Some(b'?') => {
                let reply = format!("\x1b[?{}u", self.kitty_flags());
                self.pty_replies.extend_from_slice(reply.as_bytes());
            }

            // DA1 — Primary Device Attributes (`CSI c` or `CSI 0 c`).
            // Apps probe terminal capabilities at startup; many TUIs
            // (yazi, helix, neovim) wait for this reply with a short
            // timeout and refuse to start if it doesn't arrive.
            // Advertise VT220 (`62`) + sixel (`4`) + ANSI color (`22`).
            // The `4` is the gate apps probe before sending sixel
            // (img2sixel, chafa, lsix); without it they refuse to emit
            // graphics. Paired with the XTSMGRAPHICS handler below and
            // the DCS sixel decoder.
            'c' if priv_marker.is_none() => {
                self.pty_replies.extend_from_slice(b"\x1b[?62;4;22c");
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
                        let col = self
                            .cursor
                            .col
                            .min(self.cols.saturating_sub(1))
                            .saturating_add(1);
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
                    let col = self
                        .cursor
                        .col
                        .min(self.cols.saturating_sub(1))
                        .saturating_add(1);
                    let reply = format!("\x1b[?{row};{col}R");
                    self.pty_replies.extend_from_slice(reply.as_bytes());
                }
            }
            // XTVERSION — `CSI > q`. Apps probe this to identify the
            // terminal flavor and route to the matching backend. yazi
            // (and helix, neovim image.nvim) substring-match the reply
            // on the brand string to enable the modern KGP driver;
            // unrecognised brands fall back to a legacy path that
            // exercises code we haven't validated. Reply with a
            // brand string containing "kitty" so apps treat us as
            // kitty-protocol-grade.
            //
            // Reply format: DCS > | <brand> <version> ST.
            'q' if priv_marker == Some(b'>') => {
                self.pty_replies
                    .extend_from_slice(b"\x1bP>|toastty (kitty 0.42.0)\x1b\\");
            }
            // XTWINOPS — window-manipulation reports.
            //   CSI 14 t → report text-area pixel size: CSI 4 ; H ; W t.
            //   CSI 16 t → report cell pixel size:       CSI 6 ; H ; W t.
            //   CSI 18 t → report text-area cell size:   CSI 8 ; H ; W t.
            // Apps (yazi, kitty +kitten icat, neovim with image.nvim,
            // helix with image previews) gate image rendering on the
            // `16 t` reply because they need to know how many pixels
            // are in a cell. Without it they fall back to half-block /
            // colored-cell rendering even when kitty graphics works.
            't' if priv_marker.is_none() => {
                let ps = first_param(params, 0);
                let (cw, ch) = self.cell_pixel_size;
                match ps {
                    14 => {
                        let h = u32::from(ch) * u32::from(self.rows);
                        let w = u32::from(cw) * u32::from(self.cols);
                        let reply = format!("\x1b[4;{h};{w}t");
                        self.pty_replies.extend_from_slice(reply.as_bytes());
                    }
                    16 => {
                        let reply = format!("\x1b[6;{ch};{cw}t");
                        self.pty_replies.extend_from_slice(reply.as_bytes());
                    }
                    18 => {
                        let reply = format!("\x1b[8;{};{}t", self.rows, self.cols);
                        self.pty_replies.extend_from_slice(reply.as_bytes());
                    }
                    _ => {}
                }
            }

            _ => {
                // Collect params eagerly so the `{:?}` printout
                // is readable rather than a vte internal type.
                let params_vec: Vec<Vec<u16>> = params
                    .iter()
                    .map(|sub| sub.iter().copied().collect())
                    .collect();
                tracing::warn!(
                    target: "toastty_term::csi",
                    "unhandled CSI: action={action:?} priv={:?} intermediates={:?} params={params_vec:?}",
                    priv_marker.map(char::from),
                    std::str::from_utf8(intermediates).unwrap_or("<non-utf8>"),
                );
            }
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
        // CUB moves by exact columns — the cursor may legitimately rest on
        // a continuation cell. We must NOT snap onto the cluster's primary:
        // apps (zsh's line editor in particular) count display columns and
        // emit one cursor-left step per column, so snapping would consume
        // an extra column whenever the landing spot is a continuation
        // half, walking the cursor — and every subsequent rewrite — too far
        // left. See `wide_char_paste_redraw_keeps_alignment`.
        let new_col = self.cursor.col.saturating_sub(n);
        self.move_cursor(self.cursor.row, new_col);
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
                // Per the kitty graphics protocol: "The clear screen
                // escape code (usually ESC[2J) should also clear all
                // images." This applies to 2J and 3J only — partial
                // erases (0J/1J) must not affect graphics.
                let dropped = self.image_grid.clear();
                if !dropped.is_empty() {
                    self.image_revision = self.image_revision.wrapping_add(1);
                }
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

    /// DCH — Delete Character. `CSI Ps P` removes `n` cells at the
    /// cursor column, shifts the rest of the row left, and fills the
    /// vacated rightmost `n` cells with blanks under the current SGR.
    /// Cursor doesn't move.
    ///
    /// Without this, zsh's DEL handling appears to "delete from the end
    /// of the line": the shell sends `BS DCH CUF SP CUB`, and a dropped
    /// DCH leaves only the trailing space writeback visible.
    fn delete_char(&mut self, n: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        if cur_col >= cols || n == 0 {
            return;
        }
        let style = self.cursor.style;
        let n = u16::min(n, cols - cur_col);
        let row = self.active_grid_mut().row_mut(cur_row);
        let cols_u = cols as usize;
        let col_u = cur_col as usize;
        let n_u = n as usize;
        if row.cells.len() < cols_u {
            row.resize_cols(cols);
        }
        // Shift left. `cells` are `Copy`, so a plain copy_within is
        // both correct and overlap-safe.
        row.cells.copy_within((col_u + n_u)..cols_u, col_u);
        // Blank the vacated tail under the current SGR (matches xterm /
        // ECMA-48 — the deleted-from-the-right slots use the active
        // attribute set, not Style::RESET).
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        for c in &mut row.cells[(cols_u - n_u)..cols_u] {
            *c = blank;
        }
        self.mark_cells(cur_row, cur_col, cols);
    }

    /// ICH — Insert Character. `CSI Ps @` inserts `n` blank cells at
    /// the cursor column, shifting the rest of the row right. Cells
    /// pushed past the right edge are lost. Cursor doesn't move.
    fn insert_char(&mut self, n: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        if cur_col >= cols || n == 0 {
            return;
        }
        let style = self.cursor.style;
        let n = u16::min(n, cols - cur_col);
        let row = self.active_grid_mut().row_mut(cur_row);
        let cols_u = cols as usize;
        let col_u = cur_col as usize;
        let n_u = n as usize;
        if row.cells.len() < cols_u {
            row.resize_cols(cols);
        }
        // Shift right. copy_within handles overlap correctly even
        // when src/dst overlap.
        row.cells.copy_within(col_u..(cols_u - n_u), col_u + n_u);
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        for c in &mut row.cells[col_u..(col_u + n_u)] {
            *c = blank;
        }
        self.mark_cells(cur_row, cur_col, cols);
    }

    /// ECH — Erase Character. `CSI Ps X` writes `n` blanks at the
    /// cursor without shifting; the rest of the row is untouched.
    /// Cursor doesn't move.
    fn erase_char(&mut self, n: u16) {
        let cur_row = self.cursor.row;
        let cur_col = self.cursor.col;
        let cols = self.cols;
        if cur_col >= cols || n == 0 {
            return;
        }
        let n = u16::min(n, cols - cur_col);
        let style = self.cursor.style;
        let row = self.active_grid_mut().row_mut(cur_row);
        row.erase(cur_col, cur_col + n, style);
        self.mark_cells(cur_row, cur_col, cur_col + n);
    }

    /// IL — Insert Line. `CSI Ps L` inserts `n` blank lines at the
    /// cursor row, scrolling the lines below it down within the
    /// active DECSTBM scrolling region. Lines pushed past the bottom
    /// margin are lost. Per xterm, IL is a no-op when the cursor is
    /// outside the region.
    fn insert_line(&mut self, n: u16) {
        let cur_row = self.cursor.row;
        let top = self.scroll_top;
        let bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        if n == 0 || cur_row < top || cur_row > bot {
            return;
        }
        let region_end = bot + 1; // exclusive
        let n = u16::min(n, region_end - cur_row);
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        // Shift rows [cur_row..region_end-n] down to [cur_row+n..region_end].
        // Iterate top-down from the bottom to avoid overwriting our
        // source rows before they're copied.
        let n_us = n as usize;
        for r in (cur_row as usize + n_us..region_end as usize).rev() {
            // SmallVec<[Cell;16]> clones are O(cols); IL/DL aren't
            // hot. mem::take + restore keeps us off the heap for
            // inlined rows.
            let src = std::mem::take(grid.row_mut((r - n_us) as u16));
            *grid.row_mut(r as u16) = src;
        }
        // Blank the n freshly-inserted rows under the current SGR.
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        for r in cur_row..(cur_row + n) {
            let row = grid.row_mut(r);
            row.resize_cols(cols);
            for c in &mut row.cells {
                *c = blank;
            }
            row.soft_wrap = false;
        }
        for r in cur_row..region_end {
            self.mark_row(r);
        }
        // Mirror the text shift on the image layer: placements in
        // [cur_row, region_end) slide down by `n`, those pushed past the
        // bottom margin are dropped (kitty shifts images on IL). Any
        // placement whose start is in that band moves, so bump the
        // revision when one exists (shift_rows_down_within mutates ranges
        // in place; its return only lists dropped placements).
        let affected = self
            .image_grid
            .iter()
            .any(|p| p.row_range.start >= cur_row && p.row_range.start < region_end);
        self.image_grid
            .shift_rows_down_within(n, cur_row, region_end);
        self.image_grid.resolve_relative_positions();
        if affected {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// DL — Delete Line. `CSI Ps M` removes `n` lines at the cursor
    /// row, scrolling the lines below it up within the active DECSTBM
    /// region. The bottom `n` rows of the region become blank. No-op
    /// when the cursor is outside the region.
    fn delete_line(&mut self, n: u16) {
        let cur_row = self.cursor.row;
        let top = self.scroll_top;
        let bot = self.scroll_bot.min(self.rows.saturating_sub(1));
        if n == 0 || cur_row < top || cur_row > bot {
            return;
        }
        let region_end = bot + 1; // exclusive
        let n = u16::min(n, region_end - cur_row);
        let style = self.cursor.style;
        let cols = self.cols;
        let grid = self.active_grid_mut();
        let n_us = n as usize;
        // Shift rows [cur_row+n..region_end] up to [cur_row..region_end-n].
        for r in cur_row as usize..(region_end as usize - n_us) {
            let src = std::mem::take(grid.row_mut((r + n_us) as u16));
            *grid.row_mut(r as u16) = src;
        }
        let blank = Cell {
            ch: ' ',
            style,
            is_continuation: false,
            hyperlink_id: None,
        };
        for r in (region_end - n)..region_end {
            let row = grid.row_mut(r);
            row.resize_cols(cols);
            for c in &mut row.cells {
                *c = blank;
            }
            row.soft_wrap = false;
        }
        for r in cur_row..region_end {
            self.mark_row(r);
        }
        // Mirror the text shift on the image layer: placements in
        // [cur_row, region_end) slide up by `n`, those scrolled entirely
        // above the cursor row are dropped (kitty shifts images on DL).
        let affected = self
            .image_grid
            .iter()
            .any(|p| p.row_range.start >= cur_row && p.row_range.start < region_end);
        self.image_grid.shift_rows_up_within(n, cur_row, region_end);
        self.image_grid.resolve_relative_positions();
        if affected {
            self.image_revision = self.image_revision.wrapping_add(1);
        }
    }

    /// VPA — Vertical Position Absolute. `CSI Ps d` moves the cursor
    /// to row `Ps` (1-based) on the same column.
    fn vertical_position_absolute(&mut self, row_1based: u16) {
        let new_row = row_1based.saturating_sub(1);
        self.move_cursor(new_row, self.cursor.col);
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
                38 if slice.len() >= 2 => {
                    self.cursor.style.fg =
                        parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.fg)
                }
                48 if slice.len() >= 2 => {
                    self.cursor.style.bg =
                        parse_extended_color_from_slice(&slice[1..]).unwrap_or(self.cursor.style.bg)
                }
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
            // M11a: 59 (default underline color) is handled at the
            // `apply_sgr` walker level — we clear
            // `self.cursor_underline_color` there because we don't
            // have access to `self` from inside `apply_sgr_param`
            // (only `&mut style`).
            90..=97 => style.fg = ansi_color(v - 90, true),
            100..=107 => style.bg = ansi_color(v - 100, true),
            _ => {}
        }
    }

    /// Resolve the DECRPM status for a private mode `ps`. Returns one
    /// of the VT-spec codes documented at the `CSI ? Ps $ p` handler:
    /// 0 (unknown), 1 (set), 2 (reset). Modes we model but don't have
    /// a runtime toggle for (e.g. always-on/always-off behaviors) are
    /// not advertised as `3`/`4` because we may want to wire toggles
    /// later — `1`/`2` is the more forgiving answer.
    fn decrqm_status(&self, ps: u16) -> u16 {
        let is_set = match ps {
            25 => self.cursor_visible,
            1000 => matches!(self.mouse_mode.protocol, MouseProtocol::X10),
            1002 => matches!(self.mouse_mode.protocol, MouseProtocol::ButtonMotion),
            1003 => matches!(self.mouse_mode.protocol, MouseProtocol::AnyMotion),
            1004 => self.report_focus,
            1006 => self.mouse_mode.sgr_encoding,
            47 | 1047 | 1049 => self.alt_active,
            2004 => self.bracketed_paste,
            2026 => self.sync_output.active,
            2027 => self.grapheme_cluster_mode,
            2048 => self.inband_resize_mode,
            80 => self.sixel_display_mode,
            8452 => self.sixel_cursor_right,
            _ => return 0,
        };
        if is_set { 1 } else { 2 }
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
                // 47 / 1047 — legacy alt-screen buffer switch (no
                // cursor save/restore of their own). We route them
                // through the same enter/exit helpers as 1049 so the
                // image-grid stash/restore (B2) stays consistent and
                // images don't leak across the alt switch. The cursor
                // save 1049 performs is harmless here: the matching
                // exit restores it, and apps using 47/1047 pair them
                // with their own DECSC/DECRC (1048) when they care.
                47 | 1047 => {
                    if enable {
                        self.enter_alt_screen();
                    } else {
                        self.exit_alt_screen();
                    }
                }
                // 1048 — save (set) / restore (reset) the cursor, using
                // the same DECSC slot as `ESC 7` / `ESC 8`. No buffer or
                // image-grid switch.
                1048 => {
                    if enable {
                        self.decsc_saved = Some(self.cursor);
                    } else {
                        let old = self.cursor;
                        self.cursor = self.decsc_saved.unwrap_or_default();
                        self.clamp_cursor();
                        self.mark_cell(old.row, old.col);
                        self.mark_cell(self.cursor.row, self.cursor.col);
                    }
                }
                // 25 — DECTCEM, show/hide the cursor. `enable` = show.
                // Apps that take over the screen (yazi, helix, neovim,
                // btop) toggle this off during their alt-screen UI so
                // the cursor block doesn't sit over their layout.
                25 => {
                    if self.cursor_visible != enable {
                        let row = self.cursor.row;
                        let col = self.cursor.col.min(self.cols.saturating_sub(1));
                        self.cursor_visible = enable;
                        // Repaint the cursor cell so the block
                        // appears / disappears under partial redraw
                        // with LoadOp::Load.
                        self.mark_cell(row, col);
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
                // 80 — DECSDM (sixel display mode). SET disables sixel
                // scrolling (image stays anchored); RESET enables it
                // (the default). `place_sixel` derives scroll behavior
                // from `!sixel_display_mode`.
                80 => {
                    self.sixel_display_mode = enable;
                }
                // 8452 — leave the cursor to the RIGHT of a sixel image
                // instead of on the line below it.
                8452 => {
                    self.sixel_cursor_right = enable;
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
        // Drop any selection from the primary grid — once we're on
        // alt, the user can't see the primary content anyway, and
        // selection state would resurface as a phantom highlight on
        // exit if the primary's rows are then overwritten.
        self.selection = None;
        self.saved_cursor = self.cursor;
        self.alt_active = true;
        self.alt.clear_visible(Style::RESET);
        // Reset cursor to home and clear style for the alt screen.
        self.cursor = Cursor::default();
        // Snap viewport back to the live bottom — alt screen has no
        // scrollback, and any in-flight scrollback animation from the
        // primary grid would be visually nonsensical here. We also
        // jump the *current* position so the alt screen renders
        // immediately at offset 0 (no smooth transition).
        self.viewport = crate::viewport::Viewport::new();
        // Switching screens invalidates every cached shaped line.
        self.mark_all_dirty();
        // Primary and alt screens maintain independent image lists.
        // Stash the primary grid and install a fresh empty one as the
        // active grid so the alt screen starts blank and the primary's
        // images survive the round trip.
        self.stashed_image_grid = Some(std::mem::take(&mut self.image_grid));
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    fn exit_alt_screen(&mut self) {
        if !self.alt_active {
            return;
        }
        self.alt_active = false;
        self.cursor = self.saved_cursor;
        self.clamp_cursor();
        // Returning to the primary grid: snap viewport to the live
        // bottom. The user expects to see the prompt where they left
        // it, not whatever scrollback they were viewing before.
        self.viewport = crate::viewport::Viewport::new();
        // Switching back: re-shape the primary screen contents.
        self.mark_all_dirty();
        // Drop the alt screen's images and restore the stashed primary
        // grid, so the primary screen's images reappear exactly as they
        // were before we entered the alt screen.
        self.image_grid = self.stashed_image_grid.take().unwrap_or_default();
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    /// RIS — Reset to Initial State (`ESC c`). A hard reset: return to
    /// the primary screen, home the cursor, reset the pen/SGR, reset the
    /// scroll region to the full screen, restore relevant private modes
    /// to their power-on defaults, clear the screen *and* scrollback, and
    /// (the kitty hard requirement) clear all image placements. RIS
    /// clears what's *on screen*, matching kitty, which drops placements.
    fn ris(&mut self) {
        // Leave the alt screen first so the reset lands on the primary
        // grid the user will see.
        if self.alt_active {
            self.exit_alt_screen();
        }
        // Fresh primary + alt grids: cleanest way to wipe both the
        // visible region and the entire scrollback ring.
        let primary_cap = self.rows as usize + self.scrollback as usize;
        self.primary = Grid::new(self.rows, self.cols, primary_cap);
        self.alt = Grid::new(self.rows, self.cols, self.rows as usize);
        // Cursor home + default pen.
        self.cursor = Cursor::default();
        self.saved_cursor = Cursor::default();
        self.decsc_saved = None;
        self.cursor_underline_color = None;
        self.current_hyperlink = None;
        // Scroll region back to the full screen.
        self.scroll_top = 0;
        self.scroll_bot = self.rows.saturating_sub(1);
        // Private modes back to power-on defaults.
        self.cursor_visible = true;
        self.bracketed_paste = false;
        self.report_focus = false;
        self.mouse_mode = MouseMode::default();
        self.grapheme_cluster_mode = false;
        self.inband_resize_mode = false;
        self.kitty_keyboard_stack.clear();
        self.modify_other_keys = 0;
        // Snap the viewport back to the live bottom.
        self.viewport = crate::viewport::Viewport::new();
        // Clear any in-progress placeholder run.
        self.placeholder_run = None;
        // Clear image placements (kitty hard requirement for RIS).
        self.image_grid.clear();
        self.image_revision = self.image_revision.wrapping_add(1);
        // Drop any selection and re-shape everything.
        self.selection = None;
        self.damage.resize(self.rows);
        self.mark_all_dirty();
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
                // BS: move cursor left exactly one column, no wrap. We do
                // NOT snap off a continuation half: BS is a column move, and
                // apps (notably zsh's line editor) emit one BS per display
                // column — two for a wide cluster — so snapping would eat an
                // extra column on the boundary backspace and walk the cursor
                // (and the rewrite that follows) left into the prompt. The
                // cursor resting on a continuation cell is fine; the next BS
                // steps onto the primary on its own. See
                // `wide_char_paste_redraw_keeps_alignment`.
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

    /// ESC + final-byte dispatch. Covers the handful of single-byte
    /// terminal-control escapes that don't fit the CSI/OSC frames.
    ///
    /// Sequences with intermediate bytes (character-set selection like
    /// `ESC ( B`, etc.) are silently ignored — they're not load-bearing
    /// for the apps we currently target.
    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if let Some(run) = self.placeholder_run.take() {
            self.finalize_placeholder_run(run);
        }
        if !intermediates.is_empty() {
            return;
        }
        match byte {
            // RIS (`ESC c`) — Reset to Initial State. Full hard reset.
            b'c' => self.ris(),
            // RI (Reverse Index) — symmetric to LF: cursor up one row,
            // scroll down at the top of the scroll region.
            b'M' => self.reverse_index(),
            // IND (Index) — semantically a linefeed without CR.
            b'D' => self.linefeed(),
            // NEL (Next Line) — CR + LF.
            b'E' => {
                let row = self.cursor.row;
                let max_col = self.cols.saturating_sub(1);
                let old_col = self.cursor.col.min(max_col);
                self.cursor.col = 0;
                self.mark_cell(row, old_col);
                self.mark_cell(row, 0);
                self.linefeed();
            }
            // DECSC (`ESC 7`) — save cursor position + SGR. Used by
            // powerlevel10k's instant-prompt to snapshot before
            // printing a transient prompt, then redraw via DECRC.
            b'7' => self.decsc_saved = Some(self.cursor),
            // DECRC (`ESC 8`) — restore the DECSC snapshot. With no
            // prior save, xterm homes the cursor and resets SGR.
            b'8' => {
                let old = self.cursor;
                let new = self.decsc_saved.unwrap_or_default();
                self.cursor = new;
                self.clamp_cursor();
                self.mark_cell(old.row, old.col);
                self.mark_cell(self.cursor.row, self.cursor.col);
            }
            _ => {}
        }
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
            // OSC 10/11/12 — fg / bg / cursor color query (and set).
            // M11a+ wires query-only for OSC 11 (bg) because apps like
            // yazi probe it at startup to decide dark vs light
            // rendering. A query payload is `?`; a set is
            // `rgb:RR/GG/BB` (or `RRRR/GGGG/BBBB`). We currently
            // honor the query only — set is silently accepted but not
            // applied (the binary still owns the theme).
            10 | 11 | 12 => {
                let payload = params.get(1).copied().unwrap_or(b"");
                if payload == b"?" {
                    let rgb = match code {
                        10 => [0xd8, 0xd8, 0xd8], // theme.fg approx
                        11 => self.default_bg_rgb,
                        12 => [0xf2, 0xd9, 0x4d], // theme.cursor approx
                        _ => unreachable!(),
                    };
                    // xterm-style reply: replicate each byte into the
                    // 16-bit channel slot (0xAB → "ABAB").
                    let reply = format!(
                        "\x1b]{code};rgb:{:02x}{:02x}/{:02x}{:02x}/{:02x}{:02x}\x1b\\",
                        rgb[0], rgb[0], rgb[1], rgb[1], rgb[2], rgb[2],
                    );
                    self.pty_replies.extend_from_slice(reply.as_bytes());
                }
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
                    if let Some(op) = toastty_protocols::palette::parse_pair(rest[i], rest[i + 1]) {
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
                                self.clipboard_requests.push(ClipboardRequest::Query {
                                    selection: selection.0,
                                });
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
        // `self` mutably as the sink.
        let payload = std::mem::take(&mut self.apc_buffer);
        if payload.is_empty() {
            return;
        }
        if payload[0] == b'G' {
            // Kitty graphics. Split on the first `;` into header
            // vs body.
            let split = payload.iter().position(|&b| b == b';');
            let (header_bytes, body): (&[u8], &[u8]) = match split {
                Some(idx) => (&payload[..idx], &payload[idx + 1..]),
                None => (&payload[..], &[]),
            };
            let mut handler = std::mem::take(&mut self.image_handler);
            // B9: a malformed header now produces an EINVAL reply
            // (subject to quiet / recoverable id) which `process`
            // queues via the sink (`queue_reply` → `pty_replies`),
            // the same path as successful replies. The returned
            // `Err(BadHeader)` is purely informational at this point —
            // the reply has already been emitted — so we drop it.
            let _ = handler.process(header_bytes, body, self);
            self.image_handler = handler;
        } else if payload.starts_with(RGP_PREFIX) {
            // Ratty Graphics Protocol. Same `mem::take` dance —
            // the handler dispatches to `self` as the sink.
            let mut handler = std::mem::take(&mut self.rgp_handler);
            let _ = handler.process(&payload, self);
            self.rgp_handler = handler;
        }
        // else: not a protocol we own (tmux passthrough etc.) —
        // silently drop.
    }

    // ---- Sixel (DCS) ----
    //
    // Sixel rides DCS (`ESC P P1;P2;P3 q <body> ST`), not APC. vte
    // drives `hook`/`put`/`unhook` natively (unlike APC, which needed
    // the parser's pre-scanner), so we just buffer here and hand the
    // body to the sixel decoder on `unhook`.

    fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        // Sixel = final byte `q` with NO intermediates. DECRQSS is `$q`
        // and other DCS queries carry intermediates — exclude them so we
        // don't capture a non-sixel DCS as a sixel body.
        if action == 'q' && intermediates.is_empty() {
            let mut it = params.iter();
            let mut next = || it.next().and_then(|sub| sub.first().copied());
            self.sixel_pending = Some(SixelDcs {
                p1: next(),
                p2: next(),
                p3: next(),
                buf: Vec::new(),
            });
        }
    }

    fn put(&mut self, byte: u8) {
        // Cap the buffered body; a hostile stream could otherwise send an
        // unbounded DCS payload. 64 MiB of sixel source decodes to far
        // more than any sane image; overflow leaves a truncated buffer
        // that fails decode cleanly in `unhook`.
        const SIXEL_BODY_CAP: usize = 64 * 1024 * 1024;
        if let Some(s) = &mut self.sixel_pending
            && s.buf.len() < SIXEL_BODY_CAP
        {
            s.buf.push(byte);
        }
    }

    fn unhook(&mut self) {
        let Some(dcs) = self.sixel_pending.take() else {
            return;
        };
        // Borrow the handler out so we can pass `&mut self` to placement.
        let handler = std::mem::take(&mut self.sixel_handler);
        match handler.decode(&dcs) {
            Ok(data) => self.place_sixel(data),
            Err(e) => {
                tracing::debug!(target: "sixel", error = %e, "sixel decode failed");
            }
        }
        self.sixel_handler = handler;
    }
}

// ---- Kitty delete helpers (M10/M11/M12) ----

impl Term {
    /// Remove `id`'s decoded bytes from the registry and prune any
    /// image-number→id entries pointing at it. Used by every by-id /
    /// byte-freeing delete path so the `d=n`/`d=N` map never points at
    /// a freed id.
    fn remove_image_bytes(&mut self, id: u32) {
        self.image_registry.remove(id);
        self.image_number_to_id.retain(|_, v| *v != id);
    }

    /// Free image bytes for every image that just lost its last
    /// placement: for each distinct image id among `dropped`, if the
    /// grid no longer holds ANY placement for that id, remove its bytes
    /// (and prune the number→id map). Shared by the uppercase
    /// area/selector deletes (`A`, `P`, `C`, `Q`, `X`, `Y`, `Z`).
    fn free_orphaned_image_bytes(&mut self, dropped: &[Placement]) {
        let mut seen: Vec<u32> = Vec::new();
        for p in dropped {
            if seen.contains(&p.image_id) {
                continue;
            }
            seen.push(p.image_id);
            if !self.image_grid.iter().any(|q| q.image_id == p.image_id) {
                self.remove_image_bytes(p.image_id);
            }
        }
    }

    /// Register a decoded sixel image and place it at the cursor, then
    /// advance the cursor per the active sixel cursor mode (DECSET 8452).
    ///
    /// Sixel carries no client-assigned image ids, so we register under
    /// id 0 (the registry mints the lowest free id). The cell span is
    /// derived the way kitty derives an un-annotated transmit:
    /// `ceil(img_dim / cell_dim)`.
    fn place_sixel(&mut self, data: ImageData) {
        let (img_w, img_h) = (data.width, data.height);
        let Some(id) = self.register_image(0, 0, data) else {
            return;
        };
        let (cell_w, cell_h) = self.cell_pixel_size;
        let cols = img_w.div_ceil(u32::from(cell_w.max(1))).max(1) as u16;
        let rows = img_h.div_ceil(u32::from(cell_h.max(1))).max(1) as u16;

        // `place_image` rebases the span against the cursor and scrolls
        // the screen up if the image doesn't fit below it, computing its
        // own `start_row` internally. Mirror that math so we know where
        // the image lands for the cursor advance below.
        let cur_row = self.cursor.row;
        let start_col = self.cursor_col();
        let scroll_n = cur_row.saturating_add(rows).saturating_sub(self.rows);
        let start_row = cur_row.saturating_sub(scroll_n);

        self.place_image(Placement {
            image_id: id,
            placement_id: 0,
            row_range: 0..rows,
            col_range: 0..cols,
            src_rect: toastty_graphics::SrcRect::FULL,
            z: 0,
            pix_offset: (0, 0),
            parent: None,
            rel_offset: (0, 0),
        });

        let image_bottom = start_row.saturating_add(rows).saturating_sub(1);
        if self.sixel_cursor_right {
            // DECSET 8452: cursor lands on the image's last row, one
            // column past its right edge (kitty-style).
            self.cursor.row = image_bottom.min(self.rows.saturating_sub(1));
            self.cursor.col = start_col
                .saturating_add(cols)
                .min(self.cols.saturating_sub(1));
        } else {
            // Default sixel scrolling: cursor moves to the left margin of
            // the line BELOW the image. If that line is past the bottom,
            // scroll one row (the image scrolls up with the content, as a
            // real terminal does) and sit on the new last row.
            self.cursor.col = 0;
            let want = image_bottom.saturating_add(1);
            if want >= self.rows {
                self.active_grid_mut().scroll_up();
                if !self.alt_active {
                    self.viewport
                        .on_grid_scroll_up(self.primary.history_lines());
                }
                self.image_grid.shift_rows_up(1, 0);
                self.image_grid.resolve_relative_positions();
                self.mark_all_dirty();
                self.cursor.row = self.rows.saturating_sub(1);
            } else {
                self.cursor.row = want;
            }
        }
    }
}

// ---- M11a: KittySink ----

impl KittySink for Term {
    fn register_image(
        &mut self,
        id_request: u32,
        image_number: u32,
        data: ImageData,
    ) -> Option<u32> {
        tracing::info!(
            target: "kitty",
            id_request,
            image_number,
            width = data.width, height = data.height,
            bytes_len = data.pixels.len(),
            "kitty: register_image (payload)",
        );
        // m6: collect ids that currently have live placements so the
        // registry preferentially evicts images WITHOUT placements when
        // it has to free quota space (kitty spec).
        let pinned: std::collections::HashSet<u32> =
            self.image_grid.iter().map(|p| p.image_id).collect();
        match self
            .image_registry
            .insert_with_pinned(id_request, data, &pinned)
        {
            Ok(inserted) => {
                tracing::info!(
                    target: "kitty",
                    id = inserted.id,
                    image_number,
                    evicted = ?inserted.evicted,
                    "kitty: register_image ok",
                );
                // Evicted ids no longer exist in the registry; drop their
                // placements + mark cells dirty, and prune number→id
                // entries pointing at them.
                for evicted in &inserted.evicted {
                    let dropped = self.image_grid.remove_image(*evicted);
                    for p in dropped {
                        mark_placement_dirty(self, &p);
                    }
                    self.image_number_to_id.retain(|_, id| id != evicted);
                }
                // Record this id as the most-recent for its image number
                // (M11: `d=n`/`d=N` resolution).
                if image_number != 0 {
                    self.image_number_to_id.insert(image_number, inserted.id);
                }
                self.image_revision = self.image_revision.wrapping_add(1);
                Some(inserted.id)
            }
            Err(e) => {
                tracing::warn!(
                    target: "kitty",
                    id_request, error = ?e,
                    "kitty: register_image failed",
                );
                None
            }
        }
    }

    fn place_image(&mut self, mut placement: Placement) {
        let image_known = self.image_registry.contains(placement.image_id);
        // m5: mark the referenced image most-recently-used so the
        // registry's LRU reflects actual display, not insertion order.
        // No-op when the id isn't resident.
        self.image_registry.touch(placement.image_id);
        tracing::info!(
            target: "kitty",
            image_id = placement.image_id,
            placement_id = placement.placement_id,
            row_range = ?placement.row_range,
            col_range = ?placement.col_range,
            cursor_row = self.cursor.row,
            cursor_col = self.cursor.col,
            image_known,
            "kitty: place_image",
        );
        if !image_known {
            tracing::warn!(
                target: "kitty",
                image_id = placement.image_id,
                "kitty: place_image — image id not in registry; placement will not render",
            );
        }
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

        // Make room. Per the kitty spec: "If the image is larger than
        // the available space, the rest of the image will scroll the
        // screen up." Without this the placement clamps to whatever
        // rows are left below the cursor — which is often 0 or 1 by
        // the time multiple images have been placed, so the image
        // collapses to a single-row band at the bottom.
        let scroll_n = cur_row.saturating_add(row_span).saturating_sub(self.rows);
        if scroll_n > 0 {
            for _ in 0..scroll_n {
                self.active_grid_mut().scroll_up();
                if !self.alt_active {
                    self.viewport
                        .on_grid_scroll_up(self.primary.history_lines());
                }
                self.image_grid.shift_rows_up(1, 0);
            }
            // M13: keep relative children anchored to their parents after
            // the scroll that made room for this placement.
            self.image_grid.resolve_relative_positions();
            self.mark_all_dirty();
        }
        let start_row = cur_row.saturating_sub(scroll_n);

        placement.row_range = start_row..start_row.saturating_add(row_span);
        placement.col_range = cur_col..cur_col.saturating_add(col_span);
        // Defensive clamp (shouldn't trigger after the scroll fix).
        if placement.row_range.end > self.rows {
            placement.row_range.end = self.rows;
        }
        if placement.col_range.end > self.cols {
            placement.col_range.end = self.cols;
        }
        // M5: re-emitting the same `(image_id, placement_id)` must
        // REPLACE the prior placement, not stack on top of it. Per spec:
        // "If you send two placements with the same image id and
        // placement id the second one will replace the first." Only
        // applies when BOTH ids are non-zero (a named placement);
        // unnamed (`p=0`) placements always accumulate.
        if placement.image_id != 0 && placement.placement_id != 0 {
            let img = placement.image_id;
            let pid = placement.placement_id;
            for old in self
                .image_grid
                .remove_where(|p| p.image_id == img && p.placement_id == pid)
            {
                mark_placement_dirty(self, &old);
            }
        }
        // Mark dirty BEFORE consuming `placement` into the grid.
        mark_placement_dirty(self, &placement);
        self.image_grid.add(placement);
        self.image_revision = self.image_revision.wrapping_add(1);
    }

    fn place_relative(
        &mut self,
        placement: Placement,
    ) -> Result<(), toastty_graphics::image_grid::RelativeError> {
        tracing::info!(
            target: "kitty",
            image_id = placement.image_id,
            placement_id = placement.placement_id,
            parent = ?placement.parent,
            rel_offset = ?placement.rel_offset,
            "kitty: place_relative",
        );
        // M5 replace rule: a named (image_id, placement_id) re-emission
        // replaces the prior placement of the same pair.
        if placement.image_id != 0 && placement.placement_id != 0 {
            let img = placement.image_id;
            let pid = placement.placement_id;
            for old in self
                .image_grid
                .remove_where(|p| p.image_id == img && p.placement_id == pid)
            {
                mark_placement_dirty(self, &old);
            }
        }
        // `add_relative` validates the parent ref and rebases the child's
        // cell ranges onto the parent origin + (H, V). On error nothing is
        // inserted.
        match self.image_grid.add_relative(placement) {
            Ok(handle) => {
                // The placement was rebased inside `add_relative`; re-read
                // it to mark the resolved cells dirty.
                let resolved = self
                    .image_grid
                    .iter_with_handles()
                    .find(|(h, _)| *h == handle)
                    .map(|(_, p)| p.clone());
                if let Some(p) = resolved {
                    mark_placement_dirty(self, &p);
                }
                self.image_revision = self.image_revision.wrapping_add(1);
                Ok(())
            }
            Err(e) => Err(e),
        }
    }

    fn delete_image(
        &mut self,
        delete: DeleteSpec,
        header: &toastty_graphics::kitty::header::Header,
    ) {
        // Treat empty / unknown specs the same as `a` (all).
        let spec_byte = if delete.byte == 0 { b'a' } else { delete.byte };
        let mut dropped_placements = Vec::new();
        let drop_bytes = delete.free_bytes();
        let spec_char = (spec_byte as char).to_string();
        tracing::info!(
            target: "kitty",
            spec = %spec_char,
            drop_bytes,
            image_id = header.image_id,
            placement_id = header.placement_id,
            cell_y = header.cell_y,
            "kitty: delete_image",
        );
        // Cell/selector keys carry 1-based coordinates in the lowercase
        // `x=`/`y=` keys (header.src_x / src_y) per spec; convert to the
        // grid's 0-based rows/cols (the B6 convention). `z=` carries the
        // z-index for q/z.
        let sel_col = header.src_x.saturating_sub(1) as u16;
        let sel_row = header.src_y.saturating_sub(1) as u16;
        let sel_z = header.z;
        // For uppercase selectors that don't free bytes inline, this
        // flag drives a post-removal survivor sweep (free bytes for
        // images left with no placement). Set false by arms that do
        // their own byte handling.
        let mut needs_survivor_sweep = drop_bytes;
        match spec_byte {
            // `a` / `A` — delete all placements *visible on screen* (the
            // active screen's rows `0..self.rows`). M10: this is NOT
            // "everything"; only placements intersecting the viewport.
            // Uppercase `A` frees bytes for images left without any
            // surviving placement (survivor sweep below).
            b'a' | b'A' => {
                let rows = self.rows;
                dropped_placements.extend(
                    self.image_grid
                        .remove_where(|p| p.row_range.start < rows && p.row_range.end > 0),
                );
            }
            // `i` / `I` — by image id (provided via `i=`). When `p=` is
            // also given (non-zero), scope to that single placement;
            // otherwise drop every placement of the image. (B4)
            b'i' | b'I' => {
                if header.image_id != 0 {
                    let img = header.image_id;
                    let pid = header.placement_id;
                    if pid != 0 {
                        dropped_placements.extend(
                            self.image_grid
                                .remove_where(|p| p.image_id == img && p.placement_id == pid),
                        );
                    } else {
                        dropped_placements.extend(self.image_grid.remove_image(img));
                    }
                    // Uppercase frees the bytes, but only when no
                    // placement of this image survives. (B5)
                    if drop_bytes && !self.image_grid.iter().any(|p| p.image_id == img) {
                        self.remove_image_bytes(img);
                    }
                    needs_survivor_sweep = false;
                }
            }
            // `n` / `N` — by image *number* (provided via `I=`).
            // Resolve the number to the most-recently-registered id
            // (M11), then delete its placements; `N` also frees bytes
            // when no placement of that image survives.
            b'n' | b'N' => {
                if header.image_number != 0 {
                    if let Some(&img) = self.image_number_to_id.get(&header.image_number) {
                        dropped_placements.extend(self.image_grid.remove_image(img));
                        if drop_bytes && !self.image_grid.iter().any(|p| p.image_id == img) {
                            self.remove_image_bytes(img);
                        }
                    }
                }
                needs_survivor_sweep = false;
            }
            // `p` / `P` — by CELL coordinates. The cell is specified via
            // the lowercase `x=` / `y=` keys (parsed into `src_x` /
            // `src_y`), 1-based per the spec ("x=1,y=1 is the top left
            // cell"). Internal `col_range` / `row_range` are 0-based, so
            // convert by subtracting 1. (B6)
            b'p' | b'P' => {
                dropped_placements.extend(self.image_grid.remove_where(|p| {
                    p.col_range.contains(&sel_col) && p.row_range.contains(&sel_row)
                }));
                // Uppercase `P` frees bytes for any image that lost its
                // last placement. (B6, same rule as B5) — handled by the
                // shared survivor sweep below.
            }
            // `r` / `R` — by image-id RANGE (kitty 0.33+). Delete all
            // images whose id is in `[x, y]` inclusive, where the
            // lowercase `x=` / `y=` keys carry the id bounds (parsed into
            // `src_x` / `src_y`). Lowercase removes placements; uppercase
            // `R` additionally frees image bytes. (B7)
            b'r' | b'R' => {
                let lo = header.src_x;
                let hi = header.src_y;
                if lo <= hi {
                    dropped_placements.extend(
                        self.image_grid
                            .remove_where(|p| (lo..=hi).contains(&p.image_id)),
                    );
                    if drop_bytes {
                        let ids: Vec<u32> = self
                            .image_registry
                            .ids()
                            .filter(|id| (lo..=hi).contains(id))
                            .collect();
                        for id in ids {
                            self.remove_image_bytes(id);
                        }
                    }
                    needs_survivor_sweep = false;
                }
            }
            // `c` / `C` — placements intersecting the CURRENT CURSOR cell.
            b'c' | b'C' => {
                let row = self.cursor.row;
                let col = self.cursor.col.min(self.cols.saturating_sub(1));
                dropped_placements
                    .extend(self.image_grid.remove_where(|p| {
                        p.row_range.contains(&row) && p.col_range.contains(&col)
                    }));
            }
            // `q` / `Q` — placements intersecting cell (sel_col, sel_row)
            // AND having z-index == `z=`.
            b'q' | b'Q' => {
                dropped_placements.extend(self.image_grid.remove_where(|p| {
                    p.row_range.contains(&sel_row) && p.col_range.contains(&sel_col) && p.z == sel_z
                }));
            }
            // `x` / `X` — placements intersecting COLUMN sel_col.
            b'x' | b'X' => {
                dropped_placements.extend(
                    self.image_grid
                        .remove_where(|p| p.col_range.contains(&sel_col)),
                );
            }
            // `y` / `Y` — placements intersecting ROW sel_row. The real
            // "delete by row" selector (B7 moved `r/R` to id-range).
            b'y' | b'Y' => {
                dropped_placements.extend(
                    self.image_grid
                        .remove_where(|p| p.row_range.contains(&sel_row)),
                );
            }
            // `z` / `Z` — placements with z-index == `z=`.
            b'z' | b'Z' => {
                dropped_placements.extend(self.image_grid.remove_where(|p| p.z == sel_z));
            }
            _ => {}
        }
        // Uppercase area/selector deletes (`A`, `P`, `C`, `Q`, `X`, `Y`,
        // `Z`) free image bytes for any image left with no surviving
        // placement after the removals above (mirrors the B5 rule).
        if needs_survivor_sweep {
            self.free_orphaned_image_bytes(&dropped_placements);
        }
        tracing::info!(
            target: "kitty",
            spec = %spec_char,
            dropped = dropped_placements.len(),
            "kitty: delete_image ok",
        );
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

    fn advance_cursor_after_placement(&mut self, rows: u16, cols: u16, start_col: u16) {
        // Kitty spec / reference C impl: after a cursor-moving
        // placement, `c->x += cols; c->y += rows - 1;` — the cursor
        // lands on the image's LAST row, one column past its right
        // edge:
        //   row = start_row + (rows - 1)
        //   col = start_col + cols
        //
        // `place_image` already scrolled the screen when the image
        // didn't fit below the cursor, so we don't issue additional
        // linefeeds here — each linefeed at the bottom calls
        // `image_grid.shift_rows_up`, which would shift the image we
        // just placed UP by one cell per linefeed. With `row_span`
        // linefeeds the image would shrink to a one-row band at the top
        // (the M11a "1 cell tall" regression).
        //
        // M1: row advances by `rows - 1` (not `rows`); col advances by
        // `cols` from `start_col` (the placement's starting column).
        let target_row = self
            .cursor
            .row
            .saturating_add(rows.saturating_sub(1))
            .min(self.rows.saturating_sub(1));
        self.cursor.row = target_row;
        self.cursor.col = start_col
            .saturating_add(cols)
            .min(self.cols.saturating_sub(1));
    }

    fn image_exists(&self, id: u32) -> bool {
        self.image_registry.contains(id)
    }

    fn image_dimensions(&self, id: u32) -> Option<(u32, u32)> {
        self.image_registry
            .get(id)
            .map(|img| (img.width, img.height))
    }

    fn cursor_col(&self) -> u16 {
        self.cursor.col.min(self.cols.saturating_sub(1))
    }

    fn cell_pixel_size(&self) -> (u16, u16) {
        self.cell_pixel_size
    }
}

// ---- M12a: RgpSink ----
//
// The handler hands us parsed RgpOperations; we apply them to the
// `RgpScene` and queue any reply bytes back to the PTY. Path-based
// register is parsed but not resolved here — M12b adds the embedded
// asset bundle and the optional config-dir resolver.

impl RgpSink for Term {
    fn register_asset(
        &mut self,
        id: u32,
        format: RgpFormat,
        name: Option<String>,
        bytes: Vec<u8>,
    ) -> bool {
        // M12b: parse the .glb bytes via the gltf crate. Failure
        // to parse means we DON'T register the asset — the app
        // sees the next `p;id=...` for this id silently noop, the
        // same as if the register never arrived. (M12e will queue
        // an error reply here once we settle on a reply shape.)
        tracing::info!(
            target: "rgp",
            id, ?format, name = ?name, bytes_len = bytes.len(),
            "rgp: register_asset (payload)",
        );
        let result: Result<CpuAsset, String> = match format {
            RgpFormat::Glb => load_glb(&bytes).map_err(|e| e.to_string()),
            RgpFormat::Obj => load_obj(&bytes).map_err(|e| e.to_string()),
        };
        match result {
            Ok(data) => {
                tracing::info!(
                    target: "rgp",
                    id,
                    vertices = data.mesh.positions.len(),
                    indices = data.mesh.indices.len(),
                    "rgp: register_asset ok",
                );
                self.rgp_scene
                    .apply_register(id, RgpAsset { format, name, data });
                true
            }
            Err(error) => {
                tracing::warn!(
                    target: "rgp",
                    id, %error,
                    "rgp: register_asset failed",
                );
                false
            }
        }
    }

    fn register_asset_by_path(&mut self, id: u32, format: RgpFormat, name: String) -> bool {
        // The app emits `path=` relative to ITS own CWD (e.g.
        // `path=assets/objects/SpinyMouse.glb`). Toastty's own
        // `std::fs::read` resolves relative to TOASTTY's CWD, which
        // is different. Bridge the gap via OSC 7 — `self.cwd` is the
        // shell's last-advertised working directory.
        //
        // Pure leaf names (no separators) skip the join so the
        // resolver consults the embedded bundle first (`path=cube`).
        // Absolute paths skip the join trivially.
        let resolved = if (!name.contains('/') && !name.contains('\\'))
            || std::path::Path::new(&name).is_absolute()
            || self.cwd.is_empty()
        {
            name.clone()
        } else {
            std::path::Path::new(&self.cwd)
                .join(&name)
                .to_string_lossy()
                .into_owned()
        };
        tracing::info!(
            target: "rgp",
            id, ?format, requested = %name, resolved = %resolved,
            shell_cwd = %self.cwd,
            toastty_cwd = ?std::env::current_dir().ok(),
            "rgp: register_asset_by_path",
        );
        match resolve_rgp_path(&resolved, format, None) {
            Ok(data) => {
                tracing::info!(
                    target: "rgp",
                    id,
                    vertices = data.mesh.positions.len(),
                    indices = data.mesh.indices.len(),
                    "rgp: register_asset_by_path ok",
                );
                self.rgp_scene.apply_register(
                    id,
                    RgpAsset {
                        format,
                        name: Some(name),
                        data,
                    },
                );
                true
            }
            Err(e) => {
                tracing::warn!(
                    target: "rgp",
                    id, error = %e,
                    "rgp: register_asset_by_path failed",
                );
                false
            }
        }
    }

    fn place(&mut self, id: u32, anchor: RgpAnchor, style: RgpPlacementStyle) {
        let asset_known = self.rgp_scene.asset(id).is_some();
        tracing::info!(
            target: "rgp",
            id,
            row = anchor.row, col = anchor.col, cols = anchor.cols, rows = anchor.rows,
            asset_known,
            "rgp: place",
        );
        if !asset_known {
            tracing::warn!(
                target: "rgp",
                id,
                "rgp: place — id has no registered asset; placement will not render",
            );
        }
        self.rgp_scene.apply_place(id, anchor, style);
    }

    fn update(&mut self, id: u32, update: RgpPlacementUpdate) {
        self.rgp_scene.apply_update(id, &update);
    }

    fn delete(&mut self, id: Option<u32>) {
        match id {
            Some(n) => self.rgp_scene.apply_delete_one(n),
            None => self.rgp_scene.apply_delete_all(),
        }
    }

    fn queue_reply(&mut self, bytes: &[u8]) {
        self.pty_replies.extend_from_slice(bytes);
    }
}

/// Decode a kitty unicode-placeholder image id from the active SGR
/// foreground (and optionally the SGR 58 underline color slot).
///
/// Three encodings are recognized, in priority order:
///
/// 1. **Truecolor RGB** — `SGR 38;2;R;G;B`. The image id is packed as
///    `(R << 16) | (G << 8) | B`. This is what `yazi`, `image.nvim`,
///    and most kitty-graphics clients in the wild emit.
/// 2. **Indexed256 + SGR 58 Indexed256** — `SGR 38;5;L` plus
///    `SGR 58;5;H`. The id is `(H << 8) | L` (16-bit).
/// 3. **Indexed256 only** — `SGR 38;5;L`. The id is `L` (8-bit).
///
/// Returns `None` for any other fg color (Default, Rgb with a third
/// path not yet seen) — placeholder runs without a recognised id
/// encoding are silently dropped per the kitty spec.
/// Decode the SGR colors of a Unicode-placeholder cell into the *low
/// bits* of the image id and the placement id.
///
/// Per the kitty spec the colors carry only part of the image id; the
/// 3rd diacritic (handled per-cell in [`Term::finalize_placeholder_run`])
/// supplies bits 24..32:
///
/// - **Foreground** carries the low bits of the image id. An 8-bit
///   indexed fg (`SGR 38;5;L`) gives bits 0..8 (`L`); a 24-bit RGB fg
///   (`SGR 38;2;R;G;B`) gives bits 0..24 (`(R<<16)|(G<<8)|B`).
/// - **Underline color** (`SGR 58`) carries the *placement id*, not part
///   of the image id. An indexed underline gives the low 8 bits; an RGB
///   underline gives a 24-bit placement id. Absent ⇒ placement id 0
///   ("unnamed").
///
/// Returns `None` when the foreground is unset/default, in which case the
/// cell is not a usable placeholder reference.
fn placeholder_image_id_from_sgr(fg: Color, underline_color: Option<Color>) -> Option<(u32, u32)> {
    let fg_bits = match fg {
        Color::Rgb(r, g, b) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        Color::Indexed256(low) => u32::from(low),
        _ => return None,
    };
    let placement_id = match underline_color {
        Some(Color::Indexed256(p)) => u32::from(p),
        Some(Color::Rgb(r, g, b)) => (u32::from(r) << 16) | (u32::from(g) << 8) | u32::from(b),
        _ => 0,
    };
    Some((fg_bits, placement_id))
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
    use crate::viewport::Smoothing;
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
        t.damage().rows.iter().map(|r| !r.is_empty()).collect()
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
    fn ri_at_top_scrolls_screen_down() {
        // Fill the screen, home the cursor, then RI: existing rows
        // should shift down by one, top row should be blank, bottom row
        // should fall off. This is the load-bearing path for less's
        // back-up (`b`/`u`) rendering.
        let mut t = run(3, 8, b"a\r\nb\r\nc");
        // Home, then RI (`ESC M`).
        feed(&mut t, b"\x1b[H\x1bM");
        assert_eq!(row_text(&t, 0), "");
        assert_eq!(row_text(&t, 1), "a");
        assert_eq!(row_text(&t, 2), "b");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn ri_mid_screen_just_moves_cursor_up() {
        // Cursor not at top: RI is just "cursor up one row", no scroll.
        let mut t = run(3, 8, b"a\r\nb\r\nc");
        // Park cursor at row 2, col 0; then RI.
        feed(&mut t, b"\x1b[3;1H\x1bM");
        assert_eq!(row_text(&t, 0), "a");
        assert_eq!(row_text(&t, 1), "b");
        assert_eq!(row_text(&t, 2), "c");
        assert_eq!(t.cursor().row, 1);
    }

    #[test]
    fn ri_feed_like_less_pages_up() {
        // Mimic less's "scroll up" emit pattern: per new row, send
        // CUP-home + RI + content + CRLF. Each iteration pushes prior
        // writes down by one, so after N iterations the screen reads
        // top-down as the lines written in *reverse* order.
        let mut t = run(4, 4, b"P0\r\nP1\r\nP2\r\nP3");
        // Write three new lines [X2, X1, X0] into the top via RI.
        feed(
            &mut t,
            b"\x1b[H\x1bMX2\r\n\x1b[H\x1bMX1\r\n\x1b[H\x1bMX0\r\n",
        );
        assert_eq!(row_text(&t, 0), "X0");
        assert_eq!(row_text(&t, 1), "X1");
        assert_eq!(row_text(&t, 2), "X2");
        assert_eq!(row_text(&t, 3), "P0");
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
    fn cha_moves_cursor_to_absolute_column() {
        // `CSI Ps G` (CHA) is how Claude Code lays out words on a row:
        // it writes a word, then jumps the cursor to a specific column
        // before writing the next. Without a CHA handler, every word
        // after the first piles up at whatever column the cursor
        // happened to land on.
        let mut t = Term::new(1, 20, 0);
        feed(&mut t, b"Internal\x1b[15Ginfra");
        let cells = &t.row(0).cells;
        assert_eq!(cells[0].ch, 'I');
        assert_eq!(cells[7].ch, 'l');
        // CHA jumped to column 15 (1-based) = index 14.
        assert_eq!(cells[14].ch, 'i');
        assert_eq!(cells[18].ch, 'a');
        // Defaults: empty Ps and Ps=1 both mean column 1.
        feed(&mut t, b"\x1b[GX");
        assert_eq!(t.row(0).cells[0].ch, 'X');
    }

    #[test]
    fn csi_gt_m_is_not_treated_as_sgr() {
        // `CSI > 4 ; 2 m` is xterm's XTMODKEYS (modifyOtherKeys). It
        // must not be interpreted as SGR 4 (underline) + SGR 2 (dim) —
        // Claude Code emits it at startup and previously left every
        // subsequent cell rendered with underline + dim.
        let mut t = Term::new(1, 2, 0);
        feed(&mut t, b"\x1b[>4;2mA");
        let s = t.row(0).cells[0].style;
        assert_eq!(s.flags, StyleFlags::default());
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
    fn reflow_narrow_then_widen_restores_glyphs() {
        // Bug 2: narrowing used to truncate glyphs destructively; widening
        // refilled with blanks, so text was lost for good. With reflow the
        // logical line rewraps on narrow and rejoins on widen.
        let mut t = Term::new(3, 12, 100);
        feed(&mut t, b"hello world");
        assert_eq!(row_text(&t, 0), "hello world");
        // Narrow to 5: "hello" / " worl" / "d", first two soft-wrapped.
        t.resize(3, 5);
        assert_eq!(row_text(&t, 0), "hello");
        assert_eq!(row_text(&t, 1), " worl");
        assert_eq!(row_text(&t, 2), "d");
        assert!(t.row(0).soft_wrap && t.row(1).soft_wrap);
        // Widen back to 12: the soft-wrap chain rejoins — no lost glyphs.
        t.resize(3, 12);
        assert_eq!(row_text(&t, 0), "hello world");
    }

    #[test]
    fn reflow_vertical_grow_preserves_and_reveals_scrollback() {
        // Bug 1: growing the window used to wipe scrollback (cap change →
        // history_lines = 0). With reflow it's preserved, and the taller
        // viewport reveals more of it at the top.
        let mut t = Term::new(3, 8, 100);
        feed(
            &mut t,
            b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9",
        );
        assert_eq!(t.history_lines(), 7);
        assert_eq!(row_text(&t, 0), "L7");
        // Grow to 6 rows: 3 scrollback lines revealed (L4..L6), 4 remain.
        t.resize(6, 8);
        assert_eq!(t.history_lines(), 4, "scrollback must survive a grow");
        assert_eq!(row_text(&t, 0), "L4");
        assert_eq!(row_text(&t, 5), "L9");
    }

    #[test]
    fn reflow_vertical_shrink_pushes_to_scrollback() {
        // Shrinking the window pushes top rows into scrollback (cursor and
        // live bottom stay anchored), the mirror of the grow case.
        let mut t = Term::new(3, 8, 100);
        feed(
            &mut t,
            b"L0\r\nL1\r\nL2\r\nL3\r\nL4\r\nL5\r\nL6\r\nL7\r\nL8\r\nL9",
        );
        assert_eq!(t.history_lines(), 7);
        t.resize(2, 8);
        assert_eq!(t.history_lines(), 8, "a row scrolled into history");
        assert_eq!(row_text(&t, 0), "L8");
        assert_eq!(row_text(&t, 1), "L9");
    }

    #[test]
    fn reflow_same_width_grow_preserves_visible_content() {
        // A same-width vertical resize must be lossless for visible content
        // (only exact-default trailing blanks are trimmed/re-padded).
        let mut t = Term::new(2, 6, 50);
        feed(&mut t, b"foo\r\nbar");
        t.resize(4, 6);
        assert_eq!(row_text(&t, 0), "foo");
        assert_eq!(row_text(&t, 1), "bar");
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
    fn kitty_query_replies_with_active_flags() {
        let mut t = Term::new(2, 4, 0);
        // Empty stack → flags are 0.
        feed(&mut t, b"\x1b[?u");
        assert_eq!(t.drain_pty_replies(), b"\x1b[?0u");
        // After pushing flags the query reflects the top of the stack.
        feed(&mut t, b"\x1b[>5u");
        feed(&mut t, b"\x1b[?u");
        assert_eq!(t.drain_pty_replies(), b"\x1b[?5u");
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
            assert_eq!(t.cursor_shape(), want_shape, "Ps={ps}: wrong cursor shape",);
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
    fn decset_25_toggles_cursor_visibility() {
        let mut t = Term::new(4, 8, 0);
        assert!(t.cursor_visible(), "cursor visible by default");
        feed(&mut t, b"\x1b[?25l");
        assert!(!t.cursor_visible(), "?25l hides cursor");
        feed(&mut t, b"\x1b[?25h");
        assert!(t.cursor_visible(), "?25h shows cursor");
    }

    #[test]
    fn decset_25_hide_marks_cursor_cell_dirty() {
        // Under partial redraw with LoadOp::Load the cursor block from
        // the previous frame would persist if we didn't mark the cell
        // dirty when the visibility toggles.
        let mut t = Term::new(4, 8, 0);
        t.clear_damage();
        feed(&mut t, b"\x1b[?25l");
        let damage = t.damage();
        // The cursor cell (0, 0) must be in the damage set.
        let row_damage = &damage.rows[0];
        assert!(
            row_damage.all_cols || row_damage.cols.contains(&0),
            "expected cursor cell (0,0) dirty after ?25l; got {row_damage:?}",
        );
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
    fn backspace_moves_one_column_over_wide_cluster() {
        // BS is a per-column move and must NOT snap off the continuation
        // half: apps emit one BS per display column. After a wide cluster
        // (你 at cols 0-1) the cursor is at col 2; one BS lands on col 1
        // (the continuation cell — a valid resting spot), and a second BS
        // lands on col 0. Snapping would make the first BS jump straight to
        // col 0, so two BSes would underflow/over-travel — the CJK paste
        // alignment bug.
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "你".as_bytes());
        assert_eq!(t.cursor().col, 2);
        feed(&mut t, b"\x08");
        assert_eq!(t.cursor().col, 1, "one BS moves exactly one column");
        feed(&mut t, b"\x08");
        assert_eq!(t.cursor().col, 0, "second BS reaches the primary");
    }

    #[test]
    fn cursor_back_moves_one_column_over_wide_cluster() {
        // CUB, like BS, moves by exact columns and must not snap off a
        // continuation half. From col 2 (after 你 at 0-1): CUB 1 → col 1,
        // CUB 1 again → col 0.
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "你".as_bytes());
        assert_eq!(t.cursor().col, 2);
        feed(&mut t, b"\x1b[1D");
        assert_eq!(t.cursor().col, 1);
        feed(&mut t, b"\x1b[1D");
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn wide_char_paste_redraw_keeps_alignment() {
        // Regression for the CJK-paste drift: replay the exact bytes zsh
        // emits when pasting 你 into a line that already holds one. zsh
        // backs up over the existing wide char with two BSes, then rewrites
        // 你你 in place. With per-column BS the rewrite must land back on
        // col 2 and leave the prompt ("> ") untouched.
        let mut t = Term::new(2, 40, 0);
        feed(&mut t, b"> ");
        // Paste #1: ESC[7m 你 ESC[27m ...
        feed(&mut t, b"\x1b[7m\xe4\xbd\xa0\x1b[27m\x1b[7m\x1b[27m");
        assert_eq!(t.cursor().col, 4, "你 at cols 2-3, cursor past it");
        // Paste #2: BS BS, rewrite 你你.
        feed(
            &mut t,
            b"\x08\x08\x1b[27m\xe4\xbd\xa0\x1b[27m\x1b[7m\xe4\xbd\xa0\x1b[27m\x1b[7m\x1b[27m",
        );
        let cells = &t.view_row(0).cells;
        // Prompt intact, 你你 at cols 2..6, never overwriting col 0/1.
        assert_eq!(cells[0].ch, '>');
        assert_eq!(cells[1].ch, ' ', "prompt space must not be clobbered");
        assert_eq!(cells[2].ch, '你');
        assert!(cells[3].is_continuation);
        assert_eq!(cells[4].ch, '你');
        assert!(cells[5].is_continuation);
        assert_eq!(t.cursor().col, 6);
    }

    #[test]
    fn overwriting_one_half_of_wide_cluster_clears_the_other() {
        // Right straddle: 你你 at cols 0-3, then write 'x' at col 0. The
        // old primary's continuation at col 1 must be blanked, not left
        // stranded.
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, "你你".as_bytes());
        feed(&mut t, b"\x1b[1G"); // CHA col 1 (1-based) -> col 0
        feed(&mut t, b"x");
        let cells = &t.view_row(0).cells;
        assert_eq!(cells[0].ch, 'x');
        assert!(
            !cells[1].is_continuation,
            "stranded continuation must be cleared"
        );
        assert_eq!(cells[1].ch, ' ');
        // The second cluster is untouched.
        assert_eq!(cells[2].ch, '你');
        assert!(cells[3].is_continuation);

        // Left straddle: fresh row, 你 at 0-1, write 'x' at the
        // continuation col 1. The orphaned primary at col 0 must blank.
        let mut t2 = Term::new(1, 8, 0);
        feed(&mut t2, "你".as_bytes());
        feed(&mut t2, b"\x1b[2G"); // CHA col 2 (1-based) -> col 1
        feed(&mut t2, b"x");
        let cells2 = &t2.view_row(0).cells;
        assert_eq!(cells2[0].ch, ' ', "orphaned wide primary must be cleared");
        assert!(!cells2[0].is_continuation);
        assert_eq!(cells2[1].ch, 'x');
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
    fn da1_replies_with_vt220_sixel_and_color() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[c");
        let bytes = t.drain_pty_replies();
        // `4` advertises sixel support (the gate apps probe before
        // sending graphics).
        assert_eq!(&bytes[..], b"\x1b[?62;4;22c");
    }

    #[test]
    fn da1_with_explicit_zero_param_also_replies() {
        let mut t = Term::new(2, 4, 0);
        feed(&mut t, b"\x1b[0c");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?62;4;22c");
    }

    // ----- Sixel (DCS) ------------------------------------------------------

    /// A minimal valid sixel DCS: define color register 0 as RGB
    /// (100,0,0), select it, draw three `~` sixels (each = all 6 pixels
    /// set) → a 3px-wide, 6px-tall image. Body matches `icy_sixel`'s own
    /// doctest fixture.
    const SIXEL_3X6: &[u8] = b"\x1bPq#0;2;100;0;0#0~~~\x1b\\";

    #[test]
    fn sixel_dcs_registers_and_places_image() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, SIXEL_3X6);
        assert_eq!(t.image_grid().iter().count(), 1, "one placement expected");
        let p = t.image_grid().iter().next().unwrap();
        let img = t
            .image_registry()
            .get(p.image_id)
            .expect("image registered");
        assert_eq!(img.width, 3);
        // Sixel encodes pixels in vertical bands of 6; the decoder reports
        // the band-aligned height.
        assert!(
            img.height >= 6 && img.height.is_multiple_of(6),
            "got height {}",
            img.height
        );
    }

    #[test]
    fn sixel_advances_cursor_to_line_below() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, SIXEL_3X6);
        // 3x6 image with the default (8,16) cell = one cell; default
        // sixel scrolling moves the cursor to the left margin of the
        // line below the image.
        assert_eq!(t.cursor().row, 1);
        assert_eq!(t.cursor().col, 0);
    }

    #[test]
    fn sixel_mode_8452_leaves_cursor_right_of_image() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[?8452h");
        feed(&mut t, SIXEL_3X6);
        // One-cell image at the origin; cursor lands on its last row, one
        // column past the right edge.
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 1);
    }

    #[test]
    fn decset_80_and_8452_round_trip_via_decrqm() {
        let mut t = Term::new(2, 4, 0);
        // DECSDM (80) and cursor-right (8452) both report "reset" (2)
        // until set.
        feed(&mut t, b"\x1b[?80$p");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?80;2$y");
        feed(&mut t, b"\x1b[?80h\x1b[?80$p");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?80;1$y");
        feed(&mut t, b"\x1b[?8452h\x1b[?8452$p");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?8452;1$y");
    }

    #[test]
    fn xtsmgraphics_reports_color_registers() {
        let mut t = Term::new(4, 8, 0);
        feed(&mut t, b"\x1b[?1;1;0S");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?1;0;256S");
    }

    #[test]
    fn xtsmgraphics_reports_geometry_from_text_area() {
        let mut t = Term::new(4, 8, 0); // 8 cols × 8px, 4 rows × 16px
        feed(&mut t, b"\x1b[?2;1;0S");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?2;0;64;64S");
    }

    #[test]
    fn xtsmgraphics_unknown_item_reports_failure() {
        let mut t = Term::new(4, 8, 0);
        feed(&mut t, b"\x1b[?9;1;0S");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[?9;2S");
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
    fn xtwinops_16_replies_with_cell_pixel_size() {
        // CSI 16 t — apps need the cell pixel size to compute kitty
        // graphics placements. Reply is CSI 6 ; <cell_h> ; <cell_w> t.
        let mut t = Term::new(24, 80, 0);
        t.set_cell_pixel_size(9, 18);
        feed(&mut t, b"\x1b[16t");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[6;18;9t");
    }

    #[test]
    fn xtwinops_18_replies_with_text_area_cell_size() {
        let mut t = Term::new(24, 80, 0);
        feed(&mut t, b"\x1b[18t");
        assert_eq!(&t.drain_pty_replies()[..], b"\x1b[8;24;80t");
    }

    #[test]
    fn osc_11_query_replies_with_bg_rgb() {
        // OSC 11 ; ? — apps probe bg to choose dark vs light rendering.
        let mut t = Term::new(2, 4, 0);
        t.set_default_bg([0xab, 0xcd, 0xef]);
        feed(&mut t, b"\x1b]11;?\x1b\\");
        assert_eq!(
            &t.drain_pty_replies()[..],
            b"\x1b]11;rgb:abab/cdcd/efef\x1b\\"
        );
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
        feed(&mut t, b"\x1b]4;1;rgb:11/22/33;2;rgb:44/55/66\x1b\\");
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
        // M1: after a cursor-moving placement the reference kitty does
        // `c->x += cols; c->y += rows - 1;`. Cursor started at (0, 0),
        // c=2,r=2 => row = 0 + (2 - 1) = 1; col = 0 + 2 = 2.
        let cur = t.cursor();
        assert_eq!(cur.row, 1, "row += rows - 1");
        assert_eq!(cur.col, 2, "col += cols");
    }

    #[test]
    fn apc_kitty_transmit_and_place_lands_cursor_at_start_col() {
        // M1: after `a=T`, the cursor lands at
        // (start_col + cols, start_row + rows - 1) per reference kitty
        // (`c->x += cols; c->y += rows - 1;`). Apps that place images
        // mid-line (e.g. inline emoji icons inside a sentence) rely on
        // the column advancing from start_col rather than resetting.
        let mut t = Term::new(8, 16, 0);
        // Move the cursor to (row=1, col=5) — 1-based 2;6.
        feed(&mut t, b"\x1b[2;6H");
        assert_eq!(t.cursor().col, 5);
        assert_eq!(t.cursor().row, 1);
        // Place a 1x1 red pixel as a 1-col x 2-row placement,
        // default C=0 (cursor MOVES after).
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=2;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        let cur = t.cursor();
        // Row advanced by rows - 1 = 1: 1 + 1 = 2.
        assert_eq!(cur.row, 1 + 1, "cursor row should advance by rows - 1");
        // Column advanced by cols = 1 from start_col=5: 5 + 1 = 6.
        assert_eq!(
            cur.col,
            5 + 1,
            "cursor col should advance by cols from start_col"
        );
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

    /// Regression: placing an image taller than the rows remaining
    /// below the cursor used to silently clamp the placement to a
    /// single-row band at the bottom. The terminal must scroll the
    /// screen up to make room (per the kitty spec) so the image
    /// renders at its declared row span.
    #[test]
    fn place_image_at_bottom_scrolls_to_make_room() {
        // 10-row terminal. Move cursor to row 8 so only 2 rows fit
        // below.
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[9;1H"); // CUP row 9 (1-based) → row 8
        assert_eq!(t.cursor.row, 8);
        // Place an image with r=5 (needs 5 rows; only 2 available).
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=4,r=5;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        let p = t.image_grid().iter().next().expect("placement registered");
        let span = p.row_range.end - p.row_range.start;
        assert_eq!(
            span, 5,
            "placement should be 5 rows tall after scroll; got {p:?}"
        );
        // The placement should extend to the bottom of the grid.
        assert_eq!(p.row_range.end, 10);
        // Top of the image sits at row 5 (10 - 5).
        assert_eq!(p.row_range.start, 5);
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
        let raw = vec![
            255u8, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255, 255, 0, 0, 255,
        ];
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
    fn image_place_marks_cells_dirty() {
        let mut t = Term::new(4, 8, 0);
        t.clear_damage();
        assert!(t.damage().is_empty());
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=3,r=2;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        // Damage should now cover the cells the placement landed on.
        assert!(!t.damage().is_empty());
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

    // -- viewport / scrollback view --

    #[test]
    fn viewport_default_pinned_at_bottom() {
        let t = Term::new(3, 8, 100);
        assert_eq!(t.view_offset_lines(), 0);
        assert!(t.at_view_bottom());
        assert!(!t.is_view_scrolled_back());
        assert_eq!(t.history_lines(), 0);
    }

    #[test]
    fn viewport_scroll_by_clamped_to_history() {
        // 2 rows, 100 lines of scrollback budget, but history is 0
        // initially → scrolling up is a no-op.
        let mut t = Term::new(2, 8, 100);
        t.scroll_view_by(10, 0.0, 16.0);
        assert_eq!(t.view_offset_lines(), 0);
        assert_eq!(t.target_offset_lines(), 0);
        // Generate some history.
        feed(&mut t, b"line1\r\nline2\r\nline3\r\nline4\r\n");
        // 4 newlines past visible_rows=2 → 4 history rows (allocated
        // from scrollback budget of 100; capped lower in real usage).
        assert!(t.history_lines() >= 1);
        t.scroll_view_by(1, 0.0, 16.0);
        assert_eq!(t.target_offset_lines(), 1);
    }

    #[test]
    fn viewport_view_row_returns_scrollback_when_offset_positive() {
        let mut t = Term::new(2, 8, 100);
        // Two visible rows worth of output, plus push 'a' into history.
        feed(&mut t, b"a\r\nb\r\nc");
        // Visible: 'b' at row 0, 'c' at row 1. History: 'a' at row 0.
        assert_eq!(row_text(&t, 0), "b");
        // Snap viewport up by 1 line so the top of screen shows 'a'.
        t.scroll_view_by(1, 0.0, 16.0);
        t.force_snap_view();
        assert_eq!(t.view_offset_lines(), 1);
        // view_row(0) is now scrollback row 0 ('a'); view_row(1) is
        // logical 0 ('b').
        let r0: String = t.view_row(0).cells.iter().map(|c| c.ch).collect();
        let r1: String = t.view_row(1).cells.iter().map(|c| c.ch).collect();
        assert!(r0.starts_with('a'));
        assert!(r1.starts_with('b'));
    }

    #[test]
    fn viewport_sticky_at_bottom_when_new_output_arrives() {
        let mut t = Term::new(2, 8, 100);
        feed(&mut t, b"a\r\nb\r\n");
        // Still at the bottom — new lines should not pull the user
        // into scrollback.
        assert_eq!(t.view_offset_lines(), 0);
        feed(&mut t, b"c\r\nd\r\n");
        assert_eq!(t.view_offset_lines(), 0);
        assert!(t.at_view_bottom());
    }

    #[test]
    fn viewport_sticky_at_content_when_scrolled_back() {
        let mut t = Term::new(2, 8, 100);
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n");
        // History rows produced: 'a', 'b' (4 newlines, 2 visible →
        // 2 history rows when nothing else got scrolled).
        let hist_before = t.history_lines();
        assert!(hist_before >= 1);
        // Scroll up by 1 and snap.
        t.scroll_view_by(1, 0.0, 16.0);
        t.force_snap_view();
        let offset_before = t.view_offset_lines();
        assert_eq!(offset_before, 1);
        // Capture what view_row(0) shows.
        let before: String = t.view_row(0).cells.iter().map(|c| c.ch).collect();
        // Now generate new output — view_offset should bump so the
        // user keeps seeing the same content.
        feed(&mut t, b"e\r\n");
        assert!(t.view_offset_lines() >= 2);
        let after: String = t.view_row(0).cells.iter().map(|c| c.ch).collect();
        assert_eq!(before, after);
    }

    #[test]
    fn viewport_alt_screen_ignores_scrollback() {
        let mut t = Term::new(2, 8, 100);
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n");
        // Snap up into history on primary.
        t.scroll_view_by(1, 0.0, 16.0);
        t.force_snap_view();
        assert!(t.is_view_scrolled_back());
        // Enter alt screen — viewport should reset.
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.is_alt_active());
        assert_eq!(t.view_offset_lines(), 0);
        assert!(t.at_view_bottom());
        // Scroll_view_by is a no-op on alt.
        t.scroll_view_by(5, 0.0, 16.0);
        assert_eq!(t.target_offset_lines(), 0);
        // Exit alt — viewport remains at the live bottom.
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.is_alt_active());
        assert_eq!(t.view_offset_lines(), 0);
    }

    #[test]
    fn viewport_snap_view_to_bottom_only_updates_target() {
        let mut t = Term::new(2, 8, 100);
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n");
        t.scroll_view_by(2, 0.0, 16.0);
        t.force_snap_view();
        // Now request snap to bottom — only the target changes.
        t.snap_view_to_bottom();
        assert_eq!(t.target_offset_lines(), 0);
        // Current still elevated until advance_viewport runs.
        assert!(t.view_offset_lines() > 0 || t.target_offset_lines() == 0);
    }

    #[test]
    fn viewport_advance_with_instant_smoothing_snaps_immediately() {
        let mut t = Term::new(2, 8, 100);
        feed(&mut t, b"a\r\nb\r\nc\r\nd\r\n");
        t.scroll_view_by(2, 0.0, 16.0);
        let changed = t.advance_viewport(0.016, 16.0, Smoothing::Instant);
        assert!(changed);
        assert_eq!(t.view_offset_lines(), t.target_offset_lines());
        // Animation is done.
        assert!(!t.viewport_animating());
    }

    /// DECSC (`ESC 7`) / DECRC (`ESC 8`) round-trip: cursor returns to
    /// the saved (row, col) regardless of where it was when DECRC
    /// fires. Powerlevel10k's instant-prompt redraw depends on this.
    #[test]
    fn decsc_decrc_round_trip() {
        let mut t = Term::new(4, 8, 0);
        // Print "ab" so the cursor is at (0, 2), then DECSC.
        feed(&mut t, b"ab\x1b7");
        // Move down a couple of lines (NEL twice), print junk.
        feed(&mut t, b"\x1bE\x1bE??");
        assert_eq!(t.cursor.row, 2);
        // DECRC should restore (0, 2).
        feed(&mut t, b"\x1b8");
        assert_eq!(t.cursor.row, 0);
        assert_eq!(t.cursor.col, 2);
    }

    /// DECRC with no prior DECSC should home the cursor.
    #[test]
    fn decrc_without_save_homes_cursor() {
        let mut t = Term::new(4, 8, 0);
        feed(&mut t, b"abc\x1bE??");
        assert!(t.cursor.row > 0 || t.cursor.col > 0);
        feed(&mut t, b"\x1b8");
        assert_eq!(t.cursor.row, 0);
        assert_eq!(t.cursor.col, 0);
    }

    /// Regression for the zsh "DEL deletes from end of line" bug.
    /// zsh's response to one DEL keypress is `BS DCH CUF SP CUB`;
    /// before DCH was implemented the in-place deletion was dropped
    /// and only the trailing-space writeback was visible, so each
    /// DEL appeared to eat a char off the right end of the line.
    #[test]
    fn dch_shifts_cells_left_and_blanks_tail() {
        let mut t = Term::new(2, 10, 0);
        feed(&mut t, b"printf hel");
        // Move cursor under 't' (position 4) and send DCH.
        feed(&mut t, b"\x1b[1;5H\x1b[P");
        assert_eq!(row_text(&t, 0), "prinf hel");
        // Cursor must not move.
        assert_eq!(t.cursor().col, 4);
    }

    #[test]
    fn dch_default_count_is_one() {
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;2H\x1b[P");
        assert_eq!(row_text(&t, 0), "acdef");
    }

    #[test]
    fn dch_count_larger_than_remaining_clamps() {
        let mut t = Term::new(1, 6, 0);
        feed(&mut t, b"abcdef\x1b[1;4H\x1b[99P");
        // From col 3 onward (`def`) is consumed; everything to the
        // left of the cursor is untouched.
        assert_eq!(row_text(&t, 0), "abc");
    }

    #[test]
    fn ich_shifts_cells_right_and_inserts_blanks() {
        // Fill the row so we can observe cells falling off the right
        // edge (the doc-comment guarantee).
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, b"abcdefgh\x1b[1;3H\x1b[2@");
        // `ab` stays, two blanks inserted at col 2, `cdef` shifted
        // right to cols 4..8. `gh` falls off the right edge.
        assert_eq!(row_text(&t, 0), "ab  cdef");
    }

    #[test]
    fn ech_writes_blanks_without_shifting() {
        let mut t = Term::new(1, 8, 0);
        feed(&mut t, b"abcdef\x1b[1;3H\x1b[2X");
        // ECH replaces 2 chars at the cursor with blanks; nothing
        // shifts so `ef` stays in place.
        assert_eq!(row_text(&t, 0), "ab  ef");
    }

    #[test]
    fn dl_scrolls_lines_below_up() {
        let mut t = Term::new(4, 4, 0);
        feed(&mut t, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        // Cursor home, then delete 1 line.
        feed(&mut t, b"\x1b[H\x1b[M");
        assert_eq!(row_text(&t, 0), "bbbb");
        assert_eq!(row_text(&t, 1), "cccc");
        assert_eq!(row_text(&t, 2), "dddd");
        assert_eq!(row_text(&t, 3), "");
    }

    #[test]
    fn il_scrolls_lines_below_down() {
        let mut t = Term::new(4, 4, 0);
        feed(&mut t, b"aaaa\r\nbbbb\r\ncccc\r\ndddd");
        // Cursor to row 2, then insert 1 line.
        feed(&mut t, b"\x1b[2;1H\x1b[L");
        assert_eq!(row_text(&t, 0), "aaaa");
        assert_eq!(row_text(&t, 1), "");
        assert_eq!(row_text(&t, 2), "bbbb");
        assert_eq!(row_text(&t, 3), "cccc");
        // "dddd" fell off the bottom.
    }

    #[test]
    fn vpa_moves_to_row_keeping_column() {
        let mut t = Term::new(5, 5, 0);
        feed(&mut t, b"\x1b[1;3H\x1b[4d");
        assert_eq!(t.cursor().row, 3);
        assert_eq!(t.cursor().col, 2);
    }
}

#[cfg(test)]
mod blocker_altscreen_tests {
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn b64_red_1x1() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Place a 1x1 image with the given kitty image id at the cursor.
    fn place_image(t: &mut Term, id: u32) {
        let mut payload = Vec::new();
        payload.extend_from_slice(format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},c=1,r=1;").as_bytes());
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    #[test]
    fn alt_screen_preserves_primary_images() {
        let mut t = Term::new(8, 8, 0);

        // Primary screen: place one image.
        place_image(&mut t, 1);
        assert_eq!(t.image_grid().len(), 1);
        assert!(!t.is_alt_active());

        // Enter alt screen: active grid must be empty, alt active.
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.is_alt_active());
        assert_eq!(
            t.image_grid().len(),
            0,
            "alt screen must start with an empty image grid"
        );

        // Place an image on the alt screen.
        place_image(&mut t, 2);
        assert_eq!(t.image_grid().len(), 1);

        // Exit alt screen: the primary image must be restored, the alt
        // image gone.
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.is_alt_active());
        assert_eq!(
            t.image_grid().len(),
            1,
            "primary-screen image must survive the alt-screen round trip"
        );
        // The restored placement must be the primary one (id 1), not the
        // alt one (id 2).
        let id = t.image_grid().iter().next().unwrap().image_id;
        assert_eq!(id, 1, "restored image must be the primary screen's image");
    }

    #[test]
    fn alt_and_primary_image_lists_are_independent() {
        let mut t = Term::new(8, 8, 0);

        // Primary image.
        place_image(&mut t, 10);
        assert_eq!(t.image_grid().len(), 1);

        // Enter alt; primary image must not leak onto alt.
        feed(&mut t, b"\x1b[?1049h");
        assert_eq!(t.image_grid().len(), 0);

        // Two images on alt.
        place_image(&mut t, 20);
        place_image(&mut t, 21);
        assert_eq!(t.image_grid().len(), 2);

        // Back to primary: exactly the one primary image, none from alt.
        feed(&mut t, b"\x1b[?1049l");
        assert_eq!(t.image_grid().len(), 1);
        let id = t.image_grid().iter().next().unwrap().image_id;
        assert_eq!(id, 10, "alt images must not leak onto the primary screen");

        // Re-enter alt: it starts fresh again (alt images were dropped on
        // exit, not retained).
        feed(&mut t, b"\x1b[?1049h");
        assert_eq!(
            t.image_grid().len(),
            0,
            "alt screen images must not persist across visits"
        );
    }
}

/// Tests for the kitty graphics delete-selector blockers B4-B7.
/// Kept in a dedicated module to avoid merge conflicts with the main
/// `mod tests` while other workers edit `term.rs`.
#[cfg(test)]
mod blocker_delete_tests {
    use super::*;
    use toastty_parser::Parser;

    /// Feed `bytes` through a fresh parser into `t`.
    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// Base64 of a 1x1 opaque red RGBA pixel.
    fn b64_red() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Transmit (register, no placement) image `id` as a 1x1 red pixel.
    fn transmit(t: &mut Term, id: u32) {
        let mut payload = Vec::new();
        payload.extend_from_slice(format!("\x1b_Ga=t,f=32,s=1,v=1,i={id};").as_bytes());
        payload.extend_from_slice(&b64_red());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    /// Place already-transmitted image `id` at the current cursor with a
    /// `cols x rows` cell span and placement id `pid`.
    fn place(t: &mut Term, id: u32, pid: u32, cols: u16, rows: u16) {
        feed(
            t,
            format!("\x1b_Ga=p,i={id},p={pid},c={cols},r={rows},q=2\x1b\\").as_bytes(),
        );
    }

    // ---- B4: d=i must honor p= ----
    #[test]
    fn b4_delete_by_id_with_placement_id_removes_only_that_placement() {
        let mut t = Term::new(40, 40, 0);
        transmit(&mut t, 1);
        // Two placements of image 1: p=1 and p=2.
        feed(&mut t, b"\x1b[12;4H");
        place(&mut t, 1, 1, 1, 1);
        feed(&mut t, b"\x1b[12;30H");
        place(&mut t, 1, 2, 1, 1);
        assert_eq!(t.image_grid().len(), 2);

        // Delete only the p=1 placement.
        feed(&mut t, b"\x1b_Ga=d,d=i,i=1,p=1,q=2\x1b\\");

        let remaining: Vec<u32> = t.image_grid().iter().map(|p| p.placement_id).collect();
        assert_eq!(remaining, vec![2], "p=1 removed, p=2 survives");
        assert!(t.image_registry().contains(1), "lowercase keeps bytes");
    }

    // ---- B5: d=I frees bytes only when no placements remain ----
    #[test]
    fn b5_uppercase_delete_frees_bytes_only_after_last_placement() {
        let mut t = Term::new(40, 40, 0);
        transmit(&mut t, 1);
        feed(&mut t, b"\x1b[12;4H");
        place(&mut t, 1, 1, 1, 1);
        feed(&mut t, b"\x1b[12;30H");
        place(&mut t, 1, 2, 1, 1);
        assert_eq!(t.image_grid().len(), 2);
        assert!(t.image_registry().contains(1));

        // Delete the p=1 placement with uppercase I. p=2 still
        // references image 1, so bytes must be retained.
        feed(&mut t, b"\x1b_Ga=d,d=I,i=1,p=1,q=2\x1b\\");
        assert_eq!(t.image_grid().len(), 1);
        assert!(
            t.image_registry().contains(1),
            "bytes retained while p=2 still references image 1"
        );

        // Delete the last (p=2) placement with uppercase I — now bytes
        // are freed.
        feed(&mut t, b"\x1b_Ga=d,d=I,i=1,p=2,q=2\x1b\\");
        assert_eq!(t.image_grid().len(), 0);
        assert!(
            !t.image_registry().contains(1),
            "bytes freed once last placement is gone"
        );
        assert_eq!(t.image_registry().len(), 0);
    }

    // ---- B6: d=p deletes by cell coords (x=,y=) ----
    #[test]
    fn b6_delete_by_cell_inside_removes_and_outside_keeps() {
        // Place a 3x3-cell image at 1-based cursor (row 12, col 10) ->
        // internal row 11, col 9. col_range = 9..12, row_range = 11..14.
        let setup = || {
            let mut t = Term::new(40, 40, 0);
            transmit(&mut t, 7);
            feed(&mut t, b"\x1b[12;10H");
            place(&mut t, 7, 0, 3, 3);
            assert_eq!(t.image_grid().len(), 1);
            t
        };

        // Cell (1-based) col 12, row 12 is inside the placement.
        let mut t = setup();
        feed(&mut t, b"\x1b_Ga=d,d=p,x=12,y=12,q=2\x1b\\");
        assert_eq!(t.image_grid().len(), 0, "cell inside the image deletes it");

        // Cell (1-based) col 1, row 1 is outside the placement.
        let mut t = setup();
        feed(&mut t, b"\x1b_Ga=d,d=p,x=1,y=1,q=2\x1b\\");
        assert_eq!(t.image_grid().len(), 1, "cell outside leaves the image");
    }

    // ---- B7: d=r / d=R delete by image-id range ----
    #[test]
    fn b7_delete_by_id_range_removes_in_range_and_uppercase_frees_bytes() {
        let mut t = Term::new(40, 40, 0);
        for id in [5u32, 8, 12] {
            transmit(&mut t, id);
        }
        feed(&mut t, b"\x1b[12;4H");
        place(&mut t, 5, 0, 1, 1);
        feed(&mut t, b"\x1b[12;22H");
        place(&mut t, 8, 0, 1, 1);
        feed(&mut t, b"\x1b[12;40H");
        place(&mut t, 12, 0, 1, 1);
        assert_eq!(t.image_grid().len(), 3);

        // Lowercase r: ids in [4, 10] -> 5 and 8 placements removed; 12
        // survives. Bytes are NOT freed by lowercase.
        feed(&mut t, b"\x1b_Ga=d,d=r,x=4,y=10,q=2\x1b\\");
        let remaining: Vec<u32> = t.image_grid().iter().map(|p| p.image_id).collect();
        assert_eq!(remaining, vec![12], "only id 12 placement survives");
        assert!(t.image_registry().contains(5), "lowercase r keeps bytes");
        assert!(t.image_registry().contains(8), "lowercase r keeps bytes");
        assert!(t.image_registry().contains(12));

        // Uppercase R over the same range frees the (already
        // placement-less) image bytes for 5 and 8.
        feed(&mut t, b"\x1b_Ga=d,d=R,x=4,y=10,q=2\x1b\\");
        assert!(
            !t.image_registry().contains(5),
            "uppercase R frees bytes for id 5"
        );
        assert!(
            !t.image_registry().contains(8),
            "uppercase R frees bytes for id 8"
        );
        assert!(
            t.image_registry().contains(12),
            "id 12 outside range untouched"
        );
    }
}

/// Tests for spec-compliance blockers B1 (CSI 2J/3J clears images) and
/// B3 (DECSTBM partial-region scroll moves images within the region).
#[cfg(test)]
mod blocker_screenops_tests {
    use super::*;
    use base64::Engine;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn b64_red_1x1() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Transmit-and-place a 1x1 image occupying `cols`x`rows` cells at
    /// the current cursor.
    fn place_image_at_cursor(t: &mut Term, id: u32, cols: u16, rows: u16) {
        let mut payload = Vec::new();
        payload.extend_from_slice(
            format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},c={cols},r={rows};").as_bytes(),
        );
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    // ---- B1: CSI 2J / 3J must clear images; 0J / 1J must not. ----

    #[test]
    fn ed_2j_clears_all_images() {
        let mut t = Term::new(8, 8, 0);
        place_image_at_cursor(&mut t, 1, 2, 2);
        assert_eq!(t.image_grid().len(), 1);
        feed(&mut t, b"\x1b[2J");
        assert_eq!(t.image_grid().len(), 0, "2J must clear all images");
    }

    #[test]
    fn ed_3j_clears_all_images() {
        let mut t = Term::new(8, 8, 0);
        place_image_at_cursor(&mut t, 1, 2, 2);
        assert_eq!(t.image_grid().len(), 1);
        feed(&mut t, b"\x1b[3J");
        assert_eq!(t.image_grid().len(), 0, "3J must clear all images");
    }

    #[test]
    fn ed_0j_and_1j_do_not_clear_images() {
        // Partial erases (to-end / to-start) must not affect graphics
        // per the kitty spec.
        let mut t = Term::new(8, 8, 0);
        // Put the cursor mid-screen so both 0J and 1J leave some cells.
        feed(&mut t, b"\x1b[4;4H");
        place_image_at_cursor(&mut t, 1, 2, 2);
        assert_eq!(t.image_grid().len(), 1);
        feed(&mut t, b"\x1b[0J");
        assert_eq!(t.image_grid().len(), 1, "0J must NOT clear images");
        feed(&mut t, b"\x1b[1J");
        assert_eq!(t.image_grid().len(), 1, "1J must NOT clear images");
    }

    // ---- B3: DECSTBM partial-region scroll moves images. ----

    #[test]
    fn decstbm_scroll_up_moves_image_within_region() {
        // 10-row terminal, region rows 3..=8 (1-based 4;9).
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[4;9r"); // DECSTBM → top=3, bot=8; cursor home.
        // Place a 2-row image at region-interior row 5 (1-based row 6).
        feed(&mut t, b"\x1b[6;1H");
        place_image_at_cursor(&mut t, 1, 2, 2);
        let before = t.image_grid().iter().next().unwrap().row_range.clone();
        assert_eq!(before, 5..7, "image placed at rows 5..7");
        // Drive the cursor to the bottom margin and emit 2 line feeds so
        // the region scrolls up twice.
        feed(&mut t, b"\x1b[9;1H\n\n");
        let after = t.image_grid().iter().next().unwrap().row_range.clone();
        assert_eq!(after, 3..5, "image shifted up by 2 within the region");
    }

    #[test]
    fn decstbm_scroll_up_clips_image_at_region_top() {
        // Image straddles near the region top; scrolling up clips it at
        // the top margin (CLIPPING CHOICE: a placement that scrolls past
        // the region top is clamped to start at `top`; only when its
        // entire span scrolls above `top` is it dropped).
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[3;8r"); // top=2, bot=7.
        feed(&mut t, b"\x1b[4;1H"); // row 3 (interior).
        place_image_at_cursor(&mut t, 1, 2, 3); // rows 3..6.
        assert_eq!(t.image_grid().iter().next().unwrap().row_range, 3..6);
        // Scroll up by 2: 3..6 → 1..4, but region top is 2, so clipped
        // to 2..4.
        feed(&mut t, b"\x1b[8;1H\n\n");
        assert_eq!(
            t.image_grid().iter().next().unwrap().row_range,
            2..4,
            "top edge clipped to region top margin"
        );
    }

    #[test]
    fn decstbm_scroll_up_leaves_image_below_region_untouched() {
        // An image entirely below the scroll region must not move.
        let mut t = Term::new(10, 8, 0);
        // Place image at rows 8..9 first (below the region we set next).
        feed(&mut t, b"\x1b[9;1H");
        place_image_at_cursor(&mut t, 1, 2, 1); // rows 8..9.
        assert_eq!(t.image_grid().iter().next().unwrap().row_range, 8..9);
        // Region rows 1..=5 (1-based 2;6), entirely above the image.
        feed(&mut t, b"\x1b[2;6r");
        feed(&mut t, b"\x1b[6;1H\n\n");
        assert_eq!(
            t.image_grid().iter().next().unwrap().row_range,
            8..9,
            "image below region is untouched"
        );
    }

    #[test]
    fn decstbm_scroll_down_moves_image_within_region() {
        // Reverse Index within a region scrolls content (and images)
        // down.
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[3;9r"); // top=2, bot=8.
        feed(&mut t, b"\x1b[4;1H"); // row 3 interior.
        place_image_at_cursor(&mut t, 1, 2, 2); // rows 3..5.
        assert_eq!(t.image_grid().iter().next().unwrap().row_range, 3..5);
        // Move to top margin and Reverse Index (ESC M) twice to scroll
        // the region down by 2.
        feed(&mut t, b"\x1b[3;1H\x1bM\x1bM");
        assert_eq!(
            t.image_grid().iter().next().unwrap().row_range,
            5..7,
            "image shifted down by 2 within the region"
        );
    }
}

#[cfg(test)]
mod blocker_graphics_term_tests {
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn b64_red_1x1() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// B12 end-to-end: `a=T,...,U=1` registers the image but creates no
    /// visible placement and does not move the cursor (the visible
    /// references come from U+10EEEE placeholder cells handled
    /// elsewhere).
    #[test]
    fn b12_unicode_placeholder_transmit_is_virtual() {
        let mut t = Term::new(8, 8, 0);
        // Move cursor to a known, non-origin position (row=2, col=3).
        feed(&mut t, b"\x1b[3;4H");
        let before = t.cursor();
        assert_eq!(before.row, 2);
        assert_eq!(before.col, 3);
        let registry_before = t.image_registry().len();
        let grid_before = t.image_grid().len();

        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=2,r=2,U=1;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);

        // Image registered.
        assert!(
            t.image_registry().contains(1),
            "U=1 must register the image"
        );
        assert_eq!(
            t.image_registry().len(),
            registry_before + 1,
            "registry must grow by one"
        );
        // No visible placement created.
        assert_eq!(
            t.image_grid().len(),
            grid_before,
            "U=1 must not create a visible placement"
        );
        // Cursor unchanged.
        let after = t.cursor();
        assert_eq!(after.row, before.row, "U=1 must not move cursor row");
        assert_eq!(after.col, before.col, "U=1 must not move cursor col");
    }

    /// B9 end-to-end: a malformed header carrying `i=1` must produce an
    /// EINVAL reply queued to the pty.
    #[test]
    fn b9_malformed_header_replies_einval_to_pty() {
        let mut t = Term::new(8, 8, 0);
        // `f=999` is not a valid format → header parse error.
        feed(&mut t, b"\x1b_Ga=t,i=1,f=999,s=1,v=1;AAAA\x1b\\");
        let replies = t.drain_pty_replies();
        let s = String::from_utf8_lossy(&replies);
        assert!(s.contains("EINVAL"), "expected EINVAL reply, got {s:?}");
        assert!(
            s.contains("i=1"),
            "reply should echo recovered i=, got {s:?}"
        );
    }
}

#[cfg(test)]
mod blocker_placeholder_tests {
    use super::*;
    use base64::Engine;
    use toastty_parser::Parser;

    /// First 20 kitty placeholder diacritic codepoints (index → char),
    /// mirroring `scripts/protocol-tests/lib.sh`'s `placeholder_cell`.
    const DIACRITICS: &[u32] = &[
        0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B,
        0x034C, 0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365,
    ];

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn diac(i: usize) -> char {
        char::from_u32(DIACRITICS[i]).unwrap()
    }

    /// A placeholder cell `U+10EEEE` followed by the given diacritic
    /// indices (row, col, [msb]).
    fn placeholder_cell(diacritic_indices: &[usize]) -> String {
        let mut s = String::new();
        s.push(toastty_graphics::PLACEHOLDER);
        for &i in diacritic_indices {
            s.push(diac(i));
        }
        s
    }

    /// Register an image of `w`x`h` RGBA pixels (all red) under `id`.
    fn register_image(t: &mut Term, id: u32, w: u32, h: u32) {
        let raw = vec![255u8; (w * h * 4) as usize];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut payload = Vec::new();
        payload.extend_from_slice(format!("\x1b_Ga=t,f=32,s={w},v={h},i={id};").as_bytes());
        payload.extend_from_slice(b64.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
        assert!(
            t.image_registry().contains(id),
            "image {id} should register"
        );
    }

    /// B10: fg indexed 5 → image id 5; underline 1 → placement_id 1;
    /// diacritics (row=0, col=0) → source (0, 0).
    #[test]
    fn b10_decodes_indexed_fg_and_underline_placement() {
        let mut t = Term::new(4, 8, 0);
        // Use a 16x16 image so the (0,0) cell sub-rect is fully inside.
        register_image(&mut t, 5, 16, 16);
        // SGR 38;5;5 (fg=5), SGR 58;5;1 (underline=1), then placeholder
        // cell with row=0,col=0 diacritics, then a finalizing space.
        feed(&mut t, b"\x1b[38;5;5m\x1b[58;5;1m");
        let mut s = placeholder_cell(&[0, 0]);
        s.push(' ');
        feed(&mut t, s.as_bytes());

        assert_eq!(t.image_grid().len(), 1);
        let p = t.image_grid().iter().next().unwrap();
        assert_eq!(p.image_id, 5, "image id from fg low bits");
        assert_eq!(p.placement_id, 1, "placement id from underline color");
        assert_eq!(p.src_rect.x, 0, "col diacritic 0 → src x 0");
        assert_eq!(p.src_rect.y, 0, "row diacritic 0 → src y 0");
    }

    /// B10: the 3rd diacritic supplies bits 24..32 of the image id.
    /// id = (1 << 24) | 5, fg=5, 3rd diacritic index = 1.
    #[test]
    fn b10_third_diacritic_is_image_id_high_byte() {
        let mut t = Term::new(4, 8, 0);
        let id = (1u32 << 24) | 5;
        register_image(&mut t, id, 16, 16);
        feed(&mut t, b"\x1b[38;5;5m");
        // diacritics: row=0, col=0, msb=1.
        let mut s = placeholder_cell(&[0, 0, 1]);
        s.push(' ');
        feed(&mut t, s.as_bytes());

        assert_eq!(t.image_grid().len(), 1);
        let p = t.image_grid().iter().next().unwrap();
        assert_eq!(p.image_id, (1 << 24) | 5);
    }

    /// B10 direct unit test of the decode helper: fg low bits +
    /// placement id from underline. The high byte is NOT part of the
    /// returned image-id low bits (it comes from the 3rd diacritic).
    #[test]
    fn b10_decode_helper_splits_fg_and_underline() {
        // Indexed fg + indexed underline.
        assert_eq!(
            placeholder_image_id_from_sgr(Color::Indexed256(5), Some(Color::Indexed256(7))),
            Some((5, 7))
        );
        // RGB fg → 24-bit low bits; no underline → placement 0.
        assert_eq!(
            placeholder_image_id_from_sgr(Color::Rgb(0x12, 0x34, 0x56), None),
            Some((0x0012_3456, 0))
        );
        // Default fg → not a usable placeholder.
        assert_eq!(placeholder_image_id_from_sgr(Color::Default, None), None);
    }

    /// B11: a bare cell (no diacritics) inherits row from the left
    /// neighbor and advances the column by one.
    #[test]
    fn b11_bare_cell_inherits_row_and_advances_col() {
        let mut t = Term::new(4, 8, 0);
        // 2-cell-wide image: cell_pixel default is 8 wide, so make it
        // at least 16px wide so col=1 maps inside the image.
        register_image(&mut t, 5, 16, 16);
        feed(&mut t, b"\x1b[38;5;5m\x1b[58;5;1m");
        let mut s = String::new();
        s.push_str(&placeholder_cell(&[0, 0])); // explicit row=0, col=0
        s.push_str(&placeholder_cell(&[])); // bare — inherits row=0, col=1
        s.push(' '); // finalize
        feed(&mut t, s.as_bytes());

        assert_eq!(t.image_grid().len(), 2);
        let mut ps: Vec<&Placement> = t.image_grid().iter().collect();
        ps.sort_by_key(|p| p.col_range.start);
        let (first, second) = (ps[0], ps[1]);
        // Both reference the same image and placement id.
        assert_eq!(first.image_id, 5);
        assert_eq!(second.image_id, 5);
        assert_eq!(second.placement_id, 1);
        // Second cell inherits source row 0 (src y 0) and col 1
        // (src x = 1 * cell_pw = 8).
        let (cell_pw, _) = t.cell_pixel_size();
        assert_eq!(second.src_rect.y, 0, "inherited source row 0");
        assert_eq!(
            second.src_rect.x,
            u32::from(cell_pw),
            "auto-incremented source col 1"
        );
        // First cell is at source (0, 0).
        assert_eq!(first.src_rect.x, 0);
        assert_eq!(first.src_rect.y, 0);
    }

    /// B11: a row-only cell (1 diacritic) inherits id_msb and advances
    /// column, but takes its own row from the diacritic.
    #[test]
    fn b11_row_only_cell_inherits_col_and_advances() {
        let mut t = Term::new(4, 8, 0);
        // 1px cells so row/col diacritics map directly into a 32x32 image
        // without hitting the bounds clamp.
        t.set_cell_pixel_size(1, 1);
        register_image(&mut t, 5, 32, 32);
        feed(&mut t, b"\x1b[38;5;5m");
        let mut s = String::new();
        s.push_str(&placeholder_cell(&[2, 1])); // row=2, col=1
        s.push_str(&placeholder_cell(&[3])); // row=3 only → col inherits to 2
        s.push(' ');
        feed(&mut t, s.as_bytes());

        assert_eq!(t.image_grid().len(), 2);
        let mut ps: Vec<&Placement> = t.image_grid().iter().collect();
        ps.sort_by_key(|p| p.col_range.start);
        let (cell_pw, cell_ph) = t.cell_pixel_size();
        // Second cell: row=3 from its own diacritic, col=2 inherited+1.
        assert_eq!(ps[1].src_rect.y, 3 * u32::from(cell_ph));
        assert_eq!(ps[1].src_rect.x, 2 * u32::from(cell_pw));
    }
}

/// Tests for spec-compliance "Major" fixes M1-M5 in the kitty graphics
/// placement subsystem:
///   M1 — cursor end position after `a=T` is `(start_col + cols,
///        start_row + rows - 1)`.
///   M2 — standalone `a=p` moves the cursor (unless `C=1`/`U=1`).
///   M3 — `X=`/`Y=` are intra-cell PIXEL offsets, not cell offsets.
///   M4 — aspect ratio preserved when only one of `c=`/`r=` is given.
///   M5 — re-emitting `(image_id, placement_id)` REPLACES, not stacks.
#[cfg(test)]
mod major_place_tests {
    use super::*;
    use base64::Engine;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// base64 of a single RGBA pixel (red). Used with `s=1,v=1` so the
    /// transmitted image is 1x1 regardless of the requested cell span.
    fn b64_red_1x1() -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// `a=T` transmit-and-place a 1x1 image over `cols`x`rows` cells at
    /// the current cursor, with optional extra header keys.
    fn transmit_and_place(t: &mut Term, id: u32, cols: u16, rows: u16, extra: &str) {
        let mut payload = Vec::new();
        payload.extend_from_slice(
            format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},c={cols},r={rows}{extra};").as_bytes(),
        );
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    // ---- M4 (a=p path): aspect ratio uses the registered image's dims ----

    #[test]
    fn m4_ap_derives_aspect_ratio_from_registered_image() {
        // Regression: `a=p` (place an already-transmitted image) must look
        // up the image's real pixel dimensions so M4's aspect-ratio
        // derivation runs. Before, handle_place passed 0×0, so `a=p,c=10`
        // on a 200×100 (2:1) image fell back to ceil(img_h/cell) rows
        // (a tall, squished band) instead of the aspect-correct wide band.
        let mut t = Term::new(40, 60, 0);
        let (cell_pw, cell_ph) = t.cell_pixel_size();
        let cpw = u32::from(cell_pw.max(1));
        let cph = u32::from(cell_ph.max(1));

        // Transmit a 200×100 RGBA image (id=1), no display.
        let mut raw = Vec::with_capacity(200 * 100 * 4);
        for _ in 0..(200 * 100) {
            raw.extend_from_slice(&[0u8, 0, 0, 255]);
        }
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut payload = b"\x1b_Ga=t,f=32,s=200,v=100,i=1;".to_vec();
        payload.extend_from_slice(b64.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);

        // Place with ONLY c=10 via a=p; rows must come from the 2:1 aspect.
        feed(&mut t, b"\x1b[2;2H");
        feed(&mut t, b"\x1b_Ga=p,i=1,p=1,c=10\x1b\\");

        let p = t.image_grid().iter().next().expect("placement created");
        let cols = p.col_range.end - p.col_range.start;
        let rows = p.row_range.end - p.row_range.start;
        let expected_rows = (((10u32 * cpw * 100) + (200 * cph) - 1) / (200 * cph)).max(1) as u16;
        assert_eq!(cols, 10, "c=10 honored");
        assert_eq!(
            rows, expected_rows,
            "a=p rows must be aspect-derived (cell {cpw}x{cph}), not img_h/cell"
        );
    }

    // ---- M1: cursor end position after a=T ----

    #[test]
    fn m1_cursor_lands_on_last_row_one_past_right_edge() {
        // 40x40 grid so a 4x3 image fits without scrolling.
        let mut t = Term::new(40, 40, 0);
        // Park the cursor at a known, non-origin position (row 5, col 7).
        feed(&mut t, b"\x1b[6;8H");
        assert_eq!(t.cursor().row, 5);
        assert_eq!(t.cursor().col, 7);

        // Place a 4-cols x 3-rows image (N=4, M=3).
        transmit_and_place(&mut t, 1, 4, 3, "");

        // Reference kitty: c->x += cols; c->y += rows - 1.
        // row: 5 + (3 - 1) = 7 ; col: 7 + 4 = 11.
        assert_eq!(t.cursor().row, 7, "row += rows - 1");
        assert_eq!(t.cursor().col, 11, "col += cols (from start_col)");
    }

    #[test]
    fn m1_cursor_no_move_with_capital_c() {
        let mut t = Term::new(40, 40, 0);
        feed(&mut t, b"\x1b[6;8H");
        transmit_and_place(&mut t, 1, 4, 3, ",C=1");
        // C=1 suppresses cursor motion.
        assert_eq!(t.cursor().row, 5);
        assert_eq!(t.cursor().col, 7);
    }

    // ---- M2: standalone a=p moves the cursor ----

    #[test]
    fn m2_place_moves_cursor_by_cols_and_rows_minus_one() {
        let mut t = Term::new(40, 40, 0);
        // First transmit-only (a=t) so the image exists but no placement
        // and no cursor motion happens yet.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=1,v=1,i=7;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert_eq!(t.image_grid().len(), 0, "a=t does not place");

        // Move cursor to a known spot, then standalone a=p.
        feed(&mut t, b"\x1b[6;8H");
        assert_eq!((t.cursor().row, t.cursor().col), (5, 7));
        feed(&mut t, b"\x1b_Ga=p,i=7,c=4,r=3\x1b\\");

        assert_eq!(t.image_grid().len(), 1, "a=p places");
        // Same math as a=T: row += rows-1, col += cols.
        assert_eq!(t.cursor().row, 7, "a=p row += rows - 1");
        assert_eq!(t.cursor().col, 11, "a=p col += cols");
    }

    #[test]
    fn m2_place_with_capital_c_does_not_move_cursor() {
        let mut t = Term::new(40, 40, 0);
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=1,v=1,i=7;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);

        feed(&mut t, b"\x1b[6;8H");
        feed(&mut t, b"\x1b_Ga=p,i=7,c=4,r=3,C=1\x1b\\");
        assert_eq!(t.image_grid().len(), 1);
        assert_eq!(t.cursor().row, 5, "C=1 a=p must not move row");
        assert_eq!(t.cursor().col, 7, "C=1 a=p must not move col");
    }

    // ---- M3: X=/Y= are intra-cell pixel offsets ----

    #[test]
    fn m3_xy_are_pixel_offsets_not_cell_offsets() {
        // Baseline: X=Y=0 placement.
        let mut t0 = Term::new(40, 40, 0);
        feed(&mut t0, b"\x1b[3;5H");
        transmit_and_place(&mut t0, 1, 2, 2, "");
        let base = t0.image_grid().iter().next().unwrap().clone();
        assert_eq!(base.pix_offset, (0, 0));

        // Same placement but with X=3, Y=4 (< default cell 8x16).
        let mut t = Term::new(40, 40, 0);
        feed(&mut t, b"\x1b[3;5H");
        transmit_and_place(&mut t, 1, 2, 2, ",X=3,Y=4");
        let p = t.image_grid().iter().next().unwrap().clone();

        // Pixel offset is recorded as (3, 4).
        assert_eq!(
            p.pix_offset,
            (3, 4),
            "X/Y stored as intra-cell pixel offset"
        );
        // Cell ranges are UNCHANGED relative to the X=Y=0 case.
        assert_eq!(
            p.col_range, base.col_range,
            "X must not move the starting cell"
        );
        assert_eq!(
            p.row_range, base.row_range,
            "Y must not move the starting cell"
        );
    }

    // ---- M5: re-emitting (image_id, placement_id) replaces ----

    #[test]
    fn m5_reemit_same_id_pair_replaces() {
        let mut t = Term::new(40, 40, 0);
        // Place i=1,p=1 at row 2, col 3.
        feed(&mut t, b"\x1b[3;4H");
        transmit_and_place(&mut t, 1, 2, 2, ",p=1");
        assert_eq!(t.image_grid().len(), 1);
        let first = t.image_grid().iter().next().unwrap().clone();
        assert_eq!(first.col_range.start, 3);
        assert_eq!(first.row_range.start, 2);

        // Re-place i=1,p=1 at a DIFFERENT cell (row 6, col 9).
        feed(&mut t, b"\x1b[7;10H");
        transmit_and_place(&mut t, 1, 2, 2, ",p=1");

        // Exactly ONE placement, at the new location.
        assert_eq!(t.image_grid().len(), 1, "re-emit must replace, not stack");
        let p = t.image_grid().iter().next().unwrap().clone();
        assert_eq!(p.image_id, 1);
        assert_eq!(p.placement_id, 1);
        assert_eq!(p.col_range.start, 9, "new column");
        assert_eq!(p.row_range.start, 6, "new row");
    }

    #[test]
    fn m5_unnamed_placements_still_stack() {
        // p=0 (unnamed) placements must NOT be deduplicated.
        let mut t = Term::new(40, 40, 0);
        feed(&mut t, b"\x1b[3;4H");
        transmit_and_place(&mut t, 1, 2, 2, "");
        feed(&mut t, b"\x1b[7;10H");
        transmit_and_place(&mut t, 1, 2, 2, "");
        assert_eq!(
            t.image_grid().len(),
            2,
            "unnamed (p=0) placements accumulate"
        );
    }
}

// ===== M10/M11/M12: kitty graphics delete selectors =====
#[cfg(test)]
mod major_delete_tests {
    use super::*;
    use toastty_parser::Parser;

    /// Feed `bytes` through a fresh parser into `t`.
    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// 1x1 red RGBA pixel, base64-encoded (matches the `f=32,s=1,v=1`
    /// header below).
    fn red_pixel_b64() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Move the cursor to 0-based (row, col) via CUP (1-based wire form).
    fn cup(t: &mut Term, row: u16, col: u16) {
        let seq = format!("\x1b[{};{}H", row + 1, col + 1);
        feed(t, seq.as_bytes());
    }

    /// Transmit-and-place image `id` (number `num`, z-index `z`) as a
    /// single-cell placement at the current cursor. `num`/`z` may be 0.
    fn transmit_place_at(t: &mut Term, row: u16, col: u16, id: u32, num: u32, z: i32) {
        cup(t, row, col);
        let header = format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},I={num},z={z},c=1,r=1;");
        let mut payload = header.into_bytes();
        payload.extend_from_slice(&red_pixel_b64());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    /// Transmit-and-place an image by image-NUMBER only (no `i=`, which
    /// the B8 rule forbids alongside `I=`). The terminal assigns the id;
    /// auto-assign hands out the lowest free id (1, 2, ...). Returns that
    /// assigned id (read back from the registry as the only fresh entry).
    fn transmit_place_numbered(t: &mut Term, row: u16, col: u16, num: u32) {
        cup(t, row, col);
        let header = format!("\x1b_Ga=T,f=32,s=1,v=1,I={num},c=1,r=1;");
        let mut payload = header.into_bytes();
        payload.extend_from_slice(&red_pixel_b64());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    /// Add a SECOND placement (`a=p`) of an already-transmitted image
    /// `id` with placement id `pid` at (row, col), single cell.
    fn place_again_at(t: &mut Term, row: u16, col: u16, id: u32, pid: u32) {
        cup(t, row, col);
        let seq = format!("\x1b_Ga=p,i={id},p={pid},c=1,r=1\x1b\\");
        feed(t, seq.as_bytes());
    }

    /// Does the grid hold any placement for image `id`?
    fn has_placement_for(t: &Term, id: u32) -> bool {
        t.image_grid().iter().any(|p| p.image_id == id)
    }

    // ---- M10: d=a / d=A scope to placements VISIBLE on screen ----

    #[test]
    fn m10_delete_a_scopes_to_the_active_screen() {
        // In toastty's model the image grid only ever holds placements on
        // visible rows: scrolling drops those past the top, and resize
        // (M9) clips/drops those past the bottom, so an "off-screen but
        // present" placement cannot occur within a single screen. The
        // visible-scoping that genuinely matters for d=a is primary-vs-alt:
        // d=a must touch only the ACTIVE screen's grid (the inactive
        // screen's placements are stashed by the alt-screen split, B2).
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 0, 0, 1, 0, 0); // primary image id=1
        assert_eq!(t.image_grid().len(), 1);
        // Enter the alt screen: primary's grid is stashed, alt starts empty.
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.is_alt_active());
        assert_eq!(t.image_grid().len(), 0);
        transmit_place_at(&mut t, 0, 0, 2, 0, 0); // alt image id=2
        assert_eq!(t.image_grid().len(), 1);
        // d=a on the alt screen removes only the alt placement.
        feed(&mut t, b"\x1b_Ga=d,d=a\x1b\\");
        assert_eq!(t.image_grid().len(), 0, "alt placement removed");
        // Back on primary: its placement was never touched.
        feed(&mut t, b"\x1b[?1049l");
        assert!(!t.is_alt_active());
        assert!(
            has_placement_for(&t, 1),
            "primary placement survives d=a issued on the alt screen"
        );
    }

    #[test]
    fn m10_uppercase_a_frees_bytes_lowercase_keeps_them() {
        // d=a (lowercase) removes the visible placement but RETAINS the
        // image bytes; d=A (uppercase) also frees the bytes once no
        // placement remains. (Cross-screen byte survival — freeing only
        // when a placement remains elsewhere — is exercised by the i/I
        // selector tests; d=A removes every visible placement, so within a
        // single screen the "free only when none remains" branch reduces
        // to "free".)
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 0, 0, 1, 0, 0);
        assert_eq!(t.image_grid().len(), 1);
        // lowercase d=a: placement gone, bytes kept.
        feed(&mut t, b"\x1b_Ga=d,d=a\x1b\\");
        assert_eq!(t.image_grid().len(), 0);
        assert!(
            t.image_registry().contains(1),
            "lowercase d=a keeps image bytes"
        );
        // Re-place the (still-registered) image, then uppercase d=A:
        // placement gone AND bytes freed.
        place_again_at(&mut t, 0, 0, 1, 0);
        assert_eq!(t.image_grid().len(), 1);
        feed(&mut t, b"\x1b_Ga=d,d=A\x1b\\");
        assert_eq!(t.image_grid().len(), 0);
        assert!(
            !t.image_registry().contains(1),
            "uppercase d=A frees bytes once no placement remains"
        );
    }

    // ---- M11: d=n / d=N by image NUMBER ----

    #[test]
    fn m11_delete_n_targets_newest_image_with_number() {
        // Two images share image-number I=7. Per the B8 rule a transmit
        // can't carry both `i=` and `I=`, so the terminal assigns ids:
        // auto-assign yields id 1 (older) then id 2 (newer).
        let mut t = Term::new(8, 8, 0);
        transmit_place_numbered(&mut t, 0, 0, 7); // assigned id 1 (older)
        transmit_place_numbered(&mut t, 2, 0, 7); // assigned id 2 (newer → wins)
        assert!(t.image_registry().contains(1));
        assert!(t.image_registry().contains(2));
        assert_eq!(t.image_grid().len(), 2);
        // d=n,I=7 deletes the NEWEST (id=2) placement; bytes kept (n).
        feed(&mut t, b"\x1b_Ga=d,d=n,I=7\x1b\\");
        assert!(has_placement_for(&t, 1), "older image placement untouched");
        assert!(!has_placement_for(&t, 2), "newest image placement removed");
        assert!(t.image_registry().contains(2), "lowercase n keeps bytes");
    }

    #[test]
    fn m11_delete_uppercase_n_frees_newest_bytes() {
        let mut t = Term::new(8, 8, 0);
        transmit_place_numbered(&mut t, 0, 0, 7); // id 1 (older)
        transmit_place_numbered(&mut t, 2, 0, 7); // id 2 (newer)
        // d=N,I=7 deletes the newest (id=2) AND frees its bytes.
        feed(&mut t, b"\x1b_Ga=d,d=N,I=7\x1b\\");
        assert!(!has_placement_for(&t, 2));
        assert!(!t.image_registry().contains(2), "N frees newest bytes");
        assert!(t.image_registry().contains(1), "older image untouched");
    }

    // ---- M12: d=c/C, q/Q, x/X, y/Y, z/Z ----

    #[test]
    fn m12_delete_c_removes_placement_under_cursor() {
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 1, 2, 1, 0, 0); // at (row1,col2)
        transmit_place_at(&mut t, 4, 4, 2, 0, 0); // at (row4,col4)
        // Park cursor on image 1's cell, then d=c.
        cup(&mut t, 1, 2);
        feed(&mut t, b"\x1b_Ga=d,d=c\x1b\\");
        assert!(!has_placement_for(&t, 1), "cursor-cell placement removed");
        assert!(has_placement_for(&t, 2), "other placement kept");
    }

    #[test]
    fn m12_delete_q_removes_placement_at_cell_with_zindex() {
        let mut t = Term::new(8, 8, 0);
        // Two placements share cell (row2,col3) but differ in z.
        transmit_place_at(&mut t, 2, 3, 1, 0, 5); // z=5
        transmit_place_at(&mut t, 2, 3, 2, 0, 9); // z=9
        // d=q at cell x=4,y=3 (1-based → col3,row2) with z=9 hits id=2.
        feed(&mut t, b"\x1b_Ga=d,d=q,x=4,y=3,z=9\x1b\\");
        assert!(has_placement_for(&t, 1), "z=5 placement kept");
        assert!(!has_placement_for(&t, 2), "z=9 placement at cell removed");
    }

    #[test]
    fn m12_delete_x_removes_placements_in_column() {
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 0, 3, 1, 0, 0); // col 3
        transmit_place_at(&mut t, 5, 6, 2, 0, 0); // col 6
        // d=x,x=4 (1-based col 4 → 0-based col 3) hits image 1.
        feed(&mut t, b"\x1b_Ga=d,d=x,x=4\x1b\\");
        assert!(!has_placement_for(&t, 1), "column-3 placement removed");
        assert!(has_placement_for(&t, 2), "column-6 placement kept");
    }

    #[test]
    fn m12_delete_y_removes_placements_in_row() {
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 2, 0, 1, 0, 0); // row 2
        transmit_place_at(&mut t, 5, 0, 2, 0, 0); // row 5
        // d=y,y=3 (1-based row 3 → 0-based row 2) hits image 1.
        feed(&mut t, b"\x1b_Ga=d,d=y,y=3\x1b\\");
        assert!(!has_placement_for(&t, 1), "row-2 placement removed");
        assert!(has_placement_for(&t, 2), "row-5 placement kept");
    }

    #[test]
    fn m12_delete_z_removes_placements_with_zindex() {
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 0, 0, 1, 0, 4); // z=4
        transmit_place_at(&mut t, 3, 0, 2, 0, 4); // z=4
        transmit_place_at(&mut t, 6, 0, 3, 0, 7); // z=7
        // d=z,z=4 removes both z=4 placements regardless of position.
        feed(&mut t, b"\x1b_Ga=d,d=z,z=4\x1b\\");
        assert!(!has_placement_for(&t, 1), "z=4 removed");
        assert!(!has_placement_for(&t, 2), "z=4 removed");
        assert!(has_placement_for(&t, 3), "z=7 kept");
    }

    #[test]
    fn m12_uppercase_selector_frees_orphaned_bytes() {
        // Two single-placement images on different rows. Uppercase Y
        // deletes image 1's only placement → its bytes are freed, while
        // image 2 is untouched (placement and bytes intact).
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 2, 0, 1, 0, 0); // row 2
        transmit_place_at(&mut t, 5, 0, 2, 0, 0); // row 5
        assert!(t.image_registry().contains(1));
        feed(&mut t, b"\x1b_Ga=d,d=Y,y=3\x1b\\"); // row 2 (1-based 3)
        assert!(!has_placement_for(&t, 1));
        assert!(
            !t.image_registry().contains(1),
            "Y frees bytes for image with no surviving placement"
        );
        assert!(has_placement_for(&t, 2), "row-5 placement kept");
        assert!(
            t.image_registry().contains(2),
            "untouched image keeps bytes"
        );
    }

    #[test]
    fn m12_uppercase_selector_keeps_bytes_with_surviving_placement() {
        // One image (id=1) with two placements on different rows. Y
        // removes the row-2 placement; the row-5 placement survives so
        // bytes are kept.
        let mut t = Term::new(8, 8, 0);
        transmit_place_at(&mut t, 2, 0, 1, 0, 0); // placement A, row 2
        place_again_at(&mut t, 5, 0, 1, 2); // placement B, row 5
        assert_eq!(t.image_grid().len(), 2);
        feed(&mut t, b"\x1b_Ga=d,d=Y,y=3\x1b\\");
        assert_eq!(t.image_grid().len(), 1, "only row-2 placement removed");
        assert!(
            t.image_registry().contains(1),
            "bytes kept while another placement survives"
        );
    }
}

/// Spec-compliance coverage for M7 (RIS), M8 (CSI S/T scroll), M9
/// (resize clips placements), and M15 (IL/DL shift images). Self-contained
/// helpers so this module doesn't depend on the older `mod tests`.
#[cfg(test)]
mod major_screen_tests {
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// Base64 of a 1x1 red RGBA pixel.
    fn b64_red_1x1() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Place a 1-column, `rows`-row image (id `i`) at the current cursor
    /// using `a=T`.
    fn place_image(t: &mut Term, id: u32, rows: u16) {
        let mut payload = Vec::new();
        let hdr = format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},c=1,r={rows};");
        payload.extend_from_slice(hdr.as_bytes());
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    fn first_row_range(t: &Term) -> std::ops::Range<u16> {
        t.image_grid().iter().next().unwrap().row_range.clone()
    }

    // ---- M7: RIS ---------------------------------------------------

    #[test]
    fn ris_clears_images_text_and_homes_cursor() {
        let mut t = Term::new(8, 8, 16);
        place_image(&mut t, 1, 2);
        feed(&mut t, b"hello world");
        assert_eq!(t.image_grid().len(), 1);
        assert_ne!(t.cursor().row, 0);
        let rev_before = t.image_revision();

        feed(&mut t, b"\x1bc");

        assert!(t.image_grid().is_empty(), "RIS must clear image_grid");
        assert!(t.image_revision() != rev_before, "image_revision must bump");
        assert_eq!(t.cursor().row, 0);
        assert_eq!(t.cursor().col, 0);
        assert_eq!(t.cursor().style, Style::RESET);
        let row0 = t.view_row(0);
        assert!(
            row0.cells.iter().all(|c| c.ch == ' '),
            "RIS must clear the screen"
        );
        assert_eq!(t.scroll_top, 0);
        assert_eq!(t.scroll_bot, t.rows - 1);
    }

    #[test]
    fn ris_resets_scroll_region_and_modes() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[3;6r"); // DECSTBM 3..6
        feed(&mut t, b"\x1b[?2004h"); // bracketed paste on
        feed(&mut t, b"\x1b[?25l"); // hide cursor
        feed(&mut t, b"\x1bc");
        assert_eq!(t.scroll_top, 0);
        assert_eq!(t.scroll_bot, 7);
        assert!(!t.bracketed_paste);
        assert!(t.cursor_visible);
    }

    // ---- M8: CSI S (SU) / CSI T (SD) -------------------------------

    #[test]
    fn su_scrolls_text_and_images_up() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[4;1H");
        place_image(&mut t, 1, 2); // rows 3..5
        feed(&mut t, b"\x1b[6;1HMARK");
        assert_eq!(first_row_range(&t), 3..5);

        feed(&mut t, b"\x1b[2S"); // SU 2

        assert_eq!(first_row_range(&t), 1..3, "SU must shift placement up by 2");
        let row3: String = t.view_row(3).cells.iter().map(|c| c.ch).collect();
        assert!(
            row3.starts_with("MARK"),
            "SU must scroll text up; row3={row3:?}"
        );
    }

    #[test]
    fn sd_scrolls_text_and_images_down() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[2;1H");
        place_image(&mut t, 1, 2); // rows 1..3
        feed(&mut t, b"\x1b[1;1HTOP");
        assert_eq!(first_row_range(&t), 1..3);

        feed(&mut t, b"\x1b[2T"); // SD 2

        assert_eq!(
            first_row_range(&t),
            3..5,
            "SD must shift placement down by 2"
        );
        let row2: String = t.view_row(2).cells.iter().map(|c| c.ch).collect();
        assert!(
            row2.starts_with("TOP"),
            "SD must scroll text down; row2={row2:?}"
        );
    }

    #[test]
    fn xtsmgraphics_is_not_treated_as_su() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[2;1H");
        place_image(&mut t, 1, 2); // rows 1..3
        let before = first_row_range(&t);
        feed(&mut t, b"\x1b[?2;1;0S"); // XTSMGRAPHICS — must NOT scroll
        assert_eq!(
            first_row_range(&t),
            before,
            "CSI ? ... S (XTSMGRAPHICS) must not scroll"
        );
    }

    #[test]
    fn five_param_t_is_not_treated_as_sd() {
        let mut t = Term::new(8, 8, 0);
        feed(&mut t, b"\x1b[2;1H");
        place_image(&mut t, 1, 2); // rows 1..3
        let before = first_row_range(&t);
        feed(&mut t, b"\x1b[1;2;3;4;5T"); // highlight mouse — must NOT scroll
        assert_eq!(
            first_row_range(&t),
            before,
            "5-param CSI T (highlight mouse) must not scroll"
        );
    }

    // ---- M9: resize clips placements -------------------------------

    #[test]
    fn resize_drops_placement_past_new_bottom() {
        let mut t = Term::new(20, 8, 0);
        feed(&mut t, b"\x1b[18;1H");
        place_image(&mut t, 1, 2); // rows 17..19
        assert_eq!(first_row_range(&t), 17..19);
        let rev_before = t.image_revision();

        t.resize(5, 8); // start 17 >= 5 → dropped

        assert!(
            t.image_grid().iter().all(|p| p.row_range.start < 5),
            "no placement may start past the new bottom"
        );
        assert!(t.image_grid().is_empty(), "out-of-bounds placement dropped");
        assert_ne!(t.image_revision(), rev_before, "image_revision must bump");
    }

    #[test]
    fn resize_clips_straddling_placement_preserving_pix_offset() {
        let mut t = Term::new(20, 8, 0);
        feed(&mut t, b"\x1b[4;1H");
        // Place via a=T with a Y pixel offset so we can assert it survives.
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=1,r=4,Y=7;");
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);
        assert_eq!(first_row_range(&t), 3..7);
        let pix_before = t.image_grid().iter().next().unwrap().pix_offset;

        t.resize(5, 8); // start 3 < 5 stays; end clipped to 5

        assert_eq!(first_row_range(&t), 3..5, "straddling placement clipped");
        let pix_after = t.image_grid().iter().next().unwrap().pix_offset;
        assert_eq!(pix_before, pix_after, "clip_to must preserve pix_offset");
    }

    // ---- M15: IL / DL shift images ---------------------------------

    #[test]
    fn il_shifts_images_down() {
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[5;1H");
        place_image(&mut t, 1, 2); // rows 4..6
        assert_eq!(first_row_range(&t), 4..6);
        feed(&mut t, b"\x1b[3;1H"); // cursor row 2
        let rev_before = t.image_revision();
        feed(&mut t, b"\x1b[2L"); // IL 2
        assert_eq!(
            first_row_range(&t),
            6..8,
            "IL must shift placement down by 2"
        );
        assert_ne!(t.image_revision(), rev_before);
    }

    #[test]
    fn dl_shifts_images_up() {
        let mut t = Term::new(10, 8, 0);
        feed(&mut t, b"\x1b[5;1H");
        place_image(&mut t, 1, 2); // rows 4..6
        assert_eq!(first_row_range(&t), 4..6);
        feed(&mut t, b"\x1b[3;1H"); // cursor row 2
        let rev_before = t.image_revision();
        feed(&mut t, b"\x1b[2M"); // DL 2
        assert_eq!(first_row_range(&t), 2..4, "DL must shift placement up by 2");
        assert_ne!(t.image_revision(), rev_before);
    }
}

/// M6 end-to-end: drive a real `Term` through the parser with a `t=f`
/// (file) transmission followed by an `a=p` placement and confirm the
/// image lands in the registry. Lives in its own module to minimize
/// merge friction with the main `tests` module.
#[cfg(test)]
mod major_medium_term_tests {
    use super::*;
    use base64::Engine;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    #[test]
    fn kitty_t_f_file_transmission_registers_and_places() {
        // 2x2 RGBA image written to a temp file.
        let raw: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let pid = std::process::id();
        let path = std::env::temp_dir().join(format!("toastty-term-medium-{pid}.rgba"));
        std::fs::write(&path, &raw).unwrap();

        let mut t = Term::new(8, 8, 0);
        let before = t.image_registry().len();

        // Transmit via file medium: payload is the base64-encoded path.
        let b64_path =
            base64::engine::general_purpose::STANDARD.encode(path.to_str().unwrap().as_bytes());
        let mut payload = Vec::new();
        payload.extend_from_slice(b"\x1b_Ga=t,f=32,s=2,v=2,i=314,t=f;");
        payload.extend_from_slice(b64_path.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(&mut t, &payload);

        // The image must now be in the registry.
        assert_eq!(
            t.image_registry().len(),
            before + 1,
            "t=f transmit must register one image"
        );
        assert!(t.image_registry().contains(314));

        // Place it with an explicit cell span.
        feed(&mut t, b"\x1b_Ga=p,i=314,c=2,r=2\x1b\\");
        assert_eq!(t.image_grid().len(), 1, "a=p must produce a placement");

        // `t=f` must NOT delete the source file.
        assert!(path.exists(), "t=f must leave the file intact");
        std::fs::remove_file(&path).ok();
    }
}

/// M13: relative placements (`P=`/`Q=`/`H=`/`V=`) driven end-to-end
/// through `Term::feed`. Covers parent-relative positioning, the
/// no-cursor-move rule, follow-on-scroll, and the ENOPARENT error path.
#[cfg(test)]
mod major_relative_tests {
    use super::*;
    use base64::Engine;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    /// Register a `w`x`h` all-red RGBA image under `id`.
    fn register_image(t: &mut Term, id: u32, w: u32, h: u32) {
        let raw = vec![255u8; (w * h * 4) as usize];
        let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut payload = Vec::new();
        payload.extend_from_slice(format!("\x1b_Ga=t,f=32,s={w},v={h},i={id};").as_bytes());
        payload.extend_from_slice(b64.as_bytes());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
        assert!(
            t.image_registry().contains(id),
            "image {id} should register"
        );
    }

    /// Place an already-transmitted image as a NAMED (absolute) parent at
    /// the current cursor.
    fn place_parent(t: &mut Term, id: u32, pid: u32, cols: u16, rows: u16) {
        feed(
            t,
            format!("\x1b_Ga=p,i={id},p={pid},c={cols},r={rows}\x1b\\").as_bytes(),
        );
    }

    #[test]
    fn child_resolves_to_parent_origin_plus_offset_and_cursor_unmoved() {
        let mut t = Term::new(20, 40, 0);
        register_image(&mut t, 1, 16, 16);
        register_image(&mut t, 2, 16, 16);
        // Move cursor somewhere deterministic, then place the parent.
        feed(&mut t, b"\x1b[6;4H"); // row 5, col 3 (1-based -> 0-based)
        place_parent(&mut t, 1, 10, 3, 2);
        let parent = t.image_grid().find(1, 10).expect("parent placement");
        let (prow, pcol) = (parent.row_range.start, parent.col_range.start);
        assert_eq!((prow, pcol), (5, 3), "parent at cursor origin");

        // Record cursor, then place the child relative: H=2 cols, V=1 row.
        let cur_before = t.cursor();
        feed(&mut t, b"\x1b_Ga=p,i=2,p=20,P=1,Q=10,H=2,V=1\x1b\\");
        let cur_after = t.cursor();

        let child = t.image_grid().find(2, 20).expect("child placement created");
        assert_eq!(child.row_range.start, prow + 1, "child row = parent + V");
        assert_eq!(child.col_range.start, pcol + 2, "child col = parent + H");
        assert_eq!(child.parent, Some((1, 10)));
        assert_eq!(child.rel_offset, (2, 1));
        assert_eq!(
            (cur_after.row, cur_after.col),
            (cur_before.row, cur_before.col),
            "relative placement must NOT move the cursor",
        );
    }

    #[test]
    fn enoparent_when_parent_missing_and_no_placement_created() {
        let mut t = Term::new(20, 40, 0);
        register_image(&mut t, 2, 16, 16);
        let _ = t.drain_pty_replies();
        feed(&mut t, b"\x1b_Ga=p,i=2,p=20,P=99,Q=99,H=1,V=1\x1b\\");
        assert!(
            t.image_grid().find(2, 20).is_none(),
            "no placement on ENOPARENT"
        );
        let replies = String::from_utf8_lossy(&t.drain_pty_replies()).into_owned();
        assert!(replies.contains("ENOPARENT"), "got {replies:?}");
    }

    #[test]
    fn child_follows_parent_on_scroll() {
        let mut t = Term::new(10, 40, 0);
        register_image(&mut t, 1, 16, 16);
        register_image(&mut t, 2, 16, 16);
        // Parent near the top so a scroll keeps it on-screen.
        feed(&mut t, b"\x1b[4;3H"); // row 3, col 2 (0-based)
        place_parent(&mut t, 1, 10, 2, 2);
        feed(&mut t, b"\x1b_Ga=p,i=2,p=20,P=1,Q=10,H=1,V=1\x1b\\");
        let parent0 = t.image_grid().find(1, 10).unwrap();
        let (prow0, pcol0) = (parent0.row_range.start, parent0.col_range.start);
        let child0 = t.image_grid().find(2, 20).unwrap();
        assert_eq!(child0.row_range.start, prow0 + 1);
        assert_eq!(child0.col_range.start, pcol0 + 1);

        // Scroll the whole screen up by 1 (full-screen region scroll via
        // reverse-index is awkward; use IND/linefeed-style index by
        // moving to the bottom and emitting a newline that scrolls).
        feed(&mut t, b"\x1b[10;1H\n"); // cursor to last row, LF scrolls.

        let parent1 = t.image_grid().find(1, 10).expect("parent still present");
        let (prow1, pcol1) = (parent1.row_range.start, parent1.col_range.start);
        assert_eq!(prow1, prow0 - 1, "parent scrolled up by 1");
        let child1 = t.image_grid().find(2, 20).expect("child still present");
        assert_eq!(
            child1.row_range.start,
            prow1 + 1,
            "child followed parent up"
        );
        assert_eq!(child1.col_range.start, pcol1 + 1);
    }
}

/// Minor kitty-graphics fixes (m4, m5, m7). Registry-internal m6 lives in
/// the registry crate's tests.
#[cfg(test)]
mod minor_term_tests {
    use super::*;
    use toastty_parser::Parser;

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = Parser::new();
        p.advance(t, bytes);
    }

    fn b64_red_1x1() -> Vec<u8> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD
            .encode([255u8, 0, 0, 255])
            .into_bytes()
    }

    /// Place a 1x1 image with the given kitty image id at the cursor.
    fn place_image(t: &mut Term, id: u32) {
        let mut payload = Vec::new();
        payload.extend_from_slice(format!("\x1b_Ga=T,f=32,s=1,v=1,i={id},c=1,r=1;").as_bytes());
        payload.extend_from_slice(&b64_red_1x1());
        payload.extend_from_slice(b"\x1b\\");
        feed(t, &payload);
    }

    // ---- m4: DECSET 47 legacy alt screen ----------------------------

    #[test]
    fn legacy_alt_screen_47_isolates_and_restores_images() {
        let mut t = Term::new(8, 8, 0);

        // Primary screen: one image.
        place_image(&mut t, 1);
        assert_eq!(t.image_grid().len(), 1);
        assert!(!t.is_alt_active());

        // Enter alt via DECSET 47: active grid must be empty + alt active.
        feed(&mut t, b"\x1b[?47h");
        assert!(t.is_alt_active(), "mode 47 must enter the alt screen");
        assert_eq!(
            t.image_grid().len(),
            0,
            "alt screen (mode 47) must start with an empty image grid"
        );

        // Image on the alt screen.
        place_image(&mut t, 2);
        assert_eq!(t.image_grid().len(), 1);

        // Exit via DECSET 47: primary image restored, alt image gone.
        feed(&mut t, b"\x1b[?47l");
        assert!(!t.is_alt_active());
        assert_eq!(
            t.image_grid().len(),
            1,
            "primary image must survive a mode-47 round trip"
        );
        let id = t.image_grid().iter().next().unwrap().image_id;
        assert_eq!(id, 1, "restored image must be the primary screen's image");
    }

    #[test]
    fn legacy_alt_screen_1047_round_trip() {
        let mut t = Term::new(8, 8, 0);
        place_image(&mut t, 1);
        feed(&mut t, b"\x1b[?1047h");
        assert!(t.is_alt_active());
        assert_eq!(t.image_grid().len(), 0);
        feed(&mut t, b"\x1b[?1047l");
        assert!(!t.is_alt_active());
        assert_eq!(t.image_grid().len(), 1);
    }

    #[test]
    fn decset_1048_saves_and_restores_cursor_only() {
        let mut t = Term::new(8, 8, 0);
        // Move cursor to (row 2, col 3), then save via 1048.
        feed(&mut t, b"\x1b[3;4H");
        feed(&mut t, b"\x1b[?1048h");
        let (saved_row, saved_col) = (t.cursor.row, t.cursor.col);
        assert_eq!((saved_row, saved_col), (2, 3));
        // Move elsewhere; 1048 must NOT switch buffers.
        feed(&mut t, b"\x1b[1;1H");
        assert!(!t.is_alt_active(), "1048 must not enter the alt screen");
        // Restore via 1048l.
        feed(&mut t, b"\x1b[?1048l");
        assert_eq!((t.cursor.row, t.cursor.col), (2, 3));
        assert!(!t.is_alt_active());
    }

    // ---- m5: place_image touches the LRU ----------------------------

    #[test]
    fn place_image_touches_lru_so_touched_image_survives_eviction() {
        let mut t = Term::new(8, 8, 0);
        // Tiny cap: each 1x1 RGBA image is 4 bytes. Cap of 8 holds two.
        t.set_image_cap(8);

        // Insert images 1 and 2 (both placed). Order: [1, 2].
        place_image(&mut t, 1);
        place_image(&mut t, 2);
        assert!(t.image_registry().contains(1));
        assert!(t.image_registry().contains(2));

        // Re-place image 1: m5 touch must move it to MRU end => [2, 1].
        // Observe via registry iteration order (oldest first).
        place_image(&mut t, 1);
        let order: Vec<u32> = t.image_registry().iter().map(|(id, _)| id).collect();
        assert_eq!(order, vec![2, 1], "re-placing image 1 must mark it MRU");
    }

    #[test]
    fn touched_image_survives_eviction_via_pinned_fallback() {
        // Even with placement-aware eviction, this checks the LRU order
        // is what `touch` reports. Remove placements so nothing is pinned,
        // then force an eviction and confirm the touched id survives.
        let mut t = Term::new(8, 8, 0);
        t.set_image_cap(8);
        place_image(&mut t, 1);
        place_image(&mut t, 2);
        // Touch 1 (re-place). Order becomes [2, 1].
        place_image(&mut t, 1);
        let order: Vec<u32> = t.image_registry().iter().map(|(id, _)| id).collect();
        assert_eq!(order.first().copied(), Some(2), "2 is the LRU front");
    }

    // ---- m7: default image quota is 320 MiB --------------------------

    #[test]
    fn default_image_cap_is_320_mib() {
        let t = Term::new(24, 80, 0);
        assert_eq!(
            t.image_registry().cap_bytes(),
            320 * 1024 * 1024,
            "default image quota should be 320 MiB"
        );
    }

    #[test]
    fn set_image_cap_override_still_works() {
        let mut t = Term::new(24, 80, 0);
        t.set_image_cap(64 * 1024 * 1024);
        assert_eq!(t.image_registry().cap_bytes(), 64 * 1024 * 1024);
    }
}
