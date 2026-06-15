//! wgpu pipeline + texture storage for the image-drawing pass.
//!
//! Strategy: ONE bind group, swap the bound texture per draw call.
//! Each draw covers exactly one image (the cache holds up to ~14
//! textures simultaneously, well within the downlevel limit).

use bytemuck::{Pod, Zeroable};
use std::collections::HashMap;
use std::num::NonZeroU64;
use toastty_term::{ImageData, ImageRegistry};

use super::atlas::{ImageTexEntry, ImageTextureCache};
use super::instance::ImageInstance;

/// Uniform buffer matching the WGSL `Globals` struct.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct ImageGlobals {
    pub viewport: [f32; 2],
    pub tex_dims: [f32; 2],
}

/// The image rendering pipeline + per-image texture cache.
#[derive(Debug)]
pub struct ImagePipeline {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
    globals_buf: wgpu::Buffer,
    instance_buf: wgpu::Buffer,
    instance_buf_cap: u64,
    sampler: wgpu::Sampler,
    /// Texture storage: each entry's index matches
    /// `ImageTexEntry::texture_index`.
    textures: Vec<wgpu::Texture>,
    /// Free indices we can reuse when an entry is evicted.
    free_indices: Vec<usize>,
    /// Cached views (rebuilt on insert / replace).
    views: Vec<wgpu::TextureView>,
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
            sampler,
            textures: Vec::new(),
            free_indices: Vec::new(),
            views: Vec::new(),
        }
    }

    /// Sync `cache` with `registry`: insert any missing entries (with a
    /// fresh GPU texture); replace any entries whose `content_hash`
    /// changed; drop entries the cache evicted.
    ///
    /// Returns true iff anything changed (the caller flips
    /// `needs_full_clear` based on this).
    pub fn sync_registry(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        registry: &ImageRegistry,
        cache: &mut ImageTextureCache,
    ) -> bool {
        let mut changed = false;

        // Drop entries the host removed.
        let resident_ids: Vec<u32> = cache.iter().map(|(id, _)| id).collect();
        for id in resident_ids {
            if !registry.contains(id)
                && let Some(entry) = cache.remove(id)
            {
                self.release_texture(entry.texture_index);
                changed = true;
            }
        }

        // Upload anything new or changed.
        for (id, data) in registry.iter() {
            let hash = hash_image(data);
            match cache.get(id) {
                Some(entry) if entry.content_hash == hash => {
                    // Up to date.
                }
                _ => {
                    // Insert (or replace).
                    //
                    // Two leak paths to plug:
                    //   1. Replace: if `id` already had an entry, its
                    //      old texture_index is about to be orphaned
                    //      by the new one. Free the slot first.
                    //   2. Evict: `cache.insert` may pop the LRU
                    //      victim(s). Predict them via
                    //      `peek_evictions_for_insert`, free their
                    //      slots, *then* allocate so the new texture
                    //      can reuse the just-freed slot. This keeps
                    //      `self.textures` bounded at `max_active`.
                    let prior_idx = cache.get(id).map(|e| e.texture_index);
                    if let Some(old) = prior_idx {
                        self.release_texture(old);
                    }
                    let predicted_evict = cache.peek_evictions_for_insert(id);
                    if !predicted_evict.is_empty() {
                        let pre_state: HashMap<u32, usize> =
                            cache.iter().map(|(id, e)| (id, e.texture_index)).collect();
                        for victim in &predicted_evict {
                            if let Some(&slot) = pre_state.get(victim) {
                                self.release_texture(slot);
                            }
                        }
                    }
                    let idx = self.allocate_texture(device, queue, data);
                    let entry = ImageTexEntry {
                        texture_index: idx,
                        width: data.width,
                        height: data.height,
                        content_hash: hash,
                    };
                    let actual_evicted = cache.insert(id, entry);
                    debug_assert_eq!(
                        actual_evicted, predicted_evict,
                        "peek_evictions_for_insert disagreed with actual insert"
                    );
                    changed = true;
                }
            }
        }

        if changed {
            // The viewport size hasn't necessarily changed but the
            // globals buf was just allocated and never written. Write
            // a placeholder; `render` re-writes per-frame anyway.
        }

        changed
    }

    /// Encode draws for `instances`. The caller has already filtered
    /// to a contiguous slice (e.g. below-text or above-text). One draw
    /// per instance (we rebind the texture between draws).
    ///
    /// `viewport` is `(width, height)` in pixels.
    pub fn render(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        instances: &[ImageInstance],
        viewport: (f32, f32),
    ) {
        if instances.is_empty() {
            return;
        }
        // Upload globals.
        let globals = ImageGlobals {
            viewport: [viewport.0, viewport.1],
            tex_dims: [0.0, 0.0],
        };
        queue.write_buffer(&self.globals_buf, 0, bytemuck::bytes_of(&globals));

        // Grow instance buffer if needed.
        let needed = std::mem::size_of_val(instances) as u64;
        if needed > self.instance_buf_cap {
            let new_cap = needed.next_power_of_two();
            self.instance_buf = device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("toastty-image instances (grown)"),
                size: new_cap,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.instance_buf_cap = new_cap;
        }
        queue.write_buffer(&self.instance_buf, 0, bytemuck::cast_slice(instances));

        pass.set_pipeline(&self.pipeline);
        pass.set_vertex_buffer(0, self.instance_buf.slice(..));

        // One draw per instance, rebinding the texture between draws.
        for (i, inst) in instances.iter().enumerate() {
            let Some(view) = self.views.get(inst.texture_index as usize) else {
                continue;
            };
            let bg = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("toastty-image bg per-draw"),
                layout: &self.bind_layout,
                entries: &[
                    wgpu::BindGroupEntry {
                        binding: 0,
                        resource: self.globals_buf.as_entire_binding(),
                    },
                    wgpu::BindGroupEntry {
                        binding: 1,
                        resource: wgpu::BindingResource::TextureView(view),
                    },
                    wgpu::BindGroupEntry {
                        binding: 2,
                        resource: wgpu::BindingResource::Sampler(&self.sampler),
                    },
                ],
            });
            pass.set_bind_group(0, &bg, &[]);
            let start = i as u32;
            pass.draw(0..6, start..start + 1);
        }
    }

    fn allocate_texture(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        data: &ImageData,
    ) -> usize {
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("toastty-image texture"),
            size: wgpu::Extent3d {
                width: data.width.max(1),
                height: data.height.max(1),
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8UnormSrgb,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &data.pixels,
            wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(data.width * 4),
                rows_per_image: Some(data.height),
            },
            wgpu::Extent3d {
                width: data.width.max(1),
                height: data.height.max(1),
                depth_or_array_layers: 1,
            },
        );
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        if let Some(idx) = self.free_indices.pop() {
            self.textures[idx] = texture;
            self.views[idx] = view;
            idx
        } else {
            self.textures.push(texture);
            self.views.push(view);
            self.textures.len() - 1
        }
    }

    fn release_texture(&mut self, index: usize) {
        // We don't actually deallocate the wgpu::Texture (just mark the
        // slot as free for the next allocation). The texture is dropped
        // when overwritten.
        self.free_indices.push(index);
    }
}

/// Cheap content hash for an image — used to detect re-uploads.
fn hash_image(data: &ImageData) -> u32 {
    // FNV-1a over a fingerprint: dims + first/last few bytes + length.
    // Full-content hashing would dominate the upload path; this is
    // good enough to catch the "in-place replace" case for the
    // texture cache.
    let mut h: u32 = 0x811C_9DC5;
    let mul = 0x0100_0193u32;
    for b in data.width.to_le_bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(mul);
    }
    for b in data.height.to_le_bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(mul);
    }
    for b in (data.pixels.len() as u32).to_le_bytes() {
        h = (h ^ u32::from(b)).wrapping_mul(mul);
    }
    for &b in data.pixels.iter().take(32) {
        h = (h ^ u32::from(b)).wrapping_mul(mul);
    }
    for &b in data.pixels.iter().rev().take(32) {
        h = (h ^ u32::from(b)).wrapping_mul(mul);
    }
    h
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

    #[test]
    fn hash_image_is_stable_for_same_input() {
        let a = img(2, 2, 5);
        let b = img(2, 2, 5);
        assert_eq!(hash_image(&a), hash_image(&b));
    }

    #[test]
    fn hash_image_differs_for_different_dims() {
        let a = img(2, 2, 5);
        let b = img(4, 4, 5);
        assert_ne!(hash_image(&a), hash_image(&b));
    }

    #[test]
    fn hash_image_differs_for_different_pixels() {
        let a = img(2, 2, 0);
        let b = img(2, 2, 1);
        assert_ne!(hash_image(&a), hash_image(&b));
    }

    /// Drives `ImagePipeline::sync_registry` past a tiny cache cap and
    /// asserts that the pipeline's slot bookkeeping stays bounded (no
    /// per-eviction texture leak).
    ///
    /// Without C1's fix, `textures.len()` grows monotonically because
    /// the cache evicts ids while the pipeline never recycles their
    /// slots into `free_indices`.
    #[test]
    fn sync_registry_does_not_leak_textures_past_cap() {
        use crate::{instance_descriptor, instance_flags_for_tests};
        use pollster::block_on;
        use toastty_term::ImageRegistry;
        use wgpu::{DeviceDescriptor, PowerPreference, RequestAdapterOptions};

        let instance = wgpu::Instance::new(instance_descriptor(instance_flags_for_tests()));
        let Ok(adapter) = block_on(instance.request_adapter(&RequestAdapterOptions {
            power_preference: PowerPreference::LowPower,
            force_fallback_adapter: false,
            compatible_surface: None,
        })) else {
            // No GPU available in CI sandbox — skip.
            return;
        };
        let Ok((device, queue)) = block_on(adapter.request_device(&DeviceDescriptor {
            label: Some("image_pipeline test device"),
            required_features: wgpu::Features::empty(),
            required_limits: wgpu::Limits::downlevel_defaults(),
            memory_hints: wgpu::MemoryHints::Performance,
            experimental_features: wgpu::ExperimentalFeatures::disabled(),
            trace: wgpu::Trace::Off,
        })) else {
            return;
        };

        let mut pipeline = ImagePipeline::new(&device, wgpu::TextureFormat::Rgba8UnormSrgb);
        let cap = 2usize;
        let mut cache = ImageTextureCache::new(cap);
        let mut registry = ImageRegistry::new(1024 * 1024);

        // Insert 6 images one at a time, syncing after each. The live
        // slot count (textures.len() - free_indices.len()) must equal
        // cache.len() and stay <= cap. Without C1's fix, free_indices
        // never grows on eviction and textures grows unboundedly.
        for id in 1u32..=6 {
            registry.insert(id, img(4, 4, id as u8)).unwrap();
            pipeline.sync_registry(&device, &queue, &registry, &mut cache);
            let live = pipeline.textures.len() - pipeline.free_indices.len();
            assert_eq!(
                live,
                cache.len(),
                "live slot count must match cache size after id={id} \
                 (textures={}, free_indices={}, cache={})",
                pipeline.textures.len(),
                pipeline.free_indices.len(),
                cache.len()
            );
            assert!(cache.len() <= cap);
            // `textures.len()` is also bounded by the cap once we've
            // hit steady state — eviction must recycle slots, not
            // grow the vec.
            assert!(
                pipeline.textures.len() <= cap,
                "textures vec grew past cap={cap} (id={id}): len={}, free={}",
                pipeline.textures.len(),
                pipeline.free_indices.len()
            );
        }

        // Replace path: re-insert id=6 with a different content hash;
        // the pipeline should release the old slot and reuse it.
        registry.insert(6, img(4, 4, 99)).unwrap();
        let len_before = pipeline.textures.len();
        pipeline.sync_registry(&device, &queue, &registry, &mut cache);
        let live = pipeline.textures.len() - pipeline.free_indices.len();
        assert_eq!(live, cache.len());
        assert_eq!(
            pipeline.textures.len(),
            len_before,
            "replace must not grow the textures vec"
        );
    }
}
