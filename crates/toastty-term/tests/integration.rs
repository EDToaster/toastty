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
fn rgp_path_register_with_missing_file_does_not_register() {
    let mut t = Term::new(4, 16, 0);
    let mut p = Parser::new();
    // v1 policy is permissive (any relative or absolute path,
    // resolved from CWD). A file that doesn't exist still fails
    // gracefully — no asset registered, no panic. See decision §1.
    p.advance(
        &mut t,
        &rgp_apc("r;id=1;fmt=glb;path=definitely/not/here.glb"),
    );
    assert!(t.rgp_scene().asset(1).is_none());
}

#[test]
fn apc_demux_routes_kitty_and_rgp_to_their_own_handlers() {
    let mut t = Term::new(8, 16, 0);
    let mut p = Parser::new();
    // Kitty query packet — should NOT touch rgp_scene.
    p.advance(&mut t, b"\x1b_Ga=q,i=1,s=1,v=1;AAAA\x1b\\");
    assert_eq!(
        t.rgp_revision(),
        0,
        "Kitty packet must not bump RGP revision"
    );
    // Then an RGP packet — must NOT show up in image registry/grid.
    p.advance(&mut t, &rgp_apc("p;id=99;row=1;col=1;w=2;h=2"));
    assert!(t.rgp_scene().placement(99).is_some());
    assert!(
        t.image_grid().is_empty(),
        "RGP packet must not place a Kitty image"
    );
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
        &rgp_apc(&format!("r;id=7;fmt=glb;source=payload;more=1;{first}")),
    );
    // Asset must NOT exist yet — we're mid-upload.
    assert!(t.rgp_scene().asset(7).is_none());
    p.advance(
        &mut t,
        &rgp_apc(&format!("r;id=7;fmt=glb;source=payload;more=0;{second}")),
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

mod selection {
    use super::*;
    use toastty_term::{Pos, Selection, SelectionMode};

    /// Build a fresh Term, write some lines, and return it. Lines are
    /// terminated by CRLF so the cursor advances and wraps naturally.
    fn term_with_lines(rows: u16, cols: u16, lines: &[&str]) -> Term {
        let mut t = Term::new(rows, cols, 64);
        let mut p = Parser::new();
        for (i, line) in lines.iter().enumerate() {
            p.advance(&mut t, line.as_bytes());
            if i + 1 < lines.len() {
                p.advance(&mut t, b"\r\n");
            }
        }
        t
    }

    fn pos_for(t: &Term, row: u16, col: u16) -> Pos {
        // Translate visible-row index to live line_id.
        let (rows, _) = t.size();
        let bottom = t.bottom_id();
        let delta = u64::from(rows.saturating_sub(1) - row);
        Pos::new(bottom - delta, col)
    }

    #[test]
    fn extract_char_single_row() {
        let t = term_with_lines(4, 16, &["hello world"]);
        // "hello world" is on visible row 0 because the writes happen
        // before any LF — cursor sits on row 0 throughout.
        let mut sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 0, 4));
        let mut t = t;
        t.set_selection(sel);
        assert_eq!(t.extract_selection_text().as_deref(), Some("hello"));
    }

    #[test]
    fn extract_char_multi_row_joins_with_newline() {
        let t = term_with_lines(4, 16, &["foo", "bar", "baz"]);
        let mut sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 2, 2));
        let mut t = t;
        t.set_selection(sel);
        assert_eq!(t.extract_selection_text().as_deref(), Some("foo\nbar\nbaz"),);
    }

    #[test]
    fn extract_strips_trailing_whitespace_per_row() {
        // Each row gets padded with blanks out to `cols`; the extractor
        // must strip them.
        let t = term_with_lines(4, 16, &["hi", "world"]);
        let mut sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 1, 15));
        let mut t = t;
        t.set_selection(sel);
        assert_eq!(t.extract_selection_text().as_deref(), Some("hi\nworld"));
    }

    #[test]
    fn extract_block_rectangle() {
        let t = term_with_lines(4, 16, &["abcdefgh", "ijklmnop", "qrstuvwx"]);
        let mut sel = Selection::new(pos_for(&t, 0, 2), SelectionMode::Block);
        sel.set_active(pos_for(&t, 2, 4));
        let mut t = t;
        t.set_selection(sel);
        // Block: cols [2..=4] on each of rows 0..=2.
        assert_eq!(t.extract_selection_text().as_deref(), Some("cde\nklm\nstu"),);
    }

    #[test]
    fn extract_soft_wrap_joins_without_newline() {
        // Cols = 4 so "hello world" wraps as "hell" + "o wo" + "rld".
        // First two rows should soft_wrap = true; we expect the joined
        // text to be the original string for char-mode selection.
        let mut t = Term::new(4, 4, 32);
        let mut p = Parser::new();
        p.advance(&mut t, b"hello world");
        let mut sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 2, 3));
        t.set_selection(sel);
        assert_eq!(t.extract_selection_text().as_deref(), Some("hello world"),);
    }

    #[test]
    fn selection_clears_on_resize() {
        let mut t = term_with_lines(4, 16, &["hello"]);
        let sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Char);
        t.set_selection(sel);
        assert!(t.selection().is_some());
        t.resize(5, 16);
        assert!(t.selection().is_none());
    }

    /// Select `(r0, c0)..=(r1, c1)` (visible-row coordinates) in `mode`.
    fn select(t: &mut Term, mode: SelectionMode, (r0, c0): (u16, u16), (r1, c1): (u16, u16)) {
        let mut sel = Selection::new(pos_for(t, r0, c0), mode);
        sel.set_active(pos_for(t, r1, c1));
        t.set_selection(sel);
    }

    fn feed(t: &mut Term, bytes: &[u8]) {
        Parser::new().advance(t, bytes);
    }

    #[test]
    fn selection_works_on_alt_screen() {
        // Audit #3: less / man / vim (mouse=) run on the alt screen.
        let mut t = term_with_lines(4, 16, &["primary"]);
        select(&mut t, SelectionMode::Char, (0, 0), (0, 3));
        feed(&mut t, b"\x1b[?1049h");
        assert!(t.is_alt_active());
        // Entering alt drops the primary selection.
        assert!(t.selection().is_none());
        feed(&mut t, b"\x1b[2;1Halt one\r\nalt two");
        select(&mut t, SelectionMode::Char, (1, 0), (2, 2));
        assert!(t.selection().is_some());
        assert_eq!(t.extract_selection_text().as_deref(), Some("alt one\nalt"));
        let id = pos_for(&t, 1, 4).line_id;
        assert!(t.is_cell_selected(id, 4));
        assert!(!t.is_cell_selected(pos_for(&t, 0, 0).line_id, 0));
        // Row lookup addresses the alt grid while it's active…
        let row = t.row_by_line_id(id).unwrap();
        assert_eq!(row.cells[0].ch, 'a');
        // …and the primary accessor still reaches the primary grid.
        assert_eq!(
            t.primary_row_by_line_id(pos_for(&t, 0, 0).line_id)
                .unwrap()
                .cells[0]
                .ch,
            'p'
        );
        // Leaving alt drops the alt selection — its ids mean nothing on
        // the primary grid.
        feed(&mut t, b"\x1b[?1049l");
        assert!(t.selection().is_none());
        assert!(!t.is_cell_selected(id, 4));
    }

    #[test]
    fn alt_screen_word_and_line_bounds() {
        let mut t = Term::new(4, 8, 16);
        feed(&mut t, b"\x1b[?1049hfoo bar baz");
        let at = pos_for(&t, 1, 1); // "baz" wrapped onto row 1 → 'a'
        let (lo, hi) = t.word_bounds(at, char::is_alphanumeric).unwrap();
        assert_eq!((lo.col, hi.col), (0, 2));
        let (a, b) = t.logical_line_bounds(at.line_id).unwrap();
        assert_eq!((a, b), (pos_for(&t, 0, 0).line_id, at.line_id));
    }

    #[test]
    fn soft_wrap_keeps_spaces_at_wrap_boundary() {
        // Audit #7: `abcd efgh` on 5 columns wraps right after the
        // space; the space is real content, not row padding.
        let mut t = Term::new(4, 5, 16);
        feed(&mut t, b"abcd efgh");
        select(&mut t, SelectionMode::Char, (0, 0), (1, 4));
        assert_eq!(t.extract_selection_text().as_deref(), Some("abcd efgh"));

        let mut t = Term::new(4, 5, 16);
        feed(&mut t, b"ab   cd");
        select(&mut t, SelectionMode::Char, (0, 0), (1, 4));
        assert_eq!(t.extract_selection_text().as_deref(), Some("ab   cd"));
    }

    #[test]
    fn hard_line_end_and_selection_end_are_trimmed() {
        let mut t = Term::new(4, 5, 16);
        feed(&mut t, b"ab   cd\r\nx");
        // Selection ending on the soft-wrapped row's spaces: trimmed
        // (end of selection).
        select(&mut t, SelectionMode::Char, (0, 0), (0, 4));
        assert_eq!(t.extract_selection_text().as_deref(), Some("ab"));
        // Through the hard end of row 1 into row 2.
        select(&mut t, SelectionMode::Char, (0, 0), (2, 0));
        assert_eq!(t.extract_selection_text().as_deref(), Some("ab   cd\nx"));
    }

    #[test]
    fn wide_char_pad_cell_before_wrap_is_not_copied() {
        // `abcd` fills 4 of 5 columns; `日` doesn't fit in the last one
        // and wraps, leaving a blank pad cell behind.
        let mut t = Term::new(4, 5, 16);
        feed(&mut t, b"abcd\xe6\x97\xa5x");
        select(&mut t, SelectionMode::Char, (0, 0), (1, 2));
        assert_eq!(t.extract_selection_text().as_deref(), Some("abcd\u{65e5}x"));
    }

    #[test]
    fn either_half_of_wide_char_selects_whole_char() {
        // Audit #16. "日本" occupies cols 0-1 and 2-3.
        let mut t = Term::new(4, 10, 16);
        feed(&mut t, "日本語".as_bytes());
        let id = pos_for(&t, 0, 0).line_id;
        // Start on the right half of 日.
        select(&mut t, SelectionMode::Char, (0, 1), (0, 3));
        assert_eq!(t.extract_selection_text().as_deref(), Some("日本"));
        assert!(t.is_cell_selected(id, 0), "left half of 日 highlighted");
        assert!(t.is_cell_selected(id, 1));
        assert!(!t.is_cell_selected(id, 4));
        // End on the left half of 本.
        select(&mut t, SelectionMode::Char, (0, 0), (0, 2));
        assert_eq!(t.extract_selection_text().as_deref(), Some("日本"));
        assert!(t.is_cell_selected(id, 3), "right half of 本 highlighted");
        assert!(!t.is_cell_selected(id, 4));
        // Reverse drag from the right half of 語 to the right half of 本.
        select(&mut t, SelectionMode::Char, (0, 5), (0, 3));
        assert_eq!(t.extract_selection_text().as_deref(), Some("本語"));
        // Block mode widens per row too.
        select(&mut t, SelectionMode::Block, (0, 1), (0, 1));
        assert_eq!(t.extract_selection_text().as_deref(), Some("日"));
        // Cells outside the selection's rows are never selected.
        assert!(!t.is_cell_selected(id + 1, 0));
    }

    #[test]
    fn triple_click_bounds_span_soft_wrapped_logical_line() {
        // Audit #17. Cols 4: "hello world" wraps as hell|o wo|rld.
        let mut t = Term::new(5, 4, 16);
        feed(&mut t, b"hello world\r\nnext");
        let top = pos_for(&t, 0, 0).line_id;
        for r in 0..3 {
            let id = pos_for(&t, r, 0).line_id;
            assert_eq!(t.logical_line_bounds(id), Some((top, top + 2)), "row {r}");
        }
        let next = pos_for(&t, 3, 0).line_id;
        assert_eq!(t.logical_line_bounds(next), Some((next, next)));
        assert_eq!(t.logical_line_bounds(next + 100), None);
        // The whole logical line copies back as one line.
        select(&mut t, SelectionMode::Line, (0, 0), (2, 3));
        assert_eq!(t.extract_selection_text().as_deref(), Some("hello world"));
    }

    #[test]
    fn word_bounds_unicode_and_across_soft_wrap() {
        // Audit #15: caf\u{e9}_x must select whole; words also continue
        // across a soft wrap.
        let mut t = Term::new(4, 8, 16);
        feed(&mut t, "café_x ~/src/a.rs".as_bytes());
        let is_word = |c: char| c.is_alphanumeric() || "_-./~".contains(c);
        let (lo, hi) = t.word_bounds(pos_for(&t, 0, 2), is_word).unwrap();
        assert_eq!((lo, hi), (pos_for(&t, 0, 0), pos_for(&t, 0, 5)));
        // "~/src/a.rs" starts at col 7 of row 0 and wraps over row 1
        // onto row 2 ("~" | "/src/a.r" | "s").
        let (lo, hi) = t.word_bounds(pos_for(&t, 1, 3), is_word).unwrap();
        assert_eq!((lo, hi), (pos_for(&t, 0, 7), pos_for(&t, 2, 0)));
        // On a separator: single cell.
        let sp = pos_for(&t, 0, 6);
        assert_eq!(t.word_bounds(sp, is_word), Some((sp, sp)));
        // Evicted / unknown line.
        assert_eq!(t.word_bounds(Pos::new(10_000, 0), is_word), None);
    }

    #[test]
    fn word_bounds_wide_chars_and_hard_line_ends() {
        let mut t = Term::new(4, 10, 16);
        feed(&mut t, "x 日本語 y\r\nab".as_bytes());
        // Right half of 本 (col 5) classifies like 本.
        let (lo, hi) = t
            .word_bounds(pos_for(&t, 0, 5), char::is_alphanumeric)
            .unwrap();
        assert_eq!((lo.col, hi.col), (2, 7));
        // A word at the end of a hard line doesn't continue onto the
        // next row.
        let mut t = Term::new(4, 4, 16);
        feed(&mut t, b"  ab\r\ncd");
        let (lo, hi) = t
            .word_bounds(pos_for(&t, 0, 3), char::is_alphanumeric)
            .unwrap();
        assert_eq!((lo, hi), (pos_for(&t, 0, 2), pos_for(&t, 0, 3)));
        // …and one at the start of a row doesn't reach back across a
        // hard break.
        let (lo, _) = t
            .word_bounds(pos_for(&t, 1, 0), char::is_alphanumeric)
            .unwrap();
        assert_eq!(lo, pos_for(&t, 1, 0));
        // At the very first retained line there's no previous row.
        let t = Term::new(2, 4, 0);
        let p = pos_for(&t, 0, 0);
        assert_eq!(t.word_bounds(p, |_| true), Some((p, pos_for(&t, 0, 3))));
    }

    #[test]
    fn partially_evicted_selection_copies_remaining_rows() {
        // Audit #18. 2 visible rows + 2 scrollback rows.
        let mut t = Term::new(2, 8, 2);
        feed(&mut t, b"l0\r\nl1\r\nl2\r\nl3");
        let l1 = pos_for(&t, 0, 0).line_id - 1;
        let mut sel = Selection::new(Pos::new(l1, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 1, 1)); // through "l3"
        t.set_selection(sel);
        assert_eq!(t.extract_selection_text().as_deref(), Some("l1\nl2\nl3"));
        // Two more lines evict l0 and l1.
        feed(&mut t, b"\r\nl4\r\nl5");
        assert_eq!(t.extract_selection_text().as_deref(), Some("l2\nl3"));
        // Everything evicted → nothing to copy.
        feed(&mut t, b"\r\nl6\r\nl7\r\nl8\r\nl9");
        assert_eq!(t.extract_selection_text(), None);
    }

    #[test]
    fn committed_selection_clears_when_selected_rows_change() {
        let mut t = term_with_lines(4, 16, &["keep me", "other"]);
        select(&mut t, SelectionMode::Char, (0, 0), (0, 3));
        t.commit_selection();
        // Output elsewhere (row 1) and an identical redraw of row 0
        // keep the highlight.
        feed(&mut t, b"\x1b[2;1Hchanged\x1b[1;1Hkeep me");
        assert!(!t.revalidate_selection());
        assert!(t.selection().is_some());
        // Rewriting row 0 drops it.
        feed(&mut t, b"\x1b[1;1HKEEP");
        assert!(t.revalidate_selection());
        assert!(t.selection().is_none());
        // Nothing left to validate.
        assert!(!t.revalidate_selection());
    }

    #[test]
    fn in_progress_selection_is_not_revalidated() {
        let mut t = term_with_lines(4, 16, &["drag"]);
        select(&mut t, SelectionMode::Char, (0, 0), (0, 3));
        feed(&mut t, b"\x1b[1;1HDRAG");
        assert!(!t.revalidate_selection());
        assert!(t.selection().is_some());
        // Updating the active end re-arms "in progress" too.
        t.commit_selection();
        t.update_selection_active(pos_for(&t, 0, 2));
        feed(&mut t, b"\x1b[1;1Hzzzz");
        assert!(!t.revalidate_selection());
        assert!(t.selection().is_some());
    }

    #[test]
    fn ed3_drops_a_selection_on_erased_scrollback() {
        // SCR-13: ED 3 erases the scrollback (and the screen), so the
        // selection is dropped right away, committed or mid-drag.
        let mut t = Term::new(2, 8, 8);
        feed(&mut t, b"old\r\nmid\r\nbot");
        let old = pos_for(&t, 0, 0).line_id - 1;
        let mut sel = Selection::new(Pos::new(old, 0), SelectionMode::Char);
        sel.set_active(Pos::new(old, 2));
        t.set_selection(sel);
        t.commit_selection();
        assert_eq!(t.extract_selection_text().as_deref(), Some("old"));
        feed(&mut t, b"\x1b[3J");
        assert!(t.selection().is_none());
        assert!(!t.revalidate_selection(), "nothing left to revalidate");
        // Drag in progress (not committed) spanning history and screen.
        feed(&mut t, b"\x1b[Hab\r\ncd\r\nef");
        let top = pos_for(&t, 0, 0).line_id - 1;
        let mut sel = Selection::new(Pos::new(top, 0), SelectionMode::Char);
        sel.set_active(pos_for(&t, 1, 1));
        t.set_selection(sel);
        feed(&mut t, b"\x1b[3J");
        assert!(t.selection().is_none());
        assert_eq!(t.extract_selection_text(), None);
    }

    #[test]
    fn committed_selection_survives_scroll_but_not_eviction() {
        let mut t = Term::new(2, 8, 2);
        feed(&mut t, b"sel");
        select(&mut t, SelectionMode::Char, (0, 0), (0, 2));
        t.commit_selection();
        // Scrolling the row into scrollback keeps it (content intact).
        feed(&mut t, b"\r\na\r\nb");
        assert!(!t.revalidate_selection());
        assert_eq!(t.extract_selection_text().as_deref(), Some("sel"));
        // Evicting it clears.
        feed(&mut t, b"\r\nc\r\nd");
        assert!(t.revalidate_selection());
        assert!(t.selection().is_none());
    }

    #[test]
    fn committed_selection_in_scrollback_survives_reverse_index() {
        // RI at the top inserts a blank line on screen and leaves
        // scrollback alone (SCR-9), so a selection on a scrollback row
        // keeps pointing at the same text.
        let mut t = Term::new(2, 8, 4);
        feed(&mut t, b"top\r\nmid\r\nbot");
        let top = pos_for(&t, 0, 0).line_id - 1;
        let mut sel = Selection::new(Pos::new(top, 0), SelectionMode::Char);
        sel.set_active(Pos::new(top, 2));
        t.set_selection(sel);
        t.commit_selection();
        feed(&mut t, b"\x1b[1;1H\x1bM");
        assert!(!t.revalidate_selection());
        assert_eq!(t.extract_selection_text().as_deref(), Some("top"));
        assert_eq!(t.history_lines(), 1);
    }

    #[test]
    fn committed_selection_on_screen_clears_when_reverse_index_moves_it() {
        // On-screen rows keep their line ids across RI while their
        // content moves down a row, so a committed selection there sees
        // its text change and is dropped.
        let mut t = Term::new(2, 8, 4);
        feed(&mut t, b"mid\r\nbot");
        select(&mut t, SelectionMode::Char, (0, 0), (0, 2));
        t.commit_selection();
        feed(&mut t, b"\x1b[1;1H\x1bM");
        assert!(t.revalidate_selection());
        assert!(t.selection().is_none());
    }

    #[test]
    fn commit_without_selection_and_clear_are_noops() {
        let mut t = term_with_lines(4, 16, &["x"]);
        t.commit_selection();
        assert!(!t.revalidate_selection());
        t.clear_selection();
        assert!(t.selection().is_none());
        assert_eq!(t.extract_selection_text(), None);
    }

    #[test]
    fn ris_drops_selection() {
        let mut t = term_with_lines(4, 16, &["x"]);
        select(&mut t, SelectionMode::Char, (0, 0), (0, 0));
        feed(&mut t, b"\x1bc");
        assert!(t.selection().is_none());
    }
}
