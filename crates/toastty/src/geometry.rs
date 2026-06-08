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
}
