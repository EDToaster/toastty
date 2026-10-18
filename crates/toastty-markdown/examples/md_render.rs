//! Render a Markdown file to a PNG with the hermetic fonts.
//!
//! `cargo run -p toastty-markdown --example md_render -- in.md out.png [cols] [light]`

use toastty_markdown::{LayoutParams, Renderer, ResourcePolicy, Theme};

fn light() -> Theme {
    Theme {
        fg: [0x1f, 0x23, 0x28, 0xff],
        bg: [0xff, 0xff, 0xff, 0xff],
        ansi: [
            [0x24, 0x29, 0x2f, 0xff],
            [0xcf, 0x22, 0x2e, 0xff],
            [0x11, 0x63, 0x29, 0xff],
            [0x4d, 0x2d, 0x00, 0xff],
            [0x09, 0x69, 0xda, 0xff],
            [0x82, 0x50, 0xdf, 0xff],
            [0x1b, 0x7c, 0x83, 0xff],
            [0x6e, 0x77, 0x81, 0xff],
            [0x57, 0x60, 0x6a, 0xff],
            [0xa4, 0x0e, 0x26, 0xff],
            [0x1a, 0x7f, 0x37, 0xff],
            [0x63, 0x3c, 0x01, 0xff],
            [0x21, 0x8b, 0xff, 0xff],
            [0xa4, 0x75, 0xf9, 0xff],
            [0x31, 0x92, 0xaa, 0xff],
            [0x8c, 0x95, 0x9f, 0xff],
        ],
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let src = std::fs::read_to_string(&args[1]).expect("read input");
    let cols: u16 = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(80);
    let theme = if args.get(4).is_some_and(|s| s == "light") {
        light()
    } else {
        Theme::default()
    };
    let params = LayoutParams {
        cols,
        cell_w: 10,
        cell_h: 22,
        font_px: 16.0,
        theme: theme.clone(),
        resources: ResourcePolicy::default(),
        max_rows: 2000,
    };
    let mut r = Renderer::hermetic();
    let t = std::time::Instant::now();
    let block = r.render(&src, &params).expect("render");
    let t1 = t.elapsed();
    let t = std::time::Instant::now();
    let _ = r.render(&src, &params).expect("render");
    eprintln!("rows {} first {t1:?} second {:?}", block.rows, t.elapsed());
    // Composite over the theme background for viewing.
    let mut px = block.image.pixels.clone();
    for p in px.as_chunks_mut::<4>().0 {
        let a = u32::from(p[3]);
        for (c, bg) in p[..3].iter_mut().zip(theme.bg) {
            *c = ((u32::from(*c) * a + u32::from(bg) * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    image::RgbaImage::from_raw(block.image.width, block.image.height, px)
        .expect("size")
        .save(&args[2])
        .expect("save");
    for (i, row) in block.text.iter().enumerate() {
        let line: Vec<String> = row
            .iter()
            .map(|r| format!("@{}:{}", r.col, r.text))
            .collect();
        println!("{i:3} {}", line.join("  "));
    }
    for l in &block.links {
        println!("link {l:?}");
    }
}
