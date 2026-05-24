//! `RgpScene` — in-memory snapshot of registered assets + live
//! placements.
//!
//! The scene is a concrete struct with `&self` accessors only.
//! `toastty-render` reads from it; nothing else does. Decision §2 in
//! `docs/decisions/rgp-protocol.md` explains why this is not a trait
//! and why there is no Bevy-backed alternative implementation.
//!
//! Mutation goes through `apply_*` methods (called by
//! [`crate::rgp::handler::RgpSink::*`] on `Term`). Every mutation
//! bumps [`RgpScene::revision`], which the renderer uses as a sync
//! gate the same way the M11a image registry uses
//! `Term::image_revision`.

use std::collections::HashMap;

use crate::rgp::asset::CpuAsset;
use crate::rgp::operation::{
    RgpAnchor, RgpFormat, RgpPlacementStyle, RgpPlacementUpdate,
};

/// A registered RGP asset.
///
/// `data` is the parsed mesh + material. M12b's payload-mode `r`
/// verb runs `glb_loader::load_glb` on the incoming base64 and
/// fails registration if parsing fails — see decision §1 ("we
/// register the asset OR we return an error reply; we never
/// silently register garbage").
#[derive(Debug, Clone, PartialEq)]
pub struct RgpAsset {
    /// Declared format on the wire.
    pub format: RgpFormat,
    /// Optional `name=` hint, kept for diagnostics only.
    pub name: Option<String>,
    /// Parsed CPU-side mesh + material, ready for the renderer to
    /// upload.
    pub data: CpuAsset,
}

/// A live placement of a registered asset.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RgpPlacement {
    /// Anchor + cell span.
    pub anchor: RgpAnchor,
    /// Style + transform fields (mutated by `u`).
    pub style: RgpPlacementStyle,
}

/// In-memory snapshot of all registered assets and live placements.
///
/// The renderer pulls from this every frame. Accessors are
/// `&self`-only; the renderer must not (and cannot) mutate.
#[derive(Debug, Default)]
pub struct RgpScene {
    assets: HashMap<u32, RgpAsset>,
    placements: HashMap<u32, RgpPlacement>,
    revision: u32,
}

impl RgpScene {
    /// New, empty scene.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Read access to the registered asset table.
    #[must_use]
    pub fn asset(&self, id: u32) -> Option<&RgpAsset> {
        self.assets.get(&id)
    }

    /// Iterator over `(id, &asset)` pairs. Order is unspecified.
    pub fn assets(&self) -> impl Iterator<Item = (u32, &RgpAsset)> {
        self.assets.iter().map(|(k, v)| (*k, v))
    }

    /// Iterator over `(id, &placement)` pairs. Order is unspecified.
    pub fn placements(&self) -> impl Iterator<Item = (u32, &RgpPlacement)> {
        self.placements.iter().map(|(k, v)| (*k, v))
    }

    /// Look up a single placement by id.
    #[must_use]
    pub fn placement(&self, id: u32) -> Option<&RgpPlacement> {
        self.placements.get(&id)
    }

    /// Revision counter. Bumped on every mutation. The renderer
    /// compares this against its own seen-revision to detect changes.
    #[must_use]
    pub fn revision(&self) -> u32 {
        self.revision
    }

    /// Insert or replace a registered asset.
    pub fn apply_register(&mut self, id: u32, asset: RgpAsset) {
        self.assets.insert(id, asset);
        self.bump();
    }

    /// Insert or replace a placement. Note: re-placing an existing
    /// id replaces the *whole* style — the `u` verb is how you do a
    /// partial update.
    pub fn apply_place(
        &mut self,
        id: u32,
        anchor: RgpAnchor,
        style: RgpPlacementStyle,
    ) {
        self.placements.insert(id, RgpPlacement { anchor, style });
        self.bump();
    }

    /// Merge a sparse style update onto an existing placement.
    /// Returns `true` iff the placement existed (and was thus
    /// updated). Absent placements are a silent no-op — the `u`
    /// verb against a never-placed id is not an error.
    pub fn apply_update(&mut self, id: u32, update: &RgpPlacementUpdate) -> bool {
        let Some(p) = self.placements.get_mut(&id) else {
            return false;
        };
        p.style.apply(update);
        self.bump();
        true
    }

    /// `d;id=<n>`: drop the placement (the asset stays registered so
    /// a future `p` can reuse it). The spec is ambiguous about
    /// whether `d` should also drop the asset; we follow the
    /// principle of least surprise — `d` deletes the *placement* by
    /// id, `d` without `id` wipes everything.
    pub fn apply_delete_one(&mut self, id: u32) {
        let removed = self.placements.remove(&id).is_some();
        if removed {
            self.bump();
        }
    }

    /// `d` with no `id`: wipe all RGP state (assets + placements).
    pub fn apply_delete_all(&mut self) {
        if self.assets.is_empty() && self.placements.is_empty() {
            return;
        }
        self.assets.clear();
        self.placements.clear();
        self.bump();
    }

    fn bump(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_anchor() -> RgpAnchor {
        RgpAnchor {
            row: 5,
            col: 10,
            cols: 3,
            rows: 2,
        }
    }

    fn dummy_asset() -> RgpAsset {
        RgpAsset {
            format: RgpFormat::Glb,
            name: None,
            data: CpuAsset::unit_cube(),
        }
    }

    #[test]
    fn new_scene_is_empty() {
        let s = RgpScene::new();
        assert_eq!(s.revision(), 0);
        assert!(s.placement(1).is_none());
        assert!(s.asset(1).is_none());
        assert_eq!(s.assets().count(), 0);
        assert_eq!(s.placements().count(), 0);
    }

    #[test]
    fn register_then_place_bumps_revision_each_step() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        assert_eq!(s.revision(), 1);
        s.apply_place(1, dummy_anchor(), RgpPlacementStyle::default());
        assert_eq!(s.revision(), 2);
        assert!(s.asset(1).is_some());
        assert!(s.placement(1).is_some());
    }

    #[test]
    fn update_merges_only_set_fields() {
        let mut s = RgpScene::new();
        let style = RgpPlacementStyle {
            brightness: 0.5,
            rotation: [10.0, 20.0, 30.0],
            ..RgpPlacementStyle::default()
        };
        s.apply_place(7, dummy_anchor(), style);

        // Update only ry. Brightness and rx/rz must be preserved.
        let upd = RgpPlacementUpdate {
            rotation: [None, Some(99.0), None],
            ..Default::default()
        };
        let ok = s.apply_update(7, &upd);
        assert!(ok);
        let p = s.placement(7).unwrap();
        assert!((p.style.brightness - 0.5).abs() < 1e-6);
        assert!((p.style.rotation[0] - 10.0).abs() < 1e-6);
        assert!((p.style.rotation[1] - 99.0).abs() < 1e-6);
        assert!((p.style.rotation[2] - 30.0).abs() < 1e-6);
    }

    #[test]
    fn update_unknown_id_is_silent_noop() {
        let mut s = RgpScene::new();
        let upd = RgpPlacementUpdate::default();
        let ok = s.apply_update(99, &upd);
        assert!(!ok);
        assert_eq!(s.revision(), 0, "no-op must not bump revision");
    }

    #[test]
    fn delete_one_drops_placement_keeps_asset() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), RgpPlacementStyle::default());
        let rev_before = s.revision();
        s.apply_delete_one(1);
        assert!(s.placement(1).is_none());
        assert!(s.asset(1).is_some(), "asset must survive delete-one");
        assert_eq!(s.revision(), rev_before + 1);
    }

    #[test]
    fn delete_all_wipes_everything() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), RgpPlacementStyle::default());
        s.apply_delete_all();
        assert!(s.asset(1).is_none());
        assert!(s.placement(1).is_none());
    }

    #[test]
    fn delete_one_missing_does_not_bump_revision() {
        let mut s = RgpScene::new();
        s.apply_delete_one(42);
        assert_eq!(s.revision(), 0);
    }
}
