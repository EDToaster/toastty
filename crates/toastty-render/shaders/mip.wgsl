// Image mip-chain generation (SIZE-12). Same filter as `cpu_levels` in
// `src/image/mip.rs`.
//
// One full-target triangle per mip level. Each output texel filters the
// level above, read with `textureLoad` (no sampler, so no filtering
// surprises). Per axis: a 2-tap box for even sizes, and for odd sizes
// the standard 3-tap footprint `(n-i, n, i+1) / (2n+1)` over source
// texels `2i, 2i+1, 2i+2` (`n` = output size), so no source row or
// column is dropped. The average is alpha-weighted: kitty images are
// straight (non-premultiplied) alpha, and a plain average would bleed
// the RGB of fully transparent texels into the edges.
//
// Source and target are `Rgba8UnormSrgb`, so `textureLoad` returns
// linear-light values and the store re-encodes: the filter runs in
// linear light.

@group(0) @binding(0) var src: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Oversized triangle covering the whole target: (-1,-1) (3,-1) (-1,3).
    let x = f32((vi << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vi & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

// Tap weights along one axis for output texel `i` of a `size`-texel
// source; the taps are at `2i + k` (k = 0, 1, 2).
fn axis_weights(size: i32, i: i32) -> vec3<f32> {
    if (size <= 1) {
        return vec3<f32>(1.0, 0.0, 0.0);
    }
    if ((size & 1) == 0) {
        return vec3<f32>(0.5, 0.5, 0.0);
    }
    let n = size / 2;
    return vec3<f32>(f32(n - i), f32(n), f32(i + 1)) / f32(2 * n + 1);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dst = vec2<i32>(pos.xy);
    let dims = vec2<i32>(textureDimensions(src));
    let max_xy = dims - vec2<i32>(1, 1);
    let wx = axis_weights(dims.x, dst.x);
    let wy = axis_weights(dims.y, dst.y);
    // A 1-texel axis samples texel 0 (2i would be out of range).
    let base = select(dst * 2, vec2<i32>(0, 0), dims <= vec2<i32>(1, 1));
    var rgb = vec3<f32>(0.0);
    var a = 0.0;
    for (var ky = 0; ky < 3; ky++) {
        for (var kx = 0; kx < 3; kx++) {
            let w = wx[kx] * wy[ky];
            if (w > 0.0) {
                let p = min(base + vec2<i32>(kx, ky), max_xy);
                let t = textureLoad(src, p, 0);
                rgb += t.rgb * (t.a * w);
                a += t.a * w;
            }
        }
    }
    // Un-premultiply; a fully transparent footprint stays transparent black.
    let out_rgb = select(vec3<f32>(0.0), rgb / a, a > 0.0);
    return vec4<f32>(out_rgb, a);
}
