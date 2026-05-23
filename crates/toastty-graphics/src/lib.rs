//! Inline graphics protocols: Kitty (primary), Sixel (fallback),
//! Ratty Graphics Protocol (experimental 3D).

#![forbid(unsafe_code)]

pub mod image_grid;
pub mod registry;

pub use image_grid::{ImageGrid, Placement, PlacementHandle, SrcRect};
pub use registry::{ImageData, ImageRegistry, InsertError, Inserted};
