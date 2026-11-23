// SPDX-License-Identifier: MPL-2.0
//! The agent-facing grass handle: [`TerrainGrassHandle`], shared between the host (insert it as
//! a resource), the agent protocol (`terrain.grass` reads it, `terrain.set_grass` writes a
//! [`GrassRequest`]) and the bound [`crate::GrassLayer`], which applies the request on its next
//! frame and records a [`GrassReport`] (`book/agents/terrain-grass.md`).

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use regia_app::prelude::*;
use serde::{Deserialize, Serialize};

use super::quality::GrassQuality;

/// What the host or the agent asks of the grass. Applied on the next frame.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GrassRequest {
    /// Draw grass at all.
    pub enabled: bool,
    /// The tier; `None` follows the frame's render profile ([`GrassQuality::from_terrain_budget`]).
    pub quality: Option<GrassQuality>,
    /// Multiplies the tier's density, `0..=1`.
    pub density_scale: f32,
    /// Multiplies the tier's range, `0.1..=4` (offline renders push the far field out).
    pub range_scale: f32,
    /// Grass casts sun shadows where the tier allows.
    pub shadows: bool,
    /// Multiplies every class's sway, `0..=4` (0 = still air).
    pub sway_scale: f32,
    /// Debug tint: 0 shaded, 1 geometry LOD (near red, mid green, far blue), 2 patch level,
    /// 3 fade (the rank-band shrink), 4 the foliage profile's transmission through each blade
    /// toward the sun, raw linear RGB (ADR-3170).
    pub debug_view: u32,
}

impl Default for GrassRequest {
    fn default() -> Self {
        Self {
            enabled: true,
            quality: None,
            density_scale: 1.0,
            range_scale: 1.0,
            shadows: true,
            sway_scale: 1.0,
            debug_view: 0,
        }
    }
}

impl GrassRequest {
    /// Clamps every field into its documented range (non-finite values take the default).
    #[must_use]
    pub fn clamped(mut self) -> Self {
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        self.density_scale = fix(self.density_scale, 0.0, 1.0, 1.0);
        self.range_scale = fix(self.range_scale, 0.1, 4.0, 1.0);
        self.sway_scale = fix(self.sway_scale, 0.0, 4.0, 1.0);
        self.debug_view = self.debug_view.min(4);
        self
    }
}

/// A blast front the grass bends to (ADR-1497, from `regia_vfx`'s crash sequence): blades
/// behind a front running out from `center` lean away from it with the Friedlander wind
/// pulse `(1 − x) e^(−x)`, `x` the distance behind the front over [`Self::length`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct GrassGust {
    /// Crash point, world `[x, z]`.
    pub center: [f32; 2],
    /// Front radius this frame, metres.
    pub radius: f32,
    /// Front radius the frame before, metres (the blades' motion vectors).
    pub prev_radius: f32,
    /// Tip displacement right behind the front, as a fraction of the blade height (`0..=1`).
    pub bend: f32,
    /// Length of the outward wind behind the front, metres (its positive phase times the
    /// front speed).
    pub length: f32,
}

impl GrassGust {
    /// The gust with its fields made safe: `None` when any is non-finite or there is no bend;
    /// the bend clamped to `0..=1`, the length to at least 0.5 m and the radii to `>= 0`.
    #[must_use]
    pub fn sanitized(self) -> Option<Self> {
        let all = [
            self.center[0],
            self.center[1],
            self.radius,
            self.prev_radius,
            self.bend,
            self.length,
        ];
        if !all.iter().all(|v| v.is_finite()) || self.bend <= 0.0 {
            return None;
        }
        Some(Self {
            radius: self.radius.max(0.0),
            prev_radius: self.prev_radius.max(0.0),
            bend: self.bend.min(1.0),
            length: self.length.max(0.5),
            ..self
        })
    }
}

/// What the grass drew last frame.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct GrassReport {
    /// A grass layer is bound to the terrain draw and has prepared a frame.
    pub installed: bool,
    /// Drawing this frame (enabled, a tier that draws, at least one class).
    pub drawing: bool,
    /// The tier in effect (`"high"`).
    pub quality: String,
    /// The tier came from the frame's render profile (no host or agent choice).
    pub quality_auto: bool,
    /// Class names, set order.
    pub classes: Vec<String>,
    /// Patches selected (every class).
    pub patches: u32,
    /// Patches per level, fine first.
    pub patches_per_level: Vec<u32>,
    /// Patches inside the camera frustum.
    pub camera_patches: u32,
    /// Patches inside the shadow distance.
    pub shadow_patches: u32,
    /// Candidate clumps the GPU walks this frame (the drawn clumps are the ones the density
    /// law and the masks keep, at most `max_instances`).
    pub candidates: u64,
    /// The instance buffer's size, clumps.
    pub max_instances: u32,
    /// Range in effect, metres (the widest class).
    pub range_m: f32,
    /// Shadow distance in effect, metres (0 = no grass shadows).
    pub shadow_m: f32,
    /// Sun cascades the grass draws into.
    pub shadow_cascades: u32,
    /// Mask texels per side.
    pub mask_side: u32,
    /// Mask generation uploaded.
    pub mask_generation: u64,
    /// Sway clock, seconds (the host's [`TerrainGrassHandle::set_time`]).
    pub time_s: f32,
    /// Frames prepared since the layer was bound.
    pub frames: u64,
    /// Sun cascades whose grass shadow sways this frame: the ones the renderer's dynamic-caster
    /// class refreshes, once the sway clock has moved (spec 88 § 2.4; 0 = rest-pose shadows).
    pub shadow_sway_cascades: u32,
    /// Boxes the swaying shadow grass reported to the dynamic-caster class this frame.
    pub dynamic_boxes: u32,
    /// The blades were shaded with the surface-weather variant this frame: wet blades darken
    /// and gloss like the ground under them, snow whitens their tips (spec 99 § 6.4, grass
    /// stage 3). False on a dry frame, which keeps the dry pipeline.
    pub surface_weather: bool,
    /// Frames drawn with the surface-weather variant since the layer was bound.
    pub weather_frames: u64,
    /// The blades took the foliage profile's transmission this frame: the render profile's
    /// subsurface switch is on (`TerrainBudget::foliage_subsurface`, ADR-3170).
    pub subsurface: bool,
    /// The blast gust the grass bent to this frame ([`TerrainGrassHandle::set_gust`]).
    pub gust: Option<GrassGust>,
}

#[derive(Default)]
struct HandleState {
    linked: bool,
    /// The camera-body wake of this frame (ADR-1502).
    wake: Option<super::GrassWake>,
    request: GrassRequest,
    request_generation: u64,
    time_s: f32,
    /// The wind the blades sway in (ADR-1853).
    wind: crate::wind::TerrainWind,
    /// Queued mask edits `[x, z, radius, factor]`.
    edits: Vec<[f32; 4]>,
    /// A whole re-baked mask the layer takes on its next frame (ADR-7214).
    pending_mask: Option<super::GrassMask>,
    /// Masks the layer refused: their side or class count differed from the drawn mask.
    masks_refused: u64,
    /// Masks the layer took since the handle was made.
    masks_replaced: u64,
    gust: Option<GrassGust>,
    report: GrassReport,
}

/// Shared grass state (see the module docs). Cloning shares it.
#[derive(Resource, Clone, Default)]
pub struct TerrainGrassHandle {
    inner: Arc<Mutex<HandleState>>,
}

impl std::fmt::Debug for TerrainGrassHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.lock();
        f.debug_struct("TerrainGrassHandle")
            .field("linked", &state.linked)
            .field("request", &state.request)
            .finish_non_exhaustive()
    }
}

impl TerrainGrassHandle {
    fn lock(&self) -> MutexGuard<'_, HandleState> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A handle a [`crate::GrassLayer`] owns (the layer marks it linked).
    pub(crate) fn linked() -> Self {
        let handle = Self::default();
        handle.lock().linked = true;
        handle
    }

    /// A handle marked linked without a layer: for tools and tests that drive the request
    /// and read the report themselves (a [`crate::GrassLayer`] makes its own).
    #[must_use]
    pub fn new_linked() -> Self {
        Self::linked()
    }

    /// Whether a grass layer owns this handle (a default-constructed resource is not linked).
    #[must_use]
    pub fn is_linked(&self) -> bool {
        self.lock().linked
    }

    /// Sets the passing body's wake the grass bends to this frame (ADR-1502), `None` for still
    /// air. The host sets it every frame the body moves.
    pub fn set_wake(&self, wake: Option<super::GrassWake>) {
        self.lock().wake = wake;
    }

    /// The wake set by [`Self::set_wake`].
    #[must_use]
    pub fn wake(&self) -> Option<super::GrassWake> {
        self.lock().wake
    }

    /// The request in effect.
    #[must_use]
    pub fn request(&self) -> GrassRequest {
        self.lock().request.clone()
    }

    /// Replaces the request (clamped, [`GrassRequest::clamped`]); applied on the next frame.
    pub fn set_request(&self, request: GrassRequest) {
        let mut state = self.lock();
        state.request = request.clamped();
        state.request_generation += 1;
    }

    /// The request and its generation (bumped by every [`Self::set_request`]).
    pub(crate) fn request_with_generation(&self) -> (GrassRequest, u64) {
        let state = self.lock();
        (state.request.clone(), state.request_generation)
    }

    /// Sets the sway clock, seconds (the host's simulation time; a fixed-step render passes the
    /// step's time so two renders sway the same). Non-finite values are ignored.
    pub fn set_time(&self, seconds: f32) {
        if seconds.is_finite() {
            self.lock().time_s = seconds;
        }
    }

    /// The sway clock, seconds.
    #[must_use]
    pub fn time(&self) -> f32 {
        self.lock().time_s
    }

    /// Sets the wind the blades sway in (clamped, [`crate::TerrainWind::clamped`]); applied on
    /// the next frame. [`crate::TerrainPlugin`] pushes a changed [`crate::TerrainWind`] resource
    /// here ([`crate::sync_terrain_wind`]).
    pub fn set_wind(&self, wind: crate::wind::TerrainWind) {
        self.lock().wind = wind.clamped();
    }

    /// The wind in effect.
    #[must_use]
    pub fn wind(&self) -> crate::wind::TerrainWind {
        self.lock().wind
    }

    /// Queues a density edit: every class's mask within `radius` metres of `(x, z)` is scaled
    /// by `factor` (`0..=1`; 0 clears: a crater, a burnt patch) on the next frame. Non-finite or
    /// negative inputs are ignored.
    pub fn scale_density(&self, x: f32, z: f32, radius: f32, factor: f32) {
        if [x, z, radius, factor].iter().all(|v| v.is_finite()) && radius > 0.0 {
            self.lock().edits.push([x, z, radius, factor.clamp(0.0, 1.0)]);
        }
    }

    /// Sets the blast gust the grass bends to from the next frame on, or clears it with `None`
    /// ([`GrassGust::sanitized`]: a gust with a non-finite field or no bend clears it too).
    pub fn set_gust(&self, gust: Option<GrassGust>) {
        self.lock().gust = gust.and_then(GrassGust::sanitized);
    }

    /// The blast gust in effect.
    #[must_use]
    pub fn gust(&self) -> Option<GrassGust> {
        self.lock().gust
    }

    /// Takes the queued edits (the layer applies them).
    pub(crate) fn take_edits(&self) -> Vec<[f32; 4]> {
        std::mem::take(&mut self.lock().edits)
    }

    /// Queues a whole re-baked mask (spec 65 § 8: the biome multipliers changed and the host
    /// re-baked with [`super::GrassMaskInputs::multipliers`]). The layer takes it on its next frame
    /// when its side and class count match the drawn mask, and refuses it otherwise
    /// ([`Self::masks_refused`]). Edits queued before it apply to the new mask. A later call
    /// replaces a mask not yet taken.
    pub fn replace_mask(&self, mask: super::GrassMask) {
        self.lock().pending_mask = Some(mask);
    }

    /// The re-baked mask queued by [`Self::replace_mask`] and not yet taken.
    #[must_use]
    pub fn pending_mask(&self) -> Option<super::GrassMask> {
        self.lock().pending_mask.clone()
    }

    /// Takes the queued mask (the layer).
    pub(crate) fn take_mask(&self) -> Option<super::GrassMask> {
        self.lock().pending_mask.take()
    }

    /// Records that the layer took (`true`) or refused (`false`) a queued mask.
    pub(crate) fn note_mask(&self, taken: bool) {
        let mut state = self.lock();
        if taken {
            state.masks_replaced += 1;
        } else {
            state.masks_refused += 1;
        }
    }

    /// Re-baked masks the layer took.
    #[must_use]
    pub fn masks_replaced(&self) -> u64 {
        self.lock().masks_replaced
    }

    /// Re-baked masks the layer refused (another side or class count than the drawn mask).
    #[must_use]
    pub fn masks_refused(&self) -> u64 {
        self.lock().masks_refused
    }

    /// The last frame's report.
    #[must_use]
    pub fn report(&self) -> GrassReport {
        self.lock().report.clone()
    }

    /// Rewrites the stored report in place. Quality and class name strings are replaced only
    /// when they differ from `quality` and `class_names`. Returns whether those names were
    /// already current. `fill` writes the numeric fields.
    pub(crate) fn update_report(
        &self,
        quality: &str,
        class_names: &[&str],
        fill: impl FnOnce(&mut GrassReport),
    ) -> bool {
        let mut state = self.lock();
        let report = &mut state.report;
        let same_quality = report.quality == quality;
        let same_classes = report.classes.len() == class_names.len()
            && report
                .classes
                .iter()
                .zip(class_names)
                .all(|(have, want)| have == want);
        if !same_quality {
            report.quality.clear();
            report.quality.push_str(quality);
        }
        if !same_classes {
            report.classes.clear();
            report
                .classes
                .extend(class_names.iter().map(|name| (*name).to_owned()));
        }
        let unchanged = same_quality && same_classes;
        fill(report);
        unchanged
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_clamped_and_counted() {
        let handle = TerrainGrassHandle::linked();
        assert!(handle.is_linked());
        assert!(!TerrainGrassHandle::default().is_linked());
        let (_, g0) = handle.request_with_generation();
        handle.set_request(GrassRequest {
            density_scale: 7.0,
            range_scale: f32::NAN,
            sway_scale: -1.0,
            debug_view: 9,
            ..GrassRequest::default()
        });
        let (r, g1) = handle.request_with_generation();
        assert_eq!(g1, g0 + 1);
        assert_eq!(r.density_scale, 1.0);
        assert_eq!(r.range_scale, 1.0);
        assert_eq!(r.sway_scale, 0.0);
        // The last view is the foliage transmission (ADR-3170).
        assert_eq!(r.debug_view, 4);
    }

    #[test]
    fn the_clock_ignores_non_finite_times() {
        let handle = TerrainGrassHandle::linked();
        handle.set_time(2.5);
        handle.set_time(f32::INFINITY);
        assert_eq!(handle.time(), 2.5);
    }

    #[test]
    fn edits_queue_until_taken() {
        let handle = TerrainGrassHandle::linked();
        handle.scale_density(1.0, 2.0, 3.0, 0.0);
        handle.scale_density(f32::NAN, 2.0, 3.0, 0.0);
        handle.scale_density(1.0, 2.0, -3.0, 0.0);
        assert_eq!(handle.take_edits(), vec![[1.0, 2.0, 3.0, 0.0]]);
        assert!(handle.take_edits().is_empty());
    }

    #[test]
    fn gusts_are_sanitized_and_cleared() {
        let handle = TerrainGrassHandle::linked();
        assert_eq!(handle.gust(), None);
        let gust = GrassGust {
            center: [700.0, 5.0],
            radius: 3_000.0,
            prev_radius: 2_994.3,
            bend: 3.0,
            length: 0.1,
        };
        handle.set_gust(Some(gust));
        let got = handle.gust().expect("set");
        assert_eq!(got.bend, 1.0);
        assert_eq!(got.length, 0.5);
        assert_eq!(got.radius, 3_000.0);
        handle.set_gust(Some(GrassGust {
            radius: f32::NAN,
            ..gust
        }));
        assert_eq!(handle.gust(), None, "non-finite clears");
        handle.set_gust(Some(gust));
        handle.set_gust(None);
        assert_eq!(handle.gust(), None);
        assert_eq!(GrassGust { bend: 0.0, ..gust }.sanitized(), None);
    }

    #[test]
    fn requests_serialize_quality_names() {
        let r = GrassRequest {
            quality: Some(GrassQuality::Low),
            ..GrassRequest::default()
        };
        let text = serde_json_like(&r);
        assert!(text.contains("low"), "{text}");
    }

    fn serde_json_like(r: &GrassRequest) -> String {
        ron::to_string(r).unwrap_or_default()
    }
}
