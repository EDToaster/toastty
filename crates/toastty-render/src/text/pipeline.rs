//! wgpu render pipeline for the text/cell pass.
//!
//! Consumes a `[CellInstance]` slice (see [`super::instance`]) and a
//! pair of atlas textures (mask + color), draws each instance as a
//! triangle-strip quad with vertex pulling.

use bytemuck::{Pod, Zeroable};
use wgpu::{
    BindGroup, BindGroupDescriptor, BindGroupEntry, BindGroupLayout, BindGroupLayoutDescriptor,
    BindGroupLayoutEntry, BindingResource, BindingType, BlendComponent, BlendFactor,
    BlendOperation, BlendState, Buffer, BufferAddress, BufferBindingType, BufferDescriptor,
    BufferUsages, ColorTargetState, ColorWrites, Device, FragmentState, FrontFace,
    MultisampleState, PipelineCompilationOptions, PipelineLayoutDescriptor, PrimitiveState,
    PrimitiveTopology, Queue, RenderPipeline, RenderPipelineDescriptor, SamplerBindingType,
    SamplerDescriptor, ShaderModule, ShaderModuleDescriptor, ShaderSource, ShaderStages,
    TextureFormat, TextureSampleType, TextureView, TextureViewDescriptor, TextureViewDimension,
    VertexAttribute, VertexBufferLayout, VertexFormat, VertexState, VertexStepMode,
};

use crate::text::instance::CellInstance;

/// Globals UBO shared with shaders/text.wgsl.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct GlobalsUbo {
    /// `[viewport_width_px, viewport_height_px, atlas_width_px, atlas_height_px]`.
    pub viewport_and_atlas: [f32; 4],
}

/// The text/cell pipeline plus its bind-group skeleton.
pub struct TextPipeline {
    pipeline: RenderPipeline,
    bind_group_layout: BindGroupLayout,
    instance_buffer: Buffer,
    instance_capacity: usize,
    globals_buffer: Buffer,
}

impl std::fmt::Debug for TextPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TextPipeline")
            .field("instance_capacity", &self.instance_capacity)
            .finish_non_exhaustive()
    }
}

impl TextPipeline {
    /// Build the pipeline targeted at `color_format`.
    pub fn new(device: &Device, color_format: TextureFormat) -> Self {
        let shader = device.create_shader_module(ShaderModuleDescriptor {
            label: Some("toastty-text shader"),
            source: ShaderSource::Wgsl(std::borrow::Cow::Borrowed(include_str!(
                "../../shaders/text.wgsl"
            ))),
        });

        let bind_group_layout = device.create_bind_group_layout(&BindGroupLayoutDescriptor {
            label: Some("toastty-text bgl"),
            entries: &[
                BindGroupLayoutEntry {
                    binding: 0,
                    visibility: ShaderStages::VERTEX | ShaderStages::FRAGMENT,
                    ty: BindingType::Buffer {
                        ty: BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 1,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 2,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Texture {
                        sample_type: TextureSampleType::Float { filterable: true },
                        view_dimension: TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                BindGroupLayoutEntry {
                    binding: 3,
                    visibility: ShaderStages::FRAGMENT,
                    ty: BindingType::Sampler(SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let pipeline_layout = device.create_pipeline_layout(&PipelineLayoutDescriptor {
            label: Some("toastty-text pl"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });

        let pipeline = make_pipeline(device, &shader, &pipeline_layout, color_format);

        // Reasonable starting capacity. Grows on demand.
        let instance_capacity = 1024;
        let instance_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("toastty-text instances"),
            size: (instance_capacity * std::mem::size_of::<CellInstance>()) as u64,
            usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let globals_buffer = device.create_buffer(&BufferDescriptor {
            label: Some("toastty-text globals"),
            size: std::mem::size_of::<GlobalsUbo>() as u64,
            usage: BufferUsages::UNIFORM | BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            bind_group_layout,
            instance_buffer,
            instance_capacity,
            globals_buffer,
        }
    }

    /// Build a bind group binding the supplied atlas textures.
    pub fn make_bind_group(
        &self,
        device: &Device,
        mask_view: &TextureView,
        color_view: &TextureView,
    ) -> BindGroup {
        let sampler = device.create_sampler(&SamplerDescriptor {
            label: Some("toastty-text atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
            ..Default::default()
        });

        device.create_bind_group(&BindGroupDescriptor {
            label: Some("toastty-text bg"),
            layout: &self.bind_group_layout,
            entries: &[
                BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buffer.as_entire_binding(),
                },
                BindGroupEntry {
                    binding: 1,
                    resource: BindingResource::TextureView(mask_view),
                },
                BindGroupEntry {
                    binding: 2,
                    resource: BindingResource::TextureView(color_view),
                },
                BindGroupEntry {
                    binding: 3,
                    resource: BindingResource::Sampler(&sampler),
                },
            ],
        })
    }

    /// Upload globals + instances and record the draw call into `pass`.
    pub fn render<'pass>(
        &'pass mut self,
        device: &Device,
        queue: &Queue,
        pass: &mut wgpu::RenderPass<'pass>,
        bind_group: &'pass BindGroup,
        globals: GlobalsUbo,
        instances: &[CellInstance],
    ) {
        queue.write_buffer(&self.globals_buffer, 0, bytemuck::bytes_of(&globals));

        if instances.is_empty() {
            return;
        }

        // Grow buffer if needed.
        if instances.len() > self.instance_capacity {
            let new_cap = instances.len().next_power_of_two();
            self.instance_buffer = device.create_buffer(&BufferDescriptor {
                label: Some("toastty-text instances (grown)"),
                size: (new_cap * std::mem::size_of::<CellInstance>()) as u64,
                usage: BufferUsages::VERTEX | BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_capacity = new_cap;
        }

        queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(instances));

        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, bind_group, &[]);
        pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
        // Each instance is a 4-vertex triangle strip.
        pass.draw(0..4, 0..instances.len() as u32);
    }
}

fn make_pipeline(
    device: &Device,
    shader: &ShaderModule,
    pipeline_layout: &wgpu::PipelineLayout,
    color_format: TextureFormat,
) -> RenderPipeline {
    let vertex_layout = instance_buffer_layout();

    device.create_render_pipeline(&RenderPipelineDescriptor {
        label: Some("toastty-text pipeline"),
        layout: Some(pipeline_layout),
        vertex: VertexState {
            module: shader,
            entry_point: Some("vs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            buffers: &[vertex_layout],
        },
        primitive: PrimitiveState {
            topology: PrimitiveTopology::TriangleStrip,
            strip_index_format: None,
            front_face: FrontFace::Ccw,
            cull_mode: None,
            polygon_mode: wgpu::PolygonMode::Fill,
            unclipped_depth: false,
            conservative: false,
        },
        // Cell layer writes z=0.5 (from the shader); RGP pass tests
        // against this so 3D objects can occlude or sit underneath
        // text by their per-placement `depth` field.
        depth_stencil: Some(wgpu::DepthStencilState {
            format: wgpu::TextureFormat::Depth32Float,
            depth_write_enabled: Some(true),
            depth_compare: Some(wgpu::CompareFunction::LessEqual),
            stencil: wgpu::StencilState::default(),
            bias: wgpu::DepthBiasState::default(),
        }),
        multisample: MultisampleState {
            count: 1,
            mask: !0,
            alpha_to_coverage_enabled: false,
        },
        fragment: Some(FragmentState {
            module: shader,
            entry_point: Some("fs_main"),
            compilation_options: PipelineCompilationOptions::default(),
            targets: &[Some(ColorTargetState {
                format: color_format,
                blend: Some(BlendState {
                    color: BlendComponent {
                        src_factor: BlendFactor::SrcAlpha,
                        dst_factor: BlendFactor::OneMinusSrcAlpha,
                        operation: BlendOperation::Add,
                    },
                    alpha: BlendComponent {
                        src_factor: BlendFactor::One,
                        dst_factor: BlendFactor::OneMinusSrcAlpha,
                        operation: BlendOperation::Add,
                    },
                }),
                write_mask: ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

/// Vertex attribute layout for [`CellInstance`].
///
/// All seven attributes share `step_mode = Instance` — there is no
/// per-vertex data; the vertex shader pulls the four quad corners from
/// `vertex_index`.
fn instance_buffer_layout() -> VertexBufferLayout<'static> {
    // We can't use vertex_attr_array! cleanly because of the `u32` flags
    // attribute coming after vec4s; spell it out so the offsets stay
    // explicit.
    VertexBufferLayout {
        array_stride: std::mem::size_of::<CellInstance>() as BufferAddress,
        step_mode: VertexStepMode::Instance,
        attributes: &INSTANCE_ATTRIBUTES,
    }
}

const INSTANCE_ATTRIBUTES: [VertexAttribute; 7] = [
    // pos
    VertexAttribute {
        offset: 0,
        shader_location: 0,
        format: VertexFormat::Float32x2,
    },
    // size
    VertexAttribute {
        offset: 8,
        shader_location: 1,
        format: VertexFormat::Float32x2,
    },
    // uv_min
    VertexAttribute {
        offset: 16,
        shader_location: 2,
        format: VertexFormat::Float32x2,
    },
    // uv_max
    VertexAttribute {
        offset: 24,
        shader_location: 3,
        format: VertexFormat::Float32x2,
    },
    // fg
    VertexAttribute {
        offset: 32,
        shader_location: 4,
        format: VertexFormat::Float32x4,
    },
    // bg
    VertexAttribute {
        offset: 48,
        shader_location: 5,
        format: VertexFormat::Float32x4,
    },
    // flags
    VertexAttribute {
        offset: 64,
        shader_location: 6,
        format: VertexFormat::Uint32,
    },
];

/// Helper: build a `TextureView` for the given texture with default
/// settings.
pub fn default_view(t: &wgpu::Texture) -> TextureView {
    t.create_view(&TextureViewDescriptor::default())
}
