//! Text rendering — pure-fn submodules + the wgpu glue.
//!
//! Layout:
//!
//! - [`cluster_width`] — mode 2027 cluster snap (pure).
//! - [`atlas`] — shelf-pack glyph atlas (pure).
//! - [`instance`] — `CellInstance` + `build_instances` (pure).
//! - [`viewport`] — smooth-scroll state (pure).
//! - [`presentation`] — UTS #51 emoji-vs-text presentation selection.
//! - [`glyph_rasterizer`] — cosmic-text + swash + GPU atlas upload.
//! - [`pipeline`] — wgpu pipeline that consumes `[CellInstance]`.
//!
//! See [`docs/decisions/text-stack.md`](../../../docs/decisions/text-stack.md).

pub mod atlas;
pub mod cluster_width;
pub mod instance;
pub mod presentation;
pub mod viewport;

pub mod glyph_rasterizer;
pub mod pipeline;
