//! Kitty animation round trips through `Term` (§9 "Animation"): frames,
//! control, composition, frame deletes, playback and replies. Expected
//! replies were measured on kitty 0.49.2.

use std::time::{Duration, Instant};

use base64::Engine;
use toastty_graphics::Animation;
use toastty_parser::Parser;

use super::*;

fn feed(t: &mut Term, bytes: &[u8]) {
    let mut p = Parser::new();
    p.advance(t, bytes);
}

fn b64(data: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// `ESC _ G keys ; base64(data) ESC \`.
fn apc(keys: &str, data: &[u8]) -> Vec<u8> {
    let mut v = format!("\x1b_G{keys}").into_bytes();
    if !data.is_empty() {
        v.push(b';');
        v.extend_from_slice(b64(data).as_bytes());
    }
    v.extend_from_slice(b"\x1b\\");
    v
}

fn solid(px: [u8; 4], w: u32, h: u32) -> Vec<u8> {
    px.repeat((w * h) as usize)
}

const R: [u8; 4] = [255, 0, 0, 255];
const G: [u8; 4] = [0, 255, 0, 255];
const B: [u8; 4] = [0, 0, 255, 255];
const W: [u8; 4] = [255, 255, 255, 255];

/// Replies queued so far, as text, drained.
fn replies(t: &mut Term) -> Vec<String> {
    let bytes = t.drain_pty_replies();
    let s = String::from_utf8_lossy(&bytes).into_owned();
    s.split("\x1b\\")
        .filter(|r| !r.is_empty())
        .map(|r| r.trim_start_matches("\x1b_G").to_string())
        .collect()
}

/// A term with image 1 (4x2, `root` colour) placed at the home cell.
fn term_with_image(root: [u8; 4]) -> Term {
    let mut t = Term::new(10, 40, 0);
    feed(
        &mut t,
        &apc("a=T,f=32,s=4,v=2,i=1,q=2,C=1", &solid(root, 4, 2)),
    );
    t
}

/// The store key of client image `id` on the active screen (#42), or
/// `ImageKey::NONE` when it isn't stored.
fn key(t: &Term, id: u32) -> ImageKey {
    t.image_store().key_of(id).unwrap_or(ImageKey::NONE)
}

fn shown(t: &Term, id: u32) -> [u8; 4] {
    t.image_registry().get(key(t, id)).unwrap().pixels[..4]
        .try_into()
        .unwrap()
}

fn current(t: &Term, id: u32) -> u32 {
    t.image_registry()
        .animation(key(t, id))
        .map_or(1, Animation::current_frame)
}

#[test]
fn frame_replies_match_kitty() {
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,r=2,f=32,s=4,v=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,r=9,f=32,s=4,v=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,c=7,f=32,s=4,v=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,f=32,s=5,v=2", &solid(B, 5, 2)));
    feed(&mut t, &apc("a=f,i=1,f=32,s=4,v=2", &solid(B, 4, 2)[..10]));
    feed(
        &mut t,
        &apc("a=f,i=1,p=5,q=1,f=32,s=4,v=2", &solid(B, 4, 2)),
    );
    feed(&mut t, &apc("a=f,i=1,p=5,f=32,s=4,v=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,i=42,f=32,s=4,v=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,f=32,s=4,v=2", &solid(B, 4, 2)));
    assert_eq!(
        replies(&mut t),
        [
            "i=1,r=2;OK",
            "i=1,r=2;OK",
            "i=1,r=3;OK",
            "i=1,r=4;EINVAL:No frame with number: 7 found",
            "i=1,r=4;EINVAL:Frame width 5 larger than image width: 4",
            "i=1,r=4;ENODATA:Insufficient image data: 10 < 32",
            "i=1,p=5,r=5;OK",
            "i=42;ENOENT:Animation command refers to non-existent image with id: 42 and number: 0",
        ]
    );
    assert_eq!(t.image_registry().frame_count(key(&t, 1)), Some(5));
}

#[test]
fn base_naming_the_frame_being_made_replies_ok() {
    // #47 review 5, kitty 0.49.2: `c=2` while creating frame 2 is OK;
    // `c=3` (past it) is the usual EINVAL.
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,c=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,c=4,f=32,s=4,v=2", &solid(G, 4, 2)));
    assert_eq!(
        replies(&mut t),
        ["i=1,r=2;OK", "i=1,r=3;EINVAL:No frame with number: 4 found"]
    );
}

#[test]
fn frames_by_image_number_reply_with_the_number() {
    let mut t = Term::new(10, 40, 0);
    feed(&mut t, &apc("a=t,f=32,s=4,v=2,I=7", &solid(R, 4, 2)));
    feed(&mut t, &apc("a=f,I=7,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=c,I=7,r=1,c=2", &[]));
    feed(&mut t, &apc("a=c,I=7,r=1,c=5", &[]));
    feed(&mut t, &apc("a=a,I=8,s=3", &[]));
    assert_eq!(
        replies(&mut t),
        [
            "i=1,I=7;OK",
            "I=7,r=2;OK",
            "I=7;OK",
            "I=7;ENOENT:No destination frame number 5 exists in image id: 1\n",
            "I=8;ENOENT:Animation command refers to non-existent image with id: 0 and number: 8",
        ]
    );
}

#[test]
fn animation_control_never_replies() {
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    for keys in [
        "a=a,i=1,s=3",
        "a=a,i=1,c=9",
        "a=a,i=1,r=9,z=10",
        "a=a,i=1,s=9,v=3",
        "a=a,s=3",
    ] {
        feed(&mut t, &apc(keys, &[]));
    }
    assert!(replies(&mut t).is_empty());
    feed(&mut t, &apc("a=a,i=99", &[]));
    assert_eq!(
        replies(&mut t),
        ["i=99;ENOENT:Animation command refers to non-existent image with id: 99 and number: 0"]
    );
}

#[test]
fn composition_replies_match_kitty() {
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    for keys in [
        "a=c,i=1",
        "a=c,i=1,r=1,c=2",
        "a=c,i=1,r=1,c=2,q=1",
        "a=c,i=1,r=3,c=1",
        "a=c,i=1,r=1,c=2,w=5",
        "a=c,i=1,r=1,c=2,X=1",
        "a=c,i=1,r=2,c=2,w=1,h=1",
        "a=c,i=1,r=2,c=2,w=1,h=1,x=1",
        "a=c,i=1,r=1,c=3,q=2",
        "a=c,r=1,c=1",
    ] {
        feed(&mut t, &apc(keys, &[]));
    }
    assert_eq!(
        replies(&mut t),
        [
            "i=1;ENOENT:No source frame number 0 exists in image id: 1\n",
            "i=1;OK",
            "i=1;ENOENT:No source frame number 3 exists in image id: 1\n",
            "i=1;EINVAL:The destination rectangle is out of bounds",
            "i=1;EINVAL:The source rectangle is out of bounds",
            "i=1;EINVAL:The source and destination rectangles overlap and the src and destination frames are the same",
            "i=1;OK",
        ]
    );
}

/// Split `data`'s base64 into `n`-byte chunks: the first chunk carries
/// `first` keys, the others `cont`.
fn chunked(first: &str, cont: &str, data: &[u8], n: usize) -> Vec<u8> {
    let enc = b64(data);
    let parts: Vec<&str> = enc
        .as_bytes()
        .chunks(n)
        .map(|c| std::str::from_utf8(c).unwrap())
        .collect();
    let mut out = Vec::new();
    for (k, p) in parts.iter().enumerate() {
        let more = u8::from(k + 1 < parts.len());
        let keys = if k == 0 {
            format!("{first},m={more}")
        } else {
            format!("{cont}m={more}")
        };
        out.extend_from_slice(format!("\x1b_G{keys};{p}\x1b\\").as_bytes());
    }
    out
}

#[test]
fn chunked_frames_route_their_continuations() {
    // #37 review: continuation chunks of an `a=f` upload, with or
    // without `a=f` / `i=` on them, finish the frame (kitty: `r=4;OK`)
    // instead of starting an anonymous transmit.
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(
        &mut t,
        &chunked("a=f,i=1,f=32,s=4,v=2", "a=f,i=1,", &solid(B, 4, 2), 8),
    );
    // Last chunk without ids: like kitty, no reply (the frame is made).
    feed(
        &mut t,
        &chunked("a=f,i=1,f=32,s=4,v=2", "a=f,", &solid(B, 4, 2), 8),
    );
    // Bare continuations: answered with the first chunk's ids.
    feed(
        &mut t,
        &chunked("a=f,i=1,f=32,s=4,v=2", "", &solid(B, 4, 2), 8),
    );
    // `q=1` on the first chunk covers the whole upload.
    feed(
        &mut t,
        &chunked("a=f,i=1,q=1,f=32,s=4,v=2", "a=f,i=1,", &solid(B, 4, 2), 8),
    );
    // An edit, chunked.
    feed(
        &mut t,
        &chunked("a=f,i=1,r=2,f=32,s=4,v=2", "a=f,i=1,", &solid(W, 4, 2), 8),
    );
    assert_eq!(replies(&mut t), ["i=1,r=4;OK", "i=1,r=6;OK", "i=1,r=2;OK"]);
    assert_eq!(t.image_registry().frame_count(key(&t, 1)), Some(7));
    assert_eq!(t.image_registry().len(), 1, "no stray anonymous image");
    // A chunked frame for an unknown image: one ENOENT, and the
    // continuations are dropped without a reply.
    feed(
        &mut t,
        &chunked("a=f,i=77,f=32,s=4,v=2", "a=f,", &solid(B, 4, 2), 8),
    );
    assert_eq!(
        replies(&mut t),
        ["i=77;ENOENT:Animation command refers to non-existent image with id: 77 and number: 0"]
    );
    assert_eq!(t.image_registry().len(), 1);
}

#[test]
fn control_changes_the_displayed_frame_and_redraws() {
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    assert_eq!(shown(&t, 1), R, "a new frame doesn't change the display");
    let rev = t.image_revision();
    let generation = t.image_registry().generation(key(&t, 1));
    feed(&mut t, &apc("a=a,i=1,c=2", &[]));
    assert_eq!(shown(&t, 1), G);
    assert_ne!(t.image_revision(), rev);
    assert_ne!(t.image_registry().generation(key(&t, 1)), generation);
}

/// icat's stream for a GIF: root, root gap, loading, frames each built
/// on the previous one, then run forever.
fn icat_like(t: &mut Term, frames: u32, gap: u32) {
    feed(
        t,
        &apc("a=T,q=2,f=32,s=4,v=2,I=5,C=1", &solid([0, 0, 0, 255], 4, 2)),
    );
    feed(t, &apc(&format!("a=a,v=1,r=1,I=5,z={gap}"), &[]));
    for k in 1..frames {
        if k == 2 {
            feed(t, &apc(&format!("a=a,s=2,v=1,r=1,I=5,z={gap}"), &[]));
        }
        let mut px = [0u8, 0, 0, 255];
        px[0] = k as u8;
        let keys = format!("a=f,q=2,f=32,s=1,v=1,x={},c={k},I=5,z={gap}", k % 4);
        feed(t, &apc(&keys, &px));
    }
    feed(t, &apc(&format!("a=a,s=3,v=1,r=1,I=5,z={gap}"), &[]));
}

/// Tick every `step` ms up to `total`; the frame numbers shown, in order.
fn play(t: &mut Term, id: u32, t0: Instant, step: u64, total: u64) -> Vec<u32> {
    let mut seen = vec![current(t, id)];
    let mut ms = 0;
    while ms <= total {
        t.tick_animations(t0 + Duration::from_millis(ms));
        if *seen.last().unwrap() != current(t, id) {
            seen.push(current(t, id));
        }
        ms += step;
    }
    seen
}

#[test]
fn icat_gif_stream_plays_every_frame_in_order_and_loops() {
    let mut t = Term::new(10, 40, 0);
    icat_like(&mut t, 12, 100);
    assert!(replies(&mut t).is_empty(), "icat's stream gets no replies");
    let id = t.image_id_for_number(5).unwrap();
    assert_eq!(t.image_registry().frame_count(key(&t, id)), Some(12));
    let seen = play(&mut t, id, Instant::now(), 10, 2500);
    let want: Vec<u32> = (1..=12).chain(1..=12).chain(1..=2).collect();
    assert_eq!(seen, want);
    // The display is each frame, composed: frame k sets pixel k%4 of
    // row 0 to red k (on top of frames before it).
    let a = t.image_registry().get(key(&t, id)).unwrap();
    assert_eq!(a.pixels[4..8], [1, 0, 0, 255], "frame 2: pixel 1 is red 1");
}

#[test]
fn full_size_frames_built_on_the_previous_one_play() {
    // icat's stream for a scaled GIF: every frame covers the image and
    // names the previous frame as its base (`c=k`), RGB, 60 ms.
    let (w, h) = (60u32, 40u32);
    let mut t = Term::new(10, 40, 0);
    let rgb = |v: u8| [v, 0, 0].repeat((w * h) as usize);
    feed(
        &mut t,
        &apc(&format!("a=T,q=2,f=24,s={w},v={h},I=9,C=1"), &rgb(0)),
    );
    feed(&mut t, &apc("a=a,v=1,r=1,I=9,z=60", &[]));
    for k in 1..10u32 {
        let keys = format!("a=f,q=2,f=24,s={w},v={h},c={k},I=9,z=60");
        feed(&mut t, &apc(&keys, &rgb(k as u8)));
    }
    feed(&mut t, &apc("a=a,s=3,v=1,r=1,I=9,z=60", &[]));
    let id = t.image_id_for_number(9).unwrap();
    let seen = play(&mut t, id, Instant::now(), 10, 1150);
    let want: Vec<u32> = (1..=10).chain(1..=10).collect();
    assert_eq!(seen, want);
}

#[test]
fn next_animation_frame_is_only_set_while_running() {
    let mut t = term_with_image(R);
    let t0 = Instant::now();
    assert!(!t.tick_animations(t0));
    assert_eq!(t.next_animation_frame(), None, "idle: no wake-ups");
    feed(
        &mut t,
        &apc("a=f,i=1,q=2,f=32,s=4,v=2,z=100", &solid(G, 4, 2)),
    );
    t.tick_animations(t0);
    assert_eq!(t.next_animation_frame(), None, "stopped by default");
    feed(&mut t, &apc("a=a,i=1,s=3,r=1,z=100", &[]));
    t.tick_animations(t0);
    assert_eq!(
        t.next_animation_frame(),
        Some(t0 + Duration::from_millis(100))
    );
    assert!(t.tick_animations(t0 + Duration::from_millis(100)));
    assert_eq!(shown(&t, 1), G);
    feed(&mut t, &apc("a=a,i=1,s=1", &[]));
    t.tick_animations(t0 + Duration::from_millis(300));
    assert_eq!(t.next_animation_frame(), None);
}

#[test]
fn only_displayed_images_play() {
    let mut t = Term::new(10, 40, 0);
    feed(&mut t, &apc("a=t,f=32,s=4,v=2,i=1,q=2", &solid(R, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=a,i=1,s=3,r=1,z=40", &[]));
    let t0 = Instant::now();
    assert!(!t.tick_animations(t0 + Duration::from_secs(1)));
    assert_eq!(t.next_animation_frame(), None, "not placed: no timer");
    feed(&mut t, b"\x1b[H");
    feed(&mut t, &apc("a=p,i=1,q=2,C=1", &[]));
    t.tick_animations(t0);
    assert!(t.next_animation_frame().is_some());
}

#[test]
fn placeholder_images_animate() {
    // An image shown only through Unicode placeholders (a `U=1` virtual
    // placement, no real one) plays like any other.
    let mut t = Term::new(10, 40, 0);
    feed(
        &mut t,
        &apc("a=T,U=1,f=32,s=4,v=2,i=3,q=2,c=4,r=1", &solid(R, 4, 2)),
    );
    feed(&mut t, &apc("a=f,i=3,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=a,i=3,s=3,r=1,z=40", &[]));
    assert!(t.image_grid().is_empty(), "virtual only");
    let t0 = Instant::now();
    t.tick_animations(t0);
    assert!(t.next_animation_frame().is_some());
    assert!(t.tick_animations(t0 + Duration::from_millis(40)));
    assert_eq!(shown(&t, 3), G);
}

#[test]
fn retransmitting_the_root_drops_the_frames() {
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=t,f=32,s=4,v=2,i=1,q=2", &solid(B, 4, 2)));
    feed(&mut t, &apc("a=f,i=1,f=32,s=4,v=2", &solid(G, 4, 2)));
    assert_eq!(replies(&mut t), ["i=1,r=2;OK"]);
}

#[test]
fn delete_frames_d_f_and_d_upper_f() {
    // Three frames R, G, B, showing B.
    let three = |t: &mut Term| {
        feed(t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
        feed(t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(B, 4, 2)));
        feed(t, &apc("a=a,i=1,c=3", &[]));
    };
    let mut t = term_with_image(R);
    three(&mut t);
    feed(&mut t, &apc("a=d,d=f,i=1,r=3", &[]));
    assert_eq!(
        (t.image_registry().frame_count(key(&t, 1)), shown(&t, 1)),
        (Some(2), G)
    );
    // d=F with frames left deletes a frame too, not the image.
    feed(&mut t, &apc("a=d,d=F,i=1", &[]));
    assert_eq!(
        (t.image_registry().frame_count(key(&t, 1)), shown(&t, 1)),
        (Some(1), G)
    );
    // One frame left: d=f does nothing, d=F frees the image.
    feed(&mut t, &apc("a=d,d=f,i=1", &[]));
    assert!(t.image_registry().contains(key(&t, 1)));
    feed(&mut t, &apc("a=d,d=F,i=1", &[]));
    assert!(!t.image_registry().contains(key(&t, 1)));
    assert!(t.image_grid().is_empty());
    assert!(replies(&mut t).is_empty(), "deletes never reply");
    // By number.
    let mut t = Term::new(10, 40, 0);
    feed(&mut t, &apc("a=t,f=32,s=4,v=2,I=3,q=2", &solid(R, 4, 2)));
    feed(&mut t, &apc("a=f,I=3,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=d,d=f,I=3", &[]));
    let id = t.image_id_for_number(3).unwrap();
    assert_eq!(t.image_registry().frame_count(key(&t, id)), Some(1));
    assert_eq!(shown(&t, id), G, "d=f without r= deletes the root");
}

#[test]
fn frames_count_against_the_frame_budget() {
    let mut t = term_with_image(R);
    // A frame budget for the root copy and two frames (32 bytes each, too
    // small to compress, plus the per-frame overhead); the third gets
    // kitty's ENOSPC.
    let per = 32 + toastty_graphics::frames::FRAME_OVERHEAD;
    t.set_image_cap(3 * per / 2);
    assert_eq!(t.image_registry().frame_cap_bytes(), 3 * per);
    for _ in 0..3 {
        feed(&mut t, &apc("a=f,i=1,f=32,s=4,v=2", &solid(G, 4, 2)));
    }
    assert_eq!(
        replies(&mut t),
        [
            "i=1,r=2;OK",
            "i=1,r=3;OK",
            "i=1,r=4;ENOSPC:Cache size exceeded cannot add new frames"
        ]
    );
    let r = t.image_registry();
    // The image itself costs its pixels plus #42's per-image overhead.
    assert_eq!(
        (r.total_bytes(), r.frame_bytes()),
        (32 + toastty_graphics::IMAGE_OVERHEAD_BYTES, 3 * per)
    );
}

#[test]
fn compressed_frames_stretch_the_budget() {
    // 64x64 frames are 16 KiB raw; a flat colour compresses to a few
    // dozen bytes. A frame budget that holds 4 raw frames holds all 40.
    let mut t = Term::new(10, 40, 0);
    feed(
        &mut t,
        &apc("a=T,f=32,s=64,v=64,i=1,q=2,C=1", &solid(R, 64, 64)),
    );
    t.set_image_cap(64 * 64 * 4 * 2);
    for k in 0..40u8 {
        let px = [k, 255 - k, 0, 255];
        feed(
            &mut t,
            &apc("a=f,i=1,q=2,f=32,s=64,v=64", &solid(px, 64, 64)),
        );
    }
    assert!(replies(&mut t).is_empty());
    assert_eq!(t.image_registry().frame_count(key(&t, 1)), Some(41));
    assert!(t.image_registry().frame_bytes() < 64 * 64 * 4);
    // And they come back intact.
    feed(&mut t, &apc("a=a,i=1,c=41", &[]));
    assert_eq!(shown(&t, 1), [39, 216, 0, 255]);
}

#[test]
fn the_stashed_screens_animations_wait() {
    // Merge train (#47 x #42): frames live in the screen's own store. While
    // the alt screen is up, the primary's animation is stashed with it and
    // doesn't play (and doesn't wake the event loop); it plays again, frames
    // intact, once the primary is back.
    let mut t = term_with_image(R);
    feed(&mut t, &apc("a=f,i=1,q=2,f=32,s=4,v=2", &solid(G, 4, 2)));
    feed(&mut t, &apc("a=a,i=1,s=3,r=1,z=40", &[]));
    let t0 = Instant::now();
    feed(&mut t, b"\x1b[?1049h");
    assert!(!t.tick_animations(t0 + Duration::from_secs(1)));
    assert_eq!(t.next_animation_frame(), None);
    feed(&mut t, b"\x1b[?1049l");
    assert_eq!(
        t.image_registry().frame_count(key(&t, 1)),
        Some(2),
        "frames kept"
    );
    t.tick_animations(t0);
    assert!(t.next_animation_frame().is_some());
}
