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
//!
//! Each parent directory is canonicalized before watching: inotify gives
//! two spellings of one directory (a stowed `~/.config/toastty` symlink,
//! `a/../b`) the same watch, and notify then reports every event under
//! whichever spelling was added last. The file name itself is kept as
//! is, so a symlinked shader is reloaded when the link changes, not when
//! its target is edited in another directory.

use std::collections::{BTreeSet, HashSet};
use std::path::PathBuf;
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
        let WatchSet { dirs, targets } = watch_set(paths);
        if dirs.is_empty() {
            return None;
        }
        let pending = Arc::new(AtomicBool::new(false));
        let pending_for_cb = Arc::clone(&pending);

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

/// What to watch and what to match, spelled the way notify reports
/// events: absolute, with the parent directory canonicalized.
#[derive(Debug, Default, PartialEq, Eq)]
struct WatchSet {
    /// Distinct canonical parent directories.
    dirs: BTreeSet<PathBuf>,
    /// `canonical parent / file name` for each path.
    targets: HashSet<PathBuf>,
}

/// Build the [`WatchSet`] for `paths`. Relative paths are made absolute
/// against the CWD (notify does the same for the watch). Paths whose
/// parent directory doesn't exist are skipped.
fn watch_set(paths: &[PathBuf]) -> WatchSet {
    let mut set = WatchSet::default();
    for path in paths {
        let Ok(abs) = std::path::absolute(path) else {
            continue;
        };
        let (Some(parent), Some(name)) = (abs.parent(), abs.file_name()) else {
            continue;
        };
        match std::fs::canonicalize(parent) {
            Ok(dir) => {
                set.targets.insert(dir.join(name));
                set.dirs.insert(dir);
            }
            Err(e) => debug!(dir = ?parent, "shader dir not watchable: {e}"),
        }
    }
    set
}

/// True when `ev` is a payload event (see
/// [`crate::config_watcher`]) touching one of `targets`. Siblings in a
/// watched directory are ignored.
fn is_relevant(ev: &notify::Event, targets: &HashSet<PathBuf>) -> bool {
    is_payload_event(ev.kind) && ev.paths.iter().any(|p| targets.contains(p))
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

    #[cfg(unix)]
    #[test]
    fn watch_set_canonicalizes_parent_and_dedups() {
        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = real.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let paths = vec![
            real.join("a.glsl"),
            link.join("b.glsl"),
            real.join("sub/../c.glsl"),
            real.join("missing/d.glsl"),
        ];
        std::fs::create_dir(real.join("sub")).unwrap();
        let set = watch_set(&paths);
        assert_eq!(set.dirs.into_iter().collect::<Vec<_>>(), vec![real.clone()]);
        assert_eq!(
            set.targets,
            ["a.glsl", "b.glsl", "c.glsl"]
                .iter()
                .map(|n| real.join(n))
                .collect()
        );
        assert_eq!(watch_set(&[]), WatchSet::default());
    }

    #[test]
    fn watch_set_makes_relative_paths_absolute() {
        let cwd = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let set = watch_set(&[PathBuf::from("x.glsl")]);
        assert_eq!(set.dirs.into_iter().collect::<Vec<_>>(), vec![cwd.clone()]);
        assert!(set.targets.contains(&cwd.join("x.glsl")));
    }

    /// Real notify watcher: writes through a symlinked spelling of the
    /// directory must match the canonical targets.
    #[cfg(unix)]
    #[test]
    fn real_events_match_targets() {
        use std::sync::mpsc;
        use std::time::{Duration, Instant};

        let tmp = tempfile::tempdir().unwrap();
        let real = std::fs::canonicalize(tmp.path()).unwrap().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = real.parent().unwrap().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let target = link.join("x.glsl");
        let set = watch_set(&[target.clone(), real.join("y.glsl")]);

        let (tx, rx) = mpsc::channel();
        let mut w = notify::recommended_watcher(move |res| {
            let _ = tx.send(res);
        })
        .unwrap();
        for dir in &set.dirs {
            w.watch(dir, RecursiveMode::NonRecursive).unwrap();
        }
        std::fs::write(link.join("sibling.txt"), "x").unwrap();
        std::fs::write(&target, "void mainImage() {}").unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut saw_sibling = false;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let ev = rx.recv_timeout(left).expect("no event for target").unwrap();
            if ev.paths.iter().any(|p| p.ends_with("sibling.txt")) {
                saw_sibling = true;
                assert!(!is_relevant(&ev, &set.targets), "{ev:?}");
            } else if is_relevant(&ev, &set.targets) {
                break;
            }
        }
        assert!(saw_sibling, "sibling write should arrive first");
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
}
