//! `Renderer::layout` + `Renderer::paint` must be indistinguishable from
//! `Renderer::render`: painting a block band by band (as the terminal
//! does, one GPU tile at a time) gives exactly the full image, and the
//! width-independent caches never change the output.

use toastty_markdown::{LayoutParams, Renderer, ResourcePolicy, Theme};

const DEMO: &str = include_str!("golden/demo.md");
const KITCHEN_SINK: &str = include_str!("golden/kitchen_sink.md");

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

/// 1 px strokes go through tiny-skia's fixed-point hairline rasterizer,
/// whose coverage can move by a few levels with the band offset. Nothing
/// else may differ at all.
const HAIRLINE_TOLERANCE: u8 = 4;

/// `None` when `a` matches `b` (within the hairline tolerance), else a
/// description of the first differing rows.
fn mismatch(a: &[u8], b: &[u8], width: u32) -> Option<String> {
    if a.len() != b.len() {
        return Some(format!("length {} vs {}", a.len(), b.len()));
    }
    let mut rows = std::collections::BTreeMap::<u32, (u32, u8)>::new();
    for (i, (x, y)) in a.iter().zip(b).enumerate() {
        let d = x.abs_diff(*y);
        if d > HAIRLINE_TOLERANCE {
            let e = rows.entry((i / 4) as u32 / width).or_default();
            e.0 += 1;
            e.1 = e.1.max(d);
        }
    }
    (!rows.is_empty()).then(|| {
        format!(
            "{} rows differ (row: (bytes, max diff)): {:?}",
            rows.len(),
            rows.iter().take(8).collect::<Vec<_>>()
        )
    })
}

/// Paint `layout` in bands of `band` px and stitch them together.
fn stitched(r: &mut Renderer, l: &toastty_markdown::BlockLayout, band: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((l.width * l.height * 4) as usize);
    let mut y = 0;
    while y < l.height {
        let px = r.paint(l, y, band);
        assert_eq!(px.width, l.width);
        assert_eq!(px.height, band.min(l.height - y));
        out.extend_from_slice(&px.pixels);
        y += band;
    }
    out
}

#[test]
fn bands_stitch_to_the_full_render() {
    for (src, cols) in [
        (DEMO, 90),
        (DEMO, 43),
        (KITCHEN_SINK, 80),
        (KITCHEN_SINK, 40),
    ] {
        let p = params(cols);
        let mut r = Renderer::hermetic();
        let full = r.render(src, &p).unwrap();
        let l = r.layout(src, &p).unwrap();
        assert_eq!(l.rows, full.rows);
        assert_eq!((l.width, l.height), (full.image.width, full.image.height));
        assert_eq!(l.text(), full.text.as_slice());
        assert_eq!(l.links(), full.links.as_slice());
        // 16-row GPU tiles, and an awkward band that splits rows.
        for band in [16 * 22, 37] {
            let st = stitched(&mut r, &l, band);
            if let Some(m) = mismatch(&st, &full.image.pixels, l.width) {
                panic!("cols {cols}, band {band}: {m}");
            }
        }
    }
}

#[test]
fn warm_caches_do_not_change_the_output() {
    let p = params(90);
    let cold = Renderer::hermetic().render(DEMO, &p).unwrap();
    let mut warm = Renderer::hermetic();
    // A resize sweep through other widths, then back.
    for cols in [120, 64, 40, 77, 90, 41] {
        warm.layout(DEMO, &params(cols)).unwrap();
    }
    assert_eq!(warm.render(DEMO, &p).unwrap(), cold);
}

#[test]
fn truncated_blocks_paint_the_note_in_any_band() {
    let mut p = params(60);
    p.max_rows = 9;
    let mut r = Renderer::hermetic();
    let full = r.render(DEMO, &p).unwrap();
    assert_eq!(full.rows, 9);
    let l = r.layout(DEMO, &p).unwrap();
    for band in [22, 5, 100] {
        let st = stitched(&mut r, &l, band);
        if let Some(m) = mismatch(&st, &full.image.pixels, l.width) {
            panic!("band {band}: {m}");
        }
    }
}

#[test]
fn paint_clamps_to_the_block() {
    let mut r = Renderer::hermetic();
    let l = r.layout("# Hi", &params(20)).unwrap();
    assert_eq!(r.paint(&l, l.height, 10).height, 0);
    assert_eq!(r.paint(&l, l.height - 3, 10).height, 3);
}
