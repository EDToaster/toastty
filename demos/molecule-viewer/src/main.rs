//! Molecule viewer — type a molecular formula (`C2H6O`) or a name
//! (`aspirin`) and render it as a rotatable 3D ball-and-stick model
//! inside toastty via the Ratty Graphics Protocol (RGP).

// Cross-module `pub` items are internal to this binary, and some
// (e.g. the wave-2 interaction emitters) aren't wired until the TUI
// lands. Quiet the in-development noise; revisit once wave 2 is done.
#![allow(unreachable_pub)]

mod app;
mod elements;
mod geometry;
mod glb;
mod model;
mod pubchem;
mod rgp;
mod sdf;
mod ui;

use anyhow::{Context, Result};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // `--print <formula-or-name>`: headless mode. Run the full pipeline
    // and emit the RGP byte stream to stdout (for testing without the
    // TUI, or piping into a running toastty).
    if let Some(pos) = args.iter().position(|a| a == "--print") {
        let query = args
            .get(pos + 1)
            .context("usage: molecule-viewer --print <formula-or-name>")?;
        return print_mode(query);
    }

    app::run()
}

/// Headless pipeline: `PubChem` → SDF → meshes → GLB → RGP escape bytes.
fn print_mode(query: &str) -> Result<()> {
    let candidates = pubchem::search(query)?;
    for (i, c) in candidates.iter().enumerate() {
        eprintln!("  [{i}] CID {} — {} ({})", c.cid, c.title, c.formula);
    }
    let chosen = candidates
        .first()
        .context("no PubChem candidates for query")?;
    eprintln!("Rendering CID {} ({})", chosen.cid, chosen.title);

    let sdf_text = pubchem::fetch_sdf_3d(chosen.cid)?;
    let mol = sdf::parse_sdf(&sdf_text)?;
    let meshes = geometry::build(&mol);

    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for (i, cm) in meshes.iter().enumerate() {
        let id = u32::try_from(i + 1).unwrap();
        let glb = glb::write(&cm.mesh);
        rgp::register_payload(&mut out, id, &glb)?;
        rgp::place(
            &mut out,
            id,
            &rgp::Placement {
                row: 12,
                col: 40,
                w: 40,
                h: 24,
                depth: -5.0,
                scale: 1.0,
                rx: 20.0,
                ry: 30.0,
                rz: 0.0,
                color: cm.color,
                animate: false,
            },
        )?;
    }
    Ok(())
}
