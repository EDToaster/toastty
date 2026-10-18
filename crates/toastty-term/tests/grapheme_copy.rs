//! Grapheme-cluster storage and copy fidelity (audit bugs #8 and #20).
//!
//! Drives `Term` through the real parser and checks both the cell-grid
//! layout (columns / cursor arithmetic, which apps depend on) and the
//! text `extract_selection_text` hands to the clipboard, which must be
//! byte-exact for combining marks, ZWJ sequences, variation selectors and
//! literal tabs.

use toastty_parser::Parser;
use toastty_term::{CellExtra, Pos, Selection, SelectionMode, Term};

fn feed(t: &mut Term, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(t, bytes);
}

fn run(rows: u16, cols: u16, s: &str) -> Term {
    let mut t = Term::new(rows, cols, 64);
    feed(&mut t, s.as_bytes());
    t
}

fn pos_for(t: &Term, row: u16, col: u16) -> Pos {
    let (rows, _) = t.size();
    Pos::new(t.bottom_id() - u64::from(rows - 1 - row), col)
}

/// Char-select visible `(r0, c0)..=(r1, c1)` and return the copied text.
fn copy(t: &mut Term, r0: u16, c0: u16, r1: u16, c1: u16) -> String {
    let mut sel = Selection::new(pos_for(t, r0, c0), SelectionMode::Char);
    sel.set_active(pos_for(t, r1, c1));
    t.set_selection(sel);
    t.extract_selection_text().unwrap_or_default()
}

/// Copy the whole of visible row `r`.
fn copy_row(t: &mut Term, r: u16) -> String {
    let cols = t.size().1;
    copy(t, r, 0, r, cols - 1)
}

/// Full text of the cell at `(r, c)`: `ch` + grapheme tail.
fn cell_text(t: &Term, r: u16, c: u16) -> String {
    let cell = t.row(r).cells[c as usize];
    format!("{}{}", cell.ch, t.grapheme_tail(&cell))
}

#[test]
fn decomposed_accents_copy_exactly_and_keep_columns() {
    let s = "e\u{301} n\u{303} a\u{308}\u{304}";
    let mut t = run(4, 20, s);
    // Three base letters + two spaces: zero-width marks take no column.
    assert_eq!(t.cursor().col, 5);
    assert_eq!(cell_text(&t, 0, 0), "e\u{301}");
    assert_eq!(cell_text(&t, 0, 2), "n\u{303}");
    assert_eq!(cell_text(&t, 0, 4), "a\u{308}\u{304}");
    assert_eq!(copy_row(&mut t, 0), s);
}

#[test]
fn zwj_family_occupies_first_emoji_width() {
    let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}";
    let mut t = run(4, 20, &format!("{fam}x"));
    let row = t.row(0);
    assert_eq!(row.cells[0].ch, '\u{1F468}');
    assert!(row.cells[1].is_continuation);
    assert_eq!(row.cells[2].ch, 'x');
    assert_eq!(cell_text(&t, 0, 0), fam);
    assert_eq!(t.cursor().col, 3);
    assert_eq!(copy_row(&mut t, 0), format!("{fam}x"));
}

#[test]
fn zwj_family_same_under_2027() {
    let fam = "\u{1F468}\u{200D}\u{1F469}\u{200D}\u{1F467}\u{200D}\u{1F466}";
    let mut t = run(4, 20, &format!("\x1b[?2027h{fam}|"));
    assert_eq!(t.cursor().col, 3);
    assert_eq!(copy_row(&mut t, 0), format!("{fam}|"));
}

#[test]
fn vs16_heart_legacy_stays_one_cell() {
    let mut t = run(4, 20, "\u{2764}\u{FE0F}|");
    assert_eq!(t.cursor().col, 2);
    assert_eq!(t.row(0).cells[1].ch, '|');
    assert_eq!(copy_row(&mut t, 0), "\u{2764}\u{FE0F}|");
}

#[test]
fn vs16_heart_under_2027_grows_to_two_cells() {
    let mut t = run(4, 20, "\x1b[?2027h\u{2764}\u{FE0F}|");
    assert!(t.row(0).cells[1].is_continuation);
    assert_eq!(t.row(0).cells[2].ch, '|');
    assert_eq!(t.cursor().col, 3);
    assert_eq!(copy_row(&mut t, 0), "\u{2764}\u{FE0F}|");
}

#[test]
fn growth_clobbers_wide_neighbour_cleanly() {
    // Cursor moved back onto a narrow cell sitting before a wide char;
    // growing the narrow cell must not strand the wide char's half.
    let mut t = run(4, 20, "\x1b[?2027h\u{2764}\u{4F60}\x1b[1;2H\u{FE0F}");
    // The cursor is at col 1 (on 你's primary); the left cell is ❤.
    let row = t.row(0);
    assert!(row.cells[1].is_continuation);
    assert_eq!(row.cells[2].ch, ' ');
    assert!(!row.cells[2].is_continuation);
    assert_eq!(copy_row(&mut t, 0), "\u{2764}\u{FE0F}");
}

#[test]
fn zwj_with_narrow_base_grows_to_two_cells() {
    // ❤ (wcwidth 1) + ZWJ + 🔥 (wcwidth 2) renders as one 2-cell glyph;
    // the grid must reserve both cells in either mode.
    let s = "\u{2764}\u{200D}\u{1F525}";
    let mut t = run(4, 20, &format!("{s}|"));
    assert!(t.row(0).cells[1].is_continuation);
    assert_eq!(t.cursor().col, 3);
    assert_eq!(copy_row(&mut t, 0), format!("{s}|"));
}

#[test]
fn no_growth_in_last_column() {
    let mut t = run(4, 4, "\x1b[?2027habc\u{2764}\u{FE0F}");
    // ❤ sits in the last column (pending wrap); no room to grow.
    assert_eq!(cell_text(&t, 0, 3), "\u{2764}\u{FE0F}");
    assert!(!t.row(1).cells[0].is_continuation);
    assert_eq!(copy_row(&mut t, 0), "abc\u{2764}\u{FE0F}");
}

#[test]
fn skin_tone_modifier_joins_its_base() {
    let s = "\u{1F44D}\u{1F3FD}";
    for prefix in ["", "\x1b[?2027h"] {
        let mut t = run(4, 20, &format!("{prefix}{s}|"));
        assert_eq!(cell_text(&t, 0, 0), s);
        assert!(t.row(0).cells[1].is_continuation);
        assert_eq!(t.row(0).cells[2].ch, '|');
        assert_eq!(copy_row(&mut t, 0), format!("{s}|"));
    }
    // A lone modifier (no emoji base) stays its own wide cell.
    let mut t = run(4, 20, "a\u{1F3FD}|");
    assert_eq!(t.row(0).cells[1].ch, '\u{1F3FD}');
    assert_eq!(copy_row(&mut t, 0), "a\u{1F3FD}|");
}

#[test]
fn regional_indicator_flag_legacy_and_2027() {
    let flag = "\u{1F1EF}\u{1F1F5}";
    // Legacy: two width-1 cells (per-codepoint wcwidth), copied intact.
    let mut t = run(4, 20, &format!("{flag}|"));
    assert_eq!(t.cursor().col, 3);
    assert_eq!(copy_row(&mut t, 0), format!("{flag}|"));
    // 2027: one cluster, two cells.
    let mut t = run(4, 20, &format!("\x1b[?2027h{flag}|"));
    assert_eq!(cell_text(&t, 0, 0), flag);
    assert!(t.row(0).cells[1].is_continuation);
    assert_eq!(t.cursor().col, 3);
    // A third RI starts a new cluster.
    let mut t2 = run(4, 20, &format!("\x1b[?2027h{flag}\u{1F1FA}"));
    assert_eq!(cell_text(&t2, 0, 2), "\u{1F1FA}");
    assert_eq!(copy_row(&mut t, 0), format!("{flag}|"));
    assert_eq!(copy_row(&mut t2, 0), format!("{flag}\u{1F1FA}"));
}

#[test]
fn devanagari_copies_exactly() {
    let s = "\u{928}\u{92E}\u{938}\u{94D}\u{924}\u{947}"; // नमस्ते
    let mut t = run(4, 20, s);
    // न म स(+virama) त(+e-sign): wcwidth sum is 4.
    assert_eq!(t.cursor().col, 4);
    assert_eq!(copy_row(&mut t, 0), s);
    let mut t = run(4, 20, &format!("\x1b[?2027h{s}"));
    assert_eq!(copy_row(&mut t, 0), s);
}

#[test]
fn hangul_jamo_compose_into_one_cell() {
    let s = "\u{1100}\u{1161}\u{11A8}"; // 각 as conjoining jamo
    let mut t = run(4, 20, s);
    assert_eq!(cell_text(&t, 0, 0), s);
    assert_eq!(t.cursor().col, 2);
    assert_eq!(copy_row(&mut t, 0), s);
}

#[test]
fn combining_mark_after_wide_char_attaches_to_primary() {
    let mut t = run(4, 20, "\u{4F60}\u{301}x");
    assert_eq!(cell_text(&t, 0, 0), "\u{4F60}\u{301}");
    assert!(t.row(0).cells[1].is_continuation);
    assert_eq!(copy_row(&mut t, 0), "\u{4F60}\u{301}x");
}

#[test]
fn combining_mark_at_column_zero_is_dropped() {
    let mut t = run(4, 20, "\u{301}a");
    assert_eq!(t.row(0).cells[0].ch, 'a');
    assert_eq!(t.row(0).cells[0].extra, None);
    assert_eq!(copy_row(&mut t, 0), "a");
}

#[test]
fn combining_mark_in_pending_wrap_attaches_to_last_column() {
    let mut t = run(4, 3, "abc\u{301}d");
    assert_eq!(cell_text(&t, 0, 2), "c\u{301}");
    assert_eq!(t.row(1).cells[0].ch, 'd');
    assert_eq!(copy(&mut t, 0, 0, 1, 2), "abc\u{301}d");
}

#[test]
fn ascii_after_zwj_starts_new_cell() {
    let mut t = run(4, 20, "\u{1F468}\u{200D}a");
    assert_eq!(cell_text(&t, 0, 0), "\u{1F468}\u{200D}");
    assert_eq!(t.row(0).cells[2].ch, 'a');
    assert_eq!(copy_row(&mut t, 0), "\u{1F468}\u{200D}a");
}

#[test]
fn non_joining_wide_char_after_zwj_tail_starts_new_cell() {
    // CJK after ZWJ: UAX #29 breaks (GB11 needs ExtPict on both sides).
    let t = run(4, 20, "\u{1F468}\u{200D}\u{4F60}");
    assert_eq!(t.row(0).cells[2].ch, '\u{4F60}');
    assert_eq!(t.cursor().col, 4);
}

#[test]
fn overwrite_and_erase_drop_the_grapheme() {
    let mut t = run(4, 20, "e\u{301}x");
    feed(&mut t, b"\r");
    feed(&mut t, b"E");
    assert_eq!(t.row(0).cells[0].extra, None);
    assert_eq!(copy_row(&mut t, 0), "Ex");
    let mut t = run(4, 20, "e\u{301}x\r\x1b[K");
    assert_eq!(t.row(0).cells[0].extra, None);
    assert_eq!(copy_row(&mut t, 0), "");
}

#[test]
fn insert_and_delete_chars_carry_the_grapheme() {
    // ICH then DCH: the grapheme moves with its cell.
    let mut t = run(4, 20, "e\u{301}x\r\x1b[2@");
    assert_eq!(cell_text(&t, 0, 2), "e\u{301}");
    assert_eq!(copy_row(&mut t, 0), "  e\u{301}x");
    feed(&mut t, b"\x1b[2P");
    assert_eq!(copy_row(&mut t, 0), "e\u{301}x");
}

#[test]
fn graphemes_survive_scrollback_and_reflow() {
    let line = "ab\u{1F468}\u{200D}\u{1F469}cd e\u{301}f";
    let mut t = Term::new(3, 10, 64);
    feed(&mut t, line.as_bytes());
    feed(&mut t, b"\r\n1\r\n2\r\n3\r\n");
    // Reflow narrower (wraps the line) then wider again.
    t.resize(3, 4);
    t.resize(3, 12);
    // The line is now in scrollback; find its line id by walking back.
    let bottom = t.bottom_id();
    let mut found = None;
    for back in 0..10u64 {
        let Some(id) = bottom.checked_sub(back) else {
            break;
        };
        if let Some(row) = t.primary_row_by_line_id(id)
            && row.cells.first().is_some_and(|c| c.ch == 'a')
        {
            found = Some(id);
        }
    }
    let id = found.expect("line retained");
    let mut sel = Selection::new(Pos::new(id, 0), SelectionMode::Char);
    sel.set_active(Pos::new(id, 11));
    t.set_selection(sel);
    assert_eq!(t.extract_selection_text().as_deref(), Some(line));
}

#[test]
fn ris_resets_extras() {
    let mut t = run(4, 20, "e\u{301}");
    feed(&mut t, b"\x1bc");
    assert_eq!(t.row(0).cells[0].extra, None);
    // Interning still works after the reset.
    feed(&mut t, "o\u{308}".as_bytes());
    assert_eq!(cell_text(&t, 0, 0), "o\u{308}");
}

#[test]
fn grapheme_length_is_capped() {
    let mut s = String::from("a");
    for _ in 0..100 {
        s.push('\u{301}');
    }
    s.push('b');
    let t = run(4, 20, &s);
    let tail = t.grapheme_tail(&t.row(0).cells[0]).to_string();
    assert!(tail.len() <= 64 && !tail.is_empty());
    assert!(tail.chars().all(|c| c == '\u{301}'));
    // The overflow marks were swallowed, not printed as new cells.
    assert_eq!(t.row(0).cells[1].ch, 'b');
}

#[test]
fn alt_screen_cells_keep_graphemes() {
    let t = run(4, 20, "\x1b[?1049he\u{301}");
    assert_eq!(cell_text(&t, 0, 0), "e\u{301}");
}

// ---- tabs (bug #20) ---------------------------------------------------

#[test]
fn tab_copies_as_literal_tab() {
    let mut t = run(4, 40, "a\tb\tc");
    assert_eq!(t.row(0).cells[8].ch, 'b');
    assert!(matches!(
        t.cell_extra(&t.row(0).cells[1]),
        Some(CellExtra::Tab(7))
    ));
    assert_eq!(copy_row(&mut t, 0), "a\tb\tc");
}

#[test]
fn tab_at_tab_stop_and_consecutive_tabs() {
    let mut t = run(4, 40, "\t\tx\r\n12345678\ty");
    assert_eq!(copy_row(&mut t, 0), "\t\tx");
    assert_eq!(copy_row(&mut t, 1), "12345678\ty");
}

#[test]
fn tab_over_existing_text_copies_the_text() {
    let mut t = run(4, 40, "abcdefghij\r\tX");
    assert_eq!(copy_row(&mut t, 0), "abcdefghXj");
}

#[test]
fn text_written_into_tab_run_breaks_the_tab() {
    let mut t = run(4, 40, "a\tb\r\x1b[4Cz");
    // "a" + 3 blanks + "z" + 3 blanks + "b".
    assert_eq!(copy_row(&mut t, 0), "a   z   b");
    // Overwriting the start cell (a space) drops the marker too.
    let mut t = run(4, 40, "a\tb\r\x1b[1C ");
    assert_eq!(copy_row(&mut t, 0), "a       b");
}

#[test]
fn erased_tab_run_copies_as_spaces() {
    let mut t = run(4, 40, "a\tb\r\x1b[1C\x1b[3X");
    assert_eq!(copy_row(&mut t, 0), "a       b");
}

#[test]
fn partial_selection_of_tab_run_copies_spaces() {
    let mut t = run(4, 40, "a\tb");
    // Ends inside the run.
    assert_eq!(copy(&mut t, 0, 0, 0, 4), "a");
    // Starts inside the run.
    assert_eq!(copy(&mut t, 0, 3, 0, 8), "     b");
    // Covers the whole run exactly.
    assert_eq!(copy(&mut t, 0, 1, 0, 8), "\tb");
}

#[test]
fn trailing_tab_is_trimmed_like_whitespace() {
    let mut t = run(4, 40, "a\t\r\nb");
    assert_eq!(copy(&mut t, 0, 0, 1, 0), "a\nb");
}

#[test]
fn tab_clamped_at_last_column() {
    let mut t = run(4, 10, "abcdefgh\tX\r\nabcdefghi\tY");
    // 8 → clamped to 9 (a one-cell run), then X lands in col 9.
    assert_eq!(copy_row(&mut t, 0), "abcdefgh\tX");
    // At col 9 already: HT doesn't move, nothing to mark.
    assert_eq!(copy_row(&mut t, 1), "abcdefghiY");
}

#[test]
fn combining_mark_after_tab_attaches_to_run_start() {
    // Degenerate, but must not panic or lose the mark.
    let mut t = run(4, 40, "a\t\u{301}");
    assert_eq!(
        copy(&mut t, 0, 0, 0, 39)
            .chars()
            .filter(|&c| c == '\u{301}')
            .count(),
        1
    );
}

#[test]
fn block_selection_respects_tab_runs() {
    let mut t = run(4, 40, "a\tb\r\nc\td");
    let mut sel = Selection::new(pos_for(&t, 0, 0), SelectionMode::Block);
    sel.set_active(pos_for(&t, 1, 8));
    t.set_selection(sel);
    assert_eq!(t.extract_selection_text().as_deref(), Some("a\tb\nc\td"));
}
