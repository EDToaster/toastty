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

#[cfg(test)]
mod tests {
    use super::*;

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
