//! Hot-reload of the user config file.
//!
//! Watches the parent directory of the resolved config path so we still
//! see CREATE events for a file that doesn't exist yet (the parent
//! directory itself must exist — otherwise we silently skip; the user
//! can create the directory + file and restart). On any event touching
//! the target path we:
//!
//! 1. Set a shared [`AtomicBool`] so the main thread knows a reload is
//!    pending.
//! 2. Send `UserEvent::Wake` through the winit `EventLoopProxy` so the
//!    event loop wakes up even when idle.
//!
//! The main thread drains the flag in its `Event::User` handler and
//! re-parses the file there.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use toastty_window::EventLoopProxy;
use tracing::{debug, warn};

/// Handle returned to the binary. Drop it to stop the watcher.
pub struct ConfigWatcher {
    /// Kept alive so the watcher thread keeps running.
    #[allow(dead_code)]
    inner: RecommendedWatcher,
    /// Shared flag. The binary calls [`take_pending`] each `Event::User`.
    pending: Arc<AtomicBool>,
}

impl std::fmt::Debug for ConfigWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ConfigWatcher")
            .field("pending", &self.pending.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ConfigWatcher {
    /// Spawn a watcher targeting `path`. Returns `None` if the parent
    /// directory doesn't exist or the watcher can't be installed —
    /// failure here is non-fatal (the terminal still runs without
    /// hot-reload), so we log + return None instead of bubbling.
    pub fn spawn(
        path: PathBuf,
        proxy: EventLoopProxy<toastty_window::UserEvent>,
    ) -> Option<Self> {
        let parent = path.parent()?.to_path_buf();
        if !parent.exists() {
            debug!(?parent, "config parent dir missing — not watching");
            return None;
        }
        let pending = Arc::new(AtomicBool::new(false));
        let pending_for_cb = Arc::clone(&pending);
        let target = path.clone();

        let mut watcher = match notify::recommended_watcher(
            move |res: notify::Result<notify::Event>| match res {
                Ok(ev) => {
                    if !matches_target(&ev, &target) {
                        return;
                    }
                    if !is_payload_event(ev.kind) {
                        return;
                    }
                    pending_for_cb.store(true, Ordering::SeqCst);
                    // Wake the event loop. If the loop has already
                    // exited, the proxy returns Err — there's nothing
                    // useful to do here, so just drop the result.
                    let _ = proxy.send_event(toastty_window::UserEvent::Wake);
                }
                Err(e) => warn!("config watcher error: {e}"),
            },
        ) {
            Ok(w) => w,
            Err(e) => {
                warn!("config watcher init failed: {e}");
                return None;
            }
        };

        if let Err(e) = watcher.watch(&parent, RecursiveMode::NonRecursive) {
            warn!("config watch() failed for {}: {e}", parent.display());
            return None;
        }

        debug!(?path, "config watcher installed");
        Some(Self {
            inner: watcher,
            pending,
        })
    }

    /// Atomically clear and return whether a reload is pending. Called
    /// from the main thread on each `Event::User`.
    pub fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

/// True when the event references the target path. Editor-style writes
/// often arrive as events on a tempfile + a rename to the target, so we
/// match by exact path equality across all `paths`.
fn matches_target(ev: &notify::Event, target: &Path) -> bool {
    ev.paths.iter().any(|p| p == target)
}

/// Filter out access / metadata-only notifications so we don't fire a
/// reload for every `stat` the editor does. Create / Modify(Data) /
/// Modify(Name) (atomic rename) / Remove are the interesting kinds.
fn is_payload_event(kind: EventKind) -> bool {
    use notify::event::ModifyKind;
    matches!(
        kind,
        EventKind::Create(_)
            | EventKind::Remove(_)
            | EventKind::Modify(
                ModifyKind::Data(_) | ModifyKind::Name(_) | ModifyKind::Any
            )
            | EventKind::Any
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_filter_passes_data_modify() {
        use notify::event::{DataChange, ModifyKind};
        assert!(is_payload_event(EventKind::Modify(ModifyKind::Data(
            DataChange::Any
        ))));
    }

    #[test]
    fn payload_filter_rejects_access() {
        use notify::event::{AccessKind, AccessMode};
        assert!(!is_payload_event(EventKind::Access(AccessKind::Read)));
        assert!(!is_payload_event(EventKind::Access(AccessKind::Open(
            AccessMode::Read
        ))));
    }

    #[test]
    fn matches_target_compares_paths() {
        let target = PathBuf::from("/tmp/x/config.toml");
        let ev = notify::Event {
            kind: EventKind::Any,
            paths: vec![PathBuf::from("/tmp/x/config.toml")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(matches_target(&ev, &target));
        let other = notify::Event {
            kind: EventKind::Any,
            paths: vec![PathBuf::from("/tmp/x/other.toml")],
            attrs: notify::event::EventAttributes::default(),
        };
        assert!(!matches_target(&other, &target));
    }
}
