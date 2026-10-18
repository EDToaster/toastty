//! Code highlighting with syntect, coloured from the terminal's ANSI
//! palette so code blocks match the rest of the terminal.

use std::str::FromStr;

use syntect::easy::HighlightLines;
use syntect::highlighting::{
    Color, FontStyle, ScopeSelectors, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::SyntaxSet;
use syntect::util::LinesWithEndings;

use super::style::Palette;
use crate::Rgba;

/// Code bigger than this is shown without highlighting (time guard).
const MAX_HIGHLIGHT_BYTES: usize = 128 * 1024;

/// One highlighted piece of a line.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Tok {
    pub text: String,
    pub color: Rgba,
    pub bold: bool,
    pub italic: bool,
}

/// Syntax definitions plus the last palette-derived theme.
#[derive(Default)]
pub(crate) struct Highlighter {
    syntaxes: Option<SyntaxSet>,
    theme: Option<(Vec<Rgba>, Theme)>,
}

impl std::fmt::Debug for Highlighter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Highlighter").finish_non_exhaustive()
    }
}

fn col(c: Rgba) -> Color {
    Color {
        r: c[0],
        g: c[1],
        b: c[2],
        a: 0xff,
    }
}

fn build_theme(p: &Palette) -> Theme {
    // (scope selector, ANSI index or None = fg/muted, bold, italic)
    let muted = p.muted;
    let rules: &[(&str, Option<usize>, bool, bool)] = &[
        ("comment, punctuation.definition.comment", None, false, true),
        (
            "string, punctuation.definition.string, constant.character.escape",
            Some(2),
            false,
            false,
        ),
        (
            "constant.numeric, constant.language, constant.character, support.constant",
            Some(6),
            false,
            false,
        ),
        (
            "keyword, storage, keyword.operator.word",
            Some(5),
            false,
            false,
        ),
        (
            "keyword.operator, punctuation.separator.key-value",
            Some(5),
            false,
            false,
        ),
        (
            "entity.name.function, support.function, meta.function-call variable.function, variable.function",
            Some(4),
            false,
            false,
        ),
        (
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, support.type, support.class, storage.type.primitive",
            Some(3),
            false,
            false,
        ),
        ("entity.name.tag, meta.tag.sgml", Some(1), false, false),
        ("entity.other.attribute-name", Some(3), false, false),
        (
            "variable.parameter, variable.language",
            Some(1),
            false,
            false,
        ),
        (
            "entity.name.namespace, entity.name.module, meta.path",
            Some(6),
            false,
            false,
        ),
        (
            "meta.attribute, meta.annotation, entity.name.label, keyword.other.attribute",
            Some(6),
            false,
            false,
        ),
        ("markup.heading, entity.name.section", Some(4), true, false),
        ("markup.bold", None, true, false),
        ("markup.italic", None, false, true),
        ("markup.inserted", Some(2), false, false),
        ("markup.deleted", Some(1), false, false),
        (
            "markup.underline.link, string.other.link",
            Some(4),
            false,
            false,
        ),
        ("invalid", Some(1), false, false),
    ];
    let mut scopes = vec![];
    for &(sel, ansi, bold, italic) in rules {
        let Ok(scope) = ScopeSelectors::from_str(sel) else {
            continue;
        };
        let fg = match ansi {
            Some(i) => p.syntax(i),
            None if italic && !bold && sel.starts_with("comment") => muted,
            None => p.fg,
        };
        let mut fs = FontStyle::empty();
        if bold {
            fs |= FontStyle::BOLD;
        }
        if italic {
            fs |= FontStyle::ITALIC;
        }
        scopes.push(ThemeItem {
            scope,
            style: StyleModifier {
                foreground: Some(col(fg)),
                background: None,
                font_style: Some(fs),
            },
        });
    }
    Theme {
        name: Some("toastty-ansi".into()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(col(p.fg)),
            background: Some(col(p.code_bg)),
            ..ThemeSettings::default()
        },
        scopes,
    }
}

impl Highlighter {
    fn syntaxes(&mut self) -> &SyntaxSet {
        self.syntaxes
            .get_or_insert_with(SyntaxSet::load_defaults_newlines)
    }

    /// Whether `lang` names a syntax we know.
    pub(crate) fn knows(&mut self, lang: &str) -> bool {
        !lang.is_empty() && find(self.syntaxes(), lang).is_some()
    }

    /// Highlight `code` (one entry per line, newlines stripped). Unknown
    /// or empty languages come back as plain lines.
    pub(crate) fn highlight(&mut self, code: &str, lang: &str, p: &Palette) -> Vec<Vec<Tok>> {
        let plain = |code: &str| {
            code.lines()
                .map(|l| {
                    vec![Tok {
                        text: l.to_owned(),
                        color: p.fg,
                        bold: false,
                        italic: false,
                    }]
                })
                .collect::<Vec<_>>()
        };
        if lang.is_empty() || code.len() > MAX_HIGHLIGHT_BYTES || !self.knows(lang) {
            return plain(code);
        }
        let key: Vec<Rgba> = p.ansi.iter().copied().chain([p.fg, p.bg]).collect();
        if self.theme.as_ref().is_none_or(|(k, _)| *k != key) {
            self.theme = Some((key, build_theme(p)));
        }
        let ss = self.syntaxes.as_ref().expect("loaded by knows()");
        let theme = &self.theme.as_ref().expect("set above").1;
        let Some(syntax) = find(ss, lang) else {
            return plain(code);
        };
        let mut hl = HighlightLines::new(syntax, theme);
        let mut out = vec![];
        for line in LinesWithEndings::from(code) {
            let Ok(regions) = hl.highlight_line(line, ss) else {
                return plain(code);
            };
            let toks = regions
                .into_iter()
                .filter_map(|(st, text)| {
                    let text = text.trim_end_matches(['\n', '\r']);
                    if text.is_empty() {
                        return None;
                    }
                    let c = st.foreground;
                    Some(Tok {
                        text: text.to_owned(),
                        color: [c.r, c.g, c.b, 0xff],
                        bold: st.font_style.contains(FontStyle::BOLD),
                        italic: st.font_style.contains(FontStyle::ITALIC),
                    })
                })
                .collect();
            out.push(toks);
        }
        out
    }
}

fn find<'a>(ss: &'a SyntaxSet, lang: &str) -> Option<&'a syntect::parsing::SyntaxReference> {
    let alias = match lang.to_ascii_lowercase().as_str() {
        "sh" | "shell" | "zsh" | "console" => "bash".to_owned(),
        "js" | "jsx" | "javascript" | "mjs" => "js".to_owned(),
        "ts" | "typescript" | "tsx" => "ts".to_owned(),
        "py" | "python3" => "py".to_owned(),
        "yml" => "yaml".to_owned(),
        "toml" => "toml".to_owned(),
        "c++" | "cxx" => "cpp".to_owned(),
        "rs" => "rust".to_owned(),
        other => other.to_owned(),
    };
    ss.find_syntax_by_token(&alias)
        .or_else(|| ss.find_syntax_by_token(lang))
        .filter(|s| s.name != "Plain Text")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Theme as T;

    #[test]
    fn rust_gets_coloured_unknown_is_plain() {
        let p = Palette::new(&T::default());
        let mut h = Highlighter::default();
        let lines = h.highlight("fn main() {\n    let s = \"hi\"; // c\n}\n", "rust", &p);
        assert_eq!(lines.len(), 3);
        let colors: std::collections::HashSet<Rgba> =
            lines.iter().flatten().map(|t| t.color).collect();
        assert!(colors.len() >= 3, "{colors:?}");
        let s: String = lines[1].iter().map(|t| t.text.as_str()).collect();
        assert_eq!(s, "    let s = \"hi\"; // c");
        let plain = h.highlight("a\nb", "no-such-lang", &p);
        assert_eq!(plain.len(), 2);
        assert_eq!(plain[0][0].color, p.fg);
    }
}
