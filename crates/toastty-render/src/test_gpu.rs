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
