# M12 — RGP (Ratty Graphics Protocol) 3D pass

**Goal.** The ecosystem bet from the architecture doc — first non-Ratty terminal to support inline 3D objects via RGP.

**Scope.** Hand-rolled wgpu scene per decision #4 (rend3 is unmaintained, Bevy is too heavy; ~700 LoC of wgpu owns the entire 3D pass at 6.7 ms/frame, 92 MiB idle RAM).

`toastty-graphics::rgp` parses RGP's APC messages — the streaming APC scanner from decision #5 already handles the large payloads (RGP `.glb` assets can be tens of MB). The protocol has five core ops: support query, asset register, object place, object update, object delete. Translate each onto operations on a minimal scene graph.

Scene graph: `Node { transform, mesh: Option<MeshId>, children }`, `Mesh { vertex_buffer, index_buffer, material_id }`, `Material { base_color, base_color_texture: Option<TextureId> }`. Basic directional lighting via a single sun direction + ambient. No shadows, no PBR specularity, no normal mapping — start unlit + lambertian and let the upgrade path open up if RGP demands more.

`gltf` crate for loading. Vertex pulling for instancing where possible. Reuse the existing wgpu device/queue from `Renderer`.

**Render graph integration.** The 3D pass already has a slot in `architecture.md`'s diagram — pass 2, between background and cells. RGP renders into color + depth attachments; the cell pass reads that depth buffer so text can occlude objects, or sit underneath them, depending on the per-object z-index from the RGP protocol.

**Out of scope.** PBR materials, shadow maps, skinned animation. If scope grows past this, the escape hatch is Bevy headless (decision #4 has the cost numbers — 32 MB binary vs 5 MB hand-rolled, 184 MB active RAM vs 103 MB). Re-evaluate when we have a concrete need.
