// SPDX-License-Identifier: MPL-2.0
//! Visible-light budget for clustered forward (`specs/13` § 5, `specs/26` § 4).
//!
//! The GPU buffer holds [`MAX_UPLOADED_LIGHTS`]. Each profile uploads at most
//! `RenderProfile::max_visible_lights`. The previous path kept the first
//! [`crate::LAB_MAX_LIGHTS`] (64) and logged nothing when the rest were
//! discarded. A froxel that was already full (`max_lights_per_cluster`) dropped
//! lights the same way. Both caps now warn.
//!
//! DO27's tunnels authored 165 lights and a look pass reported 48 reaching the
//! image. This crate has no constant 48. The hard upload cap was 64. The lab
//! harness also stops a cluster at 32, and Desktop Low / Deck stop at 64, so a
//! view whose lights piled into a few froxels could show fewer than the upload
//! cap. Desktop High now uploads 256 and allows 128 per cluster.
//!
//! ## Ranking
//!
//! Screen importance is the light's luminous intensity (candela) times the
//! square of its projected radius. That radius is `radius / max(depth, near, 0.01)`,
//! clamped to 4, so two lights whose projections both exceed 4 tie and the
//! lower authored index is kept. Olsson, Billeter and
//! Assarsson, "Clustered Deferred and Forward Shading", HPG 2012, assign
//! lights by which froxels they touch; the projected-sphere weight is the
//! usual key for "which light matters on screen" (Persson, "Practical
//! Clustered Shading", 2013). While the authored set fits in the cap, upload
//! order stays the authored order so frames that already fit do not reshuffle
//! shadow tiles. Over the cap, the lowest scores are the ones dropped.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use regia_core::glam::Vec3;

use crate::lab_frame::LabLocalLight;
use crate::view_math::look_at_view;
use crate::{CLUSTER_GRID_X, CLUSTER_GRID_Y, CLUSTER_GRID_Z, cluster_slice_depth};

/// Storage-buffer capacity. Every profile's visible-light cap is at most this.
pub const MAX_UPLOADED_LIGHTS: u32 = 512;

/// One view the budget ranks lights against.
#[derive(Clone, Copy, Debug)]
pub struct LightView {
    /// Camera position, world metres.
    pub eye: [f32; 3],
    /// Look-at point, world metres.
    pub target: [f32; 3],
    /// Vertical field of view, radians.
    pub fov_y: f32,
    /// Width / height.
    pub aspect: f32,
    /// Near plane.
    pub near: f32,
    /// Far plane.
    pub far: f32,
}

impl LightView {
    /// A perspective view. A non-positive near or far is clamped so the cluster
    /// math stays finite.
    #[must_use]
    pub fn new(eye: [f32; 3], target: [f32; 3], fov_y: f32, aspect: f32, near: f32, far: f32) -> Self {
        let near = if near.is_finite() && near > 1.0e-3 {
            near
        } else {
            0.05
        };
        let far = if far.is_finite() && far > near + 1.0e-2 {
            far
        } else {
            near + 50.0
        };
        Self {
            eye,
            target,
            fov_y: if fov_y.is_finite() && fov_y > 0.05 {
                fov_y
            } else {
                1.0
            },
            aspect: if aspect.is_finite() && aspect > 1.0e-3 {
                aspect
            } else {
                1.0
            },
            near,
            far,
        }
    }

    fn forward(self) -> Vec3 {
        let to = Vec3::from_array(self.target) - Vec3::from_array(self.eye);
        if to.length_squared() > 1.0e-8 {
            to.normalize()
        } else {
            Vec3::NEG_Z
        }
    }
}

/// Which authored lights are uploaded this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LightSelection {
    /// Indices into the authored list. Authored order when everything fits.
    pub kept: Vec<usize>,
    /// Authored count.
    pub authored: u32,
    /// Profile cap that was applied.
    pub cap: u32,
    /// `authored - kept.len()`, saturated.
    pub dropped: u32,
}

/// What the last frame did with the light list. Copied out for tests and logs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct LightUploadReport {
    /// Lights in the scene.
    pub authored: u32,
    /// Lights written into the storage buffer.
    pub uploaded: u32,
    /// Lights the profile cap did not upload.
    pub dropped: u32,
    /// The cap that was applied.
    pub cap: u32,
    /// Froxels whose light list filled up and rejected at least one more light.
    pub cluster_overflow: u32,
}

/// Point or spot, as candela, radius and world position.
fn influence(light: &LabLocalLight) -> ([f32; 3], f32, f32) {
    // KHR_lights_punctual: candela = lumens / 4π. Spots are already candela.
    let lumens_to_candela = 1.0 / (4.0 * std::f32::consts::PI);
    match *light {
        LabLocalLight::Point(light) => (
            light.position,
            light.radius.max(0.05),
            (light.intensity.max(0.0) * lumens_to_candela).max(0.0),
        ),
        LabLocalLight::Spot(light) => (light.position, light.range.max(0.05), light.intensity.max(0.0)),
    }
}

/// Projected-area importance. Zero when the light or the view is not finite.
///
/// The projected radius is `radius / max(depth, near, 0.01)`, clamped to 4.
/// The score is candela times that radius squared. Over the upload cap, equal
/// scores keep authored order (`select_visible_lights` sorts by score, then by
/// index). A light outside the view cone is scaled by `1e-4` so an in-view
/// light wins a contested slot, but it is not discarded: under the cap it
/// still uploads.
#[must_use]
pub fn screen_importance(light: &LabLocalLight, view: &LightView) -> f32 {
    let (position, radius, candela) = influence(light);
    if !position.iter().all(|c| c.is_finite()) || !radius.is_finite() || !candela.is_finite() {
        return 0.0;
    }
    let to = Vec3::from_array(position) - Vec3::from_array(view.eye);
    let forward = view.forward();
    let depth = to.dot(forward);
    let depth_for_proj = depth.max(view.near).max(1.0e-2);
    let projected = (radius / depth_for_proj).clamp(0.0, 4.0);
    let mut score = candela * projected * projected;
    // View-space lateral offset, axis by axis: the frustum cross-section is a
    // rectangle (half-height × half-width at the light's depth), not the
    // circle of the longer axis, so a corner of the frame is tested against
    // the true edges. The view frame is the same +Y-up frame `assign` uses.
    let lateral = to - forward * depth;
    let right = forward.cross(Vec3::Y);
    let (right, up) = if right.length_squared() > 1.0e-8 {
        let right = right.normalize();
        (right, right.cross(forward))
    } else {
        (Vec3::X, Vec3::X.cross(forward))
    };
    let half_y = (view.fov_y * 0.5).tan().max(0.05);
    let behind = depth < -radius;
    let beyond = depth > view.far + radius;
    let off_x = lateral.dot(right).abs() > depth_for_proj * half_y * view.aspect + radius;
    let off_y = lateral.dot(up).abs() > depth_for_proj * half_y + radius;
    let outside = behind || beyond || off_x || off_y;
    if outside {
        score *= 1.0e-4;
    }
    if score.is_finite() { score } else { 0.0 }
}

/// Lights to upload, best-first when the cap drops some.
///
/// Logs a warning the first time a given `(authored, uploaded)` pair is dropped, de-duplicated
/// process-wide; a renderer uses [`LightDropWarnings::select_visible_lights`] to de-duplicate
/// its own.
#[must_use]
pub fn select_visible_lights(lights: &[LabLocalLight], view: &LightView, cap: u32) -> LightSelection {
    PROCESS_WARNINGS.select_visible_lights(lights, view, cap)
}

/// [`select_visible_lights`] without the warning.
fn select_visible(lights: &[LabLocalLight], view: &LightView, cap: u32) -> LightSelection {
    let cap = cap.min(MAX_UPLOADED_LIGHTS);
    let authored = lights.len() as u32;
    if lights.len() <= cap as usize {
        return LightSelection {
            kept: (0..lights.len()).collect(),
            authored,
            cap,
            dropped: 0,
        };
    }
    let mut order: Vec<usize> = (0..lights.len()).collect();
    // Each light scored once, not once per comparison (ADR-385); same order.
    let scores: Vec<f32> = lights
        .iter()
        .map(|light| screen_importance(light, view))
        .collect();
    order.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    order.truncate(cap as usize);
    let dropped = authored.saturating_sub(order.len() as u32);
    LightSelection {
        kept: order,
        authored,
        cap,
        dropped,
    }
}

/// Froxel assignment mirroring `cluster_build.wgsl`.
///
/// `light_stored[i]` is true when light `i` landed in at least one cluster.
/// `overflow_clusters` counts froxels that rejected a light because
/// `max_per` was already full. The view looks along world +Y up, matching an
/// ordinary camera. A cubemap face whose up is not +Y is not what this check
/// mirrors; the GPU pass uses that face's own view.
///
/// A spot that survives the influence sphere is tested again against its
/// cone (`cone_may_hit_aabb`). A froxel beside the cone is not in the list.
///
/// Warns like [`select_visible_lights`] when a full froxel drops a light (process-wide
/// de-duplication; see [`LightDropWarnings::assign_clusters`]).
#[must_use]
pub fn assign_clusters(lights: &[LabLocalLight], view: &LightView, max_per: u32) -> (Vec<bool>, u32) {
    PROCESS_WARNINGS.assign_clusters(lights, view, max_per)
}

/// A light in view space for the froxel test: centre, influence radius, and a spot's
/// `(axis, cos outer)`.
type ViewLight = (Vec3, f32, Option<(Vec3, f32)>);

/// Froxels in one depth slice (`cluster_bounds` order: x, then y, then z).
const CELLS_PER_SLICE: usize = (CLUSTER_GRID_X * CLUSTER_GRID_Y) as usize;

/// [`assign_clusters`] without the warning. Test builds only: a release build scans through
/// [`assign_with_bounds`] so a repeated projection keeps its boxes.
///
/// Each light is moved into view space once, not once per froxel, and each depth slice first
/// keeps the lights whose sphere reaches its depth slab (ADR-385). Every froxel of a slice spans
/// the same view depth (`cell_aabb`), so a light that misses the slab misses each of its boxes:
/// the result is the per-froxel scan's, in the same light order.
#[cfg(test)]
fn assign(lights: &[LabLocalLight], view: &LightView, max_per: u32) -> (Vec<bool>, u32) {
    let bounds = cluster_bounds(view);
    assign_with_bounds(lights, view, max_per, &bounds)
}

/// `assign` against boxes the caller already built. `bounds` is [`cluster_bounds`] order.
fn assign_with_bounds(
    lights: &[LabLocalLight],
    view: &LightView,
    max_per: u32,
    bounds: &[(Vec3, Vec3)],
) -> (Vec<bool>, u32) {
    let max_per = max_per.max(1);
    let view_mat = look_at_view(Vec3::from_array(view.eye), Vec3::from_array(view.target), Vec3::Y);
    let placed: Vec<ViewLight> = lights
        .iter()
        .map(|light| {
            let (position, radius, _) = influence(light);
            let lp = view_mat.transform_point3(Vec3::from_array(position));
            let cone = match light {
                LabLocalLight::Spot(spot) => Some((
                    view_mat.transform_vector3(Vec3::from_array(spot.direction)),
                    spot.outer_angle.cos(),
                )),
                LabLocalLight::Point(_) => None,
            };
            (lp, radius, cone)
        })
        .collect();
    let mut stored = vec![false; lights.len()];
    let mut overflow_clusters = 0u32;
    let mut slab: Vec<usize> = Vec::with_capacity(lights.len());
    for slice in bounds.as_chunks::<CELLS_PER_SLICE>().0 {
        let (slice_lo, slice_hi) = slice[0];
        slab.clear();
        slab.extend(
            placed
                .iter()
                .enumerate()
                .filter(|(_, (lp, radius, _))| slab_hits(lp.z, *radius, slice_lo.z, slice_hi.z))
                .map(|(index, _)| index),
        );
        for (lo, hi) in slice {
            let mut written = 0u32;
            let mut overflowed = false;
            for &index in &slab {
                let (lp, radius, cone) = placed[index];
                if !sphere_hits_aabb(lp, radius, *lo, *hi) {
                    continue;
                }
                if let Some((dir_v, cos_outer)) = cone
                    && !cone_may_hit_aabb(lp, dir_v, radius, cos_outer, *lo, *hi)
                {
                    continue;
                }
                if written < max_per {
                    stored[index] = true;
                    written += 1;
                } else {
                    overflowed = true;
                }
            }
            if overflowed {
                overflow_clusters += 1;
            }
        }
    }
    (stored, overflow_clusters)
}

/// `false` only when [`sphere_hits_aabb`] is `false` for every box spanning view depths
/// `lo..=hi`: the depth gap alone is wider than the radius. The same clamp and length as that
/// test with the other two axes at zero; adding their non-negative squares can only lengthen
/// the distance, so the reject is exact.
fn slab_hits(z: f32, radius: f32, lo: f32, hi: f32) -> bool {
    let c = z.max(lo).min(hi);
    Vec3::new(0.0, 0.0, c - z).length() <= radius
}

/// Froxel-box inputs. Equal floats compare equal, including −0 and the NaN patterns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ProjKey {
    fov_y: u32,
    aspect: u32,
    near: u32,
    far: u32,
}

/// One light as `assign` reads it. Colour, intensity, shadows and the inner angle are absent:
/// they do not change the mask or the overflow.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LightScanKey {
    position: [u32; 3],
    /// Point radius or spot range, after the 0.05 m floor [`influence`] applies.
    radius: u32,
    /// Spot axis bits and `cos(outer_angle)` bits. Absent for a point.
    spot: Option<([u32; 3], u32)>,
}

/// The last grid and the last scan of one [`LightDropWarnings`].
///
/// The boxes depend on the projection only. The scan does not: each light is transformed by the
/// eye, so a camera move can change which froxels overflow (ADR-5206).
#[derive(Debug)]
struct ClusterCache {
    proj: ProjKey,
    has_grid: bool,
    bounds: Vec<(Vec3, Vec3)>,
    eye: [u32; 3],
    target: [u32; 3],
    max_per: u32,
    lights: Vec<LightScanKey>,
    stored: Vec<bool>,
    overflow: u32,
    has_scan: bool,
}

impl ClusterCache {
    const EMPTY: Self = Self {
        proj: ProjKey {
            fov_y: 0,
            aspect: 0,
            near: 0,
            far: 0,
        },
        has_grid: false,
        bounds: Vec::new(),
        eye: [0; 3],
        target: [0; 3],
        max_per: 0,
        lights: Vec::new(),
        stored: Vec::new(),
        overflow: 0,
        has_scan: false,
    };
}

fn proj_key(view: &LightView) -> ProjKey {
    ProjKey {
        fov_y: view.fov_y.to_bits(),
        aspect: view.aspect.to_bits(),
        near: view.near.to_bits(),
        far: view.far.to_bits(),
    }
}

fn light_scan_key(light: &LabLocalLight) -> LightScanKey {
    match *light {
        LabLocalLight::Point(light) => LightScanKey {
            position: light.position.map(f32::to_bits),
            radius: light.radius.max(0.05).to_bits(),
            spot: None,
        },
        LabLocalLight::Spot(light) => LightScanKey {
            position: light.position.map(f32::to_bits),
            radius: light.range.max(0.05).to_bits(),
            spot: Some((
                light.direction.map(f32::to_bits),
                light.outer_angle.cos().to_bits(),
            )),
        },
    }
}

fn scan_matches(cache: &ClusterCache, view: &LightView, max_per: u32, lights: &[LabLocalLight]) -> bool {
    cache.has_scan
        && cache.max_per == max_per
        && cache.eye == view.eye.map(f32::to_bits)
        && cache.target == view.target.map(f32::to_bits)
        && cache.lights.len() == lights.len()
        && cache
            .lights
            .iter()
            .zip(lights)
            .all(|(key, light)| *key == light_scan_key(light))
}

/// The once-per-change warning state of the light caps: each cap warns when the
/// `(authored, kept)` pair it drops differs from the last pair it warned about.
///
/// Each [`crate::FrameRenderer`] owns one, so two renderers with different light sets (a main
/// view and a capture, or tests running in parallel) neither repeat each other's warnings every
/// frame nor swallow them. The free functions ([`select_visible_lights`], [`assign_clusters`])
/// share one process-wide instance.
#[derive(Debug)]
pub struct LightDropWarnings {
    visible: AtomicU64,
    cluster: AtomicU64,
    shadow: AtomicU64,
    /// Froxel cells walked by [`Self::assign_clusters`] scans (see [`Self::froxel_visits`]).
    froxel_visits: AtomicU64,
    /// Times [`Self::assign_clusters`] built the froxel grid. Test builds only.
    #[cfg(test)]
    grid_builds: AtomicU64,
    /// Times [`Self::assign_clusters`] scanned lights into that grid. Test builds only.
    #[cfg(test)]
    scans: AtomicU64,
    /// Last froxel grid and last scan. The grid is reused when the projection matches; the scan
    /// is reused when the eye, the target, the cap and the lights that affect it match.
    cache: Mutex<ClusterCache>,
}

impl Default for LightDropWarnings {
    fn default() -> Self {
        Self::new()
    }
}

impl LightDropWarnings {
    /// Nothing warned yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            visible: AtomicU64::new(u64::MAX),
            cluster: AtomicU64::new(u64::MAX),
            shadow: AtomicU64::new(u64::MAX),
            froxel_visits: AtomicU64::new(0),
            #[cfg(test)]
            grid_builds: AtomicU64::new(0),
            #[cfg(test)]
            scans: AtomicU64::new(0),
            cache: Mutex::new(ClusterCache::EMPTY),
        }
    }

    /// Froxel cells walked by [`Self::assign_clusters`] since this value was created.
    ///
    /// One cell of the cluster grid is one visit. A second call with the same light
    /// geometry, the same view and the same per-cluster cap reuses the stored mask
    /// and does not add to this count.
    #[must_use]
    pub fn froxel_visits(&self) -> u64 {
        self.froxel_visits.load(Ordering::Relaxed)
    }

    /// [`select_visible_lights`], warning through this instance.
    #[tracing::instrument(level = "trace", skip_all)]
    #[must_use]
    pub fn select_visible_lights(
        &self,
        lights: &[LabLocalLight],
        view: &LightView,
        cap: u32,
    ) -> LightSelection {
        let selection = select_visible(lights, view, cap);
        if selection.dropped > 0 {
            warn_drop(
                "visible light",
                selection.authored,
                selection.kept.len() as u32,
                selection.cap,
                &self.visible,
            );
        }
        selection
    }

    /// [`assign_clusters`], warning through this instance.
    ///
    /// A repeated view reuses the froxel boxes when `fov_y`, `aspect`, `near` and `far` are
    /// unchanged, and reuses the mask when the eye, the target, `max_per` and the lights that
    /// the scan reads are unchanged. A moved eye scans again: the overflow is not a function of
    /// the projection alone.
    #[tracing::instrument(level = "info", skip_all)]
    #[must_use]
    pub fn assign_clusters(
        &self,
        lights: &[LabLocalLight],
        view: &LightView,
        max_per: u32,
    ) -> (Vec<bool>, u32) {
        let max_per = max_per.max(1);
        let mut built_grid = false;
        let mut scanned = false;
        let (stored, overflow) = {
            let mut cache = self.cache.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let proj = proj_key(view);
            if !cache.has_grid || cache.proj != proj {
                cache.bounds = cluster_bounds(view);
                cache.proj = proj;
                cache.has_grid = true;
                cache.has_scan = false;
                built_grid = true;
            }
            if scan_matches(&cache, view, max_per, lights) {
                (cache.stored.clone(), cache.overflow)
            } else {
                scanned = true;
                let (stored, overflow) = assign_with_bounds(lights, view, max_per, &cache.bounds);
                let walked = cache.bounds.as_chunks::<CELLS_PER_SLICE>().0.len() * CELLS_PER_SLICE;
                self.froxel_visits.fetch_add(walked as u64, Ordering::Relaxed);
                cache.eye = view.eye.map(f32::to_bits);
                cache.target = view.target.map(f32::to_bits);
                cache.max_per = max_per;
                cache.lights.clear();
                cache.lights.extend(lights.iter().map(light_scan_key));
                cache.stored.clone_from(&stored);
                cache.overflow = overflow;
                cache.has_scan = true;
                (stored, overflow)
            }
        };
        #[cfg(test)]
        {
            if built_grid {
                self.grid_builds.fetch_add(1, Ordering::Relaxed);
            }
            if scanned {
                self.scans.fetch_add(1, Ordering::Relaxed);
            }
        }
        #[cfg(not(test))]
        {
            let _ = (built_grid, scanned);
        }
        self.warn_cluster(lights, &stored, overflow, max_per);
        (stored, overflow)
    }

    fn warn_cluster(&self, lights: &[LabLocalLight], stored: &[bool], overflow: u32, max_per: u32) {
        if overflow == 0 {
            return;
        }
        let missed = stored.iter().filter(|hit| !**hit).count() as u32;
        warn_drop(
            "cluster",
            lights.len() as u32,
            lights.len() as u32 - missed,
            max_per,
            &self.cluster,
        );
    }

    /// Froxel grids this instance has built.
    #[cfg(test)]
    fn grid_build_count(&self) -> u64 {
        self.grid_builds.load(Ordering::Relaxed)
    }

    /// Full cluster scans this instance has run.
    #[cfg(test)]
    fn scan_count(&self) -> u64 {
        self.scans.load(Ordering::Relaxed)
    }

    /// Warns when a shadow-atlas cap drops casters (same de-dupe as the upload cap).
    pub fn warn_shadow_cap(&self, authored_casters: u32, shadowed: u32, cap: u32) {
        warn_drop(
            "shadowed local light",
            authored_casters,
            shadowed,
            cap,
            &self.shadow,
        );
    }
}

/// Atlas tiles one caster needs. A point is six cube faces; a spot is one.
#[must_use]
pub fn shadow_view_count(light: &LabLocalLight) -> usize {
    match light {
        LabLocalLight::Point(_) => 6,
        LabLocalLight::Spot(_) => 1,
    }
}

fn casts_shadow(light: &LabLocalLight) -> bool {
    match *light {
        LabLocalLight::Point(light) => light.cast_shadows,
        LabLocalLight::Spot(light) => light.cast_shadows,
    }
}

/// Casters to try for atlas tiles.
///
/// While every caster fits in the light cap and the tile cap, [`Self::indices`]
/// stays in authored order. That keeps an existing atlas slot (the m15b near
/// point is tile 0) from swapping when nothing was dropped. Over either cap
/// the order is screen importance, highest first, so a near spot beats a far
/// point.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShadowCandidateOrder {
    /// Indices into the light list. Authored order when [`Self::all_fit`].
    pub indices: Vec<usize>,
    /// Every caster fits both caps.
    pub all_fit: bool,
}

/// Order in which shadowed lights should be given atlas tiles.
#[must_use]
pub fn order_shadow_casters(
    lights: &[LabLocalLight],
    view: &LightView,
    max_lights: u32,
    max_tiles: usize,
) -> ShadowCandidateOrder {
    let indices: Vec<usize> = lights
        .iter()
        .enumerate()
        .filter(|(_, light)| casts_shadow(light))
        .map(|(index, _)| index)
        .collect();
    if shadow_casters_fit(lights, &indices, max_lights, max_tiles) {
        return ShadowCandidateOrder {
            indices,
            all_fit: true,
        };
    }
    let mut ranked = indices;
    // Each light scored once, not once per comparison (ADR-385); same order.
    let scores: Vec<f32> = lights
        .iter()
        .map(|light| screen_importance(light, view))
        .collect();
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    ShadowCandidateOrder {
        indices: ranked,
        all_fit: false,
    }
}

fn shadow_casters_fit(
    lights: &[LabLocalLight],
    indices: &[usize],
    max_lights: u32,
    max_tiles: usize,
) -> bool {
    let mut tiles = 0usize;
    for (assigned, &index) in indices.iter().enumerate() {
        if assigned as u32 >= max_lights {
            return false;
        }
        let Some(light) = lights.get(index) else {
            return false;
        };
        let views = shadow_view_count(light);
        if tiles.saturating_add(views) > max_tiles {
            return false;
        }
        tiles += views;
    }
    true
}

/// The warning state of the free functions.
static PROCESS_WARNINGS: LightDropWarnings = LightDropWarnings::new();

fn warn_drop(kind: &str, authored: u32, kept: u32, cap: u32, slot: &AtomicU64) {
    if authored <= kept {
        return;
    }
    let key = (u64::from(authored) << 32) | u64::from(kept);
    if slot.swap(key, Ordering::Relaxed) == key {
        return;
    }
    tracing::warn!(
        target: "regia_render",
        kind,
        authored,
        kept,
        dropped = authored - kept,
        cap,
        "profile cap dropped lights"
    );
}

fn cluster_bounds(view: &LightView) -> Vec<(Vec3, Vec3)> {
    let count = (CLUSTER_GRID_X * CLUSTER_GRID_Y * CLUSTER_GRID_Z) as usize;
    let mut bounds = Vec::with_capacity(count);
    // Same linear order as `cluster_index_3d`: x, then y, then z.
    for z in 0..CLUSTER_GRID_Z {
        for y in 0..CLUSTER_GRID_Y {
            for x in 0..CLUSTER_GRID_X {
                bounds.push(cell_aabb(x, y, z, view));
            }
        }
    }
    bounds
}

fn cell_aabb(cell_x: u32, cell_y: u32, cell_z: u32, view: &LightView) -> (Vec3, Vec3) {
    let x0 = cell_x as f32 / CLUSTER_GRID_X as f32;
    let x1 = (cell_x + 1) as f32 / CLUSTER_GRID_X as f32;
    let y0 = cell_y as f32 / CLUSTER_GRID_Y as f32;
    let y1 = (cell_y + 1) as f32 / CLUSTER_GRID_Y as f32;
    let z0 = cluster_slice_depth(cell_z, view.near, view.far);
    let z1 = cluster_slice_depth(cell_z + 1, view.near, view.far);
    let mut lo = Vec3::splat(1.0e30);
    let mut hi = Vec3::splat(-1.0e30);
    for iz in 0..2 {
        let z = if iz == 0 { z0 } else { z1 };
        for iy in 0..2 {
            let uy = if iy == 0 { y0 } else { y1 };
            let ndc_y = 1.0 - uy * 2.0;
            for ix in 0..2 {
                let ux = if ix == 0 { x0 } else { x1 };
                let ndc_x = ux * 2.0 - 1.0;
                let p = view_corner(ndc_x, ndc_y, z, view.fov_y, view.aspect);
                lo = lo.min(p);
                hi = hi.max(p);
            }
        }
    }
    (lo, hi)
}

fn view_corner(ndc_x: f32, ndc_y: f32, depth: f32, fov_y: f32, aspect: f32) -> Vec3 {
    let tan_y = (fov_y * 0.5).tan();
    Vec3::new(ndc_x * aspect * tan_y * depth, ndc_y * tan_y * depth, -depth)
}

fn sphere_hits_aabb(center: Vec3, radius: f32, lo: Vec3, hi: Vec3) -> bool {
    let c = center.clamp(lo, hi);
    c.distance(center) <= radius
}

/// Spot cone against one froxel AABB. Mirrors `cone_may_hit_aabb` in
/// `cluster_build.wgsl`. A reject is disjoint. An accept may still miss: the
/// box's bounding sphere is tested at the widest slice the box reaches.
///
/// `dir` is the cone axis in the same space as `apex` and the box (view space
/// on the assignment path). `cos_outer` is `cos` of the outer half-angle, the
/// value packed in `GpuLight::cone_shadow.y`. Wider than about 87°
/// (`cos_outer <= 0.05`) keeps the sphere result; `tan` of that half-angle
/// is unstable. A zero-length axis does the same.
///
/// Persson, "Practical Clustered Shading", SIGGRAPH 2013, culls a spot by its
/// cone rather than the influence sphere. The support test (the box is
/// entirely behind the apex) and the sphere-at-the-far-slice bound are the
/// conservative cone/AABB rejection in Ericson, *Real-Time Collision
/// Detection* (2005), and Eberly, "Intersection of a Cone and an AABB"
/// (Geometric Tools). This is the cheap reject, not Eberly's exact test.
fn cone_may_hit_aabb(apex: Vec3, dir_in: Vec3, range: f32, cos_outer: f32, lo: Vec3, hi: Vec3) -> bool {
    if !sphere_hits_aabb(apex, range, lo, hi) {
        return false;
    }
    if !cos_outer.is_finite() || cos_outer <= 0.05 {
        return true;
    }
    let dir_len = dir_in.length();
    if dir_len <= 1.0e-4 {
        return true;
    }
    let dir = dir_in / dir_len;
    let center = (lo + hi) * 0.5;
    let ext = (hi - lo) * 0.5;
    let rel = center - apex;
    let along = rel.dot(dir);
    let along_radius = ext.abs().dot(dir.abs());
    if along + along_radius < 0.0 {
        return false;
    }
    let cos_safe = cos_outer.max(1.0e-3);
    let sin2 = (1.0 - cos_safe * cos_safe).max(0.0);
    // `sqrt(sin2) <= (sin2 + 1) / 2`, and the L1 extent contains the corner
    // radius. The widened cone contains the true one, so a reject is disjoint.
    // The cluster shader uses this form because a `sqrt` there miscompiled on
    // lavapipe and dropped every spot cluster. See `cluster_build.wgsl`.
    let tan_upper = ((sin2 + 1.0) * 0.5) / cos_safe;
    let reach = (along + along_radius).clamp(0.0, range);
    let perp2 = (rel.length_squared() - along * along).max(0.0);
    let half_l1 = ext.x.abs() + ext.y.abs() + ext.z.abs();
    let limit = reach * tan_upper + half_l1;
    perp2 <= limit * limit
}

/// Point lights along +X at y = 1.2, one every `spacing` metres.
///
/// Used by the 200-light test and `render_lab many_lights`. Intensity is 600 lm
/// and the radius is `spacing`, so neighbouring lights meet but a Desktop High
/// cluster (128) still has room when the camera looks along the row.
#[must_use]
pub fn corridor_point_lights(count: u32, spacing: f32) -> Vec<LabLocalLight> {
    let spacing = if spacing.is_finite() && spacing > 0.05 {
        spacing
    } else {
        2.0
    };
    (0..count)
        .map(|index| {
            LabLocalLight::Point(crate::LabPointLight {
                position: [index as f32 * spacing, 1.2, 0.0],
                color: [1.0, 0.92, 0.8],
                intensity: 600.0,
                radius: spacing,
                cast_shadows: false,
                source_radius: None,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame_work::counted::*;
    use crate::lab_frame::{LabPointLight, LabSpotLight};
    use crate::{FrameQuality, RenderProfile, RenderProfilePreset};

    /// The froxel scan before ADR-385: every light transformed and tested in every froxel.
    fn assign_reference(lights: &[LabLocalLight], view: &LightView, max_per: u32) -> (Vec<bool>, u32) {
        let max_per = max_per.max(1);
        let bounds = cluster_bounds(view);
        let view_mat = look_at_view(Vec3::from_array(view.eye), Vec3::from_array(view.target), Vec3::Y);
        let mut stored = vec![false; lights.len()];
        let mut overflow_clusters = 0u32;
        for (lo, hi) in &bounds {
            let mut written = 0u32;
            let mut overflowed = false;
            for (index, light) in lights.iter().enumerate() {
                let (position, radius, _) = influence(light);
                let lp = view_mat.transform_point3(Vec3::from_array(position));
                if !sphere_hits_aabb(lp, radius, *lo, *hi) {
                    continue;
                }
                if let LabLocalLight::Spot(spot) = light {
                    let dir_v = view_mat.transform_vector3(Vec3::from_array(spot.direction));
                    if !cone_may_hit_aabb(lp, dir_v, radius, spot.outer_angle.cos(), *lo, *hi) {
                        continue;
                    }
                }
                if written < max_per {
                    stored[index] = true;
                    written += 1;
                } else {
                    overflowed = true;
                }
            }
            if overflowed {
                overflow_clusters += 1;
            }
        }
        (stored, overflow_clusters)
    }

    /// A deterministic scatter of points and spots around the origin (a small LCG).
    fn scattered_lights(count: usize, seed: u32) -> Vec<LabLocalLight> {
        let mut state = seed;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (state >> 8) as f32 / (1u32 << 24) as f32
        };
        (0..count)
            .map(|i| {
                let position = [next() * 120.0 - 60.0, next() * 12.0, next() * 120.0 - 60.0];
                if i.is_multiple_of(3) {
                    LabLocalLight::Spot(LabSpotLight {
                        position,
                        direction: [next() - 0.5, -next(), next() - 0.5],
                        color: [1.0; 3],
                        intensity: 400.0,
                        range: 1.0 + next() * 20.0,
                        inner_angle: 0.2,
                        outer_angle: 0.1 + next() * 1.4,
                        cast_shadows: false,
                        source_radius: None,
                    })
                } else {
                    LabLocalLight::Point(LabPointLight {
                        position,
                        color: [1.0; 3],
                        intensity: 800.0,
                        radius: 0.5 + next() * 30.0,
                        cast_shadows: false,
                        source_radius: None,
                    })
                }
            })
            .collect()
    }

    #[test]
    fn slab_filtered_assignment_matches_the_per_froxel_scan() {
        let views = [
            LightView::new([0.0, 1.7, 0.0], [10.0, 1.5, 3.0], 1.0, 16.0 / 9.0, 0.1, 300.0),
            LightView::new([-40.0, 30.0, -40.0], [0.0, 0.0, 0.0], 0.6, 2.4, 0.5, 2000.0),
            LightView::new([5.0, 2.0, 50.0], [5.0, 2.5, -50.0], 1.6, 1.0, 0.05, 80.0),
        ];
        for (seed, count) in [(1u32, 40usize), (7, 300), (99, 512)] {
            let lights = scattered_lights(count, seed);
            for view in &views {
                for max_per in [1, 8, 64, 256] {
                    assert_eq!(
                        assign(&lights, view, max_per),
                        assign_reference(&lights, view, max_per),
                        "seed {seed}, {count} lights, max_per {max_per}"
                    );
                }
            }
        }
        // The corridor view of the 200-light test as well.
        let lights = corridor_point_lights(200, 2.0);
        let view = view_at_corridor();
        for max_per in [4, 128] {
            assert_eq!(
                assign(&lights, &view, max_per),
                assign_reference(&lights, &view, max_per)
            );
        }
    }

    #[test]
    fn froxels_of_one_slice_share_their_depth_extent() {
        // `assign` filters a whole slice by its first froxel's depth: that is only exact while
        // every froxel of the slice spans the same view depth.
        let view = LightView::new([0.0, 1.7, 0.0], [10.0, 1.5, 3.0], 1.2, 1.7, 0.1, 900.0);
        let bounds = cluster_bounds(&view);
        let (slices, rest) = bounds.as_chunks::<CELLS_PER_SLICE>();
        assert!(rest.is_empty(), "the grid is whole slices");
        assert_eq!(slices.len(), CLUSTER_GRID_Z as usize);
        for slice in slices {
            let (lo0, hi0) = slice[0];
            for (lo, hi) in slice {
                assert_eq!(lo.z.to_bits(), lo0.z.to_bits());
                assert_eq!(hi.z.to_bits(), hi0.z.to_bits());
            }
        }
    }

    fn view_at_corridor() -> LightView {
        // Looking down the row so every sphere crosses the centre froxels.
        // A side view leaves the ends outside the frustum, which is not a drop.
        LightView::new([-10.0, 1.6, 0.0], [400.0, 1.2, 0.0], 1.0, 16.0 / 9.0, 0.2, 500.0)
    }

    #[test]
    fn desktop_high_keeps_two_hundred_lights_and_every_one_is_clustered() {
        let lights = corridor_point_lights(200, 2.0);
        let view = view_at_corridor();
        let high = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        assert!(high.max_visible_lights >= 256);
        let selection = select_visible_lights(&lights, &view, high.max_visible_lights);
        assert_eq!(selection.dropped, 0);
        assert_eq!(selection.kept.len(), 200);
        let quality = FrameQuality::from_profile(&high);
        let (stored, overflow) = assign_clusters(&lights, &view, quality.max_lights_per_cluster);
        let missed = stored.iter().filter(|hit| !**hit).count();
        assert_eq!(missed, 0, "lights outside every froxel: {missed}");
        assert_eq!(
            overflow, 0,
            "Desktop High must not overflow a spread of 200 lights"
        );
    }

    /// 165 tight corridor lights overflow a small per-cluster cap. The second call of the
    /// same set must not walk the grid again; moving one light must.
    #[test]
    fn unchanged_light_set_does_not_rescan_clusters() {
        let warnings = LightDropWarnings::new();
        let mut lights = corridor_point_lights(165, 0.15);
        let view = LightView::new([-2.0, 1.6, 0.0], [8.0, 1.2, 0.0], 1.0, 16.0 / 9.0, 0.2, 40.0);
        let max_per = 8u32;
        let grid = (CLUSTER_GRID_X * CLUSTER_GRID_Y * CLUSTER_GRID_Z) as u64;
        let (stored, overflow) = warnings.assign_clusters(&lights, &view, max_per);
        let oracle = assign(&lights, &view, max_per);
        assert_eq!((stored, overflow), oracle);
        assert!(overflow > 0, "the fixture must overflow or the cache is untested");
        let visits = warnings.froxel_visits();
        assert!(
            visits >= grid,
            "one scan walks the {grid}-cell grid, counted {visits}"
        );
        let (stored2, overflow2) = warnings.assign_clusters(&lights, &view, max_per);
        let after = warnings.froxel_visits();
        assert_eq!(
            after,
            visits,
            "a second assign of the same lights rescanned {} froxels (first scan {visits})",
            after - visits
        );
        assert_eq!((stored2, overflow2), oracle);
        if let LabLocalLight::Point(point) = &mut lights[0] {
            point.color[0] = 0.2;
        }
        let (stored_color, overflow_color) = warnings.assign_clusters(&lights, &view, max_per);
        assert_eq!(
            warnings.froxel_visits(),
            visits,
            "colour does not change froxel assignment"
        );
        assert_eq!((stored_color, overflow_color), oracle);
        if let LabLocalLight::Point(point) = &mut lights[0] {
            point.position[0] += 50.0;
        }
        let before_move = warnings.froxel_visits();
        let (stored3, overflow3) = warnings.assign_clusters(&lights, &view, max_per);
        assert!(
            warnings.froxel_visits() > before_move,
            "moving a light must rescan"
        );
        assert_eq!((stored3, overflow3), assign(&lights, &view, max_per));
    }

    /// Collects the `regia_render` warnings logged while it is the default subscriber.
    #[derive(Clone, Default)]
    struct WarnLog {
        events: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl tracing::Subscriber for WarnLog {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            *metadata.level() == tracing::Level::WARN
        }
        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }
        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}
        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}
        fn event(&self, event: &tracing::Event<'_>) {
            if *event.metadata().level() != tracing::Level::WARN
                || event.metadata().target() != "regia_render"
            {
                return;
            }
            let mut text = String::new();
            struct Dump<'a>(&'a mut String);
            impl tracing::field::Visit for Dump<'_> {
                fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                    self.0.push_str(field.name());
                    self.0.push('=');
                    self.0.push_str(&format!("{value:?}"));
                    self.0.push(';');
                }
                fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
                    self.0.push_str(field.name());
                    self.0.push('=');
                    self.0.push_str(value);
                    self.0.push(';');
                }
                fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
                    self.0.push_str(field.name());
                    self.0.push('=');
                    self.0.push_str(&value.to_string());
                    self.0.push(';');
                }
                fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
                    self.record_u64(field, value as u64);
                }
            }
            event.record(&mut Dump(&mut text));
            if let Ok(mut events) = self.events.lock() {
                events.push(text);
            }
        }
        fn enter(&self, _span: &tracing::span::Id) {}
        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// Runs `f` with `log` as this thread's subscriber. The warning's callsite interest is
    /// rebuilt first: tracing caches one interest per callsite for the process, and while a
    /// single scoped subscriber exists it computes it from the subscriber of whichever thread
    /// hits the callsite first. A parallel test (or the thread below) that warned first with no
    /// subscriber leaves the callsite at "never", and this thread's subscriber would see nothing.
    fn capture<T>(log: WarnLog, f: impl FnOnce() -> T) -> T {
        tracing::subscriber::with_default(log, || {
            tracing::callsite::rebuild_interest_cache();
            f()
        })
    }

    /// Sets the flag when dropped (also when the test body panics, so the thread it stops ends
    /// and the scope can return the panic).
    struct SetOnDrop<'a>(&'a std::sync::atomic::AtomicBool);

    impl Drop for SetOnDrop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }

    #[test]
    fn a_cap_below_the_authored_set_drops_the_dimmer_lights_and_warns() {
        let mut lights = corridor_point_lights(20, 2.0);
        if let LabLocalLight::Point(point) = &mut lights[3] {
            point.intensity = 80_000.0;
        }
        if let LabLocalLight::Point(point) = &mut lights[17] {
            point.intensity = 1.0;
        }
        let view = view_at_corridor();
        // This test's own de-dup state, as a renderer owns one. Other tests (and other
        // renderers) drop other light sets at the same time: a thread below keeps doing that
        // through the process-wide slot and a second instance for the whole test, and must
        // neither steal this instance's one-shot warning nor make it repeat (the shared slot
        // did both under a parallel test run, stability-audit hole 17).
        let warnings = LightDropWarnings::new();
        // Register the warning's callsite on this thread, with a subscriber, before the other
        // thread can be the first to hit it (see `capture`).
        let _ = capture(WarnLog::default(), || {
            LightDropWarnings::new().select_visible_lights(&lights, &view, 8)
        });
        let stop = std::sync::atomic::AtomicBool::new(false);
        let thrashed = std::sync::atomic::AtomicU64::new(0);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                let other = LightDropWarnings::new();
                let mut cap = 1;
                while !stop.load(Ordering::Relaxed) || thrashed.load(Ordering::Relaxed) < 100 {
                    cap = cap % 7 + 1;
                    let _ = select_visible_lights(&lights, &view, cap);
                    let _ = other.select_visible_lights(&lights, &view, cap);
                    thrashed.fetch_add(1, Ordering::Relaxed);
                    std::thread::yield_now();
                }
            });
            let _stop = SetOnDrop(&stop);
            while thrashed.load(Ordering::Relaxed) < 10 {
                std::thread::yield_now();
            }
            check_warns_once(&warnings, &lights, &view, &thrashed);
        });
    }

    /// Selects 8 of `lights` through `warnings` twice: the first warns, the second does not,
    /// while `thrashed` shows another thread dropping other light sets in between.
    fn check_warns_once(
        warnings: &LightDropWarnings,
        lights: &[LabLocalLight],
        view: &LightView,
        thrashed: &std::sync::atomic::AtomicU64,
    ) {
        use std::sync::Arc;

        let log = WarnLog::default();
        let events = Arc::clone(&log.events);
        let selection = capture(log, || warnings.select_visible_lights(lights, view, 8));
        assert_eq!(selection.kept.len(), 8);
        assert_eq!(selection.dropped, 12);
        assert!(
            selection.kept.contains(&3),
            "the bright light must survive the cap, kept {:?}",
            selection.kept
        );
        assert!(
            !selection.kept.contains(&17),
            "the dim light is the one to drop, kept {:?}",
            selection.kept
        );
        let warned = events.lock().map(|events| events.clone()).unwrap_or_default();
        assert!(
            warned.iter().any(|event| {
                event.contains("profile cap dropped lights")
                    && event.contains("visible light")
                    && event.contains("dropped=12")
            }),
            "the cap must log a warning a subscriber can see, got {warned:?}"
        );
        let before = warned.len();
        // Let the other thread drop other sets in between.
        let seen = thrashed.load(Ordering::Relaxed);
        while thrashed.load(Ordering::Relaxed) < seen + 20 {
            std::thread::yield_now();
        }
        // Same pair must not warn a second time (the slot already holds this key).
        let log = WarnLog {
            events: Arc::clone(&events),
        };
        let again = capture(log, || warnings.select_visible_lights(lights, view, 8));
        assert_eq!(again.dropped, 12);
        let after = events.lock().map(|events| events.len()).unwrap_or(before);
        assert_eq!(after, before, "the same drop must not warn twice");
    }

    #[test]
    fn deck_visible_cap_is_below_desktop_high() {
        let deck = RenderProfile::from_preset(RenderProfilePreset::Deck);
        let high = RenderProfile::from_preset(RenderProfilePreset::DesktopHigh);
        assert!(deck.max_visible_lights < high.max_visible_lights);
        assert!(high.max_visible_lights >= 256);
        assert!(deck.max_visible_lights >= 64);
        let lights = corridor_point_lights(200, 2.0);
        let selection = select_visible_lights(&lights, &view_at_corridor(), deck.max_visible_lights);
        assert_eq!(selection.kept.len(), deck.max_visible_lights as usize);
        assert!(selection.dropped > 0);
    }

    #[test]
    fn shadow_casters_keep_authored_order_until_a_cap_drops_one() {
        let lights = [
            LabLocalLight::Point(LabPointLight {
                position: [0.0, 2.2, 0.2],
                color: [1.0, 0.9, 0.7],
                intensity: 4_000.0,
                radius: 6.0,
                cast_shadows: true,
                source_radius: None,
            }),
            LabLocalLight::Point(LabPointLight {
                position: [18.0, 2.0, 0.0],
                color: [0.6, 0.7, 1.0],
                intensity: 2_000.0,
                radius: 3.0,
                cast_shadows: true,
                source_radius: None,
            }),
        ];
        let view = LightView::new(
            [3.2, 2.4, 3.6],
            [0.0, 0.4, 0.0],
            45.0_f32.to_radians(),
            160.0 / 120.0,
            0.1,
            40.0,
        );
        let fitting = order_shadow_casters(&lights, &view, 16, 64);
        assert!(fitting.all_fit);
        assert_eq!(fitting.indices, vec![0, 1], "the near point stays in slot 0");
        let ranked = order_shadow_casters(&lights, &view, 1, 64);
        assert!(!ranked.all_fit);
        assert_eq!(ranked.indices[0], 0, "the near point wins the one shadow slot");
    }

    #[test]
    fn under_the_cap_authored_order_is_preserved() {
        let lights = corridor_point_lights(5, 3.0);
        let selection = select_visible_lights(&lights, &view_at_corridor(), 64);
        assert_eq!(selection.kept, vec![0, 1, 2, 3, 4]);
    }

    fn oracle_lights() -> Vec<LabLocalLight> {
        // Camera looks along +X. Keep every light in the view so the oracle is only
        // the documented candela × projected-radius-squared key and stable index tie.
        [
            (4.0, 2.0, 1_000.0),
            (8.0, 2.0, 4_000.0),
            (4.0, 2.0, 1_000.0), // Exact score tie with authored index 0.
            (16.0, 4.0, 64_000.0),
            (2.0, 1.0, 4_000.0),
            (12.0, 3.0, 36_000.0),
            (8.0, 2.0, 16_000.0),
        ]
        .into_iter()
        .map(|(depth, radius, lumens)| {
            LabLocalLight::Point(LabPointLight {
                position: [depth, 0.0, 0.0],
                color: [1.0; 3],
                intensity: lumens,
                radius,
                cast_shadows: false,
                source_radius: None,
            })
        })
        .collect()
    }

    fn documented_oracle_order(lights: &[LabLocalLight]) -> Vec<usize> {
        let mut ranked: Vec<_> = lights
            .iter()
            .enumerate()
            .map(|(index, light)| {
                let LabLocalLight::Point(point) = light else {
                    unreachable!("oracle fixture contains only point lights")
                };
                // Shared 1/(4π) cancels for comparison; no production scoring helper is used.
                (
                    index,
                    point.intensity * (point.radius / point.position[0]).powi(2),
                )
            })
            .collect();
        ranked.sort_by(|(left_index, left_score), (right_index, right_score)| {
            right_score
                .total_cmp(left_score)
                .then(left_index.cmp(right_index))
        });
        ranked.into_iter().map(|(index, _)| index).collect()
    }

    #[test]
    fn cap_sweep_matches_independent_importance_oracle_and_drop_counts() {
        let lights = oracle_lights();
        let view = LightView::new([0.0; 3], [1.0, 0.0, 0.0], 1.0, 1.0, 0.1, 32.0);
        let ranked = documented_oracle_order(&lights);
        assert_eq!(ranked[0..2], [3, 5]);
        assert_eq!(ranked[2..4], [4, 6]);
        assert_eq!(ranked[4..], [0, 1, 2]);

        for cap in 0..=lights.len() as u32 {
            let selection = select_visible_lights(&lights, &view, cap);
            let expected = if cap as usize == lights.len() {
                // The spec preserves authored order whenever the entire set fits.
                (0..lights.len()).collect::<Vec<_>>()
            } else {
                ranked[..cap as usize].to_vec()
            };
            assert_eq!(selection.kept, expected, "cap {cap}");
            assert_eq!(selection.authored, lights.len() as u32, "cap {cap}");
            assert_eq!(selection.cap, cap, "cap {cap}");
            assert_eq!(selection.dropped, lights.len() as u32 - cap, "cap {cap}");
        }

        // Nested prefixes are promised while every case is over-cap. The fitting case
        // returns to authored order, so it is intentionally excluded from this relation.
        for cap in 0..lights.len() as u32 - 1 {
            let smaller = select_visible_lights(&lights, &view, cap);
            let larger = select_visible_lights(&lights, &view, cap + 1);
            assert_eq!(
                larger.kept[..cap as usize],
                smaller.kept,
                "caps {cap} and {}",
                cap + 1
            );
        }
    }

    #[test]
    fn ranking_is_translation_invariant_and_shadow_flags_do_not_change_fitting_order() {
        let mut lights = oracle_lights();
        let view = LightView::new([0.0; 3], [1.0, 0.0, 0.0], 1.0, 1.0, 0.1, 32.0);
        let baseline = select_visible_lights(&lights, &view, 3);
        let fitting_before = select_visible_lights(&lights, &view, lights.len() as u32);

        for light in &mut lights {
            let LabLocalLight::Point(point) = light else {
                unreachable!("oracle fixture contains only point lights")
            };
            point.cast_shadows = true;
        }
        assert_eq!(
            select_visible_lights(&lights, &view, lights.len() as u32).kept,
            fitting_before.kept,
            "shadow flags do not reorder a set that fits"
        );

        for light in &mut lights {
            let LabLocalLight::Point(point) = light else {
                unreachable!("oracle fixture contains only point lights")
            };
            point.position[0] += 20.0;
            point.position[1] -= 7.0;
            point.position[2] += 11.0;
            point.cast_shadows = true;
        }
        let translated_view = LightView::new([20.0, -7.0, 11.0], [21.0, -7.0, 11.0], 1.0, 1.0, 0.1, 32.0);
        let translated = select_visible_lights(&lights, &translated_view, 3);
        assert_eq!(translated.kept, baseline.kept);
        assert_eq!(translated.dropped, baseline.dropped);
        assert_eq!(
            select_visible_lights(&lights, &translated_view, lights.len() as u32).kept,
            fitting_before.kept
        );
    }

    #[test]
    fn hard_upload_cap_boundary_is_clamped_without_losing_best_ranked_identities() {
        let mut lights = corridor_point_lights(MAX_UPLOADED_LIGHTS + 1, 2.0);
        if let LabLocalLight::Point(point) = &mut lights[MAX_UPLOADED_LIGHTS as usize] {
            point.position = [0.0, 1.2, 0.0];
            point.intensity = 1_000_000.0;
        }
        let view = view_at_corridor();
        let boundary = select_visible_lights(
            &lights[..MAX_UPLOADED_LIGHTS as usize],
            &view,
            MAX_UPLOADED_LIGHTS,
        );
        assert_eq!(boundary.kept.len(), MAX_UPLOADED_LIGHTS as usize);
        assert_eq!(boundary.dropped, 0);
        assert_eq!(boundary.kept.first(), Some(&0));

        let over = select_visible_lights(&lights, &view, MAX_UPLOADED_LIGHTS + 1);
        assert_eq!(over.cap, MAX_UPLOADED_LIGHTS);
        assert_eq!(over.kept.len(), MAX_UPLOADED_LIGHTS as usize);
        assert_eq!(over.dropped, 1);
        let mut expected = vec![MAX_UPLOADED_LIGHTS as usize];
        expected.extend(0..MAX_UPLOADED_LIGHTS as usize - 1);
        assert_eq!(over.kept, expected);
    }

    #[test]
    fn an_on_frame_corner_light_beats_one_past_the_top_edge() {
        // fov_y 60°, 16:9. At depth 10 the half-height is 10·tan(30°) ≈ 5.774 m
        // and the half-width ≈ 10.264 m. A light centred on the frustum corner
        // is on screen; a light 8 m up the view axis is past the 5.774 m top
        // edge. The old circular test (half-height × aspect.max(1)) demoted the
        // corner light to 1e-4 and kept the off-screen one.
        let corner = Vec3::new(
            10.0 * (std::f32::consts::FRAC_PI_6).tan() * 16.0 / 9.0,
            10.0 * (std::f32::consts::FRAC_PI_6).tan(),
            -10.0,
        );
        let above = Vec3::new(0.0, 8.0, -10.0);
        let lights = [
            LabLocalLight::Point(LabPointLight {
                position: corner.to_array(),
                color: [1.0; 3],
                intensity: 4_000.0,
                radius: 0.5,
                cast_shadows: false,
                source_radius: None,
            }),
            LabLocalLight::Point(LabPointLight {
                position: above.to_array(),
                color: [1.0; 3],
                intensity: 4_000.0,
                radius: 0.5,
                cast_shadows: false,
                source_radius: None,
            }),
        ];
        let view = LightView::new(
            [0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0],
            std::f32::consts::FRAC_PI_3,
            16.0 / 9.0,
            0.1,
            100.0,
        );
        let selection = select_visible_lights(&lights, &view, 1);
        assert_eq!(selection.kept, vec![0], "the on-frame corner light is kept");
        // The corner light's own score is not demoted below the off-screen one.
        assert!(
            screen_importance(&lights[0], &view) > screen_importance(&lights[1], &view),
            "corner {} vs above {}",
            screen_importance(&lights[0], &view),
            screen_importance(&lights[1], &view)
        );
        // Portrait: aspect 0.5. The true half-width is 10·tan(30°)·0.5 ≈ 2.887 m.
        // A light at (4, 4, −10) is past the right edge but sat inside the old
        // circle; a light on the true corner (2.887, 5.774, −10) is on frame.
        let portrait = LightView::new(
            [0.0, 0.0, 0.0],
            [0.0, 0.0, -1.0],
            std::f32::consts::FRAC_PI_3,
            0.5,
            0.1,
            100.0,
        );
        let half_y = 10.0 * (std::f32::consts::FRAC_PI_6).tan();
        let portrait_lights = [
            LabLocalLight::Point(LabPointLight {
                position: [half_y * 0.5, half_y, -10.0],
                color: [1.0; 3],
                intensity: 4_000.0,
                radius: 0.5,
                cast_shadows: false,
                source_radius: None,
            }),
            LabLocalLight::Point(LabPointLight {
                position: [4.0, 4.0, -10.0],
                color: [1.0; 3],
                intensity: 4_000.0,
                radius: 0.5,
                cast_shadows: false,
                source_radius: None,
            }),
        ];
        let selection = select_visible_lights(&portrait_lights, &portrait, 1);
        assert_eq!(selection.kept, vec![0], "the portrait corner light is kept");
    }

    #[test]
    fn near_bright_light_outranks_a_far_one() {
        // The m15b paged-shadow scene: the mover overlaps light 0 only.
        // Importance must keep that light first so the atlas slot does not swap.
        let lights = [
            LabLocalLight::Point(LabPointLight {
                position: [0.0, 2.2, 0.2],
                color: [1.0, 0.9, 0.7],
                intensity: 4_000.0,
                radius: 6.0,
                cast_shadows: true,
                source_radius: None,
            }),
            LabLocalLight::Point(LabPointLight {
                position: [18.0, 2.0, 0.0],
                color: [0.6, 0.7, 1.0],
                intensity: 2_000.0,
                radius: 3.0,
                cast_shadows: true,
                source_radius: None,
            }),
        ];
        let view = LightView::new(
            [3.2, 2.4, 3.6],
            [0.0, 0.4, 0.0],
            45.0_f32.to_radians(),
            160.0 / 120.0,
            0.1,
            40.0,
        );
        let near = screen_importance(&lights[0], &view);
        let far = screen_importance(&lights[1], &view);
        assert!(near > far, "near {near} should beat far {far}");
        let spot = LabLocalLight::Spot(LabSpotLight {
            position: [0.0, 2.0, 0.0],
            direction: [0.0, -1.0, 0.0],
            color: [1.0; 3],
            intensity: 40.0,
            range: 6.0,
            inner_angle: 0.2,
            outer_angle: 0.5,
            cast_shadows: true,
            source_radius: None,
        });
        assert!(screen_importance(&spot, &view).is_finite());
    }

    fn tiny(center: Vec3) -> (Vec3, Vec3) {
        let h = Vec3::splat(0.04);
        (center - h, center + h)
    }

    #[test]
    fn spot_cone_rejects_a_box_the_influence_sphere_would_keep() {
        let axis = Vec3::new(0.0, -1.0, 0.0);
        let range = 8.0;
        let cos_outer = 0.6_f32.cos();
        // Entirely behind the apex, still inside the sphere.
        let (lo, hi) = tiny(Vec3::new(0.0, 1.5, 0.0));
        assert!(sphere_hits_aabb(Vec3::ZERO, range, lo, hi));
        assert!(
            !cone_may_hit_aabb(Vec3::ZERO, axis, range, cos_outer, lo, hi),
            "a froxel behind the apex is not in the cone"
        );
        // On the axis, inside the range.
        let (lo, hi) = tiny(Vec3::new(0.0, -3.0, 0.0));
        assert!(cone_may_hit_aabb(Vec3::ZERO, axis, range, cos_outer, lo, hi));
        // Beside the cone, inside the sphere.
        let side = Vec3::new(0.0, -1.5, 4.0);
        let (lo, hi) = tiny(side);
        assert!(sphere_hits_aabb(Vec3::ZERO, range, lo, hi));
        assert!(
            !cone_may_hit_aabb(Vec3::ZERO, axis, range, cos_outer, lo, hi),
            "a sideways froxel inside the range is outside a narrow cone"
        );
        // Points inside the outer angle, at several distances. A tiny box
        // around each one must be kept: the test has no false negatives there.
        for angle in [0.0_f32, 0.15, 0.35] {
            for dist in [1.0_f32, 4.0, 7.5] {
                let point = Vec3::new(0.0, -dist * angle.cos(), dist * angle.sin());
                let (lo, hi) = tiny(point);
                assert!(
                    cone_may_hit_aabb(Vec3::ZERO, axis, range, cos_outer, lo, hi),
                    "angle {angle} dist {dist} is inside the cone"
                );
            }
        }
        // A flood (cos ≤ 0.05) and a zero axis keep the sphere result.
        let (lo, hi) = tiny(side);
        assert!(cone_may_hit_aabb(Vec3::ZERO, axis, range, 0.04, lo, hi));
        assert!(cone_may_hit_aabb(
            Vec3::ZERO,
            Vec3::ZERO,
            range,
            cos_outer,
            lo,
            hi
        ));
    }

    /// Sphere hits versus cone hits for one spot, over the real froxel grid.
    fn froxel_hits(light: &LabLocalLight, view: &LightView) -> (u32, u32) {
        let view_mat = look_at_view(Vec3::from_array(view.eye), Vec3::from_array(view.target), Vec3::Y);
        let (position, radius, _) = influence(light);
        let lp = view_mat.transform_point3(Vec3::from_array(position));
        let mut sphere = 0u32;
        let mut cone = 0u32;
        for (lo, hi) in cluster_bounds(view) {
            if !sphere_hits_aabb(lp, radius, lo, hi) {
                continue;
            }
            sphere += 1;
            let keep = if let LabLocalLight::Spot(spot) = light {
                let dir_v = view_mat.transform_vector3(Vec3::from_array(spot.direction));
                cone_may_hit_aabb(lp, dir_v, radius, spot.outer_angle.cos(), lo, hi)
            } else {
                true
            };
            if keep {
                cone += 1;
            }
        }
        (sphere, cone)
    }

    #[test]
    fn a_narrow_spot_drops_froxels_its_sphere_would_keep() {
        let view = view_at_corridor();
        let spot = LabLocalLight::Spot(LabSpotLight {
            position: [5.0, 1.6, 0.0],
            direction: [1.0, 0.0, 0.0],
            color: [1.0, 0.9, 0.7],
            intensity: 1_800.0,
            range: 15.0,
            inner_angle: 0.12,
            outer_angle: 0.28,
            cast_shadows: false,
            source_radius: None,
        });
        let (sphere, cone) = froxel_hits(&spot, &view);
        assert!(
            sphere > cone,
            "sphere {sphere} should cover froxels the cone {cone} misses"
        );
        assert!(cone > 0, "the cone still has to light the froxels on its axis");
        let (stored, _) = assign_clusters(&[spot], &view, 32);
        assert!(stored[0], "the spot is stored when its cone meets a froxel");
        // The cockpit spot aims at the far panel from behind the camera.
        // The cone cull has to keep that panel; a total miss is a false negative.
        let cockpit = LightView::new(
            [0.0, 0.35, 0.7],
            [0.0, 0.4, -1.2],
            70.0_f32.to_radians(),
            960.0 / 540.0,
            0.05,
            12.0,
        );
        let panel_spot = LabLocalLight::Spot(LabSpotLight {
            position: [0.15, 0.72, 1.15],
            direction: [-0.05, -0.15, -1.0],
            color: [1.0; 3],
            intensity: 4_500.0,
            range: 6.0,
            inner_angle: 0.2,
            outer_angle: 0.48,
            cast_shadows: true,
            source_radius: None,
        });
        let (sphere, cone) = froxel_hits(&panel_spot, &cockpit);
        let view_mat = look_at_view(
            Vec3::from_array(cockpit.eye),
            Vec3::from_array(cockpit.target),
            Vec3::Y,
        );
        let apex = view_mat.transform_point3(Vec3::new(0.15, 0.72, 1.15));
        let dir_v = view_mat.transform_vector3(Vec3::new(-0.05, -0.15, -1.0));
        let panel = view_mat.transform_point3(Vec3::new(0.0, 0.45, -2.2));
        let (lo, hi) = tiny(panel);
        let panel_hit = cone_may_hit_aabb(apex, dir_v, 6.0, 0.48_f32.cos(), lo, hi);
        let dir_n = dir_v.normalize();
        let mut worst = f32::MAX;
        for (lo, hi) in cluster_bounds(&cockpit) {
            if panel.cmplt(lo).any() || panel.cmpgt(hi).any() {
                continue;
            }
            let center = (lo + hi) * 0.5;
            let ext = (hi - lo) * 0.5;
            let rel = center - apex;
            let along = rel.dot(dir_n);
            let along_radius = ext.abs().dot(dir_n.abs());
            let perp = (rel - dir_n * along).length();
            let half_diag = ext.length();
            let cos_outer = 0.48_f32.cos();
            let tan_outer = (1.0 - cos_outer * cos_outer).max(0.0).sqrt() / cos_outer;
            let reach = (along + along_radius).clamp(0.0, 6.0);
            let limit = reach * tan_outer + half_diag;
            worst = worst.min(limit - perp);
        }
        assert!(
            worst > 0.05,
            "panel froxel margin {worst} is too thin to trust on the GPU"
        );
        assert!(
            cone > 50 && panel_hit,
            "cockpit spot dropped the panel: sphere {sphere} cone {cone} panel {panel_hit}"
        );
        // Point lights do not take the cone path. Every sphere hit stays.
        let point = corridor_point_lights(1, 2.0);
        let (sphere, cone) = froxel_hits(&point[0], &view);
        assert_eq!(sphere, cone);
        assert!(sphere > 0);
    }

    /// The WGSL cone test on lavapipe, with the same inputs as
    /// [`cone_may_hit_aabb`]. A vector `abs` in the first GPU version rejected
    /// the cockpit panel; this locks the scalar form to the CPU result.
    #[test]
    fn gpu_spot_cone_keeps_the_cockpit_panel_cell() {
        let device = match crate::RenderDevice::headless() {
            Ok(device) => device,
            Err(err) => {
                eprintln!("skip gpu cone: {err}");
                return;
            }
        };
        let cockpit = LightView::new(
            [0.0, 0.35, 0.7],
            [0.0, 0.4, -1.2],
            70.0_f32.to_radians(),
            960.0 / 540.0,
            0.05,
            12.0,
        );
        let view_mat = look_at_view(
            Vec3::from_array(cockpit.eye),
            Vec3::from_array(cockpit.target),
            Vec3::Y,
        );
        let apex = view_mat.transform_point3(Vec3::new(0.15, 0.72, 1.15));
        let dir_v = view_mat.transform_vector3(Vec3::new(-0.05, -0.15, -1.0));
        let panel = view_mat.transform_point3(Vec3::new(0.0, 0.45, -2.2));
        let cos_outer = 0.48_f32.cos();
        let mut cases: Vec<([f32; 16], bool)> = Vec::new();
        for (lo, hi) in cluster_bounds(&cockpit) {
            if panel.cmplt(lo).any() || panel.cmpgt(hi).any() {
                continue;
            }
            let expect = cone_may_hit_aabb(apex, dir_v, 6.0, cos_outer, lo, hi);
            cases.push((pack_cone_case(apex, dir_v, 6.0, cos_outer, lo, hi), expect));
        }
        assert!(!cases.is_empty(), "the panel point is outside every froxel");
        let axis = Vec3::new(0.0, -1.0, 0.0);
        let (lo, hi) = tiny(Vec3::new(0.0, -3.0, 0.0));
        cases.push((pack_cone_case(Vec3::ZERO, axis, 8.0, 0.6_f32.cos(), lo, hi), true));
        let (lo, hi) = tiny(Vec3::new(0.0, -1.5, 4.0));
        cases.push((
            pack_cone_case(Vec3::ZERO, axis, 8.0, 0.6_f32.cos(), lo, hi),
            false,
        ));
        let (lo, hi) = tiny(Vec3::new(0.0, 1.5, 0.0));
        cases.push((
            pack_cone_case(Vec3::ZERO, axis, 8.0, 0.6_f32.cos(), lo, hi),
            false,
        ));

        let input: Vec<f32> = cases.iter().flat_map(|(words, _)| *words).collect();
        let gpu = dispatch_cone_cases(&device, &input, cases.len());
        const N: usize = 12;
        assert_eq!(gpu.len(), cases.len() * N);
        for (index, ((_, expect), chunk)) in cases.iter().zip(gpu.chunks(N)).enumerate() {
            let accept = chunk[0] > 0.5;
            assert_eq!(
                accept, *expect,
                "case {index} gpu {accept} cpu {expect} floats {chunk:?}"
            );
            assert!(
                chunk[2].is_finite() && chunk[3].is_finite(),
                "case {index} non-finite cone terms {chunk:?}"
            );
        }
    }

    /// Audit G finding 2: a froxel with more lights than `max_per` keeps the lowest indices,
    /// and `select_visible` tie-breaks equal screen scores the same way. The tie-break is
    /// positional, so the input order defines the winner: determinism comes from
    /// `extract_frame` ordering `local_lights` by entity id
    /// (`runtime::tests::extraction_ties_break_by_entity_not_query_order`).
    #[test]
    fn full_froxels_and_equal_scores_keep_the_lowest_indices() {
        let lights: Vec<LabLocalLight> = (0..8)
            .map(|i| {
                LabLocalLight::Point(LabPointLight {
                    position: [i as f32 * 0.01, 1.0, -2.0],
                    color: [1.0; 3],
                    intensity: 1000.0,
                    radius: 100.0,
                    cast_shadows: false,
                    source_radius: None,
                })
            })
            .collect();
        let view = LightView::new([0.0, 1.0, 0.0], [0.0, 1.0, -1.0], 1.0, 16.0 / 9.0, 0.1, 100.0);
        let kept_indices = |stored: &[bool]| {
            stored
                .iter()
                .enumerate()
                .filter_map(|(i, stored)| stored.then_some(i))
                .collect::<Vec<_>>()
        };
        for max_per in [1u32, 3] {
            let (stored, overflow) = assign(&lights, &view, max_per);
            assert_eq!(
                kept_indices(&stored),
                (0..max_per as usize).collect::<Vec<_>>(),
                "max_per {max_per}: a full froxel keeps the lowest indices"
            );
            assert!(overflow > 0, "the froxel overflows");
        }
        // Positional tie-break: reversed input keeps the reversed light (slice index 0,
        // the original last light). Extraction's entity ordering is what makes slice
        // position mean "lowest entity" in a frame.
        let mut reversed = lights;
        reversed.reverse();
        let (stored, _) = assign(&reversed, &view, 1);
        assert_eq!(kept_indices(&stored), vec![0]);
        // Equal screen-importance scores: the upload cap keeps the lower indices too.
        assert_eq!(select_visible(&reversed, &view, 2).kept, vec![0, 1]);
    }

    /// Both spots project past the radius cap of 4, so both scores are 1600 and the
    /// farther authored light (index 0) wins the single slot.
    #[test]
    fn a_projected_radius_past_four_ties_and_keeps_authored_order() {
        let view = LightView::new([0.0, 0.0, 0.0], [0.0, 0.0, -10.0], 1.0, 1.0, 0.05, 100.0);
        let spot = |position: [f32; 3]| {
            LabLocalLight::Spot(LabSpotLight {
                position,
                direction: [0.0, 0.0, -1.0],
                color: [1.0; 3],
                intensity: 100.0,
                range: 8.0,
                inner_angle: 0.2,
                outer_angle: 0.4,
                cast_shadows: false,
                source_radius: None,
            })
        };
        let lights = vec![spot([0.0, 0.0, -2.0]), spot([0.0, 0.0, -1.0])];
        assert_eq!(screen_importance(&lights[0], &view), 1600.0);
        assert_eq!(screen_importance(&lights[1], &view), 1600.0);
        assert_eq!(select_visible(&lights, &view, 1).kept, vec![0]);
    }

    fn pack_cone_case(apex: Vec3, dir: Vec3, range: f32, cos_outer: f32, lo: Vec3, hi: Vec3) -> [f32; 16] {
        [
            apex.x, apex.y, apex.z, cos_outer, dir.x, dir.y, dir.z, range, lo.x, lo.y, lo.z, 0.0, hi.x, hi.y,
            hi.z, 0.0,
        ]
    }

    fn dispatch_cone_cases(device: &crate::RenderDevice, input: &[f32], cases: usize) -> Vec<f32> {
        const N: usize = 12;
        let shader = device.device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("cone cull diag"),
            source: wgpu::ShaderSource::Wgsl(CONE_DIAG_WGSL.into()),
        });
        let bgl = device
            .device
            .create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
                label: Some("cone diag bgl"),
                entries: &[
                    wgpu::BindGroupLayoutEntry {
                        binding: 0,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: true },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                    wgpu::BindGroupLayoutEntry {
                        binding: 1,
                        visibility: wgpu::ShaderStages::COMPUTE,
                        ty: wgpu::BindingType::Buffer {
                            ty: wgpu::BufferBindingType::Storage { read_only: false },
                            has_dynamic_offset: false,
                            min_binding_size: None,
                        },
                        count: None,
                    },
                ],
            });
        let pipeline = device
            .device
            .create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                label: Some("cone diag"),
                layout: Some(
                    &device
                        .device
                        .create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                            label: Some("cone diag layout"),
                            bind_group_layouts: &[Some(&bgl)],
                            immediate_size: 0,
                        }),
                ),
                module: &shader,
                entry_point: Some("main"),
                compilation_options: wgpu::PipelineCompilationOptions::default(),
                cache: crate::pipeline_cache::cache_for(&device).as_deref(),
            });
        let input_buf = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cone diag in"),
            size: (input.len() * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        device
            .queue
            .write_buffer(&input_buf, 0, bytemuck::cast_slice(input));
        let out_floats = cases * N;
        let output_buf = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cone diag out"),
            size: (out_floats * 4) as u64,
            usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        let staging = device.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("cone diag stage"),
            size: (out_floats * 4) as u64,
            usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bg = device.device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("cone diag bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry {
                    binding: 0,
                    resource: input_buf.as_entire_binding(),
                },
                wgpu::BindGroupEntry {
                    binding: 1,
                    resource: output_buf.as_entire_binding(),
                },
            ],
        });
        let mut encoder = device
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("cone diag"),
            });
        {
            let mut pass = encoder.counted_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("cone diag"),
                timestamp_writes: None,
            });
            pass.counted_set_pipeline(&pipeline);
            pass.counted_set_bind_group(0, &bg, &[]);
            pass.counted_dispatch(cases as u32, 1, 1);
        }
        encoder.copy_buffer_to_buffer(&output_buf, 0, &staging, 0, (out_floats * 4) as u64);
        device.queue.submit(Some(encoder.finish()));
        let slice = staging.slice(..);
        let (tx, rx) = std::sync::mpsc::channel();
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = tx.send(result);
        });
        device
            .device
            .poll(wgpu::PollType::wait_indefinitely())
            .expect("cone diag poll");
        rx.recv().expect("cone diag map").expect("cone diag map ok");
        let mapped = slice.get_mapped_range().expect("cone diag range");
        let words: Vec<f32> = bytemuck::cast_slice(&mapped).to_vec();
        drop(mapped);
        staging.unmap();
        words
    }

    const CONE_DIAG_WGSL: &str = r#"
@group(0) @binding(0) var<storage, read> cases: array<vec4<f32>>;
@group(0) @binding(1) var<storage, read_write> outs: array<f32>;

fn sphere_hits(center: vec3<f32>, radius: f32, lo: vec3<f32>, hi: vec3<f32>) -> bool {
    let c = clamp(center, lo, hi);
    return distance(c, center) <= radius;
}

fn run_case(apex: vec3<f32>, dir_in: vec3<f32>, range: f32, cos_outer: f32, lo: vec3<f32>, hi: vec3<f32>, base: u32) {
    let dir_len = length(dir_in);
    let dir = dir_in / max(dir_len, 1e-8);
    let center = (lo + hi) * 0.5;
    let ext = (hi - lo) * 0.5;
    let rel = center - apex;
    let along = dot(rel, dir);
    let along_s = ext.x * abs(dir.x) + ext.y * abs(dir.y) + ext.z * abs(dir.z);
    let cos_safe = max(cos_outer, 1e-3);
    let sin2 = max(1.0 - cos_safe * cos_safe, 0.0);
    let tan_upper = ((sin2 + 1.0) * 0.5) / cos_safe;
    let reach = clamp(along + along_s, 0.0, range);
    let perp2 = max(dot(rel, rel) - along * along, 0.0);
    let half_l1 = ext.x + ext.y + ext.z;
    let limit = reach * tan_upper + half_l1;
    var accept = 0.0;
    if (sphere_hits(apex, range, lo, hi)) {
        if (cos_outer <= 0.05 || cos_outer != cos_outer || dir_len <= 1e-4) {
            accept = 1.0;
        } else if (along + along_s >= 0.0 && perp2 <= limit * limit) {
            accept = 1.0;
        }
    }
    outs[base + 0u] = accept;
    outs[base + 1u] = tan_upper;
    outs[base + 2u] = perp2;
    outs[base + 3u] = limit;
    outs[base + 4u] = half_l1;
    outs[base + 5u] = along;
    outs[base + 6u] = along_s;
    outs[base + 7u] = reach;
    outs[base + 8u] = half_l1;
    outs[base + 9u] = tan_upper;
    outs[base + 10u] = cos_outer;
    outs[base + 11u] = dir_len;
}

@compute @workgroup_size(1)
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {
    let base_in = gid.x * 4u;
    let apex = cases[base_in].xyz;
    let cos_outer = cases[base_in].w;
    let dir = cases[base_in + 1u].xyz;
    let range = cases[base_in + 1u].w;
    let lo = cases[base_in + 2u].xyz;
    let hi = cases[base_in + 3u].xyz;
    run_case(apex, dir, range, cos_outer, lo, hi, gid.x * 12u);
}
"#;

    #[test]
    fn repeated_view_reuses_the_cluster_grid_and_the_scan() {
        let warnings = LightDropWarnings::new();
        let lights = scattered_lights(165, 3);
        let view = LightView::new([0.0, 1.7, 0.0], [10.0, 1.5, 3.0], 1.0, 16.0 / 9.0, 0.1, 300.0);
        let max_per = 16u32;
        let (stored, overflow) = warnings.assign_clusters(&lights, &view, max_per);
        assert_eq!(
            (stored.clone(), overflow),
            assign(&lights, &view, max_per),
            "cached assignment must match the full scan"
        );
        assert_eq!(
            warnings.grid_build_count(),
            1,
            "the first call builds the grid once"
        );
        assert_eq!(warnings.scan_count(), 1, "the first call scans once");
        let (again, overflow_again) = warnings.assign_clusters(&lights, &view, max_per);
        assert_eq!(again, stored);
        assert_eq!(overflow_again, overflow);
        assert_eq!(
            warnings.grid_build_count(),
            1,
            "a second identical frame rebuilt the froxel grid"
        );
        assert_eq!(
            warnings.scan_count(),
            1,
            "a second identical frame rescanned every froxel"
        );

        // Boxes depend on the projection only. The assignment does not: lights are
        // transformed by the eye, so a moved camera must scan again and may overflow
        // differently. Reusing the previous overflow across that move would be wrong.
        let moved = LightView {
            eye: [40.0, 18.0, -25.0],
            ..view
        };
        let (moved_stored, moved_overflow) = warnings.assign_clusters(&lights, &moved, max_per);
        assert_eq!(
            (moved_stored, moved_overflow),
            assign(&lights, &moved, max_per),
            "a moved eye must be assigned, not served the previous overflow"
        );
        assert_eq!(
            warnings.grid_build_count(),
            1,
            "the same fov, aspect, near and far must not rebuild the grid"
        );
        assert_eq!(warnings.scan_count(), 2, "a moved eye must scan again");

        let widened = LightView { fov_y: 1.4, ..moved };
        let _ = warnings.assign_clusters(&lights, &widened, max_per);
        assert_eq!(warnings.grid_build_count(), 2, "a new fov rebuilds the grid");
        assert_eq!(warnings.scan_count(), 3);

        let uncached = std::time::Instant::now();
        for _ in 0..4 {
            let _ = assign(&lights, &view, max_per);
        }
        let uncached = uncached.elapsed();
        let _ = warnings.assign_clusters(&lights, &view, max_per);
        let cached = std::time::Instant::now();
        for _ in 0..4 {
            let _ = warnings.assign_clusters(&lights, &view, max_per);
        }
        let cached = cached.elapsed();
        eprintln!("cluster assign 165 lights x4: full scan {uncached:?}, repeated view {cached:?}");
    }

    #[test]
    fn cluster_overflow_depends_on_the_eye() {
        let pile = vec![
            LabLocalLight::Point(LabPointLight {
                position: [0.0, 1.2, 0.0],
                color: [1.0; 3],
                intensity: 800.0,
                radius: 12.0,
                cast_shadows: false,
                source_radius: None,
            });
            40
        ];
        let near = LightView::new([0.0, 1.5, -3.0], [0.0, 1.2, 20.0], 1.1, 1.7, 0.1, 200.0);
        let high = LightView::new([0.0, 40.0, -3.0], [0.0, 1.2, 20.0], 1.1, 1.7, 0.1, 200.0);
        let (_, near_overflow) = assign(&pile, &near, 1);
        let (_, high_overflow) = assign(&pile, &high, 1);
        assert_ne!(
            near_overflow, high_overflow,
            "near {near_overflow} high {high_overflow}: overflow is not a function of the projection alone"
        );
    }
}
