// SPDX-License-Identifier: MPL-2.0
//! Post volumes: spatial overrides of a view's post settings
//! (`docs/specs/30-post-volumes-and-physical-camera.md`, ADR-1464).
//!
//! A [`PostVolume`] on an entity with a [`GlobalTransform`] changes the post settings of every
//! camera whose eye it reaches: a box, a sphere, a convex hull of up to 16 planes (format v3,
//! ADR-2323), or [`PostVolumeShape::Unbound`] (everywhere). Each setting in [`PostOverrides`] is
//! optional; a volume changes only what it names. A volume may also name a colour LUT
//! ([`PostVolume::lut`]) that blends into the view's LUT when the host turns that on
//! ([`crate::post_volume_luts`], off by default).
//!
//! # Weight
//!
//! [`PostVolume::weight_at`] is `enabled · clamp(weight, 0, 1) · spatial`, where `spatial` is 1
//! for an unbound volume and otherwise the exposure-zone weight of the signed distance
//! ([`crate::zone_weight`]): 1 inside and on the surface, a Hermite smoothstep across
//! [`PostVolume::blend_distance`] meters outside, 0 beyond.
//!
//! # Blend
//!
//! [`blend_post_volumes`] groups the contributions by priority and applies the tiers from the
//! lowest priority to the highest. Inside a tier, for each field, the volumes that set it are
//! weight-averaged (`mean = Σ w·x / Σ w`) and the field moves toward the mean by
//! `α = min(1, Σ w)`. That makes the result independent of entity order inside a tier, lets a
//! full-weight higher tier replace what lower tiers did, and keeps every continuous field
//! continuous in the camera position. White balance blends in mired (10⁶ / K). The tonemap
//! operator, which cannot be interpolated, switches to the tier's heaviest volume once the tier's
//! weight for it reaches one half. Results are clamped to each field's working range.
//!
//! The renderer blends at every camera's eye in `extract_frame`; a scene without volumes
//! extracts exactly the post settings it did before. A volume's [`PostOverrides::eye`] is not
//! blended here: the framework folds it into the exposure-zone eye (ADR-322).

use regia_app::prelude::*;
use regia_ecs::reflect::TypeRegistry;
use regia_scene::GlobalTransform;

use crate::exposure_zones::{
    DEFAULT_ADAPT_DOWN_SPEED, DEFAULT_ADAPT_UP_SPEED, ExposureZone, ExposureZoneShape, zone_signed_distance,
    zone_weight,
};
use crate::post::{LocalExposure, PostSettings, TonemapOperator};

/// Current version of the [`PostVolume`] component's serialized form (`post-volume` in
/// `cargo xtask check-formats`). Version 2 (ADR-1863) added the lens-effect overrides
/// ([`PostOverrides::lens_flare_intensity`] and the three after it); they are optional, so a
/// version-1 component reads unchanged with them unset. Version 3 (ADR-2323) added the
/// [`PostVolumeShape::ConvexHull`] shape and the optional [`PostVolume::lut`]; a version-2
/// component reads unchanged (its shape is one of the old three, its LUT unset).
pub const POST_VOLUME_FORMAT_VERSION: u32 = 3;

/// Coolest and warmest white balance a volume may ask for, kelvin (the Planckian fit's range).
const KELVIN_RANGE: (f32, f32) = (1667.0, 25_000.0);

/// Errors from reading a [`PostVolume`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PostVolumeError {
    /// The data was written by a newer engine.
    #[error("post volume format_version {found} is newer than this engine's {POST_VOLUME_FORMAT_VERSION}")]
    NewerVersion {
        /// The version found in the data.
        found: u32,
    },
}

/// Shape of a [`PostVolume`], in the entity's local space (the entity scale stretches it).
#[derive(Reflect, Clone, Debug, PartialEq)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::post_volumes"]
pub enum PostVolumeShape {
    /// A box centred on the entity.
    Box {
        /// Half size along local X, Y and Z, meters (a negative component counts as its absolute
        /// value).
        half_extents: [f32; 3],
    },
    /// A sphere centred on the entity; its radius scales with the mean of the entity's X and Z
    /// scale.
    Sphere {
        /// Radius, meters (a negative value counts as its absolute value).
        radius: f32,
    },
    /// Everywhere: the volume reaches every camera (a level-wide look a smaller volume can
    /// override).
    Unbound,
    /// The intersection of half-spaces `n · x ≤ d`, each plane `[nx, ny, nz, d]` in local space
    /// (the normal need not be unit length; `d` is then in units of its length). 1 to
    /// [`crate::post_volume_shapes::MAX_HULL_PLANES`] planes; more, none, a non-finite value or
    /// a zero normal makes the volume contribute nothing. The distance and the blend band are
    /// exact world meters under any entity transform (format v3, ADR-2323).
    ConvexHull {
        /// The planes, outward normals.
        planes: Vec<[f32; 4]>,
    },
}

impl Default for PostVolumeShape {
    fn default() -> Self {
        Self::Box {
            half_extents: [1.0, 1.0, 1.0],
        }
    }
}

impl PostVolumeShape {
    /// The wire name: `box`, `sphere`, `unbound` or `convex_hull`.
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Box { .. } => "box",
            Self::Sphere { .. } => "sphere",
            Self::Unbound => "unbound",
            Self::ConvexHull { .. } => "convex_hull",
        }
    }

    /// The shape can contribute: every shape but a convex hull whose planes are not valid
    /// ([`crate::post_volume_shapes::hull_planes_valid`]).
    #[must_use]
    pub fn is_valid(&self) -> bool {
        match self {
            Self::ConvexHull { planes } => crate::post_volume_shapes::hull_planes_valid(planes),
            _ => true,
        }
    }
}

/// An EV range and adaptation speeds a volume asks of the eye: the same request as an
/// [`ExposureZone`], with the volume's weight and priority (ADR-322 combining rules).
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::post_volumes"]
pub struct EyeRange {
    /// Low end of the EV100 range.
    pub ev_min: f32,
    /// High end of the EV100 range.
    pub ev_max: f32,
    /// EV per second toward a brighter target.
    pub adapt_up_speed: f32,
    /// EV per second toward a darker target.
    pub adapt_down_speed: f32,
}

impl Default for EyeRange {
    fn default() -> Self {
        Self {
            ev_min: 6.0,
            ev_max: 9.0,
            adapt_up_speed: DEFAULT_ADAPT_UP_SPEED,
            adapt_down_speed: DEFAULT_ADAPT_DOWN_SPEED,
        }
    }
}

/// The settings a [`PostVolume`] changes. Every field is optional: `None` leaves the setting to
/// the project, the level look, the camera and lower-priority volumes. A value that is not
/// finite counts as `None`.
#[derive(Reflect, Clone, Copy, Debug, PartialEq, Default)]
#[reflect(Default, Clone)]
#[type_path = "regia_render::post_volumes"]
pub struct PostOverrides {
    /// [`PostSettings::exposure_compensation`], EV (positive brightens).
    pub exposure_compensation: Option<f32>,
    /// [`PostSettings::auto_exposure_min_ev100`].
    pub exposure_min_ev100: Option<f32>,
    /// [`PostSettings::auto_exposure_max_ev100`].
    pub exposure_max_ev100: Option<f32>,
    /// [`crate::ExposureMeter::center_weight`]: 0 averages the frame, 1 weights its centre.
    pub meter_center_weight: Option<f32>,
    /// [`PostSettings::tonemap`] (switches at half weight).
    pub tonemap: Option<TonemapOperator>,
    /// [`PostSettings::contrast`].
    pub contrast: Option<f32>,
    /// [`PostSettings::saturation`].
    pub saturation: Option<f32>,
    /// [`PostSettings::bloom_intensity`], 0–1.
    pub bloom_intensity: Option<f32>,
    /// [`crate::ColorGrade::white_balance_kelvin`] (blended in mired).
    pub white_balance_kelvin: Option<f32>,
    /// [`crate::ColorGrade::tint`], −1..1.
    pub tint: Option<f32>,
    /// [`crate::ColorGrade::lift`].
    pub lift: Option<[f32; 3]>,
    /// [`crate::ColorGrade::gamma`].
    pub gamma: Option<[f32; 3]>,
    /// [`crate::ColorGrade::gain`].
    pub gain: Option<[f32; 3]>,
    /// [`PostSettings::vignette`], 0–1.
    pub vignette: Option<f32>,
    /// [`PostSettings::film_grain`], 0–1.
    pub film_grain: Option<f32>,
    /// [`LocalExposure::shadow_contrast`] (from the identity when the view has no local exposure).
    pub local_shadow_contrast: Option<f32>,
    /// [`LocalExposure::highlight_contrast`].
    pub local_highlight_contrast: Option<f32>,
    /// [`LocalExposure::detail_strength`].
    pub local_detail_strength: Option<f32>,
    /// [`PostSettings::ao_intensity`], 0–4.
    pub ao_intensity: Option<f32>,
    /// [`PostSettings::fog_tint`].
    pub fog_tint: Option<[f32; 3]>,
    /// An eye-adaptation request, folded into the exposure-zone eye (not blended here).
    pub eye: Option<EyeRange>,
    /// [`crate::LensEffects::flare_intensity`] (format v2, ADR-1863).
    pub lens_flare_intensity: Option<f32>,
    /// [`crate::LensEffects::flare_threshold`] (format v2).
    pub lens_flare_threshold: Option<f32>,
    /// [`crate::LensEffects::dirt_intensity`] (format v2).
    pub lens_dirt_intensity: Option<f32>,
    /// [`crate::LensEffects::chromatic_aberration`] (format v2).
    pub chromatic_aberration: Option<f32>,
}

impl PostOverrides {
    /// Every field's wire name, in declaration order (the agent protocol and the book use them).
    pub const FIELDS: &'static [&'static str] = &[
        "exposure_compensation",
        "exposure_min_ev100",
        "exposure_max_ev100",
        "meter_center_weight",
        "tonemap",
        "contrast",
        "saturation",
        "bloom_intensity",
        "white_balance_kelvin",
        "tint",
        "lift",
        "gamma",
        "gain",
        "vignette",
        "film_grain",
        "local_shadow_contrast",
        "local_highlight_contrast",
        "local_detail_strength",
        "ao_intensity",
        "fog_tint",
        "eye",
        "lens_flare_intensity",
        "lens_flare_threshold",
        "lens_dirt_intensity",
        "chromatic_aberration",
    ];

    /// The wire names of the fields this volume sets, in declaration order.
    #[must_use]
    pub fn set_fields(&self) -> Vec<&'static str> {
        let set = [
            self.exposure_compensation.is_some(),
            self.exposure_min_ev100.is_some(),
            self.exposure_max_ev100.is_some(),
            self.meter_center_weight.is_some(),
            self.tonemap.is_some(),
            self.contrast.is_some(),
            self.saturation.is_some(),
            self.bloom_intensity.is_some(),
            self.white_balance_kelvin.is_some(),
            self.tint.is_some(),
            self.lift.is_some(),
            self.gamma.is_some(),
            self.gain.is_some(),
            self.vignette.is_some(),
            self.film_grain.is_some(),
            self.local_shadow_contrast.is_some(),
            self.local_highlight_contrast.is_some(),
            self.local_detail_strength.is_some(),
            self.ao_intensity.is_some(),
            self.fog_tint.is_some(),
            self.eye.is_some(),
            self.lens_flare_intensity.is_some(),
            self.lens_flare_threshold.is_some(),
            self.lens_dirt_intensity.is_some(),
            self.chromatic_aberration.is_some(),
        ];
        Self::FIELDS
            .iter()
            .zip(set)
            .filter_map(|(name, on)| on.then_some(*name))
            .collect()
    }

    /// No field is set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A volume that overrides the post settings of the cameras it reaches.
///
/// Pair it with a [`GlobalTransform`]. Scene files name it
/// `regia_render::post_volumes::PostVolume` (a pinned type path); the editor inspector and
/// `scene.add_component` edit it through reflection.
///
/// # Example
///
/// ```
/// use regia_render::post_volumes::{PostOverrides, PostVolume, PostVolumeShape};
///
/// let cellar = PostVolume {
///     shape: PostVolumeShape::Box { half_extents: [6.0, 3.0, 10.0] },
///     blend_distance: 2.0,
///     priority: 1,
///     overrides: PostOverrides {
///         exposure_compensation: Some(0.5),
///         saturation: Some(1.1),
///         ..PostOverrides::default()
///     },
///     ..PostVolume::default()
/// };
/// assert_eq!(cellar.overrides.set_fields(), vec!["exposure_compensation", "saturation"]);
/// ```
#[derive(Component, Reflect, Clone, Debug, PartialEq)]
#[reflect(Component, Default, Clone)]
#[type_path = "regia_render::post_volumes"]
pub struct PostVolume {
    /// Serialized-form version ([`POST_VOLUME_FORMAT_VERSION`] when written by this engine).
    pub format_version: u32,
    /// A disabled volume contributes nothing.
    pub enabled: bool,
    /// Box, sphere, convex hull or unbound.
    #[reflect(@regia_ecs::attrs::EditCondition("enabled"))]
    pub shape: PostVolumeShape,
    /// Higher tiers are applied after lower ones (any `i32`).
    #[reflect(@regia_ecs::attrs::EditCondition("enabled"))]
    pub priority: i32,
    /// Meters outside the shape across which the weight rises from 0 to 1; 0, negative or not
    /// finite is a hard edge. Ignored by [`PostVolumeShape::Unbound`].
    #[reflect(@regia_ecs::attrs::EditCondition("enabled && shape != Unbound"))]
    pub blend_distance: f32,
    /// Scales the whole volume, clamped to 0–1 (not finite counts as 0).
    #[reflect(@regia_ecs::attrs::EditCondition("enabled"))]
    pub weight: f32,
    /// The settings this volume changes.
    #[reflect(@regia_ecs::attrs::EditCondition("enabled"))]
    pub overrides: PostOverrides,
    /// A `.cube` or PNG-strip 3D LUT (a path, relative to the project root) this volume blends
    /// into the view's LUT by its weight and priority (format v3, ADR-2323). Ignored unless the
    /// host turns volume LUTs on ([`crate::post_volume_luts::PostVolumeLuts::enabled`]).
    #[reflect(@regia_ecs::attrs::EditCondition("enabled"))]
    pub lut: Option<String>,
}

impl Default for PostVolume {
    fn default() -> Self {
        Self {
            format_version: POST_VOLUME_FORMAT_VERSION,
            enabled: true,
            shape: PostVolumeShape::default(),
            priority: 0,
            blend_distance: 0.0,
            weight: 1.0,
            overrides: PostOverrides::default(),
            lut: None,
        }
    }
}

impl PostVolume {
    /// Brings data written by an older engine up to [`POST_VOLUME_FORMAT_VERSION`].
    ///
    /// `0` (a hand-written component that left the field out) reads as version 1. Version 1 to
    /// 2 only adds the optional lens-effect overrides, which a version-1 component leaves unset;
    /// version 2 to 3 only adds the convex-hull shape (an older volume has one of the other
    /// three) and the optional LUT (unset). Every step therefore upgrades by setting the version.
    ///
    /// # Errors
    /// [`PostVolumeError::NewerVersion`] for data from a newer engine.
    pub fn migrate(mut self) -> Result<Self, PostVolumeError> {
        match self.format_version {
            0..=2 => {
                self.format_version = POST_VOLUME_FORMAT_VERSION;
                Ok(self)
            }
            POST_VOLUME_FORMAT_VERSION => Ok(self),
            found => Err(PostVolumeError::NewerVersion { found }),
        }
    }

    /// The renderer can read this version (it is not newer than the engine's).
    #[must_use]
    pub fn is_supported(&self) -> bool {
        self.format_version <= POST_VOLUME_FORMAT_VERSION
    }

    /// World signed distance from `point` to the shape, meters (positive outside);
    /// `f32::NEG_INFINITY` for an unbound volume.
    #[must_use]
    pub fn signed_distance(&self, world_from_local: &GlobalTransform, point: [f32; 3]) -> f32 {
        self.signed_distance_within(world_from_local, point, f32::INFINITY)
    }

    /// [`Self::signed_distance`], exact up to `reach` meters outside; beyond it a convex hull
    /// may return a lower bound (still above `reach`) instead of searching its edges.
    fn signed_distance_within(&self, world_from_local: &GlobalTransform, point: [f32; 3], reach: f32) -> f32 {
        let shape = match &self.shape {
            PostVolumeShape::Unbound => return f32::NEG_INFINITY,
            PostVolumeShape::Box { half_extents } => ExposureZoneShape::Box {
                half_extents: *half_extents,
            },
            PostVolumeShape::Sphere { radius } => ExposureZoneShape::Capsule {
                radius: *radius,
                half_height: 0.0,
            },
            PostVolumeShape::ConvexHull { planes } => {
                return crate::post_volume_shapes::hull_signed_distance_within(
                    planes,
                    world_from_local,
                    point,
                    reach,
                );
            }
        };
        zone_signed_distance(shape, world_from_local, point)
    }

    /// The volume's effective weight at `point`, 0–1 (see the module docs).
    #[must_use]
    pub fn weight_at(&self, world_from_local: &GlobalTransform, point: [f32; 3]) -> f32 {
        let scale = sanitize_weight(self.weight);
        if !self.enabled || scale <= 0.0 {
            return 0.0;
        }
        let spatial = match self.shape {
            PostVolumeShape::Unbound => 1.0,
            _ => {
                if !point.iter().all(|v| v.is_finite()) {
                    return 0.0;
                }
                let reach = if self.blend_distance.is_finite() {
                    self.blend_distance.max(0.0)
                } else {
                    0.0
                };
                zone_weight(
                    self.signed_distance_within(world_from_local, point, reach),
                    self.blend_distance,
                )
            }
        };
        spatial * scale
    }

    /// The exposure zone this volume's [`PostOverrides::eye`] stands for (its range, speeds and
    /// the volume's priority), `None` without an eye request. The zone's shape is unused: the
    /// caller weighs the volume with [`Self::weight_at`].
    #[must_use]
    pub fn eye_zone(&self) -> Option<ExposureZone> {
        let eye = self.overrides.eye?;
        Some(ExposureZone {
            ev_min: eye.ev_min,
            ev_max: eye.ev_max,
            adapt_up_speed: eye.adapt_up_speed,
            adapt_down_speed: eye.adapt_down_speed,
            priority: self.priority,
            ..ExposureZone::default()
        })
    }
}

/// One volume's share at a point, the input of [`blend_post_volumes`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct VolumeContribution {
    /// Effective weight at the point, 0–1 ([`PostVolume::weight_at`]).
    pub weight: f32,
    /// The volume's priority.
    pub priority: i32,
    /// A stable tie-break key (the entity bits); unique per volume.
    pub key: u64,
    /// The settings the volume changes.
    pub overrides: PostOverrides,
}

/// Collects into `out` (cleared first) the volumes that reach `eye`: supported version, positive
/// weight. `out` is the caller's scratch vector, so a steady frame allocates nothing.
pub fn gather_contributions<'a>(
    volumes: impl IntoIterator<Item = (Entity, &'a PostVolume, &'a GlobalTransform)>,
    eye: [f32; 3],
    out: &mut Vec<VolumeContribution>,
) {
    out.clear();
    for (entity, volume, world) in volumes {
        if !volume.is_supported() {
            warn_newer_once(volume.format_version);
            continue;
        }
        let weight = volume.weight_at(world, eye);
        if weight > 0.0 {
            out.push(VolumeContribution {
                weight,
                priority: volume.priority,
                key: entity.to_bits(),
                overrides: volume.overrides,
            });
        }
    }
}

/// Logs once per process that a volume from a newer engine was skipped.
fn warn_newer_once(found: u32) {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "regia_render",
            "a PostVolume has format_version {found}, newer than this engine's \
             {POST_VOLUME_FORMAT_VERSION}; it is skipped (logged once)"
        );
    }
}

/// Blends `contributions` into `post` (see the module docs). Sorts `contributions` by priority
/// and key. An empty list leaves `post` untouched.
pub fn blend_post_volumes(post: &mut PostSettings, contributions: &mut [VolumeContribution]) {
    if contributions.is_empty() {
        return;
    }
    contributions.sort_unstable_by(|a, b| a.priority.cmp(&b.priority).then(a.key.cmp(&b.key)));
    let mut local = None;
    for tier in contributions.chunk_by(|a, b| a.priority == b.priority) {
        apply_tier(post, tier, &mut local);
    }
    if let Some(local) = local {
        let local = local.sanitized();
        post.local_exposure = (!local.is_identity()).then_some(local);
    }
}

/// A weight that is finite and positive, at most 1; anything else is 0.
fn sanitize_weight(weight: f32) -> f32 {
    if weight.is_finite() && weight > 0.0 {
        weight.min(1.0)
    } else {
        0.0
    }
}

/// `(α, mean)` over the tier's volumes that set a scalar field; `None` when none does.
fn tier_mean(tier: &[VolumeContribution], get: impl Fn(&PostOverrides) -> Option<f32>) -> Option<(f32, f32)> {
    let (mut sum_w, mut sum_wx) = (0.0f32, 0.0f32);
    for c in tier {
        let w = sanitize_weight(c.weight);
        if let Some(x) = get(&c.overrides).filter(|x| x.is_finite())
            && w > 0.0
        {
            sum_w += w;
            sum_wx += w * x;
        }
    }
    (sum_w > 0.0).then(|| (sum_w.min(1.0), sum_wx / sum_w))
}

/// [`tier_mean`] for a three-channel field (every channel must be finite).
fn tier_mean3(
    tier: &[VolumeContribution],
    get: impl Fn(&PostOverrides) -> Option<[f32; 3]>,
) -> Option<(f32, [f32; 3])> {
    let (mut sum_w, mut sum_wx) = (0.0f32, [0.0f32; 3]);
    for c in tier {
        let w = sanitize_weight(c.weight);
        if let Some(x) = get(&c.overrides).filter(|x| x.iter().all(|v| v.is_finite()))
            && w > 0.0
        {
            sum_w += w;
            for (acc, v) in sum_wx.iter_mut().zip(x) {
                *acc += w * v;
            }
        }
    }
    (sum_w > 0.0).then(|| (sum_w.min(1.0), sum_wx.map(|v| v / sum_w)))
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + t * (b - a)
}

/// Moves `value` toward the tier's mean of `get`, then clamps it to `lo..=hi`.
fn blend_scalar(
    value: &mut f32,
    tier: &[VolumeContribution],
    get: impl Fn(&PostOverrides) -> Option<f32>,
    lo: f32,
    hi: f32,
) {
    if let Some((alpha, mean)) = tier_mean(tier, get) {
        *value = lerp(*value, mean, alpha).clamp(lo, hi);
    }
}

/// Moves each channel of `value` toward the tier's mean of `get`, then clamps it to `lo..=hi`.
fn blend_rgb(
    value: &mut [f32; 3],
    tier: &[VolumeContribution],
    get: impl Fn(&PostOverrides) -> Option<[f32; 3]>,
    lo: f32,
    hi: f32,
) {
    if let Some((alpha, mean)) = tier_mean3(tier, get) {
        for (v, m) in value.iter_mut().zip(mean) {
            *v = lerp(*v, m, alpha).clamp(lo, hi);
        }
    }
}

/// Kelvin to mired, inside the Planckian fit's range.
fn mired(kelvin: f32) -> f32 {
    1.0e6 / kelvin.clamp(KELVIN_RANGE.0, KELVIN_RANGE.1)
}

/// Applies one priority tier to `post`. `local` collects the local-exposure factors (started
/// from the view's, or the identity) when any tier touches them.
fn apply_tier(post: &mut PostSettings, tier: &[VolumeContribution], local: &mut Option<LocalExposure>) {
    const EV: f32 = 64.0;
    blend_scalar(
        &mut post.exposure_compensation,
        tier,
        |o| o.exposure_compensation,
        -EV,
        EV,
    );
    blend_scalar(
        &mut post.auto_exposure_min_ev100,
        tier,
        |o| o.exposure_min_ev100,
        -EV,
        EV,
    );
    blend_scalar(
        &mut post.auto_exposure_max_ev100,
        tier,
        |o| o.exposure_max_ev100,
        -EV,
        EV,
    );
    blend_scalar(
        &mut post.meter.center_weight,
        tier,
        |o| o.meter_center_weight,
        0.0,
        1.0,
    );
    blend_tonemap(&mut post.tonemap, tier);
    blend_scalar(&mut post.contrast, tier, |o| o.contrast, 0.05, f32::MAX);
    blend_scalar(&mut post.saturation, tier, |o| o.saturation, 0.0, f32::MAX);
    blend_scalar(&mut post.bloom_intensity, tier, |o| o.bloom_intensity, 0.0, 1.0);
    if let Some((alpha, mean)) = tier_mean(tier, |o| {
        o.white_balance_kelvin
            .filter(|k| k.is_finite() && *k > 0.0)
            .map(mired)
    }) {
        let current = mired(post.grade.white_balance_kelvin);
        post.grade.white_balance_kelvin =
            (1.0e6 / lerp(current, mean, alpha)).clamp(KELVIN_RANGE.0, KELVIN_RANGE.1);
    }
    blend_scalar(&mut post.grade.tint, tier, |o| o.tint, -1.0, 1.0);
    blend_rgb(&mut post.grade.lift, tier, |o| o.lift, -1.0, 1.0);
    blend_rgb(&mut post.grade.gamma, tier, |o| o.gamma, 0.05, f32::MAX);
    blend_rgb(&mut post.grade.gain, tier, |o| o.gain, 0.0, f32::MAX);
    blend_scalar(&mut post.vignette, tier, |o| o.vignette, 0.0, 1.0);
    blend_scalar(&mut post.film_grain, tier, |o| o.film_grain, 0.0, 1.0);
    let touches_local = tier.iter().any(|c| {
        c.overrides.local_shadow_contrast.is_some()
            || c.overrides.local_highlight_contrast.is_some()
            || c.overrides.local_detail_strength.is_some()
    });
    if touches_local {
        let factors = local.get_or_insert_with(|| post.local_exposure.unwrap_or_default());
        blend_scalar(
            &mut factors.shadow_contrast,
            tier,
            |o| o.local_shadow_contrast,
            0.0,
            1.0,
        );
        blend_scalar(
            &mut factors.highlight_contrast,
            tier,
            |o| o.local_highlight_contrast,
            0.0,
            1.0,
        );
        blend_scalar(
            &mut factors.detail_strength,
            tier,
            |o| o.local_detail_strength,
            0.0,
            4.0,
        );
    }
    blend_scalar(&mut post.ao_intensity, tier, |o| o.ao_intensity, 0.0, 4.0);
    blend_rgb(&mut post.fog_tint, tier, |o| o.fog_tint, 0.0, f32::MAX);
    // Lens effects (format v2, ADR-1863): from the camera's (or the project's) strengths.
    let lens = &mut post.lens_effects;
    let (lo, hi) = crate::lens_effects::FLARE_INTENSITY_RANGE;
    blend_scalar(
        &mut lens.flare_intensity,
        tier,
        |o| o.lens_flare_intensity,
        lo,
        hi,
    );
    let (lo, hi) = crate::lens_effects::FLARE_THRESHOLD_RANGE;
    blend_scalar(
        &mut lens.flare_threshold,
        tier,
        |o| o.lens_flare_threshold,
        lo,
        hi,
    );
    let (lo, hi) = crate::lens_effects::DIRT_INTENSITY_RANGE;
    blend_scalar(&mut lens.dirt_intensity, tier, |o| o.lens_dirt_intensity, lo, hi);
    let (lo, hi) = crate::lens_effects::CHROMATIC_ABERRATION_RANGE;
    blend_scalar(
        &mut lens.chromatic_aberration,
        tier,
        |o| o.chromatic_aberration,
        lo,
        hi,
    );
}

/// The discrete tonemap rule: at a tier weight of one half or more, the heaviest volume's
/// operator (ties: the lowest key, the first in the sorted tier).
fn blend_tonemap(value: &mut TonemapOperator, tier: &[VolumeContribution]) {
    let mut sum = 0.0f32;
    let mut best: Option<(f32, TonemapOperator)> = None;
    for c in tier {
        let w = sanitize_weight(c.weight);
        let Some(op) = c.overrides.tonemap else {
            continue;
        };
        if w <= 0.0 {
            continue;
        }
        sum += w;
        if best.is_none_or(|(bw, _)| w > bw) {
            best = Some((w, op));
        }
    }
    if let Some((_, op)) = best
        && sum >= 0.5
    {
        *value = op;
    }
}

/// Registers [`PostVolume`] and the types it holds with a type registry (scene files and the
/// inspector read it through reflection). `register_render_types` registers the same types on an
/// `App`; a bare `World` with an `AppTypeRegistry` (tests, tools) calls this.
pub fn register_post_volume_types(registry: &mut TypeRegistry) {
    registry.register::<PostVolume>();
    registry.register::<PostVolumeShape>();
    registry.register::<PostOverrides>();
    registry.register::<EyeRange>();
    registry.register::<TonemapOperator>();
    // Format v3 (ADR-2323): the hull planes and the LUT path.
    registry.register::<Vec<[f32; 4]>>();
    registry.register::<[f32; 4]>();
    registry.register::<Option<String>>();
}

/// The `from` versions of the format's migration steps (`0`: a hand-written component without
/// `format_version`).
#[must_use]
pub fn post_volume_migration_sources() -> Vec<u32> {
    vec![0, 1, 2]
}

/// The serialized shape of [`PostVolume`] for `cargo xtask check-formats` (entry `post-volume`):
/// every reflected field (name and type) of [`PostVolume`], [`PostOverrides`] and [`EyeRange`],
/// and every variant (with its fields) of [`PostVolumeShape`] and [`TonemapOperator`], in
/// declaration order. A change here without a [`POST_VOLUME_FORMAT_VERSION`] bump fails the
/// check.
#[must_use]
pub fn post_volume_schema_fingerprint() -> String {
    let mut out = String::new();
    struct_fields(&mut out, "PostVolume", PostVolume::type_info());
    enum_variants(&mut out, "PostVolumeShape", PostVolumeShape::type_info());
    struct_fields(&mut out, "PostOverrides", PostOverrides::type_info());
    struct_fields(&mut out, "EyeRange", EyeRange::type_info());
    enum_variants(&mut out, "TonemapOperator", TonemapOperator::type_info());
    out
}

/// Appends `Name{field:type,...};` for a reflected struct.
pub(crate) fn struct_fields(out: &mut String, name: &str, info: &regia_ecs::reflect::TypeInfo) {
    use regia_ecs::reflect::TypeInfo;
    // Field types are spelled with the crate names from before the rename (ADR-1388), so the
    // frozen hash does not move with crate names.
    let stable = regia_core::brand::fingerprint_type_path;
    out.push_str(name);
    out.push('{');
    if let TypeInfo::Struct(info) = info {
        for field in info.iter() {
            out.push_str(&format!("{}:{},", field.name(), stable(field.type_path())));
        }
    }
    out.push_str("};");
}

/// Appends `Name{Variant(field:type,...)|...};` for a reflected enum.
pub(crate) fn enum_variants(out: &mut String, name: &str, info: &regia_ecs::reflect::TypeInfo) {
    use regia_ecs::reflect::TypeInfo;
    use regia_ecs::reflect::enums::VariantInfo;
    let stable = regia_core::brand::fingerprint_type_path;
    out.push_str(name);
    out.push('{');
    if let TypeInfo::Enum(info) = info {
        for variant in info.iter() {
            out.push_str(variant.name());
            if let VariantInfo::Struct(fields) = variant {
                out.push('(');
                for field in fields.iter() {
                    out.push_str(&format!("{}:{},", field.name(), stable(field.type_path())));
                }
                out.push(')');
            }
            out.push('|');
        }
    }
    out.push_str("};");
}
