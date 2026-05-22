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
    let combined: String = (0..3).map(|r| row_text(&t, r)).collect::<Vec<_>>().join("|");
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
