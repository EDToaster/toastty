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
  - `m`, `q`, `a`, `t`, `o` and `d` are enums.
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
- **Animation on a stored image.** kitty replies `i=N,r=<frame>;OK` to
  `a=f` and frame errors to `a=c`. We stay silent until stack E implements
  animation.
- **`t=s`** still replies ENOTSUP; that is B4's job.
- **Other PNG errors** carry the `image` crate's message after `EBADPNG:`
  rather than libpng's.
