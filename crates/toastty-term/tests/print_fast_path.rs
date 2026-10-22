//! The printable-ASCII fast path (`Term::print_ascii`, fed runs by the
//! parser) must leave the terminal exactly as one `print` per character
//! does: same cells, wrap flags, cursor, pending-wrap state, scrollback
//! and damage. Each input is run both ways and the results compared.

use proptest::prelude::*;
use toastty_parser::{Params, Parser, Perform};
use toastty_term::Term;

/// Forwards everything to the terminal except `print_ascii`, which
/// keeps the trait's per-character default.
struct PerChar<'a>(&'a mut Term);

impl Perform for PerChar<'_> {
    fn print(&mut self, c: char) {
        self.0.print(c);
    }
    fn execute(&mut self, b: u8) {
        self.0.execute(b);
    }
    fn hook(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.0.hook(p, i, ig, a);
    }
    fn put(&mut self, b: u8) {
        self.0.put(b);
    }
    fn unhook(&mut self) {
        self.0.unhook();
    }
    fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
        self.0.osc_dispatch(p, bell);
    }
    fn csi_dispatch(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.0.csi_dispatch(p, i, ig, a);
    }
    fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
        self.0.esc_dispatch(i, ig, b);
    }
    fn dcs_cancel(&mut self) {
        self.0.dcs_cancel();
    }
    fn csi_param_digits(&mut self, mask: u32) {
        self.0.csi_param_digits(mask);
    }
    fn apc_start(&mut self) {
        self.0.apc_start();
    }
    fn apc_chunk(&mut self, b: &[u8]) {
        self.0.apc_chunk(b);
    }
    fn apc_end(&mut self) {
        self.0.apc_end();
    }
}

const ROWS: u16 = 5;
const COLS: u16 = 12;

fn feed(chunks: &[&[u8]], fast: bool) -> Term {
    let mut t = Term::new(ROWS, COLS, 8);
    let mut p = Parser::new();
    for c in chunks {
        if fast {
            p.advance(&mut t, c);
        } else {
            p.advance(&mut PerChar(&mut t), c);
        }
    }
    t
}

/// Everything observable about the grid state, as comparable values.
fn state(t: &Term) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    // Every retained primary row (screen and scrollback), by line id;
    // the inputs are far too short to scroll ids past this.
    for id in 0..4096 {
        if let Some(row) = t.primary_row_by_line_id(id) {
            writeln!(
                s,
                "{id}: wrap={} rich={:?} {:?}",
                row.soft_wrap,
                row.rich,
                row.cells()
            )
            .unwrap();
        }
    }
    if t.is_alt_active() {
        for r in 0..ROWS {
            let row = t.row(r);
            writeln!(s, "alt {r}: wrap={} {:?}", row.soft_wrap, row.cells()).unwrap();
        }
    }
    writeln!(s, "cursor {:?}", t.cursor()).unwrap();
    writeln!(s, "damage {:?}", t.damage()).unwrap();
    s
}

fn check(chunks: &[&[u8]]) {
    let mut fast = feed(chunks, true);
    let mut slow = feed(chunks, false);
    assert_eq!(state(&fast), state(&slow), "input: {chunks:?}");
    // Pending-wrap state isn't directly visible: one more character
    // (and a CR + run) after the input must land the same way too.
    let mut p = Parser::new();
    p.advance(&mut fast, b"Z\rab");
    let mut p = Parser::new();
    p.advance(&mut PerChar(&mut slow), b"Z\rab");
    assert_eq!(state(&fast), state(&slow), "input + probe: {chunks:?}");
}

#[test]
fn runs_wrap_scroll_and_park_like_single_prints() {
    check(&[b"hello"]);
    check(&[b"abcdefghijklmnopqrstuvwxyz0123456789"]);
    check(&[b"abcdefghijkl\r\nmno"]);
    for _ in 0..3 {
        check(&[b"0123456789AB0123456789AB0123456789AB\n\n\n\n\nxyz"]);
    }
    // DECAWM off: the run overwrites the last column.
    check(&[b"\x1b[?7labcdefghijklmnopq\x1b[?7hrs"]);
    // IRM.
    check(&[b"abcdef\r\x1b[4hXY\x1b[4lZ"]);
    // DECSLRM margins, inside and outside.
    check(&[b"\x1b[?69h\x1b[3;8sabcdefghijk\x1b[1;10Hqrstuv\x1b[?69l"]);
}

#[test]
fn runs_clear_wide_cluster_halves_at_both_ends() {
    // Overwrite starting on a continuation, and ending just before one.
    check(&["世界世界世界".as_bytes(), b"\r\x1b[2Cab"]);
    check(&["世界世界世界".as_bytes(), b"\rabc"]);
    check(&["ab世界".as_bytes(), b"\x1b[1;4Hxy"]);
}

#[test]
fn runs_carry_style_and_hyperlinks() {
    check(&[b"\x1b[1;31;44mred\x1b[0m \x1b]8;;http://x\x07link\x1b]8;;\x07 done"]);
}

fn piece() -> impl Strategy<Value = Vec<u8>> {
    let fixed: Vec<&'static [u8]> = vec![
        b"\r",
        b"\n",
        b"\r\n",
        b"\x08",
        b"\t",
        b"\x7f",
        b"\x1b[?7l",
        b"\x1b[?7h",
        b"\x1b[4h",
        b"\x1b[4l",
        b"\x1b[?69h\x1b[3;9s",
        b"\x1b[?69l",
        b"\x1b[2;4r",
        b"\x1b[r",
        b"\x1b[H",
        b"\x1b[3;11H",
        b"\x1b[1;12H",
        b"\x1b[5C",
        b"\x1b[2D",
        b"\x1b[K",
        b"\x1b[1K",
        b"\x1b[2J",
        b"\x1b[2@",
        b"\x1b[2P",
        b"\x1b[3X",
        b"\x1b[31;1m",
        b"\x1b[44m",
        b"\x1b[0m",
        b"\x1b]8;;http://x\x07",
        b"\x1b]8;;\x07",
        b"\x1b[?1049h",
        b"\x1b[?1049l",
        b"\x1bM",
        b"\x1bD",
        b"\x1b7",
        b"\x1b8",
        b"\x1b[?2027h",
        b"\x1b[?2027l",
        b"\x1b[b",
        "世".as_bytes(),
        "界".as_bytes(),
        "🎉".as_bytes(),
        "é".as_bytes(),
        "e\u{301}".as_bytes(),
        "\u{200d}".as_bytes(),
        "❤\u{fe0f}".as_bytes(),
        "\u{10EEEE}\u{305}".as_bytes(),
    ];
    prop_oneof![
        3 => proptest::sample::select(fixed).prop_map(<[u8]>::to_vec),
        3 => "[ -~]{1,30}".prop_map(String::into_bytes),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1500, ..ProptestConfig::default() })]

    #[test]
    fn fast_path_matches_per_char_prints(
        pieces in proptest::collection::vec(piece(), 0..30),
        cut in any::<prop::sample::Index>(),
    ) {
        let input = pieces.concat();
        let (a, b) = input.split_at(cut.index(input.len() + 1));
        check(&[a, b]);
    }
}
