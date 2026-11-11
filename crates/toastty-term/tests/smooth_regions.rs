//! Regressions at the real parser/input boundary, not direct command mocks.

use std::time::{Duration, Instant};

use toastty_parser::Parser;
use toastty_term::Term;

const REGION: &[u8] = b"\x1b_smooth;1;1;6;1;8;10;100\x1b\\";

fn setup() -> (Term, Parser) {
    let mut term = Term::new(10, 20, 20);
    let mut parser = Parser::new();
    parser.advance(&mut term, b"\x1b[?1049h\x1b[?1000;1006h");
    parser.advance(&mut term, REGION);
    (term, parser)
}

#[test]
fn split_apc_query_declaration_and_cleanup_use_the_actual_parser() {
    let mut t = Term::new(10, 20, 0);
    let mut p = Parser::new();
    for byte in b"\x1b_smooth;?\x1b\\" {
        p.advance(&mut t, &[*byte]);
    }
    assert_eq!(t.drain_pty_replies(), b"\x1b_smooth;ok;3\x1b\\");
    p.advance(&mut t, REGION);
    assert!(
        t.smooth_regions().is_empty(),
        "primary screen must not gain hidden cells"
    );
    p.advance(&mut t, b"\x1b[?1049h");
    for byte in REGION {
        p.advance(&mut t, &[*byte]);
    }
    assert_eq!(t.smooth_regions().len(), 1);
    p.advance(&mut t, b"\x1b_smooth;off;1\x1b\\");
    assert_eq!(t.smooth_regions(), []);
    assert!(t.damage().all);
}

#[test]
fn fractional_input_does_not_mutate_text_and_requires_sgr_tracking() {
    let (mut t, mut p) = setup();
    p.advance(&mut t, b"\x1b[2;2Hunchanged");
    let before = t.row(1).cells().to_vec();
    assert!(t.scroll_smooth_region((2.0, 2.0), 0.75, Instant::now()));
    assert_eq!(t.row(1).cells(), before);
    assert_eq!(t.smooth_regions()[0].shift(), [0.0, 0.75]);
    assert_eq!(t.smooth_input_cell((2.2, 2.5)), (2.2, 3.25));
    assert_eq!(t.smooth_input_cell((15.0, 2.5)), (15.0, 2.5));
    assert_eq!(t.drain_pty_replies(), b"");
    p.advance(&mut t, b"\x1b[?1006l");
    assert!(!t.scroll_smooth_region((2.0, 2.0), 1.0, Instant::now()));
    assert_eq!(t.drain_pty_replies(), b"");
}

#[test]
fn geometry_and_screen_lifecycle_discard_stale_regions_and_requests() {
    for sequence in [
        b"\x1b[?1049l".as_slice(),
        b"\x1b[?47l".as_slice(),
        b"\x1b[!p".as_slice(),
        b"\x1bc".as_slice(),
    ] {
        let (mut t, mut p) = setup();
        t.scroll_smooth_region((2.0, 2.0), 2.0, Instant::now());
        t.drain_pty_replies();
        p.advance(&mut t, sequence);
        assert_eq!(t.smooth_regions(), []);
        assert!(t.smooth_scroll_deadline().is_none());
    }
    let (mut t, _) = setup();
    t.resize(8, 18);
    assert_eq!(t.smooth_regions(), []);
    let (mut t, _) = setup();
    t.set_cell_pixel_size(9, 20);
    assert_eq!(t.smooth_regions(), []);
}

#[test]
fn bounded_table_and_independent_regions_accept_legacy_and_extended_forms() {
    let mut t = Term::new(30, 80, 0);
    let mut p = Parser::new();
    p.advance(&mut t, b"\x1b[?1049h");
    for id in 0..9 {
        let left = id * 8;
        p.advance(
            &mut t,
            format!("\x1b_smooth;{id};1;20;{left};{};0;100\x1b\\", left + 6).as_bytes(),
        );
    }
    assert_eq!(t.smooth_regions().len(), 8);
    p.advance(&mut t, b"\x1b_smooth;off\x1b\\\x1b_smooth;1;20;0;100\x1b\\");
    assert_eq!(t.smooth_regions().len(), 1);
    assert_eq!(t.smooth_regions()[0].rect.right, 79);
    p.advance(&mut t, b"\x1b_smooth;0;1;20;0;79;0;0;500;250\x1b\\");
    assert_eq!(t.smooth_regions()[0].shift(), [0.5, 0.25]);
}

#[test]
fn delayed_acknowledgement_times_out_without_an_idle_redraw_loop() {
    let (mut t, _) = setup();
    let now = Instant::now();
    t.scroll_smooth_region((2.0, 2.0), 2.0, now);
    assert_eq!(
        t.smooth_scroll_deadline(),
        Some(now + Duration::from_millis(500))
    );
    t.clear_damage();
    t.expire_smooth_scroll(now + Duration::from_millis(501));
    assert!(t.damage().all);
    assert_eq!(t.smooth_regions()[0].shift(), [0.0, 0.0]);
    assert!(t.smooth_scroll_deadline().is_none());
}

#[test]
fn leaving_alt_screen_in_the_ack_batch_drops_unsent_wheel_requests() {
    let (mut t, mut p) = setup();
    t.scroll_smooth_region((2.0, 2.0), 3.0, Instant::now());
    assert_eq!(t.drain_pty_replies(), b"\x1b[<65;3;3M");
    p.advance(&mut t, b"\x1b_smooth;1;1;6;1;8;11;100\x1b\\\x1b[?1049l");
    assert!(
        t.drain_pty_replies().is_empty(),
        "queued scroll must not leak to the shell"
    );
}
