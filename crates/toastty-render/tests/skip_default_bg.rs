//! `CellRenderOpts::skip_default_bg` must not change a single pixel of a
//! frame that starts with `LoadOp::Clear(theme.bg)`, whenever the
//! renderer's gate (`may_skip_default_bg`) allows it.

mod common;

use common::Offscreen;
use toastty_parser::Parser;
use toastty_render::text::instance::{EdgeBleed, may_skip_default_bg};
use toastty_term::Term;
use wgpu::TextureFormat;

fn term_from(rows: u16, cols: u16, bytes: &[u8]) -> Term {
    let mut term = Term::new(rows, cols, 100);
    Parser::new().advance(&mut term, bytes);
    term
}

/// Pixels that differ between two renders of `term`.
fn diff(term: &Term, width: u32, height: u32, one: &Offscreen, other: &Offscreen) -> usize {
    let lhs = common::render_term_offscreen_with(term, width, height, one);
    let rhs = common::render_term_offscreen_with(term, width, height, other);
    lhs.pixels()
        .zip(rhs.pixels())
        .filter(|(l, r)| l != r)
        .count()
}

/// The renderer's skip decision for a frame with `bleed` on or off.
fn gate(bleed: bool) -> bool {
    may_skip_default_bg(
        &EdgeBleed {
            active: bleed,
            ..EdgeBleed::default()
        },
        false,
    )
}

/// Several screens of coloured, decorated and wide text, in both the
/// test format and the app's swapchain format.
#[test]
fn skipping_default_bg_quads_is_pixel_identical() {
    let scenes: [&[u8]; 4] = [
        b"plain text on the default background\r\n  indented \x1b[1mbold\x1b[0m",
        b"\x1b[31mred\x1b[0m \x1b[42;30m green bg \x1b[0m \x1b[7mreversed\x1b[0m \
          \x1b[38;5;208;48;5;24m256\x1b[0m \x1b[48;2;10;20;30mrgb\x1b[0m",
        b"\x1b[4munder\x1b[0m \x1b[9mstrike\x1b[0m \x1b[53mover\x1b[0m \x1b[2mdim\x1b[0m\r\n\
          \x1b[44m\x1b[K full-width blue row\x1b[0m\r\n",
        "wide 你好 \u{1F680} box │├─┤ ░▒▓ cursor here".as_bytes(),
    ];
    for format in [TextureFormat::Rgba8Unorm, TextureFormat::Bgra8UnormSrgb] {
        for (i, bytes) in scenes.iter().enumerate() {
            let term = term_from(6, 48, bytes);
            let skip = Offscreen {
                format,
                skip_default_bg: gate(false),
                ..Offscreen::default()
            };
            let keep = Offscreen {
                skip_default_bg: false,
                ..skip
            };
            assert!(skip.skip_default_bg);
            let n = diff(&term, 520, 120, &skip, &keep);
            assert_eq!(n, 0, "{format:?} scene {i}: {n} pixels differ");
        }
    }
}

/// Scrolled back into history by part of a row, with and without
/// padding + edge bleed: rows are y-translated, a partial row hangs
/// above the top, and the grid leaves a sliver at the bottom. With
/// bleed on, the edge rows' grown quads reach over the partial rows,
/// so skipping default-bg quads *would* change pixels — the gate must
/// refuse it.
#[test]
fn skipping_default_bg_quads_is_pixel_identical_mid_row() {
    // Alternate full-width coloured rows (`\e[K` fills the row) with
    // plain ones, so an edge row's bleed always meets a default-bg row.
    let mut bytes = Vec::new();
    for i in 0..20 {
        let line = if i % 2 == 0 {
            format!("\x1b[4{}m{i:02} line\x1b[K\x1b[0m\r\n", i % 7 + 1)
        } else {
            format!("{i:02} \x1b[3{}mplain\x1b[0m\r\n", i % 7 + 1)
        };
        bytes.extend_from_slice(line.as_bytes());
    }
    for lines in [2, 3] {
        let mut term = term_from(6, 12, &bytes);
        // 19 px rows (16 px font x 1.2): `lines` and 7 px into history.
        term.scroll_view_by(lines, 7.0, 19.0);
        term.force_snap_view();
        assert!(term.view_offset_pixel() > 0.0);
        mid_row_scene(&term);
    }
}

fn mid_row_scene(term: &Term) {
    // 6 rows x 19 px = 114 px of grid in a 130 - 16 = 114 px content
    // area; 136 - 16 = 120 leaves a 6 px sliver below the grid.
    for (pad, h) in [([0; 4], 120), ([8; 4], 130), ([8; 4], 136)] {
        for format in [TextureFormat::Rgba8Unorm, TextureFormat::Bgra8UnormSrgb] {
            for bleed in [false, true] {
                let keep = Offscreen {
                    format,
                    skip_default_bg: false,
                    pad,
                    bleed,
                };
                let gated = Offscreen {
                    skip_default_bg: gate(bleed),
                    ..keep
                };
                let n = diff(term, 140, h, &gated, &keep);
                assert_eq!(n, 0, "{format:?} pad {pad:?} h {h} bleed {bleed}: {n} px");
            }
        }
        // The gate is load-bearing: forcing the skip under bleed changes
        // pixels once there is padding to bleed into.
        if pad != [0; 4] {
            let keep = Offscreen {
                skip_default_bg: false,
                pad,
                bleed: true,
                ..Offscreen::default()
            };
            let forced = Offscreen {
                skip_default_bg: true,
                ..keep
            };
            assert!(diff(term, 140, h, &forced, &keep) > 0, "pad {pad:?} h {h}");
        }
    }
}
