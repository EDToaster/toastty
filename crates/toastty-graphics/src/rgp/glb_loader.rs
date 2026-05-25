//! Minimal `.glb` → [`CpuAsset`] loader.
//!
//! Wraps the `gltf` crate (default features disabled, only `utils`
//! enabled) so we don't pull in the image / urlencoding / file-IO
//! dependencies of `import`. We feed bytes in, get mesh + material
//! out — no filesystem touched.
//!
//! v1 reads only the first primitive of the first mesh in the first
//! scene. Multi-mesh / multi-primitive glTFs are out of scope; the
//! protocol's anchor model (one object id ↔ one renderable) maps
//! cleanly onto "one mesh per asset."

use thiserror::Error;

use crate::rgp::asset::{CpuAsset, CpuMaterial, CpuMesh};

/// Errors from [`load_glb`].
#[derive(Debug, Error)]
pub enum GlbLoadError {
    /// `gltf` failed to parse the .glb container.
    #[error("glb parse error: {0}")]
    Parse(#[from] gltf::Error),
    /// The file has no meshes (e.g. a pure-scene .glb).
    #[error("no meshes in glb")]
    NoMesh,
    /// A required vertex attribute (positions) was missing.
    #[error("first primitive has no positions")]
    NoPositions,
    /// The first primitive uses a topology other than triangles.
    /// We don't support `TRIANGLE_STRIP` / `TRIANGLE_FAN` / `LINES` /
    /// `POINTS` in v1.
    #[error("unsupported primitive topology")]
    UnsupportedTopology,
}

/// Parse a `.glb` byte slice into a [`CpuAsset`].
///
/// Reads the first primitive of the first mesh; ignores skins,
/// animations, scenes, cameras, lights, and any further primitives.
/// The base color comes from `pbrMetallicRoughness.baseColorFactor`;
/// textures are not sampled (v1 lambertian solid-color shading).
pub fn load_glb(bytes: &[u8]) -> Result<CpuAsset, GlbLoadError> {
    let glb = gltf::Glb::from_slice(bytes)?;
    let doc_blob: &[u8] = glb.bin.as_deref().unwrap_or(&[]);
    let gltf = gltf::Gltf::from_slice(&glb.json)?;

    // First mesh, first primitive.
    let mesh = gltf.meshes().next().ok_or(GlbLoadError::NoMesh)?;
    let prim = mesh.primitives().next().ok_or(GlbLoadError::NoMesh)?;

    if prim.mode() != gltf::mesh::Mode::Triangles {
        return Err(GlbLoadError::UnsupportedTopology);
    }

    let reader = prim.reader(|_buffer| Some(doc_blob));

    let mut positions: Vec<[f32; 3]> = reader
        .read_positions()
        .ok_or(GlbLoadError::NoPositions)?
        .collect();
    let mut uvs: Vec<[f32; 2]> = reader
        .read_tex_coords(0)
        .map_or_else(Vec::new, |t| t.into_f32().collect());
    let mut indices: Vec<u32> = match reader.read_indices() {
        Some(it) => it.into_u32().collect(),
        None => (0..u32::try_from(positions.len()).unwrap_or(u32::MAX)).collect(),
    };
    let normals: Vec<[f32; 3]> = if let Some(it) = reader.read_normals() {
        it.collect()
    } else {
        // No normals in the GLB → synthesise flat per-face. Unweld
        // first so shared corner vertices (e.g. on a low-poly cube
        // that ships positions only) get a per-triangle copy with
        // the face normal, otherwise the fragment stage interpolates
        // averaged corner normals into a Gouraud-style gradient
        // across each face.
        let unwelded_positions: Vec<[f32; 3]> =
            indices.iter().map(|&i| positions[i as usize]).collect();
        let unwelded_uvs: Vec<[f32; 2]> = if uvs.is_empty() {
            Vec::new()
        } else {
            indices.iter().map(|&i| uvs[i as usize]).collect()
        };
        positions = unwelded_positions;
        uvs = unwelded_uvs;
        indices = (0..u32::try_from(positions.len()).unwrap_or(u32::MAX)).collect();
        derive_flat_normals(&positions, &indices)
    };

    let material = {
        let mat = prim.material();
        let bc = mat.pbr_metallic_roughness().base_color_factor();
        CpuMaterial { base_color: bc }
    };

    Ok(CpuAsset {
        mesh: CpuMesh {
            positions,
            normals,
            uvs,
            indices,
        },
        material,
    })
}

/// Derive flat per-vertex normals when a source lacks them. Walks
/// each triangle named by `indices`, computes its face normal,
/// and accumulates it onto each of the triangle's three vertices.
/// Returns a parallel `Vec` of the same length as `positions`.
///
/// Indices matter: tobj's `single_index: true` and glTF's normal
/// path both deduplicate positions and define triangles via the
/// index list, so positions are not laid out triangle-by-triangle.
/// An earlier version walked `positions.chunks_exact(3)` directly —
/// that left every vertex past the first triangle with a zero
/// normal (falling through to a default `[0, 1, 0]`), producing
/// patchy lambertian shading on indexed meshes.
///
/// Shared with the OBJ loader (which also commonly lacks normals
/// for hand-authored objects).
//
// Short single-letter names (`a`, `b`, `c`, `ab`, `ac`, `n`) are
// math conventions for triangle vertices and edge vectors.
#[allow(clippy::many_single_char_names)]
pub(crate) fn derive_flat_normals(
    positions: &[[f32; 3]],
    indices: &[u32],
) -> Vec<[f32; 3]> {
    let mut acc = vec![[0.0_f32; 3]; positions.len()];
    for tri in indices.chunks_exact(3) {
        let ia = tri[0] as usize;
        let ib = tri[1] as usize;
        let ic = tri[2] as usize;
        if ia >= positions.len() || ib >= positions.len() || ic >= positions.len() {
            continue;
        }
        let a = positions[ia];
        let b = positions[ib];
        let c = positions[ic];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &vi in &[ia, ib, ic] {
            acc[vi][0] += n[0];
            acc[vi][1] += n[1];
            acc[vi][2] += n[2];
        }
    }
    for v in &mut acc {
        let l = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
        if l > 0.0 {
            v[0] /= l;
            v[1] /= l;
            v[2] /= l;
        } else {
            *v = [0.0, 1.0, 0.0];
        }
    }
    acc
}

/// Build a tiny but valid `.glb` containing a single triangle.
///
/// Public so integration tests across the workspace (e.g.
/// `toastty-term/tests/integration.rs`) can exercise the payload
/// register path against known-good bytes without hand-rolling a
/// glTF document. The triangle:
///
///   v0 (0, 0, 0), v1 (1, 0, 0), v2 (0, 1, 0).
///
/// No normals, no uvs — the loader synthesises flat normals.
pub fn minimal_triangle_glb() -> Vec<u8> {
    let positions: [f32; 9] = [0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0, 1.0, 0.0];
    let mut bin: Vec<u8> = Vec::with_capacity(36);
    for f in positions {
        bin.extend_from_slice(&f.to_le_bytes());
    }
    let json = format!(
        r#"{{
              "asset": {{ "version": "2.0" }},
              "scene": 0,
              "scenes": [{{ "nodes": [0] }}],
              "nodes": [{{ "mesh": 0 }}],
              "meshes": [{{
                "primitives": [{{
                  "attributes": {{ "POSITION": 0 }},
                  "mode": 4
                }}]
              }}],
              "accessors": [{{
                "bufferView": 0,
                "componentType": 5126,
                "count": 3,
                "type": "VEC3",
                "min": [0.0, 0.0, 0.0],
                "max": [1.0, 1.0, 0.0]
              }}],
              "bufferViews": [{{
                "buffer": 0,
                "byteOffset": 0,
                "byteLength": {bin_len}
              }}],
              "buffers": [{{ "byteLength": {bin_len} }}]
            }}"#,
        bin_len = bin.len(),
    );
    let mut json_padded = json.into_bytes();
    while !json_padded.len().is_multiple_of(4) {
        json_padded.push(b' ');
    }
    let mut bin_padded = bin;
    while !bin_padded.len().is_multiple_of(4) {
        bin_padded.push(0);
    }
    let total: u32 = 12
        + 8
        + u32::try_from(json_padded.len()).unwrap()
        + 8
        + u32::try_from(bin_padded.len()).unwrap();
    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(b"glTF"); // magic
    out.extend_from_slice(&2u32.to_le_bytes()); // version
    out.extend_from_slice(&total.to_le_bytes());
    out.extend_from_slice(&u32::try_from(json_padded.len()).unwrap().to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_padded);
    out.extend_from_slice(&u32::try_from(bin_padded.len()).unwrap().to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin_padded);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_bytes_return_parse_error() {
        let err = load_glb(b"not a glb").unwrap_err();
        assert!(matches!(err, GlbLoadError::Parse(_)));
    }

    #[test]
    fn empty_bytes_return_parse_error() {
        let err = load_glb(b"").unwrap_err();
        assert!(matches!(err, GlbLoadError::Parse(_)));
    }

    /// Smoke test: build a minimal `.glb` in memory (single triangle
    /// with positions only, no normals/uvs) and round-trip it.
    /// Verifies the loader produces a usable `CpuMesh` and falls
    /// back to derived normals.
    #[test]
    fn derive_flat_normals_uses_indices_not_position_layout() {
        // Indexed quad: 4 deduped positions, 2 triangles sharing an
        // edge. Before the fix, the function walked positions in
        // chunks of 3 and left position[3] with a zero normal that
        // fell through to the default `[0, 1, 0]` — wrong for a
        // square sitting in the XY plane.
        let positions = [
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [1.0, -1.0, 0.0],
            [0.0, -1.0, 0.0],
        ];
        let indices = [0u32, 1, 2, 0, 2, 3];
        let normals = derive_flat_normals(&positions, &indices);
        assert_eq!(normals.len(), 4);
        for (i, n) in normals.iter().enumerate() {
            // Quad winding gives -Z normals; what matters is that
            // every vertex got the face normal, not the default.
            assert!(
                n[0].abs() < 1e-5 && n[1].abs() < 1e-5 && (n[2].abs() - 1.0).abs() < 1e-5,
                "vertex {i} normal not along Z: {n:?}",
            );
        }
    }

    #[test]
    fn minimal_triangle_glb_loads_with_derived_normals() {
        let glb = minimal_triangle_glb();
        let asset = load_glb(&glb).expect("loads");
        assert_eq!(asset.mesh.positions.len(), 3);
        assert_eq!(asset.mesh.normals.len(), 3);
        // Triangle is in the XY plane facing +Z; derived normal must be +Z.
        for n in &asset.mesh.normals {
            assert!((n[2] - 1.0).abs() < 1e-4, "expected +Z, got {n:?}");
        }
    }
}
