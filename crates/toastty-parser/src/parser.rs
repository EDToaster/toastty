use crate::perform::{Perform, VteAdapter};
use crate::utf8::Utf8Sanitizer;

/// State of the APC pre-scanner that sits in front of `vte`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ApcState {
    /// Outside any APC sequence; bytes flow to vte.
    #[default]
    Ground,
    /// Last byte was 0x1B (ESC); the next byte determines whether
    /// this is APC (`ESC _`) or some other escape (forwarded to vte).
    AfterEsc,
    /// Inside an APC payload; bytes flow to [`Perform::apc_chunk`].
    /// It ends at `ESC \\` or BEL. 8-bit ST (0x9C) is data: in a UTF-8
    /// stream it's a continuation byte (`М` is `\xd0\x9c`), and kitty
    /// only ends an APC on `ESC \\` / BEL (WIRE-5).
    Apc,
    /// Inside APC and the last byte was 0x1B; if next is `\\` we end
    /// the APC, otherwise the ESC was data.
    ApcAfterEsc,
    /// Inside an OSC string (`ESC ]`) that vte is buffering; `len`
    /// counts the payload bytes forwarded so far so we can enforce
    /// [`MAX_OSC_LEN`].
    Osc { len: usize },
    /// An OSC overran [`MAX_OSC_LEN`] and was aborted; discard bytes
    /// until the string's terminator so its tail isn't printed.
    OscDiscard,
    /// Saw `ESC P` (vte has the ESC, not the `P`) and the first
    /// `matched` bytes of [`TMUX_PREFIX`]; deciding between a tmux
    /// passthrough and an ordinary DCS.
    DcsIntro { matched: usize },
    /// Inside an ordinary DCS that vte is parsing. Bytes flow to vte
    /// except 8-bit ST (0x9C), which is dropped: vte ends a DCS on it,
    /// but in a UTF-8 stream it's just a byte (the tail of `М`,
    /// `\xd0\x9c`), and kitty only ends strings on `ESC \\` / BEL.
    /// vte ignores bytes >= 0x80 in a DCS anyway, so nothing is lost.
    /// BEL ends the DCS like ST does (kitty; vte would keep it as
    /// data), so it's handed to vte as `ESC \\` (WIRE-13).
    Dcs,
    /// Inside a DCS and the last byte was ESC (not yet given to vte).
    /// `\\` makes it ST. Anything else aborts the DCS: vte would
    /// unhook it as if it had ended normally, so the handler is told
    /// to drop it first, and the ESC then starts a new escape
    /// (WIRE-16).
    DcsAfterEsc,
    /// Inside a PM (`ESC ^`) or SOS (`ESC X`) string. vte would ignore
    /// the body and end it only on ESC / CAN / SUB; kitty also ends it on
    /// BEL (WIRE-13). So the scanner swallows the body itself, and vte
    /// never sees the introducer.
    SosPm,
    /// vte is inside a CSI (`csi`) or an escape sequence still waiting
    /// for its final byte (after intermediates or C0 controls). Bytes
    /// flow to vte; we only watch for one >= 0x80, which kitty 0.49.2
    /// treats as aborting the sequence: the byte is dropped and parsing
    /// resumes in ground at the next one (WIRE-17, WIRE-18).
    Seq { csi: bool },
}

/// Upper bound on one OSC string's payload, in bytes.
///
/// vte 0.15 (with its default `std` feature) accumulates OSC bytes in
/// an unbounded `Vec` until a terminator arrives, so an endless
/// unterminated `ESC ]` (a `cat`'d binary, a hostile remote) grows
/// memory without limit. We count OSC bytes in front of vte and abort
/// the sequence once it passes this cap. 8 MiB is far above any real
/// title / hyperlink / palette string and leaves room for OSC 52
/// clipboard writes of ~6 MiB of text (base64 overhead is 4/3).
pub const MAX_OSC_LEN: usize = 8 * 1024 * 1024;

/// Upper bound on one tmux passthrough's unwrapped payload, in bytes.
///
/// The payload is buffered until its terminator (so an oversized one
/// can be dropped whole), which makes this the parser's worst-case
/// extra memory per stream. It matches [`MAX_OSC_LEN`] so an OSC 52
/// that fits directly also fits wrapped; tmux's own
/// `input-buffer-size` (1 MiB by default) is smaller still.
pub const MAX_PASSTHROUGH_LEN: usize = MAX_OSC_LEN;

/// Bytes after `ESC P` that mark a tmux passthrough
/// (`ESC P tmux ; <payload, ESCs doubled> ESC \`).
const TMUX_PREFIX: &[u8] = b"tmux;";

/// Passthrough buffers larger than this are freed after dispatch
/// rather than kept around for the next sequence.
const PASSTHROUGH_KEEP_CAPACITY: usize = 64 * 1024;

/// Performer that drops every event; used to reset vte's state after
/// aborting an oversized OSC without dispatching the truncated string.
struct NullPerform;

impl vte::Perform for NullPerform {}

/// First byte that ends an OSC string in vte 0.15: BEL, CAN, SUB or
/// ESC (vte terminates on any ESC; `ESC \\` is the ST form).
fn osc_terminator(bytes: &[u8]) -> Option<usize> {
    let a = memchr::memchr3(0x07, 0x18, 0x1B, bytes);
    let b = memchr::memchr(0x1A, &bytes[..a.unwrap_or(bytes.len())]);
    b.or(a)
}

/// First byte in a DCS body the scanner must act on: ESC (maybe ST),
/// BEL (ends it), CAN / SUB (cancel), or 0x9C (dropped, see
/// [`ApcState::Dcs`]).
fn dcs_special(bytes: &[u8]) -> Option<usize> {
    let a = memchr::memchr3(0x1B, 0x07, 0x9C, bytes);
    let b = memchr::memchr2(0x18, 0x1A, &bytes[..a.unwrap_or(bytes.len())]);
    b.or(a)
}

/// First byte that ends a PM / SOS string: ESC, BEL, CAN or SUB (see
/// [`ApcState::SosPm`]).
fn sos_pm_terminator(bytes: &[u8]) -> Option<usize> {
    let a = memchr::memchr3(0x1B, 0x07, 0x18, bytes);
    let b = memchr::memchr(0x1A, &bytes[..a.unwrap_or(bytes.len())]);
    b.or(a)
}

/// What the byte after an ESC starts, for [`ApcState::AfterEsc`]. A
/// table so escape-heavy output pays one load per ESC.
mod after_esc {
    pub(super) const PLAIN: u8 = 0;
    pub(super) const CSI: u8 = 1;
    pub(super) const ESC_SEQ: u8 = 2;
    pub(super) const APC: u8 = 3;
    pub(super) const OSC: u8 = 4;
    pub(super) const DCS: u8 = 5;
    pub(super) const ABORT: u8 = 6;
    pub(super) const SOS_PM: u8 = 7;

    pub(super) const TABLE: [u8; 256] = {
        let mut t = [PLAIN; 256];
        let mut i = 0;
        while i < 256 {
            t[i] = match i as u8 {
                b'[' => CSI,
                b'_' => APC,
                b']' => OSC,
                b'P' => DCS,
                b'^' | b'X' => SOS_PM,
                // An intermediate, or a C0 control vte executes inside the
                // escape: still no final byte.
                0x20..=0x2F | 0x00..=0x17 | 0x19 | 0x1C..=0x1F => ESC_SEQ,
                0x80..=0xFF => ABORT,
                _ => PLAIN,
            };
            i += 1;
        }
        t
    };
}

/// Index of the byte that ends the sequence vte is in, if `bytes` holds
/// it: anything but a parameter/intermediate byte (`0x20..=0x3F` in a
/// CSI, `0x20..=0x2F` in an escape) or a C0 control vte executes in
/// place (everything but CAN, SUB and ESC). Checks 8 bytes at a time:
/// sequences are short, and escape-heavy output has one every few bytes.
#[inline(always)]
fn seq_end(bytes: &[u8], csi: bool) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    let mask: u8 = if csi { 0xE0 } else { 0xF0 };
    let (wmask, want) = (u64::from(mask) * ONES, 0x20 * ONES);
    let mut i = 0;
    while i < bytes.len() {
        if let Some(w) = bytes.get(i..i + 8) {
            let x = u64::from_le_bytes([w[0], w[1], w[2], w[3], w[4], w[5], w[6], w[7]]);
            let t = (x & wmask) ^ want;
            if t == 0 {
                i += 8;
                continue;
            }
            // The lowest set bit is in the first byte out of range.
            i += (t.trailing_zeros() / 8) as usize;
        }
        let b = bytes[i];
        if b & mask == 0x20 || (b < 0x20 && b != 0x18 && b != 0x1A && b != 0x1B) {
            i += 1;
            continue;
        }
        return Some(i);
    }
    None
}

/// Outcome of [`Scanner::step_seq`].
enum Step<'b> {
    /// Go on with this input.
    Rest(&'b [u8]),
    /// All of the input was consumed.
    Done,
    /// The sequence was aborted (WIRE-17); go on with this input, which
    /// may start mid-character.
    Aborted(&'b [u8]),
}

/// The scanner's entry into vte for real events (one copy of vte's large
/// `advance` rather than one per call site).
#[inline(never)]
fn vte_advance<P: Perform + ?Sized>(vte: &mut vte::Parser, perform: &mut P, bytes: &[u8]) {
    vte.advance(&mut VteAdapter { inner: perform }, bytes);
}

/// The APC / OSC-cap pre-scanner plus vte: everything the parser does
/// except tmux passthrough unwrapping, which [`Parser`] layers on top
/// so unwrapped payloads can be fed back through this same path.
struct Scanner {
    vte: vte::Parser,
    apc: ApcState,
    /// OSC payload cap; [`MAX_OSC_LEN`] outside tests.
    max_osc_len: usize,
    /// Rewrites invalid UTF-8 in ground text to U+FFFD the way kitty
    /// does, before vte sees it (WIRE-14).
    utf8: Utf8Sanitizer,
    /// In an all-ASCII input no byte can abort a sequence (WIRE-17), so
    /// the scanner leaves CSI / escape sequences to vte and only notes
    /// here (`Some(csi)`) that the last one may still be open. If it
    /// still is when the input ends, the state becomes
    /// [`ApcState::Seq`] so the next input is checked.
    open_seq: Option<bool>,
    /// Inside a CSI (after `ESC [`) whose final byte hasn't arrived:
    /// whether a parameter digit was seen so far. Reported through
    /// [`Perform::csi_params_explicit`] right before vte dispatches it.
    csi_digits: Option<bool>,
}

impl Scanner {
    fn new(max_osc_len: usize) -> Self {
        Self {
            vte: vte::Parser::default(),
            apc: ApcState::default(),
            max_osc_len,
            utf8: Utf8Sanitizer::default(),
            open_seq: None,
            csi_digits: None,
        }
    }

    /// Hand ground text to vte, through the UTF-8 sanitizer where
    /// needed. `text` starts at offset `at` of this `advance` call's
    /// input, and `clean` is that input's [`Utf8Sanitizer::clean_span`]:
    /// the part of `text` inside it is known-valid and goes straight to
    /// vte; the parts before (finishing a held character) and after (a
    /// trailing partial one, or an invalid chunk) are sanitized.
    /// `cut_by_esc` says an ESC follows directly (see
    /// [`Utf8Sanitizer::feed`]).
    #[inline]
    fn feed_ground<P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        text: &[u8],
        at: usize,
        clean: (usize, usize),
        cut_by_esc: bool,
    ) {
        self.note_csi_params(perform, text);
        // Hot path, the same work as with no sanitizer at all: a run
        // inside the clean span (all of an ASCII or valid chunk).
        if clean.0 <= at && at + text.len() <= clean.1 && !self.utf8.holds_partial() {
            if !text.is_empty() {
                vte_advance(&mut self.vte, perform, text);
            }
            return;
        }
        self.feed_ground_slow(perform, text, at, clean, cut_by_esc);
    }

    /// The rest of [`Self::feed_ground`]: a run that needs the sanitizer
    /// for some or all of its bytes.
    #[inline(never)]
    fn feed_ground_slow<P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        text: &[u8],
        at: usize,
        clean: (usize, usize),
        cut_by_esc: bool,
    ) {
        let vte = &mut self.vte;
        if text.is_empty() {
            // An ESC right away still ends a held partial character.
            if cut_by_esc && self.utf8.holds_partial() {
                self.utf8.feed(text, true, &mut |s| {
                    vte_advance(vte, perform, s);
                });
            }
            return;
        }
        let end = at + text.len();
        let a = clean.0.clamp(at, end) - at;
        let b = clean.1.clamp(at, end).max(at + a) - at;
        let (head, mid, tail) = (&text[..a], &text[a..b], &text[b..]);
        if !head.is_empty() {
            self.utf8
                .feed(head, cut_by_esc && a == text.len(), &mut |s| {
                    vte_advance(vte, perform, s);
                });
        }
        if !mid.is_empty() {
            vte_advance(vte, perform, mid);
        }
        if !tail.is_empty() {
            self.utf8.feed(tail, cut_by_esc, &mut |s| {
                vte_advance(vte, perform, s);
            });
        }
    }

    /// Track the parameter bytes of a CSI in ground text `chunk` (before
    /// vte sees it) and, when its final byte is in `chunk`, tell the
    /// handler whether any digit was present.
    fn note_csi_params<P: Perform + ?Sized>(&mut self, perform: &mut P, chunk: &[u8]) {
        let Some(mut digits) = self.csi_digits else {
            return;
        };
        for &b in chunk {
            match b {
                b'0'..=b'9' => digits = true,
                0x40..=0x7E => {
                    self.csi_digits = None;
                    perform.csi_params_explicit(digits);
                    return;
                }
                // CAN / SUB abort the CSI.
                0x18 | 0x1A => {
                    self.csi_digits = None;
                    return;
                }
                _ => {}
            }
        }
        self.csi_digits = Some(digits);
    }

    /// Feed bytes through the scanner. With `detect_tmux`, stops right
    /// after an `ESC P tmux ;` introducer and returns the unconsumed
    /// rest (vte is left in ground state, as after a cancelled
    /// escape); otherwise consumes everything and returns `None`.
    #[allow(clippy::too_many_lines)] // One state machine; splitting it costs (#41).
    fn advance<'b, P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        mut bytes: &'b [u8],
        detect_tmux: bool,
    ) -> Option<&'b [u8]> {
        // Check this input's UTF-8 once, not per run between escapes. An
        // aborted sequence (WIRE-17/18) can leave the rest off a character
        // boundary: it then restarts this loop with no clean span, so
        // `clean` stays constant inside the hot loop.
        let mut span_ok = true;
        'input: loop {
            let total = bytes.len();
            let (clean, ascii) = if span_ok {
                self.utf8.clean_span_ascii(bytes)
            } else {
                ((0, 0), false)
            };
            while !bytes.is_empty() {
                match self.apc {
                    ApcState::Ground => {
                        // Scan for next ESC. Everything before is normal vte
                        // input, cleaned of invalid UTF-8 first.
                        if let Some(pos) = memchr::memchr(0x1B, bytes) {
                            let at = total - bytes.len();
                            self.feed_ground(perform, &bytes[..pos], at, clean, true);
                            // Whatever sequence was open, this ESC ends it.
                            self.open_seq = None;
                            // Don't feed the ESC to vte yet — peek the next byte.
                            self.apc = ApcState::AfterEsc;
                            bytes = &bytes[pos + 1..];
                        } else {
                            let at = total - bytes.len();
                            self.feed_ground(perform, bytes, at, clean, false);
                            self.close_open_seq(bytes);
                            return None;
                        }
                    }
                    ApcState::AfterEsc => {
                        let byte = bytes[0];
                        bytes = &bytes[1..];
                        // Any ESC ends a CSI in progress; `ESC [` starts one.
                        self.csi_digits = (byte == b'[').then_some(false);
                        let csi = match after_esc::TABLE[usize::from(byte)] {
                            after_esc::APC => {
                                perform.apc_start();
                                self.apc = ApcState::Apc;
                                continue;
                            }
                            after_esc::OSC => {
                                vte_advance(&mut self.vte, perform, &[0x1B, byte]);
                                self.apc = ApcState::Osc { len: 0 };
                                continue;
                            }
                            after_esc::DCS if detect_tmux => {
                                // Give vte the ESC now (it ends any string or
                                // sequence in progress, exactly as the direct
                                // byte stream would) and hold the `P` until we
                                // know whether this is `ESC P tmux ;`.
                                vte_advance(&mut self.vte, perform, &[0x1B]);
                                self.apc = ApcState::DcsIntro { matched: 0 };
                                continue;
                            }
                            after_esc::SOS_PM => {
                                // PM / SOS (#36). The ESC still ends whatever
                                // vte had in progress; CAN then returns vte to
                                // ground (the null performer swallows its
                                // execute), and the scanner eats the body.
                                vte_advance(&mut self.vte, perform, &[0x1B]);
                                self.vte.advance(&mut NullPerform, &[0x18]);
                                self.apc = ApcState::SosPm;
                                continue;
                            }
                            after_esc::DCS => {
                                // Ordinary DCS: vte parses it, the scanner
                                // tracks its body (0x9C, BEL, CAN / SUB, ESC;
                                // #32, #36, #45, #54).
                                vte_advance(&mut self.vte, perform, &[0x1B, byte]);
                                self.apc = ApcState::Dcs;
                                continue;
                            }
                            after_esc::ABORT => {
                                // WIRE-18: kitty drops an ESC followed by a
                                // non-ASCII byte, and the byte; what follows is
                                // ground text (so a multi-byte character's
                                // continuation bytes show as U+FFFD). The ESC
                                // still ends any string vte is in; CAN then
                                // returns it to ground without dispatching.
                                self.abort_sequence(perform, true);
                                self.apc = ApcState::Ground;
                                span_ok = false; // No longer on a character boundary.
                                continue 'input;
                            }
                            after_esc::CSI => true,
                            after_esc::ESC_SEQ => false,
                            _ => {
                                // A complete escape: replay ESC + this byte.
                                vte_advance(&mut self.vte, perform, &[0x1B, byte]);
                                self.apc = ApcState::Ground;
                                continue;
                            }
                        };
                        vte_advance(&mut self.vte, perform, &[0x1B, byte]);
                        if ascii {
                            // Nothing here can abort it: vte alone parses it.
                            self.open_seq = Some(csi);
                            self.apc = ApcState::Ground;
                            continue;
                        }
                        // Usually the sequence ends (final byte, CAN, SUB
                        // or an ESC) before any byte >= 0x80: then vte
                        // alone parses it and the ground path takes the
                        // rest, as without this check. Otherwise (a
                        // non-ASCII byte or DEL first, or the input ends
                        // inside it) step through it.
                        if let Some(pos) = seq_end(bytes, csi)
                            && bytes[pos] < 0x7F
                        {
                            self.apc = ApcState::Ground;
                            continue;
                        }
                        match self.step_seq(perform, bytes, csi, total, clean) {
                            Step::Rest(rest) => bytes = rest,
                            Step::Done => return None,
                            Step::Aborted(rest) => {
                                bytes = rest;
                                span_ok = false; // No longer on a character boundary.
                                continue 'input;
                            }
                        }
                    }
                    ApcState::Seq { csi } => {
                        match self.step_seq(perform, bytes, csi, total, clean) {
                            Step::Rest(rest) => bytes = rest,
                            Step::Done => return None,
                            Step::Aborted(rest) => {
                                bytes = rest;
                                span_ok = false; // No longer on a character boundary.
                                continue 'input;
                            }
                        }
                    }
                    ApcState::DcsIntro { matched } => {
                        if bytes[0] == TMUX_PREFIX[matched] {
                            bytes = &bytes[1..];
                            if matched + 1 < TMUX_PREFIX.len() {
                                self.apc = ApcState::DcsIntro {
                                    matched: matched + 1,
                                };
                                continue;
                            }
                            // Full introducer. vte is in its escape state
                            // from the ESC; CAN returns it to ground (the
                            // null performer swallows CAN's execute).
                            self.vte.advance(&mut NullPerform, &[0x18]);
                            self.apc = ApcState::Ground;
                            return Some(bytes);
                        }
                        // Ordinary DCS: replay what we held back and
                        // reprocess this byte as DCS body — it may itself
                        // be an ESC.
                        vte_advance(&mut self.vte, perform, b"P");
                        if matched > 0 {
                            vte_advance(&mut self.vte, perform, &TMUX_PREFIX[..matched]);
                        }
                        self.apc = ApcState::Dcs;
                    }
                    ApcState::Dcs => bytes = self.advance_dcs(perform, bytes),
                    ApcState::DcsAfterEsc => {
                        if bytes[0] == b'\\' {
                            bytes = &bytes[1..];
                            vte_advance(&mut self.vte, perform, b"\x1b\\");
                            self.apc = ApcState::Ground;
                        } else {
                            // Not ST (WIRE-16). End the DCS without acting
                            // on it (vte unhooks on the ESC; CAN then returns
                            // it to ground, swallowed by the null performer)
                            // and reprocess the ESC, with this byte, as a
                            // fresh escape.
                            perform.dcs_cancel();
                            vte_advance(&mut self.vte, perform, &[0x1B]);
                            self.vte.advance(&mut NullPerform, &[0x18]);
                            self.apc = ApcState::AfterEsc;
                        }
                    }
                    ApcState::SosPm => bytes = self.advance_sos_pm(perform, bytes),
                    ApcState::Apc => {
                        // Terminators: ESC (ST is `ESC \\`) or BEL, not 0x9C.
                        if let Some(pos) = memchr::memchr2(0x1B, 0x07, bytes) {
                            if pos > 0 {
                                perform.apc_chunk(&bytes[..pos]);
                            }
                            let term = bytes[pos];
                            bytes = &bytes[pos + 1..];
                            match term {
                                0x1B => self.apc = ApcState::ApcAfterEsc,
                                0x07 => {
                                    perform.apc_end();
                                    self.apc = ApcState::Ground;
                                }
                                _ => unreachable!(),
                            }
                        } else {
                            perform.apc_chunk(bytes);
                            return None;
                        }
                    }
                    ApcState::ApcAfterEsc => {
                        let byte = bytes[0];
                        bytes = &bytes[1..];
                        match byte {
                            b'\\' => {
                                perform.apc_end();
                                self.apc = ApcState::Ground;
                            }
                            0x1B => {
                                // Prior ESC was data; this might still be ST.
                                perform.apc_chunk(&[0x1B]);
                                // stay in ApcAfterEsc
                            }
                            _ => {
                                perform.apc_chunk(&[0x1B, byte]);
                                self.apc = ApcState::Apc;
                            }
                        }
                    }
                    ApcState::Osc { len } => bytes = self.advance_osc(perform, bytes, len),
                    ApcState::OscDiscard => bytes = self.discard_osc(bytes),
                }
            }
            self.close_open_seq(&[]);
            return None;
        }
    }

    /// End of an input: if an ASCII-mode sequence (see
    /// [`Scanner::open_seq`]) is still open after `last_run`, the next
    /// input continues it, so track it from now on.
    #[inline]
    fn close_open_seq(&mut self, last_run: &[u8]) {
        if let Some(csi) = self.open_seq.take()
            && self.apc == ApcState::Ground
            && seq_end(last_run, csi).is_none()
        {
            self.apc = ApcState::Seq { csi };
        }
    }

    /// Pass a CSI (`csi`) or unfinished escape sequence to vte up to the
    /// byte that ends it: its final byte, CAN/SUB (vte aborts), ESC (vte
    /// starts over), or a non-ASCII byte (kitty aborts, WIRE-17). A
    /// sequence still open at the end of `bytes` leaves the state at
    /// [`ApcState::Seq`].
    #[inline]
    fn step_seq<'b, P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        mut bytes: &'b [u8],
        csi: bool,
        total: usize,
        clean: (usize, usize),
    ) -> Step<'b> {
        loop {
            let Some(pos) = seq_end(bytes, csi) else {
                if !bytes.is_empty() {
                    self.note_csi_params(perform, bytes);
                    vte_advance(&mut self.vte, perform, bytes);
                }
                self.apc = ApcState::Seq { csi };
                return Step::Done;
            };
            let b = bytes[pos];
            if b == 0x7F {
                // DEL: vte ignores it inside a sequence. Pass it, go on.
                self.note_csi_params(perform, &bytes[..=pos]);
                vte_advance(&mut self.vte, perform, &bytes[..=pos]);
                bytes = &bytes[pos + 1..];
                continue;
            }
            if b == 0x1B {
                if pos > 0 {
                    self.note_csi_params(perform, &bytes[..pos]);
                    vte_advance(&mut self.vte, perform, &bytes[..pos]);
                }
                self.apc = ApcState::AfterEsc;
                return Step::Rest(&bytes[pos + 1..]);
            }
            if b >= 0x80 {
                if pos > 0 {
                    self.note_csi_params(perform, &bytes[..pos]);
                    vte_advance(&mut self.vte, perform, &bytes[..pos]);
                }
                self.abort_sequence(perform, false);
                self.apc = ApcState::Ground;
                return Step::Aborted(&bytes[pos + 1..]);
            }
            // Ended normally (final byte, CAN or SUB). Hand vte the ground
            // text up to the next ESC in the same call when it is
            // known-clean, as the ground path would.
            let rest = &bytes[pos + 1..];
            let at = total - rest.len();
            let next_esc = memchr::memchr(0x1B, rest);
            let run = next_esc.unwrap_or(rest.len());
            if clean.0 <= at && at + run <= clean.1 && !self.utf8.holds_partial() {
                self.note_csi_params(perform, &bytes[..=pos + run]);
                vte_advance(&mut self.vte, perform, &bytes[..=pos + run]);
                let Some(e) = next_esc else {
                    self.apc = ApcState::Ground;
                    return Step::Done;
                };
                self.apc = ApcState::AfterEsc;
                return Step::Rest(&rest[e + 1..]);
            }
            self.note_csi_params(perform, &bytes[..=pos]);
            vte_advance(&mut self.vte, perform, &bytes[..=pos]);
            self.apc = ApcState::Ground;
            return Step::Rest(rest);
        }
    }

    /// Abort the escape or CSI sequence vte is in, with no dispatch, the
    /// way kitty drops one cut by a non-ASCII byte (WIRE-17, WIRE-18).
    /// `with_esc` first gives vte the held ESC, which ends any string in
    /// progress as usual. CAN then returns vte to ground; the null
    /// performer swallows CAN's own execute.
    fn abort_sequence<P: Perform + ?Sized>(&mut self, perform: &mut P, with_esc: bool) {
        self.csi_digits = None;
        if with_esc {
            vte_advance(&mut self.vte, perform, &[0x1B]);
        }
        self.vte.advance(&mut NullPerform, &[0x18]);
    }

    /// `ApcState::SosPm` step: drop the string body up to its
    /// terminator. Returns the unconsumed input.
    fn advance_sos_pm<'b, P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        bytes: &'b [u8],
    ) -> &'b [u8] {
        let Some(pos) = sos_pm_terminator(bytes) else {
            return &[];
        };
        match bytes[pos] {
            0x07 => self.apc = ApcState::Ground,
            // ESC ends the string (vte semantics); `ESC \\` is the ST.
            0x1B => self.apc = ApcState::AfterEsc,
            // CAN / SUB: executed, as vte does when they end a string.
            byte => {
                vte_advance(&mut self.vte, perform, &[byte]);
                self.apc = ApcState::Ground;
            }
        }
        &bytes[pos + 1..]
    }

    /// `ApcState::Dcs` step: forward DCS bytes to vte, dropping 0x9C
    /// (see [`ApcState::Dcs`]). Returns the unconsumed input.
    fn advance_dcs<'b, P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        bytes: &'b [u8],
    ) -> &'b [u8] {
        let Some(pos) = dcs_special(bytes) else {
            vte_advance(&mut self.vte, perform, bytes);
            return &[];
        };
        if pos > 0 {
            vte_advance(&mut self.vte, perform, &bytes[..pos]);
        }
        match bytes[pos] {
            // Data, not ST: drop it.
            0x9C => {}
            // Possibly ST: decided on the next byte.
            0x1B => self.apc = ApcState::DcsAfterEsc,
            // BEL ends the DCS exactly like ST.
            0x07 => {
                vte_advance(&mut self.vte, perform, b"\x1b\\");
                self.apc = ApcState::Ground;
            }
            // CAN / SUB cancel the DCS (xterm, vte). vte unhooks it as
            // if it had ended normally, so tell the handler first that
            // it must not be acted on (WIRE-15).
            byte => {
                perform.dcs_cancel();
                vte_advance(&mut self.vte, perform, &[byte]);
                self.apc = ApcState::Ground;
            }
        }
        &bytes[pos + 1..]
    }

    /// `ApcState::Osc` step: forward OSC bytes to vte while counting
    /// them, aborting the string if it outgrows `max_osc_len`. Returns
    /// the unconsumed input.
    fn advance_osc<'b, P: Perform + ?Sized>(
        &mut self,
        perform: &mut P,
        bytes: &'b [u8],
        len: usize,
    ) -> &'b [u8] {
        let term = osc_terminator(bytes);
        let body = term.unwrap_or(bytes.len());
        if len + body > self.max_osc_len {
            // Too long: make vte drop what it has buffered (CAN aborts
            // the string; the null performer swallows the truncated
            // dispatch) and skip the rest of this OSC.
            self.vte.advance(&mut NullPerform, &[0x18]);
            self.apc = ApcState::OscDiscard;
            return &bytes[body..];
        }
        if body > 0 {
            vte_advance(&mut self.vte, perform, &bytes[..body]);
        }
        let Some(pos) = term else {
            self.apc = ApcState::Osc { len: len + body };
            return &[];
        };
        if bytes[pos] == 0x1B {
            // Possibly ST; let the ESC peek logic replay it.
            self.apc = ApcState::AfterEsc;
        } else {
            vte_advance(&mut self.vte, perform, &bytes[pos..=pos]);
            self.apc = ApcState::Ground;
        }
        &bytes[pos + 1..]
    }

    /// `ApcState::OscDiscard` step: drop bytes up to and including the
    /// aborted OSC's terminator. Returns the unconsumed input.
    fn discard_osc<'b>(&mut self, bytes: &'b [u8]) -> &'b [u8] {
        let Some(pos) = osc_terminator(bytes) else {
            return &[];
        };
        self.apc = if bytes[pos] == 0x1B {
            ApcState::AfterEsc
        } else {
            ApcState::Ground
        };
        &bytes[pos + 1..]
    }
}

/// tmux passthrough unwrapping state (`ESC P tmux ; … ESC \`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum TmuxState {
    /// Not inside a passthrough; bytes go to the [`Scanner`].
    #[default]
    Off,
    /// Inside a passthrough payload, buffering unwrapped bytes.
    Body,
    /// Inside a payload and the last byte was ESC: `\` ends the
    /// passthrough, anything else is the escaped byte (`ESC ESC` is a
    /// literal ESC).
    BodyAfterEsc,
    /// The payload outgrew the cap and was dropped; skip to its end.
    Discard,
    /// Discarding and the last byte was ESC.
    DiscardAfterEsc,
}

/// Top-level parser. Pre-scans the byte stream for APC sequences
/// (which vte 0.15 drops) and forwards everything else to vte. Also
/// tracks OSC strings so their length can be capped (see
/// [`MAX_OSC_LEN`]).
///
/// tmux passthrough (`ESC P tmux ; <payload> ESC \`, every ESC in the
/// payload doubled) is unwrapped here as well: the payload is
/// buffered (up to [`MAX_PASSTHROUGH_LEN`]), un-doubled, and fed back
/// through the normal path once the terminator arrives, so wrapped
/// OSC / APC / DCS / CSI sequences behave exactly as if they had been
/// sent bare.
pub struct Parser {
    scanner: Scanner,
    tmux: TmuxState,
    /// Unwrapped payload of the passthrough in progress.
    passthrough: Vec<u8>,
    /// Passthrough payload cap; [`MAX_PASSTHROUGH_LEN`] outside tests.
    max_passthrough_len: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Self {
            scanner: Scanner::new(MAX_OSC_LEN),
            tmux: TmuxState::default(),
            passthrough: Vec::new(),
            max_passthrough_len: MAX_PASSTHROUGH_LEN,
        }
    }
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("apc", &self.scanner.apc)
            .field("tmux", &self.tmux)
            .field("passthrough_len", &self.passthrough.len())
            .finish_non_exhaustive()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Parser with a custom OSC length cap (tests use a tiny one).
    #[cfg(test)]
    fn with_max_osc_len(max_osc_len: usize) -> Self {
        Self {
            scanner: Scanner::new(max_osc_len),
            ..Self::default()
        }
    }

    /// Parser with a custom passthrough payload cap (tests use a tiny one).
    #[cfg(test)]
    fn with_max_passthrough_len(max_passthrough_len: usize) -> Self {
        Self {
            max_passthrough_len,
            ..Self::default()
        }
    }

    /// Feed bytes through the parser. Calls handler methods on
    /// `perform` as events are recognized.
    pub fn advance<P: Perform + ?Sized>(&mut self, perform: &mut P, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.tmux {
                TmuxState::Off => match self.scanner.advance(perform, bytes, true) {
                    None => return,
                    Some(rest) => {
                        self.passthrough.clear();
                        self.tmux = TmuxState::Body;
                        bytes = rest;
                    }
                },
                TmuxState::Body => {
                    let pos = memchr::memchr3(0x1B, 0x18, 0x1A, bytes);
                    let body = pos.unwrap_or(bytes.len());
                    if !self.push_passthrough(&bytes[..body]) {
                        bytes = &bytes[body..];
                        continue;
                    }
                    let Some(pos) = pos else { return };
                    let byte = bytes[pos];
                    bytes = &bytes[pos + 1..];
                    if byte == 0x1B {
                        self.tmux = TmuxState::BodyAfterEsc;
                    } else {
                        // CAN / SUB cancel a DCS (ECMA-48, vte, xterm):
                        // drop the payload and let the control itself
                        // reach the scanner as it would outside one.
                        self.end_passthrough();
                        self.scanner.advance(perform, &[byte], true);
                    }
                }
                TmuxState::BodyAfterEsc => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    if byte == b'\\' {
                        self.dispatch_passthrough(perform);
                    } else {
                        // tmux keeps the byte after an ESC and drops the
                        // ESC, so `ESC ESC` is one literal ESC (and a
                        // stray `ESC x` is just `x`).
                        self.tmux = TmuxState::Body;
                        self.push_passthrough(&[byte]);
                    }
                }
                TmuxState::Discard => {
                    let Some(pos) = memchr::memchr3(0x1B, 0x18, 0x1A, bytes) else {
                        return;
                    };
                    let byte = bytes[pos];
                    bytes = &bytes[pos + 1..];
                    if byte == 0x1B {
                        self.tmux = TmuxState::DiscardAfterEsc;
                    } else {
                        self.tmux = TmuxState::Off;
                        self.scanner.advance(perform, &[byte], true);
                    }
                }
                TmuxState::DiscardAfterEsc => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    self.tmux = if byte == b'\\' {
                        TmuxState::Off
                    } else {
                        TmuxState::Discard
                    };
                }
            }
        }
    }

    /// Append unwrapped payload bytes, switching to
    /// [`TmuxState::Discard`] (and dropping the buffer) if they would
    /// exceed the cap. Returns `false` on overflow.
    fn push_passthrough(&mut self, bytes: &[u8]) -> bool {
        if self.passthrough.len() + bytes.len() > self.max_passthrough_len {
            self.end_passthrough();
            self.tmux = TmuxState::Discard;
            return false;
        }
        self.passthrough.extend_from_slice(bytes);
        true
    }

    /// Feed the finished payload through the scanner. tmux passthrough
    /// does not nest, so an inner `ESC P tmux ;` is an ordinary DCS.
    /// Whatever state the payload leaves the scanner in (an
    /// unterminated OSC, say) carries on, as if it had been sent bare.
    fn dispatch_passthrough<P: Perform + ?Sized>(&mut self, perform: &mut P) {
        let payload = std::mem::take(&mut self.passthrough);
        self.scanner.advance(perform, &payload, false);
        self.passthrough = payload;
        self.end_passthrough();
    }

    /// Leave passthrough mode and release the payload buffer.
    fn end_passthrough(&mut self) {
        self.tmux = TmuxState::Off;
        self.passthrough.clear();
        if self.passthrough.capacity() > PASSTHROUGH_KEEP_CAPACITY {
            self.passthrough = Vec::new();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Params;

    type ParamVec = Vec<Vec<u16>>;
    type DcsRecord = (ParamVec, Vec<u8>, char, Vec<u8>);

    #[derive(Default, Debug, PartialEq)]
    struct Recorder {
        text: String,
        execs: Vec<u8>,
        csi: Vec<(ParamVec, Vec<u8>, char)>,
        osc: Vec<Vec<Vec<u8>>>,
        esc: Vec<(Vec<u8>, u8)>,
        dcs: Vec<DcsRecord>,
        dcs_active: Option<DcsRecord>,
        apc: Vec<Vec<u8>>,
        apc_pending: Vec<u8>,
    }

    fn params_to_vecs(params: &Params) -> ParamVec {
        params.iter().map(<[u16]>::to_vec).collect()
    }

    impl Perform for Recorder {
        fn print(&mut self, c: char) {
            self.text.push(c);
        }
        fn execute(&mut self, byte: u8) {
            self.execs.push(byte);
        }
        fn hook(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
            self.dcs_active = Some((
                params_to_vecs(params),
                intermediates.to_vec(),
                action,
                Vec::new(),
            ));
        }
        fn put(&mut self, byte: u8) {
            if let Some(ref mut active) = self.dcs_active {
                active.3.push(byte);
            }
        }
        fn unhook(&mut self) {
            if let Some(active) = self.dcs_active.take() {
                self.dcs.push(active);
            }
        }
        fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
            self.osc.push(params.iter().map(|p| p.to_vec()).collect());
        }
        fn csi_dispatch(
            &mut self,
            params: &Params,
            intermediates: &[u8],
            _ignore: bool,
            action: char,
        ) {
            self.csi
                .push((params_to_vecs(params), intermediates.to_vec(), action));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
            self.esc.push((intermediates.to_vec(), byte));
        }
        fn apc_start(&mut self) {
            self.apc_pending.clear();
        }
        fn apc_chunk(&mut self, bytes: &[u8]) {
            self.apc_pending.extend_from_slice(bytes);
        }
        fn apc_end(&mut self) {
            let payload = std::mem::take(&mut self.apc_pending);
            self.apc.push(payload);
        }
    }

    fn parse(bytes: &[u8]) -> Recorder {
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, bytes);
        r
    }

    /// `bytes` whole and byte by byte must give the same events.
    fn parse_both(bytes: &[u8]) -> Recorder {
        let whole = parse(bytes);
        let mut p = Parser::new();
        let mut r = Recorder::default();
        for b in bytes {
            p.advance(&mut r, std::slice::from_ref(b));
        }
        assert_eq!(whole, r, "byte-at-a-time differs for {bytes:?}");
        whole
    }

    /// WIRE-17, checked on kitty 0.49.2 with `kitten @ get-text`: a byte
    /// of 0x80 or more inside a CSI aborts it (no dispatch) and is dropped; the
    /// rest is ground text, so a multi-byte character's continuation
    /// bytes show as U+FFFD.
    #[test]
    fn high_byte_inside_csi_aborts_it_like_kitty() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b[3\x9c1mX", "1mX"),
            (b"\x1b[31\x9cmX", "mX"),
            (b"\x1b[\x9c31mX", "31mX"),
            (b"\x1b[3\x801mX", "1mX"),
            (b"\x1b[3\xff1mX", "1mX"),
            (b"\x1b[3\xe2\x82\xac1mX", "\u{fffd}\u{fffd}1mX"),
            (b"\x1b[3\xc3\xa91mX", "\u{fffd}1mX"),
            (b"\x1b[?25\x9chX", "hX"),
            (b"\x1b[1;2\x9cHX", "HX"),
            (b"\x1b[3\xc2\x9c1mX", "\u{fffd}1mX"),
            (b"\x1b[3\xc2\x851mX", "\u{fffd}1mX"),
            (b"\x1b[1 \x9cqX", "qX"),
        ];
        for &(input, want) in cases {
            let r = parse_both(input);
            assert_eq!(r.text, want, "{input:?}");
            assert!(r.csi.is_empty() && r.esc.is_empty(), "{input:?}: {r:?}");
        }
        // The next sequence after an aborted one works.
        let r = parse_both(b"\x1b[3\x9c\x1b[32mG");
        assert_eq!((r.text.as_str(), r.csi.len()), ("G", 1));
        assert_eq!(r.csi[0].0, vec![vec![32u16]]);
        // C0 inside a CSI is still executed and the CSI goes on (kitty
        // `ESC[3 BS 1mX` applies SGR 31).
        let r = parse_both(b"\x1b[3\x081mX");
        assert_eq!((r.text.as_str(), r.execs.as_slice()), ("X", &[0x08u8][..]));
        assert_eq!(r.csi[0].0, vec![vec![31u16]]);
    }

    /// WIRE-18 (kitty 0.49.2): ESC followed by a non-ASCII byte is
    /// dropped with that byte, at any point before the escape's final
    /// byte; the rest is ground text.
    #[test]
    fn esc_followed_by_non_ascii_is_dropped_like_kitty() {
        let cases: &[(&[u8], &str)] = &[
            (b"\x1b\xe2\x82\xacA", "\u{fffd}\u{fffd}A"),
            (b"\x1b\xc3\xa9A", "\u{fffd}A"),
            (b"\x1b\x9cA", "A"),
            (b"\x1b\x80A", "A"),
            (b"\x1b\xffA", "A"),
            (b"\x1b\xc2\x9cA", "\u{fffd}A"),
            (b"\x1b(\xe2\x82\xacA", "\u{fffd}\u{fffd}A"),
            (b"\x1b\xf0\x9f\x98\x80A", "\u{fffd}\u{fffd}\u{fffd}A"),
            (b"\x1b(\x9cBX", "BX"),
            (b"\x1b#\x9c8X", "8X"),
        ];
        for &(input, want) in cases {
            let r = parse_both(input);
            assert_eq!(r.text, want, "{input:?}");
            assert!(r.esc.is_empty() && r.csi.is_empty(), "{input:?}: {r:?}");
        }
        // Ordinary escapes are untouched.
        let r = parse_both(b"\x1b(BX\x1b7\x1b8ok");
        assert_eq!(r.text, "Xok");
        assert_eq!(r.esc.len(), 3);
    }

    /// The dropped byte can be split from its continuation bytes by a
    /// read boundary: those still show as U+FFFD.
    #[test]
    fn abort_split_across_reads() {
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1b[3\xe2");
        p.advance(&mut r, b"\x82\xac1mX");
        assert_eq!(r.text, "\u{fffd}\u{fffd}1mX");
        assert!(r.csi.is_empty());
        // An all-ASCII read that ends inside a sequence (the fast path
        // that leaves sequences to vte) still lets the next read abort it.
        for (a, b, want) in [
            (&b"x\x1b[3"[..], &b"\x9c1mX"[..], "x1mX"),
            (b"x\x1b[", b"\x9c31mX", "x31mX"),
            (b"x\x1b(", b"\xe2\x82\xacA", "x\u{fffd}\u{fffd}A"),
        ] {
            let mut p = Parser::new();
            let mut r = Recorder::default();
            p.advance(&mut r, a);
            p.advance(&mut r, b);
            assert_eq!(r.text, want, "{a:?} | {b:?}");
            assert!(r.csi.is_empty() && r.esc.is_empty(), "{r:?}");
        }
    }

    #[test]
    fn plain_text_becomes_print_events() {
        let r = parse(b"hello");
        assert_eq!(r.text, "hello");
    }

    #[test]
    fn c0_controls_become_execute_events() {
        let r = parse(b"\x07\x08\r\n\t");
        assert_eq!(r.execs, vec![0x07, 0x08, b'\r', b'\n', b'\t']);
    }

    #[test]
    fn csi_sgr_red_dispatches() {
        let r = parse(b"\x1b[31m");
        assert_eq!(r.csi.len(), 1);
        let (params, intermediates, action) = &r.csi[0];
        assert_eq!(params, &vec![vec![31u16]]);
        assert_eq!(intermediates, b"");
        assert_eq!(*action, 'm');
    }

    #[test]
    fn csi_with_multiple_params_and_intermediates() {
        // CSI ? 25 h — DECSET 25 (cursor visible)
        let r = parse(b"\x1b[?25h");
        assert_eq!(r.csi.len(), 1);
        let (params, intermediates, action) = &r.csi[0];
        assert_eq!(params, &vec![vec![25u16]]);
        assert_eq!(intermediates, b"?");
        assert_eq!(*action, 'h');
    }

    #[test]
    fn osc_set_title() {
        let r = parse(b"\x1b]0;hello\x1b\\");
        assert_eq!(r.osc.len(), 1);
        assert_eq!(r.osc[0], vec![b"0".to_vec(), b"hello".to_vec()]);
    }

    #[test]
    fn osc_terminated_by_bel() {
        let r = parse(b"\x1b]2;title\x07");
        assert_eq!(r.osc.len(), 1);
        assert_eq!(r.osc[0], vec![b"2".to_vec(), b"title".to_vec()]);
    }

    fn parse_capped(cap: usize, chunks: &[&[u8]]) -> Recorder {
        let mut p = Parser::with_max_osc_len(cap);
        let mut r = Recorder::default();
        for c in chunks {
            p.advance(&mut r, c);
        }
        r
    }

    #[test]
    fn default_osc_cap_is_max_osc_len() {
        assert_eq!(Parser::new().scanner.max_osc_len, MAX_OSC_LEN);
        assert_eq!(Parser::new().max_passthrough_len, MAX_PASSTHROUGH_LEN);
    }

    #[test]
    fn osc_at_cap_dispatches_across_chunks() {
        // "52;c;aGk=" is 9 bytes; split mid-payload and ST-terminated.
        let r = parse_capped(9, &[b"\x1b]52;c", b";aGk=", b"\x1b\\x"]);
        assert_eq!(
            r.osc,
            vec![vec![b"52".to_vec(), b"c".to_vec(), b"aGk=".to_vec()]]
        );
        assert_eq!(r.text, "x");
        // BEL / CAN / SUB terminators still dispatch.
        for t in [0x07u8, 0x18, 0x1A] {
            let r = parse_capped(9, &[b"\x1b]52;c;aGk=", &[t], b"y"]);
            assert_eq!(r.osc.len(), 1, "terminator {t:#x}");
            assert_eq!(r.text, "y");
        }
    }

    #[test]
    fn oversized_osc_is_dropped_and_tail_discarded() {
        // One byte over the cap, BEL-terminated: no dispatch, and the
        // remainder of the string never prints.
        let r = parse_capped(8, &[b"\x1b]52;c;aGk=", b"zzzz", b"zz\x07ok"]);
        assert!(r.osc.is_empty());
        assert_eq!(r.text, "ok");
        assert!(r.execs.is_empty(), "abort CAN must not leak: {:?}", r.execs);
        // ST-terminated variant, overflow detected in a single chunk.
        let r = parse_capped(4, &[b"\x1b]0;long title\x1b\\ok"]);
        assert!(r.osc.is_empty());
        assert_eq!(r.text, "ok");
        // An endless unterminated OSC stays discarded across chunks;
        // the next OSC after a terminator parses normally.
        let mut p = Parser::with_max_osc_len(16);
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1b]52;c;");
        for _ in 0..100 {
            p.advance(&mut r, b"QUFBQUFBQUFB");
        }
        assert!(r.text.is_empty());
        p.advance(&mut r, b"\x18\x1b]2;t\x07z");
        assert_eq!(r.osc, vec![vec![b"2".to_vec(), b"t".to_vec()]]);
        assert_eq!(r.text, "z");
    }

    #[test]
    fn osc_cap_resets_per_sequence() {
        // Two back-to-back OSCs each under the cap but together over it.
        let r = parse_capped(7, &[b"\x1b]2;abcde\x07\x1b]2;fghij\x07"]);
        assert_eq!(r.osc.len(), 2);
    }

    #[test]
    fn csi_params_explicit_tells_absent_from_zero() {
        #[derive(Default)]
        struct Rec(Vec<(bool, char)>, Option<bool>);
        impl Perform for Rec {
            fn csi_params_explicit(&mut self, any_digit: bool) {
                self.1 = Some(any_digit);
            }
            fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, action: char) {
                self.0
                    .push((self.1.take().expect("hook before dispatch"), action));
            }
        }
        let mut r = Rec::default();
        Parser::new().advance(&mut r, b"\x1b[<u\x1b[<0u\x1b[u\x1b[5;0ux\x1b[?1h");
        assert_eq!(
            r.0,
            vec![
                (false, 'u'),
                (true, 'u'),
                (false, 'u'),
                (true, 'u'),
                (true, 'h')
            ]
        );
        // Split inside the CSI.
        let mut r = Rec::default();
        let mut p = Parser::new();
        p.advance(&mut r, b"\x1b[<");
        p.advance(&mut r, b"0");
        p.advance(&mut r, b"u");
        assert_eq!(r.0, vec![(true, 'u')]);
    }

    #[test]
    fn esc_dispatch_for_keypad_mode() {
        let r = parse(b"\x1b=");
        assert_eq!(r.esc, vec![(vec![], b'=')]);
    }

    #[test]
    fn dcs_full_roundtrip() {
        // DCS 1$r 0 q ST  (some DCS reply payload "0")
        let r = parse(b"\x1bP1$r0\x1b\\");
        assert_eq!(r.dcs.len(), 1);
        let (params, intermediates, action, body) = &r.dcs[0];
        assert_eq!(params, &vec![vec![1u16]]);
        assert_eq!(intermediates, b"$");
        assert_eq!(*action, 'r');
        assert_eq!(body, b"0");
    }

    #[test]
    fn apc_single_payload_st_terminated() {
        let r = parse(b"\x1b_Gf=24,s=1\x1b\\");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"Gf=24,s=1".to_vec());
    }

    #[test]
    fn apc_bel_terminator() {
        let r = parse(b"\x1b_kitty data\x07");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"kitty data".to_vec());
    }

    /// WIRE-5: 0x9C — bare, the tail of UTF-8 `М` (`\xd0\x9c`), or the
    /// UTF-8 encoding of U+009C (`\xc2\x9c`) — never ends a string
    /// sequence (kitty 0.49.2). Only `ESC \\` (and BEL where vte /
    /// the APC scanner accept it) does.
    const C1_ST_BODIES: [&[u8]; 3] = [b"a\x9cb", b"a\xd0\x9cb", b"a\xc2\x9cb"];

    #[test]
    fn apc_eight_bit_st_is_data() {
        for body in C1_ST_BODIES {
            let mut input = b"\x1b_X".to_vec();
            input.extend_from_slice(body);
            input.extend_from_slice(b"\x1b\\after");
            let r = parse(&input);
            let mut want = b"X".to_vec();
            want.extend_from_slice(body);
            assert_eq!(r.apc, vec![want], "body {body:x?}");
            assert_eq!(r.text, "after", "body {body:x?}");
        }
    }

    #[test]
    fn apc_eight_bit_st_is_data_across_chunks() {
        // Streaming: the 0x9C lands at a chunk boundary.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1b_Xhello \xd0");
        p.advance(&mut r, b"\x9c");
        p.advance(&mut r, b" world\x1b");
        p.advance(&mut r, b"\\ok");
        assert_eq!(r.apc, vec![b"Xhello \xd0\x9c world".to_vec()]);
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn dcs_eight_bit_st_does_not_end_it() {
        for body in C1_ST_BODIES {
            let mut input = b"\x1bP1$r".to_vec();
            input.extend_from_slice(body);
            input.extend_from_slice(b"\x1b\\after");
            let r = parse(&input);
            // vte drops bytes >= 0x80 from a DCS body; the rest is kept
            // and the DCS ends at ESC \\, not at 0x9C.
            assert_eq!(r.dcs.len(), 1, "body {body:x?}");
            assert_eq!(r.dcs[0].3, b"ab", "body {body:x?}");
            assert_eq!(r.text, "after", "body {body:x?}");
        }
        // Split across chunks, and a DCS that only looked like tmux.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1bPtm");
        p.advance(&mut r, b"x\x9c");
        p.advance(&mut r, b"y\x1b\\ok");
        assert_eq!(r.dcs.len(), 1);
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn dcs_still_ends_on_st_can_and_sub() {
        // vte unhooks (dispatches) the DCS on each terminator, and CAN /
        // SUB are then executed; the byte after is ground text again.
        for (term, execs) in [
            (&b"\x1b\\"[..], &b""[..]),
            (b"\x18", b"\x18"),
            (b"\x1a", b"\x1a"),
        ] {
            let mut input = b"\x1bP1$rab".to_vec();
            input.extend_from_slice(term);
            input.extend_from_slice(b"x");
            let r = parse(&input);
            assert_eq!(r.dcs.len(), 1, "terminator {term:x?}");
            let (params, intermediates, action, body) = &r.dcs[0];
            assert_eq!(params, &vec![vec![1u16]], "terminator {term:x?}");
            assert_eq!(intermediates, b"$", "terminator {term:x?}");
            assert_eq!(*action, 'r', "terminator {term:x?}");
            assert_eq!(body, b"ab", "terminator {term:x?}");
            assert_eq!(r.execs, execs, "terminator {term:x?}");
            assert_eq!(r.text, "x", "terminator {term:x?}");
        }
        // A DCS right after another one, and back-to-back ESCs (the
        // first ESC ends DCS 2; `ESC ESC \\` is then a lone ST).
        let r = parse(b"\x1bP1$ra\x1b\\\x1bP2$rb\x1b\x1b\\z");
        let bodies: Vec<(Vec<Vec<u16>>, &[u8])> = r
            .dcs
            .iter()
            .map(|d| (d.0.clone(), d.3.as_slice()))
            .collect();
        assert_eq!(
            bodies,
            vec![(vec![vec![1u16]], &b"a"[..]), (vec![vec![2u16]], &b"b"[..])]
        );
        assert_eq!(r.text, "z");
    }

    #[test]
    fn dcs_cancel_is_signalled_before_unhook() {
        #[derive(Default)]
        struct Rec(Vec<&'static str>);
        impl Perform for Rec {
            fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {
                self.0.push("hook");
            }
            fn dcs_cancel(&mut self) {
                self.0.push("cancel");
            }
            fn unhook(&mut self) {
                self.0.push("unhook");
            }
        }
        for cancel in [0x18u8, 0x1A] {
            let mut input = b"\x1bP$qm".to_vec();
            input.push(cancel);
            let mut r = Rec::default();
            Parser::new().advance(&mut r, &input);
            assert_eq!(r.0, ["hook", "cancel", "unhook"], "{cancel:#x}");
        }
        // CAN in a separate read.
        let mut r = Rec::default();
        let mut p = Parser::new();
        p.advance(&mut r, b"\x1bP$qm");
        p.advance(&mut r, b"\x18");
        assert_eq!(r.0, ["hook", "cancel", "unhook"], "split");
        // CAN before the final byte: no hook happened, and vte sends no
        // unhook, but the cancel still fires (handlers must cope).
        let mut r = Rec::default();
        Parser::new().advance(&mut r, b"\x1bP$\x18");
        assert_eq!(r.0, ["cancel"], "before the final byte");
        // ST and BEL end it normally: no cancel.
        for end in [&b"\x1b\\"[..], b"\x07"] {
            let mut input = b"\x1bP$qm".to_vec();
            input.extend_from_slice(end);
            let mut r = Rec::default();
            Parser::new().advance(&mut r, &input);
            assert_eq!(r.0, ["hook", "unhook"], "{end:x?}");
        }
    }

    /// WIRE-16: an ESC that doesn't start ST ends the DCS without it
    /// being acted on, and then starts its own escape.
    #[test]
    fn dcs_cut_off_by_esc_is_cancelled() {
        #[derive(Default)]
        struct Rec(Vec<String>);
        impl Perform for Rec {
            fn hook(&mut self, _: &Params, _: &[u8], _: bool, action: char) {
                self.0.push(format!("hook {action}"));
            }
            fn dcs_cancel(&mut self) {
                self.0.push("cancel".into());
            }
            fn unhook(&mut self) {
                self.0.push("unhook".into());
            }
            fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, action: char) {
                self.0.push(format!("csi {action}"));
            }
            fn esc_dispatch(&mut self, _: &[u8], _: bool, byte: u8) {
                self.0.push(format!("esc {}", byte as char));
            }
            fn apc_start(&mut self) {
                self.0.push("apc".into());
            }
            fn apc_end(&mut self) {
                self.0.push("apc end".into());
            }
            fn print(&mut self, c: char) {
                self.0.push(format!("print {c}"));
            }
        }
        let cases: &[(&[u8], &[&str])] = &[
            // ST: no cancel. (vte also reports the ST's `ESC \\`.)
            (
                b"\x1bP$qm\x1b\\x",
                &["hook q", "unhook", "esc \\", "print x"],
            ),
            // A CSI after the ESC still runs.
            (
                b"\x1bP$qm\x1b[cx",
                &["hook q", "cancel", "unhook", "csi c", "print x"],
            ),
            // ESC + an ordinary final byte.
            (
                b"\x1bP$qm\x1b7x",
                &["hook q", "cancel", "unhook", "esc 7", "print x"],
            ),
            // A new DCS, ended by ST.
            (
                b"\x1bP$qm\x1bP$qr\x1b\\",
                &["hook q", "cancel", "unhook", "hook q", "unhook", "esc \\"],
            ),
            // An APC (kitty graphics) right after.
            (
                b"\x1bP$qm\x1b_Ga=q\x1b\\x",
                &["hook q", "cancel", "unhook", "apc", "apc end", "print x"],
            ),
            // ESC ESC: the first ESC cuts the DCS off.
            (
                b"\x1bP$qm\x1b\x1b[cx",
                &["hook q", "cancel", "unhook", "csi c", "print x"],
            ),
            // Before the final byte: cancel without a hook.
            (b"\x1bP$\x1b[cx", &["cancel", "csi c", "print x"]),
        ];
        for (input, want) in cases {
            // Same events for every split point.
            for split in 0..=input.len() {
                let mut r = Rec::default();
                let mut p = Parser::new();
                p.advance(&mut r, &input[..split]);
                p.advance(&mut r, &input[split..]);
                assert_eq!(r.0, *want, "{input:x?} split at {split}");
            }
        }
    }

    /// WIRE-13: kitty 0.49.2 ends every string type on BEL. OSC and APC
    /// already did; DCS, PM and SOS now do too.
    #[test]
    fn dcs_ends_on_bel_like_st() {
        let r = parse(b"\x1bP1$rab\x07x");
        assert_eq!(r.dcs.len(), 1);
        let (params, intermediates, action, body) = &r.dcs[0];
        assert_eq!(params, &vec![vec![1u16]]);
        assert_eq!(intermediates, b"$");
        assert_eq!(*action, 'r');
        assert_eq!(body, b"ab");
        assert!(r.execs.is_empty(), "BEL ends the DCS, it isn't rung");
        assert_eq!(r.text, "x");
        // Split at every point.
        let input = b"\x1bP1$rab\x07x";
        for cut in 1..input.len() {
            let mut p = Parser::new();
            let mut r = Recorder::default();
            p.advance(&mut r, &input[..cut]);
            p.advance(&mut r, &input[cut..]);
            assert_eq!(r.dcs.len(), 1, "cut {cut}");
            assert_eq!(r.dcs[0].3, b"ab", "cut {cut}");
            assert_eq!(r.text, "x", "cut {cut}");
        }
        // BEL before the final byte: no hook, nothing printed.
        let r = parse(b"\x1bP1\x07x");
        assert!(r.dcs.is_empty());
        assert_eq!(r.text, "x");
        // A DCS that started like a tmux passthrough.
        let r = parse(b"\x1bPtm\x07x");
        assert_eq!(r.text, "x");
    }

    #[test]
    fn sixel_dcs_ends_on_bel() {
        let r = parse(b"\x1bPq#0;2;100;0;0#0~~\x07ok");
        assert_eq!(r.dcs.len(), 1);
        assert_eq!(r.dcs[0].2, 'q');
        assert_eq!(r.dcs[0].3, b"#0;2;100;0;0#0~~");
        assert_eq!(r.text, "ok");
    }

    #[test]
    fn pm_and_sos_end_on_bel_st_can_sub_and_esc() {
        // `(tail, execs, csi count)`: what follows `hello`, then the
        // expected executed controls and CSIs. The text after is always
        // `x`, whole and split at every byte.
        let cases: [(&[u8], &[u8], usize); 5] = [
            (b"\x07x", b"", 0),     // BEL ends it, isn't rung
            (b"\x1b\\x", b"", 0),   // ST
            (b"\x18x", b"\x18", 0), // CAN: executed, as in vte
            (b"\x1ax", b"\x1a", 0), // SUB
            (b"\x1b[31mx", b"", 1), // ESC + other starts a new escape
        ];
        for intro in [&b"\x1b^"[..], b"\x1bX"] {
            for (tail, execs, csi) in cases {
                let mut input = intro.to_vec();
                input.extend_from_slice(b"hello");
                input.extend_from_slice(tail);
                for cut in 0..input.len() {
                    let mut p = Parser::new();
                    let mut r = Recorder::default();
                    p.advance(&mut r, &input[..cut]);
                    p.advance(&mut r, &input[cut..]);
                    let what = format!("{intro:x?} {tail:x?} cut {cut}");
                    assert_eq!(r.text, "x", "{what}");
                    assert_eq!(r.execs, execs, "{what}");
                    assert_eq!(r.csi.len(), csi, "{what}");
                }
            }
        }
        // A PM cuts off a CSI in progress, as the ESC did before.
        let r = parse(b"\x1b[1\x1b^zz\x07x");
        assert!(r.csi.is_empty());
        assert_eq!(r.text, "x");
    }

    #[test]
    fn osc_pm_sos_eight_bit_st_is_data() {
        for body in C1_ST_BODIES {
            let mut osc = b"\x1b]2;".to_vec();
            osc.extend_from_slice(body);
            osc.extend_from_slice(b"\x1b\\after");
            let r = parse(&osc);
            assert_eq!(r.osc, vec![vec![b"2".to_vec(), body.to_vec()]]);
            assert_eq!(r.text, "after", "OSC body {body:x?}");
            for intro in [&b"\x1b^"[..], b"\x1bX"] {
                let mut s = intro.to_vec();
                s.extend_from_slice(body);
                s.extend_from_slice(b"\x1b\\after");
                let r = parse(&s);
                assert_eq!(r.text, "after", "{intro:x?} body {body:x?}");
            }
        }
    }

    #[test]
    fn apc_with_esc_in_data_not_followed_by_backslash() {
        // ESC followed by non-backslash inside APC must be treated as data.
        let r = parse(b"\x1b_abc\x1bzdef\x1b\\");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"abc\x1bzdef".to_vec());
    }

    #[test]
    fn apc_with_double_esc_then_st() {
        // First ESC data; second ESC \ is the terminator.
        let r = parse(b"\x1b_data\x1b\x1b\\");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"data\x1b".to_vec());
    }

    #[test]
    fn apc_split_across_advance_calls() {
        // Most important streaming property: chunk boundaries don't matter.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1b_part");
        p.advance(&mut r, b"_one_");
        p.advance(&mut r, b"part_two\x1b\\");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"part_one_part_two".to_vec());
    }

    #[test]
    fn apc_split_inside_terminator() {
        // ESC arrives at end of one advance; `\` at start of next.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"\x1b_data\x1b");
        p.advance(&mut r, b"\\");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"data".to_vec());
    }

    #[test]
    fn apc_split_after_introducer() {
        // ESC at end of one advance, `_` at start of next.
        let mut p = Parser::new();
        let mut r = Recorder::default();
        p.advance(&mut r, b"hi\x1b");
        p.advance(&mut r, b"_payload\x1b\\");
        assert_eq!(r.text, "hi");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"payload".to_vec());
    }

    #[test]
    fn apc_followed_by_normal_text() {
        let r = parse(b"\x1b_apc1\x1b\\after");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"apc1".to_vec());
        assert_eq!(r.text, "after");
    }

    #[test]
    fn multiple_apcs_back_to_back() {
        let r = parse(b"\x1b_first\x1b\\\x1b_second\x1b\\");
        assert_eq!(r.apc.len(), 2);
        assert_eq!(r.apc[0], b"first".to_vec());
        assert_eq!(r.apc[1], b"second".to_vec());
    }

    #[test]
    fn esc_then_non_underscore_replays_to_vte() {
        // ESC = is an esc_dispatch, must not be consumed by APC scanner.
        let r = parse(b"\x1b=");
        assert_eq!(r.esc, vec![(vec![], b'=')]);
        assert!(r.apc.is_empty());
    }

    #[test]
    fn long_apc_payload_chunked_naturally() {
        // 100 KB payload with no terminator until the end. Walks the
        // memchr fast path many times. We split into 1 KB advances
        // to stress chunk boundaries.
        let mut body = vec![b'A'; 100 * 1024];
        body[50_000] = b'B';
        let mut input = vec![0x1B, b'_'];
        input.extend_from_slice(&body);
        input.extend_from_slice(b"\x1b\\");

        let mut p = Parser::new();
        let mut r = Recorder::default();
        for chunk in input.chunks(1024) {
            p.advance(&mut r, chunk);
        }
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0].len(), body.len());
        assert_eq!(r.apc[0], body);
    }

    #[test]
    fn intermixed_csi_apc_print() {
        let r = parse(b"hi\x1b[31m\x1b_g\x1b\\there");
        assert_eq!(r.text, "hithere");
        assert_eq!(r.csi.len(), 1);
        assert_eq!(r.apc.len(), 1);
    }

    mod tmux;
}
