//! Reply encoding for the RGP `s` (support query) verb.
//!
//! The capability string lists every field the implementation honors.
//! See `docs/decisions/rgp-protocol.md` for the policy choices behind
//! each capability (notably the permissive v1 path policy).

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
/// - `fmt=obj|glb` — Wavefront OBJ + binary glTF.
/// - `path=1` — `path=` resolves against the foreground process's
///   CWD (permissive v1 policy; see decision §1 amendment).
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
        b"s;v=1;fmt=obj|glb;path=1;payload=1;chunk=1;anim=1;\
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
        for cap in [
            "v=1",
            "fmt=obj|glb",
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
