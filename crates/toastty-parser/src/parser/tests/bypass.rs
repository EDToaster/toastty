//! The ground-text bypass ([`dispatch_ground`]): with vte known to be
//! in ground, the scanner hands text to the handler itself, printable
//! ASCII in [`Perform::print_ascii`] runs. It must produce exactly the
//! events vte would, so these tests run every input both ways (the
//! `no_bypass` hook routes all ground text through vte) and compare.

use super::*;
use proptest::prelude::*;

/// Every event, in order. `print_ascii` runs are flattened into
/// `Print`s (that is the contract) but also checked to be printable
/// ASCII.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Ev {
    Print(char),
    Exec(u8),
    Csi(Vec<Vec<u16>>, Vec<u8>, bool, char),
    Digits(u32),
    Esc(Vec<u8>, bool, u8),
    Hook(Vec<Vec<u16>>, Vec<u8>, bool, char),
    Put(u8),
    Unhook,
    DcsCancel,
    Osc(Vec<Vec<u8>>, bool),
    ApcStart,
    Apc(Vec<u8>),
    ApcEnd,
}

#[derive(Debug, Default)]
struct Log {
    ev: Vec<Ev>,
    runs: usize,
}

impl Log {
    /// Adjacent APC chunks merged: their boundaries follow the reads.
    fn normalized(self) -> Vec<Ev> {
        let mut out: Vec<Ev> = Vec::new();
        for e in self.ev {
            if let (Ev::Apc(b), Some(Ev::Apc(prev))) = (&e, out.last_mut()) {
                prev.extend_from_slice(b);
                continue;
            }
            out.push(e);
        }
        out
    }
}

fn ps(p: &Params) -> Vec<Vec<u16>> {
    p.iter().map(<[u16]>::to_vec).collect()
}

impl Perform for Log {
    fn print(&mut self, c: char) {
        self.ev.push(Ev::Print(c));
    }
    fn print_ascii(&mut self, run: &[u8]) {
        assert!(!run.is_empty(), "empty run");
        assert!(
            run.iter().all(|b| (0x20..0x7F).contains(b)),
            "non-printable byte in an ASCII run: {run:?}"
        );
        self.runs += 1;
        self.ev
            .extend(run.iter().map(|&b| Ev::Print(char::from(b))));
    }
    fn execute(&mut self, b: u8) {
        self.ev.push(Ev::Exec(b));
    }
    fn hook(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.ev.push(Ev::Hook(ps(p), i.to_vec(), ig, a));
    }
    fn put(&mut self, b: u8) {
        self.ev.push(Ev::Put(b));
    }
    fn unhook(&mut self) {
        self.ev.push(Ev::Unhook);
    }
    fn dcs_cancel(&mut self) {
        self.ev.push(Ev::DcsCancel);
    }
    fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
        self.ev
            .push(Ev::Osc(p.iter().map(|s| s.to_vec()).collect(), bell));
    }
    fn csi_dispatch(&mut self, p: &Params, i: &[u8], ig: bool, a: char) {
        self.ev.push(Ev::Csi(ps(p), i.to_vec(), ig, a));
    }
    fn csi_param_digits(&mut self, mask: u32) {
        self.ev.push(Ev::Digits(mask));
    }
    fn esc_dispatch(&mut self, i: &[u8], ig: bool, b: u8) {
        self.ev.push(Ev::Esc(i.to_vec(), ig, b));
    }
    fn apc_start(&mut self) {
        self.ev.push(Ev::ApcStart);
    }
    fn apc_chunk(&mut self, b: &[u8]) {
        self.ev.push(Ev::Apc(b.to_vec()));
    }
    fn apc_end(&mut self) {
        self.ev.push(Ev::ApcEnd);
    }
}

/// Feed `chunks` through a parser with the bypass on or off.
fn run(chunks: &[&[u8]], bypass: bool) -> Log {
    let mut p = Parser::new();
    p.scanner.no_bypass = !bypass;
    let mut log = Log::default();
    for c in chunks {
        p.advance(&mut log, c);
    }
    log
}

/// Same events with and without the bypass; returns the bypass log's
/// `print_ascii` run count.
fn check(chunks: &[&[u8]]) -> usize {
    let fast = run(chunks, true);
    let runs = fast.runs;
    let slow = run(chunks, false);
    assert_eq!(slow.runs, 0, "no_bypass must route all text through vte");
    assert_eq!(fast.normalized(), slow.normalized(), "input: {chunks:?}");
    runs
}

#[test]
fn plain_text_comes_in_runs() {
    assert_eq!(check(&[b"hello world"]), 1);
    // Controls split runs; DEL is printed, not a run byte.
    assert_eq!(check(&[b"ab\r\ncd\x7fef\tg"]), 4);
    // Non-ASCII (and an encoded C1 control) between runs.
    assert_eq!(check(&["é世x\u{9b}y🎉z".as_bytes()]), 3);
}

#[test]
fn text_after_each_kind_of_sequence_takes_the_bypass() {
    // CSI, escape with intermediate, OSC by BEL and by ST, DCS by ST,
    // APC: the text after each is a run of its own.
    let input = b"a\x1b[1;31mb\x1b(Bc\x1b]0;t\x07d\x1b]0;t\x1b\\e\x1bPq#0\x1b\\f\x1b_G;x\x1b\\g";
    assert_eq!(check(&[input]), 7);
}

#[test]
fn vte_keeps_text_it_might_not_be_in_ground_for() {
    // `ESC ESC` leaves vte in an escape and DEL doesn't end one, so the
    // next byte is vte's, not text; `ESC ^E [` starts a CSI after a C0
    // control inside the escape.
    for input in [
        &b"\x1b\x1bAb"[..],
        b"\x1b\x7fAb",
        b"\x1b\x05[31mx",
        b"\x1b\x05]0;t\x07x",
        b"\x1b\x05Pq\x1b\\x",
        b"\x1b\x05_x\x1b\\y",
        b"\x1b\x05^x\x1b\\y",
        b"\x1b(\x05[x",
    ] {
        check(&[input]);
    }
}

#[test]
fn sequences_split_across_reads() {
    let input: &[u8] = b"ab\x1b[38;5;196mcd\x1b]8;;http://x\x1b\\ef\x1b(0gh\xe4\xb8\x96ij";
    for cut in 0..=input.len() {
        let (a, b) = input.split_at(cut);
        check(&[a, b]);
        for cut2 in cut..=input.len() {
            let (b1, b2) = input[cut..].split_at(cut2 - cut);
            check(&[a, b1, b2]);
        }
    }
}

/// Pieces that exercise every scanner and vte state transition, so a
/// random concatenation hits the bypass bookkeeping from all sides.
fn piece() -> impl Strategy<Value = Vec<u8>> {
    let fixed: Vec<&'static [u8]> = vec![
        b"\x1b",
        b"[",
        b"]",
        b"P",
        b"X",
        b"^",
        b"_",
        b"\\",
        b"(",
        b" ",
        b"#",
        b"?",
        b">",
        b"$",
        b";",
        b":",
        b"0",
        b"1",
        b"38",
        b"5",
        b"m",
        b"H",
        b"q",
        b"A",
        b"z",
        b"~",
        b"\x07",
        b"\x18",
        b"\x1a",
        b"\x7f",
        b"\x05",
        b"\r",
        b"\n",
        b"\t",
        b"\x9c",
        b"\x9b",
        b"\x80",
        b"\xff",
        b"\xc2",
        b"\xe4\xb8",
        b"\x96",
        "é".as_bytes(),
        "世".as_bytes(),
        "🎉".as_bytes(),
        "\u{9b}".as_bytes(),
        b"tmux;",
        b"hello",
        b"\x1b[",
        b"\x1b]0;",
        b"\x1bP",
        b"\x1b_",
        b"\x1b\\",
    ];
    prop_oneof![
        4 => proptest::sample::select(fixed).prop_map(<[u8]>::to_vec),
        1 => proptest::collection::vec(any::<u8>(), 1..4),
        1 => "[ -~]{1,12}".prop_map(String::into_bytes),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 2000, ..ProptestConfig::default() })]

    #[test]
    fn bypass_matches_vte(
        pieces in proptest::collection::vec(piece(), 0..40),
        cuts in proptest::collection::vec(any::<prop::sample::Index>(), 0..4),
    ) {
        let input = pieces.concat();
        let mut at: Vec<usize> = cuts.iter().map(|i| i.index(input.len() + 1)).collect();
        at.sort_unstable();
        let mut chunks: Vec<&[u8]> = Vec::new();
        let mut prev = 0;
        for c in at {
            chunks.push(&input[prev..c]);
            prev = c;
        }
        chunks.push(&input[prev..]);
        check(&chunks);
    }
}
