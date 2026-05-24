//! RGP 3D render pass.
//!
//! Owns the WGSL shader (`shaders/rgp.wgsl`), the matrix helpers
//! used to compose model matrices from the RGP protocol's
//! transform fields, the GPU mesh cache keyed by asset id, and
//! the pipeline that issues one indexed draw per placement.
//!
//! Composition with text + image happens via the shared
//! `Depth32Float` attachment owned by [`crate::Renderer`].

pub mod matrix;
pub mod mesh;
pub mod pipeline;

pub use mesh::{GpuAssetCache, GpuMesh, Vertex};
pub use pipeline::Rgp3dPipeline;
