//! Round-trip tests for kitty Unicode placeholders: bytes in, resolved
//! runs and virtual placements out. The fit geometry itself is tested in
//! `toastty-graphics` (`placeholder_slice`).

use base64::Engine;
use toastty_parser::Parser;

use super::*;

/// First kitty placeholder diacritics (index → codepoint).
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A,
];

fn feed(t: &mut Term, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(t, bytes);
}

/// `U+10EEEE` followed by the given diacritic indices (row, col, msb).
fn cell(diacritics: &[usize]) -> String {
    let mut s = String::from(crate::PLACEHOLDER);
    for &i in diacritics {
        s.push(char::from_u32(DIACRITICS[i]).unwrap());
    }
    s
}

/// `rows × cols` placeholder block, full diacritics on every cell, one
/// line per row starting at column 0 of the cursor's row.
fn block(rows: usize, cols: usize) -> String {
    let mut s = String::new();
    for r in 0..rows {
        for c in 0..cols {
            s.push_str(&cell(&[r, c]));
        }
        s.push_str("\r\n");
    }
    s
}

/// Transmit a `w`×`h` RGBA image as `id`, with extra control keys.
fn transmit(t: &mut Term, keys: &str, w: u32, h: u32) {
    let raw = vec![200u8; (w * h * 4) as usize];
    let b64 = base64::engine::general_purpose::STANDARD.encode(&raw);
    let mut p = format!("\x1b_G{keys},f=32,s={w},v={h},q=2;").into_bytes();
    p.extend_from_slice(b64.as_bytes());
    p.extend_from_slice(b"\x1b\\");
    feed(t, &p);
}

/// Runs of the live grid's row `r`.
fn runs(t: &Term, r: u16) -> Vec<PlaceholderRun> {
    let mut out = Vec::new();
    t.placeholder_runs(t.active_grid().row(r), &mut out);
    out
}

/// Runs of rendered viewport row `r` (scrollback-aware).
fn view_runs(t: &Term, r: u16) -> Vec<PlaceholderRun> {
    let mut out = Vec::new();
    t.placeholder_runs(t.view_row(r), &mut out);
    out
}

fn run(col: u16, len: u16, image_id: u32, img_row: u32, img_col: u32) -> PlaceholderRun {
    PlaceholderRun {
        col,
        len,
        image_id,
        placement_id: 0,
        img_row,
        img_col,
    }
}

// ---- cell storage + run resolution ---------------------------------

#[test]
fn full_block_resolves_one_run_per_row() {
    let mut t = Term::new(6, 10, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(2, 3).as_bytes());
    assert_eq!(runs(&t, 0), vec![run(0, 3, 42, 0, 0)]);
    assert_eq!(runs(&t, 1), vec![run(0, 3, 42, 1, 0)]);
    // The cells are plain text: no ImageGrid tiles any more.
    assert!(t.image_grid().is_empty());
}

#[test]
fn diacritics_and_placement_id_are_stored_in_the_cell() {
    let mut t = Term::new(2, 6, 0);
    feed(&mut t, b"\x1b[38;5;7m\x1b[58;5;3m");
    feed(&mut t, cell(&[2, 4, 1]).as_bytes());
    let c = t.active_grid().row(0).cells[0];
    assert_eq!(c.ch, crate::PLACEHOLDER);
    assert_eq!(
        t.cell_extra(&c),
        Some(&CellExtra::Placeholder(PlaceholderExtra {
            placement_id: 3,
            row: 3,
            col: 5,
            id_msb: 2,
        }))
    );
    // Diacritics are zero-width: the cursor advanced one cell only.
    assert_eq!(t.cursor().col, 1);
    let r = runs(&t, 0);
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].image_id, (1 << 24) | 7, "3rd diacritic = id MSB");
    assert_eq!(r[0].placement_id, 3, "underline colour = placement id");
    assert_eq!((r[0].img_row, r[0].img_col), (2, 4));
}

#[test]
fn row_only_first_cell_then_bare_cells_inherit() {
    // The spec's "2 rows by 3 columns" example: only the first column
    // carries a (row) diacritic.
    let mut t = Term::new(4, 8, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    let s = format!(
        "{}{}{}\r\n{}{}{}",
        cell(&[0]),
        cell(&[]),
        cell(&[]),
        cell(&[1]),
        cell(&[]),
        cell(&[])
    );
    feed(&mut t, s.as_bytes());
    assert_eq!(runs(&t, 0), vec![run(0, 3, 42, 0, 0)]);
    assert_eq!(runs(&t, 1), vec![run(0, 3, 42, 1, 0)]);
}

#[test]
fn bare_cells_without_a_row_show_nothing() {
    // PH-11: kitty 0.49.2 draws nothing for a run whose first cell
    // has no row diacritic.
    let mut t = Term::new(2, 8, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, cell(&[]).repeat(4).as_bytes());
    assert!(runs(&t, 0).is_empty());
}

#[test]
fn inheritance_only_continues_a_compatible_left_neighbour() {
    let mut t = Term::new(2, 12, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    // (2,1) then row-only (3): a different row can't inherit, so a new
    // run starts at image column 0 (kitty's run-start default).
    feed(&mut t, cell(&[2, 1]).as_bytes());
    feed(&mut t, cell(&[3]).as_bytes());
    // (0,5) then (0,7): a column jump starts a new run.
    feed(&mut t, cell(&[0, 5]).as_bytes());
    feed(&mut t, cell(&[0, 7]).as_bytes());
    // Matching explicit diacritics continue the run.
    feed(&mut t, cell(&[0, 8]).as_bytes());
    assert_eq!(
        runs(&t, 0),
        vec![
            run(0, 1, 42, 2, 1),
            run(1, 1, 42, 3, 0),
            run(2, 1, 42, 0, 5),
            run(3, 2, 42, 0, 7),
        ]
    );
}

#[test]
fn colour_changes_split_runs() {
    let mut t = Term::new(2, 12, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    feed(&mut t, cell(&[]).as_bytes());
    // Same fg, new placement id: a bare cell can't inherit across it.
    feed(&mut t, b"\x1b[58;5;9m");
    feed(&mut t, cell(&[]).as_bytes());
    feed(&mut t, b"\x1b[59m\x1b[38;5;43m");
    feed(&mut t, cell(&[1, 0]).as_bytes());
    let r = runs(&t, 0);
    assert_eq!(r.len(), 2, "{r:?}");
    assert_eq!(r[0], run(0, 2, 42, 0, 0));
    assert_eq!(r[1], run(3, 1, 43, 1, 0));
}

#[test]
fn msb_is_inherited_along_a_run() {
    let mut t = Term::new(2, 8, 0);
    feed(&mut t, b"\x1b[38;5;5m");
    feed(&mut t, cell(&[0, 0, 2]).as_bytes());
    feed(&mut t, cell(&[0, 1]).as_bytes());
    feed(&mut t, cell(&[]).as_bytes());
    assert_eq!(runs(&t, 0), vec![run(0, 3, (2 << 24) | 5, 0, 0)]);
}

#[test]
fn every_colour_encoding_is_an_image_id() {
    // PH-10: SGR 30–37 / 90–97 are palette indices 0–15, like kitty.
    let mut t = Term::new(5, 4, 0);
    feed(&mut t, b"\x1b[31m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    feed(&mut t, b"\r\n\x1b[95m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    feed(&mut t, b"\r\n\x1b[38;5;200m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    feed(&mut t, b"\r\n\x1b[38;2;1;2;3m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    feed(&mut t, b"\r\n\x1b[39m");
    feed(&mut t, cell(&[0, 0]).as_bytes());
    assert_eq!(runs(&t, 0)[0].image_id, 1);
    assert_eq!(runs(&t, 1)[0].image_id, 13);
    assert_eq!(runs(&t, 2)[0].image_id, 200);
    assert_eq!(runs(&t, 3)[0].image_id, 0x01_0203);
    assert!(runs(&t, 4).is_empty(), "default fg = image 0 = nothing");
}

#[test]
fn diacritic_attaches_across_a_pending_wrap() {
    let mut t = Term::new(3, 4, 0);
    feed(&mut t, b"\x1b[38;5;42m\x1b[1;4H");
    feed(&mut t, cell(&[1, 3]).as_bytes());
    assert_eq!(runs(&t, 0), vec![run(3, 1, 42, 1, 3)]);
    // The next placeholder wraps; the row below starts a new run.
    feed(&mut t, cell(&[2, 0]).as_bytes());
    assert_eq!(runs(&t, 1), vec![run(0, 1, 42, 2, 0)]);
}

#[test]
fn stray_diacritic_after_text_is_not_a_placeholder() {
    let mut t = Term::new(2, 6, 0);
    feed(&mut t, b"\x1b[38;5;42ma");
    feed(&mut t, "\u{0305}".as_bytes());
    assert!(runs(&t, 0).is_empty());
    assert_eq!(
        t.grapheme_tail(&t.active_grid().row(0).cells[0]),
        "\u{0305}"
    );
}

// ---- the image follows the text (PH-1, PH-4, PH-8) -------------------

#[test]
fn overwriting_and_erasing_cells_removes_the_image() {
    let mut t = Term::new(6, 10, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(3, 6).as_bytes());
    // Overwrite the middle of row 0 with text.
    feed(&mut t, b"\x1b[39m\x1b[1;3Hxy");
    assert_eq!(runs(&t, 0), vec![run(0, 2, 42, 0, 0), run(4, 2, 42, 0, 4)]);
    // EL on row 1 from column 1.
    feed(&mut t, b"\x1b[2;2H\x1b[K");
    assert_eq!(runs(&t, 1), vec![run(0, 1, 42, 1, 0)]);
    // ECH on row 2.
    feed(&mut t, b"\x1b[3;1H\x1b[6X");
    assert!(runs(&t, 2).is_empty());
}

#[test]
fn ich_and_dch_move_the_image_with_the_cells() {
    let mut t = Term::new(4, 12, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(2, 4).as_bytes());
    // ICH 3 at column 0 of row 0 pushes the run right.
    feed(&mut t, b"\x1b[1;1H\x1b[3@");
    assert_eq!(runs(&t, 0), vec![run(3, 4, 42, 0, 0)]);
    // DCH 1 at column 1 of row 1 drops image column 1: the rest can no
    // longer continue the run (column jump), so it splits.
    feed(&mut t, b"\x1b[2;2H\x1b[P");
    assert_eq!(runs(&t, 1), vec![run(0, 1, 42, 1, 0), run(1, 2, 42, 1, 2)]);
}

#[test]
fn scrolled_off_placeholders_render_from_scrollback() {
    // PH-4: rows scrolled into history keep their placeholder cells.
    let mut t = Term::new(4, 8, 100);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(2, 3).as_bytes());
    feed(&mut t, b"\x1b[39m\r\n\r\n\r\n\r\n\r\n");
    assert!(runs(&t, 0).is_empty(), "scrolled off the live screen");
    t.scroll_view_by(4, 0.0, 16.0);
    t.force_snap_view();
    let found: Vec<_> = (0..4).flat_map(|r| view_runs(&t, r)).collect();
    assert_eq!(found, vec![run(0, 3, 42, 0, 0), run(0, 3, 42, 1, 0)]);
}

#[test]
fn narrowing_then_widening_restores_the_image() {
    // PH-8: reflow moves the cells, so the image comes back whole.
    let mut t = Term::new(4, 12, 100);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(1, 8).as_bytes());
    feed(&mut t, b"\x1b[39m$ ");
    t.resize(4, 5);
    t.resize(4, 12);
    let found: Vec<_> = (0..4).flat_map(|r| runs(&t, r)).collect();
    assert_eq!(found, vec![run(0, 8, 42, 0, 0)]);
}

// ---- virtual placements ----------------------------------------------

#[test]
fn u1_creates_a_virtual_placement_and_nothing_else() {
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=42,U=1,c=10,r=5,z=-3", 4, 2);
    assert!(t.image_grid().is_empty());
    assert!(t.has_virtual_placements());
    let vp = t.virtual_placements().find(42, 0).copied().unwrap();
    assert_eq!((vp.cols, vp.rows, vp.z), (10, 5, -3));
    // `a=p,U=1` on an existing image adds a named one.
    feed(&mut t, b"\x1b_Ga=p,i=42,p=7,U=1,c=3,q=2\x1b\\");
    assert_eq!(t.virtual_placements().find(42, 7).map(|v| v.cols), Some(3));
    // Unknown image: nothing is created (kitty).
    feed(&mut t, b"\x1b_Ga=p,i=99,U=1,c=3,q=2\x1b\\");
    assert!(!t.virtual_placements().contains_image(99));
}

#[test]
fn placeholders_resolve_against_the_current_table() {
    // PH-5: cells printed before the image resolve once it arrives.
    let mut t = Term::new(6, 10, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(2, 3).as_bytes());
    let r = runs(&t, 0)[0];
    assert!(t.resolve_placeholder(&r).is_none());
    let rev = t.image_revision();
    transmit(&mut t, "a=T,i=42,U=1,c=3,r=2", 4, 2);
    assert_ne!(t.image_revision(), rev, "renderer must redraw");
    assert_eq!(t.resolve_placeholder(&r).map(|v| v.image_id), Some(42));
    // A placement id picks that placement; an unknown one nothing.
    feed(&mut t, b"\x1b_Ga=p,i=42,p=5,U=1,c=9,q=2\x1b\\");
    let named = PlaceholderRun {
        placement_id: 5,
        ..r
    };
    assert_eq!(t.resolve_placeholder(&named).map(|v| v.cols), Some(9));
    let missing = PlaceholderRun {
        placement_id: 6,
        ..r
    };
    assert!(t.resolve_placeholder(&missing).is_none());
    // pid 0 keeps choosing the oldest placement.
    assert_eq!(t.resolve_placeholder(&r).map(|v| v.cols), Some(3));
}

#[test]
fn no_virtual_placement_means_no_image() {
    // PH-9: an image transmitted without U=1 isn't shown by
    // placeholders.
    let mut t = Term::new(4, 8, 0);
    transmit(&mut t, "a=t,i=42", 4, 2);
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(1, 3).as_bytes());
    assert!(!t.has_virtual_placements());
    assert!(t.resolve_placeholder(&runs(&t, 0)[0]).is_none());
}

#[test]
fn positional_deletes_and_clears_keep_virtual_placements() {
    // PH-7 / kitty 0.49.2: d=a/A/p/x/y/z/c, ED 2 and RIS never touch
    // virtual placements.
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=42,U=1,c=4,r=2", 4, 2);
    for d in [
        "d=a",
        "d=A",
        "d=p,x=1,y=1",
        "d=P,x=1,y=1",
        "d=x,x=1",
        "d=y,y=1",
        "d=z,z=0",
        "d=c",
        "d=C",
    ] {
        feed(&mut t, format!("\x1b_Ga=d,{d},q=2\x1b\\").as_bytes());
        assert!(t.virtual_placements().contains_image(42), "{d} removed it");
        assert!(t.image_registry().contains(42), "{d} freed the image");
    }
    feed(&mut t, b"\x1b[2J\x1b[3J\x1bc");
    assert!(t.virtual_placements().contains_image(42));
}

#[test]
fn id_deletes_remove_virtual_placements() {
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=1,U=1", 4, 2);
    feed(&mut t, b"\x1b_Ga=p,i=1,p=2,U=1,q=2\x1b\\");
    // d=i with p= removes just that placement.
    feed(&mut t, b"\x1b_Ga=d,d=i,i=1,p=2,q=2\x1b\\");
    assert!(t.virtual_placements().find(1, 2).is_none());
    assert!(t.virtual_placements().find(1, 0).is_some());
    // d=i removes the rest but keeps the bytes; d=I frees them.
    feed(&mut t, b"\x1b_Ga=d,d=i,i=1,q=2\x1b\\");
    assert!(!t.has_virtual_placements());
    assert!(t.image_registry().contains(1));
    transmit(&mut t, "a=T,i=3,U=1", 4, 2);
    feed(&mut t, b"\x1b_Ga=d,d=I,i=3,q=2\x1b\\");
    assert!(!t.has_virtual_placements());
    assert!(!t.image_registry().contains(3));
    // d=r over an id range, d=N by image number.
    transmit(&mut t, "a=T,i=10,U=1", 4, 2);
    transmit(&mut t, "a=T,i=11,U=1", 4, 2);
    feed(&mut t, b"\x1b_Ga=d,d=r,x=10,y=10,q=2\x1b\\");
    assert!(!t.virtual_placements().contains_image(10));
    assert!(t.virtual_placements().contains_image(11));
    transmit(&mut t, "a=T,I=77,U=1", 4, 2);
    assert_eq!(t.virtual_placements().len(), 2);
    feed(&mut t, b"\x1b_Ga=d,d=N,I=77,q=2\x1b\\");
    assert_eq!(t.virtual_placements().len(), 1);
}

#[test]
fn retransmitting_the_image_drops_its_virtual_placements() {
    // Spec + kitty 0.49.2: re-sending an id deletes its placements; a
    // fresh `U=1` brings the existing cells back (PH-6).
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=42,U=1,c=4,r=2", 4, 2);
    transmit(&mut t, "a=t,i=42", 8, 8);
    assert!(!t.has_virtual_placements());
    feed(&mut t, b"\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    feed(&mut t, b"\x1b[38;5;42m");
    feed(&mut t, block(2, 4).as_bytes());
    transmit(&mut t, "a=T,i=42,U=1,c=4,r=2", 8, 8);
    let r = runs(&t, 0)[0];
    assert_eq!(t.resolve_placeholder(&r).map(|v| v.image_id), Some(42));
    assert_eq!(t.image_registry().get(42).map(|i| i.width), Some(8));
}

#[test]
fn virtual_placements_are_per_screen() {
    // kitty keeps one table per screen.
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=1,U=1", 4, 2);
    feed(&mut t, b"\x1b[?1049h");
    assert!(
        !t.has_virtual_placements(),
        "main's table isn't visible on alt"
    );
    transmit(&mut t, "a=T,i=2,U=1", 4, 2);
    assert!(t.virtual_placements().contains_image(2));
    feed(&mut t, b"\x1b[?1049l");
    assert!(t.virtual_placements().contains_image(1));
    assert!(
        !t.virtual_placements().contains_image(2),
        "alt's dropped on exit"
    );
}

#[test]
fn placeholder_origin_is_the_minimum_cell() {
    // The REL-5 anchor: min row and min column over the cells that draw
    // the placement.
    let mut t = Term::new(8, 12, 0);
    transmit(&mut t, "a=T,i=42,U=1,p=3,c=4,r=3", 4, 2);
    assert_eq!(t.placeholder_origin(42, 3), None, "no cells yet");
    feed(&mut t, b"\x1b[38;5;42m\x1b[58;5;3m");
    // Row 2 starts at column 5, row 3 at column 2.
    feed(&mut t, format!("\x1b[3;6H{}", cell(&[0, 1])).as_bytes());
    feed(
        &mut t,
        format!("\x1b[4;3H{}{}", cell(&[1, 0]), cell(&[])).as_bytes(),
    );
    assert_eq!(t.placeholder_origin(42, 3), Some((2, 2)));
    assert_eq!(t.placeholder_origin(42, 4), None);
}

// ---- review fixes (#28) ---------------------------------------------

/// Placeholder cell with arbitrary diacritic indices (beyond the short
/// `DIACRITICS` table above).
fn cell_any(row: u16, col: u16, msb: u16) -> String {
    use toastty_graphics::kitty::placeholder::index_to_diacritic;
    let mut s = String::from(crate::PLACEHOLDER);
    for i in [row, col, msb] {
        s.push(index_to_diacritic(i).unwrap());
    }
    s
}

/// Print `n` placeholder cells with distinct (row, col, msb) triples,
/// starting at triple number `from`, as kitten icat would over many
/// images with random ids. Each lands in the extras table as its own
/// entry.
fn print_distinct(t: &mut Term, from: u32, n: u32) {
    let mut s = String::from("\x1b[38;5;42m");
    for k in from..from + n {
        let (row, col, msb) = (
            (k % 200) as u16,
            ((k / 200) % 200) as u16,
            (k / 40_000) as u16,
        );
        s.push_str(&cell_any(row, col, msb));
    }
    s.push_str("\x1b[39m");
    feed(t, s.as_bytes());
}

#[test]
fn a_full_extras_table_reclaims_scrolled_off_placeholders() {
    // #28 review blocker: every distinct placeholder cell took a table
    // entry for the rest of the session, so ~100 icat runs blanked new
    // images and dropped combining marks. Entries whose cells are gone
    // are now reclaimed when the table fills.
    let mut t = Term::new(4, 20, 10);
    let max = crate::extra::MAX_ENTRIES as u32;
    print_distinct(&mut t, 0, max + 5_000);
    // The table filled, was reclaimed, and is still accepting.
    assert!(t.extras.len() <= crate::extra::MAX_ENTRIES);
    // A new placeholder keeps its diacritics…
    feed(&mut t, b"\r\n\x1b[38;5;7m");
    feed(&mut t, cell(&[3, 4]).as_bytes());
    let r = runs(&t, t.cursor().row);
    assert_eq!(
        r.last().map(|r| (r.image_id, r.img_row, r.img_col)),
        Some((7, 3, 4))
    );
    // …and so does a combining mark.
    feed(&mut t, "\x1b[39me\u{301}".as_bytes());
    let row = t.active_grid().row(t.cursor().row);
    let e = row.cells[usize::from(t.cursor().col) - 1];
    assert_eq!(t.grapheme_tail(&e), "\u{301}");
}

#[test]
fn long_icat_loops_stay_bounded() {
    // Two table-fulls of distinct cells: what's live is bounded by
    // what's on the grids (here 4 + 10 rows of 20 cells), not by
    // history.
    let mut t = Term::new(4, 20, 10);
    let max = crate::extra::MAX_ENTRIES as u32;
    for round in 0..2 {
        print_distinct(&mut t, round * max, max);
    }
    t.collect_extras();
    assert!(t.extras.len() <= 14 * 20, "{} live entries", t.extras.len());
    // Reclamation never frees what's still on screen.
    let r = runs(&t, 3);
    assert!(!r.is_empty());
    for c in &t.active_grid().row(3).cells {
        if c.ch == crate::PLACEHOLDER {
            assert!(matches!(t.cell_extra(c), Some(CellExtra::Placeholder(_))));
        }
    }
}

#[test]
fn copy_keeps_the_diacritics_like_kitty() {
    let mut t = Term::new(2, 8, 0);
    feed(&mut t, b"\x1b[38;5;42m");
    let printed = format!("{}{}{}", cell(&[1, 2]), cell(&[]), cell(&[1, 4, 3]));
    feed(&mut t, printed.as_bytes());
    let mut sel = crate::selection::Selection::new(
        crate::selection::Pos::new(t.bottom_id() - 1, 0),
        crate::selection::SelectionMode::Char,
    );
    sel.set_active(crate::selection::Pos::new(t.bottom_id() - 1, 2));
    t.set_selection(sel);
    assert_eq!(
        t.extract_selection_text().as_deref(),
        Some(printed.as_str())
    );
}

#[test]
fn retransmit_on_alt_drops_mains_stale_placement() {
    // #28 review: re-transmission / eviction only cleaned the active
    // screen's table, so a stashed placement's old c/r applied to an
    // image that later reused the id.
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=5,U=1,c=3,r=2", 4, 2);
    feed(&mut t, b"\x1b[?1049h");
    transmit(&mut t, "a=t,i=5", 8, 8);
    feed(&mut t, b"\x1b[?1049l");
    assert!(!t.virtual_placements().contains_image(5));
}

#[test]
fn eviction_or_free_on_alt_drops_mains_stale_placement() {
    let mut t = Term::new(6, 10, 0);
    transmit(&mut t, "a=T,i=5,U=1,c=3,r=2", 4, 2);
    transmit(&mut t, "a=T,i=6,U=1,c=3,r=2", 4, 2);
    feed(&mut t, b"\x1b[?1049h");
    // Evict everything while alt is up, then reuse id 5 there.
    t.set_image_cap(1);
    assert!(!t.image_registry().contains(5));
    feed(&mut t, b"\x1b[?1049l");
    assert!(!t.virtual_placements().contains_image(5), "evicted on alt");
    // Reusing the id on main doesn't bring the old c/r back.
    t.set_image_cap(64 * 1024 * 1024);
    transmit(&mut t, "a=t,i=5", 8, 8);
    assert!(t.virtual_placements().find(5, 0).is_none());
    // A byte-freeing delete on alt also cleans main's table.
    transmit(&mut t, "a=T,i=7,U=1,c=3,r=2", 4, 2);
    feed(&mut t, b"\x1b[?1049h\x1b_Ga=d,d=I,i=7,q=2\x1b\\\x1b[?1049l");
    assert!(!t.virtual_placements().contains_image(7));
}

#[test]
fn scrollback_full_of_live_images_drops_the_oldest_not_the_newest() {
    // The reviewer's case with everything still in scrollback: more
    // distinct placeholder cells than the table holds are all live.
    // The oldest scrollback rows give up their image data so new images
    // keep working, and combining marks have their own reserve.
    let mut t = Term::new(4, 20, 10_000);
    let max = crate::extra::MAX_ENTRIES as u32;
    print_distinct(&mut t, 0, max + 20_000);
    assert!(t.extras.placeholder_count() <= crate::extra::MAX_PLACEHOLDER_ENTRIES);
    // New placeholder cells resolve.
    feed(&mut t, b"\r\n\x1b[38;5;7m");
    feed(&mut t, cell(&[3, 4]).as_bytes());
    let r = runs(&t, t.cursor().row);
    assert_eq!(
        r.last().map(|r| (r.image_id, r.img_row, r.img_col)),
        Some((7, 3, 4))
    );
    // The newest scrollback rows still have theirs; the oldest don't.
    let mut out = Vec::new();
    t.placeholder_runs(t.primary.scrollback_row(0).unwrap(), &mut out);
    assert!(!out.is_empty(), "recent history keeps its images");
    let oldest = t.primary.history_lines() - 1;
    out.clear();
    t.placeholder_runs(t.primary.scrollback_row(oldest).unwrap(), &mut out);
    assert!(out.is_empty(), "the oldest history row gave its data up");
    // Combining marks are unaffected.
    feed(&mut t, "\x1b[39me\u{301}".as_bytes());
    let row = t.active_grid().row(t.cursor().row);
    let e = row.cells[usize::from(t.cursor().col) - 1];
    assert_eq!(t.grapheme_tail(&e), "\u{301}");
}

#[test]
fn graphemes_keep_their_reserve() {
    // Placeholders are capped below the table size, so a grapheme can
    // always be interned while placeholders are at their quota.
    let mut t = Term::new(4, 20, 10_000);
    print_distinct(&mut t, 0, crate::extra::MAX_ENTRIES as u32);
    assert!(t.extras.placeholder_count() <= crate::extra::MAX_PLACEHOLDER_ENTRIES);
    assert!(
        !t.extras
            .would_reject(&CellExtra::Grapheme("\u{302}".into()))
    );
}
