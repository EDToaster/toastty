//! Validates every WGSL shader under `shaders/` at build time.
//!
//! Fails the build with a precise message if any shader has parse or
//! validation errors — so AI agents catch broken shaders at
//! `cargo check`, not at first run.

use std::path::Path;

fn main() {
    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR");
    let shaders_dir = Path::new(&manifest_dir).join("shaders");

    println!("cargo:rerun-if-changed=shaders");

    if !shaders_dir.is_dir() {
        return;
    }

    let entries = std::fs::read_dir(&shaders_dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", shaders_dir.display()));

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("wgsl") {
            continue;
        }

        let src = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));

        let module = match naga::front::wgsl::parse_str(&src) {
            Ok(m) => m,
            Err(e) => panic!("WGSL parse error in {}:\n{}", path.display(), e.emit_to_string(&src)),
        };

        let mut validator = naga::valid::Validator::new(
            naga::valid::ValidationFlags::all(),
            naga::valid::Capabilities::all(),
        );

        if let Err(e) = validator.validate(&module) {
            panic!("WGSL validation error in {}: {e:?}", path.display());
        }
    }
}
