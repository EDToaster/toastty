//! Layout unit tests: per element row counts, text runs, link spans,
//! truncation, image policy and raster placement (with fake maths /
//! Mermaid backends).

use super::images::tests::{data_png, png};
use super::style::Palette;
use crate::{LayoutParams, Raster, RenderedBlock, Renderer, ResourcePolicy, Rgba, TextRun, Theme};

fn params(cols: u16) -> LayoutParams {
    LayoutParams {
        cols,
        cell_w: 10,
        cell_h: 22,
        font_px: 16.0,
        theme: Theme::default(),
        resources: ResourcePolicy::default(),
        max_rows: 2000,
    }
}

fn light() -> Theme {
    Theme {
        fg: [0x1f, 0x23, 0x28, 0xff],
        bg: [0xff; 4],
        ansi: Theme::default().ansi,
    }
}

thread_local! {
    static RENDERER: std::cell::RefCell<Renderer> = std::cell::RefCell::new(Renderer::hermetic());
}

fn render_with(md: &str, p: &LayoutParams) -> RenderedBlock {
    let b = RENDERER.with(|r| r.borrow_mut().render(md, p).expect("render"));
    check_invariants(&b, p);
    b
}

fn render(md: &str) -> RenderedBlock {
    render_with(md, &params(80))
}

fn check_invariants(b: &RenderedBlock, p: &LayoutParams) {
    assert!(b.rows >= 1);
    assert_eq!(b.text.len(), usize::from(b.rows));
    assert_eq!(b.image.width, p.width_px());
    assert_eq!(b.image.height, u32::from(b.rows) * p.cell_h);
    assert_eq!(
        b.image.pixels.len(),
        (b.image.width * b.image.height * 4) as usize
    );
    assert!(b.rows <= p.max_rows.max(1));
    for row in &b.text {
        let mut end = 0u16;
        for (i, r) in row.iter().enumerate() {
            assert_ne!(r.text, "");
            if i > 0 {
                assert!(r.col >= end, "overlapping runs in {row:?}");
            }
            end = r.col + unicode_width::UnicodeWidthStr::width(r.text.as_str()) as u16;
        }
    }
    for l in &b.links {
        assert!(l.row < b.rows);
        assert!(l.cols.start < l.cols.end && l.cols.end <= p.cols);
    }
}

/// All text in reading order, one entry per row (runs joined).
fn lines(b: &RenderedBlock) -> Vec<String> {
    b.text
        .iter()
        .map(|r| {
            r.iter()
                .map(|t| t.text.as_str())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|s| !s.is_empty())
        .collect()
}

fn px(b: &RenderedBlock, x: u32, y: u32) -> Rgba {
    let i = ((y * b.image.width + x) * 4) as usize;
    b.image.pixels[i..i + 4].try_into().unwrap()
}

fn count_color(b: &RenderedBlock, c: Rgba, tol: i32) -> usize {
    b.image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .filter(|p| p[3] == 255 && (0..3).all(|i| (i32::from(p[i]) - i32::from(c[i])).abs() <= tol))
        .count()
}

fn with_backends<T>(
    math: super::MathFn,
    mermaid: super::MermaidFn,
    f: impl FnOnce(&mut Renderer) -> T,
) -> T {
    let mut r = Renderer::hermetic();
    r.cache.math = math;
    r.cache.mermaid = super::MermaidBackend::Fake(mermaid);
    f(&mut r)
}

#[test]
fn empty_document_is_one_blank_row() {
    let b = render("");
    assert_eq!(b.rows, 1);
    assert_eq!(b.text[0], vec![]);
    assert!(b.image.pixels.iter().all(|&v| v == 0));
}

#[test]
fn zero_sized_params_are_rejected() {
    let mut r = Renderer::hermetic();
    let mut p = params(0);
    assert!(r.render("x", &p).is_err());
    p.cols = 10;
    p.cell_h = 0;
    assert!(r.render("x", &p).is_err());
    let mut p = params(10);
    p.font_px = f32::NAN;
    assert!(r.render("x", &p).is_err());
}

#[test]
fn paragraph_text_runs() {
    let b = render("Hello *world*, this is `code`.");
    assert_eq!(b.rows, 1);
    assert_eq!(
        b.text[0],
        vec![TextRun {
            col: 1,
            text: "Hello world, this is code.".into()
        }]
    );
}

#[test]
fn paragraph_wraps_to_width_and_keeps_words() {
    let src = "The quick brown fox jumps over the lazy dog and keeps running far away.";
    let b = render_with(src, &params(20));
    assert!(b.rows >= 4, "rows {}", b.rows);
    let joined = lines(&b).join(" ");
    assert_eq!(joined, src);
    // Soft breaks become spaces.
    let b = render("one\ntwo");
    assert_eq!(lines(&b), vec!["one two"]);
    // Hard breaks start a new row.
    let b = render("one  \ntwo");
    assert_eq!(lines(&b), vec!["one", "two"]);
}

#[test]
fn long_word_is_split() {
    let b = render_with(&"x".repeat(200), &params(20));
    assert!(b.rows >= 7);
    assert_eq!(lines(&b).concat(), "x".repeat(200));
}

#[test]
fn headings_have_real_sizes() {
    let h1 = render("# Title");
    let h3 = render("### Title");
    let p = render("Title");
    assert_eq!(lines(&h1), vec!["Title"]);
    assert!(
        h1.rows > h3.rows || h1.rows >= 2,
        "h1 {} h3 {}",
        h1.rows,
        h3.rows
    );
    assert!(h3.rows >= p.rows);
    // h1 is about twice as tall as body text: measure ink height.
    let ink = |b: &RenderedBlock| {
        let rows: Vec<u32> = (0..b.image.height)
            .filter(|&y| (0..b.image.width).any(|x| px(b, x, y)[3] > 128))
            .collect();
        rows.last().unwrap() - rows.first().unwrap()
    };
    let (a, c) = (ink(&render("# Hxg")), ink(&render("Hxg")));
    assert!(a as f32 > c as f32 * 1.6, "h1 ink {a} body ink {c}");
    // Setext headings too, and h1/h2 get an underline rule.
    let b = render("Title\n=====\n");
    assert_eq!(lines(&b), vec!["Title"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&h1, pal.border, 2) > 400);
    assert_eq!(count_color(&h3, pal.border, 2), 0);
}

#[test]
fn unordered_ordered_and_nested_lists() {
    let b = render("- a\n- b\n  - c\n    - d\n");
    let t = &b.text;
    assert_eq!(lines(&b), vec!["- a", "- b", "- c", "- d"]);
    let col = |r: usize| t[r][0].col;
    assert!(col(2) > col(1) && col(3) > col(2), "{t:?}");
    let b = render("3. x\n4. y\n");
    assert_eq!(lines(&b), vec!["3. x", "4. y"]);
    // Hanging indent: wrapped lines start under the text, not the bullet.
    let b = render_with("- aaaa bbbb cccc dddd eeee ffff gggg hhhh", &params(20));
    assert!(b.rows >= 2);
    assert!(b.text[1][0].col > b.text[0][0].col);
}

#[test]
fn loose_lists_have_gaps() {
    let tight = render("- a\n- b\n");
    let loose = render("- a\n\n- b\n");
    assert!(loose.rows > tight.rows || loose.text.iter().filter(|r| r.is_empty()).count() > 0);
}

#[test]
fn task_lists_draw_checkboxes() {
    let b = render("- [x] done\n- [ ] todo\n");
    assert_eq!(lines(&b), vec!["[x] done", "[ ] todo"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&b, pal.checkbox_on, 2) > 50);
}

#[test]
fn block_quotes_prefix_and_nest() {
    let b = render("> quoted\n>\n> > deeper\n");
    assert_eq!(lines(&b), vec!["> quoted", "> > deeper"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&b, pal.quote_bar, 2) > 40);
    // Alerts get a title.
    let b = render("> [!NOTE]\n> Read this.\n");
    assert_eq!(lines(&b), vec!["> Note", "> Read this."]);
}

#[test]
fn code_blocks_are_verbatim_on_a_panel() {
    let src = "```\nfn  x() {\n    y\n}\n```\n";
    let b = render(src);
    assert_eq!(lines(&b), vec!["fn  x() {", "    y", "}"]);
    assert!(b.rows >= 4);
    let pal = Palette::new(&Theme::default());
    // Panel is opaque; outside it is transparent.
    assert_eq!(px(&b, 30, 3), pal.code_bg);
    assert_eq!(px(&b, 2, 3)[3], 0);
    // Indented lines keep their leading spaces, from the same column.
    assert_eq!(b.text[2][0].col, b.text[1][0].col);
    assert_eq!(b.text[2][0].text, "    y");
    // Indented code blocks too.
    let b = render("    indented\n    code\n");
    assert_eq!(lines(&b), vec!["indented", "code"]);
}

#[test]
fn code_is_highlighted_from_the_palette() {
    let colors = |b: &RenderedBlock| {
        let mut set = std::collections::HashSet::new();
        for p in b.image.pixels.as_chunks::<4>().0 {
            if p[3] == 255 {
                set.insert([p[0], p[1], p[2]]);
            }
        }
        set.len()
    };
    let src = "fn main() { let s = \"x\"; // c\n}\n";
    let hl = render(&format!("```rust\n{src}```\n"));
    let plain = render(&format!("```nope\n{src}```\n"));
    assert_eq!(lines(&hl), lines(&plain));
    assert!(
        colors(&hl) > colors(&plain) + 20,
        "{} vs {}",
        colors(&hl),
        colors(&plain)
    );
    let pal = Palette::new(&Theme::default());
    assert!(
        count_color(&hl, pal.syntax(2), 0) > 0,
        "strings use ANSI green"
    );
}

#[test]
fn long_code_lines_wrap() {
    let b = render_with(&format!("```\n{}\n```\n", "word ".repeat(30)), &params(30));
    assert!(lines(&b).len() >= 4);
}

#[test]
fn tables_join_cells_and_shade_the_header() {
    let b = render("| a | b |\n|:--|--:|\n| 1 | 2 |\n| 3 | 4 |\n");
    assert_eq!(lines(&b), vec!["a | b", "1 | 2", "3 | 4"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&b, pal.table_head_bg, 0) > 100);
    assert!(count_color(&b, pal.border, 0) > 100);
}

#[test]
fn table_column_alignment() {
    // A right-aligned column puts short text at the right edge.
    let b = render("| left | right |\n|:--|--:|\n| x | y |\n| wide wide wide | wide wide wide |\n");
    let row = b
        .text
        .iter()
        .find(|r| r.iter().any(|t| t.text.starts_with("x |")))
        .unwrap();
    assert_eq!(row.len(), 1);
    // Pixels: the "y" cell ink sits right, the "x" cell ink left.
    let y_row = (0..b.rows).find(|&r| b.text[r as usize] == *row).unwrap();
    let y0 = u32::from(y_row) * 22;
    let ink_x: Vec<u32> = (0..b.image.width)
        .filter(|&x| (y0..y0 + 22).any(|y| px(&b, x, y)[3] > 200 && px(&b, x, y)[0] > 150))
        .collect();
    assert_ne!(ink_x, Vec::<u32>::new());
}

#[test]
fn wide_tables_wrap_cells() {
    let src = "| a | b |\n|---|---|\n| one two three four five six seven | eight nine ten eleven twelve |\n";
    let wide = render_with(src, &params(100));
    let narrow = render_with(src, &params(30));
    assert!(narrow.rows > wide.rows);
    let all = lines(&narrow).join(" ");
    for w in ["one", "seven", "eight", "twelve"] {
        assert!(all.contains(w), "{w} missing from {all}");
    }
}

#[test]
fn horizontal_rule() {
    let b = render("a\n\n---\n\nb");
    assert_eq!(lines(&b), vec!["a", "b"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&b, pal.rule, 0) > 700);
}

#[test]
fn link_spans_cover_link_text() {
    let b = render("see [the docs](https://e.com/d) now");
    assert_eq!(lines(&b), vec!["see the docs now"]);
    assert_eq!(b.links.len(), 1);
    let l = &b.links[0];
    assert_eq!(l.url, "https://e.com/d");
    assert_eq!(l.row, 0);
    // "see " is ~3 cells of proportional text after the 1-cell pad.
    assert!(l.cols.start >= 3 && l.cols.start <= 5, "{l:?}");
    assert!(
        l.cols.end - l.cols.start >= 5 && l.cols.end - l.cols.start <= 9,
        "{l:?}"
    );
    // Autolinks and reference links.
    let b = render("<https://x.org> and [r]\n\n[r]: https://r.org\n");
    let urls: Vec<&str> = b.links.iter().map(|l| l.url.as_str()).collect();
    assert_eq!(urls, vec!["https://x.org", "https://r.org"]);
    // A link wrapping over two rows gives one span per row.
    let b = render_with("[a very long link text that wraps](u)", &params(14));
    let rows: std::collections::BTreeSet<u16> = b.links.iter().map(|l| l.row).collect();
    assert!(rows.len() >= 2);
    assert!(b.links.iter().all(|l| l.url == "u"));
}

#[test]
fn truncation_respects_max_rows() {
    let src = (0..200)
        .map(|i| format!("para {i} [l](u{i})"))
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut p = params(40);
    p.max_rows = 10;
    let b = render_with(&src, &p);
    assert_eq!(b.rows, 10);
    assert_eq!(
        b.text[9],
        vec![TextRun {
            col: 1,
            text: "\u{2026} truncated".into()
        }]
    );
    assert!(b.links.iter().all(|l| l.row < 9));
    assert!(lines(&b)[0].starts_with("para 0"));
    // max_rows 0 is treated as 1.
    p.max_rows = 0;
    let b = render_with("a\n\nb", &p);
    assert_eq!(b.rows, 1);
}

#[test]
fn huge_documents_stay_bounded() {
    let src = "word ".repeat(400_000);
    let mut p = params(80);
    p.max_rows = 50;
    let t = std::time::Instant::now();
    let b = render_with(&src, &p);
    assert_eq!(b.rows, 50);
    assert!(t.elapsed().as_secs() < 30);
    let src = "```\n".to_owned() + &"line\n".repeat(100_000) + "```\n";
    let b = render_with(&src, &p);
    assert_eq!(b.rows, 50);
}

#[test]
fn data_uri_images_render_scaled_to_fit() {
    let red = [220, 20, 20, 255];
    let b = render(&format!("![alt]({})", data_png(40, 20, red)));
    assert!(
        count_color(&b, red, 3) >= 600,
        "{}",
        count_color(&b, red, 3)
    );
    assert!(lines(&b).concat().contains("alt"));
    // Wider than the block: scaled down to the content width.
    let b = render_with(
        &format!("![alt]({})", data_png(2000, 100, red)),
        &params(40),
    );
    let reds: Vec<u32> = (0..b.image.width)
        .filter(|&x| (0..b.image.height).any(|y| px(&b, x, y)[0] > 200 && px(&b, x, y)[3] == 255))
        .collect();
    assert!(
        reds.len() as u32 >= 370 && reds.len() as u32 <= 381,
        "{}",
        reds.len()
    );
    assert!(*reds.last().unwrap() < 400);
}

#[test]
fn image_policy_local_and_remote() {
    let dir = std::env::temp_dir().join(format!("toastty-md-layout-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let blue = [20, 40, 230, 255];
    std::fs::write(dir.join("pic.png"), png(30, 30, blue)).unwrap();
    let md = "![a picture](pic.png)";
    // Denied by default: placeholder with alt text, no pixels.
    let b = render(md);
    assert_eq!(lines(&b), vec!["a picture"]);
    assert_eq!(count_color(&b, blue, 3), 0);
    // Allowed and resolved against base_dir.
    let mut p = params(80);
    p.resources = ResourcePolicy {
        allow_local_files: true,
        base_dir: Some(dir.clone()),
    };
    let b = render_with(md, &p);
    assert!(count_color(&b, blue, 3) >= 800);
    // Remote is never fetched, even with local files allowed.
    let b = render_with("![remote](https://example.com/pic.png)", &p);
    assert_eq!(lines(&b), vec!["remote"]);
    // Missing file: placeholder.
    let b = render_with("![gone](nope.png)", &p);
    assert_eq!(lines(&b), vec!["gone"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn maths_falls_back_to_inline_code() {
    let b = render("Euler: $e^{i\\pi}+1=0$ done");
    assert_eq!(lines(&b), vec!["Euler: $e^{i\\pi}+1=0$ done"]);
    let b = render("$$\n\\int_0^1 x\\,dx\n$$\n");
    assert_eq!(lines(&b), vec!["$$\\int_0^1 x\\,dx$$"]);
    let b = render("```math\na^2+b^2\n```\n");
    assert_eq!(lines(&b), vec!["$$a^2+b^2$$"]);
    // Invalid TeX (unbalanced group) falls back to the code-styled source.
    let b = render("```math\n\\frac{a\n```\n");
    assert_eq!(lines(&b), vec!["$$\\frac{a$$"]);
    let pal = Palette::new(&Theme::default());
    assert!(count_color(&b, pal.inline_code_bg, 0) > 100);
}

#[test]
fn mermaid_falls_back_to_source_code_block() {
    // Not a diagram type mermaid knows: shown as a code block of the source.
    let b = render("```mermaid\nnotadiagram\n  A-->B\n```\n");
    assert_eq!(lines(&b), vec!["notadiagram", "  A-->B"]);
    let pal = Palette::new(&Theme::default());
    assert_eq!(px(&b, 30, 4), pal.code_bg);
}

#[test]
fn mermaid_diagram_renders_as_image() {
    let b = render("```mermaid\ngraph TD\n  A-->B\n```\n");
    // The source still sits under the block for copy/search.
    assert_eq!(&lines(&b)[..2], &["graph TD", "  A-->B"]);
    // Drawn as a diagram, not a code panel, and taller than the 2 lines.
    let pal = Palette::new(&Theme::default());
    assert_ne!(px(&b, 30, 4), pal.code_bg);
    assert!(b.rows > 4, "rows = {}", b.rows);
    let ink = b.image.pixels.chunks_exact(4).filter(|p| p[3] > 0).count();
    assert!(ink > 500, "diagram has ink: {ink}");
}

const RED: Rgba = [255, 0, 0, 255];

fn solid(w: u32, h: u32, baseline: Option<u32>) -> Raster {
    Raster {
        width: w,
        height: h,
        pixels: RED.repeat((w * h) as usize),
        baseline,
    }
}

fn fake_math(tex: &str, display: bool, font_px: f32, _c: Rgba) -> Result<Raster, String> {
    if tex == "bad" {
        return Err("parse error".into());
    }
    let h = (font_px * 1.5) as u32;
    if display {
        Ok(solid(120, 60, None))
    } else {
        Ok(solid(30, h, Some(h - 8)))
    }
}

fn fake_mermaid(_src: &str, _max_w: u32, _f: f32, _t: &Theme) -> Result<Raster, String> {
    Ok(solid(2000, 400, None))
}

fn red_bbox(b: &RenderedBlock) -> (u32, u32, u32, u32) {
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0, 0);
    for y in 0..b.image.height {
        for x in 0..b.image.width {
            let p = px(b, x, y);
            if p[3] == 255 && p[0] == 255 && p[1] < 10 {
                x0 = x0.min(x);
                y0 = y0.min(y);
                x1 = x1.max(x);
                y1 = y1.max(y);
            }
        }
    }
    (x0, y0, x1, y1)
}

#[test]
fn inline_maths_raster_sits_on_the_baseline() {
    with_backends(fake_math, fake_mermaid, |r| {
        let p = params(80);
        let b = r.render("ab $x$ cd", &p).unwrap();
        check_invariants(&b, &p);
        assert_eq!(lines(&b), vec!["ab $x$ cd"]);
        let (x0, y0, x1, y1) = red_bbox(&b);
        assert_eq!(x1 - x0 + 1, 30);
        assert_eq!(y1 - y0 + 1, 24);
        // The raster's baseline (16 px below its top) matches the text
        // baseline: find the bottom of the "b" ink (no descender).
        let text_bottom = (0..b.image.height)
            .rev()
            .find(|&y| (10..x0.saturating_sub(2)).any(|x| px(&b, x, y)[3] > 128))
            .unwrap();
        let raster_base = y0 + 16;
        assert!(
            (i64::from(text_bottom) - i64::from(raster_base)).abs() <= 1,
            "text {text_bottom} raster {raster_base}"
        );
        // The line grew to fit the raster: row pitch still >= cell.
        assert!(b.rows >= 1);
        // A parse failure falls back to code.
        let b = r.render("ab $bad$ cd", &p).unwrap();
        assert_eq!(red_bbox(&b).0, u32::MAX);
        assert_eq!(lines(&b), vec!["ab $bad$ cd"]);
    });
}

#[test]
fn display_maths_and_diagrams_are_centered_and_fit() {
    with_backends(fake_math, fake_mermaid, |r| {
        let p = params(80);
        let b = r.render("$$\nx\n$$\n", &p).unwrap();
        check_invariants(&b, &p);
        let (x0, y0, x1, y1) = red_bbox(&b);
        assert_eq!((x1 - x0 + 1, y1 - y0 + 1), (120, 60));
        let center = x0.midpoint(x1);
        assert!((i64::from(center) - 400).abs() <= 2, "center {center}");
        assert_eq!(lines(&b), vec!["$$x$$"]);
        // Display maths in the middle of a paragraph gets its own line.
        let b = r.render("before $$x$$ after", &p).unwrap();
        assert_eq!(lines(&b), vec!["before", "$$x$$", "after"]);
        // Fenced maths.
        let b = r.render("```math\nx\n```", &p).unwrap();
        assert_eq!(red_bbox(&b).2 - red_bbox(&b).0 + 1, 120);

        let b = r.render("```mermaid\ngraph TD\nA-->B\n```\n", &p).unwrap();
        check_invariants(&b, &p);
        let (x0, y0, x1, y1) = red_bbox(&b);
        assert_eq!(x1 - x0 + 1, 780, "scaled to the content width");
        assert_eq!(y1 - y0 + 1, 156);
        assert_eq!(lines(&b), vec!["graph TD", "A-->B"]);
    });
}

#[test]
fn raw_html_and_footnotes_are_text() {
    let b = render("<div class=\"x\">hi</div>\n\nInline <b>bold</b>.");
    assert_eq!(
        lines(&b),
        vec!["<div class=\"x\">hi</div>", "Inline <b>bold</b>."]
    );
    let b = render("Claim[^1].\n\n[^1]: The source.\n");
    let l = lines(&b);
    assert_eq!(l[0], "Claim[1].");
    assert!(
        l[1].contains("[1]") && l[1].contains("The source."),
        "{l:?}"
    );
}

#[test]
fn straight_alpha_glyph_edges_keep_their_colour() {
    let b = render("Hello");
    let fg = Theme::default().fg;
    let edge = b
        .image
        .pixels
        .as_chunks::<4>()
        .0
        .iter()
        .find(|p| p[3] > 40 && p[3] < 200)
        .expect("anti-aliased pixel");
    for i in 0..3 {
        assert!(
            (i32::from(edge[i]) - i32::from(fg[i])).abs() <= 3,
            "{edge:?} vs {fg:?}"
        );
    }
}

#[test]
fn light_theme_and_narrow_widths_render() {
    let src = include_str!("../../tests/golden/kitchen_sink.md");
    for cols in [12, 40, 80, 200] {
        for theme in [Theme::default(), light()] {
            let mut p = params(cols);
            p.theme = theme;
            let b = render_with(src, &p);
            if cols >= 40 {
                assert!(lines(&b).iter().any(|l| l.contains("Toastty Markdown")));
            } else {
                assert!(lines(&b).concat().contains("Toast"));
            }
        }
    }
    // Tiny widths never panic.
    for cols in 1..6 {
        let _ = render_with(src, &params(cols));
    }
}

#[test]
fn reading_order_is_preserved() {
    let src = include_str!("../../tests/golden/kitchen_sink.md");
    let b = render(src);
    let all = lines(&b).join("\n");
    let order = [
        "Toastty Markdown",
        "Features",
        "- Headings",
        "Tasks",
        "[x] parse",
        "> Block quotes",
        "fn main()",
        "Element | Status | Notes",
        "Smaller heading",
    ];
    let mut at = 0;
    for w in order {
        let i = all[at..]
            .find(w)
            .unwrap_or_else(|| panic!("{w} out of order in\n{all}"));
        at += i;
    }
}

#[test]
fn hostile_inputs_do_not_panic() {
    for src in [
        "> ".repeat(2000),
        "- ".repeat(2000),
        "[".repeat(5000),
        "|a|\n|-|\n".to_owned() + &"|x|\n".repeat(3000),
        "$".repeat(3000),
        "\u{0}\u{7}\t\u{200b}\u{fe0f}é\u{301} 日本語のテキスト 🎉 مرحبا".to_owned(),
        "![x](data:image/png;base64,!!!!)".to_owned(),
        format!("![x](data:image/png;base64,{})", "A".repeat(10_000)),
    ] {
        let mut p = params(40);
        p.max_rows = 200;
        let _ = render_with(&src, &p);
    }
}

/// Fake maths raster: a rounded, anti-aliased "formula" blob whose size
/// tracks the source length, with a baseline for inline use.
fn blob_math(tex: &str, display: bool, font_px: f32, c: Rgba) -> Result<Raster, String> {
    if tex.contains("bad") {
        return Err("parse error".into());
    }
    let w = (tex.len() as f32 * font_px * 0.5).max(font_px) as u32;
    let h = if display {
        (font_px * 2.6) as u32
    } else {
        (font_px * 1.3) as u32
    };
    let mut pixels = vec![0u8; (w * h * 4) as usize];
    for y in 0..h {
        for x in 0..w {
            let edge = x.min(w - 1 - x).min(y).min(h - 1 - y);
            let a = if edge == 0 { 90 } else { 255 };
            let i = ((y * w + x) * 4) as usize;
            pixels[i..i + 4].copy_from_slice(&[c[0], c[1] / 2, c[2], a]);
        }
    }
    Ok(Raster {
        width: w,
        height: h,
        pixels,
        baseline: (!display).then_some(h * 3 / 4),
    })
}

/// Opt-in visual dump: `MD_DUMP_DIR=/tmp/md-core cargo test -p toastty-markdown dump_`.
#[test]
fn dump_feature_renders() {
    let Some(dir) = std::env::var_os("MD_DUMP_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = format!(
        "## Maths and diagrams\n\nInline $a^2+b^2=c^2$ sits on the baseline, $bad$ falls back.\n\n$$\n\\sum_{{i=0}}^n i\n$$\n\n```mermaid\ngraph TD\n  A-->B\n```\n\n\
> [!WARNING]\n> Alerts have a coloured title.\n\n> [!TIP]\n> Tips too.\n\n\
| Name | Description |\n|---|---|\n| wrap | a long description that has to wrap inside its table cell because the table is far too wide |\n| short | ok |\n\n\
Footnote reference[^n] and an ![remote image](https://x/y.png) and a local ![file](a.png) and ![data]({}).\n\n[^n]: The footnote text.\n\n\
1. one\n2. two\n   - nested *italic* and ~~struck~~\n10. ten\n\n<details>raw html</details>\n",
        super::images::tests::data_png(64, 24, [80, 160, 240, 255])
    );
    for (name, theme) in [("dark", Theme::default()), ("light", light())] {
        for cols in [80u16, 40] {
            let mut p = params(cols);
            p.theme = theme.clone();
            let b = with_backends(
                blob_math,
                |s, w, f, t| {
                    let _ = (s, f);
                    let (w, h) = (w.min(500), 160);
                    let mut px = vec![0u8; (w * h * 4) as usize];
                    for (i, p) in px.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                        let (x, y) = (i as u32 % w, i as u32 / w);
                        let border = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
                        *p = if border {
                            t.fg
                        } else {
                            [t.ansi[4][0], t.ansi[4][1], t.ansi[4][2], 60]
                        };
                    }
                    Ok(Raster {
                        width: w,
                        height: h,
                        pixels: px,
                        baseline: None,
                    })
                },
                |r| r.render(&src, &p).unwrap(),
            );
            let mut px = b.image.pixels.clone();
            for q in px.as_chunks_mut::<4>().0 {
                let a = u32::from(q[3]);
                for (c, bg) in q[..3].iter_mut().zip(theme.bg) {
                    *c = ((u32::from(*c) * a + u32::from(bg) * (255 - a)) / 255) as u8;
                }
                q[3] = 255;
            }
            image::RgbaImage::from_raw(b.image.width, b.image.height, px)
                .unwrap()
                .save(dir.join(format!("features_{name}_{cols}.png")))
                .unwrap();
            let txt: Vec<String> = b.text.iter().map(|r| format!("{r:?}")).collect();
            std::fs::write(
                dir.join(format!("features_{name}_{cols}.txt")),
                txt.join("\n"),
            )
            .unwrap();
        }
    }
}

#[test]
fn colour_changes_keep_the_row_count() {
    let src = include_str!("../../tests/golden/kitchen_sink.md");
    let dark = render(src);
    let mut p = params(80);
    p.theme = light();
    let lt = render_with(src, &p);
    assert_eq!(dark.rows, lt.rows);
    assert_eq!(dark.text, lt.text);
    assert_eq!(dark.links, lt.links);
    assert_ne!(dark.image, lt.image);
}

#[test]
fn links_in_tables_and_linked_images() {
    let b = render("| site |\n|---|\n| [home](https://h) |\n");
    assert_eq!(b.links.len(), 1);
    assert_eq!(b.links[0].url, "https://h");
    let row = usize::from(b.links[0].row);
    assert_eq!(b.text[row][0].text, "home");
    let img = data_png(20, 10, [1, 200, 1, 255]);
    let b = render(&format!("[![pic]({img})](https://target)"));
    assert_eq!(b.links.len(), 1);
    assert_eq!(b.links[0].url, "https://target");
    assert!(b.links[0].cols.end - b.links[0].cols.start >= 2);
}

#[test]
fn hidpi_and_tall_cells_scale_the_layout() {
    let src = "# Title\n\nBody text that wraps a little bit.\n\n- item\n";
    let lo = render_with(src, &params(40));
    let mut p = params(40);
    p.cell_w = 20;
    p.cell_h = 44;
    p.font_px = 32.0;
    let hi = render_with(src, &p);
    assert_eq!(lo.rows, hi.rows);
    assert_eq!(lines(&lo), lines(&hi));
    // Taller cells than the font needs: one body line per row still.
    let mut p = params(40);
    p.cell_h = 30;
    let b = render_with("a  \nb  \nc", &p);
    assert_eq!(b.rows, 3);
}

#[test]
fn oversized_inline_rasters_are_scaled_to_fit() {
    fn huge(_: &str, _: bool, _: f32, _: Rgba) -> Result<Raster, String> {
        Ok(solid(3000, 50, Some(40)))
    }
    with_backends(huge, fake_mermaid, |r| {
        let p = params(40);
        let b = r.render("$x$", &p).unwrap();
        check_invariants(&b, &p);
        let (x0, _, x1, _) = red_bbox(&b);
        assert!(x1 - x0 < 380 && x1 < 400);
    });
}

#[test]
fn bogus_backend_rasters_fall_back() {
    fn short(_: &str, _: bool, _: f32, _: Rgba) -> Result<Raster, String> {
        Ok(Raster {
            width: 10,
            height: 10,
            pixels: vec![0; 4],
            baseline: None,
        })
    }
    fn empty(_: &str, _: u32, _: f32, _: &Theme) -> Result<Raster, String> {
        Ok(Raster {
            width: 0,
            height: 0,
            pixels: vec![],
            baseline: None,
        })
    }
    with_backends(short, empty, |r| {
        let p = params(40);
        let b = r.render("$x$\n\n```mermaid\ngraph\n```", &p).unwrap();
        assert_eq!(lines(&b), vec!["$x$", "graph"]);
    });
}

#[test]
fn system_renderer_smoke() {
    // Works with or without system fonts (bundled fallbacks).
    let mut r = Renderer::new();
    let b = r.render("Hello **world** `code`", &params(40)).unwrap();
    assert_eq!(b.rows, 1);
    assert_eq!(lines(&b), vec!["Hello world code"]);
}
