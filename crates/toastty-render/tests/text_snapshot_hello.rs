//! Snapshot: "Hello, toastty!" plain text.

mod common;

use toastty_parser::Parser;
use toastty_term::Term;

#[test]
fn snapshot_hello_text() {
    let mut term = Term::new(8, 30, 0);
    let mut parser = Parser::new();
    parser.advance(&mut term, b"Hello, toastty!");

    let img = common::render_term_offscreen(&term, 480, 200);
    common::assert_matches_golden("text_hello", &img);
}
