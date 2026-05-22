//! Headless snapshot tests.
//!
//! Each test creates a hidden wgpu device with validation enabled (per
//! decision §8), renders a canonical scenario into an offscreen texture,
//! reads the pixels back via `Buffer::map_async`, and asserts against a
//! known value. No window, no surface — works in CI.
//!
//! Tests are NOT `#[ignore]`d any more; the M4a clear-color path lets us
//! actually validate the headless harness up front so M4b's text rendering
//! has a foundation to build on.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use pollster::block_on;
use toastty_render::{color, instance_descriptor, instance_flags_for_tests};
use wgpu::{
    Color, CommandEncoderDescriptor, DeviceDescriptor, Extent3d, LoadOp, Operations,
    PowerPreference, RenderPassColorAttachment, RenderPassDescriptor, RequestAdapterOptions,
    StoreOp, TexelCopyBufferInfo, TexelCopyBufferLayout, TextureDescriptor, TextureDimension,
    TextureFormat, TextureUsages, TextureViewDescriptor,
};

const SIZE: u32 = 32;
const BYTES_PER_PIXEL: u32 = 4;

/// Helper: pick a clear color and assert middle pixel after readback.
#[allow(clippy::too_many_lines)]
fn render_and_readback(linear_rgba: [f32; 4]) -> [u8; 4] {
    let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));

    let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
        power_preference: PowerPreference::LowPower,
        force_fallback_adapter: false,
        compatible_surface: None,
    }))
    .expect("no adapter for headless test — does this machine have a GPU?");

    let (device, queue) = block_on(adapter.request_device(&DeviceDescriptor {
        label: Some("snapshot-test device"),
        required_features: wgpu::Features::empty(),
        required_limits: wgpu::Limits::downlevel_defaults(),
        memory_hints: wgpu::MemoryHints::Performance,
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        trace: wgpu::Trace::Off,
    }))
    .expect("device request failed");

    // wgpu 29 papercut: error scope is a guard, but for snapshot tests
    // we keep things simple — uncaptured errors panic via the validation
    // layer.
    let format = TextureFormat::Bgra8UnormSrgb;

    let texture = device.create_texture(&TextureDescriptor {
        label: Some("snapshot target"),
        size: Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: TextureDimension::D2,
        format,
        usage: TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&TextureViewDescriptor::default());

    // Readback buffer: bytes-per-row must be 256-aligned per wgpu spec.
    // For 32×32 BGRA8, raw row = 128 bytes, padded row = 256.
    let row_bytes = SIZE * BYTES_PER_PIXEL;
    let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
    let padded_row_bytes = row_bytes.div_ceil(align) * align;
    let buffer_size = u64::from(padded_row_bytes) * u64::from(SIZE);

    let readback = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("snapshot readback"),
        size: buffer_size,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });

    let mut encoder = device.create_command_encoder(&CommandEncoderDescriptor {
        label: Some("snapshot encoder"),
    });

    {
        let _rp = encoder.begin_render_pass(&RenderPassDescriptor {
            label: Some("snapshot clear"),
            color_attachments: &[Some(RenderPassColorAttachment {
                view: &view,
                depth_slice: None,
                resolve_target: None,
                ops: Operations {
                    load: LoadOp::Clear(Color {
                        r: f64::from(linear_rgba[0]),
                        g: f64::from(linear_rgba[1]),
                        b: f64::from(linear_rgba[2]),
                        a: f64::from(linear_rgba[3]),
                    }),
                    store: StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None, // wgpu 29 papercut: required even when unused.
        });
    }

    encoder.copy_texture_to_buffer(
        texture.as_image_copy(),
        TexelCopyBufferInfo {
            buffer: &readback,
            layout: TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes),
                rows_per_image: Some(SIZE),
            },
        },
        Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
    );

    queue.submit(std::iter::once(encoder.finish()));

    // Map and read back. Use the standard map_async + poll pattern.
    let slice = readback.slice(..);
    let mapped = Arc::new(AtomicBool::new(false));
    let mapped_clone = mapped.clone();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        res.expect("map_async failed");
        mapped_clone.store(true, Ordering::SeqCst);
    });
    // Block until the callback fires.
    while !mapped.load(Ordering::SeqCst) {
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
    }

    let data = slice.get_mapped_range();

    // Sample the middle pixel. Bytes per row may be padded; index by
    // padded_row_bytes, not by row_bytes.
    let mid_y = SIZE / 2;
    let mid_x = SIZE / 2;
    let offset = (mid_y * padded_row_bytes + mid_x * BYTES_PER_PIXEL) as usize;
    let px: [u8; 4] = [
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ];

    drop(data);
    readback.unmap();
    px
}

fn close(a: u8, b: u8, tol: i32) -> bool {
    (i32::from(a) - i32::from(b)).abs() <= tol
}

#[test]
fn clear_to_red_round_trips_through_srgb() {
    // Linear red — should encode to (B=0, G=0, R=255, A=255) in BGRA8-sRGB.
    let px = render_and_readback([1.0, 0.0, 0.0, 1.0]);
    let expected = color::linear_rgba_to_bgra_srgb_bytes([1.0, 0.0, 0.0, 1.0]);
    println!("got {px:?} expected {expected:?}");
    for i in 0..4 {
        assert!(
            close(px[i], expected[i], 1),
            "channel {i}: got {} expected {}",
            px[i],
            expected[i]
        );
    }
}

#[test]
fn clear_to_mid_gray_uses_srgb_curve() {
    // Linear 0.5 → sRGB ≈ 188 per the gamma curve.
    let linear = [0.5, 0.5, 0.5, 1.0];
    let px = render_and_readback(linear);
    let expected = color::linear_rgba_to_bgra_srgb_bytes(linear);
    println!("got {px:?} expected {expected:?}");
    // 188 ± 1 for each color channel; alpha = 255.
    for i in 0..3 {
        assert!(
            close(px[i], expected[i], 1),
            "channel {i}: got {} expected {} (linear 0.5 should be sRGB ~188)",
            px[i],
            expected[i]
        );
    }
    assert_eq!(px[3], 255);
}

#[test]
fn clear_to_black_is_zero_in_all_channels_except_alpha() {
    let px = render_and_readback([0.0, 0.0, 0.0, 1.0]);
    assert_eq!(px[0], 0);
    assert_eq!(px[1], 0);
    assert_eq!(px[2], 0);
    assert_eq!(px[3], 255);
}
