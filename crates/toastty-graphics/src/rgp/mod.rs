//! Ratty Graphics Protocol implementation.
//!
//! Submodules:
//! - [`operation`] — wire-format types + buffered-payload parser.
//! - [`reply`] — capability-string encoder for the `s` (support
//!   query) verb.
//! - [`scene`] — in-memory snapshot of registered assets + live
//!   placements ([`scene::RgpScene`]). Concrete struct, `&self`
//!   accessors only — no trait, no Bevy backend (see
//!   `docs/decisions/rgp-protocol.md` §2).
//! - [`handler`] — stateful dispatcher with per-id chunked-payload
//!   reassembly that calls back into a [`handler::RgpSink`] (the
//!   host's interpretation of "register / place / update / delete /
//!   queue reply").
//! - [`asset`] — CPU-side mesh + material types ([`asset::CpuMesh`],
//!   [`asset::CpuMaterial`], [`asset::CpuAsset`]) and the bundled
//!   procedural cube ([`asset::CpuMesh::unit_cube`]).
//! - [`glb_loader`] — `.glb` byte slice → [`asset::CpuAsset`] via
//!   the `gltf` crate. Used by the payload-mode `r` verb when
//!   `fmt=glb`.
//! - [`obj_loader`] — `.obj` byte slice → [`asset::CpuAsset`] via
//!   the `tobj` crate. Used when `fmt=obj`. Materials (the
//!   referenced `.mtl` file) are not resolved — v1's renderer
//!   uses a solid base color.
//! - [`path_resolver`] — leaf-name `path=` lookup against the
//!   embedded bundle + optional `[rgp] asset_dir`. Decision §1's
//!   policy (now permissive, see the doc amendment).

pub mod asset;
pub mod glb_loader;
pub mod handler;
pub mod obj_loader;
pub mod operation;
pub mod path_resolver;
pub mod reply;
pub mod scene;
