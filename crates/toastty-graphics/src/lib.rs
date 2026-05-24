//! Inline graphics protocols: Kitty (primary), Sixel (fallback),
//! Ratty Graphics Protocol (experimental 3D).

#![forbid(unsafe_code)]

pub mod image_grid;
pub mod kitty;
pub mod registry;
pub mod rgp;

pub use image_grid::{ImageGrid, Placement, PlacementHandle, SrcRect};
pub use kitty::{
    handler::{HandlerError, KittyHandler, KittySink},
    header::{Action, Compression, Format, Header, Quiet, Transmission},
    placeholder::{PLACEHOLDER, diacritic_to_index, is_diacritic, is_placeholder},
    reply::{ErrorCode, encode_error, encode_ok},
};
pub use registry::{ImageData, ImageRegistry, InsertError, Inserted};
pub use rgp::{
    asset::{CpuAsset, CpuMaterial, CpuMesh},
    glb_loader::{GlbLoadError, load_glb},
    handler::{RgpHandler, RgpSink},
    obj_loader::{ObjLoadError, load_obj},
    operation::{
        RGP_PREFIX, RgpAnchor, RgpFormat, RgpOperation, RgpParseError, RgpPlacementStyle,
        RgpPlacementUpdate, RgpRegisterSource, parse as parse_rgp,
    },
    path_resolver::{ResolveError, embedded_bundle_names, resolve as resolve_rgp_path},
    reply::{frame_apc as frame_rgp_apc, support_reply as rgp_support_reply},
    scene::{RgpAsset, RgpPlacement, RgpScene},
};
