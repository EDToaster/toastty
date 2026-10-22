//! wgpu pipeline + texture storage for the image-drawing pass.
//!
//! Strategy: one bind group per texture slot (built once at upload),
//! swapped between draws. Each draw covers a run of consecutive
//! instances that share a texture, so only one texture is ever bound at
//! a time and the resident count is not limited by binding limits.

use bytemuck::{Pod, Zeroable};
use std::borrow::Cow;
use std::num::NonZeroU64;
use toastty_term::{ImageData, ImageKey, ImageRegistry};

use super::atlas::{ImageTexEntry, ImageTextureCache};
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
            mipmap_filter: wgpu::MipmapFilterMode::Nearest,
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
    /// `registry`. Images larger than the device's texture limit are
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
                    let entry = ImageTexEntry {
                        texture_index: self.allocate_texture(device, queue, &fitted),
                        width: fitted.width,
                        height: fitted.height,
                        generation,
                    };
                    cache.insert(key, entry);
                    stats.uploaded += 1;
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

    fn allocate_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &ImageData,
    ) -> usize {
        self.upload_rgba(device, queue, data.width, data.height, &data.pixels)
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
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("toastty-image texture"),
            size: wgpu::Extent3d {
                width: width.max(1),
                height: height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
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
        let slot = Some(TexSlot {
            _texture: texture,
            bind_group,
        });

        if let Some(idx) = self.free_indices.pop() {
            self.slots[idx] = slot;
            idx
        } else {
            self.slots.push(slot);
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
            size: [1.0, 1.0],
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

        registry.insert(ImageKey(77), bordered(16, 16, [255, 0, 0])).unwrap();
        let (stats, px) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 1);
        assert_eq!(centre(&px), [255, 0, 0]);

        // Re-send without a sync in between (one PTY read, no render).
        registry.insert(ImageKey(77), bordered(16, 16, [0, 255, 0])).unwrap();
        let (stats, px) = draw(&mut pipeline, &mut cache, &registry);
        assert_eq!(stats.uploaded, 1, "stale texture reused");
        assert_eq!(centre(&px), [0, 255, 0]);
        assert_eq!(pipeline.live_slots(), 1, "old texture freed");

        // Re-send with the sync in between (normal revision path).
        registry.insert(ImageKey(77), bordered(16, 16, [0, 0, 255])).unwrap();
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
}
