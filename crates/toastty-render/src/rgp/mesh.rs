//! GPU-side mesh cache for RGP assets.
//!
//! Each registered asset (by id) becomes one `GpuMesh` — an
//! interleaved vertex buffer plus an index buffer. The cache
//! diffs against `RgpScene::revision()` and uploads / evicts as
//! needed.

use std::collections::HashMap;

use bytemuck::{Pod, Zeroable};
use toastty_graphics::rgp::asset::CpuAsset;
use wgpu::util::DeviceExt;

/// Interleaved vertex layout: `(position vec3, normal vec3, uv vec2)`.
/// 8 floats, 32 bytes per vertex. Matches `Vertex` in `rgp.wgsl`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Pod, Zeroable)]
pub struct Vertex {
    pub pos: [f32; 3],
    pub normal: [f32; 3],
    pub uv: [f32; 2],
}

impl Vertex {
    pub const ATTRIBUTES: [wgpu::VertexAttribute; 3] = wgpu::vertex_attr_array![
        0 => Float32x3,
        1 => Float32x3,
        2 => Float32x2,
    ];

    pub fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Self>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRIBUTES,
        }
    }
}

/// One asset uploaded to the GPU.
pub struct GpuMesh {
    pub vertex_buffer: wgpu::Buffer,
    pub index_buffer: wgpu::Buffer,
    pub index_count: u32,
    /// Material base color (linear-light RGBA). Reused per-draw to
    /// compute the final tint.
    pub base_color: [f32; 4],
}

impl std::fmt::Debug for GpuMesh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GpuMesh")
            .field("index_count", &self.index_count)
            .field("base_color", &self.base_color)
            .finish_non_exhaustive()
    }
}

impl GpuMesh {
    /// Build a `GpuMesh` from a CPU-side `CpuAsset` by interleaving
    /// its attributes and uploading both buffers.
    pub fn upload(device: &wgpu::Device, asset: &CpuAsset) -> Self {
        let positions = &asset.mesh.positions;
        let normals = &asset.mesh.normals;
        let uvs = &asset.mesh.uvs;
        let n = positions.len();
        let mut interleaved = Vec::<Vertex>::with_capacity(n);
        for i in 0..n {
            interleaved.push(Vertex {
                pos: positions[i],
                normal: *normals.get(i).unwrap_or(&[0.0, 1.0, 0.0]),
                uv: *uvs.get(i).unwrap_or(&[0.0, 0.0]),
            });
        }
        let vertex_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rgp vertex buffer"),
            contents: bytemuck::cast_slice(&interleaved),
            usage: wgpu::BufferUsages::VERTEX,
        });
        let index_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("rgp index buffer"),
            contents: bytemuck::cast_slice(&asset.mesh.indices),
            usage: wgpu::BufferUsages::INDEX,
        });
        Self {
            vertex_buffer,
            index_buffer,
            index_count: u32::try_from(asset.mesh.indices.len()).expect("index count fits u32"),
            base_color: asset.material.base_color,
        }
    }
}

/// GPU asset cache keyed by RGP asset id. Drops entries whose ids
/// no longer appear in the scene.
#[derive(Debug, Default)]
pub struct GpuAssetCache {
    pub meshes: HashMap<u32, GpuMesh>,
}

impl GpuAssetCache {
    pub fn new() -> Self {
        Self {
            meshes: HashMap::new(),
        }
    }

    /// Rebuild the GPU cache from the scene's registered assets.
    ///
    /// Wipes + re-uploads every asset rather than diffing by id. The
    /// scene's revision counter only bumps on actual mutations
    /// (register / place / update / delete), so this is not called
    /// per frame — animation ticks don't bump revision. The
    /// previous "missing id → upload" diff missed re-registers of
    /// an existing id (e.g. the `draw` example calling
    /// `register_payload` repeatedly on id=700 with growing OBJ
    /// data), leaving the GPU stuck on the first version. Always
    /// re-uploading is simpler than per-asset version tracking and
    /// the meshes are small in v1.
    pub fn sync(
        &mut self,
        device: &wgpu::Device,
        scene: &toastty_graphics::rgp::scene::RgpScene,
    ) {
        self.meshes.clear();
        for (id, asset) in scene.assets() {
            self.meshes.insert(id, GpuMesh::upload(device, &asset.data));
        }
    }
}
