## Viewports & compositing (section 10)  (VIEW)

### VIEW-1: Inline viewport binding survives ScrollbackLine eviction undefined **[USER DECISION]**
- kind: missing-behavior | section: 10.1 / 10.2
- desc: Anchor::Inline { line: ScrollbackLine, col } pins a viewport to a specific scrollback line, but the doc never says what happens when that line is evicted from the bounded scrollback ring (Term::new takes a scrollback cap).
- why: An inline viewport whose anchor line is dropped becomes an orphan holding GPU offscreen+depth targets and a Camera node reference, leaking VRAM under the very caps §15.2 promises to enforce.
- interacts: scrollback eviction, §15.2 VRAM caps, §10.4 cached layer compositing, viewport dirty flags §6.5
- options: Auto-destroy the viewport (and emit a tgp;e or tgp;x to a subscribed app) when its line is evicted | Keep the viewport state but mark it permanently off-screen (never composited, retained until explicit vp destroy) | Promote eviction to convert the inline anchor into a detached/hidden state the app can re-anchor
- rec: Auto-destroy on eviction and, if the app subscribed, emit a lifecycle event; document it as a defined teardown so VRAM is reclaimed.

### VIEW-2: Inline placeholder row/col cap (~297) silently truncates large viewports **[USER DECISION]**
- kind: underspecified | section: 10.2
- desc: Inline anchoring is 'implemented via a Unicode-placeholder-style cell binding (a la kitty)'; per the grounding that scheme caps addressable rows/cols at ~297, but §10.1 cells: CellRect uses u16 and no doc text reconciles the two.
- why: An app requesting cells=0,12,40,400 (legal u16) silently exceeds the placeholder encoding and the inline region cannot be addressed past ~297, producing a partially-bound viewport with no error.
- interacts: kitty placeholder table, §10.1 CellRect u16, §15.3 structured errors, §7.1 advertised caps
- options: Advertise a max_inline_cols/max_inline_rows cap in tgp;r and reject oversized inline viewports with a tgp;x error | Redesign inline binding to not inherit the placeholder limit (custom binding) | Clamp silently to the encodable range (matches RGP silent-drop ethos but violates §15.3)
- rec: Advertise an explicit inline cell cap in the capability reply and reject over-cap inline viewports with a structured error; do not silently clamp.

### VIEW-3: Inline region occupancy vs text written into the same cells is undefined **[USER DECISION]**
- kind: interaction | section: 10.2
- desc: An inline viewport 'occupies real cells in the text grid' via placeholders, but the doc never specifies what happens when the app (or a TUI like vim/less) writes ordinary text into those occupied cells while the viewport is live.
- why: Whether text overwrites/evicts the viewport, whether the viewport masks the text, or whether they composite (transparent clear) determines reflow correctness in line-based apps the doc explicitly targets (vim/less/tmux).
- interacts: §10.4 transparent vs clear compositing, kitty placeholder semantics, reflow §10.3, ScrollbackLine edit
- options: Text writes into placeholder cells tear down the viewport binding (cells revert to text) | Viewport always occludes its cells; text written there is buffered but hidden until viewport destroyed | clear=None viewports composite over the underlying text; clear=Some occludes it
- rec: Tie occupancy to clear: clear=Some fully occludes its cells, clear=None composites over whatever text occupies them; define that overwriting placeholder cells with new text detaches the binding.

### VIEW-4: Reflow recompute on edit/reflow of the anchor line is undefined
- kind: missing-behavior | section: 10.3
- desc: Reflow is defined only for SIGWINCH/font-size (recompute pixel rect from cell rect), but the doc is silent on what happens when the anchor ScrollbackLine is edited or wrapped/reflowed (resize-driven rewrap) so the anchored col/line position moves or the line splits.
- why: On a narrower resize a wrapped line splits into multiple rows; the inline viewport's single {line,col} anchor no longer maps to a unique grid position, so its pixel rect is undefined exactly when reflow is supposed to 'just work like text.'
- interacts: §10.3 SIGWINCH reflow, ScrollbackLine reflow/rewrap, kitty placeholder cell re-emit, scene_revision vs viewport dirty
- options: Re-derive viewport position from where the placeholder cells actually land after rewrap (cells are the source of truth, not {line,col}) | Pin to the line's first display row after wrap and let cols clip | Forbid inline viewports on lines that can rewrap (only at column 0 full-width)
- rec: Make the placeholder cells the source of truth post-reflow (derive the rect from their landed positions), since the doc already commits to placeholder binding 'so it flows naturally.'

### VIEW-5: Viewport referencing a missing/deleted camera node has no defined behavior **[USER DECISION]**
- kind: missing-behavior | section: 10.1 / 10.5
- desc: Viewport.camera: NodeId is mandatory and references a Camera node by id, but §8.6 allows node.remove of any node and viewports can be created (vp) before the camera node is upserted (see §18.1 step 3 creates vp=2 referencing 'cam' before txn=2 creates 'cam'); the doc never defines render behavior when camera is absent or removed.
- why: A vp that points at a non-existent or just-removed camera must either fail to render, render with a default camera, or error; undefined means a dangling-reference crash path on a hot per-frame lookup.
- interacts: §8.6 node.remove, §18.1 vp-before-camera ordering, §10.5 one-scene-many-cameras, §15.3 structured errors, default camera
- options: Define an implicit default camera (like MaterialId 0) used when camera is missing/removed | Render nothing (blank/clear) and emit a tgp;x for that viewport when its camera is absent | Reject vp creation if camera node doesn't yet exist (forbids §18.1's ordering)
- rec: Provide an implicit default camera fallback (auto-frame the root subtree) so dangling refs degrade gracefully; emit an opt-in tgp;x diagnostic but keep rendering.

### VIEW-6: No default camera concept despite default-material/default-light precedent
- kind: underspecified | section: 10.1
- desc: The doc establishes implicit defaults for materials (MaterialId 0) and lighting (implicit key+ambient) but Viewport.camera is a required NodeId with no implicit default, so the simplest possible scene (one mesh, one viewport) cannot render without the app authoring a Camera node.
- why: It breaks the zero-config promise asymmetrically: materials and lights are zero-config but viewing is not, and it forces every minimal example (and the RGP adapter, which has no camera node concept) to synthesize a camera.
- interacts: §11.2 default material, §11.3 default lighting, §4 RGP adapter (RGP has no camera nodes), §10.5
- options: Make Viewport.camera Optional with an implicit auto-framing default camera | Require an explicit camera always (current implied state) | Auto-create a default Camera node id when a viewport names a camera that doesn't exist
- rec: Make camera optional with an implicit auto-framing default; this is also what the RGP adapter must synthesize anyway, so define it once.

### VIEW-7: Cached-layer re-composite is wrong when only the text plane moved under the viewport
- kind: contradiction | section: 10.4
- desc: §10.4 says a viewport that 'hasn't changed content' is re-composited from its cached layer by re-running the depth test against new text positions, but a cached color+depth layer was generated for the OLD pixel rect; if the inline viewport itself moved (scrolled) its cached depth is in stale viewport-local space.
- why: Depth values are computed in the viewport's own projection at render time; blitting the cached depth at a new screen offset and depth-testing against the (now differently-positioned) text plane gives incorrect occlusion unless depth is re-projected, which the doc treats as free.
- interacts: §10.4 stickers-with-depth, §6.5 viewport-moved-only dirty path, scroll handling, text plane z=0.5 §lib.rs:1539
- options: Cached layer is valid only for pure z-order/screen-translation moves where the text plane depth is constant (true for text at fixed NDC z); document that invariant | Re-render on any scroll that changes relative text/viewport depth | Store cached depth in screen space and re-bias on move
- rec: Clarify that text sits at a constant NDC depth so a pure screen translation preserves the depth comparison, making cache-recomposite valid for translation-only; spell out this invariant in the doc.

### VIEW-8: Z-order ties between overlapping viewports (and pinned vs inline) are unresolved
- kind: underspecified | section: 10.4
- desc: Overlapping viewports composite by z: i32, but the doc gives no tiebreaker when two viewports share the same z, nor any rule for pinned-vs-inline precedence when an inline viewport scrolls under a pinned one occupying the same cells.
- why: Non-deterministic compositing order on z ties produces flicker/instability, and pinned-over-inline overlap is a guaranteed real case (a pinned HUD over scrolling inline content) with no defined winner.
- interacts: §10.4 z-order composite, Anchor Inline vs Pinned, clip_to_scroll_region, viewport iteration order
- options: Break z ties by ViewportId (stable, deterministic) | Break ties by creation order | Define pinned always composites above inline regardless of z (or vice versa)
- rec: Break z ties deterministically by ViewportId and explicitly state inline/pinned both live in the same z space (no implicit layer separation) so apps control ordering via z.

### VIEW-9: Transparent (clear=None) depth-composite exactness for text BEHIND vs IN-FRONT is underspecified
- kind: underspecified | section: 10.4
- desc: clear=None means 'composite over text' and the depth test makes opaque geometry partly in front of / behind the text plane, but the doc doesn't define what fills the viewport's transparent pixels where no geometry was drawn: the underlying text, the clear color of nothing, or the cell background.
- why: Where the offscreen layer has no fragment (depth = far), compositing must show the text/cell content beneath; if the offscreen color target was cleared to opaque the inline viewport will punch a hole over the text it was supposed to overlay.
- interacts: §10.4 transparent vs clear, §10.4 cell background, text bg pass §lib.rs:1539, alpha of offscreen color target
- options: Offscreen color target uses premultiplied alpha; transparent (un-drawn) pixels alpha=0 and the composite is a straight over-blend on text | clear=None forces alpha-aware composite, clear=Some replaces underlying cells entirely | Always draw the cell/text background into the viewport layer first
- rec: Specify a premultiplied-alpha offscreen color target so un-drawn pixels (alpha 0) reveal text exactly, with the depth test only gating drawn opaque fragments.

### VIEW-10: Cell rect off-screen, larger than screen, or zero-size has no defined handling **[USER DECISION]**
- kind: missing-behavior | section: 10.1 / 10.3
- desc: CellRect (col,row,cols,rows) can place a viewport fully off-screen, larger than the terminal, or with cols=0/rows=0, but the doc never bounds it; per §15.2 offscreen targets are sized from the pixel rect, so a 10000x10000 cell rect demands a huge offscreen+depth allocation.
- why: An oversized cell rect is an unbounded-allocation / VRAM-DoS vector (the exact threat §15.1 cites) and a zero-size rect is a degenerate offscreen target; both need defined clamp/reject behavior.
- interacts: §15.2 max VRAM / offscreen sizing, §10.3 pixel rect mapping, §15.3 structured errors, cell->pixel rounding
- options: Clamp the offscreen target to the visible screen rect (allocate only the on-screen intersection) | Reject viewports whose pixel rect exceeds an advertised max with tgp;x | Allocate full requested size up to the VRAM cap, then error
- rec: Clamp the rendered/allocated region to the on-screen intersection (and treat zero-size as not-rendered), plus advertise a max viewport pixel size; never allocate off-screen area.

### VIEW-11: clip_to_scroll_region under tmux: terminal cannot see tmux pane geometry **[USER DECISION]**
- kind: interaction | section: 10.4 / 10.1
- desc: clip_to_scroll_region (default true) is meant to stop a viewport bleeding 'across tmux panes / scroll regions,' but a tmux pane is a tmux-side construct invisible to the inner toastty terminal, which only sees its DECSTBM scroll region within a single PTY.
- why: Under tmux the terminal physically cannot clip to a pane boundary it doesn't know about, so a pinned viewport in one tmux pane can render over an adjacent pane; the doc promises protection the architecture can't deliver.
- interacts: tmux passthrough (§16 open question), DECSTBM scroll region, Pinned anchor, binary frame passthrough §8.2
- options: Define clip_to_scroll_region as clipping only to the DECSTBM region/terminal viewport (honest scope), document tmux-pane clipping as out of reach | Require tmux integration/passthrough to forward pane geometry (heavy, §16) | Disable pinned viewports under detected multiplexers
- rec: Redefine clip_to_scroll_region as 'clip to the DECSTBM scroll region and terminal bounds' and explicitly state cross-tmux-pane clipping is not possible without multiplexer cooperation.

### VIEW-12: Inline anchor uses placeholder cell binding but anchor stores {line,col} — dual source of truth
- kind: contradiction | section: 10.1 / 10.2
- desc: Anchor::Inline carries an explicit { line: ScrollbackLine, col } yet §10.2 says position is realized via placeholder cells that 'flow naturally through line-based apps'; these are two competing position sources that can disagree after a TUI redraws/moves the placeholder cells.
- why: If the app (or vim) repaints and moves the placeholder glyphs to a different line/col, the stored {line,col} is stale; the renderer must pick one authority, and the doc names both.
- interacts: §10.2 placeholder binding, reflow §10.3, occupancy vs text interaction, kitty placeholder re-emit
- options: Make placeholder cells authoritative; {line,col} is only the initial hint | Make {line,col} authoritative and treat placeholders as render markers only | Forbid placeholder movement (static inline region)
- rec: Make the placeholder cells the single authority for inline position and demote {line,col} to an initial placement hint, consistent with the kitty model the doc cites.

