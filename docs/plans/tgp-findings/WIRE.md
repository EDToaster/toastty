## Wire framing, binary-length read, encoding, chunking (sections 8.2, 8.3, 8.7)  (WIRE)

### WIRE-1: What consumes the ST terminator after raw binary bytes is undefined **[USER DECISION]**
- kind: underspecified | section: 8.2
- desc: The example `... len=20512 ESC \  <20512 raw bytes of CBOR>` shows the ST (ESC \) closing the *header* APC, then raw bytes follow with no stated terminator; it is unspecified whether a trailing ST follows the raw bytes, and whether the parser re-enters Ground or expects another framing token after consuming exactly N bytes.
- why: The current parser (parser.rs Apc state) terminates APC on ESC/BEL/0x9C; after the binary read state consumes N bytes the next byte could be the first byte of the next frame's ESC, a stray ST, or garbage, and getting this wrong desyncs the entire stream.
- interacts: binary-length read state, chunking (8.7), control frames (8.2a), RGP apc_end demux
- options: Header ST closes header; raw bytes are NOT followed by any terminator and parser returns to Ground after exactly N bytes | Require a trailing ST after the raw bytes as a sync check; if absent, declare desync and drop | Put the raw bytes *inside* the APC (no header ST) and end with ST after the bytes, parser counts N then expects ST
- rec: Header ST closes the header APC; parser reads exactly N raw bytes then returns to Ground with NO trailing terminator, but treat an optional immediately-following ST as a tolerated no-op sync marker for robustness.

### WIRE-2: Parser back-channel: apc_end returns unit, cannot trigger binary-read mode
- kind: missing-behavior | section: 8.2
- desc: The doc says the parser, 'on seeing a tgp binary header, switches to consume exactly len bytes' but apc_start/apc_chunk/apc_end all return () (parser.rs), so the Perform impl that parses `len=N` has no way to tell the Parser to enter binary-read mode.
- why: This is the single load-bearing mechanism for binary frames; without a defined signal path the entire binary framing cannot be implemented, and the choice (Perform return value vs Parser peeking the header itself) decides whether toastty-parser must understand tgp semantics.
- interacts: toastty-parser/toastty-term crate boundary, RGP demux on first bytes, max-len cap enforcement
- options: Change apc_end to return Option<BinaryFollow{len}> (cross-crate API change) | Have the Parser itself peek/parse the `tgp;...;len=` header inline before the binary state | Add a side-channel callback/state object the Perform sets that the Parser reads after apc_end
- rec: Have apc_end return an enum signal (Option<BinaryFollow{len,enc}>); keeps tgp parsing in the Perform/Term layer and keeps the Parser a dumb pre-scanner, matching the existing layering.

### WIRE-3: len mismatch with actual CBOR payload size is unhandled (lies/truncation/overflow)
- kind: missing-behavior | section: 8.2 / 8.3
- desc: Nothing defines behavior when `len` disagrees with the real payload: len smaller than the CBOR (extra bytes leak into the stream as text), len larger than what arrives before EOF/next frame, or len present but CBOR is itself truncated; the doc only mentions a max cap.
- why: A hostile or buggy sender can desync the parser (under-len leaks binary into the text grid corrupting the terminal) or wedge it forever (over-len waits for bytes that never come), which is exactly the untrusted-input threat in 15.1.
- interacts: max_msg_mb cap (7.1), structured errors (15.3), partial frames across PTY reads, chunking reassembly
- options: len is authoritative: consume exactly N bytes then attempt CBOR decode; decode-too-short/too-long -> x parse_error, return to Ground | Validate CBOR self-terminates exactly at N; mismatch -> drop+error | Trust len for framing only, ignore internal mismatch and let CBOR decoder report
- rec: len is authoritative for framing: always consume exactly N bytes (capped), then CBOR-decode that buffer; any decode error or trailing-bytes-in-buffer -> emit x parse_error and return cleanly to Ground so the stream resyncs.

### WIRE-4: tmux/screen passthrough may strip or split the raw binary bytes **[USER DECISION]**
- kind: interaction | section: 8.2
- desc: The robustness fallback says base64 is for transports that 'can't carry raw bytes,' but the terminal cannot know it is behind tmux; tmux passthrough wraps DCS/APC and can strip ST-looking bytes (0x1B 0x5C) or truncate at its own buffer size, silently corrupting a raw binary frame whose len was already announced.
- why: If tmux splits or strips inside the N raw bytes, the terminal still consumes N bytes from a now-misaligned stream and desyncs; the doc punts this to 'validate against real tmux configs' (16) but provides no detection or recovery contract.
- interacts: enc=b64 fallback, capability reply enc= advertisement, len authoritative read, ST terminator consumption
- options: Make enc=bin opt-in only when app asserts a clean transport; default advertise enc=b64 first | Add a checksum/magic trailer after raw bytes so corruption is detected and surfaced as x error | Detect tmux passthrough wrapping and refuse enc=bin (only advertise b64)
- rec: Advertise both but recommend the app default to base64 unless it has confirmed a clean PTY; add a small fixed magic/length-echo trailer after the raw bytes so split/strip corruption is detected and reported via x rather than silently desyncing.

### WIRE-5: Chunk reassembly keyed by asset id collides across concurrent uploads and interleaving **[USER DECISION]**
- kind: interaction | section: 8.7
- desc: Chunking keys reassembly by `id` (asset id) with a `more` flag like RGP, but it is undefined what happens when two interleaved asset.add chunk streams share an id, when a non-asset.add patch arrives mid-chunk-stream, or when a second `more=1` for the same id starts before the first completes.
- why: RGP keys pending buffers per-id (handler.rs) and TGP inherits this, but TGP frames are full patches (txn) not single ops, so the keying granularity (id vs txn vs id+txn) is genuinely ambiguous and wrong choices allow one stream to corrupt another's reassembly buffer.
- interacts: txn atomicity (8.6), patch ordering 'receive order', RGP more= adapter, per-id/per-session caps (15.2)
- options: Key reassembly by (asset id) and reject any interleaving other frame as out-of-order error | Key by a chunk-stream id distinct from asset id, allow interleaving of independent streams | Forbid interleaving entirely: a more=1 stream must be contiguous, any other frame mid-stream aborts it
- rec: Key by asset id but forbid interleaving within an id (second open stream for same id or format mismatch aborts the prior, matching RGP's mid-stream-mismatch drop), and allow independent ids to interleave; surface an aborted stream via x error.

### WIRE-6: Chunked asset.add vs patch atomicity: a chunk stream is not transactional
- kind: contradiction | section: 8.7 / 8.6
- desc: 8.6 says all ops in a patch apply atomically and bump scene_revision once, but 8.7 says a chunked asset.add spans multiple binary frames reassembled before parsing — so a multi-frame asset that fails on the last chunk leaves partial state, and it is unclear whether the chunk frames are one txn or several.
- why: If each chunk is its own frame/txn the 'atomic patch' guarantee breaks for the most expensive op (asset upload); if the whole reassembled blob is one txn, then txn correlation ids across N frames need defined semantics that the doc lacks.
- interacts: txn correlation id (8.6), ack a frame (8.5), x error op index, asset_revision bump timing
- options: A chunk stream completes into a single logical op that is then applied atomically within its enclosing txn; intermediate chunks are pure buffering and bump nothing | Each chunk is its own txn and asset.add becomes non-atomic, with partial-asset cleanup on failure | Require the asset.add op to be the only op in its patch when chunked
- rec: Treat chunk frames as pure pre-parse buffering keyed by id that carry no txn semantics; only the final assembled patch is decoded and applied atomically, bumping asset_revision once on success.

### WIRE-7: Two len semantics under enc=bin vs enc=b64 are conflated
- kind: underspecified | section: 8.2
- desc: Under enc=bin, len is the raw byte count read outside escape scanning; under enc=b64 the doc says `len=<encoded-len>` (base64 char count) terminated 'with ST as usual' inside the APC — these are two different read paths and two different meanings of len, but they share the field name and the example doesn't show how the parser picks the path before it has parsed enc=.
- why: The parser must decide whether to enter raw-byte-count mode or normal APC-scan mode based on enc=, which may appear after len= in the header; field ordering and default-when-absent are unspecified, risking the parser entering the wrong mode.
- interacts: binary-read state trigger, apc_end back-channel, base64 fallback decode, header field ordering
- options: Mandate enc= appears before len= in the header so the parser knows the mode when it reads len | Default to b64 (normal APC scan) when enc= absent, only enter raw mode on explicit enc=bin | Make len mean raw-bytes-to-follow only for enc=bin; for enc=b64 omit len and rely on ST
- rec: Require enc= to precede len= in the header and default enc=b64 when absent; only enc=bin engages the raw-byte-count read state, so the parser always knows its mode before acting on len.

### WIRE-8: max_msg_mb cap vs chunk reassembly cap relationship and overflow behavior unstated
- kind: underspecified | section: 8.7 / 15.2
- desc: 7.1 advertises max_msg_mb=64 (per message) and 15.2 mentions a separate 'max chunk reassembly size,' but it's undefined whether len> max_msg_mb is rejected before reading (and how to drain those raw bytes from the stream) or after, and whether a chunk stream's total is bounded by max_msg_mb or a larger reassembly cap.
- why: If the terminal rejects an oversized len but the N raw bytes are already in flight, it must still consume/discard exactly N bytes to stay synced — an over-cap len that is also a lie becomes a denial-of-sync vector; RGP today silently drops on overflow (handler.rs).
- interacts: len authoritative read, RGP 64/256 MiB caps, structured errors (15.3), partial frames across PTY reads
- options: Reject len>cap immediately but still consume-and-discard N bytes to resync, then emit x cap_exceeded | Reject and abort the connection-resync by returning to Ground without consuming (risks desync) | Cap len at a hard ceiling, read up to the ceiling, discard rest, emit error
- rec: If len exceeds the advertised cap, still consume-and-discard exactly N bytes (bounded by an absolute hard ceiling beyond which the parser declares unrecoverable desync) then emit x cap_exceeded; never leave un-drained announced bytes in the stream.

### WIRE-9: Partial binary frame spanning multiple PTY reads has no defined buffering contract
- kind: missing-behavior | section: 8.2
- desc: Parser::advance is called per PTY read chunk; a len=20512 raw payload will routinely arrive split across many advance() calls, so the binary-read state must persist a remaining-byte counter and partial buffer across calls, but the doc never states this lifecycle (it only says 'split across PTY read chunks' is a concern in the brief, not the doc).
- why: Without an explicit cross-call state machine the parser either blocks (impossible, advance is non-blocking) or mis-counts when a frame straddles a read boundary, and the 256 MiB-style buffer must accumulate safely under the cap across reads.
- interacts: binary-read state in Parser, max_msg_mb cap accumulation, apc_buffer 256 MiB cap (term.rs), EOF mid-binary
- options: Parser carries (remaining:usize, buf:Vec) state across advance() calls, appending until remaining==0 | Require apps to flush whole frames per write (unenforceable over PTY) | Buffer at the Term layer instead of Parser, with Parser only emitting raw-byte callbacks
- rec: Make the binary-read state explicit in the Parser with a persisted remaining-count and a capped accumulation buffer across advance() calls; on terminal EOF with remaining>0, discard the partial and (if opted in) emit x parse_error truncated.

### WIRE-10: CBOR decode-error surfacing depends on the currently-discarded Result path
- kind: interaction | section: 8.3 / 15.3
- desc: 8.3 picks CBOR and 15.3 promises structured x parse_error on malformed input, but today the RGP handler.process Result is discarded (term.rs:3417) and the demux silently drops unrecognized payloads (term.rs:3420-3421); TGP CBOR decode errors must flow back through queue_reply, which the doc never connects to the decode site.
- why: The 'structured errors vs silent drop' headline (15.3) is only real if the decode error actually reaches queue_reply -> pty_replies; otherwise TGP inherits RGP's silent-drop and the differentiator is vapor, and the error must include txn which a failed CBOR decode may not have parsed yet.
- interacts: queue_reply writeback (RgpSink), txn correlation in x errors, error opt-in gating (15.3), len authoritative framing
- options: Decode site returns Result that Term forwards to queue_reply when error reporting negotiated; if txn unparsed, emit x with txn omitted/unknown | Parse txn from the header (text) before CBOR so x always has txn even on body decode failure | Keep silent drop when error reporting not negotiated, surface only when opted in
- rec: Put txn (and enc/len) in the text header so it's known pre-CBOR; route the decode Result to queue_reply emitting x parse_error with the header txn when error reporting is negotiated, else silently drop — this makes 15.3 real without changing dumb-reader behavior.

### WIRE-11: base64 fallback frame still hits APC C0/ESC scanning and 256 MiB APC cap, not the binary cap
- kind: interaction | section: 8.2 / 8.3
- desc: The enc=b64 path sends the payload inside the APC 'ST terminated as usual,' meaning it flows through the existing apc_chunk path bounded by the 256 MiB APC cap (term.rs) and memchr3 terminator scan — a different code path and a different cap than the enc=bin max_msg_mb=64, with no stated reconciliation.
- why: The same logical message has two different effective size limits and two different parse paths depending on enc=, so an app that pre-trims to max_msg_mb=64 for bin could exceed/undershoot when forced to b64 (which is ~33% larger encoded), and base64 decode errors are a new failure mode not covered by the CBOR-only error story.
- interacts: max_msg_mb cap (7.1), APC 256 MiB cap, CBOR decode errors, tmux fallback selection
- options: Apply max_msg_mb to the DECODED size for both paths; for b64 cap the encoded length at ceil(max*4/3) | Advertise separate caps for bin vs b64 | Make b64 reuse the same binary-read accounting after a decode step, unifying caps
- rec: Define max_msg_mb as the decoded-payload cap for both encodings (b64 encoded length bounded at ceil(cap*4/3)), and route both through the same post-decode CBOR error path so the b64 detour shares one cap and one error contract.

