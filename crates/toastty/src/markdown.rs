//! Adapter from the terminal's rich-block hook
//! ([`toastty_term::RichBlockLayout`]) to the Markdown layout engine
//! ([`toastty_markdown::Renderer`]).
//!
//! `toastty-term` knows nothing about Markdown: it parses OSC 7700,
//! owns the block rows and asks this adapter to lay a block out. The
//! engine (and its font database, which is expensive to build) is
//! created lazily on the first block, so a session that never shows
//! Markdown pays nothing.

use std::sync::{Arc, Mutex};

use toastty_markdown::{
    BlockLayout, LayoutParams, LinkSpan, RenderedBlock, Renderer, ResourcePolicy, Rgba, TextRun,
    Theme,
};
use toastty_term::{
    RichBlockLayout, RichImage, RichLayout, RichLayoutRequest, RichLinkSpan, RichPaint,
    RichPalette, RichTextRun,
};

/// The only block kind this adapter renders.
pub const KIND_MARKDOWN: &str = "md";

/// [`RichBlockLayout`] backed by the Markdown engine.
///
/// Layout runs on every reflow (each step of a window resize), so it
/// only lays out: the block's image is a [`MarkdownPaint`] that paints
/// the rows the renderer actually shows, when it shows them. Painters
/// share the engine (it owns the glyph cache) behind a mutex.
#[derive(Debug, Default)]
pub struct MarkdownLayout {
    renderer: Option<Arc<Mutex<Renderer>>>,
}

/// Paints bands of one laid-out block on demand.
#[derive(Debug)]
pub struct MarkdownPaint {
    renderer: Arc<Mutex<Renderer>>,
    layout: BlockLayout,
}

impl RichPaint for MarkdownPaint {
    fn paint(&self, y0: u32, h: u32) -> Option<Vec<u8>> {
        let mut r = self.renderer.lock().ok()?;
        let raster = r.paint(&self.layout, y0, h);
        (raster.width == self.layout.width && raster.height == h).then_some(raster.pixels)
    }

    fn byte_len(&self) -> u64 {
        self.layout.approx_bytes()
    }
}

impl MarkdownLayout {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adapter over a caller-built engine (tests use one with only
    /// bundled fonts).
    #[must_use]
    pub fn with_renderer(renderer: Renderer) -> Self {
        Self {
            renderer: Some(Arc::new(Mutex::new(renderer))),
        }
    }
}

impl RichBlockLayout for MarkdownLayout {
    fn layout(&mut self, req: &RichLayoutRequest<'_>) -> Option<RichLayout> {
        if req.kind != KIND_MARKDOWN {
            return None;
        }
        let renderer = Arc::clone(
            self.renderer
                .get_or_insert_with(|| Arc::new(Mutex::new(Renderer::new()))),
        );
        let params = layout_params(req);
        let laid = renderer.lock().ok()?.layout(req.source, &params);
        match laid {
            Ok(layout) => Some(convert_layout(layout, renderer, &params)),
            Err(e) => {
                tracing::warn!("markdown block layout failed: {e}");
                None
            }
        }
    }
}

fn opaque(c: [u8; 3]) -> Rgba {
    [c[0], c[1], c[2], 0xff]
}

/// The engine's theme for a terminal palette.
#[must_use]
pub fn theme_from_palette(p: &RichPalette) -> Theme {
    Theme {
        fg: opaque(p.fg),
        bg: opaque(p.bg),
        ansi: p.ansi.map(opaque),
    }
}

/// Engine parameters for a terminal layout request.
#[must_use]
pub fn layout_params(req: &RichLayoutRequest<'_>) -> LayoutParams {
    LayoutParams {
        cols: req.cols,
        cell_w: req.cell_w,
        cell_h: req.cell_h,
        font_px: req.font_px,
        theme: theme_from_palette(&req.palette),
        resources: ResourcePolicy {
            allow_local_files: req.allow_local_files,
            base_dir: req.base_dir.map(std::path::Path::to_path_buf),
        },
        max_rows: req.max_rows,
        mono_family: req.mono_family.map(str::to_owned),
    }
}

/// Convert a lazily painted layout into the terminal's block layout.
/// Same contract checks as [`convert_block`]; a layout that draws
/// nothing gets no image.
#[must_use]
pub fn convert_layout(
    layout: BlockLayout,
    renderer: Arc<Mutex<Renderer>>,
    params: &LayoutParams,
) -> RichLayout {
    let rows = layout.rows.clamp(1, params.max_rows.max(1));
    let want_w = params.width_px();
    let want_h = u32::from(rows) * params.cell_h;
    let ok = layout.width == want_w
        && layout.height == want_h
        && want_w > 0
        && want_h > 0
        && !layout.is_blank();
    let text = convert_text(layout.text().iter().cloned(), rows);
    let links = convert_links(layout.links().iter().cloned(), rows);
    let image =
        ok.then(|| RichImage::new(want_w, want_h, Arc::new(MarkdownPaint { renderer, layout })));
    RichLayout {
        rows,
        image,
        text,
        links,
    }
}

/// Convert the engine's output into the terminal's block layout.
///
/// Defensive against an engine that breaks its own contract: the row
/// count is clamped to `max_rows` (and at least 1), an image whose
/// size or byte length doesn't match `cols * cell_w` x `rows * cell_h`
/// is dropped (the block then shows its text), and text / links past
/// the last row are discarded.
#[must_use]
pub fn convert_block(block: RenderedBlock, params: &LayoutParams) -> RichLayout {
    let rows = block.rows.clamp(1, params.max_rows.max(1));
    let want_w = params.width_px();
    let want_h = u32::from(rows) * params.cell_h;
    let img = block.image;
    let image = (img.width == want_w
        && img.height == want_h
        && want_w > 0
        && want_h > 0
        && img.pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0))
    .then(|| RichImage::from_pixels(img.width, img.height, img.pixels))
    .flatten();
    RichLayout {
        rows,
        image,
        text: convert_text(block.text.into_iter(), rows),
        links: convert_links(block.links.into_iter(), rows),
    }
}

fn convert_text(text: impl Iterator<Item = Vec<TextRun>>, rows: u16) -> Vec<Vec<RichTextRun>> {
    text.take(usize::from(rows))
        .map(|runs| {
            runs.into_iter()
                .map(|r| RichTextRun {
                    col: r.col,
                    text: r.text,
                })
                .collect()
        })
        .collect()
}

fn convert_links(links: impl Iterator<Item = LinkSpan>, rows: u16) -> Vec<RichLinkSpan> {
    links
        .filter(|l| l.row < rows && l.cols.start < l.cols.end)
        .map(|l| RichLinkSpan {
            row: l.row,
            cols: l.cols,
            url: l.url,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use toastty_markdown::Raster;

    fn palette() -> RichPalette {
        let mut ansi = [[0u8; 3]; 16];
        for (i, c) in ansi.iter_mut().enumerate() {
            *c = [u8::try_from(i).unwrap(), 1, 2];
        }
        RichPalette {
            fg: [200, 201, 202],
            bg: [10, 11, 12],
            ansi,
        }
    }

    fn request<'a>(kind: &'a str, base: Option<&'a Path>) -> RichLayoutRequest<'a> {
        RichLayoutRequest {
            kind,
            source: "# hi",
            cols: 10,
            cell_w: 8,
            cell_h: 16,
            font_px: 26.0,
            mono_family: None,
            palette: palette(),
            allow_local_files: true,
            base_dir: base,
            max_rows: 5,
        }
    }

    fn block(rows: u16, w: u32, h: u32, alpha: u8) -> RenderedBlock {
        let mut pixels = vec![0u8; (w * h * 4) as usize];
        for px in pixels.as_chunks_mut::<4>().0 {
            px[3] = alpha;
        }
        RenderedBlock {
            rows,
            image: Raster {
                width: w,
                height: h,
                pixels,
                baseline: None,
            },
            text: (0..rows)
                .map(|r| {
                    vec![TextRun {
                        col: 1,
                        text: format!("row{r}"),
                    }]
                })
                .collect(),
            links: vec![
                LinkSpan {
                    row: 0,
                    cols: 1..4,
                    url: "https://a".into(),
                },
                LinkSpan {
                    row: rows + 3,
                    cols: 0..2,
                    url: "https://gone".into(),
                },
                LinkSpan {
                    row: 0,
                    cols: 3..3,
                    url: "https://empty".into(),
                },
            ],
        }
    }

    #[test]
    fn params_carry_geometry_theme_and_policy() {
        let base = Path::new("/tmp/docs");
        let p = layout_params(&request("md", Some(base)));
        assert_eq!((p.cols, p.cell_w, p.cell_h, p.max_rows), (10, 8, 16, 5));
        assert!((p.font_px - 26.0).abs() < f32::EPSILON);
        assert_eq!(p.theme.fg, [200, 201, 202, 255]);
        assert_eq!(p.theme.bg, [10, 11, 12, 255]);
        assert_eq!(p.theme.ansi[7], [7, 1, 2, 255]);
        assert!(p.resources.allow_local_files);
        assert_eq!(p.resources.base_dir.as_deref(), Some(base));
        assert_eq!(layout_params(&request("md", None)).resources.base_dir, None);
    }

    #[test]
    fn params_carry_the_terminal_font_family() {
        assert_eq!(layout_params(&request("md", None)).mono_family, None);
        let mut req = request("md", None);
        req.mono_family = Some("Iosevka Term");
        assert_eq!(
            layout_params(&req).mono_family.as_deref(),
            Some("Iosevka Term")
        );
    }

    #[test]
    fn unknown_font_family_falls_back_to_the_default_face() {
        let mut req = request("md", None);
        req.source = "```
fn main() {}
```
";
        req.cols = 40;
        req.max_rows = 50;
        let render = |req: &RichLayoutRequest<'_>| {
            let mut l = MarkdownLayout::with_renderer(Renderer::hermetic());
            let out = l.layout(req).unwrap();
            let img = out.image.unwrap();
            (out.text, img.paint(0, img.height).unwrap())
        };
        let default = render(&req);
        req.mono_family = Some("No Such Family Mono");
        assert_eq!(render(&req), default);
        // A family the engine has is used (the bundled body face here).
        req.mono_family = Some("Noto Sans");
        let noto = render(&req);
        // Its taller line may add a row; the text keeps its columns.
        let runs = |t: &[Vec<RichTextRun>]| t.iter().flatten().cloned().collect::<Vec<_>>();
        assert_eq!(runs(&noto.0), runs(&default.0));
        assert_ne!(noto.1, default.1);
    }

    #[test]
    fn convert_keeps_valid_output() {
        let p = layout_params(&request("md", None));
        let out = convert_block(block(2, 80, 32, 255), &p);
        assert_eq!(out.rows, 2);
        let img = out.image.unwrap();
        assert_eq!((img.width, img.height), (80, 32));
        assert_eq!(img.paint(0, 32).unwrap().len(), 80 * 32 * 4);
        assert_eq!(out.text.len(), 2);
        assert_eq!(out.text[1][0].text, "row1");
        assert_eq!(out.text[1][0].col, 1);
        assert_eq!(out.links.len(), 1, "out-of-range and empty spans dropped");
        assert_eq!(out.links[0].url, "https://a");
        assert_eq!(out.links[0].cols, 1..4);
    }

    #[test]
    fn convert_drops_mismatched_or_blank_images() {
        let p = layout_params(&request("md", None));
        // Wrong width.
        assert!(convert_block(block(2, 79, 32, 255), &p).image.is_none());
        // Wrong height for the row count.
        assert!(convert_block(block(2, 80, 48, 255), &p).image.is_none());
        // Fully transparent: nothing to draw, show the text instead.
        assert!(convert_block(block(2, 80, 32, 0), &p).image.is_none());
        // Truncated pixel buffer.
        let mut b = block(2, 80, 32, 255);
        b.image.pixels.truncate(100);
        assert!(convert_block(b, &p).image.is_none());
    }

    #[test]
    fn convert_clamps_rows_to_cap() {
        let p = layout_params(&request("md", None));
        let out = convert_block(block(9, 80, 9 * 16, 255), &p);
        assert_eq!(out.rows, 5);
        assert_eq!(out.text.len(), 5);
        assert!(
            out.image.is_none(),
            "image no longer matches the clamped rows"
        );
        let out = convert_block(block(0, 80, 0, 255), &p);
        assert_eq!(out.rows, 1);
    }

    #[test]
    fn other_kinds_are_declined_without_building_the_engine() {
        let mut l = MarkdownLayout::new();
        assert!(l.layout(&request("svg", None)).is_none());
        assert!(l.renderer.is_none());
    }

    #[test]
    fn layout_paints_lazily_and_matches_a_full_render() {
        let mut l = MarkdownLayout::with_renderer(Renderer::hermetic());
        let mut req = request("md", None);
        req.source = "# Title\n\nSome *text* and `code`.\n\n- a\n- b\n";
        req.max_rows = 50;
        let out = l.layout(&req).unwrap();
        let img = out.image.unwrap();
        assert_eq!(img.height, u32::from(out.rows) * 16);
        let full = Renderer::hermetic()
            .render(req.source, &layout_params(&req))
            .unwrap();
        assert_eq!(img.paint(0, img.height).unwrap(), full.image.pixels);
        // A band equals the same rows of the full image.
        let stride = 80 * 4;
        let band = img.paint(16, 32).unwrap();
        assert_eq!(band, full.image.pixels[16 * stride..48 * stride]);
    }

    #[test]
    fn blank_layout_has_no_image() {
        let mut l = MarkdownLayout::with_renderer(Renderer::hermetic());
        let mut req = request("md", None);
        req.source = "   ";
        let out = l.layout(&req).unwrap();
        assert!(out.image.is_none());
    }
}
