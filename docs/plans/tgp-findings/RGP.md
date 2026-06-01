## RGP adapter & namespace coexistence (§4)  (RGP )

### RGP -1: RGP u32 node-key collisions with TGP string node-ids in one shared namespace **[USER DECISION]**
- kind: interaction | section: §4 (Adapter mapping) + §8.4 (Identity)
- desc: §8.4 mandates ONE global node namespace per session with node ids as app-assigned UTF-8 strings, but §4 maps RGP `p` (u32-keyed placements) to `node.upsert` flat nodes; the doc never says how an RGP u32 placement key becomes a string node id or how it is kept from colliding with a TGP app's own string ids.
- why: If both an RGP frame and a TGP patch run in one session, the adapter must mint node ids that can never alias a TGP-chosen id like "7"; otherwise an RGP placement silently mutates (upsert) a TGP node or vice-versa, corrupting either scene.
- interacts: TGP node.upsert/node.remove, §8.4 collision rule (upsert mutates), RGP apply_place/apply_update/apply_delete_one
- options: Reserve a non-app-typeable prefix for adapter-minted ids (e.g. "rgp:<u32>") that TGP bounded-length string ids can't legally produce | Keep RGP placements in a separate internal node sub-namespace never exposed to TGP addressing | Stringify the u32 raw ("7") and document collision as app's problem | Forbid mixing RGP and TGP in one session entirely
- rec: Reserve an adapter-only id prefix (e.g. "rgp:") and make it lexically illegal as a native-TGP node id, so the single namespace is shared but provably collision-free.

### RGP -2: Two scene models or one — diagram says one scene, but RGP keeps RgpScene semantics **[USER DECISION]**
- kind: contradiction | section: §4 + §5 + §6
- desc: §4/§5 diagrams show RGP and TGP feeding ONE TGP scene model, yet §4's "compatibility carve-out" says RGP frames keep RGP semantics including permissive path= and the hardcoded-spin/animation behavior, which live in RgpScene (two u32 wrapping revisions, animation_phase_rad spin) — features TGP explicitly removes; it is unstated whether the adapter writes into the unified TGP Scene or keeps a parallel RgpScene.
- why: RGP's always-on 1.0 rad/s spin (scene.rs:27) and its REPLACES-whole-style place semantics contradict TGP's app-authoritative no-surprise-animation principle; if RGP nodes live in the shared scene, the renderer must apply spin to some nodes and not others within one viewport.
- interacts: §14 animation (spin removed), §6.5 dirty-tracking (u64 vs RGP u32 wrapping revisions), §12.3 explore auto_spin, renderer per-viewport flow §11.1
- options: Adapter fully translates RGP into native TGP nodes and drops RGP-only behaviors (spin becomes nothing or an anim clip) | Keep RgpScene as a distinct retained store rendered by a legacy path, only the renderer is shared | Mark adapter-origin nodes with a per-node flag the renderer special-cases for spin
- rec: Keep a per-node origin flag so adapter nodes preserve RGP spin/place semantics while living in the shared scene; do not silently drop the spin since demos depend on it.

### RGP -3: RGP hardcoded spin preserved vs TGP 'no surprise animation' — which viewport drives it
- kind: interaction | section: §4 + §11.3/§12.3/§14
- desc: The brief confirms RGP spin must be preserved, but TGP introduces N viewports/cameras into one scene; the doc never says whether an RGP-placed (spinning) node spins in every TGP viewport that includes it, or only in the implicit RGP single viewport, nor whether tick_animations (which does NOT bump revision) still drives repaint scheduling under TGP's dirty-tracking.
- why: RGP's animation_deadline()=33ms repaint loop and revision-free tick must be reconciled with TGP's scene_revision/viewport-dirty model; if not, RGP nodes either freeze (no dirty flag) or force every TGP viewport to re-render every 33ms, defeating the cached-layer composite win (§10.4).
- interacts: §10.4 cached-layer re-composite, §6.5 dirty flags, scene.rs tick_animations/animation_deadline, §10.5 one scene many cameras
- options: Adapter spin sets a per-node dirty flag each tick so only viewports containing it re-render | Convert RGP spin into a terminal-side anim clip bound to that node (reuses §14 machinery) | Restrict RGP spin to a single legacy compatibility viewport
- rec: Convert RGP spin into an internal opt-in-equivalent anim clip on the adapter node so it rides §14's playback + dirty machinery uniformly.

### RGP -4: RGP s-reply and TGP r-reply both claim the capability writeback channel with overlapping prefixes
- kind: interaction | section: §4 (Capabilities) + §7.1 + §8.5
- desc: RGP answers `ESC _ ratty;g;s;… ESC \` and TGP answers `ESC _ tgp;r;… ESC \`, both via the single queue_reply->pty_replies channel; the doc says the replies are separate but never specifies behavior when an app sends BOTH a `ratty;g;s` query and a `tgp;q` in one burst, nor the ordering guarantee between the two reply families on the shared PTY writeback.
- why: An app probing both protocols (likely during migration) needs deterministic, non-interleaved replies; the brief notes both share DA1's flush path, so interleaving/ordering is real and unspecified.
- interacts: queue_reply / pty_replies drain, DA1 reply ordering, §7.1 always-answered DA1 pairing, RGP support_reply()
- options: Guarantee replies drain in receive order of their queries (document it) | State the two reply families are independent and apps must correlate by prefix | Define that a tgp;q also implies RGP support so apps need only one query
- rec: Document strict receive-order draining (already true of pty_replies) and that prefixes (`ratty;g;s` vs `tgp;r`) disambiguate; no merge.

### RGP -5: Capability advertisement mismatch: RGP advertises path=1/fmt=obj while TGP advertises neither
- kind: contradiction | section: §4 (Capabilities) + §7.1 + §15.4
- desc: RGP's support_reply() advertises `path=1;…;fmt=obj|glb` (brief: reply.rs) but TGP's reply (§7.1) advertises `feat=…` with NO path feature and the doc says native TGP has no path at all (§15.4); a migrating app that reads `path=1` from the RGP query may wrongly assume path works for TGP frames.
- why: The two capability surfaces describe contradictory file-access policies for the SAME terminal; an app mixing both could send a path= asset expecting it to work and get silent drop under the TGP namespace.
- interacts: §15.4 no file access, RGP support_reply path=1, §8.4 asset.add (inline only), adapter path carve-out
- options: TGP r reply explicitly states path=0/no-path feature to contrast RGP's path=1 | Document that path capability is namespace-scoped (RGP-only) and apps must not cross-read | Drop fmt=obj from RGP reply too since brief says OBJ is rejected at load
- rec: Explicitly advertise the absence (e.g. omit any path feat) AND note in the doc that RGP's path=1 is namespace-local; also flag that RGP advertises fmt=obj while the brief says OBJ is rejected (stale capability).

### RGP -6: Adapter mapping for RGP 'r' with source=payload vs path= is undefined for the TGP asset path
- kind: underspecified | section: §4 (Adapter mapping: r->asset.add)
- desc: §4 maps RGP `r`->`asset.add`, but RGP `r` has two source modes (path=<name> permissive file read, OR source=payload;more=;<b64> chunked inline); TGP asset.add is inline-only with a different chunking model (more keyed by id, §8.7) and numeric u32 ids — the doc never says whether the adapter performs the file read itself (preserving permissive path) then feeds bytes to asset.add, or maps path= to something TGP cannot express.
- why: asset.add immutability (§8.4: add on existing id is an error) conflicts with RGP's register-by-path/register-by-payload which can re-register; and the permissive path read must happen on the RGP side of the adapter, not inside the path-free TGP core.
- interacts: §8.4 asset immutability error, §8.7 chunking (more by id), RGP register_asset_by_path / register_asset, §15.4 no-file core
- options: Adapter resolves path= via the existing permissive resolver, then injects bytes into asset.add (path never crosses into TGP core) | Map RGP re-register to TGP remove+add to satisfy immutability | Keep RGP assets in the RgpScene store entirely, never as TGP assets
- rec: Adapter performs the permissive path read on the RGP side and feeds resulting bytes to an internal asset.add-equivalent, translating RGP re-register into remove+add to honor TGP immutability.

### RGP -7: RGP errors silently dropped vs TGP structured x errors — does adapter surface RGP failures **[USER DECISION]**
- kind: missing-behavior | section: §4 + §15.3
- desc: §15.3 contrasts TGP structured `x` errors with RGP's silent drop (term.rs:3420-3421/3417), but never says whether a frame that fails IN the RGP adapter (path read fail, format mismatch, cap overflow) gets a TGP `x` reply or preserves RGP silent-drop; the brief confirms RGP drops are intentional today.
- why: An app that negotiated TGP error reporting but sends an RGP frame has ambiguous error semantics; surfacing x errors for RGP frames would change RGP's documented behavior, while not surfacing them leaves adapter failures invisible.
- interacts: §15.3 opt-in errors, RGP silent drop term.rs:3420-3421, RGP handler caps 64/256 MiB, queue_reply channel
- options: RGP frames always keep silent-drop (carve-out parity), TGP x only for tgp; frames | Emit tgp;x for adapter failures only if the app also negotiated TGP error reporting | Add an opt-in ratty;g; error verb (out of scope)
- rec: Preserve RGP silent-drop for ratty;g; frames (carve-out parity) and document that x errors are TGP-namespace only; adapter failures stay silent.

### RGP -8: RGP cell-anchored placements vs TGP viewports: which viewport renders adapter nodes **[USER DECISION]**
- kind: interaction | section: §4 (flat nodes) + §10
- desc: RGP placements are cell-anchored directly (col/row -> pixel via pipeline.rs:196-212) with one implicit ortho camera, but TGP renders nodes ONLY through explicit Viewport objects with Camera nodes; §4 says RGP becomes flat nodes but never defines an implicit RGP viewport/camera, so RGP nodes have no viewport to render in under the TGP compositor.
- why: Under the new per-viewport offscreen compositor (brief constraint #2), a node with no viewport is never drawn; RGP's per-placement cell anchor doesn't map to TGP's per-viewport cell rect + camera model.
- interacts: §10.1 Viewport/Anchor, §10.5 one scene many cameras, RGP cell->pixel pipeline.rs:196-212, §11.1 per-viewport flow
- options: Adapter synthesizes one implicit full-grid inline viewport + ortho camera matching RGP's current projection | Per-placement implicit micro-viewport per RGP cell anchor | Render RGP via a retained legacy direct-to-pass path bypassing the viewport compositor
- rec: Synthesize an implicit RGP-compatibility viewport with an ortho camera reproducing pipeline.rs:196-212 so adapter nodes render identically to today.

### RGP -9: RGP apply_place REPLACES whole style vs TGP node.upsert sparse-merge
- kind: contradiction | section: §4 (p->node.upsert) + §8.6
- desc: §4 maps RGP `p` to `node.upsert`, but RGP apply_place REPLACES the whole placement style (preserving only animation phase) while TGP node.upsert is explicitly sparse — omitted fields preserved (§8.6); RGP `u` (which maps to patch/property-set) is the sparse one. Mapping `p` to upsert inverts RGP's replace semantics.
- why: An RGP app that re-places a node expecting fields it omitted to RESET will instead see them preserved under upsert's sparse merge — a silent behavior change for existing RGP apps, the exact thing the carve-out promises not to do.
- interacts: §8.6 sparse upsert, RGP apply_place (replace) vs apply_update (merge), §4 u->patch mapping, molecule-viewer demo
- options: Adapter maps RGP p to a full-replace upsert variant (clear-then-set) and u to sparse merge | Add a replace flag to node.upsert the adapter uses for p | Document the divergence as acceptable
- rec: Have the adapter implement RGP p as clear-all-fields-then-set (replace) rather than naive sparse upsert, so RGP replace semantics are preserved exactly.

### RGP -10: RGP u32 asset ids vs TGP u32 asset ids share immutability rule — cross-namespace asset aliasing **[USER DECISION]**
- kind: interaction | section: §4 + §8.4
- desc: Both RGP and TGP use app-assigned u32 asset ids in one global namespace (§8.4 + brief), but §8.4 makes TGP asset.add on an existing id an ERROR while RGP register_asset freely re-registers; an app mixing both, or two libs, can have an RGP `r id=5` and a TGP `asset.add id=5` collide with opposite collision rules.
- why: RGP register silently overwrites; TGP asset.add errors — the same numeric id under two verbs behaves contradictorily, and an RGP re-register could replace an asset a TGP node references, or vice-versa.
- interacts: §8.4 asset immutability error, RGP register_asset overwrite, asset_revision bump on register, node mesh references
- options: Separate adapter asset id space from TGP asset id space internally | Make RGP re-register also go through remove+add semantics | Document that asset id space is shared and last-writer-wins for RGP, error for TGP (status quo)
- rec: Internally namespace adapter asset ids separately from TGP asset ids; surface only via the verb that created them, eliminating the contradictory collision rules.

### RGP -11: RGP frames are one-op-per-APC and non-transactional; mapping to TGP atomic patches is undefined
- kind: interaction | section: §4 + §8.6 (atomic txn)
- desc: TGP patches are atomic multi-op transactions bumping scene_revision ONCE; RGP (brief) is strictly one op per APC with NO transaction. §4 maps each RGP verb to a TGP op but never says whether each RGP frame becomes its own one-op txn, whether it gets a txn id, or whether it can emit an ack/error.
- why: If each RGP op becomes a singleton txn it bumps scene_revision per frame (matching RGP's per-mutation revision bump, good), but the doc's ack (§8.5 a) and x-error machinery key off txn ids RGP frames don't have.
- interacts: §8.5 ack a, §8.6 atomic txn + scene_revision, RGP one-op-per-APC, §15.3 x errors cite op index
- options: Each RGP frame becomes an implicit single-op txn with a synthetic/absent txn id, no acks/errors | RGP frames bypass the txn layer and mutate the scene directly like today | Assign RGP frames a reserved txn id range
- rec: Treat each RGP frame as an implicit single-op application that bumps scene_revision once (matching today) but carries no txn id and never emits ack/x — keeping RGP's fire-and-forget contract.

