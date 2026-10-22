//! Term-level tests for OSC 7700 rich blocks, using a fake layout.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use base64::Engine as _;
use toastty_parser::Parser;
use unicode_segmentation::UnicodeSegmentation;

use crate::rich::{
    RichBlockLayout, RichImage, RichLayout, RichLayoutRequest, RichLinkSpan, RichTextRun,
};
use crate::selection::{Pos, Selection, SelectionMode};
use crate::{RichEnv, SecurityFlags, Term};

/// `(cols, cell_w, cell_h, font_px, allow_local_files, base_dir, mono_family)`
type LastReq = (
    u16,
    u32,
    u32,
    f32,
    bool,
    Option<std::path::PathBuf>,
    Option<String>,
);

/// Fake layout: every source line is wrapped into `cols`-wide chunks,
/// one row each, so the row count depends on the width. The image is a
/// solid `palette.fg` fill. Lines starting with `link:` become a link
/// over the whole row.
#[derive(Clone, Default)]
struct Fake {
    calls: Arc<AtomicUsize>,
    last: Arc<Mutex<Option<LastReq>>>,
    no_image: bool,
    /// Add an extra blank row when cells are wider than 10 px, so a
    /// cell-size change alters the block's row count.
    taller_when_wide: bool,
}

impl RichBlockLayout for Fake {
    fn layout(&mut self, req: &RichLayoutRequest<'_>) -> Option<RichLayout> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        *self.last.lock().unwrap() = Some((
            req.cols,
            req.cell_w,
            req.cell_h,
            req.font_px,
            req.allow_local_files,
            req.base_dir.map(std::path::Path::to_path_buf),
            req.mono_family.map(str::to_owned),
        ));
        if req.source == "FAIL" {
            return None;
        }
        let cols = req.cols as usize;
        let mut text = Vec::new();
        let mut links = Vec::new();
        for line in req.source.lines() {
            if line.is_empty() {
                text.push(Vec::new());
                continue;
            }
            // Wrap at `cols` cells, whole grapheme clusters per row.
            let mut chunks: Vec<(String, usize)> = vec![];
            let mut cur = (String::new(), 0);
            for g in line.graphemes(true) {
                let w = super::block_cluster_cells(g);
                if cur.1 + w > cols && !cur.0.is_empty() {
                    chunks.push(std::mem::take(&mut cur));
                }
                cur.0.push_str(g);
                cur.1 += w;
            }
            chunks.push(cur);
            for (s, cells) in chunks {
                if let Some(url) = s.strip_prefix("link:") {
                    links.push(RichLinkSpan {
                        row: u16::try_from(text.len()).unwrap(),
                        cols: 0..u16::try_from(cells).unwrap(),
                        url: url.to_owned(),
                    });
                }
                text.push(vec![RichTextRun { col: 0, text: s }]);
            }
        }
        if text.is_empty() {
            text.push(Vec::new());
        }
        if self.taller_when_wide && req.cell_w > 10 {
            text.push(Vec::new());
        }
        let rows = u16::try_from(text.len()).unwrap().min(req.max_rows);
        let w = u32::from(req.cols) * req.cell_w;
        let h = u32::from(rows) * req.cell_h;
        let fg = req.palette.fg;
        let image = (!self.no_image).then(|| {
            let mut px = Vec::with_capacity((w * h * 4) as usize);
            for _ in 0..w * h {
                px.extend_from_slice(&[fg[0], fg[1], fg[2], 255]);
            }
            RichImage::from_pixels(w, h, px).unwrap()
        });
        Some(RichLayout {
            rows,
            image,
            text,
            links,
        })
    }
}

fn feed(t: &mut Term, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(t, bytes);
}

fn term(rows: u16, cols: u16, sb: u16) -> (Term, Fake) {
    let mut t = Term::new(rows, cols, sb);
    let fake = Fake::default();
    t.set_rich_layout(Box::new(fake.clone()));
    (t, fake)
}

fn b64(s: &str) -> String {
    base64::engine::general_purpose::STANDARD.encode(s)
}

fn osc(md: &str) -> Vec<u8> {
    format!("\x1b]7700;md;;{}\x1b\\", b64(md)).into_bytes()
}

fn row_text(t: &Term, r: u16) -> String {
    t.row(r)
        .cells()
        .iter()
        .filter(|c| !c.is_continuation)
        .map(|c| c.ch)
        .collect::<String>()
        .trim_end()
        .to_owned()
}

fn tag(t: &Term, r: u16) -> Option<(u64, u16)> {
    t.row(r).rich.map(|x| (x.block, x.index))
}

#[test]
fn block_rows_text_tags_and_cursor() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("hello\nworld"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(row_text(&t, 0), "hello");
    assert_eq!(row_text(&t, 1), "world");
    let id = t.row(0).rich.unwrap().block;
    assert_eq!(tag(&t, 0), Some((id, 0)));
    assert_eq!(tag(&t, 1), Some((id, 1)));
    assert_eq!(tag(&t, 2), None);
    let c = t.cursor();
    assert_eq!((c.row, c.col), (2, 0));
    let img = t.rich_blocks().image(id).unwrap();
    assert_eq!((img.width, img.height), (80, 32));
    assert_eq!(t.rich_block_source(id), Some("hello\nworld"));
}

#[test]
fn bel_terminator_and_unpadded_payload() {
    let (mut t, _) = term(4, 10, 0);
    // "hi" → aGk (unpadded), BEL-terminated.
    feed(&mut t, b"\x1b]7700;md;;aGk\x07");
    assert_eq!(row_text(&t, 0), "hi");
    assert!(t.row(0).rich.is_some());
}

#[test]
fn keys_cwd_and_unknown_keys() {
    let (mut t, fake) = term(4, 10, 0);
    t.set_security(SecurityFlags {
        markdown_local_files: true,
        ..SecurityFlags::default()
    });
    let seq = format!(
        "\x1b]7700;md;future=1:cwd={};{}\x1b\\",
        b64("/tmp/docs"),
        b64("x")
    );
    feed(&mut t, seq.as_bytes());
    assert!(t.row(0).rich.is_some());
    let last = fake.last.lock().unwrap().clone().unwrap();
    assert!(last.4);
    assert_eq!(last.5.as_deref(), Some(std::path::Path::new("/tmp/docs")));
}

#[test]
fn base_dir_withheld_when_local_files_off() {
    let (mut t, fake) = term(4, 10, 0);
    let seq = format!("\x1b]7700;md;cwd={};{}\x1b\\", b64("/tmp"), b64("x"));
    feed(&mut t, seq.as_bytes());
    let last = fake.last.lock().unwrap().clone().unwrap();
    assert!(!last.4);
    assert_eq!(last.5, None);
}

#[test]
fn invalid_sequences_are_ignored() {
    let (mut t, fake) = term(4, 10, 0);
    feed(&mut t, b"\x1b]7700;md;;!!!notbase64\x1b\\");
    feed(&mut t, b"\x1b]7700;svg;;aGk=\x1b\\");
    feed(&mut t, b"\x1b]7700;md;aGk=\x1b\\");
    feed(&mut t, b"\x1b]7700\x1b\\");
    feed(&mut t, b"\x1b]7700;md;;/w==\x1b\\"); // invalid UTF-8
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert!(t.rich_blocks().is_empty());
    assert_eq!(row_text(&t, 0), "");
    assert_eq!((t.cursor().row, t.cursor().col), (0, 0));
}

#[test]
fn layout_failure_writes_nothing() {
    let (mut t, fake) = term(4, 10, 0);
    feed(&mut t, &osc("FAIL"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(t.rich_blocks().is_empty());
    assert_eq!((t.cursor().row, t.cursor().col), (0, 0));
}

#[test]
fn gating_off_ignores_sequence() {
    let (mut t, fake) = term(4, 10, 0);
    t.set_security(SecurityFlags {
        markdown: false,
        ..SecurityFlags::default()
    });
    feed(&mut t, &osc("hello"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn security_defaults() {
    let f = SecurityFlags::default();
    assert!(f.markdown);
    assert!(!f.markdown_local_files);
}

#[test]
fn without_layout_sequence_is_ignored() {
    let mut t = Term::new(4, 10, 0);
    feed(&mut t, &osc("hello"));
    assert_eq!(row_text(&t, 0), "");
    assert!(!t.has_rich_layout());
}

#[test]
fn alt_screen_ignores_blocks() {
    let (mut t, fake) = term(4, 10, 0);
    feed(&mut t, b"\x1b[?1049h");
    feed(&mut t, &osc("hello"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 0);
    assert_eq!(row_text(&t, 0), "");
    feed(&mut t, b"\x1b[?1049l");
    assert_eq!(row_text(&t, 0), "");
}

#[test]
fn newline_first_when_cursor_not_at_col0() {
    let (mut t, _) = term(6, 10, 0);
    feed(&mut t, b"$ cmd");
    feed(&mut t, &osc("a\nb"));
    assert_eq!(row_text(&t, 0), "$ cmd");
    assert!(t.row(0).rich.is_none());
    assert_eq!(row_text(&t, 1), "a");
    assert_eq!(row_text(&t, 2), "b");
    assert_eq!((t.cursor().row, t.cursor().col), (3, 0));
    feed(&mut t, b"$ ");
    assert_eq!(row_text(&t, 3), "$");
}

#[test]
fn pending_wrap_counts_as_not_col0() {
    let (mut t, _) = term(6, 4, 0);
    feed(&mut t, b"abcd"); // cursor at the pending-wrap sentinel
    feed(&mut t, &osc("x"));
    assert_eq!(row_text(&t, 0), "abcd");
    assert_eq!(row_text(&t, 1), "x");
    assert_eq!((t.cursor().row, t.cursor().col), (2, 0));
}

#[test]
fn text_is_clipped_and_wide_chars_take_two_cells() {
    let (mut t, _) = term(4, 4, 0);
    // The fake wraps by char count, so "日本語" is one 3-char row: the
    // third wide char doesn't fit in 4 columns and is clipped.
    feed(&mut t, &osc("日本語"));
    let r = t.row(0);
    assert_eq!(r.cells()[0].ch, '日');
    assert!(r.cells()[1].is_continuation);
    assert_eq!(r.cells()[2].ch, '本');
    assert!(r.cells()[3].is_continuation);
    assert_eq!(r.cells().len(), 4);
}

#[test]
fn block_taller_than_screen_scrolls_into_scrollback() {
    let (mut t, _) = term(3, 10, 20);
    feed(&mut t, b"before\r\n");
    feed(&mut t, &osc("l0\nl1\nl2\nl3\nl4"));
    feed(&mut t, b"after");
    // Visible: l3, l4, after.
    assert_eq!(row_text(&t, 0), "l3");
    assert_eq!(row_text(&t, 1), "l4");
    assert_eq!(row_text(&t, 2), "after");
    let id = t.row(0).rich.unwrap().block;
    assert_eq!(tag(&t, 0), Some((id, 3)));
    // Scroll the viewport back: the history rows keep their tags.
    t.scroll_view_by(3, 0.0, 16.0);
    t.force_snap_view();
    let tags: Vec<_> = (0..3)
        .map(|r| t.view_row(r).rich.map(|x| x.index))
        .collect();
    // history = before, l0, l1, l2 ; view offset 3 shows l0..l2.
    assert_eq!(tags, vec![Some(0), Some(1), Some(2)]);
    assert_eq!(t.rich_block_view_span(id), Some((0, 2)));
    // Still renderable: the image is in the store.
    t.sync_rich_blocks();
    assert!(t.rich_blocks().image(id).is_some());
}

#[test]
fn printing_over_a_tagged_row_clears_its_tag() {
    let (mut t, _) = term(5, 10, 0);
    feed(&mut t, &osc("a\nb\nc"));
    feed(&mut t, b"\x1b[2;1HX");
    assert!(t.row(0).rich.is_some());
    assert!(t.row(1).rich.is_none(), "app overwrote row 1");
    assert!(t.row(2).rich.is_some());
}

#[test]
fn erase_clears_tags() {
    let (mut t, _) = term(6, 10, 0);
    feed(&mut t, &osc("a\nb\nc\nd"));
    feed(&mut t, b"\x1b[1;3H\x1b[K"); // EL 0 on row 0
    feed(&mut t, b"\x1b[2;1H\x1b[2X"); // ECH on row 1
    assert!(t.row(0).rich.is_none());
    assert!(t.row(1).rich.is_none());
    assert!(t.row(2).rich.is_some());
    feed(&mut t, b"\x1b[3;1H\x1b[J"); // ED 0 from row 2
    assert!(t.row(2).rich.is_none());
    assert!(t.row(3).rich.is_none());
}

#[test]
fn delete_and_insert_line_move_tags_with_rows() {
    let (mut t, _) = term(6, 10, 0);
    feed(&mut t, &osc("a\nb\nc"));
    let id = t.row(0).rich.unwrap().block;
    feed(&mut t, b"\x1b[1;1H\x1b[L"); // IL at row 0
    assert_eq!(tag(&t, 0), None);
    assert_eq!(tag(&t, 1), Some((id, 0)));
    assert_eq!(tag(&t, 3), Some((id, 2)));
    feed(&mut t, b"\x1b[1;1H\x1b[2M"); // DL 2 at row 0
    assert_eq!(tag(&t, 0), Some((id, 1)));
    assert_eq!(tag(&t, 4), None);
}

#[test]
fn gc_frees_unreferenced_blocks() {
    let (mut t, _) = term(4, 10, 0);
    feed(&mut t, &osc("a\nb"));
    assert_eq!(t.rich_blocks().len(), 1);
    let rev = t.rich_revision();
    feed(&mut t, b"\x1b[2J");
    assert!(t.sync_rich_blocks());
    assert!(t.rich_blocks().is_empty());
    assert_ne!(t.rich_revision(), rev);
}

#[test]
fn gc_frees_blocks_scrolled_out_of_history() {
    let (mut t, _) = term(2, 10, 2);
    feed(&mut t, &osc("a"));
    let id = t.rich_blocks().ids().next().unwrap();
    for _ in 0..6 {
        feed(&mut t, b"x\r\n");
    }
    t.sync_rich_blocks();
    assert!(t.rich_blocks().get(id).is_none());
}

#[test]
fn ris_clears_blocks() {
    let (mut t, _) = term(4, 10, 4);
    feed(&mut t, &osc("a"));
    feed(&mut t, b"\x1bc");
    t.sync_rich_blocks();
    assert!(t.rich_blocks().is_empty());
}

#[test]
fn eviction_falls_back_to_text() {
    let (mut t, _) = term(10, 10, 0);
    // Default cell 8x16: a 1-row block image is 80*16*4 = 5120 bytes.
    t.set_rich_budget(6000);
    feed(&mut t, &osc("one"));
    feed(&mut t, &osc("two"));
    let a = t.row(0).rich.unwrap().block;
    let b = t.row(1).rich.unwrap().block;
    assert!(t.rich_blocks().image(a).is_none(), "older evicted");
    assert!(t.rich_blocks().get(a).unwrap().is_evicted());
    assert!(t.rich_blocks().image(b).is_some());
    // The text is still in the cells for the fallback draw.
    assert_eq!(row_text(&t, 0), "one");
}

#[test]
fn text_only_layout_is_supported() {
    let mut t = Term::new(4, 10, 0);
    t.set_rich_layout(Box::new(Fake {
        no_image: true,
        ..Fake::default()
    }));
    feed(&mut t, &osc("plain"));
    let id = t.row(0).rich.unwrap().block;
    assert!(t.rich_blocks().image(id).is_none());
    assert_eq!(row_text(&t, 0), "plain");
}

#[test]
fn links_become_hyperlinks() {
    let (mut t, _) = term(4, 30, 0);
    feed(&mut t, b"\x1b]8;;https://first\x1b\\x\x1b]8;;\x1b\\\r\n");
    feed(&mut t, &osc("link:https://ex.com\nplain"));
    let r = t.row(1);
    let id = r.cells()[0].hyperlink_id.unwrap();
    assert_eq!(t.hyperlink_url(id), Some("https://ex.com"));
    assert!(r.cells()[18].hyperlink_id.is_some());
    assert!(r.cells()[19].hyperlink_id.is_none());
    assert!(t.row(2).cells()[0].hyperlink_id.is_none());
    // Shares the OSC 8 table (the earlier link keeps id 1).
    let first = t.row(0).cells()[0].hyperlink_id.unwrap();
    assert_eq!(t.hyperlink_url(first), Some("https://first"));
    assert_ne!(first, id);
}

#[test]
fn selection_copy_returns_rendered_text() {
    let (mut t, _) = term(5, 10, 0);
    feed(&mut t, &osc("Title\nbody text"));
    let top = t.bottom_id() - 4;
    let mut sel = Selection::new(Pos::new(top, 0), SelectionMode::Char);
    sel.set_active(Pos::new(top + 1, 9));
    t.set_selection(sel);
    assert_eq!(
        t.extract_selection_text().as_deref(),
        Some("Title\nbody text")
    );
}

/// Every retained primary row, oldest first.
fn all_lines(t: &Term) -> Vec<String> {
    let total = u64::from(t.history_lines()) + u64::from(t.size().0);
    let first = t.bottom_id() + 1 - total;
    (first..=t.bottom_id())
        .map(|id| {
            t.primary_row_by_line_id(id)
                .unwrap()
                .cells()
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .trim_end()
                .to_owned()
        })
        .collect()
}

fn visible_texts(t: &Term) -> Vec<String> {
    (0..t.size().0).map(|r| row_text(t, r)).collect()
}

#[test]
fn reflow_narrower_relayouts_block_and_keeps_neighbours() {
    let (mut t, fake) = term(10, 10, 20);
    feed(&mut t, b"above\r\n");
    feed(&mut t, &osc("abcdefgh\nxy"));
    feed(&mut t, b"below");
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    t.resize(10, 4);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let v = visible_texts(&t);
    // above (5 chars) rewraps to 2 rows; block → abcd, efgh, xy; below → 2.
    assert_eq!(&v[..7], &["abov", "e", "abcd", "efgh", "xy", "belo", "w"]);
    let id = t.row(2).rich.unwrap().block;
    assert_eq!(tag(&t, 2), Some((id, 0)));
    assert_eq!(tag(&t, 4), Some((id, 2)));
    assert_eq!(tag(&t, 5), None);
    assert_eq!(t.rich_blocks().get(id).unwrap().rows, 3);
    let img = t.rich_blocks().image(id).unwrap();
    assert_eq!((img.width, img.height), (32, 48));
    // Cursor stays after "below".
    assert_eq!((t.cursor().row, t.cursor().col), (6, 1));
}

#[test]
fn reflow_wider_relayouts_block() {
    let (mut t, _) = term(10, 4, 20);
    feed(&mut t, b"ab\r\n");
    feed(&mut t, &osc("abcdefgh"));
    feed(&mut t, b"cd");
    assert_eq!(visible_texts(&t)[..4], ["ab", "abcd", "efgh", "cd"]);
    t.resize(10, 10);
    let v = visible_texts(&t);
    assert_eq!(&v[..3], &["ab", "abcdefgh", "cd"]);
    assert!(t.row(1).rich.is_some());
    assert!(t.row(2).rich.is_none());
    assert_eq!((t.cursor().row, t.cursor().col), (2, 2));
}

#[test]
fn reflow_with_block_in_scrollback_keeps_order() {
    let (mut t, _) = term(3, 8, 50);
    feed(&mut t, b"one\r\n");
    feed(&mut t, &osc("aaaaaaaa\nbb"));
    for l in ["two", "three", "four"] {
        feed(&mut t, l.as_bytes());
        feed(&mut t, b"\r\n");
    }
    t.resize(3, 4);
    let all = all_lines(&t);
    let joined: Vec<&str> = all
        .iter()
        .map(String::as_str)
        .filter(|s| !s.is_empty())
        .collect();
    assert_eq!(
        joined,
        ["one", "aaaa", "aaaa", "bb", "two", "thre", "e", "four"]
    );
}

#[test]
fn vertical_only_resize_does_not_relayout() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abc"));
    t.resize(8, 10);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert!(t.row(0).rich.is_some());
}

#[test]
fn overwritten_block_is_not_relaid_out_on_reflow() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abcdef\nghijkl"));
    feed(&mut t, b"\x1b[1;1HZ");
    t.resize(6, 4);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "frozen block");
    // The surviving tagged row stays a tagged row (truncated).
    let tagged: Vec<_> = (0..6).filter(|r| t.row(*r).rich.is_some()).collect();
    assert_eq!(tagged.len(), 1);
}

#[test]
fn cell_size_change_relayouts_on_sync() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, b"x\r\n");
    feed(&mut t, &osc("abc\ndef"));
    feed(&mut t, b"y");
    t.set_cell_pixel_size(10, 20);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1, "lazy");
    assert!(t.sync_rich_blocks());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let id = t.row(1).rich.unwrap().block;
    let img = t.rich_blocks().image(id).unwrap();
    assert_eq!((img.width, img.height), (100, 40));
    assert_eq!(visible_texts(&t)[..4], ["x", "abc", "def", "y"]);
    // Nothing pending → no work.
    assert!(!t.sync_rich_blocks());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn font_size_change_relayouts_on_sync() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abc"));
    let env = t.rich_env();
    t.set_rich_env(RichEnv {
        font_px: 30.0,
        ..env
    });
    t.sync_rich_blocks();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert!((fake.last.lock().unwrap().as_ref().unwrap().3 - 30.0).abs() < f32::EPSILON);
}

#[test]
fn font_family_is_passed_and_a_change_relayouts_on_sync() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abc"));
    assert_eq!(fake_family(&fake), None, "unset by default");
    let mut env = t.rich_env();
    env.mono_family = Some("JetBrains Mono".into());
    t.set_rich_env(env.clone());
    assert!(t.sync_rich_blocks());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2, "re-laid out");
    assert_eq!(fake_family(&fake).as_deref(), Some("JetBrains Mono"));
    // Same env again: nothing to do.
    t.set_rich_env(env);
    assert!(!t.sync_rich_blocks());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    // New blocks and reflow carry it too.
    feed(&mut t, &osc("def"));
    assert_eq!(fake.calls.load(Ordering::SeqCst), 3);
    assert_eq!(fake_family(&fake).as_deref(), Some("JetBrains Mono"));
    t.resize(6, 12);
    assert!(fake.calls.load(Ordering::SeqCst) >= 4);
    assert_eq!(fake_family(&fake).as_deref(), Some("JetBrains Mono"));
}

fn fake_family(f: &Fake) -> Option<String> {
    f.last.lock().unwrap().as_ref().unwrap().6.clone()
}

#[test]
fn palette_change_recolors_and_keeps_rows() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abc\ndef"));
    let id = t.row(0).rich.unwrap().block;
    let rev = t.rich_blocks().block_revision(id).unwrap();
    let mut env = t.rich_env();
    env.palette.fg = [1, 2, 3];
    t.set_rich_env(env);
    assert!(t.sync_rich_blocks());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    assert_ne!(t.rich_blocks().block_revision(id).unwrap(), rev);
    let img = t.rich_blocks().image(id).unwrap();
    assert_eq!(&img.paint(0, 1).unwrap()[..4], &[1, 2, 3, 255]);
    assert_eq!(img.height, 32);
    assert_eq!(t.rich_blocks().get(id).unwrap().rows, 2);
}

#[test]
fn osc4_override_of_ansi_colour_triggers_recolor() {
    let (mut t, fake) = term(6, 10, 10);
    feed(&mut t, &osc("abc"));
    feed(&mut t, b"\x1b]4;200;rgb:ff/00/00\x1b\\");
    t.sync_rich_blocks();
    assert_eq!(
        fake.calls.load(Ordering::SeqCst),
        1,
        "index >= 16 is not used"
    );
    feed(&mut t, b"\x1b]4;1;rgb:ff/00/00\x1b\\");
    t.sync_rich_blocks();
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
}

#[test]
fn block_with_scroll_region_respects_linefeed() {
    let (mut t, _) = term(4, 10, 10);
    feed(&mut t, b"\x1b[4;1H");
    feed(&mut t, &osc("a\nb"));
    // Each row LF'd at the bottom: the screen scrolled twice.
    assert_eq!(row_text(&t, 1), "a");
    assert_eq!(row_text(&t, 2), "b");
    // `a`, `b` then cursor row: two rows scrolled into history.
    assert_eq!(t.history_lines(), 2);
    assert_eq!((t.cursor().row, t.cursor().col), (3, 0));
}

#[test]
fn block_text_is_written_one_grapheme_cluster_per_cell() {
    let (mut t, _) = term(3, 20, 0);
    let line = "x\u{2764}\u{fe0f}y \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}z e\u{301}!\u{200b}.";
    feed(&mut t, &osc(line));
    let cells = &t.row(0).cells();
    let shape: Vec<(char, bool, &str)> = cells[..12]
        .iter()
        .map(|c| (c.ch, c.is_continuation, t.grapheme_tail(c)))
        .collect();
    assert_eq!(
        shape,
        vec![
            ('x', false, ""),
            ('\u{2764}', false, "\u{fe0f}"),
            ('\0', true, ""),
            ('y', false, ""),
            (' ', false, ""),
            ('\u{1f468}', false, "\u{200d}\u{1f469}\u{200d}\u{1f467}"),
            ('\0', true, ""),
            ('z', false, ""),
            (' ', false, ""),
            ('e', false, "\u{301}"),
            ('!', false, ""),
            ('.', false, ""),
        ]
    );
    // Copy gives the clusters back (the zero-width space takes no cell
    // and is dropped).
    let top = t.bottom_id() - 2;
    let mut sel = Selection::new(Pos::new(top, 0), SelectionMode::Char);
    sel.set_active(Pos::new(top, 11));
    t.set_selection(sel);
    assert_eq!(
        t.extract_selection_text().as_deref(),
        Some("x\u{2764}\u{fe0f}y \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}z e\u{301}!.")
    );
}

#[test]
fn block_cluster_cells_matches_the_layout_engine() {
    // Same case table as `grapheme_cell_widths` in toastty-markdown
    // (`layout/grid.rs`): the two rules must agree or the cells drift
    // from the drawn glyphs. Keep both tables in sync.
    for (g, w) in [
        ("a", 1),
        ("\u{4e2d}", 2),
        ("e\u{301}", 1),
        ("\u{2764}\u{fe0f}", 2),
        ("\u{2764}", 1),
        ("\u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467}", 2),
        ("\u{1f1e8}\u{1f1e6}", 2),
        ("\u{200b}", 0),
        ("\u{301}", 0),
        ("\u{1b}", 0),
    ] {
        assert_eq!(super::block_cluster_cells(g), w, "{g:?}");
    }
}

/// Copy of the whole block row `r` (from the top of the screen).
fn copy_row(t: &mut Term, r: u16) -> Option<String> {
    let top = t.bottom_id() - u64::from(t.size().0 - 1) + u64::from(r);
    let mut sel = Selection::new(Pos::new(top, 0), SelectionMode::Char);
    sel.set_active(Pos::new(top, t.size().1 - 1));
    t.set_selection(sel);
    t.extract_selection_text()
}

#[test]
fn grapheme_tails_survive_resize_relayout() {
    let (mut t, fake) = term(4, 20, 10);
    let line = "e\u{301} \u{2764}\u{fe0f} \u{1f468}\u{200d}\u{1f469}\u{200d}\u{1f467} abcdefgh";
    feed(&mut t, &osc(line));
    let tails = |t: &Term, r: u16| -> Vec<String> {
        t.row(r)
            .cells()
            .iter()
            .map(|c| t.grapheme_tail(c).to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    };
    let want = ["\u{301}", "\u{fe0f}", "\u{200d}\u{1f469}\u{200d}\u{1f467}"];
    assert_eq!(tails(&t, 0), want);
    assert_eq!(copy_row(&mut t, 0).as_deref(), Some(line));
    // 16 cells: wraps at 12 (after "abcd"), clusters kept whole.
    t.resize(4, 12);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 2);
    let block = t.row(0).rich.unwrap().block;
    let rows: Vec<u16> = (0..4)
        .filter(|&r| t.row(r).rich.is_some_and(|x| x.block == block))
        .collect();
    assert_eq!(rows.len(), 2);
    assert_eq!(tails(&t, rows[0]), want);
    assert_eq!(row_text(&t, rows[1]), "efgh");
    // Back to 20: one row again, tails intact, copy round-trips.
    t.resize(4, 20);
    let r = (0..4)
        .find(|&r| t.row(r).rich.is_some_and(|x| x.block == block))
        .unwrap();
    assert_eq!(tails(&t, r), want);
    assert_eq!(copy_row(&mut t, r).as_deref(), Some(line));
}

fn one_run(text: &str) -> RichLayout {
    RichLayout {
        rows: 1,
        image: None,
        text: vec![vec![RichTextRun {
            col: 0,
            text: text.to_owned(),
        }]],
        links: vec![],
    }
}

#[test]
fn long_grapheme_tails_are_clipped_to_the_cap() {
    // 40 combining acutes (80 bytes) on one base: the tail keeps the
    // first 64 bytes (32 marks), on a char boundary.
    let marks = "\u{301}".repeat(40);
    let text = format!("e{marks}x\u{2764}{}", "\u{fe0f}".repeat(30));
    let mut extras = crate::extra::ExtraTable::default();
    let rows = super::build_block_rows(&one_run(&text), 1, 10, &mut vec![], &mut extras);
    let cells = &rows[0].cells();
    assert_eq!(cells[0].ch, 'e');
    let tail = extras.grapheme_tail(cells[0].extra);
    assert_eq!(tail.len(), crate::extra::MAX_GRAPHEME_EXTRA_BYTES);
    assert!(tail.chars().all(|c| c == '\u{301}'));
    assert_eq!(cells[1].ch, 'x');
    // 3-byte VS16s: clipped to 63 bytes (21 of them), never mid-char.
    assert_eq!(cells[2].ch, '\u{2764}');
    assert_eq!(extras.grapheme_tail(cells[2].extra), "\u{fe0f}".repeat(21));
    assert!(cells[3].is_continuation);
}

#[test]
fn wide_cluster_never_straddles_the_last_column() {
    let mut extras = crate::extra::ExtraTable::default();
    // 9 cells, then a 2-cell cluster that would need columns 9 and 10.
    let rows = super::build_block_rows(
        &one_run("123456789\u{2764}\u{fe0f}"),
        1,
        10,
        &mut vec![],
        &mut extras,
    );
    let cells = &rows[0].cells();
    assert_eq!(cells.len(), 10);
    assert_eq!(cells[8].ch, '9');
    assert_eq!(
        cells[9],
        crate::cell::Cell::BLANK,
        "dropped, not half drawn"
    );
    // It fits when it ends exactly on the last column.
    let rows = super::build_block_rows(
        &one_run("12345678\u{2764}\u{fe0f}"),
        1,
        10,
        &mut vec![],
        &mut extras,
    );
    let cells = &rows[0].cells();
    assert_eq!(cells[8].ch, '\u{2764}');
    assert_eq!(extras.grapheme_tail(cells[8].extra), "\u{fe0f}");
    assert!(cells[9].is_continuation);
}

#[test]
fn cell_size_relayout_moves_images_with_their_text() {
    // #31 review: the same-size reflow a rich-block re-layout does must
    // carry image placements along too.
    let mut t = Term::new(10, 20, 10);
    let fake = Fake {
        taller_when_wide: true,
        ..Fake::default()
    };
    t.set_rich_layout(Box::new(fake.clone()));
    t.set_cell_pixel_size(10, 20);
    feed(&mut t, &osc("abc"));
    feed(&mut t, b"y");
    let y_row = t.cursor().row;
    feed(
        &mut t,
        b"\x1b_Ga=T,f=32,s=1,v=1,i=1,c=2,r=1,C=1,q=2;/wAA/w==\x1b\\",
    );
    let p = t.image_grid().iter().next().unwrap().clone();
    assert_eq!(p.line - t.image_top_line(), i64::from(y_row));
    t.set_cell_pixel_size(12, 20);
    assert!(t.sync_rich_blocks());
    assert_eq!(row_text(&t, y_row + 1), "y", "the block grew by a row");
    let p = t.image_grid().iter().next().unwrap();
    assert_eq!(
        p.line - t.image_top_line(),
        i64::from(y_row) + 1,
        "image followed"
    );
}
