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
    let positions: Vec<[f32; 3]> = mesh
        .positions
        .chunks_exact(3)
        .map(|c| [c[0], c[1], c[2]])
        .collect();
    let normals: Vec<[f32; 3]> = if mesh.normals.is_empty() {
        derive_flat_normals(&positions)
    } else {
        mesh.normals
            .chunks_exact(3)
            .map(|c| [c[0], c[1], c[2]])
            .collect()
    };
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
