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
//! M8 lifts the M4b "only 1-cell case" restriction; `cluster_cells`
//! now snaps each cluster's total advance to a true integer multiple
//! of `cell_width`, so CJK / VS16 / ZWJ emoji land cleanly on the
//! grid.

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

/// Snap each cluster's total advance to a multiple of `cell_width`,
/// using the **same** `cluster_cells` value for every cluster in the
/// run. For runs that mix narrow + wide clusters, use
/// [`snap_cluster_widths_per_cluster`].
///
/// `cluster_cells` is the declared cell-width of clusters: `1` for the
/// default case, `2` for wide CJK / VS16 / ZWJ emoji. A value of `0` is
/// treated as `1` (defensive — see the docstring note below).
#[must_use]
pub fn snap_cluster_widths(
    glyphs: &[GlyphPos],
    cell_width: f32,
    cluster_cells: u8,
) -> Vec<GlyphPos> {
    let cells = if cluster_cells == 0 { 1 } else { cluster_cells };
    snap_cluster_widths_impl(glyphs, cell_width, ClusterWidths::Uniform(cells))
}

/// Per-cluster cell-width variant of [`snap_cluster_widths`]: the
/// `widths` slice supplies one `u8` per cluster, in the order clusters
/// appear in `glyphs` (grouped by `(start, end)`).
///
/// Excess `widths` entries are ignored; missing entries fall back to
/// `1` cell.
#[must_use]
pub fn snap_cluster_widths_per_cluster(
    glyphs: &[GlyphPos],
    cell_width: f32,
    widths: &[u8],
) -> Vec<GlyphPos> {
    snap_cluster_widths_impl(glyphs, cell_width, ClusterWidths::PerCluster(widths))
}

enum ClusterWidths<'a> {
    Uniform(u8),
    PerCluster(&'a [u8]),
}

impl ClusterWidths<'_> {
    fn width_for(&self, cluster_idx: usize) -> u8 {
        match self {
            ClusterWidths::Uniform(w) => *w,
            ClusterWidths::PerCluster(slice) => slice.get(cluster_idx).copied().unwrap_or(1),
        }
    }
}

fn snap_cluster_widths_impl(
    glyphs: &[GlyphPos],
    cell_width: f32,
    widths: ClusterWidths<'_>,
) -> Vec<GlyphPos> {
    if glyphs.is_empty() {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(glyphs.len());
    let mut cursor_x: f32 = glyphs[0].x;
    let mut cluster_idx: usize = 0;

    let mut i = 0;
    while i < glyphs.len() {
        let start = glyphs[i].start;
        let end = glyphs[i].end;

        // Find the cluster's contiguous run [i..j).
        let mut j = i + 1;
        while j < glyphs.len() && glyphs[j].start == start && glyphs[j].end == end {
            j += 1;
        }

        let cells = widths.width_for(cluster_idx);
        let cells_nonzero = if cells == 0 { 1 } else { cells };
        let target_width = f32::from(cells_nonzero) * cell_width;
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
        cluster_idx += 1;
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
    fn cluster_cells_two_snaps_to_2x_cell_width() {
        // CJK ideograph case: cosmic-text reported 13.71 px for a width-2
        // cluster (text-stack.md table). Snap must yield exactly 16 px.
        let glyphs = [GlyphPos {
            start: 0,
            end: 4,
            x: 0.0,
            w: 13.71,
        }];
        let out = snap_cluster_widths(&glyphs, 8.0, 2);
        assert_eq!(out.len(), 1);
        assert!(approx(out[0].w, 16.0));
    }

    #[test]
    fn cluster_cells_zero_treats_as_one() {
        // 0 cells is nonsense for a terminal grid — we clamp to 1 cell
        // rather than passing through whatever the shaper produced.
        let glyphs = [GlyphPos {
            start: 0,
            end: 1,
            x: 0.0,
            w: 8.57,
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

    #[test]
    fn per_cluster_widths_with_mixed_1_and_2_cells() {
        // Three single-glyph clusters, widths [1, 2, 1]: total span
        // 1+2+1 = 4 cells = 32 px starting at x=0.
        let glyphs = [
            GlyphPos {
                start: 0,
                end: 1,
                x: 0.0,
                w: 7.7,
            },
            GlyphPos {
                start: 1,
                end: 4,
                x: 7.7,
                w: 13.71,
            },
            GlyphPos {
                start: 4,
                end: 5,
                x: 21.41,
                w: 8.1,
            },
        ];
        let out = snap_cluster_widths_per_cluster(&glyphs, 8.0, &[1, 2, 1]);
        assert_eq!(out.len(), 3);
        // Cluster 0: x=0, w=8
        assert!(approx(out[0].x, 0.0));
        assert!(approx(out[0].w, 8.0));
        // Cluster 1: x=8, w=16 (two cells)
        assert!(approx(out[1].x, 8.0));
        assert!(approx(out[1].w, 16.0));
        // Cluster 2: x=24, w=8
        assert!(approx(out[2].x, 24.0));
        assert!(approx(out[2].w, 8.0));
    }

    #[test]
    fn single_int_variant_matches_per_cluster_uniform() {
        // The single-int variant and the per-cluster variant must agree
        // when the per-cluster slice is a constant.
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
        let single = snap_cluster_widths(&glyphs, 8.0, 1);
        let per = snap_cluster_widths_per_cluster(&glyphs, 8.0, &[1, 1]);
        assert_eq!(single.len(), per.len());
        for (a, b) in single.iter().zip(per.iter()) {
            assert!(approx(a.x, b.x));
            assert!(approx(a.w, b.w));
        }
    }

    #[test]
    fn per_cluster_missing_entries_default_to_one_cell() {
        // Two clusters but widths slice is empty — both default to 1.
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
                w: 7.4,
            },
        ];
        let out = snap_cluster_widths_per_cluster(&glyphs, 8.0, &[]);
        assert_eq!(out.len(), 2);
        assert!(approx(out[1].x - out[0].x, 8.0));
    }
}
