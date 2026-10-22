// UI shapes: the close-confirmation dialog's scrim, shadow, panel,
// buttons, keycaps and icons.
//
// One instanced quad per shape; the fragment shader evaluates a signed
// distance to a rounded rect so every edge is anti-aliased at device
// resolution. Positions are physical framebuffer px; colours are
// straight-alpha linear light. Output is premultiplied alpha for the
// pipeline's PREMULTIPLIED_ALPHA_BLENDING.

struct Globals {
    viewport: vec2<f32>,
    _pad: vec2<f32>,
};

@group(0) @binding(0) var<uniform> globals: Globals;

const KIND_RECT: u32 = 0u;
const KIND_SHADOW: u32 = 1u;
const KIND_PROMPT_ICON: u32 = 2u;
const KIND_RETURN_ICON: u32 = 3u;

struct Inst {
    @location(0) rect: vec4<f32>,
    @location(1) fill: vec4<f32>,
    @location(2) border: vec4<f32>,
    @location(3) radius: f32,
    @location(4) border_width: f32,
    @location(5) softness: f32,
    @location(6) kind: u32,
};

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) px: vec2<f32>,
    @location(1) @interpolate(flat) rect: vec4<f32>,
    @location(2) @interpolate(flat) fill: vec4<f32>,
    @location(3) @interpolate(flat) border: vec4<f32>,
    // (radius, border_width, softness)
    @location(4) @interpolate(flat) params: vec3<f32>,
    @location(5) @interpolate(flat) kind: u32,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32, inst: Inst) -> VsOut {
    let c = vec2<f32>(f32(vid & 1u), f32((vid >> 1u) & 1u));
    // Grow the quad so the AA fringe (or the shadow's blur) isn't clipped.
    var grow = 1.0;
    if inst.kind == KIND_SHADOW {
        grow = inst.softness + 1.0;
    }
    let lo = inst.rect.xy - vec2<f32>(grow, grow);
    let hi = inst.rect.zw + vec2<f32>(grow, grow);
    let p = mix(lo, hi, c);
    var out: VsOut;
    // Depth near the far end: tests nothing (Always), but resets the
    // depth under the dialog so its glyphs (z = 0.5, LessEqual) always
    // land on top of the panel.
    out.clip = vec4<f32>(
        (p.x / globals.viewport.x) * 2.0 - 1.0,
        1.0 - (p.y / globals.viewport.y) * 2.0,
        0.9,
        1.0,
    );
    out.px = p;
    out.rect = inst.rect;
    out.fill = inst.fill;
    out.border = inst.border;
    out.params = vec3<f32>(inst.radius, inst.border_width, inst.softness);
    out.kind = inst.kind;
    return out;
}

// Signed distance to a rounded box of half-size `b`, corner radius `r`.
fn round_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return length(max(q, vec2<f32>(0.0, 0.0))) + min(max(q.x, q.y), 0.0) - r;
}

// Distance from `p` to the segment `a`-`b`.
fn seg_dist(p: vec2<f32>, a: vec2<f32>, b: vec2<f32>) -> f32 {
    let pa = p - a;
    let ba = b - a;
    let h = clamp(dot(pa, ba) / dot(ba, ba), 0.0, 1.0);
    return length(pa - ba * h);
}

// Premultiplied "src over dst" with a straight-alpha source.
fn over(dst: vec4<f32>, rgb: vec3<f32>, a: f32) -> vec4<f32> {
    return vec4<f32>(rgb * a + dst.rgb * (1.0 - a), a + dst.a * (1.0 - a));
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let center = (in.rect.xy + in.rect.zw) * 0.5;
    let half = max((in.rect.zw - in.rect.xy) * 0.5, vec2<f32>(0.0, 0.0));
    let rel = in.px - center;
    let r = clamp(in.params.x, 0.0, min(half.x, half.y));
    let d = round_box(rel, half, r);
    var acc = vec4<f32>(0.0, 0.0, 0.0, 0.0);
    if in.kind == KIND_SHADOW {
        // Smoothstep falloff across +-softness around the edge.
        let s = max(in.params.z, 1.0);
        let t = clamp(0.5 - 0.5 * d / s, 0.0, 1.0);
        let cov = t * t * (3.0 - 2.0 * t);
        acc = vec4<f32>(in.fill.rgb * in.fill.a * cov, in.fill.a * cov);
    } else if in.kind == KIND_PROMPT_ICON {
        // `›_` on a 28-unit grid centred in the rect: a chevron and an
        // underscore with round caps.
        let u = half.x / 14.0;
        let a = vec2<f32>(-5.5, -3.5) * u;
        let b = vec2<f32>(-1.5, 0.0) * u;
        let c = vec2<f32>(-5.5, 3.5) * u;
        let e0 = vec2<f32>(0.5, 3.5) * u;
        let e1 = vec2<f32>(5.5, 3.5) * u;
        let dist = min(min(seg_dist(rel, a, b), seg_dist(rel, b, c)), seg_dist(rel, e0, e1));
        let cov = clamp(0.5 - (dist - in.params.y * 0.5), 0.0, 1.0);
        acc = vec4<f32>(in.fill.rgb * in.fill.a * cov, in.fill.a * cov);
    } else if in.kind == KIND_RETURN_ICON {
        // Return arrow: a stem down the right, a bar back to the left
        // and an arrowhead there, in a unit box centred in the rect.
        let u = min(half.x, half.y);
        let top = vec2<f32>(0.75, -0.7) * u;
        let corner = vec2<f32>(0.75, 0.3) * u;
        let tip = vec2<f32>(-0.75, 0.3) * u;
        let head_up = vec2<f32>(-0.3, -0.15) * u;
        let head_down = vec2<f32>(-0.3, 0.75) * u;
        let dist = min(
            min(seg_dist(rel, top, corner), seg_dist(rel, corner, tip)),
            min(seg_dist(rel, tip, head_up), seg_dist(rel, tip, head_down)),
        );
        let cov = clamp(0.5 - (dist - in.params.y * 0.5), 0.0, 1.0);
        acc = vec4<f32>(in.fill.rgb * in.fill.a * cov, in.fill.a * cov);
    } else {
        let cov = clamp(0.5 - d, 0.0, 1.0);
        acc = vec4<f32>(in.fill.rgb * in.fill.a * cov, in.fill.a * cov);
        if in.params.y > 0.0 {
            // Ring hugging the inside of the edge.
            let inner = clamp(0.5 - (d + in.params.y), 0.0, 1.0);
            acc = over(acc, in.border.rgb, in.border.a * (cov - inner));
        }
    }
    if acc.a <= 0.0 {
        discard;
    }
    return acc;
}
