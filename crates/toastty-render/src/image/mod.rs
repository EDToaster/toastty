//! M11a image rendering pipeline (Kitty graphics).
//!
//! Three submodules:
//! - [`atlas`] — `ImageTextureCache`, the GPU-side texture-per-image
//!   LRU mirror of `ImageRegistry`.
//! - [`instance`] — `ImageInstance` GPU layout + `build_image_instances`.
//! - [`pipeline`] — `ImagePipeline`, the wgpu pipeline + bind group.

pub mod atlas;
pub mod instance;
pub mod pipeline;
