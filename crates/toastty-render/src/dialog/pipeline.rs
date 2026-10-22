//! wgpu pipeline for UI shapes (`shaders/ui.wgsl`): instanced SDF
//! rounded rects with an optional inner ring, soft shadows and the close
//! dialog's prompt icon.
//!
//! The shapes are drawn last in the term pass, over everything, so they
//! test no depth. They do *write* depth (at the far end of the range):
//! the dialog's text is drawn by the glyph pipeline after them, at the
//! cell layer's z = 0.5 with a `LessEqual` test, and an RGP object in
//! front of the cell layer would otherwise hide that text through the
//! panel. Output is premultiplied alpha.

use bytemuck::{Pod, Zeroable};

/// [`UiRect::kind`]: a rounded rect, filled, with an inner ring.
pub const UI_KIND_RECT: u32 = 0;
/// [`UiRect::kind`]: a blurred rounded rect (`softness` = blur reach).
pub const UI_KIND_SHADOW: u32 = 1;
/// [`UiRect::kind`]: the `›_` terminal-prompt icon centred in `rect`,
/// stroked `border_width` wide in `fill`.
pub const UI_KIND_PROMPT_ICON: u32 = 2;
/// [`UiRect::kind`]: the return-key arrow (`↵`) fitted to `rect`,
/// stroked `border_width` wide in `fill`.
pub const UI_KIND_RETURN_ICON: u32 = 3;

/// One UI shape instance (64 bytes). Positions are physical framebuffer
/// px (absolute, no content origin added); colours are straight-alpha
/// linear light.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Pod, Zeroable)]
pub struct UiRect {
    /// `[x0, y0, x1, y1]`.
    pub rect: [f32; 4],
    pub fill: [f32; 4],
    pub border: [f32; 4],
    /// Corner radius (clamped to half the shorter side in the shader).
    pub radius: f32,
    /// Inner ring width (0 = none); stroke width for the icon.
    pub border_width: f32,
    /// Shadow blur reach either side of the edge.
    pub softness: f32,
    /// One of the `UI_KIND_*` constants.
    pub kind: u32,
}

impl UiRect {
    /// A plain [`UI_KIND_RECT`].
    #[must_use]
    pub fn rect(
        rect: [f32; 4],
        fill: [f32; 4],
        border: [f32; 4],
        radius: f32,
        border_width: f32,
    ) -> Self {
        Self {
            rect,
            fill,
            border,
            radius,
            border_width,
            softness: 0.0,
            kind: UI_KIND_RECT,
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct UiGlobals {
    viewport: [f32; 2],
    _pad: [f32; 2],
}

const ATTRS: [wgpu::VertexAttribute; 7] = wgpu::vertex_attr_array![
    0 => Float32x4,
    1 => Float32x4,
    2 => Float32x4,
    3 => Float32,
    4 => Float32,
    5 => Float32,
    6 => Uint32,
];

/// The UI-shape pipeline + its instance buffer and uniform.
#[derive(Debug)]
pub struct UiPipeline {
    pipeline: wgpu::RenderPipeline,
    globals_buf: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
    instance_buf: wgpu::Buffer,
    capacity: usize,
}

impl UiPipeline {
    /// Build the pipeline for the colour target `format`.
    #[must_use]
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-ui shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/ui.wgsl").into()),
        });
        let bind_layout =
            device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("toastty-ui bind group layout"),
                entries: &[wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: wgpu::BufferSize::new(
                            std::mem::size_of::<UiGlobals>() as u64
                        ),
                    },
                    count: None,
                }],
            });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-ui pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("toastty-ui pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[wgpu::VertexBufferLayout {
                    array_stride: std::mem::size_of::<UiRect>() as u64,
                    step_mode: wgpu::VertexStepMode::Instance,
                    attributes: &ATTRS,
                }],
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::PREMULTIPLIED_ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleStrip,
                ..wgpu::PrimitiveState::default()
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::Always),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });
        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-ui globals"),
            size: std::mem::size_of::<UiGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("toastty-ui bind group"),
            layout: &bind_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: globals_buf.as_entire_binding(),
            }],
        });
        let capacity = 16;
        let instance_buf = Self::instance_buffer(device, capacity);
        Self {
            pipeline,
            globals_buf,
            bind_group,
            instance_buf,
            capacity,
        }
    }

    fn instance_buffer(device: &wgpu::Device, capacity: usize) -> wgpu::Buffer {
        device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-ui instances"),
            size: (capacity * std::mem::size_of::<UiRect>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        })
    }

    /// Upload `rects` and record their draw into `rp`, in order.
    #[allow(clippy::cast_possible_truncation)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rp: &mut wgpu::RenderPass<'_>,
        rects: &[UiRect],
        viewport: (f32, f32),
    ) {
        if rects.is_empty() {
            return;
        }
        if rects.len() > self.capacity {
            self.capacity = rects.len().next_power_of_two();
            self.instance_buf = Self::instance_buffer(device, self.capacity);
        }
        let globals = UiGlobals {
            viewport: [viewport.0, viewport.1],
            _pad: [0.0; 2],
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));
        queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(rects));
        rp.set_pipeline(&self.pipeline);
        rp.set_bind_group(0, &self.bind_group, &[]);
        rp.set_vertex_buffer(0, self.instance_buf.slice(..));
        rp.draw(0..4, 0..rects.len() as u32);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn instance_layout_matches_the_shader() {
        assert_eq!(std::mem::size_of::<UiRect>(), 64);
        assert_eq!(std::mem::offset_of!(UiRect, radius), 48);
        assert_eq!(std::mem::offset_of!(UiRect, kind), 60);
    }
}
