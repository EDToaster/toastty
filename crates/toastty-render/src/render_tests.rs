//! End-to-end tests through [`Renderer::render_term`] on the headless
//! target (an offscreen `Bgra8UnormSrgb` texture standing in for the
//! swapchain): the real frame path, with and without post shaders.

use std::path::PathBuf;

use toastty_parser::Parser;
use toastty_term::Term;

use crate::{DEFAULT_FONT_SIZE_PX, PostShaderSource, RenderOutcome, Renderer};

const W: u32 = 400;
const H: u32 = 240;

/// Pass-through: samples `iChannel0` at the fragment's own pixel.
const PASS: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                    \x20   fragColor = texture(iChannel0, fragCoord / iResolution.xy);\n\
                    }\n";

const INVERT: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                      \x20   vec4 c = texture(iChannel0, fragCoord / iResolution.xy);\n\
                      \x20   fragColor = vec4(1.0 - c.rgb, c.a);\n\
                      }\n";

/// White while button 1 is held, pass-through otherwise.
const MOUSE: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                     \x20   vec4 c = texture(iChannel0, fragCoord / iResolution.xy);\n\
                     \x20   fragColor = iMouse.z > 0.0 ? vec4(1.0) : c;\n\
                     }\n";

fn shader(name: &str, source: &str) -> Vec<PostShaderSource> {
    vec![PostShaderSource {
        path: PathBuf::from(name),
        source: source.to_owned(),
    }]
}

fn renderer(surface_view_formats: bool) -> Option<Renderer> {
    let mut r = Renderer::headless(W, H, surface_view_formats)?;
    r.with_font(None, DEFAULT_FONT_SIZE_PX);
    Some(r)
}

/// A term with some coloured text in the top rows (the bottom rows stay
/// blank, under where the error banner goes) and a steady cursor, so
/// blink timing can't differ between two renderers.
fn term(r: &Renderer) -> Term {
    let (cw, ch) = r.cell_size();
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let (rows, cols) = ((H as f32 / ch) as u16, (W as f32 / cw) as u16);
    let mut t = Term::new(rows, cols, 0);
    Parser::new().advance(
        &mut t,
        b"\x1b[2 qhello \x1b[31mred \x1b[42mgreen bg\x1b[0m\r\n\x1b[1mbold\x1b[0m $ ",
    );
    t
}

fn feed(t: &mut Term, bytes: &[u8]) {
    Parser::new().advance(t, bytes);
}

/// Render a frame; clear the damage the way the binary does.
fn frame(r: &mut Renderer, t: &mut Term) -> (RenderOutcome, Vec<u8>) {
    let outcome = r.render_term(t).expect("render_term");
    if outcome == RenderOutcome::Rendered {
        t.clear_damage();
    }
    (outcome, r.read_offscreen())
}

fn px(img: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    [img[i], img[i + 1], img[i + 2], img[i + 3]]
}

#[test]
fn pass_through_chain_matches_the_plain_frame_path() {
    for view_formats in [true, false] {
        let (Some(mut plain), Some(mut post)) = (renderer(view_formats), renderer(view_formats))
        else {
            return;
        };
        post.set_post_shaders(&shader("p.glsl", PASS))
            .expect("compiles");
        let (mut ta, mut tb) = (term(&plain), term(&post));
        // Full frame, then a partial (LoadOp::Load) frame.
        let a = frame(&mut plain, &mut ta).1;
        let colours: std::collections::HashSet<&[u8]> = a.chunks(4).collect();
        assert!(colours.len() > 3, "the frame should show text");
        assert!(a == frame(&mut post, &mut tb).1, "full frame differs");
        feed(&mut ta, b"more\r\n");
        feed(&mut tb, b"more\r\n");
        let (oa, a) = frame(&mut plain, &mut ta);
        let (ob, b) = frame(&mut post, &mut tb);
        assert_eq!((oa, ob), (RenderOutcome::Rendered, RenderOutcome::Rendered));
        assert!(
            a == b,
            "partial frame differs (view_formats={view_formats})"
        );
    }
}

#[test]
fn chain_shades_the_content_but_not_the_ui() {
    let (Some(mut plain), Some(mut post)) = (renderer(true), renderer(true)) else {
        return;
    };
    post.set_post_shaders(&shader("inv.glsl", INVERT))
        .expect("compiles");
    for r in [&mut plain, &mut post] {
        r.set_error_banner(Some("banner"));
        r.set_debug_overlay(Some(" fps "));
    }
    let (mut ta, mut tb) = (term(&plain), term(&post));
    let a = frame(&mut plain, &mut ta).1;
    let b = frame(&mut post, &mut tb).1;
    // Content (a text row and the blank grid): inverted, alpha kept.
    for (x, y) in [(3, 8), (200, 120), (W - 5, H / 2)] {
        let (pa, pb) = (px(&a, x, y), px(&b, x, y));
        assert_eq!(
            pb,
            [255 - pa[0], 255 - pa[1], 255 - pa[2], pa[3]],
            "({x}, {y})"
        );
    }
    // Error banner (bottom row) and debug overlay (top right): as drawn
    // without shaders.
    let ch = plain.cell_size().1;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let banner_y = H - (ch as u32) / 2;
    for (x, y) in [(2, banner_y), (100, banner_y), (W - 3, 4)] {
        assert_eq!(px(&b, x, y), px(&a, x, y), "UI pixel ({x}, {y})");
    }
}

#[test]
fn post_only_frame_runs_on_an_event_without_damage() {
    let (Some(mut plain), Some(mut post)) = (renderer(true), renderer(true)) else {
        return;
    };
    post.set_post_shaders(&shader("m.glsl", MOUSE))
        .expect("compiles");
    let (mut ta, mut tb) = (term(&plain), term(&post));
    let first = frame(&mut post, &mut tb).1;
    let _ = frame(&mut plain, &mut ta);
    // Idle: nothing to draw.
    assert_eq!(frame(&mut post, &mut tb).0, RenderOutcome::Skipped);
    assert_eq!(post.next_redraw_deadline(&tb), None);
    // A press the shader reads: a frame, though nothing is damaged.
    post.set_pointer((10.0, 10.0), true);
    plain.set_pointer((10.0, 10.0), true);
    assert_eq!(
        post.next_redraw_deadline(&tb),
        Some(std::time::Duration::ZERO)
    );
    let (outcome, pressed) = frame(&mut post, &mut tb);
    assert_eq!(outcome, RenderOutcome::Rendered);
    assert_eq!(px(&pressed, 50, 100), [255; 4]);
    assert_eq!(frame(&mut plain, &mut ta).0, RenderOutcome::Skipped);
    // Release: back to the terminal image.
    post.set_pointer((10.0, 10.0), false);
    let released = frame(&mut post, &mut tb).1;
    assert!(released == first);
}

#[test]
fn no_post_deadline_while_sync_output_pauses_rendering() {
    let Some(mut post) = renderer(true) else {
        return;
    };
    post.set_post_shaders(&shader("m.glsl", MOUSE))
        .expect("compiles");
    let mut t = term(&post);
    let _ = frame(&mut post, &mut t);
    post.set_pointer((1.0, 1.0), true);
    feed(&mut t, b"\x1b[?2026h");
    assert!(t.pause_rendering());
    assert_eq!(post.next_redraw_deadline(&t), None);
    feed(&mut t, b"\x1b[?2026l");
    assert_eq!(
        post.next_redraw_deadline(&t),
        Some(std::time::Duration::ZERO)
    );
}

#[test]
fn turning_the_chain_off_restores_the_plain_frame() {
    let (Some(mut plain), Some(mut post)) = (renderer(true), renderer(true)) else {
        return;
    };
    post.set_post_shaders(&shader("inv.glsl", INVERT))
        .expect("compiles");
    let (mut ta, mut tb) = (term(&plain), term(&post));
    let a = frame(&mut plain, &mut ta).1;
    let inverted = frame(&mut post, &mut tb).1;
    assert!(inverted != a);
    // A failed reload keeps the inverting chain.
    let bad = shader(
        "bad.glsl",
        "void mainImage(out vec4 c, in vec2 p) { c = nope; }\n",
    );
    let errs = post.set_post_shaders(&bad).expect_err("fails");
    assert_eq!(errs[0].line, Some(1));
    assert!(post.has_post_shaders());
    feed(&mut tb, b"\x1b[H");
    feed(&mut ta, b"\x1b[H");
    let a = frame(&mut plain, &mut ta).1;
    assert_eq!(
        px(&frame(&mut post, &mut tb).1, 200, 120)[0],
        255 - px(&a, 200, 120)[0]
    );
    // Off: the plain path again, pixel for pixel.
    post.set_post_shaders(&[]).expect("off");
    assert!(!post.has_post_shaders());
    assert!(frame(&mut post, &mut tb).1 == a);
}
