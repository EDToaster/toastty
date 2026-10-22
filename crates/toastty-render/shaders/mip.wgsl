// Image mip-chain generation (SIZE-12).
//
// One full-target triangle per mip level: each output texel is the 2x2
// box average of the level above, read with `textureLoad` (no sampler,
// so no filtering surprises). The average is alpha-weighted: kitty
// images are straight (non-premultiplied) alpha, and a plain average
// would bleed the RGB of fully transparent texels into the edges.
//
// Source and target are `Rgba8UnormSrgb`, so `textureLoad` returns
// linear-light values and the store re-encodes: the box filter runs in
// linear light. Odd source sizes drop the last row / column (wgpu mip
// sizes round down); the clamp keeps 1-texel sources in bounds.

@group(0) @binding(0) var src: texture_2d<f32>;

@vertex
fn vs_main(@builtin(vertex_index) vi: u32) -> @builtin(position) vec4<f32> {
    // Oversized triangle covering the whole target: (-1,-1) (3,-1) (-1,3).
    let x = f32((vi << 1u) & 2u) * 2.0 - 1.0;
    let y = f32(vi & 2u) * 2.0 - 1.0;
    return vec4<f32>(x, y, 0.0, 1.0);
}

@fragment
fn fs_main(@builtin(position) pos: vec4<f32>) -> @location(0) vec4<f32> {
    let dst = vec2<i32>(pos.xy);
    let max_xy = vec2<i32>(textureDimensions(src)) - vec2<i32>(1, 1);
    let base = dst * 2;
    var rgb = vec3<f32>(0.0);
    var a = 0.0;
    for (var i = 0; i < 4; i++) {
        let p = min(base + vec2<i32>(i & 1, i >> 1u), max_xy);
        let t = textureLoad(src, p, 0);
        rgb += t.rgb * t.a;
        a += t.a;
    }
    // Un-premultiply; a fully transparent box stays transparent black.
    let out_rgb = select(vec3<f32>(0.0), rgb / a, a > 0.0);
    return vec4<f32>(out_rgb, a * 0.25);
}
