//! Pure-function surface format negotiation.
//!
//! Pulled out so it's covered by unit tests without spinning up a real
//! adapter. The renderer calls [`pick`] with whatever
//! `Surface::get_capabilities().formats` returned and uses the result.

use wgpu::TextureFormat;

/// Preferred surface format: sRGB-aware BGRA8. Almost every modern
/// platform's swapchain supports this and it Just Works with the cell
/// pass's linear-color blending in M4b.
pub const PREFERRED: TextureFormat = TextureFormat::Bgra8UnormSrgb;

/// Pick a surface format from `available`.
///
/// Returns:
/// - `Some(PREFERRED)` if the preferred format is supported,
/// - else the first sRGB format that is supported,
/// - else the first available format,
/// - `None` only if the slice is empty (which would be a wgpu bug).
#[must_use]
pub fn pick(available: &[TextureFormat]) -> Option<TextureFormat> {
    if available.contains(&PREFERRED) {
        return Some(PREFERRED);
    }
    if let Some(srgb) = available.iter().copied().find(TextureFormat::is_srgb) {
        return Some(srgb);
    }
    available.first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefers_bgra8_unorm_srgb_when_present() {
        let av = [
            TextureFormat::Rgba8Unorm,
            TextureFormat::Bgra8UnormSrgb,
            TextureFormat::Rgba8UnormSrgb,
        ];
        assert_eq!(pick(&av), Some(TextureFormat::Bgra8UnormSrgb));
    }

    #[test]
    fn falls_back_to_other_srgb_when_preferred_missing() {
        let av = [TextureFormat::Rgba8Unorm, TextureFormat::Rgba8UnormSrgb];
        assert_eq!(pick(&av), Some(TextureFormat::Rgba8UnormSrgb));
    }

    #[test]
    fn falls_back_to_first_when_no_srgb_at_all() {
        let av = [TextureFormat::Rgba8Unorm, TextureFormat::Rg16Float];
        assert_eq!(pick(&av), Some(TextureFormat::Rgba8Unorm));
    }

    #[test]
    fn returns_none_on_empty_capabilities() {
        let av: [TextureFormat; 0] = [];
        assert_eq!(pick(&av), None);
    }

    #[test]
    fn picks_preferred_even_when_other_srgb_is_first() {
        let av = [TextureFormat::Rgba8UnormSrgb, TextureFormat::Bgra8UnormSrgb];
        assert_eq!(pick(&av), Some(TextureFormat::Bgra8UnormSrgb));
    }
}
