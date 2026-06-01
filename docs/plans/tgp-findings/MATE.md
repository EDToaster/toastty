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

