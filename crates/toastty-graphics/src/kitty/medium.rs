//! File transmission mediums (`t=f`, `t=t`): path policy and bounded reads.
//!
//! The path comes from whatever wrote to the PTY: `cat` of a file, a
//! remote host over ssh, a sandboxed process. The spec's rules for it:
//!
//! - Only regular files may be read, checked on the path *before* it is
//!   opened ("merely opening a file can have side-effects"). Symlinks are
//!   followed. Sensitive trees (`/proc`, `/sys`, `/dev`) may be refused.
//! - Every failure must look the same to the client
//!   ([`FILE_READ_ERROR`]), so it can't probe files it couldn't read
//!   itself. The real reason ([`FileError`]) is for the local log only.
//! - `t=t` deletes the file afterwards, but only when it lies in a known
//!   temp dir AND its path contains [`TEMP_FILE_MARKER`].
//!
//! [`read_image_file`] implements all three. It never blocks on a client
//! controlled object: the file type is checked with `stat` before the
//! `open`, the `open` is `O_NONBLOCK | O_NOFOLLOW`, and an `fstat` of the
//! opened fd re-checks the type and identity to close the race. Reads are
//! capped by the caller's budget, so `/dev/zero`-style inputs (refused
//! anyway) or huge regular files can't exhaust memory.

use std::path::{Path, PathBuf};

/// The single reply detail for every `t=f`/`t=t` failure (SEC-4). Matches
/// kitty's `EBADF:Failed to read image file`.
pub(super) const FILE_READ_ERROR: &str = "Failed to read image file";

/// `t=t` only deletes files whose path contains this string (spec §
/// "transmission medium"; kitty's `kitten icat` names its temp files
/// `tty-graphics-protocol-*`).
pub(super) const TEMP_FILE_MARKER: &str = "tty-graphics-protocol";

/// Longest path we accept. Linux `PATH_MAX`; kitty caps at 2048.
const MAX_PATH_LEN: usize = 4096;

/// Why a file read failed. Logged, never sent to the client.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub(super) struct FileError(pub(super) String);

impl FileError {
    fn new(msg: impl Into<String>) -> Self {
        Self(msg.into())
    }
}

/// What to read, and what to do with the file afterwards.
#[derive(Debug, Clone, Copy)]
pub(super) struct FileRequest<'a> {
    /// The path exactly as the client sent it.
    pub(super) path: &'a Path,
    /// `O=`: byte offset to start reading at.
    pub(super) offset: u64,
    /// Bytes to read; `0` means "to end of file". A file shorter than a
    /// non-zero `size` is an error (the spec lists "smaller than the
    /// client claimed" among the failures).
    pub(super) size: u64,
    /// Hard cap on the bytes read. A request (or, with `size == 0`, a
    /// file) larger than this fails without reading anything.
    pub(super) max: u64,
    /// `t=t`: delete the file afterwards if [`temp_file_to_delete`]
    /// allows it.
    pub(super) delete_temp: bool,
}

/// Read the bytes `req` asks for, applying the medium's security policy,
/// then (for `t=t`) delete the file when the temp-file rule allows it.
///
/// The deletion happens whenever the path resolved to a regular file,
/// even if the read or a later decode fails, matching kitty: a `t=t` file
/// is the client's hand-off and is consumed either way.
pub(super) fn read_image_file(req: &FileRequest<'_>) -> Result<Vec<u8>, FileError> {
    read_image_file_with_roots(req, &temp_roots())
}

/// [`read_image_file`] with an explicit list of canonical temp-dir roots
/// (tests use a private one).
pub(super) fn read_image_file_with_roots(
    req: &FileRequest<'_>,
    roots: &[PathBuf],
) -> Result<Vec<u8>, FileError> {
    let opened = open_regular_file(req.path)?;
    let result = read_bounded(&opened, req);
    if req.delete_temp
        && let Some(target) = temp_file_to_delete(req.path, &opened.canonical, roots)
        && let Err(e) = std::fs::remove_file(&target)
    {
        tracing::debug!(path = %target.display(), error = %e, "kitty: t=t unlink failed");
    }
    result
}

/// A validated, opened regular file.
#[derive(Debug)]
struct OpenedFile {
    file: std::fs::File,
    /// Fully resolved path (no `.`/`..`, no symlinks).
    canonical: PathBuf,
    /// `st_size` from the `fstat` of the open fd.
    len: u64,
}

/// Resolve `path`, refuse sensitive locations and anything that isn't a
/// regular file, then open it without blocking.
fn open_regular_file(path: &Path) -> Result<OpenedFile, FileError> {
    if path.as_os_str().is_empty() {
        return Err(FileError::new("empty path"));
    }
    if path.as_os_str().len() > MAX_PATH_LEN {
        return Err(FileError::new("path too long"));
    }
    // Follows symlinks (as the spec requires) and resolves `..`, so the
    // location checks below see where the bytes really live.
    let canonical =
        std::fs::canonicalize(path).map_err(|e| FileError(format!("{}: {e}", path.display())))?;
    if is_sensitive_path(&canonical) {
        return Err(FileError(format!(
            "{}: refusing a path under /proc, /sys or /dev",
            canonical.display()
        )));
    }
    // `stat` before `open`: opening a FIFO blocks, opening a device can
    // have side effects.
    let before = std::fs::metadata(&canonical)
        .map_err(|e| FileError(format!("{}: {e}", canonical.display())))?;
    if !before.file_type().is_file() {
        return Err(FileError(format!(
            "{}: not a regular file",
            canonical.display()
        )));
    }
    let file = open_nonblocking(&canonical)?;
    // `fstat` the fd: the path may have been swapped between `stat` and
    // `open`.
    let after = file
        .metadata()
        .map_err(|e| FileError(format!("{}: fstat: {e}", canonical.display())))?;
    if !after.file_type().is_file() || !same_file(&before, &after) {
        return Err(FileError(format!(
            "{}: changed between stat and open",
            canonical.display()
        )));
    }
    Ok(OpenedFile {
        file,
        canonical,
        len: after.len(),
    })
}

/// Read `[offset, offset + size)` (or to EOF) from an opened file, within
/// `req.max`.
fn read_bounded(opened: &OpenedFile, req: &FileRequest<'_>) -> Result<Vec<u8>, FileError> {
    use std::io::{Read, Seek, SeekFrom};
    let available = opened.len.saturating_sub(req.offset);
    let want = if req.size == 0 {
        available
    } else if req.size > available {
        return Err(FileError(format!(
            "{}: file is shorter than S={} at O={}",
            opened.canonical.display(),
            req.size,
            req.offset
        )));
    } else {
        req.size
    };
    if want == 0 {
        return Err(FileError(format!(
            "{}: nothing to read",
            opened.canonical.display()
        )));
    }
    if want > req.max {
        return Err(FileError(format!(
            "{}: {want} bytes exceeds the {} byte limit",
            opened.canonical.display(),
            req.max
        )));
    }
    let mut file = &opened.file;
    if req.offset > 0 {
        file.seek(SeekFrom::Start(req.offset))
            .map_err(|e| FileError(format!("{}: seek: {e}", opened.canonical.display())))?;
    }
    // `want <= max`, so the allocation is bounded; `take` keeps a file
    // that grows while we read from pushing past it.
    let mut buf = Vec::with_capacity(usize::try_from(want).unwrap_or(0));
    file.take(want)
        .read_to_end(&mut buf)
        .map_err(|e| FileError(format!("{}: read: {e}", opened.canonical.display())))?;
    if (buf.len() as u64) < want {
        return Err(FileError(format!(
            "{}: short read ({} of {want} bytes)",
            opened.canonical.display(),
            buf.len()
        )));
    }
    Ok(buf)
}

/// True for paths under `/proc`, `/sys` or `/dev`, which the spec lets
/// terminals refuse. `/dev/shm/<name>` stays readable, as in kitty: it is
/// a temp dir that clients legitimately use.
fn is_sensitive_path(canonical: &Path) -> bool {
    use std::path::Component;
    let mut parts = canonical.components().filter_map(|c| match c {
        Component::Normal(s) => Some(s),
        _ => None,
    });
    match parts.next().and_then(|s| s.to_str()) {
        Some("proc" | "sys") => true,
        Some("dev") => !(parts.next().is_some_and(|s| s == "shm") && parts.next().is_some()),
        _ => false,
    }
}

/// The directory entry `t=t` may unlink, or `None` to keep the file.
///
/// Matches kitty (`is_path_in_temp_dir` + the marker check in
/// `graphics.c`):
///
/// - The *sent* path must contain [`TEMP_FILE_MARKER`] somewhere (the
///   file name or a directory name).
/// - The *resolved* path must lie in a temp-dir root, so `/tmp/../home/…`
///   and a `/tmp` symlink into `$HOME` are kept.
/// - The entry removed is the sent path's own entry: a final-component
///   symlink is unlinked, never its target.
///
/// One check stricter than kitty: the directory holding that entry must
/// itself resolve into a temp root, so a symlinked parent can't make us
/// unlink an entry that lives outside it.
fn temp_file_to_delete(sent: &Path, canonical: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    if !path_contains_marker(sent) || !is_under_any_root(canonical, roots) {
        return None;
    }
    let name = sent.file_name()?;
    let parent = match sent.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent = std::fs::canonicalize(parent).ok()?;
    if !is_under_any_root(&parent, roots) && !roots.contains(&parent) {
        return None;
    }
    Some(parent.join(name))
}

fn path_contains_marker(path: &Path) -> bool {
    let bytes = path.as_os_str().as_encoded_bytes();
    let marker = TEMP_FILE_MARKER.as_bytes();
    bytes.windows(marker.len()).any(|w| w == marker)
}

/// `canonical` is strictly inside one of `roots` (component-wise, so
/// `/tmpfoo` is not under `/tmp`).
fn is_under_any_root(canonical: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| canonical != root && canonical.starts_with(root))
}

/// Canonical known temp dirs: `/tmp`, `/dev/shm`, `$TMPDIR` and the
/// platform temp dir (`std::env::temp_dir`). The same set kitty uses.
fn temp_roots() -> Vec<PathBuf> {
    let mut roots = vec![
        PathBuf::from("/tmp"),
        PathBuf::from("/dev/shm"),
        std::env::temp_dir(),
    ];
    if let Some(dir) = std::env::var_os("TMPDIR").filter(|d| !d.is_empty()) {
        roots.push(PathBuf::from(dir));
    }
    let mut canonical: Vec<PathBuf> = roots
        .into_iter()
        .filter_map(|r| std::fs::canonicalize(r).ok())
        .collect();
    canonical.sort();
    canonical.dedup();
    canonical
}

#[cfg(unix)]
fn open_nonblocking(path: &Path) -> Result<std::fs::File, FileError> {
    use rustix::fs::{Mode, OFlags};
    // NONBLOCK: a FIFO swapped in after the `stat` can't park us in
    // `open()`. NOFOLLOW: `path` is canonical, so a symlink here means it
    // was swapped. NOCTTY: never acquire a controlling terminal.
    let flags =
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY;
    rustix::fs::open(path, flags, Mode::empty())
        .map(std::fs::File::from)
        .map_err(|e| FileError(format!("{}: open: {e}", path.display())))
}

#[cfg(not(unix))]
fn open_nonblocking(path: &Path) -> Result<std::fs::File, FileError> {
    Err(FileError(format!(
        "{}: file mediums are only supported on unix",
        path.display()
    )))
}

#[cfg(unix)]
fn same_file(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::MetadataExt;
    a.dev() == b.dev() && a.ino() == b.ino()
}

#[cfg(not(unix))]
fn same_file(_a: &std::fs::Metadata, _b: &std::fs::Metadata) -> bool {
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    /// A private directory tree for one test, removed on drop:
    /// `<dir>/tmp` plays the only temp-dir root, `<dir>/home` is
    /// "outside". Nothing outside `<dir>` is ever touched.
    struct Sandbox {
        dir: PathBuf,
        tmp: PathBuf,
        home: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir().join(format!(
                "toastty-medium-sandbox-{tag}-{}-{n}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            let tmp = dir.join("tmp");
            let home = dir.join("home");
            std::fs::create_dir_all(&tmp).unwrap();
            std::fs::create_dir_all(&home).unwrap();
            let dir = std::fs::canonicalize(dir).unwrap();
            Self {
                tmp: dir.join("tmp"),
                home: dir.join("home"),
                dir,
            }
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.tmp.clone()]
        }

        /// Run a `t=t` read of `sent` against this sandbox's roots.
        fn take(&self, sent: &Path) -> Result<Vec<u8>, FileError> {
            read_image_file_with_roots(&req(sent, 0, 0, true), &self.roots())
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn write_file(path: &Path, bytes: &[u8]) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        path.to_path_buf()
    }

    fn req(path: &Path, offset: u64, size: u64, delete_temp: bool) -> FileRequest<'_> {
        FileRequest {
            path,
            offset,
            size,
            max: 1 << 20,
            delete_temp,
        }
    }

    fn exists(p: &Path) -> bool {
        p.symlink_metadata().is_ok()
    }

    fn mkfifo(path: &Path) {
        let ok = std::process::Command::new("mkfifo")
            .arg(path)
            .status()
            .unwrap()
            .success();
        assert!(ok, "mkfifo failed");
    }

    /// Run `f` on a helper thread and fail (instead of hanging the test
    /// run) if it doesn't return within `secs`.
    fn within<T: Send + 'static>(secs: u64, f: impl FnOnce() -> T + Send + 'static) -> T {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f());
        });
        rx.recv_timeout(std::time::Duration::from_secs(secs))
            .expect("read blocked: the call did not return in time")
    }

    // ---- reads (SEC-2 / SEC-3) ----

    #[test]
    fn reads_regular_file_with_offset_and_size() {
        let sb = Sandbox::new("read");
        let p = write_file(&sb.home.join("img.rgba"), &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            read_image_file_with_roots(&req(&p, 0, 0, false), &sb.roots()).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8]
        );
        assert_eq!(
            read_image_file_with_roots(&req(&p, 2, 4, false), &sb.roots()).unwrap(),
            vec![3, 4, 5, 6]
        );
    }

    #[test]
    fn file_shorter_than_size_is_an_error() {
        let sb = Sandbox::new("short");
        let p = write_file(&sb.home.join("img.rgba"), &[1, 2, 3]);
        assert!(read_image_file_with_roots(&req(&p, 0, 4, false), &sb.roots()).is_err());
        assert!(read_image_file_with_roots(&req(&p, 3, 0, false), &sb.roots()).is_err());
    }

    #[test]
    fn read_larger_than_max_fails_without_reading() {
        let sb = Sandbox::new("max");
        let p = write_file(&sb.home.join("big.bin"), &[0u8; 64]);
        let mut r = req(&p, 0, 0, false);
        r.max = 63;
        let err = read_image_file_with_roots(&r, &sb.roots()).unwrap_err();
        assert!(err.0.contains("exceeds"), "{err}");
        r.size = 64;
        assert!(read_image_file_with_roots(&r, &sb.roots()).is_err());
        r.max = 64;
        assert_eq!(
            read_image_file_with_roots(&r, &sb.roots()).unwrap().len(),
            64
        );
    }

    /// SEC-2: a FIFO is refused before `open()`, so the call returns at
    /// once instead of blocking until a writer shows up.
    #[test]
    fn fifo_is_refused_without_blocking() {
        let sb = Sandbox::new("fifo");
        let fifo = sb.tmp.join("tty-graphics-protocol-fifo");
        mkfifo(&fifo);
        let roots = sb.roots();
        let sent = fifo.clone();
        let res = within(5, move || {
            read_image_file_with_roots(&req(&sent, 0, 0, true), &roots)
        });
        let err = res.unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
        assert!(exists(&fifo), "t=t must not delete a FIFO");
    }

    /// The `O_NONBLOCK` open itself never blocks on a FIFO either (the
    /// stat→open race window).
    #[test]
    fn nonblocking_open_of_fifo_returns_immediately() {
        let sb = Sandbox::new("fifo-open");
        let fifo = sb.tmp.join("f");
        mkfifo(&fifo);
        let p = fifo.clone();
        let file = within(5, move || open_nonblocking(&p)).unwrap();
        assert!(!file.metadata().unwrap().file_type().is_file());
    }

    /// SEC-3: `/dev/zero` (directly or through a symlink) is refused, so
    /// there is no unbounded read.
    #[test]
    fn dev_zero_is_refused_directly_and_via_symlink() {
        let sb = Sandbox::new("zero");
        let link = sb.tmp.join("zero-link");
        symlink("/dev/zero", &link).unwrap();
        for p in [Path::new("/dev/zero"), link.as_path()] {
            let p = p.to_path_buf();
            let roots = sb.roots();
            let err = within(5, move || {
                read_image_file_with_roots(&req(&p, 0, 0, false), &roots)
            })
            .unwrap_err();
            assert!(err.0.contains("/dev"), "{err}");
        }
    }

    #[test]
    fn proc_and_sys_are_refused() {
        let sb = Sandbox::new("proc");
        for p in [
            "/proc/self/cmdline",
            "/proc/self/status",
            "/sys/kernel/hostname",
        ] {
            let p = Path::new(p);
            if !exists(p) {
                continue;
            }
            assert!(
                read_image_file_with_roots(&req(p, 0, 0, false), &sb.roots()).is_err(),
                "{} must be refused",
                p.display()
            );
        }
    }

    #[test]
    fn sensitive_path_classification() {
        for p in [
            "/proc/1/environ",
            "/sys/x",
            "/dev/zero",
            "/dev/null",
            "/dev",
            "/dev/shm",
        ] {
            assert!(is_sensitive_path(Path::new(p)), "{p}");
        }
        for p in [
            "/dev/shm/tty-graphics-protocol-x",
            "/tmp/x",
            "/home/u/proc",
            "/devices/x",
        ] {
            assert!(!is_sensitive_path(Path::new(p)), "{p}");
        }
    }

    #[test]
    fn directory_missing_file_and_symlink_loop_fail() {
        let sb = Sandbox::new("misc");
        let a = sb.tmp.join("loop-a");
        let b = sb.tmp.join("loop-b");
        symlink(&b, &a).unwrap();
        symlink(&a, &b).unwrap();
        for p in [sb.tmp.clone(), sb.tmp.join("missing"), a, PathBuf::new()] {
            assert!(
                read_image_file_with_roots(&req(&p, 0, 0, false), &sb.roots()).is_err(),
                "{} must fail",
                p.display()
            );
        }
    }

    #[test]
    fn symlink_to_regular_file_is_followed() {
        let sb = Sandbox::new("follow");
        let target = write_file(&sb.home.join("img.rgba"), &[9, 9, 9, 9]);
        let link = sb.tmp.join("link");
        symlink(&target, &link).unwrap();
        assert_eq!(
            read_image_file_with_roots(&req(&link, 0, 0, false), &sb.roots()).unwrap(),
            vec![9, 9, 9, 9]
        );
    }

    // ---- t=t deletion (SEC-1), the checklist table plus extras ----

    #[test]
    fn dotdot_out_of_temp_dir_is_kept() {
        let sb = Sandbox::new("dotdot");
        let f = write_file(&sb.home.join("plain.rgba"), &[1, 2, 3, 4]);
        let sent = sb.tmp.join("../home/plain.rgba");
        assert!(sb.take(&sent).is_ok());
        assert!(exists(&f));
        // Even with the marker on the way out.
        let f = write_file(&sb.home.join("tty-graphics-protocol-x"), &[1]);
        let sent = sb.tmp.join("../home/tty-graphics-protocol-x");
        assert!(sb.take(&sent).is_ok());
        assert!(exists(&f));
    }

    #[test]
    fn marker_outside_temp_dir_is_kept() {
        let sb = Sandbox::new("outside");
        let f = write_file(&sb.home.join("tty-graphics-protocol-x.rgba"), &[1]);
        assert!(sb.take(&f).is_ok());
        assert!(exists(&f));
    }

    #[test]
    fn temp_file_without_marker_is_kept() {
        let sb = Sandbox::new("nomarker");
        let f = write_file(&sb.tmp.join("plain.rgba"), &[1]);
        assert!(sb.take(&f).is_ok());
        assert!(exists(&f));
    }

    #[test]
    fn temp_file_with_marker_is_deleted() {
        let sb = Sandbox::new("marker");
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-x.rgba"), &[1, 2]);
        assert_eq!(sb.take(&f).unwrap(), vec![1, 2]);
        assert!(!exists(&f));
    }

    #[test]
    fn t_f_never_deletes() {
        let sb = Sandbox::new("tf");
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-x.rgba"), &[1]);
        assert!(read_image_file_with_roots(&req(&f, 0, 0, false), &sb.roots()).is_ok());
        assert!(exists(&f));
    }

    #[test]
    fn marker_in_directory_name_counts() {
        let sb = Sandbox::new("markerdir");
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-d/plain.rgba"), &[1]);
        assert!(sb.take(&f).is_ok());
        assert!(!exists(&f));
    }

    #[test]
    fn dotdot_within_temp_dir_is_deleted() {
        let sb = Sandbox::new("dotdot-in");
        std::fs::create_dir_all(sb.tmp.join("sub")).unwrap();
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-dd"), &[1]);
        assert!(
            sb.take(&sb.tmp.join("sub/../tty-graphics-protocol-dd"))
                .is_ok()
        );
        assert!(!exists(&f));
    }

    /// Checklist row: a symlink in the temp dir pointing into `$HOME`.
    /// kitty keeps both the link and the target.
    #[test]
    fn temp_symlink_to_outside_file_keeps_link_and_target() {
        let sb = Sandbox::new("link-out");
        let target = write_file(&sb.home.join("target.rgba"), &[1]);
        let link = sb.tmp.join("tty-graphics-protocol-link");
        symlink(&target, &link).unwrap();
        assert!(sb.take(&link).is_ok());
        assert!(exists(&link) && exists(&target));
    }

    /// kitty: the marker is checked on the sent path and the sent entry is
    /// removed, so a marked link to an unmarked temp file loses the link
    /// only.
    #[test]
    fn marked_link_to_temp_file_unlinks_link_only() {
        let sb = Sandbox::new("link-in");
        let target = write_file(&sb.tmp.join("plain-target.rgba"), &[1]);
        let link = sb.tmp.join("tty-graphics-protocol-link");
        symlink(&target, &link).unwrap();
        assert!(sb.take(&link).is_ok());
        assert!(!exists(&link), "link must be unlinked");
        assert!(exists(&target), "target must be kept");
    }

    /// kitty: an unmarked link to a marked temp file is kept, and so is
    /// the target.
    #[test]
    fn unmarked_link_to_marked_temp_file_is_kept() {
        let sb = Sandbox::new("link-unmarked");
        let target = write_file(&sb.tmp.join("tty-graphics-protocol-t"), &[1]);
        let link = sb.tmp.join("plain-link");
        symlink(&target, &link).unwrap();
        assert!(sb.take(&link).is_ok());
        assert!(exists(&link) && exists(&target));
    }

    #[test]
    fn marked_dir_symlink_out_of_temp_is_kept() {
        let sb = Sandbox::new("dirlink");
        let f = write_file(&sb.home.join("plain2.rgba"), &[1]);
        let dirlink = sb.tmp.join("tty-graphics-protocol-dirlink");
        symlink(&sb.home, &dirlink).unwrap();
        assert!(sb.take(&dirlink.join("plain2.rgba")).is_ok());
        assert!(exists(&f));
    }

    /// Stricter than kitty: the resolved file is in the temp dir, but the
    /// entry we would unlink (a link reached through a symlinked parent)
    /// lives outside it. Keep everything.
    #[test]
    fn entry_outside_temp_dir_is_never_unlinked() {
        let sb = Sandbox::new("parent-out");
        let target = write_file(&sb.tmp.join("f"), &[1]);
        std::fs::create_dir_all(sb.home.join("d")).unwrap();
        let outside_link = sb.home.join("d/tty-graphics-protocol-l");
        symlink(&target, &outside_link).unwrap();
        symlink(sb.home.join("d"), sb.tmp.join("dl")).unwrap();
        assert!(sb.take(&sb.tmp.join("dl/tty-graphics-protocol-l")).is_ok());
        assert!(exists(&outside_link) && exists(&target));
    }

    #[test]
    fn marked_directory_is_kept() {
        let sb = Sandbox::new("dir");
        let d = sb.tmp.join("tty-graphics-protocol-adir");
        std::fs::create_dir_all(&d).unwrap();
        assert!(sb.take(&d).is_err());
        assert!(exists(&d));
    }

    /// kitty deletes a `t=t` file even when the read fails after the open
    /// (here: shorter than `S=`).
    #[test]
    fn failed_read_of_regular_temp_file_still_deletes() {
        let sb = Sandbox::new("short-del");
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-short"), &[1, 2]);
        let r = req(&f, 0, 4, true);
        assert!(read_image_file_with_roots(&r, &sb.roots()).is_err());
        assert!(!exists(&f));
    }

    #[test]
    fn root_prefix_is_component_wise() {
        let root = PathBuf::from("/x/tmp");
        assert!(is_under_any_root(
            Path::new("/x/tmp/a"),
            std::slice::from_ref(&root)
        ));
        assert!(!is_under_any_root(
            Path::new("/x/tmpfoo/a"),
            std::slice::from_ref(&root)
        ));
        assert!(!is_under_any_root(
            Path::new("/x/tmp"),
            std::slice::from_ref(&root)
        ));
    }

    #[test]
    fn production_roots_are_canonical_and_include_tmp() {
        let roots = temp_roots();
        assert!(roots.contains(&std::fs::canonicalize("/tmp").unwrap()));
        for r in &roots {
            assert_eq!(&std::fs::canonicalize(r).unwrap(), r);
        }
    }
}
