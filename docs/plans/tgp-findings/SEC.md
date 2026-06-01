## Caps enforcement, structured errors, hardened parsing, fuzz (section 15)  (CAPS)

### CAPS-1: Binary-frame len cap vs txn atomicity: error reply requires a txn that was never parsed
- kind: interaction | section: 8.2, 15.2, 15.3
- desc: A binary frame announces len=N; if N exceeds max_msg_mb the parser must drop the raw bytes BEFORE decoding the CBOR that contains txn/op indices, yet 15.3 says errors are per-txn and cite op index. The cap fires before txn is knowable.
- why: Per the brief, the parser switches to consume-exactly-N-raw-bytes with no back-channel; a too-large len has no recoverable txn, so the doc's 'x;txn=..;op=..' shape cannot be filled, and the app's correlation/ack loop stalls waiting on txn it never gets an error for.
- interacts: 8.2 binary framing, 8.6 patch atomicity, 8.5 a-ack, 7.1 max_msg_mb
- options: Put txn in the TEXT header (tgp;p;txn=42;len=N) so it survives a payload-level cap and x can always cite it | Emit a txn-less x;code=msg_too_large and require apps to time out un-acked txns | Reject at header parse and resync by still consuming/discarding N bytes to keep the stream aligned
- rec: Hoist txn into the text header (already shown in the §8.2 example) and mandate that the terminal still drains exactly len bytes on a cap rejection so framing stays aligned; emit x;txn;code=msg_too_large.

### CAPS-2: Truncation / EOF mid-binary across PTY read chunks has no defined cap or recovery
- kind: missing-behavior | section: 8.2, 15.2
- desc: The doc says binary frames split across PTY reads and must handle truncation/EOF mid-binary, but 15.2 lists no cap or behavior for a len=N where the writer dies after K<N bytes, or for an indefinitely-stalled partial frame.
- why: An attacker (or a hung curl) sends a huge len then drips bytes, holding the parser in consume-N mode forever; this is a denial-of-service / stuck-terminal bomb not covered by the byte caps, and the brief flags split-across-chunks as a required new parser state.
- interacts: 8.2 consume-exactly-N state, 15.2 max_msg_mb, 8.7 chunking more= flag, parser back-channel
- options: Add a partial-frame timeout/byte-stall cap that aborts and resyncs | Bound total outstanding-partial-frame bytes per session | On any ESC/host-reset while mid-binary, abort the frame and re-enter Ground
- rec: Define a max in-flight partial-frame budget plus an inactivity timeout; on breach abort, discard, emit x;code=truncated, return to Ground. Engineering can pick the constants.

### CAPS-3: Chunk reassembly cap vs partial allocation: when does an over-cap multi-frame asset get torn down
- kind: underspecified | section: 8.7, 15.2
- desc: asset.add chunks span frames keyed by id with more=; 15.2 caps reassembly size but doesn't say whether the cap is checked per incoming frame (drop early) or only at final assembly, nor whether already-buffered bytes for that id are freed immediately on breach.
- why: The brief notes RGP checks caps BEFORE accepting bytes and silently drops on overflow; if TGP only checks at assembly it permits up to max_msg_mb x many-frames of partial allocation, a parse-time bomb, and leaves orphaned buffers if teardown isn't specified.
- interacts: 15.2 max chunk reassembly, 8.6 txn atomicity, 8.4 add-on-existing-id is error, RGP DEFAULT_PENDING_CAP_PER_ID
- options: Check cumulative size on every chunk and reject+free at first overflow (RGP's model) | Reserve max budget on first chunk | Per-id pending cap plus per-session total cap mirroring RGP 64/256 MiB
- rec: Mirror RGP exactly: per-id pending cap + per-session total, checked before accepting each chunk, immediate free + x;code=cap_exceeded;detail=chunk_size on breach.

### CAPS-4: VRAM cap is whole-session but txn atomicity implies all-or-nothing GPU commit
- kind: interaction | section: 8.6, 15.2
- desc: max_vram_mb is a session-wide cap, but a single atomic txn can contain many asset.add ops; the doc doesn't say whether VRAM accounting is checked pre-commit for the whole txn, whether it counts CPU-staged bytes or actual GPU residency, or how instancing/textures count.
- why: A txn whose 3rd asset.add busts VRAM must reject the WHOLE txn (15.3) and free assets 1-2 already staged/uploaded; without a defined accounting point you get partial GPU allocation, leaked buffers, and the brief notes there is no VRAM accounting today at all.
- interacts: 8.6 atomic txn, 6.5 asset_revision re-upload, 6.4 dynamic instance re-upload, 15.3 cap_exceeded
- options: Two-phase: validate total txn VRAM delta against cap before any upload, then commit | Account at CPU-staging time and only upload after txn validates | Count actual GPU residency with a high-water reservation per txn
- rec: Two-phase commit: compute the txn's net VRAM delta CPU-side, reject atomically if over cap (freeing nothing because nothing was committed), upload only on validation pass.

### CAPS-5: Error-code taxonomy is illustrative, not enumerated
- kind: underspecified | section: 15.3
- desc: Only cap_exceeded and parse_error appear with ad-hoc detail= strings (max_verts, accessor_oob); there is no closed enumeration of codes for msg_too_large, truncated, bad_node_ref, dup_asset_id, unknown_op, bad_enc, unsupported_fmt, vram_exhausted, etc.
- why: Apps must branch on code to drive fallback (principle 4); an open/undocumented code+detail set makes programmatic fallback unreliable and invites every implementer to invent strings, defeating the structured-error promise.
- interacts: 8.4 dup-asset-id error, 8.6 op-index citing, 7.1 caps, 15.5 hardened parsing rejects
- options: Define a closed code enum in the doc with stable string values and free-form detail | Numeric codes + symbolic alias | Tier codes: fatal-txn vs warning
- rec: Pin a closed, versioned code enum (codes are stable, detail is free-form/advisory) covering at least cap_exceeded, parse_error, msg_too_large, truncated, bad_ref, dup_id, unknown_op, unsupported, vram. Mark detail explicitly non-load-bearing.

### CAPS-6: Opt-in error reporting has no defined negotiation knob **[USER DECISION]**
- kind: missing-behavior | section: 15.3, 7.1, 12.5
- desc: 15.3 says errors are 'only emitted to apps that opted in' but neither §7.1's caps reply nor §8.5's taxonomy defines HOW an app opts in (no x in feat=, no sub type for errors, no flag on q).
- why: Without a concrete opt-in mechanism the central improvement over RGP's silent drop is unreachable; worse, the default (silent like RGP, per the brief's discarded-Result path) means an app sending its very first malformed txn gets no signal and can't even learn why detection/upload failed.
- interacts: 7.1 tgp;q query flags, 8.5 sub message, 12.5 event subscription, 15.3 x replies
- options: A flag on the query: tgp;q;v=2;errors=1 | Reuse sub to subscribe to x/e at session scope | Implicitly enable errors whenever the app has sent any tgp; frame (proven non-dumb-reader)
- rec: Implicitly enable x for the rest of the session once the app sends a valid tgp;q (it has proven it speaks TGP and reads replies), and additionally allow an explicit errors=0 opt-out; this avoids a silent first-failure.

### CAPS-7: Decode/parse-time cap (decompression bomb) has no enforcement locus or units
- kind: underspecified | section: 15.2, 15.5
- desc: 15.2 lists 'max decode/parse time' to guard decompression bombs but doesn't define units (wall time? output bytes? ratio?), where it's enforced (CBOR decode vs glTF accessor expansion vs texture/Draco decode), or what happens to the in-flight allocation on trip.
- why: Wall-clock time caps are non-deterministic and untestable in the GPU-free test harness the brief mandates; a CBOR/GLB with a huge declared accessor count or KTX2/Basis blob can expand far beyond max_msg_mb after passing the byte cap, the classic terminal-graphics crash vector.
- interacts: 15.5 hardened glTF accessor bounds, 11.2 texture/KTX2 decode, 8.3 CBOR decode, 15.2 byte caps
- options: Replace time cap with an output-size cap (max expanded verts/indices/texels) checked during decode | Cap decompression ratio per blob | Combine: hard output-byte ceiling plus a watchdog time as a backstop
- rec: Make the primary guard a deterministic max-expanded-output cap (verts/indices/texels, already implied by max_verts) enforced incrementally during decode; keep a wall-clock watchdog only as a non-test-relied backstop.

### CAPS-8: Which failures stay silent vs reported is contradictory across native TGP and the RGP adapter **[USER DECISION]**
- kind: contradiction | section: 4, 15.3, 15.5
- desc: 15.3 promises structured errors for malformed TGP, but §4 routes RGP frames through an adapter that keeps RGP semantics (silent drop per brief term.rs:3420-3421/3417); the doc never says whether an error in an adapter-translated op surfaces as a tgp;x or stays silent.
- why: An app that negotiated TGP error reporting but sends a ratty;g; frame (or a mixed session) gets undefined behavior; also unknown-but-not-ratty;g/not-G APC payloads are silently dropped today, so a typo'd tgp prefix vanishes with no error, undermining the detection-never-hangs guarantee.
- interacts: 4 adapter mapping, 15.3 opt-in errors, term.rs demux silent drop, 7.1 detection handshake
- options: Adapter-translated errors surface as tgp;x only if the session negotiated TGP, else stay RGP-silent | Keep RGP path fully silent always; document the asymmetry | Emit x for any malformed APC whose prefix is a near-miss of tgp;
- rec: Keep RGP-via-adapter silent (preserves the compat carve-out) but document the asymmetry explicitly, AND for native sessions emit x on a malformed tgp; frame rather than dropping, so detection failures are diagnosable.

### CAPS-9: Caps are advertised per-session but several are inherently per-asset/per-buffer, with no scoping in the reply
- kind: underspecified | section: 7.1, 15.2
- desc: The caps reply mixes scopes: max_verts/max_instances read as per-asset/per-buffer, max_vram_mb/max nodes are per-session, but the wire (max_verts=4000000) doesn't label scope, and 15.2 adds 'max nodes' and 'max instances per buffer' not in the §7.1 reply at all.
- why: An app pre-trimming (principle 4, 'pre-trim instead of getting silently rejected') can't tell if max_instances is the cap for one node.instances buffer or the session total across all instanced nodes, so it can't correctly budget a multi-viewport / multi-instanced scene.
- interacts: 6.4 instance buffers, 10.5 one scene many viewports, 15.2 cap list, 7.1 reply fields
- options: Suffix scope in field names (max_verts_per_asset, max_vram_mb_session, max_nodes_session) | Group reply into per_asset/per_session blocks | Document each field's scope in the spec and keep names terse
- rec: Rename caps with explicit scope suffixes and ensure §7.1 reply and §15.2 list are the SAME closed set (add max_nodes, max_instances_per_buffer, max_tex_dim to the reply).

### CAPS-10: node.instances re-upload-per-frame path has no rate/backpressure cap and unclear cap scope
- kind: interaction | section: 6.4, 8.6, 15.2
- desc: Dynamic instances re-upload the whole buffer every frame (point-cloud path, §18.4), but caps only bound size, not frequency or per-second VRAM churn, and a per-frame txn that busts max_instances mid-stream has undefined teardown (reject txn keeps last good buffer? clears node?).
- why: A malicious/buggy stream of full-buffer re-uploads is a sustained VRAM-churn / parse-bomb the size caps don't catch; and since txn rejection is atomic, a single over-cap frame in a live stream must leave the prior buffer intact or the viewport flickers/empties unpredictably.
- interacts: 6.4 dynamic instances, 8.6 atomic rejection, 15.2 max_instances, 6.5 scene_revision per mutation
- options: On over-cap node.instances, reject txn and explicitly retain the last committed buffer | Add an advertised max instance-buffer upload rate / coalesce to one per frame | Cap per-session instance-upload bytes/sec with backpressure via withheld acks
- rec: Specify that a rejected node.instances txn retains the last committed buffer (no flicker), and coalesce multiple instance uploads to the same node within a frame to the latest; defer a hard rate cap but reserve a caps field for it.

### CAPS-11: Fuzz targets named but no defined oracle for 'safe' vs the silent-drop baseline
- kind: open-question | section: 15.5
- desc: The fuzz harness targets the framer/decoder and asset parser, but the doc doesn't state the invariant being fuzzed (no-crash? no-OOM-above-cap? no-unbounded-time? structured-error-or-clean-drop?), nor whether fuzzing runs through the GPU-free Term seam the brief describes.
- why: Without an oracle the harness can only catch hard crashes, missing the cap-bypass / partial-allocation / stuck-parser bombs that are the actual threat; the brief's GPU-free Term::tgp_scene seam is the natural fuzz entry but isn't named as the target.
- interacts: 15.2 caps, 15.3 structured errors, test harness Term seam, 8.2 parser binary state
- options: Define invariants: every input either commits a bounded scene or yields x/clean-drop within bounded time+memory | Fuzz at the Parser::advance(&mut Term, bytes) seam to cover framer+demux+decode together | Add differential fuzz: adapter RGP path vs native must both stay bounded
- rec: State the oracle (bounded memory/time + either valid bounded scene or structured-error/clean-drop, never crash/leak) and target the GPU-free Parser->Term seam so caps and teardown are exercised end-to-end.

