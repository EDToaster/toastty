//! Stateful RGP dispatcher.
//!
//! Receives buffered APC payloads (post-`ESC _` / pre-`ESC \`) and:
//!
//! 1. Parses them into [`RgpOperation`]s via
//!    [`crate::rgp::operation::parse`].
//! 2. Handles per-id chunked-payload reassembly for `r;source=payload;
//!    more=1 ... more=0` sequences.
//! 3. Dispatches each completed operation to an [`RgpSink`] (the
//!    host's interpretation of "register / place / update / delete /
//!    queue reply").
//!
//! Mirrors [`crate::kitty::handler::KittyHandler`] shape so the
//! `Term` integration follows the same demux pattern.

use std::collections::HashMap;

use crate::rgp::operation::{
    RgpAnchor, RgpFormat, RgpOperation, RgpParseError, RgpPlacementStyle,
    RgpPlacementUpdate, RgpRegisterSource, parse,
};
use crate::rgp::reply::support_reply;

/// Maximum bytes the handler will buffer for a single in-flight
/// chunked register. Overflow causes the upload to be abandoned.
pub const DEFAULT_PENDING_CAP_PER_ID: usize = 64 * 1024 * 1024;

/// Maximum total bytes across all in-flight chunked registers.
/// Defends against a stream that opens many partial uploads to
/// exhaust memory without ever finishing one.
pub const DEFAULT_PENDING_TOTAL_CAP: usize = 256 * 1024 * 1024;

/// Callback surface for the host (`Term`) into the handler.
pub trait RgpSink {
    /// Register a fully-assembled asset. `bytes` is the raw payload
    /// in `format`. The host decides how (and whether) to parse it.
    /// `name` is the optional `name=` field for diagnostics; the
    /// host MUST NOT use it as a filename.
    ///
    /// Returns `true` iff the registration succeeded (host accepted
    /// the bytes). v1 always returns `true` — M12b will start
    /// failing here for malformed glTF.
    fn register_asset(
        &mut self,
        id: u32,
        format: RgpFormat,
        name: Option<String>,
        bytes: Vec<u8>,
    ) -> bool;

    /// Register an asset by path-based lookup. The host resolves
    /// `name` against its asset bundle / configured directory; the
    /// handler intentionally does not do filesystem I/O.
    ///
    /// Returns `true` iff the resolution + registration succeeded.
    fn register_asset_by_path(
        &mut self,
        id: u32,
        format: RgpFormat,
        name: String,
    ) -> bool;

    /// Apply a `p` verb: insert/replace a placement.
    fn place(&mut self, id: u32, anchor: RgpAnchor, style: RgpPlacementStyle);

    /// Apply a `u` verb: merge a sparse style update onto an
    /// existing placement. The host may silently no-op if the id
    /// is not currently placed.
    fn update(&mut self, id: u32, update: RgpPlacementUpdate);

    /// Apply a `d` verb. `id == None` ⇒ wipe everything; `Some(n)`
    /// ⇒ drop placement `n`.
    fn delete(&mut self, id: Option<u32>);

    /// Queue bytes for write back to the PTY. The host's drain
    /// returns these to the binary's PTY writer.
    fn queue_reply(&mut self, bytes: &[u8]);
}

/// In-flight chunked register payload.
#[derive(Debug)]
struct Pending {
    format: RgpFormat,
    name: Option<String>,
    bytes: Vec<u8>,
}

/// Per-id chunked-upload state + dispatch logic.
#[derive(Debug, Default)]
pub struct RgpHandler {
    pending: HashMap<u32, Pending>,
    pending_total: usize,
    /// Per-id cap on buffered chunked-upload bytes. Overflow ⇒ drop.
    pub per_id_cap: usize,
    /// Total cap across all pending uploads. Overflow ⇒ drop.
    pub total_cap: usize,
}

impl RgpHandler {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            pending_total: 0,
            per_id_cap: DEFAULT_PENDING_CAP_PER_ID,
            total_cap: DEFAULT_PENDING_TOTAL_CAP,
        }
    }

    /// Number of in-flight chunked uploads (those still waiting for
    /// `more=0`). Exposed for tests + diagnostics.
    #[must_use]
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// Total bytes buffered across all in-flight uploads.
    #[must_use]
    pub fn pending_bytes(&self) -> usize {
        self.pending_total
    }

    /// Process one buffered APC payload (must start with the
    /// `ratty;g;` namespace prefix — the caller is expected to
    /// demux Kitty vs RGP based on the first byte).
    ///
    /// Returns `Ok(())` if the payload was parsed and dispatched
    /// (or buffered as an in-flight chunked register); `Err` if
    /// the payload was malformed enough that no dispatch happened.
    pub fn process(
        &mut self,
        payload: &[u8],
        sink: &mut dyn RgpSink,
    ) -> Result<(), RgpParseError> {
        let op = parse(payload)?;
        self.dispatch(op, sink);
        Ok(())
    }

    fn dispatch(&mut self, op: RgpOperation, sink: &mut dyn RgpSink) {
        match op {
            RgpOperation::SupportQuery => {
                sink.queue_reply(&support_reply());
            }
            RgpOperation::Register { id, format, source } => {
                self.handle_register(id, format, source, sink);
            }
            RgpOperation::Place { id, anchor, style } => sink.place(id, anchor, style),
            RgpOperation::Update { id, update } => sink.update(id, update),
            RgpOperation::Delete { id } => sink.delete(id),
        }
    }

    fn handle_register(
        &mut self,
        id: u32,
        format: RgpFormat,
        source: RgpRegisterSource,
        sink: &mut dyn RgpSink,
    ) {
        match source {
            RgpRegisterSource::Path { name } => {
                // Path-based register is one-shot. If we had a
                // pending chunked upload for this id, drop it — the
                // app has changed its mind.
                self.drop_pending(id);
                sink.register_asset_by_path(id, format, name);
            }
            RgpRegisterSource::Payload { name, more, data } => {
                self.handle_payload_chunk(id, format, name, more, data, sink);
            }
        }
    }

    // `name` and `data` are taken by value because the final-chunk
    // path moves both into `sink.register_asset(...)`. The earlier
    // exit branches (caps overflow, format mismatch) just drop them;
    // taking by reference would force a clone on the dispatch path.
    #[allow(clippy::needless_pass_by_value)]
    fn handle_payload_chunk(
        &mut self,
        id: u32,
        format: RgpFormat,
        name: Option<String>,
        more: bool,
        data: Vec<u8>,
        sink: &mut dyn RgpSink,
    ) {
        // Caps check BEFORE accepting bytes. If accepting this chunk
        // would put us over the per-id or total cap, drop the
        // upload.
        if let Some(existing) = self.pending.get(&id) {
            if existing.bytes.len().saturating_add(data.len()) > self.per_id_cap {
                self.drop_pending(id);
                return;
            }
        } else if data.len() > self.per_id_cap {
            return;
        }
        let new_total = self.pending_total.saturating_add(data.len());
        if new_total > self.total_cap {
            return;
        }

        // Fold the chunk into the per-id buffer.
        let entry = self.pending.entry(id).or_insert_with(|| Pending {
            format,
            name: name.clone(),
            bytes: Vec::new(),
        });
        // If a continuation packet declares a different format than
        // the opener, the upload is corrupt — drop it. Same for
        // `name` if both are set (we keep the first).
        if entry.format != format {
            self.drop_pending(id);
            return;
        }
        let added = data.len();
        entry.bytes.extend_from_slice(&data);
        self.pending_total = self.pending_total.saturating_add(added);

        if !more {
            // Final chunk: hand off to the sink. Always drain the
            // pending entry (success or fail) so memory accounting
            // stays accurate.
            let pending = self.pending.remove(&id).expect("just inserted");
            self.pending_total = self.pending_total.saturating_sub(pending.bytes.len());
            sink.register_asset(id, pending.format, pending.name, pending.bytes);
        }
    }

    fn drop_pending(&mut self, id: u32) {
        if let Some(p) = self.pending.remove(&id) {
            self.pending_total = self.pending_total.saturating_sub(p.bytes.len());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal test sink: records every dispatched call.
    #[derive(Default)]
    struct RecordingSink {
        registered: Vec<(u32, RgpFormat, Option<String>, Vec<u8>)>,
        registered_by_path: Vec<(u32, RgpFormat, String)>,
        placed: Vec<(u32, RgpAnchor, RgpPlacementStyle)>,
        updated: Vec<(u32, RgpPlacementUpdate)>,
        deleted: Vec<Option<u32>>,
        replies: Vec<Vec<u8>>,
    }
    impl RgpSink for RecordingSink {
        fn register_asset(
            &mut self,
            id: u32,
            format: RgpFormat,
            name: Option<String>,
            bytes: Vec<u8>,
        ) -> bool {
            self.registered.push((id, format, name, bytes));
            true
        }
        fn register_asset_by_path(
            &mut self,
            id: u32,
            format: RgpFormat,
            name: String,
        ) -> bool {
            self.registered_by_path.push((id, format, name));
            true
        }
        fn place(&mut self, id: u32, anchor: RgpAnchor, style: RgpPlacementStyle) {
            self.placed.push((id, anchor, style));
        }
        fn update(&mut self, id: u32, update: RgpPlacementUpdate) {
            self.updated.push((id, update));
        }
        fn delete(&mut self, id: Option<u32>) {
            self.deleted.push(id);
        }
        fn queue_reply(&mut self, bytes: &[u8]) {
            self.replies.push(bytes.to_vec());
        }
    }

    fn run(handler: &mut RgpHandler, sink: &mut RecordingSink, payload: &[u8]) {
        handler.process(payload, sink).expect("parse ok");
    }

    #[test]
    fn support_query_queues_reply() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(&mut h, &mut s, b"ratty;g;s");
        assert_eq!(s.replies.len(), 1);
        assert!(s.replies[0].starts_with(b"\x1b_ratty;g;s;"));
    }

    #[test]
    fn place_dispatches_to_sink() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(&mut h, &mut s, b"ratty;g;p;id=1;row=5;col=10;w=3;h=2;ry=90");
        assert_eq!(s.placed.len(), 1);
        let (id, anchor, style) = &s.placed[0];
        assert_eq!(*id, 1);
        assert_eq!(anchor.row, 5);
        assert_eq!(anchor.col, 10);
        assert_eq!(anchor.cols, 3);
        assert_eq!(anchor.rows, 2);
        assert!((style.rotation[1] - 90.0).abs() < 1e-6);
    }

    #[test]
    fn path_register_dispatches_immediately() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(&mut h, &mut s, b"ratty;g;r;id=7;fmt=glb;path=Ferris.glb");
        assert_eq!(s.registered_by_path.len(), 1);
        let (id, fmt, name) = &s.registered_by_path[0];
        assert_eq!(*id, 7);
        assert_eq!(*fmt, RgpFormat::Glb);
        assert_eq!(name, "Ferris.glb");
        assert!(s.registered.is_empty());
    }

    #[test]
    fn payload_register_in_one_shot_dispatches_immediately() {
        // base64 "abc" = "YWJj"
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=1;fmt=glb;source=payload;more=0;YWJj",
        );
        assert_eq!(s.registered.len(), 1);
        assert_eq!(s.registered[0].3, b"abc".to_vec());
        assert_eq!(h.pending_count(), 0);
    }

    #[test]
    fn chunked_payload_reassembles_across_packets() {
        // base64 of "hello world" split: "hello " = "aGVsbG8g",
        // "world" = "d29ybGQ=". Send as two packets.
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=42;fmt=glb;source=payload;more=1;aGVsbG8g",
        );
        // No dispatch yet — we're waiting for `more=0`.
        assert!(s.registered.is_empty());
        assert_eq!(h.pending_count(), 1);
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=42;fmt=glb;source=payload;more=0;d29ybGQ=",
        );
        assert_eq!(s.registered.len(), 1);
        assert_eq!(s.registered[0].3, b"hello world".to_vec());
        assert_eq!(h.pending_count(), 0);
        assert_eq!(h.pending_bytes(), 0);
    }

    #[test]
    fn per_id_cap_drops_overlarge_upload_without_orphaning_state() {
        let mut h = RgpHandler::new();
        // 5 bytes: rejects "hello " (6 bytes) but accepts "world"
        // (5 bytes) as a fresh one-shot.
        h.per_id_cap = 5;
        let mut s = RecordingSink::default();
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=1;fmt=glb;source=payload;more=1;aGVsbG8g",
        );
        // Upload was rejected on entry; nothing pending.
        assert_eq!(h.pending_count(), 0);
        assert_eq!(h.pending_bytes(), 0);
        // A fresh `more=0` packet under the cap registers cleanly —
        // the rejected first packet left no orphan state behind.
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=1;fmt=glb;source=payload;more=0;d29ybGQ=",
        );
        assert_eq!(s.registered.len(), 1);
        assert_eq!(s.registered[0].3, b"world".to_vec());
    }

    #[test]
    fn delete_one_and_all_route_correctly() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(&mut h, &mut s, b"ratty;g;d;id=3");
        run(&mut h, &mut s, b"ratty;g;d");
        assert_eq!(s.deleted, vec![Some(3), None]);
    }

    #[test]
    fn update_carries_sparse_fields() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        run(&mut h, &mut s, b"ratty;g;u;id=1;ry=45;animate=1");
        assert_eq!(s.updated.len(), 1);
        let (id, upd) = &s.updated[0];
        assert_eq!(*id, 1);
        assert_eq!(upd.animate, Some(true));
        assert_eq!(upd.rotation[1], Some(45.0));
        // Unset fields stay None.
        assert!(upd.rotation[0].is_none());
        assert!(upd.scale.is_none());
    }

    #[test]
    fn path_register_drops_pending_chunked_upload_for_same_id() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        // Start a chunked payload upload for id=5...
        run(
            &mut h,
            &mut s,
            b"ratty;g;r;id=5;fmt=glb;source=payload;more=1;YWJj",
        );
        assert_eq!(h.pending_count(), 1);
        // ...then send a path-based register for the same id.
        run(&mut h, &mut s, b"ratty;g;r;id=5;fmt=glb;path=foo.glb");
        // Pending must be cleared (no orphan memory).
        assert_eq!(h.pending_count(), 0);
        assert_eq!(s.registered_by_path.len(), 1);
    }

    #[test]
    fn non_rgp_payload_returns_not_rgp_error() {
        let mut h = RgpHandler::new();
        let mut s = RecordingSink::default();
        let err = h.process(b"Ga=T,f=24,s=1,v=1;AAAA", &mut s).unwrap_err();
        assert_eq!(err, RgpParseError::NotRgp);
    }
}
