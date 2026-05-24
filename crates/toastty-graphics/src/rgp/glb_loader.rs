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

    let positions: Vec<[f32; 3]> = reader
        .read_positions()
        .ok_or(GlbLoadError::NoPositions)?
        .collect();
    let normals: Vec<[f32; 3]> = if let Some(it) = reader.read_normals() {
        it.collect()
    } else {
        derive_flat_normals(&positions)
    };
    let uvs: Vec<[f32; 2]> = reader
        .read_tex_coords(0)
        .map_or_else(Vec::new, |t| t.into_f32().collect());
    let indices: Vec<u32> = match reader.read_indices() {
        Some(it) => it.into_u32().collect(),
        None => (0..u32::try_from(positions.len()).unwrap_or(u32::MAX)).collect(),
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

/// Derive flat per-vertex normals when the glTF lacks them. Walks
/// triangles assuming the position list is already indexable; called
/// only when `read_normals` returned `None`. Returns a parallel
/// `Vec` of the same length as `positions`, with each vertex's
/// normal set to the area-weighted average of its incident
/// triangles' face normals.
//
// Short single-letter names (`a`, `b`, `c`, `ab`, `ac`, `n`) are
// math conventions for triangle vertices and edge vectors.
#[allow(clippy::many_single_char_names)]
fn derive_flat_normals(positions: &[[f32; 3]]) -> Vec<[f32; 3]> {
    let mut acc = vec![[0.0_f32; 3]; positions.len()];
    let mut i = 0;
    while i + 2 < positions.len() {
        let a = positions[i];
        let b = positions[i + 1];
        let c = positions[i + 2];
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let n = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for j in 0..3 {
            let v = &mut acc[i + j];
            v[0] += n[0];
            v[1] += n[1];
            v[2] += n[2];
        }
        i += 3;
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
