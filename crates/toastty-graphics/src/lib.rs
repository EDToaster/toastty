//! Inline graphics protocols: Kitty (primary), Sixel (fallback),
//! Ratty Graphics Protocol (experimental 3D).

#![forbid(unsafe_code)]

pub mod image_grid;
pub mod kitty;
pub mod registry;

pub use image_grid::{ImageGrid, Placement, PlacementHandle, SrcRect};
pub use kitty::{
    handler::{HandlerError, KittyHandler, KittySink},
    header::{Action, Compression, Format, Header, Quiet, Transmission},
    placeholder::{PLACEHOLDER, diacritic_to_index, is_diacritic, is_placeholder},
    reply::{ErrorCode, encode_error, encode_ok},
};
pub use registry::{ImageData, ImageRegistry, InsertError, Inserted};
