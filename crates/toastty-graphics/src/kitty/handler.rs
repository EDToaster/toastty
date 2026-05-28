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

use std::collections::HashMap;

use base64::Engine;

use super::decode::{DecodeError, decode};
use super::header::{Action, Compression, DeleteSpec, Header, Quiet, Transmission};
use super::reply::{ErrorCode, encode_error, encode_ok};
use crate::image_grid::{Placement, SrcRect};
use crate::registry::ImageData;

/// Maximum pending-upload buffer size in bytes. Larger uploads are
/// rejected mid-stream with [`ErrorCode::Efbig`].
pub const DEFAULT_PENDING_CAP: usize = 64 * 1024 * 1024;

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
    fn register_image(&mut self, id_request: u32, image_number: u32, data: ImageData)
    -> Option<u32>;

    /// Insert a placement onto the cell grid.
    fn place_image(&mut self, placement: Placement);

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
    /// Consulted on `a=q` (Query) so the handler can reply with
    /// [`ErrorCode::Enoent`] when the queried image is not present
    /// rather than falsely confirming "yes, we have it". Default
    /// returns `false` so existing test sinks see the conservative
    /// answer without having to override the method.
    fn image_exists(&self, _id: u32) -> bool {
        false
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
}

/// Top-level Kitty dispatcher.
#[derive(Debug, Default)]
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
}

impl KittyHandler {
    /// Fresh handler with default caps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            active_upload_id: None,
            pending_cap: DEFAULT_PENDING_CAP,
        }
    }

    /// Override the maximum pending-upload buffer size.
    pub fn set_pending_cap(&mut self, cap: usize) {
        self.pending_cap = cap;
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
        let header = match super::header::parse(header_bytes) {
            Ok(h) => h,
            Err(e) => {
                // B9: a malformed header must produce an EINVAL reply
                // (subject to quiet), not be silently swallowed. The
                // parse failed so we don't have a `Header` to read the
                // id from — best-effort recover `i=`/`I=` by scanning
                // the raw header for those keys (kitty parses all keys
                // then validates, echoing the id in the EINVAL reply).
                // If no id is recoverable the reply stays silent, in
                // line with the "anonymous requests get no reply" rule.
                emit_bad_header_einval(header_bytes, sink);
                return Err(HandlerError::BadHeader(e));
            }
        };
        // B8: specifying both `i=` and `I=` in any command is an error.
        // Reply EINVAL (respecting quiet) and perform no action.
        if header.image_id != 0 && header.image_number != 0 {
            reply_error_if_verbose(&header, sink, ErrorCode::Einval, "both i= and I= specified");
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
            Action::Frame | Action::Animate => {
                // Animation: pre-approved Enotsup.
                reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "animation");
            }
            Action::Compose => {
                reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "composition");
            }
        }
    }

    fn handle_query<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // Empty body: app is probing for protocol support, not testing
        // a specific payload. Reply OK directly.
        if body.is_empty() {
            reply_ok_if_verbose(&header, sink);
            return;
        }
        if !matches!(header.transmission, Transmission::Direct) {
            reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "transmission medium");
            return;
        }
        // Decode and validate. Same pipeline as `finalize_transmit`
        // but we discard the result (Query must not store/display).
        let decoded = match decode_base64(body) {
            Ok(d) => d,
            Err(e) => {
                reply_error_if_verbose(
                    &header,
                    sink,
                    ErrorCode::Einval,
                    &format!("bad base64: {e}"),
                );
                return;
            }
        };
        let raw = match header.compression {
            Compression::None => decoded,
            Compression::Zlib => match inflate_zlib(&decoded) {
                Ok(d) => d,
                Err(e) => {
                    reply_error_if_verbose(
                        &header,
                        sink,
                        ErrorCode::Einval,
                        &format!("zlib decompress: {e}"),
                    );
                    return;
                }
            },
        };
        match decode(header.format, &raw, header.source_width, header.source_height) {
            Ok(_) => reply_ok_if_verbose(&header, sink),
            Err(e) => {
                let code = match e {
                    DecodeError::BadPng(_) => ErrorCode::Ebadf,
                    DecodeError::LengthMismatch { .. } | DecodeError::MissingDims { .. } => {
                        ErrorCode::Einval
                    }
                };
                reply_error_if_verbose(&header, sink, code, &e.to_string());
            }
        }
    }

    fn handle_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // Only direct base64 transmission is supported in M11a.
        if !matches!(header.transmission, Transmission::Direct) {
            reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "transmission medium");
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
        if has_route
            && let Some(pending) = self.pending.get_mut(&key)
        {
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
            pending.buf.extend_from_slice(body);
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
            self.active_upload_id = Some(key);
            self.pending.insert(
                key,
                PendingUpload {
                    head: header,
                    buf: body.to_vec(),
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
        let decoded = match decode_base64(&b64_buf) {
            Ok(d) => d,
            Err(e) => {
                reply_error_if_verbose(
                    &header,
                    sink,
                    ErrorCode::Einval,
                    &format!("bad base64: {e}"),
                );
                return;
            }
        };

        // Optional zlib decompression.
        let raw = match header.compression {
            Compression::None => decoded,
            Compression::Zlib => match inflate_zlib(&decoded) {
                Ok(d) => d,
                Err(e) => {
                    reply_error_if_verbose(
                        &header,
                        sink,
                        ErrorCode::Einval,
                        &format!("zlib decompress: {e}"),
                    );
                    return;
                }
            },
        };

        let img = match decode(header.format, &raw, header.source_width, header.source_height) {
            Ok(d) => d,
            Err(e) => {
                let code = match e {
                    DecodeError::BadPng(_) => ErrorCode::Ebadf,
                    DecodeError::LengthMismatch { .. } | DecodeError::MissingDims { .. } => {
                        ErrorCode::Einval
                    }
                };
                reply_error_if_verbose(&header, sink, code, &e.to_string());
                return;
            }
        };

        // Hand off to the host. Capture dimensions BEFORE the move so
        // we can build the placement after the borrow ends.
        let (img_w, img_h) = (img.width, img.height);
        let Some(final_id) = sink.register_image(header.image_id, header.image_number, img) else {
            reply_error_if_verbose(&header, sink, ErrorCode::Efbig, "registry full");
            return;
        };

        // If the action also says "place", emit a default placement at
        // the cursor. The host's adapter uses the cursor's current
        // (row, col) and the configured cell dims to size the rect.
        //
        // B12: `U=1` requests a VIRTUAL placement — the image is
        // registered (done above) but NOT displayed at the cursor, and
        // the cursor must NOT advance. The visible references come from
        // subsequent U+10EEEE placeholder cells (handled elsewhere). So
        // for `U=1` we skip both the visible placement and the cursor
        // advance, but still reply OK.
        if matches!(header.action, Action::TransmitAndPlace) && !header.unicode_placeholder {
            let (cell_pw, cell_ph) = sink.cell_pixel_size();
            let placement =
                default_placement_from_header(&header, final_id, img_w, img_h, cell_pw, cell_ph);
            let rows_span = placement.row_range.end - placement.row_range.start;
            let cols_span = placement.col_range.end - placement.col_range.start;
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
        if header.image_id == 0 {
            reply_error_if_verbose(&header, sink, ErrorCode::Einval, "place requires i=");
            return;
        }
        // B12: `U=1` is a virtual placement — register-only, no visible
        // placement at the cursor and no cursor motion. The image was
        // already transmitted; the placeholder pipeline (handled
        // elsewhere) provides the visible references. Reply OK and
        // return without calling `place_image`.
        if header.unicode_placeholder {
            reply_ok_if_verbose_with_id(&header, sink, header.image_id);
            return;
        }
        // Standalone `a=p` doesn't carry img dims at this call site; we
        // pass zeroes so the auto-derive falls back to 1×1 when `c=`/`r=`
        // are unset (preserving the pre-fix behavior for this path).
        let (cell_pw, cell_ph) = sink.cell_pixel_size();
        let placement =
            default_placement_from_header(&header, header.image_id, 0, 0, cell_pw, cell_ph);
        let rows_span = placement.row_range.end - placement.row_range.start;
        let cols_span = placement.col_range.end - placement.col_range.start;
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

    #[allow(clippy::needless_pass_by_value, clippy::unused_self)]
    fn handle_delete<S: KittySink>(&mut self, header: Header, sink: &mut S) {
        sink.delete_image(header.delete, &header);
        reply_ok_if_verbose(&header, sink);
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

/// Two headers that belong to the same chunked upload should agree on
/// the fields below.
fn headers_continuation_compatible(first: &Header, next: &Header) -> bool {
    first.image_id == next.image_id
        && first.format == next.format
        && first.compression == next.compression
        && first.action == next.action
        && first.source_width == next.source_width
        && first.source_height == next.source_height
}

fn decode_base64(input: &[u8]) -> Result<Vec<u8>, String> {
    // Strip whitespace (some clients wrap lines).
    let cleaned: Vec<u8> = input
        .iter()
        .copied()
        .filter(|b| !b.is_ascii_whitespace())
        .collect();
    base64::engine::general_purpose::STANDARD
        .decode(&cleaned)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(&cleaned))
        .map_err(|e| e.to_string())
}

fn inflate_zlib(input: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let mut out = Vec::new();
    let mut d = flate2::read::ZlibDecoder::new(input);
    d.read_to_end(&mut out).map_err(|e| e.to_string())?;
    Ok(out)
}

fn default_placement_from_header(
    header: &Header,
    id: u32,
    img_w: u32,
    img_h: u32,
    cell_pw: u16,
    cell_ph: u16,
) -> Placement {
    // Source rect: take what the header asked for, defaulting to
    // "full image" when unset.
    let src = if header.src_w == 0 && header.src_h == 0 {
        SrcRect::FULL
    } else {
        SrcRect {
            x: header.src_x,
            y: header.src_y,
            w: header.src_w,
            h: header.src_h,
        }
    };
    // Cell span: when `c=`/`r=` are specified, use them. Otherwise the
    // kitty spec says the image occupies `ceil(img_dim / cell_dim)`
    // cells. Falling through to 1×1 (the old behavior) made
    // unannotated transmits — e.g. Ratty's widget demo — render as a
    // single cell band.
    //
    // M4: when ONLY ONE of `c=`/`r=` is given, the other axis is
    // derived from the source aspect ratio in PIXEL space, not from the
    // image's natural cell count. Per spec, the image is scaled to fit
    // the given axis preserving aspect ratio:
    //   cols * cell_pw : rows * cell_ph == img_w : img_h
    // So when only `c=` is set:
    //   rows = ceil(cols * cell_pw * img_h / (img_w * cell_ph))
    // and symmetrically when only `r=` is set:
    //   cols = ceil(rows * cell_ph * img_w / (img_h * cell_pw))
    // Both-zero keeps the natural-size derivation; both-set is verbatim.
    let cell_pw = u32::from(cell_pw.max(1));
    let cell_ph = u32::from(cell_ph.max(1));
    let derived_cols = (img_w + cell_pw - 1) / cell_pw;
    let derived_rows = (img_h + cell_ph - 1) / cell_ph;
    let img_w_eff = img_w.max(1);
    let img_h_eff = img_h.max(1);
    let (cols, rows) = match (header.cols, header.rows) {
        // Both set: use verbatim.
        (c, r) if c != 0 && r != 0 => (c as u16, r as u16),
        // Only `c=`: derive rows from the source aspect ratio.
        (c, 0) if c != 0 => {
            let num = c * cell_pw * img_h_eff;
            let den = img_w_eff * cell_ph;
            let r = ((num + den - 1) / den).max(1);
            (c as u16, r as u16)
        }
        // Only `r=`: derive cols from the source aspect ratio.
        (0, r) if r != 0 => {
            let num = r * cell_ph * img_w_eff;
            let den = img_h_eff * cell_pw;
            let c = ((num + den - 1) / den).max(1);
            (c as u16, r as u16)
        }
        // Neither set: natural cell count.
        _ => (derived_cols.max(1) as u16, derived_rows.max(1) as u16),
    };
    Placement {
        image_id: id,
        placement_id: header.placement_id,
        // The host rebases these ranges against the current cursor (see
        // `Term::place_image`); we carry only the SPAN here, anchored at
        // the origin. M3: `X=`/`Y=` are NOT cell offsets — they are
        // intra-cell pixel offsets, threaded via `pix_offset` below, so
        // they must NOT shift the cell range.
        row_range: 0..rows,
        col_range: 0..cols,
        src_rect: src,
        z: header.z,
        // M3: `X=` / `Y=` are sub-cell pixel offsets within the first
        // cell, applied at render time. They do not move the placement's
        // cells. (Header field names kept as `cell_x`/`cell_y` for now.)
        pix_offset: (header.cell_x, header.cell_y),
    }
}

/// Emit an EINVAL reply for a header that failed to parse (B9).
///
/// Because parsing failed we have no [`Header`]; recover the `i=` /
/// `I=` identifiers by scanning the raw header bytes for those keys so
/// the reply can echo them (matching reference kitty, which parses all
/// keys then validates and replies EINVAL with the id). If neither is
/// recoverable there is nothing to correlate a reply against, so we
/// stay silent — consistent with the anonymous-no-reply rule.
///
/// `q=` cannot be honored reliably from a malformed header, so we only
/// suppress when an explicit `q=2` (Silent) is recoverable; otherwise
/// we reply. A best-effort scan keeps this simple and robust.
fn emit_bad_header_einval<S: KittySink>(header_bytes: &[u8], sink: &mut S) {
    let s = match std::str::from_utf8(header_bytes) {
        Ok(s) => s.strip_prefix('G').unwrap_or(s),
        Err(_) => return,
    };
    let mut image_id = 0u32;
    let mut image_number = 0u32;
    let mut quiet = Quiet::Verbose;
    for pair in s.split(',') {
        if let Some((key, value)) = pair.split_once('=') {
            match key.trim() {
                "i" => image_id = value.trim().parse().unwrap_or(image_id),
                "I" => image_number = value.trim().parse().unwrap_or(image_number),
                "q" => {
                    quiet = match value.trim() {
                        "2" => Quiet::Silent,
                        "1" => Quiet::NoOk,
                        _ => Quiet::Verbose,
                    };
                }
                _ => {}
            }
        }
    }
    if matches!(quiet, Quiet::Silent) {
        return;
    }
    // Anonymous request (no id to echo) → no reply.
    if image_id == 0 && image_number == 0 {
        return;
    }
    sink.queue_reply(&encode_error(
        image_id,
        image_number,
        ErrorCode::Einval,
        "malformed header",
    ));
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
    sink.queue_reply(&encode_ok(image_id, header.image_number));
}

fn reply_error_if_verbose<S: KittySink>(
    header: &Header,
    sink: &mut S,
    code: ErrorCode,
    detail: &str,
) {
    if matches!(header.quiet, Quiet::Silent) {
        return;
    }
    if !client_wants_reply(header) {
        return;
    }
    sink.queue_reply(&encode_error(
        header.image_id,
        header.image_number,
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
        fn delete_image(&mut self, delete: DeleteSpec, header: &Header) {
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
        h.process(header.as_bytes(), body.as_bytes(), &mut sink).unwrap();
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
        h.process(header.as_bytes(), body.as_bytes(), &mut sink).unwrap();
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
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=7,m=1",
            a.as_bytes(),
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert_eq!(h.pending_uploads(), 1);
        // Final: m=0.
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=7,m=0",
            b.as_bytes(),
            &mut sink,
        )
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
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=42,m=1",
            a.as_bytes(),
            &mut sink,
        )
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
        h.process(
            b"Gf=24,s=2,v=2,c=2,r=2,a=T,m=1",
            a.as_bytes(),
            &mut sink,
        )
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

        h.process(b"Gf=24,s=2,v=2,c=2,r=2,a=T,m=1", a.as_bytes(), &mut sink).unwrap();
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
        h.process(b"Ga=t,f=32,s=1,v=1,i=7", b64_red_pixel().as_bytes(), &mut sink).unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains("i=7"), "must echo client id, got {joined:?}");
        assert!(joined.contains(";OK"), "must reply OK, got {joined:?}");
    }

    #[test]
    fn upload_with_image_number_only_replies_with_assigned_id() {
        // Per spec, when the client passes `I=M`, the terminal must
        // include the newly-assigned `i=` alongside `I=M` in the
        // reply. The new gate must still let this through.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=t,f=32,s=1,v=1,I=11", b64_red_pixel().as_bytes(), &mut sink).unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains("I=11"), "must echo I=, got {joined:?}");
        assert!(joined.contains("i="), "must include assigned i=, got {joined:?}");
        assert!(joined.contains(";OK"), "must reply OK, got {joined:?}");
    }

    #[test]
    fn chunked_header_mismatch_is_einval() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=7,m=1",
            b"AAAA",
            &mut sink,
        )
        .unwrap();
        // Second chunk with mismatched format.
        h.process(
            b"Ga=t,f=24,s=1,v=1,i=7,m=0",
            b"BBBB",
            &mut sink,
        )
        .unwrap();
        // Reply should be EINVAL; upload abandoned.
        assert_eq!(sink.registered.len(), 0);
        assert_eq!(h.pending_uploads(), 0);
        assert!(sink.replies.iter().any(|r| {
            std::str::from_utf8(r).is_ok_and(|s| s.contains("EINVAL"))
        }));
    }

    #[test]
    fn oversized_declared_payload_rejected_with_efbig() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(16);
        h.process(
            b"Ga=t,f=32,s=64,v=64,i=1,S=16384",
            b"",
            &mut sink,
        )
        .unwrap();
        assert_eq!(sink.registered.len(), 0);
        assert!(sink.replies.iter().any(|r| {
            std::str::from_utf8(r).is_ok_and(|s| s.contains("EFBIG"))
        }));
    }

    #[test]
    fn animate_action_replies_enotsup() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=a,i=1", b"", &mut sink).unwrap();
        assert!(sink.replies.iter().any(|r| {
            std::str::from_utf8(r).is_ok_and(|s| s.contains("ENOTSUP"))
        }));
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

    #[test]
    fn place_without_id_but_with_image_number_still_replies_einval() {
        // Sanity: when the client did give us an identifier (`I=`),
        // the reply suppression doesn't apply — they get to know
        // their place command was rejected.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=p,I=9", b"", &mut sink).unwrap();
        assert!(sink.replies.iter().any(|r| {
            std::str::from_utf8(r).is_ok_and(|s| s.contains("EINVAL"))
        }));
    }

    #[test]
    fn quiet_level_one_suppresses_ok() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel();
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=42,q=1",
            body.as_bytes(),
            &mut sink,
        )
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
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=42,q=2",
            b"!!!",
            &mut sink,
        )
        .unwrap();
        assert!(sink.replies.is_empty(), "got {:?}", sink.replies);
    }

    #[test]
    fn bad_header_is_handler_error() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let err = h
            .process(b"NotG", b"", &mut sink)
            .unwrap_err();
        assert!(matches!(err, HandlerError::BadHeader(_)));
    }

    #[test]
    fn query_with_empty_body_replies_ok() {
        // Per kitty spec, `a=q` is "transmit + test" — apps probing
        // for protocol support send a tiny (often 1x1) payload and
        // expect OK if the terminal understands kitty graphics. A
        // body-less query (some tools emit this as a bare protocol
        // probe) also gets OK.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=q,i=42", b"", &mut sink).unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(
            joined.contains(";OK"),
            "empty-body query should reply OK, got {joined:?}",
        );
    }

    #[test]
    fn query_with_valid_body_replies_ok_without_storing() {
        // The yazi probe shape: `a=q,s=1,v=1,t=d,f=24;AAAA` — 1x1 RGB
        // pixel. Decode succeeds → OK. Registry stays empty (Query
        // must not store).
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(
            b"Ga=q,s=1,v=1,t=d,f=24,i=31",
            b"AAAA",
            &mut sink,
        )
        .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains(";OK"), "got {joined:?}");
        assert!(
            sink.registered.is_empty(),
            "Query must not store the image"
        );
    }

    #[test]
    fn query_with_garbage_body_replies_error() {
        // Body that is valid base64 of garbage PNG bytes → Ebadf
        // (PNG decoder rejects). The base64 layer succeeds first.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        // "garbage!" base64-encoded.
        h.process(
            b"Ga=q,f=100,i=99",
            b"Z2FyYmFnZSE=",
            &mut sink,
        )
        .unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(joined.contains("EBADF"), "got {joined:?}");
    }

    #[test]
    fn query_known_image_returns_ok() {
        // Transmit id=7 first, then query it — must reply OK.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        let body = b64_red_pixel();
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=7",
            body.as_bytes(),
            &mut sink,
        )
        .unwrap();
        // Drop the transmit reply so we only inspect query replies.
        sink.replies.clear();
        h.process(b"Ga=q,i=7", b"", &mut sink).unwrap();
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
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=5,m=1",
            b"AAAAAA",
            &mut sink,
        )
        .unwrap();
        // Overflow the 8-byte pending cap.
        h.process(
            b"Ga=t,f=32,s=1,v=1,i=5,m=1",
            b"BBBBBB",
            &mut sink,
        )
        .unwrap();
        assert!(sink.replies.iter().any(|r| {
            std::str::from_utf8(r).is_ok_and(|s| s.contains("EFBIG"))
        }));
        assert_eq!(h.pending_uploads(), 0);
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
        fn place_image(&mut self, placement: Placement) {
            self.placements.push(placement);
        }
        fn delete_image(&mut self, _delete: DeleteSpec, _header: &Header) {}
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
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
        assert!(joined.contains("i=1"), "reply should echo i=, got {joined:?}");
        assert!(joined.contains("I=2"), "reply should echo I=, got {joined:?}");
    }

    // ----- B9: malformed header replies EINVAL (id recoverable) -----

    #[test]
    fn b9_malformed_header_with_id_replies_einval() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // `f=999` is not a valid format enum → BadEnum on parse.
        let err = h
            .process(b"Ga=t,i=1,f=999,s=1,v=1", b64_red_pixel().as_bytes(), &mut sink)
            .unwrap_err();
        assert!(matches!(err, HandlerError::BadHeader(_)));
        let joined = sink.joined_replies();
        assert!(joined.contains("EINVAL"), "expected EINVAL, got {joined:?}");
        // Best-effort id recovery: the reply echoes the scanned i=1.
        assert!(joined.contains("i=1"), "reply should echo recovered i=, got {joined:?}");
        assert!(sink.registered.is_empty(), "malformed header must not register");
    }

    #[test]
    fn b9_malformed_header_with_image_number_replies_einval() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // `a=X` is an invalid action enum.
        h.process(b"Ga=X,I=7", b"", &mut sink).unwrap_err();
        let joined = sink.joined_replies();
        assert!(joined.contains("EINVAL"), "expected EINVAL, got {joined:?}");
        assert!(joined.contains("I=7"), "reply should echo recovered I=, got {joined:?}");
    }

    #[test]
    fn b9_malformed_header_without_id_is_silent() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // Invalid action and no recoverable i=/I=.
        h.process(b"Ga=X,f=999", b"", &mut sink).unwrap_err();
        assert!(
            sink.replies.is_empty(),
            "malformed header with no id must stay silent, got {:?}",
            sink.joined_replies()
        );
    }

    #[test]
    fn b9_malformed_header_silenced_by_q2() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // Has a recoverable id but q=2 (Silent) suppresses the reply.
        h.process(b"Ga=X,i=3,q=2", b"", &mut sink).unwrap_err();
        assert!(
            sink.replies.is_empty(),
            "q=2 must silence the EINVAL reply, got {:?}",
            sink.joined_replies()
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
        assert_eq!(sink.registered.len(), 1, "U=1 must still register the image");
        assert_eq!(sink.registered[0].0, 5);
        // No visible placement, no cursor advance.
        assert!(
            sink.placements.is_empty(),
            "U=1 must not create a visible placement"
        );
        assert_eq!(
            sink.cursor_advances, 0,
            "U=1 must not advance the cursor"
        );
        // Still replies OK (id present).
        assert!(sink.joined_replies().contains(";OK"));
    }

    #[test]
    fn b12_place_with_u1_is_virtual() {
        let mut h = KittyHandler::new();
        let mut sink = RecordingSink::new();
        // a=p with U=1 — no visible placement, no cursor advance.
        h.process(b"Ga=p,i=5,U=1", b"", &mut sink).unwrap();
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
        assert_eq!(p.col_range, 0..10);
        assert_eq!(p.row_range, 0..3, "rows derived from aspect, not natural cell count");
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
        assert_eq!(p.row_range, 0..4);
        assert_eq!(p.col_range, 0..16, "cols derived from aspect, not natural cell count");
    }

    #[test]
    fn m4_both_zero_uses_natural_cell_count() {
        // 200x100 image, 10x20 cell: natural = (20 cols, 5 rows).
        let header = Header::default();
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!(p.col_range, 0..20);
        assert_eq!(p.row_range, 0..5);
    }

    #[test]
    fn m4_both_set_used_verbatim() {
        let header = Header {
            cols: 7,
            rows: 9,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 200, 100, 10, 20);
        assert_eq!(p.col_range, 0..7);
        assert_eq!(p.row_range, 0..9);
    }

    // ---- M3: X=/Y= map to pix_offset, leave cell ranges alone ----

    #[test]
    fn m3_xy_become_pixel_offset_not_cell_range() {
        let base = default_placement_from_header(&Header::default(), 1, 16, 16, 8, 8);
        let header = Header {
            cell_x: 3,
            cell_y: 4,
            ..Header::default()
        };
        let p = default_placement_from_header(&header, 1, 16, 16, 8, 8);
        assert_eq!(p.pix_offset, (3, 4));
        assert_eq!(p.col_range, base.col_range, "X must not shift the cell range");
        assert_eq!(p.row_range, base.row_range, "Y must not shift the cell range");
    }
}
