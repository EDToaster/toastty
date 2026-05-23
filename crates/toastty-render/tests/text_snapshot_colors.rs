//! Snapshot: SGR colors + reverse.

mod common;

use toastty_parser::Parser;
use toastty_term::Term;

#[test]
fn snapshot_colors_and_reverse() {
    let mut term = Term::new(8, 40, 0);
    let mut parser = Parser::new();
    parser.advance(
        &mut term,
        b"\x1b[31mred\x1b[0m \x1b[1;32mbold green\x1b[0m \x1b[7mreversed\x1b[0m",
    );

    let img = common::render_term_offscreen(&term, 600, 200);
    common::assert_matches_golden("text_colors", &img);
}
