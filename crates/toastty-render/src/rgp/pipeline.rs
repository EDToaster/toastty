//! RGP 3D pipeline.
//!
//! One draw call per placement. Per-draw uniform carries the MVP
//! matrix, the normal matrix (mat3 in a mat4 slot for std140
//! alignment), and the modulated color tint. Lighting + sun
//! direction are hardcoded in the shader (see `rgp.wgsl`).
//!
//! No texturing in v1. Solid base color × protocol `color` tint ×
//! `brightness` is the only shading channel.

use bytemuck::{Pod, Zeroable};
use toastty_graphics::rgp::scene::{RgpPlacement, RgpScene};

use crate::rgp::matrix::{
    Mat4, identity, mul, ortho_screen, rotate_x_deg, rotate_y_deg, rotate_z_deg, scale, translate,
};
use crate::rgp::mesh::{GpuAssetCache, Vertex};

/// Per-draw uniform — must mirror `DrawUniforms` in `rgp.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
struct DrawUniforms {
    mvp: Mat4,
    normal: Mat4,
    color_tint: [f32; 4],
}

/// RGP 3D pipeline + a pool of per-draw uniform buffers (re-used
/// across frames; grown on demand).
pub struct Rgp3dPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    /// Per-draw uniform buffers, indexed by draw order. Each frame
    /// we may write to many of them; we recycle the vec across
    /// frames to avoid reallocation.
    draw_slots: Vec<DrawSlot>,
}

struct DrawSlot {
    buffer: wgpu::Buffer,
    bind_group: wgpu::BindGroup,
}

impl std::fmt::Debug for Rgp3dPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Rgp3dPipeline")
            .field("draw_slots", &self.draw_slots.len())
            .finish_non_exhaustive()
    }
}

impl Rgp3dPipeline {
    pub fn new(device: &wgpu::Device, color_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-rgp shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/rgp.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("toastty-rgp bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-rgp pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("toastty-rgp pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[Vertex::layout()],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                strip_index_format: None,
                front_face: wgpu::FrontFace::Ccw,
                // No back-face culling: v1's procedural cube + glTF
                // loader don't guarantee a winding convention, and
                // back-facing fragments contribute (lambertian → 0,
                // ambient only) without harming the look.
                cull_mode: None,
                unclipped_depth: false,
                polygon_mode: wgpu::PolygonMode::Fill,
                conservative: false,
            },
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format: color_format,
                    blend: Some(wgpu::BlendState {
                        color: wgpu::BlendComponent {
                            src_factor: wgpu::BlendFactor::SrcAlpha,
                            dst_factor: wgpu::BlendFactor::OneMinusSrcAlpha,
                            operation: wgpu::BlendOperation::Add,
                        },
                        alpha: wgpu::BlendComponent::OVER,
                    }),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_layout,
            draw_slots: Vec::new(),
        }
    }

    /// Ensure at least `n` draw slots exist; allocate any new ones.
    fn ensure_slots(&mut self, device: &wgpu::Device, n: usize) {
        while self.draw_slots.len() < n {
            let buffer = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("toastty-rgp draw uniform"),
                size: std::mem::size_of::<DrawUniforms>() as u64,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("toastty-rgp draw bind group"),
                layout: &self.bind_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: buffer.as_entire_binding(),
                }],
            });
            self.draw_slots.push(DrawSlot { buffer, bind_group });
        }
    }

    /// Render every placement in `scene` against the matching GPU
    /// mesh from `cache`. Caller must have already called
    /// `cache.sync(...)` for this frame.
    ///
    /// `viewport` is the physical pixel size of the render target.
    /// `cell_size` is the cell pixel size (width, height) used to
    /// map cell-space anchor coordinates to pixel-space.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rp: &mut wgpu::RenderPass<'_>,
        scene: &RgpScene,
        cache: &GpuAssetCache,
        viewport: (f32, f32),
        cell_size: (f32, f32),
    ) {
        if scene.placements().next().is_none() {
            return;
        }
        // Collect placements ordered by id for determinism.
        let mut placements: Vec<(u32, RgpPlacement)> = scene
            .placements()
            .map(|(id, p)| (id, *p))
            .collect();
        placements.sort_by_key(|(id, _)| *id);

        // Ensure we have enough uniform slots.
        self.ensure_slots(device, placements.len());

        let proj = ortho_screen(viewport.0, viewport.1);

        rp.set_pipeline(&self.pipeline);

        for (slot_idx, (id, p)) in placements.iter().enumerate() {
            let Some(mesh) = cache.meshes.get(id) else {
                continue;
            };

            // Cell-space anchor → pixel-space centre.
            let center_px_x = (f32::from(p.anchor.col) + f32::from(p.anchor.cols) * 0.5)
                * cell_size.0;
            let center_px_y = (f32::from(p.anchor.row) + f32::from(p.anchor.rows) * 0.5)
                * cell_size.1;
            // Pick the half-extent so the unit-cube model fits the
            // placement cell box. Use the smaller dimension so the
            // model isn't stretched in non-square placements.
            let half_w_px = f32::from(p.anchor.cols) * cell_size.0 * 0.5;
            let half_h_px = f32::from(p.anchor.rows) * cell_size.1 * 0.5;
            let fit_half = half_w_px.min(half_h_px) * p.style.scale;

            // Model: scale × non-uniform scale × rotate × translate.
            //
            // X / Y scale is pixel-space (so the cube fits the cell
            // box). Z scale is **abstract**, chosen so the cube's
            // local z extent (±0.5 from `CpuMesh::unit_cube`) maps
            // to ±0.25 in world z — well inside the ortho's [-1, 1]
            // world-z window (which becomes NDC [0, 1] per
            // decision §3). Mixing pixel-x/y with abstract-z is the
            // whole point of the orthographic projection's split
            // unit convention.
            const Z_MODEL_SCALE: f32 = 0.5;
            let s_uniform = scale(fit_half, fit_half, Z_MODEL_SCALE);
            let s_nonuniform = scale(p.style.scale3[0], p.style.scale3[1], p.style.scale3[2]);
            let rx = rotate_x_deg(p.style.rotation[0]);
            let ry = rotate_y_deg(p.style.rotation[1]);
            let rz = rotate_z_deg(p.style.rotation[2]);
            // Protocol `depth` maps to abstract world-z via the
            // decision §3 factor (`ndc_z = 0.5 + 0.05 * depth`,
            // i.e. `world_z = 0.1 * depth` since the ortho scales
            // world-z by 0.5). The placement's `pz` offset uses
            // the same abstract scale.
            const PROTOCOL_DEPTH_TO_WORLD_Z: f32 = 0.1;
            let depth_world_z =
                p.style.depth * PROTOCOL_DEPTH_TO_WORLD_Z + p.style.offset[2] * Z_MODEL_SCALE;
            let t = translate(
                center_px_x + p.style.offset[0] * fit_half,
                center_px_y + p.style.offset[1] * fit_half,
                depth_world_z,
            );
            let model = compose(&[t, rz, ry, rx, s_uniform, s_nonuniform]);
            let mvp = mul(&proj, &model);

            // Normal matrix = upper-3x3 of model, transposed and
            // inverted. For pure-rotation × uniform-scale models
            // (the common case) this is just the rotation part —
            // shortcut: compose rotations only.
            let normal = compose(&[rx, ry, rz]);

            // Color tint: base_color × protocol color × brightness.
            let prot_color = p.style.color.unwrap_or([255, 255, 255]);
            let r = mesh.base_color[0]
                * (f32::from(prot_color[0]) / 255.0)
                * p.style.brightness;
            let g = mesh.base_color[1]
                * (f32::from(prot_color[1]) / 255.0)
                * p.style.brightness;
            let b = mesh.base_color[2]
                * (f32::from(prot_color[2]) / 255.0)
                * p.style.brightness;
            let a = mesh.base_color[3];
            let uniforms = DrawUniforms {
                mvp,
                normal,
                color_tint: [r, g, b, a],
            };

            let slot = &self.draw_slots[slot_idx];
            queue.write_buffer(&slot.buffer, 0, bytemuck::bytes_of(&uniforms));
            rp.set_bind_group(0, &slot.bind_group, &[]);
            rp.set_vertex_buffer(0, mesh.vertex_buffer.slice(..));
            rp.set_index_buffer(mesh.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
            rp.draw_indexed(0..mesh.index_count, 0, 0..1);
        }
    }
}

/// Right-to-left mat4 composition: `a[0] * a[1] * ... * a[n-1]`.
fn compose(ms: &[Mat4]) -> Mat4 {
    let mut out = identity();
    for m in ms {
        out = mul(&out, m);
    }
    out
}

