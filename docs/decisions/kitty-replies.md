# Decision: Kitty graphics replies and header parsing

Date: 2026-10-21. Author: kgp-B (KGP fix programme, PR B2).
Status: **implemented**. Closes PLC-1, PLC-3, DEL-1, WIRE-3, WIRE-6,
WIRE-7, WIRE-8 and WIRE-12 in `docs/kitty-graphics-checklist.md`.

## TL;DR

Replies follow kitty 0.49.2 byte for byte. Every case in the PR's
transcript was measured live with `scripts/kgp-stress/kitty-ref.sh`.
Fewer replies is the safer side: a reply that nobody reads lands in the
shell's input as typed text (APP-1).

## Header parsing (`kitty/header.rs`)

- **Key types.** Each key has one wire type, whatever the action, as in
  kitty:
  - `z`, `H` and `V` are signed. A signed key takes a magnitude that fits
    32 bits and wraps into `i32`, so `z=2147483648` is accepted.
  - `a`, `t`, `o` and `d` are enums. `q` and `m` are numbers: `q=2` and
    above are silent, any non-zero `m=` means more chunks.
  - Every other key is an unsigned 32-bit number with decimal digits only.
- **`C=` and `U=` are numbers.** `a=c` uses `C=` as its blend mode, so
  `C=2` must parse. `Header::cursor_no_move` means `C=1`.
- **What kitty drops silently.** All of these are errors, and the handler
  drops the command without a reply (WIRE-6):
  - an unknown key;
  - whitespace;
  - an empty or trailing `,` pair;
  - a `+` sign, a hex number, or an overflowing number;
  - a bad enum value.
- **`f=` accepts any number.** An unsupported one is `Format::Unknown(n)`
  and replies `EINVAL:Unknown image format: N` when a transmit decodes.

## Animation keys (for stack E)

The animation actions reuse placement keys. Use these accessors instead
of the placement-named fields:

- **`Header::frame() -> FrameKeys`** (`a=f`):
  - `x`, `y`;
  - `base_frame` (`c`), `edit_frame` (`r`), `gap_ms` (`z`, signed);
  - `blend_mode` (`X`), `background` (`Y`, RGBA).
- **`Header::animation() -> AnimationKeys`** (`a=a`):
  - `state` (`s`), `frame` (`r`), `gap_ms` (`z`);
  - `current_frame` (`c`), `loops` (`v`).
- **`Header::composition() -> CompositionKeys`** (`a=c`):
  - `dest_frame` (`c`), `src_frame` (`r`);
  - `dest_x` and `dest_y` (`x`, `y`), `width` and `height` (`w`, `h`);
  - `src_x` and `src_y` (`X`, `Y`), `blend_mode` (`C`).

`KittyHandler::handle_animation` is the hook for stack E:
- `resolve_image` has already checked that the image exists, or replied
  ENOENT.
- On a stored image the command is currently a silent no-op.

## Reply rules (`kitty/handler.rs`)

- **`a=p` (PLC-1, PLC-3).** It resolves `i=`, or the newest stored image
  with `I=`, before doing anything.
  - An unknown image gets `ENOENT:Put command refers to non-existent image
    with id: N and number: M`. `q=1` does not suppress it, and nothing is
    placed or moved.
  - An `I=` placement replies `i=<id>,I=<n>;OK`.
- **`a=d` (DEL-1)** never replies.
- **Animation (WIRE-3, WIRE-12).** An unknown image gets `ENOENT:Animation
  command refers to non-existent image with id: N and number: M`. A
  stored image gets nothing.
- **`a=q`** replies only when it carries `i=`. An empty query is validated
  like any other (`EINVAL:Zero width/height not allowed`).
- **Transmit errors (WIRE-7)** use kitty's codes and messages:
  - `ENODATA:Insufficient image data: GOT < EXPECTED`;
  - `EFBIG:Too much data` for more than 10 bytes past `s*v*bpp` (up to
    10 extra bytes are accepted, as kitty does), or for compressed data
    more than about 1 KiB past it;
  - `EBADPNG:Not a PNG file`, `EBADPNG:PNG data is truncated: …`;
  - `EINVAL:Failed to inflate image data with error: Z_BUF_ERROR` or
    `Z_DATA_ERROR`;
  - `EINVAL:Image data size post inflation does not match expected size`.

  Undecodable base64 counts as empty data, so the reply is the ENODATA
  one, as in kitty.
- **`f=100,o=z`** is held to `S=`, as the spec requires. Without `S=` the
  reply is the size-mismatch error.
- **A failed `I=`-only transmit** reports the id the host would assign
  (`KittySink::next_image_id`), because kitty allocates the id before
  validating.
- **`q=` on continuation chunks (WIRE-8).** A non-zero `q=` on any chunk
  applies to the whole upload, and the latest one wins. `q=0` can't lower
  it, since it can't be told apart from a missing `q=`.

## New `KittySink` methods

- `image_id_for_number(n) -> Option<u32>`: the newest stored image with
  number `n`. `Term` keeps `image_number_to_id: HashMap<u32, Vec<u32>>`,
  so when the newest image with a number is freed, the next newest takes
  over.
- `next_image_id() -> u32`: the id an anonymous insert would get. `Term`
  returns `ImageRegistry::next_free_id()`, which is now public.

## Deliberate divergences from kitty

- **The leading comma.** kitty writes `I=`-only error replies with a
  stray leading comma (`\x1b_G,I=999;ENOENT…`). We omit it.
- **Animation on a stored image.** Implemented since (see
  `kitty-animation.md`): `a=f` replies `i=N,r=<frame>;OK` and `a=c`
  replies OK or its frame errors, as in kitty.
- **`t=s`** still replies ENOTSUP; that is B4's job.
- **Other PNG errors** carry the `image` crate's message after `EBADPNG:`
  rather than libpng's.

## Chunked transport (B3: WIRE-4, WIRE-9, WIRE-10, WIRE-11, DEL-3)

These rules were measured live on kitty 0.49.2.

- **One loading slot.** `KittyHandler::loading` is an `Option<Upload>`. A
  first chunk with `m=1` starts it. kitty checks the format and the
  dimensions *then*:
  - `EINVAL:Zero width/height not allowed` and `EINVAL:Unknown image
    format: N` are replied at once, and nothing loads.
  - Dimensions over 10000 are dropped silently. kitty answers those under
    a stale id, which is nearly always none.
- **Every direct transmit during a load is a continuation**, whatever its
  `i=` or other keys. Only `q=` is taken from it. A stray `a=t,i=57` from
  another program is therefore appended to the load and answered under
  the loading image's id, as kitty does. The 60 s idle expiry is the
  safety net (a divergence: kitty has none). It runs at the start of each
  graphics command and after each batch of PTY input
  (`Term::drain_pty_replies`), so an abandoned upload doesn't stay
  resident.
- **Per-chunk base64** (`kitty/b64.rs`). Each chunk is decoded on arrival
  and may carry its own `=` padding. toastty is more lenient than kitty in
  three ways:
  - whitespace is skipped;
  - an unpadded partial quantum carries over into the next chunk;
  - decoding continues after a `=` inside a chunk.

  As in kitty, decoding a chunk stops at its first junk byte and keeps
  the bytes before it (`/wAA*w==` gives 3 bytes).
- **Bounded before decoding.** Before a chunk is decoded, its decoded
  size is bounded from its base64 length (`b64::decoded_len_bound`, three
  bytes per four characters, plus any carried partial quantum), and a
  chunk that can't fit the cap is refused with `EFBIG:Too much data`
  without allocating. One APC can carry up to 256 MiB of base64.
- **The cap counts decoded bytes.** It is kitty's buffer size:
  - `s*v*bpp` + 10, or + about 1 KiB for `o=z`, capped at the quota plus
    that slack;
  - the image quota for PNG.

  Past it, kitty replies `EFBIG:Too much data` and drops the upload. `S=`
  is ignored for `t=d`.
- **Aborts.** Any delete aborts the load ("the partial upload must be
  aborted"), whatever it deletes, and so does a file-medium transmit.
  Remaining chunks get no reply. A bare continuation has no dimensions,
  so it fails the first-chunk check, anonymously.
- **Queries.** A query without `i=` (anonymous or `I=`-only) is dropped
  before any loading logic: it doesn't append to, finish, abort or EILSEQ
  an upload in progress.
  - A chunked `a=q` (with `i=`) is checked up front, then consumes the
    next chunk and ends with no reply. Nothing is stored.
  - A chunked `a=q` during a transmit replies `EILSEQ:More payload loading
    refers to non-existent image` for the upload and drops it. A second
    chunked `a=q` while a chunked query is loading gets the same EILSEQ,
    under its own id.
  - A non-chunked `a=q` during a transmit is that transmit's final chunk:
    the image is stored under its own id, and the reply goes out under the
    query's id.
- **Puts during a load.** `a=p` on the loading image replies
  `ENOENT:Put command refers to image with id: N that could not load its
  data`, and the load carries on. `a=a` on the loading image is silent.

Deliberate divergences:
- the three base64 leniencies above;
- `a=f` during a load, which needs animation (stack E);
- the stale-id reply for dimensions over 10000.

## File-medium queries and usage hints (B4a: WIRE-1, §9 `N=1`)

- **`a=q` with `t=f` / `t=t`** goes through the same hardened read path as
  a transmit (`KittyHandler::query_medium` → `medium::read_image_file`),
  `t=t` deletion rule included. Order, as in kitty 0.49.2: a query without
  `i=` is ignored and the file isn't touched; `f=` and the dimensions are
  checked before the file is opened (`EINVAL:Zero width/height not
  allowed`); every failure after that, decode errors too, is the uniform
  `EBADF:Failed to read image file`; success is `OK`, nothing is stored.
  The same pre-check now applies to file-medium transmits.
- **Short files.** A file shorter than `S=` is read to its end; too little
  data then fails in the decoder (uniform EBADF). kitty does this, and
  `kitten icat --detect-support` depends on it.
- **`t=s`** still replies ENOTSUP to `a=q` (shared memory is its own PR).
  Detection therefore reports `files`, not kitty's `memory`, for now.
- **Usage hints.** `N` is a bitmask; bit 0 is "transient". After a
  successful store the handler calls `KittySink::mark_transient`; `Term`
  marks the registry entry. Eviction order under pressure: unplaced
  transient (LRU), then unplaced, then placed. A re-send of the id clears
  the mark, since the hint belongs to the data it came with.
