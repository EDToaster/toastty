//! Protocol handler registry.
//!
//! One module per protocol; each implements small pure-function
//! encoders / decoders. See `docs/protocols.md` for the full support
//! matrix.
//!
//! M8 (synchronized output, grapheme clusters, in-band resize) lands
//! three new modules:
//! - [`synchronized`] (mode 2026)
//! - [`unicode_core`] (mode 2027)
//! - [`resize_inband`] (mode 2048)

#![forbid(unsafe_code)]

pub mod osc_cwd;
pub mod palette;
pub mod resize_inband;
pub mod semantic_prompt;
pub mod synchronized;
pub mod unicode_core;
