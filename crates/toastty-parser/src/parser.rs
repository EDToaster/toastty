use crate::perform::{Perform, VteAdapter};

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
    Apc,
    /// Inside APC and the last byte was 0x1B; if next is `\` we end
    /// the APC, otherwise the ESC was data.
    ApcAfterEsc,
}

/// Top-level parser. Pre-scans the byte stream for APC sequences
/// (which vte 0.15 drops) and forwards everything else to vte.
#[derive(Default)]
pub struct Parser {
    vte: vte::Parser,
    apc: ApcState,
}

impl std::fmt::Debug for Parser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Parser")
            .field("apc", &self.apc)
            .finish_non_exhaustive()
    }
}

impl Parser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed bytes through the parser. Calls handler methods on
    /// `perform` as events are recognized.
    pub fn advance<P: Perform + ?Sized>(&mut self, perform: &mut P, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            match self.apc {
                ApcState::Ground => {
                    // Scan for next ESC. Everything before is normal vte input.
                    if let Some(pos) = memchr::memchr(0x1B, bytes) {
                        if pos > 0 {
                            self.vte
                                .advance(&mut VteAdapter { inner: perform }, &bytes[..pos]);
                        }
                        // Don't feed the ESC to vte yet — peek the next byte.
                        self.apc = ApcState::AfterEsc;
                        bytes = &bytes[pos + 1..];
                    } else {
                        self.vte.advance(&mut VteAdapter { inner: perform }, bytes);
                        return;
                    }
                }
                ApcState::AfterEsc => {
                    let byte = bytes[0];
                    bytes = &bytes[1..];
                    if byte == b'_' {
                        perform.apc_start();
                        self.apc = ApcState::Apc;
                    } else {
                        // Replay ESC + this byte to vte.
                        self.vte
                            .advance(&mut VteAdapter { inner: perform }, &[0x1B, byte]);
                        self.apc = ApcState::Ground;
                    }
                }
                ApcState::Apc => {
                    // Scan for terminators: ESC, BEL (0x07), or 8-bit ST (0x9C).
                    if let Some(pos) = memchr::memchr3(0x1B, 0x07, 0x9C, bytes) {
                        if pos > 0 {
                            perform.apc_chunk(&bytes[..pos]);
                        }
                        let term = bytes[pos];
                        bytes = &bytes[pos + 1..];
                        match term {
                            0x1B => self.apc = ApcState::ApcAfterEsc,
                            0x07 | 0x9C => {
                                perform.apc_end();
                                self.apc = ApcState::Ground;
                            }
                            _ => unreachable!(),
                        }
                    } else {
                        perform.apc_chunk(bytes);
                        return;
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Params;

    type ParamVec = Vec<Vec<u16>>;
    type DcsRecord = (ParamVec, Vec<u8>, char, Vec<u8>);

    #[derive(Default, Debug)]
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

    #[test]
    fn apc_eight_bit_st_terminator() {
        let r = parse(b"\x1b_payload\x9c");
        assert_eq!(r.apc.len(), 1);
        assert_eq!(r.apc[0], b"payload".to_vec());
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
}
