//! RGP wire-protocol emitters.
//!
//! Frame: `ESC _ ratty;g;<verb>;<k=v>;... ESC \`  (i.e. `\x1b_ratty;g;…\x1b\\`).
//! Verbs used: `r` (register), `p` (place), `u` (update), `d` (delete).
//!
//! Register via inline payload, chunked (base64 in ~4 KiB pieces):
//! `r;id=N;fmt=glb;source=payload;more=1;<b64-chunk>` for all but the
//! last chunk, `more=0` on the final chunk. The base64 payload comes
//! LAST in the field list (toastty's parser treats the first bare /
//! unknown token under `source=payload` as the chunk). Use STANDARD
//! base64. Each chunk is its own complete APC frame.
//!
//! Place fields: `id,row,col,w,h` required; optional `depth,scale,
//! color=RRGGBB,animate=0|1,rx,ry,rz,px,py,pz,sx,sy,sz`. row/col are the
//! 1-based CENTER cell; w/h are the cell span.

use std::io::{self, Write};

use base64::Engine as _;

/// APC introducer: ESC `_`
const APC_BEG: &[u8] = b"\x1b_";
/// APC terminator: ESC `\`
const APC_END: &[u8] = b"\x1b\\";

/// Maximum base64 chunk size (bytes of base64 text, not raw bytes).
/// ~4 KiB keeps each APC frame comfortably under terminal buffer limits.
///
/// LOAD-BEARING: toastty decodes each register frame's base64 payload
/// INDEPENDENTLY (`toastty-graphics` `operation.rs::parse_register`, then
/// the handler concatenates the decoded *bytes*) — so every chunk must be
/// valid base64 on its own, i.e. its length must be a multiple of 4. The
/// full base64 string length is always a multiple of 4, so a chunk size
/// that is a multiple of 4 guarantees the final remainder chunk is too.
/// Do NOT change this to a non-multiple of 4 without reworking chunking.
const CHUNK_B64: usize = 4096;
const _: () = assert!(
    CHUNK_B64.is_multiple_of(4),
    "CHUNK_B64 must be a multiple of 4 (per-frame base64 decode)"
);

/// Anchor + transform for a placement. row/col = 1-based center cell.
#[derive(Debug, Clone, Copy)]
pub struct Placement {
    pub row: u16,
    pub col: u16,
    pub w: u16,
    pub h: u16,
    pub depth: f32,
    pub scale: f32,
    pub rx: f32,
    pub ry: f32,
    pub rz: f32,
    pub color: [u8; 3],
    pub animate: bool,
}

/// Register a GLB asset under `id` via chunked base64 payload
/// (`more=1` … `more=0`). Each chunk is emitted as its own APC frame.
pub fn register_payload<W: Write>(w: &mut W, id: u32, glb: &[u8]) -> io::Result<()> {
    // Encode the entire payload to base64 first.
    let b64 = base64::engine::general_purpose::STANDARD.encode(glb);

    // Split into chunks of at most CHUNK_B64 bytes of base64 text.
    let chunks: Vec<&str> = if b64.is_empty() {
        vec![""]
    } else {
        b64.as_bytes()
            .chunks(CHUNK_B64)
            .map(|c| {
                // SAFETY: base64 output is always ASCII.
                std::str::from_utf8(c).expect("base64 is always ASCII")
            })
            .collect()
    };

    let total = chunks.len();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = u8::from(i + 1 < total);
        w.write_all(APC_BEG)?;
        write!(
            w,
            "ratty;g;r;id={id};fmt=glb;source=payload;more={more};{chunk}"
        )?;
        w.write_all(APC_END)?;
    }

    Ok(())
}

/// Place a registered asset (`p` verb).
pub fn place<W: Write>(w: &mut W, id: u32, p: &Placement) -> io::Result<()> {
    let [cr, cg, cb] = p.color;
    let animate = u8::from(p.animate);
    w.write_all(APC_BEG)?;
    write!(
        w,
        "ratty;g;p;id={id};row={row};col={col};w={w};h={h};depth={depth};scale={scale};rx={rx};ry={ry};rz={rz};color={cr:02x}{cg:02x}{cb:02x};animate={animate}",
        row = p.row,
        col = p.col,
        w = p.w,
        h = p.h,
        depth = p.depth,
        scale = p.scale,
        rx = p.rx,
        ry = p.ry,
        rz = p.rz,
    )?;
    w.write_all(APC_END)?;
    Ok(())
}

/// Update rotation + uniform scale of a placement (`u` verb). Used by
/// the drag-rotate / scroll-zoom path — must NOT re-register geometry.
pub fn update_transform<W: Write>(
    w: &mut W,
    id: u32,
    rx: f32,
    ry: f32,
    rz: f32,
    scale: f32,
) -> io::Result<()> {
    w.write_all(APC_BEG)?;
    write!(w, "ratty;g;u;id={id};rx={rx};ry={ry};rz={rz};scale={scale}")?;
    w.write_all(APC_END)?;
    Ok(())
}

/// Toggle a placement's default animation (`u;id=..;animate=0|1`).
pub fn set_animate<W: Write>(w: &mut W, id: u32, on: bool) -> io::Result<()> {
    let animate = u8::from(on);
    w.write_all(APC_BEG)?;
    write!(w, "ratty;g;u;id={id};animate={animate}")?;
    w.write_all(APC_END)?;
    Ok(())
}

/// Delete all RGP state (`d` with no id).
pub fn delete_all<W: Write>(w: &mut W) -> io::Result<()> {
    w.write_all(APC_BEG)?;
    w.write_all(b"ratty;g;d")?;
    w.write_all(APC_END)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ------------------------------------------------------------------ helpers

    fn run<F: FnOnce(&mut Vec<u8>) -> io::Result<()>>(f: F) -> Vec<u8> {
        let mut buf = Vec::new();
        f(&mut buf).expect("write failed");
        buf
    }

    // ------------------------------------------------------------------ delete_all

    #[test]
    fn test_delete_all_exact() {
        let out = run(|w| delete_all(w));
        assert_eq!(out, b"\x1b_ratty;g;d\x1b\\");
    }

    // ------------------------------------------------------------------ place

    #[test]
    fn test_place_frame() {
        let p = Placement {
            row: 5,
            col: 10,
            w: 8,
            h: 4,
            depth: 0.0,
            scale: 1.0,
            rx: 0.0,
            ry: 0.0,
            rz: 0.0,
            color: [0xff, 0x88, 0x44],
            animate: false,
        };
        let out = run(|w| place(w, 7, &p));
        let s = std::str::from_utf8(&out).unwrap();

        assert!(
            s.starts_with("\x1b_ratty;g;p;id=7;"),
            "starts with prefix: {s:?}"
        );
        assert!(s.contains("color=ff8844"), "contains color=ff8844: {s:?}");
        assert!(s.contains("animate=0"), "contains animate=0: {s:?}");
        assert!(s.ends_with("\x1b\\"), "ends with ST: {s:?}");
    }

    #[test]
    fn test_place_animate_on() {
        let p = Placement {
            row: 1,
            col: 1,
            w: 4,
            h: 4,
            depth: 0.5,
            scale: 2.0,
            rx: 10.0,
            ry: 20.0,
            rz: 30.0,
            color: [0x00, 0x00, 0x00],
            animate: true,
        };
        let out = run(|w| place(w, 42, &p));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("animate=1"), "animate should be 1: {s:?}");
        assert!(s.contains("id=42"), "id=42: {s:?}");
        assert!(s.contains("color=000000"), "color=000000: {s:?}");
    }

    // ------------------------------------------------------------------ register_payload

    #[test]
    fn test_register_tiny_single_frame() {
        // A tiny payload → exactly one frame with more=0.
        let glb = b"hello world";
        let out = run(|w| register_payload(w, 3, glb));
        let s = std::str::from_utf8(&out).unwrap();

        // Count frames by counting APC_BEG occurrences.
        let frame_count = s.matches("\x1b_").count();
        assert_eq!(frame_count, 1, "expected 1 frame, got {frame_count}: {s:?}");
        assert!(s.starts_with("\x1b_ratty;g;r;id="), "frame prefix: {s:?}");
        assert!(s.contains("more=0"), "tiny payload must have more=0: {s:?}");
        assert!(s.ends_with("\x1b\\"), "ends with ST: {s:?}");
    }

    #[test]
    fn test_register_empty_single_frame() {
        let out = run(|w| register_payload(w, 1, b""));
        let s = std::str::from_utf8(&out).unwrap();
        let frame_count = s.matches("\x1b_").count();
        assert_eq!(frame_count, 1, "empty payload → 1 frame: {s:?}");
        assert!(
            s.contains("more=0"),
            "empty payload must have more=0: {s:?}"
        );
        assert!(s.ends_with("\x1b\\"), "ends with ST: {s:?}");
    }

    #[test]
    fn test_register_large_chunked() {
        // 10 KiB of raw bytes → base64 expands to ~13.7 KiB → needs >1 chunk
        // at CHUNK_B64=4096 bytes per chunk (ceil(13653/4096) = 4 chunks).
        let glb = vec![0xabu8; 10 * 1024];
        let out = run(|w| register_payload(w, 99, &glb));
        let s = std::str::from_utf8(&out).unwrap();

        // Split on APC_END + APC_BEG boundary to get frames.
        let frames: Vec<&str> = s.split("\x1b\\").filter(|f| !f.is_empty()).collect();
        let frame_count = frames.len();
        assert!(
            frame_count > 1,
            "10 KiB payload must produce >1 frame, got {frame_count}"
        );

        for (i, frame) in frames.iter().enumerate() {
            assert!(
                frame.starts_with("\x1b_ratty;g;r;id="),
                "frame {i} must start with prefix: {frame:?}"
            );
            assert!(
                frame.ends_with('\0') || !frame.ends_with("\x1b\\"),
                // Frames from splitting on \x1b\\ won't end with it.
            );
            if i + 1 < frame_count {
                assert!(
                    frame.contains("more=1"),
                    "non-last frame {i} must contain more=1: {frame:?}"
                );
            } else {
                assert!(
                    frame.contains("more=0"),
                    "last frame must contain more=0: {frame:?}"
                );
            }
        }

        // Verify round-trip: all frames start with the expected prefix.
        for frame in &frames {
            assert!(
                frame.starts_with("\x1b_ratty;g;r;id="),
                "frame prefix check: {frame:?}"
            );
        }
    }

    // ------------------------------------------------------------------ update_transform

    #[test]
    fn test_update_transform() {
        let out = run(|w| update_transform(w, 5, 10.0, 20.0, 30.0, 1.5));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("\x1b_ratty;g;u;id=5;"), "prefix: {s:?}");
        assert!(s.contains("rx=10"), "rx: {s:?}");
        assert!(s.contains("ry=20"), "ry: {s:?}");
        assert!(s.contains("rz=30"), "rz: {s:?}");
        assert!(s.contains("scale=1.5"), "scale: {s:?}");
        assert!(s.ends_with("\x1b\\"), "ST: {s:?}");
    }

    // ------------------------------------------------------------------ set_animate

    #[test]
    fn test_set_animate_on() {
        let out = run(|w| set_animate(w, 2, true));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.starts_with("\x1b_ratty;g;u;id=2;"), "prefix: {s:?}");
        assert!(s.contains("animate=1"), "animate=1: {s:?}");
        assert!(s.ends_with("\x1b\\"), "ST: {s:?}");
    }

    #[test]
    fn test_set_animate_off() {
        let out = run(|w| set_animate(w, 2, false));
        let s = std::str::from_utf8(&out).unwrap();
        assert!(s.contains("animate=0"), "animate=0: {s:?}");
    }
}
