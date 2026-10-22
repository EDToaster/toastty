//! Hot-reload of the `[shader] paths` files (M13).
//!
//! Same design as [`crate::config_watcher`]: each distinct parent
//! directory is watched non-recursively (so editor save-via-rename and
//! files created later are seen), events are filtered to the target
//! files and to payload kinds, and a hit sets a shared pending flag and
//! sends `UserEvent::Wake`. The main thread drains the flag with
//! [`ShaderWatcher::take_pending`] and reloads the whole chain.
//!
//! When the path list changes (config reload), the binary drops the old
//! watcher and spawns a new one; that's one inotify instance, so cheap.
//! Paths are matched literally: a symlinked shader is reloaded when the
//! link changes, not when its target is edited in another directory.

use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use toastty_window::EventLoopProxy;
use tracing::{debug, warn};

use crate::config_watcher::is_payload_event;

/// Handle returned to the binary. Drop it to stop watching.
pub struct ShaderWatcher {
    /// Kept alive so the watcher thread keeps running.
    #[allow(dead_code)]
    inner: RecommendedWatcher,
    pending: Arc<AtomicBool>,
}

impl std::fmt::Debug for ShaderWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ShaderWatcher")
            .field("pending", &self.pending.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl ShaderWatcher {
    /// Watch `paths` (resolved shader files). Returns `None` when there
    /// is nothing to watch (empty list, or no parent directory exists)
    /// or the watcher can't be installed. Non-fatal: shaders still load
    /// once, just without hot-reload.
    pub fn spawn(
        paths: &[PathBuf],
        proxy: EventLoopProxy<toastty_window::UserEvent>,
    ) -> Option<Self> {
        let dirs: Vec<PathBuf> = parent_dirs(paths)
            .into_iter()
            .filter(|d| {
                let ok = d.is_dir();
                if !ok {
                    debug!(dir = ?d, "shader dir missing — not watching");
                }
                ok
            })
            .collect();
        if dirs.is_empty() {
            return None;
        }
        let pending = Arc::new(AtomicBool::new(false));
        let pending_for_cb = Arc::clone(&pending);
        let targets: HashSet<PathBuf> = paths.iter().cloned().collect();

        let mut watcher =
            match notify::recommended_watcher(move |res: notify::Result<notify::Event>| match res {
                Ok(ev) => {
                    if !is_relevant(&ev, &targets) {
                        return;
                    }
                    pending_for_cb.store(true, Ordering::SeqCst);
                    // Err means the loop has exited; nothing to do.
                    let _ = proxy.send_event(toastty_window::UserEvent::Wake);
                }
                Err(e) => warn!("shader watcher error: {e}"),
            }) {
                Ok(w) => w,
                Err(e) => {
                    warn!("shader watcher init failed: {e}");
                    return None;
                }
            };

        let mut n_watched = 0;
        for dir in &dirs {
            match watcher.watch(dir, RecursiveMode::NonRecursive) {
                Ok(()) => n_watched += 1,
                Err(e) => warn!("shader watch() failed for {}: {e}", dir.display()),
            }
        }
        if n_watched == 0 {
            return None;
        }

        debug!(?paths, "shader watcher installed");
        Some(Self {
            inner: watcher,
            pending,
        })
    }

    /// Atomically clear and return whether a reload is pending.
    pub fn take_pending(&self) -> bool {
        self.pending.swap(false, Ordering::SeqCst)
    }
}

/// Distinct parent directories of `paths`, sorted. A bare relative file
/// name has an empty parent, which means the CWD.
fn parent_dirs(paths: &[PathBuf]) -> BTreeSet<PathBuf> {
    paths
        .iter()
        .filter_map(|p| p.parent())
        .map(|d| {
            if d.as_os_str().is_empty() {
                PathBuf::from(".")
            } else {
                d.to_path_buf()
            }
        })
        .collect()
}

/// True when `ev` is a payload event (see
/// [`crate::config_watcher`]) touching one of `targets`. Siblings in a
/// watched directory are ignored.
fn is_relevant(ev: &notify::Event, targets: &HashSet<PathBuf>) -> bool {
    is_payload_event(ev.kind) && ev.paths.iter().any(|p| matches(p, targets))
}

/// Exact match, plus `./name` for targets that are bare relative names
/// (their directory is watched as `.`, so events arrive as `./name`).
fn matches(p: &Path, targets: &HashSet<PathBuf>) -> bool {
    targets.contains(p) || p.strip_prefix(".").is_ok_and(|rest| targets.contains(rest))
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::EventKind;
    use notify::event::{AccessKind, CreateKind, DataChange, ModifyKind, RenameMode};

    fn ev(kind: EventKind, paths: &[&str]) -> notify::Event {
        notify::Event {
            kind,
            paths: paths.iter().map(PathBuf::from).collect(),
            attrs: notify::event::EventAttributes::default(),
        }
    }

    fn targets(paths: &[&str]) -> HashSet<PathBuf> {
        paths.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn parent_dirs_are_distinct() {
        let paths: Vec<PathBuf> = ["/a/x.glsl", "/a/y.wgsl", "/b/z.glsl", "rel.glsl"]
            .iter()
            .map(PathBuf::from)
            .collect();
        let dirs: Vec<PathBuf> = parent_dirs(&paths).into_iter().collect();
        assert_eq!(
            dirs,
            vec![PathBuf::from("/a"), PathBuf::from("/b"), PathBuf::from(".")]
        );
        assert!(parent_dirs(&[]).is_empty());
    }

    #[test]
    fn data_write_to_any_target_is_relevant() {
        let t = targets(&["/a/x.glsl", "/b/z.glsl"]);
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(is_relevant(&ev(data, &["/a/x.glsl"]), &t));
        assert!(is_relevant(&ev(data, &["/b/z.glsl"]), &t));
    }

    #[test]
    fn sibling_files_are_ignored() {
        let t = targets(&["/a/x.glsl"]);
        let data = EventKind::Modify(ModifyKind::Data(DataChange::Any));
        assert!(!is_relevant(&ev(data, &["/a/x.glsl.swp"]), &t));
        assert!(!is_relevant(&ev(data, &["/a/other.glsl"]), &t));
    }

    #[test]
    fn rename_onto_target_is_relevant() {
        // Editor save: write a temp file, rename it over the target.
        let t = targets(&["/a/x.glsl"]);
        let rename = EventKind::Modify(ModifyKind::Name(RenameMode::Both));
        assert!(is_relevant(
            &ev(rename, &["/a/.x.glsl.tmp", "/a/x.glsl"]),
            &t
        ));
        assert!(is_relevant(
            &ev(EventKind::Create(CreateKind::File), &["/a/x.glsl"]),
            &t
        ));
    }

    #[test]
    fn access_events_are_ignored() {
        let t = targets(&["/a/x.glsl"]);
        assert!(!is_relevant(
            &ev(EventKind::Access(AccessKind::Read), &["/a/x.glsl"]),
            &t
        ));
    }

    #[test]
    fn bare_relative_target_matches_dot_prefixed_event() {
        let t = targets(&["x.glsl"]);
        assert!(is_relevant(&ev(EventKind::Any, &["./x.glsl"]), &t));
        assert!(!is_relevant(&ev(EventKind::Any, &["./y.glsl"]), &t));
    }
}
