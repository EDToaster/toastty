## Animation clips, playback, skinning (section 14)  (ANIM)

### ANIM-1: Frame clock for terminal-side playback is undefined and the only precedent is wall-clock Instant
- kind: underspecified | section: 14 (playback); cf. scene.rs:27 / tick_animations(now)
- desc: section 14 says the terminal 'advances the clip clock' but never names the clock source; the only existing animation driver (tick_animations(now)) advances proportional to REAL elapsed Instant time, which is non-deterministic and untestable.
- why: The test harness (send-bytes -> assert tgp_scene()) is GPU-free and must be deterministic; a wall-clock clip clock makes node world-matrix assertions flaky and unreproducible.
- interacts: anim;seek, anim;speed, tgp_scene()/world-matrix accessors, animation_deadline()/repaint scheduling
- options: Drive the clip clock from a deterministic, injectable frame-clock abstraction (mockable in tests); keep wall-clock only as the production impl | Advance clips by an explicit per-frame delta passed into a tick(dt) call the harness can supply | Add an anim;seek-driven manual-advance test path while production uses Instant | Spec a 'logical tick' (e.g. fixed 33ms steps tied to animation_deadline) decoupled from wall time
- rec: Define a single injectable monotonic clock interface used by all playback; tests inject a controllable clock and advance it explicitly, mirroring how seek must already set absolute time.

### ANIM-2: Terminal-side playback vs app transform patches on the same node has no conflict/priority rule **[USER DECISION]**
- kind: interaction | section: 14 (app-driven by default vs opt-in playback); cf. 8.6 node.upsert sparse update
- desc: While 'anim;play;node=base' is active, an app can still send 'node.upsert id=base trs=...' patches; the doc says playback 'takes control of those nodes' but never says whether the patch is rejected, wins for one frame then gets overwritten, or co-applies.
- why: This is the central two-writers conflict; without a rule, app patches silently flicker or no-op, and the atomic-txn guarantee (8.6) is violated for any txn touching a playing node.
- interacts: node.upsert patches (8.6), anim;play/pause/stop, structured errors x (15.3), scene_revision bump semantics
- options: Playback owns the node's animated TRS channels exclusively; conflicting app upserts on those channels return an x error (op rejected) | App patch implicitly stops playback on that node (last-writer-wins, like explore yields to app) | Co-apply: clip sets channels it animates, app upsert sets the rest; undefined channels app-owned | Playback only advances when no app patch targeted the node this frame
- rec: Clip exclusively owns the node's TRS while playing; an app upsert on a playing node returns x;code=node_busy unless it carries an implicit stop, matching explore's 'terminal controls camera, app controls model' separation.

### ANIM-3: Clip referencing node ids that are removed (or re-added) during playback has no defined behavior **[USER DECISION]**
- kind: missing-behavior | section: 14 (clips reference node ids); cf. 8.6 node.remove, 8.4 upsert collision rule
- desc: Clips reference node ids and animate by setting their TRS, but the doc never says what happens when a node.remove deletes a target mid-playback, or when a same-id node.upsert re-creates it (does the old clip re-bind to the new node?).
- why: node ids are app-assigned strings and upsert mutates an existing id, so a remove+re-add cycle silently re-targets or orphans an active playback with no error surfaced.
- interacts: node.remove (8.6), node.upsert collision rule (8.4), anim;stop, x error replies (15.3)
- options: Removing a node auto-stops any clip playback bound to it (and any descendants) | Playback continues but silently skips missing target ids until they reappear, then re-binds | node.remove on a node with active playback returns x;code=node_busy | Bind playback to a node identity/generation so a re-added id is treated as a different node and playback does not re-attach
- rec: node.remove auto-stops bound playback for that node and its subtree and emits an event/ack; a later same-id upsert starts fresh with no auto-rebind, since string ids are reused intentionally.

### ANIM-4: play/pause/seek/stop is presented as verbs but no state machine, target granularity, or handle is defined
- kind: underspecified | section: 14; 8.5 (anim message taxonomy)
- desc: anim;play/pause/seek/stop are listed without a playback-instance identity: pause/seek/stop address a clip+node but the doc never defines whether multiple plays of the same clip on different nodes are distinct, how stop differs from pause+seek=0, or what play on an already-playing target does (restart vs ignore).
- why: Without a playback handle and defined transitions, an app cannot reliably pause/resume one of several concurrent animations, and idempotency/restart semantics are guessed by each implementer.
- interacts: anim;play/pause/seek/stop, clip ids (u32) + node ids (string), a ack frames (8.5), events (camera-style sync-back 12.3)
- options: Address playback by (clip,node) pair as the implicit handle; play on an active pair restarts from seek=0 | Return an explicit playback-instance id from play (in an ack) used by pause/seek/stop | Disallow concurrent plays of the same clip on one node; play while playing is a seek-to-start | Define an explicit state enum (Stopped/Playing/Paused) with documented transitions per (clip,node)
- rec: Use (clip,node) as the handle, define a Stopped/Playing/Paused state machine where play-while-playing restarts, stop resets to bind pose, pause holds current pose; echo state in optional a acks.

### ANIM-5: Animation playback and explore can both move nodes, but only camera-vs-model separation is specified **[USER DECISION]**
- kind: interaction | section: 14 vs 12.3 (explore moves camera node only)
- desc: Explore is justified as safe because it touches the camera node while the app owns model nodes; but anim;play can target a Camera node (clip=0;node=cam) just as explore controls that same camera, and nothing forbids it.
- why: If a viewport has both explore enabled and a clip playing on its camera node, two terminal-side controllers fight over one transform with no priority, breaking the clean 'different nodes' invariant the design leans on.
- interacts: explore controller (12.3), auto_spin ExploreOpts, camera sync-back events (12.3), Camera node kind (6.1)
- options: Forbid anim;play targeting a node that is the active camera of an explore-enabled viewport (return x) | Explore input overrides clip playback on the camera while the user is interacting, clip resumes on idle | Last-enabled-wins: enabling explore stops camera clips and vice versa | Allow both and define explicit composition order (clip first, explore delta applied on top)
- rec: Treat camera as exclusively owned by at most one terminal-side controller per viewport; enabling explore on a viewport stops/blocks clip playback on its camera node and emits an x or event, since both are 'terminal takes control' conveniences.

### ANIM-6: loop and speed have no spec for negative/zero speed, loop count, or non-looping end-of-clip behavior **[USER DECISION]**
- kind: underspecified | section: 14 (loop=1;speed=1.0)
- desc: loop is shown as a boolean and speed as a float with no bounds; undefined: speed=0 (freeze?), negative speed (reverse?), loop as count vs bool, and what happens at the end of a non-looping clip (hold last frame? snap to bind pose? auto-stop and emit an event?).
- why: End-of-clip is a lifecycle event with no defined response; apps relying on a one-shot animation finishing (then re-tinting/swapping) cannot know if the node stays posed or resets, and a reverse/freeze speed is a plausible request.
- interacts: anim;stop semantics, scene_revision bump (does end-of-clip bump?), a/e replies (end-of-clip notification), seek interaction with loop wrap
- options: loop=0/1 bool only, speed clamped to (0, max]; non-looping clip holds last frame and emits an anim_end event if subscribed | Allow speed<=0 (0=freeze, <0=reverse) and loop as an integer count with 0=infinite | On non-loop end, auto-stop and reset to bind pose with an event | loop bool + speed>0 only in v1; defer reverse/count to a later version flag
- rec: v1: loop bool, speed clamped to (0, speed_max] (advertise cap), non-looping clips hold the last frame and emit an optional anim_end event; defer reverse/freeze/count to a later version.

### ANIM-7: Skinning is 'in the model' but render support is staged — v1 behavior for a skinned asset is undefined **[USER DECISION]**
- kind: contradiction | section: 14 (skinning) + 16 (skeletal polish staged) + 17 phase 4
- desc: section 14 says skinning (joints + skin matrices + in-shader vertex skinning) is in the model, but 3.2/16/17 defer production skinning; the doc never says what the v1 renderer does when it receives a skinned glTF asset (render bind pose? rigid per-node? reject? error?).
- why: glTF skinning is defined over the node tree (per 6.3), so a real skinned asset will arrive day one via the inline-GLB path; with no defined fallback the renderer either crashes (counter to safe-by-default) or silently misrenders.
- interacts: asset.add glb parsing (15.5 full mesh set), feat=skin capability flag (7.1), x parse errors (15.3), instancing pipeline (skinned + instanced?)
- options: v1 accepts skinned assets but renders the bind pose (ignore skin) and does not advertise feat=skin | Reject skinned assets with x;code=unsupported;detail=skin until phase 4 | Render rigidly using node TRS, ignoring joint weights, as a graceful degrade | Advertise feat=skin only when the shader path lands; gate parsing on the flag
- rec: Do not advertise feat=skin in v1; accept skinned GLB but render the bind pose (skin ignored) so untrusted assets are safe, and let apps detect the missing flag to choose fallback per principle 4.

### ANIM-8: anim is a control frame but clip data has no defined upload path or id collision rule
- kind: missing-behavior | section: 14 (clips travel with asset) + 8.4 (clip ids u32) + 8.6 (no clip.add op)
- desc: Clips are said to be imported from glTF and addressed by u32 ClipId, but 8.6's op list has no clip.add and 8.4's collision rule covers nodes/assets, not clips; how a clip id is assigned (auto from glb? app-chosen?) and what happens on id collision or referencing an unknown clip id in anim;play is unspecified.
- why: anim;play;clip=0 in the robot-arm example assumes clip ids exist, but there is no op to create them and no error path for clip=<unknown>, so playback against a non-existent clip is undefined (silent no-op like RGP's apply_update absent case?).
- interacts: asset.add glb import (8.6), support_reply/caps (7.1), x error code for unknown clip (15.3), RgpScene-style silent no-op precedent
- options: Clips are auto-registered when their owning glb asset is added; clip id = (asset id, channel index) or a returned mapping in an ack | Add an explicit clip.add op to the patch taxonomy with the same add-is-error-on-collision rule as assets | anim;play on unknown clip returns x;code=unknown_clip; on collision, error | App assigns clip ids in asset.add metadata mapping glb animation index -> ClipId
- rec: Auto-register clips on asset.add and return the glb-animation-index -> ClipId mapping in an ack; anim;play against an unknown clip id must emit x;code=unknown_clip rather than silently no-op like RGP.

### ANIM-9: Terminal-side playback never specifies whether it bumps scene_revision or how it schedules repaints
- kind: interaction | section: 14 + 6.5 dirty-tracking; cf. scene.rs animation does NOT bump revision
- desc: Today's RGP spin advances phase WITHOUT bumping revision and uses a 33ms animation_deadline; section 14's playback mutates node TRS, which per 6.5 should bump scene_revision + node dirty, but the doc never states this nor how an animating subtree triggers per-frame world-matrix recompute and re-render.
- why: If playback follows RGP and skips the revision bump, the renderer (which gates on revision equality, u32 wrapping) won't re-render the animation; if it bumps every frame, it conflicts with the 'transform spam stays uniform-only, no re-upload' optimization and re-emits cells each frame.
- interacts: scene_revision/asset_revision split (6.5), node dirty flags + dirty subtree recompute (6.2/6.5), animation_deadline()/repaint scheduling, compositor cached-layer re-composite (10.4)
- options: Playback marks animated nodes' dirty flags each frame and bumps scene_revision (re-render, no asset re-upload), reusing the 33ms deadline | Introduce a separate playback-tick that re-renders the affected viewport without touching scene_revision (like RGP spin) | Bump a third 'animation_revision' so renderer re-renders but cell re-emit is skipped | Drive playback off the per-viewport dirty flag (viewport-local re-render) rather than global scene_revision
- rec: Playback sets per-node dirty flags and re-renders only the affected viewport via the existing dirty machinery without re-emitting unrelated cells; avoid bumping global scene_revision every frame to preserve the no-re-upload optimization.

### ANIM-10: anim playback control has no atomicity/ordering relationship with patches in the same byte stream
- kind: interaction | section: 14 (anim control frame) vs 8.6 (patches apply in receive order, atomic)
- desc: Patches are atomic and ordered; anim is a separate control frame. If an app sends a patch creating node 'base' and an anim;play;node=base in the same write, ordering across the two frame types and whether play takes effect before the patch commits is undefined.
- why: An app that plays a clip on a node it just upserted (robot-arm example does exactly this) relies on the upsert committing first; if anim is processed against pre-patch state it errors with unknown node.
- interacts: patch txn commit (8.6), node.upsert ordering, x unknown-node error (15.3), a acks
- options: All frames (patch + anim) process strictly in PTY receive order; anim sees committed scene state of prior patches | anim is deferred until the next render tick so any same-tick patch has committed | Require the app to await an ack before issuing anim against new nodes | anim;play against a not-yet-existing node is queued (pending) until the node appears
- rec: Process all TGP frames in strict PTY receive order so a prior patch's upsert is committed before a following anim;play observes the scene; an anim against a still-unknown node emits x;code=unknown_node.

### ANIM-11: Clip channels referencing nodes outside the playback's node= target are undefined for multi-node rigs **[USER DECISION]**
- kind: underspecified | section: 14 (anim;play;clip=0;node=base) + 6.3 (animation defined over node tree)
- desc: glTF clips animate many nodes (whole skeletons), but anim;play takes a single node=; it's unclear whether node= is the subtree root the clip drives (channels apply to descendants by name) or whether the clip carries absolute node targets and node= is just an offset/anchor.
- why: The robot-arm 'clip=0;node=base' must animate shoulder/elbow/wrist/hand; if node= is a single node the clip can only drive one node, contradicting the articulated-rig use case that motivates the whole graph.
- interacts: clip import from glTF channels (14), node hierarchy/world transform (6.2), skinning joint targeting (14), node id string addressing (8.4)
- options: node= is a re-root: clip channels target nodes by relative path/name under node=, enabling instancing a rig at multiple roots | Clip stores absolute node ids; node= is ignored or only a validation root | node= names the subtree root and the clip's channel node-ids are resolved against descendants | Support both: a flag selecting absolute vs relative-to-node= channel resolution
- rec: Define node= as the subtree root and resolve clip channel targets relative to it (by relative id/index), so one imported rig clip can be played on multiple instantiated subtrees; document the resolution rule explicitly.

