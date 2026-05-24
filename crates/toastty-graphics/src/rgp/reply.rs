//! Reply encoding for the RGP `s` (support query) verb.
//!
//! The capability string lists every field the implementation honors.
//! See `docs/decisions/rgp-protocol.md` for which capabilities v1
//! actually advertises (notably: `path=1` is advertised because the
//! field is supported as a leaf-name lookup; `obj` is NOT in `fmt=`
//! until the OBJ loader ships).

/// Wrap an RGP payload body in the APC framing (`ESC _ ratty;g;…ESC \`).
///
/// Public so tests and the demo script can use the same encoder.
#[must_use]
pub fn frame_apc(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + b"\x1b_ratty;g;".len() + 2);
    out.extend_from_slice(b"\x1b_ratty;g;");
    out.extend_from_slice(body);
    out.extend_from_slice(b"\x1b\\");
    out
}

/// The capability reply for the `s` verb, fully framed.
///
/// v1 capabilities:
/// - `v=1` — protocol version.
/// - `fmt=glb` — GLB only (OBJ deferred to v1.1).
/// - `path=1` — leaf-name lookup against the embedded asset bundle
///   plus the optional user `asset_dir` config (decision §1). NOT
///   arbitrary disk reads.
/// - `payload=1`, `chunk=1` — inline base64 with multi-packet
///   chunking.
/// - `anim=1` — `animate=1` produces a default spin.
/// - `depth=1` — `depth=<f32>` is honored (decision §3 mapping).
/// - `color=1`, `brightness=1` — modulate the lit fragment color.
/// - `transform=1` — `px/py/pz`, `rx/ry/rz`, `sx/sy/sz` honored.
/// - `update=1` — the `u` verb is supported.
#[must_use]
pub fn support_reply() -> Vec<u8> {
    frame_apc(
        b"s;v=1;fmt=glb;path=1;payload=1;chunk=1;anim=1;\
          depth=1;color=1;brightness=1;transform=1;update=1",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_reply_is_apc_framed() {
        let r = support_reply();
        assert!(r.starts_with(b"\x1b_ratty;g;s;"));
        assert!(r.ends_with(b"\x1b\\"));
    }

    #[test]
    fn support_reply_includes_every_advertised_capability() {
        let r = support_reply();
        let s = std::str::from_utf8(&r).unwrap();
        // Negative: OBJ is NOT advertised.
        assert!(!s.contains("obj"), "v1 must not advertise fmt=obj: {s}");
        // Positive: every cap in decision §1 of rgp-protocol.md.
        for cap in [
            "v=1",
            "fmt=glb",
            "path=1",
            "payload=1",
            "chunk=1",
            "anim=1",
            "depth=1",
            "color=1",
            "brightness=1",
            "transform=1",
            "update=1",
        ] {
            assert!(s.contains(cap), "missing capability `{cap}` in `{s}`");
        }
    }
}
