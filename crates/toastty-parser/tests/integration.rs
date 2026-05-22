//! Cross-cutting parser tests plus a property test that random byte
//! streams never panic the parser.

use proptest::prelude::*;
use toastty_parser::{Params, Parser, Perform};

#[derive(Default)]
struct Counter {
    prints: usize,
    execs: usize,
    csi: usize,
    osc: usize,
    esc: usize,
    hook: usize,
    put: usize,
    unhook: usize,
    apc_start: usize,
    apc_chunk: usize,
    apc_end: usize,
}

impl Perform for Counter {
    fn print(&mut self, _c: char) {
        self.prints += 1;
    }
    fn execute(&mut self, _b: u8) {
        self.execs += 1;
    }
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
        self.hook += 1;
    }
    fn put(&mut self, _: u8) {
        self.put += 1;
    }
    fn unhook(&mut self) {
        self.unhook += 1;
    }
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {
        self.osc += 1;
    }
    fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
        self.csi += 1;
    }
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.esc += 1;
    }
    fn apc_start(&mut self) {
        self.apc_start += 1;
    }
    fn apc_chunk(&mut self, _: &[u8]) {
        self.apc_chunk += 1;
    }
    fn apc_end(&mut self) {
        self.apc_end += 1;
    }
}

#[test]
fn realistic_shell_prompt_output() {
    // Vaguely what `bash -c 'echo hi'` plus a colored prompt would emit.
    let bytes =
        b"\x1b]0;user@host\x1b\\\x1b[01;32mhost\x1b[00m:\x1b[01;34m~\x1b[00m$ echo hi\r\nhi\r\n";
    let mut p = Parser::new();
    let mut c = Counter::default();
    p.advance(&mut c, bytes);
    assert_eq!(c.osc, 1, "set window title");
    assert!(c.csi >= 4, "at least four SGR sequences, got {}", c.csi);
    assert!(c.prints >= 10);
    assert!(c.execs >= 2, "CR/LF");
}

#[test]
fn apc_start_chunk_end_balanced() {
    let bytes = b"\x1b_kitty payload\x1b\\";
    let mut p = Parser::new();
    let mut c = Counter::default();
    p.advance(&mut c, bytes);
    assert_eq!(c.apc_start, 1);
    assert_eq!(c.apc_end, 1);
    assert!(c.apc_chunk >= 1);
}

#[test]
fn byte_at_a_time_matches_bulk() {
    let bytes = b"\x1b[31mRED\x1b[0m \x1b]8;;https://e.com\x1b\\link\x1b]8;;\x1b\\";
    let mut bulk_parser = Parser::new();
    let mut bulk_counter = Counter::default();
    bulk_parser.advance(&mut bulk_counter, bytes);

    let mut byte_parser = Parser::new();
    let mut byte_counter = Counter::default();
    for &b in bytes {
        byte_parser.advance(&mut byte_counter, &[b]);
    }

    assert_eq!(byte_counter.prints, bulk_counter.prints);
    assert_eq!(byte_counter.csi, bulk_counter.csi);
    assert_eq!(byte_counter.osc, bulk_counter.osc);
}

proptest! {
    /// Random byte streams of any shape must not crash the parser.
    /// (Strict event-count parity across split boundaries is *not* a
    /// property we promise — vte buffers UTF-8 fragments and in-flight
    /// CSI/OSC state across `advance` calls, so arbitrary random binary
    /// can legitimately produce different event counts depending on
    /// where it's split. The deterministic split tests in `parser.rs`
    /// cover the cases that matter for real terminal output.)
    #[test]
    fn arbitrary_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..2048)) {
        let mut p = Parser::new();
        let mut c = Counter::default();
        p.advance(&mut c, &bytes);
    }

    /// Splitting at any boundary must not crash either.
    #[test]
    fn arbitrary_chunking_never_panics(
        bytes in proptest::collection::vec(any::<u8>(), 0..1024),
        split_at in 0usize..1024,
    ) {
        let split = split_at.min(bytes.len());
        let mut p = Parser::new();
        let mut c = Counter::default();
        p.advance(&mut c, &bytes[..split]);
        p.advance(&mut c, &bytes[split..]);
    }
}
