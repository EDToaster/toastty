//! Stateful Kitty graphics dispatcher.
//!
//! Receives `(Header, body)` pairs from the parser; reassembles multi-chunk
//! uploads (`m=1` → ... → `m=0`); on a complete payload, decodes and forwards
//! the result to a [`KittySink`] (the host's interpretation of "register an
//! image", "place an image", "delete an image", "queue a reply").
//!
//! Split from `Term`'s `Perform::apc_*` so the parsing logic stays
//! testable without `Term` and so the implementation can grow without
//! bloating `term.rs`.
//!
//! Chunked-upload reassembly:
//! - `m=1` keeps `id`'s pending buffer alive across chunks.
//! - `m=0` (or no `m=`) is the final chunk; concatenate, decode, dispatch.
//! - A header mismatch between chunks → `Einval` and we abandon the upload.
//! - Cap on pending buffer size — overflow → `Efbig`.
//! - Global caps on in-flight uploads (count and total bytes) evict the
//!   least recently active one; an upload idle for
//!   [`PENDING_UPLOAD_TTL`] is freed (SEC-6).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use base64::Engine;

use super::decode::{DecodeError, decode, raw_len};
use super::header::{Action, Compression, DeleteSpec, Header, Quiet, Transmission};
use super::medium::{self, FileError, FileRequest};
use super::placeholder::VirtualPlacement;
use super::reply::{ErrorCode, encode_error, encode_ok};
use crate::image_grid::{Placement, RelativeError, SrcRect};
use crate::registry::ImageData;

/// Maximum pending-upload buffer size in bytes. Larger uploads are
/// rejected mid-stream with [`ErrorCode::Efbig`].
pub const DEFAULT_PENDING_CAP: usize = 64 * 1024 * 1024;

/// Most chunked uploads in flight at once (SEC-6). The spec says a client
/// must finish one upload before it starts another, so this only has to
/// absorb abandoned ones. Starting one more evicts the least recently
/// active upload.
pub const MAX_PENDING_UPLOADS: usize = 8;

/// Cap on the buffers of all in-flight uploads together (SEC-6). Room for
/// a new chunk is made by evicting the least recently active other
/// uploads. At least [`DEFAULT_PENDING_CAP`], so one upload always fits.
pub const DEFAULT_PENDING_TOTAL_CAP: usize = 2 * DEFAULT_PENDING_CAP;

/// An upload that receives no chunk for this long is abandoned, and its
/// buffer freed on the next graphics command (SEC-6).
pub const PENDING_UPLOAD_TTL: Duration = Duration::from_mins(1);

/// Extra compressed bytes past `s*v*bpp` that kitty buffers for an
/// `o=z` raw upload before replying `EFBIG:Too much data`. Measured on
/// kitty 0.49.2 (a 1x1 RGBA upload takes ~1027 compressed bytes).
const COMPRESSED_DATA_SLACK: u64 = 1024;

/// Decoded-image quota for sinks that don't report one
/// ([`KittySink::max_image_bytes`]). kitty's per-image `MAX_DATA_SZ`.
pub const DEFAULT_MAX_IMAGE_BYTES: u64 = 400_000_000;

/// Calls back into the host (a `Term`) for each Kitty action.
///
/// Returning `None` from `register_image` indicates the host could not
/// accept the image (e.g. `Efbig` — too big to fit even after eviction).
/// The handler turns this into an error reply.
pub trait KittySink {
    /// Insert (or replace) decoded `data` into the host's registry.
    ///
    /// `id_request` is the client-supplied id (`i=` key); `0` means the
    /// host should assign one. `image_number` is the client-supplied
    /// image *number* (`I=` key; `0` when absent) — the host records a
    /// number→most-recent-id mapping so `d=n`/`d=N` can resolve it.
    /// Returns the *final* id, or `None` if the host could not accept the
    /// image. The handler maps `None` to [`ErrorCode::Efbig`].
    fn register_image(
        &mut self,
        id_request: u32,
        image_number: u32,
        data: ImageData,
    ) -> Option<u32>;

    /// Insert a placement onto the cell grid.
    fn place_image(&mut self, placement: Placement);

    /// Insert a *relative* placement (M13). `placement.parent` is
    /// `Some((parent_image, parent_placement))` and `placement.rel_offset`
    /// the `(H, V)` cell offset. The host validates the parent reference
    /// (existence, cycles, depth) and, on success, rebases the
    /// placement's cell ranges onto the parent's origin plus the offset.
    /// Returns the resolution error otherwise; the handler maps it onto
    /// the appropriate protocol error code.
    ///
    /// The default implementation rejects with [`RelativeError::NoParent`]
    /// so sinks that don't track a placement grid (simple test sinks) get
    /// a sane answer without overriding.
    fn place_relative(&mut self, _placement: Placement) -> Result<(), RelativeError> {
        Err(RelativeError::NoParent)
    }

    /// Create (or, for a non-zero placement id, replace) a *virtual*
    /// placement: `U=1` on `a=T` / `a=p`. Nothing is drawn and the cursor
    /// doesn't move; Unicode placeholder cells refer to it. Default is a
    /// no-op so simple test sinks don't need to track them.
    fn place_virtual(&mut self, _placement: VirtualPlacement) {}

    /// Delete by spec. Receives both the header (for `i=` / `I=` /
    /// `delete` fields) and the resolved id (or `0` for "no specific
    /// id"). Implementations interpret the `DeleteSpec` byte per the
    /// protocol (lowercase = only placements; uppercase = also free
    /// image bytes; `a` = all; `i`/`I` = by id; etc.).
    fn delete_image(&mut self, delete: DeleteSpec, header: &Header);

    /// Queue `bytes` for write back to the PTY. The host's drain
    /// (`Term::drain_pty_replies`) returns these to the binary.
    fn queue_reply(&mut self, bytes: &[u8]);

    /// Approximate bytes the registry could still accept. The handler
    /// uses this for an early-reject on declared payload size (`S=`),
    /// so a 4 GiB declared upload doesn't allocate before failing.
    /// Default returns `u64::MAX` — i.e. "no budget gate from the host".
    fn pending_budget_remaining(&self) -> u64 {
        u64::MAX
    }

    /// Largest decoded image, in RGBA8 bytes, the host could ever store
    /// (its registry cap, not what is free right now: inserts evict).
    ///
    /// The handler bounds work by it *before* allocating: zlib inflation
    /// (SEC-5), a PNG's IHDR dimensions (SEC-7) and `t=f`/`t=t` reads
    /// (SEC-3). Default [`DEFAULT_MAX_IMAGE_BYTES`].
    fn max_image_bytes(&self) -> u64 {
        DEFAULT_MAX_IMAGE_BYTES
    }

    /// Hint: the cursor should advance after a `TransmitAndPlace` that
    /// didn't set `C=1`. The host translates this into actual cursor
    /// motion. Default is a no-op so simple test sinks don't need to
    /// care about cursor state.
    ///
    /// `start_col` is the column the placement started at. M1: the
    /// reference kitty does `c->x += cols; c->y += rows - 1;`, so the
    /// cursor lands at `(start_col + cols, start_row + rows - 1)` — the
    /// image's LAST row, one column past its right edge. The handler
    /// captures `start_col` via [`KittySink::cursor_col`] before the
    /// placement is consumed.
    fn advance_cursor_after_placement(&mut self, _rows: u16, _cols: u16, _start_col: u16) {}

    /// Current cursor column. Consulted by the handler so it can
    /// thread `start_col` into
    /// [`KittySink::advance_cursor_after_placement`] without taking
    /// a separate borrow of the sink. Default `0` keeps the trait
    /// usable from test sinks that don't model a cursor.
    fn cursor_col(&self) -> u16 {
        0
    }

    /// True iff the host's image registry holds an entry for `id`.
    ///
    /// `a=p` and the animation actions reply ENOENT for an image that
    /// isn't stored (PLC-1, WIRE-12), before placing anything or moving
    /// the cursor. Default `false`.
    fn image_exists(&self, _id: u32) -> bool {
        false
    }

    /// The id of the newest stored image with number `number` (`I=`), or
    /// `None`. Commands that name an image by number act on the newest
    /// one with that number (PLC-3). Default `None`.
    fn image_id_for_number(&self, _number: u32) -> Option<u32> {
        None
    }

    /// The id the host would give an image sent without `i=` right now.
    /// kitty allocates the id before validating a transmit, so a failed
    /// `I=`-only transmit still reports it (`i=4,I=156;ENODATA:…`).
    /// Default `0` (no id in the reply).
    fn next_image_id(&self) -> u32 {
        0
    }

    /// Pixel dimensions `(width, height)` of a registered image, or `None`
    /// if not registered. Consulted by `a=p` (`handle_place`) so a
    /// standalone placement of an already-transmitted image can derive its
    /// natural cell span and aspect-ratio fit (M4) — the image's bytes
    /// aren't in hand at that call site the way they are for `a=T`. Default
    /// `None` keeps simple test sinks on the old 1×1 fallback.
    fn image_dimensions(&self, _id: u32) -> Option<(u32, u32)> {
        None
    }

    /// Host's cell size in pixels. The handler uses this to derive the
    /// cell span when the client omits `r=`/`c=` — kitty spec says the
    /// image then occupies `ceil(img_dim / cell_dim)` cells. Default
    /// `(1, 1)` keeps test sinks producing the same legacy 1-cell-per-
    /// pixel behavior unless they override.
    fn cell_pixel_size(&self) -> (u16, u16) {
        (1, 1)
    }
}

/// In-flight chunked transmission state.
#[derive(Debug)]
struct PendingUpload {
    /// Header from the first chunk. The handler validates incoming
    /// chunk headers against this.
    head: Header,
    /// Concatenated base64 bytes (NOT decoded yet — kitty's
    /// reassembly happens on the wire-format bytes).
    buf: Vec<u8>,
    /// When the latest chunk arrived; drives [`PENDING_UPLOAD_TTL`]
    /// expiry.
    last_chunk: Instant,
    /// Handler-wide chunk counter at the latest chunk. Orders eviction
    /// (least recently active first) without relying on clock ticks.
    last_seq: u64,
}

/// Top-level Kitty dispatcher.
#[derive(Debug)]
pub struct KittyHandler {
    /// Pending chunked uploads keyed by `i=` (image id). For `I=`
    /// (image number) we don't currently maintain a parallel slot —
    /// `i=` is the canonical id (pre-approved trade-off).
    pending: HashMap<u32, PendingUpload>,
    /// Image id of the upload currently in progress. Set by the first
    /// chunk (which carries `a=t|T,i=N,m=1`); consulted by continuation
    /// chunks that omit `i=` (yazi, kitty +kitten icat, every
    /// real-world client). Cleared when `m=0` finalizes or the upload
    /// is abandoned with an error.
    ///
    /// The kitty spec doesn't strictly require `i=` on continuation
    /// chunks — terminals are expected to track an active upload — so
    /// rejecting headerless continuations would break every client.
    active_upload_id: Option<u32>,
    /// Per-instance override of [`DEFAULT_PENDING_CAP`].
    pending_cap: usize,
    /// Per-instance override of [`DEFAULT_PENDING_TOTAL_CAP`].
    pending_total_cap: usize,
    /// Chunks seen so far; stamps [`PendingUpload::last_seq`].
    chunk_seq: u64,
}

impl Default for KittyHandler {
    fn default() -> Self {
        Self::new()
    }
}

impl KittyHandler {
    /// Fresh handler with default caps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            active_upload_id: None,
            pending_cap: DEFAULT_PENDING_CAP,
            pending_total_cap: DEFAULT_PENDING_TOTAL_CAP,
            chunk_seq: 0,
        }
    }

    /// Override the maximum pending-upload buffer size. The global cap
    /// grows with it if needed, so a single upload always fits.
    pub fn set_pending_cap(&mut self, cap: usize) {
        self.pending_cap = cap;
        self.pending_total_cap = self.pending_total_cap.max(cap);
    }

    /// Bytes buffered across all in-flight chunked uploads.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        // Count allocated capacity, not length: `extend_from_slice` grows
        // the buffer geometrically, so capacity is the memory actually
        // held. Counting it keeps the global cap honest (reviewer item 7).
        self.pending.values().map(|p| p.buf.capacity()).sum()
    }

    /// Number of in-flight chunked uploads.
    #[must_use]
    pub fn pending_uploads(&self) -> usize {
        self.pending.len()
    }

    /// Process one APC payload split into `(header, body)`.
    ///
    /// `header_bytes` MUST start with `G` — the caller stripped only the
    /// surrounding APC framing.
    pub fn process<S: KittySink>(
        &mut self,
        header_bytes: &[u8],
        body: &[u8],
        sink: &mut S,
    ) -> Result<(), HandlerError> {
        self.expire_pending(Instant::now());
        // WIRE-6: kitty 0.49.2 drops a malformed header (unknown key, bad
        // number, bad enum value) without a reply, even when it carries
        // `i=`. A reply nobody reads ends up as typed text in the shell,
        // so we do the same.
        let header = super::header::parse(header_bytes).map_err(HandlerError::BadHeader)?;
        // B8: specifying both `i=` and `I=` in any command is an error.
        // Reply EINVAL (respecting quiet) and perform no action.
        if header.image_id != 0 && header.image_number != 0 {
            reply_error_if_verbose(
                &header,
                sink,
                ErrorCode::Einval,
                "Must not specify both image id and image number",
            );
            return Ok(());
        }
        self.dispatch(header, body, sink);
        Ok(())
    }

    fn dispatch<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        match header.action {
            Action::Transmit | Action::TransmitAndPlace => {
                self.handle_transmit(header, body, sink);
            }
            Action::Place => {
                self.handle_place(header, sink);
            }
            Action::Delete => {
                self.handle_delete(header, sink);
            }
            Action::Query => {
                // Per the kitty spec, `a=q` is "transmit + test": the
                // app sends a (typically tiny) image and the terminal
                // replies OK if it could decode it, or the appropriate
                // error code if not. The terminal must NOT store or
                // display the payload.
                //
                // This is what apps use to probe kitty-graphics support
                // at startup (yazi, helix, btop, ...). Replying ENOENT
                // here — which M11a-followup.I2 did by looking the id
                // up in the registry — tells the probing app "no
                // support", which is the opposite of what we want.
                //
                // `KittySink::image_exists` is still exposed for future
                // use cases (e.g. a hypothetical "does cache hold N"
                // action) but is NOT consulted by `a=q`.
                self.handle_query(header, body, sink);
            }
            Action::Frame | Action::Animate | Action::Compose => {
                self.handle_animation(&header, sink);
            }
        }
    }

    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn handle_query<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // kitty answers a query only when it carries `i=`: an anonymous
        // or `I=`-only query is silent, success or failure (WIRE-7).
        if header.image_id == 0 {
            return;
        }
        if !matches!(header.transmission, Transmission::Direct) {
            reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "transmission medium");
            return;
        }
        // Decode and validate. Same pipeline as `finalize_transmit`
        // but we discard the result (Query must not store/display). An
        // empty body is not special: like kitty it fails the dimension
        // or data checks (`EINVAL:Zero width/height not allowed`).
        let decoded = decode_base64(body);
        match decode_payload(&header, decoded, sink.max_image_bytes()) {
            Ok(_) => reply_ok_if_verbose(&header, sink),
            Err(e) => reply_error_if_verbose(&header, sink, e.code, &e.detail),
        }
    }

    fn handle_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // M6: Non-direct mediums (`t=f`, `t=t`, `t=s`) carry a
        // base64-encoded PATH (or shm name) in the body rather than the
        // image data itself. They are inherently single-shot — the file
        // holds the whole payload — so they never participate in the
        // chunked (`m=1`) reassembly machinery below. Acquire the bytes
        // from the medium, then run them through the SAME
        // compression/format decode-and-place tail as direct
        // transmission via `finalize_decoded`.
        if !matches!(header.transmission, Transmission::Direct) {
            self.handle_medium_transmit(header, body, sink);
            return;
        }

        // Resolve which pending upload this chunk targets. Real-world
        // clients (yazi, kitty +kitten icat) emit the first chunk with
        // `a=T,i=N,m=1,<other params>` and subsequent chunks with just
        // `m={1|0}` — no `i=`. Per the kitty spec the terminal must
        // remember the active upload and route headerless continuations
        // to it. Without this, `m=0` would be parsed as a brand-new
        // single-chunk transmit with no `s=`/`v=`, which trips the
        // decoder's "missing required dimensions" check.
        let key = if header.image_id != 0 {
            header.image_id
        } else {
            self.active_upload_id.unwrap_or(0)
        };

        // We have a continuation target either because the client passed
        // an explicit `i=` (rare with chunked uploads) or because we're
        // already mid-stream on an anonymous (no-`i=`) upload that we've
        // been tracking via `active_upload_id`. Without the latter
        // branch, clients like bannerfetch that omit `i=` on every chunk
        // would have their `m=0` final chunk parsed as a brand-new
        // single-shot transmit — the bare header carries no `s=` / `v=`
        // and the decoder rejects it with `EINVAL`.
        let has_route = header.image_id != 0 || self.active_upload_id.is_some();

        // Continuation: append to the existing pending buffer.
        if has_route && let Some(pending) = self.pending.get_mut(&key) {
            // WIRE-8: a `q=` on a continuation chunk applies to the whole
            // upload, as in kitty. `q=0` can't be told apart from no `q=`,
            // so it never lowers an earlier chunk's quietness.
            if header.quiet != Quiet::Verbose {
                pending.head.quiet = header.quiet;
            }
            // Compare critical header fields. Mismatches → Einval and
            // abandon the upload. Skip the check when the continuation
            // chunk was routed via `active_upload_id` (its header is
            // intentionally bare — only `m=` is set — so comparing it
            // against the first-chunk header would always fail).
            let bare_continuation = header.image_id == 0;
            if !bare_continuation && !headers_continuation_compatible(&pending.head, &header) {
                let head = pending.head.clone();
                self.pending.remove(&key);
                self.active_upload_id = None;
                reply_error_if_verbose(&head, sink, ErrorCode::Einval, "header mismatch");
                return;
            }
            if pending.buf.len() + body.len() > self.pending_cap {
                let head = pending.head.clone();
                self.pending.remove(&key);
                self.active_upload_id = None;
                reply_error_if_verbose(&head, sink, ErrorCode::Efbig, "pending overflow");
                return;
            }
            // SEC-6: stay under the global cap by evicting other uploads.
            self.make_room(Some(key), body.len());
            self.chunk_seq += 1;
            let pending = self.pending.get_mut(&key).expect("make_room keeps `key`");
            // Geometric growth is fine: `pending_bytes` counts capacity,
            // so the global cap already accounts for the slack.
            pending.buf.extend_from_slice(body);
            pending.last_chunk = Instant::now();
            pending.last_seq = self.chunk_seq;
            if header.more {
                return;
            }
            // Final chunk — pop and finalize.
            let pending = self.pending.remove(&key).unwrap();
            self.active_upload_id = None;
            self.finalize_transmit(pending.head, pending.buf, sink);
            return;
        }

        // First / only chunk.
        // Early reject if declared payload size exceeds host budget.
        if header.size > 0 && u64::from(header.size) > sink.pending_budget_remaining() {
            reply_error_if_verbose(&header, sink, ErrorCode::Efbig, "declared size > budget");
            return;
        }
        if body.len() > self.pending_cap {
            reply_error_if_verbose(&header, sink, ErrorCode::Efbig, "body > pending cap");
            return;
        }

        if header.more {
            // First chunk of a multi-chunk upload. Always record
            // `active_upload_id` — including `Some(0)` for fully
            // anonymous uploads (no `i=` on any chunk). Continuation
            // chunks omit `i=` in every real-world client, so this is
            // the only state we can route them back through.
            // SEC-6: free a slot and room under the global byte cap first.
            self.pending.remove(&key);
            self.make_room(None, body.len());
            self.chunk_seq += 1;
            self.active_upload_id = Some(key);
            self.pending.insert(
                key,
                PendingUpload {
                    head: header,
                    buf: body.to_vec(),
                    last_chunk: Instant::now(),
                    last_seq: self.chunk_seq,
                },
            );
            return;
        }

        // Single-chunk transmission.
        self.finalize_transmit(header, body.to_vec(), sink);
    }

    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn finalize_transmit<S: KittySink>(&mut self, header: Header, b64_buf: Vec<u8>, sink: &mut S) {
        // base64-decode. Kitty uses standard base64; whitespace is
        // tolerated by some clients but not all. We use the lenient
        // decoder that accepts standard alphabet, no padding required.
        let decoded = decode_base64(&b64_buf);
        // After base64 decode, direct and medium transmissions converge:
        // `decoded` is the raw (still possibly zlib-compressed) payload.
        self.finalize_decoded(header, decoded, PayloadSource::Direct, sink);
    }

    /// Drop uploads that have had no chunk for [`PENDING_UPLOAD_TTL`].
    /// Silent: the client that abandoned them isn't listening.
    fn expire_pending(&mut self, now: Instant) {
        let before = self.pending.len();
        self.pending
            .retain(|_, p| now.saturating_duration_since(p.last_chunk) < PENDING_UPLOAD_TTL);
        if self.pending.len() != before {
            tracing::debug!(
                expired = before - self.pending.len(),
                "kitty: expired abandoned chunked uploads"
            );
            self.forget_active_if_gone();
        }
    }

    /// Evict least-recently-active uploads (never `keep`) until there is
    /// a free slot (when starting a new upload, `keep == None`) and
    /// `incoming` more bytes fit under the global byte cap. Silent, like
    /// expiry.
    fn make_room(&mut self, keep: Option<u32>, incoming: usize) {
        loop {
            let need_slot = keep.is_none() && self.pending.len() >= MAX_PENDING_UPLOADS;
            let over_bytes = self.pending_bytes() + incoming > self.pending_total_cap;
            if !need_slot && !over_bytes {
                break;
            }
            let Some(victim) = self
                .pending
                .iter()
                .filter(|(k, _)| Some(**k) != keep)
                .min_by_key(|(_, p)| p.last_seq)
                .map(|(k, _)| *k)
            else {
                break;
            };
            let freed = self.pending.remove(&victim).map_or(0, |p| p.buf.len());
            tracing::debug!(
                image_id = victim,
                freed,
                "kitty: evicted an in-flight chunked upload"
            );
        }
        self.forget_active_if_gone();
    }

    fn forget_active_if_gone(&mut self) {
        if self
            .active_upload_id
            .is_some_and(|k| !self.pending.contains_key(&k))
        {
            self.active_upload_id = None;
        }
    }

    /// M6: non-direct transmission mediums — `t=f` (file), `t=t` (temp
    /// file), `t=s` (POSIX shared memory).
    ///
    /// For `t=f`/`t=t` the body is a base64-encoded filesystem PATH; we
    /// read its bytes (honoring `O=` offset and `S=` size) and feed them
    /// into the same decode tail as direct transmission. The path policy
    /// (regular files only, no `/proc`/`/sys`/`/dev`, non-blocking open,
    /// bounded read, `t=t` deletion rules) lives in [`medium`]. Every
    /// failure, up to and including decoding the bytes read, replies
    /// `EBADF:Failed to read image file` (SEC-4). `t=s` is ENOTSUP — see
    /// the match arm.
    #[allow(clippy::needless_pass_by_value)]
    fn handle_medium_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        match header.transmission {
            Transmission::Direct => unreachable!("direct handled by caller"),
            Transmission::File | Transmission::TempFile => {
                let raw = match read_medium_file(&header, body, sink.max_image_bytes()) {
                    Ok(bytes) => bytes,
                    Err(e) => {
                        reply_file_error(&header, sink, &e);
                        return;
                    }
                };
                self.finalize_decoded(header, raw, PayloadSource::File, sink);
            }
            Transmission::Shared => {
                // POSIX shared memory (`shm_open`/`mmap`/`shm_unlink`)
                // needs a libc binding. `toastty-graphics` deliberately
                // does NOT depend on `libc`, and we avoid pulling a heavy
                // new dependency just for this medium. Reply ENOTSUP for
                // now; `t=f`/`t=t` cover the common large-image transfer
                // case (`kitten icat --transfer-mode=file`).
                reply_error_if_verbose(
                    &header,
                    sink,
                    ErrorCode::Enotsup,
                    "shared-memory transmission medium",
                );
            }
        }
    }

    /// Decode tail shared by all transmission mediums: take the raw
    /// (post-base64 for direct, post-file-read for mediums) bytes,
    /// optionally zlib-inflate, decode per `f=`, register, and place.
    /// Preserves the B12 `U=1` virtual-placement guard and the M1–M5
    /// cursor-advance / pixel-offset / aspect-ratio placement behavior.
    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn finalize_decoded<S: KittySink>(
        &mut self,
        header: Header,
        decoded: Vec<u8>,
        source: PayloadSource,
        sink: &mut S,
    ) {
        let img = match decode_payload(&header, decoded, sink.max_image_bytes()) {
            Ok(d) => d,
            Err(e) => {
                match source {
                    // SEC-4: a decode error would reveal what the file
                    // holds (its size, whether it is a PNG).
                    PayloadSource::File => reply_file_error(&header, sink, &e.detail),
                    PayloadSource::Direct => {
                        reply_transmit_error(&header, sink, e.code, &e.detail);
                    }
                }
                return;
            }
        };

        // Hand off to the host. Capture dimensions BEFORE the move so
        // we can build the placement after the borrow ends.
        let (img_w, img_h) = (img.width, img.height);
        let Some(final_id) = sink.register_image(header.image_id, header.image_number, img) else {
            reply_transmit_error(&header, sink, ErrorCode::Efbig, "registry full");
            return;
        };

        // If the action also says "place", emit a default placement at
        // the cursor. The host's adapter uses the cursor's current
        // (row, col) and the configured cell dims to size the rect.
        //
        // B12: `U=1` requests a VIRTUAL placement — the image is
        // registered (done above) but NOT displayed at the cursor, and
        // the cursor must NOT advance. The host stores the virtual
        // placement; U+10EEEE placeholder cells refer to it. So for
        // `U=1` we skip both the visible placement and the cursor
        // advance, but still reply OK.
        if matches!(header.action, Action::TransmitAndPlace) && header.unicode_placeholder {
            sink.place_virtual(virtual_placement_from_header(&header, final_id));
        } else if matches!(header.action, Action::TransmitAndPlace) {
            let (cell_pw, cell_ph) = sink.cell_pixel_size();
            let placement =
                default_placement_from_header(&header, final_id, img_w, img_h, cell_pw, cell_ph);
            let rows_span = placement.rows;
            let cols_span = placement.cols;
            // M1: capture the cursor's start_col BEFORE place_image
            // consumes the placement. The cursor lands at
            // (start_col + cols, start_row + rows - 1).
            let start_col = sink.cursor_col();
            sink.place_image(placement);
            // Kitty spec: T advances the cursor by the placement size
            // unless `C=1`. We delegate to the host so the cursor
            // motion respects scroll/wrap.
            if !header.cursor_no_move {
                sink.advance_cursor_after_placement(rows_span, cols_span, start_col);
            }
        }

        // Reply OK (subject to quietness).
        reply_ok_if_verbose_with_id(&header, sink, final_id);
    }

    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn handle_place<S: KittySink>(&mut self, header: Header, sink: &mut S) {
        // PLC-1 / PLC-3: resolve `i=`, or the newest image with `I=`, and
        // refuse an image that isn't stored (kitty: ENOENT, sent even with
        // `q=1`) before anything is placed or the cursor moves. From here
        // on `header.image_id` is the resolved id, so an `I=` placement
        // replies `i=<id>,I=<n>;OK` like kitty.
        let Some(id) = resolve_image(&header, sink, "Put") else {
            return;
        };
        let mut header = header;
        header.image_id = id;
        // B12: `U=1` is a virtual placement — no visible placement at
        // the cursor and no cursor motion. The image was already
        // transmitted; the host stores the virtual placement and the
        // placeholder cells provide the visible references. Reply OK
        // and return without calling `place_image`.
        if header.unicode_placeholder {
            sink.place_virtual(virtual_placement_from_header(&header, header.image_id));
            reply_ok_if_verbose_with_id(&header, sink, header.image_id);
            return;
        }
        // M13: relative placement. Taken when BOTH `P=` (parent image)
        // and `Q=` (parent placement) are set. The child is positioned at
        // the parent placement's origin plus `(H, V)` cells and the
        // cursor NEVER moves (regardless of `C=`). If only one of `P`/`Q`
        // is set the reference is incomplete → EINVAL.
        if header.parent_image != 0 || header.parent_placement != 0 {
            if header.parent_image == 0 || header.parent_placement == 0 {
                reply_error_if_verbose(
                    &header,
                    sink,
                    ErrorCode::Einval,
                    "relative placement requires both P= and Q=",
                );
                return;
            }
            let (cell_pw, cell_ph) = sink.cell_pixel_size();
            let (img_w, img_h) = sink.image_dimensions(header.image_id).unwrap_or((0, 0));
            let mut placement = default_placement_from_header(
                &header,
                header.image_id,
                img_w,
                img_h,
                cell_pw,
                cell_ph,
            );
            placement.parent = Some((header.parent_image, header.parent_placement));
            placement.rel_offset = (header.h_offset, header.v_offset);
            match sink.place_relative(placement) {
                Ok(()) => reply_ok_if_verbose_with_id(&header, sink, header.image_id),
                Err(e) => {
                    let code = match e {
                        RelativeError::NoParent => ErrorCode::Enoparent,
                        RelativeError::Cycle => ErrorCode::Ecycle,
                        RelativeError::TooDeep => ErrorCode::Etoodeep,
                    };
                    reply_error_if_verbose(&header, sink, code, "relative placement");
                }
            }
            // Relative placements never move the cursor.
            return;
        }
        // Standalone `a=p`: look up the already-transmitted image's pixel
        // dimensions so the placement can derive its natural cell span and
        // aspect-ratio fit (M4) — e.g. `a=p,c=10` on a 200×100 image yields
        // 3 rows, not the 1×1/`ceil(img_h/cell)` fallbacks. Falls back to
        // 0×0 (→ 1×1) only if the image isn't registered.
        let (cell_pw, cell_ph) = sink.cell_pixel_size();
        let (img_w, img_h) = sink.image_dimensions(header.image_id).unwrap_or((0, 0));
        let placement =
            default_placement_from_header(&header, header.image_id, img_w, img_h, cell_pw, cell_ph);
        let rows_span = placement.rows;
        let cols_span = placement.cols;
        // M2: capture the start column BEFORE place_image consumes the
        // placement, so the cursor lands one column past the image's
        // right edge (start_col + cols).
        let start_col = sink.cursor_col();
        sink.place_image(placement);
        // M2: a standalone `a=p` moves the cursor by `(cols, rows-1)`
        // just like `a=T`, unless `C=1` (cursor_no_move) or `U=1`
        // (unicode_placeholder — handled by the early return above).
        if !header.cursor_no_move {
            sink.advance_cursor_after_placement(rows_span, cols_span, start_col);
        }
        reply_ok_if_verbose_with_id(&header, sink, header.image_id);
    }

    /// DEL-1: kitty never replies to `a=d`, even with `i=` and `q=0`, so
    /// neither do we. A client that doesn't read replies would otherwise
    /// get them on stdin.
    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn handle_delete<S: KittySink>(&mut self, header: Header, sink: &mut S) {
        sink.delete_image(header.delete, &header);
    }

    /// `a=f` / `a=a` / `a=c` (WIRE-3, WIRE-12). Animation isn't
    /// implemented yet: on a stored image the command is accepted
    /// silently, so the still first frame stays up and nothing lands in
    /// the shell (kitty doesn't answer `kitten icat`'s `a=a` commands
    /// either). On an image that isn't stored it gets kitty's ENOENT.
    ///
    /// An implementation reads the keys under their animation meaning
    /// with [`Header::frame`], [`Header::animation`] and
    /// [`Header::composition`].
    #[allow(clippy::unused_self)]
    fn handle_animation<S: KittySink>(&mut self, header: &Header, sink: &mut S) {
        let Some(_id) = resolve_image(header, sink, "Animation") else {
            return;
        };
    }
}

/// Errors from [`KittyHandler::process`] before any sink calls happen.
/// Errors detected later are turned into protocol replies via the sink.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandlerError {
    /// Malformed header.
    #[error("malformed kitty header: {0}")]
    BadHeader(super::header::KittyHeaderError),
}

/// A continuation chunk belongs to the same upload as the first chunk
/// when it routes to the same in-flight identity.
///
/// Reference kitty ignores everything except `m=` (and the running
/// payload) on continuation chunks — the spec says continuation chunks
/// "must have only the `m` and optionally `q` keys", and terminals
/// ignore extras rather than validating them. We therefore only compare
/// the chunk-routing identity (`i=`); incidental keys like `f=`, `o=`,
/// `a=`, `S=`/`v=` on a continuation chunk are tolerated (and ignored)
/// rather than rejected with EINVAL.
fn headers_continuation_compatible(first: &Header, next: &Header) -> bool {
    first.image_id == next.image_id
}

/// Decode a base64 payload. Whitespace is stripped (some clients wrap
/// lines) and padding is optional. Data that still doesn't decode is
/// treated as empty, the way kitty's decoder drops bad input, so the
/// reply is about the missing data (`ENODATA:Insufficient image data:
/// 0 < N`) rather than the encoding.
fn decode_base64(input: &[u8]) -> Vec<u8> {
    let cleaned: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .unwrap_or_else(|e| {
            tracing::debug!(error = %e, "kitty: undecodable base64 payload");
            Vec::new()
        })
}

/// Where a payload's bytes came from. File payloads get SEC-4's uniform
/// error reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PayloadSource {
    /// `t=d`: the bytes were in the escape code.
    Direct,
    /// `t=f` / `t=t`: the bytes were read from a file.
    File,
}

/// A failed [`decode_payload`]: the reply code and detail.
#[derive(Debug)]
struct PayloadError {
    code: ErrorCode,
    detail: String,
}

impl From<DecodeError> for PayloadError {
    fn from(e: DecodeError) -> Self {
        Self {
            code: e.code(),
            detail: e.to_string(),
        }
    }
}

/// Inflate (`o=z`) and decode (`f=`) a payload, within `quota` bytes of
/// decoded image. Codes and messages follow kitty 0.49.2 (WIRE-7).
fn decode_payload(header: &Header, data: Vec<u8>, quota: u64) -> Result<ImageData, PayloadError> {
    // kitty checks `f=` and the dimensions before it looks at the data.
    let expected = raw_len(header.format, header.source_width, header.source_height)?;
    let raw = match header.compression {
        Compression::None => data,
        Compression::Zlib => {
            if let Some(n) = expected
                && data.len() as u64 > n.saturating_add(COMPRESSED_DATA_SLACK)
            {
                return Err(DecodeError::TooMuchData.into());
            }
            // The size the stream must inflate to: `s*v*bpp` for raw
            // pixels; for PNG the spec requires `S=` (the PNG's own size)
            // with compression, and kitty holds the stream to it.
            let inflated = expected.unwrap_or(u64::from(header.size));
            let mismatch = || PayloadError {
                code: ErrorCode::Einval,
                detail: "Image data size post inflation does not match expected size".into(),
            };
            if inflated == 0 {
                return Err(mismatch());
            }
            // SEC-5: never inflate past what the image can use. When the
            // declared size already exceeds the quota, reject before
            // inflating at all (reviewer item 6 on #30); kitty replies
            // EFBIG:Too much data for an over-cap image.
            if inflated > quota {
                return Err(DecodeError::TooMuchData.into());
            }
            let out = inflate_zlib(&data, inflated).map_err(|e| PayloadError {
                code: ErrorCode::Einval,
                detail: format!("Failed to inflate image data with error: {}", e.zlib_name()),
            })?;
            if out.len() as u64 != inflated {
                return Err(mismatch());
            }
            out
        }
    };
    Ok(decode(
        header.format,
        &raw,
        header.source_width,
        header.source_height,
        quota,
    )?)
}

/// Why [`inflate_zlib`] failed, named after the zlib status kitty
/// reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InflateError {
    /// Corrupt stream or bad checksum.
    Data,
    /// The stream ended early, or goes on past the output limit.
    Buf,
}

impl InflateError {
    fn zlib_name(self) -> &'static str {
        match self {
            InflateError::Data => "Z_DATA_ERROR",
            InflateError::Buf => "Z_BUF_ERROR",
        }
    }
}

/// Inflate a zlib stream into at most `limit` bytes (SEC-5), so a small
/// bomb can't allocate gigabytes. Like kitty inflating into a buffer of
/// the expected size:
/// - output that doesn't fit, or a stream cut off before its end (for
///   example a missing checksum), is `Z_BUF_ERROR`;
/// - a corrupt stream is `Z_DATA_ERROR`;
/// - bytes after the end of the stream are ignored.
fn inflate_zlib(input: &[u8], limit: u64) -> Result<Vec<u8>, InflateError> {
    use flate2::{Decompress, FlushDecompress, Status};
    let mut z = Decompress::new(true);
    let mut out = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let left = limit - out.len() as u64;
        let room = usize::try_from(left).unwrap_or(usize::MAX).min(chunk.len());
        // Once `limit` bytes are out, probe with one byte of room: any
        // more output means the stream is longer than the image.
        let full = room == 0;
        let dst = &mut chunk[..room.max(1)];
        let (in0, out0) = (z.total_in(), z.total_out());
        let consumed = usize::try_from(in0).unwrap_or(usize::MAX).min(input.len());
        let status = z
            .decompress(&input[consumed..], dst, FlushDecompress::None)
            .map_err(|_| InflateError::Data)?;
        let produced = usize::try_from(z.total_out() - out0).unwrap_or(usize::MAX);
        if full && produced > 0 {
            return Err(InflateError::Buf);
        }
        out.extend_from_slice(&dst[..produced]);
        match status {
            Status::StreamEnd => return Ok(out),
            Status::BufError => return Err(InflateError::Buf),
            Status::Ok if produced == 0 && z.total_in() == in0 => return Err(InflateError::Buf),
            Status::Ok => {}
        }
    }
}

/// Read the file a `t=f` / `t=t` command names. `body` is the base64
/// path. See [`medium::read_image_file`] for the policy.
fn read_medium_file(header: &Header, body: &[u8], quota: u64) -> Result<Vec<u8>, FileError> {
    // Undecodable base64 becomes an empty path, which fails like any other.
    let path_bytes = decode_base64(body);
    let path = path_from_bytes(path_bytes);
    // Uncompressed raw pixels need exactly `s*v*bpp` bytes: read that
    // much and no more. A longer file is fine (kitty ignores the tail), a
    // shorter one fails. Otherwise read `S=` bytes, or to EOF.
    let size = match (header.size, header.compression) {
        (0, Compression::None) => raw_len(header.format, header.source_width, header.source_height)
            .ok()
            .flatten()
            .unwrap_or(0),
        (size, _) => u64::from(size),
    };
    medium::read_image_file(&FileRequest {
        path: &path,
        offset: u64::from(header.offset),
        size,
        max: quota,
        delete_temp: matches!(header.transmission, Transmission::TempFile),
    })
}

/// Paths are bytes on the wire; on unix they need not be UTF-8.
#[cfg(unix)]
fn path_from_bytes(bytes: Vec<u8>) -> std::path::PathBuf {
    use std::os::unix::ffi::OsStringExt;
    std::ffi::OsString::from_vec(bytes).into()
}

#[cfg(not(unix))]
fn path_from_bytes(bytes: Vec<u8>) -> std::path::PathBuf {
    String::from_utf8_lossy(&bytes).into_owned().into()
}

/// SEC-4: every `t=f` / `t=t` failure gets the same reply, so a remote or
/// sandboxed client learns nothing about files it can't read itself. The
/// real reason goes to the local log only.
fn reply_file_error<S: KittySink>(header: &Header, sink: &mut S, reason: &dyn std::fmt::Display) {
    tracing::debug!(
        image_id = header.image_id,
        image_number = header.image_number,
        %reason,
        "kitty: file medium failed"
    );
    reply_transmit_error(header, sink, ErrorCode::Ebadf, medium::FILE_READ_ERROR);
}

/// Error reply for a transmit (`a=t` / `a=T`). kitty creates the image
/// before validating it, so an `I=`-only transmit that fails still
/// reports the id it was given: `i=4,I=156;ENODATA:…` (WIRE-7).
fn reply_transmit_error<S: KittySink>(
    header: &Header,
    sink: &mut S,
    code: ErrorCode,
    detail: &str,
) {
    if header.image_id == 0 && header.image_number != 0 {
        let mut header = header.clone();
        header.image_id = sink.next_image_id();
        reply_error_if_verbose(&header, sink, code, detail);
    } else {
        reply_error_if_verbose(header, sink, code, detail);
    }
}

/// PLC-1 / PLC-3 / WIRE-12: the stored image a command refers to: `i=`,
/// or the newest image with number `I=`.
///
/// A command naming neither is ignored silently (there is no id to reply
/// to). One naming an image that isn't stored gets kitty's ENOENT, which
/// `q=1` doesn't suppress:
/// `i=5;ENOENT:Put command refers to non-existent image with id: 5 and
/// number: 0`. `command` is kitty's word for the action (`Put`,
/// `Animation`).
fn resolve_image<S: KittySink>(header: &Header, sink: &mut S, command: &str) -> Option<u32> {
    if header.image_id == 0 && header.image_number == 0 {
        return None;
    }
    let id = if header.image_id != 0 {
        Some(header.image_id)
    } else {
        sink.image_id_for_number(header.image_number)
    };
    if let Some(id) = id
        && sink.image_exists(id)
    {
        return Some(id);
    }
    reply_error_if_verbose(
        header,
        sink,
        ErrorCode::Enoent,
        &format!(
            "{command} command refers to non-existent image with id: {} and number: {}",
            header.image_id, header.image_number
        ),
    );
    None
}

fn default_placement_from_header(
    header: &Header,
    id: u32,
    img_w: u32,
    img_h: u32,
    cell_pw: u16,
    cell_ph: u16,
) -> Placement {
    // Source rect as given: an unset `w`/`h` means "to the edge" from
    // `x`/`y` (SIZE-5), so keep every key. The renderer resolves it
    // against the image with the same `SrcRect::resolve` (SIZE-6).
    let src = SrcRect {
        x: header.src_x,
        y: header.src_y,
        w: header.src_w,
        h: header.src_h,
    };
    // The drawn size follows kitty 0.49.2 (`Placement::draw_rect`): the
    // source rect's native pixels with no `c=`/`r=` (SIZE-1, SIZE-4), the
    // given side plus the source aspect ratio with only one (SIZE-2), the
    // box with both (SIZE-3: kitty stretches). The cell box reserved for
    // it is `c=`/`r=` when given, else `ceil((drawn + offset) / cell)`,
    // which is what kitty reserves (and moves the cursor by).
    let cols_req = u16::try_from(header.cols).unwrap_or(u16::MAX);
    let rows_req = u16::try_from(header.rows).unwrap_or(u16::MAX);
    let mut p = Placement::new(id, rows_req.max(1), cols_req.max(1));
    p.placement_id = header.placement_id;
    p.src_rect = src;
    p.z = header.z;
    p.fit_cols = cols_req;
    p.fit_rows = rows_req;
    // M3: `X=` / `Y=` are sub-cell pixel offsets within the first cell,
    // applied at render time; they don't move the box. Clamped to the
    // cell like kitty (SIZE-10). (Header names kept as `cell_x`/`cell_y`.)
    // The relative branch in `handle_place` sets `parent` / `rel_offset`
    // for `a=p` with `P=`/`Q=` set (M13).
    let cw = u32::from(cell_pw.max(1));
    let ch = u32::from(cell_ph.max(1));
    p.pix_offset = (header.cell_x.min(cw - 1), header.cell_y.min(ch - 1));
    let shown = if img_w == 0 || img_h == 0 {
        // Image size unknown (not registered): reserve a single cell.
        (0, 0)
    } else {
        let r = src.resolve(img_w, img_h);
        (r.w, r.h)
    };
    #[allow(clippy::cast_precision_loss)]
    let ((ox, oy), (w, h)) = p.draw_rect(shown, (cw as f32, ch as f32));
    let span = |px: f32, off: f32, cell: u32| -> u16 {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss
        )]
        let n = ((px + off) / cell as f32).ceil() as u32;
        u16::try_from(n.max(1)).unwrap_or(u16::MAX)
    };
    if cols_req == 0 {
        p.cols = span(w, ox, cw);
    }
    if rows_req == 0 {
        p.rows = span(h, oy, ch);
    }
    p.clip_rows = 0..p.rows;
    p
}

/// The virtual placement a `U=1` command creates. `c=`/`r=` are kept
/// raw: `0` means "natural size", resolved against the cell size when
/// the placeholder cells are drawn.
fn virtual_placement_from_header(header: &Header, id: u32) -> VirtualPlacement {
    VirtualPlacement {
        image_id: id,
        placement_id: header.placement_id,
        cols: header.cols,
        rows: header.rows,
        z: header.z,
    }
}

fn reply_ok_if_verbose<S: KittySink>(header: &Header, sink: &mut S) {
    reply_ok_if_verbose_with_id(header, sink, header.image_id);
}

fn reply_ok_if_verbose_with_id<S: KittySink>(header: &Header, sink: &mut S, image_id: u32) {
    if !matches!(header.quiet, Quiet::Verbose) {
        return;
    }
    if !client_wants_reply(header) {
        return;
    }
    sink.queue_reply(&encode_ok(
        image_id,
        header.image_number,
        header.placement_id,
    ));
}

fn reply_error_if_verbose<S: KittySink>(
    header: &Header,
    sink: &mut S,
    code: ErrorCode,
    detail: &str,
) {
    // M16: `q=2` (`Quiet::Silent`) suppresses ALL replies — including
    // errors — NOT just successes. This intentionally diverges from the
    // literal spec text (which reads as "q=2 suppresses failures"). We
    // match reference kitty, whose `graphics.c` gate is effectively
    //   `if (g->quiet) { if (is_ok_response || g->quiet > 1) return; }`
    // i.e. `q>=2` returns no response of any kind. Do NOT "fix" this to
    // the literal spec — real clients (and kitty's own test suite)
    // expect total silence at q=2.
    if matches!(header.quiet, Quiet::Silent) {
        return;
    }
    if !client_wants_reply(header) {
        return;
    }
    sink.queue_reply(&encode_error(
        header.image_id,
        header.image_number,
        header.placement_id,
        code,
        detail,
    ));
}

/// True iff the client gave us something to echo back in the reply.
///
/// Mirrors reference kitty (`graphics.c` `finish_command_response`,
/// gated on `g->id || g->image_number`): if the client provided
/// neither `i=` nor `I=`, they have no identifier to correlate a
/// reply with, so we don't send one. Beyond the spec angle, this
/// prevents the APC reply leaking into the shell when the upstream
/// client exits without draining its replies (bannerfetch and other
/// fire-and-forget tools) — bash's readline treats the `ESC _`
/// prefix as `M-_` (yank-last-arg) and the rest of the reply lands
/// on the prompt as literal text.
fn client_wants_reply(header: &Header) -> bool {
    header.image_id != 0 || header.image_number != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal sink that records every call.
    #[derive(Debug, Default)]
    struct MockSink {
        registered: Vec<(u32, ImageData)>,
        placements: Vec<Placement>,
        deletes: Vec<(DeleteSpec, Header)>,
        replies: Vec<Vec<u8>>,
        budget: u64,
        assign_next: u32,
        /// `I=` → newest id, like `Term::image_number_to_id`.
        numbers: std::collections::HashMap<u32, u32>,
        deleted: Vec<u32>,
        cursor_advances: usize,
    }

    impl MockSink {
        fn with_budget(budget: u64) -> Self {
            Self {
                budget,
                assign_next: 1,
                ..Self::default()
            }
        }
    }

    impl KittySink for MockSink {
        fn register_image(
            &mut self,
            id_request: u32,
            image_number: u32,
            data: ImageData,
        ) -> Option<u32> {
            let id = if id_request == 0 {
                let assigned = self.assign_next;
                self.assign_next += 1;
                assigned
            } else {
                id_request
            };
            self.registered.push((id, data));
            if image_number != 0 {
                self.numbers.insert(image_number, id);
            }
            Some(id)
        }
        fn image_exists(&self, id: u32) -> bool {
            !self.deleted.contains(&id) && self.registered.iter().any(|(rid, _)| *rid == id)
        }
        fn image_id_for_number(&self, number: u32) -> Option<u32> {
            self.numbers.get(&number).copied()
        }
        fn next_image_id(&self) -> u32 {
            self.assign_next
        }
        fn place_image(&mut self, placement: Placement) {
            self.placements.push(placement);
        }
        fn advance_cursor_after_placement(&mut self, _rows: u16, _cols: u16, _start_col: u16) {
            self.cursor_advances += 1;
        }
        fn delete_image(&mut self, delete: DeleteSpec, header: &Header) {
            if delete.free_bytes() {
                self.deleted.push(header.image_id);
            }
            self.deletes.push((delete, header.clone()));
        }
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
        fn pending_budget_remaining(&self) -> u64 {
            self.budget
        }
    }

    /// 1x1 RGBA red pixel.
    fn b64_red_pixel() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode([255u8, 0, 0, 255])
    }

    #[test]
    fn transmit_only_one_chunk_decodes_and_replies() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let header = "Ga=t,f=32,s=1,v=1,i=42";
        let body = b64_red_pixel();
        h.process(header.as_bytes(), body.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].0, 42);
        assert_eq!(sink.registered[0].1.width, 1);
        assert_eq!(sink.registered[0].1.pixels, vec![255, 0, 0, 255]);
        assert_eq!(sink.replies.len(), 1);
        assert!(sink.replies[0].starts_with(b"\x1b_G"));
        assert!(sink.replies[0].ends_with(b"\x1b\\"));
    }

    #[test]
    fn transmit_and_place_emits_placement() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let header = "Ga=T,f=32,s=1,v=1,i=1,c=2,r=3";
        let body = b64_red_pixel();
        h.process(header.as_bytes(), body.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.placements.len(), 1);
        let p = &sink.placements[0];
        assert_eq!(p.image_id, 1);
    }

    #[test]
    fn chunked_uploads_reassemble_in_order() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel(); // 4 bytes => base64 "..." (~8 chars).
        let half = body.len() / 2;
        let (a, b) = body.split_at(half);
        // First chunk: m=1 (more coming).
        h.process(b"Ga=t,f=32,s=1,v=1,i=7,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert_eq!(h.pending_uploads(), 1);
        // Final: m=0.
        h.process(b"Ga=t,f=32,s=1,v=1,i=7,m=0", b.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].0, 7);
        assert_eq!(h.pending_uploads(), 0);
    }

    /// Regression: real-world clients (yazi, `kitty +kitten icat`) send
    /// the first chunk with the full header (`i=N,a=T,...,m=1`) and
    /// every subsequent chunk with only `m={1|0}` — `i=` is omitted.
    /// Per the kitty spec the terminal must remember the active upload
    /// and route headerless continuations to it. Before this fix the
    /// bare continuations were treated as new single-chunk transmits
    /// and the final `m=0` chunk's missing `s=`/`v=` tripped the
    /// decoder's "missing required dimensions" check (EINVAL).
    #[test]
    fn bare_continuation_chunks_route_to_active_upload() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel();
        let third = body.len() / 3;
        let (a, rest) = body.split_at(third);
        let (b, c) = rest.split_at(third);
        // First chunk: full header (i=, s=, v=, format, m=1).
        h.process(b"Ga=t,f=32,s=1,v=1,i=42,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        // Continuation: only m=1, no i= and no s=/v=.
        h.process(b"Gm=1", b.as_bytes(), &mut sink).unwrap();
        // Final: only m=0.
        h.process(b"Gm=0", c.as_bytes(), &mut sink).unwrap();
        assert_eq!(sink.registered.len(), 1, "expected one registered image");
        assert_eq!(sink.registered[0].0, 42);
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(
            !joined.contains("EINVAL") && !joined.contains("EBADF"),
            "expected no error reply, got {joined:?}"
        );
    }

    /// Regression: bannerfetch (and other minimalist clients) omit
    /// `i=` on *every* chunk, including the first — kitty assigns the
    /// id. The first chunk arrives with `f=24,s=W,v=H,c=,r=,a=T,m=1`
    /// (no `i=`); the second chunk is bare `m=0`. Before this fix,
    /// `active_upload_id` was only set when the first chunk had a
    /// non-zero `i=`, so the bare final chunk was parsed as a fresh
    /// single-chunk transmit with width=height=0 and tripped the
    /// decoder's "missing required dimensions" EINVAL.
    #[test]
    fn fully_anonymous_chunked_upload_reassembles() {
        use base64::Engine;
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);

        // 2x2 RGB (12 bytes). Split the base64 across two APC blocks
        // exactly the way bannerfetch does it.
        let rgb = vec![
            255, 0, 0, // red
            0, 255, 0, // green
            0, 0, 255, // blue
            255, 255, 255, // white
        ];
        let body = base64::engine::general_purpose::STANDARD.encode(&rgb);
        let half = body.len() / 2;
        let (a, b) = body.split_at(half);

        // First chunk: full header, NO `i=`.
        h.process(b"Gf=24,s=2,v=2,c=2,r=2,a=T,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 0, "first chunk shouldn't finalize");
        assert_eq!(h.pending_uploads(), 1, "first chunk must enter pending");

        // Final chunk: bare m=0.
        h.process(b"Gm=0", b.as_bytes(), &mut sink).unwrap();

        // Exactly one image registered, with the assigned id, no
        // EINVAL on the wire.
        assert_eq!(
            sink.registered.len(),
            1,
            "anonymous upload must produce one image, replies={:?}",
            sink.replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect::<Vec<_>>(),
        );
        assert_eq!(sink.registered[0].1.width, 2);
        assert_eq!(sink.registered[0].1.height, 2);
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(
            !joined.contains("EINVAL"),
            "anonymous upload must not EINVAL: {joined:?}",
        );
        assert_eq!(h.pending_uploads(), 0, "pending state must drain");
    }

    /// Reply policy: a fully anonymous successful upload (no `i=`, no
    /// `I=`) gets no acknowledgement. This matches reference kitty's
    /// `graphics.c` gate (`if (g->id || g->image_number)`) and stops
    /// the OK reply from leaking into the shell when the client exits
    /// without draining stdin — bash's readline reads the `ESC _`
    /// prefix as `M-_` (yank-last-arg) and the rest of the reply
    /// lands on the prompt as literal text.
    #[test]
    fn anonymous_successful_upload_emits_no_reply() {
        use base64::Engine;
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let rgb = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let body = base64::engine::general_purpose::STANDARD.encode(&rgb);
        let half = body.len() / 2;
        let (a, b) = body.split_at(half);

        h.process(b"Gf=24,s=2,v=2,c=2,r=2,a=T,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        h.process(b"Gm=0", b.as_bytes(), &mut sink).unwrap();

        assert_eq!(sink.registered.len(), 1, "image must still register");
        assert!(
            sink.replies.is_empty(),
            "anonymous upload must not reply, got {:?}",
            sink.replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect::<Vec<_>>(),
        );
    }

    #[test]
    fn anonymous_failed_upload_emits_no_reply() {
        // Errors are suppressed under the same gate — the client has
        // no identifier to correlate the failure against. Trigger a
        // bad-base64 error to exercise the error path.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=t,f=32,s=1,v=1", b"!!!", &mut sink).unwrap();
        assert!(
            sink.replies.is_empty(),
            "anonymous error path must not reply, got {:?}",
            sink.replies
        );
    }

    #[test]
    fn upload_with_explicit_id_still_replies_ok() {
        // Sanity: the new gate must not regress the i=N case. yazi,
        // helix, btop etc. all rely on the `OK` reply being delivered.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=7",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(
            joined.contains("i=7"),
            "must echo client id, got {joined:?}"
        );
        assert!(joined.contains(";OK"), "must reply OK, got {joined:?}");
    }

    #[test]
    fn upload_with_image_number_only_replies_with_assigned_id() {
        // Per spec, when the client passes `I=M`, the terminal must
        // include the newly-assigned `i=` alongside `I=M` in the
        // reply. The new gate must still let this through.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,I=11",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains("I=11"), "must echo I=, got {joined:?}");
        assert!(
            joined.contains("i="),
            "must include assigned i=, got {joined:?}"
        );
        assert!(joined.contains(";OK"), "must reply OK, got {joined:?}");
    }

    /// The first chunk's header governs the upload; a different `f=` on
    /// a continuation is ignored, as in kitty. The 6 bytes are within
    /// kitty's 10-byte slack for a 1x1 RGBA image, so it succeeds.
    #[test]
    fn chunked_continuation_keys_are_ignored() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=t,f=32,s=1,v=1,i=7,m=1", b"AAAA", &mut sink)
            .unwrap();
        h.process(b"Ga=t,f=24,s=1,v=1,i=7,m=0", b"BBBB", &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].1.pixels.len(), 4);
        assert_eq!(h.pending_uploads(), 0);
        assert_eq!(sink.replies, vec![b"\x1b_Gi=7;OK\x1b\\".to_vec()]);
    }

    #[test]
    fn oversized_declared_payload_rejected_with_efbig() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(16);
        h.process(b"Ga=t,f=32,s=64,v=64,i=1,S=16384", b"", &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert!(
            sink.replies
                .iter()
                .any(|r| { std::str::from_utf8(r).is_ok_and(|s| s.contains("EFBIG")) })
        );
    }

    /// WIRE-12: kitty answers an animation command on an image that
    /// isn't stored with ENOENT (not ENOTSUP).
    #[test]
    fn animation_on_unknown_image_replies_enoent() {
        for (cmd, header) in [
            ("a=f", &b"Ga=f,i=1"[..]),
            ("a=a", b"Ga=a,i=1,s=3"),
            ("a=c", b"Ga=c,i=1,C=2"),
        ] {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::with_budget(1 << 30);
            h.process(header, b"", &mut sink).unwrap();
            assert_eq!(
                sink.replies,
                vec![
                    b"\x1b_Gi=1;ENOENT:Animation command refers to non-existent image with id: 1 and number: 0\x1b\\"
                        .to_vec()
                ],
                "{cmd}"
            );
        }
        // By number, and with q=1 (errors still go out); q=2 is silent.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=f,I=603,q=1", b"", &mut sink).unwrap();
        h.process(b"Ga=a,i=4,q=2", b"", &mut sink).unwrap();
        h.process(b"Ga=f", b"", &mut sink).unwrap();
        assert_eq!(
            sink.replies,
            vec![
                b"\x1b_GI=603;ENOENT:Animation command refers to non-existent image with id: 0 and number: 603\x1b\\"
                    .to_vec()
            ]
        );
    }

    /// WIRE-3: until animation is implemented, `a=f`/`a=a`/`a=c` on a
    /// stored image are silent no-ops, so `kitten icat anim.gif` gets no
    /// ENOTSUP replies injected into the shell. The image is untouched.
    #[test]
    fn animation_on_stored_image_is_silent() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=9,q=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        for header in [
            &b"Ga=f,i=9,f=32,s=1,v=1"[..],
            b"Ga=a,i=9,s=3,v=1",
            b"Ga=a,i=9,c=1",
            b"Ga=c,i=9,r=1,c=1",
            b"Ga=c,i=9,C=2",
            b"Ga=f,m=1",
        ] {
            h.process(header, b64_red_pixel().as_bytes(), &mut sink)
                .unwrap();
        }
        assert!(sink.replies.is_empty(), "{:?}", sink.replies);
        assert_eq!(sink.registered.len(), 1);
        assert!(sink.placements.is_empty());
        assert_eq!(h.pending_uploads(), 0);
    }

    #[test]
    fn delete_dispatches_through_sink() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=d,d=A", b"", &mut sink).unwrap();
        assert_eq!(sink.deletes.len(), 1);
        assert!(sink.deletes[0].0.is_all());
        assert!(sink.deletes[0].0.free_bytes());
    }

    #[test]
    fn place_without_id_is_silently_dropped() {
        // `a=p` with neither `i=` nor `I=` is malformed, but per kitty
        // reference behaviour we have nothing to attach a reply to:
        // the client gave us no identifier to correlate a reply
        // against. The placement is dropped (no `sink.place_image`
        // call) and we emit nothing on the wire.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=p", b"", &mut sink).unwrap();
        assert!(
            sink.placements.is_empty(),
            "malformed place must not produce a placement"
        );
        assert!(
            sink.replies.is_empty(),
            "anonymous a=p must not emit a reply: {:?}",
            sink.replies
        );
    }

    /// PLC-3: `a=p,I=N` with no stored image numbered N replies kitty's
    /// ENOENT. (kitty writes this reply with a stray leading comma,
    /// `\x1b_G,I=9;…`; we leave it out.)
    #[test]
    fn place_by_unknown_image_number_replies_enoent() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=p,I=9", b"", &mut sink).unwrap();
        assert_eq!(
            sink.replies,
            vec![
                b"\x1b_GI=9;ENOENT:Put command refers to non-existent image with id: 0 and number: 9\x1b\\"
                    .to_vec()
            ]
        );
        assert!(sink.placements.is_empty());
    }

    #[test]
    fn quiet_level_one_suppresses_ok() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel();
        h.process(b"Ga=t,f=32,s=1,v=1,i=42,q=1", body.as_bytes(), &mut sink)
            .unwrap();
        // OK reply suppressed.
        let replies_str: Vec<String> = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).to_string())
            .collect();
        // No OK reply.
        // We're keeping q=1 conservative — current impl drops only the
        // OK reply on Verbose vs not-Verbose split. q=1 in our impl is
        // NoOk: still drops the OK reply.
        assert!(
            !replies_str.iter().any(|s| s.contains(";OK")),
            "expected no OK, got {replies_str:?}"
        );
    }

    #[test]
    fn quiet_level_two_suppresses_all() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        // Trigger an error (bad b64).
        h.process(b"Ga=t,f=32,s=1,v=1,i=42,q=2", b"!!!", &mut sink)
            .unwrap();
        assert!(sink.replies.is_empty(), "got {:?}", sink.replies);
    }

    #[test]
    fn bad_header_is_handler_error() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let err = h.process(b"NotG", b"", &mut sink).unwrap_err();
        assert!(matches!(err, HandlerError::BadHeader(_)));
    }

    /// WIRE-7: an empty `a=q` is validated like any other (kitty:
    /// `EINVAL:Zero width/height not allowed`), and a query is answered
    /// only when it carries `i=`.
    #[test]
    fn query_replies_like_kitty() {
        let px = b64_red_pixel();
        let cases: [(&[u8], &[u8], &str); 7] = [
            (
                b"Ga=q,i=42",
                b"",
                "\x1b_Gi=42;EINVAL:Zero width/height not allowed\x1b\\",
            ),
            (
                b"Ga=q,i=43,f=32,s=1,v=1",
                b"",
                "\x1b_Gi=43;ENODATA:Insufficient image data: 0 < 4\x1b\\",
            ),
            (
                b"Ga=q,i=44,f=32,s=2,v=2",
                px.as_bytes(),
                "\x1b_Gi=44;ENODATA:Insufficient image data: 4 < 16\x1b\\",
            ),
            (
                b"Ga=q,i=45,f=32,s=1,v=1",
                px.as_bytes(),
                "\x1b_Gi=45;OK\x1b\\",
            ),
            (b"Ga=q,I=150,f=32,s=1,v=1", px.as_bytes(), ""),
            (b"Ga=q,I=151,f=32,s=2,v=2", px.as_bytes(), ""),
            (b"Ga=q,f=32,s=1,v=1", px.as_bytes(), ""),
        ];
        for (header, body, want) in cases {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::with_budget(1 << 30);
            h.process(header, body, &mut sink).unwrap();
            let got: String = sink
                .replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect();
            assert_eq!(got, want, "{}", String::from_utf8_lossy(header));
            assert!(sink.registered.is_empty(), "Query must not store");
        }
    }

    #[test]
    fn query_with_valid_body_replies_ok_without_storing() {
        // The yazi probe shape: `a=q,s=1,v=1,t=d,f=24;AAAA` — 1x1 RGB
        // pixel. Decode succeeds → OK. Registry stays empty (Query
        // must not store).
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=q,s=1,v=1,t=d,f=24,i=31", b"AAAA", &mut sink)
            .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains(";OK"), "got {joined:?}");
        assert!(sink.registered.is_empty(), "Query must not store the image");
    }

    #[test]
    fn query_with_garbage_body_replies_error() {
        // Body that is valid base64 of garbage PNG bytes → EBADPNG
        // (PNG decoder rejects). The base64 layer succeeds first.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        // "garbage!" base64-encoded.
        h.process(b"Ga=q,f=100,i=99", b"Z2FyYmFnZSE=", &mut sink)
            .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert_eq!(joined, "\x1b_Gi=99;EBADPNG:Not a PNG file\x1b\\");
    }

    #[test]
    fn query_known_image_returns_ok() {
        // Transmit id=7 first, then query the same id with a payload:
        // the query tests the payload, not the registry (no ENOENT).
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel();
        h.process(b"Ga=t,f=32,s=1,v=1,i=7", body.as_bytes(), &mut sink)
            .unwrap();
        // Drop the transmit reply so we only inspect query replies.
        sink.replies.clear();
        h.process(b"Ga=q,i=7,f=32,s=1,v=1", body.as_bytes(), &mut sink)
            .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains(";OK"), "expected OK, got {joined:?}");
        assert!(!joined.contains("ENOENT"));
    }

    #[test]
    fn pending_cap_overflow_during_continuation() {
        let mut h = KittyHandler::new();
        h.set_pending_cap(8);
        let mut sink = MockSink::with_budget(1 << 30);
        // Start.
        h.process(b"Ga=t,f=32,s=1,v=1,i=5,m=1", b"AAAAAA", &mut sink)
            .unwrap();
        // Overflow the 8-byte pending cap.
        h.process(b"Ga=t,f=32,s=1,v=1,i=5,m=1", b"BBBBBB", &mut sink)
            .unwrap();
        assert!(
            sink.replies
                .iter()
                .any(|r| { std::str::from_utf8(r).is_ok_and(|s| s.contains("EFBIG")) })
        );
        assert_eq!(h.pending_uploads(), 0);
    }

    fn joined(sink: &MockSink) -> String {
        sink.replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect()
    }

    fn enoent_put(id: u32, number: u32) -> String {
        format!(
            "ENOENT:Put command refers to non-existent image with id: {id} and number: {number}"
        )
    }

    /// PLC-1: `a=p` on an id that isn't stored replies kitty's ENOENT —
    /// also with `q=1`, silent only with `q=2` — and places nothing and
    /// doesn't move the cursor. Same with `p=`, with `U=1`, and after the
    /// image is freed.
    #[test]
    fn plc1_place_unknown_id_replies_enoent() {
        let cases: [(&[u8], String); 5] = [
            (
                b"Ga=p,i=143999",
                format!("\x1b_Gi=143999;{}\x1b\\", enoent_put(143999, 0)),
            ),
            (
                b"Ga=p,i=143998,q=1",
                format!("\x1b_Gi=143998;{}\x1b\\", enoent_put(143998, 0)),
            ),
            (b"Ga=p,i=143997,q=2", String::new()),
            (
                b"Ga=p,i=143996,p=5",
                format!("\x1b_Gi=143996,p=5;{}\x1b\\", enoent_put(143996, 0)),
            ),
            (
                b"Ga=p,i=143995,U=1",
                format!("\x1b_Gi=143995;{}\x1b\\", enoent_put(143995, 0)),
            ),
        ];
        for (header, want) in cases {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::with_budget(1 << 30);
            h.process(header, b"", &mut sink).unwrap();
            assert_eq!(joined(&sink), want, "{}", String::from_utf8_lossy(header));
            assert!(sink.placements.is_empty());
            assert_eq!(sink.cursor_advances, 0);
        }
    }

    #[test]
    fn plc1_place_freed_id_replies_enoent() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let px = b64_red_pixel();
        h.process(b"Ga=t,i=500,f=32,s=1,v=1,q=2", px.as_bytes(), &mut sink)
            .unwrap();
        h.process(b"Ga=d,d=I,i=500", b"", &mut sink).unwrap();
        h.process(b"Ga=p,i=500", b"", &mut sink).unwrap();
        assert_eq!(
            joined(&sink),
            format!("\x1b_Gi=500;{}\x1b\\", enoent_put(500, 0))
        );
        assert!(sink.placements.is_empty());
        // A stored image still places and replies OK.
        h.process(b"Ga=t,i=501,f=32,s=1,v=1,q=2", px.as_bytes(), &mut sink)
            .unwrap();
        sink.replies.clear();
        h.process(b"Ga=p,i=501,C=1", b"", &mut sink).unwrap();
        assert_eq!(joined(&sink), "\x1b_Gi=501;OK\x1b\\");
        assert_eq!(sink.placements.len(), 1);
    }

    /// PLC-3: `a=p,I=N` places the newest image numbered N and replies
    /// with its id, like kitty's `i=1,I=144;OK`.
    #[test]
    fn plc3_place_by_number_uses_newest_image() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let px = b64_red_pixel();
        h.process(b"Ga=t,I=144,f=32,s=1,v=1", px.as_bytes(), &mut sink)
            .unwrap();
        h.process(b"Ga=t,I=144,f=32,s=1,v=1", px.as_bytes(), &mut sink)
            .unwrap();
        h.process(b"Ga=p,I=144,C=1", b"", &mut sink).unwrap();
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=1,I=144;OK\x1b\\\x1b_Gi=2,I=144;OK\x1b\\\x1b_Gi=2,I=144;OK\x1b\\"
        );
        assert_eq!(sink.placements.len(), 1);
        assert_eq!(sink.placements[0].image_id, 2);
    }

    /// DEL-1: kitty never replies to a delete, even with `i=` and `q=0`.
    #[test]
    fn del1_delete_sends_no_reply() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,i=149,f=32,s=1,v=1,q=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        for header in [
            &b"Ga=d,d=i,i=148"[..],
            b"Ga=d,d=I,i=149,q=0",
            b"Ga=d,d=n,I=150",
            b"Ga=d",
        ] {
            h.process(header, b"", &mut sink).unwrap();
        }
        assert!(sink.replies.is_empty(), "{}", joined(&sink));
        assert_eq!(sink.deletes.len(), 4);
        // Both i= and I= is still an error kitty reports.
        h.process(b"Ga=d,d=i,i=791,I=791", b"", &mut sink).unwrap();
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=791,I=791;EINVAL:Must not specify both image id and image number\x1b\\"
        );
    }

    /// WIRE-7: transmit errors use kitty's codes and messages, and an
    /// `I=`-only failure reports the id kitty would have allocated.
    #[test]
    fn wire7_transmit_errors_match_kitty() {
        let px = b64_red_pixel();
        let b64 = |n: usize| base64::engine::general_purpose::STANDARD.encode(vec![0u8; n]);
        let cases: Vec<(&str, String, &str)> = vec![
            (
                "a=t,i=720,f=32,s=2,v=2",
                px.clone(),
                "i=720;ENODATA:Insufficient image data: 4 < 16",
            ),
            (
                "a=t,i=721,f=32,s=1,v=1",
                b64(16),
                "i=721;EFBIG:Too much data",
            ),
            ("a=t,i=780,f=32,s=1,v=1", b64(14), "i=780;OK"),
            (
                "a=t,i=722,f=100",
                "bm90IGEgcG5nIGF0IGFsbA==".into(),
                "i=722;EBADPNG:Not a PNG file",
            ),
            (
                "a=t,i=727,f=32",
                px.clone(),
                "i=727;EINVAL:Zero width/height not allowed",
            ),
            (
                "a=t,i=728,f=32,s=1",
                px.clone(),
                "i=728;EINVAL:Zero width/height not allowed",
            ),
            (
                "a=t,i=729,f=32,s=1,v=1",
                "!!!!".into(),
                "i=729;ENODATA:Insufficient image data: 0 < 4",
            ),
            (
                "a=t,i=730,f=32,s=1,v=1",
                String::new(),
                "i=730;ENODATA:Insufficient image data: 0 < 4",
            ),
            (
                "a=t,i=731,I=731,f=32,s=1,v=1",
                px.clone(),
                "i=731,I=731;EINVAL:Must not specify both image id and image number",
            ),
            (
                "a=t,I=156,f=32,s=2,v=2",
                px.clone(),
                "i=1,I=156;ENODATA:Insufficient image data: 4 < 16",
            ),
            (
                "a=T,I=4301,f=32,s=2,v=2",
                px.clone(),
                "i=1,I=4301;ENODATA:Insufficient image data: 4 < 16",
            ),
            (
                "a=t,I=800,f=100",
                "anVuaw==".into(),
                "i=1,I=800;EBADPNG:PNG data is truncated: not enough bytes to satisfy read request",
            ),
            ("a=t,I=157,f=32,s=1,v=1", px.clone(), "i=1,I=157;OK"),
            ("a=t,f=32,s=2,v=2", px.clone(), ""),
        ];
        for (keys, body, want) in cases {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::with_budget(1 << 30);
            h.process(format!("G{keys}").as_bytes(), body.as_bytes(), &mut sink)
                .unwrap();
            let want = if want.is_empty() {
                String::new()
            } else {
                format!("\x1b_G{want}\x1b\\")
            };
            assert_eq!(joined(&sink), want, "{keys}");
        }
    }

    /// WIRE-8: a `q=` on a continuation chunk applies to the upload, the
    /// way kitty behaves: the latest non-zero `q=` wins.
    #[test]
    fn wire8_quiet_on_continuation_chunks() {
        let px = b64_red_pixel();
        let (a, b) = px.split_at(4);
        let cases: [(&[&[u8]], &str, &str, &str); 7] = [
            (&[b"Ga=t,i=760,f=32,s=1,v=1,m=1", b"Gm=0,q=2"], a, b, ""),
            (&[b"Ga=t,i=761,f=32,s=1,v=1,m=1", b"Gm=0,q=1"], a, b, ""),
            (&[b"Ga=t,i=762,f=32,s=1,v=1,q=2,m=1", b"Gm=0,q=0"], a, b, ""),
            (&[b"Ga=t,i=763,f=32,s=1,v=1,q=2,m=1", b"Gm=0"], a, b, ""),
            (
                &[b"Ga=t,i=764,f=32,s=2,v=2,m=1", b"Gm=0,q=1"],
                a,
                b,
                "\x1b_Gi=764;ENODATA:Insufficient image data: 4 < 16\x1b\\",
            ),
            (&[b"Ga=t,i=765,f=32,s=2,v=2,m=1", b"Gm=0,q=2"], a, b, ""),
            (
                &[b"Ga=t,i=767,f=32,s=1,v=1,m=1", b"Gm=0"],
                a,
                b,
                "\x1b_Gi=767;OK\x1b\\",
            ),
        ];
        for (headers, first, last, want) in cases {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::with_budget(1 << 30);
            h.process(headers[0], first.as_bytes(), &mut sink).unwrap();
            h.process(headers[1], last.as_bytes(), &mut sink).unwrap();
            assert_eq!(
                joined(&sink),
                want,
                "{}",
                String::from_utf8_lossy(headers[0])
            );
        }
        // q=2 on a middle chunk.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=t,i=766,f=32,s=1,v=1,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        h.process(b"Gm=1,q=2", b"", &mut sink).unwrap();
        h.process(b"Gm=0", b.as_bytes(), &mut sink).unwrap();
        assert!(sink.replies.is_empty(), "{}", joined(&sink));
        assert_eq!(sink.registered.len(), 1);
    }

    fn start_upload(h: &mut KittyHandler, sink: &mut MockSink, id: u32, body: &[u8]) {
        let header = format!("Ga=t,f=32,s=1,v=1,i={id},m=1");
        h.process(header.as_bytes(), body, sink).unwrap();
    }

    /// SEC-6: more than [`MAX_PENDING_UPLOADS`] in flight evicts the
    /// least recently active ones, silently.
    #[test]
    fn pending_upload_count_is_capped() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let n = MAX_PENDING_UPLOADS as u32 + 2;
        for id in 1..=n {
            start_upload(&mut h, &mut sink, id, b"AAAA");
        }
        assert_eq!(h.pending_uploads(), MAX_PENDING_UPLOADS);
        assert!(!h.pending.contains_key(&1) && !h.pending.contains_key(&2));
        assert!((3..=n).all(|id| h.pending.contains_key(&id)));
        assert_eq!(h.active_upload_id, Some(n));
        assert!(sink.replies.is_empty(), "eviction is silent");
    }

    /// SEC-6: all buffers together stay under the global byte cap, both
    /// when an upload starts and when one grows.
    #[test]
    fn pending_total_bytes_are_capped() {
        let mut h = KittyHandler::new();
        h.pending_cap = 60;
        h.pending_total_cap = 90;
        let mut sink = MockSink::with_budget(1 << 30);
        start_upload(&mut h, &mut sink, 1, &[b'A'; 40]);
        start_upload(&mut h, &mut sink, 2, &[b'A'; 40]);
        assert_eq!(h.pending_bytes(), 80);
        // 120 > 90: the oldest (1) goes.
        start_upload(&mut h, &mut sink, 3, &[b'A'; 40]);
        assert!(!h.pending.contains_key(&1));
        assert_eq!(h.pending_bytes(), 80);
        // Growing 3 to 60 would make 100 > 90: 2 goes, 3 is kept.
        h.process(b"Gm=1", &[b'A'; 20], &mut sink).unwrap();
        assert!(!h.pending.contains_key(&2));
        assert_eq!(h.pending[&3].buf.len(), 60);
        // Capacity (what `pending_bytes` counts) may round up past len.
        assert!(h.pending_bytes() >= 60);
        assert!(sink.replies.is_empty(), "eviction is silent");
    }

    /// The checklist repro, scaled down: many abandoned uploads with
    /// different ids never hold more than the global cap.
    #[test]
    fn many_abandoned_uploads_stay_under_the_cap() {
        let mut h = KittyHandler::new();
        h.pending_total_cap = 1000;
        let mut sink = MockSink::with_budget(1 << 30);
        for id in 1..=16 {
            start_upload(&mut h, &mut sink, id, &[b'A'; 300]);
            assert!(h.pending_bytes() <= 1000);
        }
        assert_eq!(h.pending_uploads(), 3);
    }

    /// SEC-6: an upload with no chunk for [`PENDING_UPLOAD_TTL`] is freed,
    /// and headerless continuations no longer route to it.
    #[test]
    fn abandoned_upload_expires() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        start_upload(&mut h, &mut sink, 1, b"AAAA");
        h.expire_pending(Instant::now());
        assert_eq!(h.pending_uploads(), 1, "fresh upload must survive");
        h.expire_pending(Instant::now() + PENDING_UPLOAD_TTL + Duration::from_secs(1));
        assert_eq!(h.pending_uploads(), 0);
        assert_eq!(h.pending_bytes(), 0);
        assert_eq!(h.active_upload_id, None);
    }

    /// A continuation chunk refreshes the TTL.
    #[test]
    fn continuation_refreshes_expiry() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        start_upload(&mut h, &mut sink, 1, b"AAAA");
        let t0 = h.pending[&1].last_chunk;
        h.process(b"Gm=1", b"AAAA", &mut sink).unwrap();
        assert!(h.pending[&1].last_chunk >= t0);
        h.expire_pending(h.pending[&1].last_chunk + PENDING_UPLOAD_TTL / 2);
        assert_eq!(h.pending_uploads(), 1);
    }

    #[test]
    fn default_handler_has_default_caps() {
        let h = KittyHandler::default();
        assert_eq!(h.pending_cap, DEFAULT_PENDING_CAP);
        assert_eq!(h.pending_total_cap, DEFAULT_PENDING_TOTAL_CAP);
    }

    #[test]
    fn medium_query_still_enotsup() {
        // M6: Query (`a=q`) with a non-direct medium remains ENOTSUP —
        // only transmit (`a=t`/`a=T`) gained file/temp support.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=q,t=f,i=1", b"L3RtcC94", &mut sink).unwrap();
        assert!(
            sink.replies
                .iter()
                .any(|r| { std::str::from_utf8(r).is_ok_and(|s| s.contains("ENOTSUP")) })
        );
    }
}

#[cfg(test)]
mod blocker_graphics_tests {
    use super::*;

    /// Sink that records registrations, placements, replies, and the
    /// number of cursor-advance signals so the B12 virtual-placement
    /// behavior can be asserted at the handler level.
    #[derive(Debug, Default)]
    struct RecordingSink {
        registered: Vec<(u32, ImageData)>,
        placements: Vec<Placement>,
        virtuals: Vec<VirtualPlacement>,
        replies: Vec<Vec<u8>>,
        cursor_advances: usize,
        assign_next: u32,
    }

    impl RecordingSink {
        fn new() -> Self {
            Self {
                assign_next: 1,
                ..Self::default()
            }
        }

        fn joined_replies(&self) -> String {
            self.replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect()
        }
    }

    impl KittySink for RecordingSink {
        fn register_image(
            &mut self,
            id_request: u32,
            _image_number: u32,
            data: ImageData,
        ) -> Option<u32> {
            let id = if id_request == 0 {
                let assigned = self.assign_next;
                self.assign_next += 1;
                assigned
            } else {
                id_request
            };
            self.registered.push((id, data));
            Some(id)
        }
        fn image_exists(&self, id: u32) -> bool {
            self.registered.iter().any(|(rid, _)| *rid == id)
        }
        fn place_image(&mut self, placement: Placement) {
            self.placements.push(placement);
        }
        fn delete_image(&mut self, _delete: DeleteSpec, _header: &Header) {}
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
        fn place_virtual(&mut self, placement: VirtualPlacement) {
            self.virtuals.push(placement);
        }
        fn advance_cursor_after_placement(&mut self, _rows: u16, _cols: u16, _start_col: u16) {
            self.cursor_advances += 1;
        }
    }

    fn b64_red_pixel() -> String {
        base64::engine::general_purpose::STANDARD.encode([255u8, 0, 0, 255])
    }

    // ----- B8: i= and I= together is EINVAL -----

    #[test]
    fn b8_both_i_and_image_number_replies_einval_and_does_not_register() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // Valid in every other respect; only the i= + I= conflict.
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=1,I=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        let joined = sink.joined_replies();
        assert!(joined.contains("EINVAL"), "expected EINVAL, got {joined:?}");
        assert!(
            sink.registered.is_empty(),
            "i=+I= conflict must not register an image"
        );
        // The reply should echo both identifiers.
        assert!(
            joined.contains("i=1"),
            "reply should echo i=, got {joined:?}"
        );
        assert!(
            joined.contains("I=2"),
            "reply should echo I=, got {joined:?}"
        );
    }

    // ----- WIRE-6: malformed headers get no reply (kitty 0.49.2) -----

    /// Every case kitty 0.49.2 drops silently, even with `i=`/`I=` and
    /// `q=0`. Nothing is registered either.
    #[test]
    fn wire6_malformed_header_is_silent() {
        for header in [
            &b"Ga=t,i=700,f=32,s=abc,v=1"[..],
            b"Ga=t,i=701,f=32,s=,v=1",
            b"Ga=t,i=702,f=32,s=1,v=1,q=-1",
            b"Ga=X,i=704",
            b"Ga=X,I=7",
            b"Ga=t,i=705,t=x,f=32,s=1,v=1",
            b"Ga=t,i=706,o=y,f=32,s=1,v=1",
            b"Ga=T,i=707,x=-5,f=32,s=1,v=1",
            b"Ga=t,i=742,f=32,s=1,v=1,W=5",
            b"Ga=t,i=-1,f=32,s=1,v=1",
            b"Ga=t,,i=802,f=32,s=1,v=1",
            b"Ga=t, i=4102,f=32,s=1,v=1",
            b"Ga=t,i=4101,f=32,s=1,v=1,",
        ] {
            let mut h = KittyHandler::new();
            let mut sink = RecordingSink::new();
            let err = h
                .process(header, b64_red_pixel().as_bytes(), &mut sink)
                .unwrap_err();
            assert!(matches!(err, HandlerError::BadHeader(_)));
            assert!(
                sink.replies.is_empty(),
                "{}: got {:?}",
                String::from_utf8_lossy(header),
                sink.joined_replies()
            );
            assert!(sink.registered.is_empty());
            assert_eq!(h.pending_uploads(), 0);
        }
    }

    /// Review of #37: `q=3` stores the image silently; `m=2` is `m=1`
    /// (kitty 0.49.2).
    #[test]
    fn wire6_q_and_m_are_numbers() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        h.process(
            b"Ga=t,i=702,f=32,s=1,v=1,q=3",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        assert!(sink.replies.is_empty(), "{}", sink.joined_replies());
        assert_eq!(sink.registered.len(), 1);
        let px = b64_red_pixel();
        let (a, b) = px.split_at(4);
        h.process(b"Ga=t,i=703,f=32,s=1,v=1,m=2", a.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(h.pending_uploads(), 1, "m=2 starts a chunked upload");
        h.process(b"Gm=0", b.as_bytes(), &mut sink).unwrap();
        assert_eq!(sink.joined_replies(), "\x1b_Gi=703;OK\x1b\\");
        assert_eq!(sink.registered.len(), 2);
    }

    /// kitty accepts an out-of-range `z=` that still fits 32 bits, and an
    /// unknown `f=` number is a reply-worthy error, not a parse error.
    #[test]
    fn wire6_kitty_accepts_these() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        h.process(
            b"Ga=T,i=708,z=2147483648,f=32,s=1,v=1",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        h.process(
            b"Ga=t,i=4112,f=8,s=1,v=1",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(
            sink.joined_replies(),
            "\x1b_Gi=708;OK\x1b\\\x1b_Gi=4112;EINVAL:Unknown image format: 8\x1b\\"
        );
    }

    // ----- B12: U=1 virtual placement -----

    #[test]
    fn b12_transmit_and_place_with_u1_is_virtual() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        h.process(
            b"Ga=T,f=32,s=1,v=1,i=5,c=2,r=2,U=1",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        // Image IS registered.
        assert_eq!(
            sink.registered.len(),
            1,
            "U=1 must still register the image"
        );
        assert_eq!(sink.registered[0].0, 5);
        // No visible placement, no cursor advance.
        assert!(
            sink.placements.is_empty(),
            "U=1 must not create a visible placement"
        );
        assert_eq!(sink.cursor_advances, 0, "U=1 must not advance the cursor");
        // The virtual placement is handed to the host with its box.
        assert_eq!(
            sink.virtuals,
            vec![VirtualPlacement {
                image_id: 5,
                placement_id: 0,
                cols: 2,
                rows: 2,
                z: 0,
            }]
        );
        // Still replies OK (id present).
        assert!(sink.joined_replies().contains(";OK"));
    }

    #[test]
    fn b12_place_with_u1_is_virtual() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=5,q=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        // a=p with U=1 — no visible placement, no cursor advance.
        h.process(b"Ga=p,i=5,p=3,U=1,c=7,z=-4", b"", &mut sink)
            .unwrap();
        assert_eq!(
            sink.virtuals,
            vec![VirtualPlacement {
                image_id: 5,
                placement_id: 3,
                cols: 7,
                rows: 0,
                z: -4,
            }],
            "c=/r= stay raw (0 = natural size, resolved at draw time)"
        );
        assert!(
            sink.placements.is_empty(),
            "a=p,U=1 must not create a visible placement"
        );
        assert_eq!(sink.cursor_advances, 0, "a=p,U=1 must not advance cursor");
        assert!(sink.joined_replies().contains(";OK"));
    }
}

/// M4: aspect-ratio derivation in `default_placement_from_header` when
/// only one of `c=` / `r=` is supplied. Also covers the M3 invariant
/// that `X=`/`Y=` (stored in `cell_x`/`cell_y`) become `pix_offset` and
/// do NOT shift the placement's cell ranges.
#[cfg(test)]
mod major_place_handler_tests {
    use super::*;

    // ---- M4: only one axis given => derive the other from aspect ----

    #[test]
    fn m4_only_cols_given_derives_rows_from_aspect() {
        // 200x100 image, 10x20 cell, c=10.
        // rows = ceil(c * cell_pw * img_h / (img_w * cell_ph))
        //      = ceil(10 * 10 * 100 / (200 * 20))
        //      = ceil(10000 / 4000) = ceil(2.5) = 3.
        // (NOT the natural-size value ceil(100/20) = 5.)
        let header = Header {
            cols: 10,
            rows: 0,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!(p.cols, 10);
        assert_eq!(
            p.rows, 3,
            "rows derived from aspect, not natural cell count"
        );
    }

    #[test]
    fn m4_only_rows_given_derives_cols_from_aspect() {
        // 200x100 image, 10x20 cell, r=4.
        // cols = ceil(r * cell_ph * img_w / (img_h * cell_pw))
        //      = ceil(4 * 20 * 200 / (100 * 10))
        //      = ceil(16000 / 1000) = 16.
        let header = Header {
            cols: 0,
            rows: 4,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!(p.rows, 4);
        assert_eq!(
            p.cols, 16,
            "cols derived from aspect, not natural cell count"
        );
    }

    #[test]
    fn m4_both_zero_uses_natural_cell_count() {
        // 200x100 image, 10x20 cell: natural = (20 cols, 5 rows).
        let header = Header::default();
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!((p.cols, p.rows), (20, 5));
    }

    #[test]
    fn m4_both_set_used_verbatim() {
        let header = Header {
            cols: 7,
            rows: 9,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!((p.cols, p.rows), (7, 9));
    }

    // ---- M3 / SIZE-10: X=/Y= are pixel offsets, clamped to the cell ----

    #[test]
    fn m3_xy_become_pixel_offset_and_widen_the_reservation() {
        // The box stays at the cursor (`col`/`line` untouched), but like
        // kitty the reservation covers the offset too: ceil((16+3)/8) = 3
        // (kitty-ref: X=8,Y=19 on 200x100 with 9x20 cells reserves 24x6).
        let base = default_placement_from_header(&Header::default(), 1, 16, 16, 8, 8);
        let header = Header {
            cell_x: 3,
            cell_y: 4,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 16, 16, 8, 8);
        assert_eq!(p.pix_offset, (3, 4));
        assert_eq!(
            (p.col, p.line),
            (base.col, base.line),
            "the box doesn't move"
        );
        assert_eq!((base.cols, base.rows), (2, 2));
        assert_eq!((p.cols, p.rows), (3, 3));
    }

    #[test]
    fn size10_offsets_clamp_to_the_cell() {
        // kitty-ref, 9x20 cells: X=25,Y=40 lands at +8,+19.
        let header = Header {
            cell_x: 25,
            cell_y: 40,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 200, 100, 9, 20);
        assert_eq!(p.pix_offset, (8, 19));
        assert_eq!((p.cols, p.rows), (24, 6));
    }

    /// `(cols, rows)` reserved and the drawn `(w, h)` for `header` on a
    /// `w`x`h` image with 9x20 cells (kitty-ref's), as kitty measures it.
    fn fit(header: &Header, w: u32, h: u32) -> ((u16, u16), (f32, f32)) {
        let p = default_placement_from_header(header, 1, w, h, 9, 20);
        let src = p.src_rect.resolve(w, h);
        ((p.cols, p.rows), p.draw_rect((src.w, src.h), (9.0, 20.0)).1)
    }

    fn hdr(f: impl FnOnce(&mut Header)) -> Header {
        let mut h = Header::default();
        f(&mut h);
        h
    }

    #[test]
    fn kitty_ref_sizes_match() {
        // Every expected size below was measured on kitty 0.49.2 (PIL
        // bounding boxes, 9x20 cells; see the A3 PR).
        // SIZE-1: native size.
        assert_eq!(fit(&Header::default(), 205, 105).1, (205.0, 105.0));
        assert_eq!(fit(&Header::default(), 205, 105).0, (23, 6));
        assert_eq!(fit(&Header::default(), 3, 50).1, (3.0, 50.0));
        assert_eq!(fit(&Header::default(), 1, 1).1, (1.0, 1.0));
        // SIZE-2: one side given, the other from the aspect ratio.
        assert_eq!(fit(&hdr(|h| h.cols = 10), 400, 200).1, (90.0, 45.0));
        assert_eq!(fit(&hdr(|h| h.cols = 10), 400, 200).0, (10, 3));
        assert_eq!(fit(&hdr(|h| h.rows = 3), 400, 200).1, (120.0, 60.0));
        assert_eq!(fit(&hdr(|h| h.cols = 10), 333, 100).1, (90.0, 27.0));
        assert_eq!(fit(&hdr(|h| h.rows = 1), 100, 333).1, (6.0, 20.0));
        // SIZE-3: both given: kitty stretches to the box.
        let both = hdr(|h| (h.cols, h.rows) = (4, 8));
        assert_eq!(fit(&both, 200, 100), ((4, 8), (36.0, 160.0)));
        // SIZE-4: size and aspect come from the source rect.
        let src = hdr(|h| (h.src_w, h.src_h) = (60, 40));
        assert_eq!(fit(&src, 200, 100), ((7, 2), (60.0, 40.0)));
        let src12 = hdr(|h| (h.src_w, h.src_h, h.cols) = (60, 40, 12));
        assert_eq!(fit(&src12, 200, 100).1, (108.0, 72.0));
        // SIZE-5: a partial rect runs to the edge.
        assert_eq!(fit(&hdr(|h| h.src_w = 50), 200, 100).1, (50.0, 100.0));
        assert_eq!(fit(&hdr(|h| h.src_h = 30), 200, 100).1, (200.0, 30.0));
        assert_eq!(fit(&hdr(|h| h.src_x = 150), 200, 100).1, (50.0, 100.0));
        assert_eq!(fit(&hdr(|h| h.src_y = 70), 200, 100).1, (200.0, 30.0));
        // SIZE-6: intersected with the image.
        let part = hdr(|h| (h.src_x, h.src_w) = (150, 100));
        assert_eq!(fit(&part, 200, 100).1, (50.0, 100.0));
        let over = hdr(|h| (h.src_w, h.src_h) = (500, 500));
        assert_eq!(fit(&over, 200, 100).1, (200.0, 100.0));
        let out = hdr(|h| (h.src_x, h.src_w) = (250, 50));
        let p = default_placement_from_header(&out, 1, 200, 100, 9, 20);
        assert!(p.src_rect.resolve(200, 100).is_empty(), "shows nothing");
        // SIZE-10: offsets fitted inside c/r.
        let xc = hdr(|h| (h.cell_x, h.cols) = (5, 6));
        assert_eq!(fit(&xc, 200, 100).1, (49.0, 25.0));
        let yr = hdr(|h| (h.cell_y, h.rows) = (7, 3));
        assert_eq!(fit(&yr, 200, 100).1, (106.0, 53.0));
        let xyb = hdr(|h| (h.cell_x, h.cell_y, h.cols, h.rows) = (5, 7, 6, 3));
        assert_eq!(fit(&xyb, 200, 100).1, (49.0, 53.0));
        let x3 = hdr(|h| (h.cell_x, h.cols) = (3, 7));
        assert_eq!(fit(&x3, 333, 100).1, (60.0, 18.0));
    }

    #[test]
    fn size11_zoom_keeps_native_size_and_rescales_cell_fits() {
        // kitty-ref after two font-size steps (cells ~12x29): a natural
        // 205x105 stays 205x105; c=10 on 400x200 becomes 120x60.
        let nat = default_placement_from_header(&Header::default(), 1, 205, 105, 9, 20);
        assert_eq!(nat.draw_rect((205, 105), (12.0, 29.0)).1, (205.0, 105.0));
        let c10 = default_placement_from_header(&hdr(|h| h.cols = 10), 1, 400, 200, 9, 20);
        assert_eq!(c10.draw_rect((400, 200), (12.0, 29.0)).1, (120.0, 60.0));
    }
}

/// M6: file (`t=f`), temp-file (`t=t`), and shared-memory (`t=s`)
/// transmission mediums. Exercised through the public `process` entry
/// point with a mock sink so we can inspect the registered image and
/// emitted replies without a real `Term`.
#[cfg(test)]
mod major_medium_tests {
    use super::*;

    /// Minimal sink: records registrations, placements, and replies.
    /// The cursor/cell/budget methods fall back to trait defaults;
    /// `quota` overrides [`KittySink::max_image_bytes`].
    #[derive(Debug, Default)]
    struct MockSink {
        registered: Vec<(u32, ImageData)>,
        placements: Vec<Placement>,
        replies: Vec<Vec<u8>>,
        quota: Option<u64>,
    }

    impl KittySink for MockSink {
        fn register_image(
            &mut self,
            id_request: u32,
            _image_number: u32,
            data: ImageData,
        ) -> Option<u32> {
            let id = if id_request == 0 { 1 } else { id_request };
            self.registered.push((id, data));
            Some(id)
        }
        fn place_image(&mut self, placement: Placement) {
            self.placements.push(placement);
        }
        fn delete_image(&mut self, _delete: DeleteSpec, _header: &Header) {}
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
        fn max_image_bytes(&self) -> u64 {
            self.quota.unwrap_or(DEFAULT_MAX_IMAGE_BYTES)
        }
    }

    fn replies_joined(sink: &MockSink) -> String {
        sink.replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect()
    }

    /// Unique temp path under the system temp dir, with the
    /// `tty-graphics-protocol` marker (so `t=t` deletion is allowed).
    /// Includes the pid + a counter to avoid collisions across
    /// concurrent test threads.
    fn unique_temp_path(tag: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        std::env::temp_dir().join(format!(
            "tty-graphics-protocol-toastty-medium-{tag}-{pid}-{n}.rgba"
        ))
    }

    fn b64_path(path: &std::path::Path) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(path.to_str().unwrap().as_bytes())
            .into_bytes()
    }

    #[test]
    fn t_f_reads_file_and_registers_image() {
        // 2x2 RGBA: red, green, blue, white.
        let raw: Vec<u8> = vec![
            255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
        ];
        let path = unique_temp_path("tf");
        std::fs::write(&path, &raw).unwrap();

        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=32,s=2,v=2,i=77,t=f", &b64_path(&path), &mut sink)
            .unwrap();

        assert_eq!(
            sink.registered.len(),
            1,
            "replies={:?}",
            replies_joined(&sink)
        );
        assert_eq!(sink.registered[0].0, 77);
        assert_eq!(sink.registered[0].1.width, 2);
        assert_eq!(sink.registered[0].1.height, 2);
        assert_eq!(sink.registered[0].1.pixels, raw);
        // File must NOT be deleted for `t=f`.
        assert!(path.exists(), "t=f must not delete the source file");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn t_t_reads_file_then_deletes_it() {
        let raw: Vec<u8> = vec![1, 2, 3, 255, 4, 5, 6, 255]; // 2x1 RGBA.
        let path = unique_temp_path("tt");
        std::fs::write(&path, &raw).unwrap();
        assert!(path.exists());

        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=32,s=2,v=1,i=88,t=t", &b64_path(&path), &mut sink)
            .unwrap();

        assert_eq!(
            sink.registered.len(),
            1,
            "replies={:?}",
            replies_joined(&sink)
        );
        assert_eq!(sink.registered[0].1.pixels, raw);
        // Temp file under the temp dir must be deleted after reading.
        assert!(
            !path.exists(),
            "t=t must delete the temp file after reading"
        );
    }

    #[test]
    fn t_f_honors_offset_and_size() {
        // Write 4 RGBA pixels; read the middle 2 via O=4, S=8.
        let raw: Vec<u8> = vec![
            10, 10, 10, 255, // pixel 0 (skipped)
            20, 20, 20, 255, // pixel 1 (read)
            30, 30, 30, 255, // pixel 2 (read)
            40, 40, 40, 255, // pixel 3 (skipped)
        ];
        let path = unique_temp_path("tfoff");
        std::fs::write(&path, &raw).unwrap();

        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        // 2x1 image, offset 4 bytes, size 8 bytes.
        h.process(
            b"Ga=t,f=32,s=2,v=1,i=5,t=f,O=4,S=8",
            &b64_path(&path),
            &mut sink,
        )
        .unwrap();

        assert_eq!(
            sink.registered.len(),
            1,
            "replies={:?}",
            replies_joined(&sink)
        );
        assert_eq!(sink.registered[0].1.pixels, raw[4..12]);
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn t_f_nonexistent_path_replies_ebadf_no_image() {
        let path = unique_temp_path("missing");
        // Do NOT create it.
        assert!(!path.exists());

        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=32,s=2,v=2,i=9,t=f", &b64_path(&path), &mut sink)
            .unwrap();

        assert!(
            sink.registered.is_empty(),
            "no image must register on read failure"
        );
        assert_eq!(
            replies_joined(&sink),
            "\x1b_Gi=9;EBADF:Failed to read image file\x1b\\"
        );
    }

    #[test]
    fn t_s_replies_enotsup() {
        // Shared memory is intentionally unimplemented (no libc dep in
        // toastty-graphics). It must reply ENOTSUP, not silently succeed.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=32,s=1,v=1,i=3,t=s", b"L215c2htb2Jq", &mut sink)
            .unwrap();
        assert!(sink.registered.is_empty());
        let joined = replies_joined(&sink);
        assert!(joined.contains("ENOTSUP"), "got {joined:?}");
    }

    // ---- SEC-1..SEC-5, SEC-7 through the handler ----

    const FILE_ERR: &str = "EBADF:Failed to read image file";

    /// A private directory for one test (its name carries the
    /// `tty-graphics-protocol` marker), removed on drop.
    struct TestDir(std::path::PathBuf);

    impl TestDir {
        fn new(tag: &str) -> Self {
            let dir = unique_temp_path(tag).with_extension("d");
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self, name: &str) -> std::path::PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn b64(bytes: &[u8]) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .encode(bytes)
            .into_bytes()
    }

    fn zlib(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::best());
        e.write_all(bytes).unwrap();
        e.finish().unwrap()
    }

    fn run(header: &str, body: &[u8], quota: Option<u64>) -> MockSink {
        let mut h = KittyHandler::new();
        let mut sink = MockSink {
            quota,
            ..MockSink::default()
        };
        h.process(header.as_bytes(), body, &mut sink).unwrap();
        sink
    }

    /// SEC-4: every way a file read can fail puts the same bytes on the
    /// wire, so a client can't tell *why* it failed — "missing" from
    /// "directory" from "not a PNG". kitty: `EBADF:Failed to read image
    /// file` for all of them. (kitty answers `EBADPNG` for a readable
    /// non-PNG; we don't, see the checklist.)
    ///
    /// This removes a *content* oracle only. A successful read still
    /// reveals that a file exists and is readable, and `O=` plus the
    /// OK/EBADF split lets a client binary-search its size — inherent to
    /// `t=f` and the same in kitty.
    #[test]
    fn file_failures_all_reply_identically() {
        use std::os::unix::fs::symlink;
        let d = TestDir::new("sec4");
        std::fs::write(d.path("short.rgba"), [1, 2, 3]).unwrap();
        std::fs::write(d.path("notpng.png"), b"hello, not a png").unwrap();
        std::fs::write(d.path("garbage.z"), b"not zlib").unwrap();
        std::fs::write(
            d.path("big.png"),
            crate::kitty::decode::test_png::claiming(12000, 12000),
        )
        .unwrap();
        std::fs::write(d.path("pixels.rgba"), [0u8; 64]).unwrap();
        std::fs::create_dir(d.path("adir")).unwrap();
        symlink(d.path("loop-b"), d.path("loop-a")).unwrap();
        symlink(d.path("loop-a"), d.path("loop-b")).unwrap();
        symlink("/dev/zero", d.path("zero-link")).unwrap();
        let path = |p: std::path::PathBuf| b64_path(&p);
        let abs = |p: &str| b64_path(std::path::Path::new(p));
        let cases: Vec<(&str, &str, Vec<u8>, Option<u64>)> = vec![
            ("missing", "f=32,s=1,v=1", path(d.path("nope")), None),
            ("directory", "f=32,s=1,v=1", path(d.path("adir")), None),
            ("symlink loop", "f=32,s=1,v=1", path(d.path("loop-a")), None),
            (
                "short raw file",
                "f=32,s=1,v=1",
                path(d.path("short.rgba")),
                None,
            ),
            (
                "S= past EOF",
                "f=100,S=100",
                path(d.path("short.rgba")),
                None,
            ),
            (
                "O= past EOF",
                "f=100,O=10",
                path(d.path("short.rgba")),
                None,
            ),
            (
                "raw length mismatch",
                "f=32,s=1,v=1,S=3",
                path(d.path("short.rgba")),
                None,
            ),
            ("not a PNG", "f=100", path(d.path("notpng.png")), None),
            ("PNG over quota", "f=100", path(d.path("big.png")), None),
            (
                "bad zlib",
                "f=32,s=1,v=1,o=z",
                path(d.path("garbage.z")),
                None,
            ),
            (
                "file over quota",
                "f=100",
                path(d.path("pixels.rgba")),
                Some(32),
            ),
            (
                "raw size over quota",
                "f=32,s=4,v=4",
                path(d.path("pixels.rgba")),
                Some(32),
            ),
            ("/dev/zero", "f=32,s=1,v=1", abs("/dev/zero"), None),
            ("/dev/zero, no size", "f=100", abs("/dev/zero"), None),
            (
                "symlink to /dev/zero",
                "f=100",
                path(d.path("zero-link")),
                None,
            ),
            ("/dev/null", "f=32,s=1,v=1", abs("/dev/null"), None),
            (
                "/proc/self/cmdline",
                "f=32,s=1,v=1",
                abs("/proc/self/cmdline"),
                None,
            ),
            (
                "bad base64 path",
                "f=32,s=1,v=1",
                b"!!!not base64!!!".to_vec(),
                None,
            ),
            ("empty path", "f=32,s=1,v=1", Vec::new(), None),
        ];
        for (i, (name, keys, body, quota)) in cases.into_iter().enumerate() {
            let id = 300 + i as u32;
            let sink = run(&format!("Ga=t,t=f,{keys},i={id}"), &body, quota);
            assert!(sink.registered.is_empty(), "{name}: must not register");
            assert_eq!(
                replies_joined(&sink),
                format!("\x1b_Gi={id};{FILE_ERR}\x1b\\"),
                "{name}"
            );
        }
    }

    /// SEC-2: `t=f`/`t=t` on a FIFO answers at once instead of blocking
    /// in `open()` until a writer appears, and `t=t` keeps the FIFO.
    #[test]
    fn fifo_replies_ebadf_without_blocking() {
        let d = TestDir::new("fifo");
        let fifo = d.path("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo)
                .status()
                .unwrap()
                .success()
        );
        for medium in ["f", "t"] {
            let body = b64_path(&fifo);
            let header = format!("Ga=t,t={medium},f=32,s=2,v=2,i=7");
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || {
                let _ = tx.send(replies_joined(&run(&header, &body, None)));
            });
            let reply = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("t=f on a FIFO blocked the handler");
            assert_eq!(reply, format!("\x1b_Gi=7;{FILE_ERR}\x1b\\"), "t={medium}");
            assert!(fifo.exists(), "t={medium} must not delete a FIFO");
        }
    }

    /// Raw pixels need `s*v*bpp` bytes; like kitty, only that much is
    /// read and a longer file is accepted.
    #[test]
    fn t_f_raw_reads_only_the_bytes_it_needs() {
        let d = TestDir::new("long");
        let p = d.path("long.rgba");
        let mut bytes = vec![7u8; 4];
        bytes.extend_from_slice(&[0u8; 60]);
        std::fs::write(&p, &bytes).unwrap();
        let sink = run("Ga=t,t=f,f=32,s=1,v=1,i=8", &b64_path(&p), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=8;OK\x1b\\");
        assert_eq!(sink.registered[0].1.pixels, vec![7u8; 4]);
    }

    #[test]
    fn t_f_zlib_file_decodes() {
        let d = TestDir::new("zf");
        let p = d.path("px.z");
        std::fs::write(&p, zlib(&[1, 2, 3, 255])).unwrap();
        let sink = run("Ga=t,t=f,f=32,s=1,v=1,o=z,i=9", &b64_path(&p), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=9;OK\x1b\\");
        assert_eq!(sink.registered[0].1.pixels, vec![1, 2, 3, 255]);
    }

    /// SEC-1: a temp file without the marker is read but kept.
    #[test]
    fn t_t_without_marker_keeps_the_file() {
        let path = std::env::temp_dir().join(format!(
            "toastty-medium-nomarker-{}-{}.rgba",
            std::process::id(),
            line!()
        ));
        std::fs::write(&path, [1, 2, 3, 255]).unwrap();
        let sink = run("Ga=t,t=t,f=32,s=1,v=1,i=10", &b64_path(&path), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=10;OK\x1b\\");
        assert!(path.exists(), "no marker: t=t must keep the file");
        std::fs::remove_file(&path).ok();
    }

    /// kitty deletes a `t=t` file it could open even when the read or
    /// the decode then fails.
    #[test]
    fn t_t_deletes_even_when_read_or_decode_fails() {
        let d = TestDir::new("ttfail");
        let short = d.path("short.rgba");
        let notpng = d.path("notpng.png");
        std::fs::write(&short, [1, 2]).unwrap();
        std::fs::write(&notpng, b"not a png").unwrap();
        for (keys, p) in [("f=32,s=1,v=1", &short), ("f=100", &notpng)] {
            let sink = run(&format!("Ga=t,t=t,{keys},i=11"), &b64_path(p), None);
            assert_eq!(
                replies_joined(&sink),
                format!("\x1b_Gi=11;{FILE_ERR}\x1b\\")
            );
            assert!(!p.exists(), "{} must be deleted", p.display());
        }
    }

    /// SEC-1 through the handler, with the real temp roots: `..` that
    /// stays inside the temp dir still deletes. (Escapes out of the temp
    /// dir are tested in `medium` against a private root, so no test
    /// ever points `t=t` at a file outside its own sandbox.)
    #[test]
    fn t_t_dotdot_inside_temp_dir_deletes() {
        let d = TestDir::new("dotdot");
        let inside = d.path("x.rgba");
        std::fs::write(&inside, [1, 2, 3, 255]).unwrap();
        std::fs::create_dir(d.path("sub")).unwrap();
        let sent = d.path("sub/../x.rgba");
        let sink = run("Ga=t,t=t,f=32,s=1,v=1,i=12", &b64_path(&sent), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=12;OK\x1b\\");
        assert!(!inside.exists());
    }

    /// SEC-5: a zlib bomb never inflates past the expected `s*v*bpp`
    /// size. kitty refuses compressed data more than ~1 KiB past the
    /// expected size outright (`EFBIG:Too much data`), and stops a
    /// smaller stream at the expected size (`Z_BUF_ERROR`).
    #[test]
    fn zlib_bomb_stops_at_expected_size() {
        let bomb = zlib(&vec![0u8; 16 << 20]);
        assert!(bomb.len() < 64 * 1024);
        let sink = run("Ga=t,f=32,s=1,v=1,o=z,i=3", &b64(&bomb), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=3;EFBIG:Too much data\x1b\\");
        let small_bomb = zlib(&vec![0u8; 1 << 20]);
        assert!(small_bomb.len() < 400 + 1024);
        for action in ["t", "q"] {
            let sink = run(
                &format!("Ga={action},f=32,s=10,v=10,o=z,i=3"),
                &b64(&small_bomb),
                None,
            );
            assert_eq!(
                replies_joined(&sink),
                "\x1b_Gi=3;EINVAL:Failed to inflate image data with error: Z_BUF_ERROR\x1b\\"
            );
        }
    }

    /// SEC-5, reviewer item 6: when the declared raw size already exceeds
    /// the quota, reply EFBIG before inflating anything. The 1.4 KB bomb
    /// below would otherwise inflate up to the quota first.
    #[test]
    fn zlib_over_quota_declared_size_rejected_before_inflating() {
        let bomb = zlib(&vec![0u8; 4 << 20]);
        let sink = run("Ga=t,f=32,s=1000,v=1000,o=z,i=7", &b64(&bomb), Some(1024));
        assert_eq!(replies_joined(&sink), "\x1b_Gi=7;EFBIG:Too much data\x1b\\");
    }

    /// SEC-5: `f=100,o=z` has no `s*v`; its `S=` is bounded by the quota.
    #[test]
    fn zlib_png_is_bounded_by_quota() {
        let z = zlib(&vec![0u8; 1 << 20]);
        // `S=` over the quota: refused before inflating.
        let sink = run("Ga=t,f=100,o=z,S=1048576,i=4", &b64(&z), Some(1024));
        assert_eq!(replies_joined(&sink), "\x1b_Gi=4;EFBIG:Too much data\x1b\\");
        // Within the quota, the stream is held to `S=`: a longer stream
        // stops at `S=` bytes (kitty: Z_BUF_ERROR).
        let sink = run("Ga=t,f=100,o=z,S=1000,i=4", &b64(&z), Some(1024));
        assert_eq!(
            replies_joined(&sink),
            "\x1b_Gi=4;EINVAL:Failed to inflate image data with error: Z_BUF_ERROR\x1b\\"
        );
    }

    /// The spec requires `S=` (the PNG's size) with `f=100,o=z`; kitty
    /// holds the inflated stream to it (checked live on 0.49.2).
    #[test]
    fn zlib_png_needs_s_like_kitty() {
        use image::ImageEncoder;
        let mut png = Vec::new();
        image::codecs::png::PngEncoder::new(&mut png)
            .write_image(&[255, 0, 0, 255], 1, 1, image::ExtendedColorType::Rgba8)
            .unwrap();
        let z = b64(&zlib(&png));
        let n = png.len();
        let mismatch = "EINVAL:Image data size post inflation does not match expected size";
        let zbuf = "EINVAL:Failed to inflate image data with error: Z_BUF_ERROR";
        for (keys, want) in [
            (format!(",S={n}"), "OK"),
            (String::new(), mismatch),
            (format!(",S={}", n + 10), mismatch),
            (format!(",S={}", n - 10), zbuf),
            (",S=1".to_string(), zbuf),
        ] {
            let sink = run(&format!("Ga=t,f=100,o=z{keys},i=9"), &z, None);
            assert_eq!(
                replies_joined(&sink),
                format!("\x1b_Gi=9;{want}\x1b\\"),
                "{keys}"
            );
        }
    }

    /// WIRE-7: zlib failures carry kitty's messages.
    #[test]
    fn zlib_errors_match_kitty() {
        let px = [255u8, 0, 0, 255];
        let full = zlib(&px);
        let no_checksum = &full[..full.len() - 4];
        let mut trailing = full.clone();
        trailing.extend_from_slice(&[0u8; 8]);
        let cases: [(&str, Vec<u8>, &str); 6] = [
            (
                "f=32,s=1,v=1",
                no_checksum.to_vec(),
                "EINVAL:Failed to inflate image data with error: Z_BUF_ERROR",
            ),
            (
                "f=32,s=1,v=1",
                b"garbage!".to_vec(),
                "EINVAL:Failed to inflate image data with error: Z_DATA_ERROR",
            ),
            (
                "f=32,s=1,v=1",
                zlib(&[0u8; 8]),
                "EINVAL:Failed to inflate image data with error: Z_BUF_ERROR",
            ),
            (
                "f=32,s=2,v=2",
                zlib(&[0u8; 4]),
                "EINVAL:Image data size post inflation does not match expected size",
            ),
            ("f=32,s=1,v=1", trailing, "OK"),
            ("f=32,s=1,v=1", full, "OK"),
        ];
        for (keys, data, want) in cases {
            let sink = run(&format!("Ga=t,{keys},o=z,i=5"), &b64(&data), None);
            assert_eq!(
                replies_joined(&sink),
                format!("\x1b_Gi=5;{want}\x1b\\"),
                "{keys} {data:?}"
            );
        }
    }

    #[test]
    fn inflate_zlib_limit_is_exact() {
        let z = zlib(&[1, 2, 3, 4]);
        assert_eq!(inflate_zlib(&z, 4).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(inflate_zlib(&z, 3), Err(InflateError::Buf));
        assert_eq!(inflate_zlib(&z, u64::MAX).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(inflate_zlib(&z[..z.len() - 4], 4), Err(InflateError::Buf));
        assert_eq!(inflate_zlib(b"garbage!", 4), Err(InflateError::Data));
        assert_eq!(inflate_zlib(b"", 4), Err(InflateError::Buf));
        let big = zlib(&vec![9u8; 200_000]);
        assert_eq!(inflate_zlib(&big, 200_000).unwrap().len(), 200_000);
    }

    /// SEC-7: an oversized PNG is refused from its IHDR, before decoding,
    /// with kitty's `ENOMEM:PNG image is too large`.
    #[test]
    fn png_over_quota_replies_enomem_like_kitty() {
        let png = crate::kitty::decode::test_png::claiming(12000, 12000);
        let sink = run("Ga=t,f=100,i=6", &b64(&png), None);
        assert_eq!(
            replies_joined(&sink),
            "\x1b_Gi=6;ENOMEM:PNG image is too large\x1b\\"
        );
        let sink = run("Ga=q,f=100,i=6", &b64(&png), None);
        assert_eq!(
            replies_joined(&sink),
            "\x1b_Gi=6;ENOMEM:PNG image is too large\x1b\\"
        );
        // The sink's quota is what counts.
        let small = crate::kitty::decode::test_png::claiming(16, 16);
        let sink = run("Ga=t,f=100,i=6", &b64(&small), Some(1023));
        assert!(replies_joined(&sink).contains("ENOMEM"));
    }
}

/// M13: handler-level tests for relative placements (`P=`/`Q=`/`H=`/`V=`)
/// and the `ENOPARENT` / `ECYCLE` / `ETOODEEP` error codes. Uses a sink
/// backed by a real [`crate::image_grid::ImageGrid`] so the parent
/// resolution + validation logic exercises the production code path.
#[cfg(test)]
mod major_relative_handler_tests {
    use super::*;
    use crate::image_grid::ImageGrid;

    /// Sink backed by an `ImageGrid`, so `place_relative` runs real
    /// validation. `place_image` adds at the placement's span anchored at
    /// origin (good enough for these tests; the real `Term` rebases on the
    /// cursor).
    #[derive(Default)]
    struct GridSink {
        grid: ImageGrid,
        registered: Vec<u32>,
        replies: Vec<Vec<u8>>,
    }

    impl KittySink for GridSink {
        fn register_image(&mut self, id: u32, _num: u32, _data: ImageData) -> Option<u32> {
            self.registered.push(id);
            Some(id)
        }
        fn image_exists(&self, id: u32) -> bool {
            self.registered.contains(&id)
        }
        fn place_image(&mut self, placement: Placement) {
            self.grid.add(placement);
        }
        fn place_relative(&mut self, placement: Placement) -> Result<(), RelativeError> {
            self.grid.add_relative(placement).map(|_| ())
        }
        fn delete_image(&mut self, _delete: DeleteSpec, _header: &Header) {}
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
    }

    impl GridSink {
        fn replies_joined(&self) -> String {
            self.replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect::<Vec<_>>()
                .join("|")
        }
    }

    /// Seed a parent placement (image 1, placement 10) at the origin.
    fn seed_parent(sink: &mut GridSink) {
        sink.registered.push(1);
        let mut parent = Placement::new(1, 2, 3);
        parent.placement_id = 10;
        parent.line = 5;
        parent.col = 3;
        sink.grid.add(parent);
    }

    #[test]
    fn relative_placement_resolves_and_replies_ok() {
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        seed_parent(&mut sink);
        sink.registered.push(2);
        // Child image 2 placement 20, parent (1,10), H=2 cols, V=1 row.
        h.process(b"Ga=p,i=2,p=20,P=1,Q=10,H=2,V=1", b"", &mut sink)
            .unwrap();
        let child = sink.grid.find(2, 20).expect("child placement created");
        assert_eq!(child.line, 6); // parent row 5 + V=1
        assert_eq!(child.col, 5); // parent col 3 + H=2
        assert!(sink.replies_joined().contains("OK"));
    }

    #[test]
    fn relative_placement_missing_parent_replies_enoparent() {
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        sink.registered.push(2);
        h.process(b"Ga=p,i=2,p=20,P=99,Q=99,H=1,V=1", b"", &mut sink)
            .unwrap();
        assert!(sink.grid.find(2, 20).is_none());
        assert!(
            sink.replies_joined().contains("ENOPARENT"),
            "got {:?}",
            sink.replies_joined()
        );
    }

    #[test]
    fn relative_placement_cycle_replies_ecycle() {
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        // Seed (1,10) whose parent is (2,20).
        sink.registered.push(1);
        sink.registered.push(2);
        let mut looped = Placement::new(1, 1, 1);
        looped.placement_id = 10;
        looped.parent = Some((2, 20));
        sink.grid.add(looped);
        // Now add (2,20) parented to (1,10) — closes the loop.
        h.process(b"Ga=p,i=2,p=20,P=1,Q=10,H=1,V=1", b"", &mut sink)
            .unwrap();
        assert!(sink.grid.find(2, 20).is_none());
        assert!(
            sink.replies_joined().contains("ECYCLE"),
            "got {:?}",
            sink.replies_joined()
        );
    }

    #[test]
    fn relative_placement_too_deep_replies_etoodeep() {
        use crate::image_grid::MAX_RELATIVE_DEPTH;
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        sink.registered.push(1);
        // Root (1,1), no parent.
        let mut root = Placement::new(1, 1, 1);
        root.placement_id = 1;
        sink.grid.add(root);
        // Build the deepest allowed chain (place=(MAX+1) has MAX ancestors).
        for i in 2..=MAX_RELATIVE_DEPTH as u32 + 1 {
            let hdr = format!("Ga=p,i=1,p={i},P=1,Q={},H=1,V=0", i - 1);
            h.process(hdr.as_bytes(), b"", &mut sink).unwrap();
        }
        // One more exceeds the cap.
        let last = MAX_RELATIVE_DEPTH as u32 + 1;
        let pid = last + 1;
        let hdr = format!("Ga=p,i=1,p={pid},P=1,Q={last},H=1,V=0");
        h.process(hdr.as_bytes(), b"", &mut sink).unwrap();
        assert!(sink.grid.find(1, pid).is_none());
        assert!(
            sink.replies_joined().contains("ETOODEEP"),
            "got {:?}",
            sink.replies_joined()
        );
    }

    #[test]
    fn relative_placement_partial_ref_replies_einval() {
        // Only P= set, no Q= → incomplete reference → EINVAL.
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        seed_parent(&mut sink);
        sink.registered.push(2);
        h.process(b"Ga=p,i=2,p=20,P=1,H=1,V=1", b"", &mut sink)
            .unwrap();
        assert!(sink.grid.find(2, 20).is_none());
        assert!(
            sink.replies_joined().contains("EINVAL"),
            "got {:?}",
            sink.replies_joined()
        );
    }
}

/// Coverage for the minor kitty-protocol fixes m1 (replies echo the
/// placement id `p=`) and m8 (continuation chunks tolerate stray
/// incidental keys instead of being rejected with EINVAL).
#[cfg(test)]
mod minor_graphics_tests {
    use super::*;

    /// Sink that records registered images and replies, mirroring the
    /// `MockSink` pattern in the main test module.
    #[derive(Debug, Default)]
    struct MockSink {
        registered: Vec<(u32, ImageData)>,
        replies: Vec<Vec<u8>>,
        budget: u64,
        assign_next: u32,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
                budget: 1 << 30,
                assign_next: 1,
                ..Self::default()
            }
        }

        /// All queued replies concatenated into a single string for easy
        /// substring assertions.
        fn replies_joined(&self) -> String {
            self.replies
                .iter()
                .map(|r| String::from_utf8_lossy(r).into_owned())
                .collect()
        }
    }

    impl KittySink for MockSink {
        fn register_image(
            &mut self,
            id_request: u32,
            _image_number: u32,
            data: ImageData,
        ) -> Option<u32> {
            let id = if id_request == 0 {
                let assigned = self.assign_next;
                self.assign_next += 1;
                assigned
            } else {
                id_request
            };
            self.registered.push((id, data));
            Some(id)
        }
        fn image_exists(&self, id: u32) -> bool {
            self.registered.iter().any(|(rid, _)| *rid == id)
        }
        fn place_image(&mut self, _placement: Placement) {}
        fn delete_image(&mut self, _delete: DeleteSpec, _header: &Header) {}
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
        fn pending_budget_remaining(&self) -> u64 {
            self.budget
        }
    }

    /// 1x1 RGBA red pixel as base64.
    fn b64_red_pixel() -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode([255u8, 0, 0, 255])
    }

    // ---- m1: reply echoes placement id -------------------------------

    #[test]
    fn ok_reply_echoes_placement_id() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::new();
        // a=t transmit with an explicit image id and placement id.
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=5,p=3",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.replies.len(), 1);
        let reply = sink.replies_joined();
        assert!(reply.contains("i=5"), "missing i=5 in {reply:?}");
        assert!(reply.contains("p=3"), "missing p=3 in {reply:?}");
        assert!(reply.contains("OK"), "expected OK in {reply:?}");
    }

    #[test]
    fn ok_reply_omits_placement_id_when_zero() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::new();
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=5,p=0",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.replies.len(), 1);
        let reply = sink.replies_joined();
        assert!(reply.contains("i=5"), "missing i=5 in {reply:?}");
        assert!(!reply.contains("p="), "unexpected p= in {reply:?}");
    }

    #[test]
    fn error_reply_echoes_placement_id() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::new();
        // A transmit-and-display that asks for both i= and I= is rejected
        // with EINVAL; the error reply must still echo i= and p=. (We use
        // the i=/I= conflict because it errors regardless of sink state.)
        h.process(b"Ga=t,f=32,s=1,v=1,i=7,I=2,p=9", b"", &mut sink)
            .unwrap();
        assert_eq!(sink.replies.len(), 1);
        let reply = sink.replies_joined();
        assert!(reply.contains("EINVAL"), "expected EINVAL in {reply:?}");
        assert!(reply.contains("i=7"), "missing i=7 in {reply:?}");
        assert!(reply.contains("p=9"), "missing p=9 in {reply:?}");
    }

    // ---- m8: continuation chunks tolerate stray keys -----------------

    #[test]
    fn continuation_chunk_with_extra_keys_still_completes() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::new();
        let body = b64_red_pixel();
        let half = body.len() / 2;
        let (a, b) = body.split_at(half);
        // First chunk: full header, more coming.
        h.process(b"Ga=t,f=32,s=1,v=1,i=11,m=1", a.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert_eq!(h.pending_uploads(), 1);
        // Final chunk: carries a DIFFERENT incidental key (stray S= and a
        // mismatched f=) plus the routing i=. Reference kitty ignores
        // these; we must NOT reject with EINVAL.
        h.process(b"Ga=t,f=100,S=999,i=11,m=0", b.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(
            sink.registered.len(),
            1,
            "upload should complete; replies={:?}",
            sink.replies_joined()
        );
        assert_eq!(sink.registered[0].0, 11);
        assert_eq!(h.pending_uploads(), 0);
        assert!(
            !sink.replies_joined().contains("EINVAL"),
            "unexpected EINVAL in {:?}",
            sink.replies_joined()
        );
    }
}
