// Text + cell rendering.
//
// Vertex pulling: one quad per cell-instance, expanded in the vertex
// stage. No vertex buffer; the four corner positions come from
// `vertex_index` inside a single instance.
//
// The instance buffer carries:
//   - cell position + size in pixels
//   - atlas UV range in pixels (uv_min = uv_max means "no glyph")
//   - foreground/background color
//   - flag bits (cursor, color glyph, no glyph, underline)
//
// Two fragment entry points so the cell layer can render in two passes
// with the RGP 3D pass sandwiched between them:
//   - `fs_bg`   — paints cell backgrounds. Runs before RGP with depth
//                 test/write disabled, so 3D objects overpaint bg.
//   - `fs_glyph` — paints glyphs, cursor, underline. Runs after RGP at
//                  z=0.5 with LessEqual depth test, so 3D objects with
//                  protocol `depth < 0` occlude glyphs (and depth > 0
//                  glyphs occlude 3D).
//
// Build-time validated by `build.rs`.

struct Globals {
    // (viewport_width_px, viewport_height_px, atlas_width_px, atlas_height_px)
    viewport_and_atlas: vec4<f32>,
    // Pixel-space cursor bounds (x_min, y_min, x_max, y_max). Degenerate
    // (x_min == x_max or y_min == y_max) when the cursor is hidden, so the
    // `in_cursor` test below never matches and the glyph pass keeps its
    // normal foreground color.
    cursor_rect: vec4<f32>,
    // Linear-light cursor color. The glyph pass picks a black-or-white
    // contrast color from this (Rec. 709 luminance) for any glyph pixel
    // inside `cursor_rect`, so the glyph stays maximally legible
    // against the cursor block.
    cursor_color: vec4<f32>,
    // (origin_x_px, origin_y_px, _, _) — window-padding inset. Added to
    // px,py in the vertex shader before the px->NDC map.
    content_origin: vec4<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var mask_atlas: texture_2d<f32>;
@group(0) @binding(2) var color_atlas: texture_2d<f32>;
@group(0) @binding(3) var atlas_sampler: sampler;

struct CellInstance {
    @location(0) pos:     vec2<f32>,
    @location(1) size:    vec2<f32>,
    @location(2) uv_min:  vec2<f32>,
    @location(3) uv_max:  vec2<f32>,
    @location(4) fg:      vec4<f32>,
    @location(5) bg:      vec4<f32>,
    @location(6) flags:   u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv:    vec2<f32>,
    @location(1) fg:    vec4<f32>,
    @location(2) bg:    vec4<f32>,
    @location(3) flags: u32,
    @location(4) has_glyph: f32,
};

const FLAG_CURSOR: u32       = 1u;
const FLAG_COLOR_GLYPH: u32  = 2u;
const FLAG_NO_GLYPH: u32     = 4u;
const FLAG_UNDERLINE: u32    = 8u;

@vertex
fn vs_main(
    @builtin(vertex_index) vid: u32,
    inst: CellInstance,
) -> VsOut {
    // Quad corners: 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1) via strip
    let cx = f32(vid & 1u);
    let cy = f32((vid >> 1u) & 1u);

    // Position in pixels.
    let px = inst.pos.x + cx * inst.size.x + globals.content_origin.x;
    let py = inst.pos.y + cy * inst.size.y + globals.content_origin.y;

    // Convert to clip-space [-1, 1]. y is flipped (pixel y=0 = top of
    // screen, clip y=+1 = top of screen).
    let vw = globals.viewport_and_atlas.x;
    let vh = globals.viewport_and_atlas.y;
    let clip_x = (px / vw) * 2.0 - 1.0;
    let clip_y = 1.0 - (py / vh) * 2.0;

    // Atlas UV in normalized [0, 1] coords for the texture sample.
    let aw = globals.viewport_and_atlas.z;
    let ah = globals.viewport_and_atlas.w;
    let u = mix(inst.uv_min.x, inst.uv_max.x, cx) / aw;
    let v = mix(inst.uv_min.y, inst.uv_max.y, cy) / ah;

    let no_glyph_flagged: f32 = select(0.0, 1.0, (inst.flags & FLAG_NO_GLYPH) != 0u);
    let zero_extent: f32 = select(
        0.0, 1.0,
        inst.uv_min.x == inst.uv_max.x || inst.uv_min.y == inst.uv_max.y
    );
    let has_glyph = 1.0 - max(no_glyph_flagged, zero_extent);

    var out: VsOut;
    // NDC z = 0.5: cell glyph layer sits in the middle of the depth
    // buffer so RGP objects with protocol `depth < 0` render in
    // front and `depth > 0` render behind. The bg pass uses a
    // pipeline with depth test/write disabled, so this z is only
    // consulted by the glyph pass.
    out.clip = vec4<f32>(clip_x, clip_y, 0.5, 1.0);
    out.uv = vec2<f32>(u, v);
    out.fg = inst.fg;
    out.bg = inst.bg;
    out.flags = inst.flags;
    out.has_glyph = has_glyph;
    return out;
}

// Background pass. Paints cell bgs AND the cursor block. Output is
// premultiplied alpha; the pipeline uses a REPLACE (One/Zero) blend.
//
// The cursor moved to the bg pass so glyphs render *over* the cursor
// instead of being obscured by it. The glyph pass then inverts the
// glyph color where it overlaps `cursor_rect`, keeping the glyph
// visible against the cursor.
@fragment
fn fs_bg(in: VsOut) -> @location(0) vec4<f32> {
    // Underline is still a foreground decoration — it renders in the
    // glyph pass at z=0.5 so 3D can occlude it.
    if ((in.flags & FLAG_UNDERLINE) != 0u) {
        discard;
    }
    // Glyph-shaped instances would only paint a glyph-sized rect, not
    // the full cell; the cell bg is emitted as a separate FLAG_NO_GLYPH
    // instance and handled above.
    if (in.has_glyph >= 0.5) {
        discard;
    }
    // For both ordinary cell bg quads and the cursor instance, the
    // color we want to paint is in `bg` (cursor color for the cursor
    // instance — see CellInstance::cursor_for_shape).
    //
    // Output premultiplied alpha (rgb * a, a): no-op when in.bg.a==1
    // (rgb*1==rgb); enables premultiplied transparency when in.bg.a<1.
    return vec4<f32>(in.bg.rgb * in.bg.a, in.bg.a);
}

// Test whether the current framebuffer pixel is inside the cursor's
// pixel-space rect. Degenerate rects (zero size) never match, which is
// the contract used when the cursor is hidden.
fn pixel_in_cursor(px: vec2<f32>) -> bool {
    let r = globals.cursor_rect;
    return px.x >= r.x && px.x < r.z && px.y >= r.y && px.y < r.w;
}

// Maximum-contrast foreground for a glyph sitting on the cursor block:
// pick black or white based on the cursor's perceived luminance. Naive
// `1 - cursor_color` looks dim because mid-tone cursors invert to other
// mid-tones; this gives clean visibility for any cursor color.
//
// Uses Rec. 709 luminance weights against the linear-light cursor color
// (the cursor color in `globals` is already linear). The 0.5 threshold
// is in linear-light space, which biases slightly toward "use black" —
// good for the common warm/bright cursor case.
fn cursor_contrast_color() -> vec3<f32> {
    let c = globals.cursor_color.rgb;
    let lum = dot(c, vec3<f32>(0.2126, 0.7152, 0.0722));
    return select(vec3<f32>(1.0, 1.0, 1.0), vec3<f32>(0.0, 0.0, 0.0), lum > 0.5);
}

// Glyph pass. Paints glyphs and underline strips. The cursor block is
// rendered in the bg pass; here we only invert the glyph color where it
// overlaps the cursor so the glyph stays legible. Output is
// premultiplied alpha; the pipeline uses One/OneMinusSrcAlpha blend.
@fragment
fn fs_glyph(in: VsOut) -> @location(0) vec4<f32> {
    // Cursor block already painted in the bg pass — drop it here.
    if ((in.flags & FLAG_CURSOR) != 0u) {
        discard;
    }

    // Underline strip: SGR underline / OSC 8 hyperlink. The color is
    // stored in `bg` for backward compatibility with the old combined
    // shader (see `underline_instance`).
    if ((in.flags & FLAG_UNDERLINE) != 0u) {
        return vec4<f32>(in.bg.rgb * in.bg.a, in.bg.a);
    }

    // Plain bg quad — handled by the bg pass, skip here.
    if (in.has_glyph < 0.5) {
        discard;
    }

    // If this fragment lands inside the cursor rect, recolor the glyph
    // to a luminance-based black or white so it always contrasts
    // strongly against the cursor block underneath. Applies to both
    // mono and color glyphs.
    let on_cursor = pixel_in_cursor(in.clip.xy);
    let contrast_rgb = cursor_contrast_color();

    let is_color = (in.flags & FLAG_COLOR_GLYPH) != 0u;
    if (is_color) {
        // Color atlas is already premultiplied.
        let sampled = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        if (on_cursor) {
            // Preserve the emoji silhouette via `sampled.a`, but recolor
            // to the contrast color so it stays readable.
            return vec4<f32>(contrast_rgb * sampled.a, sampled.a);
        }
        return sampled;
    }

    // Monochrome glyph: mask in R channel, modulated by fg color.
    // Output premultiplied so it blends correctly over whatever the bg
    // pass / RGP pass laid down.
    let mask = textureSampleLevel(mask_atlas, atlas_sampler, in.uv, 0.0).r;
    let a = mask * in.fg.a;
    let rgb = select(in.fg.rgb, contrast_rgb, on_cursor);
    return vec4<f32>(rgb * a, a);
}
