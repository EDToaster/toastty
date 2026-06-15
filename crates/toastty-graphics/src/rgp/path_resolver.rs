//! `path=` resolver for the RGP `r` verb.
//!
//! v1 policy (relaxed from the original decision §1 leaf-only rule,
//! see `docs/decisions/rgp-protocol.md`):
//!
//! 1. If `name` is a pure leaf and matches an entry in the embedded
//!    bundle (`cube`), return it.
//! 2. Otherwise treat `name` as a filesystem path — relative paths
//!    resolve from the process CWD; absolute paths read directly.
//!    Read the bytes and parse via [`crate::rgp::glb_loader::load_glb`].
//!
//! The original B′ policy (leaf-only against bundle + sandboxed
//! `asset_dir`) was rejected because real Ratty apps universally
//! emit paths like `assets/objects/SpinyMouse.glb`, and the
//! strict policy left every existing demo broken. Hardening is
//! tracked as a v2 follow-up — see decision §1 "Open questions".

use std::path::Path;

use thiserror::Error;

use crate::rgp::asset::CpuAsset;
use crate::rgp::glb_loader::{GlbLoadError, load_glb};
use crate::rgp::obj_loader::{ObjLoadError, load_obj};
use crate::rgp::operation::RgpFormat;

/// Errors from [`resolve`].
#[derive(Debug, Error)]
pub enum ResolveError {
    /// `name` was empty.
    #[error("empty asset name")]
    Empty,
    /// I/O error reading from disk (file missing, permission denied,
    /// etc.).
    #[error("read failed for `{0}`: {1}")]
    Io(String, std::io::Error),
    /// Bytes loaded but failed glTF parse.
    #[error("glb decode failed for `{0}`: {1}")]
    DecodeGlb(String, GlbLoadError),
    /// Bytes loaded but failed OBJ parse.
    #[error("obj decode failed for `{0}`: {1}")]
    DecodeObj(String, ObjLoadError),
}

/// Resolve a `path=` value to a [`CpuAsset`].
///
/// `format` selects the parser when the bytes come from disk:
/// `Glb` → `gltf` crate, `Obj` → `tobj`. The embedded bundle short-
/// circuits format dispatch (assets are pre-parsed). `_asset_dir`
/// is reserved for the v2 sandboxed-resolver design.
pub fn resolve(
    name: &str,
    format: RgpFormat,
    _asset_dir: Option<&Path>,
) -> Result<CpuAsset, ResolveError> {
    if name.is_empty() {
        return Err(ResolveError::Empty);
    }
    // Embedded bundle: only consult when `name` is a pure leaf.
    // A path-like input (containing a separator) means the app is
    // addressing the filesystem, not the bundle.
    if !name.contains('/')
        && !name.contains('\\')
        && let Some(asset) = embedded_lookup(name)
    {
        return Ok(asset);
    }
    let bytes =
        std::fs::read(Path::new(name)).map_err(|e| ResolveError::Io(name.to_string(), e))?;
    match format {
        RgpFormat::Glb => {
            load_glb(&bytes).map_err(|e| ResolveError::DecodeGlb(name.to_string(), e))
        }
        RgpFormat::Obj => {
            load_obj(&bytes).map_err(|e| ResolveError::DecodeObj(name.to_string(), e))
        }
    }
}

/// The embedded asset bundle. v1: a single name, `cube`, returns
/// the procedural unit cube.
fn embedded_lookup(name: &str) -> Option<CpuAsset> {
    match name {
        "cube" => Some(CpuAsset::unit_cube()),
        _ => None,
    }
}

/// Iterator of every leaf name in the embedded bundle. Exposed for
/// diagnostics + tests.
#[must_use]
pub fn embedded_bundle_names() -> &'static [&'static str] {
    &["cube"]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cube_resolves_from_bundle() {
        let a = resolve("cube", RgpFormat::Glb, None).expect("cube is bundled");
        assert_eq!(a.mesh.positions.len(), 24);
    }

    #[test]
    fn empty_name_rejected() {
        let e = resolve("", RgpFormat::Glb, None).unwrap_err();
        assert!(matches!(e, ResolveError::Empty));
    }

    #[test]
    fn missing_path_returns_io_error() {
        let e = resolve("definitely/not/here.glb", RgpFormat::Glb, None).unwrap_err();
        assert!(matches!(e, ResolveError::Io(_, _)), "got {e:?}");
    }

    #[test]
    fn bundle_names_includes_cube() {
        assert!(embedded_bundle_names().contains(&"cube"));
    }
}
