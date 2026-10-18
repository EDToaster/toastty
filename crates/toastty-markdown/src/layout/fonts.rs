//! Font roles (body sans, code monospace), bundled fallbacks and the
//! per-face vertical metrics layout needs.

use cosmic_text::{FontSystem, Style, Weight, fontdb};

const NOTO_REGULAR: &[u8] = include_bytes!("../../fonts/NotoSans-Regular.ttf");
const NOTO_BOLD: &[u8] = include_bytes!("../../fonts/NotoSans-Bold.ttf");
const NOTO_ITALIC: &[u8] = include_bytes!("../../fonts/NotoSans-Italic.ttf");
const NOTO_BOLD_ITALIC: &[u8] = include_bytes!("../../fonts/NotoSans-BoldItalic.ttf");
const FIRA_MONO: &[u8] = include_bytes!("../../fonts/FiraMono-Medium.ttf");

/// Family name of the bundled body font.
pub(crate) const BUNDLED_SANS: &str = "Noto Sans";
/// Family name of the bundled code font (single Medium face).
pub(crate) const BUNDLED_MONO: &str = "Fira Mono";

/// Vertical metrics of a face, in em units (positive values).
#[derive(Debug, Clone, Copy)]
pub(crate) struct FaceMetrics {
    pub ascent: f32,
    pub descent: f32,
    pub underline_offset: f32,
    pub underline_thickness: f32,
    pub x_height: f32,
}

impl FaceMetrics {
    const FALLBACK: Self = Self {
        ascent: 0.95,
        descent: 0.25,
        underline_offset: 0.1,
        underline_thickness: 0.06,
        x_height: 0.53,
    };
}

/// One resolved font role: a concrete family name plus the weight to
/// request for "regular" text (Fira Mono only ships Medium, and
/// cosmic-text only matches a named family when the weight matches).
#[derive(Debug, Clone)]
pub(crate) struct Role {
    pub family: String,
    pub regular: Weight,
    pub metrics: FaceMetrics,
}

#[derive(Debug, Clone)]
pub(crate) struct Fonts {
    pub sans: Role,
    pub mono: Role,
}

/// Add the bundled faces to `fonts` (harmless if already present) and
/// pick the families for each role: the database's generic sans-serif /
/// monospace family when it has such a face, else the bundled one.
pub(crate) fn resolve(fonts: &mut FontSystem) -> Fonts {
    let db = fonts.db_mut();
    let have_bundled = db.faces().any(|f| {
        f.families.iter().any(|(n, _)| n == BUNDLED_SANS)
            && f.post_script_name.starts_with("NotoSans-")
    });
    if !have_bundled {
        for data in [NOTO_REGULAR, NOTO_BOLD, NOTO_ITALIC, NOTO_BOLD_ITALIC] {
            db.load_font_data(data.to_vec());
        }
    }
    if !db.faces().any(|f| f.post_script_name == "FiraMono-Medium") {
        db.load_font_data(FIRA_MONO.to_vec());
    }
    let sans = pick(
        fonts,
        fontdb::Family::SansSerif,
        SANS_CANDIDATES,
        BUNDLED_SANS,
    );
    let mono = pick(
        fonts,
        fontdb::Family::Monospace,
        MONO_CANDIDATES,
        BUNDLED_MONO,
    );
    Fonts { sans, mono }
}

/// Common system families tried (after the database's generic family)
/// before falling back to the bundled face.
const SANS_CANDIDATES: &[&str] = &[
    "SF Pro Text",
    "Helvetica Neue",
    "Segoe UI",
    "Noto Sans",
    "DejaVu Sans",
    "Liberation Sans",
    "Arial",
];
const MONO_CANDIDATES: &[&str] = &[
    "SF Mono",
    "Menlo",
    "Noto Sans Mono",
    "DejaVu Sans Mono",
    "Consolas",
    "Liberation Mono",
];

fn pick(
    fonts: &mut FontSystem,
    generic: fontdb::Family<'_>,
    candidates: &[&str],
    bundled: &str,
) -> Role {
    let db = fonts.db();
    let generic_name = db.family_name(&generic).to_owned();
    // The face's own spelling of `name`, if any face has that family.
    let find = |name: &str| {
        db.faces().find_map(|f| {
            f.families
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(n, _)| n.clone())
        })
    };
    let family = std::iter::once(generic_name.as_str())
        .chain(candidates.iter().copied())
        .filter(|n| !n.is_empty())
        .find_map(find)
        .unwrap_or_else(|| bundled.to_owned());
    let (regular, id) = resolve_weight(fonts.db(), &family);
    let metrics =
        id.and_then(|id| fonts.get_font(id, regular))
            .map_or(FaceMetrics::FALLBACK, |font| {
                let m = font.metrics();
                let upem = f32::from(m.units_per_em.max(1));
                let (uo, ut) = m
                    .underline
                    .map_or((0.1, 0.06), |d| (-d.offset / upem, d.thickness / upem));
                FaceMetrics {
                    ascent: m.ascent / upem,
                    descent: (-m.descent / upem).max(0.0),
                    underline_offset: uo.max(0.05),
                    underline_thickness: ut.clamp(0.03, 0.12),
                    x_height: m.x_height.map_or(0.53, |x| x / upem),
                }
            });
    Role {
        family,
        regular,
        metrics,
    }
}

/// The weight of the face the database picks for `family` at the
/// default (normal) query, so single-weight faces such as Fira Mono
/// Medium are requested with the weight they actually have.
fn resolve_weight(db: &fontdb::Database, family: &str) -> (Weight, Option<fontdb::ID>) {
    let normal = db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight: Weight::NORMAL,
        stretch: fontdb::Stretch::Normal,
        style: Style::Normal,
    });
    match normal.and_then(|id| db.face(id).map(|f| (id, f.weight))) {
        Some((id, w)) => (w, Some(id)),
        None => (Weight::NORMAL, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hermetic_db_resolves_bundled_faces() {
        let mut fs = FontSystem::new_with_locale_and_db("en-US".into(), fontdb::Database::new());
        let f = resolve(&mut fs);
        assert_eq!(f.sans.family, BUNDLED_SANS);
        assert_eq!(f.sans.regular, Weight::NORMAL);
        assert_eq!(f.mono.family, BUNDLED_MONO);
        assert_eq!(f.mono.regular, Weight::MEDIUM);
        assert!(f.sans.metrics.ascent > 0.9 && f.sans.metrics.ascent < 1.2);
        assert!(f.mono.metrics.descent > 0.1);
        // Loading twice does not duplicate faces.
        let n = fs.db().len();
        let _ = resolve(&mut fs);
        assert_eq!(fs.db().len(), n);
    }
}
