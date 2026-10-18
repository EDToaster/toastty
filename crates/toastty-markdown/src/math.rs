//! LaTeX maths → [`Raster`]. STUB until the RaTeX backend lands.

use crate::{Raster, Rgba};

/// Render a LaTeX maths expression.
///
/// - `display`: display style (`$$…$$`, centered block) vs inline (`$…$`).
/// - `font_px`: the surrounding body font size in pixels.
/// - `color`: glyph colour.
///
/// Inline results set [`Raster::baseline`].
///
/// # Errors
/// Returns a message describing the parse/render failure; callers fall
/// back to showing the source as inline code.
pub fn render_math(tex: &str, display: bool, font_px: f32, color: Rgba) -> Result<Raster, String> {
    let _ = (tex, display, font_px, color);
    Err("maths rendering not available".into())
}
