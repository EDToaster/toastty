//! Shared data contract for the molecule viewer.
//!
//! No logic lives here — only the types that flow between modules, so
//! the independently-implemented modules agree on interfaces. Treat
//! these definitions as frozen: implement against them, don't change
//! them (flag any that seem wrong instead).

/// A parsed molecule: 3D atom positions + connectivity.
#[derive(Debug, Clone, PartialEq)]
pub struct Molecule {
    pub atoms: Vec<Atom>,
    pub bonds: Vec<Bond>,
}

/// One atom: element symbol (e.g. `"C"`, `"O"`, `"Cl"`) + 3D position
/// in angstroms (as given by the source SDF).
#[derive(Debug, Clone, PartialEq)]
pub struct Atom {
    pub symbol: String,
    pub pos: [f32; 3],
}

/// A bond between two atoms (indices into [`Molecule::atoms`]) with a
/// bond order (1 = single, 2 = double, 3 = triple, …).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bond {
    pub a: usize,
    pub b: usize,
    pub order: u8,
}

/// A triangle mesh: parallel position/normal arrays + triangle indices
/// (length a multiple of 3). Positions are object-local; `geometry`
/// normalizes the whole molecule before handing meshes to `glb`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Mesh {
    pub positions: Vec<[f32; 3]>,
    pub normals: Vec<[f32; 3]>,
    pub indices: Vec<u32>,
}

/// A mesh plus the CPK tint to render it with. `geometry::build`
/// produces one per element group (atoms-of-one-element as spheres),
/// plus one for all bonds. The RGP layer registers each as its own
/// object and places it with the matching `color=` — this is how we
/// get multi-color CPK rendering under toastty's one-color-per-asset
/// renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct ColoredMesh {
    /// CPK RGB tint applied via the RGP `color=` field.
    pub color: [u8; 3],
    /// Human label for the group (`"C"`, `"O"`, `"bonds"`) — diagnostics.
    pub label: String,
    pub mesh: Mesh,
}
