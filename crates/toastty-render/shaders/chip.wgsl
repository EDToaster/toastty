// Scroll-to-bottom chip.
//
// A single screen-space quad around a circle; the fragment shader
// evaluates signed distances for the disc, its hairline ring and a
// chevron so every edge is anti-aliased at device resolution,
// independent of the cell grid and the font. All positions are in
// physical framebuffer pixels (content origin already applied on the
// CPU). Colours are straight (non-premultiplied) RGBA; the shader
// composites ring + icon over the fill and returns straight alpha for
// the pipeline's ALPHA_BLENDING.

struct Chip {
    viewport: vec2<f32>,
    center: vec2<f32>,
    radius: f32,
    // Physical px per logical px (window scale factor).
    scale: f32,
    // Global opacity (fade in / out), 0..1.
    alpha: f32,
    _pad: f32,
    fill: vec4<f32>,
    border: vec4<f32>,
    icon: vec4<f32>,
};

@group(0) @binding(0) var<uniform> chip: Chip;

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) px: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> VsOut {
    var corners = array<vec2<f32>, 6>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(-1.0, 1.0),
        vec2<f32>(1.0, -1.0),
        vec2<f32>(1.0, 1.0),
        vec2<f32>(-1.0, 1.0),
    );
    // Pad the quad by a couple of px so the AA fringe isn't clipped.
    let half = chip.radius + 2.0 * chip.scale;
    let p = chip.center + corners[vi] * half;
    var out: VsOut;
    out.clip = vec4<f32>(
        (p.x / chip.viewport.x) * 2.0 - 1.0,
        1.0 - (p.y / chip.viewport.y) * 2.0,
        0.0,
        1.0,
    );
    out.px = p;
    return out;
}

// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

// Straight-alpha "src over dst" on premultiplied accumulators.
fn over(dst: vec4<f32>, rgb: vec3<f32>, a: f32) -> vec4<f32> {
    return vec4<f32>(rgb * a + dst.rgb * (1.0 - a), a + dst.a * (1.0 - a));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let rel = in.px - chip.center;
    let d = length(rel) - chip.radius;
    let disc = clamp(0.5 - d, 0.0, 1.0);
    if disc <= 0.0 {
        discard;
    }
    // Hairline ring hugging the inside of the edge.
    let bw = max(1.0, chip.scale);
    let inner = clamp(0.5 - (d + bw), 0.0, 1.0);
    let ring = disc - inner;

    // Chevron pointing down, centred (nudged down slightly for optical balance).
    let s = chip.radius * 0.30;
    let o = vec2<f32>(0.0, chip.radius * 0.06);
    let a = vec2<f32>(-s, -s * 0.5) + o;
    let b = vec2<f32>(0.0, s * 0.5) + o;
    let c = vec2<f32>(s, -s * 0.5) + o;
    let hw = 0.9 * chip.scale;
    let cd = min(seg_dist(rel, a, b), seg_dist(rel, b, c)) - hw;
    let icon = clamp(0.5 - cd, 0.0, 1.0) * disc;

    var acc = vec4<f32>(chip.fill.rgb * chip.fill.a * disc, chip.fill.a * disc);
    acc = over(acc, chip.border.rgb, chip.border.a * ring);
    acc = over(acc, chip.icon.rgb, chip.icon.a * icon);
    let out_a = acc.a * chip.alpha;
    if out_a <= 0.0 {
        discard;
    }
    return vec4<f32>(acc.rgb / max(acc.a, 1e-5), out_a);
}
