//! Relative placements (A4: REL-1..REL-7, SCR-8), driven end to end
//! through the parser. Expected behaviour was checked on kitty 0.49.2
//! (kitty-ref); each test names the case.

use base64::Engine;
use toastty_parser::Parser;

use super::*;

fn feed(t: &mut Term, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(t, bytes);
}

/// Transmit a 4×4 RGBA image as `id` (quietly).
fn transmit(t: &mut Term, id: u32) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![200u8; 4 * 4 * 4]);
    feed(
        t,
        format!("\x1b_Ga=t,f=32,s=4,v=4,i={id},q=2;{b64}\x1b\\").as_bytes(),
    );
}

/// Send `keys` and return the reply text.
fn cmd(t: &mut Term, keys: &str) -> String {
    t.drain_pty_replies();
    feed(t, format!("\x1b_G{keys}\x1b\\").as_bytes());
    String::from_utf8_lossy(&t.drain_pty_replies()).into_owned()
}

/// `(screen row, col)` of placement `(img, pid)`.
fn at(t: &Term, img: u32, pid: u32) -> Option<(i64, i32)> {
    let p = t.image_grid().find(img, pid)?;
    Some((p.line - t.image_top_line(), p.col))
}

/// Parent (1,1) at row 2 col 4, child (2,1) at H=12, grandchild (3,1)
/// at V=3 under the child — the layout used on kitty-ref.
fn family(t: &mut Term) {
    for id in 1..=3 {
        transmit(t, id);
    }
    feed(t, b"\x1b[3;5H");
    assert!(cmd(t, "a=p,i=1,p=1,c=10,r=5,C=1").contains("OK"));
    assert!(cmd(t, "a=p,i=2,p=1,P=1,Q=1,H=12,c=4,r=2").contains("OK"));
    assert!(cmd(t, "a=p,i=3,p=1,P=2,Q=1,V=3,c=4,r=2").contains("OK"));
    assert_eq!(at(t, 2, 1), Some((2, 16)));
    assert_eq!(at(t, 3, 1), Some((5, 16)));
}

#[test]
fn rel1_rejected_replacement_keeps_the_existing_placements() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    // Make the parent relative to its own grandchild: a cycle.
    let r = cmd(&mut t, "a=p,i=1,p=1,P=3,Q=1,c=10,r=5");
    assert!(r.contains("ECYCLE"), "{r}");
    // Re-place the child as its own parent: EINVAL (it exists).
    let r = cmd(&mut t, "a=p,i=2,p=1,P=2,Q=1,c=4,r=2");
    assert!(r.contains("EINVAL"), "{r}");
    // Nothing was removed: the whole family is still in place, and a
    // new child of the parent still finds it (kitty: OK).
    assert_eq!(at(&t, 1, 1), Some((2, 4)));
    assert_eq!(at(&t, 2, 1), Some((2, 16)));
    assert_eq!(at(&t, 3, 1), Some((5, 16)));
    transmit(&mut t, 4);
    assert!(cmd(&mut t, "a=p,i=4,p=3,P=1,Q=1,V=10,c=2,r=1").contains("OK"));
    assert_eq!(at(&t, 4, 3), Some((12, 4)));
}

#[test]
fn rel2_deleting_a_parent_deletes_the_group_and_frees_child_images() {
    // kitty 0.49.2: lowercase d=i on the parent removes children and
    // grandchildren and frees their images (no placements left); the
    // parent's own image survives a lowercase delete.
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    cmd(&mut t, "a=d,d=i,i=1");
    assert!(
        t.image_grid().is_empty(),
        "{:?}",
        t.image_grid().iter().collect::<Vec<_>>()
    );
    assert!(t.image_registry().contains(1));
    assert!(!t.image_registry().contains(2));
    assert!(!t.image_registry().contains(3));
}

#[test]
fn rel2_deleting_a_child_directly_keeps_its_image() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    cmd(&mut t, "a=d,d=i,i=2");
    // The grandchild goes with the child (and its image with it); the
    // child's image stays (kitty: a direct lowercase delete).
    assert!(t.image_grid().find(2, 1).is_none());
    assert!(t.image_grid().find(3, 1).is_none());
    assert!(t.image_registry().contains(2));
    assert!(!t.image_registry().contains(3));
    assert_eq!(at(&t, 1, 1), Some((2, 4)));
}

#[test]
fn rel2_every_removal_path_cascades() {
    // Positional deletes and ED 2 take the group with them.
    for clear in [
        &b"\x1b_Ga=d,d=p,x=5,y=3,q=2\x1b\\"[..],
        b"\x1b[2J",
        b"\x1b_Ga=d,d=A,q=2\x1b\\",
    ] {
        let mut t = Term::new(30, 80, 100);
        family(&mut t);
        // d=p hits only the parent's cell; the cascade takes the rest.
        feed(&mut t, clear);
        assert!(
            t.image_grid().is_empty(),
            "{:?}",
            String::from_utf8_lossy(clear)
        );
    }
}

#[test]
fn rel3_children_follow_a_replaced_parent() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    feed(&mut t, b"\x1b[10;1H");
    assert!(cmd(&mut t, "a=p,i=1,p=1,c=6,r=3,C=1").contains("OK"));
    assert_eq!(at(&t, 1, 1), Some((9, 0)));
    assert_eq!(at(&t, 2, 1), Some((9, 12)), "child follows (kitty-ref)");
    assert_eq!(at(&t, 3, 1), Some((12, 12)), "grandchild too");
    assert_eq!(t.image_grid().len(), 3, "replaced in place, not added");
}

#[test]
fn rel4_transmit_and_place_is_relative_and_keeps_the_cursor() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    let before = t.cursor();
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![200u8; 4 * 4 * 4]);
    feed(
        &mut t,
        format!("\x1b_Ga=T,f=32,s=4,v=4,i=5,p=1,P=1,Q=1,H=-3,V=-2,c=3,r=2;{b64}\x1b\\").as_bytes(),
    );
    assert_eq!(at(&t, 5, 1), Some((0, 1)), "kitty-ref: (row 0, col 1)");
    assert_eq!(
        (t.cursor().row, t.cursor().col),
        (before.row, before.col),
        "relative placements never move the cursor"
    );
}

#[test]
fn rel6_negative_offsets_are_not_clamped() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    assert!(cmd(&mut t, "a=p,i=2,p=5,P=1,Q=1,H=-6,V=-4,c=4,r=2").contains("OK"));
    // Two columns left of the screen and two rows above row 0 (in
    // scrollback when there is history, else hanging above the screen).
    assert_eq!(at(&t, 2, 5), Some((-2, -2)));
    let p = t.image_grid().find(2, 5).unwrap();
    assert_eq!(p.col_range(), 0..2, "the on-screen part of the box");
}

#[test]
fn rel7_error_codes_match_kitty() {
    let mut t = Term::new(30, 80, 100);
    family(&mut t);
    transmit(&mut t, 4);
    // Self-reference to a placement that doesn't exist yet.
    assert!(cmd(&mut t, "a=p,i=2,p=7,P=2,Q=7").contains("ENOPARENT"));
    // A virtual placement can't be relative.
    assert!(cmd(&mut t, "a=p,i=4,p=5,U=1,P=1,Q=1").contains("EINVAL"));
    // P without Q: relative to the parent image's placement.
    assert!(cmd(&mut t, "a=p,i=4,p=1,P=1,V=6,c=3,r=2").contains("OK"));
    assert_eq!(at(&t, 4, 1), Some((8, 4)));
    // Q without P: an ordinary placement at the cursor.
    assert!(cmd(&mut t, "a=p,i=4,p=2,Q=1,H=20,V=6,c=3,r=2,C=1").contains("OK"));
    assert_eq!(t.image_grid().find(4, 2).map(|p| p.parent), Some(None));
}

#[test]
fn scr8_child_keeps_its_offset_through_history_and_goes_with_its_parent() {
    // Parent at row 2, child V=6 below it; 10 rows, 8 lines of history.
    let mut t = Term::new(10, 40, 8);
    transmit(&mut t, 1);
    transmit(&mut t, 2);
    feed(&mut t, b"\x1b[3;5H");
    cmd(&mut t, "a=p,i=1,p=1,c=4,r=2,C=1");
    cmd(&mut t, "a=p,i=2,p=1,P=1,Q=1,V=6,c=4,r=2");
    feed(&mut t, b"\x1b[10;1H");
    // Scroll the parent into history (partly, then fully): the child
    // keeps its offset and crops like any placement.
    for n in 1..=8 {
        feed(&mut t, b"\n");
        let (pr, pc) = at(&t, 1, 1).unwrap();
        assert_eq!(at(&t, 2, 1), Some((pr + 6, pc)), "after {n} lines");
    }
    // Two more lines push the parent out of the history; kitty drops the
    // child with it (kitty-ref, small scrollback) although the child's own
    // rows are still retained.
    feed(&mut t, b"\n\n\n\n");
    assert!(at(&t, 1, 1).is_none());
    assert!(at(&t, 2, 1).is_none(), "the child goes with its parent");
}

#[test]
fn rel5_child_of_a_placeholder_image_anchors_to_its_min_x_and_y() {
    // Spec: a virtual parent's position is the minimum x and the minimum
    // y over the placeholder images that refer to it, independently.
    let mut t = Term::new(20, 40, 100);
    let b64 = base64::engine::general_purpose::STANDARD.encode(vec![200u8; 4 * 4 * 4]);
    feed(
        &mut t,
        format!("\x1b_Ga=T,f=32,s=4,v=4,i=7,p=3,U=1,c=2,r=2,q=2;{b64}\x1b\\").as_bytes(),
    );
    transmit(&mut t, 8);
    let cell = |r: char, c: char| format!("{}{r}{c}", crate::PLACEHOLDER);
    let (d0, d1) = ('\u{0305}', '\u{030D}');
    // Row 3 from column 7, row 4 from column 2: min y = 3, min x = 2.
    let s = format!(
        "\x1b[38;5;7m\x1b[58;5;3m\x1b[4;8H{}{}\x1b[5;3H{}{}\x1b[m",
        cell(d0, d0),
        cell(d0, d1),
        cell(d1, d0),
        cell(d1, d1)
    );
    feed(&mut t, s.as_bytes());
    assert!(cmd(&mut t, "a=p,i=8,p=1,P=7,Q=3,H=1,V=1,c=1,r=1").contains("OK"));
    assert_eq!(at(&t, 8, 1), Some((4, 3)));
}
