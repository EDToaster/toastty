//! Headless snapshot tests.
//!
//! Each test creates a hidden wgpu device with validation enabled,
//! renders a canonical scenario into an offscreen texture, reads the
//! pixels back, and compares them against a golden PNG in
//! `tests/snapshots/` using `image-compare`'s SSIM metric. Drift above
//! the threshold fails the test and writes `*.actual.png` alongside
//! the golden so a human (or agent) can inspect the diff.
//!
//! Tests are `#[ignore]` until rendering exists; flip them on as
//! pipelines come online.

#![allow(dead_code)]

const SSIM_THRESHOLD: f64 = 0.995;

fn require_validation_instance() -> wgpu::Instance {
    wgpu::Instance::new(wgpu::InstanceDescriptor {
        backends: wgpu::Backends::PRIMARY,
        flags: wgpu::InstanceFlags::VALIDATION | wgpu::InstanceFlags::DEBUG,
        backend_options: wgpu::BackendOptions::default(),
        memory_budget_thresholds: wgpu::MemoryBudgetThresholds::default(),
        display: None,
    })
}

#[test]
#[ignore = "harness only — no pipelines to snapshot yet"]
fn smoke_headless_instance() {
    let _ = require_validation_instance();
}
