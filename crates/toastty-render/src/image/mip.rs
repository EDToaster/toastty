//! Mip chains for kitty image textures drawn downscaled (SIZE-12).
//!
//! - [`needed_levels`] picks how many levels a draw actually samples, so
//!   a texture only gets the levels its smallest draw needs (a ½-scale
//!   video frame: 3 levels, not 10).
//! - Small textures build their levels on the CPU ([`cpu_levels`]) and
//!   upload them with `write_texture`: no render passes at all.
//! - Larger ones use [`MipGen`], one GPU pass per level, encoded into the
//!   frame's own command encoder.
//!
//! Both paths use the same filter, in linear light and alpha-weighted
//! (kitty images are straight alpha, so a plain average would bleed the
//! RGB of transparent texels into edges). Per axis it is a 2-tap box for
//! even sizes and the standard 3-tap footprint for odd ones, so no
//! source row or column is dropped and content doesn't drift toward the
//! top-left at heavy downscale.

use std::sync::OnceLock;

use super::atlas::full_mip_count;
use super::instance::ImageInstance;

/// LOD bias applied when sampling images (`shaders/image.wgsl` uses the
/// same literal). Slightly negative to offset trilinear's softness on
/// thin features. At 1:1 or when upscaling the LOD is already <= 0, so
/// the bias changes nothing there.
pub(super) const LOD_BIAS: f32 = -0.5;

/// How much `inst` shrinks its source rect of a `width × height`
/// texture: the larger of the per-axis source-px / drawn-px ratios.
/// `<= 1` means 1:1 or upscaled.
#[allow(clippy::cast_precision_loss)] // image dims fit comfortably in 23-bit mantissa.
pub(super) fn draw_ratio(inst: &ImageInstance, width: u32, height: u32) -> f32 {
    let src_w = (inst.uv_max[0] - inst.uv_min[0]).abs() * width as f32;
    let src_h = (inst.uv_max[1] - inst.uv_min[1]).abs() * height as f32;
    let rx = src_w / inst.size[0].max(f32::MIN_POSITIVE);
    let ry = src_h / inst.size[1].max(f32::MIN_POSITIVE);
    rx.max(ry)
}

/// Levels a draw at `ratio` samples with [`LOD_BIAS`]: trilinear reads
/// `floor(lod)` and the level after it, so `floor(lod) + 2`, capped at
/// the full chain. 1 when the biased LOD is <= 0 (level 0 only).
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)] // lod > 0 and < 32 here.
pub(super) fn needed_levels(ratio: f32, width: u32, height: u32) -> u32 {
    let lod = ratio.log2() + LOD_BIAS;
    if lod.is_nan() || lod <= 0.0 {
        return 1;
    }
    (lod.floor() as u32 + 2).min(full_mip_count(width, height))
}

/// Texel count of levels `from..to` of a `width × height` chain.
pub(super) fn level_texels(width: u32, height: u32, from: u32, to: u32) -> u64 {
    (from..to)
        .map(|l| u64::from((width >> l).max(1)) * u64::from((height >> l).max(1)))
        .sum()
}

/// Source taps `(index, weight)` for output texel `i` along an axis of
/// `src` texels. Even sizes: the 2×1 box. Odd sizes (> 1): the 3-tap
/// footprint `(n-i, n, i+1) / (2n+1)` over `2i, 2i+1, 2i+2`, `n` being
/// the output size. A 1-texel axis copies. Unused taps have weight 0.
#[allow(clippy::cast_precision_loss)] // texture dims fit comfortably in 23-bit mantissa.
pub(super) fn axis_taps(src: u32, i: u32) -> [(u32, f32); 3] {
    if src <= 1 {
        [(0, 1.0), (0, 0.0), (0, 0.0)]
    } else if src.is_multiple_of(2) {
        [(2 * i, 0.5), (2 * i + 1, 0.5), (0, 0.0)]
    } else {
        let n = src / 2;
        let d = (2 * n + 1) as f32;
        [
            (2 * i, (n - i) as f32 / d),
            (2 * i + 1, n as f32 / d),
            (2 * i + 2, (i + 1) as f32 / d),
        ]
    }
}

/// sRGB byte -> linear-light table.
fn srgb_to_linear_table() -> &'static [f32; 256] {
    static T: OnceLock<[f32; 256]> = OnceLock::new();
    T.get_or_init(|| {
        let mut t = [0.0; 256];
        for (v, slot) in t.iter_mut().enumerate() {
            #[allow(clippy::cast_possible_truncation)]
            {
                *slot = crate::text::instance::srgb_channel_to_linear(v as u8);
            }
        }
        t
    })
}

/// Entries in the linear -> sRGB encode table (fine enough that dark
/// values round like the exact formula to within one code).
const ENCODE_STEPS: usize = 1 << 14;

/// Linear-light -> sRGB byte table, indexed by `linear * (STEPS - 1)`.
fn linear_to_srgb_table() -> &'static [u8] {
    static T: OnceLock<Vec<u8>> = OnceLock::new();
    T.get_or_init(|| {
        (0..ENCODE_STEPS)
            .map(|k| {
                #[allow(clippy::cast_precision_loss)]
                let l = k as f32 / (ENCODE_STEPS - 1) as f32;
                let s = if l <= 0.003_130_8 {
                    l * 12.92
                } else {
                    1.055 * l.powf(1.0 / 2.4) - 0.055
                };
                #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
                let b = (s * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
                b
            })
            .collect()
    })
}

#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss
)]
fn encode_srgb(l: f32) -> u8 {
    let t = linear_to_srgb_table();
    t[(l.clamp(0.0, 1.0) * (ENCODE_STEPS - 1) as f32 + 0.5) as usize]
}

/// Build levels `1..count` of a straight-alpha sRGB RGBA8 `base` level
/// (`width × height`) on the CPU. Returns each level's bytes, level 1
/// first. Works on premultiplied linear-light floats (so the average is
/// alpha-weighted, like [`MipGen`]) and keeps them between levels;
/// matches the GPU pass to within rounding.
pub(super) fn cpu_levels(base: &[u8], width: u32, height: u32, count: u32) -> Vec<Vec<u8>> {
    let lut = srgb_to_linear_table();
    let decode = |p: &[u8]| -> [f32; 4] {
        let a = f32::from(p[3]) / 255.0;
        [
            lut[p[0] as usize] * a,
            lut[p[1] as usize] * a,
            lut[p[2] as usize] * a,
            a,
        ]
    };
    let (mut sw, mut sh) = (width.max(1) as usize, height.max(1) as usize);
    let mut out = Vec::new();
    if count <= 1 || base.len() < sw * sh * 4 {
        return out;
    }
    // Level 1 straight from the bytes when both axes are even (the
    // common case), skipping a full decode of level 0.
    let mut src: Vec<[f32; 4]> = if sw.is_multiple_of(2) && sh.is_multiple_of(2) {
        let (dw, dh) = (sw / 2, sh / 2);
        let mut dst = Vec::with_capacity(dw * dh);
        for y in 0..dh {
            let r0 = &base[2 * y * sw * 4..(2 * y + 1) * sw * 4];
            let r1 = &base[(2 * y + 1) * sw * 4..(2 * y + 2) * sw * 4];
            for x in 0..dw {
                let i = 8 * x;
                dst.push(avg4(
                    decode(&r0[i..i + 4]),
                    decode(&r0[i + 4..i + 8]),
                    decode(&r1[i..i + 4]),
                    decode(&r1[i + 4..i + 8]),
                ));
            }
        }
        (sw, sh) = (dw, dh);
        out.push(encode_level(&dst));
        dst
    } else {
        base.chunks_exact(4).map(decode).collect()
    };
    while out.len() + 1 < count as usize {
        let dst = reduce(&src, sw, sh);
        (sw, sh) = ((sw / 2).max(1), (sh / 2).max(1));
        out.push(encode_level(&dst));
        src = dst;
    }
    out
}

fn avg4(a: [f32; 4], b: [f32; 4], c: [f32; 4], d: [f32; 4]) -> [f32; 4] {
    [
        (a[0] + b[0] + c[0] + d[0]) * 0.25,
        (a[1] + b[1] + c[1] + d[1]) * 0.25,
        (a[2] + b[2] + c[2] + d[2]) * 0.25,
        (a[3] + b[3] + c[3] + d[3]) * 0.25,
    ]
}

/// One level down from premultiplied linear `src` (`sw × sh`).
fn reduce(src: &[[f32; 4]], sw: usize, sh: usize) -> Vec<[f32; 4]> {
    let (dw, dh) = ((sw / 2).max(1), (sh / 2).max(1));
    let mut dst = Vec::with_capacity(dw * dh);
    if sw.is_multiple_of(2) && sh.is_multiple_of(2) {
        for y in 0..dh {
            let r0 = &src[2 * y * sw..(2 * y + 1) * sw];
            let r1 = &src[(2 * y + 1) * sw..(2 * y + 2) * sw];
            for x in 0..dw {
                dst.push(avg4(r0[2 * x], r0[2 * x + 1], r1[2 * x], r1[2 * x + 1]));
            }
        }
        return dst;
    }
    // Odd or 1-texel axes: per-axis taps (3-tap for odd sizes).
    #[allow(clippy::cast_possible_truncation)] // texture dims are u32.
    let taps_x: Vec<_> = (0..dw).map(|x| axis_taps(sw as u32, x as u32)).collect();
    for y in 0..dh {
        #[allow(clippy::cast_possible_truncation)]
        let ty = axis_taps(sh as u32, y as u32);
        for tx in &taps_x {
            let mut acc = [0.0f32; 4];
            for &(sy, wy) in &ty {
                if wy == 0.0 {
                    continue;
                }
                let row = &src[(sy as usize).min(sh - 1) * sw..][..sw];
                for &(sx, wx) in tx {
                    if wx == 0.0 {
                        continue;
                    }
                    let p = row[(sx as usize).min(sw - 1)];
                    let w = wx * wy;
                    for (a, v) in acc.iter_mut().zip(p) {
                        *a += v * w;
                    }
                }
            }
            dst.push(acc);
        }
    }
    dst
}

/// Premultiplied linear texels -> straight-alpha sRGB RGBA8 bytes.
fn encode_level(level: &[[f32; 4]]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(level.len() * 4);
    for p in level {
        let a = p[3];
        if a > 0.0 {
            #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let alpha = (a * 255.0 + 0.5).clamp(0.0, 255.0) as u8;
            bytes.extend_from_slice(&[
                encode_srgb(p[0] / a),
                encode_srgb(p[1] / a),
                encode_srgb(p[2] / a),
                alpha,
            ]);
        } else {
            bytes.extend_from_slice(&[0, 0, 0, 0]);
        }
    }
    bytes
}

/// GPU mip-chain generator: one filter pass per level
/// (`shaders/mip.wgsl`, same filter as [`cpu_levels`]).
#[derive(Debug)]
pub(super) struct MipGen {
    pipeline: wgpu::RenderPipeline,
    bind_layout: wgpu::BindGroupLayout,
}

impl MipGen {
    pub(super) fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
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
        Self {
            pipeline,
            bind_layout,
        }
    }

    /// Encode passes filling levels `from..to` of `texture`, each from
    /// the level above.
    pub(super) fn encode(
        &self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        texture: &wgpu::Texture,
        from: u32,
        to: u32,
    ) {
        let level_view = |level: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                label: Some("toastty-image mip level"),
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        for level in from.max(1)..to {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::instance::UNRESOLVED_TEXTURE;

    fn inst(w: f32, h: f32) -> ImageInstance {
        ImageInstance {
            pos: [0.0, 0.0],
            size: [w, h],
            uv_min: [0.0, 0.0],
            uv_max: [1.0, 1.0],
            texture_index: UNRESOLVED_TEXTURE,
            z: 0,
            image_id: 1,
            key: 1,
        }
    }

    /// Review of #34: only the levels the draw samples. 1:1, upscale and
    /// mild downscale (biased LOD <= 0) need none; ½ needs 2; 128 px
    /// drawn at 20 px needs 4; never more than the full chain.
    #[test]
    fn needed_levels_follow_the_draw_scale() {
        assert_eq!(needed_levels(draw_ratio(&inst(16.0, 8.0), 16, 8), 16, 8), 1);
        assert_eq!(
            needed_levels(draw_ratio(&inst(32.0, 16.0), 16, 8), 16, 8),
            1
        );
        assert_eq!(needed_levels(1.3, 64, 64), 1);
        assert_eq!(needed_levels(2.0, 640, 360), 2);
        assert_eq!(needed_levels(128.0 / 20.0, 128, 128), 4);
        assert_eq!(needed_levels(5.0, 1500, 900), 3);
        assert_eq!(needed_levels(1e6, 16, 8), 5);
        // Ratio comes from the larger axis, and from the source rect.
        assert!((draw_ratio(&inst(4.0, 8.0), 16, 8) - 4.0).abs() < 1e-6);
        let half = ImageInstance {
            uv_max: [0.5, 1.0],
            ..inst(8.0, 8.0)
        };
        assert!((draw_ratio(&half, 16, 8) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn level_texels_sums_the_chain() {
        assert_eq!(level_texels(8, 4, 1, 4), 8 + 2 + 1);
        assert_eq!(level_texels(128, 128, 1, 4), 4096 + 1024 + 256);
    }

    #[test]
    fn odd_axis_taps_cover_every_source_texel_equally() {
        // Each source texel gets the same total weight, dst/src, so the
        // mean is preserved and nothing is dropped.
        for src in [3u32, 5, 7, 1023] {
            let n = src / 2;
            let mut total = vec![0.0f64; src as usize];
            for i in 0..n {
                for (s, w) in axis_taps(src, i) {
                    total[s as usize] += f64::from(w);
                }
            }
            let want = f64::from(n) / f64::from(src);
            assert!(
                total.iter().all(|t| (t - want).abs() < 1e-5),
                "src={src}: {total:?}"
            );
        }
        assert_eq!(axis_taps(4, 1), [(2, 0.5), (3, 0.5), (0, 0.0)]);
        assert_eq!(axis_taps(1, 0)[0], (0, 1.0));
    }

    /// Review of #34: a 1023-wide row whose right quarter is red keeps
    /// its red coverage at every level (it lost ~35 % at the bottom of
    /// the chain when odd sizes dropped their last column).
    #[test]
    fn odd_sizes_keep_coverage_down_the_chain() {
        let w = 1023u32;
        let red_from = w - 256;
        let mut px = Vec::new();
        for x in 0..w {
            px.extend_from_slice(if x >= red_from {
                &[255, 0, 0, 255]
            } else {
                &[0, 0, 0, 255]
            });
        }
        let levels = cpu_levels(&px, w, 1, full_mip_count(w, 1));
        let lut = srgb_to_linear_table();
        let ideal = 256.0 / f64::from(w);
        for (n, level) in levels.iter().enumerate() {
            let texels = level.len() / 4;
            let mean: f64 = level
                .chunks(4)
                .map(|p| f64::from(lut[p[0] as usize]))
                .sum::<f64>()
                / f64::from(u32::try_from(texels).unwrap());
            assert!(
                (mean - ideal).abs() < 0.01,
                "level {}: mean {mean} vs {ideal}",
                n + 1
            );
        }
    }

    #[test]
    fn cpu_filter_is_alpha_weighted_in_linear_light() {
        // 2x2: one opaque red, three transparent green -> pure red, a/4.
        let mut px = vec![255, 0, 0, 255];
        for _ in 0..3 {
            px.extend_from_slice(&[0, 255, 0, 0]);
        }
        let l = cpu_levels(&px, 2, 2, 2);
        assert_eq!(l[0], vec![255, 0, 0, 64]);
        // Opaque black + white average in linear light: sRGB ~188, not 128.
        let px = [0, 0, 0, 255, 255, 255, 255, 255];
        let l = cpu_levels(&px, 2, 1, 2);
        assert!((187..=189).contains(&l[0][0]), "{:?}", l[0]);
    }
}
