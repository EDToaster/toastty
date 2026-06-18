//! Pixel → cell-grid math.

/// Compute `(cols, rows)` for a window of `px_w` × `px_h` physical
/// pixels when one cell measures `cell_w` × `cell_h` pixels.
///
/// Guarantees `cols >= 1` and `rows >= 1`; rounds DOWN so partial trailing
/// cells aren't reported as full ones. Clamped to `u16::MAX`.
#[must_use]
pub fn grid_dims_from_pixels(px_w: u32, px_h: u32, cell_w: f32, cell_h: f32) -> (u16, u16) {
    // Cast u32 → f64 is lossless. f32 cell dims are fine.
    let cols = ((f64::from(px_w) / f64::from(cell_w)).floor() as i64).max(1);
    let rows = ((f64::from(px_h) / f64::from(cell_h)).floor() as i64).max(1);
    let cols = u16::try_from(cols).unwrap_or(u16::MAX);
    let rows = u16::try_from(rows).unwrap_or(u16::MAX);
    (cols, rows)
}

/// Physical font size in pixels for a logical (DPI-independent) size at a
/// given monitor scale factor.
///
/// The config's `font.size_px` is treated as a *logical* size: the value
/// the user wants at scale 1.0. The window's backing surface is sized in
/// physical pixels, so glyphs must be rasterized at `logical × scale` to
/// (a) keep the same apparent size as every other DPI-aware app on a
/// scaled monitor, and (b) be rasterized at the monitor's true resolution
/// rather than upscaled from a smaller bitmap (the source of soft text on
/// `HiDPI` displays).
///
/// Clamped to a 1 px floor so a degenerate scale factor can't yield a
/// zero-size font (which would panic downstream in cosmic-text / the
/// cell-grid division).
#[must_use]
pub fn effective_font_size_px(logical_px: f32, scale_factor: f64) -> f32 {
    #[allow(clippy::cast_possible_truncation)]
    let scaled = (f64::from(logical_px) * scale_factor) as f32;
    scaled.max(1.0)
}

/// Scale a logical (DPI-independent) padding value to physical px using the
/// same `.round()` rule as [`effective_font_size_px`].
///
/// Kept private + shared by [`content_rect_from_padding`] and the binary's
/// `push_padding_to_renderer` (via the public sibling [`scaled_pads`]) so the
/// renderer origin, the `EdgeBleed` pad, and the grid inset are all derived
/// from byte-identical scaling — never re-scaled with a divergent rounding
/// rule.
#[must_use]
fn scale_pad(v: u16, scale_factor: f64) -> u32 {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let s = (f64::from(v) * scale_factor).round() as u32;
    s
}

/// Scale the four logical pads `(top, right, bottom, left)` to physical px,
/// matching [`content_rect_from_padding`]'s scaling exactly.
#[must_use]
pub fn scaled_pads(pad: (u16, u16, u16, u16), scale_factor: f64) -> (u32, u32, u32, u32) {
    (
        scale_pad(pad.0, scale_factor),
        scale_pad(pad.1, scale_factor),
        scale_pad(pad.2, scale_factor),
        scale_pad(pad.3, scale_factor),
    )
}

/// Compute the content rect (the cell-grid region inset from the window edges
/// by the configured window padding) for a physical-px `surface`.
///
/// `pad` is `(top, right, bottom, left)` in *logical* px (matching
/// `PaddingConfig`); it is scaled to physical px via the same `.round()` rule
/// as [`effective_font_size_px`]. Returns `(origin_x, origin_y, content_w,
/// content_h)` in physical px, where the origin is `(pad_left, pad_top)`.
///
/// Content dims clamp to a 1 px floor (so [`grid_dims_from_pixels`]' own
/// `.max(1)` still yields at least one cell). The origin is pinned to
/// `surface - 1` so the lone clamped cell can't draw fully off-screen.
#[must_use]
pub fn content_rect_from_padding(
    surface: (u32, u32),
    pad: (u16, u16, u16, u16),
    scale_factor: f64,
) -> (u32, u32, u32, u32) {
    let (sw, sh) = surface;
    let (pt, pr, pb, pl) = scaled_pads(pad, scale_factor);
    let origin_x = pl.min(sw.saturating_sub(1));
    let origin_y = pt.min(sh.saturating_sub(1));
    let content_w = sw.saturating_sub(pl + pr).max(1);
    let content_h = sh.saturating_sub(pt + pb).max(1);
    (origin_x, origin_y, content_w, content_h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn font_size_unscaled_at_1x() {
        assert!((effective_font_size_px(16.0, 1.0) - 16.0).abs() < 1e-6);
    }

    #[test]
    fn font_size_scales_with_factor() {
        // 2.0× HiDPI and 1.25× fractional scaling both round-trip exactly.
        assert!((effective_font_size_px(16.0, 2.0) - 32.0).abs() < 1e-6);
        assert!((effective_font_size_px(16.0, 1.25) - 20.0).abs() < 1e-6);
    }

    #[test]
    fn font_size_clamped_to_one() {
        // Degenerate inputs must never produce a zero-size font.
        assert!(effective_font_size_px(0.0, 1.0) >= 1.0);
        assert!(effective_font_size_px(16.0, 0.0) >= 1.0);
    }

    #[test]
    fn simple_division() {
        let (c, r) = grid_dims_from_pixels(800, 600, 10.0, 20.0);
        assert_eq!((c, r), (80, 30));
    }

    #[test]
    fn floors_partial_cells() {
        let (c, r) = grid_dims_from_pixels(805, 605, 10.0, 20.0);
        assert_eq!((c, r), (80, 30));
    }

    #[test]
    fn never_returns_zero() {
        let (c, r) = grid_dims_from_pixels(5, 5, 100.0, 100.0);
        assert_eq!((c, r), (1, 1));
    }

    #[test]
    fn clamps_to_u16_max() {
        let (c, _) = grid_dims_from_pixels(u32::MAX, 100, 1.0, 1.0);
        assert_eq!(c, u16::MAX);
    }

    #[test]
    fn content_dims_subtract_padding() {
        // surface 800×600, pad {top:10, right:20, bottom:10, left:20} @ 1×.
        let (ox, oy, cw, ch) = content_rect_from_padding((800, 600), (10, 20, 10, 20), 1.0);
        assert_eq!((ox, oy), (20, 10)); // origin = (left, top)
        assert_eq!((cw, ch), (760, 580)); // 800-40, 600-20
    }

    #[test]
    fn content_dims_clamp_to_one_cell_when_padding_huge() {
        // Padding far exceeds the surface — content must clamp to >=1px and
        // grid_dims_from_pixels must still yield >=(1,1) with no panic.
        let (ox, oy, cw, ch) = content_rect_from_padding((100, 100), (500, 500, 500, 500), 1.0);
        // Origin pinned to surface-1 so the lone cell stays on-screen.
        assert_eq!((ox, oy), (99, 99));
        assert_eq!((cw, ch), (1, 1));
        let (cols, rows) = grid_dims_from_pixels(cw, ch, 10.0, 20.0);
        assert!(cols >= 1 && rows >= 1);
    }

    #[test]
    fn padding_scaled_by_scale_factor() {
        // logical 8 @ 2.0× → 16 physical px, matching effective_font_size_px's
        // .round() rule. surface 800×600 → content 768×568, origin (16,16).
        let (ox, oy, cw, ch) = content_rect_from_padding((800, 600), (8, 8, 8, 8), 2.0);
        assert_eq!((ox, oy), (16, 16));
        assert_eq!((cw, ch), (768, 568));
        // scaled_pads agrees with content_rect_from_padding's scaling.
        assert_eq!(scaled_pads((8, 8, 8, 8), 2.0), (16, 16, 16, 16));
    }

    #[test]
    fn padding_round_not_truncate() {
        // logical 5 @ 1.25× = 6.25 → rounds to 6 (a truncating cast would
        // give 6 here too, but logical 3 @ 1.5× = 4.5 → rounds to 5).
        assert_eq!(scaled_pads((3, 0, 0, 0), 1.5).0, 5);
    }
}
