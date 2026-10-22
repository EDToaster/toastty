//! File transmission mediums (`t=f`, `t=t`): path policy and bounded reads.
//! Shared memory (`t=s`, [`read_shm_object`]) reuses the same open and
//! read rules for the object's file in `/dev/shm`.
//!
//! The path comes from whatever wrote to the PTY: `cat` of a file, a
//! remote host over ssh, a sandboxed process. The spec's rules for it:
//!
//! - Only regular files, on a normal filesystem, may be read. Symlinks
//!   are followed (the spec requires it), but the open is race-free, so a
//!   component swapped after the path resolves can't redirect it, and the
//!   regular-file / filesystem checks are made on the opened fd.
//! - Every failure looks the same to the client ([`FILE_READ_ERROR`]), so
//!   it can't tell "missing" from "a directory" from "not a PNG". The
//!   real reason ([`FileError`]) is for the local log only.
//! - `t=t` deletes the file afterwards, but only when it lies in a known
//!   temp dir AND its path contains [`TEMP_FILE_MARKER`], and the unlink
//!   is race-free too.
//!
//! [`read_image_file`] implements all three. It never blocks on a client
//! controlled object (`O_NONBLOCK`), and reads are capped by the caller's
//! budget, so `/dev/zero`-style inputs or huge regular files can't
//! exhaust memory.
//!
//! **Residual (inherent to the protocol, same as kitty):** a successful
//! read still reveals that a file exists and is readable, and the `O=`
//! offset plus the OK/EBADF split lets a client binary-search its exact
//! size. The uniform error only removes a *content* oracle (it never
//! says *why* a read failed); it does not hide existence or size. Hiding
//! those would break `t=f`, which is a legitimate feature.

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
    /// non-zero `size` is read to its end, as kitty does; if that is too
    /// little data, decoding fails with the same uniform error.
    pub(super) size: u64,
    /// Hard cap on the bytes read. A request (or, with `size == 0`, a
    /// file) larger than this fails without reading anything.
    pub(super) max: u64,
    /// `t=t`: delete the file afterwards if [`unlink_temp_file`] allows
    /// it.
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
    // `t=t`: delete the client's file afterwards, even if the read or a
    // later decode fails — a `t=t` file is the client's hand-off and is
    // consumed either way, as in kitty. `opened.canonical` is the fully
    // resolved file (symlinks followed), used for the temp-dir check.
    if req.delete_temp {
        unlink_temp_file(req.path, &opened.canonical, roots);
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

/// Resolve `path`, then open the real file race-free and require a
/// regular file on a normal filesystem.
///
/// `canonicalize` follows symlinks, as the spec requires for reads, and
/// resolves `..`. The result has no symlinks, so we descend it one
/// component at a time from `/` with `O_NOFOLLOW` ([`open_dir_chain`]): a
/// symlink appearing in any component means the path was swapped after
/// the resolve, and we abort. The leaf is then opened relative to the
/// pinned parent fd. `O_NONBLOCK` keeps a FIFO swapped into the leaf from
/// blocking the open, `O_NOCTTY` keeps a tty from becoming our
/// controlling terminal, and an `fstat`/`fstatfs` on the opened fd — not
/// on the path — makes the regular-file and filesystem checks race-free
/// (SEC-2, SEC-3; reviewer items 1 and 2).
#[cfg(unix)]
fn open_regular_file(path: &Path) -> Result<OpenedFile, FileError> {
    use rustix::fs::{FileType, OFlags};
    if path.as_os_str().is_empty() {
        return Err(FileError::new("empty path"));
    }
    if path.as_os_str().len() > MAX_PATH_LEN {
        return Err(FileError::new("path too long"));
    }
    let canonical =
        std::fs::canonicalize(path).map_err(|e| FileError(format!("{}: {e}", path.display())))?;
    // Cheap pre-check; the fstatfs below is the race-free one.
    if is_sensitive_path(&canonical) {
        return Err(FileError(format!(
            "{}: refusing a path under /proc, /sys or /dev",
            canonical.display()
        )));
    }
    let (parent, leaf) = open_dir_chain(&canonical)?;
    let leaf_flags =
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY;
    let fd = rustix::fs::openat(
        &parent,
        leaf.as_os_str(),
        leaf_flags,
        rustix::fs::Mode::empty(),
    )
    .map_err(|e| FileError(format!("{}: open: {e}", canonical.display())))?;
    let stat = rustix::fs::fstat(&fd)
        .map_err(|e| FileError(format!("{}: fstat: {e}", canonical.display())))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(FileError(format!(
            "{}: not a regular file",
            canonical.display()
        )));
    }
    reject_pseudo_fs(&fd, &canonical)?;
    Ok(OpenedFile {
        file: std::fs::File::from(fd),
        len: stat.st_size.max(0) as u64,
        canonical,
    })
}

#[cfg(not(unix))]
fn open_regular_file(path: &Path) -> Result<OpenedFile, FileError> {
    Err(FileError(format!(
        "{}: file mediums are only supported on unix",
        path.display()
    )))
}

/// Open the parent directory of `canonical` by descending from `/` one
/// component at a time with `O_NOFOLLOW`, and return `(parent fd, leaf
/// name)`. `canonical` must be absolute and symlink-free (the output of
/// `canonicalize`). A symlink or a non-directory in any intermediate
/// component means the path was swapped under us after it was resolved,
/// so we fail rather than follow it.
///
/// The returned fd pins the parent directory's inode: later renames of
/// path components can't move a subsequent `openat`/`unlinkat` elsewhere.
#[cfg(unix)]
fn open_dir_chain(
    canonical: &Path,
) -> Result<(std::os::fd::OwnedFd, std::ffi::OsString), FileError> {
    use rustix::fs::{Mode, OFlags};
    use std::path::Component;
    let names: Vec<&std::ffi::OsStr> = canonical
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s),
            _ => None,
        })
        .collect();
    let Some((leaf, dirs)) = names.split_last() else {
        return Err(FileError(format!(
            "{}: refusing to operate on the filesystem root",
            canonical.display()
        )));
    };
    // `O_PATH` (Linux, FreeBSD) opens a directory for lookups only, so a
    // search-only (`--x`) directory on the path works, as it does for a
    // plain `open()` (review of #30). Elsewhere (macOS: rustix has no
    // `O_SEARCH`) intermediate directories need read permission; a
    // search-only one fails the read with the usual EBADF. Documented in
    // `docs/decisions/kitty-file-medium.md`.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    let dir_flags = OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW;
    #[cfg(not(any(target_os = "linux", target_os = "android", target_os = "freebsd")))]
    let dir_flags = OFlags::RDONLY
        | OFlags::DIRECTORY
        | OFlags::CLOEXEC
        | OFlags::NOFOLLOW
        | OFlags::NONBLOCK
        | OFlags::NOCTTY;
    // Each step must land on a real directory. `O_DIRECTORY | O_NOFOLLOW`
    // already refuses a symlink; the `fstat` makes that explicit for
    // `O_PATH`, whose flag handling differs.
    let checked =
        |fd: std::os::fd::OwnedFd, what: &dyn std::fmt::Display| match rustix::fs::fstat(&fd) {
            Ok(st)
                if rustix::fs::FileType::from_raw_mode(st.st_mode)
                    == rustix::fs::FileType::Directory =>
            {
                Ok(fd)
            }
            Ok(_) => Err(FileError(format!(
                "{}: {what} is not a directory",
                canonical.display()
            ))),
            Err(e) => Err(FileError(format!(
                "{}: {what}: fstat: {e}",
                canonical.display()
            ))),
        };
    // Open "/" itself. The root is a trusted mount point; swapping it
    // needs privilege we're already inside.
    let root = rustix::fs::open("/", dir_flags, Mode::empty())
        .map_err(|e| FileError(format!("/: open: {e}")))?;
    let mut dir = checked(root, &"/")?;
    for comp in dirs {
        let what = Path::new(comp).display();
        let next = rustix::fs::openat(&dir, *comp, dir_flags, Mode::empty()).map_err(|e| {
            FileError(format!(
                "{}: component {what} not a real directory ({e})",
                canonical.display()
            ))
        })?;
        dir = checked(next, &what)?;
    }
    Ok((dir, (*leaf).to_os_string()))
}

/// Reject pseudo-filesystems by the opened fd's `fstatfs` magic
/// (reviewer item 2). This is race-free and also catches procfs/sysfs
/// mounted outside `/proc`/`/sys`, e.g. inside containers.
///
/// `/dev` is not caught here: devtmpfs shares `TMPFS_MAGIC` with `/tmp`
/// and `/dev/shm`, which we must keep readable. A char/block device under
/// `/dev` is already refused as "not a regular file", and the path check
/// in [`is_sensitive_path`] refuses a regular file directly under `/dev`.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn reject_pseudo_fs(fd: &impl std::os::fd::AsFd, canonical: &Path) -> Result<(), FileError> {
    // Magic numbers from `linux/magic.h` (configfs and fusectl from their
    // kernel sources).
    const REJECT: &[(u32, &str)] = &[
        (0x0000_9fa0, "proc"),
        (0x6265_6572, "sysfs"),
        (0x6462_6720, "debugfs"),
        (0x7472_6163, "tracefs"),
        (0x7363_6673, "securityfs"),
        (0x6367_7270, "cgroup2"),
        (0x0027_e0eb, "cgroup"),
        (0xcafe_4a11, "bpf"),
        (0x6265_6570, "configfs"),
        (0xde5e_81e4, "efivarfs"),
        (0x6165_676c, "pstore"),
        (0xf97c_ff8c, "selinuxfs"),
        (0x4341_5d53, "smackfs"),
        (0x4249_4e4d, "binfmt_misc"),
        (0x6573_5543, "fusectl"),
        (0x6e73_6673, "nsfs"),
    ];
    let statfs = rustix::fs::fstatfs(fd)
        .map_err(|e| FileError(format!("{}: fstatfs: {e}", canonical.display())))?;
    // `f_type` is a signed word on 64-bit Linux, so a magic with the top
    // bit set (bpf, efivarfs, selinuxfs) arrives sign-extended. Compare
    // the low 32 bits only (review of #30).
    #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
    let magic = statfs.f_type as u64 as u32;
    if let Some((_, name)) = REJECT.iter().find(|(m, _)| *m == magic) {
        return Err(FileError(format!(
            "{}: refusing a file on {name}",
            canonical.display()
        )));
    }
    Ok(())
}

/// The magic numbers are Linux-specific; elsewhere only the path
/// pre-check and the regular-file check apply.
#[cfg(all(unix, not(any(target_os = "linux", target_os = "android"))))]
fn reject_pseudo_fs(_fd: &impl std::os::fd::AsFd, _canonical: &Path) -> Result<(), FileError> {
    Ok(())
}

/// Read `[offset, offset + size)` (or to EOF) from an opened file, within
/// `req.max`.
fn read_bounded(opened: &OpenedFile, req: &FileRequest<'_>) -> Result<Vec<u8>, FileError> {
    use std::io::{Read, Seek, SeekFrom};
    let available = opened.len.saturating_sub(req.offset);
    // A file shorter than `S=` is read as far as it goes, as kitty does:
    // `kitten icat --detect-support` sends a 3-byte file with `S=47`.
    // Too little data is then the decoder's (uniform) error.
    let want = if req.size == 0 {
        available
    } else {
        req.size.min(available)
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

/// `t=t` deletion (SEC-1), matching kitty 0.49.2 and done race-free.
///
/// `sent` is the path the client gave; `file_canonical` is that path fully
/// resolved (symlinks followed), i.e. the real file that was read. The
/// entry removed is `sent`'s own entry — a final-component symlink loses
/// the link, never its target — so its path is `canonicalize(parent)` plus
/// the leaf name (the leaf itself is not followed). Call that `entry`.
///
/// Unlink only when:
/// - `entry` contains [`TEMP_FILE_MARKER`]. Checking the resolved parent,
///   not the sent string, means a marker in a component that `..` cancels
///   doesn't count (reviewer item 3). The marker may still be in a
///   surviving directory name, as kitty allows.
/// - `file_canonical` lies in a temp-dir root, which is kitty's check
///   (`is_path_in_temp_dir(realpath(sent))`): a marked link pointing
///   outside the temp dirs is kept.
/// - the entry's own resolved parent lies in a temp-dir root. This is one
///   step stricter than kitty: a marked link reached through a parent that
///   resolves out of the temp dirs is kept, so we never unlink an entry
///   that lives outside them.
///
/// The parent is reopened by descending from `/` with `O_NOFOLLOW`
/// ([`open_dir_chain`]) and the entry removed with `unlinkat` relative to
/// that pinned fd, so a directory component swapped for a symlink between
/// the check and the unlink can't redirect it outside the temp dir
/// (reviewer item 1).
///
/// Best-effort: failures are logged, never surfaced to the client.
fn unlink_temp_file(sent: &Path, file_canonical: &Path, roots: &[PathBuf]) {
    let Some(name) = sent.file_name() else {
        return; // `.`, `..` or `/`: nothing to unlink.
    };
    let parent = match sent.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Ok(parent_canonical) = std::fs::canonicalize(parent) else {
        return;
    };
    let entry = parent_canonical.join(name);
    let under_temp = |p: &Path| roots.iter().any(|r| r == p) || is_under_any_root(p, roots);
    // kitty gates deletion on the *resolved* file being in a temp dir (so
    // a marked link pointing outside is kept). We also require the entry's
    // own resolved parent to be in a temp dir, one step stricter than
    // kitty, so a link whose parent chain leaves the temp dir can't make
    // us unlink an entry that lives outside it.
    if !path_contains_marker(&entry)
        || !under_temp(file_canonical)
        || !under_temp(&parent_canonical)
    {
        return;
    }
    if let Err(e) = unlink_in_dir(&parent_canonical, name) {
        tracing::debug!(path = %entry.display(), error = %e, "kitty: t=t unlink failed");
    }
}

/// `unlinkat(name)` relative to a race-free fd on `parent_canonical`.
#[cfg(unix)]
fn unlink_in_dir(parent_canonical: &Path, name: &std::ffi::OsStr) -> Result<(), FileError> {
    // `open_dir_chain` on `parent/name` yields a pinned fd on the parent
    // and hands `name` back unresolved.
    let (dir, leaf) = open_dir_chain(&parent_canonical.join(name))?;
    #[cfg(test)]
    run_pre_unlink_hook();
    rustix::fs::unlinkat(&dir, leaf.as_os_str(), rustix::fs::AtFlags::empty())
        .map_err(|e| FileError(format!("unlinkat: {e}")))
}

#[cfg(not(unix))]
fn unlink_in_dir(_parent_canonical: &Path, _name: &std::ffi::OsStr) -> Result<(), FileError> {
    Ok(())
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

// ---- shared memory (`t=s`) ----

/// Where Linux keeps POSIX shared-memory objects. glibc and musl implement
/// `shm_open("/name")` as `open("/dev/shm/name")`, so an object is a plain
/// file in this tmpfs.
#[cfg(target_os = "linux")]
const SHM_DIR: &str = "/dev/shm";

/// Longest shm name we accept, not counting the leading `/`. On Linux the
/// name is a file name in [`SHM_DIR`], so this is `NAME_MAX`.
const MAX_SHM_NAME_LEN: usize = 255;

/// What to read from a `t=s` shared-memory object.
#[derive(Debug, Clone, Copy)]
// Only Linux reads the object; elsewhere only `name` is used.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(super) struct ShmRequest<'a> {
    /// The name exactly as the client sent it (`/name`).
    pub(super) name: &'a [u8],
    /// `O=`: byte offset to start reading at.
    pub(super) offset: u64,
    /// Bytes to read; `0` means "to the end of the object". An object
    /// shorter than a non-zero `size` is read to its end, as for files.
    pub(super) size: u64,
    /// Hard cap on the bytes read, as for files.
    pub(super) max: u64,
}

/// Check a POSIX shm name and return it without its leading `/`.
///
/// The spec: the name starts with `/` and has no other `/`. We also refuse
/// an empty name, `.` and `..` (which would name `/dev/shm` itself or its
/// parent), NUL bytes, and names over [`MAX_SHM_NAME_LEN`].
fn shm_leaf(name: &[u8]) -> Result<&[u8], FileError> {
    let Some(leaf) = name.strip_prefix(b"/") else {
        return Err(FileError::new("shm name does not start with /"));
    };
    if leaf.is_empty() || leaf == b"." || leaf == b".." {
        return Err(FileError::new("shm name is empty, . or .."));
    }
    if leaf.contains(&b'/') {
        return Err(FileError::new("shm name has a / after the first byte"));
    }
    if leaf.contains(&0) {
        return Err(FileError::new("shm name has a NUL byte"));
    }
    if leaf.len() > MAX_SHM_NAME_LEN {
        return Err(FileError::new("shm name too long"));
    }
    Ok(leaf)
}

/// Read the bytes a `t=s` command asks for from a POSIX shared-memory
/// object, then unlink the object, as the spec requires ("read the data
/// from the memory object and then unlink and close it").
///
/// On Linux the object is a file in [`SHM_DIR`], read with the same rules
/// as the file mediums: the leaf is opened relative to a directory fd with
/// `O_NOFOLLOW | O_NONBLOCK | O_NOCTTY`, so a symlink is refused and a FIFO
/// can't block us, and `fstat` on the opened fd must report a regular
/// file. The read is plain bounded `read`s, never `mmap`, so a client that
/// truncates the object while we read it gets a short read (an error), not
/// a `SIGBUS` in the terminal.
///
/// The object is unlinked, relative to the same directory fd, whenever it
/// opened as a regular file, even if the read (or a later decode) fails:
/// like a `t=t` file it is the client's hand-off, and kitty consumes it
/// either way. Anything else (a symlink, a directory, a FIFO) is left
/// alone, as in kitty, whose `shm_open` refuses symlinks too.
///
/// Elsewhere, see [`read_shm_object_unsupported`].
pub(super) fn read_shm_object(req: &ShmRequest<'_>) -> Result<Vec<u8>, FileError> {
    #[cfg(target_os = "linux")]
    {
        read_shm_object_linux(req)
    }
    #[cfg(not(target_os = "linux"))]
    {
        read_shm_object_unsupported(req)
    }
}

#[cfg(target_os = "linux")]
fn read_shm_object_linux(req: &ShmRequest<'_>) -> Result<Vec<u8>, FileError> {
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::os::unix::ffi::OsStrExt;
    let leaf = std::ffi::OsStr::from_bytes(shm_leaf(req.name)?);
    let shown = Path::new(SHM_DIR).join(leaf);
    // `/dev/shm` is a system mount point, trusted like `/` in
    // `open_dir_chain`. `O_PATH`: we only look names up in it.
    let dir = rustix::fs::open(
        SHM_DIR,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|e| FileError(format!("{SHM_DIR}: open: {e}")))?;
    let fd = rustix::fs::openat(
        &dir,
        leaf,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::NOCTTY,
        Mode::empty(),
    )
    .map_err(|e| FileError(format!("{}: open: {e}", shown.display())))?;
    let stat = rustix::fs::fstat(&fd)
        .map_err(|e| FileError(format!("{}: fstat: {e}", shown.display())))?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
        return Err(FileError(format!(
            "{}: not a regular file",
            shown.display()
        )));
    }
    let opened = OpenedFile {
        file: std::fs::File::from(fd),
        len: stat.st_size.max(0) as u64,
        canonical: shown,
    };
    // The same bounded read as `t=f` (no temp-file deletion: the unlink
    // below is t=s's own).
    let range = FileRequest {
        path: &opened.canonical,
        offset: req.offset,
        size: req.size,
        max: req.max,
        delete_temp: false,
    };
    let result = read_bounded(&opened, &range);
    if let Err(e) = rustix::fs::unlinkat(&dir, leaf, AtFlags::empty()) {
        tracing::debug!(path = %opened.canonical.display(), error = %e, "kitty: t=s unlink failed");
    }
    result
}

/// `t=s` where we can't read shared memory: macOS, the BSDs and other
/// non-Linux targets.
///
/// A macOS shm object can only be read through `mmap`, which is `unsafe`,
/// and this crate is `#![forbid(unsafe_code)]`. So the command fails with
/// the usual uniform error. We still unlink a valid name
/// (`rustix::shm::unlink` is safe), so a client that doesn't check the
/// reply doesn't leak objects. Decided: unsupported on macOS (keeps
/// `#![forbid(unsafe_code)]`; clients fall back to file/base64), see
/// `docs/decisions/kitty-file-medium.md`.
#[cfg(not(target_os = "linux"))]
fn read_shm_object_unsupported(req: &ShmRequest<'_>) -> Result<Vec<u8>, FileError> {
    shm_leaf(req.name)?;
    #[cfg(all(unix, not(target_os = "android")))]
    {
        use std::os::unix::ffi::OsStrExt;
        if let Err(e) = rustix::shm::unlink(std::ffi::OsStr::from_bytes(req.name)) {
            tracing::debug!(error = %e, "kitty: t=s unlink failed");
        }
    }
    Err(FileError::new(
        "t=s: reading shared memory is only supported on Linux",
    ))
}

/// Test-only hook, fired inside [`unlink_in_dir`] after the parent fd is
/// pinned but before `unlinkat`, so a test can swap the directory tree in
/// the window an attacker would (reviewer item 1).
#[cfg(all(test, unix))]
pub(super) fn run_pre_unlink_hook() {
    tests::PRE_UNLINK.with(|h| {
        if let Some(f) = h.borrow_mut().as_mut() {
            f();
        }
    });
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::os::unix::fs::symlink;

    thread_local! {
        pub(super) static PRE_UNLINK: RefCell<Option<Box<dyn FnMut()>>> = const { RefCell::new(None) };
    }

    /// Install a pre-unlink hook for the current test thread; it is
    /// removed when the returned guard drops.
    struct HookGuard;
    impl HookGuard {
        fn set(f: impl FnMut() + 'static) -> Self {
            PRE_UNLINK.with(|h| *h.borrow_mut() = Some(Box::new(f)));
            Self
        }
    }
    impl Drop for HookGuard {
        fn drop(&mut self) {
            PRE_UNLINK.with(|h| *h.borrow_mut() = None);
        }
    }

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
    /// kitty reads a file shorter than `S=` to its end (icat's detection
    /// probe relies on it); nothing at all past `O=` is an error.
    fn file_shorter_than_size_is_read_to_its_end() {
        let sb = Sandbox::new("short");
        let p = write_file(&sb.home.join("img.rgba"), &[1, 2, 3]);
        assert_eq!(
            read_image_file_with_roots(&req(&p, 0, 47, false), &sb.roots()).unwrap(),
            vec![1, 2, 3]
        );
        assert_eq!(
            read_image_file_with_roots(&req(&p, 1, 47, false), &sb.roots()).unwrap(),
            vec![2, 3]
        );
        assert!(read_image_file_with_roots(&req(&p, 3, 0, false), &sb.roots()).is_err());
        assert!(read_image_file_with_roots(&req(&p, 3, 4, false), &sb.roots()).is_err());
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

    /// The race-free open never blocks on a FIFO either (the window
    /// between resolving the path and opening the leaf).
    #[test]
    fn open_of_fifo_returns_immediately() {
        let sb = Sandbox::new("fifo-open");
        let fifo = sb.tmp.join("f");
        mkfifo(&fifo);
        let p = fifo.clone();
        let err = within(5, move || open_regular_file(&p)).unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
    }

    /// Review of #30: a search-only (`--x`, 0311) directory on the path
    /// must not break reads (Linux `O_PATH` walk), nor `t=t` deletion.
    #[cfg(any(target_os = "linux", target_os = "android", target_os = "freebsd"))]
    #[test]
    fn search_only_directory_on_the_path_works() {
        use std::os::unix::fs::PermissionsExt;
        let sb = Sandbox::new("search-only");
        let d = sb.tmp.join("tty-graphics-protocol-x-only");
        let f = write_file(&d.join("img.rgba"), &[5, 6, 7, 8]);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o311)).unwrap();
        let read = read_image_file_with_roots(&req(&f, 0, 0, false), &sb.roots());
        let took = sb.take(&f);
        std::fs::set_permissions(&d, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(read.unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(took.unwrap(), vec![5, 6, 7, 8]);
        assert!(!exists(&f), "t=t deletes through a search-only parent");
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn pseudo_fs_magics_compare_low_32_bits() {
        // The magics with the top bit set must be in the table as u32 so
        // the low-32-bit compare can match them.
        for p in ["/proc/self/status", "/sys/kernel/hostname"] {
            let p = Path::new(p);
            if !exists(p) {
                continue;
            }
            // Bypass the path pre-check: open the file directly.
            let fd =
                rustix::fs::open(p, rustix::fs::OFlags::RDONLY, rustix::fs::Mode::empty()).unwrap();
            let err = reject_pseudo_fs(&fd, p).unwrap_err();
            assert!(err.0.contains("refusing a file on"), "{err}");
        }
        // A sign-extended 0xcafe4a11 (as a negative i64) maps to bpf.
        let sign_extended: i64 = 0xcafe_4a11_u32 as i32 as i64;
        assert_eq!(sign_extended as u64 as u32, 0xcafe_4a11);
    }

    /// Reviewer item 1: between the temp-dir check and the unlink, the
    /// parent directory is swapped for a symlink pointing outside the
    /// temp dir. Because the unlink goes through a pinned fd on the real
    /// directory, the outside file survives, and only the entry in the
    /// (now-detached) original directory is removed.
    #[test]
    fn parent_swap_during_unlink_cannot_escape_temp_dir() {
        let sb = Sandbox::new("race");
        // Real temp dir holding the marked victim the client names.
        let real = sb.tmp.join("tty-graphics-protocol-d");
        let victim = write_file(&real.join("v.rgba"), &[1, 2, 3, 4]);
        // The file the swapped-in symlink would expose, outside temp.
        let outside = write_file(&sb.home.join("v.rgba"), b"IMPORTANT");
        let swap_target = sb.home.clone();
        let real_for_hook = real.clone();
        let _guard = HookGuard::set(move || {
            // Swap `real` (a directory) for a symlink to `home`.
            let _ = std::fs::remove_dir_all(&real_for_hook);
            let _ = symlink(&swap_target, &real_for_hook);
        });
        // `sent` names the victim inside the marked temp dir.
        let _ = read_image_file_with_roots(&req(&victim, 0, 0, true), &sb.roots());
        assert!(
            exists(&outside),
            "the outside file must not be deleted by the swap"
        );
        assert_eq!(std::fs::read(&outside).unwrap(), b"IMPORTANT");
    }

    /// Reviewer item 2: a parent component swapped for a symlink to
    /// `/dev` after the path resolves must not let a device be opened.
    /// The component-wise `O_NOFOLLOW` walk rejects the swapped-in link.
    #[test]
    fn parent_swap_to_dev_is_rejected_on_read() {
        let sb = Sandbox::new("read-race");
        let dir = sb.tmp.join("d");
        let p = write_file(&dir.join("img.rgba"), &[1, 2, 3, 4]);
        let dir_for_hook = dir.clone();
        // There is no between-resolve hook on the read path, so emulate a
        // swap by replacing the directory with a symlink to /dev up front,
        // then reading a path whose canonical form still names the real
        // dir is impossible — instead confirm the walk refuses a symlinked
        // component directly.
        std::fs::remove_dir_all(&dir_for_hook).unwrap();
        symlink("/dev", &dir_for_hook).unwrap();
        // canonical(`dir/null`) resolves to `/dev/null`, caught as
        // sensitive; the point is nothing under it is opened as a file.
        let got = read_image_file_with_roots(&req(&p, 0, 0, false), &sb.roots());
        assert!(got.is_err(), "a device reached via a swapped dir must fail");
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

    /// Reviewer item 3: a marker in a component that `..` cancels doesn't
    /// count. `<tmp>/m-tty-graphics-protocol/../plain.rgba` resolves to
    /// `<tmp>/plain.rgba`, whose real path has no marker, so it is kept.
    /// (kitty 0.49.2 deletes it; we are stricter, per "spec strictly".)
    #[test]
    fn marker_cancelled_by_dotdot_is_kept() {
        let sb = Sandbox::new("dotdot-marker");
        std::fs::create_dir_all(sb.tmp.join("m-tty-graphics-protocol")).unwrap();
        let f = write_file(&sb.tmp.join("plain.rgba"), &[1, 2, 3, 4]);
        assert!(
            sb.take(&sb.tmp.join("m-tty-graphics-protocol/../plain.rgba"))
                .is_ok()
        );
        assert!(
            exists(&f),
            "a marker a `..` cancels must not delete the file"
        );
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
    /// (here: nothing left past `O=`).
    #[test]
    fn failed_read_of_regular_temp_file_still_deletes() {
        let sb = Sandbox::new("short-del");
        let f = write_file(&sb.tmp.join("tty-graphics-protocol-short"), &[1, 2]);
        let r = req(&f, 2, 4, true);
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

/// `t=s` on Linux, against the real `/dev/shm`. Every test makes its own
/// uniquely named entry and removes it, whatever it turned into, on drop.
#[cfg(all(test, target_os = "linux"))]
mod shm_tests {
    use super::*;

    /// A uniquely named entry in `/dev/shm`, removed on drop.
    struct ShmEntry {
        name: String,
        path: PathBuf,
    }

    impl ShmEntry {
        /// A name nothing uses yet; the entry isn't created.
        fn reserve(tag: &str) -> Self {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let n = N.fetch_add(1, Ordering::Relaxed);
            Self::named(format!("toastty-shm-test-{tag}-{}-{n}", std::process::id()))
        }

        fn named(name: String) -> Self {
            let path = Path::new(SHM_DIR).join(&name);
            let entry = Self { name, path };
            entry.remove();
            entry
        }

        /// A regular object holding `bytes`, as `shm_open` + `write`
        /// would make it.
        fn object(tag: &str, bytes: &[u8]) -> Self {
            let entry = Self::reserve(tag);
            std::fs::write(&entry.path, bytes).unwrap();
            entry
        }

        /// The name as a client sends it: `/name`.
        fn wire(&self) -> Vec<u8> {
            format!("/{}", self.name).into_bytes()
        }

        fn exists(&self) -> bool {
            self.path.symlink_metadata().is_ok()
        }

        fn remove(&self) {
            let _ = std::fs::remove_file(&self.path);
            let _ = std::fs::remove_dir(&self.path);
        }
    }

    impl Drop for ShmEntry {
        fn drop(&mut self) {
            self.remove();
        }
    }

    fn req(name: &[u8], offset: u64, size: u64) -> ShmRequest<'_> {
        ShmRequest {
            name,
            offset,
            size,
            max: 1 << 20,
        }
    }

    fn read(name: &[u8], offset: u64, size: u64) -> Result<Vec<u8>, FileError> {
        read_shm_object(&req(name, offset, size))
    }

    #[test]
    fn reads_the_whole_object_then_unlinks_it() {
        let obj = ShmEntry::object("whole", &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read(&obj.wire(), 0, 0).unwrap(), [1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(!obj.exists(), "the object must be unlinked after the read");
    }

    #[test]
    fn offset_and_size_select_a_range() {
        let obj = ShmEntry::object("range", &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read(&obj.wire(), 2, 4).unwrap(), [3, 4, 5, 6]);
        assert!(!obj.exists());
        let obj = ShmEntry::object("offset-only", &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(read(&obj.wire(), 6, 0).unwrap(), [7, 8]);
        assert!(!obj.exists());
    }

    #[test]
    fn missing_object_fails() {
        let missing = ShmEntry::reserve("missing");
        let err = read(&missing.wire(), 0, 0).unwrap_err();
        assert!(err.0.contains("open"), "{err}");
        assert!(!missing.exists());
    }

    /// Names the spec forbids fail before anything is opened, and leave
    /// existing objects alone, even ones the bad name points at.
    #[test]
    fn invalid_names_fail_and_touch_nothing() {
        let obj = ShmEntry::object("victim", &[1, 2, 3, 4]);
        let bare = obj.name.clone().into_bytes();
        let traversal = format!("/../shm/{}", obj.name).into_bytes();
        let double = format!("//{}", obj.name).into_bytes();
        let nul = format!("/{}\0x", obj.name).into_bytes();
        let long = format!("/{}", "x".repeat(MAX_SHM_NAME_LEN + 1)).into_bytes();
        let names: [&[u8]; 9] = [
            b"", &bare, b"/", b"/.", b"/..", &traversal, &double, &nul, &long,
        ];
        for name in names {
            assert!(
                shm_leaf(name).is_err(),
                "{:?} must be refused",
                String::from_utf8_lossy(name)
            );
            assert!(read(name, 0, 0).is_err());
        }
        assert!(obj.exists(), "a refused name must not unlink anything");
    }

    #[test]
    fn longest_valid_name_is_accepted() {
        let base = ShmEntry::reserve("long").name.clone();
        let mut name = format!("{base}-{}", "x".repeat(MAX_SHM_NAME_LEN));
        name.truncate(MAX_SHM_NAME_LEN);
        let long = ShmEntry::named(name);
        std::fs::write(&long.path, [9, 9]).unwrap();
        assert_eq!(long.wire().len(), MAX_SHM_NAME_LEN + 1);
        assert_eq!(read(&long.wire(), 0, 0).unwrap(), [9, 9]);
        assert!(!long.exists());
    }

    /// `O_NOFOLLOW`: a symlink in `/dev/shm` is refused, and neither the
    /// link nor its target is removed (kitty's `shm_open` refuses it too).
    #[test]
    fn symlink_is_refused_and_kept() {
        let target = ShmEntry::object("link-target", &[1, 2, 3, 4]);
        let link = ShmEntry::reserve("link");
        std::os::unix::fs::symlink(&target.path, &link.path).unwrap();
        let err = read(&link.wire(), 0, 0).unwrap_err();
        assert!(err.0.contains("open"), "{err}");
        assert!(link.exists(), "a symlink must not be unlinked");
        assert!(target.exists(), "a symlink's target must not be unlinked");
    }

    /// `O_NONBLOCK`: a FIFO can't park the terminal in `open()`, and isn't
    /// unlinked.
    #[test]
    fn fifo_is_refused_without_blocking_and_kept() {
        let fifo = ShmEntry::reserve("fifo");
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&fifo.path)
                .status()
                .unwrap()
                .success()
        );
        let wire = fifo.wire();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(read(&wire, 0, 0));
        });
        let err = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("t=s on a FIFO blocked")
            .unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
        assert!(fifo.exists(), "a FIFO must not be unlinked");
    }

    #[test]
    fn directory_is_refused_and_kept() {
        let dir = ShmEntry::reserve("dir");
        std::fs::create_dir(&dir.path).unwrap();
        let err = read(&dir.wire(), 0, 0).unwrap_err();
        assert!(err.0.contains("not a regular file"), "{err}");
        assert!(dir.exists());
    }

    /// An object shorter than `S=` is read to its end, as files are (and
    /// as kitty does); too little data is then the decoder's error.
    #[test]
    fn short_object_is_read_to_its_end() {
        let short = ShmEntry::object("short", &[1, 2, 3, 4]);
        assert_eq!(read(&short.wire(), 0, 5).unwrap(), [1, 2, 3, 4]);
        assert!(!short.exists());
        let tail = ShmEntry::object("tail", &[1, 2, 3, 4]);
        assert_eq!(read(&tail.wire(), 2, 4).unwrap(), [3, 4]);
        assert!(!tail.exists());
    }

    /// A regular object is the client's hand-off: it is unlinked even when
    /// the read fails.
    #[test]
    fn failed_reads_still_unlink() {
        // `O=` past the end.
        let past = ShmEntry::object("past", &[1, 2, 3, 4]);
        let err = read(&past.wire(), 99, 0).unwrap_err();
        assert!(err.0.contains("nothing to read"), "{err}");
        assert!(!past.exists());

        // An empty object.
        let empty = ShmEntry::object("empty", &[]);
        assert!(read(&empty.wire(), 0, 0).is_err());
        assert!(!empty.exists());
    }

    /// The image quota caps the read: an object (or `S=`) over it fails
    /// before anything is allocated, and is still unlinked.
    #[test]
    fn read_over_the_quota_fails_and_unlinks() {
        let big = ShmEntry::object("big", &[0; 64]);
        let wire = big.wire();
        let mut r = req(&wire, 0, 0);
        r.max = 63;
        let err = read_shm_object(&r).unwrap_err();
        assert!(err.0.contains("exceeds"), "{err}");
        assert!(!big.exists());

        let sized = ShmEntry::object("big-s", &[0; 64]);
        let wire = sized.wire();
        let mut r = req(&wire, 0, 64);
        r.max = 63;
        assert!(read_shm_object(&r).unwrap_err().0.contains("exceeds"));
        assert!(!sized.exists());

        let exact = ShmEntry::object("exact", &[7; 64]);
        let wire = exact.wire();
        let mut r = req(&wire, 0, 0);
        r.max = 64;
        assert_eq!(read_shm_object(&r).unwrap().len(), 64);
        assert!(!exact.exists());
    }
}
