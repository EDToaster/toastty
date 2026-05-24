//! Leaf-name `path=` resolver for the RGP `r` verb.
//!
//! Policy (per `docs/decisions/rgp-protocol.md` §1, "B′"):
//!
//! 1. Reject anything containing `/`, `\`, `..`, or starting with
//!    `.`. The `name` must be a pure leaf.
//! 2. Look up in the embedded bundle first. v1 ships one entry —
//!    the procedural cube under name `cube`.
//! 3. If not found and an `asset_dir` is configured, join the leaf
//!    name onto it, canonicalize, and assert the canonical path is
//!    still inside `asset_dir` (defence against symlinks). If yes,
//!    read the file and parse via [`crate::rgp::glb_loader::load_glb`].
//! 4. Otherwise: not found.
//!
//! The resolver never reads files outside `asset_dir`. It never
//! calls `std::fs` if `asset_dir` is `None`.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::rgp::asset::CpuAsset;
use crate::rgp::glb_loader::{GlbLoadError, load_glb};

/// Errors from [`resolve`].
#[derive(Debug, Error)]
pub enum ResolveError {
    /// Name contains a separator, parent component, or starts with
    /// `.`. We treat all of these as "the app is trying to address
    /// something other than a bundle leaf"; reject up-front.
    #[error("name `{0}` is not a leaf identifier")]
    NotALeaf(String),
    /// Name didn't resolve to a bundled asset and there's no
    /// `asset_dir` configured (or the file wasn't found inside it).
    #[error("no asset named `{0}` in bundle or asset_dir")]
    NotFound(String),
    /// `asset_dir` was set but the canonicalized target escaped it
    /// (e.g. via a symlink). Treated as a hard reject, not a
    /// silent fallback to `NotFound` — the app explicitly asked for
    /// something we're refusing.
    #[error("`{0}` resolves outside the configured asset_dir")]
    OutsideAssetDir(String),
    /// I/O error reading from `asset_dir`.
    #[error("read failed for `{0}`: {1}")]
    Io(String, std::io::Error),
    /// Bytes loaded but failed glTF parse.
    #[error("glb decode failed for `{0}`: {1}")]
    Decode(String, GlbLoadError),
}

/// Validate that `name` is a pure leaf (no separators, no `..`,
/// not hidden). Returns the name back unchanged on success so
/// callers can pipe through `resolve(validate_leaf(n)?, dir)`.
fn validate_leaf(name: &str) -> Result<&str, ResolveError> {
    if name.is_empty()
        || name.starts_with('.')
        || name.contains('/')
        || name.contains('\\')
        || name.contains("..")
    {
        return Err(ResolveError::NotALeaf(name.to_string()));
    }
    Ok(name)
}

/// Resolve a leaf name to a [`CpuAsset`].
///
/// `asset_dir` is the user-configured `[rgp] asset_dir` (a canonical
/// path the caller has already canonicalized once). When `None`, only
/// the embedded bundle is searched.
pub fn resolve(name: &str, asset_dir: Option<&Path>) -> Result<CpuAsset, ResolveError> {
    let leaf = validate_leaf(name)?;
    if let Some(asset) = embedded_lookup(leaf) {
        return Ok(asset);
    }
    let Some(root) = asset_dir else {
        return Err(ResolveError::NotFound(leaf.to_string()));
    };

    let candidate: PathBuf = root.join(leaf);
    let canonical = candidate
        .canonicalize()
        .map_err(|e| ResolveError::Io(leaf.to_string(), e))?;
    let root_canonical = root
        .canonicalize()
        .map_err(|e| ResolveError::Io(leaf.to_string(), e))?;
    if !canonical.starts_with(&root_canonical) {
        return Err(ResolveError::OutsideAssetDir(leaf.to_string()));
    }
    let bytes =
        std::fs::read(&canonical).map_err(|e| ResolveError::Io(leaf.to_string(), e))?;
    load_glb(&bytes).map_err(|e| ResolveError::Decode(leaf.to_string(), e))
}

/// The embedded asset bundle. v1: a single name, `cube`, returns the
/// procedural unit cube.
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
        let a = resolve("cube", None).expect("cube is bundled");
        assert_eq!(a.mesh.positions.len(), 24);
    }

    #[test]
    fn empty_name_rejected() {
        let e = resolve("", None).unwrap_err();
        assert!(matches!(e, ResolveError::NotALeaf(_)));
    }

    #[test]
    fn names_with_separator_rejected() {
        for bad in ["a/b", "a\\b", "../etc/passwd", "./hidden"] {
            let e = resolve(bad, None).unwrap_err();
            assert!(
                matches!(e, ResolveError::NotALeaf(_)),
                "expected NotALeaf for `{bad}`, got {e:?}"
            );
        }
    }

    #[test]
    fn hidden_files_rejected() {
        let e = resolve(".ssh", None).unwrap_err();
        assert!(matches!(e, ResolveError::NotALeaf(_)));
    }

    #[test]
    fn parent_component_rejected_even_without_separator() {
        let e = resolve("..foo", None).unwrap_err();
        assert!(matches!(e, ResolveError::NotALeaf(_)));
    }

    #[test]
    fn unknown_name_with_no_asset_dir_is_not_found() {
        let e = resolve("nothere", None).unwrap_err();
        assert!(matches!(e, ResolveError::NotFound(_)));
    }

    #[test]
    fn bundle_names_includes_cube() {
        assert!(embedded_bundle_names().contains(&"cube"));
    }
}
