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
//   - flag bits (cursor, color glyph, no glyph)
//
// Build-time validated by `build.rs`.

struct Globals {
    // (viewport_width_px, viewport_height_px, atlas_width_px, atlas_height_px)
    viewport_and_atlas: vec4<f32>,
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

@vertex
fn vs_main(
    @builtin(vertex_index) vid: u32,
    inst: CellInstance,
) -> VsOut {
    // Quad corners: 0=(0,0) 1=(1,0) 2=(0,1) 3=(1,1) via strip
    let cx = f32(vid & 1u);
    let cy = f32((vid >> 1u) & 1u);

    // Position in pixels.
    let px = inst.pos.x + cx * inst.size.x;
    let py = inst.pos.y + cy * inst.size.y;

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
    out.clip = vec4<f32>(clip_x, clip_y, 0.0, 1.0);
    out.uv = vec2<f32>(u, v);
    out.fg = inst.fg;
    out.bg = inst.bg;
    out.flags = inst.flags;
    out.has_glyph = has_glyph;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Cursor: solid bg fill, no glyph (yet — could overlay character
    // later by also passing the cursor's underlying cell).
    if ((in.flags & FLAG_CURSOR) != 0u) {
        return in.bg;
    }

    if (in.has_glyph < 0.5) {
        return in.bg;
    }

    let is_color = (in.flags & FLAG_COLOR_GLYPH) != 0u;
    if (is_color) {
        let s = textureSampleLevel(color_atlas, atlas_sampler, in.uv, 0.0);
        // Pre-multiplied: composite over the bg.
        return vec4<f32>(s.rgb + in.bg.rgb * (1.0 - s.a), s.a + in.bg.a * (1.0 - s.a));
    }

    // Monochrome glyph: sample alpha from R channel; blend fg over bg.
    let mask = textureSampleLevel(mask_atlas, atlas_sampler, in.uv, 0.0).r;
    let rgb = mix(in.bg.rgb, in.fg.rgb, mask);
    let a = mix(in.bg.a, in.fg.a, mask);
    return vec4<f32>(rgb, a);
}
