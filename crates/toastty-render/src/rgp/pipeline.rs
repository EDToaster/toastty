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
    Mat4, Z_HALF_RANGE_PX, identity, mul, ortho_screen, rotate_x_deg, rotate_y_deg, rotate_z_deg,
    scale, translate,
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
    /// `viewport` is the physical pixel size of the render target (full
    /// surface — the px->NDC divisor stays full-surface).
    /// `cell_size` is the cell pixel size (width, height) used to
    /// map cell-space anchor coordinates to pixel-space.
    /// `content_origin` is `(pad_left, pad_top)` in physical px — the
    /// window-padding inset, folded into the per-placement anchor below.
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        rp: &mut wgpu::RenderPass<'_>,
        scene: &RgpScene,
        cache: &GpuAssetCache,
        viewport: (f32, f32),
        cell_size: (f32, f32),
        content_origin: (f32, f32),
    ) {
        if scene.placements().next().is_none() {
            return;
        }
        // Collect placements ordered by id for determinism.
        let mut placements: Vec<(u32, RgpPlacement)> =
            scene.placements().map(|(id, p)| (id, *p)).collect();
        placements.sort_by_key(|(id, _)| *id);

        // Ensure we have enough uniform slots.
        self.ensure_slots(device, placements.len());

        let proj = ortho_screen(viewport.0, viewport.1);

        rp.set_pipeline(&self.pipeline);

        for (slot_idx, (id, p)) in placements.iter().enumerate() {
            let Some(mesh) = cache.meshes.get(id) else {
                continue;
            };

            // Cell-space anchor → world-space centre. Per the RGP
            // spec (protocols/graphics.md §3), `row`/`col` is the
            // *center* cell of the placement, not its top-left.
            //
            // X is straightforward (cells go left→right). Y is
            // inverted because the ortho projection treats world as
            // Y-up (matching the OBJ/glTF model-space convention
            // used by ratty), while terminal rows count top→bottom.
            //
            // content_origin folds in the window-padding inset. X is a
            // straight +pad_left. Y subtracts pad_top: a screen-down
            // inset is a world-Y-DOWN shift because RGP world is Y-up
            // (ortho_screen maps worldY → 2*worldY/h - 1 over the
            // full-surface h, which is left unchanged).
            let center_px_x = (f32::from(p.anchor.col) + 0.5) * cell_size.0 + content_origin.0;
            let center_px_y =
                viewport.1 - (f32::from(p.anchor.row) + 0.5) * cell_size.1 - content_origin.1;
            // Pick the half-extent so the unit-cube model fits the
            // placement cell box. Use the smaller dimension so the
            // model isn't stretched in non-square placements.
            let half_w_px = f32::from(p.anchor.cols) * cell_size.0 * 0.5;
            let half_h_px = f32::from(p.anchor.rows) * cell_size.1 * 0.5;
            let fit_half = half_w_px.min(half_h_px) * p.style.scale;

            // Model: scale × non-uniform scale × rotate × translate.
            //
            // All three model axes use the SAME pixel scale. That's
            // critical: rotations mix axes, and asymmetric scales
            // (e.g. pixel x/y but abstract z) collapse the cube to a
            // line at 90° rotations. The ortho projection then maps
            // pixel z to NDC via `Z_HALF_RANGE_PX`.
            let s_uniform = scale(fit_half, fit_half, fit_half);
            let s_nonuniform = scale(p.style.scale3[0], p.style.scale3[1], p.style.scale3[2]);
            let rx = rotate_x_deg(p.style.rotation[0]);
            let ry = rotate_y_deg(p.style.rotation[1]);
            let rz = rotate_z_deg(p.style.rotation[2]);
            // M12c: `animate=1` tumbles the object around all three
            // world axes simultaneously, composed AFTER the protocol's
            // pose so the app's chosen `rx/ry/rz` selects a starting
            // orientation and the animation spins it from there.
            // Per-axis multipliers are irrational w.r.t. each other so
            // the object doesn't quickly realign to its starting pose.
            // `Term::tick_rgp_animations` updates the phase each frame.
            let phase_deg = p.animation_phase_rad.to_degrees();
            let r_animate = compose(&[
                rotate_z_deg(phase_deg * 1.3),
                rotate_y_deg(phase_deg),
                rotate_x_deg(phase_deg * 0.7),
            ]);
            // Protocol `depth` maps to pixel-z via the decision §3
            // factor: `ndc_z = 0.5 + 0.05 * depth` and the ortho
            // scales pixel z by `0.5 / Z_HALF_RANGE_PX`, so
            // `world_z_px = 0.1 * Z_HALF_RANGE_PX * depth`. depth=±10
            // still spans ±10% of the NDC depth volume regardless of
            // the constant's value; the headroom (currently 10×) keeps
            // large rotating placements from clipping the near plane.
            let depth_world_z =
                p.style.depth * Z_HALF_RANGE_PX * 0.1 + p.style.offset[2] * fit_half;
            let t = translate(
                center_px_x + p.style.offset[0] * fit_half,
                center_px_y + p.style.offset[1] * fit_half,
                depth_world_z,
            );
            // Order applied to v: scale → proto rotate → animate
            // rotate (world Y) → translate.
            let model = compose(&[t, r_animate, rz, ry, rx, s_uniform, s_nonuniform]);
            let mvp = mul(&proj, &model);

            // Normal matrix mirrors the rotational part of the
            // model (uniform scales don't disturb normals). Include
            // the animation phase so lighting tracks the rotating
            // faces.
            let normal = compose(&[r_animate, rz, ry, rx]);

            // Color tint: base_color × protocol color × brightness.
            let prot_color = p.style.color.unwrap_or([255, 255, 255]);
            let r = mesh.base_color[0] * (f32::from(prot_color[0]) / 255.0) * p.style.brightness;
            let g = mesh.base_color[1] * (f32::from(prot_color[1]) / 255.0) * p.style.brightness;
            let b = mesh.base_color[2] * (f32::from(prot_color[2]) / 255.0) * p.style.brightness;
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
