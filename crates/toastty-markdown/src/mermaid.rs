//! Mermaid → [`Raster`]. STUB until the mermaid-rs-renderer backend lands.

use crate::{Raster, Theme};

/// Render a Mermaid diagram no wider than `max_width_px` (scaled down
/// to fit, never up past its natural size × `scale`), coloured from
/// `theme`. `font_px` is the body font size used for labels.
///
/// # Errors
/// Returns a message describing the failure; callers fall back to
/// showing the source as a code block.
pub fn render_mermaid(
    src: &str,
    max_width_px: u32,
    font_px: f32,
    theme: &Theme,
) -> Result<Raster, String> {
    let _ = (src, max_width_px, font_px, theme);
    Err("mermaid rendering not available".into())
}
