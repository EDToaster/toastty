// Placeholder shader.
//
// Build-time validated by `build.rs`. Used as a smoke-test so the shader
// pipeline (parse + validate) has something to chew on before M4b adds the
// real text/cell/post passes.
//
// Minimal valid module: one full-screen triangle vertex stage, one
// fragment stage that paints a constant color. Not bound by any pipeline
// yet — just here to keep the build hook honest.

struct VsOut {
    @builtin(position) clip: vec4<f32>,
    @location(0) uv: vec2<f32>,
};

@vertex
fn vs_main(@builtin(vertex_index) vid: u32) -> VsOut {
    // Triangle that covers the screen via the standard 3-vertex trick.
    let x = f32((vid << 1u) & 2u);
    let y = f32(vid & 2u);
    var out: VsOut;
    out.uv = vec2<f32>(x, y);
    out.clip = vec4<f32>(x * 2.0 - 1.0, 1.0 - y * 2.0, 0.0, 1.0);
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    return vec4<f32>(in.uv.x, in.uv.y, 0.5, 1.0);
}
