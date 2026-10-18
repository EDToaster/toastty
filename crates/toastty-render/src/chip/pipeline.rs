//! wgpu pipeline for the scroll-to-bottom chip (`shaders/chip.wgsl`).
//!
//! Uniform-only: no vertex buffers, one 6-vertex draw. Depth is
//! attached (the term pass carries a depth buffer) but neither tested
//! nor written, so the chip always lands on top of the frame.

use bytemuck::{Pod, Zeroable};
use std::num::NonZeroU64;

use super::{ChipColors, ChipGeometry};

/// Mirror of the WGSL `Chip` uniform (80 bytes, std140-compatible).
/// Down chevron (scroll-to-bottom chip).
pub const ICON_CHEVRON: f32 = 0.0;
/// Two overlapping squares (rich block "Copy Markdown" chip).
pub const ICON_COPY: f32 = 1.0;

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ChipUniform {
    pub viewport: [f32; 2],
    pub center: [f32; 2],
    pub radius: f32,
    pub scale: f32,
    pub alpha: f32,
    /// Icon selector: [`ICON_CHEVRON`] or [`ICON_COPY`].
    pub icon_kind: f32,
    pub fill: [f32; 4],
    pub border: [f32; 4],
    pub icon: [f32; 4],
}

impl ChipUniform {
    /// Pack geometry, colours and opacity for one draw.
    #[must_use]
    pub fn new(
        viewport: (f32, f32),
        geom: &ChipGeometry,
        scale: f32,
        alpha: f32,
        colors: &ChipColors,
    ) -> Self {
        Self {
            viewport: [viewport.0, viewport.1],
            center: geom.center,
            radius: geom.radius,
            scale,
            alpha: alpha.clamp(0.0, 1.0),
            icon_kind: ICON_CHEVRON,
            fill: colors.fill,
            border: colors.border,
            icon: colors.icon,
        }
    }

    /// Same uniform with a different icon.
    #[must_use]
    pub fn with_icon(mut self, icon_kind: f32) -> Self {
        self.icon_kind = icon_kind;
        self
    }
}

/// The chip pipeline + its uniform buffer and bind group.
#[derive(Debug)]
pub struct ChipPipeline {
    pipeline: wgpu::RenderPipeline,
    uniform_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl ChipPipeline {
    /// Build the pipeline for the swapchain `format`.
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-chip shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/chip.wgsl").into()),
        });
        let size = std::mem::size_of::<ChipUniform>() as u64;
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("toastty-chip bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size),
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-chip pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("toastty-chip pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(false),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-chip uniform"),
            size,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("toastty-chip bind group"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buf.as_entire_binding(),
            }],
        });
        Self {
            pipeline,
            uniform_buf,
            bind_group,
        }
    }

    /// Upload `uniform` and record the chip draw into `rp`.
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        rp: &mut wgpu::RenderPass<'_>,
        uniform: &ChipUniform,
    ) {
        queue.write_buffer(&self.uniform_buf, 0, bytemuck::bytes_of(uniform));
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        rp.draw(0..6, 0..1);
    }
}
