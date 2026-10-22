//! Text rendering — pure-fn submodules + the wgpu glue.
//!
//! Layout:
//!
//! - [`cluster_width`] — mode 2027 cluster snap (pure).
//! - [`atlas`] — shelf-pack glyph atlas (pure).
//! - [`decoration`] — SGR decoration geometry, faint colour, blink /
//!   conceal visibility (pure).
//! - [`instance`] — `CellInstance` + `build_instances` (pure).
//! - [`viewport`] — smooth-scroll state (pure).
//! - [`presentation`] — UTS #51 emoji-vs-text presentation selection.
//! - [`glyph_rasterizer`] — cosmic-text + swash + GPU atlas upload.
//! - [`row_shaper`] — per-frame row shaping + the row-shape cache.
//! - [`pipeline`] — wgpu pipeline that consumes `[CellInstance]`.
//!
//! See [`docs/decisions/text-stack.md`](../../../docs/decisions/text-stack.md).

pub mod atlas;
pub mod cluster_width;
pub mod decoration;
pub mod instance;
pub mod presentation;
pub mod viewport;

pub mod glyph_rasterizer;
pub mod pipeline;
pub mod row_shaper;
