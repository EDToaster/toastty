## Explore, events, picking, subscriptions, raw mode (section 12)  (EXPL)

### EXPL-1: Explore + Events + Raw mode all consume the same pointer stream with no precedence rule **[USER DECISION]**
- kind: interaction | section: 12.1, 12.2
- desc: section 12.2 says the three levels are 'independently selectable', and section 12.1's router shows explore, events, and raw all reacting to the same mouse input, but nothing defines what happens when two or three are active on one viewport (e.g. a drag must orbit the camera AND be picked AND be forwarded raw).
- why: A drag is simultaneously an orbit gesture, a potential click/pick, and a raw event; without a precedence/consumption model the same gesture triggers conflicting actions or duplicate reports.
- interacts: 12.3 explore, 12.4 picking, 12.5 events, raw mode, 12.6 worked loop
- options: Define strict precedence: raw (if set) consumes everything and suppresses explore+events; else explore consumes drag/wheel and events still fire click/hover on the press/release | Make the three mutually exclusive per viewport (sub validation error if raw + events/explore both requested) | Let explore and events coexist with explicit gesture partitioning (tap=click event, drag=orbit) and forbid raw alongside either
- rec: Make raw mutually exclusive with explore+events on a viewport, and define explore+events coexistence by gesture: press/release that didn't move = click event; movement = explore (orbit/pan/wheel), with hover/enter/leave always firing.

### EXPL-2: App-enabled SGR mouse reporting vs viewport pointer capture — which subsystem eats the click **[USER DECISION]**
- kind: interaction | section: 12.1, 12.5
- desc: section 12.5 says 'Normal mouse reporting is unaffected unless the app enabled it' but never says what happens when the app HAS enabled SGR mouse reporting AND has an interactive TGP viewport: does a click inside the viewport go to SGR, to TGP events, to both, or is it position-dependent?
- why: TUIs that already use SGR mouse (vim, tmux, fzf) embedding an inline TGP viewport will either double-report the click or have the viewport silently swallow mouse input the TUI expects.
- interacts: 12.5 event reports, SGR mouse reporting, 10.2 inline viewport cell binding, raw mode
- options: Viewport with an active sub captures the pointer in its cell rect; SGR sees only clicks outside any viewport | Always send both SGR and tgp;e (app dedupes) | App declares capture policy in sub (capture | passthrough | both) | SGR always wins; TGP events only fire when SGR mouse mode is off
- rec: Default: an active TGP sub captures pointer events within its viewport cell rect (suppressing SGR there); outside any subscribed viewport, SGR is untouched. Add a sub flag to opt into both for hybrid TUIs.

### EXPL-3: Subscription lifecycle on node.remove / viewport destroy is undefined
- kind: missing-behavior | section: 8.5, 12.5
- desc: sub subscribes to a viewport and a node list, but the doc never says whether removing a subscribed node (node.remove) or destroying its viewport (vp destroy) auto-unsubscribes, leaves a dangling sub, or errors; nor whether re-creating a node id re-attaches the old sub.
- why: Dangling subs leak terminal state and can resurrect/misroute events when an app reuses node ids (node ids are app-assigned strings, easily recycled), causing events for the wrong object.
- interacts: node.remove, node.upsert (id reuse), vp destroy, 12.4 pick target allocation, 8.4 collision rule
- options: Auto-unsubscribe nodes on node.remove and drop the whole sub on vp destroy; recreating an id does NOT re-subscribe | Keep subs as id-pattern filters that auto-reattach when an id reappears (esp. nodes=*) | Treat removing a subscribed node as a txn error | Tear down node subs but keep nodes=* viewport-wide subs alive across membership changes
- rec: vp destroy drops all of that viewport's subs and frees its pick target; node.remove silently drops that node from explicit subs; nodes=* is a viewport-wide filter that naturally covers re-created ids. Recreating a specific id does not revive an explicit sub.

### EXPL-4: Pick readback timing/async vs event emission ordering is unspecified
- kind: underspecified | section: 12.4, 11.1
- desc: Picking renders a pick target and 'reads back the single pixel under the cursor', but GPU readback is asynchronous (1+ frame latency); the doc doesn't say whether the tgp;e click report is emitted synchronously on the input event, deferred until readback completes, or how stale-pick (object moved/removed between click and readback) is handled.
- why: Sync readback stalls the render thread on every click; async readback can emit a node id that no longer exists or has moved, producing events that contradict the current scene_revision.
- interacts: 6.5 dirty-tracking/scene_revision, 12.5 event reports, node.remove, 11.1 per-viewport pick pass, explore camera motion
- options: Async readback; stamp each tgp;e with the scene_revision the pick was resolved against so the app can detect staleness | Synchronous readback per click (simpler, accepts a stall) | Maintain a CPU-side last-pick-buffer refreshed each render and resolve clicks against it (bounded staleness, no stall) | Re-render pick target on-demand at the cursor only when a pointer event arrives
- rec: Async readback against a per-viewport pick buffer refreshed on render; include the resolving scene_revision in the tgp;e report and drop the report if the node no longer exists at emit time.

### EXPL-5: Hover/enter/leave state machine across frames, viewports, and scene mutation is undefined
- kind: missing-behavior | section: 12.5
- desc: enter/leave imply per-node hover state held across frames, but the doc never defines transitions when: the pointer is stationary while the object moves under it (explore/anim/patch), the cursor crosses between overlapping viewports, or a hovered node is removed/hidden — does leave fire, and against which node?
- why: Without a defined state machine the app gets missing or duplicate leave events (e.g. a removed hovered node never emits leave), leaving app-side highlight/tooltip state stuck on.
- interacts: 12.4 picking, node.remove, node.visible, explore camera motion, 10.4 overlapping viewports, anim playback
- options: Recompute hover each frame from the current pick buffer; synthesize leave whenever the previously-hovered (node,inst,vp) is no longer under the cursor — including when removed/hidden/moved | Only recompute hover on actual pointer-move input (cheaper, but stale when objects move under a still cursor) | Track hover per viewport independently and fire leave-old/enter-new on viewport boundary crossings
- rec: Recompute hover from the pick buffer every render (not just on pointer move); always synthesize leave for the prior (vp,node,inst) on any change including removal/hide/move, then enter for the new target. Hover state is keyed per viewport.

### EXPL-6: Focus loss / pointer leaving the terminal mid-drag (explore and raw) has no defined behavior
- kind: missing-behavior | section: 12.2, 12.3
- desc: Explore orbit and raw forwarding are drag-based, but the doc never addresses what happens when the terminal loses focus, the pointer leaves the window, or button-up is never delivered (common with X11/Wayland/SSH) mid-drag — does the orbit freeze, snap, keep damping, and is a synthetic up/leave emitted to the app?
- why: A dropped button-up leaves explore stuck in drag (camera spins forever) or the app's raw-mode drag state stuck open, a classic terminal mouse-capture bug.
- interacts: 12.3 explore (orbit/damping), raw mode, 12.5 drag/up events, focus events
- options: On focus-out / pointer-leave, terminate any active drag: stop orbit (honor damping), and emit a synthetic ev=up (and ev=leave) to subscribed apps | Freeze drag state and resume on focus-in (resync from current pointer) | Ignore focus loss; require apps to handle missing up themselves
- rec: On focus-out or pointer-leave, end the active gesture: explore stops dragging (damping continues to rest), and the terminal emits synthetic ev=up then ev=leave for any subscribed viewport so app drag state can close.

### EXPL-7: Camera sync-back report format and trigger cadence are unspecified **[USER DECISION]**
- kind: underspecified | section: 12.3, 12.5
- desc: section 12.3 promises optional camera sync-back and 12.5 lists ev=camera, but no field schema is given (pose as TRS? eye/target/up? orbit angles?) and no cadence (every frame of an orbit drag? on drag end? throttled?), unlike click/hover which have concrete fields.
- why: Per-frame camera reports during a 60fps orbit flood the PTY back-channel (the same queue_reply path as DA1/error replies), and an undefined pose format makes app persistence/round-trip non-interoperable.
- interacts: 12.3 explore sync-back, queue_reply/pty_replies back-channel, 8.4 addressing, node.upsert camera (app writing pose back)
- options: Emit ev=camera with a full TRS (matching node.upsert camera trs) on drag-end and on a throttled interval (e.g. <=10Hz), never per-frame | Emit eye/target/up triplet plus fov | Emit only on explicit app request (poll), not streamed | Emit orbit params (yaw/pitch/dist) specific to the explore controller
- rec: Report ev=camera carrying the camera node's resolved TRS (same schema the app would node.upsert), throttled (settle-on-end + capped rate), so the app can persist it round-trip-identically; never per-frame.

### EXPL-8: Overlapping viewports: which viewport receives the click is undefined for input despite z-order being defined for compositing
- kind: underspecified | section: 10.4, 12.1, 12.5
- desc: section 10.4 defines z-order for visual compositing of overlapping viewports, but section 12 never says input hit-testing uses the same z to pick the top viewport, nor what happens when the top viewport has clear=transparent and the click visually lands on a lower viewport's object showing through.
- why: With transparent viewports stacked, the visually-clicked object (lower vp) and the input-owning viewport (top vp by z) diverge, so the click is routed to a viewport showing nothing under the cursor.
- interacts: 10.4 z-order/clear transparency, 12.4 picking, 12.5 event reports (vp= field), clip_to_scroll_region, depth-aware composite
- options: Topmost viewport by z whose cell rect contains the cursor owns the input, regardless of transparency | Hit-test top-down through transparent viewports until the pick target reports a hit (pierce empty regions) | Only viewports with an active sub participate in hit-testing, top-down by z | Route to whichever viewport's pick pixel is closest in composited depth
- rec: Hit-test top-down by z among viewports with an active sub; if the top viewport's pick pixel is empty (no node) and its clear is transparent, fall through to the next lower subscribed viewport, so the click lands on the visible object.

### EXPL-9: Raw mode coexistence with terminal-driven reflow, explore, and the cached-layer composite is contradictory
- kind: interaction | section: 12.2, 10.3, 10.4
- desc: Raw mode says 'the app drives everything itself', yet section 10.3 says reflow is terminal-driven (app does nothing) and 10.4 caches/re-composites layers terminal-side; it's unclear whether raw mode also hands the app camera/reflow control or only forwards pointer bytes while the terminal still owns layout.
- why: If 'drives everything' includes the camera, raw mode silently disables explore and possibly reflow, conflicting with principle 1's separation; if it doesn't, the escape-hatch promise is overstated.
- interacts: 12.3 explore, 10.3 reflow, 10.4 compositing/cached layer, node.upsert camera
- options: Raw mode is pointer-forwarding only: terminal still reflows + composites; app must node.upsert the camera itself (explore is implicitly off) | Raw mode hands the app full control including suppressing terminal reflow (app re-issues viewport rects) | Disallow raw + explore simultaneously and keep reflow always terminal-side
- rec: Scope raw mode to pointer-event forwarding only; the terminal continues reflow and compositing, and explore is implicitly disabled for that viewport (raw and explore are mutually exclusive). The app drives the camera via node.upsert.

### EXPL-10: Wheel events split between explore-zoom and the app (events/raw) with no ownership rule **[USER DECISION]**
- kind: interaction | section: 12.3, 12.5
- desc: Explore offers zoom (scroll/pinch) and section 12.5 lists ev=wheel as a reportable event; when a viewport has explore zoom enabled AND subscribes to wheel (or scrolls over an inline viewport embedded in a scrollable pager), it's undefined whether the wheel zooms the camera, reports to the app, or scrolls the surrounding text.
- why: Scrolling a pager that contains an inline 3D viewport will either get trapped zooming the model or pass through and never zoom, and an app subscribed to wheel can't tell if explore already consumed it.
- interacts: 12.3 explore zoom, 12.5 wheel event, 10.2 inline viewport in pagers/less/vim, SGR mouse wheel, scrollback
- options: Explore zoom consumes wheel inside the viewport when enabled; wheel events only reported if explore zoom is off | Wheel always reported to app if subscribed; explore zoom only when not subscribed | Modifier-gated: plain wheel scrolls text/pager, modifier+wheel zooms explore | Wheel inside an inline viewport zooms; outside scrolls text, never both
- rec: Within a viewport: explore zoom (if enabled) consumes wheel; else if wheel is subscribed, report it; else pass through to surrounding text scroll. Document that explore-zoom and wheel-subscription on one viewport are mutually exclusive (validation warning).

### EXPL-11: Event back-channel ordering vs patch acks and the binary framer is unspecified under load
- kind: interaction | section: 12.5, 8.5, 8.6
- desc: tgp;e events, tgp;a acks, tgp;x errors, and DA1 replies all share the single queue_reply/pty_replies back-channel, but there's no statement about ordering or backpressure when high-rate events (drag/hover/camera) interleave with txn acks/errors the app is correlating.
- why: An app waiting for an ack of txn=42 may receive a flood of hover/camera events first, and event coalescing isn't defined, risking back-channel saturation and ack-latency that breaks the app's request/response correlation.
- interacts: 8.5 a/x/e frames, queue_reply back-channel, 12.3 camera sync-back rate, 12.5 hover/drag rate, 7.1 max_msg_mb caps
- options: Single FIFO back-channel preserving emit order; coalesce successive hover/camera events, never coalesce acks/errors | Priority queue: acks/errors ahead of high-rate events | Separate logical channels for control replies vs input events (distinct frame routing) | Drop oldest coalescable events under backpressure, never drop acks/errors
- rec: Keep one FIFO back-channel (matches today's queue_reply) but coalesce consecutive hover/camera/drag events per viewport and never coalesce or reorder a/x/q-r; advertise the back-channel as ordered so apps correlate acks reliably.

