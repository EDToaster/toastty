//! Snapshot: cursor positioned at (5, 10) via `CSI 5;10 H` lands at
//! row 4 col 9 (0-indexed). Renders a small grid with the cursor block.

mod common;

use toastty_parser::Parser;
use toastty_term::Term;

#[test]
fn snapshot_cursor_at_non_trivial_position() {
    let mut term = Term::new(10, 20, 0);
    let mut parser = Parser::new();
    // Print a corner marker, then move the cursor and stop.
    parser.advance(&mut term, b"top\x1b[5;10Hx");
    // After 'x' the cursor sits one past the end (still on the same row);
    // step back one column so the cursor block visibly covers 'x'.
    parser.advance(&mut term, b"\x1b[5;10H");

    // Pixel-perfect: row 4 (0-idx) × line_height, col 9 × cell_width.
    let img = common::render_term_offscreen(&term, 360, 240);
    common::assert_matches_golden("text_cursor", &img);
}
