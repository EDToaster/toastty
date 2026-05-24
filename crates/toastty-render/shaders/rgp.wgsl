// RGP 3D pass.
//
// One draw per placement. Vertex pulling: positions/normals/uvs
// arrive in interleaved vertex buffer (toastty-render builds the
// buffer on the CPU when it syncs from `RgpScene`).
//
// The per-draw uniform carries the model-view-projection matrix,
// the normal matrix, and the lit-color modulation (base color ×
// protocol `color` tint × `brightness`).
//
// Lighting: directional sun + ambient. Hardcoded for v1; promote
// to a globals UBO when we want per-frame customisation.
//
// Build-time validated by `build.rs`.

struct DrawUniforms {
    mvp:           mat4x4<f32>,
    // mat3 stored as mat4 for std140 alignment safety. Top-left
    // 3x3 is the normal matrix; the rest is unused.
    normal:        mat4x4<f32>,
    // RGBA. Already premultiplied with `brightness` on the CPU.
    color_tint:    vec4<f32>,
};

@group(0) @binding(0) var<uniform> draw: DrawUniforms;

// Directional sun direction (world space, points TO the light).
// Hardcoded for v1.
const SUN_DIR: vec3<f32> = vec3<f32>(0.408248, 0.816497, 0.408248);
const SUN_COLOR: vec3<f32> = vec3<f32>(1.0, 0.97, 0.92);
const AMBIENT: vec3<f32> = vec3<f32>(0.25, 0.27, 0.32);

struct Vertex {
    @location(0) position: vec3<f32>,
    @location(1) normal:   vec3<f32>,
    @location(2) uv:       vec2<f32>,
};

struct VsOut {
    @builtin(position) clip:    vec4<f32>,
    @location(0)       n_world: vec3<f32>,
    @location(1)       uv:      vec2<f32>,
};

@vertex
fn vs_main(v: Vertex) -> VsOut {
    var out: VsOut;
    out.clip = draw.mvp * vec4<f32>(v.position, 1.0);
    // Transform the normal by the normal matrix's upper-3x3.
    let nm = mat3x3<f32>(
        draw.normal[0].xyz,
        draw.normal[1].xyz,
        draw.normal[2].xyz,
    );
    out.n_world = normalize(nm * v.normal);
    out.uv = v.uv;
    return out;
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    // Lambertian shading: clamp(N · L) × sun_color + ambient.
    let n = normalize(in.n_world);
    let ndotl = max(dot(n, SUN_DIR), 0.0);
    let lit = SUN_COLOR * ndotl + AMBIENT;
    let rgb = draw.color_tint.rgb * lit;
    return vec4<f32>(rgb, draw.color_tint.a);
}
