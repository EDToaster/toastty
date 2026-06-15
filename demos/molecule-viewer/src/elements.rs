//! CPK colors + covalent radii for common elements.
//!
//! Hardcoded table (no external crate). Cover at least the common
//! organic/bio elements: H, C, N, O, F, P, S, Cl, Br, I, plus B, Si,
//! and a few metals (Na, K, Ca, Fe, Mg, Zn). Unknown symbols fall back
//! to a neutral value so rendering never panics.

/// CPK color (RGB, 0–255) for an element symbol. Unknown → a neutral
/// fallback (e.g. pink `[255, 192, 203]` per the CPK "other" convention,
/// or mid-grey — implementer's choice, just be consistent).
///
/// Symbol matching should be case-sensitive as written in SDF
/// (`"Cl"`, `"Br"`), but be lenient about a leading-cap/rest-lower
/// normalization if convenient.
#[must_use]
pub fn cpk_color(symbol: &str) -> [u8; 3] {
    // Normalize to first-letter uppercase, rest lowercase.
    let normalized = normalize_symbol(symbol);
    match normalized.as_str() {
        "H" => [0xFF, 0xFF, 0xFF],  // FFFFFF
        "C" => [0x90, 0x90, 0x90],  // 909090
        "N" => [0x30, 0x50, 0xF8],  // 3050F8
        "O" => [0xFF, 0x0D, 0x0D],  // FF0D0D
        "F" => [0x90, 0xE0, 0x50],  // 90E050
        "P" => [0xFF, 0x80, 0x00],  // FF8000
        "S" => [0xFF, 0xFF, 0x30],  // FFFF30
        "Cl" => [0x1F, 0xF0, 0x1F], // 1FF01F
        "Br" => [0xA6, 0x29, 0x29], // A62929
        "I" => [0x94, 0x00, 0x94],  // 940094
        "B" => [0xFF, 0xB5, 0xB5],  // FFB5B5
        "Si" => [0xF0, 0xC8, 0xA0], // F0C8A0
        "Na" => [0xAB, 0x5C, 0xF2], // AB5CF2
        "K" => [0x8F, 0x40, 0xD4],  // 8F40D4
        "Ca" => [0x3D, 0xFF, 0x00], // 3DFF00
        "Mg" => [0x8A, 0xFF, 0x00], // 8AFF00
        "Fe" => [0xE0, 0x66, 0x33], // E06633
        "Zn" => [0x7D, 0x80, 0xB0], // 7D80B0
        _ => [255, 192, 203],       // CPK pink fallback
    }
}

/// Covalent radius in angstroms for an element symbol. Used to size
/// atom spheres (ball-and-stick scales these down) and as a fallback
/// for bond inference. Unknown → a sane fallback (~0.7 Å).
#[must_use]
pub fn covalent_radius(symbol: &str) -> f32 {
    let normalized = normalize_symbol(symbol);
    match normalized.as_str() {
        "H" => 0.31,
        "C" => 0.76,
        "N" => 0.71,
        "O" => 0.66,
        "F" => 0.57,
        "P" => 1.07,
        "S" => 1.05,
        "Cl" => 1.02,
        "Br" => 1.20,
        "I" => 1.39,
        "B" => 0.84,
        "Si" => 1.11,
        "Na" => 1.66,
        "K" => 2.03,
        "Ca" => 1.76,
        "Mg" => 1.41,
        "Fe" => 1.32,
        "Zn" => 1.22,
        _ => 0.7,
    }
}

/// Normalize a symbol to first-letter uppercase, rest lowercase.
/// E.g. "cl" → "Cl", "CL" → "Cl", "C" → "C".
fn normalize_symbol(symbol: &str) -> String {
    let mut chars = symbol.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => {
            let mut s = first.to_uppercase().to_string();
            s.extend(chars.flat_map(char::to_lowercase));
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_carbon_grey() {
        assert_eq!(cpk_color("C"), [0x90, 0x90, 0x90]);
    }

    #[test]
    fn test_oxygen_red() {
        assert_eq!(cpk_color("O"), [0xFF, 0x0D, 0x0D]);
    }

    #[test]
    fn test_hydrogen_white() {
        assert_eq!(cpk_color("H"), [0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn test_hydrogen_radius() {
        assert!((covalent_radius("H") - 0.31).abs() < 1e-6);
    }

    #[test]
    fn test_carbon_radius() {
        assert!((covalent_radius("C") - 0.76).abs() < 1e-6);
    }

    #[test]
    fn test_oxygen_radius() {
        assert!((covalent_radius("O") - 0.66).abs() < 1e-6);
    }

    #[test]
    fn test_chlorine_color_sdf_case() {
        // SDF spells it "Cl" — must match
        assert_eq!(cpk_color("Cl"), [0x1F, 0xF0, 0x1F]);
    }

    #[test]
    fn test_bromine_color_sdf_case() {
        assert_eq!(cpk_color("Br"), [0xA6, 0x29, 0x29]);
    }

    #[test]
    fn test_unknown_color_fallback() {
        assert_eq!(cpk_color("Xx"), [255, 192, 203]);
        assert_eq!(cpk_color("Q"), [255, 192, 203]);
        assert_eq!(cpk_color(""), [255, 192, 203]);
    }

    #[test]
    fn test_unknown_radius_fallback() {
        assert!((covalent_radius("Xx") - 0.7).abs() < 1e-6);
        assert!((covalent_radius("Q") - 0.7).abs() < 1e-6);
    }

    #[test]
    fn test_case_normalization() {
        // Lowercase input should still match
        assert_eq!(cpk_color("c"), cpk_color("C"));
        assert_eq!(cpk_color("cl"), cpk_color("Cl"));
        assert_eq!(covalent_radius("h"), covalent_radius("H"));
    }

    #[test]
    fn test_iron_color() {
        assert_eq!(cpk_color("Fe"), [0xE0, 0x66, 0x33]);
    }

    #[test]
    fn test_zinc_radius() {
        assert!((covalent_radius("Zn") - 1.22).abs() < 1e-6);
    }
}
