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
//! Chunked-upload reassembly follows kitty 0.49.2 (WIRE-9/10/11, DEL-3):
//! - There is one loading slot. `m=1` on a first chunk starts it, after
//!   checking the format and dimensions; while it's busy, every direct
//!   transmit is a continuation, whatever its own keys say.
//! - Chunks are base64-decoded on arrival ([`b64`]), each with its own
//!   padding (WIRE-4), so the size cap counts decoded bytes: kitty's
//!   `s*v*bpp` plus slack, or the image quota for PNG (SEC-6, WIRE-11).
//!   Past it the upload is dropped with `EFBIG:Too much data`.
//! - `m=0` (or no `m=`) is the final chunk: decode and dispatch.
//! - Any delete, or a file-medium transmit, aborts the upload silently;
//!   so does [`PENDING_UPLOAD_TTL`] of idleness.

use std::time::{Duration, Instant};

#[cfg(test)]
use base64::Engine as _;

use super::b64;
use super::decode::{DecodeError, RAW_DATA_SLACK, decode, raw_len};
use super::header::{
    Action, AnimationKeys, CompositionKeys, Compression, DeleteSpec, FrameKeys, Header, Quiet,
    Transmission,
};
use super::medium::{self, FileError, FileRequest, ShmRequest};
use super::placeholder::VirtualPlacement;
use super::reply::{ErrorCode, encode_error, encode_frame_error, encode_frame_ok, encode_ok};
use crate::frames::{FrameError, frame_target};
use crate::image_grid::{Placement, RelativeError, SrcRect};
use crate::registry::ImageData;

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

    /// The image `id` was sent with the transient usage hint (`N=1`):
    /// the host may evict it before other images under storage pressure
    /// (spec "Usage hints"). Called right after a successful
    /// [`KittySink::register_image`]. Default: ignore the hint, which the
    /// spec allows.
    fn mark_transient(&mut self, _id: u32) {}

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

    /// Frames of stored image `id`, root included (1 for a still image).
    /// The handler needs it before decoding an `a=f` payload: kitty tags
    /// the reply, errors included, with the frame the command targets.
    fn frame_count(&self, _id: u32) -> u32 {
        1
    }

    /// `a=f`: add or edit a frame of stored image `id` from the decoded
    /// `data` (see [`crate::frames`]). Returns the 1-based frame number
    /// created or edited. Default: animation unsupported.
    fn add_frame(
        &mut self,
        _id: u32,
        _keys: &FrameKeys,
        _data: ImageData,
    ) -> Result<u32, FrameError> {
        Err(FrameError {
            code: ErrorCode::Enotsup,
            detail: "animation".into(),
        })
    }

    /// `a=a`: control the animation of stored image `id`. Never replies.
    fn control_animation(&mut self, _id: u32, _keys: &AnimationKeys) {}

    /// `a=c`: compose frames of stored image `id`. Default: animation
    /// unsupported.
    fn compose_frames(&mut self, _id: u32, _keys: &CompositionKeys) -> Result<(), FrameError> {
        Err(FrameError {
            code: ErrorCode::Enotsup,
            detail: "animation".into(),
        })
    }
}

/// The in-flight chunked upload. kitty has exactly one loading slot: while
/// it is busy, every direct transmit is a continuation of it, whatever its
/// own keys say (WIRE-9).
#[derive(Debug)]
struct Upload {
    /// Header from the first chunk; it governs the whole upload. Only
    /// `q=` is taken from later chunks (WIRE-8).
    head: Header,
    /// Decoded payload so far. Chunks are decoded on arrival (WIRE-4), so
    /// caps count decoded bytes, not base64 (WIRE-11).
    data: Vec<u8>,
    /// base64 state carried between chunks.
    b64: b64::Decoder,
    /// Most decoded bytes this upload may hold: kitty's buffer capacity
    /// (`s*v*bpp` plus slack, or the image quota for PNG). Past it, kitty
    /// replies `EFBIG:Too much data` and drops the upload.
    limit: u64,
    /// `a=q,m=1`: kitty consumes exactly one more chunk and drops the
    /// query silently (WIRE-10).
    query: bool,
    /// When the latest chunk arrived; drives [`PENDING_UPLOAD_TTL`].
    last_chunk: Instant,
}

impl Upload {
    /// True iff `header` (an `a=p` or animation command) names the image
    /// this upload is loading.
    fn targets(&self, header: &Header) -> bool {
        if self.query {
            return false;
        }
        if header.image_id != 0 {
            header.image_id == self.head.image_id
        } else {
            header.image_number != 0 && header.image_number == self.head.image_number
        }
    }
}

/// Top-level Kitty dispatcher.
#[derive(Debug)]
pub struct KittyHandler {
    /// The one in-flight chunked upload, if any.
    loading: Option<Upload>,
    /// Extra ceiling on an upload's decoded bytes, on top of the
    /// format/quota limit (`usize::MAX` by default; tests lower it).
    pending_cap: usize,
    /// Set while finishing an upload whose last chunk was an `a=q` with
    /// `i=`: kitty replies under the query's id.
    reply_id_override: Option<u32>,
    /// Set while finishing a chunked `a=f` whose last chunk said `a=f`:
    /// the header its reply echoes (#47).
    frame_reply: Option<Header>,
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
            loading: None,
            pending_cap: usize::MAX,
            reply_id_override: None,
            frame_reply: None,
        }
    }

    /// Cap an upload's decoded bytes below the format/quota limit.
    pub fn set_pending_cap(&mut self, cap: usize) {
        self.pending_cap = cap;
    }

    /// Bytes held by the in-flight upload. Counts capacity, the memory
    /// actually allocated.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.loading.as_ref().map_or(0, |u| u.data.capacity())
    }

    /// Number of in-flight chunked uploads (0 or 1, as in kitty).
    #[must_use]
    pub fn pending_uploads(&self) -> usize {
        usize::from(self.loading.is_some())
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
                // display the payload. Apps probe kitty-graphics support
                // this way at startup (yazi, helix, btop, ...), so the
                // registry is not consulted.
                self.handle_query(header, body, sink);
            }
            Action::Frame => {
                self.handle_frame(header, body, sink);
            }
            Action::Animate | Action::Compose => {
                // The image still loading counts as present (kitty doesn't
                // answer `a=a` on it).
                if self.loading.as_ref().is_some_and(|u| u.targets(&header)) {
                    return;
                }
                if header.action == Action::Animate {
                    handle_animate(&header, sink);
                } else {
                    handle_compose(&header, sink);
                }
            }
        }
    }

    /// `a=f`: frame data. It travels like a transmit (chunks, mediums,
    /// compression, formats; #44's loading slot) and is finished by
    /// [`finalize_frame`]. A first chunk naming an image that isn't stored
    /// gets kitty's ENOENT and starts no upload; its continuation chunks,
    /// which carry no id, then fall on the floor (as in kitty). While an
    /// upload is loading, an `a=f` chunk continues it like any direct
    /// transmit (WIRE-9).
    fn handle_frame<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        let continues =
            self.loading.is_some() && matches!(header.transmission, Transmission::Direct);
        if !continues && resolve_image(&header, sink, "Animation").is_none() {
            return;
        }
        self.handle_transmit(header, body, sink);
    }

    #[allow(clippy::needless_pass_by_value)]
    fn handle_query<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // kitty drops a query without `i=` (anonymous or `I=`-only) before
        // any of its loading logic: it doesn't append to, finish, abort or
        // EILSEQ an upload in progress, and gets no reply (WIRE-7/9/10).
        if header.image_id == 0 {
            return;
        }
        if !matches!(header.transmission, Transmission::Direct) {
            self.abort_loading("file query");
            self.query_medium(&header, body, sink);
            return;
        }
        // During an upload a direct query is, to kitty, more of that
        // upload (WIRE-9/10, measured on 0.49.2):
        if let Some(up) = &self.loading {
            if up.query && header.more {
                // A second chunked query while one is loading: EILSEQ,
                // under the new query's id.
                self.loading = None;
                reply_error_if_verbose(
                    &header,
                    sink,
                    ErrorCode::Eilseq,
                    "More payload loading refers to non-existent image",
                );
            } else if up.query {
                // The chunk after `a=q,m=1` ends that query, silently.
                self.loading = None;
            } else if header.more {
                // A chunked query can't continue a transmit: kitty
                // replies EILSEQ for the upload and drops it.
                let head = up.head.clone();
                self.loading = None;
                reply_error_if_verbose(
                    &head,
                    sink,
                    ErrorCode::Eilseq,
                    "More payload loading refers to non-existent image",
                );
            } else {
                self.continue_upload(&header, body, sink);
            }
            return;
        }
        let quota = sink.max_image_bytes();
        if header.more {
            // WIRE-10: a chunked query is checked like the first chunk of
            // a transmit, then consumes the next chunk and ends without a
            // reply, as in kitty. Nothing is stored.
            if let Err(e) = self.start_upload(&header, body, quota, true)
                && !e.silent
            {
                reply_error_if_verbose(&header, sink, e.code, &e.detail);
            }
            return;
        }
        // Decode and validate. Same pipeline as `finalize_transmit`
        // but we discard the result (Query must not store/display). An
        // empty body is not special: like kitty it fails the dimension
        // or data checks (`EINVAL:Zero width/height not allowed`).
        if let Err(e) = check_body_fits(&header, body, quota) {
            reply_error_if_verbose(&header, sink, e.code, &e.detail);
            return;
        }
        let decoded = b64::decode_all(body);
        match decode_payload(&header, decoded, quota) {
            Ok(_) => reply_ok_if_verbose(&header, sink),
            Err(e) if !e.silent => reply_error_if_verbose(&header, sink, e.code, &e.detail),
            Err(_) => {}
        }
    }

    /// WIRE-1 / WIRE-2: `a=q` with `t=f` / `t=t` / `t=s`, as kitty 0.49.2
    /// answers it.
    ///
    /// The format and dimensions are checked first, before the file is
    /// touched (`EINVAL:Zero width/height not allowed`, which says nothing
    /// about the file). Then the file goes through the hardened
    /// [`medium::read_image_file`] path, `t=t` deletion rule included, or
    /// for `t=s` [`medium::read_shm_object`], which unlinks the object
    /// (`kitten icat --detect-support` finds `memory` this way). Every
    /// failure from there on, decode errors too, is the uniform
    /// `EBADF:Failed to read image file` (SEC-4). Nothing is stored.
    #[allow(clippy::unused_self)]
    fn query_medium<S: KittySink>(&mut self, header: &Header, body: &[u8], sink: &mut S) {
        match header.transmission {
            Transmission::File | Transmission::TempFile | Transmission::Shared => {
                if let Err(e) = check_medium_header(header) {
                    if !e.silent {
                        reply_error_if_verbose(header, sink, e.code, &e.detail);
                    }
                    return;
                }
                let quota = sink.max_image_bytes();
                let result = read_medium(header, body, quota).and_then(|raw| {
                    decode_payload(header, raw, quota).map_err(|e| FileError(e.detail))
                });
                match result {
                    Ok(_) => reply_ok_if_verbose(header, sink),
                    Err(e) => reply_file_error(header, sink, &e),
                }
            }
            Transmission::Direct => unreachable!("direct queries handled by the caller"),
        }
    }

    fn handle_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // M6: Non-direct mediums (`t=f`, `t=t`, `t=s`) carry a
        // base64-encoded PATH (or shm name) in the body rather than the
        // image data itself. They are inherently single-shot — the file
        // holds the whole payload — so they never join the chunked
        // reassembly below; like kitty, one arriving mid-upload drops
        // that upload. Acquire the bytes from the medium, then run them
        // through the SAME compression/format decode-and-place tail as
        // direct transmission via `finalize_decoded`.
        if !matches!(header.transmission, Transmission::Direct) {
            self.abort_loading("file transmit");
            self.handle_medium_transmit(header, body, sink);
            return;
        }

        // While an upload is loading, kitty treats every direct transmit
        // as its continuation, whatever its own `i=`/keys say: real
        // clients (yazi, `kitten icat`) send continuations as bare
        // `m=1`/`m=0`, and a stray command from another program is
        // appended too (WIRE-9). The upload's own first header governs;
        // the 60 s idle expiry keeps an abandoned upload from capturing
        // later images forever.
        if let Some(up) = &self.loading {
            if up.query {
                self.loading = None;
            } else {
                self.continue_upload(&header, body, sink);
            }
            return;
        }

        let quota = sink.max_image_bytes();
        if header.more {
            // First chunk of a multi-chunk upload. kitty checks the format
            // and dimensions now, replies at once if they're wrong, and
            // then doesn't start loading (so the rest of the chunks fall
            // on the floor).
            if let Err(e) = self.start_upload(&header, body, quota, false)
                && !e.silent
            {
                reply_payload_error(&header, sink, e.code, &e.detail);
            }
            return;
        }

        // Single-chunk transmission.
        if let Err(e) = check_body_fits(&header, body, quota) {
            reply_payload_error(&header, sink, e.code, &e.detail);
            return;
        }
        let decoded = b64::decode_all(body);
        self.finalize_decoded(header, decoded, PayloadSource::Direct, sink);
    }

    /// Begin loading a chunked upload with its first chunk. Fails (and
    /// loads nothing) when kitty would reject the first chunk: bad format
    /// or dimensions, or already more data than the upload may hold.
    fn start_upload(
        &mut self,
        header: &Header,
        body: &[u8],
        quota: u64,
        query: bool,
    ) -> Result<(), PayloadError> {
        let limit = upload_limit(header, quota)?.min(self.pending_cap as u64);
        let mut up = Upload {
            head: header.clone(),
            data: Vec::new(),
            b64: b64::Decoder::default(),
            limit,
            query,
            last_chunk: Instant::now(),
        };
        // Bound the decoded size from the base64 length before decoding,
        // so one huge APC can't make us allocate it first.
        if b64::decoded_len_bound(body, 0) > up.limit {
            return Err(DecodeError::TooMuchData.into());
        }
        up.b64.feed(body, &mut up.data);
        if up.data.len() as u64 > up.limit {
            return Err(DecodeError::TooMuchData.into());
        }
        self.loading = Some(up);
        Ok(())
    }

    /// Append a continuation chunk to the loading upload; on its last
    /// chunk, decode and dispatch the image.
    fn continue_upload<S: KittySink>(&mut self, header: &Header, body: &[u8], sink: &mut S) {
        let Some(up) = self.loading.as_mut() else {
            return;
        };
        // WIRE-8: a `q=` on a continuation chunk applies to the whole
        // upload, as in kitty. `q=0` can't be told apart from no `q=`,
        // so it never lowers an earlier chunk's quietness.
        if header.quiet != Quiet::Verbose {
            up.head.quiet = header.quiet;
        }
        up.last_chunk = Instant::now();
        let fits =
            up.data.len() as u64 + b64::decoded_len_bound(body, up.b64.carried()) <= up.limit;
        if fits {
            up.b64.feed(body, &mut up.data);
        }
        if !fits || up.data.len() as u64 > up.limit {
            // kitty replies at once and drops the upload; the rest of its
            // chunks then fall on the floor.
            let head = up.head.clone();
            self.loading = None;
            reply_payload_error(&head, sink, ErrorCode::Efbig, "Too much data");
            return;
        }
        if header.more {
            return;
        }
        let Some(mut up) = self.loading.take() else {
            return;
        };
        up.b64.finish(&mut up.data);
        if header.action == Action::Query && header.image_id != 0 {
            self.reply_id_override = Some(header.image_id);
        }
        if up.head.action == Action::Frame && header.action == Action::Frame {
            // kitty answers a chunked frame whose last chunk says `a=f`
            // with that chunk's ids (usually none: no reply). A bare
            // `m=0` last chunk is answered with the first's (#47).
            let mut r = up.head.clone();
            r.image_id = header.image_id;
            r.image_number = header.image_number;
            r.placement_id = header.placement_id;
            self.frame_reply = Some(r);
        }
        self.finalize_decoded(up.head, up.data, PayloadSource::Direct, sink);
        self.reply_id_override = None;
    }

    /// Drop the loading upload, if any (a delete or a file transmit
    /// arrived, kitty's abort points). Silent.
    fn abort_loading(&mut self, why: &str) {
        if let Some(up) = self.loading.take() {
            tracing::debug!(
                image_id = up.head.image_id,
                image_number = up.head.image_number,
                why,
                "kitty: chunked upload aborted"
            );
        }
    }

    /// Drop an upload that has had no chunk for [`PENDING_UPLOAD_TTL`].
    /// Silent: the client that abandoned it isn't listening. Runs at the
    /// start of every graphics command; the host also calls it after each
    /// batch of PTY input, so an abandoned upload doesn't stay resident
    /// when no further graphics command comes.
    pub fn expire_pending(&mut self, now: Instant) {
        if self
            .loading
            .as_ref()
            .is_some_and(|u| now.saturating_duration_since(u.last_chunk) >= PENDING_UPLOAD_TTL)
        {
            self.abort_loading("expired");
        }
    }

    /// M6: non-direct transmission mediums — `t=f` (file), `t=t` (temp
    /// file), `t=s` (POSIX shared memory).
    ///
    /// For `t=f`/`t=t` the body is a base64-encoded filesystem PATH; we
    /// read its bytes (honoring `O=` offset and `S=` size) and feed them
    /// into the same decode tail as direct transmission. The path policy
    /// (regular files only, no `/proc`/`/sys`/`/dev`, non-blocking open,
    /// bounded read, `t=t` deletion rules) lives in [`medium`]. For `t=s`
    /// the body is a base64 POSIX shm name (`/name`); the object is read
    /// the same way and then unlinked ([`medium::read_shm_object`]). Every
    /// failure, up to and including decoding the bytes read, replies
    /// `EBADF:Failed to read image file` (SEC-4).
    #[allow(clippy::needless_pass_by_value)]
    fn handle_medium_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        match header.transmission {
            Transmission::Direct => unreachable!("direct handled by caller"),
            Transmission::File | Transmission::TempFile | Transmission::Shared => {
                // kitty checks the format and dimensions before it touches
                // the file, and replies about the command, not the file.
                if let Err(e) = check_medium_header(&header) {
                    if !e.silent {
                        reply_payload_error(&header, sink, e.code, &e.detail);
                    }
                    return;
                }
                let raw = match read_medium(&header, body, sink.max_image_bytes()) {
                    Ok(bytes) => bytes,
                    Err(e) if header.action == Action::Frame => {
                        let target = frame_target_for(&header, sink);
                        reply_frame_file_error(&header, target, sink, &e);
                        return;
                    }
                    Err(e) => {
                        reply_file_error(&header, sink, &e);
                        return;
                    }
                };
                if header.action == Action::Frame {
                    finalize_frame(&header, &header, raw, PayloadSource::File, sink);
                    return;
                }
                self.finalize_decoded(header, raw, PayloadSource::File, sink);
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
        // kitty answers an upload that an `a=q` chunk finished under the
        // query's id (the image is still stored under its own).
        let reply_as = self.reply_id_override.take();
        if header.action == Action::Frame {
            let reply = self.frame_reply.take().unwrap_or_else(|| header.clone());
            finalize_frame(&header, &reply, decoded, source, sink);
            return;
        }
        let reply_head = match reply_as {
            Some(id) => Header {
                image_id: id,
                ..header.clone()
            },
            None => header.clone(),
        };
        let img = match decode_payload(&header, decoded, sink.max_image_bytes()) {
            Ok(d) => d,
            Err(e) => {
                match source {
                    // SEC-4: a decode error would reveal what the file
                    // holds (its size, whether it is a PNG).
                    PayloadSource::File => reply_file_error(&header, sink, &e.detail),
                    PayloadSource::Direct if e.silent => {}
                    PayloadSource::Direct => {
                        reply_transmit_error(&reply_head, sink, e.code, &e.detail);
                    }
                }
                return;
            }
        };

        // Hand off to the host. Capture dimensions BEFORE the move so
        // we can build the placement after the borrow ends.
        let (img_w, img_h) = (img.width, img.height);
        let Some(final_id) = sink.register_image(header.image_id, header.image_number, img) else {
            reply_transmit_error(&reply_head, sink, ErrorCode::Efbig, "registry full");
            return;
        };
        // §9: `N` is a bitmask; bit 0 is "transient".
        if header.usage_hint & 1 != 0 {
            sink.mark_transient(final_id);
        }

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
        // REL-4: `a=T` with `P=` is a relative placement, exactly like
        // `a=p` (kitty 0.49.2). It never moves the cursor, and a rejected
        // parent reference is replied to with its error, but the image
        // stays registered.
        if matches!(header.action, Action::TransmitAndPlace)
            && !header.unicode_placeholder
            && header.parent_image != 0
        {
            place_relative_from_header(&header, final_id, img_w, img_h, sink);
            return;
        }
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
        reply_ok_if_verbose_with_id(&reply_head, sink, reply_as.unwrap_or(final_id));
    }

    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn handle_place<S: KittySink>(&mut self, header: Header, sink: &mut S) {
        // PLC-1 / PLC-3: resolve `i=`, or the newest image with `I=`, and
        // refuse an image that isn't stored (kitty: ENOENT, sent even with
        // `q=1`) before anything is placed or the cursor moves. From here
        // on `header.image_id` is the resolved id, so an `I=` placement
        // replies `i=<id>,I=<n>;OK` like kitty.
        // A put for the image still loading: kitty replies this ENOENT
        // and leaves the upload running.
        if let Some(up) = self.loading.as_ref().filter(|u| u.targets(&header)) {
            let mut reply = header.clone();
            reply.image_id = if up.head.image_id != 0 {
                up.head.image_id
            } else {
                sink.next_image_id()
            };
            reply_error_if_verbose(
                &reply,
                sink,
                ErrorCode::Enoent,
                &format!(
                    "Put command refers to image with id: {} that could not load its data",
                    header.image_id
                ),
            );
            return;
        }
        let Some(id) = resolve_image(&header, sink, "Put") else {
            return;
        };
        let mut header = header;
        header.image_id = id;
        // REL-7: a virtual placement can't be relative (spec). kitty
        // rejects `a=p,U=1,P=…` with EINVAL. (For `a=T,U=1,P=…` kitty
        // replies OK and makes the virtual placement, so that path ignores
        // `P`/`Q`.)
        if header.unicode_placeholder && header.parent_image != 0 {
            reply_error_if_verbose(
                &header,
                sink,
                ErrorCode::Einval,
                "virtual placement cannot refer to a parent",
            );
            return;
        }
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
        // M13: relative placement, taken whenever `P=` (parent image) is
        // set. kitty 0.49.2 accepts `P` without `Q` (any placement of the
        // parent image) and ignores `Q` without `P` (an ordinary placement
        // at the cursor). Relative placements never move the cursor.
        if header.parent_image != 0 {
            let (img_w, img_h) = sink.image_dimensions(header.image_id).unwrap_or((0, 0));
            place_relative_from_header(&header, header.image_id, img_w, img_h, sink);
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
    ///
    /// DEL-3: any delete aborts a chunked upload in progress ("the partial
    /// upload must be aborted"), whatever it deletes. Like kitty, the
    /// remaining chunks then get no reply: a bare continuation has no
    /// dimensions, so it fails the first-chunk check, anonymously.
    #[allow(clippy::needless_pass_by_value)]
    fn handle_delete<S: KittySink>(&mut self, header: Header, sink: &mut S) {
        self.abort_loading("delete");
        sink.delete_image(header.delete, &header);
    }
}

/// `a=a`: animation control. kitty never answers it, except ENOENT for
/// an image that isn't stored (WIRE-3, WIRE-12), so `kitten icat`'s
/// control commands (sent without `q=`) put nothing in the shell.
fn handle_animate<S: KittySink>(header: &Header, sink: &mut S) {
    if let Some(id) = resolve_image(header, sink, "Animation") {
        sink.control_animation(id, &header.animation());
    }
}

/// `a=c`: frame composition. Replies OK or kitty's frame error, keyed by
/// the client's own `i=` / `I=`.
fn handle_compose<S: KittySink>(header: &Header, sink: &mut S) {
    let Some(id) = resolve_image(header, sink, "Animation") else {
        return;
    };
    match sink.compose_frames(id, &header.composition()) {
        Ok(()) => reply_ok_if_verbose(header, sink),
        Err(e) => reply_error_if_verbose(header, sink, e.code, &e.detail),
    }
}

/// The frame an `a=f` with `header` targets, for its reply's `r=`.
fn frame_target_for<S: KittySink>(header: &Header, sink: &S) -> u32 {
    let id = stored_image(header, sink).unwrap_or(0);
    frame_target(sink.frame_count(id), header.frame().edit_frame)
}

/// The stored image `header` names (`i=`, or the newest `I=`), if any.
fn stored_image<S: KittySink>(header: &Header, sink: &S) -> Option<u32> {
    let id = if header.image_id != 0 {
        Some(header.image_id)
    } else if header.image_number != 0 {
        sink.image_id_for_number(header.image_number)
    } else {
        None
    };
    id.filter(|&id| sink.image_exists(id))
}

/// Decode a complete `a=f` payload and add or edit the frame. `header`
/// holds the frame keys (the first chunk's); `reply` the ids the answer
/// echoes. Replies are tagged `r=<frame>`, errors included, in kitty's
/// order: decode errors, then frame size, then base frame (WIRE-3).
fn finalize_frame<S: KittySink>(
    header: &Header,
    reply: &Header,
    decoded: Vec<u8>,
    source: PayloadSource,
    sink: &mut S,
) {
    // The image may have gone while the chunks were arriving.
    let Some(id) = stored_image(header, sink) else {
        resolve_image(reply, sink, "Animation");
        return;
    };
    let keys = header.frame();
    let target = frame_target(sink.frame_count(id), keys.edit_frame);
    let data = match decode_payload(header, decoded, sink.max_image_bytes()) {
        Ok(d) => d,
        Err(e) => {
            match source {
                PayloadSource::File => reply_frame_file_error(reply, target, sink, &e.detail),
                PayloadSource::Direct => {
                    reply_frame_error_if_verbose(reply, sink, target, e.code, &e.detail);
                }
            }
            return;
        }
    };
    match sink.add_frame(id, &keys, data) {
        Ok(frame) => {
            if reply.quiet == Quiet::Verbose && client_wants_reply(reply) {
                sink.queue_reply(&encode_frame_ok(
                    reply.image_id,
                    reply.image_number,
                    reply.placement_id,
                    frame,
                ));
            }
        }
        Err(e) => reply_frame_error_if_verbose(reply, sink, target, e.code, &e.detail),
    }
}

fn reply_frame_error_if_verbose<S: KittySink>(
    reply: &Header,
    sink: &mut S,
    frame: u32,
    code: ErrorCode,
    detail: &str,
) {
    if reply.quiet == Quiet::Silent || !client_wants_reply(reply) {
        return;
    }
    sink.queue_reply(&encode_frame_error(
        reply.image_id,
        reply.image_number,
        reply.placement_id,
        frame,
        code,
        detail,
    ));
}

/// An error found before a transmit's or frame's payload is decoded
/// (format, dimensions, size). An `a=f` is answered tagged with the frame
/// it targets, as its decode errors are (#47); anything else as a
/// transmit.
fn reply_payload_error<S: KittySink>(header: &Header, sink: &mut S, code: ErrorCode, detail: &str) {
    if header.action == Action::Frame {
        let target = frame_target_for(header, sink);
        reply_frame_error_if_verbose(header, sink, target, code, detail);
    } else {
        reply_transmit_error(header, sink, code, detail);
    }
}

/// SEC-4 for frames: the uniform file error, tagged with the frame.
fn reply_frame_file_error<S: KittySink>(
    reply: &Header,
    frame: u32,
    sink: &mut S,
    reason: &dyn std::fmt::Display,
) {
    tracing::debug!(image_id = reply.image_id, %reason, "kitty: frame file medium failed");
    reply_frame_error_if_verbose(
        reply,
        sink,
        frame,
        ErrorCode::Ebadf,
        medium::FILE_READ_ERROR,
    );
}

/// Errors from [`KittyHandler::process`] before any sink calls happen.
/// Errors detected later are turned into protocol replies via the sink.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum HandlerError {
    /// Malformed header.
    #[error("malformed kitty header: {0}")]
    BadHeader(super::header::KittyHeaderError),
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
    /// Send no reply. kitty answers "Image too large, width or height
    /// greater than 10000" under a stale id, which in practice is nearly
    /// always none at all.
    silent: bool,
}

impl From<DecodeError> for PayloadError {
    fn from(e: DecodeError) -> Self {
        Self {
            code: e.code(),
            detail: e.to_string(),
            silent: matches!(e, DecodeError::DimsTooLarge),
        }
    }
}

/// Most decoded bytes a chunked upload with first header `header` may
/// hold, as kitty sizes its load buffer (WIRE-11): `s*v*bpp` plus
/// [`RAW_DATA_SLACK`] (or [`COMPRESSED_DATA_SLACK`] for `o=z`), capped at
/// the quota plus that slack; the quota for PNG. Also runs kitty's
/// first-chunk checks on the format and dimensions.
fn upload_limit(header: &Header, quota: u64) -> Result<u64, PayloadError> {
    let slack = match header.compression {
        Compression::None => RAW_DATA_SLACK as u64,
        Compression::Zlib => COMPRESSED_DATA_SLACK,
    };
    Ok(
        match raw_len(header.format, header.source_width, header.source_height)? {
            Some(n) => n.min(quota) + slack,
            None => quota,
        },
    )
}

/// A single-chunk payload whose base64 alone can't fit the upload limit
/// is refused before it's decoded (`EFBIG:Too much data`), the way a
/// chunked one is. Bad format or dimensions are left to the decoder.
fn check_body_fits(header: &Header, body: &[u8], quota: u64) -> Result<(), PayloadError> {
    match upload_limit(header, quota) {
        Ok(limit) if b64::decoded_len_bound(body, 0) > limit => {
            Err(DecodeError::TooMuchData.into())
        }
        _ => Ok(()),
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
                silent: false,
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
                silent: false,
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

/// kitty's checks on a file-medium command before the file is opened:
/// a known `f=`, and for raw pixels both dimensions within 0 < d <= 10000.
fn check_medium_header(header: &Header) -> Result<(), PayloadError> {
    raw_len(header.format, header.source_width, header.source_height)?;
    Ok(())
}

/// Read the file a `t=f` / `t=t` command names, or the shared-memory
/// object a `t=s` command names. `body` is the base64 path or shm name.
/// See [`medium::read_image_file`] and [`medium::read_shm_object`] for the
/// policy.
fn read_medium(header: &Header, body: &[u8], quota: u64) -> Result<Vec<u8>, FileError> {
    // Undecodable base64 becomes an empty name, which fails like any other.
    let name = b64::decode_all(body);
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
    let offset = u64::from(header.offset);
    if header.transmission == Transmission::Shared {
        return medium::read_shm_object(&ShmRequest {
            name: &name,
            offset,
            size,
            max: quota,
        });
    }
    medium::read_image_file(&FileRequest {
        path: &path_from_bytes(name),
        offset,
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
    // it (and moved over by the cursor) is kitty's, see
    // `Placement::reserved_cells`: offsets only widen a natural-size box,
    // and a single given side derives the other from its own span.
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
    p.reserve(shown, (cw, ch));
    p
}

/// Create the relative placement (`P=`/`Q=`/`H=`/`V=`) a header asks for
/// and reply: OK, or the error code kitty 0.49.2 uses for the rejection
/// (REL-7). Shared by `a=p` and `a=T` (REL-4). Never moves the cursor.
fn place_relative_from_header<S: KittySink>(
    header: &Header,
    image_id: u32,
    img_w: u32,
    img_h: u32,
    sink: &mut S,
) {
    let cell_px = sink.cell_pixel_size();
    let mut placement =
        default_placement_from_header(header, image_id, img_w, img_h, cell_px.0, cell_px.1);
    placement.parent = Some((header.parent_image, header.parent_placement));
    placement.rel_offset = (header.h_offset, header.v_offset);
    match sink.place_relative(placement) {
        Ok(()) => reply_ok_if_verbose_with_id(header, sink, image_id),
        Err(e) => {
            let (code, msg) = match e {
                RelativeError::NoParent => {
                    (ErrorCode::Enoparent, "parent placement does not exist")
                }
                RelativeError::Cycle => (ErrorCode::Ecycle, "parent reference creates a cycle"),
                RelativeError::TooDeep => (ErrorCode::Etoodeep, "parent chain too deep"),
                RelativeError::SelfParent => (
                    ErrorCode::Einval,
                    "placement refers to itself as its own parent",
                ),
            };
            reply_error_if_verbose(header, sink, code, msg);
        }
    }
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
        assign_next: u32,
        /// `I=` → newest id, like `Term::image_number_to_id`.
        numbers: std::collections::HashMap<u32, u32>,
        deleted: Vec<u32>,
        cursor_advances: usize,
        /// `a=f` frames received: (image id, keys, decoded size).
        frames: Vec<(u32, FrameKeys, (u32, u32))>,
        controls: Vec<(u32, AnimationKeys)>,
        composes: Vec<(u32, CompositionKeys)>,
    }

    impl MockSink {
        /// The budget argument is historical: the handler no longer asks
        /// the sink for a byte budget (WIRE-11).
        fn with_budget(_budget: u64) -> Self {
            Self {
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
        fn frame_count(&self, id: u32) -> u32 {
            1 + self
                .frames
                .iter()
                .filter(|f| f.0 == id && f.1.edit_frame == 0)
                .count() as u32
        }
        fn add_frame(
            &mut self,
            id: u32,
            keys: &FrameKeys,
            data: ImageData,
        ) -> Result<u32, FrameError> {
            let n = frame_target(self.frame_count(id), keys.edit_frame);
            if keys.base_frame > self.frame_count(id) {
                return Err(FrameError {
                    code: ErrorCode::Einval,
                    detail: format!("No frame with number: {} found", keys.base_frame),
                });
            }
            self.frames.push((id, *keys, (data.width, data.height)));
            Ok(n)
        }
        fn control_animation(&mut self, id: u32, keys: &AnimationKeys) {
            self.controls.push((id, *keys));
        }
        fn compose_frames(&mut self, id: u32, keys: &CompositionKeys) -> Result<(), FrameError> {
            self.composes.push((id, *keys));
            if keys.src_frame == 0 {
                return Err(FrameError {
                    code: ErrorCode::Enoent,
                    detail: format!("No source frame number 0 exists in image id: {id}\n"),
                });
            }
            Ok(())
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

    /// WIRE-11: `S=` on a direct transmission is a hint, not a budget
    /// gate: the reply is about the missing data, as in kitty.
    #[test]
    fn declared_size_is_not_a_budget_gate() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(16);
        h.process(b"Ga=t,f=32,s=64,v=64,i=1,S=16384", b"", &mut sink)
            .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert_eq!(
            sink.replies,
            vec![b"\x1b_Gi=1;ENODATA:Insufficient image data: 0 < 16384\x1b\\".to_vec()]
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

    /// §9: `a=f` / `a=a` / `a=c` on a stored image reach the sink with
    /// their keys under the animation meaning, and get kitty's replies:
    /// `a=f` tagged `r=<frame>`, `a=a` never, `a=c` OK or its error. A
    /// bare `a=f,m=1` names no image and is dropped.
    #[test]
    fn animation_commands_reach_the_sink() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=9,q=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        for header in [
            &b"Ga=f,i=9,f=32,s=1,v=1,x=3,y=4,c=1,z=-7,X=1,Y=255"[..],
            b"Ga=f,i=9,f=32,s=1,v=1,c=5",
            b"Ga=f,i=9,f=32,s=1,v=1,q=1",
            b"Ga=a,i=9,s=3,v=2,r=1,z=150,c=2",
            b"Ga=c,i=9,r=1,c=2,x=1,y=2,w=3,h=4,X=5,Y=6,C=1",
            b"Ga=c,i=9",
            b"Ga=f,m=1",
        ] {
            h.process(header, b64_red_pixel().as_bytes(), &mut sink)
                .unwrap();
        }
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=9,r=2;OK\x1b\\\
             \x1b_Gi=9,r=3;EINVAL:No frame with number: 5 found\x1b\\\
             \x1b_Gi=9;OK\x1b\\\
             \x1b_Gi=9;ENOENT:No source frame number 0 exists in image id: 9\n\x1b\\"
        );
        let (id, keys, size) = sink.frames[0];
        assert_eq!((id, size), (9, (1, 1)));
        assert_eq!(
            (
                keys.x,
                keys.y,
                keys.base_frame,
                keys.gap_ms,
                keys.blend_mode,
                keys.background
            ),
            (3, 4, 1, -7, 1, 255)
        );
        assert_eq!(sink.frames.len(), 2, "q=1 frame stored, its OK suppressed");
        let (_, a) = sink.controls[0];
        assert_eq!(
            (a.state, a.loops, a.frame, a.gap_ms, a.current_frame),
            (3, 2, 1, 150, 2)
        );
        let (_, c) = sink.composes[0];
        assert_eq!(
            (
                c.src_frame,
                c.dest_frame,
                c.dest_x,
                c.dest_y,
                c.width,
                c.height,
                c.src_x,
                c.src_y,
                c.blend_mode
            ),
            (1, 2, 1, 2, 3, 4, 5, 6, 1)
        );
        assert_eq!(sink.registered.len(), 1);
        assert!(sink.placements.is_empty());
        assert_eq!(h.pending_uploads(), 0);
    }

    /// `a=f` decode errors carry `r=` too, and come before frame checks.
    #[test]
    fn frame_decode_errors_are_tagged_with_the_frame() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=9,q=2",
            b64_red_pixel().as_bytes(),
            &mut sink,
        )
        .unwrap();
        h.process(b"Ga=f,i=9,f=32,s=4,v=2,c=5", b"AAAA", &mut sink)
            .unwrap();
        h.process(b"Ga=f,i=9,f=32", b"AAAA", &mut sink).unwrap();
        h.process(b"Ga=f,i=9,f=32,s=1,v=1,o=z", b"AAAA", &mut sink)
            .unwrap();
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=9,r=2;ENODATA:Insufficient image data: 3 < 32\x1b\\\
             \x1b_Gi=9,r=2;EINVAL:Zero width/height not allowed\x1b\\\
             \x1b_Gi=9,r=2;EINVAL:Failed to inflate image data with error: Z_DATA_ERROR\x1b\\"
        );
        assert!(sink.frames.is_empty());
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

    fn run_seq(cmds: &[(&str, &str)]) -> (KittyHandler, MockSink) {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        for (head, body) in cmds {
            let _ = h.process(head.as_bytes(), body.as_bytes(), &mut sink);
        }
        (h, sink)
    }

    /// WIRE-9 (kitty 0.49.2): a bare final chunk completes a leftover
    /// upload.
    #[test]
    fn wire9_bare_final_chunk_completes_leftover() {
        let (h, sink) = run_seq(&[
            ("Ga=t,i=110,f=32,s=2,v=2,m=1", "AQIDBA=="),
            ("Gm=0", "/wAA/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=110;ENODATA:Insufficient image data: 8 < 16\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
    }

    /// WIRE-9 (kitty 0.49.2): while an upload is loading, a direct
    /// transmit with another `i=` is appended to it and answered as the
    /// loading image. One slot keeps memory bounded by one upload.
    #[test]
    fn wire9_new_transmit_during_load_is_a_continuation() {
        let (h, sink) = run_seq(&[
            ("Ga=t,i=112,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=t,i=113,f=32,s=1,v=1", "/wAA/w=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=112;OK\x1b\\");
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].0, 112);
        assert_eq!(h.pending_uploads(), 0);
        // A new chunked upload during a load joins it too.
        let (_, sink) = run_seq(&[
            ("Ga=t,i=115,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=t,i=116,f=32,s=1,v=1,m=1", "/wAA"),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=115;OK\x1b\\");
    }

    /// WIRE-9 (kitty 0.49.2): a stale anonymous upload swallows an
    /// anonymous `a=T,I=…`, so no reply comes. The 60 s expiry is what
    /// keeps an abandoned upload from doing this forever.
    #[test]
    fn wire9_stale_anonymous_upload_swallows_next_transmit() {
        let (h, sink) = run_seq(&[
            ("Ga=T,f=32,s=1,v=1,C=1,m=1", "/wAA"),
            ("Ga=T,I=965,f=32,s=1,v=1,C=1", "/wAA/w=="),
        ]);
        assert!(sink.replies.is_empty(), "{}", joined(&sink));
        assert_eq!(h.pending_uploads(), 0);
    }

    /// kitty checks the format and dimensions on the first chunk, replies
    /// at once, and doesn't start loading; the rest of the chunks fall on
    /// the floor silently.
    #[test]
    fn first_chunk_is_validated_up_front() {
        let px = b64_red_pixel();
        let (h, sink) = run_seq(&[
            ("Ga=t,i=180,f=32,m=1", "/wAA"),
            ("Gm=0", "/w=="),
            ("Ga=t,i=183,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=180;EINVAL:Zero width/height not allowed\x1b\\\x1b_Gi=183;OK\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
        let (_, sink) = run_seq(&[("Ga=t,i=181,f=8,s=1,v=1,m=1", "/wAA"), ("Gm=0", "/w==")]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=181;EINVAL:Unknown image format: 8\x1b\\"
        );
        // Over 10000 px: kitty answers under a stale id, so in practice
        // not at all; we stay silent, single-shot or chunked, and nothing
        // loads.
        let (h, sink) = run_seq(&[
            ("Ga=t,i=211,f=32,s=10001,v=10000", &px),
            ("Ga=t,i=217,f=32,s=20000,v=20000,m=1", "/wAA"),
            ("Ga=t,i=218,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=218;OK\x1b\\");
        assert_eq!(h.pending_uploads(), 0);
    }

    /// kitty: an upload finished by an `a=q` chunk is stored under its own
    /// id but answered under the query's.
    #[test]
    fn query_chunk_finishing_an_upload_replies_as_the_query() {
        let (_, sink) = run_seq(&[
            ("Ga=t,i=202,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,i=203,f=32,s=1,v=1", "/wAA/w=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=203;OK\x1b\\");
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].0, 202);
    }

    /// Review of #44, measured on kitty 0.49.2: a query without `i=` is
    /// dropped before any loading logic. During an upload it doesn't
    /// finish, EILSEQ or abort it; the upload carries on.
    #[test]
    fn query_without_id_never_touches_the_upload() {
        // Anonymous query mid-upload: silent, nothing stored, upload still
        // loading.
        let (h, sink) = run_seq(&[
            ("Ga=t,i=370,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,f=32,s=1,v=1", "/w=="),
        ]);
        assert!(sink.replies.is_empty(), "{}", joined(&sink));
        assert!(sink.registered.is_empty());
        assert_eq!(h.pending_uploads(), 1);
        // Chunked anonymous query: not appended, no EILSEQ.
        let (_, sink) = run_seq(&[
            ("Ga=t,i=372,f=32,s=2,v=1,m=1", "/wAA"),
            ("Ga=q,f=32,s=1,v=1,m=1", "/wAA"),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=372;ENODATA:Insufficient image data: 4 < 8\x1b\\"
        );
        // File-medium anonymous query: the load is not aborted.
        let (_, sink) = run_seq(&[
            ("Ga=t,i=373,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,t=f,f=32,s=1,v=1", "L25vbmV4aXN0ZW50"),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=373;OK\x1b\\");
        // `I=`-only query: the same.
        let (_, sink) = run_seq(&[
            ("Ga=t,i=324,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,I=9,f=32,s=1,v=1", "/w=="),
        ]);
        assert!(sink.replies.is_empty());
        assert!(sink.registered.is_empty());
    }

    /// kitty: a second chunked `a=q` while a chunked query is loading
    /// replies EILSEQ under the new query's id.
    #[test]
    fn second_chunked_query_replies_eilseq() {
        let (h, sink) = run_seq(&[
            ("Ga=q,i=321,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,i=322,f=32,s=1,v=1,m=1", "/wAA"),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=322;EILSEQ:More payload loading refers to non-existent image\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
    }

    /// Review of #44: a chunk's decoded size is bounded from its base64
    /// length before it is decoded, so a huge APC for a 1x1 image is
    /// refused without decoding it. (Tested via the bound: a 4 MiB body
    /// decodes to far more than 14 bytes.)
    #[test]
    fn oversized_chunk_is_refused_before_decoding() {
        let body = "A".repeat(4 << 20);
        assert!(b64::decoded_len_bound(body.as_bytes(), 0) > 14);
        let (h, sink) = run_seq(&[("Ga=t,i=380,f=32,s=1,v=1,m=1", &body)]);
        assert_eq!(joined(&sink), "\x1b_Gi=380;EFBIG:Too much data\x1b\\");
        assert_eq!(h.pending_bytes(), 0, "nothing was decoded or kept");
        // Continuation and single-chunk paths too.
        let (h, sink) = run_seq(&[("Ga=t,i=381,f=32,s=1,v=1,m=1", "/wAA"), ("Gm=1", &body)]);
        assert_eq!(joined(&sink), "\x1b_Gi=381;EFBIG:Too much data\x1b\\");
        assert_eq!(h.pending_uploads(), 0);
        let (_, sink) = run_seq(&[("Ga=t,i=382,f=32,s=1,v=1", &body)]);
        assert_eq!(joined(&sink), "\x1b_Gi=382;EFBIG:Too much data\x1b\\");
        // The exact limit still fits: 14 bytes for a 1x1 RGBA image.
        let fourteen = base64::engine::general_purpose::STANDARD.encode([7u8; 14]);
        let (_, sink) = run_seq(&[("Ga=t,i=383,f=32,s=1,v=1", &fourteen)]);
        assert_eq!(joined(&sink), "\x1b_Gi=383;OK\x1b\\");
    }

    /// WIRE-11: the cap counts decoded bytes: `s*v*bpp` plus kitty's
    /// 10-byte slack. Over it, the upload is dropped at once with
    /// `EFBIG:Too much data`; the next command starts fresh.
    #[test]
    fn wire11_overflow_drops_the_upload() {
        let z12 = "AAAAAAAAAAAAAAAA";
        let (h, sink) = run_seq(&[
            ("Ga=t,i=133,f=32,s=1,v=1,m=1", z12),
            ("Gm=1", z12),
            ("Ga=t,i=134,f=32,s=1,v=1", "/wAA/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=133;EFBIG:Too much data\x1b\\\x1b_Gi=134;OK\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
    }

    /// WIRE-11: `S=` on a direct transmission is only a hint.
    #[test]
    fn wire11_s_is_a_hint_for_direct() {
        let (_, sink) = run_seq(&[
            ("Ga=t,i=130,f=32,s=1,v=1,S=999999999", "/wAA/w=="),
            ("Ga=t,i=132,f=32,s=1,v=1,S=1", "/wAA/w=="),
            ("Ga=t,i=177,f=32,s=1,v=1,S=999999999,m=1", "/wAA"),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=130;OK\x1b\\\x1b_Gi=132;OK\x1b\\\x1b_Gi=177;OK\x1b\\"
        );
    }

    /// WIRE-11: a 4096x4096 RGBA upload (64 MiB decoded, 89 MiB base64)
    /// fits: the limit is `s*v*4 + 10` decoded bytes, where B1's 64 MiB
    /// base64 cap refused it.
    #[test]
    fn wire11_large_upload_limit_counts_decoded_bytes() {
        let h = Header {
            format: crate::kitty::header::Format::Rgba32,
            source_width: 4096,
            source_height: 4096,
            ..Header::default()
        };
        assert_eq!(upload_limit(&h, 320 << 20).unwrap(), (4096 * 4096 * 4) + 10);
        // A real (smaller) chunked upload end to end: 1024x512 RGBA.
        let raw = vec![0x5au8; 1024 * 512 * 4];
        let enc = base64::engine::general_purpose::STANDARD.encode(&raw);
        let mut handler = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let parts: Vec<&str> = enc
            .as_bytes()
            .chunks(4096)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect();
        for (n, part) in parts.iter().enumerate() {
            let more = u8::from(n + 1 < parts.len());
            let head = if n == 0 {
                format!("Ga=t,i=4096,f=32,s=1024,v=512,m={more}")
            } else {
                format!("Gm={more}")
            };
            handler
                .process(head.as_bytes(), part.as_bytes(), &mut sink)
                .unwrap();
            assert!(handler.pending_bytes() <= raw.len() * 2);
        }
        assert_eq!(joined(&sink), "\x1b_Gi=4096;OK\x1b\\");
        assert_eq!(sink.registered[0].1.pixels, raw);
    }

    /// WIRE-4: chunks padded independently, as kitty sends them.
    #[test]
    fn wire4_per_chunk_padding() {
        let (_, sink) = run_seq(&[
            ("Ga=t,i=101,f=32,s=2,v=1,m=1", "AQIDBA=="),
            ("Gm=0", "BQYHCA=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=101;OK\x1b\\");
        // toastty's leniency: a split mid-quantum (kitty: ENODATA 7 < 8).
        let (_, sink) = run_seq(&[
            ("Ga=t,i=103,f=32,s=2,v=1,m=1", "AQIDB"),
            ("Gm=0", "AUGBwg="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=103;OK\x1b\\");
    }

    /// WIRE-10 (kitty 0.49.2): a chunked `a=q` is checked up front, then
    /// consumes the next chunk and ends without a reply; nothing is
    /// stored and the following command is independent.
    #[test]
    fn wire10_chunked_query() {
        let px = b64_red_pixel();
        let (h, sink) = run_seq(&[
            ("Ga=q,i=120,f=32,s=1,v=1,m=1", "/wAA"),
            ("Gm=0", "/w=="),
            ("Ga=t,i=123,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=123;OK\x1b\\");
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(h.pending_uploads(), 0);
        // The next chunk ends the query even when it says m=1.
        let (_, sink) = run_seq(&[
            ("Ga=q,i=190,f=32,s=1,v=1,m=1", "/wAA"),
            ("Gm=1", "/w=="),
            ("Ga=t,i=191,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=191;OK\x1b\\");
        // Bad dimensions are reported on the first chunk.
        let (_, sink) = run_seq(&[("Ga=q,i=188,f=32,m=1", "/wAA")]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=188;EINVAL:Zero width/height not allowed\x1b\\"
        );
        // Anonymous chunked queries are ignored entirely.
        let (_, sink) = run_seq(&[
            ("Ga=q,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=t,i=192,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=192;OK\x1b\\");
        // A chunked query in the middle of a transmit: EILSEQ for the
        // upload, which is dropped.
        let (h, sink) = run_seq(&[
            ("Ga=t,i=207,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=q,i=208,f=32,s=1,v=1,m=1", "/wAA"),
            ("Gm=0", "/w=="),
            ("Ga=t,i=209,f=32,s=1,v=1", &px),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=207;EILSEQ:More payload loading refers to non-existent image\x1b\\\x1b_Gi=209;OK\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
    }

    /// DEL-3 (kitty 0.49.2): any delete aborts the upload in progress,
    /// and its remaining chunks get no reply.
    #[test]
    fn del3_delete_aborts_the_upload() {
        let z8 = "AAAAAAAAAAA=";
        for del in [
            "Ga=d,d=i,i=140",
            "Ga=d,d=I,i=140",
            "Ga=d,d=a",
            "Ga=d,d=i,i=999",
        ] {
            let (h, sink) = run_seq(&[
                ("Ga=t,i=140,f=32,s=2,v=2,m=1", z8),
                (del, ""),
                ("Gm=1", "AAAAAA=="),
                ("Gm=0", z8),
                ("Ga=p,i=140", ""),
            ]);
            assert_eq!(
                joined(&sink),
                "\x1b_Gi=140;ENOENT:Put command refers to non-existent image with id: 140 and number: 0\x1b\\",
                "{del}"
            );
            assert!(sink.registered.is_empty(), "{del}");
            assert_eq!(h.pending_uploads(), 0);
        }
    }

    /// kitty: a put for the image still loading gets a dedicated ENOENT,
    /// and the upload carries on.
    #[test]
    fn put_on_the_loading_image() {
        let (_, sink) = run_seq(&[
            ("Ga=t,i=119,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=p,i=119", ""),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=119;ENOENT:Put command refers to image with id: 119 that could not load its data\x1b\\\x1b_Gi=119;OK\x1b\\"
        );
        // By number: kitty reports the id the image was given.
        let (_, sink) = run_seq(&[
            ("Ga=t,I=172,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=p,I=172", ""),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=1,I=172;ENOENT:Put command refers to image with id: 0 that could not load its data\x1b\\\x1b_Gi=1,I=172;OK\x1b\\"
        );
    }

    /// A file transmit or a direct query during a load: kitty processes
    /// the new command on its own and drops the upload (file), or appends
    /// the query's data to it (query).
    #[test]
    fn other_commands_during_a_load() {
        let (h, sink) = run_seq(&[
            ("Ga=t,i=200,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=t,t=f,i=201,f=32,s=1,v=1", "L25vbmV4aXN0ZW50"),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(
            joined(&sink),
            "\x1b_Gi=201;EBADF:Failed to read image file\x1b\\"
        );
        assert_eq!(h.pending_uploads(), 0);
        // An `a=a` on the loading image is silent and harmless.
        let (_, sink) = run_seq(&[
            ("Ga=t,i=215,f=32,s=1,v=1,m=1", "/wAA"),
            ("Ga=a,i=215,s=3", ""),
            ("Gm=0", "/w=="),
        ]);
        assert_eq!(joined(&sink), "\x1b_Gi=215;OK\x1b\\");
    }

    /// SEC-6: an upload with no chunk for [`PENDING_UPLOAD_TTL`] is freed,
    /// so it can't swallow later transmits.
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
    }

    /// A continuation chunk refreshes the TTL.
    #[test]
    fn continuation_refreshes_expiry() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        start_upload(&mut h, &mut sink, 1, b"AAAA");
        let t0 = h.loading.as_ref().unwrap().last_chunk;
        h.process(b"Gm=1", b"", &mut sink).unwrap();
        let t1 = h.loading.as_ref().unwrap().last_chunk;
        assert!(t1 >= t0);
        h.expire_pending(t1 + PENDING_UPLOAD_TTL / 2);
        assert_eq!(h.pending_uploads(), 1);
    }

    #[test]
    fn default_handler_has_no_extra_cap() {
        let h = KittyHandler::default();
        assert_eq!(h.pending_cap, usize::MAX);
        assert_eq!(h.pending_uploads(), 0);
    }

    #[test]
    fn shm_query_of_a_missing_object_is_ebadf() {
        // WIRE-2: `a=q,t=s` is answered like a file query, on every
        // platform; the full `t=s` cases are with the mediums.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let name = base64::engine::general_purpose::STANDARD
            .encode(format!("/toastty-no-such-shm-{}", std::process::id()));
        h.process(b"Ga=q,t=s,f=32,s=1,v=1,i=1", name.as_bytes(), &mut sink)
            .unwrap();
        assert_eq!(
            sink.replies,
            vec![b"\x1b_Gi=1;EBADF:Failed to read image file\x1b\\".to_vec()]
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

    /// Review of #39 (kitty-ref, 9x20): `X=`/`Y=` widen the reservation
    /// only for a natural-size image. With one side given, the derived
    /// side comes from the given span plus its own offset.
    #[test]
    fn single_axis_fit_reserves_like_kitty() {
        let c10y = hdr(|h| (h.cols, h.cell_y) = (10, 19));
        assert_eq!(fit(&c10y, 400, 200).0, (10, 3), "kitty 8;31, not 4 rows");
        let r3x = hdr(|h| (h.rows, h.cell_x) = (3, 9));
        assert_eq!(fit(&r3x, 400, 200).0, (14, 3), "kitty 8;35, not 15 cols");
        let r3y = hdr(|h| (h.rows, h.cell_y) = (3, 19));
        assert_eq!(fit(&r3y, 400, 200).0, (18, 3), "kitty 8;39");
        let c4x = hdr(|h| (h.cols, h.cell_x) = (4, 8));
        assert_eq!(fit(&c4x, 400, 200).0, (4, 2), "kitty 7;25");
        // Natural size: both offsets count (unchanged).
        let nat = hdr(|h| (h.cell_x, h.cell_y) = (8, 19));
        assert_eq!(fit(&nat, 200, 100).0, (24, 6));
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
        transient: Vec<u32>,
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
        fn mark_transient(&mut self, id: u32) {
            self.transient.push(id);
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

    // ---- t=s (WIRE-2) ----

    /// A uniquely named POSIX shm object, made the way `shm_open` makes
    /// one on Linux (a file in `/dev/shm`), removed on drop.
    #[cfg(target_os = "linux")]
    struct Shm(std::path::PathBuf);

    #[cfg(target_os = "linux")]
    impl Shm {
        fn new(tag: &str, bytes: Option<&[u8]>) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let path = std::path::PathBuf::from(format!(
                "/dev/shm/toastty-handler-shm-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            if let Some(bytes) = bytes {
                std::fs::write(&path, bytes).unwrap();
            }
            Self(path)
        }

        /// The base64 `/name` a client sends.
        fn body(&self) -> Vec<u8> {
            let name = self.0.file_name().unwrap().to_str().unwrap();
            b64(format!("/{name}").as_bytes())
        }

        fn exists(&self) -> bool {
            self.0.exists()
        }
    }

    #[cfg(target_os = "linux")]
    impl Drop for Shm {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    #[cfg(target_os = "linux")]
    const RGBA_2X2: [u8; 16] = [
        255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255,
    ];

    #[cfg(target_os = "linux")]
    #[test]
    fn t_s_reads_registers_and_unlinks() {
        let shm = Shm::new("t", Some(&RGBA_2X2));
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=32,s=2,v=2,i=3,t=s", &shm.body(), &mut sink)
            .unwrap();
        assert_eq!(replies_joined(&sink), "\x1b_Gi=3;OK\x1b\\");
        assert_eq!(sink.registered.len(), 1);
        assert_eq!(sink.registered[0].0, 3);
        assert_eq!(sink.registered[0].1.width, 2);
        assert_eq!(sink.registered[0].1.pixels, RGBA_2X2);
        assert!(!shm.exists(), "the spec: read, then unlink");
    }

    /// `kitten icat --transfer-mode=memory` and `mpv --vo-kitty-use-shm`
    /// send `a=T,t=s` with `S=`, `q=2`: displayed, no reply.
    #[cfg(target_os = "linux")]
    #[test]
    fn t_s_transmit_and_place_with_size_and_offset() {
        let mut bytes = vec![0xAA; 3];
        bytes.extend_from_slice(&RGBA_2X2);
        bytes.extend_from_slice(&[0xBB; 5]);
        let shm = Shm::new("T", Some(&bytes));
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(
            b"Ga=T,f=32,s=2,v=2,i=4,t=s,O=3,S=16,q=2",
            &shm.body(),
            &mut sink,
        )
        .unwrap();
        assert!(sink.replies.is_empty());
        assert_eq!(sink.registered[0].1.pixels, RGBA_2X2);
        assert_eq!(sink.placements.len(), 1);
        assert!(!shm.exists());
    }

    /// Every failure is the uniform EBADF (SEC-4). A regular object is
    /// unlinked even when the read or the decode fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn t_s_failures_reply_ebadf_and_still_unlink() {
        let missing = Shm::new("missing", None);
        let short = Shm::new("short", Some(&RGBA_2X2[..15]));
        let junk = Shm::new("junk", Some(b"not a png at all"));
        let past = Shm::new("past", Some(&RGBA_2X2));
        let cases: [(&[u8], &Shm); 4] = [
            (b"a=t,f=32,s=2,v=2,i=9,t=s", &missing),
            (b"a=t,f=32,s=2,v=2,i=9,t=s", &short),
            (b"a=t,f=100,i=9,t=s", &junk),
            (b"a=t,f=100,i=9,t=s,O=99", &past),
        ];
        for (keys, shm) in cases {
            let mut h = KittyHandler::new();
            let mut sink = MockSink::default();
            let ctl = [b"G", keys].concat();
            h.process(&ctl, &shm.body(), &mut sink).unwrap();
            assert_eq!(
                replies_joined(&sink),
                format!("\x1b_Gi=9;{FILE_ERR}\x1b\\"),
                "{}",
                String::from_utf8_lossy(keys)
            );
            assert!(sink.registered.is_empty());
            assert!(!shm.exists(), "{}", String::from_utf8_lossy(keys));
        }
        // An invalid name (no leading `/`) gets the same reply.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=100,i=9,t=s", &b64(b"no-slash"), &mut sink)
            .unwrap();
        assert_eq!(replies_joined(&sink), format!("\x1b_Gi=9;{FILE_ERR}\x1b\\"));
    }

    /// `a=q,t=s` (how `kitten icat --detect-support` finds `memory`): the
    /// object is read, decoded and unlinked, OK is sent, and nothing is
    /// stored. Matches kitty 0.49.2, except that a decode failure is EBADF
    /// rather than kitty's EBADPNG (SEC-4).
    #[cfg(target_os = "linux")]
    #[test]
    fn t_s_query_reads_unlinks_and_stores_nothing() {
        let ok = Shm::new("q-ok", Some(&RGBA_2X2));
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=q,f=32,s=2,v=2,i=31,t=s", &ok.body(), &mut sink)
            .unwrap();
        assert_eq!(replies_joined(&sink), "\x1b_Gi=31;OK\x1b\\");
        assert!(sink.registered.is_empty() && sink.placements.is_empty());
        assert!(!ok.exists());

        let junk = Shm::new("q-junk", Some(b"junk"));
        let mut sink = MockSink::default();
        h.process(b"Ga=q,f=100,i=32,t=s", &junk.body(), &mut sink)
            .unwrap();
        assert_eq!(
            replies_joined(&sink),
            format!("\x1b_Gi=32;{FILE_ERR}\x1b\\")
        );
        assert!(!junk.exists());

        let missing = Shm::new("q-missing", None);
        let mut sink = MockSink::default();
        h.process(b"Ga=q,f=100,i=33,t=s", &missing.body(), &mut sink)
            .unwrap();
        assert_eq!(
            replies_joined(&sink),
            format!("\x1b_Gi=33;{FILE_ERR}\x1b\\")
        );

        // No `i=`: ignored entirely, as in kitty (WIRE-7). kitty leaves
        // the object in place too.
        let anon = Shm::new("q-anon", Some(&RGBA_2X2));
        let mut sink = MockSink::default();
        h.process(b"Ga=q,f=32,s=2,v=2,t=s", &anon.body(), &mut sink)
            .unwrap();
        assert!(sink.replies.is_empty());
        assert!(anon.exists(), "kitty doesn't read an anonymous query");
    }

    /// A `t=s` transmit or query during a chunked upload drops the upload,
    /// as a file transmit does.
    #[cfg(target_os = "linux")]
    #[test]
    fn t_s_aborts_a_loading_upload() {
        let shm = Shm::new("abort", Some(&RGBA_2X2));
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(
            b"Ga=t,f=32,s=2,v=2,i=5,m=1",
            &b64(&RGBA_2X2[..8]),
            &mut sink,
        )
        .unwrap();
        h.process(b"Ga=q,f=32,s=2,v=2,i=6,t=s", &shm.body(), &mut sink)
            .unwrap();
        assert_eq!(replies_joined(&sink), "\x1b_Gi=6;OK\x1b\\");
        // The next chunk starts nothing: the upload is gone.
        h.process(b"Gm=0", &b64(&RGBA_2X2[8..]), &mut sink).unwrap();
        assert!(sink.registered.is_empty());
    }

    /// Off Linux, `t=s` can't be read without `mmap` (see
    /// `medium::read_shm_object`): it fails with the uniform EBADF, and
    /// the object is still unlinked so clients don't leak it.
    #[cfg(all(unix, not(target_os = "linux"), not(target_os = "android")))]
    #[test]
    fn t_s_off_linux_replies_ebadf_and_unlinks() {
        use rustix::shm;
        let name = format!("/toastty-handler-shm-{}", std::process::id());
        let _ = shm::unlink(name.as_str());
        let fd = shm::open(
            name.as_str(),
            shm::OFlags::CREATE | shm::OFlags::EXCL | shm::OFlags::RDWR,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .unwrap();
        drop(fd);
        let mut h = KittyHandler::new();
        let mut sink = MockSink::default();
        h.process(b"Ga=t,f=100,i=9,t=s", &b64(name.as_bytes()), &mut sink)
            .unwrap();
        let gone = shm::open(
            name.as_str(),
            shm::OFlags::RDONLY,
            rustix::fs::Mode::empty(),
        )
        .is_err();
        let _ = shm::unlink(name.as_str());
        assert_eq!(replies_joined(&sink), format!("\x1b_Gi=9;{FILE_ERR}\x1b\\"));
        assert!(
            gone,
            "t=s must unlink the object even when it can't read it"
        );
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

    /// WIRE-1, as measured on kitty 0.49.2: `a=q` with `t=f`/`t=t` reads
    /// through the hardened path, answers OK or the uniform EBADF, never
    /// stores, and `t=t` deletes under the SEC-1 rule.
    #[test]
    fn wire1_file_medium_queries() {
        let d = TestDir::new("wire1");
        let px = [255u8, 0, 0, 255];
        let ok = d.path("q.rgba");
        std::fs::write(&ok, px).unwrap();
        let cases: [(&str, std::path::PathBuf, &str); 6] = [
            (
                "Ga=q,i=401,t=f,f=32,s=1,v=1",
                ok.clone(),
                "\x1b_Gi=401;OK\x1b\\",
            ),
            (
                "Ga=q,i=402,t=f,f=32,s=1,v=1",
                d.path("nope"),
                "\x1b_Gi=402;EBADF:Failed to read image file\x1b\\",
            ),
            (
                "Ga=q,i=403,t=f,f=32,s=2,v=2",
                ok.clone(),
                "\x1b_Gi=403;EBADF:Failed to read image file\x1b\\",
            ),
            (
                "Ga=q,i=408,t=f,f=32",
                ok.clone(),
                "\x1b_Gi=408;EINVAL:Zero width/height not allowed\x1b\\",
            ),
            ("Ga=q,i=409,t=f,f=32,s=1,v=1,q=1", ok.clone(), ""),
            ("Ga=q,I=407,t=f,f=32,s=1,v=1", ok.clone(), ""),
        ];
        for (header, path, want) in cases {
            let sink = run(header, &b64_path(&path), None);
            assert_eq!(replies_joined(&sink), want, "{header}");
            assert!(sink.registered.is_empty(), "a query never stores");
        }
        assert!(ok.exists(), "t=f queries never delete");

        // t=t: the marked file (TestDir names carry the marker) is deleted
        // after an OK and after a failed decode; queries without `i=`
        // leave it alone.
        let marked = d.path("m1.rgba");
        std::fs::write(&marked, px).unwrap();
        let sink = run("Ga=q,i=404,t=t,f=32,s=1,v=1", &b64_path(&marked), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=404;OK\x1b\\");
        assert!(!marked.exists());
        std::fs::write(&marked, px).unwrap();
        let sink = run("Ga=q,i=406,t=t,f=32,s=2,v=2", &b64_path(&marked), None);
        assert_eq!(
            replies_joined(&sink),
            format!("\x1b_Gi=406;{FILE_ERR}\x1b\\")
        );
        assert!(!marked.exists());
        for header in ["Ga=q,t=t,f=32,s=1,v=1", "Ga=q,I=412,t=t,f=32,s=1,v=1"] {
            std::fs::write(&marked, px).unwrap();
            let sink = run(header, &b64_path(&marked), None);
            assert!(sink.replies.is_empty(), "{header}");
            assert!(marked.exists(), "{header}: an ignored query keeps the file");
        }
        // An unmarked temp file is answered and kept.
        let plain =
            std::env::temp_dir().join(format!("toastty-wire1-plain-{}.rgba", std::process::id()));
        std::fs::write(&plain, px).unwrap();
        let sink = run("Ga=q,i=405,t=t,f=32,s=1,v=1", &b64_path(&plain), None);
        assert_eq!(replies_joined(&sink), "\x1b_Gi=405;OK\x1b\\");
        assert!(plain.exists());
        std::fs::remove_file(&plain).ok();
    }

    /// kitty checks the format and dimensions of a file-medium transmit
    /// before it opens the file, so a bad command leaves a marked `t=t`
    /// file in place and gets an EINVAL that says nothing about the file.
    #[test]
    fn file_medium_header_is_checked_before_the_file() {
        let d = TestDir::new("precheck");
        let marked = d.path("m.rgba");
        for (header, want) in [
            (
                "Ga=t,i=414,t=t,f=32",
                "\x1b_Gi=414;EINVAL:Zero width/height not allowed\x1b\\",
            ),
            (
                "Ga=t,i=415,t=t,f=8,s=1,v=1",
                "\x1b_Gi=415;EINVAL:Unknown image format: 8\x1b\\",
            ),
        ] {
            std::fs::write(&marked, [255u8, 0, 0, 255]).unwrap();
            let sink = run(header, &b64_path(&marked), None);
            assert_eq!(replies_joined(&sink), want, "{header}");
            assert!(marked.exists(), "{header}: the file is not touched");
        }
    }

    /// §9 usage hints: `N=1` (bit 0 of the bitmask) marks the stored
    /// image transient; other values don't.
    #[test]
    fn usage_hint_marks_transient() {
        let px = b64(&[255, 0, 0, 255]);
        for (header, want) in [
            ("Ga=t,i=410,f=32,s=1,v=1,N=1", vec![410]),
            ("Ga=t,i=411,f=32,s=1,v=1,N=3", vec![411]),
            ("Ga=t,i=412,f=32,s=1,v=1,N=2", vec![]),
            ("Ga=t,i=413,f=32,s=1,v=1", vec![]),
        ] {
            let sink = run(header, &px, None);
            assert_eq!(sink.transient, want, "{header}");
        }
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
        let z = zlib(&vec![0u8; 256 << 10]);
        assert!(z.len() < 1024);
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
        cursor_advances: usize,
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
        fn advance_cursor_after_placement(&mut self, _rows: u16, _cols: u16, _start_col: u16) {
            self.cursor_advances += 1;
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
        // (2,20) is a child of (1,10); re-placing (1,10) relative to
        // (2,20) would close the loop.
        sink.registered.push(1);
        sink.registered.push(2);
        let mut root = Placement::new(1, 1, 1);
        root.placement_id = 10;
        sink.grid.add(root);
        h.process(b"Ga=p,i=2,p=20,P=1,Q=10,H=1,V=1,q=2", b"", &mut sink)
            .unwrap();
        assert!(sink.grid.find(2, 20).is_some());
        h.process(b"Ga=p,i=1,p=10,P=2,Q=20,H=1,V=1", b"", &mut sink)
            .unwrap();
        assert!(sink.grid.find(1, 10).unwrap().parent.is_none(), "unchanged");
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
    fn relative_placement_partial_refs_match_kitty() {
        // REL-7, kitty 0.49.2: `P` without `Q` is relative to any
        // placement of the parent image; `Q` without `P` is an ordinary
        // placement. Both reply OK.
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        seed_parent(&mut sink);
        sink.registered.push(2);
        h.process(b"Ga=p,i=2,p=20,P=1,H=1,V=1", b"", &mut sink)
            .unwrap();
        let child = sink.grid.find(2, 20).expect("relative to image 1");
        assert_eq!(child.parent, Some((1, 0)));
        assert_eq!((child.line, child.col), (6, 4));
        h.process(b"Ga=p,i=2,p=21,Q=10,H=7,V=7", b"", &mut sink)
            .unwrap();
        assert_eq!(sink.grid.find(2, 21).map(|p| p.parent), Some(None));
        assert!(
            !sink.replies_joined().contains("EINVAL"),
            "got {:?}",
            sink.replies_joined()
        );
    }

    #[test]
    fn relative_errors_match_kitty() {
        // REL-7: self-reference of an existing placement → EINVAL; of a
        // new one → ENOPARENT; `a=p,U=1` with a parent → EINVAL.
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        seed_parent(&mut sink);
        h.process(b"Ga=p,i=1,p=10,P=1,Q=10", b"", &mut sink)
            .unwrap();
        assert!(
            sink.replies_joined().contains("EINVAL"),
            "{}",
            sink.replies_joined()
        );
        assert!(sink.grid.find(1, 10).is_some(), "REL-1: still there");
        sink.replies.clear();
        sink.registered.push(2);
        h.process(b"Ga=p,i=2,p=7,P=2,Q=7", b"", &mut sink).unwrap();
        assert!(
            sink.replies_joined().contains("ENOPARENT"),
            "{}",
            sink.replies_joined()
        );
        sink.replies.clear();
        h.process(b"Ga=p,i=2,p=8,U=1,P=1,Q=10", b"", &mut sink)
            .unwrap();
        assert!(
            sink.replies_joined().contains("EINVAL"),
            "{}",
            sink.replies_joined()
        );
    }

    #[test]
    fn transmit_and_place_honours_parent() {
        // REL-4: `a=T` with P/Q/H/V is relative and doesn't move the
        // cursor; negative offsets aren't clamped (REL-6).
        let mut h = KittyHandler::new();
        let mut sink = GridSink::default();
        seed_parent(&mut sink);
        h.process(
            b"Ga=T,f=32,s=1,v=1,i=5,p=1,P=1,Q=10,H=-4,V=-2,c=3,r=2",
            b"/wAA/w==",
            &mut sink,
        )
        .unwrap();
        let child = sink.grid.find(5, 1).expect("relative child");
        assert_eq!((child.line, child.col), (3, -1));
        assert_eq!(child.parent, Some((1, 10)));
        assert_eq!(
            sink.cursor_advances, 0,
            "relative placements never move the cursor"
        );
        assert!(
            sink.replies_joined().contains(";OK"),
            "{}",
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
        assign_next: u32,
    }

    impl MockSink {
        fn new() -> Self {
            Self {
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
