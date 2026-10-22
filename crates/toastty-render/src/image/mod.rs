//! M11a image rendering pipeline (Kitty graphics).
//!
//! Three submodules:
//! - [`atlas`] — `ImageTextureCache`, the GPU-side texture-per-image
//!   LRU mirror of `ImageRegistry`.
//! - [`instance`] — `ImageInstance` GPU layout + `build_image_instances`.
//! - [`pipeline`] — `ImagePipeline`, the wgpu pipeline + bind group.
//! - `mip` — mip levels for images drawn downscaled (SIZE-12).

pub mod atlas;
pub mod instance;
mod mip;
pub mod pipeline;
pub mod rich;
