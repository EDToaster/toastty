//! CPU-side mesh + material types for RGP assets.
//!
//! `CpuAsset` is what the renderer eventually uploads to the GPU.
//! It is a simple bag of vertex attribute vectors + a base color;
//! richer material features (textures, PBR, normal maps) are
//! deferred per `docs/decisions/rgp-protocol.md`'s "out of scope"
//! list.
//!
//! The bundled procedural [`CpuMesh::unit_cube`] is the v1 demo
//! asset — license-clean (no external file) and small enough to
//! make the path-resolver smoke test trivial.

/// Face-of-cube helper: four corner positions + one shared normal.
type FaceQuad = (([f32; 3], [f32; 3], [f32; 3], [f32; 3]), [f32; 3]);

/// CPU-side mesh data: positions / normals / uvs, indexed.
///
/// Vertex layout is `(position vec3, normal vec3, uv vec2)`; the
/// renderer's vertex buffer interleaves these in M12d.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuMesh {
    /// Per-vertex position in object-local space.
    pub positions: Vec<[f32; 3]>,
    /// Per-vertex normal (unit length).
    pub normals: Vec<[f32; 3]>,
    /// Per-vertex UV coordinate. Empty when the mesh has no uvs.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle indices. Length is a multiple of 3.
    pub indices: Vec<u32>,
}

/// CPU-side material. v1 is lambertian + ambient — solid base
/// color, no texture sampling.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CpuMaterial {
    /// Linear-light RGBA. From the glTF
    /// `pbrMetallicRoughness.baseColorFactor` if present, else white.
    pub base_color: [f32; 4],
}

impl Default for CpuMaterial {
    fn default() -> Self {
        Self {
            base_color: [1.0, 1.0, 1.0, 1.0],
        }
    }
}

/// A loaded RGP asset, ready to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct CpuAsset {
    pub mesh: CpuMesh,
    pub material: CpuMaterial,
}

impl CpuMesh {
    /// Generate a unit cube centred at the origin, edge length 1.0.
    /// Per-face normals (each face has 4 unique vertices so the
    /// normal is constant — sharp edges, not smooth-shaded).
    #[must_use]
    pub fn unit_cube() -> Self {
        // Six faces × 4 vertices = 24 vertices, two tris per face.
        // Face-local UV: (0,0) bottom-left → (1,1) top-right.
        let h = 0.5_f32;
        #[rustfmt::skip]
        let faces: [FaceQuad; 6] = [
            // +X face
            (([ h, -h, -h], [ h,  h, -h], [ h,  h,  h], [ h, -h,  h]), [ 1.0,  0.0,  0.0]),
            // -X face
            (([-h, -h,  h], [-h,  h,  h], [-h,  h, -h], [-h, -h, -h]), [-1.0,  0.0,  0.0]),
            // +Y face
            (([-h,  h, -h], [-h,  h,  h], [ h,  h,  h], [ h,  h, -h]), [ 0.0,  1.0,  0.0]),
            // -Y face
            (([-h, -h,  h], [-h, -h, -h], [ h, -h, -h], [ h, -h,  h]), [ 0.0, -1.0,  0.0]),
            // +Z face
            (([-h, -h,  h], [ h, -h,  h], [ h,  h,  h], [-h,  h,  h]), [ 0.0,  0.0,  1.0]),
            // -Z face
            (([ h, -h, -h], [-h, -h, -h], [-h,  h, -h], [ h,  h, -h]), [ 0.0,  0.0, -1.0]),
        ];
        let mut positions = Vec::with_capacity(24);
        let mut normals = Vec::with_capacity(24);
        let mut uvs = Vec::with_capacity(24);
        let mut indices = Vec::with_capacity(36);
        for ((v0, v1, v2, v3), n) in faces {
            let base = u32::try_from(positions.len()).expect("at most 24 vertices");
            positions.extend_from_slice(&[v0, v1, v2, v3]);
            normals.extend_from_slice(&[n, n, n, n]);
            uvs.extend_from_slice(&[[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]]);
            indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
        }
        Self {
            positions,
            normals,
            uvs,
            indices,
        }
    }
}

impl CpuAsset {
    /// The built-in demo cube, used as the only entry in the
    /// embedded asset bundle for v1.
    #[must_use]
    pub fn unit_cube() -> Self {
        Self {
            mesh: CpuMesh::unit_cube(),
            material: CpuMaterial::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_cube_has_24_vertices_36_indices() {
        let m = CpuMesh::unit_cube();
        assert_eq!(m.positions.len(), 24);
        assert_eq!(m.normals.len(), 24);
        assert_eq!(m.uvs.len(), 24);
        assert_eq!(m.indices.len(), 36);
    }

    #[test]
    fn unit_cube_positions_are_in_bounds() {
        let m = CpuMesh::unit_cube();
        for p in &m.positions {
            for axis in p {
                assert!((-0.5..=0.5).contains(axis), "axis out of unit range: {axis}");
            }
        }
    }

    #[test]
    fn unit_cube_indices_are_valid() {
        let m = CpuMesh::unit_cube();
        let n = u32::try_from(m.positions.len()).unwrap();
        for &i in &m.indices {
            assert!(i < n, "index {i} >= vertex count {n}");
        }
        assert_eq!(m.indices.len() % 3, 0, "indices must be triangles");
    }

    #[test]
    fn unit_cube_normals_are_unit_length() {
        let m = CpuMesh::unit_cube();
        for n in &m.normals {
            let l2 = n[0] * n[0] + n[1] * n[1] + n[2] * n[2];
            assert!((l2 - 1.0).abs() < 1e-6, "normal not unit length: {n:?}");
        }
    }

    #[test]
    fn default_material_is_white() {
        let m = CpuMaterial::default();
        for (i, c) in m.base_color.iter().enumerate() {
            assert!((c - 1.0).abs() < f32::EPSILON, "channel {i} != 1.0: {c}");
        }
    }
}
