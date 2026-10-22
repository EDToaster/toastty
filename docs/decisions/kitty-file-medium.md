# Decision: Kitty file mediums and upload limits

Date: 2026-10-21. Author: kgp-B (KGP fix programme, PR B1).
Status: **implemented**. Closes SEC-1 to SEC-7 in
`docs/kitty-graphics-checklist.md`. B4b extends it to shared memory
(`t=s`, WIRE-2) on Linux; see [Shared memory](#shared-memory-ts-wire-2).

## TL;DR

Anything that can write to the PTY can send a kitty graphics command: `cat`
of a file, `curl` output, a remote host over ssh. So `t=f` / `t=t` paths
and every payload are untrusted.

- File reads (`kitty/medium.rs`) resolve the path, then open it race-free
  by descending from `/` one component at a time with `O_NOFOLLOW`, open
  the leaf `O_NONBLOCK | O_NOFOLLOW | O_NOCTTY`, and check the *opened fd*
  with `fstat` (regular file) and `fstatfs` (not a pseudo-filesystem).
  Reads are bounded.
- Every file failure replies `EBADF:Failed to read image file`. The real
  reason goes to the debug log. This removes a *content* oracle only: a
  successful read, and the `O=` offset, still reveal a file's existence
  and exact size, which is inherent to `t=f` and the same in kitty.
- `t=t` deletes only when the entry named contains `tty-graphics-protocol`
  and both the resolved file and its resolved parent lie in a temp dir.
  The unlink goes through a pinned directory fd, so it can't be raced out
  of the temp dir.
- Decoding is bounded by `KittySink::max_image_bytes()`, which `Term` sets
  to the registry cap. This bound covers zlib output and the PNG's native
  decoded size.
- In-flight chunked uploads have caps on their count and their total
  bytes, and expire after a TTL.

The target behaviour is kitty 0.49.2, checked live with
`scripts/kgp-stress/kitty-ref.sh`.

## File reads (SEC-2, SEC-3)

The spec says to check the path *before* opening it, because opening can
have side effects: a FIFO blocks, and a device may act on open. The first
cut (reviewed in #30) checked `canonicalize`, then `stat` and `open`,
which each re-resolve the path — so a directory component swapped for a
symlink between the calls defeated the checks (a TOCTOU the reviewer
demonstrated for the delete path). The open is now race-free:

1. `canonicalize` follows symlinks, as the spec requires for reads, and
   resolves `..`. The result has no symlinks.
2. Cheap pre-check: refuse `/proc`, `/sys`, `/dev` (except
   `/dev/shm/<name>`) by path. The `fstatfs` below is the race-free guard.
3. Descend the canonical path from `/` one component at a time with
   `openat(O_DIRECTORY | O_NOFOLLOW)` (`open_dir_chain`), plus an `fstat`
   that each step is a directory. Since the canonical path has no
   symlinks, a symlink or non-directory in any component means it was
   swapped after the resolve, and we abort. The final directory fd pins
   that inode, so later renames can't move the leaf `openat`/`unlinkat`
   elsewhere.
   - On Linux and FreeBSD the directories are opened with `O_PATH`, which
     needs only search (`x`) permission, so a search-only (`--x`)
     directory on the path works as it does for a plain `open()`.
   - **Limitation:** rustix has no `O_SEARCH` for macOS, so there the
     walk opens directories `O_RDONLY` and a search-only directory on the
     path fails the read (uniform EBADF). Reading through such a
     directory is rare; the alternative would reintroduce the race.
4. Open the leaf relative to that fd with
   `O_RDONLY | O_NONBLOCK | O_NOFOLLOW | O_NOCTTY | O_CLOEXEC`. `NONBLOCK`
   keeps a FIFO from parking the thread, `NOCTTY` keeps a tty from
   becoming our controlling terminal. `rustix` is now a unix-only
   dependency of `toastty-graphics` (workspace feature `fs`).
5. Check the *opened fd*, not the path, so these are race-free: `fstat`
   must report a regular file, and on Linux `fstatfs` must not report a
   pseudo-filesystem (proc, sysfs, debugfs, tracefs, securityfs, cgroup,
   cgroup2, bpf, configfs, efivarfs, pstore, selinuxfs, smackfs,
   binfmt_misc, fusectl, nsfs). The magic is compared on its low 32 bits,
   because `f_type` is a signed word and magics with the top bit set
   arrive sign-extended. `/dev` isn't caught by `fstatfs` — devtmpfs shares its magic with
   `/tmp` and `/dev/shm` — but a device there is already rejected as "not
   a regular file", and the path pre-check refuses a regular file directly
   under `/dev`.
6. Read `S=` bytes, or to EOF, but never more than the quota. A larger
   request fails before it allocates. For uncompressed raw pixels with no
   `S=`, read exactly `s*v*bpp` bytes. kitty does the same: a longer file
   is fine, a shorter one fails.

The read still runs on the PTY-processing thread. Nothing a client sends
can block it any more. The only remaining wait is a slow regular file,
such as one on NFS.

## One error for every file failure (SEC-4)

All of these reply `EBADF:Failed to read image file`:

- a missing path, a directory, no permission, or a symlink loop;
- a special file or a sensitive tree;
- a file shorter than `S=` or `s*v*bpp`, or larger than the quota;
- a bad base64 path or an empty path;
- any inflate or decode failure of bytes that came from a file.

The last one goes further than kitty. kitty answers `EBADPNG` for a
readable file that isn't a PNG. That tells a remote client something
about a local file, so we don't. The PM decided this (spec read strictly).
Replies for direct (`t=d`) payloads are unchanged.

**Residual, inherent to the protocol (and the same in kitty):** the
uniform reply hides only *why* a read failed. It does not hide that a file
exists and is readable — a successful read says so — and a client can
binary-search a file's exact size with `O=` offsets, since the OK/EBADF
split is an at-least-N-bytes oracle. Hiding that would break `t=f`, which
is a legitimate feature. The earlier claim that SEC-4 hides file size was
wrong; it is corrected here and in the code comments.

## Temp-file deletion (SEC-1)

kitty's rule, from `kitty/utils.py` `is_path_in_temp_dir` and
`boss.safe_delete_temp_file`, confirmed live:

- The entry removed — `canonicalize(parent)` joined with the leaf name,
  the leaf not followed — must contain `tty-graphics-protocol`. Checking
  the resolved entry, not the sent string, means a marker in a component
  that `..` cancels doesn't count (`<tmp>/m-tty-graphics-protocol/../x`
  whose real path is `<tmp>/x`). This is one step stricter than kitty,
  which checks the sent string; the PM's "spec strictly" call covers it.
- `realpath(sent)` must be under a temp root (kitty's own check: a marked
  link pointing outside the temp dirs is kept). The roots are `/tmp`,
  `/dev/shm`, `$TMPDIR` and `std::env::temp_dir()`, all canonicalized, and
  the comparison is component-wise, so `/tmpfoo` is not under `/tmp`.
- The sent path's own entry is unlinked. If the sent path is a symlink,
  only the link is removed, never its target.
- The file must have opened as a regular file. When it did, the file is
  deleted even if the read or decode then fails, as kitty does.

Two checks stricter than kitty:

- The resolved parent of the entry must also be under a temp root, so a
  marked link reached through a parent that resolves out of the temp dirs
  is kept — we never unlink an entry that lives outside them.
- The unlink is race-free. The first cut (#30) did
  `remove_file(canonicalize(parent).join(name))`, which re-resolves the
  path; the reviewer won that race by swapping a component for a symlink
  and deleted a file outside every temp dir. Now the parent is reopened
  with the same component-wise `O_NOFOLLOW` walk as reads, and the entry
  removed with `unlinkat` relative to the pinned fd, so a swap after the
  check can't redirect it. A `#[cfg(test)]` hook fires between the check
  and the `unlinkat`; a regression test swaps the parent for a symlink
  there and asserts the outside file survives.

## Decode limits (SEC-5, SEC-7)

- `KittySink::max_image_bytes()` is new. Its default is
  `DEFAULT_MAX_IMAGE_BYTES`, 400 MB, kitty's `MAX_DATA_SZ`. `Term` returns
  the registry cap, which is 320 MiB by default. The registry evicts to
  make room, so the whole cap is what one image may use.
- zlib output stops at `s*v*bpp` for raw formats, or at the quota for PNG
  or when dimensions are missing. We read `limit + 1` bytes through
  `Read::take`. Anything over the limit is `EINVAL:zlib decompress:
  inflated data exceeds N bytes`. When the declared `s*v*bpp` already
  exceeds the quota, we reply `EFBIG` *before* inflating, so an attacker
  can't make us inflate up to the cap for free.
- PNG: a header-only pass builds the decoder and reads its *native*
  decoded size (`ImageDecoder::total_bytes()`), which accounts for bit
  depth and channel count — a 16-bit image is not under-counted the way
  `w*h*4` would. If that exceeds the quota, the reply is `ENOMEM:PNG image
  is too large`, kitty's literal reply, before any pixel data is decoded.
  The image crate's own `max_alloc` is set to the quota too, so an image
  right at the cap is `ENOMEM` rather than a decode error.

## In-flight uploads (SEC-6)

Superseded by B3 (#44), which follows kitty's loading model:

- There is **one** loading slot, as in kitty. While it is busy, every
  direct transmit is a continuation of it (WIRE-9), so the "many
  abandoned uploads" attack can only ever grow one buffer.
- That buffer holds **decoded** bytes (chunks are base64-decoded on
  arrival), and is capped at kitty's own buffer size: `s*v*bpp` plus 10
  bytes (plus about 1 KiB for `o=z`), or the image quota for PNG. Past
  it, the upload is dropped with `EFBIG:Too much data`. Memory in flight
  is therefore at most one image's worth, bounded by the registry cap.
- `PENDING_UPLOAD_TTL = 60 s` after the last chunk still applies: an
  abandoned upload is dropped silently, so it can't capture later images
  forever. Expiry runs at the start of every graphics command and after
  each batch of PTY input, so a quota-sized abandoned upload doesn't stay
  resident when no further graphics command comes. (kitty has no expiry.)
- A chunk's decoded size is bounded from its base64 length before it is
  decoded, so one 256 MiB APC can't make us allocate its decoded size
  just to find it's too big.
- B1's count cap, global byte cap and LRU eviction are gone; with one
  slot they no longer mean anything.

## Shared memory (`t=s`, WIRE-2)

Added in B4b. The body of a `t=s` command is a base64 POSIX shm name.
The spec: the name starts with `/` and has no other `/`, and the terminal
"must read the data from the memory object and then unlink and close
it". `O=` and `S=` work as for files. The code is
`medium::read_shm_object`.

**Linux** (glibc and musl implement `shm_open("/x")` as
`open("/dev/shm/x")`, so an object is a file in that tmpfs):

1. Validate the name: a leading `/`, no other `/`, not empty, `.` or
   `..`, no NUL, at most 255 bytes after the `/` (`NAME_MAX`). Anything
   else fails before any syscall.
2. Open `/dev/shm` (`O_PATH | O_DIRECTORY`), then the leaf relative to
   that fd with `O_RDONLY | O_NOFOLLOW | O_NONBLOCK | O_NOCTTY |
   O_CLOEXEC`. A symlink fails the open (kitty's `shm_open` refuses
   symlinks too), a FIFO can't block it.
3. `fstat` the opened fd and require a regular file.
4. Read the `O=`/`S=` range with the same bounded `read`s as `t=f`,
   capped at the image quota. Like a file (B4a, #49), an object shorter
   than `S=` is read to its end and the decoder judges the data. We never
   `mmap` the object, so a client that truncates it while we read gets a
   short read (EBADF), not a `SIGBUS` in the terminal.
5. `unlinkat` the leaf relative to the same directory fd. This happens
   whenever step 3 passed, even if the read or the later decode fails:
   the object is the client's hand-off, as kitty does. A symlink, a
   directory or a FIFO is never unlinked.

Every failure, including a bad name and a decode failure, replies the
uniform `EBADF:Failed to read image file` (SEC-4), with the reason in the
debug log.

**`a=q,t=s`** (how `kitten icat --detect-support` finds `memory`) goes
through B4a's medium-query path (WIRE-1): the header is checked first,
then the object is read, decoded and unlinked exactly like a transmit,
the reply is `OK` or EBADF, and nothing is stored. Without `i=` the
query is ignored and the object is left alone, as in kitty (WIRE-7).

**Measured on kitty 0.49.2** (`scripts/kgp-stress/kitty-ref.sh`):

| Command | kitty | toastty |
|---|---|---|
| `a=q,i=31,t=s` good PNG | `OK`, unlinked | same |
| `a=q,i=32,t=s` missing | `EBADF:Failed to read image file` | same |
| `a=q,i=33,t=s` not a PNG | `EBADPNG:Not a PNG file`, unlinked | `EBADF:…`, unlinked (SEC-4) |
| `a=q,t=s` without `i=` | no reply, object kept | same |
| `a=t,t=s,S=` larger than the object | `OK`, unlinked | same |
| `a=t,t=s,O=` past the end | `EBADPNG:PNG data is truncated…`, unlinked | `EBADF:…`, unlinked (SEC-4) |
| `a=t,t=s,f=32,s=2,v=2`, 8-byte object | `EBADF:…`, unlinked | same |
| `a=t,t=s` symlink | `EBADF:…`, link kept | same |
| `a=t,t=s` directory | `EBADF:…`, kept | same |
| `a=t,t=s` name without `/`, or `/a/b` | `EBADF:…` | same |
| `a=T,t=s` good PNG | displayed, `OK`, unlinked | same |

One deliberate difference, from reading SEC-4 strictly: decode failures
are EBADF, not kitty's `EBADPNG`. That matches what `t=f` already does.

**mpv.** `mpv --vo=kitty --vo-kitty-use-shm=yes` (mpv 0.41.0) sends the
shm name *without* the leading `/` (`mpv-kitty-0x…`). kitty 0.49.2
refuses it ("POSIX SHM names must start with /" in its log, once per
frame) and shows nothing; toastty refuses it the same way, following
the spec. mpv unlinks its own object on exit, so nothing leaks in
either terminal. Accepting slash-less names would be a one-line
leniency; it is not done here because both the spec and kitty refuse
them.

**Not Linux.** macOS shm objects can't be read with `read(2)`; the only
way in is `mmap`, which is `unsafe`, and `toastty-graphics` is
`#![forbid(unsafe_code)]`. So off Linux, `t=s` validates the name,
`shm_unlink`s it (`rustix::shm::unlink`, safe) so a client that ignores
the reply doesn't leak the object, and replies the uniform EBADF. A
client that probes with `a=q,t=s` (icat's `--detect-support`) then falls
back to `t=t`/`t=d` on its own. **Decision (PM/user): decided: unsupported on macOS (keeps `#![forbid(unsafe_code)]`; clients fall back to file/base64).** No audited `unsafe` `mmap` reader
will be added.
