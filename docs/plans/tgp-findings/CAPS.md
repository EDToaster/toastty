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

