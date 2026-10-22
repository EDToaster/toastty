//! Per-cell "extra" payloads that don't fit in [`Cell`](crate::Cell)'s
//! single `char`: the trailing codepoints of a multi-codepoint grapheme
//! cluster (combining marks, ZWJ emoji sequences, variation selectors,
//! regional-indicator pairs under DECSET 2027), the horizontal-tab
//! marker that lets copy reproduce a literal `\t`, and the diacritics +
//! placement id of a kitty Unicode-placeholder cell.
//!
//! # Why an intern table
//!
//! The overwhelmingly common cell is a single codepoint. Storing the
//! extras inline (a `String` / `SmallVec` per cell) would multiply the
//! grid's memory footprint for the rare cell that needs them. Instead
//! each cell carries an `Option<CellExtraId>` (`NonZeroU16`, so the
//! `Option` niche-packs to 2 bytes — the same trick as
//! [`HyperlinkId`](crate::HyperlinkId)) that indexes this table.
//!
//! Because the id lives *in* the cell, every existing cell move — ICH /
//! DCH shifts, scroll-region copies, scrollback eviction, reflow on
//! resize, erase (which writes a fresh blank with `extra: None`) — keeps
//! the grapheme attached to the right cell for free. A per-row side
//! table keyed by column would need every one of those paths to be
//! taught to shift / clear it.
//!
//! Entries are deduplicated (`"\u{301}"` is stored once no matter how
//! many cells carry it), so the table size is bounded by the number of
//! *distinct* extras a session ever sees, not by grid size. It is capped
//! at [`MAX_ENTRIES`] distinct values; past that, new extras are dropped
//! (the pre-fix behaviour) rather than growing memory without bound.
//! Each grapheme's extra text is likewise capped at
//! [`MAX_GRAPHEME_EXTRA_BYTES`] so a stream of combining marks can't
//! build one unbounded entry (kitty / Ghostty cap cluster length too).

use std::collections::HashMap;
use std::num::NonZeroU16;

/// Intern-table identifier for a [`CellExtra`].
pub type CellExtraId = NonZeroU16;

/// Maximum number of distinct extras retained per terminal.
pub(crate) const MAX_ENTRIES: usize = u16::MAX as usize;

/// Maximum UTF-8 length of the codepoints attached after a cell's base
/// `char`. 64 bytes comfortably fits the longest RGI emoji ZWJ
/// sequences (a skin-toned family is ~35 bytes after the first emoji)
/// and stacks of several combining marks.
pub(crate) const MAX_GRAPHEME_EXTRA_BYTES: usize = 64;

/// What a cell's extra id resolves to.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CellExtra {
    /// Codepoints that follow `Cell::ch` in the same grapheme cluster,
    /// in stream order (e.g. `"\u{301}"` for a decomposed `é`, or
    /// `"\u{200D}👩\u{200D}👧"` for the tail of a family emoji).
    Grapheme(Box<str>),
    /// This cell is where a horizontal tab (HT) started, and the tab
    /// advanced the cursor over `span` columns (this one included) that
    /// were all blank at the time. Copy emits a literal `\t` for the run
    /// as long as those cells are still blank — matching kitty and
    /// Ghostty, which preserve tabs in copied text (xterm copies spaces).
    Tab(u16),
    /// The per-cell data of a kitty Unicode-placeholder cell
    /// (`Cell::ch == U+10EEEE`) that the cell's style doesn't already
    /// hold. The image id's low bits are the cell's foreground colour.
    /// A placeholder cell with no extra is a bare cell with placement
    /// id 0.
    Placeholder(PlaceholderExtra),
}

/// See [`CellExtra::Placeholder`]. The diacritic fields hold the
/// diacritic's table index **plus one**, so `0` means "not given" and
/// the value is inherited from the cell to the left at render time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct PlaceholderExtra {
    /// Placement id, from the SGR 58 underline colour when the cell was
    /// printed (`0` = any virtual placement of the image).
    pub placement_id: u32,
    /// First diacritic: the image row.
    pub row: u16,
    /// Second diacritic: the image column.
    pub col: u16,
    /// Third diacritic: bits 24..32 of the image id.
    pub id_msb: u16,
}

impl PlaceholderExtra {
    /// Record the next diacritic (table index `idx`) in stream order.
    /// Returns `false` when all three slots are already taken.
    pub fn push_diacritic(&mut self, idx: u16) -> bool {
        let v = idx.saturating_add(1);
        if self.row == 0 {
            self.row = v;
        } else if self.col == 0 {
            self.col = v;
        } else if self.id_msb == 0 {
            self.id_msb = v;
        } else {
            return false;
        }
        true
    }
}

/// Deduplicating intern table for [`CellExtra`] values.
#[derive(Debug, Default)]
pub struct ExtraTable {
    entries: Vec<CellExtra>,
    index: HashMap<CellExtra, CellExtraId>,
}

impl ExtraTable {
    /// Intern `extra`, returning its id. Returns `None` once the table
    /// holds [`MAX_ENTRIES`] distinct values and `extra` isn't one of
    /// them — callers then fall back to dropping the extra.
    pub fn intern(&mut self, extra: CellExtra) -> Option<CellExtraId> {
        if let Some(&id) = self.index.get(&extra) {
            return Some(id);
        }
        if self.entries.len() >= MAX_ENTRIES {
            return None;
        }
        self.entries.push(extra.clone());
        // `len()` is 1..=MAX_ENTRIES here, so both conversions succeed.
        let id = u16::try_from(self.entries.len())
            .ok()
            .and_then(NonZeroU16::new)?;
        self.index.insert(extra, id);
        Some(id)
    }

    /// Resolve an id. `None` for ids this table never issued.
    #[must_use]
    pub fn get(&self, id: CellExtraId) -> Option<&CellExtra> {
        self.entries.get(usize::from(id.get()) - 1)
    }

    /// The grapheme tail for `id`, or `""` for a tab marker / unknown id.
    #[must_use]
    pub fn grapheme_tail(&self, id: Option<CellExtraId>) -> &str {
        match id.and_then(|id| self.get(id)) {
            Some(CellExtra::Grapheme(s)) => s,
            _ => "",
        }
    }

    /// Number of distinct entries interned so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing has been interned yet.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intern_dedupes_and_resolves() {
        let mut t = ExtraTable::default();
        assert!(t.is_empty());
        let a = t.intern(CellExtra::Grapheme("\u{301}".into())).unwrap();
        let b = t.intern(CellExtra::Grapheme("\u{301}".into())).unwrap();
        let c = t.intern(CellExtra::Tab(4)).unwrap();
        assert_eq!(a, b);
        assert_ne!(a, c);
        assert_eq!(t.len(), 2);
        assert_eq!(t.get(a), Some(&CellExtra::Grapheme("\u{301}".into())));
        assert_eq!(t.get(c), Some(&CellExtra::Tab(4)));
        assert_eq!(t.grapheme_tail(Some(a)), "\u{301}");
        assert_eq!(t.grapheme_tail(Some(c)), "");
        assert_eq!(t.grapheme_tail(None), "");
    }

    #[test]
    fn unknown_id_resolves_to_none() {
        let t = ExtraTable::default();
        assert_eq!(t.get(NonZeroU16::new(7).unwrap()), None);
        assert_eq!(t.grapheme_tail(NonZeroU16::new(7)), "");
    }

    #[test]
    fn table_is_capped() {
        let mut t = ExtraTable::default();
        for span in 0..=u16::MAX {
            if t.len() == MAX_ENTRIES {
                break;
            }
            assert!(t.intern(CellExtra::Tab(span)).is_some());
        }
        assert_eq!(t.len(), MAX_ENTRIES);
        // Full: a new value is refused, an existing one still resolves.
        assert_eq!(t.intern(CellExtra::Grapheme("x".into())), None);
        assert!(t.intern(CellExtra::Tab(0)).is_some());
    }

    #[test]
    fn option_extra_id_niche_packs_to_two_bytes() {
        assert_eq!(std::mem::size_of::<Option<CellExtraId>>(), 2);
    }
}
