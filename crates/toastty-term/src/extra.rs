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
//! *distinct* live extras, not by grid size. It is capped at
//! [`MAX_ENTRIES`] distinct values; when it fills, `Term` frees the
//! entries no cell refers to any more (see `Term::intern_extra`), and
//! only if everything is still in use are new extras dropped
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

/// Most live [`CellExtra::Placeholder`] entries the table holds. The
/// rest of [`MAX_ENTRIES`] stays reserved for grapheme tails and tab
/// markers, so a scrollback full of distinct placeholder images can never
/// starve combining marks. `Term` keeps placeholders under this by
/// reclaiming, and if need be by dropping the image data of the oldest
/// scrollback rows (see `Term::intern_extra`).
pub(crate) const MAX_PLACEHOLDER_ENTRIES: usize = MAX_ENTRIES / 4 * 3;

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

    /// The diacritic codepoints this cell was printed with, in stream
    /// order (row, column, id-MSB; stops at the first one not given).
    pub fn diacritics(&self) -> impl Iterator<Item = char> {
        [self.row, self.col, self.id_msb]
            .into_iter()
            .take_while(|&v| v != 0)
            .filter_map(|v| toastty_graphics::kitty::placeholder::index_to_diacritic(v - 1))
    }
}

/// Deduplicating intern table for [`CellExtra`] values.
///
/// Ids are recycled: [`ExtraTable::retain`] frees the entries no cell
/// refers to any more (the owner — [`crate::Term`] — finds those by
/// scanning its grids when the table fills up), and later interns reuse
/// the freed ids.
#[derive(Debug, Default)]
pub struct ExtraTable {
    /// Slot `id - 1`; `None` = freed, its id is on `free`.
    entries: Vec<Option<CellExtra>>,
    index: HashMap<CellExtra, CellExtraId>,
    free: Vec<CellExtraId>,
    /// Live entries that are [`CellExtra::Placeholder`].
    placeholders: usize,
}

impl ExtraTable {
    /// Intern `extra`, returning its id. Returns `None` once the table
    /// holds [`MAX_ENTRIES`] distinct live values (or, for a placeholder,
    /// [`MAX_PLACEHOLDER_ENTRIES`] placeholders) and `extra` isn't one of
    /// them — callers then fall back to dropping the extra.
    pub fn intern(&mut self, extra: CellExtra) -> Option<CellExtraId> {
        if let Some(&id) = self.index.get(&extra) {
            return Some(id);
        }
        let placeholder = matches!(extra, CellExtra::Placeholder(_));
        if placeholder && self.placeholders >= MAX_PLACEHOLDER_ENTRIES {
            return None;
        }
        let id = if let Some(id) = self.free.pop() {
            self.entries[usize::from(id.get()) - 1] = Some(extra.clone());
            id
        } else {
            if self.entries.len() >= MAX_ENTRIES {
                return None;
            }
            self.entries.push(Some(extra.clone()));
            // `len()` is 1..=MAX_ENTRIES here, so both conversions succeed.
            u16::try_from(self.entries.len())
                .ok()
                .and_then(NonZeroU16::new)?
        };
        self.index.insert(extra, id);
        if placeholder {
            self.placeholders += 1;
        }
        Some(id)
    }

    /// True when [`ExtraTable::intern`] would refuse `extra`: the table is
    /// full and `extra` isn't already in it.
    #[must_use]
    pub fn would_reject(&self, extra: &CellExtra) -> bool {
        let full = if matches!(extra, CellExtra::Placeholder(_)) {
            self.placeholders >= MAX_PLACEHOLDER_ENTRIES
                || (self.free.is_empty() && self.entries.len() >= MAX_ENTRIES)
        } else {
            self.free.is_empty() && self.entries.len() >= MAX_ENTRIES
        };
        full && !self.index.contains_key(extra)
    }

    /// Number of live [`CellExtra::Placeholder`] entries.
    #[must_use]
    pub fn placeholder_count(&self) -> usize {
        self.placeholders
    }

    /// Free every entry whose id `live` returns `false` for. Returns the
    /// number of entries freed. The caller must have checked every cell
    /// that could still hold an id: a freed id is handed out again.
    pub fn retain(&mut self, mut live: impl FnMut(CellExtraId) -> bool) -> usize {
        let mut freed = 0;
        for (i, slot) in self.entries.iter_mut().enumerate() {
            let Some(id) = u16::try_from(i + 1).ok().and_then(NonZeroU16::new) else {
                continue;
            };
            if slot.is_some() && !live(id) {
                if let Some(old) = slot.take() {
                    if matches!(old, CellExtra::Placeholder(_)) {
                        self.placeholders -= 1;
                    }
                    self.index.remove(&old);
                }
                self.free.push(id);
                freed += 1;
            }
        }
        freed
    }

    /// Resolve an id. `None` for ids this table never issued (or freed).
    #[must_use]
    pub fn get(&self, id: CellExtraId) -> Option<&CellExtra> {
        self.entries
            .get(usize::from(id.get()) - 1)
            .and_then(Option::as_ref)
    }

    /// The grapheme tail for `id`, or `""` for a tab marker / unknown id.
    #[must_use]
    pub fn grapheme_tail(&self, id: Option<CellExtraId>) -> &str {
        match id.and_then(|id| self.get(id)) {
            Some(CellExtra::Grapheme(s)) => s,
            _ => "",
        }
    }

    /// Number of live (interned, not freed) entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len() - self.free.len()
    }

    /// True when no entry is live.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
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
    fn retain_frees_and_recycles_ids() {
        let mut t = ExtraTable::default();
        let a = t.intern(CellExtra::Tab(1)).unwrap();
        let b = t.intern(CellExtra::Tab(2)).unwrap();
        assert_eq!(t.retain(|id| id == b), 1);
        assert_eq!(t.len(), 1);
        assert_eq!(t.get(a), None, "freed id resolves to nothing");
        assert_eq!(t.get(b), Some(&CellExtra::Tab(2)));
        // The freed id is reused; the old value is no longer deduped to it.
        let c = t.intern(CellExtra::Tab(3)).unwrap();
        assert_eq!(c, a);
        assert_eq!(t.intern(CellExtra::Tab(1)).map(|id| id == a), Some(false));
    }

    #[test]
    fn full_table_accepts_again_after_retain() {
        let mut t = ExtraTable::default();
        for span in 0..=u16::MAX {
            if t.len() == MAX_ENTRIES {
                break;
            }
            t.intern(CellExtra::Tab(span));
        }
        let x = CellExtra::Grapheme("x".into());
        assert!(t.would_reject(&x));
        assert!(!t.would_reject(&CellExtra::Tab(0)), "already present");
        t.retain(|id| id.get() % 2 == 0);
        assert!(!t.would_reject(&x));
        assert!(t.intern(x).is_some());
    }

    #[test]
    fn placeholders_stop_at_their_quota() {
        let mut t = ExtraTable::default();
        let ph = |n: usize| {
            CellExtra::Placeholder(PlaceholderExtra {
                placement_id: u32::try_from(n).unwrap(),
                ..PlaceholderExtra::default()
            })
        };
        for n in 0..MAX_PLACEHOLDER_ENTRIES {
            assert!(t.intern(ph(n)).is_some());
        }
        assert_eq!(t.placeholder_count(), MAX_PLACEHOLDER_ENTRIES);
        assert!(t.would_reject(&ph(usize::MAX >> 40)));
        assert_eq!(t.intern(ph(1 << 20)), None, "over the placeholder quota");
        assert!(t.intern(ph(5)).is_some(), "existing values still resolve");
        // The reserve: graphemes and tabs still go in.
        let g = CellExtra::Grapheme("\u{301}".into());
        assert!(!t.would_reject(&g));
        assert!(t.intern(g).is_some());
        // Freeing placeholders gives the quota back.
        assert_eq!(t.retain(|id| id.get() > 10), 10);
        assert_eq!(t.placeholder_count(), MAX_PLACEHOLDER_ENTRIES - 10);
        assert!(t.intern(ph(1 << 20)).is_some());
    }

    #[test]
    fn option_extra_id_niche_packs_to_two_bytes() {
        assert_eq!(std::mem::size_of::<Option<CellExtraId>>(), 2);
    }
}
