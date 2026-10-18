//! Snapshot: SGR renditions — dim, strikethrough, overline, double
//! underline, conceal, and combinations with colour, reverse and wide
//! (CJK) clusters. Decorations use the bundled font's metrics.

mod common;

use toastty_parser::Parser;
use toastty_term::Term;

#[test]
fn snapshot_sgr_attributes() {
    let mut term = Term::new(8, 40, 0);
    let mut parser = Parser::new();
    parser.advance(
        &mut term,
        "plain \x1b[2mdim\x1b[22m \x1b[9mstrike\x1b[29m \x1b[53mover\x1b[55m\r\n\
         \x1b[4msingle\x1b[24m \x1b[21mdouble\x1b[24m \x1b[4:3mcurly\x1b[0m\r\n\
         [\x1b[8mhidden\x1b[28m] \x1b[1;2;9;53mall\x1b[0m \x1b[2;31mdim red\x1b[0m\r\n\
         \x1b[7;9mrev strike\x1b[0m \x1b[2;7mrev dim\x1b[0m\r\n\
         \x1b[9;53;21m中文字\x1b[0m \x1b[9;44m  \x1b[0m"
            .as_bytes(),
    );
    let img = common::render_term_offscreen(&term, 600, 200);
    common::assert_matches_golden("text_attrs", &img);
}
