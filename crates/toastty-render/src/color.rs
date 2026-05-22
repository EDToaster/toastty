//! Pure-function color conversion helpers.
//!
//! Two needs in M4a:
//!   - Snapshot test reads `Bgra8UnormSrgb` pixels back as u8s and needs
//!     to assert against the *linear* clear color we asked wgpu for.
//!   - The `clear_color` demo wants an HSV→RGB cycle.

/// Convert a single linear-light channel in `[0, 1]` to sRGB-encoded u8.
///
/// Mirrors what the GPU does when it writes to an sRGB surface format:
/// linear shader output → sRGB-encoded bytes in the swapchain image.
#[must_use]
pub fn linear_to_srgb_u8(linear: f32) -> u8 {
    let l = linear.clamp(0.0, 1.0);
    let enc = if l <= 0.003_130_8 {
        l * 12.92
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    (enc * 255.0 + 0.5) as u8
}

/// Convert a linear-light RGBA tuple in `[0, 1]` to a 4-byte BGRA u8
/// array — exactly what we'd read back from a `Bgra8UnormSrgb` swapchain.
#[must_use]
pub fn linear_rgba_to_bgra_srgb_bytes(rgba: [f32; 4]) -> [u8; 4] {
    [
        linear_to_srgb_u8(rgba[2]),                    // B
        linear_to_srgb_u8(rgba[1]),                    // G
        linear_to_srgb_u8(rgba[0]),                    // R
        (rgba[3].clamp(0.0, 1.0) * 255.0 + 0.5) as u8, // A (always linear)
    ]
}

/// Convert HSV → RGB (all in `[0, 1]`). H wraps automatically.
///
/// Lifted from the standard HSV definition; included as a pure helper so
/// the demo doesn't pull in `palette` or similar.
#[must_use]
#[allow(clippy::many_single_char_names)] // standard HSV variable names
pub fn hsv_to_rgb(hue: f32, sat: f32, val: f32) -> [f32; 3] {
    let h = hue.rem_euclid(1.0);
    let s = sat.clamp(0.0, 1.0);
    let v = val.clamp(0.0, 1.0);

    let c = v * s;
    let h6 = h * 6.0;
    let x = c * (1.0 - (h6.rem_euclid(2.0) - 1.0).abs());

    let (r1, g1, b1) = if h6 < 1.0 {
        (c, x, 0.0)
    } else if h6 < 2.0 {
        (x, c, 0.0)
    } else if h6 < 3.0 {
        (0.0, c, x)
    } else if h6 < 4.0 {
        (0.0, x, c)
    } else if h6 < 5.0 {
        (x, 0.0, c)
    } else {
        (c, 0.0, x)
    };

    let m = v - c;
    [r1 + m, g1 + m, b1 + m]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx_eq(a: f32, b: f32, eps: f32) -> bool {
        (a - b).abs() <= eps
    }

    #[test]
    fn linear_zero_is_zero() {
        assert_eq!(linear_to_srgb_u8(0.0), 0);
    }

    #[test]
    fn linear_one_is_255() {
        assert_eq!(linear_to_srgb_u8(1.0), 255);
    }

    #[test]
    fn linear_to_srgb_midpoint_matches_spec() {
        // sRGB(0.5 linear) ≈ 0.735 nonlinear → 188.
        let v = linear_to_srgb_u8(0.5);
        assert!((186..=190).contains(&v), "midpoint produced {v}");
    }

    #[test]
    fn linear_to_srgb_clamps_negative() {
        assert_eq!(linear_to_srgb_u8(-1.0), 0);
    }

    #[test]
    fn linear_to_srgb_clamps_huge() {
        assert_eq!(linear_to_srgb_u8(10.0), 255);
    }

    #[test]
    fn small_linear_uses_linear_segment() {
        // Below the 0.0031308 knee, output is linear * 12.92 * 255.
        let l = 0.001_f32;
        let expected = (l * 12.92 * 255.0 + 0.5) as u8;
        assert_eq!(linear_to_srgb_u8(l), expected);
    }

    #[test]
    fn bgra_byte_order() {
        let bytes = linear_rgba_to_bgra_srgb_bytes([1.0, 0.0, 0.0, 1.0]);
        // Red linear=1 → B=0, G=0, R=255, A=255.
        assert_eq!(bytes, [0, 0, 255, 255]);
    }

    #[test]
    fn bgra_alpha_is_linear_not_srgb() {
        // Alpha at linear 0.5 must stay 128, not get gamma-bent to 188.
        let bytes = linear_rgba_to_bgra_srgb_bytes([0.0, 0.0, 0.0, 0.5]);
        assert_eq!(bytes[3], 128);
    }

    #[test]
    fn hsv_red() {
        let rgb = hsv_to_rgb(0.0, 1.0, 1.0);
        assert!(approx_eq(rgb[0], 1.0, 1e-6));
        assert!(approx_eq(rgb[1], 0.0, 1e-6));
        assert!(approx_eq(rgb[2], 0.0, 1e-6));
    }

    #[test]
    fn hsv_green() {
        let rgb = hsv_to_rgb(1.0 / 3.0, 1.0, 1.0);
        assert!(approx_eq(rgb[0], 0.0, 1e-6));
        assert!(approx_eq(rgb[1], 1.0, 1e-6));
        assert!(approx_eq(rgb[2], 0.0, 1e-6));
    }

    #[test]
    fn hsv_blue() {
        let rgb = hsv_to_rgb(2.0 / 3.0, 1.0, 1.0);
        assert!(approx_eq(rgb[0], 0.0, 1e-6));
        assert!(approx_eq(rgb[1], 0.0, 1e-6));
        assert!(approx_eq(rgb[2], 1.0, 1e-6));
    }

    #[test]
    fn hsv_wraps_hue() {
        let a = hsv_to_rgb(0.0, 0.9, 0.7);
        let b = hsv_to_rgb(1.0, 0.9, 0.7);
        for i in 0..3 {
            assert!(approx_eq(a[i], b[i], 1e-6));
        }
    }

    #[test]
    fn hsv_zero_saturation_is_gray() {
        let rgb = hsv_to_rgb(0.42, 0.0, 0.6);
        assert!(approx_eq(rgb[0], 0.6, 1e-6));
        assert!(approx_eq(rgb[1], 0.6, 1e-6));
        assert!(approx_eq(rgb[2], 0.6, 1e-6));
    }

    #[test]
    fn hsv_zero_value_is_black() {
        let rgb = hsv_to_rgb(0.42, 1.0, 0.0);
        assert!(approx_eq(rgb[0], 0.0, 1e-6));
        assert!(approx_eq(rgb[1], 0.0, 1e-6));
        assert!(approx_eq(rgb[2], 0.0, 1e-6));
    }

    #[test]
    fn hsv_h6_segment_5_yellow_orange_path() {
        // h ∈ [5/6, 1) → magenta-toward-red wedge.
        let rgb = hsv_to_rgb(11.0 / 12.0, 1.0, 1.0);
        // R should be 1.0, G should be 0, B between 0 and 1.
        assert!(approx_eq(rgb[0], 1.0, 1e-6));
        assert!(approx_eq(rgb[1], 0.0, 1e-6));
        assert!(rgb[2] > 0.0 && rgb[2] < 1.0);
    }

    #[test]
    fn hsv_h6_segment_3_cyan_path() {
        // h = 0.5 → cyan.
        let rgb = hsv_to_rgb(0.5, 1.0, 1.0);
        assert!(approx_eq(rgb[0], 0.0, 1e-6));
        assert!(approx_eq(rgb[1], 1.0, 1e-6));
        assert!(approx_eq(rgb[2], 1.0, 1e-6));
    }

    #[test]
    fn hsv_h6_segment_4_blue_path() {
        // Just past 4/6.
        let rgb = hsv_to_rgb(0.7, 1.0, 1.0);
        assert!(rgb[2] > 0.0); // Blue dominant
        assert!(rgb[2] >= rgb[0] && rgb[2] >= rgb[1]);
    }

    #[test]
    fn hsv_clamps_negative_saturation_to_zero() {
        let rgb = hsv_to_rgb(0.3, -0.5, 0.8);
        assert!(approx_eq(rgb[0], 0.8, 1e-6));
        assert!(approx_eq(rgb[1], 0.8, 1e-6));
        assert!(approx_eq(rgb[2], 0.8, 1e-6));
    }

    #[test]
    fn hsv_clamps_value_above_one() {
        let rgb = hsv_to_rgb(0.0, 1.0, 2.0);
        assert!(approx_eq(rgb[0], 1.0, 1e-6));
        assert!(approx_eq(rgb[1], 0.0, 1e-6));
        assert!(approx_eq(rgb[2], 0.0, 1e-6));
    }
}
