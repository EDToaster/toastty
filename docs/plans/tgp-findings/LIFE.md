## Terminal-lifecycle interactions (cross-cutting)  (LIFE)

### LIFE-1: Alt-screen enter/leave never defined for viewports (inline AND pinned) **[USER DECISION]**
- kind: missing-behavior | section: 10.2, 10.4
- desc: The doc defines inline viewports as flowing with text and pinned as fixed screen regions, but says nothing about what happens on DECSET 1049 alt-screen enter/leave; full-screen TUIs (vim/less/tmux) live on the alt screen while inline anchors live in primary-screen scrollback.
- why: Images today are tied to a screen buffer; an inline viewport anchored in primary scrollback must be hidden on alt-screen entry and restored on exit, while a pinned viewport's fate (hide? persist over the TUI?) is a visible, recurring user-facing decision.
- interacts: Anchor::Inline ScrollbackLine, Anchor::Pinned, scrollback eviction, RGP adapter (RGP placements have no alt-screen model today either)
- options: Bind every viewport to the screen buffer it was created in; hide on switch, restore on return (kitty image model) | Inline follows its screen buffer; pinned is global and persists across alt-screen | Per-viewport screen_affinity field (primary|alt|both) with a default
- rec: Inline viewports bind to the screen buffer of creation (hidden on switch); pinned defaults to current buffer but expose a screen_affinity field so HUDs can opt into both.

### LIFE-2: RIS / DECSTR full reset has no defined effect on the retained scene **[USER DECISION]**
- kind: missing-behavior | section: 6, 15
- desc: The scene model is 'a single retained scene per terminal session' but the doc never says whether RIS (ESC c), soft reset (DECSTR), or CSI 3J (clear scrollback) tears down assets/nodes/viewports or leaves them resident.
- why: RIS is the standard 'reset my terminal' recovery path after a crashed app leaves garbage; if it does not free up-to-512MB of VRAM/scene state, users have no recovery and a hostile process can pin GPU memory permanently; if it does, a multiplexer's reset can nuke another pane's scene.
- interacts: max_vram_mb cap (§7.1/§15.2), session teardown, multiple processes writing TGP, alt-screen leave
- options: RIS = full scene.clear + free all GPU buffers + destroy all viewports | RIS clears viewports/nodes but keeps the asset cache (faster app restart) | RIS leaves scene intact (treat scene as out-of-band like an image cache)
- rec: RIS performs a complete scene teardown (assets, nodes, viewports, GPU buffers, pending chunk buffers) as the guaranteed VRAM-recovery path; DECSTR leaves the scene untouched.

### LIFE-3: Scrollback eviction of an inline anchor line is undefined **[USER DECISION]**
- kind: missing-behavior | section: 10.2
- desc: Anchor::Inline holds a ScrollbackLine, but the doc never states what happens when that line is evicted from the bounded scrollback ring (Term::new takes a scrollback limit) — does the viewport get destroyed, orphaned, or pinned to line 0?
- why: A long-running app that places many inline viewports will silently leak scene state and VRAM as anchors fall off the bottom of scrollback unless eviction also reclaims the viewport; conversely auto-destroy may surprise an app that scrolls a viewport back into view.
- interacts: max_vram_mb cap, node.remove / scene.clear, CSI 3J clear scrollback, structured error replies (should app be told?)
- options: Evicting the anchor line destroys the viewport and frees its layer (emit an optional 'destroyed' event) | Keep the viewport state but never render it; reattach if the line is somehow restored | Pin orphaned viewports to the top scrollback boundary
- rec: Anchor eviction destroys the viewport and frees its offscreen layer, with an opt-in event so subscribed apps can re-place; document that inline lifetime is bounded by scrollback depth.

### LIFE-4: DECSTBM scroll-region scrolling vs inline viewports is unspecified
- kind: interaction | section: 10.2, 10.4
- desc: clip_to_scroll_region exists as a viewport flag, but the doc never defines how a DECSTBM-bounded scroll (region scroll inside less/vim splits) moves an inline viewport whose cells partly straddle the region boundary, nor how partial-row scrolling shifts the cached layer.
- why: Region scrolling is the core mechanic of every pager/editor the doc names as target apps; if an inline viewport straddling a scroll-region margin shifts incorrectly (or text scrolls through it), it produces visible corruption exactly in the showcase use case (notebook/chat log).
- interacts: Anchor::Inline reflow (§10.3), compositing cached-layer re-composite (§10.4), alt-screen TUIs, Unicode-placeholder cell binding
- options: Inline viewports participate in region scrolls like text (placeholder cells move, layer re-composites) | Viewports straddling a scroll-region margin are clipped/hidden during the scroll | Disallow inline viewports inside an active DECSTBM region; downgrade to pinned
- rec: Treat the placeholder cells as ordinary cells that move with the scroll region and re-composite the cached layer at the new rect; clip the portion outside the region per clip_to_scroll_region.

### LIFE-5: GPU device loss / context reset has no recovery path defined
- kind: missing-behavior | section: 11, 6.5
- desc: The compositor adds per-viewport offscreen color+depth+MSAA targets and uploaded asset/instance buffers, but the doc never addresses wgpu device loss (driver reset, GPU hang, eGPU unplug) — the scene-revision/asset-revision gating assumes buffers persist.
- why: On device loss all GPU buffers vanish but the CPU-side scene survives; without a 'force re-upload everything' path the terminal renders nothing forever, and the dirty-flag equality gating (revision unchanged → don't re-upload) actively prevents recovery.
- interacts: asset_revision dirty gating (§6.5), scratch/offscreen targets (§10.4/§11.1), RIS reset, max_vram_mb
- options: On device-loss, invalidate all GPU state and force a full re-upload from CPU-side scene (bump effective revisions) | Treat device loss as session teardown (scene cleared, app must re-send) | Recreate device and lazily re-upload per-viewport on next render
- rec: Keep the scene fully CPU-resident (the design already mandates this for tests) and on device loss rebuild the device + force-reupload all assets/instances/targets ignoring revision equality; never lose CPU state.

### LIFE-6: Single global scene shared across multiple TGP-writing processes (scene leakage) **[USER DECISION]**
- kind: security | section: 6, 8.4
- desc: The doc mandates 'one global namespace per session for the scene' and 'one shared scene', but a PTY is written by many processes (shell + curl | cat + a backgrounded job, or panes under a non-passthrough multiplexer) all sharing the same node-id string namespace.
- why: Process A's node ids ('cam','car') can be silently overwritten via node.upsert by untrusted output from a log line or curl, and a malicious frame can read/retarget another app's viewports or exhaust the shared VRAM cap — a confused-deputy and DoS surface the threat model (§15.1) does not cover.
- interacts: upsert collision rule (§8.4), max_vram_mb shared cap, interaction event routing (which process gets the click event?), RIS reset (whose scene?)
- options: Accept one shared scene (status quo) and document the trust boundary explicitly | Partition scene/namespace per foreground process group or per controlling app via an opt-in session token | Scope ids by an app-declared namespace prefix negotiated at capability time
- rec: Keep one scene for v1 (matches image-cache reality) but add the cross-process collision/DoS to §15.1 and require an opt-in app-namespace token before honoring event subscriptions, so clicks/replies never go to the wrong process.

### LIFE-7: Event/error replies emitted while no foreground app is reading stdin **[USER DECISION]**
- kind: interaction | section: 12.5, 15.3, 7.1
- desc: Event reports, error replies, and acks go back via queue_reply → pty_replies (the DA1 writeback path), but the doc never addresses what happens when the subscribing app has exited, lost focus, or a different process now owns the PTY input when the terminal emits a tgp;e click report.
- why: A click/hover/camera report injected as ESC _ tgp;e;... into a shell prompt or a different app that doesn't speak TGP becomes garbage on the command line (the classic 'mouse escape leaks into bash' bug), and unconsumed event/ack frames can back up the reply buffer.
- interacts: sub subscriptions (§12), focus loss, PTY EOF / app exit, auto-spin camera events, explore controller continuing after subscriber gone
- options: Auto-cancel all subscriptions + explore + animation on the subscribing app's exit / PTY input-owner change | Gate event emission on terminal focus and a live subscription only | Keep emitting (app's problem) but stop on PTY EOF
- rec: Tie subscriptions, explore, and terminal-side animation lifetime to the subscribing app: cancel them when the foreground process changes or the PTY input side closes, and never emit events while the terminal is unfocused.

### LIFE-8: Copy/paste and text selection over a viewport's cells is undefined **[USER DECISION]**
- kind: missing-behavior | section: 10.2
- desc: Inline viewports occupy real grid cells via Unicode placeholders, but the doc never says what a user selecting/copying across those cells yields, nor how mouse-drag selection coexists with the explore controller consuming drags in the same cells.
- why: Selecting a chat log containing an inline molecule must produce sensible clipboard text (the alt-text is the obvious answer and is already in the scene §13), and a drag that the user intends as a text selection will be silently eaten by an explore-enabled viewport — an input-routing collision.
- interacts: alt-text (§13), explore controller drag (§12.3), raw input forwarding (§12.2), Unicode-placeholder cell binding
- options: Copying placeholder cells yields the node/scene alt-text; modifier (e.g. shift-drag) forces text selection over an explore viewport | Placeholder cells copy as a fixed sentinel or nothing | Explore only claims drags that start inside the rendered geometry; empty viewport space falls through to selection
- rec: Copy over a viewport yields its alt-text; reserve a modifier (shift) to force terminal selection over explore-enabled viewports so drag intent is never ambiguous.

### LIFE-9: CSI 2J / ED erase-in-display vs viewports and the cached layer
- kind: interaction | section: 10.2, 10.4
- desc: The doc covers scroll and resize reflow but never CSI 2J (clear screen) or CSI 2K (clear line) — whether erasing the cells under/over a pinned or inline viewport destroys the viewport, clears its placeholder cells, or just clears text leaving the sticker floating.
- why: Apps routinely clear-screen between frames; if 2J leaves a pinned viewport composited over freshly-cleared cells (correct for a HUD) vs if it silently wipes an inline viewport's placeholder cells (orphaning the scene), the behavior must be deterministic or apps can't reason about redraw.
- interacts: Unicode-placeholder cells (inline), pinned persistence, scrollback eviction, alt-screen clear-on-enter
- options: 2J clears text only; pinned viewports persist, inline placeholder cells (and thus the viewport) are erased like any cell | 2J destroys all viewports on the active screen | 2J leaves all viewports; only explicit vp destroy removes them
- rec: ED/EL erase the underlying cells like normal text — erasing an inline viewport's placeholder cells destroys/orphans it, while pinned viewports (not cell-bound) persist; document this so apps clear deliberately.

### LIFE-10: Resize / device loss / theme change while explore-dragging or animating
- kind: interaction | section: 10.3, 11.2, 12.3, 14
- desc: SIGWINCH reflow, GPU device loss, and runtime theme/palette change (which affects theme-tint §11.2 and default-material legibility) are each defined in isolation but never in combination with an in-flight explore drag or a terminal-side animation/auto-spin advancing the camera/clip clock.
- why: A reflow mid-drag changes the cell→pixel rect under the cursor so drag deltas become discontinuous (camera jumps); a theme change mid-animation must re-tint without resetting the animation phase (RGP's apply_place already preserves phase, so the precedent exists); these combinations decide whether interaction feels stable or janky.
- interacts: explore damping/initial pose (§12.3), auto-spin / anim clock (§14), theme-tint render flag (§11.2), viewport dirty flag re-composite (§6.5), camera sync-back events
- options: Snapshot drag origin in viewport-normalized coords so reflow doesn't perturb in-flight deltas; recompute pixel rect without resetting drag/animation state | Cancel in-flight explore drag on resize/device-loss and emit a cancel event | Freeze animation clock during reset events and resume from saved phase
- rec: Track drag and animation state in resolution-independent terms (normalized coords + phase) so resize/theme/device-loss recompute pixel rects and re-tint without resetting interaction or animation phase; only device loss may cancel an in-flight drag.

### LIFE-11: PTY EOF / terminal close mid-binary-frame and mid-chunked-asset
- kind: missing-behavior | section: 8.2, 8.7
- desc: The binary frame switches the parser to consume-exactly-len-bytes and assets chunk across frames keyed by id with more=1, but the doc never defines behavior on PTY EOF or terminal close while a binary length read is incomplete or a multi-frame asset is half-reassembled.
- why: EOF mid-binary leaves the parser stuck expecting N more bytes (the grounding brief flags truncation/EOF mid-binary as an explicit hazard), and half-reassembled chunk buffers (up to the per-id 64MiB cap) leak VRAM/RAM until session teardown if not reclaimed on EOF.
- interacts: binary-length parser state (§8.2 / parser back-channel), chunk reassembly caps (§8.7/§15.2), session teardown, structured truncation error (§15.3)
- options: On EOF/close, abort any in-progress binary read and pending chunk buffers, freeing memory; no error (app is gone) | On a complete-but-shorter-than-len read, emit a truncation error and discard | Bound the binary read with a timeout so a stalled stream can't pin the parser
- rec: On PTY EOF/close, drop any in-progress binary frame and all pending per-id chunk buffers and free their memory immediately; add a max-message timeout so a stalled (non-EOF) binary read can't wedge the parser indefinitely.

