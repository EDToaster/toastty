## Scene model, nodes, transforms, dirty, instancing, ids, patch semantics (sections 6, 8.4, 8.6)  (SCEN)

### SCEN-1: node.upsert sparse-merge has no way to CLEAR an optional field (parent, alt, tint→none) **[USER DECISION]**
- kind: underspecified | section: 8.6 / 6.1
- desc: 8.6 says 'only the provided fields change... Omitted fields are preserved.' But Node has Option fields (parent, alt) and reparenting-to-root, removing alt-text, or resetting a tint are all 'set to none/default' — indistinguishable from 'omitted' under pure sparse-merge.
- why: Unparenting a node (making it a root) is a core graph operation; if omitting parent means 'keep' there is literally no op that detaches a node short of remove+re-add, which loses children and animation phase.
- interacts: node.upsert, roots set maintenance, node.remove, accessibility alt, transform propagation
- options: Reserve an explicit CBOR null meaning 'clear to default', distinct from key-absent | Add a dedicated clear:["parent","alt"] field on the op | Disallow clearing; require remove+re-add (document limitation) | Make parent a required field on every upsert
- rec: Adopt CBOR-null-means-clear vs key-absent-means-preserve, since CBOR distinguishes the two natively and it generalizes to every optional field.

### SCEN-2: Changing a node's kind via upsert is undefined (Mesh→Instanced, Group→Mesh, conflicting selectors) **[USER DECISION]**
- kind: underspecified | section: 8.6 / 6.1
- desc: node.upsert can carry mesh/camera/light/instances fields, but the doc never says whether upserting a 'mesh' field onto an existing Group converts its kind, errors, or is ignored — nor how conflicting fields (both mesh: and camera:) resolve.
- why: Kind transitions decide whether NodeKind is mutable in place vs immutable-per-id; they also affect asset_revision (a Group→Mesh now references an asset) and pick targets.
- interacts: NodeKind enum, asset_revision, node.instances vs node.upsert mesh, picking, dirty subtree
- options: Allow kind changes; last-writer-wins replaces NodeKind atomically in the txn | Forbid kind changes → error code kind_conflict; require remove+re-add | Allow only Group↔Mesh↔Instanced but forbid to/from Camera/Light | Make mesh/camera/light/instances mutually exclusive per op, error otherwise
- rec: Forbid kind changes on an existing id (error kind_conflict) and require mutually-exclusive kind-selector fields per op; clean and testable.

### SCEN-3: Cycle / self-parent detection in parent links is never specified
- kind: missing-behavior | section: 6.2 / 8.6
- desc: World transform = product of the parent chain (6.2) and parent is set via upsert, but nothing defines behavior if a patch makes A.parent=B and B.parent=A (or A.parent=A), which makes the lazy world-matrix walk loop forever.
- why: A cycle is a guaranteed hang/stack-overflow in the render-time chain walk — a DoS trivially reachable from untrusted PTY bytes, violating principle 3 (safe by default).
- interacts: transform propagation, dirty subtree recompute, atomic txn rollback, security threat model 15.1
- options: Detect cycles at txn commit (graph walk) and reject the whole txn with x code=cycle | Detect lazily at render and break the chain at the repeat (silent) | Cap parent-chain depth (e.g. 256) and error past it
- rec: Detect at commit time, reject atomically with structured x (code=cycle, cite op index), plus a hard depth cap as defense-in-depth.

### SCEN-4: Dangling and forward parent/asset/material refs within an atomic txn are unspecified **[USER DECISION]**
- kind: interaction | section: 8.6 / 8.4
- desc: upsert{parent:"car"} where car doesn't exist, or mesh:7 where asset 7 was never added, has no defined outcome; forward-references inside the same atomic txn (child op before parent op) are also undefined.
- why: Atomicity means the whole txn is validated together; whether refs resolve order-independently within a txn determines if apps must topologically sort ops — a major correctness/usability contract.
- interacts: atomic txn rollback, asset.add ordering, node.instances mesh ref, viewport camera/root ref, RGP adapter
- options: Validate all refs against post-txn state (order-independent); dangling → reject txn with x code=bad_ref | Validate against pre-op state (strict ordering; forward refs error) | Allow dangling parent (becomes temp root) but error dangling asset/material | Defer parent resolution lazily; error dangling asset/material immediately
- rec: Resolve all refs against the txn's final committed state (order-independent), rejecting unresolved refs atomically — matches the atomic-frame promise and frees apps from sorting ops.

### SCEN-5: Removing a node with children: orphan, cascade, or reject is undefined **[USER DECISION]**
- kind: missing-behavior | section: 8.6
- desc: node.remove takes one id; the doc never says what happens to children — recursive delete, reparent to root, reparent to grandparent, or error if children exist.
- why: Wrong choice silently leaks 10k orphaned nodes (memory cap risk) or silently deletes a wanted subtree; either way roots/dirty sets and pick targets must stay consistent.
- interacts: roots set, dirty-tracking, transform propagation, memory caps 15.2, scene.clear adapter mapping
- options: Cascade-delete the whole subtree by default | Reparent children to the removed node's parent (or root) | Error if node has children unless recursive:true flag set | Detach children to roots, preserving them
- rec: Cascade-delete the subtree by default with an optional reparent-to-root flag; cascade matches 'remove the car removes its wheels' intuition and bounds memory.

### SCEN-6: Instance buffer layout, element format, units, and count-coupling are entirely unspecified **[USER DECISION]**
- kind: underspecified | section: 6.4 / 8.6
- desc: node.instances carries xforms:<bytes> and tints:<bytes> but never pins the per-instance record layout (Trs vs mat4 — 6.4 says 'Trs|mat4'), float precision, byte order, stride, tint color space, or whether xforms/tints counts must match.
- why: This is the hot binary path; an unpinned layout means no interoperable codec and no bounds-checking — a malformed stride/count mismatch is an OOB read risk, and the renderer's @location instance attributes can't be defined.
- interacts: max_instances cap 7.1/15.2, static vs dynamic re-upload 6.4, picking instance_index, color=srgb cap, hardened parsing 15.5
- options: Fix one canonical layout: packed mat4 f32 LE + RGBA8 tint, counts must match or error | Header byte selects Trs (10 floats) vs mat4 (16 floats), counts validated | Require explicit count + stride fields in the op, bounds-checked | Mandate Trs-only in v1 (matches Node.trs) and defer mat4
- rec: Pin a single canonical packed layout (mat4 f32 LE + RGBA8) with an explicit instance count and a hard count==len/stride check; collapse the Trs|mat4 ambiguity to one fixed form for v1.

### SCEN-7: Static vs dynamic instance re-upload: 6.4 and 6.5 disagree on which revision bumps
- kind: contradiction | section: 6.4 / 6.5
- desc: 6.4 says static instances bump asset_revision 'when buffer identity/size changes'; 6.5's table says any 'instance buffer' change bumps scene_revision + node dirty. A same-size content re-upload (the point-cloud path) falls between these contradicting rules.
- why: If a per-frame point cloud bumps asset_revision, the renderer needlessly re-uploads meshes every frame (the exact regression 96da70a fixed); if it bumps only scene_revision the new instance bytes may never reach the GPU.
- interacts: asset_revision/scene_revision split 6.5, principle 6, node.instances op, point-cloud path 18.4, GPU upload gating
- options: Instance content/size changes always bump scene_revision + an instance-dirty flag; never asset_revision | Add a third revision/dirty class for instance-buffer GPU re-upload | asset_revision only when count grows past current GPU capacity; else scene_revision | Always re-upload instance buffer on node.instances; bump scene_revision
- rec: Treat instance buffers as scene-data: any node.instances bumps scene_revision plus a dedicated per-node instance-dirty flag driving a targeted instance re-upload — keeps mesh uploads gated by asset_revision.

### SCEN-8: Node-id charset/length unpinned and one-namespace collision with adapter-synthesized ids **[USER DECISION]**
- kind: security | section: 8.4
- desc: 8.4 says ids are UTF-8 'bounded length, e.g. ≤64 bytes' (the 'e.g.' leaves it unpinned) with no charset restriction; the RGP adapter maps u32 placements to nodes and must synthesize string ids in the SAME one global namespace, risking collision with app ids.
- why: Uncontrolled id strings enable memory amplification and — since 12.5 echoes the raw id back into the app's stdin inside an APC — control-char/ESC injection; adapter/app id collision silently aliases two different objects.
- interacts: RGP adapter section 4, event report wire format 12.5, one global namespace 8.4, memory caps 15.2
- options: Hard-cap 64 bytes, restrict to printable non-control non-ESC bytes, error otherwise; reserve an adapter prefix apps can't use | Allow arbitrary UTF-8 but escape ids in event reports and reserve numeric-only ids for the adapter | Keep app and adapter ids in fully separate namespaces (prefix or separate map)
- rec: Hard-cap length, forbid control/ESC bytes (they re-enter the app's stdin via 12.5), and give the adapter a reserved id prefix the protocol rejects from apps — closes both the injection and collision holes.

### SCEN-9: Asset immutability (add-is-error) contradicts chunked multi-frame asset.add
- kind: interaction | section: 8.4 / 8.7 / 8.6
- desc: 8.4: re-adding an existing asset id is an error. 8.7: a large asset.add spans multiple binary frames keyed by id with more=1. The second chunk's asset.add targets an id that 'already exists' per 8.4 — the immutability rule and the chunking protocol contradict.
- why: Every multi-frame asset would self-error on its second chunk; it's also unclear if a partially-received (more=1) asset is 'present' for node.upsert{mesh:id} refs or the duplicate-id check.
- interacts: chunking 8.7, atomic txn 8.6, bad_ref validation, asset_revision bump timing, reassembly cap
- options: Immutability applies only to fully-committed assets; in-flight chunks are a reassembly buffer, not a registered asset | Chunks share one logical id; immutability check fires only on the final more=0 frame | Require an explicit asset.begin/chunk/end sub-protocol distinct from asset.add
- rec: Define an in-flight reassembly buffer keyed by id that is NOT a registered asset until more=0 commits; the duplicate-id error and mesh-ref resolution both check only committed assets.

### SCEN-10: Revision width (u64 here vs u32-wrapping template) and equality-gate wrap hole
- kind: missing-behavior | section: 6.1 / 6.5
- desc: 6.1 declares revisions u64; principle 6 cites RGP's u32-wrapping split as the template (grounding: renderer gates on equality). The doc never states wrap behavior or that the gate is equality-based; a wrap landing exactly on the last-seen value skips a render.
- why: u64 makes wrap unreachable in practice, but mixing u64 here with the u32-wrapping cited template is an unflagged inconsistency; if anyone keeps u32 the equality gate has a rare correctness hole.
- interacts: dirty-tracking 6.5, renderer repaint gate, asset_revision/scene_revision split, per-node dirty flags
- options: Standardize on u64 everywhere and document 'wrap unreachable in practice' | Keep u32 but gate the renderer on a dirty-flag/generation token, not numeric equality | Use a saturating non-wrapping counter
- rec: Standardize on u64 (as 6.1 implies) and switch the renderer gate to consume per-node/per-viewport dirty flags rather than numeric equality, eliminating the wrap hole.

### SCEN-11: Per-node tint vs per-instance tint vs material baseColor compositing order undefined **[USER DECISION]**
- kind: interaction | section: 6.1 / 6.4 / 11.2
- desc: Mesh nodes have a tint (6.1); Instanced nodes have per-instance tints AND an optional per-instance material; 11.2 says default look is 'base × per-node tint × brightness'. For Instanced there is no single node tint, and how instance tint composes with a PBR material's baseColor (multiply? replace?) is unspecified.
- why: It defines the shader color math and whether re-tinting one instance (the §12.6 highlight-on-click flow) needs a full instance-buffer re-upload or a cheaper path; also whether tint is srgb or linear (PBR is linear-light).
- interacts: instancing 6.4, PBR materials 11.2, color=srgb cap 7.1, re-tint-on-click 12.6, tone-mapping
- options: Fixed chain: final = baseColor(or texture) × per-instance-tint × brightness, tints srgb-decoded to linear before multiply | Per-instance tint multiplies; node-level tint is N/A for Instanced (error if set) | Tint replaces baseColor when no material referenced, multiplies when material referenced | Per-instance tint is an emissive/highlight add, not a multiply
- rec: Pin one multiply chain (baseColor × tint × brightness) with explicit srgb→linear tint decode, document that Instanced nodes forbid a node-level tint, and allow single-instance re-tint via a partial instance update to avoid full re-upload.
