//! 4×4 matrix helpers, std140-aligned column-major.
//!
//! `Mat4` is `[[f32; 4]; 4]` in **column-major** order — matches
//! `mat4x4<f32>` in WGSL when the buffer is read as `uniform`. The
//! helpers here are minimal: enough to compose model matrices from
//! the protocol's transform fields and to build the orthographic
//! projection.

/// 4×4 matrix, column-major. Indexing convention: `m[col][row]`.
pub type Mat4 = [[f32; 4]; 4];

#[must_use]
pub fn identity() -> Mat4 {
    [
        [1.0, 0.0, 0.0, 0.0],
        [0.0, 1.0, 0.0, 0.0],
        [0.0, 0.0, 1.0, 0.0],
        [0.0, 0.0, 0.0, 1.0],
    ]
}

#[must_use]
pub fn translate(x: f32, y: f32, z: f32) -> Mat4 {
    let mut m = identity();
    m[3][0] = x;
    m[3][1] = y;
    m[3][2] = z;
    m
}

#[must_use]
pub fn scale(sx: f32, sy: f32, sz: f32) -> Mat4 {
    let mut m = identity();
    m[0][0] = sx;
    m[1][1] = sy;
    m[2][2] = sz;
    m
}

#[must_use]
pub fn rotate_x_deg(deg: f32) -> Mat4 {
    let (s, c) = deg.to_radians().sin_cos();
    let mut m = identity();
    m[1][1] = c;
    m[1][2] = s;
    m[2][1] = -s;
    m[2][2] = c;
    m
}

#[must_use]
pub fn rotate_y_deg(deg: f32) -> Mat4 {
    let (s, c) = deg.to_radians().sin_cos();
    let mut m = identity();
    m[0][0] = c;
    m[0][2] = -s;
    m[2][0] = s;
    m[2][2] = c;
    m
}

#[must_use]
pub fn rotate_z_deg(deg: f32) -> Mat4 {
    let (s, c) = deg.to_radians().sin_cos();
    let mut m = identity();
    m[0][0] = c;
    m[0][1] = s;
    m[1][0] = -s;
    m[1][1] = c;
    m
}

#[must_use]
pub fn mul(a: &Mat4, b: &Mat4) -> Mat4 {
    let mut out = [[0.0_f32; 4]; 4];
    for col in 0..4 {
        for row in 0..4 {
            let mut s = 0.0;
            for k in 0..4 {
                s += a[k][row] * b[col][k];
            }
            out[col][row] = s;
        }
    }
    out
}

/// Orthographic projection: world XYZ in pixel space → NDC.
///
/// - `width_px` × `height_px`: viewport in physical pixels.
/// - World x ∈ `[0, width_px]` → NDC x ∈ `[-1, 1]`.
/// - World y ∈ `[0, height_px]` → NDC y ∈ `[1, -1]` (Y-down screen → Y-up NDC).
/// - World z ∈ `[-1, 1]` → NDC z ∈ `[0, 1]` (depth=0 → NDC 0.5).
///
/// The 0.5 factor on z keeps the cell layer co-planar with protocol
/// `depth=0` (decision §3); per-placement `depth=` is folded into
/// the model matrix's z translation by the caller.
#[must_use]
pub fn ortho_screen(width_px: f32, height_px: f32) -> Mat4 {
    let mut m = identity();
    m[0][0] = 2.0 / width_px;
    m[1][1] = -2.0 / height_px;
    m[2][2] = 0.5;
    m[3][0] = -1.0;
    m[3][1] = 1.0;
    m[3][2] = 0.5;
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    fn matrix_approx(a: &Mat4, b: &Mat4) -> bool {
        (0..4).all(|i| (0..4).all(|j| approx_eq(a[i][j], b[i][j])))
    }

    #[test]
    fn identity_is_identity() {
        let i = identity();
        for col in 0..4 {
            for row in 0..4 {
                let want = if col == row { 1.0 } else { 0.0 };
                assert!(approx_eq(i[col][row], want));
            }
        }
    }

    #[test]
    fn translation_carries_to_last_column() {
        let t = translate(3.0, 4.0, 5.0);
        assert!(approx_eq(t[3][0], 3.0));
        assert!(approx_eq(t[3][1], 4.0));
        assert!(approx_eq(t[3][2], 5.0));
        assert!(approx_eq(t[3][3], 1.0));
    }

    #[test]
    fn mul_identity_is_idempotent() {
        let m = translate(1.0, 2.0, 3.0);
        let id = identity();
        assert!(matrix_approx(&mul(&id, &m), &m));
        assert!(matrix_approx(&mul(&m, &id), &m));
    }

    #[test]
    fn rotate_y_180_inverts_x_and_z() {
        let r = rotate_y_deg(180.0);
        // (1, 0, 0) → (-1, 0, 0)
        let x_col = r[0];
        assert!(approx_eq(x_col[0], -1.0));
        assert!(approx_eq(x_col[2], 0.0));
        let z_col = r[2];
        assert!(approx_eq(z_col[0], 0.0));
        assert!(approx_eq(z_col[2], -1.0));
    }

    #[test]
    fn ortho_maps_origin_to_topleft_ndc() {
        let p = ortho_screen(800.0, 600.0);
        // World (0, 0, 0) → expect NDC (-1, +1, 0.5).
        let world = [0.0, 0.0, 0.0, 1.0];
        let mut out = [0.0_f32; 4];
        for row in 0..4 {
            for k in 0..4 {
                out[row] += p[k][row] * world[k];
            }
        }
        assert!(approx_eq(out[0], -1.0), "x: {}", out[0]);
        assert!(approx_eq(out[1], 1.0), "y: {}", out[1]);
        assert!(approx_eq(out[2], 0.5), "z: {}", out[2]);
    }

    #[test]
    fn ortho_maps_bottom_right_to_ndc_extents() {
        let p = ortho_screen(800.0, 600.0);
        let world = [800.0, 600.0, 0.0, 1.0];
        let mut out = [0.0_f32; 4];
        for row in 0..4 {
            for k in 0..4 {
                out[row] += p[k][row] * world[k];
            }
        }
        assert!(approx_eq(out[0], 1.0), "x: {}", out[0]);
        assert!(approx_eq(out[1], -1.0), "y: {}", out[1]);
    }
}
