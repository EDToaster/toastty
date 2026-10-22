//! wgpu pipeline + texture storage for the image-drawing pass.
//!
//! Strategy: one bind group per texture slot (built once at upload),
//! swapped between draws. Each draw covers a run of consecutive
//! instances that share a texture, so only one texture is ever bound at
//! a time and the resident count is not limited by binding limits.
//!
//! Downscaling (SIZE-12): a kitty image drawn smaller than its source
//! rect gets the mip levels that draw samples (see [`super::mip`]) and
//! the sampler is trilinear with a small negative LOD bias. Small
//! textures build their levels on the CPU; larger ones on the GPU,
//! encoded into the frame's own command encoder
//! ([`ImagePipeline::encode_pending_mips`]). Mip work is capped per frame
//! ([`MipLimits`]); an image over the cap draws from the levels it
//! already has (level 0 at worst) and is finished on later frames, which
//! [`ResolveStats::deferred`] asks the renderer to schedule. A level is
//! only bound for sampling once it has been filled, so an image never
//! samples an empty level.
//!
//! Images only ever drawn at 1:1 or larger stay single-level: they cost
//! no extra memory or upload time (mpv-style per-frame re-sends at native
//! size pay nothing), and their sampling is unchanged: at 1:1 or when
//! upscaling the LOD is <= 0, so level 0 is read exactly as before.

use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use std::num::NonZeroU64;
use toastty_term::{ImageData, ImageKey, ImageRegistry};

use super::atlas::{ImageTexEntry, ImageTextureCache};
use super::instance::{ImageInstance, UNRESOLVED_TEXTURE};
use super::mip::{MipGen, cpu_levels, draw_ratio, level_texels, needed_levels};

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
    /// GPU mip work queued by [`Self::resolve_kitty_textures`] and
    /// encoded by the next [`Self::encode_pending_mips`]. Entries are
    /// dropped when their slot is released or replaced, so skipped
    /// frames can't pile up work for (or keep alive) dead textures.
    pending_mips: Vec<PendingMip>,
    limits: MipLimits,
    /// Mip work still allowed this frame (reset by each resolve).
    budget: MipBudget,
}

/// Per-frame caps on mip generation, so one frame that first draws many
/// downscaled images (a thumbnail grid) can't stall.
#[derive(Debug, Clone, Copy)]
pub struct MipLimits {
    /// GPU mip passes per frame (one per level built).
    pub gpu_passes_per_frame: u32,
    /// Texels of CPU-built levels per frame.
    pub cpu_texels_per_frame: u64,
    /// Wall-clock cap on CPU level building per frame (at least one
    /// image always progresses), so a slow or loaded machine can't stall
    /// a frame either.
    pub cpu_time_per_frame: std::time::Duration,
    /// Textures with at most this many texels build their levels on the
    /// CPU (no render passes); larger ones use the GPU.
    pub cpu_max_texels: u64,
}

impl Default for MipLimits {
    fn default() -> Self {
        Self {
            gpu_passes_per_frame: 16,
            cpu_texels_per_frame: 1 << 18,
            cpu_time_per_frame: std::time::Duration::from_millis(4),
            cpu_max_texels: 256 * 256,
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct MipBudget {
    gpu_passes: u32,
    cpu_texels: u64,
    /// When this frame's resolve started, for `cpu_time_per_frame`.
    started: std::time::Instant,
    /// CPU level builds done this frame.
    cpu_jobs: u32,
}

/// GPU mip levels `from..to` to build for slot `index`.
#[derive(Debug)]
struct PendingMip {
    index: usize,
    texture: wgpu::Texture,
    from: u32,
    to: u32,
}

/// The texture format of every image texture (straight-alpha sRGB).
const IMAGE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// One uploaded texture plus the bind group that samples it.
#[derive(Debug)]
struct TexSlot {
    texture: wgpu::Texture,
    /// Samples levels `0..levels_ready` only.
    bind_group: wgpu::BindGroup,
    /// Levels the texture was created with.
    levels_alloc: u32,
    /// Levels filled and bound for sampling.
    levels_ready: u32,
    /// Levels filled or queued on the GPU (`>= levels_ready`).
    levels_queued: u32,
}

/// What [`ImagePipeline::resolve_kitty_textures`] did this frame.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ResolveStats {
    /// Textures uploaded (cache misses).
    pub uploaded: usize,
    /// Textures whose mip levels were built (CPU) or queued (GPU) this
    /// frame.
    pub mipped: usize,
    /// Drawn-downscaled textures still waiting for mip levels because
    /// this frame's [`MipLimits`] ran out. They draw from the levels
    /// they have; the caller should schedule another frame.
    pub deferred: usize,
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
            mip_gen: MipGen::new(device, IMAGE_FORMAT),
            pending_mips: Vec::new(),
            limits: MipLimits::default(),
            budget: MipBudget {
                gpu_passes: 0,
                cpu_texels: 0,
                started: std::time::Instant::now(),
                cpu_jobs: 0,
            },
        }
    }

    /// Override the per-frame mip work caps.
    pub fn set_mip_limits(&mut self, limits: MipLimits) {
        self.limits = limits;
    }

    /// Encode the GPU mip passes queued by this frame's resolve into
    /// `encoder`, then bind the filled levels for sampling. Call before
    /// the render pass that draws images, in the frame's encoder, so the
    /// chain costs no extra queue submit. Until this runs, those images
    /// keep sampling only their already-filled levels.
    pub fn encode_pending_mips(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        for job in std::mem::take(&mut self.pending_mips) {
            self.mip_gen
                .encode(device, encoder, &job.texture, job.from, job.to);
            let bind_group = self.make_bind_group(device, &job.texture, job.to);
            if let Some(Some(slot)) = self.slots.get_mut(job.index) {
                slot.levels_ready = job.to;
                slot.bind_group = bind_group;
            }
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
    /// `registry`. An image drawn downscaled gets the mip levels that
    /// draw samples, within this frame's [`MipLimits`] (see the module
    /// docs). Images larger than the device's texture limit are
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
        self.budget = MipBudget {
            gpu_passes: self.limits.gpu_passes_per_frame,
            cpu_texels: self.limits.cpu_texels_per_frame,
            started: std::time::Instant::now(),
            cpu_jobs: 0,
        };
        let mut stats = ResolveStats::default();
        instances.retain_mut(|inst| {
            let key = ImageKey(inst.key);
            let (Some(generation), Some(data)) = (registry.generation(key), registry.get(key))
            else {
                return false;
            };
            let mut entry = match plan(cache.get_touch(key), generation) {
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
                    let levels = needed_levels(
                        draw_ratio(inst, fitted.width, fitted.height),
                        fitted.width,
                        fitted.height,
                    );
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
                    entry
                }
            };
            let needed = needed_levels(
                draw_ratio(inst, entry.width, entry.height),
                entry.width,
                entry.height,
            );
            if needed > entry.mip_levels {
                // Drawn smaller than any earlier draw: more levels.
                // Rebuild in place (same `texture_index`).
                let fitted = fit_to_max_dim(data, self.max_texture_dim);
                let slot = self.make_slot(device, queue, &fitted, needed);
                self.replace_slot(entry.texture_index, slot);
                entry.mip_levels = needed;
                cache.insert(key, entry);
            }
            if needed > 1 {
                match self.fill_mips(device, queue, entry.texture_index, data) {
                    Fill::Done => {}
                    Fill::Started => stats.mipped += 1,
                    Fill::Deferred => stats.deferred += 1,
                }
            }
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

        debug_assert!(
            self.pending_mips.is_empty(),
            "encode_pending_mips must run (in the frame encoder) before image passes"
        );
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

    /// Create a texture with `mip_levels` levels and upload `pixels` into
    /// level 0. Only level 0 is filled and bound; [`Self::fill_mips`]
    /// does the rest.
    fn make_slot_rgba(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        width: u32,
        height: u32,
        pixels: &[u8],
        mip_levels: u32,
    ) -> TexSlot {
        let mut usage = wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_DST
            | wgpu::TextureUsages::COPY_SRC;
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
            write_level(queue, &texture, 0, width, height, pixels);
        }
        let bind_group = self.make_bind_group(device, &texture, 1);
        TexSlot {
            texture,
            bind_group,
            levels_alloc: mip_levels.max(1),
            levels_ready: 1,
            levels_queued: 1,
        }
    }

    /// Bind group sampling levels `0..levels` of `texture`.
    fn make_bind_group(
        &self,
        device: &wgpu::Device,
        texture: &wgpu::Texture,
        levels: u32,
    ) -> wgpu::BindGroup {
        let view = texture.create_view(&wgpu::TextureViewDescriptor {
            base_mip_level: 0,
            mip_level_count: Some(levels.max(1)),
            ..Default::default()
        });
        device.create_bind_group(&wgpu::BindGroupDescriptor {
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
        })
    }

    /// Fill slot `index`'s unbuilt mip levels if this frame's budget
    /// allows: on the CPU (built from `data`, uploaded and bound now)
    /// for small textures, else queued for [`Self::encode_pending_mips`].
    fn fill_mips(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        index: usize,
        data: &ImageData,
    ) -> Fill {
        let Some(Some(slot)) = self.slots.get(index) else {
            return Fill::Done;
        };
        let (from, to) = (slot.levels_queued, slot.levels_alloc);
        if from >= to {
            return Fill::Done;
        }
        let size = slot.texture.size();
        let (w, h) = (size.width, size.height);
        if u64::from(w) * u64::from(h) <= self.limits.cpu_max_texels {
            let cost = level_texels(w, h, from, to);
            let out_of_time = self.budget.cpu_jobs > 0
                && self.budget.started.elapsed() >= self.limits.cpu_time_per_frame;
            if cost > self.budget.cpu_texels || out_of_time {
                return Fill::Deferred;
            }
            self.budget.cpu_texels -= cost;
            self.budget.cpu_jobs += 1;
            // Level 0 as uploaded: a very thin image can be both small
            // and over the device limit, so fit it the same way.
            let texture = slot.texture.clone();
            let base = fit_to_max_dim(data, self.max_texture_dim);
            debug_assert_eq!((base.width, base.height), (w, h));
            let levels = cpu_levels(&base.pixels, w, h, to);
            for (level, bytes) in (from..to).zip(levels.iter().skip(from as usize - 1)) {
                write_level(
                    queue,
                    &texture,
                    level,
                    (w >> level).max(1),
                    (h >> level).max(1),
                    bytes,
                );
            }
            let bind_group = self.make_bind_group(device, &texture, to);
            if let Some(Some(slot)) = self.slots.get_mut(index) {
                slot.levels_ready = to;
                slot.levels_queued = to;
                slot.bind_group = bind_group;
            }
        } else {
            let passes = to - from;
            if passes > self.budget.gpu_passes {
                return Fill::Deferred;
            }
            self.budget.gpu_passes -= passes;
            self.pending_mips.push(PendingMip {
                index,
                texture: slot.texture.clone(),
                from,
                to,
            });
            if let Some(Some(slot)) = self.slots.get_mut(index) {
                slot.levels_queued = to;
            }
        }
        Fill::Started
    }

    /// Swap in a new texture for slot `index`, dropping work queued for
    /// the old one.
    fn replace_slot(&mut self, index: usize, slot: TexSlot) {
        self.pending_mips.retain(|p| p.index != index);
        if let Some(s) = self.slots.get_mut(index) {
            *s = Some(slot);
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
        self.pending_mips.retain(|p| p.index != index);
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

/// Outcome of [`ImagePipeline::fill_mips`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fill {
    /// Nothing to do: every level is built or already queued.
    Done,
    /// Levels built (CPU) or queued (GPU) this frame.
    Started,
    /// Over this frame's budget; try again next frame.
    Deferred,
}

/// Upload one mip level's tightly packed RGBA8 bytes.
fn write_level(
    queue: &wgpu::Queue,
    texture: &wgpu::Texture,
    level: u32,
    width: u32,
    height: u32,
    bytes: &[u8],
) {
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture,
            mip_level: level,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytes,
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
        render_offscreen_with(device, queue, pipeline, instances, w, h, true)
    }

    /// [`render_offscreen`], optionally skipping `encode_pending_mips`.
    fn render_offscreen_with(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pipeline: &mut ImagePipeline,
        instances: &[ImageInstance],
        w: u32,
        h: u32,
        encode: bool,
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
        if encode {
            pipeline.encode_pending_mips(device, &mut encoder);
        }
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

    /// Read mip `level` of `texture` back (tightly packed RGBA8).
    fn read_level(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        texture: &wgpu::Texture,
        level: u32,
    ) -> Vec<u8> {
        let size = texture.size();
        let (w, h) = ((size.width >> level).max(1), (size.height >> level).max(1));
        let row = (w * 4).div_ceil(256) * 256;
        let buf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("test level readback"),
            size: u64::from(row * h),
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("test level readback"),
        });
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture,
                mip_level: level,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(row),
                    rows_per_image: Some(h),
                },
            },
            wgpu::Extent3d {
                width: w,
                height: h,
                depth_or_array_layers: 1,
            },
        );
        queue.submit(std::iter::once(encoder.finish()));
        buf.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("poll");
        let data = buf.slice(..).get_mapped_range();
        let mut out = Vec::new();
        for y in 0..h {
            let start = (y * row) as usize;
            out.extend_from_slice(&data[start..start + (w * 4) as usize]);
        }
        out
    }

    fn slot(pipeline: &ImagePipeline, index: usize) -> &TexSlot {
        pipeline.slots[index].as_ref().expect("live slot")
    }

    /// Limits that force the GPU path.
    fn gpu_only() -> MipLimits {
        MipLimits {
            cpu_max_texels: 0,
            ..MipLimits::default()
        }
    }

    /// Review of #34: only the levels the draw samples are built, on
    /// the first downscaled draw, in place (same texture index), counted
    /// in the byte budget; a smaller draw later adds levels; then no more
    /// work.
    #[test]
    fn resolve_builds_only_the_levels_the_draw_needs() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), img(16, 8, 1)).unwrap();
        let frame_at = |pipeline: &mut ImagePipeline, cache: &mut ImageTextureCache, w, h| {
            let mut frame = vec![inst_sized(1, w, h)];
            let stats =
                pipeline.resolve_kitty_textures(&device, &queue, &registry, cache, &mut frame);
            (stats, frame[0].texture_index)
        };

        // 1:1 and upscaled: single level, no mip work.
        for (w, h) in [(16.0, 8.0), (32.0, 16.0)] {
            let (stats, _) = frame_at(&mut pipeline, &mut cache, w, h);
            assert_eq!((stats.mipped, stats.deferred), (0, 0));
        }
        let flat = cache.get(ImageKey(1)).unwrap();
        assert_eq!(flat.mip_levels, 1);

        // ¼ scale: LOD 2 - 0.5 -> 3 levels, built now (CPU path).
        let (stats, idx) = frame_at(&mut pipeline, &mut cache, 4.0, 2.0);
        assert_eq!((stats.uploaded, stats.mipped), (0, 1));
        assert_eq!(idx, flat.texture_index as u32);
        assert_eq!(cache.get(ImageKey(1)).unwrap().mip_levels, 3);
        assert_eq!(cache.total_bytes(), (128 + 32 + 8) * 4);
        assert_eq!(slot(&pipeline, flat.texture_index).levels_ready, 3);

        // 1/16 scale: the full 5-level chain, again in place.
        let (stats, idx) = frame_at(&mut pipeline, &mut cache, 1.0, 1.0);
        assert_eq!(stats.mipped, 1);
        assert_eq!(idx, flat.texture_index as u32);
        assert_eq!(cache.get(ImageKey(1)).unwrap().mip_levels, 5);
        assert_eq!(pipeline.live_slots(), 1);

        // Later frames, at any scale: no more work.
        for (w, h) in [(1.0, 1.0), (4.0, 2.0), (16.0, 8.0)] {
            let (stats, _) = frame_at(&mut pipeline, &mut cache, w, h);
            assert_eq!(stats, ResolveStats::default());
        }
    }

    /// Review of #34: mip work is capped per frame. Images over the cap
    /// report `deferred`, draw from level 0, and finish on later frames,
    /// on both the CPU and the GPU path.
    #[test]
    fn mip_work_is_capped_per_frame_and_finishes_later() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        for (limits, side) in [
            // CPU: 32x32 at 8x8 -> levels 1, 2 = 256 + 64 texels each.
            (
                MipLimits {
                    cpu_texels_per_frame: 1000,
                    cpu_time_per_frame: std::time::Duration::MAX,
                    ..MipLimits::default()
                },
                32u32,
            ),
            // GPU: 64x64 at 16x16 -> 2 passes each.
            (
                MipLimits {
                    gpu_passes_per_frame: 4,
                    ..gpu_only()
                },
                64,
            ),
        ] {
            let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
            pipeline.set_mip_limits(limits);
            let mut cache = ImageTextureCache::default();
            let mut registry = ImageRegistry::<ImageKey>::new(16 * 1024 * 1024);
            for id in 1u32..=10 {
                registry.insert(ImageKey(id), gradient(side, side)).unwrap();
            }
            #[allow(clippy::cast_precision_loss)]
            let box_px = side as f32 / 4.0;
            let mut done = 0;
            let mut frames = 0;
            loop {
                let mut frame: Vec<ImageInstance> = (1u32..=10)
                    .map(|id| inst_sized(id, box_px, box_px))
                    .collect();
                let stats = pipeline
                    .resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
                assert!(stats.mipped <= 3, "cap exceeded: {stats:?}");
                done += stats.mipped;
                frames += 1;
                // Deferred ones still draw (from the levels they have).
                assert_eq!(frame.len(), 10);
                let mut enc =
                    device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
                pipeline.encode_pending_mips(&device, &mut enc);
                queue.submit(std::iter::once(enc.finish()));
                if stats.deferred == 0 {
                    break;
                }
                assert!(frames < 10, "mip work never finished");
            }
            assert_eq!(done, 10);
            assert!(frames >= 4, "work should spread over frames ({frames})");
            for id in 1u32..=10 {
                let e = cache.get(ImageKey(id)).unwrap();
                assert_eq!(slot(&pipeline, e.texture_index).levels_ready, 3);
            }
        }
    }

    /// Review of #34: GPU-built levels are bound only once
    /// `encode_pending_mips` has encoded them (before, an image sampled
    /// zero-filled levels if the call was missing), and rendering with
    /// unencoded work trips a debug assertion.
    #[test]
    fn gpu_levels_bind_only_after_encode_pending_mips() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        pipeline.set_mip_limits(gpu_only());
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), gradient(64, 64)).unwrap();
        let mut frame = vec![inst_sized(1, 16.0, 16.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let idx = cache.get(ImageKey(1)).unwrap().texture_index;
        assert_eq!(pipeline.pending_mips.len(), 1);
        assert_eq!(
            (
                slot(&pipeline, idx).levels_ready,
                slot(&pipeline, idx).levels_queued
            ),
            (1, 3)
        );

        if cfg!(debug_assertions) {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                render_offscreen_with(&device, &queue, &mut pipeline, &frame, 16, 16, false)
            }));
            assert!(r.is_err(), "render with unencoded mip work must assert");
        }

        let px = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 16);
        assert!(pipeline.pending_mips.is_empty());
        assert_eq!(slot(&pipeline, idx).levels_ready, 3);
        assert!(px.chunks(4).all(|p| p[3] == 255), "image drawn everywhere");
    }

    /// Review of #34: queued GPU work for a slot is dropped when the slot
    /// is released, so skipped frames can't keep dead textures alive.
    #[test]
    fn pending_mips_are_dropped_with_their_slot() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        pipeline.set_mip_limits(gpu_only());
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), gradient(64, 64)).unwrap();
        // A re-send at a downscale on every "skipped" frame (resolve,
        // no encode): only the current texture's work stays queued.
        for _ in 0..5 {
            registry.insert(ImageKey(1), gradient(64, 64)).unwrap();
            pipeline.sync_registry(&registry, &mut cache);
            let mut frame = vec![inst_sized(1, 16.0, 16.0)];
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
            assert_eq!(pipeline.pending_mips.len(), 1);
        }
        registry.remove(ImageKey(1));
        pipeline.sync_registry(&registry, &mut cache);
        assert!(pipeline.pending_mips.is_empty());
    }

    /// The GPU pass and the CPU filter build the same levels, including
    /// odd sizes (3-tap) and transparent texels (alpha-weighted).
    #[test]
    fn gpu_and_cpu_mips_agree() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut src = gradient(37, 21);
        // Some fully transparent texels with junk RGB.
        for i in (0..src.pixels.len()).step_by(4 * 7) {
            src.pixels[i + 3] = 0;
            src.pixels[i + 1] = 255;
        }
        let cpu = crate::image::mip::cpu_levels(&src.pixels, 37, 21, 6);

        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        pipeline.set_mip_limits(gpu_only());
        let mut cache = ImageTextureCache::default();
        let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
        registry.insert(ImageKey(1), src).unwrap();
        let mut frame = vec![inst_sized(1, 1.0, 1.0)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        pipeline.encode_pending_mips(&device, &mut enc);
        queue.submit(std::iter::once(enc.finish()));
        let e = cache.get(ImageKey(1)).unwrap();
        assert_eq!(e.mip_levels, 6);
        let texture = slot(&pipeline, e.texture_index).texture.clone();
        for level in 1..6u32 {
            let gpu = read_level(&device, &queue, &texture, level);
            let want = &cpu[level as usize - 1];
            assert_eq!(gpu.len(), want.len(), "level {level}");
            for (k, (g, c)) in gpu.iter().zip(want).enumerate() {
                assert!(
                    (i32::from(*g) - i32::from(*c)).abs() <= 2,
                    "level {level} byte {k}: gpu {g} cpu {c}"
                );
            }
        }
    }

    /// A 1:1 draw reproduces the source bytes exactly, both before and
    /// after the texture has gained mip levels (stack A draws native-size
    /// images 1:1 and relies on this; the LOD bias changes nothing here).
    #[test]
    fn one_to_one_draw_is_pixel_exact_with_and_without_mips() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        for limits in [MipLimits::default(), gpu_only()] {
            let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
            pipeline.set_mip_limits(limits);
            let mut cache = ImageTextureCache::default();
            let mut registry = ImageRegistry::<ImageKey>::new(1024 * 1024);
            let src = gradient(16, 8);
            registry.insert(ImageKey(1), src.clone()).unwrap();

            let mut frame = vec![inst_sized(1, 16.0, 8.0)];
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
            let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 8);
            assert_eq!(got, src.pixels, "1:1 without mips");

            let mut down = vec![inst_sized(1, 4.0, 2.0)];
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut down);
            assert!(cache.get(ImageKey(1)).unwrap().mip_levels > 1);
            let mut frame = vec![inst_sized(1, 16.0, 8.0)];
            pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
            let got = render_offscreen(&device, &queue, &mut pipeline, &frame, 16, 8);
            assert_eq!(got, src.pixels, "1:1 with mips");
        }
    }

    /// SIZE-12: 1 px white lines every 8 px, drawn at 1/8 scale. With
    /// mips every output column averages one line (uniform grey); plain
    /// bilinear sampling hits no line at all. Both mip paths.
    #[test]
    fn downscaled_thin_lines_stay_visible() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        for limits in [MipLimits::default(), gpu_only()] {
            let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
            pipeline.set_mip_limits(limits);
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
    }

    /// The GPU mip level is alpha-weighted: a transparent texel's RGB
    /// doesn't bleed into the average.
    #[test]
    fn gpu_mip_ignores_rgb_of_transparent_texels() {
        let Some((device, queue)) = crate::test_gpu::try_device_and_queue() else {
            return;
        };
        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        pipeline.set_mip_limits(gpu_only());
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
        let mut frame = vec![inst_sized(1, 0.5, 0.5)];
        pipeline.resolve_kitty_textures(&device, &queue, &registry, &mut cache, &mut frame);
        let mut enc =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        pipeline.encode_pending_mips(&device, &mut enc);
        queue.submit(std::iter::once(enc.finish()));
        let e = cache.get(ImageKey(1)).unwrap();
        assert_eq!(e.mip_levels, 2);
        let level1 = read_level(
            &device,
            &queue,
            &slot(&pipeline, e.texture_index).texture,
            1,
        );
        assert_eq!(
            level1[1], 0,
            "green bled from transparent texels: {level1:?}"
        );
        assert!(
            level1[0] >= 254 && (63..=65).contains(&level1[3]),
            "{level1:?}"
        );
    }
}
