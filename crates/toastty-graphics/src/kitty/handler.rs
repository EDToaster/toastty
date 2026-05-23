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
    /// host should assign one. Returns the *final* id, or `None` if the
    /// host could not accept the image. The handler maps `None` to
    /// [`ErrorCode::Efbig`].
    fn register_image(&mut self, id_request: u32, data: ImageData) -> Option<u32>;

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
    /// `start_col` is the column the placement started at — kitty
    /// spec says the cursor lands at `(start_row + rows, start_col)`,
    /// NOT at column 0. The handler captures this via
    /// [`KittySink::cursor_col`] before the placement is consumed.
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
    /// Per-instance override of [`DEFAULT_PENDING_CAP`].
    pending_cap: usize,
}

impl KittyHandler {
    /// Fresh handler with default caps.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
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
        let header = super::header::parse(header_bytes).map_err(HandlerError::BadHeader)?;
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
                // Query: ENOENT if no id was provided OR the host's
                // registry doesn't hold the queried image. OK only
                // when the host confirms presence via
                // `KittySink::image_exists`. (Before M11a-followup.I2
                // we unconditionally replied OK when `i!=0`, which
                // told apps we owned images we'd never received.)
                if header.image_id != 0 && sink.image_exists(header.image_id) {
                    reply_ok_if_verbose(&header, sink);
                } else {
                    reply_error_if_verbose(&header, sink, ErrorCode::Enoent, "");
                }
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

    fn handle_transmit<S: KittySink>(&mut self, header: Header, body: &[u8], sink: &mut S) {
        // Only direct base64 transmission is supported in M11a.
        if !matches!(header.transmission, Transmission::Direct) {
            reply_error_if_verbose(&header, sink, ErrorCode::Enotsup, "transmission medium");
            return;
        }

        let key = header.image_id;

        // Continuation: append to the existing pending buffer.
        if let Some(pending) = self.pending.get_mut(&key)
            && key != 0
        {
            // Compare critical header fields. Mismatches → Einval and
            // abandon the upload.
            if !headers_continuation_compatible(&pending.head, &header) {
                let head = pending.head.clone();
                self.pending.remove(&key);
                reply_error_if_verbose(&head, sink, ErrorCode::Einval, "header mismatch");
                return;
            }
            if pending.buf.len() + body.len() > self.pending_cap {
                let head = pending.head.clone();
                self.pending.remove(&key);
                reply_error_if_verbose(&head, sink, ErrorCode::Efbig, "pending overflow");
                return;
            }
            pending.buf.extend_from_slice(body);
            if header.more {
                return;
            }
            // Final chunk — pop and finalize.
            let pending = self.pending.remove(&key).unwrap();
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
            // Spec requires `i=` for chunked uploads (so continuation
            // chunks can reference the in-flight payload). If the
            // client omitted it, accept under a synthesized id of 0
            // — but in practice clients always include `i=`. We
            // tolerate the absence so we don't reject valid streams.
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
        let Some(final_id) = sink.register_image(header.image_id, img) else {
            reply_error_if_verbose(&header, sink, ErrorCode::Efbig, "registry full");
            return;
        };

        // If the action also says "place", emit a default placement at
        // the cursor. The host's adapter uses the cursor's current
        // (row, col) and the configured cell dims to size the rect.
        if matches!(header.action, Action::TransmitAndPlace) {
            let placement = default_placement_from_header(&header, final_id, img_w, img_h);
            let rows_span = placement.row_range.end - placement.row_range.start;
            let cols_span = placement.col_range.end - placement.col_range.start;
            // M11a-followup.N6: capture the cursor's start_col BEFORE
            // place_image consumes the placement. Kitty spec says the
            // cursor lands at (start_row + rows, start_col), not at
            // column 0.
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
        sink.place_image(default_placement_from_header(
            &header,
            header.image_id,
            0,
            0,
        ));
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

fn default_placement_from_header(header: &Header, id: u32, img_w: u32, img_h: u32) -> Placement {
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
    // Cell span: header.cols/rows if specified, else the host must
    // derive from img dims + cell metrics. The host knows its cell
    // size; for M11a we encode 0..cols and 0..rows as a *hint* and
    // let the host expand the actual span when applying.
    //
    // For decode-time consistency we encode (0..cols, 0..rows) when
    // both are provided; otherwise the host fills in based on image
    // pixel dims / its own cell size.
    let cols = header.cols.max(1) as u16;
    let rows = header.rows.max(1) as u16;
    let _ = (img_w, img_h); // host derives from registry if needed.
    Placement {
        image_id: id,
        placement_id: header.placement_id,
        // Host will translate (cell_x, cell_y) + (cols, rows) into
        // absolute cell ranges based on the current cursor. We carry
        // the hint here; the sink's adapter applies its policy. For
        // now we store a relative-style range and let the sink
        // rebase.
        row_range: u16::try_from(header.cell_y).unwrap_or(0)
            ..u16::try_from(header.cell_y).unwrap_or(0).saturating_add(rows),
        col_range: u16::try_from(header.cell_x).unwrap_or(0)
            ..u16::try_from(header.cell_x).unwrap_or(0).saturating_add(cols),
        src_rect: src,
        z: header.z,
    }
}

fn reply_ok_if_verbose<S: KittySink>(header: &Header, sink: &mut S) {
    reply_ok_if_verbose_with_id(header, sink, header.image_id);
}

fn reply_ok_if_verbose_with_id<S: KittySink>(header: &Header, sink: &mut S, image_id: u32) {
    if matches!(header.quiet, Quiet::Verbose) {
        sink.queue_reply(&encode_ok(image_id, header.image_number));
    }
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
    sink.queue_reply(&encode_error(
        header.image_id,
        header.image_number,
        code,
        detail,
    ));
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
        fn register_image(&mut self, id_request: u32, data: ImageData) -> Option<u32> {
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
    fn place_without_id_is_einval() {
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=p", b"", &mut sink).unwrap();
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
    fn query_unknown_image_returns_enoent() {
        // M11a-followup.I2: an `a=q` for an id we don't hold MUST
        // reply ENOENT — apps query before transmitting and expect
        // the truth.
        let mut h = KittyHandler::new();
        let mut sink = MockSink::with_budget(1 << 30);
        h.process(b"Ga=q,i=42", b"", &mut sink).unwrap();
        let joined: String = sink
            .replies
            .iter()
            .map(|r| String::from_utf8_lossy(r).into_owned())
            .collect();
        assert!(
            joined.contains("ENOENT"),
            "expected ENOENT for unknown image, got {joined:?}",
        );
        assert!(!joined.contains(";OK"));
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
