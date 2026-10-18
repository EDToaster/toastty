//! Adapter from the terminal's rich-block hook
//! ([`toastty_term::RichBlockLayout`]) to the Markdown layout engine
//! ([`toastty_markdown::Renderer`]).
//!
//! `toastty-term` knows nothing about Markdown: it parses OSC 7700,
//! owns the block rows and asks this adapter to lay a block out. The
//! engine (and its font database, which is expensive to build) is
//! created lazily on the first block, so a session that never shows
//! Markdown pays nothing.

use std::sync::Arc;

use toastty_markdown::{LayoutParams, RenderedBlock, ResourcePolicy, Rgba, Theme};
use toastty_term::{
    RichBlockLayout, RichImage, RichLayout, RichLayoutRequest, RichLinkSpan, RichPalette,
    RichTextRun,
};

/// The only block kind this adapter renders.
pub const KIND_MARKDOWN: &str = "md";

/// [`RichBlockLayout`] backed by the Markdown engine.
#[derive(Debug, Default)]
pub struct MarkdownLayout {
    renderer: Option<toastty_markdown::Renderer>,
}

impl MarkdownLayout {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Adapter over a caller-built engine (tests use one with only
    /// bundled fonts).
    #[must_use]
    pub fn with_renderer(renderer: toastty_markdown::Renderer) -> Self {
        Self {
            renderer: Some(renderer),
        }
    }
}

impl RichBlockLayout for MarkdownLayout {
    fn layout(&mut self, req: &RichLayoutRequest<'_>) -> Option<RichLayout> {
        if req.kind != KIND_MARKDOWN {
            return None;
        }
        let renderer = self
            .renderer
            .get_or_insert_with(toastty_markdown::Renderer::new);
        let params = layout_params(req);
        match renderer.render(req.source, &params) {
            Ok(block) => Some(convert_block(block, &params)),
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
        && img.pixels.len() as u64 == u64::from(want_w) * u64::from(want_h) * 4
        && want_w > 0
        && want_h > 0
        && img.pixels.as_chunks::<4>().0.iter().any(|px| px[3] != 0))
    .then(|| RichImage {
        width: img.width,
        height: img.height,
        pixels: Arc::from(img.pixels),
    });
    let text = block
        .text
        .into_iter()
        .take(usize::from(rows))
        .map(|runs| {
            runs.into_iter()
                .map(|r| RichTextRun {
                    col: r.col,
                    text: r.text,
                })
                .collect()
        })
        .collect();
    let links = block
        .links
        .into_iter()
        .filter(|l| l.row < rows && l.cols.start < l.cols.end)
        .map(|l| RichLinkSpan {
            row: l.row,
            cols: l.cols,
            url: l.url,
        })
        .collect();
    RichLayout {
        rows,
        image,
        text,
        links,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use toastty_markdown::{LinkSpan, Raster, TextRun};

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
    fn convert_keeps_valid_output() {
        let p = layout_params(&request("md", None));
        let out = convert_block(block(2, 80, 32, 255), &p);
        assert_eq!(out.rows, 2);
        let img = out.image.unwrap();
        assert_eq!(
            (img.width, img.height, img.pixels.len()),
            (80, 32, 80 * 32 * 4)
        );
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
}
