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
use std::time::{Duration, Instant};

use crate::rgp::asset::CpuAsset;
use crate::rgp::operation::{
    RgpAnchor, RgpFormat, RgpPlacementStyle, RgpPlacementUpdate,
};

/// How fast `animate=1` placements spin, in radians per second.
/// One revolution every ~6.3 s — slow enough to read, fast enough
/// to register as motion. Hardcoded for v1; could become a config
/// knob later.
pub const ANIMATION_RATE_RAD_PER_S: f32 = 1.0;

/// Frame interval the animation deadline returns. ~33 ms ≈ 30 fps —
/// matches the rate the cursor blink uses and keeps the redraw
/// budget bounded.
pub const ANIMATION_TICK_INTERVAL: Duration = Duration::from_millis(33);

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
    /// Animation-driven Y-rotation phase in radians. Advanced by
    /// [`RgpScene::tick_animations`] whenever `style.animate` is
    /// true. Wraps at 2π. Frozen (kept at last value) when
    /// `style.animate` is false.
    pub animation_phase_rad: f32,
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
    /// Last call to [`Self::tick_animations`]. `None` until the
    /// first tick, then keeps the previous tick's `Instant` so
    /// subsequent ticks can compute elapsed time.
    last_animation_tick: Option<Instant>,
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
        // Preserve the existing animation phase across a re-place
        // (the app may be re-anchoring an already-rotating object).
        let preserved_phase = self
            .placements
            .get(&id)
            .map_or(0.0, |p| p.animation_phase_rad);
        self.placements.insert(
            id,
            RgpPlacement {
                anchor,
                style,
                animation_phase_rad: preserved_phase,
            },
        );
        self.bump();
    }

    /// True iff any placement currently has `animate=1`. Used by
    /// the renderer to decide whether to force frames through even
    /// when nothing else is dirty (mirrors the cursor-blink path).
    #[must_use]
    pub fn has_active_animations(&self) -> bool {
        self.placements.values().any(|p| p.style.animate)
    }

    /// Time until the next animation tick is due. `Some(interval)`
    /// when at least one placement is animating, `None` otherwise.
    /// The interval is fixed at [`ANIMATION_TICK_INTERVAL`] for v1
    /// (the renderer's redraw scheduler uses this as a deadline to
    /// poll against).
    #[must_use]
    pub fn animation_deadline(&self) -> Option<Duration> {
        if self.has_active_animations() {
            Some(ANIMATION_TICK_INTERVAL)
        } else {
            None
        }
    }

    /// Advance every animating placement's Y-rotation phase by the
    /// elapsed time since the previous tick. First call seeds the
    /// timestamp; subsequent calls advance proportional to the
    /// real-time delta.
    ///
    /// Does NOT bump `revision` — animation is a per-frame
    /// transient. The renderer redraws based on
    /// [`Self::has_active_animations`] / [`Self::animation_deadline`]
    /// instead.
    pub fn tick_animations(&mut self, now: Instant) {
        let Some(last) = self.last_animation_tick else {
            self.last_animation_tick = Some(now);
            return;
        };
        let elapsed = now.saturating_duration_since(last);
        self.last_animation_tick = Some(now);
        if elapsed.is_zero() {
            return;
        }
        let delta_rad = elapsed.as_secs_f32() * ANIMATION_RATE_RAD_PER_S;
        let two_pi = std::f32::consts::TAU;
        for p in self.placements.values_mut() {
            if !p.style.animate {
                continue;
            }
            p.animation_phase_rad = (p.animation_phase_rad + delta_rad) % two_pi;
        }
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

    // ---- M12c: animation tick + deadline ----

    fn animating_style() -> RgpPlacementStyle {
        RgpPlacementStyle {
            animate: true,
            ..RgpPlacementStyle::default()
        }
    }

    #[test]
    fn animation_deadline_none_when_no_animations() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), RgpPlacementStyle::default());
        assert!(!s.has_active_animations());
        assert!(s.animation_deadline().is_none());
    }

    #[test]
    fn animation_deadline_some_when_at_least_one_placement_animates() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        assert!(s.has_active_animations());
        assert_eq!(s.animation_deadline(), Some(ANIMATION_TICK_INTERVAL));
    }

    #[test]
    fn tick_animations_first_call_seeds_without_advancing() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        let phase_before = s.placement(1).unwrap().animation_phase_rad;
        s.tick_animations(Instant::now());
        // First call: only seeds last_animation_tick — phase is
        // unchanged because there's no "previous" timestamp.
        assert_eq!(
            s.placement(1).unwrap().animation_phase_rad,
            phase_before
        );
    }

    #[test]
    fn tick_animations_advances_phase_proportional_to_elapsed_time() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        let t0 = Instant::now();
        s.tick_animations(t0); // seed
        let t1 = t0 + Duration::from_secs(1);
        s.tick_animations(t1);
        // Expected: ANIMATION_RATE_RAD_PER_S radians.
        let got = s.placement(1).unwrap().animation_phase_rad;
        assert!(
            (got - ANIMATION_RATE_RAD_PER_S).abs() < 1e-3,
            "expected ~{ANIMATION_RATE_RAD_PER_S}, got {got}"
        );
    }

    #[test]
    fn tick_animations_skips_non_animating_placements() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_register(2, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        s.apply_place(2, dummy_anchor(), RgpPlacementStyle::default());
        let t0 = Instant::now();
        s.tick_animations(t0);
        s.tick_animations(t0 + Duration::from_secs(1));
        assert!(s.placement(1).unwrap().animation_phase_rad > 0.0);
        assert_eq!(s.placement(2).unwrap().animation_phase_rad, 0.0);
    }

    #[test]
    fn tick_animations_does_not_bump_revision() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        let rev = s.revision();
        let t0 = Instant::now();
        s.tick_animations(t0);
        s.tick_animations(t0 + Duration::from_millis(100));
        assert_eq!(
            s.revision(),
            rev,
            "animation ticks must not invalidate the GPU mesh cache",
        );
    }

    #[test]
    fn apply_place_preserves_animation_phase_on_replace() {
        let mut s = RgpScene::new();
        s.apply_register(1, dummy_asset());
        s.apply_place(1, dummy_anchor(), animating_style());
        s.tick_animations(Instant::now());
        s.tick_animations(Instant::now() + Duration::from_secs(1));
        let phase = s.placement(1).unwrap().animation_phase_rad;
        assert!(phase > 0.0);
        // Re-place the same id (new style). Phase must survive.
        s.apply_place(1, dummy_anchor(), animating_style());
        assert_eq!(s.placement(1).unwrap().animation_phase_rad, phase);
    }
}
