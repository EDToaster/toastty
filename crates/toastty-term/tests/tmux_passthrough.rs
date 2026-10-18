//! tmux passthrough (`ESC P tmux ; … ESC \`) end to end: wrapped
//! sequences must leave `Term` in the same state as the bare ones, and
//! bytes captured from a real tmux 3.6 (see
//! `fixtures/capture-tmux-passthrough.sh`) must parse.

use toastty_parser::Parser;
use toastty_term::{ClipboardRequest, Cursor, SecurityFlags, Term};

const ROWS: u16 = 6;
const COLS: u16 = 24;

/// What tmux-aware apps send: every ESC in the payload doubled.
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

fn new_term() -> Term {
    let mut t = Term::new(ROWS, COLS, 16);
    t.set_security(SecurityFlags {
        osc_52_read: true,
        osc_52_write: true,
    });
    t
}

/// Observable state that the passthrough tests compare.
#[derive(Debug, PartialEq)]
struct Snapshot {
    cells: Vec<Vec<(toastty_term::Cell, Option<String>)>>,
    cursor: Cursor,
    title: String,
    clipboard: Vec<ClipboardRequest>,
    replies: Vec<u8>,
    images: usize,
}

fn snapshot(t: &mut Term) -> Snapshot {
    let cells = (0..ROWS)
        .map(|r| {
            t.row(r)
                .cells
                .iter()
                .map(|c| {
                    (
                        *c,
                        c.hyperlink_id
                            .and_then(|id| t.hyperlink_url(id))
                            .map(str::to_owned),
                    )
                })
                .collect()
        })
        .collect();
    Snapshot {
        cells,
        cursor: t.cursor(),
        title: t.title().to_owned(),
        clipboard: t.drain_clipboard_requests(),
        replies: t.drain_pty_replies(),
        images: t.image_registry().len(),
    }
}

fn run_chunks(chunks: &[&[u8]]) -> Snapshot {
    let mut t = new_term();
    let mut p = Parser::new();
    for c in chunks {
        p.advance(&mut t, c);
    }
    snapshot(&mut t)
}

fn run(bytes: &[u8]) -> Snapshot {
    run_chunks(&[bytes])
}

fn run_bytewise(bytes: &[u8]) -> Snapshot {
    let chunks: Vec<&[u8]> = bytes.chunks(1).collect();
    run_chunks(&chunks)
}

fn row_text(s: &Snapshot, r: usize) -> String {
    let text: String = s.cells[r].iter().map(|(c, _)| c.ch).collect();
    text.trim_end().to_owned()
}

/// Wrapped and bare must agree, whole, byte-by-byte and at every
/// two-way split of the wrapped bytes.
fn assert_wrapped_matches_direct(inner: &[u8]) {
    let direct = [b"a".as_slice(), inner, b"z"].concat();
    let wrapped = [b"a".as_slice(), &wrap(inner), b"z"].concat();
    let want = run(&direct);
    assert_eq!(run(&wrapped), want, "whole: {inner:?}");
    assert_eq!(run_bytewise(&wrapped), want, "bytewise: {inner:?}");
    for i in 0..=wrapped.len() {
        assert_eq!(
            run_chunks(&[&wrapped[..i], &wrapped[i..]]),
            want,
            "split at {i}: {inner:?}"
        );
    }
}

fn b64_red_1x1() -> &'static [u8] {
    // RGBA ff 00 00 ff.
    b"/wAA/w=="
}

#[test]
fn osc52_clipboard_write_through_passthrough() {
    let inner = b"\x1b]52;c;aGVsbG8=\x07";
    assert_wrapped_matches_direct(inner);
    let s = run(&wrap(inner));
    assert_eq!(s.clipboard.len(), 1);
    assert!(
        matches!(&s.clipboard[0], ClipboardRequest::Set { data, .. } if data == b"hello"),
        "{:?}",
        s.clipboard
    );
}

#[test]
fn osc52_query_reply_through_passthrough() {
    let inner = b"\x1b]52;c;?\x1b\\";
    assert_wrapped_matches_direct(inner);
    let s = run(&wrap(inner));
    assert!(matches!(s.clipboard[..], [ClipboardRequest::Query { .. }]));
}

#[test]
fn osc8_hyperlink_through_passthrough() {
    let inner = b"\x1b]8;;https://example.com\x1b\\link\x1b]8;;\x1b\\";
    assert_wrapped_matches_direct(inner);
    let s = run(&wrap(inner));
    assert_eq!(row_text(&s, 0), "link");
    for col in 0..4 {
        assert_eq!(s.cells[0][col].1.as_deref(), Some("https://example.com"));
    }
    assert_eq!(s.cells[0][4].1, None);
}

#[test]
fn kitty_graphics_apc_through_passthrough() {
    let inner = [
        b"\x1b_Ga=t,f=32,s=1,v=1,i=42;".as_slice(),
        b64_red_1x1(),
        b"\x1b\\",
    ]
    .concat();
    assert_wrapped_matches_direct(&inner);
    let mut t = new_term();
    Parser::new().advance(&mut t, &wrap(&inner));
    assert!(t.image_registry().contains(42));
    let replies = String::from_utf8_lossy(&t.drain_pty_replies()).into_owned();
    assert!(replies.contains(";OK"), "{replies:?}");
}

#[test]
fn kitty_chunked_upload_split_across_passthroughs() {
    // tmux-aware kitty clients wrap each `m=1` chunk separately.
    let b64 = b64_red_1x1();
    let first = [
        b"\x1b_Ga=t,f=32,s=1,v=1,i=7,m=1;".as_slice(),
        &b64[..4],
        b"\x1b\\",
    ]
    .concat();
    let second = [b"\x1b_Gm=0;".as_slice(), &b64[4..], b"\x1b\\"].concat();
    let mut t = new_term();
    let mut p = Parser::new();
    p.advance(&mut t, &wrap(&first));
    p.advance(&mut t, &wrap(&second));
    assert!(t.image_registry().contains(7));
}

#[test]
fn csi_title_and_dcs_through_passthrough() {
    for inner in [
        &b"\x1b[1;31mred\x1b[0m"[..],
        b"\x1b[3;5Hx",
        b"\x1b]2;wrapped title\x1b\\",
        b"\x1bP$qm\x1b\\",
        b"\x1bP+q544e\x1b\\",
        b"\x1b[c",
    ] {
        assert_wrapped_matches_direct(inner);
    }
    let s = run(&wrap(b"\x1b]2;wrapped title\x07"));
    assert_eq!(s.title, "wrapped title");
    // DECRQSS reply still comes back when the query was wrapped.
    let s = run(&wrap(b"\x1bP$qm\x1b\\"));
    assert!(s.replies.starts_with(b"\x1bP1$r"), "{:?}", s.replies);
}

#[test]
fn rich_block_osc_through_passthrough() {
    // Unknown / private OSC numbers (e.g. 7700 rich blocks) must reach
    // the OSC dispatcher identically whether wrapped or not.
    assert_wrapped_matches_direct(b"\x1b]7700;begin;e30=\x1b\\");
}

#[test]
fn unterminated_inner_osc_behaves_as_if_sent_bare() {
    let wrapped = [wrap(b"\x1b]2;par"), b"tial\x07ok".to_vec()].concat();
    let direct = b"\x1b]2;partial\x07ok";
    let s = run(&wrapped);
    assert_eq!(s, run(direct));
    assert_eq!(s.title, "partial");
    assert_eq!(row_text(&s, 0), "ok");
}

#[test]
fn plain_tmux_text_prints() {
    let s = run(b"tmux; Ptmux; \x1bPtmux");
    // `ESC P tmux` without `;` is an ordinary DCS (final `t`), so only
    // the plain text prints.
    assert_eq!(row_text(&s, 0), "tmux; Ptmux;");
}

// ----- bytes captured from a real tmux 3.6 ---------------------------

/// What the app wrote into the tmux pane (wrapped passthroughs).
const TMUX_PANE_INPUT: &[u8] = include_bytes!("fixtures/tmux-pane-input.bin");
/// What tmux wrote to its outer terminal (passthroughs unwrapped).
const TMUX_OUTER_OUTPUT: &[u8] = include_bytes!("fixtures/tmux-outer-output.bin");

fn assert_fixture_effects(t: &mut Term) {
    assert_eq!(t.title(), "wrapped title");
    assert!(t.image_registry().contains(42));
    let clip = t.drain_clipboard_requests();
    assert!(
        matches!(&clip[..], [ClipboardRequest::Set { data, .. }] if data == b"hello"),
        "{clip:?}"
    );
}

#[test]
fn real_tmux_pane_input_unwraps() {
    // The wrapped form, as toastty sees it when an app that believes
    // it is inside tmux talks to toastty directly.
    let wrapped_count = TMUX_PANE_INPUT
        .windows(7)
        .filter(|w| w == b"\x1bPtmux;")
        .count();
    assert_eq!(wrapped_count, 4);
    for chunking in [usize::MAX, 1, 3, 7, 64] {
        let mut t = new_term();
        let mut p = Parser::new();
        for chunk in TMUX_PANE_INPUT.chunks(chunking.min(TMUX_PANE_INPUT.len())) {
            p.advance(&mut t, chunk);
        }
        assert_fixture_effects(&mut t);
        let s = snapshot(&mut t);
        // `^@` is literal text in the capture; "link" was printed
        // inside the OSC 8 passthrough, "AFTER" outside.
        assert_eq!(row_text(&s, 0), "^@linkAFTER", "chunking {chunking}");
        for col in 2..6 {
            assert_eq!(s.cells[0][col].1.as_deref(), Some("https://example.com"));
        }
        assert_eq!(s.cells[0][6].1, None);
    }
}

#[test]
fn real_tmux_outer_output_parses() {
    // tmux with `allow-passthrough on` strips the wrapper before
    // forwarding, so this is the path toastty takes when it hosts tmux.
    assert!(!TMUX_OUTER_OUTPUT.windows(7).any(|w| w == b"\x1bPtmux;"));
    let marker = TMUX_OUTER_OUTPUT
        .windows(5)
        .position(|w| w == b"AFTER")
        .expect("fixture contains AFTER")
        + 5;
    let mut t = Term::new(24, 80, 16);
    t.set_security(SecurityFlags {
        osc_52_read: false,
        osc_52_write: true,
    });
    let mut p = Parser::new();
    for chunk in TMUX_OUTER_OUTPUT[..marker].chunks(5) {
        p.advance(&mut t, chunk);
    }
    assert_fixture_effects(&mut t);
    // tmux forwarded "link" (inside its OSC 8) at the outer cursor,
    // which its preceding DECSTBM had homed to column 0, then drew the
    // pane's "AFTER" at column 2 over the tail of it.
    let row: String = t.row(0).cells.iter().map(|c| c.ch).collect();
    assert_eq!(row.trim_end(), "liAFTER");
    let url = |col: usize| {
        t.row(0).cells[col]
            .hyperlink_id
            .and_then(|id| t.hyperlink_url(id))
            .map(str::to_owned)
    };
    assert_eq!(url(0).as_deref(), Some("https://example.com"));
    assert_eq!(url(1).as_deref(), Some("https://example.com"));
    assert_eq!(url(2), None);
    p.advance(&mut t, &TMUX_OUTER_OUTPUT[marker..]);
}
