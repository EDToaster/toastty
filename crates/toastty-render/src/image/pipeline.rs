//! wgpu pipeline + texture storage for the image-drawing pass.
//!
//! Strategy: one bind group per texture slot (built once at upload),
//! swapped between draws. Each draw covers a run of consecutive
//! instances that share a texture, so only one texture is ever bound at
//! a time and the resident count is not limited by binding limits.
//!
//! Downscaling (SIZE-12): a kitty image's texture gets a full mip chain
//! the first frame it is drawn smaller than its source rect, and the
//! sampler is trilinear. The chain is generated on the GPU ([`MipGen`])
//! inside the frame's own command encoder
//! ([`ImagePipeline::encode_pending_mips`]), so it costs no extra queue
//! submit. Images only ever drawn at 1:1 or larger stay single-level:
//! they cost no extra memory or upload time (mpv-style per-frame
//! re-sends at native size pay nothing), and their sampling is
//! unchanged. With one level, or at 1:1 where the LOD is 0, the
//! trilinear sampler reads level 0 exactly as the old bilinear one did.

use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use std::num::NonZeroU64;
use toastty_term::{ImageData, ImageKey, ImageRegistry};

use super::atlas::{ImageTexEntry, ImageTextureCache, full_mip_count};
use super::instance::{ImageInstance, UNRESOLVED_TEXTURE};

/// Uniform buffer matching the WGSL `Globals` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ImageGlobals {
    pub viewport: [f32; 2],
    /// `(origin_x_px, origin_y_px)` — window-padding inset. Added to the
    /// quad position in the vertex shader before the px->NDC map. (Was
    /// the unused `tex_dims` field.)
    pub content_origin: [f32; 2],
}

/// The image rendering pipeline + per-image texture cache.
#[derive(Debug)]
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_buf_cap: u64,
    /// Extra instance buffers for the 2nd, 3rd … `render` call of a
    /// frame. `queue.write_buffer` lands at submit time, so every draw
    /// list in one pass needs its own buffer; reusing one would make all
    /// passes see the last list. Reset by [`ImagePipeline::begin_frame`].
    extra_bufs: Vec<(wgpu::Buffer, u64)>,
    calls_this_frame: usize,
    sampler: wgpu::Sampler,
    /// Texture storage: each slot's index matches
    /// `ImageTexEntry::texture_index` / `ImageInstance::texture_index`.
    /// `None` is a freed slot (its texture already dropped).
    slots: Vec<Option<TexSlot>>,
    /// Indices of `None` slots, reused by the next upload.
    free_indices: Vec<usize>,
    /// `max_texture_dimension_2d` of the device. Larger kitty images are
    /// downscaled to fit before upload (creating the texture would be a
    /// wgpu validation panic).
    max_texture_dim: u32,
    mip_gen: MipGen,
    /// Textures whose levels `1..n` still need generating, encoded by
    /// the next [`ImagePipeline::encode_pending_mips`]. Level 0 is
    /// already queued with `queue.write_texture`, which lands first.
    pending_mips: Vec<(wgpu::Texture, u32)>,
}

/// The texture format of every image texture (straight-alpha sRGB).
const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// GPU mip-chain generator: one alpha-weighted 2×2 box pass per level
/// (`shaders/mip.wgsl`).
#[derive(Debug)]
struct MipGen {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
}

impl MipGen {
    fn new(device: &wgpu::Device) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-image mip shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/mip.wgsl").into()),
        });
        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("toastty-image mip bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Texture {
                    sample_type: wgpu::TextureSampleType::Float { filterable: false },
                    view_dimension: wgpu::TextureViewDimension::D2,
                    multisampled: false,
                },
                count: None,
            }],
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-image mip pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("toastty-image mip pipeline"),
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
                    format: IMAGE_FORMAT,
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
        Self {
            pipeline,
            bind_layout,
        }
    }

    /// Encode passes filling levels `1..levels` of `texture` from level 0.
    fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        levels: u32,
    ) {
        let level_view = |level: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("toastty-image mip level"),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        for level in 1..levels {
            let src = level_view(level - 1);
            let dst = level_view(level);
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("toastty-image mip bg"),
                layout: &self.bind_layout,
                entries: &[wgpu::BindGroupEntry {
                    binding: 0,
                    resource: wgpu::BindingResource::TextureView(&src),
                }],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("toastty-image mip pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
    }
}

/// True when `inst` draws its source rect of a `width × height` texture
/// smaller than 1:1 on either axis, so sampling it needs mips.
#[allow(clippy::cast_precision_loss)] // image dims fit comfortably in 23-bit mantissa.
fn is_downscaled(inst: &ImageInstance, width: u32, height: u32) -> bool {
    // A hair of slack so float noise at exactly 1:1 doesn't count.
    const SLACK: f32 = 1.0 - 1e-4;
    let src_w = (inst.uv_max[0] - inst.uv_min[0]).abs() * width as f32;
    let src_h = (inst.uv_max[1] - inst.uv_min[1]).abs() * height as f32;
    inst.size[0] < src_w * SLACK || inst.size[1] < src_h * SLACK
}

/// One uploaded texture plus the bind group that samples it.
#[derive(Debug)]
struct TexSlot {
    // Held so the texture lives as long as its bind group.
    _texture: wgpu::Texture,
    bind_group: wgpu::BindGroup,
}

/// What [`ImagePipeline::resolve_kitty_textures`] did this frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStats {
    /// Textures uploaded (cache misses).
    pub uploaded: usize,
    /// Textures built with a mip chain because they are drawn
    /// downscaled: fresh uploads, or resident ones upgraded in place.
    pub mipped: usize,
    /// Textures dropped to get back under the byte budget.
    pub evicted: usize,
}

impl ImagePipeline {
    /// Build the pipeline (idempotent — call once at startup).
    #[allow(clippy::too_many_lines)] // wgpu builder is long but linear.
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("toastty-image shader"),
            source: wgpu::ShaderSource::Wgsl(include_str!("../../shaders/image.wgsl").into()),
        });

        let bind_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("toastty-image bind group layout"),
            entries: &[
                // globals (UBO).
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX | wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: NonZeroU64::new(
                            std::mem::size_of::<ImageGlobals>() as u64
                        ),
                    },
                    count: None,
                },
                // image texture (per-draw rebind).
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
                // sampler.
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
                    count: None,
                },
            ],
        });

        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("toastty-image pipeline layout"),
            bind_group_layouts: &[Some(&bind_layout)],
            immediate_size: 0,
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("toastty-image pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                buffers: &[ImageInstance::layout()],
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
            // Image layer shares the cell-layer depth (z=0.5 from
            // the shader); see `docs/decisions/rgp-protocol.md` §3.
            depth_stencil: Some(wgpu::DepthStencilState {
                format: wgpu::TextureFormat::Depth32Float,
                depth_write_enabled: Some(true),
                depth_compare: Some(wgpu::CompareFunction::LessEqual),
                stencil: wgpu::StencilState::default(),
                bias: wgpu::DepthBiasState::default(),
            }),
            multisample: wgpu::MultisampleState::default(),
            multiview_mask: None,
            cache: None,
        });

        let globals_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-image globals"),
            size: std::mem::size_of::<ImageGlobals>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let instance_buf_cap: u64 = 256 * std::mem::size_of::<ImageInstance>() as u64;
        let instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("toastty-image instances"),
            size: instance_buf_cap,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("toastty-image sampler"),
            mag_filter: wgpu::FilterMode::Linear,
            min_filter: wgpu::FilterMode::Linear,
            // Trilinear. Only textures drawn downscaled have mips, and
            // the LOD is 0 at 1:1 and below 0 when upscaling, so both
            // still sample level 0 bilinearly as before.
            mipmap_filter: wgpu::MipmapFilterMode::Linear,
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            ..Default::default()
        });

        Self {
            pipeline,
            bind_layout,
            globals_buf,
            instance_buf,
            instance_buf_cap,
            extra_bufs: Vec::new(),
            calls_this_frame: 0,
            sampler,
            slots: Vec::new(),
            free_indices: Vec::new(),
            max_texture_dim: device.limits().max_texture_dimension_2d,
            mip_gen: MipGen::new(device),
            pending_mips: Vec::new(),
        }
    }

    /// Encode the mip-chain passes queued by this frame's uploads into
    /// `encoder`. Call before the render pass that draws images, in the
    /// frame's encoder, so the chain costs no extra queue submit.
    pub fn encode_pending_mips(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        for (texture, levels) in self.pending_mips.drain(..) {
            self.mip_gen.encode(device, encoder, &texture, levels);
        }
    }

    /// Drop cached textures the registry no longer backs: images that
    /// were deleted, and images re-transmitted since upload (their
    /// registry generation moved on). Uploads nothing; textures are
    /// uploaded on demand by [`Self::resolve_kitty_textures`], which also
    /// checks the generation on every hit. Call when the term's image
    /// revision advances.
    ///
    /// Returns true iff anything was dropped.
    pub fn sync_registry(
        &mut self,
        registry: &ImageRegistry<ImageKey>,
        cache: &mut ImageTextureCache,
    ) -> bool {
        let stale: Vec<ImageKey> = cache
            .iter()
            .filter(|(id, e)| registry.generation(*id) != Some(e.generation))
            .map(|(id, _)| id)
            .collect();
        for id in &stale {
            if let Some(entry) = cache.remove(*id) {
                self.release_texture(entry.texture_index);
            }
        }
        !stale.is_empty()
    }

    /// Bind kitty-image `instances` to GPU textures for this frame.
    ///
    /// Each instance's store `key` is looked up in `cache`. A miss, or a
    /// hit whose registry generation has moved on (the id was
    /// re-transmitted, whatever its size or pixels), uploads from
    /// `registry`. An image drawn downscaled gets a mip chain: uploaded
    /// with one, or, if resident without, rebuilt in its slot (same
    /// `texture_index`); the chain itself is encoded by
    /// [`Self::encode_pending_mips`]. Images larger than the device's texture limit are
    /// downscaled to fit, keeping their aspect ratio. Instances whose
    /// image is gone from the registry are dropped (order is kept).
    /// Afterwards textures not drawn this frame are evicted, least
    /// recently drawn first, while the cache is over its byte budget.
    /// Call once per frame, even with no instances, so stale textures
    /// can still be evicted.
    pub fn resolve_kitty_textures(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        registry: &ImageRegistry<ImageKey>,
        cache: &mut ImageTextureCache,
        instances: &mut Vec<ImageInstance>,
    ) -> ResolveStats {
        cache.begin_frame();
        let mut stats = ResolveStats::default();
        instances.retain_mut(|inst| {
            let key = ImageKey(inst.key);
            let (Some(generation), Some(data)) = (registry.generation(key), registry.get(key))
            else {
                return false;
            };
            let entry = match plan(cache.get_touch(key), generation) {
                Plan::Use(e)
                    if e.mip_levels == 1
                        && full_mip_count(e.width, e.height) > 1
                        && is_downscaled(inst, e.width, e.height) =>
                {
                    // Drawn downscaled for the first time: rebuild with mips.
                    let levels = full_mip_count(e.width, e.height);
                    let fitted = fit_to_max_dim(data, self.max_texture_dim);
                    let slot = self.make_slot(device, queue, &fitted, levels);
                    self.slots[e.texture_index] = Some(slot);
                    let e = ImageTexEntry {
                        mip_levels: levels,
                        ..e
                    };
                    cache.insert(key, e);
                    stats.mipped += 1;
                    e
                }
                Plan::Use(e) => e,
                Plan::Upload { replaces } => {
                    if let Some(old) = replaces {
                        self.release_texture(old);
                    }
                    let fitted = fit_to_max_dim(data, self.max_texture_dim);
                    if let Cow::Owned(f) = &fitted {
                        tracing::warn!(
                            image_id = inst.image_id,
                            "kitty image {}x{} exceeds the GPU texture limit {}; drawing it downscaled to {}x{}",
                            data.width,
                            data.height,
                            self.max_texture_dim,
                            f.width,
                            f.height,
                        );
                    }
                    let levels = if is_downscaled(inst, fitted.width, fitted.height) {
                        full_mip_count(fitted.width, fitted.height)
                    } else {
                        1
                    };
                    let slot = self.make_slot(device, queue, &fitted, levels);
                    let entry = ImageTexEntry {
                        texture_index: self.store_slot(slot),
                        width: fitted.width,
                        height: fitted.height,
                        generation,
                        mip_levels: levels,
                    };
                    cache.insert(key, entry);
                    stats.uploaded += 1;
                    stats.mipped += usize::from(levels > 1);
                    entry
                }
            };
            inst.texture_index = entry.texture_index as u32;
            true
        });
        for (_, entry) in cache.evict_over_budget() {
            self.release_texture(entry.texture_index);
            stats.evicted += 1;
        }
        stats
    }

    /// Encode draws for `instances`. The caller has already filtered
    /// to a contiguous slice (e.g. below-text or above-text). One draw
    /// per run of consecutive instances sharing a texture (we rebind
    /// the texture between runs).
    ///
    /// `viewport` is `(width, height)` in pixels (full surface).
    /// `content_origin` is `(origin_x_px, origin_y_px)` — the window-padding
    /// inset added in the vertex shader before the px->NDC map.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        instances: &[ImageInstance],
        viewport: (f32, f32),
        content_origin: [f32; 2],
    ) {
        if instances.is_empty() {
            return;
        }
        // Upload globals.
        let globals = ImageGlobals {
            viewport: [viewport.0, viewport.1],
            content_origin,
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // Pick this call's instance buffer (the first call uses the
        // primary one), growing it if needed.
        let needed = std::mem::size_of_val(instances) as u64;
        let call = self.calls_this_frame;
        self.calls_this_frame += 1;
        let (buf, cap) = if call == 0 {
            (&mut self.instance_buf, &mut self.instance_buf_cap)
        } else {
            while self.extra_bufs.len() < call {
                let b = device.create_buffer(&wgpu::BufferDescriptor {
                    label: Some("toastty-image instances (extra)"),
                    size: 256 * std::mem::size_of::<ImageInstance>() as u64,
                    usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    mapped_at_creation: false,
                });
                self.extra_bufs
                    .push((b, 256 * std::mem::size_of::<ImageInstance>() as u64));
            }
            let (b, c) = &mut self.extra_bufs[call - 1];
            (b, c)
        };
        if needed > *cap {
            let new_cap = needed.next_power_of_two();
            *buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("toastty-image instances (grown)"),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            *cap = new_cap;
        }
        queue.write_buffer(buf, 0, bytemuck::cast_slice(instances));
        let buf = buf.clone();

        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, buf.slice(..));

        // One draw per run of same-texture instances, rebinding the
        // texture between runs. Runs keep the caller's (z) order.
        let mut start = 0usize;
        while start < instances.len() {
            let tex = instances[start].texture_index;
            let end = instances[start..]
                .iter()
                .position(|i| i.texture_index != tex)
                .map_or(instances.len(), |n| start + n);
            if tex != UNRESOLVED_TEXTURE
                && let Some(Some(slot)) = self.slots.get(tex as usize)
            {
                pass.set_bind_group(0, &slot.bind_group, &[]);
                pass.draw(0..6, start as u32..end as u32);
            }
            start = end;
        }
    }

    /// Mark the start of a frame: the next [`ImagePipeline::render`]
    /// call uses the primary instance buffer again.
    pub fn begin_frame(&mut self) {
        self.calls_this_frame = 0;
    }

    /// Upload a straight-alpha sRGB RGBA8 image (`width * height * 4`
    /// bytes) into a free texture slot and return its index (usable as
    /// [`ImageInstance::texture_index`]). Free with
    /// [`ImagePipeline::free_texture`]. Used by rich blocks, which keep
    /// their own cache outside the kitty registry.
    pub fn upload_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
    ) -> usize {
        let slot = self.make_slot_rgba(device, queue, width, height, pixels, 1);
        self.store_slot(slot)
    }

    fn make_slot(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &ImageData,
        mip_levels: u32,
    ) -> TexSlot {
        self.make_slot_rgba(
            device,
            queue,
            data.width,
            data.height,
            &data.pixels,
            mip_levels,
        )
    }

    /// Create a texture with `mip_levels` levels, upload `pixels` into
    /// level 0, queue the rest for [`Self::encode_pending_mips`], and
    /// build its bind group.
    fn make_slot_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
        mip_levels: u32,
    ) -> TexSlot {
        let mut usage = wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST;
        if mip_levels > 1 {
            usage |= wgpu::TextureUsages::RENDER_ATTACHMENT;
        }
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("toastty-image texture"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: mip_levels.max(1),
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: IMAGE_FORMAT,
            usage,
            view_formats: &[],
        });
        if width > 0 && height > 0 && pixels.len() >= (width as usize * height as usize * 4) {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d::ZERO,
                    aspect: wgpu::TextureAspect::All,
                },
                pixels,
                wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(width * 4),
                    rows_per_image: Some(height),
                },
                wgpu::Extent3d {
                    width,
                    height,
                    depth_or_array_layers: 1,
                },
            );
        }
        if mip_levels > 1 {
            self.pending_mips.push((texture.clone(), mip_levels));
        }
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("toastty-image bg"),
            layout: &self.bind_layout,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: self.globals_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: wgpu::BindingResource::TextureView(&view),
                },
                wgpu::BindGroupEntry {
                    binding: 2,
                    resource: wgpu::BindingResource::Sampler(&self.sampler),
                },
            ],
        });
        TexSlot {
            _texture: texture,
            bind_group,
        }
    }

    /// Put `slot` in a free index (or a new one) and return the index.
    fn store_slot(&mut self, slot: TexSlot) -> usize {
        if let Some(idx) = self.free_indices.pop() {
            self.slots[idx] = Some(slot);
            idx
        } else {
            self.slots.push(Some(slot));
            self.slots.len() - 1
        }
    }

    /// Return a slot from [`ImagePipeline::upload_rgba`] to the pool.
    pub fn free_texture(&mut self, index: usize) {
        self.release_texture(index);
    }

    fn release_texture(&mut self, index: usize) {
        // Dropping the slot drops the texture and bind group, so evicted
        // images really give their GPU memory back. The `take` guard
        // keeps a double free from handing one index out twice.
        if let Some(slot) = self.slots.get_mut(index)
            && slot.take().is_some()
        {
            self.free_indices.push(index);
        }
    }

    /// Number of live (uploaded, not freed) texture slots.
    #[cfg(test)]
    fn live_slots(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }
}

/// What [`ImagePipeline::resolve_kitty_textures`] does for one instance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Plan {
    /// The resident texture holds the image's current generation.
    Use(ImageTexEntry),
    /// Upload the image, first freeing the stale texture slot, if any.
    Upload { replaces: Option<usize> },
}

/// Decide between reusing `hit` and uploading, given the registry's
/// current `generation` for the image.
fn plan(hit: Option<ImageTexEntry>, generation: u64) -> Plan {
    match hit {
        Some(e) if e.generation == generation => Plan::Use(e),
        stale => Plan::Upload {
            replaces: stale.map(|e| e.texture_index),
        },
    }
}

/// `data` itself if both sides fit in `max_dim`, else a copy shrunk by
/// the smallest integer factor that fits, keeping the aspect ratio.
/// Each output texel is the alpha-weighted average of its source block
/// (partial blocks at the right and bottom edges average what exists).
fn fit_to_max_dim(data: &ImageData, max_dim: u32) -> Cow<'_, ImageData> {
    let max_dim = max_dim.max(1);
    if data.width <= max_dim && data.height <= max_dim {
        return Cow::Borrowed(data);
    }
    let factor = data.width.max(data.height).div_ceil(max_dim);
    let (out_w, out_h) = (data.width.div_ceil(factor), data.height.div_ceil(factor));
    let (src_w, src_h) = (data.width as usize, data.height as usize);
    let factor = factor as usize;
    let mut pixels = Vec::with_capacity(out_w as usize * out_h as usize * 4);
    for block_y in 0..out_h as usize {
        for block_x in 0..out_w as usize {
            let (mut rgb, mut alpha, mut count) = ([0u64; 3], 0u64, 0u64);
            for y in block_y * factor..((block_y + 1) * factor).min(src_h) {
                for x in block_x * factor..((block_x + 1) * factor).min(src_w) {
                    let at = (y * src_w + x) * 4;
                    let Some(px) = data.pixels.get(at..at + 4) else {
                        continue;
                    };
                    let px_alpha = u64::from(px[3]);
                    for (sum, &chan) in rgb.iter_mut().zip(px) {
                        *sum += u64::from(chan) * px_alpha;
                    }
                    alpha += px_alpha;
                    count += 1;
                }
            }
            for sum in rgb {
                pixels.push(u8::try_from(sum.checked_div(alpha).unwrap_or(0)).unwrap_or(u8::MAX));
            }
            pixels.push(u8::try_from(alpha.checked_div(count).unwrap_or(0)).unwrap_or(u8::MAX));
        }
    }
    Cow::Owned(ImageData {
        width: out_w,
        height: out_h,
        pixels,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn img(w: u32, h: u32, fill: u8) -> ImageData {
        ImageData {
            width: w,
            height: h,
            pixels: vec![fill; (w * h * 4) as usize],
        }
    }

    fn tex(idx: usize, generation: u64) -> ImageTexEntry {
        ImageTexEntry {
            texture_index: idx,
            width: 4,
            height: 4,
            generation,
            mip_levels: 1,
        }
    }

    /// The upload decision keys on the registry generation, never on
    /// content: a re-transmit with identical size and pixels still
    /// replaces the texture (review of #24).
    #[test]
    fn plan_reuses_only_the_current_generation() {
        assert_eq!(plan(None, 1), Plan::Upload { replaces: None });
        assert_eq!(plan(Some(tex(3, 1)), 1), Plan::Use(tex(3, 1)));
        assert_eq!(plan(Some(tex(3, 1)), 2), Plan::Upload { replaces: Some(3) });
        // Same-id same-size re-send through the real registry.
        let mut r = ImageRegistry::<ImageKey>::new(1024);
        r.insert(ImageKey(77), img(4, 4, 1)).unwrap();
        let resident = tex(0, r.generation(ImageKey(77)).unwrap());
        r.insert(ImageKey(77), img(4, 4, 1)).unwrap();
        assert_eq!(
            plan(Some(resident), r.generation(ImageKey(77)).unwrap()),
            Plan::Upload { replaces: Some(0) }
        );
    }

    #[test]
    fn fit_to_max_dim_keeps_small_images_and_aspect() {
        let small = img(8, 4, 9);
        assert!(matches!(fit_to_max_dim(&small, 8), Cow::Borrowed(_)));
        // 20000x4 under a 2048 limit: factor 10 -> 2000x1.
        let wide = img(20_000, 4, 200);
        let f = fit_to_max_dim(&wide, 2048);
        assert_eq!((f.width, f.height), (2000, 1));
        assert_eq!(f.pixels.len(), 2000 * 4);
        assert!(f.pixels.iter().all(|&b| b == 200));
        // Odd sizes: 5x3 at limit 2 -> factor 3 -> 2x1.
        let odd = img(5, 3, 7);
        let f = fit_to_max_dim(&odd, 2);
        assert_eq!((f.width, f.height), (2, 1));
    }

    #[test]
    fn fit_to_max_dim_averages_alpha_weighted() {
        // 2x1, one opaque white texel and one transparent red one,
        // shrunk to 1x1: no red bleeds in; alpha halves.
        let data = ImageData {
            width: 2,
            height: 1,
            pixels: vec![255, 255, 255, 255, 255, 0, 0, 0],
        };
        let f = fit_to_max_dim(&data, 1);
        assert_eq!(f.pixels, vec![255, 255, 255, 127]);
    }

    fn inst(image_id: u32) -> ImageInstance {
        ImageInstance {
            pos: [0.0, 0.0],
            size: [4.0, 4.0],
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            texture_index: UNRESOLVED_TEXTURE,
            z: 0,
            image_id,
            key: image_id,
        }
    }

    /// SIZE-8: 500 placed images all resolve to distinct live textures
    /// (the old cache capped residency at 14), and a second frame with
    /// the same placements uploads nothing.
    #[test]
    fn resolve_uploads_every_placed_image_once() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return; // No GPU: `try_device_and_queue` printed a skip note.
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(64 * 1024 * 1024);
        for id in 1u32..=500 {
            registry.insert(ImageKey(id), img(2, 2, id as u8)).unwrap();
        }
        let placed: Vec<ImageInstance> = (1u32..=500).map(inst).collect();

        let mut frame = placed.clone();
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(stats.uploaded, 500);
        assert_eq!(frame.len(), 500);
        let mut idx: Vec<u32> = frame.iter().map(|i| i.texture_index).collect();
        idx.sort_unstable();
        idx.dedup();
        assert_eq!(idx.len(), 500, "each image has its own texture");
        assert!(idx.iter().all(|i| *i != UNRESOLVED_TEXTURE));
        assert_eq!(pipeline.live_slots(), 500);

        let mut frame = placed;
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(stats, ResolveStats::default(), "no per-frame re-upload");
    }

    /// Transmitted-but-unplaced images never reach the GPU, and a
    /// placement whose image is gone is dropped.
    #[test]
    fn resolve_uploads_only_placed_images() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(64 * 1024 * 1024);
        for id in 1u32..=100 {
            registry.insert(ImageKey(id), img(2, 2, id as u8)).unwrap();
        }
        assert!(!pipeline.sync_registry(&registry, &mut cache));
        assert!(cache.is_empty(), "sync must not upload unplaced images");

        let mut frame = vec![inst(7), inst(999), inst(42)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(stats.uploaded, 2);
        let ids: Vec<u32> = frame.iter().map(|i| i.image_id).collect();
        assert_eq!(ids, vec![7, 42], "missing image dropped, order kept");
        assert_eq!(cache.len(), 2);
        assert_eq!(pipeline.live_slots(), 2);
    }

    /// Over budget, textures not drawn this frame are freed (GPU memory
    /// actually released) and their slots reused; drawn ones stay.
    #[test]
    fn resolve_evicts_undrawn_textures_over_budget() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        // 4x4 RGBA = 64 bytes per image; the budget fits two.
        let mut cache = ImageTextureCache::new(128);
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        for id in 1u32..=6 {
            registry.insert(ImageKey(id), img(4, 4, id as u8)).unwrap();
        }
        // Frame 1 draws all six: over budget, but nothing evictable.
        let mut frame: Vec<ImageInstance> = (1u32..=6).map(inst).collect();
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!((stats.uploaded, stats.evicted), (6, 0));
        assert_eq!(pipeline.live_slots(), 6);

        // Frame 2 draws only 1 and 2: the other four are freed.
        let mut frame = vec![inst(1), inst(2)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!((stats.uploaded, stats.evicted), (0, 4));
        assert_eq!(pipeline.live_slots(), 2);
        assert_eq!(cache.total_bytes(), 128);

        // Frame 3 brings 5 back: re-uploaded into a recycled slot, and
        // the least recently drawn of the others goes.
        let slots_before = pipeline.slots.len();
        let mut frame = vec![inst(5)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!((stats.uploaded, stats.evicted), (1, 1));
        assert_eq!(pipeline.slots.len(), slots_before, "slot reused");
        assert_eq!(cache.len(), 2, "back under budget");
        assert!(cache.get(ImageKey(5)).is_some());
    }

    /// Deleting or replacing an image drops its texture on the next
    /// registry sync; the replacement uploads on its next draw.
    #[test]
    fn sync_registry_drops_deleted_and_replaced_images() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), img(4, 4, 1)).unwrap();
        registry.insert(ImageKey(2), img(4, 4, 2)).unwrap();
        let mut frame = vec![inst(1), inst(2)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(pipeline.live_slots(), 2);

        registry.remove(ImageKey(1));
        registry.insert(ImageKey(2), img(4, 4, 99)).unwrap();
        assert!(pipeline.sync_registry(&registry, &mut cache));
        assert!(cache.is_empty());
        assert_eq!(pipeline.live_slots(), 0);
        assert!(!pipeline.sync_registry(&registry, &mut cache));

        let mut frame = vec![inst(2)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(stats.uploaded, 1);
        assert_eq!(
            cache.get(ImageKey(2)).unwrap().generation,
            registry.generation(ImageKey(2)).unwrap()
        );
    }

    /// Render `instances` into a `w × h` sRGB target cleared to
    /// transparent and read the pixels back (tightly packed RGBA8).
    fn render_offscreen(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &mut ImagePipeline,
        instances: &[ImageInstance],
        w: u32,
        h: u32,
    ) -> Vec<u8> {
        let extent = wgpu::Extent3d {
            width: w,
            height: h,
            depth_or_array_layers: 1,
        };
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test target"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let depth = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("test depth"),
            size: extent,
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Depth32Float,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());
        let depth_view = depth.create_view(&wgpu::TextureViewDescriptor::default());
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test encoder"),
        });
        pipeline.encode_pending_mips(device, &mut encoder);
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("test pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: Some(wgpu::RenderPassDepthStencilAttachment {
                    view: &depth_view,
                    depth_ops: Some(wgpu::Operations {
                        load: wgpu::LoadOp::Clear(1.0),
                        store: wgpu::StoreOp::Store,
                    }),
                    stencil_ops: None,
                }),
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pipeline.begin_frame();
            #[allow(clippy::cast_precision_loss)]
            pipeline.render(
                device,
                queue,
                &mut pass,
                instances,
                (w as f32, h as f32),
                [0.0, 0.0],
            );
        }
        let row = (w * 4).div_ceil(256) * 256;
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        encoder.copy_texture_to_buffer(
            target.as_image_copy(),
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            extent,
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

    /// `w × h` opaque `centre` colour with a 1 px black border.
    fn bordered(w: u32, h: u32, centre: [u8; 3]) -> ImageData {
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                let edge = x == 0 || y == 0 || x == w - 1 || y == h - 1;
                let c = if edge { [0, 0, 0] } else { centre };
                pixels.extend_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        ImageData {
            width: w,
            height: h,
            pixels,
        }
    }

    /// Review of #24: re-sending an id with the same size and identical
    /// edges (the old fingerprint) must show the new pixels, both when
    /// the registry sync runs in between and when it doesn't.
    #[test]
    fn same_size_resend_shows_new_pixels() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        let centre = |px: &[u8]| -> [u8; 3] {
            let i = (8 * 16 + 8) * 4;
            [px[i], px[i + 1], px[i + 2]]
        };
        let draw = |pipeline: &mut ImagePipeline,
                    cache: &mut ImageTextureCache,
                    registry: &ImageRegistry<ImageKey>| {
            let mut frame = vec![ImageInstance {
                size: [16.0, 16.0],
                ..inst(77)
            }];
            let stats =
                pipeline.resolve_kitty_textures(&device, &queue, registry, cache, &mut frame);
            (
                stats,
                render_offscreen(&device, &queue, pipeline, &frame, 16, 16),
            )
        };

        registry
            .insert(ImageKey(77), bordered(16, 16, [255, 0, 0]))
            .unwrap();
        let (stats, px) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 1);
        assert_eq!(centre(&px), [255, 0, 0]);

        // Re-send without a sync in between (one PTY read, no render).
        registry
            .insert(ImageKey(77), bordered(16, 16, [0, 255, 0]))
            .unwrap();
        let (stats, px) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 1, "stale texture reused");
        assert_eq!(centre(&px), [0, 255, 0]);
        assert_eq!(pipeline.live_slots(), 1, "old texture freed");

        // Re-send with the sync in between (normal revision path).
        registry
            .insert(ImageKey(77), bordered(16, 16, [0, 0, 255]))
            .unwrap();
        assert!(pipeline.sync_registry(&registry, &mut cache));
        let (stats, px) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 1);
        assert_eq!(centre(&px), [0, 0, 255]);

        // No change: no upload.
        let (stats, _) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 0);
    }

    /// Review of #24: an image wider than `max_texture_dimension_2d`
    /// used to be a wgpu validation panic. It now uploads downscaled
    /// (aspect kept) and draws.
    #[test]
    fn image_over_texture_limit_is_downscaled_not_a_panic() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let max = device.limits().max_texture_dimension_2d;
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(u64::MAX);
        registry
            .insert(ImageKey(9), bordered(max + 1, 4, [255, 0, 0]))
            .unwrap();
        let mut frame = vec![ImageInstance {
            size: [8.0, 2.0],
            ..inst(9)
        }];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(stats.uploaded, 1);
        let e = cache.get(ImageKey(9)).unwrap();
        assert!(e.width <= max && e.height <= max);
        assert_eq!((e.width, e.height), ((max + 1).div_ceil(2), 2));
        let px = render_offscreen(&device, &queue, &mut pipeline, &frame, 8, 2);
        assert!(
            px.chunks(4).any(|p| p[0] > 60 && p[1] < 30),
            "drawn: {px:?}"
        );
    }

    /// An instance drawing all of `image_id` into a `w × h` px box.
    fn inst_sized(image_id: u32, w: f32, h: f32) -> ImageInstance {
        ImageInstance {
            size: [w, h],
            ..inst(image_id)
        }
    }

    #[test]
    fn is_downscaled_compares_box_to_source_rect() {
        assert!(!is_downscaled(&inst_sized(1, 4.0, 4.0), 4, 4), "1:1");
        assert!(!is_downscaled(&inst_sized(1, 8.0, 8.0), 4, 4), "upscale");
        assert!(is_downscaled(&inst_sized(1, 2.0, 4.0), 4, 4), "x only");
        assert!(is_downscaled(&inst_sized(1, 4.0, 3.0), 4, 4), "y only");
        // A half-image source rect drawn at its own size is 1:1.
        let half = ImageInstance {
            uv_max: [0.5, 1.0],
            ..inst_sized(1, 2.0, 4.0)
        };
        assert!(!is_downscaled(&half, 4, 4));
    }

    /// SIZE-12: mips are built only once an image is drawn downscaled,
    /// in place (same texture index), and counted in the byte budget.
    #[test]
    fn resolve_builds_mips_only_when_downscaled() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), img(16, 8, 1)).unwrap();

        // 1:1 and upscaled draws: single level, no mip work.
        for size in [(16.0, 8.0), (32.0, 16.0)] {
            let mut frame = vec![inst_sized(1, size.0, size.1)];
            let stats =
                pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
            assert_eq!(stats.mipped, 0);
        }
        let flat = cache.get(ImageKey(1)).unwrap();
        assert_eq!(flat.mip_levels, 1);
        assert_eq!(cache.total_bytes(), 16 * 8 * 4);

        // First downscaled draw: rebuilt with the full chain in place.
        let mut frame = vec![inst_sized(1, 4.0, 2.0)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!((stats.uploaded, stats.mipped), (0, 1));
        let mipped = cache.get(ImageKey(1)).unwrap();
        assert_eq!(mipped.mip_levels, 5); // 16x8 8x4 4x2 2x1 1x1
        assert_eq!(mipped.texture_index, flat.texture_index);
        assert_eq!(frame[0].texture_index, flat.texture_index as u32);
        assert_eq!(cache.total_bytes(), (128 + 32 + 8 + 2 + 1) * 4);
        assert_eq!(pipeline.live_slots(), 1);

        // Later frames, downscaled or not, do no more mip work.
        for size in [(4.0, 2.0), (16.0, 8.0)] {
            let mut frame = vec![inst_sized(1, size.0, size.1)];
            let stats =
                pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
            assert_eq!(stats, ResolveStats::default());
        }

        // A replaced image drawn downscaled uploads straight with mips.
        registry.insert(ImageKey(1), img(16, 8, 77)).unwrap();
        pipeline.sync_registry(&registry, &mut cache);
        let mut frame = vec![inst_sized(1, 4.0, 2.0)];
        let stats =
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!((stats.uploaded, stats.mipped), (1, 1));
    }

    /// Opaque `w × h` image with distinct per-pixel bytes.
    fn gradient(w: u32, h: u32) -> ImageData {
        let mut pixels = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&[
                    (x * 37 + y * 11) as u8,
                    (x * 5 + y * 61) as u8,
                    200,
                    255,
                ]);
            }
        }
        ImageData {
            width: w,
            height: h,
            pixels,
        }
    }

    /// A 1:1 draw reproduces the source bytes exactly, both before and
    /// after the texture has gained a mip chain (stack A draws native
    /// size images 1:1 and relies on this).
    #[test]
    fn one_to_one_draw_is_pixel_exact_with_and_without_mips() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        let src = gradient(16, 8);
        registry.insert(ImageKey(1), src.clone()).unwrap();

        let mut frame = vec![inst_sized(1, 16.0, 8.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 8);
        assert_eq!(got, src.pixels, "1:1 without mips");

        // Force the mip chain, then draw 1:1 again.
        let mut down = vec![inst_sized(1, 4.0, 2.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut down);
        assert!(cache.get(ImageKey(1)).unwrap().mip_levels > 1);
        let mut frame = vec![inst_sized(1, 16.0, 8.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 8);
        assert_eq!(got, src.pixels, "1:1 with mips");
    }

    /// SIZE-12: 1 px white lines every 8 px, drawn at 1/8 scale. With
    /// mips every output column averages one line (uniform grey); plain
    /// bilinear sampling would hit a line in some columns and miss it
    /// in the rest.
    #[test]
    fn downscaled_thin_lines_stay_visible() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        let (w, h) = (128u32, 8u32);
        let mut pixels = Vec::new();
        for _ in 0..h {
            for x in 0..w {
                let v = if x % 8 == 0 { 255 } else { 0 };
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        registry
            .insert(
                ImageKey(1),
                ImageData {
                    width: w,
                    height: h,
                    pixels,
                },
            )
            .unwrap();
        let mut frame = vec![inst_sized(1, 16.0, 1.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 1);
        let reds: Vec<u8> = got.chunks(4).map(|p| p[0]).collect();
        let (lo, hi) = (*reds.iter().min().unwrap(), *reds.iter().max().unwrap());
        // 1/8 linear coverage is sRGB ~99; allow filter slop.
        assert!(lo >= 70, "a column lost its line: {reds:?}");
        assert!(hi - lo <= 12, "columns should be near-uniform: {reds:?}");
    }

    /// The mip box filter is alpha-weighted: a transparent texel's RGB
    /// doesn't bleed into the average.
    #[test]
    fn mip_average_ignores_rgb_of_transparent_texels() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        // 2x2: one opaque red texel, three fully transparent green ones.
        let mut pixels = vec![255, 0, 0, 255];
        for _ in 0..3 {
            pixels.extend_from_slice(&[0, 255, 0, 0]);
        }
        registry
            .insert(
                ImageKey(1),
                ImageData {
                    width: 2,
                    height: 2,
                    pixels,
                },
            )
            .unwrap();
        // Drawn at 1x1: samples level 1 (the 1x1 mip).
        let mut frame = vec![inst_sized(1, 1.0, 1.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        assert_eq!(cache.get(ImageKey(1)).unwrap().mip_levels, 2);
        let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 1, 1);
        // Alpha-blended over transparent black: red at 1/4 coverage,
        // no green at all.
        assert_eq!(got[1], 0, "green bled from transparent texels: {got:?}");
        assert!(got[0] > 100, "red lost: {got:?}");
    }
}
