//! toastty — lightweight GPU-accelerated terminal emulator.

use anyhow::Result;
use tracing::info;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    info!("toastty {} starting", env!("CARGO_PKG_VERSION"));
    Ok(())
}
