//! Image instances for kitty Unicode-placeholder cells.
//!
//! Placeholder images are resolved from the cells every frame (see
//! `toastty-term`'s `term/placeholder.rs`): for each rendered row the
//! term splits the row's `U+10EEEE` cells into runs, each run is looked
//! up in the active screen's virtual-placement table, and
//! [`placeholder_slice`] says which part of the fitted image the run
//! shows. Nothing is cached between frames, so the image always matches
//! the text it is made of — including scrollback rows and placeholders
//! printed before their image arrived.
//!
//! Draw calls are the cost that matters (the image pipeline issues one
//! per instance): each run becomes one instance, and runs of the same
//! image that stack exactly on consecutive rows are merged into one, so
//! an undisturbed image is a single instance however many cells it
//! covers.

use std::collections::HashMap;

use toastty_term::{Damage, PlaceholderRun, Term, placeholder_slice};

use super::instance::{ImageInstance, UNRESOLVED_TEXTURE};

/// Where the rendered rows sit, as the text builders lay them out.
#[derive(Debug, Clone, Copy)]
pub struct RowLayout {
    /// Number of view rows to draw (`rows_to_render`).
    pub rows_rendered: u16,
    /// Y offset added to every row (`view_offset_pixel - cell_h` while
    /// fractionally scrolled, else 0).
    pub y_translate: f32,
}

/// Append the instances for every visible placeholder run to `out`,
/// then re-sort all of `out` by `(z, image_id)`, the order
/// [`super::instance::split_layers`] expects. Stable, so the ordinary
/// placements already in `out` keep their relative order.
///
/// `damage` is `Some` on a partial-redraw frame (`LoadOp::Load`). Then
/// only the dirty cells of a run are drawn: the clean ones still hold
/// last frame's pixels, and drawing over them again would double-blend
/// any translucent pixels.
#[allow(clippy::cast_precision_loss)] // cell coordinates are far below 2^24.
pub fn build_placeholder_instances(
    out: &mut Vec<ImageInstance>,
    term: &Term,
    cell: (f32, f32),
    layout: RowLayout,
    damage: Option<&Damage>,
) {
    if !term.has_virtual_placements() {
        return;
    }
    let registry = term.image_registry();
    let mut runs: Vec<PlaceholderRun> = Vec::new();
    let mut pieces: Vec<(u16, u16)> = Vec::new();
    // Vertical merge: instances that ended on the previous row, keyed by
    // everything that must match for the next row to extend them.
    let mut open: HashMap<MergeKey, usize> = HashMap::new();
    let mut next_open: HashMap<MergeKey, usize> = HashMap::new();
    for r in 0..layout.rows_rendered {
        runs.clear();
        term.placeholder_runs(term.view_row(r), &mut runs);
        let row_y = f32::from(r) * cell.1 + layout.y_translate;
        for run in &runs {
            let Some(vp) = term.resolve_placeholder(run) else {
                continue;
            };
            let Some(image) = registry.get(run.image_id) else {
                continue;
            };
            pieces.clear();
            match damage {
                None => pieces.push((run.col, run.len)),
                Some(d) => dirty_pieces(d, r, run.col, run.len, &mut pieces),
            }
            for &(col, len) in &pieces {
                let img_col = run.img_col + u32::from(col - run.col);
                let Some(s) = placeholder_slice(
                    (image.width, image.height),
                    (vp.cols, vp.rows),
                    cell,
                    run.img_row,
                    img_col,
                    u32::from(len),
                ) else {
                    continue;
                };
                let inst = ImageInstance {
                    pos: [f32::from(col) * cell.0 + s.dst_pos[0], row_y + s.dst_pos[1]],
                    size: s.dst_size,
                    uv_min: s.uv_min,
                    uv_max: s.uv_max,
                    // Bound by `ImagePipeline::resolve_kitty_textures`.
                    texture_index: UNRESOLVED_TEXTURE,
                    z: vp.z,
                    image_id: run.image_id,
                    _pad: [0; 1],
                };
                let key = MergeKey::of(&inst);
                if let Some(&i) = open.get(&key)
                    && extends(&out[i], &inst)
                {
                    out[i].size[1] = inst.pos[1] + inst.size[1] - out[i].pos[1];
                    out[i].uv_max[1] = inst.uv_max[1];
                    next_open.insert(key, i);
                } else {
                    next_open.insert(key, out.len());
                    out.push(inst);
                }
            }
        }
        std::mem::swap(&mut open, &mut next_open);
        next_open.clear();
    }
    out.sort_by_key(|i| (i.z, i.image_id));
}

/// The parts of the run `[col, col + len)` on view row `r` that are
/// dirty, as `(col, len)` pieces.
fn dirty_pieces(damage: &Damage, r: u16, col: u16, len: u16, out: &mut Vec<(u16, u16)>) {
    let Some(rd) = damage.rows.get(usize::from(r)) else {
        return;
    };
    let end = col + len;
    if rd.all_cols {
        out.push((col, len));
        return;
    }
    let mut start: Option<u16> = None;
    let mut prev = 0u16;
    for &c in rd.cols.iter().filter(|&&c| c >= col && c < end) {
        match start {
            Some(_) if c == prev + 1 => {}
            Some(s) => {
                out.push((s, prev + 1 - s));
                start = Some(c);
            }
            None => start = Some(c),
        }
        prev = c;
    }
    if let Some(s) = start {
        out.push((s, prev + 1 - s));
    }
}

/// Everything two vertically adjacent instances must share to merge:
/// the same image and z, and bit-identical x extent in both screen and
/// image space (they come from the same columns of the same box).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct MergeKey {
    z: i32,
    image_id: u32,
    x: [u32; 4],
}

impl MergeKey {
    fn of(i: &ImageInstance) -> Self {
        Self {
            z: i.z,
            image_id: i.image_id,
            x: [
                i.pos[0].to_bits(),
                i.size[0].to_bits(),
                i.uv_min[0].to_bits(),
                i.uv_max[0].to_bits(),
            ],
        }
    }
}

/// True when `next` starts exactly where `prev` ends, on screen and in
/// the image.
fn extends(prev: &ImageInstance, next: &ImageInstance) -> bool {
    const PX: f32 = 0.01;
    const UV: f32 = 1e-5;
    (prev.pos[1] + prev.size[1] - next.pos[1]).abs() < PX
        && (prev.uv_max[1] - next.uv_min[1]).abs() < UV
}

#[cfg(test)]
mod tests {
    use super::*;
    use toastty_graphics::KittySink;
    use toastty_term::{ImageData, RowDamage, VirtualPlacement};

    /// First kitty placeholder diacritics (index → codepoint).
    const DIACRITICS: &[u32] = &[0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E];
    const CELL: (f32, f32) = (10.0, 20.0);
    const LAYOUT: RowLayout = RowLayout {
        rows_rendered: 8,
        y_translate: 0.0,
    };

    fn feed(t: &mut Term, bytes: &[u8]) {
        let mut p = toastty_parser::Parser::new();
        p.advance(t, bytes);
    }

    /// 8×12 term with a 200×100 image `id` behind a `cols × rows`
    /// virtual placement.
    fn setup(id: u32, cols: u32, rows: u32) -> Term {
        let mut t = Term::new(8, 12, 0);
        let data = ImageData {
            width: 200,
            height: 100,
            pixels: vec![255; 200 * 100 * 4],
        };
        assert_eq!(KittySink::register_image(&mut t, id, 0, data), Some(id));
        KittySink::place_virtual(
            &mut t,
            VirtualPlacement {
                image_id: id,
                placement_id: 0,
                cols,
                rows,
                z: 0,
            },
        );
        t
    }

    /// Print a `rows × cols` block of image `id` (8-bit fg) at the top.
    fn print_block(t: &mut Term, id: u8, rows: usize, cols: usize) {
        let mut s = format!("\x1b[H\x1b[38;5;{id}m");
        for &row_mark in &DIACRITICS[..rows] {
            for &col_mark in &DIACRITICS[..cols] {
                s.push(toastty_term::PLACEHOLDER);
                s.push(char::from_u32(row_mark).unwrap());
                s.push(char::from_u32(col_mark).unwrap());
            }
            s.push_str("\r\n");
        }
        s.push_str("\x1b[39m");
        feed(t, s.as_bytes());
    }

    fn build(t: &Term, damage: Option<&Damage>) -> Vec<ImageInstance> {
        let mut out = Vec::new();
        build_placeholder_instances(&mut out, t, CELL, LAYOUT, damage);
        out
    }

    fn approx(a: [f32; 2], b: [f32; 2]) -> bool {
        (a[0] - b[0]).abs() < 1e-3 && (a[1] - b[1]).abs() < 1e-3
    }

    /// True when instance `i` covers any of cell `(row, col)`.
    fn covers(i: &ImageInstance, row: u16, col: u16) -> bool {
        let (x0, y0) = (f32::from(col) * CELL.0, f32::from(row) * CELL.1);
        i.pos[0] < x0 + CELL.0
            && i.pos[0] + i.size[0] > x0
            && i.pos[1] < y0 + CELL.1
            && i.pos[1] + i.size[1] > y0
    }

    #[test]
    fn a_whole_image_is_one_fitted_instance() {
        // PH-2 / PH-3: a 200×100 image in a 6×5-cell box (60×100 px)
        // fits to 60×30, centred at y = 35..65. The rows above and below
        // stay blank and the visible rows merge into one draw.
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 42, 5, 6);
        let out = build(&t, None);
        assert_eq!(out.len(), 1, "{out:?}");
        let i = out[0];
        assert!(approx(i.pos, [0.0, 35.0]), "{:?}", i.pos);
        assert!(approx(i.size, [60.0, 30.0]), "{:?}", i.size);
        assert!(approx(i.uv_min, [0.0, 0.0]) && approx(i.uv_max, [1.0, 1.0]));
        assert_eq!(
            i.texture_index, UNRESOLVED_TEXTURE,
            "bound later, by resolve"
        );
    }

    #[test]
    fn text_over_the_cells_cuts_the_image() {
        // PH-1: overwrite two cells of row 2; the image there is gone.
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 42, 5, 6);
        feed(&mut t, b"\x1b[3;3Hab");
        let out = build(&t, None);
        // Row 2 splits into cols 0..2 and 4..6; rows 1 and 3 merge with
        // neither (different x extent), so 4 instances.
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(
            !out.iter().any(|i| covers(i, 2, 2) || covers(i, 2, 3)),
            "cells (2,2) and (2,3) must show nothing: {out:?}"
        );
        assert!(out.iter().any(|i| covers(i, 2, 1)));
    }

    #[test]
    fn nothing_without_a_virtual_placement() {
        // Cells of an image with no virtual placement (PH-9).
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 43, 5, 6);
        assert!(build(&t, None).is_empty());
    }

    #[test]
    fn partial_frames_draw_only_dirty_cells() {
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 42, 5, 6);
        let mut d = Damage::new(8);
        d.clear();
        d.rows[2] = RowDamage::default();
        d.rows[2].mark(2);
        d.rows[2].mark(3);
        d.rows[2].mark(5);
        let out = build(&t, Some(&d));
        assert_eq!(out.len(), 2, "{out:?}");
        assert!(approx(out[0].pos, [20.0, 40.0]) && approx(out[0].size, [20.0, 20.0]));
        assert!(approx(out[1].pos, [50.0, 40.0]) && approx(out[1].size, [10.0, 20.0]));
        // A clean frame draws nothing.
        d.rows[2].clear();
        assert!(build(&t, Some(&d)).is_empty());
    }

    #[test]
    fn rows_follow_the_view_translation() {
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 42, 5, 6);
        let mut out = Vec::new();
        let layout = RowLayout {
            rows_rendered: 8,
            y_translate: -7.0,
        };
        build_placeholder_instances(&mut out, &t, CELL, layout, None);
        assert!(approx(out[0].pos, [0.0, 28.0]), "{:?}", out[0].pos);
    }

    #[test]
    fn instances_are_sorted_by_z_with_existing_ones() {
        let mut t = setup(42, 6, 5);
        print_block(&mut t, 42, 5, 6);
        let other = ImageInstance {
            pos: [0.0; 2],
            size: [1.0; 2],
            uv_min: [0.0; 2],
            uv_max: [1.0; 2],
            texture_index: 0,
            z: 5,
            image_id: 1,
            _pad: [0],
        };
        let mut out = vec![other];
        build_placeholder_instances(&mut out, &t, CELL, LAYOUT, None);
        assert_eq!(out.len(), 2);
        assert_eq!(
            (out[0].image_id, out[1].image_id),
            (42, 1),
            "z 0 before z 5"
        );
    }

    #[test]
    fn dirty_pieces_split_on_gaps() {
        let mut d = Damage::new(2);
        d.clear();
        for c in [0, 1, 2, 5, 9] {
            d.rows[0].mark(c);
        }
        let mut out = Vec::new();
        dirty_pieces(&d, 0, 1, 6, &mut out);
        assert_eq!(out, vec![(1, 2), (5, 1)]);
        out.clear();
        d.rows[1].mark_all();
        dirty_pieces(&d, 1, 3, 4, &mut out);
        assert_eq!(out, vec![(3, 4)]);
        out.clear();
        dirty_pieces(&d, 7, 0, 4, &mut out);
        assert!(out.is_empty(), "rows past the damage set are clean");
    }
}
