// Vertex stage for the user post-process passes (`crate::post`).
//
// Build-time validated by `build.rs`. One triangle that covers the
// target; the user's fragment shader (a separate module, compiled at
// runtime) reads `@builtin(position)` only, so nothing else is passed
// down.

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> @builtin(position) vec4<f32> {
    let x = f32((vid << 1u) & 2u);
    let y = f32(vid & 2u);
    return vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
}
