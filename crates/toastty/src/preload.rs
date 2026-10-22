//! Startup work that runs on worker threads while the main thread opens
//! the display and the window.
//!
//! Until the window exists the main thread is busy inside winit
//! (connecting to the display server, loading the keymap: tens of ms).
//! Two other slow pieces of startup need no window at all:
//!
//! - the GPU instance, adapter and device ([`GpuContext`]): loading and
//!   initialising the Vulkan driver, ~20 ms;
//! - the font database ([`LoadedFonts`]): scanning and parsing every
//!   installed font, then ranking the faces for the configured family,
//!   ~30–40 ms.
//!
//! [`Preload::spawn`] starts both at the top of `main`. `init_impl` then
//! takes the results, joining each thread; by then they have usually
//! finished. Anything that fails or panics on a worker yields `None`, and
//! the caller falls back to doing the work inline exactly as before, so a
//! preload can only change when work happens, never what it produces.

use std::thread::JoinHandle;

use toastty_render::{GpuContext, LoadedFonts};
use tracing::debug;

/// Handles to the startup worker threads. See the module docs.
#[derive(Debug, Default)]
pub struct Preload {
    gpu: Option<JoinHandle<Option<GpuContext>>>,
    fonts: Option<JoinHandle<LoadedFonts>>,
}

impl Preload {
    /// Start loading the GPU context and the fonts for `font_family` on two
    /// worker threads.
    #[must_use]
    pub fn spawn(font_family: &str) -> Self {
        let gpu = std::thread::Builder::new()
            .name("toastty-gpu-init".into())
            .spawn(|| match pollster::block_on(GpuContext::new()) {
                Ok(gpu) => Some(gpu),
                Err(e) => {
                    debug!("GPU preload failed, retrying with the window: {e}");
                    None
                }
            })
            .ok();
        let family = font_family.to_owned();
        let fonts = std::thread::Builder::new()
            .name("toastty-font-init".into())
            .spawn(move || toastty_render::load_fonts(Some(&family)))
            .ok();
        Self { gpu, fonts }
    }

    /// The preloaded GPU context, waiting for it if needed. `None` when
    /// it wasn't started, failed, or was already taken.
    pub fn take_gpu(&mut self) -> Option<GpuContext> {
        self.gpu.take()?.join().ok().flatten()
    }

    /// The preloaded fonts if they were loaded for `font_family`, waiting
    /// for them if needed. `None` when they weren't started, the loader
    /// panicked, were already taken, or are for another family.
    pub fn take_fonts(&mut self, font_family: &str) -> Option<LoadedFonts> {
        let fonts = self.fonts.take()?.join().ok()?;
        (fonts.font_name() == Some(font_family)).then_some(fonts)
    }
}

impl Drop for Preload {
    /// Join workers whose results were never taken (startup failed before
    /// `init_impl` got to them), so the process never exits while a
    /// worker is still inside the GPU driver's or fontconfig's setup.
    fn drop(&mut self) {
        if let Some(gpu) = self.gpu.take() {
            let _ = gpu.join();
        }
        if let Some(fonts) = self.fonts.take() {
            let _ = fonts.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_preload_yields_nothing() {
        let mut p = Preload::default();
        assert!(p.take_gpu().is_none());
        assert!(p.take_fonts("Fira Mono").is_none());
    }

    #[test]
    fn fonts_are_taken_once_and_only_for_their_family() {
        let mut p = Preload {
            gpu: None,
            fonts: Some(std::thread::spawn(|| {
                toastty_render::load_fonts(Some("Fira Mono"))
            })),
        };
        let fonts = p.take_fonts("Fira Mono").expect("fonts for Fira Mono");
        assert_eq!(fonts.font_name(), Some("Fira Mono"));
        assert!(p.take_fonts("Fira Mono").is_none(), "taken twice");

        let mut p = Preload {
            gpu: None,
            fonts: Some(std::thread::spawn(|| {
                toastty_render::load_fonts(Some("Fira Mono"))
            })),
        };
        assert!(p.take_fonts("JetBrains Mono").is_none(), "other family");
    }

    #[test]
    fn panicking_font_loader_yields_none() {
        let mut p = Preload {
            gpu: None,
            fonts: Some(std::thread::spawn(|| panic!("font loader died"))),
        };
        assert!(p.take_fonts("Fira Mono").is_none());
    }
}
