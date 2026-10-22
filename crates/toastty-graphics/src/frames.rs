//! Kitty animation frames: `a=f` frame data, `a=a` animation control,
//! `a=c` frame composition and the `d=f` / `d=F` frame delete.
//!
//! Modelled on kitty 0.49.2 (`graphics.c`), and checked against it live
//! (see `docs/decisions/kitty-animation.md`):
//!
//! - Frames are stored the way the client sent them: a rectangle of
//!   pixels, plus either a background colour (`Y=`) or a lazy reference
//!   to a base frame (`c=`). A frame's full pixels are composed on demand
//!   ("coalesced"). So editing a base frame later shows through in every
//!   frame built on it, as in kitty, and a GIF sent as deltas costs only
//!   its deltas in memory.
//! - Like kitty, a new frame whose base chain is long (5 frames, or two
//!   image areas of drawn pixels) is coalesced right away, which cuts the
//!   reference to its base.
//! - Editing a frame (`a=f,r=`) or composing onto one (`a=c`) coalesces
//!   it into a full, self-contained frame.
//!
//! The current frame is kept composed in the image's display buffer (the
//! registry's [`ImageData`] for the id), which is what every placement
//! and Unicode placeholder draws. Advancing to a frame built on the
//! frame on display composes only the new frame's rectangle onto the
//! buffer, so a delta-encoded GIF costs its delta per tick.
//!
//! Frame numbers on the wire are 1-based; frame 1 is the root frame (the
//! transmitted image). Internally frames are indexed from 0 and carry a
//! stable id for base references.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::kitty::header::{AnimationKeys, CompositionKeys, FrameKeys};
use crate::kitty::reply::ErrorCode;
use crate::registry::ImageData;

/// Gap of a new frame sent without `z=` (spec: 40 ms). The root frame
/// starts with no gap.
pub const DEFAULT_GAP_MS: u32 = 40;

/// Deepest base-frame chain [`Animation::coalesce`] follows. Chains are
/// kept short by the coalesce-on-create rule, so this only guards
/// against bugs.
const MAX_CHAIN_DEPTH: usize = 32;

/// A frame command that failed: the reply's code and detail, in kitty's
/// words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameError {
    pub code: ErrorCode,
    pub detail: String,
}

impl FrameError {
    fn new(code: ErrorCode, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}

/// `a=a,s=` animation state. New images are stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AnimationState {
    /// `s=1`: frames change only when the client says so (`a=a,c=`).
    #[default]
    Stopped,
    /// `s=2`: run, but stop at the last frame and wait for more.
    Loading,
    /// `s=3`: run, looping `v=` times.
    Running,
}

/// The 1-based frame an `a=f` command targets, given the image's frame
/// count: `r=` when it names an existing frame (an edit), otherwise a new
/// frame after the last. kitty tags both the reply and any error with
/// it (`i=1,r=3;…`).
#[must_use]
pub fn frame_target(frame_count: u32, edit_frame: u32) -> u32 {
    if edit_frame == 0 || edit_frame > frame_count {
        frame_count + 1
    } else {
        edit_frame
    }
}

/// Frame pixels as stored: LZ4-compressed unless that doesn't pay.
///
/// Frames are read back only when a frame is composed (a new frame on
/// display, an edit), and LZ4 decompresses at memory speed, so keeping
/// them compressed costs little CPU and stretches the frame budget a long
/// way: GIF frames are palette images, often with flat runs.
#[derive(Clone)]
enum Pixels {
    Raw(Vec<u8>),
    Lz4 { bytes: Vec<u8>, len: usize },
}

impl Pixels {
    /// Smaller blocks aren't worth compressing.
    const MIN_COMPRESS: usize = 4096;

    fn new(raw: Vec<u8>) -> Self {
        if raw.len() < Self::MIN_COMPRESS {
            return Self::Raw(raw);
        }
        let bytes = lz4_flex::block::compress(&raw);
        // Keep it raw unless compression saves at least an eighth.
        if bytes.len() > raw.len() - raw.len() / 8 {
            return Self::Raw(raw);
        }
        Self::Lz4 {
            bytes,
            len: raw.len(),
        }
    }

    /// Bytes held in memory.
    fn stored_len(&self) -> usize {
        match self {
            Self::Raw(v) => v.len(),
            Self::Lz4 { bytes, .. } => bytes.len(),
        }
    }

    /// The pixels, decompressed if need be.
    fn get(&self) -> std::borrow::Cow<'_, [u8]> {
        match self {
            Self::Raw(v) => std::borrow::Cow::Borrowed(v),
            Self::Lz4 { bytes, len } => std::borrow::Cow::Owned(
                // Only ever fed our own output; a failure would be a bug.
                lz4_flex::block::decompress(bytes, *len).unwrap_or_else(|_| vec![0; *len]),
            ),
        }
    }

    /// Overwrite `out` with the pixels, reusing its allocation.
    fn write_into(&self, out: &mut Vec<u8>) {
        match self {
            Self::Raw(v) => {
                out.clear();
                out.extend_from_slice(v);
            }
            Self::Lz4 { bytes, len } => {
                out.resize(*len, 0);
                if lz4_flex::block::decompress_into(bytes, out).is_err() {
                    out.fill(0);
                }
            }
        }
    }
}

impl std::fmt::Debug for Pixels {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Raw(v) => write!(f, "Raw({} bytes)", v.len()),
            Self::Lz4 { bytes, len } => write!(f, "Lz4({len} -> {} bytes)", bytes.len()),
        }
    }
}

/// One stored frame.
#[derive(Debug, Clone)]
struct Frame {
    /// Stable id, for base references. Never reused within an image.
    id: u32,
    /// Where `data` goes in the image, and its size, in pixels.
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    /// `width * height` RGBA8 pixels.
    data: Pixels,
    /// Id of the frame this one is drawn over (`c=`), if any.
    base: Option<u32>,
    /// Canvas colour (`Y=`, RGBA) under `data` when there is no base.
    background: u32,
    /// Alpha-blend `data` onto the canvas (default), or overwrite (`X=1`).
    alpha_blend: bool,
    /// Milliseconds to show this frame for; 0 = gapless (skipped).
    gap_ms: u32,
}

/// Bytes charged per frame on top of its pixels: the `Frame` itself, its
/// pixel allocation, and the index entries that find it. Without it a
/// flood of 1-pixel frames would be charged 4 bytes each.
pub const FRAME_OVERHEAD: u64 = std::mem::size_of::<Frame>() as u64 + 96;

impl Frame {
    fn covers(&self, width: u32, height: u32) -> bool {
        self.x == 0 && self.y == 0 && self.width == width && self.height == height
    }

    /// Bytes charged for this frame.
    fn cost(&self) -> u64 {
        self.data.stored_len() as u64 + FRAME_OVERHEAD
    }

    /// Turn into a self-contained full frame holding `pixels`.
    fn make_full(&mut self, pixels: Vec<u8>, width: u32, height: u32) {
        self.x = 0;
        self.y = 0;
        self.width = width;
        self.height = height;
        self.data = Pixels::new(pixels);
        self.base = None;
        self.background = 0;
        self.alpha_blend = false;
    }
}

/// A frame ready to go into an [`Animation`]: what
/// [`Animation::prepare_frame`] and [`Animation::prepare_compose`] build,
/// so the caller can check its size against a quota before
/// [`Animation::commit`] stores it.
#[derive(Debug)]
pub struct PreparedFrame {
    /// 1-based frame number (the reply's `r=`).
    pub number: u32,
    /// Index of the frame replaced; `None` appends a new frame.
    replace: Option<usize>,
    /// New gap for a replaced frame (`a=f,r=,z=`).
    gap: Option<u32>,
    frame: Frame,
}

/// Playback settings an `a=a` can give an image before it has frames:
/// the root frame's gap, the state and the loop count. Kept on a still
/// image instead of a whole [`Animation`] (which would copy the root's
/// pixels), and handed to [`Animation::new`] when frames arrive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Playback {
    pub root_gap_ms: u32,
    pub state: AnimationState,
    pub max_loops: u32,
}

impl Playback {
    /// `a=a` on an image that has only its root frame: what of it sticks
    /// (kitty applies the same keys to its single frame).
    pub fn apply(&mut self, keys: &AnimationKeys) {
        if keys.frame == 1 && keys.gap_ms != 0 {
            self.root_gap_ms = keys.gap_ms.max(0) as u32;
        }
        if let Some(state) = state_of(keys.state) {
            self.state = state;
        }
        if keys.loops != 0 {
            self.max_loops = keys.loops - 1;
        }
    }
}

fn state_of(s: u32) -> Option<AnimationState> {
    match s {
        1 => Some(AnimationState::Stopped),
        2 => Some(AnimationState::Loading),
        3 => Some(AnimationState::Running),
        _ => None,
    }
}

/// Frames and playback state of one animated image.
///
/// Every operation a client can repeat (adding a frame, a tick, a gap
/// change) is O(1) in the number of frames: the byte total, the count of
/// frames with a gap, and the id → index map are kept up to date rather
/// than recomputed.
#[derive(Debug, Clone)]
pub struct Animation {
    width: u32,
    height: u32,
    /// The frames, in order; `[0]` is the root frame.
    frames: Vec<Frame>,
    /// Frame id → index in `frames`.
    index: HashMap<u32, usize>,
    /// Deleted frames that other frames are still built on, by id. They
    /// keep their pixels for those frames and are freed with the last
    /// one; see [`Self::delete_frame`].
    hidden: HashMap<u32, Frame>,
    /// Frame id → how many frames (shown or hidden) are built on it.
    refs: HashMap<u32, u32>,
    /// Sum of [`Frame::cost`] over `frames` and `hidden`.
    stored: u64,
    /// Frames in `frames` with a non-zero gap.
    gapped: usize,
    next_id: u32,
    /// Index of the frame on display.
    current: usize,
    state: AnimationState,
    /// Wraps allowed before playback stops; 0 = loop forever.
    max_loops: u32,
    /// Wraps so far. Reset by `s=1`.
    current_loop: u32,
    /// When the current frame went up, once playback has looked at it.
    shown_at: Option<Instant>,
    /// Id of the frame whose coalesced pixels the display buffer holds,
    /// or `None` when they need recomposing.
    displayed: Option<u32>,
}

impl Animation {
    /// Start animating `root`, whose pixels are also in the display
    /// buffer, with the settings an earlier `a=a` gave it.
    #[must_use]
    pub fn new(root: &ImageData, playback: &Playback) -> Self {
        let frame = Frame {
            id: 1,
            x: 0,
            y: 0,
            width: root.width,
            height: root.height,
            data: Pixels::new(root.pixels.clone()),
            base: None,
            background: 0,
            alpha_blend: false,
            gap_ms: playback.root_gap_ms,
        };
        Self {
            width: root.width,
            height: root.height,
            stored: frame.cost(),
            gapped: usize::from(frame.gap_ms > 0),
            frames: vec![frame],
            index: HashMap::from([(1, 0)]),
            hidden: HashMap::new(),
            refs: HashMap::new(),
            next_id: 2,
            current: 0,
            state: playback.state,
            max_loops: playback.max_loops,
            current_loop: 0,
            shown_at: None,
            displayed: Some(1),
        }
    }

    /// Number of frames, root included.
    #[must_use]
    pub fn frame_count(&self) -> u32 {
        self.frames.len() as u32
    }

    /// 1-based number of the frame on display.
    #[must_use]
    pub fn current_frame(&self) -> u32 {
        self.current as u32 + 1
    }

    /// Playback state.
    #[must_use]
    pub fn state(&self) -> AnimationState {
        self.state
    }

    /// Gap of 1-based frame `number`, in ms (0 = gapless).
    #[must_use]
    pub fn gap_ms(&self, number: u32) -> Option<u32> {
        let i = (number as usize).checked_sub(1)?;
        self.frames.get(i).map(|f| f.gap_ms)
    }

    /// Bytes charged for the frames held, hidden bases included, with
    /// [`FRAME_OVERHEAD`] each (the display buffer is not included). O(1).
    #[must_use]
    pub fn byte_size(&self) -> u64 {
        self.stored
    }

    /// Deleted frames kept as bases for others.
    #[must_use]
    pub fn hidden_frames(&self) -> usize {
        self.hidden.len()
    }

    fn canvas_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    /// The frame with `id`, shown or hidden.
    fn frame(&self, id: u32) -> Option<&Frame> {
        match self.index.get(&id) {
            Some(&i) => self.frames.get(i),
            None => self.hidden.get(&id),
        }
    }

    fn index_of_number(&self, number: u32) -> Option<usize> {
        let i = (number as usize).checked_sub(1)?;
        (i < self.frames.len()).then_some(i)
    }

    fn set_gap(&mut self, idx: usize, gap_ms: u32) {
        let f = &mut self.frames[idx];
        self.gapped = self.gapped - usize::from(f.gap_ms > 0) + usize::from(gap_ms > 0);
        f.gap_ms = gap_ms;
    }

    /// One more frame is built on `base`.
    fn retain_base(&mut self, base: Option<u32>) {
        if let Some(id) = base {
            *self.refs.entry(id).or_insert(0) += 1;
        }
    }

    /// One fewer frame is built on `base`. A hidden base nobody uses any
    /// more is freed, and so on down its own chain.
    fn release_base(&mut self, base: Option<u32>) {
        let mut next = base;
        while let Some(id) = next {
            next = None;
            let Some(n) = self.refs.get_mut(&id) else {
                break;
            };
            *n -= 1;
            if *n == 0 {
                self.refs.remove(&id);
                if let Some(h) = self.hidden.remove(&id) {
                    self.stored -= h.cost();
                    next = h.base;
                }
            }
        }
    }

    /// Full RGBA pixels of frame `idx`: its base (or background) with
    /// its own rectangle composed on top.
    fn coalesce(&self, idx: usize) -> Vec<u8> {
        self.coalesce_frame(&self.frames[idx], 0)
    }

    fn coalesce_frame(&self, f: &Frame, depth: usize) -> Vec<u8> {
        let base = f
            .base
            .and_then(|id| self.frame(id))
            .filter(|_| depth < MAX_CHAIN_DEPTH);
        let mut canvas = match base {
            Some(b) => self.coalesce_frame(b, depth + 1),
            // kitty uses a full-size frame's pixels as they are: `Y=`
            // only fills what the frame doesn't cover.
            None if f.covers(self.width, self.height) => return f.data.get().into_owned(),
            None => filled(self.canvas_len(), f.background),
        };
        compose(
            &mut canvas,
            self.width,
            self.height,
            &Rect::of(f),
            &f.data.get(),
            f.alpha_blend,
        );
        canvas
    }

    /// True if the frame at `idx` is, or is built on, frame `id`.
    fn depends_on(&self, idx: usize, id: u32) -> bool {
        let mut f = &self.frames[idx];
        for _ in 0..=MAX_CHAIN_DEPTH {
            if f.id == id {
                return true;
            }
            match f.base.and_then(|b| self.frame(b)) {
                Some(b) => f = b,
                None => return false,
            }
        }
        false
    }

    /// kitty's `reference_chain_too_large`: walking from `base` down its
    /// base chain, 5 frames or two image areas of drawn pixels make a new
    /// frame on top of it coalesce at once instead of referencing it.
    fn chain_too_large(&self, base: &Frame) -> bool {
        let limit = 2 * u64::from(self.width) * u64::from(self.height);
        let mut f = base;
        let mut drawn = u64::from(f.width) * u64::from(f.height);
        let mut num = 1;
        while drawn < limit && num < 5 {
            let Some(next) = f.base.and_then(|id| self.frame(id)) else {
                break;
            };
            f = next;
            drawn += u64::from(f.width) * u64::from(f.height);
            num += 1;
        }
        num >= 5 || drawn >= limit
    }

    /// Build the frame an `a=f` command makes from `keys` and the
    /// decoded `data`, without storing it. Errors are kitty's (EINVAL).
    pub fn prepare_frame(
        &self,
        keys: &FrameKeys,
        data: ImageData,
    ) -> Result<PreparedFrame, FrameError> {
        let number = frame_target(self.frame_count(), keys.edit_frame);
        if data.width > self.width {
            return Err(FrameError::new(
                ErrorCode::Einval,
                format!(
                    "Frame width {} larger than image width: {}",
                    data.width, self.width
                ),
            ));
        }
        if data.height > self.height {
            return Err(FrameError::new(
                ErrorCode::Einval,
                format!(
                    "Frame height {} larger than image height: {}",
                    data.height, self.height
                ),
            ));
        }
        let rect = Rect {
            x: keys.x,
            y: keys.y,
            width: data.width,
            height: data.height,
        };
        let alpha_blend = keys.blend_mode != 1;
        if let Some(idx) = self.index_of_number(number) {
            return Ok(self.prepare_edit(idx, keys, &rect, &data.pixels));
        }
        let gap_ms = match keys.gap_ms {
            0 => DEFAULT_GAP_MS,
            g if g < 0 => 0,
            g => g as u32,
        };
        let mut rect = rect;
        let mut pixels = data.pixels;
        let mut base_id = None;
        // `c=` naming the frame being created: kitty adds the frame before
        // looking its base up, so this finds the new frame itself and
        // succeeds. Its pixels come out of a self-reference; ours are
        // simply drawn on the background.
        if keys.base_frame != 0 && keys.base_frame != number {
            let Some(base) = self.index_of_number(keys.base_frame) else {
                return Err(FrameError::new(
                    ErrorCode::Einval,
                    format!("No frame with number: {} found", keys.base_frame),
                ));
            };
            let base = &self.frames[base];
            if base.base.is_some() && self.chain_too_large(base) {
                let mut canvas = self.coalesce_frame(base, 0);
                compose(
                    &mut canvas,
                    self.width,
                    self.height,
                    &rect,
                    &pixels,
                    alpha_blend,
                );
                rect = Rect {
                    x: 0,
                    y: 0,
                    width: self.width,
                    height: self.height,
                };
                pixels = canvas;
            } else {
                base_id = Some(base.id);
            }
        }
        let frame = Frame {
            id: self.next_id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
            data: Pixels::new(pixels),
            base: base_id,
            background: keys.background,
            alpha_blend,
            gap_ms,
        };
        Ok(PreparedFrame {
            number,
            replace: None,
            gap: None,
            frame,
        })
    }

    /// `a=f,r=` naming frame `idx`: compose `pixels` onto the frame as it
    /// looks now; the result is a full frame with no base. `c=` and `Y=`
    /// don't apply.
    fn prepare_edit(
        &self,
        idx: usize,
        keys: &FrameKeys,
        rect: &Rect,
        pixels: &[u8],
    ) -> PreparedFrame {
        let mut canvas = self.coalesce(idx);
        compose(
            &mut canvas,
            self.width,
            self.height,
            rect,
            pixels,
            keys.blend_mode != 1,
        );
        let mut frame = self.frames[idx].clone();
        frame.make_full(canvas, self.width, self.height);
        PreparedFrame {
            number: idx as u32 + 1,
            replace: Some(idx),
            gap: (keys.gap_ms != 0).then(|| keys.gap_ms.max(0) as u32),
            frame,
        }
    }

    /// Build the result of an `a=c` command: frame `r=` (source) composed
    /// onto frame `c=` (destination). Errors and their order are
    /// kitty's; `image_id` goes into the ENOENT detail.
    pub fn prepare_compose(
        &self,
        keys: &CompositionKeys,
        image_id: u32,
    ) -> Result<PreparedFrame, FrameError> {
        let Some(src) = self.index_of_number(keys.src_frame) else {
            return Err(FrameError::new(
                ErrorCode::Enoent,
                format!(
                    "No source frame number {} exists in image id: {image_id}\n",
                    keys.src_frame
                ),
            ));
        };
        let Some(dest) = self.index_of_number(keys.dest_frame) else {
            return Err(FrameError::new(
                ErrorCode::Enoent,
                format!(
                    "No destination frame number {} exists in image id: {image_id}\n",
                    keys.dest_frame
                ),
            ));
        };
        let width = if keys.width == 0 {
            self.width
        } else {
            keys.width
        };
        let height = if keys.height == 0 {
            self.height
        } else {
            keys.height
        };
        let fits = |x: u32, y: u32| {
            u64::from(x) + u64::from(width) <= u64::from(self.width)
                && u64::from(y) + u64::from(height) <= u64::from(self.height)
        };
        if !fits(keys.dest_x, keys.dest_y) {
            return Err(FrameError::new(
                ErrorCode::Einval,
                "The destination rectangle is out of bounds",
            ));
        }
        if !fits(keys.src_x, keys.src_y) {
            return Err(FrameError::new(
                ErrorCode::Einval,
                "The source rectangle is out of bounds",
            ));
        }
        if src == dest {
            let overlaps = |a: u32, b: u32, len: u32| a.max(b) < a.min(b) + len;
            if overlaps(keys.src_x, keys.dest_x, width) && overlaps(keys.src_y, keys.dest_y, height)
            {
                return Err(FrameError::new(
                    ErrorCode::Einval,
                    "The source and destination rectangles overlap and the src and destination frames are the same",
                ));
            }
        }
        let src_px = self.coalesce(src);
        let mut canvas = self.coalesce(dest);
        // Cut the source rectangle out, then compose it like frame data.
        let row = width as usize * 4;
        let mut cut = Vec::with_capacity(row * height as usize);
        for y in 0..height as usize {
            let start = ((keys.src_y as usize + y) * self.width as usize + keys.src_x as usize) * 4;
            cut.extend_from_slice(&src_px[start..start + row]);
        }
        let rect = Rect {
            x: keys.dest_x,
            y: keys.dest_y,
            width,
            height,
        };
        compose(
            &mut canvas,
            self.width,
            self.height,
            &rect,
            &cut,
            keys.blend_mode != 1,
        );
        let mut frame = self.frames[dest].clone();
        frame.make_full(canvas, self.width, self.height);
        Ok(PreparedFrame {
            number: keys.dest_frame,
            replace: Some(dest),
            gap: None,
            frame,
        })
    }

    /// Bytes `prepared` adds to [`Self::byte_size`] once committed
    /// (negative when it shrinks a frame). An upper bound: replacing a
    /// frame may also free a hidden base it was built on.
    #[must_use]
    pub fn growth(&self, prepared: &PreparedFrame) -> i64 {
        let old = prepared.replace.map_or(0, |i| self.frames[i].cost() as i64);
        prepared.frame.cost() as i64 - old
    }

    /// Store a prepared frame. Returns true if the display buffer
    /// changed.
    pub fn commit(&mut self, prepared: PreparedFrame, display: &mut ImageData) -> bool {
        let Some(idx) = prepared.replace else {
            let frame = prepared.frame;
            self.next_id += 1;
            self.stored += frame.cost();
            self.gapped += usize::from(frame.gap_ms > 0);
            self.retain_base(frame.base);
            self.index.insert(frame.id, self.frames.len());
            self.frames.push(frame);
            return false;
        };
        let new = prepared.frame;
        let old = std::mem::replace(&mut self.frames[idx], new);
        self.stored = self.stored - old.cost() + self.frames[idx].cost();
        // The new frame is self-contained: the old one's base loses a user.
        self.release_base(old.base);
        if let Some(gap) = prepared.gap {
            self.set_gap(idx, gap);
        }
        // Frames built on this one change with it. Recompose the display
        // only if the frame on it is one of them (kitty keeps showing the
        // old pixels until the next frame change; see the decision note).
        if self.depends_on(self.current, old.id) {
            self.displayed = None;
            self.refresh_display(display)
        } else {
            false
        }
    }

    /// `a=a`: gaps, current frame, state and loop count, as kitty
    /// applies them. Out-of-range frames are ignored silently. Returns
    /// true if the display buffer changed.
    pub fn control(&mut self, keys: &AnimationKeys, display: &mut ImageData) -> bool {
        if keys.frame != 0
            && keys.gap_ms != 0
            && let Some(idx) = self.index_of_number(keys.frame)
        {
            self.set_gap(idx, keys.gap_ms.max(0) as u32);
        }
        let mut changed = false;
        if keys.current_frame != 0
            && let Some(idx) = self.index_of_number(keys.current_frame)
            && idx != self.current
        {
            self.current = idx;
            self.shown_at = None;
            changed = self.refresh_display(display);
        }
        if let Some(state) = state_of(keys.state) {
            // Restarting: like kitty, the current frame gets its full gap
            // from now, not from when it first went up.
            if self.state == AnimationState::Stopped && state != AnimationState::Stopped {
                self.shown_at = None;
            }
            self.state = state;
        }
        if keys.loops != 0 {
            self.max_loops = keys.loops - 1;
        }
        if self.state == AnimationState::Stopped {
            self.current_loop = 0;
        }
        changed
    }

    /// `d=f` / `d=F` on an image with at least two frames: delete frame
    /// `number` (`r=`; 0 or past the end clamp to the root and the last
    /// frame), adjusting the current frame exactly as kitty does. Returns
    /// true if the display buffer changed.
    ///
    /// A frame other frames are built on stays behind, hidden, as their
    /// base: their pixels stay right (kitty shows them stale) and nothing
    /// is allocated. It is freed with the last frame built on it.
    pub fn delete_frame(&mut self, number: u32, display: &mut ImageData) -> bool {
        debug_assert!(self.frames.len() > 1);
        let number = number.clamp(1, self.frame_count());
        let idx = number as usize - 1;
        let frame = self.frames.remove(idx);
        self.gapped -= usize::from(frame.gap_ms > 0);
        let id = frame.id;
        if self.refs.contains_key(&id) {
            self.hidden.insert(id, frame);
        } else {
            self.stored -= frame.cost();
            self.release_base(frame.base);
            if self.displayed == Some(id) {
                self.displayed = None;
            }
        }
        for (i, f) in self.frames.iter().enumerate().skip(idx) {
            self.index.insert(f.id, i);
        }
        self.index.remove(&id);
        // kitty compares the deleted frame's index among the *extra*
        // frames (root excluded) with the current frame's index among
        // all frames. Keep its arithmetic: it decides which frame shows.
        let removed = idx.saturating_sub(1);
        let last = self.frames.len() - 1;
        if self.current > last {
            self.current = last;
        } else if removed < self.current {
            self.current -= 1;
        }
        self.shown_at = None;
        self.refresh_display(display)
    }

    /// Whether playback would move on by itself: running or loading, with
    /// more than one frame, some frame with a gap, and loops left. O(1).
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.state != AnimationState::Stopped
            && self.frames.len() > 1
            && self.gapped > 0
            && (self.max_loops == 0 || self.current_loop < self.max_loops)
    }

    /// Advance playback to `now` (kitty's `scan_active_animations`).
    /// Gapless frames are skipped; at the last frame a loading
    /// animation waits and a running one wraps, until its loops are used
    /// up. A frame due within `slack` of `now` is shown now, so frames of
    /// several animations that fall close together share one redraw; it
    /// keeps its own schedule, so playback doesn't drift. Returns whether
    /// the display buffer changed and when the next frame is due.
    pub fn tick(
        &mut self,
        now: Instant,
        slack: Duration,
        display: &mut ImageData,
    ) -> (bool, Option<Instant>) {
        if !self.is_running() {
            return (false, None);
        }
        let shown = *self.shown_at.get_or_insert(now);
        let mut due = shown + gap(&self.frames[self.current]);
        let mut changed = false;
        if now + slack >= due {
            let start = self.current;
            loop {
                let next = (self.current + 1) % self.frames.len();
                if next == 0 {
                    if self.state == AnimationState::Loading {
                        break;
                    }
                    self.current_loop += 1;
                    if self.max_loops != 0 && self.current_loop >= self.max_loops {
                        break;
                    }
                }
                self.current = next;
                if self.frames[next].gap_ms != 0 {
                    break;
                }
            }
            if self.current != start {
                changed = self.refresh_display(display);
                // Early by up to `slack`: count the gap from when the
                // frame was due. Late: from now, as kitty does.
                let shown = now.max(due);
                self.shown_at = Some(shown);
                due = shown + gap(&self.frames[self.current]);
            }
        }
        (changed, (due > now).then_some(due))
    }

    /// Make the display buffer show the current frame. When it already
    /// shows the frame the current one is built on, only the current
    /// frame's rectangle is composed. Returns true if it changed.
    fn refresh_display(&mut self, display: &mut ImageData) -> bool {
        let f = &self.frames[self.current];
        if self.displayed == Some(f.id) {
            return false;
        }
        if f.base.is_some() && f.base == self.displayed && display.pixels.len() == self.canvas_len()
        {
            compose(
                &mut display.pixels,
                self.width,
                self.height,
                &Rect::of(f),
                &f.data.get(),
                f.alpha_blend,
            );
        } else if f.base.is_none() && f.covers(self.width, self.height) {
            // A self-contained full frame (the root, or one coalesced on
            // create): decompress straight into the buffer.
            f.data.write_into(&mut display.pixels);
        } else {
            display.pixels = self.coalesce(self.current);
        }
        self.displayed = Some(self.frames[self.current].id);
        true
    }
}

fn gap(f: &Frame) -> Duration {
    Duration::from_millis(u64::from(f.gap_ms))
}

/// Where a block of pixels goes in an image.
#[derive(Debug, Clone, Copy)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Rect {
    fn of(f: &Frame) -> Self {
        Self {
            x: f.x,
            y: f.y,
            width: f.width,
            height: f.height,
        }
    }
}

/// A canvas of `len` bytes filled with RGBA colour `rgba` (R in the high
/// byte, as `Y=` sends it).
fn filled(len: usize, rgba: u32) -> Vec<u8> {
    let px = rgba.to_be_bytes();
    if rgba == 0 {
        return vec![0; len];
    }
    px.iter().copied().cycle().take(len).collect()
}

/// Compose `over` (`rect.width * rect.height` RGBA pixels) onto `under`
/// (a `width * height` canvas) at `rect`, clipped to the canvas, either
/// alpha-blended or overwriting.
fn compose(under: &mut [u8], width: u32, height: u32, rect: &Rect, over: &[u8], blend: bool) {
    if rect.x >= width || rect.y >= height {
        return;
    }
    let cols = rect.width.min(width - rect.x) as usize;
    let rows = rect.height.min(height - rect.y) as usize;
    let over_row = rect.width as usize * 4;
    for y in 0..rows {
        let src = &over[y * over_row..][..cols * 4];
        let start = ((rect.y as usize + y) * width as usize + rect.x as usize) * 4;
        let dst = &mut under[start..start + cols * 4];
        if blend {
            for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
                alpha_blend(d, s);
            }
        } else {
            dst.copy_from_slice(src);
        }
    }
}

/// Straight-alpha "over", with kitty's float arithmetic and truncation.
fn alpha_blend(dest: &mut [u8], src: &[u8]) {
    if src[3] == 0 {
        return;
    }
    if src[3] == 255 {
        dest.copy_from_slice(src);
        return;
    }
    let da = f32::from(dest[3]) / 255.0;
    let sa = f32::from(src[3]) / 255.0;
    let alpha = sa + da * (1.0 - sa);
    dest[3] = (255.0 * alpha) as u8;
    if dest[3] == 0 {
        dest[..3].fill(0);
        return;
    }
    for i in 0..3 {
        dest[i] = ((f32::from(src[i]) * sa + f32::from(dest[i]) * da * (1.0 - sa)) / alpha) as u8;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: [u8; 4] = [255, 0, 0, 255];
    const G: [u8; 4] = [0, 255, 0, 255];
    const B: [u8; 4] = [0, 0, 255, 255];
    const W: [u8; 4] = [255, 255, 255, 255];
    const T: [u8; 4] = [0, 0, 0, 0];

    fn img(w: u32, h: u32, px: &[[u8; 4]]) -> ImageData {
        let pixels: Vec<u8> = if px.len() == 1 {
            px[0].repeat((w * h) as usize)
        } else {
            px.concat()
        };
        assert_eq!(pixels.len(), (w * h * 4) as usize);
        ImageData {
            width: w,
            height: h,
            pixels,
        }
    }

    fn keys() -> FrameKeys {
        FrameKeys {
            x: 0,
            y: 0,
            base_frame: 0,
            edit_frame: 0,
            gap_ms: 0,
            blend_mode: 0,
            background: 0,
        }
    }

    fn ctl() -> AnimationKeys {
        AnimationKeys {
            state: 0,
            frame: 0,
            gap_ms: 0,
            current_frame: 0,
            loops: 0,
        }
    }

    /// 4x2 animation, white root, plus `frames` full solid frames.
    fn anim(frames: &[[u8; 4]]) -> (Animation, ImageData) {
        let root = img(4, 2, &[W]);
        let mut a = Animation::new(&root, &Playback::default());
        let mut display = root;
        for c in frames {
            add(&mut a, &mut display, keys(), img(4, 2, &[*c])).unwrap();
        }
        (a, display)
    }

    fn add(
        anim: &mut Animation,
        display: &mut ImageData,
        keys: FrameKeys,
        data: ImageData,
    ) -> Result<u32, FrameError> {
        let prepared = anim.prepare_frame(&keys, data)?;
        let number = prepared.number;
        anim.commit(prepared, display);
        Ok(number)
    }

    fn px(buf: &[u8], x: usize, y: usize) -> [u8; 4] {
        let i = (y * 4 + x) * 4;
        buf[i..i + 4].try_into().unwrap()
    }

    fn show(a: &mut Animation, d: &mut ImageData, n: u32) {
        a.control(
            &AnimationKeys {
                current_frame: n,
                ..ctl()
            },
            d,
        );
    }

    #[test]
    fn frame_target_matches_kitty() {
        assert_eq!(frame_target(1, 0), 2);
        assert_eq!(frame_target(3, 2), 2);
        assert_eq!(frame_target(3, 3), 3);
        // Past the end: a new frame (kitty: `r=5` on one frame -> r=2).
        assert_eq!(frame_target(1, 5), 2);
    }

    #[test]
    fn new_frames_get_numbers_and_default_gaps() {
        let (mut a, mut d) = anim(&[G]);
        assert_eq!(a.frame_count(), 2);
        assert_eq!(a.gap_ms(1), Some(0), "root starts gapless");
        assert_eq!(a.gap_ms(2), Some(DEFAULT_GAP_MS));
        let n = add(
            &mut a,
            &mut d,
            FrameKeys {
                gap_ms: -5,
                ..keys()
            },
            img(4, 2, &[B]),
        )
        .unwrap();
        assert_eq!((n, a.gap_ms(3)), (3, Some(0)), "negative z is gapless");
        let n = add(
            &mut a,
            &mut d,
            FrameKeys {
                gap_ms: 70,
                ..keys()
            },
            img(4, 2, &[B]),
        )
        .unwrap();
        assert_eq!((n, a.gap_ms(4)), (4, Some(70)));
        // Adding frames never changes what's on display.
        assert_eq!(px(&d.pixels, 0, 0), W);
    }

    #[test]
    fn frame_size_and_base_errors_are_kittys() {
        let (a, _) = anim(&[]);
        let e = a.prepare_frame(&keys(), img(5, 2, &[G])).unwrap_err();
        assert_eq!(e.detail, "Frame width 5 larger than image width: 4");
        let e = a.prepare_frame(&keys(), img(4, 3, &[G])).unwrap_err();
        assert_eq!(e.detail, "Frame height 3 larger than image height: 2");
        let e = a
            .prepare_frame(
                &FrameKeys {
                    base_frame: 3,
                    ..keys()
                },
                img(4, 2, &[G]),
            )
            .unwrap_err();
        assert_eq!(
            (e.code, e.detail.as_str()),
            (ErrorCode::Einval, "No frame with number: 3 found")
        );
        // Wholly off the image is fine (clipped away).
        assert!(
            a.prepare_frame(
                &FrameKeys {
                    x: 9,
                    y: 9,
                    ..keys()
                },
                img(1, 1, &[G])
            )
            .is_ok()
        );
    }

    #[test]
    fn partial_frame_uses_background_or_transparent() {
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                x: 1,
                y: 1,
                ..keys()
            },
            img(2, 1, &[G, G]),
        )
        .unwrap();
        let k = FrameKeys {
            x: 1,
            y: 1,
            background: 0xff00_00ff,
            ..keys()
        };
        add(&mut a, &mut d, k, img(2, 1, &[G, G])).unwrap();
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 0, 0), T);
        assert_eq!(px(&d.pixels, 1, 1), G);
        show(&mut a, &mut d, 3);
        assert_eq!(px(&d.pixels, 0, 0), R, "Y= is RGBA, R in the high byte");
        assert_eq!(px(&d.pixels, 2, 1), G);
    }

    #[test]
    fn full_frame_ignores_background() {
        // kitty shows a full-size frame's own pixels; Y= doesn't show
        // through its translucent ones.
        let (mut a, mut d) = anim(&[]);
        let half_green = [0, 255, 0, 128];
        add(
            &mut a,
            &mut d,
            FrameKeys {
                background: 0xff00_00ff,
                ..keys()
            },
            img(4, 2, &[half_green]),
        )
        .unwrap();
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 0, 0), half_green);
    }

    #[test]
    fn base_frame_blend_and_overwrite() {
        let (mut a, mut d) = anim(&[]);
        let half_red = [255, 0, 0, 128];
        let k = FrameKeys {
            x: 1,
            y: 1,
            base_frame: 1,
            ..keys()
        };
        add(&mut a, &mut d, k, img(2, 1, &[half_red, T])).unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys { blend_mode: 1, ..k },
            img(2, 1, &[half_red, T]),
        )
        .unwrap();
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 0, 0), W);
        assert_eq!(
            px(&d.pixels, 1, 1),
            [255, 126, 126, 255],
            "blended over white"
        );
        assert_eq!(px(&d.pixels, 2, 1), W, "transparent leaves the base");
        show(&mut a, &mut d, 3);
        assert_eq!(px(&d.pixels, 1, 1), half_red, "X=1 overwrites");
        assert_eq!(px(&d.pixels, 2, 1), T);
    }

    #[test]
    fn edit_composes_onto_the_frame_and_sets_gap() {
        let (mut a, mut d) = anim(&[G]);
        let k = FrameKeys {
            edit_frame: 2,
            gap_ms: 70,
            ..keys()
        };
        assert_eq!(add(&mut a, &mut d, k, img(1, 1, &[B])).unwrap(), 2);
        // Y= doesn't apply to an edit.
        let k = FrameKeys {
            edit_frame: 2,
            x: 3,
            y: 1,
            background: 0xff00_00ff,
            ..keys()
        };
        add(&mut a, &mut d, k, img(1, 1, &[T])).unwrap();
        assert_eq!(a.frame_count(), 2);
        assert_eq!(a.gap_ms(2), Some(70));
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 0, 0), B);
        assert_eq!(px(&d.pixels, 1, 0), G);
        assert_eq!(px(&d.pixels, 3, 1), G);
    }

    #[test]
    fn editing_the_root_shows_through_lazy_references() {
        // kitty: a frame built on the root sees later root edits.
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 1,
                ..keys()
            },
            img(1, 1, &[G]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                edit_frame: 1,
                x: 3,
                y: 1,
                ..keys()
            },
            img(1, 1, &[B]),
        )
        .unwrap();
        assert_eq!(px(&d.pixels, 3, 1), B, "root on display is edited");
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 0, 0), G);
        assert_eq!(px(&d.pixels, 3, 1), B);
    }

    #[test]
    fn long_chains_are_coalesced_on_create() {
        // Frame 2 covers the image and is built on the root; frame 3 on
        // frame 2 then has two image areas below it, so kitty coalesces
        // it and a later root edit doesn't reach it (but reaches 2).
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 1,
                ..keys()
            },
            img(4, 2, &[T]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 2,
                x: 1,
                ..keys()
            },
            img(1, 1, &[B]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                edit_frame: 1,
                x: 3,
                y: 1,
                ..keys()
            },
            img(1, 1, &[R]),
        )
        .unwrap();
        show(&mut a, &mut d, 3);
        assert_eq!(px(&d.pixels, 1, 0), B);
        assert_eq!(px(&d.pixels, 3, 1), W);
        show(&mut a, &mut d, 2);
        assert_eq!(px(&d.pixels, 3, 1), R);
        // A short chain stays lazy.
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 1,
                ..keys()
            },
            img(1, 1, &[G]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 2,
                x: 1,
                ..keys()
            },
            img(1, 1, &[B]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                edit_frame: 1,
                x: 3,
                y: 1,
                ..keys()
            },
            img(1, 1, &[R]),
        )
        .unwrap();
        show(&mut a, &mut d, 3);
        assert_eq!(
            [
                px(&d.pixels, 0, 0),
                px(&d.pixels, 1, 0),
                px(&d.pixels, 3, 1)
            ],
            [G, B, R]
        );
    }

    fn comp(src: u32, dest: u32) -> CompositionKeys {
        CompositionKeys {
            dest_frame: dest,
            src_frame: src,
            dest_x: 0,
            dest_y: 0,
            width: 0,
            height: 0,
            src_x: 0,
            src_y: 0,
            blend_mode: 0,
        }
    }

    #[test]
    fn compose_errors_are_kittys_in_order() {
        let (a, _) = anim(&[G]);
        let err = |k: CompositionKeys| a.prepare_compose(&k, 1).unwrap_err().detail;
        assert_eq!(
            err(comp(0, 0)),
            "No source frame number 0 exists in image id: 1\n"
        );
        assert_eq!(
            err(comp(1, 3)),
            "No destination frame number 3 exists in image id: 1\n"
        );
        assert_eq!(
            err(comp(3, 3)),
            "No source frame number 3 exists in image id: 1\n"
        );
        let dest_oob = "The destination rectangle is out of bounds";
        let src_oob = "The source rectangle is out of bounds";
        assert_eq!(
            err(CompositionKeys {
                width: 5,
                ..comp(1, 2)
            }),
            dest_oob
        );
        assert_eq!(
            err(CompositionKeys {
                dest_x: 1,
                ..comp(1, 2)
            }),
            dest_oob
        );
        assert_eq!(
            err(CompositionKeys {
                src_x: 1,
                ..comp(1, 2)
            }),
            src_oob
        );
        assert_eq!(
            err(CompositionKeys {
                height: 3,
                src_y: 9,
                ..comp(1, 2)
            }),
            dest_oob
        );
        let overlap = CompositionKeys {
            width: 1,
            height: 1,
            ..comp(2, 2)
        };
        assert!(err(overlap).contains("overlap"));
        assert!(
            a.prepare_compose(
                &CompositionKeys {
                    dest_x: 1,
                    ..overlap
                },
                1
            )
            .is_ok()
        );
        let ok = CompositionKeys {
            width: 2,
            height: 2,
            dest_x: 2,
            src_x: 2,
            ..comp(1, 2)
        };
        assert!(a.prepare_compose(&ok, 1).is_ok());
    }

    #[test]
    fn compose_blends_or_overwrites_a_rectangle() {
        let half_red = [255, 0, 0, 128];
        let (mut a, mut d) = anim(&[half_red]);
        let k = CompositionKeys {
            width: 2,
            height: 1,
            dest_x: 2,
            dest_y: 1,
            ..comp(2, 1)
        };
        let p = a.prepare_compose(&k, 1).unwrap();
        assert!(a.commit(p, &mut d), "root on display changed");
        assert_eq!(px(&d.pixels, 2, 1), [255, 126, 126, 255]);
        assert_eq!(px(&d.pixels, 1, 1), W);
        let p = a
            .prepare_compose(&CompositionKeys { blend_mode: 1, ..k }, 1)
            .unwrap();
        a.commit(p, &mut d);
        assert_eq!(px(&d.pixels, 3, 1), half_red);
        // Source offset.
        let (mut a, mut d) = anim(&[]);
        add(&mut a, &mut d, keys(), img(4, 2, &[R, G, B, T, W, W, W, W])).unwrap();
        let k = CompositionKeys {
            width: 2,
            height: 1,
            src_x: 1,
            dest_y: 1,
            blend_mode: 1,
            ..comp(2, 1)
        };
        let p = a.prepare_compose(&k, 1).unwrap();
        a.commit(p, &mut d);
        assert_eq!([px(&d.pixels, 0, 1), px(&d.pixels, 1, 1)], [G, B]);
    }

    #[test]
    fn control_sets_current_frame_gap_state_and_loops() {
        let (mut a, mut d) = anim(&[G, B]);
        assert!(!a.control(
            &AnimationKeys {
                current_frame: 9,
                ..ctl()
            },
            &mut d
        ));
        assert!(a.control(
            &AnimationKeys {
                current_frame: 3,
                ..ctl()
            },
            &mut d
        ));
        assert_eq!(px(&d.pixels, 0, 0), B);
        assert!(!a.control(
            &AnimationKeys {
                current_frame: 3,
                ..ctl()
            },
            &mut d
        ));
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 150,
                ..ctl()
            },
            &mut d,
        );
        assert_eq!(a.gap_ms(1), Some(150));
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: -3,
                ..ctl()
            },
            &mut d,
        );
        assert_eq!(a.gap_ms(1), Some(0));
        a.control(
            &AnimationKeys {
                frame: 2,
                gap_ms: 0,
                ..ctl()
            },
            &mut d,
        );
        assert_eq!(a.gap_ms(2), Some(DEFAULT_GAP_MS), "z=0 is ignored");
        a.control(&AnimationKeys { state: 3, ..ctl() }, &mut d);
        assert_eq!(a.state(), AnimationState::Running);
        a.control(&AnimationKeys { state: 7, ..ctl() }, &mut d);
        assert_eq!(a.state(), AnimationState::Running, "unknown s= ignored");
        a.control(&AnimationKeys { state: 1, ..ctl() }, &mut d);
        assert_eq!(a.state(), AnimationState::Stopped);
    }

    /// Frames shown by ticking every `step` ms for `total` ms.
    fn play(a: &mut Animation, d: &mut ImageData, step: u64, total: u64) -> Vec<u32> {
        let t0 = Instant::now();
        let mut seen = vec![a.current_frame()];
        let mut t = 0;
        while t <= total {
            a.tick(t0 + Duration::from_millis(t), Duration::ZERO, d);
            if *seen.last().unwrap() != a.current_frame() {
                seen.push(a.current_frame());
            }
            t += step;
        }
        seen
    }

    #[test]
    fn stopped_animation_does_not_tick() {
        let (mut a, mut d) = anim(&[G, B]);
        assert!(!a.is_running());
        assert_eq!(
            a.tick(Instant::now(), Duration::ZERO, &mut d),
            (false, None)
        );
    }

    #[test]
    fn running_loops_forever_and_skips_gapless_frames() {
        let (mut a, mut d) = anim(&[G, B]);
        // Root keeps its zero gap: skipped on every loop.
        a.control(&AnimationKeys { state: 3, ..ctl() }, &mut d);
        assert_eq!(
            play(&mut a, &mut d, 10, 400),
            vec![1, 2, 3, 2, 3, 2, 3, 2, 3, 2, 3, 2]
        );
    }

    #[test]
    fn loop_count_v2_plays_once_and_stops_on_the_last_frame() {
        let (mut a, mut d) = anim(&[G, B]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 40,
                ..ctl()
            },
            &mut d,
        );
        a.control(
            &AnimationKeys {
                state: 3,
                loops: 2,
                ..ctl()
            },
            &mut d,
        );
        assert_eq!(play(&mut a, &mut d, 10, 500), vec![1, 2, 3]);
        assert!(!a.is_running());
        // v=3: two passes.
        let (mut a, mut d) = anim(&[G, B]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 40,
                ..ctl()
            },
            &mut d,
        );
        a.control(
            &AnimationKeys {
                state: 3,
                loops: 3,
                ..ctl()
            },
            &mut d,
        );
        assert_eq!(play(&mut a, &mut d, 10, 500), vec![1, 2, 3, 1, 2, 3]);
        // Stopping resets the loop counter, so running again replays.
        a.control(&AnimationKeys { state: 1, ..ctl() }, &mut d);
        a.control(&AnimationKeys { state: 3, ..ctl() }, &mut d);
        assert!(a.is_running());
    }

    #[test]
    fn loading_waits_at_the_last_frame_for_more() {
        let (mut a, mut d) = anim(&[G]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 40,
                state: 2,
                ..ctl()
            },
            &mut d,
        );
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        a.tick(at(0), Duration::ZERO, &mut d);
        assert!(a.tick(at(40), Duration::ZERO, &mut d).0);
        assert_eq!(a.current_frame(), 2);
        // At the end: nothing due, stays put.
        assert_eq!(a.tick(at(100), Duration::ZERO, &mut d), (false, None));
        assert_eq!(a.current_frame(), 2);
        // A new frame arrives: playback goes on.
        add(&mut a, &mut d, keys(), img(4, 2, &[B])).unwrap();
        assert!(a.tick(at(101), Duration::ZERO, &mut d).0);
        assert_eq!(a.current_frame(), 3);
        assert_eq!(px(&d.pixels, 0, 0), B);
    }

    #[test]
    fn tick_reports_the_next_deadline() {
        let (mut a, mut d) = anim(&[G]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 100,
                state: 3,
                ..ctl()
            },
            &mut d,
        );
        let t0 = Instant::now();
        assert_eq!(
            a.tick(t0, Duration::ZERO, &mut d),
            (false, Some(t0 + Duration::from_millis(100)))
        );
        let t1 = t0 + Duration::from_millis(105);
        assert_eq!(
            a.tick(t1, Duration::ZERO, &mut d),
            (true, Some(t1 + Duration::from_millis(40)))
        );
    }

    #[test]
    fn incremental_display_matches_full_coalesce() {
        // GIF-style deltas: each frame built on the previous one.
        let (mut a, mut d) = anim(&[]);
        for (i, c) in [R, G, B].iter().enumerate() {
            let k = FrameKeys {
                base_frame: i as u32 + 1,
                x: i as u32,
                ..keys()
            };
            add(&mut a, &mut d, k, img(1, 1, &[*c])).unwrap();
        }
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 10,
                state: 3,
                ..ctl()
            },
            &mut d,
        );
        let t0 = Instant::now();
        for step in 1..=8u64 {
            a.tick(
                t0 + Duration::from_millis(step * 50),
                Duration::ZERO,
                &mut d,
            );
            assert_eq!(d.pixels, a.coalesce(a.current), "step {step}");
        }
    }

    #[test]
    fn delete_frame_follows_kitty() {
        // R(1) G(2) B(3) as root/frames; shown frame after each delete,
        // as measured on kitty 0.49.2.
        let cases: [(u32, u32, [u8; 4]); 5] = [
            // (current, deleted, shown after)
            (3, 0, B), // d=f without r= deletes the root
            (1, 1, G),
            (3, 2, B),
            (3, 3, G),
            (3, 9, G), // past the end: the last frame
        ];
        for (cur, del, want) in cases {
            let root = img(4, 2, &[R]);
            let mut a = Animation::new(&root, &Playback::default());
            let mut d = root;
            add(&mut a, &mut d, keys(), img(4, 2, &[G])).unwrap();
            add(&mut a, &mut d, keys(), img(4, 2, &[B])).unwrap();
            show(&mut a, &mut d, cur);
            a.delete_frame(del, &mut d);
            assert_eq!(a.frame_count(), 2, "cur {cur} del {del}");
            assert_eq!(px(&d.pixels, 0, 0), want, "cur {cur} del {del}");
        }
    }

    #[test]
    fn deleting_a_base_frame_keeps_its_dependents_pixels() {
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 1,
                ..keys()
            },
            img(1, 1, &[G]),
        )
        .unwrap();
        a.delete_frame(1, &mut d);
        assert_eq!(a.frame_count(), 1);
        assert_eq!(px(&d.pixels, 0, 0), G);
        assert_eq!(px(&d.pixels, 1, 0), W);
    }

    // ---- #47 review ----------------------------------------------------

    /// [`Animation::byte_size`] the slow way.
    fn recount(a: &Animation) -> u64 {
        a.frames
            .iter()
            .chain(a.hidden.values())
            .map(Frame::cost)
            .sum()
    }

    /// A `w`x`h` noise image: incompressible.
    fn noise(w: u32, h: u32) -> ImageData {
        let mut x: u32 = 0x9e37_79b9;
        let pixels = (0..w * h * 4)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        ImageData {
            width: w,
            height: h,
            pixels,
        }
    }

    #[test]
    fn deleting_a_base_frame_allocates_nothing() {
        // Review blocker 1: a 200x200 noise root with 300 one-pixel frames
        // built on it. Deleting the root used to copy it into every one of
        // them (300 × 160 KB). Now it stays behind as their hidden base.
        let root = noise(200, 200);
        let mut a = Animation::new(&root, &Playback::default());
        let mut d = root.clone();
        for k in 0..300u32 {
            let keys = FrameKeys {
                base_frame: 1,
                x: k % 200,
                ..keys()
            };
            add(&mut a, &mut d, keys, img(1, 1, &[G])).unwrap();
        }
        let before = a.byte_size();
        a.delete_frame(1, &mut d);
        assert_eq!(a.frame_count(), 300);
        assert_eq!(a.hidden_frames(), 1);
        assert!(a.byte_size() <= before, "nothing allocated");
        assert_eq!(a.byte_size(), recount(&a));
        // The frames keep their pixels: the old root under one green dot.
        show(&mut a, &mut d, 5);
        let mut want = root.pixels.clone();
        want[4 * 4..5 * 4].copy_from_slice(&G);
        assert_eq!(d.pixels, want);
        // The hidden base goes with the last frame built on it.
        while a.frame_count() > 1 {
            a.delete_frame(1, &mut d);
        }
        assert_eq!(a.hidden_frames(), 1, "one dependent left");
        a.prepare_frame(
            &FrameKeys {
                edit_frame: 1,
                ..keys()
            },
            img(1, 1, &[B]),
        )
        .map(|p| a.commit(p, &mut d))
        .unwrap();
        assert_eq!(a.hidden_frames(), 0, "the edit cut the last reference");
        assert_eq!(a.byte_size(), a.frames[0].cost());
    }

    #[test]
    fn hidden_bases_chain_and_free_in_cascade() {
        // 1 <- 2 <- 3: delete 1 then 2; 3 still renders through both.
        let (mut a, mut d) = anim(&[]);
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 1,
                ..keys()
            },
            img(1, 1, &[G]),
        )
        .unwrap();
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 2,
                x: 1,
                ..keys()
            },
            img(1, 1, &[B]),
        )
        .unwrap();
        a.delete_frame(1, &mut d);
        a.delete_frame(1, &mut d);
        assert_eq!((a.frame_count(), a.hidden_frames()), (1, 2));
        assert_eq!(a.byte_size(), recount(&a));
        assert_eq!(
            [
                px(&d.pixels, 0, 0),
                px(&d.pixels, 1, 0),
                px(&d.pixels, 2, 0)
            ],
            [G, B, W]
        );
        // Editing 3 into a self-contained frame frees both, in cascade.
        a.prepare_frame(
            &FrameKeys {
                edit_frame: 1,
                ..keys()
            },
            img(1, 1, &[R]),
        )
        .map(|p| a.commit(p, &mut d))
        .unwrap();
        assert_eq!(a.hidden_frames(), 0);
        assert_eq!(a.byte_size(), a.frames[0].cost());
    }

    #[test]
    fn adding_many_frames_is_linear() {
        // Review blocker 2: every add re-summed every frame (O(N²): 80k
        // frames took 5.6 s in a release build). 200k tiny frames, built on
        // the previous one like a GIF, must take a fraction of that even
        // unoptimised.
        let start = Instant::now();
        let root = img(4, 2, &[W]);
        let mut a = Animation::new(&root, &Playback::default());
        let mut d = root;
        for k in 0..200_000u32 {
            let keys = FrameKeys {
                base_frame: k.max(1),
                gap_ms: 10,
                ..keys()
            };
            add(&mut a, &mut d, keys, img(1, 1, &[G])).unwrap();
            assert!(a.is_running() || a.state() == AnimationState::Stopped);
        }
        assert_eq!(a.frame_count(), 200_001);
        assert_eq!(a.byte_size(), recount(&a), "running total stays exact");
        let elapsed = start.elapsed();
        assert!(elapsed < Duration::from_secs(20), "took {elapsed:?}");
    }

    #[test]
    fn restarting_waits_the_full_gap() {
        // Review 4: s=1 then s=3 advanced at once; kitty waits the gap.
        let (mut a, mut d) = anim(&[G]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 600,
                state: 3,
                ..ctl()
            },
            &mut d,
        );
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        a.tick(at(0), Duration::ZERO, &mut d);
        a.control(&AnimationKeys { state: 1, ..ctl() }, &mut d);
        a.tick(at(1500), Duration::ZERO, &mut d);
        a.control(&AnimationKeys { state: 3, ..ctl() }, &mut d);
        assert!(!a.tick(at(1500), Duration::ZERO, &mut d).0, "not at once");
        assert!(!a.tick(at(2000), Duration::ZERO, &mut d).0);
        assert!(
            a.tick(at(2100), Duration::ZERO, &mut d).0,
            "600 ms after the restart"
        );
        assert_eq!(a.current_frame(), 2);
    }

    #[test]
    fn base_naming_the_new_frame_is_accepted() {
        // Review 5: kitty answers `a=f,c=<the frame being made>` OK.
        let (mut a, mut d) = anim(&[]);
        let n = add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 2,
                x: 1,
                ..keys()
            },
            img(1, 1, &[G]),
        )
        .unwrap();
        assert_eq!(n, 2);
        show(&mut a, &mut d, 2);
        assert_eq!(
            [px(&d.pixels, 0, 0), px(&d.pixels, 1, 0)],
            [T, G],
            "drawn on the background"
        );
        // One past it is still an error.
        assert!(
            a.prepare_frame(
                &FrameKeys {
                    base_frame: 4,
                    ..keys()
                },
                img(1, 1, &[G])
            )
            .is_err()
        );
    }

    #[test]
    fn edits_off_screen_leave_the_display_alone() {
        // Review 6: an edit recomposes the display only if the frame on
        // it is (built on) the edited one.
        let (mut a, mut d) = anim(&[G, B]);
        show(&mut a, &mut d, 3);
        let p = a
            .prepare_frame(
                &FrameKeys {
                    edit_frame: 2,
                    ..keys()
                },
                img(1, 1, &[R]),
            )
            .unwrap();
        assert!(!a.commit(p, &mut d), "frame 3 doesn't use frame 2");
        assert_eq!(px(&d.pixels, 0, 0), B);
        // Built on it: recomposed.
        add(
            &mut a,
            &mut d,
            FrameKeys {
                base_frame: 2,
                x: 3,
                ..keys()
            },
            img(1, 1, &[W]),
        )
        .unwrap();
        show(&mut a, &mut d, 4);
        let p = a
            .prepare_frame(
                &FrameKeys {
                    edit_frame: 2,
                    x: 1,
                    ..keys()
                },
                img(1, 1, &[B]),
            )
            .unwrap();
        assert!(a.commit(p, &mut d));
        assert_eq!([px(&d.pixels, 0, 0), px(&d.pixels, 1, 0)], [R, B]);
    }

    #[test]
    fn slack_shows_frames_early_without_drift() {
        let (mut a, mut d) = anim(&[G, B]);
        a.control(
            &AnimationKeys {
                frame: 1,
                gap_ms: 100,
                state: 3,
                ..ctl()
            },
            &mut d,
        );
        for i in 2..=3 {
            a.control(
                &AnimationKeys {
                    frame: i,
                    gap_ms: 100,
                    ..ctl()
                },
                &mut d,
            );
        }
        let t0 = Instant::now();
        let slack = Duration::from_millis(10);
        a.tick(t0, slack, &mut d);
        // Ticking 8 ms early every time: each frame shows early, but the
        // schedule stays on the 100 ms grid.
        for k in 1..=30u64 {
            let (changed, next) = a.tick(t0 + Duration::from_millis(k * 100 - 8), slack, &mut d);
            assert!(changed, "tick {k}");
            assert_eq!(next, Some(t0 + Duration::from_millis((k + 1) * 100)));
        }
    }

    #[test]
    fn pixels_compress_when_it_pays_and_round_trip() {
        // Flat: compressed, much smaller, intact.
        let flat = [7u8, 8, 9, 255].repeat(64 * 64);
        let p = Pixels::new(flat.clone());
        assert!(matches!(p, Pixels::Lz4 { .. }));
        assert!(p.stored_len() < flat.len() / 20);
        assert_eq!(&*p.get(), &flat[..]);
        let mut out = vec![1, 2, 3];
        p.write_into(&mut out);
        assert_eq!(out, flat);
        // Noise doesn't compress: kept raw.
        let mut x: u32 = 1;
        let noise: Vec<u8> = (0..64 * 64 * 4)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                x as u8
            })
            .collect();
        let p = Pixels::new(noise.clone());
        assert!(matches!(p, Pixels::Raw(_)));
        assert_eq!(p.stored_len(), noise.len());
        // Small blocks aren't worth it.
        assert!(matches!(Pixels::new(vec![0; 100]), Pixels::Raw(_)));
    }

    #[test]
    fn compressed_frames_play_like_raw_ones() {
        // Full-size 64x64 frames (compressed) built on each other, and a
        // partial one: playback composes them exactly.
        let root = img(64, 64, &[W]);
        let mut a = Animation::new(&root, &Playback::default());
        let mut d = root;
        for (i, c) in [R, G, B].iter().enumerate() {
            let k = FrameKeys {
                base_frame: i as u32 + 1,
                x: i as u32 * 8,
                ..keys()
            };
            add(&mut a, &mut d, k, img(16, 16, &[*c])).unwrap();
        }
        add(&mut a, &mut d, keys(), img(64, 64, &[G])).unwrap();
        assert!(
            a.byte_size() < 64 * 64 * 4,
            "all compressed: {}",
            a.byte_size()
        );
        for n in [2, 3, 4, 5, 1, 4] {
            show(&mut a, &mut d, n);
            assert_eq!(d.pixels, a.coalesce(n as usize - 1), "frame {n}");
        }
        show(&mut a, &mut d, 4);
        let at = |x: usize, y: usize| -> [u8; 4] {
            d.pixels[(y * 64 + x) * 4..][..4].try_into().unwrap()
        };
        assert_eq!([at(0, 0), at(8, 0), at(16, 0), at(40, 40)], [R, G, B, W]);
    }

    #[test]
    fn alpha_blend_matches_kitty() {
        let mut d = [255, 255, 255, 255];
        alpha_blend(&mut d, &[255, 0, 0, 128]);
        assert_eq!(d, [255, 126, 126, 255]);
        let mut d = [0, 0, 0, 0];
        alpha_blend(&mut d, &[0, 255, 0, 128]);
        assert_eq!(d, [0, 254, 0, 128]);
        let mut d = [9, 9, 9, 9];
        alpha_blend(&mut d, &T);
        assert_eq!(d, [9, 9, 9, 9]);
    }
}
