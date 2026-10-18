//! Markdown layout. STUB: one row per source line, text only, blank
//! (transparent) image. The real engine replaces this module.

use crate::{Error, LayoutParams, Raster, RenderedBlock, TextRun};

pub(crate) fn render(
    _fonts: &mut cosmic_text::FontSystem,
    markdown: &str,
    params: &LayoutParams,
) -> Result<RenderedBlock, Error> {
    let lines: Vec<&str> = markdown.lines().collect();
    let rows = u16::try_from(lines.len().max(1))
        .unwrap_or(u16::MAX)
        .min(params.max_rows.max(1));
    let text = (0..rows as usize)
        .map(|r| {
            let line = lines.get(r).copied().unwrap_or("");
            let clipped: String = line.chars().take(params.cols as usize).collect();
            if clipped.is_empty() {
                vec![]
            } else {
                vec![TextRun {
                    col: 0,
                    text: clipped,
                }]
            }
        })
        .collect();
    let width = params.width_px();
    let height = u32::from(rows) * params.cell_h;
    Ok(RenderedBlock {
        rows,
        image: Raster {
            width,
            height,
            pixels: vec![0; (width * height * 4) as usize],
            baseline: None,
        },
        text,
        links: vec![],
    })
}
