//! One wgpu device shared by every GPU-backed unit test in this crate.
//!
//! The test harness runs tests on parallel threads. Creating many
//! `wgpu::Instance`s and devices concurrently intermittently segfaults
//! inside Mesa's software Vulkan driver (lavapipe), which is what
//! headless CI machines use. A single lazily-created device (wgpu
//! `Device` / `Queue` are cheap `Clone` handles and `Send + Sync`)
//! avoids the concurrent driver setup, and is faster.

use std::sync::OnceLock;

use pollster::block_on;
use wgpu::{Device, DeviceDescriptor, PowerPreference, Queue, RequestAdapterOptions};

use crate::{instance_descriptor, instance_flags_for_tests};

static GPU: OnceLock<Option<(Device, Queue)>> = OnceLock::new();

/// The shared test device, or `None` when no adapter is available.
pub(crate) fn try_device_and_queue() -> Option<(Device, Queue)> {
    GPU.get_or_init(|| {
        let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));
        let adapter = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        }))
        .ok()?;
        block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("toastty-render shared test device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        }))
        .ok()
    })
    .clone()
    .or_else(|| {
        // Make the skip visible (`--nocapture`): a GPU test that returns
        // early locks nothing.
        eprintln!("skipping GPU test: no wgpu adapter available");
        None
    })
}

/// The shared test device. Panics when no adapter is available.
pub(crate) fn device_and_queue() -> (Device, Queue) {
    try_device_and_queue().expect("no GPU adapter for test")
}

/// Copy `t` (4 bytes per texel) back to the CPU, tightly packed rows.
pub(crate) fn read_texture(device: &Device, queue: &Queue, t: &wgpu::Texture) -> Vec<u8> {
    let (w, h) = (t.width(), t.height());
    let row = (w * 4).div_ceil(256) * 256;
    let buf = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("test readback"),
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
