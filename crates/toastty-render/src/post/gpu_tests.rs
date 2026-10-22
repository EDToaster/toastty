//! GPU tests for [`PostChain`]: frames go through real wgpu passes and
//! are read back.
//!
//! The "swapchain" here is an `Rgba8UnormSrgb` texture with a non-sRGB
//! view format, like the configured surface, so both output paths (direct
//! non-sRGB view, and ping-pong + copy) are covered.

use std::path::PathBuf;

use wgpu::{Device, Queue, TextureFormat};

use super::*;

const FORMAT: TextureFormat = TextureFormat::Rgba8UnormSrgb;

fn shader(name: &str, source: &str) -> PostShaderSource {
    PostShaderSource {
        path: PathBuf::from(name),
        source: source.to_owned(),
    }
}

const GLSL_PASS: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                         \x20   fragColor = texture(iChannel0, fragCoord / iResolution.xy);\n\
                         }\n";

const WGSL_PASS: &str = "fn mainImage(fragCoord: vec2f) -> vec4f {\n\
                         \x20   return textureSample(iChannel0, iChannel0Sampler, fragCoord / toastty.iResolution.xy);\n\
                         }\n";

const GLSL_INVERT: &str = "void mainImage(out vec4 fragColor, in vec2 fragCoord) {\n\
                           \x20   vec4 c = texture(iChannel0, fragCoord / iResolution.xy);\n\
                           \x20   fragColor = vec4(vec3(1.0) - c.rgb, c.a);\n\
                           }\n";

const WGSL_SWAP_RB: &str = "fn mainImage(fragCoord: vec2f) -> vec4f {\n\
                            \x20   let c = textureSample(iChannel0, iChannel0Sampler, fragCoord / toastty.iResolution.xy);\n\
                            \x20   return c.bgra;\n\
                            }\n";

fn texture(device: &Device, w: u32, h: u32) -> wgpu::Texture {
    device.create_texture(&wgpu::TextureDescriptor {
        label: Some("post test texture"),
        size: wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC
            | wgpu::TextureUsages::COPY_DST,
        view_formats: &linear_view_formats(FORMAT),
    })
}

fn upload(queue: &Queue, t: &wgpu::Texture, rgba: &[u8]) {
    queue.write_texture(
        t.as_image_copy(),
        rgba,
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some(t.width() * 4),
            rows_per_image: Some(t.height()),
        },
        t.size(),
    );
}

fn read_back(device: &Device, queue: &Queue, t: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (t.width(), t.height());
    let row = (w * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("post test readback"),
        size: u64::from(row * h),
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    encoder.copy_texture_to_buffer(
        t.as_image_copy(),
        wgpu::TexelCopyBufferInfo {
            buffer: &buf,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(row),
                rows_per_image: Some(h),
            },
        },
        t.size(),
    );
    queue.submit(std::iter::once(encoder.finish()));
    buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .expect("poll");
    let data = buf.slice(..).get_mapped_range();
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let start = (y * row) as usize;
        out.extend_from_slice(&data[start..start + (w * 4) as usize]);
    }
    out
}

/// Every byte value in every channel, with alpha varying too.
fn pattern(w: u32, h: u32) -> Vec<u8> {
    let mut v = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            v.extend_from_slice(&[
                (x % 256) as u8,
                (255 - x % 256) as u8,
                ((x * 7 + y * 13) % 256) as u8,
                (255 - (y * 31) % 128) as u8,
            ]);
        }
    }
    v
}

/// Run `shaders` over `input` (`w × h` RGBA) and read the result back.
#[allow(clippy::too_many_arguments)]
fn run(
    device: &Device,
    queue: &Queue,
    shaders: &[PostShaderSource],
    direct: bool,
    w: u32,
    h: u32,
    input: &[u8],
    uniforms: &PostUniforms,
) -> Vec<u8> {
    let compiled = compile_all(shaders).expect("compiles");
    let mut chain = PostChain::new(device, &compiled, FORMAT, direct).expect("pipelines");
    let source = texture(device, w, h);
    let frame = texture(device, w, h);
    upload(queue, &source, input);
    chain.bind_source(device, &source);
    chain.write_uniforms(queue, uniforms, true);
    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor::default());
    chain.encode(&mut encoder, &frame);
    queue.submit(std::iter::once(encoder.finish()));
    read_back(device, queue, &frame)
}

fn uniforms_for(w: u32, h: u32) -> PostUniforms {
    let mut u = PostUniforms::default();
    #[allow(clippy::cast_precision_loss)]
    {
        u.i_resolution = [w as f32, h as f32, 1.0];
    }
    u
}

fn gpu() -> Option<(Device, Queue)> {
    crate::test_gpu::try_device_and_queue()
}

#[test]
fn pass_through_reproduces_the_frame_exactly() {
    let Some((device, queue)) = gpu() else { return };
    let (w, h) = (256, 8);
    let input = pattern(w, h);
    let u = uniforms_for(w, h);
    for direct in [true, false] {
        for s in [shader("p.glsl", GLSL_PASS), shader("p.wgsl", WGSL_PASS)] {
            let out = run(
                &device,
                &queue,
                std::slice::from_ref(&s),
                direct,
                w,
                h,
                &input,
                &u,
            );
            assert!(
                out == input,
                "{} (direct={direct}) changed pixels",
                s.path.display()
            );
        }
    }
}

#[test]
fn pass_through_reproduces_a_terminal_frame() {
    // The hello-world golden is a real cell-pass frame.
    let Some((device, queue)) = gpu() else { return };
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/snapshots/text_hello.png"
    );
    let img = image::open(path).expect("golden").to_rgba8();
    let (w, h) = img.dimensions();
    let input = img.into_raw();
    let u = uniforms_for(w, h);
    let out = run(
        &device,
        &queue,
        &[shader("p.glsl", GLSL_PASS)],
        true,
        w,
        h,
        &input,
        &u,
    );
    assert!(out == input, "pass-through changed the terminal frame");
}

#[test]
fn invert_inverts() {
    let Some((device, queue)) = gpu() else { return };
    let (w, h) = (256, 4);
    let input = pattern(w, h);
    let out = run(
        &device,
        &queue,
        &[shader("inv.glsl", GLSL_INVERT)],
        true,
        w,
        h,
        &input,
        &uniforms_for(w, h),
    );
    for (i, (o, p)) in out.chunks(4).zip(input.chunks(4)).enumerate() {
        assert_eq!(o, [255 - p[0], 255 - p[1], 255 - p[2], p[3]], "pixel {i}");
    }
}

#[test]
fn chain_applies_every_shader_in_order() {
    let Some((device, queue)) = gpu() else { return };
    let (w, h) = (256, 4);
    let input = pattern(w, h);
    let u = uniforms_for(w, h);
    let expected: Vec<u8> = input
        .chunks(4)
        .flat_map(|p| [255 - p[2], 255 - p[1], 255 - p[0], p[3]])
        .collect();
    for direct in [true, false] {
        let out = run(
            &device,
            &queue,
            &[
                shader("inv.glsl", GLSL_INVERT),
                shader("swap.wgsl", WGSL_SWAP_RB),
            ],
            direct,
            w,
            h,
            &input,
            &u,
        );
        assert!(out == expected, "two-pass chain (direct={direct})");
    }
    // Three passes reuse the two ping-pong textures; inverting twice
    // cancels out.
    let out = run(
        &device,
        &queue,
        &[
            shader("a.glsl", GLSL_INVERT),
            shader("b.glsl", GLSL_INVERT),
            shader("c.wgsl", WGSL_SWAP_RB),
        ],
        false,
        w,
        h,
        &input,
        &u,
    );
    let swapped: Vec<u8> = input
        .chunks(4)
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect();
    assert!(out == swapped, "three-pass chain");
}

#[test]
fn uniforms_reach_both_languages() {
    // Reads members from the start, middle and end of the block,
    // including an array element, an int and the palette.
    let Some((device, queue)) = gpu() else { return };
    let (w, h) = (4, 4);
    let mut u = uniforms_for(w, h);
    u.i_time = 0.25;
    u.i_frame = 51;
    u.i_bells[2] = [0.0, 0.0, 0.0, 0.6];
    u.i_palette[200] = [0.0, 0.0, 0.75, 0.0];
    let glsl = "void mainImage(out vec4 c, in vec2 p) {\n\
                \x20   c = vec4(iTime, float(iFrame) / 255.0, iPalette[200].b, iBells[2].w + iResolution.x / 8.0 - 0.5);\n\
                }\n";
    let wgsl = "fn mainImage(p: vec2f) -> vec4f {\n\
                \x20   let u = toastty;\n\
                \x20   return vec4f(u.iTime, f32(u.iFrame) / 255.0, u.iPalette[200].b, u.iBells[2].w + u.iResolution.x / 8.0 - 0.5);\n\
                }\n";
    let input = vec![0u8; (w * h * 4) as usize];
    for s in [shader("u.glsl", glsl), shader("u.wgsl", wgsl)] {
        let out = run(
            &device,
            &queue,
            std::slice::from_ref(&s),
            true,
            w,
            h,
            &input,
            &u,
        );
        assert_eq!(&out[..4], &[64, 51, 191, 153], "{}", s.path.display());
    }
}

#[test]
fn backend_rejection_is_an_error_not_a_panic() {
    // Valid for naga, but wgpu refuses the pipeline: the fragment output
    // is an integer while the target is a float format.
    let Some((device, _)) = gpu() else { return };
    let bad = CompiledShader {
        path: PathBuf::from("bad.wgsl"),
        wgsl: "@fragment fn main() -> @location(0) vec4<u32> { return vec4<u32>(1u); }".to_owned(),
        entry_point: "main".to_owned(),
        inputs: ShaderInputs::default(),
    };
    let errs = PostChain::new(&device, &[bad], FORMAT, true).expect_err("rejected");
    assert_eq!(errs[0].path, PathBuf::from("bad.wgsl"));
    assert!(errs[0].message.contains("rejected"), "{}", errs[0]);
}
