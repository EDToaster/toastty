//! Minimal glTF-binary (`.glb`) writer for a single mesh.
//!
//! Mirror the byte layout of toastty's
//! `toastty_graphics::rgp::glb_loader::minimal_triangle_glb` so output
//! round-trips through toastty's own `load_glb`:
//!
//! - 12-byte header: magic `b"glTF"`, version `2u32` (LE), total length.
//! - JSON chunk: 4-byte length (LE) + `b"JSON"` + UTF-8 glTF JSON,
//!   padded with spaces (`0x20`) to a 4-byte boundary.
//! - BIN chunk: 4-byte length (LE) + `b"BIN\0"` + binary blob, padded
//!   with `0x00` to a 4-byte boundary.
//!
//! glTF JSON must declare: one buffer (the BIN blob), bufferViews +
//! accessors for `POSITION` (VEC3, F32, componentType 5126, with
//! `min`/`max`) and `NORMAL` (VEC3, F32), plus an indices accessor
//! (SCALAR, `UNSIGNED_INT` = componentType 5125). One mesh, one
//! primitive (`mode` 4 = triangles), one node, one scene.
//!
//! Note: toastty's loader reads only the FIRST primitive of the FIRST
//! mesh; `baseColorFactor` is read but we tint via the RGP `color=`
//! field instead, so a material is optional (default white is fine).

use crate::model::Mesh;

/// Serialize a mesh as a valid single-mesh, single-primitive `.glb`
/// with `POSITION` + `NORMAL` attributes and `UNSIGNED_INT` indices.
///
/// Panics if the mesh has positions but no normals, or if normals
/// length != positions length. Empty meshes (no positions) are
/// supported and produce a valid but empty GLB (no primitives).
// The function is long because it has to assemble all glTF JSON fields
// plus the binary buffer in one place; splitting it would add indirection
// without clarifying intent.
#[allow(clippy::too_many_lines)]
#[must_use]
pub fn write(mesh: &Mesh) -> Vec<u8> {
    assert!(
        mesh.positions.len() == mesh.normals.len(),
        "positions and normals must have equal length"
    );
    assert!(
        mesh.indices.len().is_multiple_of(3),
        "indices length must be a multiple of 3"
    );

    let vertex_count = mesh.positions.len();
    let index_count = mesh.indices.len();

    // Build the binary buffer.
    // Layout: [positions (VEC3 F32)] [normals (VEC3 F32)] [indices (SCALAR U32)]
    // Each section padded to 4-byte alignment (they already are, since F32=4 and U32=4).

    let pos_byte_len = vertex_count * 3 * 4; // 3 f32 per vertex
    let nor_byte_len = vertex_count * 3 * 4; // 3 f32 per vertex
    let idx_byte_len = index_count * 4; // u32 per index

    let pos_byte_offset: usize = 0;
    let nor_byte_offset: usize = pos_byte_len;
    // Indices accessor must be 4-byte aligned; nor_byte_offset + nor_byte_len
    // is already aligned since both are multiples of 4.
    let idx_byte_offset: usize = nor_byte_offset + nor_byte_len;

    let total_bin_len = idx_byte_offset + idx_byte_len;

    let mut bin: Vec<u8> = Vec::with_capacity(total_bin_len);

    // Positions
    for p in &mesh.positions {
        for &f in p {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    // Normals
    for n in &mesh.normals {
        for &f in n {
            bin.extend_from_slice(&f.to_le_bytes());
        }
    }
    // Indices
    for &idx in &mesh.indices {
        bin.extend_from_slice(&idx.to_le_bytes());
    }

    debug_assert_eq!(bin.len(), total_bin_len);

    // Compute POSITION min/max for the accessor.
    let (pos_min, pos_max) = if vertex_count > 0 {
        let mut mn = mesh.positions[0];
        let mut mx = mesh.positions[0];
        for p in &mesh.positions[1..] {
            for i in 0..3 {
                if p[i] < mn[i] {
                    mn[i] = p[i];
                }
                if p[i] > mx[i] {
                    mx[i] = p[i];
                }
            }
        }
        (mn, mx)
    } else {
        ([0.0f32; 3], [0.0f32; 3])
    };

    // Build the JSON.
    // bufferView 0 → positions
    // bufferView 1 → normals
    // bufferView 2 → indices
    // accessor 0  → POSITION
    // accessor 1  → NORMAL
    // accessor 2  → indices (SCALAR UNSIGNED_INT)
    let json = format!(
        r#"{{
  "asset": {{ "version": "2.0" }},
  "scene": 0,
  "scenes": [{{ "nodes": [0] }}],
  "nodes": [{{ "mesh": 0 }}],
  "meshes": [{{
    "primitives": [{{
      "attributes": {{ "POSITION": 0, "NORMAL": 1 }},
      "indices": 2,
      "mode": 4
    }}]
  }}],
  "accessors": [
    {{
      "bufferView": 0,
      "byteOffset": 0,
      "componentType": 5126,
      "count": {vertex_count},
      "type": "VEC3",
      "min": [{pos_min0}, {pos_min1}, {pos_min2}],
      "max": [{pos_max0}, {pos_max1}, {pos_max2}]
    }},
    {{
      "bufferView": 1,
      "byteOffset": 0,
      "componentType": 5126,
      "count": {vertex_count},
      "type": "VEC3"
    }},
    {{
      "bufferView": 2,
      "byteOffset": 0,
      "componentType": 5125,
      "count": {index_count},
      "type": "SCALAR"
    }}
  ],
  "bufferViews": [
    {{
      "buffer": 0,
      "byteOffset": {pos_byte_offset},
      "byteLength": {pos_byte_len}
    }},
    {{
      "buffer": 0,
      "byteOffset": {nor_byte_offset},
      "byteLength": {nor_byte_len}
    }},
    {{
      "buffer": 0,
      "byteOffset": {idx_byte_offset},
      "byteLength": {idx_byte_len}
    }}
  ],
  "buffers": [{{ "byteLength": {total_bin_len} }}]
}}"#,
        vertex_count = vertex_count,
        index_count = index_count,
        pos_min0 = pos_min[0],
        pos_min1 = pos_min[1],
        pos_min2 = pos_min[2],
        pos_max0 = pos_max[0],
        pos_max1 = pos_max[1],
        pos_max2 = pos_max[2],
        pos_byte_offset = pos_byte_offset,
        pos_byte_len = pos_byte_len,
        nor_byte_offset = nor_byte_offset,
        nor_byte_len = nor_byte_len,
        idx_byte_offset = idx_byte_offset,
        idx_byte_len = idx_byte_len,
        total_bin_len = total_bin_len,
    );

    let mut json_padded = json.into_bytes();
    while !json_padded.len().is_multiple_of(4) {
        json_padded.push(b' ');
    }

    let mut bin_padded = bin;
    while !bin_padded.len().is_multiple_of(4) {
        bin_padded.push(0u8);
    }

    // Header: magic (4) + version (4) + total_length (4) = 12
    // JSON chunk header: length (4) + type (4) = 8
    // BIN chunk header: length (4) + type (4) = 8
    let total: u32 = 12
        + 8
        + u32::try_from(json_padded.len()).expect("json too large")
        + 8
        + u32::try_from(bin_padded.len()).expect("bin too large");

    let mut out = Vec::with_capacity(total as usize);
    out.extend_from_slice(b"glTF");
    out.extend_from_slice(&2u32.to_le_bytes());
    out.extend_from_slice(&total.to_le_bytes());
    // JSON chunk
    out.extend_from_slice(&u32::try_from(json_padded.len()).unwrap().to_le_bytes());
    out.extend_from_slice(b"JSON");
    out.extend_from_slice(&json_padded);
    // BIN chunk
    out.extend_from_slice(&u32::try_from(bin_padded.len()).unwrap().to_le_bytes());
    out.extend_from_slice(b"BIN\0");
    out.extend_from_slice(&bin_padded);

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Mesh;

    fn single_triangle_mesh() -> Mesh {
        Mesh {
            positions: vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0]],
            normals: vec![[0.0, 0.0, 1.0], [0.0, 0.0, 1.0], [0.0, 0.0, 1.0]],
            indices: vec![0, 1, 2],
        }
    }

    #[test]
    fn glb_write_single_triangle_parses() {
        let mesh = single_triangle_mesh();
        let bytes = write(&mesh);
        let asset =
            toastty_graphics::rgp::glb_loader::load_glb(&bytes).expect("should parse successfully");
        assert_eq!(asset.mesh.positions.len(), 3, "three vertices expected");
        assert_eq!(
            asset.mesh.indices.len() % 3,
            0,
            "indices must be a multiple of 3"
        );
    }

    #[test]
    fn glb_write_normals_are_unit_length() {
        let mesh = single_triangle_mesh();
        let bytes = write(&mesh);
        let asset = toastty_graphics::rgp::glb_loader::load_glb(&bytes).unwrap();
        for n in &asset.mesh.normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!(
                (len - 1.0).abs() < 1e-4,
                "normal not unit length: {n:?} (len={len})"
            );
        }
    }

    #[test]
    fn glb_write_magic_and_version() {
        let mesh = single_triangle_mesh();
        let bytes = write(&mesh);
        assert_eq!(&bytes[0..4], b"glTF", "magic mismatch");
        let version = u32::from_le_bytes(bytes[4..8].try_into().unwrap());
        assert_eq!(version, 2, "version must be 2");
        let total_len = u32::from_le_bytes(bytes[8..12].try_into().unwrap());
        assert_eq!(
            total_len as usize,
            bytes.len(),
            "total_length field must equal actual byte count"
        );
    }
}
