//! `[selection]` table — mouse text selection behaviour.
//!
//! ```toml
//! [selection]
//! # Characters that count as part of a word for double-click
//! # selection, *in addition to* Unicode letters and digits.
//! word_chars = "_-./~:?&=%+#@"
//! ```
//!
//! Letters and digits from any script (`é`, `ß`, `日`, `٣`, …) are
//! always word characters; `word_chars` adds punctuation on top. The
//! default keeps file paths (`~/src/main.rs`), URLs
//! (`https://example.com/a?b=c&d=%20#top`) and e-mail addresses
//! together, similar to kitty's `select_by_word_characters`
//! (`@-./_~?&=%+#`) with `:` added for URL schemes and `file:line`
//! locations. Set it to `""` for letters/digits only, or `"_"` for the
//! classic identifier rule.

use serde::{Deserialize, Serialize};

/// Default extra word characters. See the module docs.
pub const DEFAULT_WORD_CHARS: &str = "_-./~:?&=%+#@";

/// Mouse-selection config.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SelectionConfig {
    /// Characters treated as word characters by double-click
    /// selection, in addition to Unicode alphanumerics.
    pub word_chars: String,
}

impl SelectionConfig {
    /// Schema defaults — [`DEFAULT_WORD_CHARS`].
    #[must_use]
    pub fn defaults() -> Self {
        Self {
            word_chars: DEFAULT_WORD_CHARS.to_owned(),
        }
    }

    /// True when `c` belongs to a word for double-click selection:
    /// any Unicode alphanumeric, or one of [`Self::word_chars`].
    #[must_use]
    pub fn is_word_char(&self, c: char) -> bool {
        c.is_alphanumeric() || self.word_chars.contains(c)
    }
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self::defaults()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_include_path_and_url_chars() {
        let d = SelectionConfig::defaults();
        assert_eq!(d, SelectionConfig::default());
        for c in "_-./~:?&=%+#@".chars() {
            assert!(d.is_word_char(c), "{c:?}");
        }
        for c in [
            ' ', '\t', '"', '\'', '(', ')', '[', ']', '<', '>', '|', ',', ';',
        ] {
            assert!(!d.is_word_char(c), "{c:?}");
        }
    }

    #[test]
    fn unicode_alphanumerics_are_always_word_chars() {
        let none = SelectionConfig {
            word_chars: String::new(),
        };
        for c in ['a', 'Z', '0', 'é', 'ß', '日', 'ж', '٣'] {
            assert!(none.is_word_char(c), "{c:?}");
        }
        assert!(!none.is_word_char('_'));
        assert!(!none.is_word_char('/'));
    }

    #[test]
    fn round_trip_and_partial_table() {
        let c = SelectionConfig {
            word_chars: "_".into(),
        };
        let t = toml::to_string(&c).unwrap();
        let p: SelectionConfig = toml::from_str(&t).unwrap();
        assert_eq!(p, c);
        let empty: SelectionConfig = toml::from_str("").unwrap();
        assert_eq!(empty, SelectionConfig::defaults());
        assert!(toml::from_str::<SelectionConfig>("bogus = 1").is_err());
    }
}
