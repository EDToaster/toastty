//! macOS: open a new terminal window when an already-running toastty is
//! launched again.
//!
//! toastty is one window per process. When the app is installed as a
//! `.app` bundle (`scripts/install_mac_app.sh`), the real binary is the
//! bundle's executable, so its process owns the bundle's Dock tile: the
//! Dock shows the app icon and name, and pinning the tile pins the app.
//!
//! The catch is that macOS treats a bundle as single-instance. Clicking the
//! Dock tile, or picking toastty in Spotlight or Finder while it is
//! running, does not start anything new. `LaunchServices` sends the running
//! process a "reopen application" Apple Event (`kAEReopenApplication`)
//! instead, and `AppKit`'s default handler just brings it to the front.
//!
//! [`install`] replaces that handler with one that starts a fresh instance
//! of the bundle (`open -n`), so every launch still gives a new window.
//! Outside a bundle (`cargo run`, a bare `./target/release/toastty`) no
//! reopen events arrive and [`install`] does nothing.
//!
//! Plain objc runtime FFI, like `led`: winit owns the `NSApplication`
//! delegate, so we register a separate handler object with
//! `NSAppleEventManager` rather than touching winit's delegate class.

use std::path::{Path, PathBuf};

/// Directory of the `.app` bundle containing `exe`, if `exe` is a bundle
/// executable (`<name>.app/Contents/MacOS/<exe>`).
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn bundle_of(exe: &Path) -> Option<PathBuf> {
    let macos = exe.parent()?;
    let contents = macos.parent()?;
    let bundle = contents.parent()?;
    let is_bundle = macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && bundle.extension()? == "app";
    is_bundle.then(|| bundle.to_path_buf())
}

/// Install the reopen handler. Call after the app has finished launching
/// (`AppKit` installs its own default handlers during launch and would
/// overwrite ours). Idempotent.
pub(crate) fn install() {
    platform::install();
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod platform {
    use std::ffi::{CStr, c_char, c_void};
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::OnceLock;

    use tracing::{debug, warn};

    type Id = *mut c_void;
    type Sel = *mut c_void;
    type Class = *mut c_void;

    // AEEventClass / AEEventID four-char codes from AppleEvents.h:
    // kCoreEventClass = 'aevt', kAEReopenApplication = 'rapp'.
    const K_CORE_EVENT_CLASS: u32 = u32::from_be_bytes(*b"aevt");
    const K_AE_REOPEN_APPLICATION: u32 = u32::from_be_bytes(*b"rapp");

    #[link(name = "objc")]
    unsafe extern "C" {
        fn objc_getClass(name: *const c_char) -> Class;
        fn objc_allocateClassPair(superclass: Class, name: *const c_char, extra: usize) -> Class;
        fn objc_registerClassPair(cls: Class);
        fn sel_registerName(name: *const c_char) -> Sel;
        fn class_addMethod(cls: Class, sel: Sel, imp: *const c_void, types: *const c_char) -> bool;
        // Declared with no arguments; always called through a transmuted,
        // correctly-typed pointer (required on arm64, where objc_msgSend
        // is not variadic).
        fn objc_msgSend();
    }

    // NSAppleEventManager lives in Foundation. winit links it already;
    // this makes the dependency explicit.
    #[link(name = "Foundation", kind = "framework")]
    unsafe extern "C" {}

    /// Bundle to relaunch; `None` when not running from a `.app`.
    static BUNDLE: OnceLock<Option<PathBuf>> = OnceLock::new();

    fn sel(name: &CStr) -> Sel {
        // Safety: `name` is a NUL-terminated selector name; the runtime
        // interns it and the returned SEL lives for the process.
        unsafe { sel_registerName(name.as_ptr()) }
    }

    /// `-[ToasttyReopenHandler handleReopen:withReplyEvent:]`.
    extern "C" fn handle_reopen(_this: Id, _cmd: Sel, _event: Id, _reply: Id) {
        let Some(Some(bundle)) = BUNDLE.get() else {
            return;
        };
        debug!(
            "reopen event: starting a new instance of {}",
            bundle.display()
        );
        // `open -n` asks LaunchServices for a new instance of the bundle,
        // so the new window gets the same launch environment as a
        // Spotlight launch. Wait on it off the main thread to reap it.
        match Command::new("/usr/bin/open").arg("-n").arg(bundle).spawn() {
            Ok(mut child) => {
                std::thread::spawn(move || {
                    let _ = child.wait();
                });
            }
            Err(e) => warn!("reopen: failed to run open -n: {e}"),
        }
    }

    pub(super) fn install() {
        static INSTALLED: OnceLock<()> = OnceLock::new();

        let exe = std::env::current_exe().ok();
        let bundle = exe.as_deref().and_then(super::bundle_of);
        let Some(bundle) = BUNDLE.get_or_init(|| bundle).as_ref() else {
            return;
        };

        if INSTALLED.set(()).is_err() {
            return;
        }
        debug!("installing reopen handler for {}", bundle.display());

        let handle_sel = sel(c"handleReopen:withReplyEvent:");

        // Safety: standard objc runtime calls. The class is created and
        // registered once (guarded by INSTALLED); each objc_msgSend is
        // cast to the exact signature of the method it calls; the
        // handler instance is intentionally never released, because
        // NSAppleEventManager does not retain its handlers.
        unsafe {
            let nsobject = objc_getClass(c"NSObject".as_ptr());
            let cls = objc_allocateClassPair(nsobject, c"ToasttyReopenHandler".as_ptr(), 0);
            if cls.is_null() {
                warn!("reopen: could not allocate handler class");
                return;
            }
            let imp: extern "C" fn(Id, Sel, Id, Id) = handle_reopen;
            class_addMethod(cls, handle_sel, imp as *const c_void, c"v@:@@".as_ptr());
            objc_registerClassPair(cls);

            let send0: unsafe extern "C" fn(Id, Sel) -> Id =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            let handler = send0(cls, sel(c"new"));

            let manager_cls = objc_getClass(c"NSAppleEventManager".as_ptr());
            let manager = send0(manager_cls, sel(c"sharedAppleEventManager"));
            if handler.is_null() || manager.is_null() {
                warn!("reopen: NSAppleEventManager unavailable");
                return;
            }

            // -[NSAppleEventManager setEventHandler:andSelector:forEventClass:andEventID:]
            let set_handler: unsafe extern "C" fn(Id, Sel, Id, Sel, u32, u32) =
                std::mem::transmute(objc_msgSend as unsafe extern "C" fn());
            set_handler(
                manager,
                sel(c"setEventHandler:andSelector:forEventClass:andEventID:"),
                handler,
                handle_sel,
                K_CORE_EVENT_CLASS,
                K_AE_REOPEN_APPLICATION,
            );
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub(super) fn install() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bundle_executable_resolves_to_its_bundle() {
        let exe = Path::new("/Applications/Toastty.app/Contents/MacOS/toastty");
        assert_eq!(
            bundle_of(exe),
            Some(PathBuf::from("/Applications/Toastty.app"))
        );
    }

    #[test]
    fn plain_binary_is_not_a_bundle() {
        assert_eq!(bundle_of(Path::new("/repo/target/release/toastty")), None);
        assert_eq!(bundle_of(Path::new("toastty")), None);
    }

    #[test]
    fn needs_the_full_contents_macos_layout() {
        // Right extension, wrong inner layout.
        assert_eq!(
            bundle_of(Path::new("/Applications/Toastty.app/MacOS/toastty")),
            None
        );
        assert_eq!(
            bundle_of(Path::new("/x/Toastty.app/Contents/Resources/toastty")),
            None
        );
        // Right layout, not a .app.
        assert_eq!(
            bundle_of(Path::new("/x/Toastty/Contents/MacOS/toastty")),
            None
        );
    }

    #[test]
    fn install_is_a_no_op_outside_a_bundle() {
        // The test binary is not in a .app on any platform; must not panic.
        install();
        install();
    }
}
