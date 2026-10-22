//! Glyph rasterizer: cosmic-text shape → swash raster → GPU atlas upload.
//!
//! Wraps a [`cosmic_text::FontSystem`] + [`cosmic_text::SwashCache`] +
//! the dual-layer GPU atlas. Given a row of text and an SGR style, it
//! shapes the row, runs the cluster-width snap (decision §3), rasterizes
//! any missing glyphs via swash, uploads them into the appropriate atlas
//! layer (mask or color), and returns the atlas slots keyed by character
//! + style.

use cosmic_text::{
    Attrs, Buffer, CacheKey, Family, FontSystem, LayoutGlyph, Metrics, Shaping, SwashCache,
    SwashContent, Weight, fontdb,
};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use toastty_protocols::unicode_core::cluster_cell_width;
use unicode_width::UnicodeWidthChar;
use wgpu::{Device, Extent3d, Queue, Texture, TextureFormat};

use crate::text::atlas::{Atlas, AtlasLayer, AtlasSlot, GlyphKey};
use crate::text::cluster_width::{GlyphPos, snap_cluster_widths_per_cluster};
use crate::text::decoration::{DecorationMetrics, FontLineMetrics};
use crate::text::instance::GlyphSlot;
use crate::text::presentation;

/// Per-row cache slot: which char was shaped into this column and the
/// atlas slot we got back. Storing `char` keeps the same defense-in-depth
/// the old `HashMap<(col, char), GlyphSlot>` had — callers re-check the
/// char so a stale cache entry (e.g. wrong-char lookup) misses cleanly.
type ColumnSlot = Option<(char, GlyphSlot)>;

/// Chars below this code point get a dense entry in
/// [`GlyphRasterizer::fast_chars`]: Latin, Greek, Cyrillic, general
/// punctuation, arrows, box drawing, block elements and Braille (which
/// btop-style graphs are made of). Everything above falls back to the
/// hash lookups.
const FAST_CHARS_LEN: usize = 0x3000;

/// What the shaping fast path does with one char. A dense mirror of
/// `char_cache` / `char_cache_misses` / `uncacheable_chars` (plus the
/// width-1 test) so the per-char cost is one indexed load instead of up
/// to three `SipHash` lookups. Recomputed by
/// [`GlyphRasterizer::sync_fast_char`] whenever one of those sets
/// changes, so the sets stay the source of truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FastChar {
    /// Leave the fast path (not width 1, uncacheable, or not cached yet).
    Bail,
    /// No glyph: blank, NUL, or a recorded cache miss.
    Skip,
    /// Cached; index into [`GlyphRasterizer::fast_slots`].
    Cached(u32),
}

/// Mask atlas dims (R8) and color atlas dims (BGRA8) — generously sized
/// per the M4b "panic when full" policy.
pub const ATLAS_W: u32 = 1024;
pub const ATLAS_H: u32 = 1024;

/// Default line-height multiplier (× `font_size_px`). 1.20 is tight enough
/// that adjacent rows of box-drawing characters connect (`│` to `│`,
/// `┴` to `┬`) while still keeping enough breathing room above descenders
/// for most monospace fonts. The `toastty-config` schema defaults to the
/// same value.
pub const DEFAULT_LINE_HEIGHT_RATIO: f32 = 1.20;

struct PendingGlyph {
    glyph: LayoutGlyph,
    col: u16,
    ch: char,
    /// Every char in the source cluster this glyph belongs to. Stored so
    /// the miss-tracking loop can credit *all* chars in a ligature, not
    /// just the cluster's first char. Without this, e.g. shaping `!=` as
    /// a single ligature glyph (cluster chars `['!', '=']`, `ch == '!'`)
    /// would silently mark `=` as a permanent cache miss because it never
    /// appeared as a `p.ch`.
    cluster_chars: Vec<char>,
    /// `Some(offset)` when this glyph is a *secondary* glyph of a
    /// grapheme cluster (a combining mark, the 2nd+ glyph of an
    /// unligated ZWJ sequence): it belongs to the cell at `col` and is
    /// drawn at `offset` px from that cell's origin (on top of the
    /// glyph's own bearing). `None` for primary glyphs.
    secondary_offset: Option<[f32; 2]>,
    /// The glyph's cluster is a multi-codepoint grapheme (contains a
    /// zero-width codepoint). Such glyphs are never served from / barred
    /// from `char_cache`: the cluster's first char may have been shaped
    /// into a precomposed form (`e` + U+0301 → `é`), which must not be
    /// cached as the standalone glyph of `e`.
    in_grapheme: bool,
}

/// True when `cluster` is a multi-codepoint grapheme as stored by the
/// term grid — i.e. it contains a zero-width codepoint (combining mark,
/// ZWJ, variation selector, …). Ligature clusters (`!=`, `->`) are made of
/// width-1 chars, one per cell, and keep the per-column handling.
fn is_grapheme_cluster(cluster: &str) -> bool {
    use unicode_segmentation::UnicodeSegmentation;
    let mut chars = cluster.chars();
    if chars.next().is_none() || chars.next().is_none() {
        return false;
    }
    // Zero-width member (the grid always folds those into one cell), or a
    // single extended grapheme (regional-indicator pair, emoji modifier
    // sequence) — but not a run of separate graphemes (a ligature).
    cluster.chars().any(|c| c.width() == Some(0)) || cluster.graphemes(true).nth(1).is_none()
}

/// Per-glyph cell-char assignment. For each glyph in shaping order, pick
/// the cluster char that lands on the glyph's column.
///
/// Multi-glyph clusters from true ligatures (`!=` shaping into two
/// half-glyphs that share `(start=0, end=2)`, `cluster_chars=['!','=']`)
/// need each glyph to key under the cell char it actually sits over —
/// glyph 0 → `'!'`, glyph 1 → `'='`. Without this, the column-keyed
/// `LineGlyphs` lookup at render time misses on every cell after the
/// cluster's first column, and the trailing cells of the ligature render
/// blank.
///
/// `glyph_clusters` is `(byte_start, byte_end, col)` per glyph in the
/// order cosmic-text emitted them (assumed left-to-right within each
/// cluster); `text` is the run's source text. Returns `(ch, cluster_chars)`
/// for each input glyph in the same order.
fn assign_glyph_chars(
    glyph_clusters: &[(usize, usize, u16)],
    text: &str,
) -> Vec<(char, Vec<char>)> {
    let mut out = Vec::with_capacity(glyph_clusters.len());
    // (start, end, base_col, chars) — `base_col` is the first glyph's
    // col in the current cluster, so `col - base_col` indexes into
    // `chars` to pick the cell char this glyph belongs to.
    let mut current: Option<(usize, usize, u16, Vec<char>)> = None;
    for &(start, end, col) in glyph_clusters {
        let needs_new = current
            .as_ref()
            .is_none_or(|(s, e, _, _)| *s != start || *e != end);
        if needs_new {
            let cluster_str = text.get(start..end).unwrap_or("");
            current = Some((start, end, col, cluster_str.chars().collect()));
        }
        let (_, _, base_col, cluster_chars) = current.as_ref().expect("just set");
        let idx = col.saturating_sub(*base_col) as usize;
        let ch = cluster_chars
            .get(idx)
            .or_else(|| cluster_chars.last())
            .copied()
            .unwrap_or(' ');
        out.push((ch, cluster_chars.clone()));
    }
    out
}

/// Result of laying out a single line: each `(char, glyph_slot)` pair the
/// caller can plug into `build_instances`.
///
/// Stored densely as `by_column[col] = Some((ch, slot))`. Switched from
/// `HashMap<(col, char), GlyphSlot>` after profiling showed ~1% of
/// main-thread wall time in hash bucket lookups during `build_instances`,
/// plus per-row heap allocations populating the map. Columns are 0..N
/// over a row, so an indexed vec is a strict upgrade.
#[derive(Debug, Clone, Default)]
pub struct LineGlyphs {
    /// Slot per column. Indices outside the populated range are
    /// implicitly `None`. The vec is grown only as far as the highest
    /// column shaped — wide-character lines that skip over a column
    /// (CJK / combining-mark continuation cells) leave those slots
    /// `None`, matching the old "key absent" semantics.
    pub by_column: Vec<ColumnSlot>,
    /// Secondary glyphs of multi-glyph grapheme cells, as
    /// `(column, slot)` in shaping order. `slot.glyph_offset` already
    /// includes the glyph's pen position inside the cell. Looked up via
    /// [`crate::text::instance::secondary_glyph_key`]; empty for the
    /// overwhelmingly common line with no combining marks / ZWJ tails.
    pub secondary: Vec<(u16, GlyphSlot)>,
}

impl LineGlyphs {
    /// Look up the slot for `col` matching `ch`. Returns `None` if the
    /// column is out of range, was never shaped, or holds a different
    /// char — same semantics as the old `HashMap::get(&(col, ch))`.
    #[must_use]
    pub fn get(&self, col: u16, ch: char) -> Option<GlyphSlot> {
        match self.by_column.get(col as usize) {
            Some(Some((c, slot))) if *c == ch => Some(*slot),
            _ => {
                let k = crate::text::instance::secondary_glyph_index(ch)?;
                self.secondary
                    .iter()
                    .filter(|(c, _)| *c == col)
                    .nth(k)
                    .map(|(_, slot)| *slot)
            }
        }
    }

    /// Append a secondary glyph for `col` (see [`Self::secondary`]).
    ///
    /// Flags the column's primary slot with `has_secondary` so the
    /// instance builders know to fetch it; a secondary for a column with
    /// no primary glyph is dropped (the builders only look secondaries up
    /// behind a primary).
    pub fn push_secondary(&mut self, col: u16, slot: GlyphSlot) {
        if let Some(Some((_, primary))) = self.by_column.get_mut(col as usize) {
            primary.has_secondary = true;
            self.secondary.push((col, slot));
        }
    }

    /// Insert a slot at `col`. Grows the underlying vec as needed.
    pub fn insert(&mut self, col: u16, ch: char, slot: GlyphSlot) {
        let idx = col as usize;
        if self.by_column.len() <= idx {
            self.by_column.resize(idx + 1, None);
        }
        self.by_column[idx] = Some((ch, slot));
    }
}

/// Cached layout per (text, style fingerprint). M4b doesn't actually
/// cache rows — every render shapes fresh. That's fine for the demo;
/// the dirty-set optimization is in M5.
/// Per-glyph pixel placement relative to a cell origin. Stays in a
/// parallel cache to the atlas slot so cache-hit paths can rebuild the
/// `GlyphSlot` without re-rasterizing.
#[derive(Debug, Clone, Copy)]
struct GlyphPlacement {
    offset: [f32; 2],
    size: [f32; 2],
}

#[derive(Debug)]
pub struct GlyphRasterizer {
    font_system: FontSystem,
    swash_cache: SwashCache,
    atlas: Atlas,
    /// GPU texture for monochrome (R8) glyphs.
    mask_texture: Texture,
    /// GPU texture for color (BGRA8) glyphs.
    color_texture: Texture,
    /// Placement (bearing + extent) cache keyed by `GlyphKey`. Parallel
    /// to `atlas`'s slot cache; same lifetime semantics.
    placements: HashMap<GlyphKey, GlyphPlacement>,
    /// Per-line buffer reused across shapings to avoid re-allocating.
    buffer: Buffer,
    /// Font metrics in pixels (`size`, `line_height`). Retained so
    /// future work (live size change, viewport recompute) doesn't have
    /// to re-derive them. Not currently read post-construction.
    #[allow(dead_code)]
    metrics: Metrics,
    /// Logical cell size in pixels (width × height). Width is computed
    /// after shaping the reference monospace glyph.
    cell_size: (f32, f32),
    /// Baseline offset from the cell's top edge, in pixels — a **constant**
    /// taken from the primary font's reference glyph (see [`measure_cell`]).
    ///
    /// Every glyph's vertical placement is `baseline_y - glyph.top`. This
    /// MUST NOT be recomputed per line from `run.line_y - run.line_top`:
    /// that value is cosmic-text's per-line `max_ascent`/`max_descent`,
    /// which grows whenever a line mixes in a taller fallback-font glyph
    /// (CJK, emoji, Nerd-Font icon). Because the resulting offset is cached
    /// per glyph and reused on every line, a per-line baseline made a glyph
    /// that was first rasterized next to a fallback glyph render lower than
    /// its neighbours *everywhere*, forever. Pinning to the primary font's
    /// baseline keeps every cell on the same grid baseline. See the
    /// `glyph_offset_is_independent_of_line_content` regression test.
    baseline_y: f32,
    /// Strikethrough / overline / double-underline geometry derived from
    /// the primary face's line metrics (see [`measure_decorations`]).
    decoration_metrics: DecorationMetrics,
    /// Configured font family, kept around for `Attrs`.
    family_name: String,
    /// Weight of the face [`Self::family_name`] resolves to (see
    /// [`resolve_family_weight`]). Every `Attrs` naming the primary family
    /// carries it, so a single-weight face such as the bundled Fira Mono
    /// Medium is actually selected instead of a regular-weight fallback.
    primary_weight: Weight,
    /// Per-character glyph cache. Keyed on `char` alone, which is **only
    /// correct under three invariants**:
    ///   1. Single-weight font (no bold/italic switching). Bold/italic
    ///      glyphs are different shapes; adding font-weight selection
    ///      requires re-keying as `(char, style_fingerprint)`.
    ///   2. No NFD combining marks. `é` as `e + U+0301` would render as
    ///      `e` and a separately-cached `U+0301` placed in the next
    ///      column — visually wrong. Pre-composed `é` (U+00E9) is fine.
    ///   3. Cell width == 1. Wide chars (CJK, fullwidth) and zero-width
    ///      chars (control, combining) must not be cached at all.
    ///      Enforced by the `unicode_width::UnicodeWidthChar::width(c)
    ///      != Some(1)` bail in the fast path.
    ///
    /// M6+ font-weight switching will require lifting invariant #1
    /// by re-keying the cache. NFD inputs (#2) and complex shaping
    /// require dropping the fast path entirely and going through
    /// cosmic-text + the cluster-width snap.
    ///
    /// Populated as `shape_line` resolves each character through
    /// cosmic-text the first time; subsequent lines containing only
    /// cached single-column characters skip the (expensive)
    /// cosmic-text layout call entirely. The per-line layout cost
    /// dominates the renderer's per-frame budget at fullscreen
    /// (~110 µs / row of ASCII on M4 Pro).
    char_cache: HashMap<char, GlyphSlot>,
    /// Characters known to be uncacheable (cosmic-text returned no
    /// glyph for them — e.g., zero-width spaces). Recording them stops
    /// repeated misses from triggering the slow path forever.
    char_cache_misses: std::collections::HashSet<char>,
    /// Characters whose in-context glyph differs from their standalone
    /// shape — i.e. the font applies GSUB substitution (ligatures /
    /// contextual alternates) to them. Adding such a char to this set
    /// forces every line containing it through the slow path, so we
    /// always shape the correct contextual glyphs and never serve the
    /// wrong substituted shape out of `char_cache`. Without this guard
    /// a Fira Code-style `!=` ligature (two single-char-cluster glyphs
    /// representing the left and right halves of `≠`) would poison
    /// `char_cache['!']` and `char_cache['=']` with their ligature
    /// shapes — making subsequent standalone `=` render as `≠` and the
    /// `!` cell render blank.
    uncacheable_chars: std::collections::HashSet<char>,
    /// Lazy cache of each char's standalone glyph id — the glyph id
    /// produced when the char is shaped alone, without any preceding
    /// or following chars to trigger GSUB substitution. Used by the
    /// slow path to detect when an in-context glyph is a contextual
    /// variant (different id ⇒ substituted ⇒ don't cache, mark char
    /// uncacheable).
    standalone_glyph_ids: HashMap<char, Option<u16>>,
    /// Single-char buffer used by [`Self::standalone_glyph_id_of`] for
    /// isolation shapes. Kept separate from `buffer` so we don't clobber
    /// the in-progress full-line shape inside `shape_line_slow`.
    isolation_buffer: Buffer,
    /// Per-char cache of the font family to force for **text-presentation**
    /// emoji (UTS #51). `Some(family)` steers a codepoint whose default
    /// presentation is text (e.g. ⏺ U+23FA) away from the color-emoji face
    /// cosmic-text's presentation-unaware fallback would otherwise pick,
    /// toward a monochrome face that covers it. `Some(None)` records "no
    /// override needed" (real emoji, or a codepoint the primary font
    /// already covers). Keyed on the bare codepoint; variation-selected
    /// clusters are rare and computed live. See [`presentation`].
    presentation_override: HashMap<char, Option<String>>,
    /// Per-char cache of the color face to force for `base + VS16` under
    /// DECSET 2027 (see [`Self::vs16_color_family`]). `Some(None)` records
    /// "no override needed / none available".
    vs16_color_override: HashMap<char, Option<String>>,
    /// True iff the family name requested by the caller resolved to at
    /// least one face in the loaded font database. When false, cosmic-
    /// text will fall back to whatever the system considers a default
    /// monospace face — the renderer still works, but the user's
    /// `font.family` config didn't take effect. Surfaced via
    /// [`GlyphRasterizer::requested_family_available`].
    requested_family_available: bool,
    /// Dense fast-path state for chars below [`FAST_CHARS_LEN`]; see
    /// [`FastChar`].
    fast_chars: Box<[FastChar]>,
    /// Slots the [`FastChar::Cached`] entries point at. Append-only
    /// between [`Self::rebuild_fast_chars`] calls.
    fast_slots: Vec<GlyphSlot>,
}

impl GlyphRasterizer {
    /// Build a rasterizer with `font_size` in pixels, the
    /// line-height ratio (× `font_size`), and an optional font name
    /// (falls back to monospace if absent or unloaded).
    ///
    /// Also bundles a "fallback" TTF as a guaranteed-present monospace
    /// face — see `Renderer::with_font`.
    ///
    /// Pass [`DEFAULT_LINE_HEIGHT_RATIO`] for the value the M4b snapshots
    /// were captured at.
    pub fn new(
        device: &Device,
        font_size: f32,
        line_height_ratio: f32,
        font_name: Option<&str>,
        bundled_font: Option<&[u8]>,
    ) -> Self {
        let mut font_system = FontSystem::new();

        if let Some(data) = bundled_font {
            font_system
                .db_mut()
                .load_font_source(cosmic_text::fontdb::Source::Binary(std::sync::Arc::new(
                    data.to_vec(),
                )));
        }

        // Line height = `font_size * line_height_ratio`, snapped to a
        // whole device pixel. Per-row origins are `row * cell_h`, so a
        // fractional cell height would land every row on a different
        // sub-pixel offset — under the atlas's Nearest sampling that
        // smears glyph stems unpredictably (the "fuzzy / shimmery"
        // artifact, worst on low-DPI monitors where one pixel is large).
        // Feeding the rounded value into `Metrics` keeps cosmic-text's
        // baseline math consistent with the cell we actually draw. See
        // [`DEFAULT_LINE_HEIGHT_RATIO`] for the rationale on the default.
        let line_height_px = (font_size * line_height_ratio).round().max(1.0);
        let metrics = Metrics::new(font_size, line_height_px);

        // Determine cell size by shaping the reference glyph "M". Its
        // advance is snapped to a whole pixel (in `measure_cell`) for the
        // same pixel-grid-alignment reason as the height above. The same
        // probe yields the constant baseline used for every glyph's
        // vertical placement (see [`Self::baseline_y`]).
        let primary_weight = resolve_family_weight(&font_system, font_name);
        let (cell_w, cell_h, baseline_y) =
            measure_cell(&mut font_system, metrics, font_name, primary_weight);
        let cell_size = (cell_w, cell_h);
        let decoration_metrics = measure_decorations(
            &mut font_system,
            metrics,
            font_name,
            primary_weight,
            cell_h,
            baseline_y,
        );

        // Pre-allocate the per-line buffer.
        let mut buffer = Buffer::new(&mut font_system, metrics);
        buffer.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
        // Tell cosmic-text we want monospace-style positioning; the
        // cluster-width snap fixes its per-glyph rounding (decision §3).
        buffer.set_monospace_width(Some(cell_size.0));

        // Tiny buffer reused for single-char standalone shapes. Same
        // monospace-width hint so isolated glyphs land at x=0.
        let mut isolation_buffer = Buffer::new(&mut font_system, metrics);
        isolation_buffer.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
        isolation_buffer.set_monospace_width(Some(cell_size.0));

        let swash_cache = SwashCache::new();
        let atlas = Atlas::new(ATLAS_W, ATLAS_H);
        let mask_texture =
            create_atlas_texture(device, TextureFormat::R8Unorm, "toastty-mask-atlas");
        let color_texture =
            create_atlas_texture(device, TextureFormat::Bgra8Unorm, "toastty-color-atlas");

        let family_name = font_name.unwrap_or("monospace").to_string();

        // Check whether the requested family is actually present in
        // the font database. `db().faces()` walks every loaded face;
        // we report whether any face's `families` list contains a
        // case-insensitive match. The bundled fallback TTF advertises
        // its own family name (e.g. "Fira Mono"), so requesting that
        // family will resolve even on a host with no system fonts of
        // the same name. Used by the binary to log a warning when the
        // user's `font.family` config doesn't match anything.
        let requested_family_available = font_name.is_none_or(|requested| {
            font_system.db().faces().any(|face| {
                face.families
                    .iter()
                    .any(|(name, _)| name.eq_ignore_ascii_case(requested))
            })
        });

        Self {
            font_system,
            swash_cache,
            atlas,
            mask_texture,
            color_texture,
            placements: HashMap::new(),
            buffer,
            metrics,
            cell_size,
            baseline_y,
            decoration_metrics,
            family_name,
            primary_weight,
            char_cache: HashMap::new(),
            char_cache_misses: std::collections::HashSet::new(),
            uncacheable_chars: std::collections::HashSet::new(),
            standalone_glyph_ids: HashMap::new(),
            isolation_buffer,
            presentation_override: HashMap::new(),
            vs16_color_override: HashMap::new(),
            requested_family_available,
            fast_chars: initial_fast_chars(),
            fast_slots: Vec::new(),
        }
    }

    /// Re-target the rasterizer at a new pixel size in place (runtime font
    /// zoom, DPI change), keeping the loaded font database.
    ///
    /// [`Self::new`] calls `FontSystem::new()`, which enumerates and parses
    /// every system font; doing that on every zoom keypress would cost tens
    /// of milliseconds and churn the allocator. Everything that depends on
    /// the size is rebuilt: metrics, cell size and baseline (re-measured
    /// exactly as `new` does, so the result is identical to a fresh
    /// rasterizer), the shaping buffers, the swash bitmap cache (keyed per
    /// size, so it would otherwise grow with every distinct size visited),
    /// the atlas packers, placements and the per-char slot cache. The
    /// atlas textures are reused: a fresh packer re-uploads every glyph it
    /// hands out, and sampling never reads outside a glyph's own rect.
    ///
    /// Size-independent caches survive: standalone glyph ids, the
    /// ligature-uncacheable and no-glyph sets and the presentation/VS16
    /// family overrides only depend on the font's cmap/GSUB, not on size.
    pub fn set_font_size(&mut self, font_size: f32, line_height_ratio: f32) {
        let line_height_px = (font_size * line_height_ratio).round().max(1.0);
        let metrics = Metrics::new(font_size, line_height_px);
        let family = self.family_name.clone();
        let (cell_w, cell_h, baseline_y) = measure_cell(
            &mut self.font_system,
            metrics,
            Some(family.as_str()),
            self.primary_weight,
        );
        for buf in [&mut self.buffer, &mut self.isolation_buffer] {
            *buf = Buffer::new(&mut self.font_system, metrics);
            buf.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
            buf.set_monospace_width(Some(cell_w));
        }
        self.metrics = metrics;
        self.cell_size = (cell_w, cell_h);
        self.baseline_y = baseline_y;
        self.swash_cache = SwashCache::new();
        self.atlas = Atlas::new(ATLAS_W, ATLAS_H);
        self.placements.clear();
        self.char_cache.clear();
        self.rebuild_fast_chars();
    }

    /// Recompute `fast_chars[ch]` from the char sets, with the same
    /// precedence the fast path used to apply to them one by one:
    /// blank → width ≠ 1 → uncacheable → miss → cached → unknown.
    fn sync_fast_char(&mut self, ch: char) {
        let Some(i) = fast_char_index(ch) else {
            return;
        };
        self.fast_chars[i] = if ch == ' ' || ch == '\0' {
            FastChar::Skip
        } else if ch.width() != Some(1) || self.uncacheable_chars.contains(&ch) {
            FastChar::Bail
        } else if self.char_cache_misses.contains(&ch) {
            FastChar::Skip
        } else if let Some(slot) = self.char_cache.get(&ch) {
            #[allow(clippy::cast_possible_truncation)] // < one entry per cache insert
            let idx = self.fast_slots.len() as u32;
            self.fast_slots.push(*slot);
            FastChar::Cached(idx)
        } else {
            FastChar::Bail
        };
    }

    /// Rebuild the whole [`Self::fast_chars`] table (after `char_cache`
    /// was cleared).
    fn rebuild_fast_chars(&mut self) {
        self.fast_chars = initial_fast_chars();
        self.fast_slots.clear();
        let chars: Vec<char> = self
            .char_cache
            .keys()
            .chain(self.char_cache_misses.iter())
            .chain(self.uncacheable_chars.iter())
            .copied()
            .collect();
        for ch in chars {
            self.sync_fast_char(ch);
        }
    }

    /// Cache `slot` as the standalone glyph of `ch`, unless one is
    /// cached already.
    fn cache_standalone(&mut self, ch: char, slot: GlyphSlot) {
        if let Entry::Vacant(e) = self.char_cache.entry(ch) {
            e.insert(slot);
            self.sync_fast_char(ch);
        }
    }

    /// Record `ch` as having no glyph. Returns true if it was new.
    fn mark_cache_miss(&mut self, ch: char) -> bool {
        let new = self.char_cache_misses.insert(ch);
        if new {
            self.sync_fast_char(ch);
        }
        new
    }

    /// Record `ch` as contextually shaped (see `uncacheable_chars`) and
    /// evict its `char_cache` entry. Returns true if it was new.
    fn mark_uncacheable(&mut self, ch: char) -> bool {
        let new = self.uncacheable_chars.insert(ch);
        if new {
            self.char_cache.remove(&ch);
            self.sync_fast_char(ch);
        }
        new
    }

    /// Glyph id this char would produce shaped alone, with no
    /// surrounding context to trigger GSUB ligatures or contextual
    /// alternates. `None` means the font has no glyph for this
    /// codepoint (rare, but we don't want to crash on it).
    ///
    /// Cached after the first lookup — every subsequent call is a
    /// HashMap hit. Used by [`Self::shape_line_slow`]'s caching block
    /// to detect contextual substitution: if the in-context glyph id
    /// differs from this standalone id, we know the font substituted
    /// the glyph (e.g. as part of a ligature) and we must not cache
    /// the contextual form under the bare-char key.
    fn standalone_glyph_id_of(&mut self, ch: char) -> Option<u16> {
        if let Some(cached) = self.standalone_glyph_ids.get(&ch) {
            return *cached;
        }

        // Shape with the same presentation-steered family the slow path
        // would use for this char. Without this, a text-presentation emoji
        // (e.g. ⏺) whose in-context glyph comes from a forced monochrome
        // face would never match its color-fallback standalone id, so it
        // would be flagged contextual and forced through the slow path on
        // every line forever. Steering both shapes the same way keeps it
        // cacheable.
        let family_name = self
            .presentation_family(ch, false, false)
            .unwrap_or_else(|| self.family_name.clone());
        let attrs = self.attrs_for(&family_name);

        let mut s = String::with_capacity(4);
        s.push(ch);

        // Split-borrow disjoint fields so we can pass `&mut font_system`
        // into the buffer method without aliasing.
        let font_system = &mut self.font_system;
        let isolation_buffer = &mut self.isolation_buffer;
        isolation_buffer.set_text(&s, &attrs, Shaping::Advanced, None);
        isolation_buffer.shape_until_scroll(font_system, false);

        let id = isolation_buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(|g| g.glyph_id));

        self.standalone_glyph_ids.insert(ch, id);
        id
    }

    /// Font family to force for the cluster led by `base`, or `None` to use
    /// the default fallback path. See [`presentation`] and
    /// [`Self::presentation_override`]. The bare (no variation selector)
    /// result is cached; VS-bearing clusters are rare and computed live.
    fn presentation_family(
        &mut self,
        base: char,
        has_vs15: bool,
        has_vs16: bool,
    ) -> Option<String> {
        // Only the no-variation-selector result is cached (keyed on the
        // bare codepoint); selected clusters are rare enough to recompute.
        let cacheable = !has_vs15 && !has_vs16;
        if cacheable && let Some(cached) = self.presentation_override.get(&base) {
            return cached.clone();
        }
        let result = self.compute_presentation_family(base, has_vs15, has_vs16);
        if cacheable {
            self.presentation_override.insert(base, result.clone());
        }
        result
    }

    /// Uncached core of [`Self::presentation_family`]: decide whether this
    /// cluster wants text presentation and, if cosmic-text's default
    /// fallback would (wrongly) render it from a color face, find a
    /// monochrome face to steer toward.
    fn compute_presentation_family(
        &mut self,
        base: char,
        has_vs15: bool,
        has_vs16: bool,
    ) -> Option<String> {
        if !presentation::wants_text_presentation(base, has_vs15, has_vs16) {
            return None;
        }
        // If the default fallback already yields a monochrome glyph (the
        // primary font, or a non-color fallback, covers `base`), leave it
        // be — no need to override.
        let font_id = self.default_resolved_font(base)?;
        if !presentation::face_is_color(self.font_system.db(), font_id) {
            return None;
        }
        // Default fallback landed on a color face but this codepoint wants
        // text: steer toward a monochrome face that covers it, if one
        // exists. If none does, return `None` and accept the default
        // (color) rather than rendering tofu.
        presentation::find_text_face(self.font_system.db(), base)
    }

    /// Color face to steer `base + VS16` toward, when the default fallback
    /// would draw `base` from a monochrome face (❤ U+2764 is in `DejaVu`, so
    /// `❤\u{FE0F}` otherwise renders as a text heart).
    ///
    /// Only used under DECSET 2027, where the grid gives a VS16 cluster
    /// two cells (`cluster_cell_width`) so the color glyph has room. In
    /// legacy mode the cluster stays one cell wide (wcwidth), and the
    /// monochrome text glyph is the one that fits it.
    fn vs16_color_family(&mut self, base: char) -> Option<String> {
        if let Some(cached) = self.vs16_color_override.get(&base) {
            return cached.clone();
        }
        let result = self.default_resolved_font(base).and_then(|id| {
            let db = self.font_system.db();
            if presentation::face_is_color(db, id) {
                None
            } else {
                presentation::find_color_face(db, base)
            }
        });
        self.vs16_color_override.insert(base, result.clone());
        result
    }

    /// Font id cosmic-text's default fallback resolves `ch` to when shaped
    /// alone with the configured family. Used to detect when the default
    /// path lands on a color-emoji face. `None` if nothing renders.
    fn default_resolved_font(&mut self, ch: char) -> Option<fontdb::ID> {
        let family_name = self.family_name.clone();
        let attrs = self.attrs_for(&family_name);

        let mut s = String::with_capacity(4);
        s.push(ch);

        let font_system = &mut self.font_system;
        let isolation_buffer = &mut self.isolation_buffer;
        isolation_buffer.set_text(&s, &attrs, Shaping::Advanced, None);
        isolation_buffer.shape_until_scroll(font_system, false);

        isolation_buffer
            .layout_runs()
            .next()
            .and_then(|run| run.glyphs.first().map(|g| g.font_id))
    }

    /// Segment `text` into byte-range runs that share a forced family, or
    /// `None` if no cluster needs presentation steering (the overwhelmingly
    /// common case — a single cheap scan rejects ASCII-only lines).
    ///
    /// Attaching chars (variation selectors, ZWJ, combining marks) inherit
    /// the preceding base's family so a `base + VS` cluster shapes as one
    /// run; a base's choice is computed with awareness of a following VS15
    /// / VS16. Adjacent equal choices are coalesced.
    fn compute_presentation_runs(
        &mut self,
        text: &str,
        mode_2027_active: bool,
    ) -> Option<Vec<(usize, usize, Option<String>)>> {
        if !text.chars().any(presentation::is_candidate) {
            return None;
        }

        let indices: Vec<(usize, char)> = text.char_indices().collect();
        let mut choices: Vec<Option<String>> = Vec::with_capacity(indices.len());
        let mut any_override = false;
        for i in 0..indices.len() {
            let (_, c) = indices[i];
            if presentation::is_attaching(c) {
                // Inherit the base cluster's family.
                choices.push(choices.last().cloned().flatten());
                continue;
            }
            let next = indices.get(i + 1).map(|&(_, c)| c);
            let has_vs16 = next == Some(presentation::VS16);
            let has_vs15 = next == Some(presentation::VS15);
            let fam = if has_vs16 && mode_2027_active {
                self.vs16_color_family(c)
            } else {
                self.presentation_family(c, has_vs15, has_vs16)
            };
            any_override |= fam.is_some();
            choices.push(fam);
        }
        if !any_override {
            return None;
        }

        let mut runs: Vec<(usize, usize, Option<String>)> = Vec::new();
        for (i, (byte, _)) in indices.iter().enumerate() {
            let end = indices.get(i + 1).map_or(text.len(), |&(b, _)| b);
            let choice = choices[i].clone();
            match runs.last_mut() {
                Some(last) if last.2 == choice => last.1 = end,
                _ => runs.push((*byte, end, choice)),
            }
        }
        Some(runs)
    }

    /// Load `text` into the shaping buffer and shape it, applying emoji
    /// presentation steering when needed.
    ///
    /// Most lines need none (a single cheap scan rejects ASCII-only text),
    /// in which case we shape with the plain configured family exactly as
    /// before. When a line contains a text-presentation emoji that would
    /// otherwise resolve to a color face, we shape via rich-text spans that
    /// force the steered monochrome family for just those clusters.
    fn shape_buffer_for_line(&mut self, text: &str, mode_2027_active: bool) {
        let runs = self.compute_presentation_runs(text, mode_2027_active);
        let default_family = self.family_name.clone();
        let primary_weight = self.primary_weight;
        let default_attrs = family_attrs(&default_family, &default_family, primary_weight);
        match &runs {
            Some(runs) => {
                let spans = runs.iter().map(|(start, end, fam)| {
                    let name = fam.as_deref().unwrap_or(default_family.as_str());
                    (
                        &text[*start..*end],
                        family_attrs(name, &default_family, primary_weight),
                    )
                });
                self.buffer
                    .set_rich_text(spans, &default_attrs, Shaping::Advanced, None);
            }
            None => {
                self.buffer
                    .set_text(text, &default_attrs, Shaping::Advanced, None);
            }
        }
        self.buffer.shape_until_scroll(&mut self.font_system, false);
    }

    /// True when the family name passed to [`Self::new`] resolved to a
    /// real face. `false` means cosmic-text will fall back to the
    /// host's default monospace; callers can log a warning so the user
    /// notices their config didn't take effect.
    /// `Attrs` for shaping with `family`: the primary family carries
    /// [`Self::primary_weight`]; any other (presentation / emoji fallback)
    /// family keeps the default weight.
    fn attrs_for<'a>(&self, family: &'a str) -> Attrs<'a> {
        family_attrs(family, &self.family_name, self.primary_weight)
    }

    pub fn requested_family_available(&self) -> bool {
        self.requested_family_available
    }

    /// Family name as requested by the caller (or `"monospace"` if the
    /// caller passed `None`). Useful for emitting a "font X not found"
    /// log line.
    pub fn family_name(&self) -> &str {
        &self.family_name
    }

    /// Cell size in pixels (width, height).
    pub fn cell_size(&self) -> (f32, f32) {
        self.cell_size
    }

    /// Font-derived geometry for SGR strikethrough, overline and double
    /// underline at the current size.
    #[must_use]
    pub fn decoration_metrics(&self) -> DecorationMetrics {
        self.decoration_metrics
    }

    /// Atlas dimensions per layer (same for both).
    pub fn atlas_dims(&self) -> (u32, u32) {
        self.atlas.dimensions()
    }

    /// Borrow the GPU textures (for binding).
    pub fn mask_texture(&self) -> &Texture {
        &self.mask_texture
    }

    pub fn color_texture(&self) -> &Texture {
        &self.color_texture
    }

    /// Shape `text` as one line and ensure every glyph is in the atlas.
    /// Returns per-column glyph slots keyed by `(column, char)`.
    ///
    /// `mode_2027_active` controls cluster-width semantics for the
    /// per-cluster snap: when true, the renderer trusts the
    /// grapheme-segmenter's answer (VS16 emoji, ZWJ family etc. snap
    /// to 2 cells); when false it falls back to the legacy `wcwidth`
    /// table (which still gives 2 for plain CJK / fullwidth chars).
    ///
    /// Fast path: if every non-space character is already in
    /// [`char_cache`](Self::char_cache), we skip cosmic-text entirely
    /// and just pack the cached slots by column. Spaces are skipped (no
    /// glyph). This avoids the per-line layout cost — measured at ~110
    /// µs per row of ASCII on M4 Pro — which dominates the renderer's
    /// per-frame budget at fullscreen.
    ///
    /// Slow path: full cosmic-text shaping. Used the first time a
    /// character is seen and any time a line contains an
    /// uncached-and-non-miss character. Populates the cache so future
    /// lines hit the fast path.
    pub fn shape_line(&mut self, queue: &Queue, text: &str, mode_2027_active: bool) -> LineGlyphs {
        if let Some(line) = self.try_shape_line_fast(text) {
            return line;
        }
        self.shape_line_slow(queue, text, mode_2027_active)
    }

    /// Fast path: per-character lookups against `char_cache`. Returns
    /// `None` if any character in `text` would require cosmic-text
    /// (cache miss for a character that has not been marked
    /// uncacheable, or any non-single-column character). When it
    /// returns `Some`, no cosmic-text work was done.
    fn try_shape_line_fast(&self, text: &str) -> Option<LineGlyphs> {
        let cell_w = self.cell_size.0;
        if cell_w <= 0.0 {
            return None;
        }
        // Trailing blanks never produce a slot and never bail, so only
        // the trimmed prefix decides the result; this also sizes
        // `by_column` to the populated range up front instead of growing
        // it slot by slot.
        let text = text.trim_end_matches([' ', '\0']);
        let mut by_column: Vec<ColumnSlot> = Vec::with_capacity(text.len());
        // Each character in a monospace terminal grid occupies one
        // cell. We assign columns by index in the iteration order; the
        // term's row text is fed cell-by-cell, so the i'th char is
        // column i. This breaks for wide / zero-width characters
        // (CJK ideographs, combining marks, controls): a wide char
        // would span two columns, and a combining mark would consume a
        // column index without advancing the cell. Such chars bail to
        // the slow path (`FastChar::Bail`, or the `width` test in
        // `fast_lookup_hashed`).
        for (col, ch) in text.chars().enumerate() {
            if col > u16::MAX as usize {
                return None;
            }
            let slot = match fast_char_index(ch) {
                Some(i) => match self.fast_chars[i] {
                    FastChar::Bail => return None,
                    FastChar::Skip => None,
                    FastChar::Cached(idx) => Some(self.fast_slots[idx as usize]),
                },
                None => self.fast_lookup_hashed(ch)?,
            };
            by_column.push(slot.map(|s| (ch, s)));
        }
        // `text` ends in a non-blank char, so the last column is either a
        // slot or a skipped miss; drop trailing misses to match
        // `LineGlyphs::insert`'s "grown only as far as the highest column
        // shaped".
        while by_column.last().is_some_and(Option::is_none) {
            by_column.pop();
        }
        Some(LineGlyphs {
            by_column,
            secondary: Vec::new(),
        })
    }

    /// Fast-path verdict for a char outside the dense `fast_chars`
    /// table: `None` = bail to the slow path, `Some(None)` = no glyph,
    /// `Some(Some(slot))` = cached.
    #[allow(clippy::option_option)] // three outcomes, read in one `?`
    fn fast_lookup_hashed(&self, ch: char) -> Option<Option<GlyphSlot>> {
        // Width-1 invariant — see `char_cache` doc comment.
        if ch.width() != Some(1) {
            return None;
        }
        // Char has been observed shaping to a contextual glyph
        // (ligature / GSUB substitution). We can't serve it from
        // `char_cache` — the cached single-char form might be the
        // wrong half of a ligature, or the line we're rendering
        // *now* might want a contextual form. Bail so the slow
        // path does the right shaping each frame.
        if self.uncacheable_chars.contains(&ch) {
            return None;
        }
        // Already-known miss: skip the cell, but keep going on
        // the fast path. This stops e.g. control characters from
        // forcing every line through cosmic-text forever.
        if self.char_cache_misses.contains(&ch) {
            return Some(None);
        }
        self.char_cache.get(&ch).map(|slot| Some(*slot))
    }

    /// Slow path: cosmic-text shaping. Populates `char_cache` /
    /// `char_cache_misses` with whatever cosmic-text resolves so that
    /// future calls with the same characters can take the fast path.
    fn shape_line_slow(&mut self, queue: &Queue, text: &str, mode_2027_active: bool) -> LineGlyphs {
        self.shape_buffer_for_line(text, mode_2027_active);

        let cell_w = self.cell_size.0;

        let mut pending: Vec<PendingGlyph> = Vec::new();

        for run in self.buffer.layout_runs() {
            // Vertical placement uses the constant primary-font baseline
            // (`self.baseline_y`), NOT this run's `line_y - line_top` —
            // see [`Self::baseline_y`] for why the per-line value caused
            // glyphs to drift lower than their neighbours.
            let positions: Vec<GlyphPos> = run
                .glyphs
                .iter()
                .map(|g| GlyphPos {
                    start: g.start,
                    end: g.end,
                    x: g.x,
                    w: g.w,
                })
                .collect();
            // Compute the per-cluster cell width. Walk `positions`
            // grouping by `(start, end)`; for each group, look up the
            // cluster substring in the original `run.text` and ask
            // `cluster_cell_width` (mode-2027 aware) for its width.
            let mut widths: Vec<u8> = Vec::new();
            {
                let mut i = 0;
                while i < positions.len() {
                    let start = positions[i].start;
                    let end = positions[i].end;
                    let mut j = i + 1;
                    while j < positions.len()
                        && positions[j].start == start
                        && positions[j].end == end
                    {
                        j += 1;
                    }
                    let cluster_str = run.text.get(start..end).unwrap_or("");
                    widths.push(cluster_cell_width(cluster_str, mode_2027_active));
                    i = j;
                }
            }
            let snapped = snap_cluster_widths_per_cluster(&positions, cell_w, &widths);
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let glyph_clusters: Vec<(usize, usize, u16)> = run
                .glyphs
                .iter()
                .zip(snapped.iter())
                .map(|(g, snap)| (g.start, g.end, (snap.x / cell_w).round() as u16))
                .collect();
            let assignments = assign_glyph_chars(&glyph_clusters, run.text);
            // Grapheme clusters (base + zero-width codepoints, one grid
            // cell): every glyph belongs to the cell of the cluster's
            // first glyph. The first is the primary (keyed by the base
            // char); the rest are secondary glyphs positioned relative to
            // that cell — their snapped pen x within the cluster plus the
            // shaper's mark-attachment offset (`x_offset` / `y_offset`,
            // in em units, scaled by font size as `LayoutGlyph::physical`
            // does).
            let mut grapheme_base: Option<(usize, usize, u16)> = None;
            for (((g, &(_, _, col)), (ch, cluster_chars)), snap) in run
                .glyphs
                .iter()
                .zip(glyph_clusters.iter())
                .zip(assignments.into_iter())
                .zip(snapped.iter())
            {
                let cluster_str = run.text.get(g.start..g.end).unwrap_or("");
                let in_grapheme = is_grapheme_cluster(cluster_str);
                let mut col = col;
                let mut ch = ch;
                let mut secondary_offset = None;
                if in_grapheme {
                    match grapheme_base {
                        Some((s, e, base_col)) if s == g.start && e == g.end => {
                            col = base_col;
                            #[allow(clippy::cast_precision_loss)]
                            let cell_x = f32::from(base_col) * cell_w;
                            secondary_offset = Some([
                                (snap.x - cell_x + g.font_size * g.x_offset).round(),
                                (-g.font_size * g.y_offset).round(),
                            ]);
                        }
                        _ => {
                            grapheme_base = Some((g.start, g.end, col));
                            ch = cluster_str.chars().next().unwrap_or(ch);
                        }
                    }
                }
                pending.push(PendingGlyph {
                    glyph: g.clone(),
                    col,
                    ch,
                    cluster_chars,
                    secondary_offset,
                    in_grapheme,
                });
            }
        }

        // Track which characters in the input source line had a glyph
        // produced. Anything *not* in this set is a cosmic-text miss
        // (e.g. zero-width spaces, control chars, characters mapped to
        // the .notdef glyph) and we record it as a miss so the fast
        // path stops trying.
        let mut produced_chars: std::collections::HashSet<char> = std::collections::HashSet::new();

        let mut out = LineGlyphs::default();
        for p in &pending {
            if let Some(slot) = self.ensure_atlas_slot(queue, &p.glyph) {
                if let Some([dx, dy]) = p.secondary_offset {
                    let mut slot = slot;
                    slot.glyph_offset[0] += dx;
                    slot.glyph_offset[1] += dy;
                    out.push_secondary(p.col, slot);
                } else {
                    out.insert(p.col, p.ch, slot);
                }
                if p.in_grapheme {
                    // Never cache (see `PendingGlyph::in_grapheme`), and
                    // don't brand the chars uncacheable either: the line
                    // already took the slow path because of its
                    // zero-width chars, and the base char standalone
                    // (`e`) is perfectly cacheable elsewhere.
                    for c in &p.cluster_chars {
                        produced_chars.insert(*c);
                    }
                    continue;
                }
                // Decide whether this glyph is safe to drop into
                // `char_cache`. Requirements:
                //   - cluster is exactly one char (no multi-char
                //     ligatures collapsed into a single glyph),
                //   - that char is width 1 (invariant #3),
                //   - the glyph id matches the char's standalone form
                //     — i.e. the font did NOT apply a contextual GSUB
                //     substitution. Fira Code-style ligatures keep
                //     single-char clusters but swap each char's
                //     glyph id for a half-ligature shape; caching
                //     those would render every later standalone
                //     occurrence with the wrong shape.
                //
                // When the in-context glyph id differs, also record
                // `p.ch` in `uncacheable_chars` so future lines
                // containing it are forced through the slow path
                // (preserving the ligature visual every frame).
                if p.cluster_chars.len() == 1 && p.ch.width() == Some(1) {
                    let standalone = self.standalone_glyph_id_of(p.ch);
                    if standalone == Some(p.glyph.glyph_id) {
                        self.cache_standalone(p.ch, slot);
                    } else if self.mark_uncacheable(p.ch) {
                        // Earlier standalone-only cache entries would now
                        // be wrong (the font wants to substitute), so
                        // `mark_uncacheable` evicted them.
                        tracing::debug!(
                            target: "toastty_render::glyph",
                            "marking {:?} (U+{:04X}) as contextually-shaped; \
                             future renders use the slow path",
                            p.ch, p.ch as u32,
                        );
                    }
                } else if p.cluster_chars.len() > 1 {
                    // Multi-char cluster: every char in it participated
                    // in a ligature / contextual shaping run. They must
                    // not be served as standalones from `char_cache`
                    // afterward — otherwise once both `!` and `=` had
                    // been seen alone, the fast path would render `!=`
                    // as plain side-by-side glyphs and silently drop
                    // the ligature visual. Force the slow path on any
                    // line containing these chars from now on.
                    for ch in &p.cluster_chars {
                        if self.mark_uncacheable(*ch) {
                            tracing::debug!(
                                target: "toastty_render::glyph",
                                "marking {:?} (U+{:04X}) as multi-char-cluster member; \
                                 future renders use the slow path",
                                ch, *ch as u32,
                            );
                        }
                    }
                }
                // Credit every char in this glyph's cluster — not just
                // the cluster's leading char — so ligature partners
                // aren't falsely marked as misses below.
                for c in &p.cluster_chars {
                    produced_chars.insert(*c);
                }
            }
        }

        // Anything in the input that didn't produce a glyph is a miss.
        for ch in text.chars() {
            if ch == ' ' || ch == '\0' {
                continue;
            }
            if !produced_chars.contains(&ch) && self.mark_cache_miss(ch) {
                tracing::debug!(
                    target: "toastty_render::glyph",
                    "marking {ch:?} (U+{:04X}) as cache miss; future renders will skip it",
                    ch as u32,
                );
            }
        }

        out
    }

    /// Rasterize `glyph` if not already in the atlas; return its slot
    /// translated into `GlyphSlot` form for `build_instances`.
    ///
    /// Vertical placement uses the constant primary-font baseline
    /// [`Self::baseline_y`] to convert swash's bearing into a cell-relative
    /// offset — never a per-line value (see that field's docs for why).
    fn ensure_atlas_slot(&mut self, queue: &Queue, glyph: &LayoutGlyph) -> Option<GlyphSlot> {
        let baseline_y = self.baseline_y;
        let physical = glyph.physical((0.0, 0.0), 1.0);
        let cache_key: CacheKey = physical.cache_key;

        // Compose a stable u64 key for our Atlas. Combine font_id +
        // glyph_id + integer-quantised size.
        let key = GlyphKey(stable_key(cache_key));

        // Atlas slot already? Fast path. Placement was cached alongside.
        if let Some(existing) = self.atlas.lookup(key) {
            let placement = self
                .placements
                .get(&key)
                .copied()
                .unwrap_or(GlyphPlacement {
                    offset: [0.0, 0.0],
                    size: [0.0, 0.0],
                });
            return Some(make_glyph_slot(existing, placement));
        }

        let Some(image) = self
            .swash_cache
            .get_image_uncached(&mut self.font_system, cache_key)
        else {
            tracing::warn!(
                target: "toastty_render::glyph",
                "swash returned no image for glyph_id={} font_id={:?} — cell will render bg only",
                cache_key.glyph_id,
                cache_key.font_id,
            );
            return None;
        };

        let w = image.placement.width;
        let h = image.placement.height;
        #[allow(clippy::cast_precision_loss)]
        let placement = GlyphPlacement {
            offset: [
                // `left`/`top` are whole pixels from swash; the baseline is
                // the only fractional term. Rounding the vertical offset
                // lands the glyph quad on an integer device pixel — with
                // integer cell origins the whole quad is then pixel-aligned,
                // so Nearest sampling copies the atlas texels 1:1 (crisp)
                // instead of straddling texel boundaries (fuzzy).
                image.placement.left as f32,
                (baseline_y - image.placement.top as f32).round(),
            ],
            size: [w as f32, h as f32],
        };

        if w == 0 || h == 0 || image.data.is_empty() {
            // Whitespace / zero-extent: cache an empty slot so we don't
            // re-rasterize next time.
            let slot = self.atlas.reserve(key, AtlasLayer::Mask, 1, 1).ok()?;
            self.placements.insert(key, placement);
            // Don't upload anything.
            return Some(make_glyph_slot(slot, placement));
        }

        let layer = match image.content {
            SwashContent::Color => AtlasLayer::Color,
            // Mask + SubpixelMask both rasterize to alpha-only data; we
            // treat them identically in M4b.
            SwashContent::Mask | SwashContent::SubpixelMask => AtlasLayer::Mask,
        };

        // Best-effort recovery: if the layer is full, evict the LRU
        // shelf and retry once. Failing that, degrade by returning
        // `None` — the renderer falls back to a background-only
        // instance and the next frame will re-shape the row.
        let slot = if let Ok(s) = self.atlas.reserve(key, layer, w, h) {
            s
        } else {
            tracing::warn!(
                target: "toastty_render::glyph",
                "atlas {layer:?} full — evicting LRU shelf to make room for {w}x{h} glyph",
            );
            self.atlas.evict_oldest_shelf(layer);
            match self.atlas.reserve(key, layer, w, h) {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(
                        target: "toastty_render::glyph",
                        "atlas {layer:?} reserve failed after eviction: {e:?} — cell will render bg only",
                    );
                    return None;
                }
            }
        };

        // Embedded color bitmaps (CBDT/sbix PNGs) arrive with straight
        // alpha and must be premultiplied to match the glyph blend; COLR
        // color glyphs are already premultiplied. See `color_rgba_to_bgra`.
        let premultiply = matches!(image.source, swash::scale::Source::ColorBitmap(_));
        upload_glyph_pixels(
            queue,
            self.atlas_texture_for(layer),
            slot,
            &image.data,
            premultiply,
        );

        self.placements.insert(key, placement);
        Some(make_glyph_slot(slot, placement))
    }

    fn atlas_texture_for(&self, layer: AtlasLayer) -> &Texture {
        match layer {
            AtlasLayer::Mask => &self.mask_texture,
            AtlasLayer::Color => &self.color_texture,
        }
    }
}

/// `fast_chars` index of `ch`, if it has one.
#[inline]
fn fast_char_index(ch: char) -> Option<usize> {
    let i = ch as usize;
    (i < FAST_CHARS_LEN).then_some(i)
}

/// `fast_chars` with nothing cached: blanks skip, everything else bails
/// to the slow path until it is cached.
fn initial_fast_chars() -> Box<[FastChar]> {
    let mut t = vec![FastChar::Bail; FAST_CHARS_LEN].into_boxed_slice();
    t[' ' as usize] = FastChar::Skip;
    t[0] = FastChar::Skip;
    t
}

fn stable_key(k: CacheKey) -> u64 {
    // font_id is wrapper around u32; glyph_id u16; font_size as bits;
    // pack a few fields into u64.
    let font_id_bits: u64 = font_id_to_u64(k.font_id);
    let glyph: u64 = k.glyph_id as u64;
    let size: u64 = k.font_size_bits as u64;
    let flags: u64 = k.flags.bits() as u64;
    (font_id_bits << 32) ^ (glyph << 16) ^ size ^ (flags << 56)
}

fn font_id_to_u64(id: cosmic_text::fontdb::ID) -> u64 {
    // fontdb::ID has no public accessors but Debug prints as `ID(N)`.
    // The most portable hash is via the inner repr through serialization,
    // but a Debug-format parse is brittle. Use a deterministic hash via
    // std::hash::Hasher instead.
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

#[allow(clippy::cast_precision_loss)] // atlas coords fit in 24 bits easily
fn make_glyph_slot(slot: AtlasSlot, placement: GlyphPlacement) -> GlyphSlot {
    GlyphSlot {
        uv_min: [slot.x as f32, slot.y as f32],
        uv_max: [(slot.x + slot.w) as f32, (slot.y + slot.h) as f32],
        is_color: matches!(slot.layer, AtlasLayer::Color),
        glyph_offset: placement.offset,
        glyph_size: placement.size,
        has_secondary: false,
    }
}

/// Read the primary face's strikeout position and stroke thickness
/// (`OS/2.yStrikeoutPosition`, `post.underlineThickness`, via swash)
/// and turn them into per-cell [`DecorationMetrics`]. The face is the
/// one cosmic-text picks for "M", i.e. the same face [`measure_cell`]
/// measured. Falls back to fraction-of-cell geometry if no face shapes.
fn measure_decorations(
    font_system: &mut FontSystem,
    metrics: Metrics,
    family: Option<&str>,
    weight: Weight,
    cell_h: f32,
    baseline_y: f32,
) -> DecorationMetrics {
    let mut probe = Buffer::new(font_system, metrics);
    probe.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
    let attrs = Attrs::new()
        .family(Family::Name(family.unwrap_or("monospace")))
        .weight(weight);
    probe.set_text("M", &attrs, Shaping::Advanced, None);
    probe.shape_until_scroll(font_system, false);
    let face = probe
        .layout_runs()
        .flat_map(|run| run.glyphs.iter())
        .map(|g| (g.font_id, g.font_weight))
        .next();
    let line = face
        .and_then(|(id, weight)| font_system.get_font(id, weight))
        .map(|font| {
            let m = font.as_swash().metrics(&[]).scale(metrics.font_size);
            FontLineMetrics {
                strikeout_offset: m.strikeout_offset,
                stroke_size: m.stroke_size,
            }
        });
    DecorationMetrics::new(cell_h, baseline_y, line)
}

/// Weight of the face `family` resolves to when asked for the regular
/// weight: the nearest weight the family actually ships (CSS font
/// matching, via fontdb). `Weight::NORMAL` when the family is unset or
/// not installed.
///
/// cosmic-text only treats a face as a match for `Attrs` naming a family
/// when the weight matches too; otherwise it prefers any other installed
/// face of the right weight. The bundled fallback is Fira Mono **Medium**
/// (500), so plain `Attrs` (400) silently shaped the default config with
/// whatever system font fallback offered (e.g. proportional Noto Sans on
/// Linux) unless Fira Mono Regular happened to be installed.
fn resolve_family_weight(font_system: &FontSystem, family: Option<&str>) -> Weight {
    let Some(family) = family else {
        return Weight::NORMAL;
    };
    let db = font_system.db();
    db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        ..fontdb::Query::default()
    })
    .and_then(|id| db.face(id))
    .map_or(Weight::NORMAL, |face| face.weight)
}

/// `Attrs` naming `family`, with `primary_weight` when it is the primary
/// family (see [`resolve_family_weight`]).
fn family_attrs<'a>(family: &'a str, primary: &str, primary_weight: Weight) -> Attrs<'a> {
    let attrs = Attrs::new().family(Family::Name(family));
    if family == primary {
        attrs.weight(primary_weight)
    } else {
        attrs
    }
}

/// Probe the primary font's reference glyph "M" to derive the cell
/// geometry. Returns `(cell_w, cell_h, baseline_y)`.
///
/// `baseline_y` (the baseline's distance from the cell's top edge) is read
/// from this single primary-font line so it is a **constant**, independent
/// of whatever a real line is later composed of. Sourcing it here rather
/// than from each rendered line's `run.line_y - run.line_top` is the whole
/// point — see [`GlyphRasterizer::baseline_y`].
fn measure_cell(
    font_system: &mut FontSystem,
    metrics: Metrics,
    family: Option<&str>,
    weight: Weight,
) -> (f32, f32, f32) {
    let mut probe = Buffer::new(font_system, metrics);
    probe.set_size(Some(f32::INFINITY), Some(f32::INFINITY));
    let fam = family.unwrap_or("monospace");
    let attrs = Attrs::new().family(Family::Name(fam)).weight(weight);
    probe.set_text("M", &attrs, Shaping::Advanced, None);
    probe.shape_until_scroll(font_system, false);

    let mut max_w: f32 = metrics.font_size * 0.6;
    // Fallback only if "M" somehow produced no run (never in practice);
    // `font_size` is a reasonable baseline approximation if so.
    let mut baseline_y: f32 = metrics.font_size;
    for run in probe.layout_runs() {
        baseline_y = run.line_y - run.line_top;
        for g in run.glyphs {
            if g.w > max_w {
                max_w = g.w;
            }
        }
    }
    // Snap the advance and height to whole device pixels so cell origins
    // (`col * cell_w`, `row * cell_h`) land on the pixel grid — the
    // precondition for crisp glyphs under the atlas's Nearest sampling.
    // `metrics.line_height` is already whole-pixel (rounded in
    // `GlyphRasterizer::new`); the `max(font_size)` guard for sub-1.0
    // line-height ratios can reintroduce a fraction, so round again.
    // `baseline_y` is left unrounded — the final glyph offset is rounded
    // once in `ensure_atlas_slot`, matching the previous behaviour for
    // primary-font lines (so existing snapshots are unchanged).
    let height = metrics.line_height.max(metrics.font_size);
    (max_w.round().max(1.0), height.round().max(1.0), baseline_y)
}

fn create_atlas_texture(device: &Device, format: TextureFormat, label: &str) -> Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some(label),
        size: Extent3d {
            width: ATLAS_W,
            height: ATLAS_H,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    })
}

/// Convert swash color-glyph RGBA bytes into the premultiplied BGRA the
/// color atlas expects.
///
/// swash hands us two different alpha conventions for `Content::Color`:
///   * COLR/vector color glyphs are composited internally and arrive
///     already **premultiplied** (`Source::ColorOutline`).
///   * Embedded color *bitmaps* (CBDT/sbix PNGs — Noto Color Emoji on
///     Linux, Apple Color Emoji) are decoded straight from the PNG with
///     **straight, non-premultiplied** alpha (`Source::ColorBitmap`).
///
/// The glyph pass blends with One/OneMinusSrcAlpha, which assumes
/// premultiplied input. Uploading straight-alpha data unchanged makes the
/// antialiased edge texels (bright RGB, partial A) over-add and bloom into
/// a white halo around the emoji — so premultiply those here. Channels are
/// also swapped RGBA -> BGRA for the `Bgra8Unorm` atlas.
fn color_rgba_to_bgra(data: &[u8], premultiply: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    for px in data.chunks_exact(4) {
        let (r, g, b, a) = (px[0], px[1], px[2], px[3]);
        if premultiply {
            // Rounded c*a/255; idempotent at a==255 and a==0.
            let scale = |c: u8| ((u32::from(c) * u32::from(a) + 127) / 255) as u8;
            out.extend_from_slice(&[scale(b), scale(g), scale(r), a]);
        } else {
            out.extend_from_slice(&[b, g, r, a]);
        }
    }
    out
}

fn upload_glyph_pixels(
    queue: &Queue,
    texture: &Texture,
    slot: AtlasSlot,
    data: &[u8],
    premultiply: bool,
) {
    let bytes_per_pixel = match texture.format() {
        TextureFormat::R8Unorm => 1u32,
        TextureFormat::Bgra8Unorm | TextureFormat::Rgba8Unorm => 4u32,
        other => panic!("unexpected atlas format: {other:?}"),
    };

    // Color content needs RGBA -> premultiplied BGRA; mask content is
    // alpha-only and uploaded as-is.
    let bytes: std::borrow::Cow<'_, [u8]> = if matches!(slot.layer, AtlasLayer::Color) {
        std::borrow::Cow::Owned(color_rgba_to_bgra(data, premultiply))
    } else {
        std::borrow::Cow::Borrowed(data)
    };

    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: 0,
            origin: wgpu::Origin3d {
                x: slot.x,
                y: slot.y,
                z: 0,
            },
            aspect: wgpu::TextureAspect::All,
        },
        &bytes,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(slot.w * bytes_per_pixel),
            rows_per_image: Some(slot.h),
        },
        Extent3d {
            width: slot.w,
            height: slot.h,
            depth_or_array_layers: 1,
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_FONT: &[u8] = include_bytes!("../../fonts/FiraMono-Medium.ttf");

    fn make_rasterizer() -> (Device, Queue, GlyphRasterizer) {
        let (device, queue) = crate::test_gpu::device_and_queue();
        let rasterizer = GlyphRasterizer::new(
            &device,
            16.0,
            DEFAULT_LINE_HEIGHT_RATIO,
            Some("Fira Mono"),
            Some(TEST_FONT),
        );
        (device, queue, rasterizer)
    }

    /// Regression: the bundled face is Fira Mono *Medium*; requesting the
    /// family at the default (regular) weight used to make cosmic-text
    /// pick a regular-weight system font instead (proportional Noto Sans
    /// on Linux), so the default config never rendered in Fira Mono and
    /// the cell width depended on the host's installed fonts.
    #[test]
    fn bundled_medium_face_is_selected_over_system_fallback() {
        let (_device, _queue, mut r) = make_rasterizer();
        assert_eq!(r.primary_weight, Weight::MEDIUM);
        let id = r.default_resolved_font('a').expect("'a' renders");
        let face = r.font_system.db().face(id).expect("face");
        assert_eq!(face.post_script_name, "FiraMono-Medium");
        // Fira Mono's advance is 0.6 em: 9.6 px at 16 px, snapped to 10.
        assert_eq!(r.cell_size().0, 10.0);
    }

    #[test]
    fn family_weight_falls_back_to_normal() {
        let (_device, _queue, r) = make_rasterizer();
        assert_eq!(resolve_family_weight(&r.font_system, None), Weight::NORMAL);
        assert_eq!(
            resolve_family_weight(&r.font_system, Some("No Such Font Family")),
            Weight::NORMAL
        );
        // Only the primary family gets the resolved weight.
        let other = family_attrs("Noto Color Emoji", "Fira Mono", Weight::MEDIUM);
        assert_eq!(other.weight, Weight::NORMAL);
        let primary = family_attrs("Fira Mono", "Fira Mono", Weight::MEDIUM);
        assert_eq!(primary.weight, Weight::MEDIUM);
    }

    fn make_rasterizer_sized(device: &Device, size: f32) -> GlyphRasterizer {
        GlyphRasterizer::new(
            device,
            size,
            DEFAULT_LINE_HEIGHT_RATIO,
            Some("Fira Mono"),
            Some(TEST_FONT),
        )
    }

    /// Regression: a glyph's vertical placement must NOT depend on what
    /// else shared its line. Before the fix, `baseline_y` was taken per
    /// line from `run.line_y - run.line_top`, which grows when a line
    /// mixes in a taller fallback-font glyph (CJK here) — so `A` shaped on
    /// `"A你"` was placed up to a full pixel lower than `A` shaped alone,
    /// and that offset was then cached and reused everywhere.
    ///
    /// Host-tolerant: only meaningful at sizes where the CJK char actually
    /// resolves to a fallback face (otherwise it maps to `FiraMono`'s
    /// `.notdef`, same metrics, and the baseline can't move). If no tested
    /// size resolves a fallback glyph, the test is a no-op rather than a
    /// false pass/fail. The size sweep includes sizes (13/14/15/18/20/24)
    /// where the pre-fix shift crossed a rounding boundary into a visible
    /// 1px — so the test would fail on the old code.
    #[test]
    fn glyph_offset_is_independent_of_line_content() {
        let (device, queue, _r) = make_rasterizer();
        let mut exercised = false;
        for size in [13.0_f32, 14.0, 15.0, 16.0, 17.0, 18.0, 20.0, 24.0] {
            let plain = make_rasterizer_sized(&device, size)
                .shape_line(&queue, "A", false)
                .get(0, 'A')
                .expect("A on plain line")
                .glyph_offset;

            let mut r_mixed = make_rasterizer_sized(&device, size);
            let line = r_mixed.shape_line(&queue, "A\u{4f60}", false);
            // Skip sizes where the CJK char didn't reach a fallback face.
            if line.get(1, '\u{4f60}').is_none() {
                continue;
            }
            exercised = true;
            let mixed = line.get(0, 'A').expect("A on mixed line").glyph_offset;

            assert!(
                (plain[1] - mixed[1]).abs() < 1e-6,
                "size {size}: A's vertical offset depends on line content \
                 (plain={:.3}, mixed-with-CJK={:.3}) — the baseline regressed \
                 to a per-line value",
                plain[1],
                mixed[1],
            );
        }
        // Fail loudly rather than pass vacuously: if no tested size reached
        // a fallback face, the regression can't be exercised here. Install a
        // CJK fallback font (e.g. Noto Sans CJK) to run this.
        assert!(
            exercised,
            "no CJK fallback face available — cannot exercise the baseline \
             regression; install one (e.g. Noto Sans CJK)",
        );
    }

    /// Regression for the user-visible loop: once a glyph is first
    /// rasterized next to a fallback glyph, every *later plain* line must
    /// still render it at the same place. Pre-fix, `char_cache` froze the
    /// poisoned offset, so a plain `"A"` after a `"A你"` rendered low
    /// forever. Host-tolerant in the same way as the test above.
    #[test]
    fn char_cache_offset_survives_mixed_first_line() {
        let (device, queue, _r) = make_rasterizer();
        let mut exercised = false;
        for size in [13.0_f32, 14.0, 15.0, 18.0, 20.0, 24.0] {
            let clean = make_rasterizer_sized(&device, size)
                .shape_line(&queue, "A", false)
                .get(0, 'A')
                .expect("clean A")
                .glyph_offset[1];

            // 'A' first seen next to CJK, then a later plain "A" line
            // (served from char_cache via the fast path).
            let mut poison = make_rasterizer_sized(&device, size);
            let first = poison.shape_line(&queue, "A\u{4f60}", false);
            if first.get(1, '\u{4f60}').is_none() {
                continue;
            }
            exercised = true;
            let later = poison
                .shape_line(&queue, "A", false)
                .get(0, 'A')
                .expect("later A")
                .glyph_offset[1];

            assert!(
                (clean - later).abs() < 1e-6,
                "size {size}: a plain \"A\" after a fallback-mixed line renders at \
                 {later:.3} but a never-poisoned \"A\" is at {clean:.3} — char_cache \
                 froze a line-dependent offset",
            );
        }
        // Fail loudly rather than pass vacuously (see the sibling test).
        assert!(
            exercised,
            "no CJK fallback face available — cannot exercise the char_cache \
             regression; install one (e.g. Noto Sans CJK)",
        );
    }

    /// Cell metrics must be whole device pixels so per-cell origins
    /// (`col * cell_w`, `row * cell_h`) land on the pixel grid — the
    /// precondition for crisp glyphs under the atlas's Nearest sampling.
    /// `make_rasterizer` builds at 16 px × 1.20 line height = 19.2 px,
    /// which must snap to 19.
    #[test]
    fn cell_metrics_are_whole_pixels() {
        let (_device, _queue, rasterizer) = make_rasterizer();
        let (w, h) = rasterizer.cell_size();
        assert!(
            (w - w.round()).abs() < 1e-6,
            "cell width {w} must be a whole pixel",
        );
        assert!(
            (h - h.round()).abs() < 1e-6,
            "cell height {h} must be a whole pixel",
        );
        assert!(
            (h - 19.0).abs() < 1e-6,
            "16px × 1.20 line height should snap to 19, got {h}",
        );
    }

    /// Font zoom: re-sizing in place must give exactly the metrics and
    /// glyphs a freshly built rasterizer at that size would, drop every
    /// size-dependent cache (so no slot from the old size is served),
    /// and survive many round trips without the atlas filling up.
    #[test]
    fn set_font_size_matches_fresh_rasterizer() {
        let (device, queue, mut rasterizer) = make_rasterizer();
        let line16 = rasterizer.shape_line(&queue, "Hello", false);
        let a16 = line16.get(0, 'H').expect("H shaped at 16px");
        assert!(!rasterizer.char_cache.is_empty());

        for size in [24.0_f32, 9.0, 72.0, 6.0] {
            rasterizer.set_font_size(size, DEFAULT_LINE_HEIGHT_RATIO);
            assert!(
                rasterizer.char_cache.is_empty(),
                "char cache kept at {size}"
            );
            assert!(
                rasterizer.placements.is_empty(),
                "placements kept at {size}"
            );
            let fresh = make_rasterizer_sized(&device, size);
            assert_eq!(rasterizer.cell_size(), fresh.cell_size(), "cell at {size}");
            assert!((rasterizer.baseline_y - fresh.baseline_y).abs() < 1e-6);
            assert!((rasterizer.metrics.font_size - size).abs() < 1e-6);
        }

        rasterizer.set_font_size(24.0, DEFAULT_LINE_HEIGHT_RATIO);
        let line24 = rasterizer.shape_line(&queue, "Hello", false);
        let a24 = line24.get(0, 'H').expect("H shaped at 24px");
        assert!(
            a24.glyph_size[1] > a16.glyph_size[1],
            "24px glyph {:?} should be taller than 16px glyph {:?}",
            a24.glyph_size,
            a16.glyph_size,
        );
        let mut fresh = make_rasterizer_sized(&device, 24.0);
        let fresh24 = fresh.shape_line(&queue, "Hello", false);
        let f = fresh24.get(0, 'H').expect("H shaped fresh");
        assert_eq!(a24.glyph_size, f.glyph_size);
        assert_eq!(a24.glyph_offset, f.glyph_offset);

        // Back to 16: identical to the original glyph again.
        rasterizer.set_font_size(16.0, DEFAULT_LINE_HEIGHT_RATIO);
        let again = rasterizer.shape_line(&queue, "Hello", false);
        let b16 = again.get(0, 'H').expect("H reshaped at 16px");
        assert_eq!(a16.glyph_size, b16.glyph_size);
        assert_eq!(a16.glyph_offset, b16.glyph_offset);

        // Many zooms: the atlas restarts each time, so it never fills.
        for i in 0..100 {
            #[allow(clippy::cast_precision_loss)]
            let size = 10.0 + (i % 30) as f32;
            rasterizer.set_font_size(size, DEFAULT_LINE_HEIGHT_RATIO);
            let lg = rasterizer.shape_line(&queue, "The quick brown fox", false);
            assert!(lg.get(0, 'T').is_some(), "shaping failed at {size}");
        }
    }

    /// Wide / zero-width characters must not enter `char_cache`: their
    /// cell-width != 1 violates invariant #3, so the fast path bails
    /// before consulting the cache and the slow path skips insertion.
    /// Without this guard, a CJK ideograph could be cached and later
    /// silently misaligned in a single-column slot.
    #[test]
    fn cjk_does_not_enter_char_cache() {
        let (_device, queue, mut rasterizer) = make_rasterizer();

        // ASCII-only line: every glyph is width-1 and should land in
        // the cache via the slow path's `char_cache.entry(...)` call.
        rasterizer.shape_line(&queue, "hello", false);
        let ascii_cache_size = rasterizer.char_cache.len();
        assert!(
            ascii_cache_size >= "hello".chars().filter(|c| *c != 'l').count(),
            "ASCII chars should populate char_cache; got {ascii_cache_size}"
        );
        let cache_before = rasterizer.char_cache.clone();

        // Now shape a line containing a CJK ideograph. `你` has
        // unicode_width 2; the fast path must bail (so the slow path
        // runs), and the slow path must NOT insert `你` into the
        // per-char cache.
        rasterizer.shape_line(&queue, "你", false);

        // The CJK char itself must not be in the cache.
        assert!(
            !rasterizer.char_cache.contains_key(&'你'),
            "wide char `你` must not be inserted into char_cache"
        );

        // No previously-cached entries should have been disturbed.
        for (k, v) in &cache_before {
            assert_eq!(
                rasterizer.char_cache.get(k),
                Some(v),
                "existing char_cache entry for {k:?} changed"
            );
        }
    }

    /// The fast path must refuse a line containing any non-width-1
    /// character, even if the ASCII parts would otherwise hit the
    /// cache.
    #[test]
    fn fast_path_bails_on_wide_char() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        // Warm the cache for ASCII letters.
        rasterizer.shape_line(&queue, "ab", false);
        assert!(rasterizer.char_cache.contains_key(&'a'));
        assert!(rasterizer.char_cache.contains_key(&'b'));

        // A line mixing ASCII + a wide char: fast path must return
        // None even though `a`/`b` are cached.
        assert!(rasterizer.try_shape_line_fast("a你b").is_none());
        // Pure ASCII still takes the fast path.
        assert!(rasterizer.try_shape_line_fast("ab").is_some());
    }

    /// Mode 2027 OFF: cluster_width falls back to `wcwidth` per
    /// codepoint. CJK still snaps to 2 cells (legacy table); ZWJ
    /// emoji come out as 1. We don't render here — we just check
    /// shape_line doesn't panic for both flag values.
    #[test]
    fn mode_2027_off_uses_default_width() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let _ = rasterizer.shape_line(&queue, "你 abc", false);
    }

    /// Mode 2027 ON: cluster_width trusts UnicodeWidthStr. ZWJ family
    /// snaps to 2; VS16 emoji snaps to 2. Shape doesn't panic and
    /// produces some glyphs. Visual correctness requires a real GPU +
    /// snapshot harness — geometry-only smoke here.
    #[test]
    fn mode_2027_on_honors_cluster_width_for_emoji() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        // FiraMono doesn't cover emoji so cosmic-text may map them to
        // the .notdef glyph — but the call must not panic and must
        // return without producing two-cell-overlapping glyphs.
        let _ = rasterizer.shape_line(&queue, "❤\u{FE0F}", true);
    }

    /// A text-presentation emoji (⏺ U+23FA: `Emoji=Yes`,
    /// `Emoji_Presentation=No`) must not render as a color emoji when the
    /// host has a monochrome face covering it. This is the exact bug:
    /// cosmic-text's presentation-unaware fallback reaches Apple Color
    /// Emoji, so the steering must redirect it to a text face.
    ///
    /// Host-tolerant: only asserts on systems whose *default* fallback
    /// actually lands on a color face AND that have a monochrome covering
    /// face to steer toward — otherwise there's nothing to fix here.
    #[test]
    fn text_presentation_emoji_avoids_color_when_possible() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let record = '\u{23FA}';

        let default_color = rasterizer
            .default_resolved_font(record)
            .is_some_and(|id| presentation::face_is_color(rasterizer.font_system.db(), id));
        let has_text_face =
            presentation::find_text_face(rasterizer.font_system.db(), record).is_some();

        let lg = rasterizer.shape_line(&queue, "\u{23FA}", false);

        if default_color && has_text_face {
            let slot = lg.get(0, record).expect("expected a glyph slot for U+23FA");
            assert!(
                !slot.is_color,
                "U+23FA must render as a monochrome text glyph, not a color emoji, after steering",
            );
        }
    }

    /// Real emoji (`Emoji_Presentation=Yes`) must keep their color glyph —
    /// the steering must not regress them. Host-tolerant: only meaningful
    /// when the host's default fallback gives 😀 a color face at all.
    #[test]
    fn real_emoji_keeps_color_presentation() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let grin = '\u{1F600}';

        let default_color = rasterizer
            .default_resolved_font(grin)
            .is_some_and(|id| presentation::face_is_color(rasterizer.font_system.db(), id));

        // No override should be recorded for a default-emoji codepoint.
        assert_eq!(
            rasterizer.presentation_family(grin, false, false),
            None,
            "default-presentation emoji must never be steered to a text face",
        );

        let lg = rasterizer.shape_line(&queue, "\u{1F600}", true);
        if default_color {
            if let Some(slot) = lg.get(0, grin) {
                assert!(
                    slot.is_color,
                    "😀 must keep its color presentation; steering must not regress real emoji",
                );
            }
        }
    }

    /// ⏺ is width-1 and not variation-selected, so after the first (slow)
    /// shape it must land in `char_cache` and serve from the fast path —
    /// not be flagged contextual and re-shaped every line. Regression
    /// guard for the `standalone_glyph_id_of` steering: if the standalone
    /// probe used the default (color) family while the in-context shape
    /// used the steered (text) family, their glyph ids would differ and ⏺
    /// would be marked uncacheable forever.
    #[test]
    fn steered_text_emoji_is_cacheable() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let record = '\u{23FA}';

        let steered = presentation::find_text_face(rasterizer.font_system.db(), record).is_some()
            && rasterizer
                .default_resolved_font(record)
                .is_some_and(|id| presentation::face_is_color(rasterizer.font_system.db(), id));

        rasterizer.shape_line(&queue, "\u{23FA}", false);

        if steered {
            assert!(
                rasterizer.char_cache.contains_key(&record),
                "steered text-presentation ⏺ must be cached for the fast path",
            );
            assert!(
                !rasterizer.uncacheable_chars.contains(&record),
                "steered text-presentation ⏺ must not be flagged contextual/uncacheable",
            );
            assert!(
                rasterizer.try_shape_line_fast("\u{23FA}").is_some(),
                "a line of only ⏺ must take the fast path after the first shape",
            );
        }
    }

    /// Regression: a char known to receive contextual GSUB substitution
    /// in this font must force its containing line through the slow
    /// path. Without this, Fira Code-style ligatures (where each char
    /// in `!=` keeps its single-char cluster but swaps to a
    /// half-ligature glyph) would poison `char_cache` with the wrong
    /// shape and every later standalone `=` would render as `≠`.
    #[test]
    fn fast_path_bails_on_uncacheable_chars() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        // Warm the cache with plain ASCII so it would otherwise hit
        // the fast path.
        rasterizer.shape_line(&queue, "ab=", false);
        assert!(rasterizer.try_shape_line_fast("ab=").is_some());

        // Simulate the slow path having observed `=` shaping to a
        // contextual variant (the half-ligature case). Future fast-
        // path attempts on any line containing `=` must bail.
        rasterizer.mark_uncacheable('=');

        assert!(
            rasterizer.try_shape_line_fast("ab=").is_none(),
            "fast path must bail when the line contains a char marked uncacheable",
        );
        // Lines that don't contain the uncacheable char still take the
        // fast path.
        assert!(rasterizer.try_shape_line_fast("ab").is_some());
    }

    /// `standalone_glyph_id_of` returns a stable id for ASCII chars in
    /// any reasonable monospace font and caches the result for repeat
    /// lookups. Smoke check — also exercises the `isolation_buffer`
    /// machinery so a regression in buffer setup surfaces here.
    #[test]
    fn standalone_glyph_id_caches_ascii_lookups() {
        let (_device, _queue, mut rasterizer) = make_rasterizer();
        let first = rasterizer.standalone_glyph_id_of('a');
        assert!(first.is_some(), "Fira Mono must have a glyph for 'a'");
        // Second call hits the cache; we can't observe that directly
        // but we *can* observe the result is stable and that the
        // standalone_glyph_ids map now contains the entry.
        let second = rasterizer.standalone_glyph_id_of('a');
        assert_eq!(first, second);
        assert!(rasterizer.standalone_glyph_ids.contains_key(&'a'));
    }

    /// Regression: shaping a multi-char cluster (any ligature) must not
    /// silently mark the non-leading chars of the cluster as cache
    /// misses. Previously, `produced_chars` only stored the cluster's
    /// first char, so e.g. `!=` shaped as a ligature glyph (cluster
    /// chars `['!', '=']`, `p.ch == '!'`) caused `=` to land in
    /// `char_cache_misses` — after which every future `=` rendered as
    /// transparent because the fast path's miss check at line 341
    /// silently `continue`s. Whether or not the bundled font actually
    /// ligates these sequences, no ASCII char in the input may ever be
    /// marked as a miss: each has a real glyph in any reasonable font.
    #[test]
    fn ligature_partners_are_not_marked_as_cache_misses() {
        let (_device, queue, mut rasterizer) = make_rasterizer();

        // Sequences that ligate in popular programming fonts (Fira Code,
        // JetBrains Mono, Cascadia, etc). Fira Mono Medium ships without
        // programming ligatures but does keep typographic ligatures
        // (`fi`, `fl`, `ffi`, `ffl`) — those are the case that bites
        // users on a fresh terminal printing `printf` or `'before:'`.
        for line in [
            "fi",
            "fl",
            "ffi",
            "ffl",
            "==",
            "!=",
            ">=",
            "<=",
            "=>",
            "->",
            "<-",
            ":=",
            "===",
            "!==",
            "<==>",
            "fi==",
            "if x != y:",
        ] {
            rasterizer.shape_line(&queue, line, false);
        }

        // Every ASCII char that appeared in the shaped lines must be
        // either rendered (in `char_cache`) or silently produced via a
        // ligature — but NEVER a "miss" that would suppress future
        // renders.
        let all_chars: std::collections::HashSet<char> = "filfffi==!><=->:if xy".chars().collect();
        for ch in all_chars {
            assert!(
                !rasterizer.char_cache_misses.contains(&ch),
                "ASCII char {ch:?} (U+{:04X}) was wrongly marked as a cache miss; \
                 ligature partners must not poison the fast path",
                ch as u32,
            );
        }
    }

    /// `assign_glyph_chars` is the pure logic that decides which cell
    /// char each glyph keys under in `LineGlyphs`. These tests pin its
    /// behavior without a GPU or a real font, so the regression that
    /// caused `!=` ligatures to render with the right half blank can be
    /// reproduced and prevented even on the bundled non-ligating font.
    mod assign_glyph_chars {
        use super::super::assign_glyph_chars;

        #[test]
        fn single_char_clusters_use_their_own_char() {
            // "abc" — three independent single-char clusters.
            let glyphs = [(0, 1, 0), (1, 2, 1), (2, 3, 2)];
            let out = assign_glyph_chars(&glyphs, "abc");
            assert_eq!(out[0].0, 'a');
            assert_eq!(out[1].0, 'b');
            assert_eq!(out[2].0, 'c');
            for (i, expected) in ['a', 'b', 'c'].iter().enumerate() {
                assert_eq!(out[i].1, vec![*expected]);
            }
        }

        /// Fira Code-style: `!=` shaped as TWO glyphs that share the same
        /// `(start=0, end=2)` cluster. Each glyph lands on a different
        /// cell, so each must key under the cell's own char — glyph 0
        /// → `'!'`, glyph 1 → `'='`. Pre-fix, both got `'!'` and the
        /// `LineGlyphs[1]` lookup at render time (against cell char `'='`)
        /// missed, leaving the ligature's right half blank.
        #[test]
        fn multi_glyph_multi_char_cluster_keys_per_cell() {
            let glyphs = [(0, 2, 0), (0, 2, 1)];
            let out = assign_glyph_chars(&glyphs, "!=");
            assert_eq!(out.len(), 2);
            assert_eq!(out[0].0, '!', "left half must key under cell 0's char");
            assert_eq!(out[1].0, '=', "right half must key under cell 1's char");
            assert_eq!(out[0].1, vec!['!', '=']);
            assert_eq!(out[1].1, vec!['!', '=']);
        }

        /// True ligature (Type 4 GSUB): the whole cluster collapses to a
        /// single glyph at the cluster's first column. The glyph keys
        /// under the cluster's leading char — there's only one cell-key
        /// to choose. The trailing cells of the cluster get no glyph
        /// entry, which is handled separately by the renderer (the bg
        /// quad in those cells leaves them blank; widening the glyph to
        /// span both cells is a follow-up).
        #[test]
        fn single_glyph_multi_char_cluster_uses_leading_char() {
            let glyphs = [(0, 2, 0)];
            let out = assign_glyph_chars(&glyphs, "fi");
            assert_eq!(out.len(), 1);
            assert_eq!(out[0].0, 'f');
            assert_eq!(out[0].1, vec!['f', 'i']);
        }

        /// Three-char multi-glyph ligature (`<=>` or `===`) at non-zero
        /// base column. The glyphs land on cells 5, 6, 7 — each must
        /// key under the matching cluster char.
        #[test]
        fn three_glyph_cluster_at_offset_keys_per_cell() {
            let glyphs = [(0, 3, 5), (0, 3, 6), (0, 3, 7)];
            let out = assign_glyph_chars(&glyphs, "===");
            assert_eq!(
                out.iter().map(|x| x.0).collect::<Vec<_>>(),
                vec!['=', '=', '=']
            );
        }

        /// Mix of clusters in one run: `a!=b` — `a` (single-char), then
        /// `!=` (2-glyph 2-char), then `b` (single-char). Each cluster
        /// must be tracked independently and `b`'s `base_col` must reset
        /// from the ligature cluster, otherwise `b` would index off the
        /// `!=` cluster's `cluster_chars` and pick the wrong char.
        #[test]
        fn cluster_boundaries_reset_base_col() {
            // Source `a!=b` (bytes 0,1,2,3). Cols 0,1,2,3.
            let glyphs = [
                (0, 1, 0), // 'a' single
                (1, 3, 1), // '!=' first half at col 1
                (1, 3, 2), // '!=' second half at col 2
                (3, 4, 3), // 'b' single
            ];
            let out = assign_glyph_chars(&glyphs, "a!=b");
            assert_eq!(
                out.iter().map(|x| x.0).collect::<Vec<_>>(),
                vec!['a', '!', '=', 'b']
            );
        }

        /// Multiple glyphs at the same column inside a multi-char
        /// cluster (combining marks under a ligature, or degenerate
        /// shaping): the per-cell index points at the same source char,
        /// so all such glyphs key under the cluster's leading char.
        #[test]
        fn multiple_glyphs_at_same_col_share_a_char() {
            // Two glyphs, both at col 0, cluster spans two chars.
            // `idx = 0` for both → both key under `'!'`.
            let glyphs = [(0, 2, 0), (0, 2, 0)];
            let out = assign_glyph_chars(&glyphs, "!=");
            assert_eq!(out[0].0, '!');
            assert_eq!(out[1].0, '!');
        }

        /// Defensive: if a later glyph in the same cluster lands at a
        /// column beyond the cluster's char count (shouldn't happen
        /// given the cell-width snap, but cosmic-text could in theory
        /// produce it), the fallback to `cluster_chars.last()` keeps a
        /// real source char as the key — rather than reverting to the
        /// `' '` default, which would re-introduce the original
        /// lookup-miss bug.
        #[test]
        fn out_of_range_col_falls_back_to_last_char() {
            // Cluster starts at col 0; second glyph anomalously lands at
            // col 5 → `idx = 5`, out of range for the 2-char cluster.
            let glyphs = [(0, 2, 0), (0, 2, 5)];
            let out = assign_glyph_chars(&glyphs, "!=");
            assert_eq!(out[0].0, '!', "first glyph keys under leading char");
            assert_eq!(out[1].0, '=', "out-of-range glyph falls back to last char");
        }

        /// Empty cluster (shaper produced a glyph for a zero-byte range,
        /// or `text.get(...)` failed): the fallback default is a space,
        /// which is the existing "absent" sentinel the renderer treats
        /// as a no-op cell.
        #[test]
        fn empty_cluster_falls_back_to_space() {
            let glyphs = [(0, 0, 0)];
            let out = assign_glyph_chars(&glyphs, "");
            assert_eq!(out[0].0, ' ');
            assert!(out[0].1.is_empty());
        }
    }

    /// Integration: shaping any string with ASCII letters/punctuation
    /// must produce a `LineGlyphs` entry for every non-blank cell, keyed
    /// by that cell's char. This is the property the bug violated for
    /// the trailing cells of multi-glyph multi-char clusters: cell 1 of
    /// `!=` had an entry under `'!'` instead of `'='`, so
    /// `LineGlyphs.get(1, '=')` missed at render time. Whether or not
    /// the bundled font actually ligates the test strings, the
    /// post-condition (every visible cell resolves to a glyph slot)
    /// holds — and on a ligating font it pins the per-glyph keying.
    #[test]
    fn shape_line_assigns_glyph_per_visible_cell() {
        let (_device, queue, mut rasterizer) = make_rasterizer();

        // Cover plain text, programming ligature sequences, and the
        // typographic ligatures Fira Mono Medium does carry (`fi`, `fl`,
        // `ffi`, `ffl`). On a font that produces multi-glyph multi-char
        // clusters for any of these, the pre-fix code mis-keyed glyphs
        // and this check catches the regression.
        for line in [
            "abc",
            "fi",
            "fl",
            "ffi",
            "ffl",
            "!=",
            "==",
            ">=",
            "<=",
            "=>",
            "->",
            "if x != y:",
            "fi==",
        ] {
            let lg = rasterizer.shape_line(&queue, line, false);
            for (col, ch) in line.chars().enumerate() {
                if ch == ' ' || ch == '\0' {
                    continue;
                }
                // The cell has a real char that any reasonable font
                // covers. Either a glyph slot is present (matching this
                // cell's char) OR the cluster collapsed to a single
                // glyph that sits in an earlier column — in which case
                // the trailing cell is genuinely empty. We assert the
                // common case: when a slot exists, its key matches the
                // cell char. The bug put `cluster_chars.first()` here
                // for every glyph in a multi-char cluster, which broke
                // this invariant.
                if let Some(entry) = lg.by_column.get(col).and_then(|s| s.as_ref()) {
                    assert_eq!(
                        entry.0, ch,
                        "{line:?} col {col}: LineGlyphs key {:?} != cell char {ch:?} \
                         — multi-glyph cluster mis-key would put `cluster_chars.first()` here",
                        entry.0,
                    );
                }
            }
        }
    }

    /// Multi-char clusters must mark every participating char as
    /// uncacheable so future renders re-shape contextually. Without
    /// this, once `!` and `=` are both cached as standalones, the fast
    /// path would serve them as plain side-by-side glyphs and silently
    /// drop the ligature. We can't force the bundled non-ligating font
    /// to produce a multi-char cluster from the outside, but we *can*
    /// verify the post-condition by hand-driving the slow path with a
    /// known ligating sequence and inspecting `uncacheable_chars` only
    /// when shaping actually produced a multi-char cluster.
    #[test]
    fn multi_char_cluster_chars_become_uncacheable() {
        let (_device, queue, mut rasterizer) = make_rasterizer();

        // Pre-warm `!` and `=` as standalones. After this they sit in
        // `char_cache` if the font produces single-char clusters for
        // them (the expected case for Fira Mono).
        rasterizer.shape_line(&queue, "! =", false);

        // Now shape a sequence that ligature-capable fonts would
        // collapse into a multi-char cluster. If the bundled font
        // does so, `!` and `=` should now be in `uncacheable_chars`
        // and removed from `char_cache`. If not, the test still
        // passes (Fira Mono doesn't ligate `!=`) — we only assert the
        // *implication*, not the trigger.
        rasterizer.shape_line(&queue, "!=", false);

        for ch in ['!', '='] {
            if rasterizer.uncacheable_chars.contains(&ch) {
                assert!(
                    !rasterizer.char_cache.contains_key(&ch),
                    "char {ch:?} is uncacheable but still in char_cache; \
                     multi-char-cluster eviction failed",
                );
            }
        }
    }

    /// Straight-alpha color bitmap data (CBDT/sbix emoji) must be
    /// premultiplied on upload, else its antialiased edges — bright RGB
    /// with partial alpha — bloom into a white halo under the
    /// One/OneMinusSrcAlpha glyph blend. The classic offender is a white
    /// matte texel at the glyph edge: full white RGB, partial coverage.
    #[test]
    fn color_bitmap_edge_is_premultiplied() {
        // One opaque red texel and one half-transparent white edge texel,
        // in swash's RGBA order.
        let rgba = [
            255, 0, 0, 255, // opaque red
            255, 255, 255, 128, // white matte edge, ~50% coverage
            200, 100, 50, 0, // fully-transparent matte (the most common edge texel)
        ];

        let bgra = color_rgba_to_bgra(&rgba, true);

        // Opaque texel: premultiply is a no-op (a==255); just RGBA->BGRA.
        assert_eq!(&bgra[0..4], &[0, 0, 255, 255], "opaque red -> BGRA");

        // Fully-transparent texel: RGB must collapse to 0 so it contributes
        // nothing under premultiplied-over (else a colored ghost leaks in).
        assert_eq!(&bgra[8..12], &[0, 0, 0, 0], "transparent edge zeroed");

        // Edge texel: every color channel must be scaled down to its alpha
        // so rgb <= a. Pre-fix this stayed (255,255,255,128) and blended to
        // a white fringe. 255*128/255 rounded == 128.
        let (b, g, r, a) = (bgra[4], bgra[5], bgra[6], bgra[7]);
        assert_eq!(
            (b, g, r, a),
            (128, 128, 128, 128),
            "white edge premultiplied"
        );
        assert!(
            b <= a && g <= a && r <= a,
            "premultiplied invariant rgb <= a violated: {:?}",
            (r, g, b, a),
        );
    }

    /// Already-premultiplied COLR data must pass through untouched (only
    /// channel-swapped) — premultiplying a second time would darken edges.
    #[test]
    fn color_outline_passthrough_only_swaps_channels() {
        // A premultiplied edge texel: rgb already <= a.
        let rgba = [64, 32, 16, 128];
        let bgra = color_rgba_to_bgra(&rgba, false);
        assert_eq!(
            &bgra[..],
            &[16, 32, 64, 128],
            "RGBA->BGRA, values unchanged"
        );
    }

    // ---- multi-glyph grapheme cells (audit bug #8) ---------------------

    fn dummy_slot(x: f32) -> GlyphSlot {
        GlyphSlot {
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            is_color: false,
            glyph_offset: [x, 0.0],
            glyph_size: [1.0, 1.0],
            has_secondary: false,
        }
    }

    #[test]
    fn line_glyphs_secondary_lookup_by_key() {
        use crate::text::instance::secondary_glyph_key;
        let mut lg = LineGlyphs::default();
        lg.insert(2, 'a', dummy_slot(0.0));
        lg.push_secondary(2, dummy_slot(3.0));
        lg.push_secondary(2, dummy_slot(4.0));
        // Primary is flagged; secondaries come back in order.
        assert!(lg.get(2, 'a').is_some_and(|s| s.has_secondary));
        let k0 = secondary_glyph_key(0).unwrap();
        let k1 = secondary_glyph_key(1).unwrap();
        let k2 = secondary_glyph_key(2).unwrap();
        assert_eq!(lg.get(2, k0).map(|s| s.glyph_offset[0]), Some(3.0));
        assert_eq!(lg.get(2, k1).map(|s| s.glyph_offset[0]), Some(4.0));
        assert_eq!(lg.get(2, k2), None);
        // Other columns / ordinary chars don't see them.
        assert_eq!(lg.get(3, k0), None);
        assert_eq!(lg.get(2, 'b'), None);
    }

    #[test]
    fn line_glyphs_secondary_without_primary_is_dropped() {
        let mut lg = LineGlyphs::default();
        lg.push_secondary(0, dummy_slot(1.0));
        assert!(lg.secondary.is_empty());
    }

    #[test]
    fn grapheme_cluster_classification() {
        assert!(is_grapheme_cluster("e\u{301}"));
        assert!(is_grapheme_cluster("\u{1F468}\u{200D}\u{1F469}"));
        assert!(is_grapheme_cluster("\u{2764}\u{FE0F}"));
        assert!(is_grapheme_cluster("\u{1F1EF}\u{1F1F5}")); // RI pair
        assert!(is_grapheme_cluster("\u{1F44D}\u{1F3FD}")); // skin tone
        // Ligatures (several graphemes) and singletons are not.
        assert!(!is_grapheme_cluster("!="));
        assert!(!is_grapheme_cluster("a"));
        assert!(!is_grapheme_cluster(""));
    }

    /// A base + two combining marks with no precomposed form shapes to
    /// several glyphs; all must land on the base's column (one primary +
    /// secondaries) instead of the marks overwriting the base or drifting
    /// a column right. And the base char must stay fast-path cacheable.
    #[test]
    fn combining_marks_become_secondary_glyphs_of_base_cell() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let lg = rasterizer.shape_line(&queue, "a\u{308}\u{304}x", false);
        let base = lg.get(0, 'a').expect("base glyph at col 0");
        assert!(lg.get(1, 'x').is_some(), "following char keeps its column");
        if base.has_secondary {
            let k0 = crate::text::instance::secondary_glyph_key(0).unwrap();
            let mark = lg.get(0, k0).expect("first mark");
            // Marks sit over the base cell, not a cell to the right.
            let cell_w = rasterizer.cell_size().0;
            assert!(
                mark.glyph_offset[0] < cell_w,
                "mark offset {:?}",
                mark.glyph_offset
            );
        }
        assert!(!rasterizer.uncacheable_chars.contains(&'a'));
        assert!(!rasterizer.uncacheable_chars.contains(&'x'));
    }

    /// VS16 under DECSET 2027 steers toward a color face when the default
    /// fallback would pick a monochrome one. Host-tolerant: only asserts
    /// when the host has a monochrome face for ❤ *and* a color face.
    #[test]
    fn vs16_under_2027_prefers_color_face() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let heart = '\u{2764}';
        let default_mono = rasterizer
            .default_resolved_font(heart)
            .is_some_and(|id| !presentation::face_is_color(rasterizer.font_system.db(), id));
        let has_color = presentation::find_color_face(rasterizer.font_system.db(), heart).is_some();
        let lg = rasterizer.shape_line(&queue, "\u{2764}\u{FE0F}", true);
        if default_mono && has_color {
            let slot = lg.get(0, heart).expect("heart glyph");
            assert!(slot.is_color, "❤ + VS16 under 2027 must use a color face");
        }
        // Cached: a second shape takes the same decision without a scan.
        assert!(rasterizer.vs16_color_override.contains_key(&heart));
        let _ = rasterizer.shape_line(&queue, "\u{2764}\u{FE0F}", true);
    }

    /// The fast path as it was before the dense `fast_chars` table: one
    /// hash lookup per set, slots inserted column by column. Kept as the
    /// oracle for `fast_table_matches_hashed_reference`.
    fn reference_fast(r: &GlyphRasterizer, text: &str) -> Option<LineGlyphs> {
        let mut out = LineGlyphs::default();
        for (col, ch) in text.chars().enumerate() {
            if ch == ' ' || ch == '\0' {
                continue;
            }
            if ch.width() != Some(1) || r.uncacheable_chars.contains(&ch) {
                return None;
            }
            if r.char_cache_misses.contains(&ch) {
                continue;
            }
            #[allow(clippy::cast_possible_truncation)]
            out.insert(col as u16, ch, *r.char_cache.get(&ch)?);
        }
        Some(out)
    }

    /// The dense table gives exactly the old hashed fast path's answer
    /// (bail vs. not, and the same slots in the same columns), and stays
    /// in sync with the char sets as shaping adds entries, misses and
    /// uncacheable chars.
    #[test]
    fn fast_table_matches_hashed_reference() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let warm = [
            "The quick brown fox jumps over the lazy dog 0123456789",
            "│ ├── src/main.rs ┼ ═ ░▒▓ ⣿⡇ → ← … «» ¿¡ ñ é ü Ω λ Ж",
            "tab\there \u{200B}zero width \u{7}bell",
            "a != b => c -> d",
            "你好 wide 😀 emoji",
        ];
        for line in warm {
            let _ = rasterizer.shape_line(&queue, line, false);
        }
        // Force one ASCII char uncacheable to cover that state.
        rasterizer.mark_uncacheable('q');
        let probes = [
            "",
            "   ",
            "hello world   ",
            "hello\0world\0\0",
            "The lazy dog",
            "The quick dog",
            "│ ├── ┼ ═ ░▒▓ ⣿⡇ → …",
            "zero\u{200B}width",
            "bell\u{7}x",
            "你好",
            "Ω λ Ж ñ é ü   trailing   ",
            "never seen: ʬ ʭ",
            "trailing miss\u{200B}",
        ];
        for probe in probes {
            let got = rasterizer.try_shape_line_fast(probe);
            let want = reference_fast(&rasterizer, probe);
            assert_eq!(
                got.as_ref().map(|g| (&g.by_column, &g.secondary)),
                want.as_ref().map(|g| (&g.by_column, &g.secondary)),
                "fast path disagrees with the hashed reference on {probe:?}",
            );
        }
        // Table invariant: every dense entry equals what the sets say.
        for i in 0..FAST_CHARS_LEN {
            let Some(ch) = char::from_u32(u32::try_from(i).unwrap()) else {
                continue;
            };
            let want = reference_fast(&rasterizer, &ch.to_string()).map(|g| g.by_column);
            let got = rasterizer
                .try_shape_line_fast(&ch.to_string())
                .map(|g| g.by_column);
            assert_eq!(got, want, "fast_chars out of sync for {ch:?}");
        }
    }

    /// `set_font_size` rebuilds the dense table: nothing stays cached at
    /// the old size, so the next line goes through the slow path.
    #[test]
    fn set_font_size_resets_fast_table() {
        let (_device, queue, mut rasterizer) = make_rasterizer();
        let _ = rasterizer.shape_line(&queue, "abc", false);
        assert!(rasterizer.try_shape_line_fast("abc").is_some());
        rasterizer.set_font_size(20.0, DEFAULT_LINE_HEIGHT_RATIO);
        assert!(rasterizer.try_shape_line_fast("abc").is_none());
    }
}
