//! tmux passthrough (`ESC P tmux ; … ESC \`) unwrapping.

use super::*;
use crate::perform::VteAdapter;
use proptest::prelude::*;

/// Wrap `inner` the way tmux-aware apps do: every ESC doubled.
fn wrap(inner: &[u8]) -> Vec<u8> {
    let mut out = b"\x1bPtmux;".to_vec();
    for &b in inner {
        if b == 0x1B {
            out.push(0x1B);
        }
        out.push(b);
    }
    out.extend_from_slice(b"\x1b\\");
    out
}

fn concat(parts: &[&[u8]]) -> Vec<u8> {
    parts.concat()
}

fn parse_chunks(p: &mut Parser, chunks: &[&[u8]]) -> Recorder {
    let mut r = Recorder::default();
    for c in chunks {
        p.advance(&mut r, c);
    }
    r
}

fn parse_bytewise(bytes: &[u8]) -> Recorder {
    let mut p = Parser::new();
    let mut r = Recorder::default();
    for b in bytes {
        p.advance(&mut r, std::slice::from_ref(b));
    }
    r
}

/// Reference: raw vte with no pre-scanner (no APC, no passthrough).
fn vte_only(bytes: &[u8]) -> Recorder {
    let mut v = vte::Parser::new();
    let mut r = Recorder::default();
    v.advance(&mut VteAdapter { inner: &mut r }, bytes);
    r
}

/// Assert `wrapped` parses like `direct`, whole, byte-by-byte and at
/// every two-way split point.
fn assert_same_as_direct(wrapped: &[u8], direct: &[u8]) {
    let want = parse(direct);
    assert_eq!(parse(wrapped), want, "whole input");
    assert_eq!(parse_bytewise(wrapped), want, "byte-by-byte");
    for i in 0..=wrapped.len() {
        let got = parse_chunks(&mut Parser::new(), &[&wrapped[..i], &wrapped[i..]]);
        assert_eq!(got, want, "split at {i}");
    }
}

/// A sample mixing every kind of inner sequence with plain text.
fn sample_inner_sequences() -> Vec<&'static [u8]> {
    vec![
        b"\x1b]52;c;aGVsbG8=\x07",
        b"\x1b]52;c;aGVsbG8=\x1b\\",
        b"\x1b]8;;https://example.com\x1b\\",
        b"\x1b]8;;\x1b\\",
        b"\x1b_Ga=T,f=32,s=1,v=1;/wAA/w==\x1b\\",
        b"\x1b[1;31m",
        b"\x1b[?2026h",
        b"\x1b]7700;rich;e30=\x1b\\",
        b"\x1bP$qm\x1b\\",
        b"\x1b=",
    ]
}

#[test]
fn osc52_inside_passthrough_dispatches() {
    let r = parse(&wrap(b"\x1b]52;c;aGVsbG8=\x07"));
    assert_eq!(
        r.osc,
        vec![vec![b"52".to_vec(), b"c".to_vec(), b"aGVsbG8=".to_vec()]]
    );
    assert!(r.dcs.is_empty() && r.dcs_active.is_none());
    assert!(r.text.is_empty() && r.execs.is_empty() && r.esc.is_empty());
}

#[test]
fn each_inner_sequence_matches_direct_at_every_split() {
    for inner in sample_inner_sequences() {
        let wrapped = concat(&[b"a", &wrap(inner), b"z"]);
        let direct = concat(&[b"a", inner, b"z"]);
        assert_same_as_direct(&wrapped, &direct);
    }
}

#[test]
fn mixed_stream_matches_direct_at_every_split() {
    let mut wrapped = b"pre ".to_vec();
    let mut direct = b"pre ".to_vec();
    for inner in sample_inner_sequences() {
        wrapped.extend_from_slice(&wrap(inner));
        wrapped.extend_from_slice(b"|");
        direct.extend_from_slice(inner);
        direct.extend_from_slice(b"|");
    }
    // Several sequences in one passthrough, too.
    let all: Vec<u8> = sample_inner_sequences().concat();
    wrapped.extend_from_slice(&wrap(&all));
    direct.extend_from_slice(&all);
    wrapped.extend_from_slice(b" post");
    direct.extend_from_slice(b" post");
    assert_same_as_direct(&wrapped, &direct);
    let r = parse(&wrapped);
    assert_eq!(r.apc.len(), 2);
    assert_eq!(r.dcs.len(), 2);
    assert_eq!(r.text, "pre |||||||||| post");
}

#[test]
fn doubled_esc_unwraps_to_single_esc() {
    // ESC ESC ESC \ : a literal ESC as the last payload byte, then ST.
    let mut p = Parser::new();
    let mut r = Recorder::default();
    p.advance(&mut r, b"\x1bPtmux;\x1b\x1b_abc\x1b\x1b\x1b\x1b\x1b\\");
    // The inner bytes are `ESC _ abc ESC ESC`: an APC whose payload
    // so far is `abc ESC`, with the second ESC possibly starting ST.
    assert_eq!(r.apc.len(), 0);
    assert_eq!(r.apc_pending, b"abc\x1b".to_vec());
    // ...which a bare `\` then finishes, exactly as if sent directly.
    p.advance(&mut r, b"\\x");
    assert_eq!(r.apc, vec![b"abc\x1b".to_vec()]);
    assert_eq!(r.text, "x");
}

#[test]
fn doubled_esc_split_between_the_pair() {
    let r = parse_chunks(
        &mut Parser::new(),
        &[b"\x1bPtmux;\x1b", b"\x1b]2;t\x1b", b"\x1b\\\x1b", b"\\ok"],
    );
    assert_eq!(r.osc, vec![vec![b"2".to_vec(), b"t".to_vec()]]);
    assert_eq!(r.text, "ok");
    // vte ends the OSC on the ESC and sees the backslash as an ESC dispatch,
    // exactly as for the bare bytes; the outer ST adds nothing.
    assert_eq!(r, parse(b"\x1b]2;t\x1b\\ok"));
}

#[test]
fn stray_esc_inside_payload_keeps_the_next_byte() {
    // tmux's `dcs_escape` state drops the ESC and keeps the byte, so a
    // non-doubled `ESC ]` arrives as a bare `]`.
    let r = parse(b"\x1bPtmux;a\x1bb\x1b\\c");
    assert_eq!(r.text, "abc");
    assert!(r.osc.is_empty() && r.esc.is_empty());
}

#[test]
fn ordinary_dcs_is_unaffected() {
    let cases: &[&[u8]] = &[
        // DECRQSS
        b"\x1bP$qm\x1b\\",
        b"\x1bP$q r\x1b\\",
        // XTGETTCAP
        b"\x1bP+q544e;436f\x1b\\",
        // sixel
        b"\x1bP0;1;0q\"1;1;2;2#0;2;100;0;0#0!2~-!2~\x1b\\",
        // Prefixes of `tmux;` that diverge.
        b"\x1bPt\x1b\\",
        b"\x1bPtm\x1b\\",
        b"\x1bPtmu\x1b\\",
        b"\x1bPtmux\x1b\\",
        b"\x1bPtmuxq\x1b\\",
        b"\x1bPtmx;q\x1b\\",
        b"\x1bPTMUX;q\x1b\\",
        // `ESC P` immediately followed by another escape.
        b"\x1bP\x1b[31mX",
        b"\x1bPtm\x1b[31mX",
        // CAN / SUB right after the introducer prefix.
        b"\x1bPtm\x18X",
        b"\x1bPtmu\x1aX",
        // A DCS whose body happens to contain `tmux;`.
        b"\x1bP$qtmux;\x1b\\",
    ];
    for case in cases {
        let want = vte_only(case);
        assert_eq!(parse(case), want, "case {case:?}");
        assert_eq!(parse_bytewise(case), want, "bytewise {case:?}");
        for i in 0..=case.len() {
            let got = parse_chunks(&mut Parser::new(), &[&case[..i], &case[i..]]);
            assert_eq!(got, want, "case {case:?} split at {i}");
        }
    }
    let r = parse(b"\x1bP$qm\x1b\\");
    assert_eq!(r.dcs.len(), 1);
    assert_eq!(r.dcs[0].2, 'q');
    assert_eq!(r.dcs[0].1, b"$".to_vec());
    assert_eq!(r.dcs[0].3, b"m".to_vec());
}

#[test]
fn plain_text_mentioning_tmux_is_unaffected() {
    for text in [
        &b"tmux;"[..],
        b"Ptmux; hello",
        b"run tmux; then ESC",
        b"\x1b[1mtmux;\x1b[0m",
    ] {
        assert_eq!(parse(text), vte_only(text), "{text:?}");
    }
    assert_eq!(parse(b"Ptmux;ok").text, "Ptmux;ok");
}

#[test]
fn oversized_passthrough_is_dropped_and_resyncs() {
    // Cap 8: a 9-byte payload is dropped whole, including the part
    // that fit; nothing of it prints and the next text does.
    let mut p = Parser::with_max_passthrough_len(8);
    let r = parse_chunks(&mut p, &[b"\x1bPtmux;\x1b\x1b]2;", b"abcd", b"e\x1b\\ok"]);
    assert!(r.osc.is_empty(), "{:?}", r.osc);
    assert_eq!(r.text, "ok");
    assert!(r.execs.is_empty() && r.esc.is_empty());
    assert_eq!(p.passthrough.len(), 0);
    // The parser is back in ground: the next passthrough works.
    let r = parse_chunks(&mut p, &[&wrap(b"\x1b]2;t\x07"), b"z"]);
    assert_eq!(r.osc, vec![vec![b"2".to_vec(), b"t".to_vec()]]);
    assert_eq!(r.text, "z");

    // Exactly at the cap (doubled ESCs count once) still dispatches.
    let inner = b"\x1b]2;abc\x07";
    assert_eq!(inner.len(), 8);
    let r = parse_chunks(&mut Parser::with_max_passthrough_len(8), &[&wrap(inner)]);
    assert_eq!(r.osc.len(), 1);

    // Doubled ESCs while discarding are skipped in pairs: `ESC ESC \`
    // is not the terminator, the following `ESC \` is.
    let mut p = Parser::with_max_passthrough_len(4);
    let r = parse_chunks(
        &mut p,
        &[
            b"\x1bPtmux;0123456789",
            b"\x1b\x1b\\still dropped",
            b"\x1b\\ok",
        ],
    );
    assert_eq!(r.text, "ok");
    assert_eq!(r.esc.len(), 0);

    // An endless payload stays bounded across many chunks.
    let mut p = Parser::with_max_passthrough_len(64);
    let mut r = Recorder::default();
    p.advance(&mut r, b"\x1bPtmux;");
    for _ in 0..1000 {
        p.advance(&mut r, b"0123456789abcdef");
        assert!(p.passthrough.len() <= 64);
    }
    p.advance(&mut r, b"\x1b\\done");
    assert_eq!(r.text, "done");
}

#[test]
fn can_and_sub_cancel_a_passthrough() {
    for c in [0x18u8, 0x1A] {
        let input = concat(&[b"\x1bPtmux;\x1b\x1b]2;t", &[c], b"x\x1b\\"]);
        let r = parse(&input);
        assert!(r.osc.is_empty(), "{c:#x}");
        // The control reaches vte like any other C0, and the stray
        // `ESC \` after it is a plain ESC dispatch.
        assert_eq!(r.text, "x");
        assert_eq!(r.esc, vec![(vec![], b'\\')]);
        // Same while discarding an oversized one.
        let r = parse_chunks(
            &mut Parser::with_max_passthrough_len(2),
            &[b"\x1bPtmux;abcdef", &[c], b"y"],
        );
        assert_eq!(r.text, "y", "{c:#x}");
    }
}

#[test]
fn unterminated_inner_osc_continues_after_passthrough() {
    // The payload opens an OSC and never closes it; the bytes after the
    // passthrough continue it, just as if the inner bytes were bare.
    let wrapped = concat(&[&wrap(b"\x1b]2;ab"), b"cd\x07ef"]);
    let direct = b"\x1b]2;abcd\x07ef";
    assert_same_as_direct(&wrapped, direct);
    let r = parse(&wrapped);
    assert_eq!(r.osc, vec![vec![b"2".to_vec(), b"abcd".to_vec()]]);
    assert_eq!(r.text, "ef");
}

#[test]
fn unterminated_inner_apc_and_csi_continue_after_passthrough() {
    let cases: [(&[u8], &[u8]); 4] = [
        (b"\x1b_Gi=1", b";AAAA\x1b\\t"),
        (b"\x1b[1;", b"31mt"),
        (b"\x1bP$q", b"m\x1b\\t"),
        // Inner payload ending in a lone ESC: the outer `[` completes
        // it into a CSI.
        (b"\x1b", b"[2Jt"),
    ];
    for (inner, after) in cases {
        let wrapped = concat(&[&wrap(inner), after]);
        let direct = concat(&[inner, after]);
        assert_same_as_direct(&wrapped, &direct);
    }
}

#[test]
fn passthrough_start_terminates_a_pending_osc() {
    // Any ESC ends an OSC in vte; the passthrough introducer is no
    // exception, and the wrapped OSC still dispatches afterwards.
    let r = parse(&concat(&[b"\x1b]2;old", &wrap(b"\x1b]2;new\x07")]));
    assert_eq!(
        r.osc,
        vec![
            vec![b"2".to_vec(), b"old".to_vec()],
            vec![b"2".to_vec(), b"new".to_vec()],
        ]
    );
    assert!(r.text.is_empty() && r.esc.is_empty() && r.execs.is_empty());
    // Same for a CSI cut short by the introducer.
    let r = parse(&concat(&[b"\x1b[3", &wrap(b"\x1b[4m")]));
    assert_eq!(r.csi.len(), 1);
    assert_eq!(r.csi[0].0, vec![vec![4u16]]);
}

#[test]
fn passthrough_inside_apc_is_apc_data() {
    let r = parse(b"\x1b_G\x1bPtmux;x\x1b\\");
    assert_eq!(r.apc, vec![b"G\x1bPtmux;x".to_vec()]);
}

#[test]
fn nested_passthrough_is_not_unwrapped() {
    // Inner `ESC P tmux ;` is just a DCS with final `t` to vte.
    let inner = b"\x1bPtmux;\x1b\\";
    let wrapped = wrap(inner);
    let r = parse(&wrapped);
    assert_eq!(r, vte_only(inner));
    assert_eq!(r.dcs.len(), 1);
    assert_eq!(r.dcs[0].2, 't');
}

#[test]
fn back_to_back_and_empty_passthroughs() {
    let wrapped = concat(&[&wrap(b""), &wrap(b"\x1b[1m"), &wrap(b"\x1b[2m"), b"x"]);
    assert_same_as_direct(&wrapped, b"\x1b[1m\x1b[2mx");
}

#[test]
fn large_passthrough_buffer_is_released() {
    let body = vec![b'A'; 200 * 1024];
    let inner = concat(&[b"\x1b_G", &body, b"\x1b\\"]);
    let wrapped = wrap(&inner);
    let mut p = Parser::new();
    let mut r = Recorder::default();
    for chunk in wrapped.chunks(4096) {
        p.advance(&mut r, chunk);
    }
    assert_eq!(r.apc.len(), 1);
    assert_eq!(r.apc[0].len(), body.len() + 1);
    assert!(p.passthrough.capacity() <= PASSTHROUGH_KEEP_CAPACITY);
    assert_eq!(p.tmux, TmuxState::Off);
}

/// Inner-payload bytes: everything that matters to the state machines
/// except CAN / SUB (which cancel a passthrough, so the wrapped and
/// bare forms legitimately differ) and `x` (so the inner bytes can't
/// spell `tmux;` and start a passthrough of their own when bare).
fn inner_byte() -> impl Strategy<Value = u8> {
    prop_oneof![
        Just(0x1Bu8),
        Just(b'\\'),
        Just(b']'),
        Just(b'_'),
        Just(b'['),
        Just(b'P'),
        Just(b';'),
        Just(b't'),
        Just(b'm'),
        Just(b'u'),
        Just(b'q'),
        Just(b'$'),
        Just(0x07),
        Just(0x9C),
        Just(b'\n'),
        b'0'..=b'9',
        b'a'..=b'f',
    ]
}

proptest! {
    #[test]
    fn wrapped_matches_bare(
        inner in proptest::collection::vec(inner_byte(), 0..48),
        after in proptest::collection::vec(inner_byte(), 0..16),
        split in any::<prop::sample::Index>(),
    ) {
        let wrapped = concat(&[b"ab", &wrap(&inner), &after]);
        let direct = concat(&[b"ab", &inner, &after]);
        let want = parse(&direct);
        prop_assert_eq!(&parse(&wrapped), &want);
        let i = split.index(wrapped.len() + 1);
        let got = parse_chunks(&mut Parser::new(), &[&wrapped[..i], &wrapped[i..]]);
        prop_assert_eq!(&got, &want);
        prop_assert_eq!(&parse_bytewise(&wrapped), &want);
    }

    #[test]
    fn chunking_never_matters(
        bytes in proptest::collection::vec(
            prop_oneof![inner_byte(), Just(b'x'), Just(0x18u8), Just(0x1Au8)],
            0..96,
        ),
        split in any::<prop::sample::Index>(),
    ) {
        // Arbitrary streams, including stray / partial / nested
        // introducers: chunk boundaries never change the result.
        let mut input = b"\x1bPtmux;".to_vec();
        input.extend_from_slice(&bytes);
        let want = parse(&input);
        let i = split.index(input.len() + 1);
        let got = parse_chunks(&mut Parser::new(), &[&input[..i], &input[i..]]);
        prop_assert_eq!(&got, &want);
        prop_assert_eq!(&parse_bytewise(&input), &want);
    }
}
