//! Cross-cutting Term tests. Drives `Term` through the real parser, so
//! these double as end-to-end coverage of parser → term wiring.

use proptest::prelude::*;
use toastty_parser::Parser;
use toastty_term::Term;

fn run(rows: u16, cols: u16, bytes: &[u8]) -> Term {
    let mut t = Term::new(rows, cols, 32);
    let mut p = Parser::new();
    p.advance(&mut t, bytes);
    t
}

fn row_text(t: &Term, r: u16) -> String {
    let mut s: String = t.row(r).cells.iter().map(|c| c.ch).collect();
    while s.ends_with(' ') {
        s.pop();
    }
    s
}

#[test]
fn shell_prompt_like_sequence_lands_correctly() {
    // Roughly what `bash -c 'printf "\033[31mfoo\033[0m\n"'` would produce.
    let bytes = b"\x1b[31mfoo\x1b[0m\r\nbar";
    let t = run(4, 16, bytes);
    assert_eq!(row_text(&t, 0), "foo");
    assert_eq!(row_text(&t, 1), "bar");
}

#[test]
fn cursor_jump_then_overwrite() {
    // CUP to (2, 5), then print — exercises the cursor-move + print pair.
    let bytes = b"hello\x1b[2;5Hworld";
    let t = run(3, 10, bytes);
    assert_eq!(row_text(&t, 0), "hello");
    // 1-based col 5 = 0-based col 4; "world" starts there.
    assert_eq!(row_text(&t, 1).trim_end(), "    world");
}

#[test]
fn cup_with_only_row_specified_homes_column() {
    // `CSI 3 H` — row 3 col 1 (column defaults).
    let bytes = b"...\x1b[3HZ";
    let t = run(5, 4, bytes);
    assert_eq!(t.row(2).cells[0].ch, 'Z');
}

#[test]
fn alt_screen_isolates_writes_from_primary() {
    let mut t = Term::new(3, 4, 8);
    let mut p = Parser::new();
    p.advance(&mut t, b"ABCD\r\nWXYZ\x1b[?1049hHIDDEN\x1b[?1049l");
    // Primary content untouched.
    assert_eq!(row_text(&t, 0), "ABCD");
    assert_eq!(row_text(&t, 1).trim_end(), "WXYZ");
    // No "HIDDEN" visible anywhere.
    let combined: String = (0..3)
        .map(|r| row_text(&t, r))
        .collect::<Vec<_>>()
        .join("|");
    assert!(!combined.contains("HIDDEN"), "got: {combined}");
}

#[test]
fn long_run_of_text_scrolls_primary() {
    // 5 lines into a 3-row terminal: 3 LFs from the bottom row cause
    // 3 scrolls. After A\r\n B\r\n C\r\n D\r\n E\r\n we end up with
    // visible D, E, "" (the trailing LF scrolled past 'E', leaving a
    // blank bottom row).
    let mut t = Term::new(3, 4, 16);
    let mut p = Parser::new();
    for n in 0..5u8 {
        p.advance(&mut t, &[b'A' + n, b'\r', b'\n']);
    }
    assert_eq!(row_text(&t, 0), "D");
    assert_eq!(row_text(&t, 1), "E");
    assert_eq!(row_text(&t, 2), "");
}

#[test]
fn split_input_across_advance_calls() {
    // Same bytes via one big advance and via byte-at-a-time should land
    // in the same on-screen state.
    let bytes: &[u8] = b"\x1b[31mhi\x1b[0m\r\nthere";
    let bulk = run(3, 16, bytes);

    let mut byte = Term::new(3, 16, 32);
    let mut p = Parser::new();
    for &b in bytes {
        p.advance(&mut byte, &[b]);
    }

    for r in 0..3 {
        assert_eq!(row_text(&bulk, r), row_text(&byte, r));
    }
    assert_eq!(bulk.cursor(), byte.cursor());
}

// ---- M12a: end-to-end APC → RGP scene wiring ----
//
// These tests drive the *real* Parser + Term combination, so they
// double as coverage for the APC demux in `Term::apc_end` (Kitty vs
// RGP) plus the `RgpHandler` → `RgpSink for Term` plumbing.

/// Build an APC packet around an RGP body: `ESC _ ratty;g;<body> ESC \`.
fn rgp_apc(body: &str) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(b"\x1b_ratty;g;");
    v.extend_from_slice(body.as_bytes());
    v.extend_from_slice(b"\x1b\\");
    v
}

#[test]
fn rgp_support_query_queues_reply_through_term() {
    let bytes = rgp_apc("s");
    let mut t = Term::new(4, 16, 0);
    let mut p = Parser::new();
    p.advance(&mut t, &bytes);
    let reply = t.drain_pty_replies();
    assert!(
        reply.starts_with(b"\x1b_ratty;g;s;"),
        "reply must be APC-framed support response: {reply:?}"
    );
    assert!(reply.ends_with(b"\x1b\\"));
}

#[test]
fn rgp_place_through_term_lands_in_scene() {
    let bytes = rgp_apc("p;id=1;row=2;col=3;w=4;h=2;ry=45");
    let mut t = Term::new(8, 16, 0);
    let mut p = Parser::new();
    p.advance(&mut t, &bytes);
    let scene = t.rgp_scene();
    let placement = scene.placement(1).expect("placement must exist");
    assert_eq!(placement.anchor.row, 2);
    assert_eq!(placement.anchor.col, 3);
    assert_eq!(placement.anchor.cols, 4);
    assert_eq!(placement.anchor.rows, 2);
    assert!((placement.style.rotation[1] - 45.0).abs() < 1e-6);
    assert!(t.rgp_revision() > 0);
}

#[test]
fn rgp_register_then_place_then_update_then_delete_through_term() {
    let mut t = Term::new(8, 16, 0);
    let mut p = Parser::new();
    // Register via the bundled cube — exercises `path=` resolution.
    p.advance(&mut t, &rgp_apc("r;id=1;fmt=glb;path=cube"));
    p.advance(&mut t, &rgp_apc("p;id=1;row=0;col=0;w=2;h=2"));
    p.advance(&mut t, &rgp_apc("u;id=1;brightness=0.5"));

    let scene = t.rgp_scene();
    let asset = scene.asset(1).expect("asset registered");
    assert_eq!(asset.name.as_deref(), Some("cube"));
    assert_eq!(asset.data.mesh.positions.len(), 24, "cube has 24 vertices");
    let p1 = scene.placement(1).expect("placement set");
    assert!((p1.style.brightness - 0.5).abs() < 1e-6);

    // Delete one — placement gone, asset stays.
    p.advance(&mut t, &rgp_apc("d;id=1"));
    assert!(t.rgp_scene().placement(1).is_none());
    assert!(t.rgp_scene().asset(1).is_some(), "delete-one keeps asset");

    // Delete all — asset gone too.
    p.advance(&mut t, &rgp_apc("d"));
    assert!(t.rgp_scene().asset(1).is_none());
}

#[test]
fn rgp_path_register_with_unknown_name_does_not_register() {
    let mut t = Term::new(4, 16, 0);
    let mut p = Parser::new();
    // Leaf name not in the embedded bundle and no asset_dir
    // configured → resolver returns NotFound → no asset registered.
    p.advance(&mut t, &rgp_apc("r;id=1;fmt=glb;path=nope"));
    assert!(t.rgp_scene().asset(1).is_none());
}

#[test]
fn rgp_path_register_rejects_paths_with_separators() {
    let mut t = Term::new(4, 16, 0);
    let mut p = Parser::new();
    // Decision §1: leaf-only. A path containing `/` must be
    // rejected by the resolver without any I/O attempt.
    p.advance(&mut t, &rgp_apc("r;id=1;fmt=glb;path=../etc/passwd"));
    assert!(t.rgp_scene().asset(1).is_none());
}

#[test]
fn apc_demux_routes_kitty_and_rgp_to_their_own_handlers() {
    let mut t = Term::new(8, 16, 0);
    let mut p = Parser::new();
    // Kitty query packet — should NOT touch rgp_scene.
    p.advance(&mut t, b"\x1b_Ga=q,i=1,s=1,v=1;AAAA\x1b\\");
    assert_eq!(t.rgp_revision(), 0, "Kitty packet must not bump RGP revision");
    // Then an RGP packet — must NOT show up in image registry/grid.
    p.advance(&mut t, &rgp_apc("p;id=99;row=1;col=1;w=2;h=2"));
    assert!(t.rgp_scene().placement(99).is_some());
    assert!(t.image_grid().is_empty(), "RGP packet must not place a Kitty image");
}

#[test]
fn rgp_chunked_payload_register_reassembles_through_term() {
    use base64::Engine;
    let mut t = Term::new(4, 16, 0);
    let mut p = Parser::new();
    // Build a known-good .glb, base64 it, split into two halves,
    // and send as a chunked payload register.
    let glb_bytes = toastty_graphics::rgp::glb_loader::minimal_triangle_glb();
    let encoded = base64::engine::general_purpose::STANDARD.encode(&glb_bytes);
    let mid = encoded.len() / 2;
    let (first, second) = encoded.split_at(mid);

    p.advance(
        &mut t,
        &rgp_apc(&format!(
            "r;id=7;fmt=glb;source=payload;more=1;{first}"
        )),
    );
    // Asset must NOT exist yet — we're mid-upload.
    assert!(t.rgp_scene().asset(7).is_none());
    p.advance(
        &mut t,
        &rgp_apc(&format!(
            "r;id=7;fmt=glb;source=payload;more=0;{second}"
        )),
    );
    let asset = t
        .rgp_scene()
        .asset(7)
        .expect("asset registered on final chunk");
    // Loader parsed the triangle's three positions.
    assert_eq!(asset.data.mesh.positions.len(), 3);
}

proptest! {
    /// Any random byte stream must not panic the Term/parser stack.
    /// Mirrors the parser-level property (parser/tests/integration.rs).
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut t = Term::new(8, 16, 64);
        let mut p = Parser::new();
        p.advance(&mut t, &bytes);
    }

    /// Same property with arbitrary chunking boundaries — the parser
    /// buffers state across calls and the Term must remain consistent.
    #[test]
    fn arbitrary_chunked_bytes_never_panic(
        bytes in proptest::collection::vec(any::<u8>(), 0..1024),
        split_at in 0usize..1024,
    ) {
        let split = split_at.min(bytes.len());
        let mut t = Term::new(8, 16, 64);
        let mut p = Parser::new();
        p.advance(&mut t, &bytes[..split]);
        p.advance(&mut t, &bytes[split..]);
    }

    /// Resize at any time, with any rows/cols (including tiny), must
    /// not panic and must leave the cursor inside the new viewport.
    #[test]
    fn arbitrary_resize_keeps_cursor_in_bounds(
        bytes in proptest::collection::vec(any::<u8>(), 0..512),
        new_rows in 1u16..32,
        new_cols in 1u16..40,
    ) {
        let mut t = Term::new(8, 16, 16);
        let mut p = Parser::new();
        p.advance(&mut t, &bytes);
        t.resize(new_rows, new_cols);
        let (r, c) = t.size();
        let cur = t.cursor();
        prop_assert!(cur.row < r);
        prop_assert!(cur.col <= c, "col {} > cols {}", cur.col, c);
    }
}
