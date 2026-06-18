// M11a image pipeline shader.
//
// One draw call per image (kitty stores each image as its own GPU
// texture; max 16 active textures per [`ImagePipeline`]). We vertex-pull
// 4 corners per instance and sample a single bound texture. Output is
// alpha-over against whatever the swapchain holds.

struct ImageInstance {
    // Top-left in screen pixels.
    @location(0) pos: vec2<f32>,
    // Width / height in screen pixels.
    @location(1) size: vec2<f32>,
    // Sub-rect in normalized image coords (0..1).
    @location(2) uv_min: vec2<f32>,
    @location(3) uv_max: vec2<f32>,
};

struct Globals {
    viewport: vec2<f32>,
    // (origin_x_px, origin_y_px) — window-padding inset. Added to the
    // quad position below before the px->NDC map so the image layer is
    // inset by the same content origin as the text layer.
    content_origin: vec2<f32>,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(0) @binding(1) var image_tex: texture_2d<f32>;
@group(0) @binding(2) var image_sampler: sampler;

@vertex
fn vs_main(
    @builtin(vertex_index) vi: u32,
    instance: ImageInstance,
) -> VsOut {
    // Build a quad from the 4 corners. Triangle-strip layout: 0=TL, 1=TR,
    // 2=BL, 3=BR. We use triangle-list so 6 verts; the caller draws
    // `draw(0..6, instance_count)`.
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(0.0, 0.0), // TL
        vec2<f32>(1.0, 0.0), // TR
        vec2<f32>(0.0, 1.0), // BL
        vec2<f32>(1.0, 0.0), // TR
        vec2<f32>(1.0, 1.0), // BR
        vec2<f32>(0.0, 1.0), // BL
    );
    let c = corners[vi];
    // Screen pixels for this vertex (+ content origin = padding inset).
    let p = instance.pos + c * instance.size + globals.content_origin;
    // Clip-space (NDC): x in [-1, 1] left→right; y in [-1, 1] top→bottom
    // is flipped to top-down for image space (our screen has +y down).
    let ndc_x = (p.x / globals.viewport.x) * 2.0 - 1.0;
    let ndc_y = 1.0 - (p.y / globals.viewport.y) * 2.0;
    var out: VsOut;
    // NDC z = 0.5: image layer shares the cell-layer depth with
    // text so RGP placements can occlude or sit underneath both.
    // See `docs/decisions/rgp-protocol.md` §3.
    out.clip = vec4<f32>(ndc_x, ndc_y, 0.5, 1.0);
    out.uv = mix(instance.uv_min, instance.uv_max, c);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let sample = textureSample(image_tex, image_sampler, in.uv);
    return sample;
}
