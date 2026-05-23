//! Cluster-width snap pass.
//!
//! `cosmic-text 0.19`'s `Buffer::set_monospace_width` rounds *per-glyph*
//! advance to the nearest cell-width multiple, but a terminal cell grid
//! needs the *cluster's* total advance to be exactly `N * cell_width`.
//!
//! See `docs/decisions/text-stack.md` ("The mode 2027 width problem").
//!
//! This module is pure: it takes a slice of glyph positions and a cell
//! width, and produces adjusted x positions and widths so each cluster
//! (group of glyphs sharing a `(start..end)` byte range) snaps to
//! `cluster_cells * cell_width`.
//!
//! # M4b scope
//!
//! Only the **1-cell** case is implemented. Mode 2027 multi-cell clusters
//! are wired through the `cluster_cells` parameter but only `1` is honored;
//! see TODO below.

/// A glyph's position and advance, abstracted so tests don't need a GPU
/// type or a real `cosmic_text::LayoutGlyph`.
///
/// Mirrors the relevant subset of `cosmic_text::LayoutGlyph`:
/// - `start..end` = byte range of the originating cluster in the line.
/// - `x` = x-offset within the line (pre-snap).
/// - `w` = natural advance from shaping.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GlyphPos {
    pub start: usize,
    pub end: usize,
    pub x: f32,
    pub w: f32,
}

/// Snap each cluster's total advance to a multiple of `cell_width`.
///
/// `cluster_cells` is the declared cell-width of clusters: `1` for the
/// common case (default mode 2027 off), `2` for wide CJK / VS16 / ZWJ
/// emoji. **Only `1` is implemented in M4b** — wider clusters fall back
/// to whatever the shaper produced (with a TODO marker below).
///
/// The returned vec has the same length as `glyphs`, in the same order.
///
/// # Algorithm
///
/// 1. Walk `glyphs`, grouping by identical `(start, end)`.
/// 2. For each cluster, target width = `cluster_cells * cell_width`.
/// 3. Position cluster at the running x cursor; redistribute glyph widths
///    proportionally to their original `w` (so multi-glyph ligatures keep
///    their relative spacing).
/// 4. Update the running x cursor by `target_width`.
///
/// If `glyphs` is empty, returns an empty vec.
///
/// If a cluster's total natural width is `0` (degenerate), we redistribute
/// equally rather than divide by zero.
#[must_use]
pub fn snap_cluster_widths(
    glyphs: &[GlyphPos],
    cell_width: f32,
    cluster_cells: u8,
) -> Vec<GlyphPos> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    // TODO(mode-2027): honor cluster_cells > 1 once mode 2027 wiring lands
    // in toastty-protocols. For now we only snap the 1-cell case; wider
    // clusters keep their natural width.
    if cluster_cells != 1 {
        return glyphs.to_vec();
    }

    let mut out = Vec::with_capacity(glyphs.len());
    let mut cursor_x: f32 = glyphs[0].x;
    let target_width = f32::from(cluster_cells) * cell_width;

    let mut i = 0;
    while i < glyphs.len() {
        let start = glyphs[i].start;
        let end = glyphs[i].end;

        // Find the cluster's contiguous run [i..j).
        let mut j = i + 1;
        while j < glyphs.len() && glyphs[j].start == start && glyphs[j].end == end {
            j += 1;
        }

        let cluster_natural: f32 = glyphs[i..j].iter().map(|g| g.w).sum();

        // Redistribute. Equal split if natural width is degenerate.
        // We cap at u16 to safely round-trip into f32; cluster sizes are
        // tiny in practice.
        let count = u16::try_from(j - i).unwrap_or(u16::MAX);
        let count_f = f32::from(count);
        let mut sub_x = cursor_x;
        for g in &glyphs[i..j] {
            let new_w = if cluster_natural > f32::EPSILON {
                target_width * (g.w / cluster_natural)
            } else {
                target_width / count_f
            };
            out.push(GlyphPos {
                start: g.start,
                end: g.end,
                x: sub_x,
                w: new_w,
            });
            sub_x += new_w;
        }

        cursor_x += target_width;
        i = j;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-4
    }

    #[test]
    fn empty_input_returns_empty() {
        assert!(snap_cluster_widths(&[], 8.0, 1).is_empty());
    }

    #[test]
    fn single_already_snapped_glyph_unchanged_position() {
        // ASCII "A": cluster has one glyph already at width ~ cell.
        let glyphs = [GlyphPos {
            start: 0,
            end: 1,
            x: 0.0,
            w: 8.0,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 1);
        assert!(approx(out[0].x, 0.0));
        assert!(approx(out[0].w, 8.0));
    }

    #[test]
    fn single_glyph_oversnap_is_corrected() {
        // shaper produced 8.57 px for a 1-cell-wide cluster — snap to 8.
        let glyphs = [GlyphPos {
            start: 0,
            end: 1,
            x: 0.0,
            w: 8.57,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 1);
        assert!(approx(out[0].w, 8.0));
    }

    #[test]
    fn two_glyph_cluster_redistributes_proportionally() {
        // A ligature "==>"-like cluster: one source range, two glyphs.
        // Natural widths 3.0 and 5.0 (ratio 3:5). Target is 1 cell = 8 px.
        // After snap, glyph widths should still be 3 and 5.
        let glyphs = [
            GlyphPos {
                start: 0,
                end: 3,
                x: 0.0,
                w: 3.0,
            },
            GlyphPos {
                start: 0,
                end: 3,
                x: 3.0,
                w: 5.0,
            },
        ];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 2);
        assert!(approx(out[0].w, 3.0));
        assert!(approx(out[1].w, 5.0));
        assert!(approx(out[0].x, 0.0));
        assert!(approx(out[1].x, 3.0));
    }

    #[test]
    fn cluster_boundary_is_one_cell_apart() {
        // Two single-glyph clusters back-to-back. After snap, the second
        // starts exactly one cell to the right of the first.
        let glyphs = [
            GlyphPos {
                start: 0,
                end: 1,
                x: 0.0,
                w: 7.4,
            },
            GlyphPos {
                start: 1,
                end: 2,
                x: 7.4,
                w: 8.6,
            },
        ];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 2);
        assert!(approx(out[0].x, 0.0));
        assert!(approx(out[0].w, 8.0));
        assert!(approx(out[1].x, 8.0));
        assert!(approx(out[1].w, 8.0));
    }

    #[test]
    fn three_glyph_cluster_with_zero_natural_width_splits_equally() {
        // Degenerate: shaping returned all zero advances for the cluster.
        // We must not produce NaN; equal split is the fallback.
        let glyphs = [
            GlyphPos {
                start: 0,
                end: 1,
                x: 0.0,
                w: 0.0,
            },
            GlyphPos {
                start: 0,
                end: 1,
                x: 0.0,
                w: 0.0,
            },
        ];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 2);
        assert!(approx(out[0].w, 4.0));
        assert!(approx(out[1].w, 4.0));
        assert!(approx(out[0].x, 0.0));
        assert!(approx(out[1].x, 4.0));
    }

    #[test]
    fn starting_x_is_preserved_from_first_glyph() {
        // The input's first x is the run's starting position. The snap
        // should not pull glyphs back to x=0 if the shaper already
        // positioned them past it (e.g. left margin).
        let glyphs = [GlyphPos {
            start: 0,
            end: 1,
            x: 24.0,
            w: 8.1,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert!(approx(out[0].x, 24.0));
        assert!(approx(out[0].w, 8.0));
    }

    #[test]
    fn cluster_cells_greater_than_one_is_pass_through() {
        // M4b: we only honor the 1-cell case; wider clusters keep natural
        // widths. The TODO in the module covers this.
        let glyphs = [GlyphPos {
            start: 0,
            end: 4,
            x: 0.0,
            w: 13.71,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 2);
        assert_eq!(out.len(), 1);
        assert!(approx(out[0].w, 13.71));
    }

    #[test]
    fn cluster_cells_zero_is_pass_through() {
        let glyphs = [GlyphPos {
            start: 0,
            end: 1,
            x: 0.0,
            w: 8.0,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 0);
        assert_eq!(out.len(), 1);
        assert!(approx(out[0].w, 8.0));
    }

    #[test]
    fn mixed_run_three_clusters_each_one_cell() {
        // Three single-glyph clusters: "abc".
        let glyphs = [
            GlyphPos {
                start: 0,
                end: 1,
                x: 0.0,
                w: 8.7,
            },
            GlyphPos {
                start: 1,
                end: 2,
                x: 8.7,
                w: 7.3,
            },
            GlyphPos {
                start: 2,
                end: 3,
                x: 16.0,
                w: 8.2,
            },
        ];
        let out = snap_cluster_widths(&glyphs, 8.0, 1);
        assert_eq!(out.len(), 3);
        for (i, g) in out.iter().enumerate() {
            let i_f = u16::try_from(i).map(f32::from).unwrap_or(0.0);
            assert!(approx(g.x, i_f * 8.0), "x at {i}: {}", g.x);
            assert!(approx(g.w, 8.0), "w at {i}: {}", g.w);
        }
    }
}
