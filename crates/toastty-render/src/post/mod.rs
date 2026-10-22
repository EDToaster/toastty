//! User post-process shaders (M13): Shadertoy / Ghostty-style fragment
//! shaders run over the terminal image.
//!
//! See [`docs/milestones/m13-user-shaders.md`](../../../docs/milestones/m13-user-shaders.md)
//! for the user-facing contract (languages, uniforms, coordinates,
//! animation rules). The pieces:
//!
//! - [`compile`]: prelude wrapping, naga parse + validate, error
//!   mapping back to the user's file, GLSL → WGSL.
//! - [`analysis`]: which uniforms a shader reads, and its
//!   `TOASTTY_EVENT_DURATION`.
//! - [`uniforms`]: the uniform block layout shared by both languages.
//! - [`state`]: the clock, pointer, focus, bell and cursor history behind
//!   the uniforms, and the animation schedule.
//! - [`PostChain`] (here): the GPU passes.
//!
//! Colour handling: the scratch texture and the swapchain are sRGB
//! formats, but every post pass reads and writes them through a non-sRGB
//! view, so shaders see (and write) sRGB-encoded values and a
//! pass-through shader reproduces the frame bit for bit. Where the
//! surface can't be viewed as non-sRGB (no `SURFACE_VIEW_FORMATS`), the
//! last pass writes a texture that is then copied into the swapchain.

mod analysis;
mod compile;
#[cfg(test)]
mod gpu_tests;
mod state;
mod uniforms;

use std::num::NonZeroU64;
use std::task::{Context, Poll, Waker};

use wgpu::TextureFormat;

pub(crate) use compile::ShaderInputs;
pub use compile::{PostShaderSource, ShaderError};
pub use state::PostAnimation;
pub(crate) use state::{CursorSnap, FrameInputs, PostState};
pub(crate) use uniforms::{PostUniforms, date_mask};

use compile::CompiledShader;
use uniforms::PALETTE_OFFSET;

/// Compile every shader. All-or-nothing: any failure returns every
/// error from every shader.
pub(crate) fn compile_all(
    shaders: &[PostShaderSource],
) -> Result<Vec<CompiledShader>, Vec<ShaderError>> {
    let mut ok = Vec::with_capacity(shaders.len());
    let mut errors = Vec::new();
    for s in shaders {
        match compile::compile(s) {
            Ok(c) => ok.push(c),
            Err(e) => errors.extend(e),
        }
    }
    if errors.is_empty() {
        Ok(ok)
    } else {
        Err(errors)
    }
}

/// The format post passes read and write: `format` without sRGB.
pub(crate) fn linear_format(format: TextureFormat) -> TextureFormat {
    format.remove_srgb_suffix()
}

/// `view_formats` for a texture of `format` that post passes access
/// through [`linear_format`].
pub(crate) fn linear_view_formats(format: TextureFormat) -> Vec<TextureFormat> {
    let linear = linear_format(format);
    if linear == format {
        Vec::new()
    } else {
        vec![linear]
    }
}

/// `t` viewed through [`linear_format`].
fn linear_view(t: &wgpu::Texture) -> wgpu::TextureView {
    t.create_view(&wgpu::TextureViewDescriptor {
        label: Some("toastty-post linear view"),
        format: Some(linear_format(t.format())),
        ..Default::default()
    })
}

/// Size-dependent resources: ping-pong textures and one bind group per
/// pass.
#[derive(Debug)]
struct Targets {
    size: (u32, u32),
    textures: Vec<wgpu::Texture>,
    views: Vec<wgpu::TextureView>,
    bind_groups: Vec<wgpu::BindGroup>,
}

/// A compiled, installed chain of post passes.
#[derive(Debug)]
pub(crate) struct PostChain {
    passes: Vec<wgpu::RenderPipeline>,
    inputs: ShaderInputs,
    layout: wgpu::BindGroupLayout,
    uniform_buf: wgpu::Buffer,
    sampler: wgpu::Sampler,
    /// Format of the scratch texture and the swapchain.
    format: TextureFormat,
    /// The last pass renders into the swapchain through a non-sRGB view
    /// (vs. into a ping-pong texture that is then copied over).
    direct: bool,
    targets: Option<Targets>,
}

impl PostChain {
    /// Build the GPU pipelines for `shaders` (already compiled by
    /// [`compile_all`]) rendering for a `format` surface. `direct`: the
    /// swapchain can be viewed as [`linear_format`]. Fails with an error
    /// per shader wgpu rejects.
    pub(crate) fn new(
        device: &wgpu::Device,
        shaders: &[CompiledShader],
        format: TextureFormat,
        direct: bool,
    ) -> Result<Self, Vec<ShaderError>> {
        let linear = linear_format(format);
        let layout = bind_group_layout(device);
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-post pipeline layout"),
            bind_group_layouts: &[Some(&layout)],
            immediate_size: 0,
        });
        let vs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-post vertex shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/post.wgsl").into()),
        });

        let mut passes = Vec::with_capacity(shaders.len());
        let mut errors = Vec::new();
        let mut inputs = ShaderInputs::default();
        for shader in shaders {
            inputs = inputs.union(shader.inputs);
            match build_pass(device, &pipeline_layout, &vs, shader, linear) {
                Ok(p) => passes.push(p),
                Err(e) => errors.push(e),
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }

        let uniform_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-post uniforms"),
            size: size_of::<PostUniforms>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("toastty-post sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });
        Ok(Self {
            passes,
            inputs,
            layout,
            uniform_buf,
            sampler,
            format,
            direct: direct || linear == format,
            targets: None,
        })
    }

    /// What the chain's shaders read (the union over its passes).
    pub(crate) fn inputs(&self) -> &ShaderInputs {
        &self.inputs
    }

    /// Bind `source` (the content texture, same size as the swapchain,
    /// created with [`linear_view_formats`] and `TEXTURE_BINDING`) and
    /// (re)create the ping-pong textures for its size. Call after
    /// creating the chain and whenever `source` is recreated.
    pub(crate) fn bind_source(&mut self, device: &wgpu::Device, source: &wgpu::Texture) {
        let size = (source.width(), source.height());
        let n = self.passes.len();
        // Pass outputs that aren't the swapchain; two textures cover any
        // chain length.
        let off_screen = n - 1 + usize::from(!self.direct);
        let (textures, views) = match self.targets.take() {
            Some(t) if t.size == size && t.textures.len() == off_screen.min(2) => {
                (t.textures, t.views)
            }
            _ => {
                let view_formats = linear_view_formats(self.format);
                let textures: Vec<wgpu::Texture> = (0..off_screen.min(2))
                    .map(|_| {
                        device.create_texture(&wgpu::TextureDescriptor {
                            label: Some("toastty-post ping-pong"),
                            size: wgpu::Extent3d {
                                width: size.0,
                                height: size.1,
                                depth_or_array_layers: 1,
                            },
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            format: self.format,
                            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                                | wgpu::TextureUsages::TEXTURE_BINDING
                                | wgpu::TextureUsages::COPY_SRC,
                            view_formats: &view_formats,
                        })
                    })
                    .collect();
                let views = textures.iter().map(linear_view).collect();
                (textures, views)
            }
        };
        let source_view = linear_view(source);
        let bind_groups = (0..n)
            .map(|i| {
                let input = if i == 0 {
                    &source_view
                } else {
                    &views[(i - 1) % 2]
                };
                device.create_bind_group(&wgpu::BindGroupDescriptor {
                    label: Some("toastty-post bind group"),
                    layout: &self.layout,
                    entries: &[
                        wgpu::BindGroupEntry {
                            binding: 0,
                            resource: self.uniform_buf.as_entire_binding(),
                        },
                        wgpu::BindGroupEntry {
                            binding: 1,
                            resource: wgpu::BindingResource::TextureView(input),
                        },
                        wgpu::BindGroupEntry {
                            binding: 2,
                            resource: wgpu::BindingResource::Sampler(&self.sampler),
                        },
                    ],
                })
            })
            .collect();
        self.targets = Some(Targets {
            size,
            textures,
            views,
            bind_groups,
        });
    }

    /// Upload the uniforms: the block up to the palette always, the
    /// palette when `palette` is set.
    pub(crate) fn write_uniforms(&self, queue: &wgpu::Queue, u: &PostUniforms, palette: bool) {
        let bytes = bytemuck::bytes_of(u);
        let end = if palette { bytes.len() } else { PALETTE_OFFSET };
        queue.write_buffer(&self.uniform_buf, 0, &bytes[..end]);
    }

    /// Record the passes: the bound source through every shader, into
    /// `frame` (the swapchain texture, or any texture of the chain's
    /// format with [`linear_view_formats`] and `COPY_DST`).
    pub(crate) fn encode(&self, encoder: &mut wgpu::CommandEncoder, frame: &wgpu::Texture) {
        let Some(t) = self.targets.as_ref() else {
            debug_assert!(false, "PostChain::encode before bind_source");
            return;
        };
        let n = self.passes.len();
        let frame_view = self.direct.then(|| linear_view(frame));
        for (i, pipeline) in self.passes.iter().enumerate() {
            let view = match frame_view.as_ref() {
                Some(v) if i + 1 == n => v,
                _ => &t.views[i % 2],
            };
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toastty-post pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // The triangle covers every pixel.
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(pipeline);
            rp.set_bind_group(0, &t.bind_groups[i], &[]);
            rp.draw(0..3, 0..1);
        }
        if !self.direct {
            encoder.copy_texture_to_texture(
                t.textures[(n - 1) % 2].as_image_copy(),
                frame.as_image_copy(),
                wgpu::Extent3d {
                    width: t.size.0,
                    height: t.size.1,
                    depth_or_array_layers: 1,
                },
            );
        }
    }
}

/// Uniform block at 0, `iChannel0` texture at 1, its sampler at 2.
fn bind_group_layout(device: &wgpu::Device) -> wgpu::BindGroupLayout {
    device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("toastty-post bind group layout"),
        entries: &[
            wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: NonZeroU64::new(size_of::<PostUniforms>() as u64),
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 1,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: true },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            },
            wgpu::BindGroupLayoutEntry {
                binding: 2,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                count: None,
            },
        ],
    })
}

/// One post pass's pipeline, created inside wgpu error scopes so a
/// shader that naga accepted but wgpu rejects comes back as an error
/// instead of reaching the uncaptured-error handler.
fn build_pass(
    device: &wgpu::Device,
    layout: &wgpu::PipelineLayout,
    vs: &wgpu::ShaderModule,
    shader: &CompiledShader,
    format: TextureFormat,
) -> Result<wgpu::RenderPipeline, ShaderError> {
    let validation = device.push_error_scope(wgpu::ErrorFilter::Validation);
    let internal = device.push_error_scope(wgpu::ErrorFilter::Internal);
    let label = shader.path.display().to_string();
    let fs = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(&label),
        source: wgpu::ShaderSource::Wgsl(shader.wgsl.as_str().into()),
    });
    let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(&label),
        layout: Some(layout),
        vertex: wgpu::VertexState {
            module: vs,
            entry_point: Some("vs_main"),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            buffers: &[],
        },
        fragment: Some(wgpu::FragmentState {
            module: &fs,
            entry_point: Some(&shader.entry_point),
            compilation_options: wgpu::PipelineCompilationOptions::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        primitive: wgpu::PrimitiveState::default(),
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        multiview_mask: None,
        cache: None,
    });
    // Pop in reverse push order.
    match poll_error(internal.pop()).or_else(|| poll_error(validation.pop())) {
        Some(e) => Err(ShaderError {
            path: shader.path.clone(),
            line: None,
            column: None,
            message: format!("rejected by the GPU backend: {e}"),
        }),
        None => Ok(pipeline),
    }
}

/// Poll an error-scope future once. wgpu's native ones are ready
/// immediately; a pending one reports no error (there is nothing we
/// could wait on in the render loop anyway).
fn poll_error(f: impl Future<Output = Option<wgpu::Error>>) -> Option<wgpu::Error> {
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(e) => e,
        Poll::Pending => None,
    }
}
