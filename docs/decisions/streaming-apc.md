# Decision: Streaming APC payloads (not buffered)

Date: 2026-05-22 — author: investigation worktree `streaming-apc`
Status: **recommendation** (pending RGP work landing)

## TL;DR

Adopt a **streaming** APC dispatch model in `toastty-parser`. Provide a
`BufferingApcHandler` adapter so handlers that don't care about streaming
(mode setters, small Kitty graphics queries) can stay buffered with no
ceremony. Do reassembly of Kitty `m=1`/`m=0` chunked uploads **one level
above** the APC parser — at the Kitty handler — not in the parser itself.

Concretely: in `toastty-parser`, add a thin custom APC scanner that
runs alongside `vte`'s CSI/OSC/DCS dispatch and surfaces APC events as
`start(&header)` / `chunk(&[u8])` / `end()`.

## Why this is even a question

`vte` 0.15.0 (latest stable) is the parser of choice elsewhere in the
codebase. Reading the source (`/tmp/vte-inspect/vte-0.15.0/src/lib.rs`)
revealed a load-bearing surprise:

> **`vte` never dispatches APC payloads.** The state machine enters
> `SosPmApcString` on `ESC _` / SOS / PM, and then routes every byte of
> the payload into `anywhere()`, which is a no-op except for
> CAN/SUB/ESC. The trait has no `apc_dispatch` method.

This is consistent with the upstream changelog: APC has never been
exposed, in any version since 0.3. Alacritty doesn't need it because
Alacritty doesn't ship Kitty graphics or RGP.

So the framing in the problem statement — "use `vte` as-is for the
buffered case" — is misleading. There is no buffered case in `vte`.
Whether we go buffered or streaming, **we have to add APC handling
ourselves.** That changes the cost calculus: streaming isn't a heavier
detour from a default that already exists; both options cost roughly
the same to build.

## What was prototyped

`prototypes/streaming-apc/` ships two parsers behind the same
trait-based dispatch shape:

| File | LoC (logical) | Role |
| --- | --- | --- |
| `src/buffered.rs` | 75 | Accumulate full payload, dispatch once |
| `src/streaming.rs` | 130 (incl. inline 40-line heapless `Vec`) | Emit `start`/`chunk`/`end` |
| `src/lib.rs` | ~50 | Traits + `BufferingApcHandler` adapter |

Both parsers handle:
- ESC `_` introducer to ST (ESC `\`) terminator
- A header section (bytes before first `;`) and a body section
- `memchr`-accelerated ground scanning, same trick `vte` itself uses
- ESC bytes inside the payload that don't form ST (`ESC \`)

The benchmark binary (`src/bin/apc-bench`) drives each parser with two
scenarios:

1. **`one50mb`** — a single APC carrying a ~50 MB fake-glTF body
   (`ESC _ R,a=glb,sz=...;<50MB>ESC \`)
2. **`chunked5k`** — Kitty `m=1`/`m=0` chunked upload simulating a 50 MB
   image as 10 240 APCs of 5 KB each

Each scenario builds the wire bytes up front, then streams them through
the parser in 64 KB feed slices (a realistic PTY read buffer).

## Numbers

Measured on macOS via `/usr/bin/time -l`, release build, opt-level=3, LTO
thin, single CPU run. Three runs each; values shown are typical.

| Mode | Scenario | Peak RSS | TTFBH | TTLBH | Parse wall |
| --- | --- | --- | --- | --- | --- |
| Buffered  | one50mb   | **108.6 MB** | **4 600 µs** | 4 600 µs | 4 600 µs |
| Streaming | one50mb   | 54.0 MB      | **< 10 µs**  | 1 700 µs | 1 700 µs |
| Buffered  | chunked5k | 54.1 MB      | ~10 µs       | 1 944 µs | 1 944 µs |
| Streaming | chunked5k | 54.1 MB      | < 1 µs       | 2 239 µs | 2 239 µs |

(50 MB input is unavoidably resident in both modes — it's the test
fixture. The interesting delta is the parser overhead on top.)

**The streaming parser is also slightly faster end-to-end** in the
single-large-APC case (1.7 ms vs 4.6 ms, ~2.7×). The buffered parser
pays for a 50 MB `Vec::extend_from_slice` that the streaming version
skips. The 50 MB peak-RSS overhead in buffered/one50mb is exactly that
`Vec`.

In the chunked case the modes are within noise of each other on RSS
and time, because each individual buffered APC is only 5 KB. **The
buffered model fails specifically at the "single large APC" shape, which
is exactly what RGP asset uploads will be.**

### Time-to-first-byte-handled

This is the most lopsided number. Buffered/one50mb gets its first
`dispatch` call at 4.6 ms — i.e. only after the entire 50 MB has been
scanned and copied. Streaming/one50mb sees its first chunk inside the
first 64 KB of input — under 10 µs. In a real PTY read loop that's the
difference between "the renderer can start uploading a texture before
the network finishes the upload" and "the renderer waits idle for the
whole transfer."

## Surprises (in priority order)

1. **`vte` silently drops APC.** The crate's `Perform` trait has no
   `apc_dispatch` method and never has. The bytes literally go into
   `anywhere()` and disappear. We were going to write APC support
   anyway; we just didn't realize how much of it.
2. **Streaming was *faster*, not slower.** I expected the trait-callback
   overhead per 64 KB to dominate; instead the saved `Vec` copies more
   than paid for it. Result is `mem::move` of 50 MB vs ~zero, and that
   shows up in wall time as 4.6 ms → 1.7 ms.
3. **Chunked Kitty uploads make the buffered/streaming choice
   irrelevant at the parser level** — each chunk is only 5 KB, so the
   transient buffer is tiny in either case. The interesting question
   for chunked is "where does reassembly live?", not "how does the
   parser deliver the chunk?". See "Where does Kitty reassembly live?"
   below.

## API shape

The recommended public surface in `toastty-parser`:

```rust
pub trait ApcHandler {
    fn start(&mut self, header: &[u8]);
    fn chunk(&mut self, bytes: &[u8]);
    fn end(&mut self);
}

/// Adapter for handlers that prefer the old buffered shape.
pub struct BufferedApcHandler<H: FnMut(&[u8])> { /* ... */ }
impl<H: FnMut(&[u8])> ApcHandler for BufferedApcHandler<H> { ... }
```

A handler that doesn't care about streaming — say a query-response
APC like a hypothetical `ESC _ Q,query=size ESC \` — wraps itself:

```rust
let mut h = BufferedApcHandler::new(|payload| {
    // payload is the full body, just like vte's osc_dispatch
});
parser.advance(bytes, &mut h);
```

Two-line opt-in. Real streaming handlers (Kitty graphics image data,
RGP `glb` upload) implement `ApcHandler` directly and pipe `chunk`
straight into their decoder or asset registry.

The prototype demonstrates both shapes (`StreamingApcHandler`,
`BufferingApcHandler<B>`).

## Where does Kitty reassembly live?

**Above the APC layer, in the Kitty handler — not in the parser.**

A Kitty `m=1` upload is N+1 *complete* APC sequences. The APC parser's
job ends at delivering each `start`/`chunk`/`end` triple. The Kitty
handler then:

1. On `start`, parses the header; sees `m=1` or `m=0`.
2. Streams `chunk` bytes into a per-id reassembly buffer (or directly
   into a base64-decoding sink that feeds the image decoder).
3. On `end` with `m=0`, finalizes and registers the image.

Reasons to keep reassembly out of the parser:

- The parser doesn't know which protocol owns the APC. We can't
  reassemble what we can't identify, and identification requires
  reading the `G`/`R`/etc. introducer that's in the header.
- Different protocols want different reassembly semantics. RGP
  hypothetically wants one-shot `glb` upload (no chunking needed
  at the protocol layer because individual APCs can be MB-sized).
  Kitty wants 4 KB chunk reassembly. Putting both into the parser
  bloats it.
- Streaming chunk arrival is exactly the right interface for a
  decoder pipeline. The Kitty handler can pipe directly to
  `base64::Decoder` → `png::Decoder` → texture upload without
  ever materializing the full payload in RAM. Whether to do that
  is a Kitty-handler call, not a parser call.

## Why not just fork `vte`?

The full Paul Williams state machine in `vte` is ~400 LoC across
ground, CSI, DCS, OSC, ESC, and APC states. Writing our own version
that handles *only* APC and delegates CSI/DCS/OSC/ESC back to `vte` is
~130 LoC. That's the sensible split, and that's what the prototype
does. If we ever want to drop `vte` entirely we can — the prototype
is already most of what we'd need for an APC-only frontend.

## Risks and follow-ups

- **Memory caps.** Streaming parsers don't eliminate OOM, they relocate
  the risk into handlers. The Kitty handler should cap reassembly at
  the protocol's stated max (Kitty doesn't actually publish one;
  pick a value like 256 MB and reject larger). RGP should validate
  the declared `sz=` against a cap before opening its sink.
- **ESC inside payload.** Kitty mandates base64 so this is fine, but
  RGP `a=glb` could be raw binary. The current prototype handles ESC
  bytes in the body correctly (any ESC not followed by `\` is emitted
  to the chunk stream verbatim). Verify against the published RGP spec
  before locking the API.
- **Coupling to `vte`'s lifetime.** `vte` 0.15.0 was released
  2025-02-02 and is stable. If upstream ever adds an
  `apc_dispatch` callback (unlikely — they don't ship graphics),
  swap our parser to delegate.
- **`bytes` crate not used.** I checked. The `bytes::BytesMut`
  approach doesn't help here because handlers want `&[u8]`, not owned
  byte buffers. The right abstraction is "borrowed slice into the PTY
  read buffer", which is already what `&[u8]` gives us. Adding `bytes`
  would be ceremony with no payoff for parser-internal storage.

## Pinned dependency versions used in the prototype

- `vte = "=0.15.0"` — referenced for CSI/OSC/DCS in the real
  integration, not used by the APC parser itself.
- `memchr = "=2.8.0"` — for `memchr::memchr(0x1B, ...)` ground scan.
- `bytes` — checked latest is `1.11.1`; not adopted (see Risks).
