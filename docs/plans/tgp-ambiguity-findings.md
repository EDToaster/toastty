# TGP ambiguity findings (raw, from lean hunt run wf_56159cac-40d)

_10 clusters, 110 items. Pre-dedup. Source: 10 parallel adversarial hunters._

## Capability negotiation & handshake  (CAPS)

### CAPS-1: DA1 pairing cannot be enforced by the terminal, yet the no-hang guarantee depends on it **[USER DECISION]**
- kind: underspecified | section: 7.1, 18.1
- desc: The 'always-answered' no-hang property relies entirely on the app voluntarily pairing tgp;q with ESC[c; the terminal cannot know a query was meant to be paired, and an app that sends tgp;q alone gets nothing back from a non-TGP terminal and hangs forever.
- why: Principle 4 makes detection the one thing the terminal 'must do well,' but the doc offloads the entire reliability guarantee onto app discipline with no terminal-side fallback (e.g. timeout guidance or a terminal that always answers tgp;q even if unsupported — impossible by definition).
- interacts: DA1 (ESC[c), tgp;q query, tgp;r reply, RGP s-query reply
- options: Mandate the DA1 pairing as protocol-required and define app behavior on DA1-reply-without-tgp;r as 'TGP absent' | Recommend an app-side timeout as a secondary fallback and document a default (e.g. 250ms) | Define a terminal-side rule that tgp;r is always flushed before the DA1 reply when both are queued so ordering is deterministic
- rec: Make the pairing normative (option 1) AND specify the terminal flushes tgp;r before the DA1 reply (option 3) so the app can treat 'DA1 seen, no preceding tgp;r' as definitive absence.

### CAPS-2: Reply/DA1 ordering is unspecified, breaking the 'DA1 arrived, no tgp;r => absent' inference
- kind: missing-behavior | section: 7.1, 18.1
- desc: 18.1 shows the app sending ESC[c then tgp;q, but never states whether the terminal must emit tgp;r before or after the DA1 reply; both replies funnel through the same pty_replies/queue_reply writeback channel (grounding item 4) so order is an implementation detail.
- why: The detection logic 'if DA1 replies but no tgp;r arrives, conclude no TGP' is only sound if tgp;r is guaranteed to precede DA1; otherwise the app must keep reading past DA1 indefinitely, reintroducing the hang the design claims to eliminate.
- interacts: queue_reply / pty_replies flush ordering, DA1 reply, tgp;r
- options: Require tgp;r to be enqueued/flushed strictly before the DA1 reply when both are pending | Require the app to send tgp;q BEFORE ESC[c and rely on FIFO writeback ordering | Leave order undefined and require apps to scan all input until DA1, buffering any tgp;r
- rec: Combine: app sends tgp;q first, terminal preserves FIFO on the shared writeback channel, so tgp;r is guaranteed before DA1 — make both halves normative.

### CAPS-3: feat= token set is presented as a strawman with no stability/extensibility contract **[USER DECISION]**
- kind: underspecified | section: 7.1, 16
- desc: The reply lists feat=geom,graph,instance,material,pbr,light,pick,event,explore,anim,skin,binframe but §16 says 'final op naming/field schema' is unpinned, leaving it undefined whether these tokens are frozen, how unknown future tokens are treated by apps, and which features are independently toggleable vs implied (e.g. does 'pbr' require 'material', does 'instance' require 'graph').
- why: Apps branch their fallback strategy on exact token strings; if a token is renamed or a dependency between tokens is unstated, an app may enable pbr without material support or mis-detect a feature, defeating partial-degradation (principle 4).
- interacts: feat= flags, material vs pbr, instance vs graph, binframe vs enc=
- options: Freeze the v1 token set as normative and define implication rules (pbr⇒material, pick⇒event-capable) | Declare apps MUST ignore unknown feat tokens and MUST NOT infer dependencies | Version the feat vocabulary alongside v= so additions are gated by version
- rec: Freeze the v1 set, state explicit implication rules, and require apps to ignore unknown tokens (forward-compat parity with §7.2 field-skipping).

### CAPS-4: Advertised caps are treated as static but the grounding brief shows caps are runtime-variable (resize, GPU device loss, headless) **[USER DECISION]**
- kind: missing-behavior | section: 7.1, 15.2
- desc: max_verts/max_instances/max_vram_mb/max_msg_mb are sent once at handshake, but VRAM and viable viewport pixel sizes change with window resize, GPU device loss, or headless/SSH operation; the doc never defines whether the terminal re-advertises caps or how an app learns its pre-trimmed budget became invalid.
- why: An app that pre-trims to advertised caps (the stated purpose of advertising) can still hit cap_exceeded after a device-loss-driven VRAM drop, and has no signal to re-query — the negotiation becomes stale silently.
- interacts: max_vram_mb cap, tgp;x cap_exceeded errors, resize event (§10.3, §12.5), viewport reflow
- options: Allow apps to re-issue tgp;q at any time and require a fresh tgp;r (re-negotiation on demand) | Define an unsolicited tgp;r push (or a caps_changed event) on material cap changes like device loss | Document caps as best-effort hints and route all real enforcement through tgp;x errors
- rec: Make tgp;q re-queryable mid-session AND emit an unsolicited tgp;r (or a caps event for error-opted-in apps) on device loss/headless transitions; keep tgp;x as the hard backstop.

### CAPS-5: Concurrent / repeated tgp;q queries have no defined response semantics
- kind: interaction | section: 7.1
- desc: The doc never says what happens if an app sends tgp;q twice (e.g. once at startup, once after resize) or interleaves tgp;q with in-flight binary patches; each query presumably produces a tgp;r, but mid-binary-frame (the consume-exactly-N state from grounding item 1) the query bytes are raw payload, not a control frame.
- why: A tgp;q whose bytes land inside an active len=N binary patch read will be silently swallowed as geometry, and an app that re-queries during streaming gets no reply and may conclude TGP was lost — a detection false-negative caused by framing state.
- interacts: binary frame consume-exactly-N state, tgp;p patches, tgp;r reply, version negotiation
- options: Specify tgp;q is always answered idempotently and queries are illegal inside a binary frame (app must wait for frame completion) | Require the terminal to never enter binary-read mode for control types so q is always parseable | Define that a second tgp;q with a different v= re-negotiates the session version, else is a no-op repeat
- rec: State tgp;q is idempotent and always answered, MUST NOT be sent mid-binary-frame, and a differing v= re-negotiates version (ties to the re-query item).

### CAPS-6: Version negotiation degrade rule is underspecified at both ends of the range **[USER DECISION]**
- kind: underspecified | section: 7.1, 7.2
- desc: 'App states max version; terminal replies with negotiated version; unknown future versions degrade to highest mutually understood' is undefined when the app's max is BELOW the terminal's min supported version, or when v= is omitted entirely from tgp;q.
- why: If the terminal only supports v2+ and an app says v=1, there is no defined reply (silent? error? v=2 anyway?), and v= omission could be read as v=1, v=latest, or malformed — each leading to different fallback decisions.
- interacts: v= in tgp;q, v= in tgp;r, feat= sets gated by version, tgp;x parse_error
- options: Reply with the terminal's minimum supported version and let the app refuse if too high | Treat below-min as 'unsupported' and emit a tgp;x (only if error reporting on) or omit tgp;r | Define v= as required; omission is parse_error; reply always carries the single negotiated integer
- rec: Require v= (omission = parse_error), reply with min(app_max, term_max) but if app_max < term_min, send tgp;r with v= set to term_min so the app can knowingly refuse — never silently drop.

### CAPS-7: Error-reporting and event opt-in signaling is split and partly undefined at handshake time **[USER DECISION]**
- kind: underspecified | section: 7.1, 15.3, 12.5
- desc: Structured errors are 'only emitted to apps that opted in' and events are gated by per-viewport sub, but the doc never says HOW error reporting is opted into — feat=event covers input events, yet there is no handshake field or op that turns on tgp;x error replies for a session.
- why: If error reporting requires opt-in but has no signaling mechanism, the very first malformed patch (including a malformed opt-in attempt) falls back to RGP-style silent drop (grounding item 5), so the app cannot distinguish 'rejected' from 'applied' — the core improvement over RGP evaporates at the boundary.
- interacts: tgp;x errors, feat=event, sub op, tgp;a acks, silent-drop path (term.rs:3417/3420)
- options: Add an explicit handshake field (e.g. tgp;q;errors=1) or a dedicated op to enable error replies | Make error replies always-on for any session that completed a tgp;q/tgp;r handshake (handshake itself is the opt-in) | Tie error reporting to the same sub mechanism as events with a reserved scope
- rec: Treat a completed tgp;q/tgp;r handshake as the error-reporting opt-in (option 2) — a participating app by definition wants structured errors; dumb readers never handshake so stay silent.

### CAPS-8: No way to distinguish a v1 TGP terminal from a no-TGP terminal when the app only speaks v1
- kind: interaction | section: 7.1, 7.2
- desc: Detection relies on receiving a tgp;r at all, but combined with the unspecified below-min version rule, a terminal that drops tgp;q for an unsupported version is indistinguishable from a terminal that does not speak TGP — both produce 'DA1 reply, no tgp;r.'
- why: An app cannot tell 'TGP present but too new/old for me' (where a different fallback like prompting to upgrade applies) from 'TGP entirely absent' (use sixel/ASCII), conflating two distinct fallback paths the design says the app must choose between.
- interacts: tgp;r presence test, v= negotiation, RGP s-query (separate reply), DA1 pairing
- options: Guarantee tgp;r is sent for ANY recognized tgp;q regardless of version (carrying the terminal's supported range), so presence always means 'TGP exists' | Add a min/max version range field (e.g. v=2;vmin=1) to tgp;r | Let apps fall back to probing the RGP s-query to detect a graphics-capable-but-not-TGP terminal
- rec: Always answer any well-formed tgp;q with a tgp;r and include a supported range (v=2;vmin=1) so 'tgp;r present' unambiguously means TGP exists and the app can compute compatibility.

### CAPS-9: binframe feat flag vs enc= negotiation overlap and conflicting failure modes **[USER DECISION]**
- kind: interaction | section: 7.1, 8.2
- desc: Binary framing is advertised both as a feat token (binframe) and via enc=bin,b64; it is undefined whether an app may send enc=bin when binframe is absent, or what happens (silent corruption vs error) if a transport strips raw bytes after the app chose enc=bin based on the reply.
- why: The grounding brief warns raw CBOR contains ESC/BEL/0x9C that corrupt the APC pre-scanner; if enc=bin is advertised but the live transport (tmux passthrough) can't carry it, the binary frame is silently mangled — and the doc gives no detection or downgrade signal mid-session.
- interacts: enc= in tgp;r, feat=binframe, len=N binary read state (grounding item 1), tgp;x parse_error, tmux passthrough
- options: Collapse binframe into enc= (drop the redundant feat token) and define enc=bin as advertised-only-when-safe | Require an app self-test (send a small enc=bin probe, expect a tgp;a ack) before committing to binary for the session | Define a tgp;x parse_error on corrupted binary so the app can downgrade to enc=b64
- rec: Drop the redundant binframe token (enc= is the single source of truth) and add a tiny enc=bin probe→ack handshake so the app verifies the live transport before streaming bulk binary.

### CAPS-10: TGP and RGP capability replies coexist with no defined precedence or cross-protocol session rule **[USER DECISION]**
- kind: interaction | section: 4 (Capabilities), 7.1
- desc: An app may send both the RGP s-query and tgp;q; the doc says the replies are 'separate' but never defines what an app should do if both answer (use TGP, ignore RGP?) or whether enabling TGP changes how subsequent ratty;g; frames are handled given they share the adapter and one scene model.
- why: A single app (or a TGP-aware app behind a wrapper that also emits RGP) could drive both protocols into one shared scene; without a precedence rule, asset/node id-space collisions between the RGP adapter (u32 placements→flat nodes) and native TGP (string node ids, u32 asset ids) are undefined.
- interacts: RGP s-query support_reply, tgp;r, RGP adapter→scene model, asset id u32 namespace, node id string namespace
- options: Define TGP-present as authoritative: an app that gets tgp;r should ignore the RGP reply and use TGP exclusively | Document that RGP-adapter and native TGP share the scene; specify id-namespace partitioning (e.g. RGP nodes get synthesized string ids) | Forbid mixing within one app and state behavior is undefined if mixed
- rec: Make tgp;r authoritative for TGP-aware apps and explicitly specify the id-namespace partitioning the shared scene model uses for adapter-originated RGP nodes vs native TGP nodes.

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

## Materials, lighting, MSAA, tone-map, theme-tint (section 11)  (MATE)

### MATE-1: Mixed default-material + PBR nodes in one viewport: tone-map applied to whom? **[USER DECISION]**
- kind: interaction | section: 11.1, 11.2
- desc: 11.2 says PBR is linear-light and gets a tone-map before composite, but 11.1's flow renders default-material and PBR nodes into the SAME offscreen color target, then 'apply tone-map (if PBR/linear)'; a viewport with both kinds has no defined rule for whether tone-map hits the default-material pixels too.
- why: Default material reuses RGP's sRGB-ish color×brightness path; running ACES/Reinhard over it would darken/desaturate the matte look that 11.2 promises is 'legible when tiny', producing visibly different colors for the same node depending on whether a PBR node shares the viewport.
- interacts: default material (11.2), PBR pipeline selection (11.1), theme-tint (11.2), sRGB color space (caps color=srgb, 7.1)
- options: Render default-material nodes directly in sRGB and PBR nodes in linear, tone-map only the linear contribution before merge (two sub-targets or a per-pixel material-class flag) | Make tone-map a whole-viewport operation but render default material ALSO in linear so it survives tone-mapping consistently | Forbid mixing: a viewport is either 'default' or 'pbr' mode, error on mixed | Always tone-map the whole viewport; redefine default material as linear-light so the result is well-defined
- rec: Render default material in linear light too (matching PBR) so the whole offscreen target is linear and a single per-viewport tone-map is unambiguous; pick the default key/ambient values to reproduce today's matte look post-tone-map.

### MATE-2: Default material 'exact look' is underspecified: hemispherical term, ambient level, brightness range **[USER DECISION]**
- kind: underspecified | section: 11.2
- desc: Default material is 'Lambertian + ambient (+ a soft hemispherical term)' with 'base × per-node tint × brightness', but no concrete ambient coefficient, hemisphere sky/ground colors, key direction, or brightness clamp/range is given, and it claims to 'generalize rgp.wgsl' which today is a fixed sun + ambient with no hemisphere term.
- why: Two implementers (or RGP-via-adapter vs native TGP) will produce visibly different default shading; the molecule-viewer RGP demo composited through the adapter must match the old hardcoded-sun look or existing demos regress.
- interacts: RGP adapter (4, color/brightness mapping), implicit default lighting (11.3), theme-tint (11.2), tone-map (11.2)
- options: Pin exact constants (key dir, key/ambient/hemisphere colors, brightness clamp) in the spec and require the adapter to reproduce rgp.wgsl bit-for-bit | Define default material = exactly today's rgp.wgsl (no hemisphere) and make hemisphere an opt-in flag | Specify the look qualitatively but require a golden-image test the adapter must pass
- rec: Pin the exact constants and explicitly state whether RGP-via-adapter uses the new hemisphere term or the legacy sun; gate the hemisphere term so adapter output is byte-stable against existing demos.

### MATE-3: Implicit default lighting + registered Light nodes: do registered lights replace or augment the built-in key? **[USER DECISION]**
- kind: interaction | section: 11.3
- desc: 'With zero lights registered, the default material is lit by a built-in key + ambient'; the doc never says what happens when the app registers ONE Light node — does the built-in key turn off (so one dim point light leaves the scene nearly black) or stay on (so registered lights only ever add)?
- why: This is the single most common lighting transition (app adds its first light) and either choice surprises someone: augment makes app lighting uncontrollable; replace makes a single weak light look broken — and it also interacts with whether default-material nodes even respond to registered lights.
- interacts: registered Light nodes (11.3), default material (11.2), PBR materials (do default-material nodes see registered lights?), light-count cap (11.3)
- options: First registered light disables the built-in key+ambient entirely (app takes over lighting) | Built-in lighting always on; registered lights are purely additive | Built-in lighting is itself a removable implicit Light node the app can delete/override by id | Per-viewport render flag chooses 'auto' vs 'manual' lighting
- rec: Treat the built-in key+ambient as an implicit default that is disabled the moment the scene contains at least one Light node, and document that default-material nodes ARE lit by registered lights; expose an override flag if an app wants both.

### MATE-4: Per-viewport light-count cap exceeded: no defined behavior
- kind: missing-behavior | section: 11.3, 15.2
- desc: 11.3 says 'a per-viewport light-count cap (advertised in caps)' but neither 11.3 nor 15.3 defines what happens when a viewport's visible subtree contains more Light nodes than the cap — drop extras, error, or merge — and the cap isn't in the 7.1 example reply (which lists max_verts/instances/vram/msg but no max_lights).
- why: Lights are nodes that animate and can be parented anywhere, so the visible light count is dynamic per frame; without a rule, exceeding it silently mid-animation gives nondeterministic shading and no app feedback, violating principle 3 (structured errors).
- interacts: Light nodes inherit transforms/animate (11.3), viewport root subtree (10.5), structured errors x (15.3), capability reply caps (7.1)
- options: Advertise max_lights in caps and emit an x error on the patch that pushes a viewport over cap | Silently use the N nearest/brightest lights per viewport and emit a one-time warning event | Hard-reject at node.upsert time if it would exceed the global light cap | Clamp per-viewport with documented selection order (by node id / by intensity) and no error
- rec: Advertise max_lights in the 7.1 reply and pick the N highest-intensity lights affecting the viewport deterministically, emitting an x error (cap_exceeded;detail=max_lights) only to apps that opted into error reporting; never silently nondeterministic.

### MATE-5: Theme-tint exact semantics + palette source undefined **[USER DECISION]**
- kind: underspecified | section: 11.2
- desc: Theme-tint says a viewport 'may request the default look adopt the user's terminal palette' but never defines the operation (which palette entries — fg/bg, the 16 ANSI, or accent?), whether it multiplies/replaces base color or recolors lighting, and how it composes with the existing 'base × per-node tint × brightness'.
- why: Without a defined formula, theme-tint output is implementation-defined and unpredictable across themes; it also collides with per-node tint and PBR baseColor — an app setting a deliberate red tint may get it overridden by a blue terminal theme.
- interacts: per-node tint (6.1 Mesh.tint, 11.2), PBR baseColor (11.2), default material only? (does theme-tint apply to PBR viewports), tone-map (11.2)
- options: Define theme-tint as a multiply of a chosen palette color (e.g. default fg) into the AMBIENT/key light only, leaving base+tint intact | Define it as remapping the neutral default base color to the theme fg, but skip nodes with an explicit non-white tint | Restrict theme-tint to default-material nodes only and specify the exact palette index used | Make theme-tint a named enum (fg-tint / palette-quantize / accent) rather than a boolean
- rec: Specify theme-tint as a bounded multiply of the terminal default-foreground color into the default material's diffuse only, applying only to nodes with no explicit tint and only on default-material (not PBR) nodes; document the exact palette source.

### MATE-6: Default MSAA sample count unspecified; cost vs VRAM cap unmodeled
- kind: underspecified | section: 11.4, 15.2
- desc: 'render.msaa is per-viewport; default a modest sample count' never names the number (2/4/8?), nor how the resulting NxMSAA color+depth offscreen memory counts against the advertised max_vram_mb, nor what happens if the requested sample count isn't supported by the GPU.
- why: MSAA multiplies per-viewport offscreen memory by the sample count, and with many viewports this can blow max_vram_mb in ways the app can't predict from the advertised cap; an unsupported sample count needs a defined fallback or the renderer fails.
- interacts: per-viewport offscreen color+depth targets (10.4), max_vram_mb cap (7.1, 15.2), pick pass target (12.4), multiple viewports (10)
- options: Default 4x; clamp requested count to GPU-supported max and report the effective count back; count MSAA memory against max_vram_mb | Default 1x (off) for inline cell-sized viewports, 4x only when explicitly requested | Advertise supported sample counts in caps so the app picks a valid one | Auto-pick sample count by viewport pixel size (small = higher MSAA)
- rec: Default 4x, advertise supported counts in the capability reply, silently clamp to the GPU max, and explicitly state MSAA targets count toward max_vram_mb (and that the pick target is always 1x, since color-ID picking must not be antialiased).

### MATE-7: MSAA on the pick target would corrupt color-ID picking
- kind: interaction | section: 11.4, 12.4
- desc: 12.4's color-ID picking encodes node/instance ids as exact pixel colors that must be read back unmodified, but 11.4 makes MSAA per-viewport and 11.1's flow renders the pick target inside the same per-viewport block; if MSAA/resolve touches the pick pass it averages id colors into garbage.
- why: An averaged id color resolves to a nonexistent or wrong node/instance, silently mis-routing click/hover events to the wrong object — a correctness bug that only appears when MSAA is enabled, i.e. exactly the recommended default.
- interacts: per-viewport MSAA (11.4), pick pass (12.4), instance_index encoding (6.4, 12.4), tone-map (must also not touch pick target)
- options: Spec that the pick pass is always 1x non-MSAA and untouched by tone-map regardless of render.msaa | Use a separate non-MSAA pick render entirely (already implied 'render subtree → pick target') | If MSAA pick is ever wanted, require nearest-sample resolve, not averaging
- rec: Explicitly state the pick target is always single-sample and never tone-mapped or MSAA-resolved; this is engineering-obvious but must be written down because 11.1 lists tone-map+resolve as a viewport-wide step.

### MATE-8: Accepted texture formats undefined; caps reply has no texture-format/dimension advertisement **[USER DECISION]**
- kind: underspecified | section: 11.2, 15.2
- desc: PBR 'optional texture maps (texture = an asset of an image type; KTX2/Basis support is a later add)' never lists which image formats ARE accepted in v1 (PNG/JPEG? raw RGBA? embedded-in-GLB only?), and 15.2 mentions 'max texture dimensions' as a cap but 7.1's reply doesn't advertise it or any accepted-format list.
- why: An app sending a PBR texture has no way to know what encoding is safe; combined with the decompression-bomb caps (15.2), an undefined decoder set is both an interop gap and a security surface (which image parser runs on untrusted bytes).
- interacts: max texture dimensions cap (15.2), capability reply (7.1), hardened parsing / fuzz (15.5), asset.add image type (8.6), GLB-embedded textures (11.2)
- options: Advertise tex_fmt=png,jpeg and max_tex_dim in caps; accept only those in v1, reject others with parse_error | v1 accepts only textures embedded in GLB (no standalone image assets) to shrink the attack surface | Accept raw uncompressed RGBA8 byte assets only (no image decoder) plus GLB-embedded, defer PNG/JPEG | Accept PNG only (single hardened decoder) for v1
- rec: Advertise an explicit tex_fmt list and max_tex_dim in the capability reply, and limit v1 to one or two hardened decoders (PNG + GLB-embedded), rejecting everything else with a structured error.

### MATE-9: color=srgb cap vs PBR linear-light: is the offscreen target sRGB or linear, and where does conversion happen?
- kind: contradiction | section: 7.1, 11.2
- desc: 7.1 advertises color=srgb (single value) while 11.2 says PBR is linear-light requiring a tone-map; it's unspecified whether instance/material/tint colors on the wire are sRGB-encoded (needing linearization before PBR) and whether the offscreen color target is an sRGB-format texture (auto-encoding) or a linear UNORM the tone-map writes to.
- why: Getting sRGB-vs-linear wrong is the classic renderer bug: tints will look washed-out or too dark, and the default-material path (which today likely treats colors as sRGB directly) will mismatch the PBR path; the composite against text (also sRGB) must agree.
- interacts: PBR tone-map (11.2), default material color×brightness (11.2), per-node/per-instance tint (6.1, 6.4), composite vs text plane (10.4), theme-tint palette (11.2)
- options: Define all wire colors as sRGB; linearize on input for PBR, keep default material in sRGB, document the offscreen target format and where tone-map outputs sRGB | Make the whole pipeline linear internally with sRGB-encoded offscreen targets so the composite/text match is automatic | Add color=srgb,linear negotiation so apps can send linear tints for PBR directly | Specify per-field: tints sRGB, PBR metallic/rough linear scalars, textures carry their own color-space flag
- rec: Declare wire colors sRGB-encoded, render the whole viewport in linear internally, tone-map then sRGB-encode into the offscreen target so the composite against the sRGB text plane is correct; write the exact conversion points into 11.2.

### MATE-10: Per-instance material override (6.4) interacts with per-viewport PBR/default pipeline selection (11.1)
- kind: interaction | section: 6.4, 11.1
- desc: 6.4 allows an Instanced node to carry an optional per-instance material (Option<MaterialId>) plus a tint, but 11.1 selects the pipeline per node ('PBR pipeline for nodes referencing registered materials'); if some instances in one Instanced node reference a PBR material and others fall back to default, a single instanced draw spans two pipelines.
- why: A single draw_indexed(0..N) can bind only one pipeline, so mixed-material instances either require splitting the draw (defeating the instancing win) or are impossible — yet the doc presents per-instance material and one-draw instancing as both holding simultaneously.
- interacts: instanced draw one-draw-0..N (6.4, 11.1), default vs PBR pipeline selection (11.1), per-instance tint (6.4), tone-map (mixed linear/sRGB within one draw)
- options: v1: per-instance material restricted to the SAME pipeline/model as the node's base material (tint varies, model fixed) — reject mixing at validation | Bucket instances by pipeline and emit one draw per bucket (loses pure single-draw but bounded by material count) | Drop per-instance material in v1 entirely (tint only), defer to bindless as 16 already hints | Require all per-instance materials share the node's model; differ only in scalar/texture params usable as instance attributes
- rec: Constrain v1 per-instance material to the same shading model as the node's shared material (so one pipeline), or drop it to tint-only as the open-questions table already leans; otherwise the single-draw instancing claim is false for mixed materials.

### MATE-11: theme-tint and tone-map ordering vs the per-viewport composite is unspecified
- kind: underspecified | section: 11.1, 11.2
- desc: 11.1 lists 'apply per-viewport tone-map (if PBR/linear) + msaa resolve' as the only post steps and never mentions theme-tint in the flow, so the order theme-tint → lighting → tone-map → resolve → composite is undefined, including whether theme-tint runs before or after tone-mapping.
- why: If theme-tint is a color multiply applied after tone-mapping it shifts hues unpredictably; if before, it interacts with the lighting model — and since theme-tint is documented as a default-material feature while tone-map is a PBR feature, their interaction in a mixed viewport is doubly undefined.
- interacts: tone-map (11.2), theme-tint (11.2), default material lighting (11.2/11.3), mixed default+PBR viewport (11.1), per-viewport composite (10.4)
- options: Define theme-tint as a material-input stage (modifies diffuse before lighting), so tone-map naturally follows | Apply theme-tint as a post-resolve, pre-composite color grade with a documented operator | Disallow theme-tint on viewports containing PBR nodes (default-material only feature) | Fold theme-tint into the default-material shader only and exclude it from the tone-mapped path
- rec: Define theme-tint as a default-material shader-input stage (pre-lighting) and state it does not apply to PBR nodes, making the pipeline order theme-tint→light→(linear)→tone-map→resolve→composite unambiguous.

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
