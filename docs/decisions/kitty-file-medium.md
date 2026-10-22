# Decision: Kitty file mediums and upload limits

Date: 2026-10-21. Author: kgp-B (KGP fix programme, PR B1).
Status: **implemented**. Closes SEC-1 to SEC-7 in
`docs/kitty-graphics-checklist.md`.

## TL;DR

Anything that can write to the PTY can send a kitty graphics command: `cat`
of a file, `curl` output, a remote host over ssh. So `t=f` / `t=t` paths
and every payload are untrusted.

- File reads (`kitty/medium.rs`) take these steps in order:
  canonicalize → refuse `/proc`, `/sys`, `/dev` → `stat` and require a
  regular file → `open(O_NONBLOCK | O_NOFOLLOW | O_NOCTTY)` → `fstat` and
  require the same regular file → read at most a bounded number of bytes.
- Every file failure replies `EBADF:Failed to read image file`. The real
  reason goes to the debug log.
- `t=t` deletes only when the sent path contains `tty-graphics-protocol`
  and the resolved file lies in a temp dir. This matches kitty.
- Decoding is bounded by `KittySink::max_image_bytes()`, which `Term` sets
  to the registry cap. This bound covers zlib output and PNG IHDR
  dimensions.
- In-flight chunked uploads have caps on their count and their total
  bytes, and expire after a TTL.

The target behaviour is kitty 0.49.2, checked live with
`scripts/kgp-stress/kitty-ref.sh`.

## File reads (SEC-2, SEC-3)

The spec says to check the path *before* opening it, because opening can
have side effects: a FIFO blocks, and a device may act on open. So:

1. `canonicalize` follows symlinks, as the spec requires, and resolves
   `..`. Every check below looks at where the bytes really live.
2. Refuse `/proc`, `/sys` and `/dev`, except `/dev/shm/<name>`, which is a
   temp dir that kitty also allows.
3. `stat` must report a regular file.
4. Open with `O_NONBLOCK`, so a FIFO swapped in after the `stat` can't park
   the thread. Add `O_NOFOLLOW`, because the canonical path holds no
   symlinks, so finding one means the path was swapped. Add `O_NOCTTY`.
   This uses `rustix`, which is now a unix-only dependency of
   `toastty-graphics` (workspace feature `fs`).
5. `fstat` the fd and require a regular file with the same `(dev, ino)`
   as the `stat`.
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

## Temp-file deletion (SEC-1)

kitty's rule, from `kitty/utils.py` `is_path_in_temp_dir` and
`boss.safe_delete_temp_file`, confirmed live:

- The *sent* path must contain `tty-graphics-protocol`, in the file name
  or in any directory name.
- `realpath(sent)` must be under a temp root. The roots are `/tmp`,
  `/dev/shm`, `$TMPDIR` and `std::env::temp_dir()`, all canonicalized.
  The comparison goes component by component, so `/tmpfoo` is not under
  `/tmp` (kitty compares string prefixes).
- The sent path's own entry is unlinked. If the sent path is a symlink,
  only the link is removed, never its target.
- The file must have opened as a regular file. When it did, the file is
  deleted even if the read or decode then fails, as kitty does.

We add one check that kitty doesn't have: the directory holding the
entry we unlink must itself resolve into a temp root. Otherwise a
symlinked parent could make us unlink an entry outside the temp dirs.

## Decode limits (SEC-5, SEC-7)

- `KittySink::max_image_bytes()` is new. Its default is
  `DEFAULT_MAX_IMAGE_BYTES`, 400 MB, kitty's `MAX_DATA_SZ`. `Term` returns
  the registry cap, which is 320 MiB by default. The registry evicts to
  make room, so the whole cap is what one image may use.
- zlib output stops at `s*v*bpp` for raw formats, or at the quota for PNG
  or when dimensions are missing. We read `limit + 1` bytes through
  `Read::take`. Anything over the limit is `EINVAL:zlib decompress:
  inflated data exceeds N bytes`.
- PNG: a header-only pass (`ImageReader::into_dimensions`) runs first. If
  `w*h*4` exceeds the quota, the reply is `ENOMEM:PNG image is too
  large`, which is kitty's literal reply.

## In-flight uploads (SEC-6)

- `MAX_PENDING_UPLOADS = 8`. The spec says a client must finish one
  upload before it starts another, so the slots only have to absorb
  abandoned uploads.
- `DEFAULT_PENDING_TOTAL_CAP = 128 MiB`. This is twice the per-upload
  cap, so one upload always fits.
- Starting an upload, or growing one past either cap, evicts the least
  recently active *other* upload. Order comes from a chunk counter, not
  the clock.
- `PENDING_UPLOAD_TTL = 60 s` after the last chunk. Expiry runs at the
  start of every graphics command.
- Eviction and expiry send no reply. The client that abandoned the upload
  isn't listening, and replies that nobody reads end up as typed text in
  the shell.

How later chunks for an evicted upload are routed is WIRE-9's job (PR B3).
