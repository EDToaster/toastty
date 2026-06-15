//! Ball-and-stick geometry baking.
//!
//! Turns a [`Molecule`] into renderable, CPK-tinted meshes. Because
//! RGP has **no instancing** and renders **one base color per asset**,
//! we bake geometry and group it by element:
//!
//! - one [`ColoredMesh`] per distinct element — every atom of that
//!   element baked as a sphere at its position, sized from the
//!   covalent radius (scaled down for the ball-and-stick look, e.g.
//!   0.25–0.4×), tinted with the element's CPK color;
//! - one [`ColoredMesh`] for **all bonds** — each bond a cylinder
//!   between the two atom centers (a neutral grey, e.g. `[200,200,200]`).
//!   Double/triple bonds may render as a single cylinder in v1.
//!
//! ## Global normalization (critical)
//!
//! toastty's GLB loader does NOT auto-normalize; the renderer fits a
//! roughly unit-extent model to the placement cell box. Compute ONE
//! global bounding box over the whole molecule (all atom spheres),
//! then center it at the origin and scale so the longest axis spans
//! `[-0.5, 0.5]`. Apply that SAME transform to every sub-mesh so the
//! per-element objects stay aligned when placed at the same anchor.
//!
//! Right-handed, +Y up (matches the renderer). Each sphere/cylinder
//! must carry correct outward normals (lambertian shading).

use std::collections::BTreeMap;
use std::f32::consts::{PI, TAU};

use crate::elements::{covalent_radius, cpk_color};
use crate::model::{ColoredMesh, Mesh, Molecule};

// ── Primitive builders ────────────────────────────────────────────────────────

/// Build a UV sphere of given `radius` centered at `center` with `rings`
/// latitude bands and `sectors` longitude sectors.
///
/// Normals are outward unit vectors (= position on unit sphere).
// Indices are small mesh-quality constants (≤ hundreds); precision loss is
// not a concern here.
#[allow(clippy::cast_precision_loss)]
fn build_uv_sphere(
    center: [f32; 3],
    radius: f32,
    rings: usize,
    sectors: usize,
    out_positions: &mut Vec<[f32; 3]>,
    out_normals: &mut Vec<[f32; 3]>,
    out_indices: &mut Vec<u32>,
) {
    let base = u32::try_from(out_positions.len()).expect("vertex count overflow");

    // rings = number of horizontal bands (between north and south poles)
    // vertices per ring = sectors+1 (seam duplicated for UV continuity)
    // +2 for the pole vertices
    for r in 0..=rings {
        let phi = PI * (r as f32) / (rings as f32); // 0 (top) → π (bottom)
        let sin_phi = phi.sin();
        let cos_phi = phi.cos();

        for s in 0..=sectors {
            let theta = TAU * (s as f32) / (sectors as f32); // 0 → 2π
            let sin_theta = theta.sin();
            let cos_theta = theta.cos();

            // Unit sphere normal
            let nx = sin_phi * cos_theta;
            let ny = cos_phi;
            let nz = sin_phi * sin_theta;

            out_positions.push([
                center[0] + radius * nx,
                center[1] + radius * ny,
                center[2] + radius * nz,
            ]);
            out_normals.push([nx, ny, nz]);
        }
    }

    let stride = u32::try_from(sectors + 1).expect("sectors overflow");
    for r in 0..(rings as u32) {
        for s in 0..(sectors as u32) {
            let a = base + r * stride + s;
            let b = base + r * stride + s + 1;
            let c = base + (r + 1) * stride + s;
            let d = base + (r + 1) * stride + s + 1;
            // Two triangles per quad, CCW winding (outward normals)
            out_indices.extend_from_slice(&[a, c, b]);
            out_indices.extend_from_slice(&[b, c, d]);
        }
    }
}

/// Build a cylinder from `p0` to `p1` with the given `radius`.
/// The cylinder has outward radial normals on the tube (flat caps
/// are omitted for v1 — the blend at joints is acceptable).
// Indices are small mesh-quality constants (≤ hundreds); precision loss is
// not a concern here.
#[allow(clippy::cast_precision_loss)]
fn build_cylinder(
    p0: [f32; 3],
    p1: [f32; 3],
    radius: f32,
    sectors: usize,
    out_positions: &mut Vec<[f32; 3]>,
    out_normals: &mut Vec<[f32; 3]>,
    out_indices: &mut Vec<u32>,
) {
    let base = u32::try_from(out_positions.len()).expect("vertex count overflow");

    // Axis vector
    let ax = p1[0] - p0[0];
    let ay = p1[1] - p0[1];
    let az = p1[2] - p0[2];
    let len = (ax * ax + ay * ay + az * az).sqrt();
    if len < 1e-10 {
        return; // degenerate bond — skip
    }

    // Unit axis
    let ux = ax / len;
    let uy = ay / len;
    let uz = az / len;

    // Build a perpendicular frame (right, up = ux,uy,uz treated as forward).
    // Pick a vector not parallel to (ux,uy,uz).
    let (rx, ry, rz) = {
        let (tx, ty, tz) = if ux.abs() < 0.9 {
            (1.0f32, 0.0, 0.0)
        } else {
            (0.0f32, 1.0, 0.0)
        };
        // right = forward × t  (normalized)
        let cx = uy * tz - uz * ty;
        let cy = uz * tx - ux * tz;
        let cz = ux * ty - uy * tx;
        let l = (cx * cx + cy * cy + cz * cz).sqrt();
        (cx / l, cy / l, cz / l)
    };

    // sx,sy,sz = second perpendicular = axis × right
    let (sx, sy, sz) = {
        let cx = uy * rz - uz * ry;
        let cy = uz * rx - ux * rz;
        let cz = ux * ry - uy * rx;
        let l = (cx * cx + cy * cy + cz * cz).sqrt();
        (cx / l, cy / l, cz / l)
    };

    // Emit two rings (bottom = p0, top = p1), sectors+1 verts each.
    for ring in 0..2usize {
        let center = if ring == 0 { p0 } else { p1 };
        for s in 0..=sectors {
            let theta = TAU * (s as f32) / (sectors as f32);
            let cos_t = theta.cos();
            let sin_t = theta.sin();

            // Radial outward normal (in the plane perpendicular to axis)
            let nx = cos_t * rx + sin_t * sx;
            let ny = cos_t * ry + sin_t * sy;
            let nz = cos_t * rz + sin_t * sz;

            out_positions.push([
                center[0] + radius * nx,
                center[1] + radius * ny,
                center[2] + radius * nz,
            ]);
            out_normals.push([nx, ny, nz]);
        }
    }

    let stride = u32::try_from(sectors + 1).expect("sectors overflow");
    for s in 0..(sectors as u32) {
        // bottom ring = base + s, top ring = base + stride + s
        let b0 = base + s;
        let b1 = base + s + 1;
        let t0 = base + stride + s;
        let t1 = base + stride + s + 1;
        // Two triangles, CCW when viewed from outside
        out_indices.extend_from_slice(&[b0, t0, b1]);
        out_indices.extend_from_slice(&[b1, t0, t1]);
    }
}

// ── Global normalization ──────────────────────────────────────────────────────

/// Normalize all positions across all meshes so the whole molecule fits
/// `[-0.5, 0.5]` on its longest axis, centered at origin.
/// Normals are left as-is (translation+uniform-scale preserves unit
/// lengths and directions).
fn global_normalize(meshes: &mut [Mesh]) {
    // First pass: bounding box over all vertices.
    let mut mn = [f32::INFINITY; 3];
    let mut mx = [f32::NEG_INFINITY; 3];
    let mut any = false;
    for mesh in meshes.iter() {
        for p in &mesh.positions {
            any = true;
            for i in 0..3 {
                if p[i] < mn[i] {
                    mn[i] = p[i];
                }
                if p[i] > mx[i] {
                    mx[i] = p[i];
                }
            }
        }
    }
    if !any {
        return;
    }

    let center = [
        (mn[0] + mx[0]) * 0.5,
        (mn[1] + mx[1]) * 0.5,
        (mn[2] + mx[2]) * 0.5,
    ];
    let extent = [mx[0] - mn[0], mx[1] - mn[1], mx[2] - mn[2]];
    let max_extent = extent[0].max(extent[1]).max(extent[2]);

    // Avoid division by zero for a degenerate (point) molecule.
    let scale = if max_extent > 1e-10 {
        1.0 / max_extent
    } else {
        1.0
    };

    // Second pass: apply transform.
    for mesh in meshes.iter_mut() {
        for p in &mut mesh.positions {
            p[0] = (p[0] - center[0]) * scale;
            p[1] = (p[1] - center[1]) * scale;
            p[2] = (p[2] - center[2]) * scale;
        }
        // Normals: uniform scale preserves direction and unit length.
        // No update needed.
    }
}

// ── Main entry point ──────────────────────────────────────────────────────────

/// Build CPK-tinted ball-and-stick meshes for a molecule.
///
/// Returns one `ColoredMesh` per element (spheres) plus one for bonds
/// (cylinders), all sharing a single global normalization into
/// `[-0.5, 0.5]`. Element groups come out in sorted symbol order so
/// asset ids are deterministic across calls.
pub fn build(mol: &Molecule) -> Vec<ColoredMesh> {
    // Sphere quality: 12 rings × 16 sectors ≈ 192 triangles per atom.
    const SPHERE_RINGS: usize = 12;
    const SPHERE_SECTORS: usize = 16;
    const SPHERE_SCALE: f32 = 0.3; // covalent radius multiplier (ball-and-stick)

    // Cylinder quality: 12 sectors; radius in angstroms.
    const CYL_SECTORS: usize = 12;
    const CYL_RADIUS: f32 = 0.08; // angstroms (pre-normalization)

    // ── Atom spheres grouped by element symbol ────────────────────────
    // BTreeMap gives stable insertion-order iteration (sorted by key).
    let mut element_meshes: BTreeMap<String, Mesh> = BTreeMap::new();

    for atom in &mol.atoms {
        let radius = covalent_radius(&atom.symbol) * SPHERE_SCALE;
        let entry = element_meshes.entry(atom.symbol.clone()).or_default();
        build_uv_sphere(
            atom.pos,
            radius,
            SPHERE_RINGS,
            SPHERE_SECTORS,
            &mut entry.positions,
            &mut entry.normals,
            &mut entry.indices,
        );
    }

    // ── Bond cylinders ────────────────────────────────────────────────
    let mut bond_mesh = Mesh::default();
    for bond in &mol.bonds {
        let p0 = mol.atoms[bond.a].pos;
        let p1 = mol.atoms[bond.b].pos;
        build_cylinder(
            p0,
            p1,
            CYL_RADIUS,
            CYL_SECTORS,
            &mut bond_mesh.positions,
            &mut bond_mesh.normals,
            &mut bond_mesh.indices,
        );
    }

    // ── Assemble all meshes for global normalization ──────────────────
    // Order: element meshes (sorted by symbol) + bond mesh last.
    let mut all_meshes: Vec<(String, [u8; 3], Mesh)> = element_meshes
        .into_iter()
        .map(|(sym, mesh)| {
            let color = cpk_color(&sym);
            (sym, color, mesh)
        })
        .collect();
    // Append bond mesh (if any bonds).
    let has_bonds = !bond_mesh.positions.is_empty();
    if has_bonds {
        all_meshes.push(("bonds".to_string(), [200, 200, 200], bond_mesh));
    }

    // Extract just the meshes for normalization.
    let mut meshes: Vec<Mesh> = all_meshes.iter().map(|(_, _, m)| m.clone()).collect();
    global_normalize(&mut meshes);

    // ── Build output ──────────────────────────────────────────────────
    all_meshes
        .into_iter()
        .zip(meshes)
        .map(|((label, color, _), mesh)| ColoredMesh { color, label, mesh })
        .collect()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Atom, Bond, Molecule};

    /// Build a two-atom molecule (C and O with a single bond) and verify
    /// the geometry and GLB round-trip.
    ///
    /// NOTE: this test calls `elements::cpk_color` and
    /// `elements::covalent_radius`, which are `todo!()` until the peer
    /// fills the table. The test *compiles* and the GLB subtests below do
    /// NOT depend on elements — they use hand-crafted `Mesh` values.
    #[allow(dead_code)] // called via the test runner once elements land
    fn make_co_molecule() -> Molecule {
        Molecule {
            atoms: vec![
                Atom {
                    symbol: "C".to_string(),
                    pos: [-0.6, 0.0, 0.0],
                },
                Atom {
                    symbol: "O".to_string(),
                    pos: [0.6, 0.0, 0.0],
                },
            ],
            bonds: vec![Bond {
                a: 0,
                b: 1,
                order: 1,
            }],
        }
    }

    // ── GLB round-trip tests (do NOT call elements — always run) ──────

    fn unit_cube_mesh() -> Mesh {
        // Simple tetrahedron-like mesh: 4 vertices, 4 triangles.
        Mesh {
            positions: vec![
                [0.0, 0.0, 0.0],
                [1.0, 0.0, 0.0],
                [0.5, 1.0, 0.0],
                [0.5, 0.5, 1.0],
            ],
            normals: vec![
                [-0.577, -0.577, -0.577],
                [0.577, -0.577, -0.577],
                [0.0, 0.577, -0.577],
                [0.0, 0.0, 1.0],
            ],
            indices: vec![0, 1, 2, 0, 1, 3, 1, 2, 3, 0, 3, 2],
        }
    }

    #[test]
    fn glb_roundtrip_positions_count() {
        let mesh = unit_cube_mesh();
        let bytes = crate::glb::write(&mesh);
        let asset =
            toastty_graphics::rgp::glb_loader::load_glb(&bytes).expect("load_glb must succeed");
        assert_eq!(
            asset.mesh.positions.len(),
            mesh.positions.len(),
            "position count must survive GLB round-trip"
        );
        assert_eq!(
            asset.mesh.indices.len() % 3,
            0,
            "indices must be a multiple of 3"
        );
    }

    #[test]
    fn glb_roundtrip_normals_unit_length() {
        let mesh = unit_cube_mesh();
        // Normalize normals to true unit length first.
        let mut mesh = mesh;
        for n in &mut mesh.normals {
            let l = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            n[0] /= l;
            n[1] /= l;
            n[2] /= l;
        }
        let bytes = crate::glb::write(&mesh);
        let asset = toastty_graphics::rgp::glb_loader::load_glb(&bytes).unwrap();
        for n in &asset.mesh.normals {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!(
                (len - 1.0).abs() < 1e-3,
                "normal not unit length: {n:?} (len={len})"
            );
        }
    }

    // ── Normalization tests (do NOT call elements) ────────────────────

    #[test]
    fn global_normalize_fits_unit_box() {
        let mut meshes = vec![Mesh {
            positions: vec![[-5.0, -3.0, 0.0], [5.0, 3.0, 0.0], [0.0, 0.0, 2.0]],
            normals: vec![[1.0, 0.0, 0.0]; 3],
            indices: vec![0, 1, 2],
        }];
        global_normalize(&mut meshes);
        let eps = 1e-5_f32;
        for p in &meshes[0].positions {
            for &coord in p {
                assert!(
                    coord >= -0.5 - eps && coord <= 0.5 + eps,
                    "position {p:?} out of [-0.5, 0.5]: coord={coord}"
                );
            }
        }
    }

    #[test]
    fn uv_sphere_indices_multiple_of_3() {
        let mut pos = Vec::new();
        let mut nor = Vec::new();
        let mut idx = Vec::new();
        build_uv_sphere([0.0, 0.0, 0.0], 1.0, 8, 12, &mut pos, &mut nor, &mut idx);
        assert_eq!(idx.len() % 3, 0, "sphere indices must be multiple of 3");
        assert_eq!(pos.len(), nor.len(), "sphere pos/nor parallel");
    }

    #[test]
    fn uv_sphere_normals_unit_length() {
        let mut pos = Vec::new();
        let mut nor = Vec::new();
        let mut idx = Vec::new();
        build_uv_sphere([0.0, 0.0, 0.0], 1.0, 8, 12, &mut pos, &mut nor, &mut idx);
        for n in &nor {
            let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
            assert!((len - 1.0).abs() < 1e-5, "sphere normal not unit: {n:?}");
        }
    }

    #[test]
    fn cylinder_indices_multiple_of_3() {
        let mut pos = Vec::new();
        let mut nor = Vec::new();
        let mut idx = Vec::new();
        build_cylinder(
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            0.1,
            12,
            &mut pos,
            &mut nor,
            &mut idx,
        );
        assert_eq!(idx.len() % 3, 0, "cylinder indices must be multiple of 3");
    }
}
