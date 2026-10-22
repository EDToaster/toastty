//! Rich blocks (OSC 7700): rendered documents that occupy whole grid
//! rows. See `docs/markdown.md`.
//!
//! The terminal core knows nothing about Markdown. The binary injects a
//! [`RichBlockLayout`] into [`crate::Term::set_rich_layout`]; the term
//! asks it to lay a source out for the current geometry and gets back a
//! [`RichLayout`] (row count, an RGBA image `rows * cell_h` tall, the
//! plain text per row and link spans). The rows are written into the
//! grid with a [`RichRow`] tag, the image goes into the
//! [`RichBlockStore`], and the renderer draws the matching slice of the
//! image over every tagged row.

use std::collections::{HashMap, HashSet};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Identifier of a rich block, unique for the lifetime of a `Term`.
pub type RichBlockId = u64;

/// Per-row tag: this grid row shows row `index` of block `block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RichRow {
    pub block: RichBlockId,
    pub index: u16,
}

/// Default hard cap on the rows of one block (`DoS` guard).
pub const RICH_MAX_ROWS: u16 = 2000;

/// Default byte budget for decoded block images held by the store.
pub const RICH_DEFAULT_BUDGET: u64 = 256 * 1024 * 1024;

/// Colours a block is drawn with: the terminal's current palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RichPalette {
    /// Default foreground (sRGB).
    pub fg: [u8; 3],
    /// Default background (sRGB).
    pub bg: [u8; 3],
    /// ANSI colours 0–15 (sRGB).
    pub ansi: [[u8; 3]; 16],
}

impl Default for RichPalette {
    fn default() -> Self {
        let mut ansi = [[0u8; 3]; 16];
        for (i, slot) in ansi.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            let idx = i as u8;
            *slot = toastty_protocols::palette::default_xterm_256(idx);
        }
        Self {
            fg: [0xd8, 0xd8, 0xd8],
            bg: [0x12, 0x12, 0x17],
            ansi,
        }
    }
}

/// Environment the binary supplies for layout (things the term doesn't
/// otherwise know). Set with [`crate::Term::set_rich_env`].
#[derive(Debug, Clone, PartialEq)]
pub struct RichEnv {
    /// Base body font size in physical pixels (font size × scale).
    pub font_px: f32,
    /// Theme palette before OSC 4 overrides.
    pub palette: RichPalette,
    /// The terminal's font family (`font.family`), for monospace text
    /// (code blocks and inline code). `None` leaves the choice to the
    /// layout engine.
    pub mono_family: Option<Arc<str>>,
}

impl Default for RichEnv {
    fn default() -> Self {
        Self {
            font_px: 14.0,
            palette: RichPalette::default(),
            mono_family: None,
        }
    }
}

/// One layout request handed to the injected [`RichBlockLayout`].
#[derive(Debug, Clone, Copy)]
pub struct RichLayoutRequest<'a> {
    /// Block kind from the protocol (only `"md"` in v1).
    pub kind: &'a str,
    pub source: &'a str,
    /// Columns the block spans (the full grid width).
    pub cols: u16,
    /// Cell size in physical pixels.
    pub cell_w: u32,
    pub cell_h: u32,
    pub font_px: f32,
    /// The terminal's font family, for monospace text (code blocks,
    /// which also line up with the cell grid, and inline code). The
    /// engine falls back to its default monospace face when it has no
    /// such family.
    pub mono_family: Option<&'a str>,
    /// Effective palette (theme + OSC 4 overrides for 0–15).
    pub palette: RichPalette,
    /// `[security] markdown_local_files`.
    pub allow_local_files: bool,
    /// Directory relative resources resolve against (`cwd=` key).
    pub base_dir: Option<&'a Path>,
    pub max_rows: u16,
}

/// A plain-text run on one block row (see `toastty_markdown::TextRun`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichTextRun {
    pub col: u16,
    pub text: String,
}

/// A clickable link in block cells.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichLinkSpan {
    pub row: u16,
    pub cols: Range<u16>,
    pub url: String,
}

/// Paints bands of a block's image on demand.
///
/// The layout engine returns one of these instead of a finished image,
/// so a re-layout (e.g. on every step of a window resize) costs no
/// pixels: the renderer paints only the rows it is about to show.
pub trait RichPaint: Send + Sync + std::fmt::Debug {
    /// Straight-alpha RGBA8 for pixel rows `y0 .. y0 + h` of the image
    /// (`width * h * 4` bytes, row-major, top-down). The caller keeps the
    /// band inside the image. `None` when painting failed.
    fn paint(&self, y0: u32, h: u32) -> Option<Vec<u8>>;

    /// Approximate bytes this painter keeps alive (for the store budget).
    fn byte_len(&self) -> u64;
}

/// A finished image held in memory (tests, simple engines).
#[derive(Debug)]
struct PixelsPaint {
    width: u32,
    pixels: Arc<[u8]>,
}

impl RichPaint for PixelsPaint {
    fn paint(&self, y0: u32, h: u32) -> Option<Vec<u8>> {
        let stride = self.width as usize * 4;
        let start = y0 as usize * stride;
        self.pixels
            .get(start..start + h as usize * stride)
            .map(<[u8]>::to_vec)
    }

    fn byte_len(&self) -> u64 {
        self.pixels.len() as u64
    }
}

/// A block image (straight-alpha RGBA8, row-major, top-down), painted
/// lazily in bands. Cheap to clone.
#[derive(Debug, Clone)]
pub struct RichImage {
    pub width: u32,
    pub height: u32,
    /// Rows the painter can produce; rows past it read as transparent.
    painted_height: u32,
    painter: Arc<dyn RichPaint>,
}

impl PartialEq for RichImage {
    fn eq(&self, other: &Self) -> bool {
        self.width == other.width
            && self.height == other.height
            && self.painted_height == other.painted_height
            && Arc::ptr_eq(&self.painter, &other.painter)
    }
}

impl Eq for RichImage {}

impl RichImage {
    /// An image `width` × `height` painted by `painter`.
    #[must_use]
    pub fn new(width: u32, height: u32, painter: Arc<dyn RichPaint>) -> Self {
        Self {
            width,
            height,
            painted_height: height,
            painter,
        }
    }

    /// An image over finished pixels; `None` unless `pixels` is exactly
    /// `width * height * 4` bytes.
    #[must_use]
    pub fn from_pixels(width: u32, height: u32, pixels: impl Into<Arc<[u8]>>) -> Option<Self> {
        let pixels = pixels.into();
        (pixels.len() as u64 == u64::from(width) * u64::from(height) * 4)
            .then(|| Self::new(width, height, Arc::new(PixelsPaint { width, pixels })))
    }

    /// Approximate bytes held.
    #[must_use]
    pub fn byte_len(&self) -> u64 {
        self.painter.byte_len()
    }

    /// Pixels of rows `y0 .. y0 + h` (clamped to the image), always
    /// `width * clamped_h * 4` bytes. `None` when the band is empty or
    /// the painter failed / returned the wrong size.
    #[must_use]
    pub fn paint(&self, y0: u32, h: u32) -> Option<Vec<u8>> {
        let y0 = y0.min(self.height);
        let h = h.min(self.height - y0);
        if h == 0 || self.width == 0 {
            return None;
        }
        let stride = self.width as usize * 4;
        let real = h.min(self.painted_height.saturating_sub(y0));
        let mut px = if real > 0 {
            let px = self.painter.paint(y0, real)?;
            if px.len() != stride * real as usize {
                return None;
            }
            px
        } else {
            Vec::new()
        };
        px.resize(stride * h as usize, 0);
        Some(px)
    }

    /// This image presented with exactly `height` rows: cropped, or
    /// padded with transparent rows. Paints nothing.
    #[must_use]
    pub fn with_height(&self, height: u32) -> Self {
        Self {
            height,
            painted_height: self.painted_height.min(height),
            ..self.clone()
        }
    }
}

/// Result of laying out a block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RichLayout {
    /// Rows the block occupies (>= 1).
    pub rows: u16,
    /// `cols * cell_w` × `rows * cell_h`. `None` = text only.
    pub image: Option<RichImage>,
    /// `text[r]`: the runs on block row `r`. May be shorter than `rows`.
    pub text: Vec<Vec<RichTextRun>>,
    pub links: Vec<RichLinkSpan>,
}

/// Lays out a rich block. Implemented by the binary over
/// `toastty_markdown::Renderer`; tests use fakes.
pub trait RichBlockLayout: Send {
    /// Lay out `req.source`. `None` drops the block (render failure).
    fn layout(&mut self, req: &RichLayoutRequest<'_>) -> Option<RichLayout>;
}

/// The parameters a block was last rendered with.
#[derive(Debug, Clone, PartialEq)]
pub struct RichParams {
    pub cols: u16,
    pub cell_w: u32,
    pub cell_h: u32,
    pub font_px: f32,
    pub mono_family: Option<Arc<str>>,
    pub palette: RichPalette,
    pub allow_local_files: bool,
}

impl RichParams {
    /// Same geometry (row count can't change from colours alone). A
    /// different font family counts: it can rewrap code.
    #[must_use]
    pub fn same_geometry(&self, other: &Self) -> bool {
        self.cols == other.cols
            && self.cell_w == other.cell_w
            && self.cell_h == other.cell_h
            && self.font_px.to_bits() == other.font_px.to_bits()
            && self.mono_family == other.mono_family
    }
}

/// One stored block.
#[derive(Debug)]
pub struct RichBlock {
    pub kind: Arc<str>,
    pub source: Arc<str>,
    pub base_dir: Option<PathBuf>,
    pub params: RichParams,
    /// Rows of the latest layout.
    pub rows: u16,
    /// Latest image; `None` when text-only or evicted.
    image: Option<RichImage>,
    /// The image was dropped to stay within the budget.
    evicted: bool,
    /// Bumped whenever `image` changes.
    revision: u32,
    last_used: AtomicU64,
    /// Rows were partially overwritten: never re-laid out again.
    pub(crate) frozen: bool,
}

impl RichBlock {
    #[must_use]
    pub fn image(&self) -> Option<&RichImage> {
        self.image.as_ref()
    }

    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    #[must_use]
    pub fn is_evicted(&self) -> bool {
        self.evicted
    }
}

/// Block id → source, params and latest image, with a byte budget and
/// LRU eviction of images (evicted blocks fall back to their text).
#[derive(Debug)]
pub struct RichBlockStore {
    blocks: HashMap<RichBlockId, RichBlock>,
    next_id: RichBlockId,
    budget: u64,
    used: u64,
    revision: u32,
    clock: AtomicU64,
}

impl Default for RichBlockStore {
    fn default() -> Self {
        Self::new(RICH_DEFAULT_BUDGET)
    }
}

impl RichBlockStore {
    #[must_use]
    pub fn new(budget: u64) -> Self {
        Self {
            blocks: HashMap::new(),
            next_id: 1,
            budget,
            used: 0,
            revision: 0,
            clock: AtomicU64::new(1),
        }
    }

    /// Bumped on every change the renderer cares about (insert, remove,
    /// image change, eviction).
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Bytes of image data currently held.
    #[must_use]
    pub fn used_bytes(&self) -> u64 {
        self.used
    }

    #[must_use]
    pub fn budget(&self) -> u64 {
        self.budget
    }

    #[must_use]
    pub fn get(&self, id: RichBlockId) -> Option<&RichBlock> {
        self.blocks.get(&id)
    }

    pub(crate) fn get_mut(&mut self, id: RichBlockId) -> Option<&mut RichBlock> {
        self.blocks.get_mut(&id)
    }

    pub fn ids(&self) -> impl Iterator<Item = RichBlockId> + '_ {
        self.blocks.keys().copied()
    }

    /// Image for `id` (marks it recently used). `None` when the block
    /// is unknown, text-only or evicted — draw the cells' text.
    #[must_use]
    pub fn image(&self, id: RichBlockId) -> Option<&RichImage> {
        let b = self.blocks.get(&id)?;
        let t = self.clock.fetch_add(1, Ordering::Relaxed);
        b.last_used.store(t, Ordering::Relaxed);
        b.image.as_ref()
    }

    /// Per-block image revision (re-upload when it changes).
    #[must_use]
    pub fn block_revision(&self, id: RichBlockId) -> Option<u32> {
        self.blocks.get(&id).map(|b| b.revision)
    }

    /// Source text of `id` (for "Copy Markdown").
    #[must_use]
    pub fn source(&self, id: RichBlockId) -> Option<&str> {
        self.blocks.get(&id).map(|b| &*b.source)
    }

    pub(crate) fn alloc_id(&mut self) -> RichBlockId {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    #[allow(clippy::too_many_arguments)]
    pub(crate) fn insert(
        &mut self,
        id: RichBlockId,
        kind: Arc<str>,
        source: Arc<str>,
        base_dir: Option<PathBuf>,
        params: RichParams,
        rows: u16,
        image: Option<RichImage>,
    ) {
        let t = self.clock.fetch_add(1, Ordering::Relaxed);
        self.blocks.insert(
            id,
            RichBlock {
                kind,
                source,
                base_dir,
                params,
                rows,
                image: None,
                evicted: false,
                revision: 0,
                last_used: AtomicU64::new(t),
                frozen: false,
            },
        );
        self.set_image(id, image);
        self.revision = self.revision.wrapping_add(1);
    }

    /// Replace the image of `id`, evicting least-recently-used images
    /// of other blocks to stay within the budget. An image larger than
    /// the whole budget is not kept (text fallback).
    pub(crate) fn set_image(&mut self, id: RichBlockId, image: Option<RichImage>) {
        let Some(b) = self.blocks.get_mut(&id) else {
            return;
        };
        if let Some(old) = b.image.take() {
            self.used -= old.byte_len();
        }
        b.revision = b.revision.wrapping_add(1);
        b.evicted = false;
        let t = self.clock.fetch_add(1, Ordering::Relaxed);
        b.last_used.store(t, Ordering::Relaxed);
        self.revision = self.revision.wrapping_add(1);
        let Some(img) = image else {
            return;
        };
        let need = img.byte_len();
        if need > self.budget {
            b.evicted = true;
            return;
        }
        b.image = Some(img);
        self.used += need;
        self.evict_to_budget(id);
    }

    fn evict_to_budget(&mut self, keep: RichBlockId) {
        while self.used > self.budget {
            let victim = self
                .blocks
                .iter()
                .filter(|(id, b)| **id != keep && b.image.is_some())
                .min_by_key(|(_, b)| b.last_used.load(Ordering::Relaxed))
                .map(|(id, _)| *id);
            let Some(v) = victim else { break };
            let b = self.blocks.get_mut(&v).expect("victim exists");
            if let Some(img) = b.image.take() {
                self.used -= img.byte_len();
            }
            b.evicted = true;
            b.revision = b.revision.wrapping_add(1);
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Change the budget; evicts as needed.
    pub fn set_budget(&mut self, budget: u64) {
        self.budget = budget;
        self.evict_to_budget(0);
    }

    pub(crate) fn remove(&mut self, id: RichBlockId) {
        if let Some(b) = self.blocks.remove(&id) {
            if let Some(img) = b.image {
                self.used -= img.byte_len();
            }
            self.revision = self.revision.wrapping_add(1);
        }
    }

    /// Drop every block not in `live`. Returns how many were freed.
    pub(crate) fn retain_live(&mut self, live: &HashSet<RichBlockId>) -> usize {
        let dead: Vec<RichBlockId> = self
            .blocks
            .keys()
            .filter(|id| !live.contains(id))
            .copied()
            .collect();
        for id in &dead {
            self.remove(*id);
        }
        dead.len()
    }
}

/// Parsed `OSC 7700` sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Osc7700 {
    pub kind: String,
    pub cwd: Option<PathBuf>,
    pub source: String,
}

fn b64_decode(s: &[u8]) -> Option<Vec<u8>> {
    use base64::Engine as _;
    use base64::engine::{DecodePaddingMode, GeneralPurpose, GeneralPurposeConfig};
    const ENGINE: GeneralPurpose = GeneralPurpose::new(
        &base64::alphabet::STANDARD,
        GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
    );
    ENGINE.decode(s).ok()
}

/// Parse the params of `OSC 7700 ; <kind> ; <keys> ; <base64> ST`
/// (`params[0]` is `7700`). `None` for anything malformed: unknown
/// kind, invalid base64 or UTF-8 in the payload, wrong field count.
/// Unknown keys and an undecodable `cwd=` are ignored.
pub fn parse_osc_7700(params: &[&[u8]]) -> Option<Osc7700> {
    let (kind, keys, payload) = match params {
        [_, kind, keys, payload] => (*kind, *keys, *payload),
        _ => return None,
    };
    if kind != b"md" {
        return None;
    }
    let mut cwd = None;
    for kv in keys.split(|b| *b == b':') {
        if kv.is_empty() {
            continue;
        }
        let mut it = kv.splitn(2, |b| *b == b'=');
        let k = it.next().unwrap_or_default();
        let v = it.next().unwrap_or_default();
        if k == b"cwd"
            && let Some(bytes) = b64_decode(v)
            && let Ok(s) = String::from_utf8(bytes)
            && !s.is_empty()
        {
            cwd = Some(PathBuf::from(s));
        }
    }
    let bytes = b64_decode(payload)?;
    let source = String::from_utf8(bytes).ok()?;
    Some(Osc7700 {
        kind: "md".to_owned(),
        cwd,
        source,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(bytes: usize) -> RichImage {
        RichImage::from_pixels(1, u32::try_from(bytes / 4).unwrap(), vec![0u8; bytes]).unwrap()
    }

    fn params() -> RichParams {
        RichParams {
            cols: 10,
            cell_w: 8,
            cell_h: 16,
            font_px: 14.0,
            mono_family: None,
            palette: RichPalette::default(),
            allow_local_files: false,
        }
    }

    fn ins(s: &mut RichBlockStore, bytes: usize) -> RichBlockId {
        let id = s.alloc_id();
        s.insert(
            id,
            "md".into(),
            "src".into(),
            None,
            params(),
            1,
            Some(img(bytes)),
        );
        id
    }

    #[test]
    fn parse_valid_padded_and_unpadded() {
        let p = parse_osc_7700(&[b"7700", b"md", b"", b"aGk="]).unwrap();
        assert_eq!(p.source, "hi");
        assert_eq!(p.cwd, None);
        let p = parse_osc_7700(&[b"7700", b"md", b"", b"aGk"]).unwrap();
        assert_eq!(p.source, "hi");
    }

    #[test]
    fn parse_keys_cwd_and_unknown() {
        // cwd=/tmp (L3RtcA==), unknown key ignored, empty segments ok.
        let p = parse_osc_7700(&[b"7700", b"md", b"x=1::cwd=L3RtcA==:zz", b"aGk="]).unwrap();
        assert_eq!(p.cwd.as_deref(), Some(Path::new("/tmp")));
        // Bad cwd value: key ignored, block kept.
        let p = parse_osc_7700(&[b"7700", b"md", b"cwd=!!!", b"aGk="]).unwrap();
        assert_eq!(p.cwd, None);
        assert_eq!(p.source, "hi");
    }

    #[test]
    fn parse_rejects_invalid() {
        assert!(parse_osc_7700(&[b"7700", b"md", b"", b"!!!"]).is_none());
        assert!(parse_osc_7700(&[b"7700", b"html", b"", b"aGk="]).is_none());
        assert!(parse_osc_7700(&[b"7700", b"md", b"aGk="]).is_none());
        assert!(parse_osc_7700(&[b"7700", b"md", b"", b"aGk=", b"x"]).is_none());
        assert!(parse_osc_7700(&[b"7700"]).is_none());
        // Invalid UTF-8 (0xff) → dropped.
        assert!(parse_osc_7700(&[b"7700", b"md", b"", b"/w=="]).is_none());
    }

    #[test]
    fn empty_payload_is_an_empty_document() {
        let p = parse_osc_7700(&[b"7700", b"md", b"", b""]).unwrap();
        assert_eq!(p.source, "");
    }

    #[test]
    fn store_evicts_lru_to_text_fallback() {
        let mut s = RichBlockStore::new(100);
        let a = ins(&mut s, 40);
        let b = ins(&mut s, 40);
        assert_eq!(s.used_bytes(), 80);
        // Touch a so b is the LRU.
        assert!(s.image(a).is_some());
        let rev = s.revision();
        let c = ins(&mut s, 40);
        assert_ne!(s.revision(), rev);
        assert!(s.image(b).is_none(), "LRU block evicted");
        assert!(s.get(b).unwrap().is_evicted());
        assert!(s.image(a).is_some());
        assert!(s.image(c).is_some());
        assert_eq!(s.used_bytes(), 80);
        // The source survives eviction.
        assert_eq!(s.source(b), Some("src"));
    }

    #[test]
    fn store_rejects_image_larger_than_budget() {
        let mut s = RichBlockStore::new(10);
        let a = ins(&mut s, 40);
        assert!(s.image(a).is_none());
        assert!(s.get(a).unwrap().is_evicted());
        assert_eq!(s.used_bytes(), 0);
    }

    #[test]
    fn store_set_image_bumps_block_revision() {
        let mut s = RichBlockStore::new(1000);
        let a = ins(&mut s, 40);
        let r0 = s.block_revision(a).unwrap();
        s.set_image(a, Some(img(80)));
        assert_ne!(s.block_revision(a).unwrap(), r0);
        assert_eq!(s.used_bytes(), 80);
    }

    #[test]
    fn store_retain_live_and_budget_shrink() {
        let mut s = RichBlockStore::new(1000);
        let a = ins(&mut s, 40);
        let b = ins(&mut s, 40);
        let live: HashSet<_> = [a].into_iter().collect();
        assert_eq!(s.retain_live(&live), 1);
        assert!(s.get(b).is_none());
        assert_eq!(s.used_bytes(), 40);
        s.set_budget(10);
        assert!(s.image(a).is_none());
        assert_eq!(s.used_bytes(), 0);
    }

    #[test]
    fn image_with_height_crops_and_pads() {
        let i = RichImage::from_pixels(1, 2, vec![1, 1, 1, 1, 2, 2, 2, 2]).unwrap();
        assert_eq!(i.with_height(1).paint(0, 9).unwrap(), &[1, 1, 1, 1]);
        assert_eq!(
            i.with_height(3).paint(0, 3).unwrap(),
            &[1, 1, 1, 1, 2, 2, 2, 2, 0, 0, 0, 0]
        );
        // Bands: the padded row alone, and a band straddling the edge.
        assert_eq!(i.with_height(3).paint(2, 1).unwrap(), &[0, 0, 0, 0]);
        assert_eq!(
            i.with_height(3).paint(1, 5).unwrap(),
            &[2, 2, 2, 2, 0, 0, 0, 0]
        );
        assert_eq!(i.paint(2, 1), None);
        assert!(RichImage::from_pixels(2, 2, vec![0; 4]).is_none());
    }
}
