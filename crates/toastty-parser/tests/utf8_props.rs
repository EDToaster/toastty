//! Property tests for the ground-text UTF-8 sanitizer (WIRE-14).
//!
//! - Chunking invariance: any split of the input into reads gives the
//!   same events as feeding it whole.
//! - Differential: on input made of the pieces below, our parser fed
//!   the raw bytes in random chunks gives exactly the events raw `vte`
//!   gives for `String::from_utf8_lossy(input)` fed in one go. That is
//!   the kitty behaviour the sanitizer exists for (one U+FFFD per maximal
//!   invalid subpart, bare C1 bytes included).

use proptest::prelude::*;
use toastty_parser::{Params, Parser, Perform};

/// Parser events, comparable across our `Perform` and `vte::Perform`.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Print(char),
    Exec(u8),
    Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
    Esc(Vec<u8>, bool, u8),
    Hook(Vec<Vec<u16>>, Vec<u8>, bool, char),
    Put(u8),
    Unhook,
    Osc(Vec<Vec<u8>>, bool),
    ApcStart,
    Apc(Vec<u8>),
    ApcEnd,
}

fn params(p: &Params) -> Vec<Vec<u16>> {
    p.iter().map(<[u16]>::to_vec).collect()
}

fn vte_params(p: &vte::Params) -> Vec<Vec<u16>> {
    p.iter().map(<[u16]>::to_vec).collect()
}

#[derive(Debug, Default)]
struct Rec(Vec<Ev>);

impl Rec {
    /// Events with adjacent APC chunks merged (chunk boundaries follow
    /// the reads, by design).
    fn normalized(self) -> Vec<Ev> {
        let mut out: Vec<Ev> = Vec::new();
        for e in self.0 {
            if let (Ev::Apc(b), Some(Ev::Apc(prev))) = (&e, out.last_mut()) {
                prev.extend_from_slice(b);
                continue;
            }
            out.push(e);
        }
        out
    }
}

impl Perform for Rec {
    fn print(&mut self, c: char) {
        self.0.push(Ev::Print(c));
    }
    fn execute(&mut self, b: u8) {
        self.0.push(Ev::Exec(b));
    }
    fn hook(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.0.push(Ev::Hook(params(p), i.to_vec(), ig, a));
    }
    fn put(&mut self, b: u8) {
        self.0.push(Ev::Put(b));
    }
    fn unhook(&mut self) {
        self.0.push(Ev::Unhook);
    }
    fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
        self.0
            .push(Ev::Osc(p.iter().map(|s| s.to_vec()).collect(), bell));
    }
    fn csi_dispatch(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.0.push(Ev::Csi(params(p), i.to_vec(), ig, a));
    }
    fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
        self.0.push(Ev::Esc(i.to_vec(), ig, b));
    }
    fn apc_start(&mut self) {
        self.0.push(Ev::ApcStart);
    }
    fn apc_chunk(&mut self, b: &[u8]) {
        self.0.push(Ev::Apc(b.to_vec()));
    }
    fn apc_end(&mut self) {
        self.0.push(Ev::ApcEnd);
    }
}

/// The same recorder for raw vte.
#[derive(Debug, Default)]
struct VteRec(Vec<Ev>);

impl vte::Perform for VteRec {
    fn print(&mut self, c: char) {
        self.0.push(Ev::Print(c));
    }
    fn execute(&mut self, b: u8) {
        self.0.push(Ev::Exec(b));
    }
    fn hook(&mut self, p: &vte::Params, i: &[u8], ig: bool, a: char) {
        self.0.push(Ev::Hook(vte_params(p), i.to_vec(), ig, a));
    }
    fn put(&mut self, b: u8) {
        self.0.push(Ev::Put(b));
    }
    fn unhook(&mut self) {
        self.0.push(Ev::Unhook);
    }
    fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
        self.0
            .push(Ev::Osc(p.iter().map(|s| s.to_vec()).collect(), bell));
    }
    fn csi_dispatch(&mut self, p: &vte::Params, i: &[u8], ig: bool, a: char) {
        self.0.push(Ev::Csi(vte_params(p), i.to_vec(), ig, a));
    }
    fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
        self.0.push(Ev::Esc(i.to_vec(), ig, b));
    }
}

/// Pieces both sides must agree on: text (valid, invalid, bare and
/// encoded C1), C0 controls, complete CSI / ESC sequences, DCS, SOS/PM.
/// Left out, because we deliberately differ from vte there: OSC (we pass
/// its raw bytes to vte, which lossy conversion would rewrite), APC (vte
/// drops it), and a bare ESC or an unfinished CSI, which a non-ASCII
/// byte aborts like kitty (WIRE-17, WIRE-18).
const PIECES: &[&[u8]] = &[
    b"a",
    b"hello ",
    b"\n",
    b"\r",
    b"\t",
    "é".as_bytes(),
    "€".as_bytes(),
    "한".as_bytes(),
    "😀".as_bytes(),
    "👨\u{200d}👩\u{200d}👧".as_bytes(),
    "e\u{301}".as_bytes(),
    b"\x80",
    b"\x85",
    b"\x9b",
    b"\x9c",
    b"\x9d",
    b"\x9f",
    b"\xa0",
    b"\xbf",
    b"\xc0\xaf",
    b"\xc2",
    b"\xe2\x82",
    b"\xed\xa0\x80",
    b"\xf0\x9f",
    b"\xf4\x90\x80\x80",
    b"\xf8\x88\x80\x80\x80",
    b"\xff",
    b"\xc2\x85",
    b"\xc2\x9b",
    b"\xc2\x9c",
    b"\x07",
    b"\x08",
    b"\x18",
    b"\x1a",
    b"\x1b[31m",
    b"\x1b[1;2H",
    b"\x1b[?25l",
    b"\x1b[38;5;196m",
    b"\x1b7",
    b"\x1b(B",
    b"\x1b\\",
    b"\x1bPq#0;2;0;0;0\x1b\\",
    b"\x1bP1$r\x1b\\",
    b"\x1bPzhi\x9cX\x1b\\",
    b"\x1bXsos\x1b\\",
    b"\x1b^pm\x1b\\",
];

/// Extra pieces for the chunking test only (our parser on both sides).
const MORE_PIECES: &[&[u8]] = &[
    b"\x1b]0;title\x07",
    b"\x1b]8;;http://x\x1b\\",
    b"\x1b]2;\xe2\x82\x9c\x07",
    b"\x1b_Ga=T,f=100;QUFB\x1b\\",
    b"\x1b_payload\x9c",
    b"\x1bPtmux;\x1b\x1b[31m\x1b\\",
    b"\x1bPtmux;\x1b\x1b_Gq=1;x\x1b\x1b\\\x1b\\",
    b"\x1b",
    b"\x1b[3",
    b"\x1b[",
    b"\x1b(",
];

fn stream(pieces: &'static [&'static [u8]]) -> impl Strategy<Value = Vec<u8>> {
    proptest::collection::vec(proptest::sample::select(pieces), 0..40).prop_map(|v| v.concat())
}

/// Feed `input` split at `cuts` (sorted, deduplicated positions).
fn ours(input: &[u8], cuts: &[usize]) -> Vec<Ev> {
    let mut p = Parser::new();
    let mut r = Rec::default();
    let mut at = 0;
    for &c in cuts.iter().chain(std::iter::once(&input.len())) {
        let c = c.min(input.len());
        if c > at {
            p.advance(&mut r, &input[at..c]);
            at = c;
        }
    }
    r.normalized()
}

fn cuts(len: usize, raw: Vec<usize>) -> Vec<usize> {
    let mut v: Vec<usize> = raw.into_iter().map(|c| c % (len + 1)).collect();
    v.sort_unstable();
    v.dedup();
    v
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(2000))]

    /// Any split into reads gives the events of the whole input.
    #[test]
    fn chunk_splits_do_not_change_events(
        input in prop_oneof![stream(PIECES), stream(MORE_PIECES)]
            .prop_flat_map(|a| stream(PIECES).prop_map(move |b| [a.clone(), b].concat())),
        raw_cuts in proptest::collection::vec(any::<usize>(), 0..24),
    ) {
        let whole = ours(&input, &[]);
        let split = ours(&input, &cuts(input.len(), raw_cuts));
        prop_assert_eq!(whole, split);
    }

    /// Byte-at-a-time is the worst case for held partial characters.
    #[test]
    fn byte_at_a_time_matches_whole(input in stream(PIECES)) {
        let every: Vec<usize> = (1..input.len()).collect();
        prop_assert_eq!(ours(&input, &[]), ours(&input, &every));
    }

    /// Raw bytes through our parser == lossily cleaned bytes through vte.
    /// Both end with an SGR reset, so a trailing partial character is
    /// settled (cut by the ESC) rather than still held for more input.
    #[test]
    fn matches_vte_on_lossily_cleaned_input(
        input in stream(PIECES).prop_map(|mut v| { v.extend_from_slice(b"\x1b[m"); v }),
        raw_cuts in proptest::collection::vec(any::<usize>(), 0..24),
    ) {
        let lossy = String::from_utf8_lossy(&input);
        let mut v = vte::Parser::new();
        let mut want = VteRec::default();
        v.advance(&mut want, lossy.as_bytes());
        prop_assert_eq!(ours(&input, &cuts(input.len(), raw_cuts)), want.0);
    }
}
