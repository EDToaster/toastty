//! Unicode placeholder + diacritic decoding for the Kitty graphics
//! protocol.
//!
//! Apps emit a `U+10EEEE` "image placeholder" codepoint followed by up
//! to three combining diacritics that encode `(row, col, image_id_high)`
//! values 0..N. The base placeholder + the SGR foreground (the low 8 or
//! 24 bits of the image id) + the SGR underline colour (the placement
//! id) + the diacritics fully address a slice of a virtual placement.
//!
//! This module holds the protocol-level pieces: the diacritic table,
//! the per-screen [`VirtualPlacements`] table, and the fit geometry
//! ([`placeholder_slice`]). The cells themselves live in the terminal
//! grid like any text; `toastty-term` resolves runs of them and the
//! renderer draws them every frame.
//!
//! Reference:
//! <https://sw.kovidgoyal.net/kitty/graphics-protocol/#unicode-placeholders>.
//! The diacritic list comes from kitty's published table.

/// The base placeholder codepoint.
pub const PLACEHOLDER: char = '\u{10EEEE}';

/// Returns the index assigned to `c` by kitty's diacritic table, or
/// `None` if `c` is not a recognized placeholder diacritic.
#[must_use]
pub fn diacritic_to_index(c: char) -> Option<u16> {
    let cp = c as u32;
    DIACRITICS
        .iter()
        .position(|&d| d == cp)
        .map(|p| u16::try_from(p).expect("table fits in u16"))
}

/// Returns the codepoint at index `i`, or `None` if out of range.
#[must_use]
pub fn index_to_diacritic(i: u16) -> Option<char> {
    let idx = i as usize;
    DIACRITICS.get(idx).copied().and_then(char::from_u32)
}

/// Number of recognized diacritics.
#[must_use]
pub fn diacritic_count() -> usize {
    DIACRITICS.len()
}

/// Returns true iff `c` is a recognized placeholder diacritic.
#[must_use]
pub fn is_diacritic(c: char) -> bool {
    diacritic_to_index(c).is_some()
}

/// Returns true iff `c` is the placeholder base codepoint.
#[must_use]
pub fn is_placeholder(c: char) -> bool {
    c == PLACEHOLDER
}

/// A virtual placement, created with `U=1` on `a=T` / `a=p`.
///
/// It is invisible. It's the prototype that placeholder cells refer to:
/// the image is fitted, aspect-preserving and centred, into a box of
/// `cols × rows` cells, and each placeholder cell shows its slice of
/// that box (see [`placeholder_slice`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VirtualPlacement {
    pub image_id: u32,
    /// Client placement id (`p=`); `0` when the client gave none.
    pub placement_id: u32,
    /// Box width in cells (`c=`). `0` means unset: the image's natural
    /// width, `ceil(img_w / cell_w)`, resolved at render time.
    pub cols: u32,
    /// Box height in cells (`r=`). `0` means unset, as for `cols`.
    pub rows: u32,
    /// Z-index (`z=`) of the images drawn over the placeholder cells.
    pub z: i32,
}

/// Upper bound on stored virtual placements. Anonymous ones (`p=0`)
/// accumulate like kitty's, so a client that re-sends `a=p,U=1` in a
/// loop would otherwise grow the table without bound; past the cap the
/// oldest entry is dropped.
pub const MAX_VIRTUAL_PLACEMENTS: usize = 4096;

/// The virtual placements of one screen, in creation order.
///
/// Placeholder cells look their placement up here at render time, so
/// the table only has to be right when a frame is built: placeholders
/// printed before the image (or its virtual placement) arrives show up
/// as soon as it does.
#[derive(Debug, Clone, Default)]
pub struct VirtualPlacements {
    entries: Vec<VirtualPlacement>,
}

impl VirtualPlacements {
    /// Add `vp`. A non-zero placement id replaces the existing
    /// placement with the same `(image, placement)` in place, as for
    /// ordinary placements. Anonymous placements (`p=0`) always add a
    /// new entry, matching kitty.
    pub fn insert(&mut self, vp: VirtualPlacement) {
        if vp.placement_id != 0
            && let Some(e) = self
                .entries
                .iter_mut()
                .find(|e| e.image_id == vp.image_id && e.placement_id == vp.placement_id)
        {
            *e = vp;
            return;
        }
        if self.entries.len() >= MAX_VIRTUAL_PLACEMENTS {
            self.entries.remove(0);
        }
        self.entries.push(vp);
    }

    /// The virtual placement a placeholder cell refers to. A non-zero
    /// `placement_id` must match exactly. `0` picks the image's first
    /// virtual placement (the spec lets the terminal choose any; kitty
    /// takes the oldest).
    #[must_use]
    pub fn find(&self, image_id: u32, placement_id: u32) -> Option<&VirtualPlacement> {
        self.entries.iter().find(|e| {
            e.image_id == image_id && (placement_id == 0 || e.placement_id == placement_id)
        })
    }

    /// True when any virtual placement refers to `image_id`.
    #[must_use]
    pub fn contains_image(&self, image_id: u32) -> bool {
        self.entries.iter().any(|e| e.image_id == image_id)
    }

    /// Remove every entry matching `pred`. Returns `true` when anything
    /// was removed.
    pub fn remove_where(&mut self, mut pred: impl FnMut(&VirtualPlacement) -> bool) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| !pred(e));
        self.entries.len() != before
    }

    /// Remove every virtual placement of `image_id`.
    pub fn remove_image(&mut self, image_id: u32) -> bool {
        self.remove_where(|e| e.image_id == image_id)
    }

    /// Iterate in creation order.
    pub fn iter(&self) -> impl Iterator<Item = &VirtualPlacement> {
        self.entries.iter()
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

/// What a run of placeholder cells draws: a destination rectangle and
/// the matching part of the image.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlaceholderSlice {
    /// Top-left of the drawn rectangle, in pixels, relative to the
    /// top-left corner of the run's first cell.
    pub dst_pos: [f32; 2],
    /// Size of the drawn rectangle in pixels. Never larger than the run.
    pub dst_size: [f32; 2],
    /// Normalised (0..1) image coordinates of the rectangle's corners.
    pub uv_min: [f32; 2],
    pub uv_max: [f32; 2],
}

/// Where the image lands in a run of `n_cols` placeholder cells that
/// show image cell `(img_row, img_col .. img_col + n_cols)` of a virtual
/// placement `vp_cols × vp_rows` cells big.
///
/// Matches kitty (`grman_put_cell_image`):
///
/// - A `0` box dimension falls back to the image's natural size in
///   cells, `ceil(img_dim / cell_dim)`, independently per axis.
/// - The image is fitted into the box with its aspect ratio kept and
///   centred on the other axis (letterbox / pillarbox).
/// - The run shows its part of the box. Whatever part of the run lies
///   outside the fitted image stays blank, so the result is cropped to
///   the image, never stretched. `None` when nothing of the image falls
///   inside the run.
#[must_use]
#[allow(clippy::cast_precision_loss)] // image / box sizes are far below 2^24.
pub fn placeholder_slice(
    img_size: (u32, u32),
    vp_size: (u32, u32),
    cell: (f32, f32),
    img_row: u32,
    img_col: u32,
    n_cols: u32,
) -> Option<PlaceholderSlice> {
    let (img_w, img_h) = img_size;
    let (cw, ch) = cell;
    if img_w == 0 || img_h == 0 || n_cols == 0 || cw <= 0.0 || ch <= 0.0 {
        return None;
    }
    let (iw, ih) = (img_w as f32, img_h as f32);
    let natural = |dim: f32, cell: f32| (dim / cell).ceil().max(1.0);
    let box_cols = if vp_size.0 == 0 {
        natural(iw, cw)
    } else {
        vp_size.0 as f32
    };
    let box_rows = if vp_size.1 == 0 {
        natural(ih, ch)
    } else {
        vp_size.1 as f32
    };
    let (box_w, box_h) = (box_cols * cw, box_rows * ch);
    // Fit: scale so the image touches the box on one axis, then centre
    // it on the other. Same comparison as kitty, cross-multiplied.
    let (scale, off_x, off_y) = if iw * box_h > ih * box_w {
        let s = box_w / iw;
        (s, 0.0, (box_h - ih * s) / 2.0)
    } else {
        let s = box_h / ih;
        (s, (box_w - iw * s) / 2.0, 0.0)
    };
    // The run and the fitted image, both in box pixels.
    let run_left = img_col as f32 * cw;
    let run_top = img_row as f32 * ch;
    let left = run_left.max(off_x);
    let top = run_top.max(off_y);
    let right = (run_left + n_cols as f32 * cw).min(off_x + iw * scale);
    let bottom = (run_top + ch).min(off_y + ih * scale);
    // Sub-pixel slivers (float noise at an exact edge) draw nothing.
    if right - left < 0.01 || bottom - top < 0.01 {
        return None;
    }
    let to_u = |x: f32| ((x - off_x) / (iw * scale)).clamp(0.0, 1.0);
    let to_v = |y: f32| ((y - off_y) / (ih * scale)).clamp(0.0, 1.0);
    Some(PlaceholderSlice {
        dst_pos: [left - run_left, top - run_top],
        dst_size: [right - left, bottom - top],
        uv_min: [to_u(left), to_v(top)],
        uv_max: [to_u(right), to_v(bottom)],
    })
}

/// Kitty's published placeholder diacritic table.
///
/// Index `i` maps to codepoint `DIACRITICS[i]`. The list is taken from
/// `<https://sw.kovidgoyal.net/kitty/_downloads/f0a0de9ec8d9ff4456206db8e0814937/rowcolumn-diacritics.txt>`.
/// The exact ordering matters: index 0 is the first usable position.
const DIACRITICS: &[u32] = &[
    0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F, 0x0346, 0x034A, 0x034B, 0x034C,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035B, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036A, 0x036B, 0x036C, 0x036D, 0x036E, 0x036F, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059C, 0x059D, 0x059E, 0x059F, 0x05A0, 0x05A1,
    0x05A8, 0x05A9, 0x05AB, 0x05AC, 0x05AF, 0x05C4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065A, 0x065B, 0x065D, 0x065E, 0x06D6, 0x06D7, 0x06D8,
    0x06D9, 0x06DA, 0x06DB, 0x06DC, 0x06DF, 0x06E0, 0x06E1, 0x06E2, 0x06E4, 0x06E7, 0x06E8, 0x06EB,
    0x06EC, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073A, 0x073D, 0x073F, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074A, 0x07EB, 0x07EC, 0x07ED, 0x07EE, 0x07EF, 0x07F0, 0x07F1, 0x07F3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081B, 0x081C, 0x081D, 0x081E, 0x081F, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082A, 0x082B, 0x082C, 0x082D, 0x0951, 0x0953, 0x0954,
    0x0F82, 0x0F83, 0x0F86, 0x0F87, 0x135D, 0x135E, 0x135F, 0x17DD, 0x193A, 0x1A17, 0x1A75, 0x1A76,
    0x1A77, 0x1A78, 0x1A79, 0x1A7A, 0x1A7B, 0x1A7C, 0x1B6B, 0x1B6D, 0x1B6E, 0x1B6F, 0x1B70, 0x1B71,
    0x1B72, 0x1B73, 0x1CD0, 0x1CD1, 0x1CD2, 0x1CDA, 0x1CDB, 0x1CE0, 0x1DC0, 0x1DC1, 0x1DC3, 0x1DC4,
    0x1DC5, 0x1DC6, 0x1DC7, 0x1DC8, 0x1DC9, 0x1DCB, 0x1DCC, 0x1DD1, 0x1DD2, 0x1DD3, 0x1DD4, 0x1DD5,
    0x1DD6, 0x1DD7, 0x1DD8, 0x1DD9, 0x1DDA, 0x1DDB, 0x1DDC, 0x1DDD, 0x1DDE, 0x1DDF, 0x1DE0, 0x1DE1,
    0x1DE2, 0x1DE3, 0x1DE4, 0x1DE5, 0x1DE6, 0x1DFE, 0x20D0, 0x20D1, 0x20D4, 0x20D5, 0x20D6, 0x20D7,
    0x20DB, 0x20DC, 0x20E1, 0x20E7, 0x20E9, 0x20F0, 0x2CEF, 0x2CF0, 0x2CF1, 0x2DE0, 0x2DE1, 0x2DE2,
    0x2DE3, 0x2DE4, 0x2DE5, 0x2DE6, 0x2DE7, 0x2DE8, 0x2DE9, 0x2DEA, 0x2DEB, 0x2DEC, 0x2DED, 0x2DEE,
    0x2DEF, 0x2DF0, 0x2DF1, 0x2DF2, 0x2DF3, 0x2DF4, 0x2DF5, 0x2DF6, 0x2DF7, 0x2DF8, 0x2DF9, 0x2DFA,
    0x2DFB, 0x2DFC, 0x2DFD, 0x2DFE, 0x2DFF, 0xA66F, 0xA67C, 0xA67D, 0xA6F0, 0xA6F1, 0xA8E0, 0xA8E1,
    0xA8E2, 0xA8E3, 0xA8E4, 0xA8E5, 0xA8E6, 0xA8E7, 0xA8E8, 0xA8E9, 0xA8EA, 0xA8EB, 0xA8EC, 0xA8ED,
    0xA8EE, 0xA8EF, 0xA8F0, 0xA8F1, 0xAAB0, 0xAAB2, 0xAAB3, 0xAAB7, 0xAAB8, 0xAABE, 0xAABF, 0xAAC1,
    0xFE20, 0xFE21, 0xFE22, 0xFE23, 0xFE24, 0xFE25, 0xFE26, 0x10A0F, 0x10A38, 0x1D185, 0x1D186,
    0x1D187, 0x1D188, 0x1D189, 0x1D1AA, 0x1D1AB, 0x1D1AC, 0x1D1AD, 0x1D242, 0x1D243, 0x1D244,
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_constant_is_correct() {
        assert_eq!(PLACEHOLDER as u32, 0x0010_EEEE);
    }

    #[test]
    fn is_placeholder_only_matches_one_codepoint() {
        assert!(is_placeholder(PLACEHOLDER));
        assert!(!is_placeholder('a'));
        assert!(!is_placeholder('\u{0305}'));
    }

    #[test]
    fn first_diacritic_maps_to_index_zero() {
        let c = char::from_u32(0x0305).unwrap();
        assert_eq!(diacritic_to_index(c), Some(0));
    }

    #[test]
    fn diacritic_round_trips() {
        for i in 0..diacritic_count() {
            let i_u16 = i as u16;
            let c = index_to_diacritic(i_u16).unwrap();
            assert_eq!(diacritic_to_index(c), Some(i_u16));
        }
    }

    #[test]
    fn unknown_chars_return_none() {
        assert_eq!(diacritic_to_index('a'), None);
        assert_eq!(diacritic_to_index(PLACEHOLDER), None);
    }

    #[test]
    fn diacritic_count_is_nonzero() {
        // We expect roughly 297 entries per the kitty spec.
        assert!(diacritic_count() > 200);
    }

    #[test]
    fn is_diacritic_matches_table() {
        // Pick a few known ones.
        assert!(is_diacritic(char::from_u32(0x0305).unwrap()));
        assert!(is_diacritic(char::from_u32(0xFE20).unwrap()));
        assert!(!is_diacritic('z'));
    }

    #[test]
    fn out_of_range_index_returns_none() {
        let n = diacritic_count() as u16;
        assert_eq!(index_to_diacritic(n), None);
    }

    fn vp(image_id: u32, placement_id: u32, cols: u32, rows: u32) -> VirtualPlacement {
        VirtualPlacement {
            image_id,
            placement_id,
            cols,
            rows,
            z: 0,
        }
    }

    #[test]
    fn virtual_find_prefers_exact_placement_and_oldest_for_zero() {
        let mut t = VirtualPlacements::default();
        t.insert(vp(7, 0, 10, 5));
        t.insert(vp(7, 0, 4, 2));
        t.insert(vp(7, 3, 20, 3));
        assert_eq!(t.len(), 3, "anonymous placements accumulate");
        // pid 0 → the oldest placement of the image (kitty).
        assert_eq!(t.find(7, 0).map(|v| (v.cols, v.rows)), Some((10, 5)));
        assert_eq!(t.find(7, 3).map(|v| (v.cols, v.rows)), Some((20, 3)));
        assert!(t.find(7, 9).is_none(), "unknown pid finds nothing");
        assert!(t.find(8, 0).is_none());
    }

    #[test]
    fn virtual_insert_with_pid_replaces_in_place() {
        let mut t = VirtualPlacements::default();
        t.insert(vp(1, 2, 10, 5));
        t.insert(vp(1, 2, 3, 3));
        assert_eq!(t.len(), 1);
        assert_eq!(t.find(1, 2).map(|v| v.cols), Some(3));
    }

    #[test]
    fn virtual_remove_and_cap() {
        let mut t = VirtualPlacements::default();
        t.insert(vp(1, 0, 1, 1));
        t.insert(vp(2, 0, 1, 1));
        assert!(t.contains_image(1));
        assert!(t.remove_image(1));
        assert!(!t.remove_image(1));
        assert!(!t.contains_image(1));
        for i in 0..MAX_VIRTUAL_PLACEMENTS + 10 {
            t.insert(vp(100, 0, i as u32, 1));
        }
        assert_eq!(t.len(), MAX_VIRTUAL_PLACEMENTS);
        assert!(!t.contains_image(2), "oldest entry dropped at the cap");
    }

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-3
    }

    #[test]
    fn slice_fits_wide_image_to_width_and_centres_vertically() {
        // 200×100 image in a 10×5 box of 10×20 cells = 100×100 px:
        // scaled to 100×50, centred at y = 25..75.
        let cell = (10.0, 20.0);
        // Row 0 (y 0..20) is above the image: blank (PH-3).
        assert!(placeholder_slice((200, 100), (10, 5), cell, 0, 0, 10).is_none());
        // Row 1 (y 20..40) shows image rows 0..15 px of 50: y 25..40.
        let s = placeholder_slice((200, 100), (10, 5), cell, 1, 0, 10).unwrap();
        assert!(approx(s.dst_pos[1], 5.0) && approx(s.dst_size[1], 15.0));
        assert!(approx(s.dst_pos[0], 0.0) && approx(s.dst_size[0], 100.0));
        assert!(approx(s.uv_min[1], 0.0) && approx(s.uv_max[1], 0.3));
        // Row 2 (y 40..60) is fully inside the image.
        let s = placeholder_slice((200, 100), (10, 5), cell, 2, 0, 10).unwrap();
        assert!(approx(s.dst_pos[1], 0.0) && approx(s.dst_size[1], 20.0));
        assert!(approx(s.uv_min[1], 0.3) && approx(s.uv_max[1], 0.7));
        // Row 4 (y 80..100) is below the image.
        assert!(placeholder_slice((200, 100), (10, 5), cell, 4, 0, 10).is_none());
    }

    #[test]
    fn slice_partial_run_maps_to_its_columns() {
        // Cols 5..10 of the same box: the right half of the image.
        let s = placeholder_slice((200, 100), (10, 5), (10.0, 20.0), 2, 5, 5).unwrap();
        assert!(approx(s.dst_size[0], 50.0));
        assert!(approx(s.uv_min[0], 0.5) && approx(s.uv_max[0], 1.0));
    }

    #[test]
    fn slice_tall_box_pillarboxes() {
        // 200×100 image in a 20×3 box of 9×20 cells = 180×60 px: fit to
        // height → 120×60, centred at x = 30..150 (kitty reference).
        let cell = (9.0, 20.0);
        let s = placeholder_slice((200, 100), (20, 3), cell, 0, 0, 20).unwrap();
        assert!(approx(s.dst_pos[0], 30.0) && approx(s.dst_size[0], 120.0));
        assert!(approx(s.uv_min[0], 0.0) && approx(s.uv_max[0], 1.0));
        // Cells 0..3 (x 0..27) are left of the image.
        assert!(placeholder_slice((200, 100), (20, 3), cell, 0, 0, 3).is_none());
    }

    #[test]
    fn slice_zero_box_uses_natural_size_per_axis() {
        // c only: rows fall back to ceil(100 / 20) = 5, not to the
        // aspect-derived 3 (kitty 0.49.2: the image sits 27.5 px down).
        let cell = (9.0, 20.0);
        let s = placeholder_slice((200, 100), (10, 0), cell, 1, 0, 10).unwrap();
        assert!(approx(s.dst_pos[1], 7.5));
        // Neither: 23×5 cells, the image at native scale, centred x.
        let s = placeholder_slice((200, 100), (0, 0), cell, 0, 0, 23).unwrap();
        assert!(approx(s.dst_pos[0], 3.5) && approx(s.dst_size[0], 200.0));
        assert!(approx(s.dst_size[1], 20.0));
    }

    #[test]
    fn slice_outside_the_box_is_blank() {
        // A placeholder past the box (row 7 of a 5-row box) is blank.
        assert!(placeholder_slice((200, 100), (10, 5), (10.0, 20.0), 7, 0, 10).is_none());
        assert!(placeholder_slice((200, 100), (10, 5), (10.0, 20.0), 2, 12, 3).is_none());
        // Degenerate inputs.
        assert!(placeholder_slice((0, 100), (10, 5), (10.0, 20.0), 2, 0, 3).is_none());
        assert!(placeholder_slice((200, 100), (10, 5), (10.0, 20.0), 2, 0, 0).is_none());
    }
}
