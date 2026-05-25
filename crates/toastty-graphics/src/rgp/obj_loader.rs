//! Minimal Wavefront `.obj` → [`CpuAsset`] loader.
//!
//! Wraps `tobj` for parsing. Takes the FIRST model in the OBJ —
//! multi-model OBJs aren't supported in v1, matching the single-
//! mesh assumption from the GLB loader. Materials are not resolved
//! (no `.mtl` file lookup): v1's shading is solid base-color, so
//! the OBJ-side material data wouldn't be used anyway.
//!
//! The OBJ format commonly lacks normals (especially for hand-
//! authored files like the `draw` example's `live_draw.obj`).
//! When that happens we synthesise flat per-face normals via
//! [`crate::rgp::glb_loader::derive_flat_normals`].

use std::io::Cursor;

use thiserror::Error;

use crate::rgp::asset::{CpuAsset, CpuMaterial, CpuMesh};
use crate::rgp::glb_loader::derive_flat_normals;

/// Errors from [`load_obj`].
#[derive(Debug, Error)]
pub enum ObjLoadError {
    /// `tobj` failed to parse the bytes as OBJ.
    #[error("obj parse error: {0}")]
    Parse(#[from] tobj::LoadError),
    /// File parsed but had no models.
    #[error("no models in obj")]
    NoModels,
    /// First model had no positions.
    #[error("first model has no positions")]
    NoPositions,
}

/// Parse `.obj` bytes into a [`CpuAsset`].
pub fn load_obj(bytes: &[u8]) -> Result<CpuAsset, ObjLoadError> {
    let options = tobj::LoadOptions {
        single_index: true,
        triangulate: true,
        ignore_points: true,
        ignore_lines: true,
        ..Default::default()
    };
    let mut cursor = Cursor::new(bytes);
    // No `.mtl` resolution: the loader callback is invoked when the
    // OBJ references `mtllib`, and v1 just returns an empty material
    // list. Solid-color shading on the renderer side means we don't
    // miss anything by skipping the material file. `Default::default()`
    // resolves to `tobj`'s internal `HashMap<String, usize>` (an
    // `ahash::AHashMap` alias) via the closure's return type.
    let (models, _materials) = tobj::load_obj_buf(&mut cursor, &options, |_path| {
        Ok((Vec::new(), Default::default()))
    })?;

    let model = models.into_iter().next().ok_or(ObjLoadError::NoModels)?;
    let mesh = model.mesh;

    if mesh.positions.is_empty() {
        return Err(ObjLoadError::NoPositions);
    }
    // tobj packs positions as flat `Vec<f32>` of xyz triples.
    let mut positions: Vec<[f32; 3]> = mesh
        .positions
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    // Normalize to a unit-extent box centered at the origin, matching
    // ratty's reference OBJ loader (ratty/src/model.rs build_meshes).
    // The renderer's `fit_half` scale assumes a roughly unit-cube model
    // (see CpuMesh::unit_cube in [-0.5, 0.5]); hand-authored OBJs use
    // whatever world units the author chose, so fold the bbox fit into
    // load time rather than the render pipeline. GLB is left as-is for
    // parity with ratty, which trusts glTF assets to come pre-scaled.
    normalize_to_unit_extent(&mut positions);
    let uvs: Vec<[f32; 2]> = mesh
        .texcoords
        .chunks_exact(2)
        .map(|c| [c[0], c[1]])
        .collect();
    let indices: Vec<u32> = if mesh.indices.is_empty() {
        (0..u32::try_from(positions.len()).unwrap_or(u32::MAX)).collect()
    } else {
        mesh.indices
    };
    let normals: Vec<[f32; 3]> = if mesh.normals.is_empty() {
        derive_flat_normals(&positions, &indices)
    } else {
        mesh.normals
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect()
    };

    Ok(CpuAsset {
        mesh: CpuMesh {
            positions,
            normals,
            uvs,
            indices,
        },
        // OBJ materials defer to the renderer's solid-color default.
        material: CpuMaterial::default(),
    })
}

fn normalize_to_unit_extent(positions: &mut [[f32; 3]]) {
    if positions.is_empty() {
        return;
    }
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in positions.iter() {
        for axis in 0..3 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    let center = [
        (min[0] + max[0]) * 0.5,
        (min[1] + max[1]) * 0.5,
        (min[2] + max[2]) * 0.5,
    ];
    let extent = [max[0] - min[0], max[1] - min[1], max[2] - min[2]];
    let max_extent = extent[0].max(extent[1]).max(extent[2]).max(1e-6);
    for p in positions.iter_mut() {
        p[0] = (p[0] - center[0]) / max_extent;
        p[1] = (p[1] - center[1]) / max_extent;
        p[2] = (p[2] - center[2]) / max_extent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_bytes_are_rejected_somehow() {
        let err = load_obj(b"not an obj file at all").unwrap_err();
        // tobj is quite permissive — anything that isn't a parse
        // error becomes a default empty model. Accept any of the
        // negative outcomes: parse failed, no models, no positions.
        assert!(
            matches!(
                err,
                ObjLoadError::Parse(_) | ObjLoadError::NoModels | ObjLoadError::NoPositions
            ),
            "got {err:?}"
        );
    }

    #[test]
    fn multi_square_obj_normalizes_to_unit_extent() {
        // Two adjacent unit squares in XY, bbox x:[0,2], y:[-1,0].
        // After normalization, max_extent = 2, so positions land in
        // [-0.5, 0.5] on the long axis and the squares stay side-by-side.
        let src = b"\
            v 0 0 0\n\
            v 1 0 0\n\
            v 1 -1 0\n\
            v 0 -1 0\n\
            f 1 2 3\n\
            f 1 3 4\n\
            v 1 0 0\n\
            v 2 0 0\n\
            v 2 -1 0\n\
            v 1 -1 0\n\
            f 5 6 7\n\
            f 5 7 8\n\
        ";
        let asset = load_obj(src).expect("loads");
        // Bbox is recentered: original center (1, -0.5, 0) → origin.
        // Original x range [0, 2] → normalized [-0.5, 0.5].
        // Original y range [-1, 0] → normalized [-0.25, 0.25].
        let mut min = [f32::INFINITY; 3];
        let mut max = [f32::NEG_INFINITY; 3];
        for p in &asset.mesh.positions {
            for axis in 0..3 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
        assert!((min[0] - -0.5).abs() < 1e-5, "min x = {}", min[0]);
        assert!((max[0] - 0.5).abs() < 1e-5, "max x = {}", max[0]);
        assert!((min[1] - -0.25).abs() < 1e-5, "min y = {}", min[1]);
        assert!((max[1] - 0.25).abs() < 1e-5, "max y = {}", max[1]);
    }

    #[test]
    fn minimal_triangle_obj_loads_with_derived_normals() {
        // 3 vertices, one triangle face.
        let src = b"\
            o tri\n\
            v 0.0 0.0 0.0\n\
            v 1.0 0.0 0.0\n\
            v 0.0 1.0 0.0\n\
            f 1 2 3\n\
        ";
        let asset = load_obj(src).expect("loads");
        assert_eq!(asset.mesh.positions.len(), 3);
        assert_eq!(asset.mesh.normals.len(), 3);
        // Triangle in XY plane facing +Z → derived normals = +Z.
        for n in &asset.mesh.normals {
            assert!((n[2] - 1.0).abs() < 1e-4, "expected +Z, got {n:?}");
        }
    }
}
