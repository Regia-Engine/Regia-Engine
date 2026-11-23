// SPDX-License-Identifier: MPL-2.0
//! Exposure zones: volumes that ask the eye for an EV range.
//!
//! A zone is a box or a capsule on an entity. Its shape is in local space; the
//! entity's [`regia_scene::GlobalTransform`] places it in the
//! world. [`zone_weight`] fades the zone in across [`ExposureZone::blend_distance`]
//! outside the surface, so a tunnel mouth blends before the camera crosses the
//! portal. [`combine_zones`] turns the weights at one point into one
//! [`EvTarget`]. [`adapt_ev`] walks the current exposure toward that target and
//! cannot step past it.
//!
//! The renderer samples them (ADR-322). Each frame the framework system
//! (`regia_framework::exposure_zones`) combines the zones at the active
//! camera and hands the result to `FrameRenderer::set_exposure_target`. The
//! main view's auto exposure then runs through [`ZoneExposure`]: the metered EV
//! is clamped into the target range and [`adapt_ev`] walks the eye toward it.
//! With no target the frame's exposure is the auto exposure it always was.
//!
//! # Metering inside a zone
//!
//! The eye chases `clamp(metered, ev_min - compensation, ev_max - compensation)`
//! ([`zone_goal_ev`]). The metered EV is the newest histogram on its own (not the
//! smoothed meter), after the auto-exposure band clamp and the bright-subject
//! bias. A bright window inside a tunnel still meters, but it cannot pull the
//! eye out of the zone's range. Exposure compensation shifts the result the same
//! way it does outside a zone. A zone may ask for an EV outside the
//! auto-exposure band: its clamp comes after the band's.
//!
//! Outside every zone's surface, [`combine_zones`] blends the zone with
//! [`ZONE_OPEN_RANGE`], the default auto-exposure band, so the range narrows
//! continuously across the mouth.
//!
//! # Distance
//!
//! Signed distance is positive outside the shape and negative inside, in meters.
//! The box formula and the Y-aligned capsule are Inigo Quilez's public-domain
//! distance functions (iquilezles.org).
//! A rigid transform, or a uniform scale, preserves that distance (a uniform
//! scale multiplies it). A box with a different scale on each axis is exact: the
//! half extents stretch with the entity axes. A capsule is exact when the local
//! X and Z scales match. A sheared matrix is orthogonalized first; entity
//! transforms are translation, rotation and scale, so the shear path is only a
//! guard.
//!
//! # Weight
//!
//! The blend band sits *outside* the surface. At [`ExposureZone::blend_distance`]
//! meters outside, the weight is 0. On the surface, and everywhere inside, the
//! weight is 1. Between those, the weight is a Hermite smoothstep. A tunnel a
//! few meters across can still fade across a 25 m mouth: the band does not have
//! to fit in the tube. A blend distance of 0 (or any non-finite value) is a hard
//! test: weight 1 for a distance of 0 or less, otherwise 0.
//!
//! # Combining and adapting
//!
//! Higher [`ExposureZone::priority`] wins wherever its weight is positive. Equal
//! priorities weight-blend. The weights are saturated to 1 and, when they sum to
//! less than 1, the remainder keeps `default`, which is how a single mouth mixes
//! sun and tube. Without a meter (the ADR-257 drive) the chased exposure is the
//! midpoint of the combined range; the renderer clamps its meter into the range
//! instead ([`zone_goal_ev`]).
//!
//! [`adapt_ev`] is one step of `dy/dt = speed * (target - y)`:
//! `current + (target - current) * (1 - exp(-speed * dt))`. `dt` is clamped to
//! the closed interval from 0 to [`ADAPT_DT_CAP_SECONDS`] seconds. The factor
//! stays in 0..=1, so the step lands on the segment between `current` and
//! `target`. Brightening uses [`ExposureZone::adapt_up_speed`]; darkening uses
//! [`ExposureZone::adapt_down_speed`]. A non-positive speed holds.
//!
//! See `book/agents/exposure-zones.md`, ADR-257 and ADR-322.

use std::borrow::Borrow;

use regia_app::prelude::*;
use regia_core::math::{Mat3, Vec3};
use regia_scene::GlobalTransform;

/// Longest step [`adapt_ev`] will take, in seconds.
///
/// A hitch longer than this adapts as if only this much time had passed, so one
/// stalled frame cannot finish a multi-stop move on its own.
pub const ADAPT_DT_CAP_SECONDS: f32 = 0.1;

/// EV per second used when no zone supplies a brightening speed.
///
/// The scripted tunnel drive uses this same rate. At 30 fps a 6 EV gap then
/// moves at most about 0.39 EV in one frame, under [`MAX_TUNNEL_FRAME_DELTA_EV`].
pub const DEFAULT_ADAPT_UP_SPEED: f32 = 2.0;

/// EV per second used when no zone supplies a darkening speed.
///
/// Darkening is slower than [`DEFAULT_ADAPT_UP_SPEED`]: the eye accepts light
/// faster than it accepts a tube.
pub const DEFAULT_ADAPT_DOWN_SPEED: f32 = 1.5;

/// Largest |ΔEV| the scripted 30 fps tunnel drive allows between samples.
///
/// This is the acceptance bound for that drive, not a clamp inside [`adapt_ev`].
/// With [`DEFAULT_ADAPT_UP_SPEED`] and a 6 EV sun-to-tube gap, one steady frame
/// moves at most about 0.39 EV. A step past 0.5 EV would be a flash.
pub const MAX_TUNNEL_FRAME_DELTA_EV: f32 = 0.5;

/// Sample capacity of [`ExposureLog::default`].
///
/// 4096 samples is a little over two minutes at 30 fps. Past the cap the oldest
/// sample is dropped.
pub const EXPOSURE_LOG_DEFAULT_CAP: usize = 4096;

/// Largest |EV − goal| at which the eye counts as settled, in EV.
///
/// The default tolerance of the agent's `exposure_settled` predicate and of
/// [`ExposureReport::settled`]. A twentieth of a stop is below what the eye sees
/// on a still frame.
pub const EXPOSURE_SETTLED_TOLERANCE_EV: f32 = 0.05;

/// Distance, in EV, at which an eye released by its zone hands back to the meter.
///
/// When the target clears, [`ZoneExposure`] keeps walking the eye toward the
/// smoothed meter with the default speeds. Once it is this close, the frame
/// returns to plain auto exposure, so the hand-back step is at most this large.
pub const ZONE_RELEASE_TOLERANCE_EV: f32 = 0.05;

/// The range a zone blends with outside its surface.
///
/// This is the default auto-exposure band (`PostSettings::auto_exposure_min_ev100`
/// and `auto_exposure_max_ev100`, EV100 −2 to 15). At weight 0 the combined
/// range is this band and the clamp leaves a metered frame alone. At weight 1 it
/// is the zone's own range.
pub const ZONE_OPEN_RANGE: EvRange = EvRange {
    ev_min: -2.0,
    ev_max: 15.0,
};

/// Distance returned when the transform or the shape cannot be measured.
///
/// Large and positive, so [`zone_weight`] treats the point as outside.
const FAR_OUTSIDE_M: f32 = 1.0e6;

/// Shortest entity axis, in meters, that still defines a zone frame.
const AXIS_EPS: f32 = 1.0e-8;

/// Local shape of an [`ExposureZone`].
///
/// Extents are in the entity's local space, before the entity scale. The world
/// distance functions stretch them by the scale.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ExposureZoneShape {
    /// Box centered on the entity. `half_extents` are along local X, Y and Z.
    Box {
        /// Half size on each local axis, in meters. A negative component is
        /// treated as its absolute value. Zero is a flat face (or a point, when
        /// every axis is zero).
        half_extents: [f32; 3],
    },
    /// Capsule along local Y.
    ///
    /// `half_height` is half the length of the cylindrical segment: the end-cap
    /// spheres sit at `±half_height` on local Y, and each cap adds `radius` past
    /// that. The total length along Y is `2 * (half_height + radius)`.
    Capsule {
        /// Tube radius, in meters. A negative value is treated as its absolute
        /// value. Zero is a line segment of length `2 * half_height`.
        radius: f32,
        /// Half the inner segment, in meters. A negative value is treated as its
        /// absolute value. Zero is a sphere of `radius`.
        half_height: f32,
    },
}

/// A volume that wants the camera exposure inside an EV range.
///
/// Pair it with a `regia_scene::GlobalTransform`. The shape is local; the
/// transform is the world placement. Several zones may overlap: [`combine_zones`]
/// keeps the highest [`Self::priority`] and weight-blends the rest of the mix
/// with the outdoor range.
///
/// The default is a 1 m box at EV 14.5, a hard edge, the default adaptation
/// speeds and priority 0.
#[derive(Component, Clone, Copy, Debug, PartialEq)]
pub struct ExposureZone {
    /// Local box or capsule.
    pub shape: ExposureZoneShape,
    /// Low end of the EV range this zone wants.
    ///
    /// The renderer keeps the eye inside `ev_min..=ev_max` by clamping its meter
    /// into it ([`zone_goal_ev`]); [`EvTarget::target_ev`] is the midpoint. The two
    /// ends may be equal when the zone wants one exposure.
    pub ev_min: f32,
    /// High end of the EV range this zone wants.
    pub ev_max: f32,
    /// Meters outside the surface across which [`zone_weight`] rises from 0 to 1.
    ///
    /// Zero is a hard boundary. A negative or non-finite value is also a hard
    /// boundary.
    pub blend_distance: f32,
    /// EV per second when the eye is moving toward a brighter target.
    ///
    /// A negative value is treated as a hold (speed 0) by [`combine_zones`] and
    /// [`adapt_ev`].
    pub adapt_up_speed: f32,
    /// EV per second when the eye is moving toward a darker target.
    pub adapt_down_speed: f32,
    /// Highest value wins when two zones both have a positive weight.
    ///
    /// Equal priorities are weight-blended. Any `i32` is allowed.
    pub priority: i32,
}

impl Default for ExposureZone {
    fn default() -> Self {
        Self {
            shape: ExposureZoneShape::Box {
                half_extents: [1.0, 1.0, 1.0],
            },
            ev_min: 14.5,
            ev_max: 14.5,
            blend_distance: 0.0,
            adapt_up_speed: DEFAULT_ADAPT_UP_SPEED,
            adapt_down_speed: DEFAULT_ADAPT_DOWN_SPEED,
            priority: 0,
        }
    }
}

impl ExposureZone {
    /// World signed distance from `point` to this shape, in meters.
    ///
    /// Positive is outside. See [`zone_signed_distance`].
    #[must_use]
    pub fn signed_distance(self, world_from_local: &GlobalTransform, point: [f32; 3]) -> f32 {
        zone_signed_distance(self.shape, world_from_local, point)
    }

    /// [`zone_weight`] at `point` using this zone's blend distance.
    #[must_use]
    pub fn weight_at(self, world_from_local: &GlobalTransform, point: [f32; 3]) -> f32 {
        zone_weight(self.signed_distance(world_from_local, point), self.blend_distance)
    }

    /// Finite, ordered copy of [`Self::ev_min`] and [`Self::ev_max`].
    #[must_use]
    pub fn range(self) -> EvRange {
        EvRange::new(self.ev_min, self.ev_max)
    }
}

/// An inclusive exposure-value range, in EV.
///
/// [`Self::target_ev`] is the midpoint, which is what the eye chases when there
/// is no meter (the renderer clamps its meter into the range instead).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvRange {
    /// Low end, EV. After [`Self::sanitized`], this is finite and at most `ev_max`.
    pub ev_min: f32,
    /// High end, EV.
    pub ev_max: f32,
}

impl EvRange {
    /// Ordered finite ends.
    ///
    /// A non-finite end is replaced with 0. If `ev_min` is above `ev_max`, the
    /// ends swap.
    #[must_use]
    pub fn new(ev_min: f32, ev_max: f32) -> Self {
        Self { ev_min, ev_max }.sanitized()
    }

    /// Both ends equal to `ev`. A non-finite `ev` becomes 0.
    #[must_use]
    pub fn point(ev: f32) -> Self {
        Self::new(ev, ev)
    }

    /// [`Self::new`] applied to an existing range.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let mut lo = if self.ev_min.is_finite() { self.ev_min } else { 0.0 };
        let mut hi = if self.ev_max.is_finite() { self.ev_max } else { 0.0 };
        if lo > hi {
            std::mem::swap(&mut lo, &mut hi);
        }
        Self {
            ev_min: lo,
            ev_max: hi,
        }
    }

    /// Midpoint of the sanitized range.
    #[must_use]
    pub fn target_ev(self) -> f32 {
        let range = self.sanitized();
        range.ev_min.midpoint(range.ev_max)
    }
}

/// One combined exposure target at a point.
///
/// `ev_min` and `ev_max` are finite and ordered. The speeds are finite and
/// non-negative. [`Self::target_ev`] is the midpoint; the renderer chases
/// [`zone_goal_ev`], its meter clamped into the range.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EvTarget {
    /// Low end of the combined range, EV.
    pub ev_min: f32,
    /// High end of the combined range, EV.
    pub ev_max: f32,
    /// Brightening speed, EV per second.
    pub adapt_up_speed: f32,
    /// Darkening speed, EV per second.
    pub adapt_down_speed: f32,
}

impl EvTarget {
    /// `range` plus the default adaptation speeds.
    ///
    /// This is what [`combine_zones`] returns when no zone contributes.
    #[must_use]
    pub fn from_range(range: EvRange) -> Self {
        let range = range.sanitized();
        Self {
            ev_min: range.ev_min,
            ev_max: range.ev_max,
            adapt_up_speed: DEFAULT_ADAPT_UP_SPEED,
            adapt_down_speed: DEFAULT_ADAPT_DOWN_SPEED,
        }
    }

    /// The combined range, without the speeds.
    #[must_use]
    pub fn range(self) -> EvRange {
        EvRange {
            ev_min: self.ev_min,
            ev_max: self.ev_max,
        }
    }

    /// Midpoint of [`Self::ev_min`] and [`Self::ev_max`].
    #[must_use]
    pub fn target_ev(self) -> f32 {
        self.ev_min.midpoint(self.ev_max)
    }
}

/// One row of an [`ExposureLog`]: time in seconds, the exposure, the target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureSample {
    /// Seconds from the start of the log.
    pub time: f32,
    /// Exposure after the step, EV.
    pub ev: f32,
    /// Exposure the step was chasing, EV.
    pub target: f32,
}

/// Capped log of `(time, ev, target)` samples.
///
/// [`Self::to_json`] writes a single object `{"samples":[...]}` with six digits
/// after the decimal point. The in-memory samples keep full `f32` values. A
/// non-finite sample is refused, so the JSON stays a plain number array.
#[derive(Clone, Debug, PartialEq)]
pub struct ExposureLog {
    samples: Vec<ExposureSample>,
    cap: usize,
}

impl Default for ExposureLog {
    fn default() -> Self {
        Self::new(EXPOSURE_LOG_DEFAULT_CAP)
    }
}

impl ExposureLog {
    /// A log that keeps at most `cap` samples.
    ///
    /// A cap of 0 becomes 1, so a record still has a place to land. Once the log
    /// is full, the oldest sample is dropped.
    #[must_use]
    pub fn new(cap: usize) -> Self {
        let cap = cap.max(1);
        Self {
            samples: Vec::new(),
            cap,
        }
    }

    /// Append one sample.
    ///
    /// Returns false, and stores nothing, when `time`, `ev` or `target` is
    /// non-finite.
    #[must_use]
    pub fn record(&mut self, time: f32, ev: f32, target: f32) -> bool {
        if !time.is_finite() || !ev.is_finite() || !target.is_finite() {
            return false;
        }
        if self.samples.len() >= self.cap {
            self.samples.remove(0);
        }
        self.samples.push(ExposureSample { time, ev, target });
        true
    }

    /// Samples in chronological order.
    #[must_use]
    pub fn samples(&self) -> &[ExposureSample] {
        &self.samples
    }

    /// How many samples are stored.
    #[must_use]
    pub fn len(&self) -> usize {
        self.samples.len()
    }

    /// True when nothing has been recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.samples.is_empty()
    }

    /// The log as one JSON object.
    ///
    /// A non-finite number, which [`Self::record`] refuses, would be written as
    /// `null` so the text stays valid JSON.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{\"samples\":[");
        for (index, sample) in self.samples.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"time\":");
            out.push_str(&json_f32(sample.time));
            out.push_str(",\"ev\":");
            out.push_str(&json_f32(sample.ev));
            out.push_str(",\"target\":");
            out.push_str(&json_f32(sample.target));
            out.push('}');
        }
        out.push_str("]}");
        out
    }
}

/// Signed distance from `point` to `shape` placed by `world_from_local`.
///
/// Positive is outside, in meters. A non-finite point, a non-finite shape, or a
/// transform with a vanished axis returns a large positive distance
/// ([`zone_weight`] then returns 0 for a positive blend).
///
/// ```
/// use regia_render::{zone_weight, ExposureZoneShape};
/// use regia_scene::GlobalTransform;
///
/// let shape = ExposureZoneShape::Box { half_extents: [1.0, 1.0, 1.0] };
/// let outside = regia_render::zone_signed_distance(
///     shape,
///     &GlobalTransform::IDENTITY,
///     [4.0, 0.0, 0.0],
/// );
/// assert!(outside > 0.0);
/// assert!(zone_weight(outside, 1.0) == 0.0);
/// ```
#[must_use]
pub fn zone_signed_distance(
    shape: ExposureZoneShape,
    world_from_local: &GlobalTransform,
    point: [f32; 3],
) -> f32 {
    let point = Vec3::from_array(point);
    if !point.is_finite() {
        return FAR_OUTSIDE_M;
    }
    let Some(frame) = world_frame(world_from_local) else {
        return FAR_OUTSIDE_M;
    };
    let local = frame.axes.transpose() * (point - frame.translation);
    let distance = match shape {
        ExposureZoneShape::Box { half_extents } => {
            let half = Vec3::from_array(half_extents).abs() * frame.scale;
            if !half.is_finite() {
                return FAR_OUTSIDE_M;
            }
            sd_box(local, half)
        }
        ExposureZoneShape::Capsule { radius, half_height } => {
            if !radius.is_finite() || !half_height.is_finite() {
                return FAR_OUTSIDE_M;
            }
            let radial = 0.5 * (frame.scale.x + frame.scale.z);
            let half = half_height.abs() * frame.scale.y;
            let tube = radius.abs() * radial;
            if !half.is_finite() || !tube.is_finite() {
                return FAR_OUTSIDE_M;
            }
            sd_capsule_y(local, half, tube)
        }
    };
    if distance.is_finite() {
        distance
    } else {
        FAR_OUTSIDE_M
    }
}

/// Weight of a zone at a signed distance.
///
/// `signed_distance` is positive outside. For a positive finite `blend_distance`
/// the weight is 0 at that distance outside, 1 on the surface and inside, and a
/// Hermite smoothstep between them. A blend of 0 or a non-finite blend is a hard
/// inside test (1 when `signed_distance <= 0`). A non-finite distance weighs 0.
///
/// ```
/// use regia_render::zone_weight;
///
/// assert!(zone_weight(30.0, 25.0) == 0.0);
/// assert!(zone_weight(0.0, 25.0) == 1.0);
/// assert!(zone_weight(-2.0, 25.0) == 1.0);
/// ```
#[must_use]
pub fn zone_weight(signed_distance: f32, blend_distance: f32) -> f32 {
    if !signed_distance.is_finite() {
        return 0.0;
    }
    if !blend_distance.is_finite() || blend_distance <= 0.0 {
        return if signed_distance <= 0.0 { 1.0 } else { 0.0 };
    }
    if signed_distance <= 0.0 {
        return 1.0;
    }
    if signed_distance >= blend_distance {
        return 0.0;
    }
    hermite(1.0 - signed_distance / blend_distance)
}

/// Blend the zones that cover one point.
///
/// Each entry is `(weight, zone)`. A weight that is non-finite or not strictly
/// positive is ignored, and so is a zone with a non-finite EV or speed. Among
/// the rest, only the highest [`ExposureZone::priority`] participates. Those
/// winners are weight-blended: each weight is saturated to 1, and when the
/// saturated weights sum to less than 1 the remainder is `default`. Speeds blend
/// the same way, with [`DEFAULT_ADAPT_UP_SPEED`] and [`DEFAULT_ADAPT_DOWN_SPEED`]
/// on the default's share. Negative zone speeds contribute 0.
///
/// An empty list, or a list with nothing usable, returns `default` (sanitized)
/// and the default speeds. The result is always finite. A non-finite `default`
/// becomes the zero range.
#[must_use]
pub fn combine_zones(zones: &[(f32, &ExposureZone)], default: EvRange) -> EvTarget {
    combine(zones, default)
}

/// [`combine_zones`] over zones held by value.
///
/// Same rules and result. A per-frame caller keeps one `Vec<(f32, ExposureZone)>`
/// and clears it each frame, so gathering the zones allocates nothing once the
/// vector has grown.
#[must_use]
pub fn combine_weighted_zones(zones: &[(f32, ExposureZone)], default: EvRange) -> EvTarget {
    combine(zones, default)
}

fn combine<Z: Borrow<ExposureZone>>(zones: &[(f32, Z)], default: EvRange) -> EvTarget {
    let fallback = EvTarget::from_range(default);
    let Some(priority) = winning_priority(zones) else {
        return fallback;
    };
    let mut ev_min = 0.0;
    let mut ev_max = 0.0;
    let mut adapt_up = 0.0;
    let mut adapt_down = 0.0;
    let mut sum = 0.0;
    for (weight, zone) in zones {
        let (weight, zone): (f32, &ExposureZone) = (*weight, zone.borrow());
        if zone.priority != priority || !contributes(weight, zone) {
            continue;
        }
        let w = weight.clamp(0.0, 1.0);
        let range = zone.range();
        ev_min += w * range.ev_min;
        ev_max += w * range.ev_max;
        adapt_up += w * zone.adapt_up_speed.max(0.0);
        adapt_down += w * zone.adapt_down_speed.max(0.0);
        sum += w;
    }
    if !sum.is_finite() || sum <= 0.0 {
        return fallback;
    }
    let (ev_min, ev_max, adapt_up, adapt_down) = if sum > 1.0 {
        let inv = 1.0 / sum;
        (ev_min * inv, ev_max * inv, adapt_up * inv, adapt_down * inv)
    } else {
        let rest = 1.0 - sum;
        (
            ev_min + rest * fallback.ev_min,
            ev_max + rest * fallback.ev_max,
            adapt_up + rest * DEFAULT_ADAPT_UP_SPEED,
            adapt_down + rest * DEFAULT_ADAPT_DOWN_SPEED,
        )
    };
    finite_target(ev_min, ev_max, adapt_up, adapt_down, fallback)
}

/// One eye-adaptation step, in EV.
///
/// Moves `current` toward `target`. Brightening (`target > current`) uses
/// `up_speed`; darkening uses `down_speed`. Both speeds are EV per second. The
/// step is `current + (target - current) * (1 - exp(-speed * dt))` with `dt`
/// clamped to the closed interval from 0 to [`ADAPT_DT_CAP_SECONDS`] seconds.
/// The factor is clamped to 0..=1, so the result lies between `current` and
/// `target`. It may land on `target`. It does not cross `target`.
///
/// A non-positive speed holds at `current`. A negative `dt` is treated as 0, so
/// it also holds. If any argument is non-finite, the function returns `current`
/// unchanged, including when `current` itself is non-finite.
#[must_use]
pub fn adapt_ev(current: f32, target: f32, dt: f32, up_speed: f32, down_speed: f32) -> f32 {
    if !all_finite(&[current, target, dt, up_speed, down_speed]) {
        return current;
    }
    let dt = dt.clamp(0.0, ADAPT_DT_CAP_SECONDS);
    let speed = if target > current { up_speed } else { down_speed };
    if speed <= 0.0 || dt == 0.0 {
        return current;
    }
    let factor = 1.0 - (-speed * dt).exp();
    let factor = if factor.is_finite() {
        factor.clamp(0.0, 1.0)
    } else {
        0.0
    };
    let next = current + (target - current) * factor;
    if next.is_finite() { next } else { current }
}

/// The EV100 the eye chases inside a zone: `metered_ev` clamped into the
/// target range.
///
/// `metered_ev` already has `compensation` subtracted (it is the EV100 auto
/// exposure would apply), so the range shifts by the same amount:
/// `clamp(metered_ev, ev_min - compensation, ev_max - compensation)`. A
/// non-finite `metered_ev` returns the range's midpoint less `compensation`, and
/// a non-finite `compensation` counts as 0.
///
/// ```
/// use regia_render::exposure_zones::{EvRange, EvTarget, zone_goal_ev};
///
/// let tube = EvTarget::from_range(EvRange::point(8.5));
/// // A dark tube meters at EV 5; the zone holds the eye at 8.5.
/// assert!(zone_goal_ev(5.0, tube, 0.0) == 8.5);
/// // A bright window meters at EV 12; it cannot pull the eye out either.
/// assert!(zone_goal_ev(12.0, tube, 0.0) == 8.5);
/// let wide = EvTarget::from_range(EvRange::new(7.0, 10.0));
/// assert!(zone_goal_ev(9.0, wide, 0.0) == 9.0);
/// ```
#[must_use]
pub fn zone_goal_ev(metered_ev: f32, target: EvTarget, compensation: f32) -> f32 {
    let compensation = if compensation.is_finite() {
        compensation
    } else {
        0.0
    };
    let range = target.range().sanitized();
    let lo = range.ev_min - compensation;
    let hi = range.ev_max - compensation;
    if metered_ev.is_finite() {
        metered_ev.clamp(lo, hi)
    } else {
        lo.midpoint(hi)
    }
}

/// Steps of [`adapt_ev`] that bring a gap of `gap_ev` within `tolerance_ev` of a
/// fixed goal.
///
/// Each step keeps `exp(-speed * dt)` of the gap, with `dt` capped at
/// [`ADAPT_DT_CAP_SECONDS`], so the answer is
/// `ceil(ln(gap / tolerance) / (speed * dt))`, and 0 when the gap is already
/// within the tolerance. `None` when the speed, `dt` or the tolerance is not
/// positive and finite (the eye never gets there), or the count overflows `u32`.
///
/// The darkening leg of the DO27 tunnel (6 EV at 1.5 EV/s and 30 fps, to within
/// [`EXPOSURE_SETTLED_TOLERANCE_EV`]) is 96 steps, 3.2 s.
///
/// ```
/// use regia_render::exposure_zones::adapt_settle_steps;
///
/// assert_eq!(adapt_settle_steps(6.0, 0.05, 1.5, 1.0 / 30.0), Some(96));
/// assert_eq!(adapt_settle_steps(0.01, 0.05, 1.5, 1.0 / 30.0), Some(0));
/// assert_eq!(adapt_settle_steps(6.0, 0.05, 0.0, 1.0 / 30.0), None);
/// ```
#[must_use]
pub fn adapt_settle_steps(gap_ev: f32, tolerance_ev: f32, speed: f32, dt: f32) -> Option<u32> {
    if !all_finite(&[gap_ev, tolerance_ev, speed, dt]) || tolerance_ev <= 0.0 || speed <= 0.0 || dt <= 0.0 {
        return None;
    }
    let gap = f64::from(gap_ev.abs());
    let tolerance = f64::from(tolerance_ev);
    if gap <= tolerance {
        return Some(0);
    }
    let per_step = f64::from(speed) * f64::from(dt.min(ADAPT_DT_CAP_SECONDS));
    let steps = ((gap / tolerance).ln() / per_step).ceil();
    if steps.is_finite() && steps <= f64::from(u32::MAX) {
        Some(steps as u32)
    } else {
        None
    }
}

/// What drove the exposure of the last main-view frame ([`ExposureReport::mode`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ZoneExposureMode {
    /// Plain auto exposure: no zone target and no eye to hand back.
    Meter,
    /// A zone target is set. The eye follows [`adapt_ev`] toward [`zone_goal_ev`].
    Zone,
    /// The target cleared and the eye is walking back to the smoothed meter.
    Release,
    /// Auto exposure is off for the view. Zones are ignored.
    Manual,
}

impl ZoneExposureMode {
    /// Stable lowercase name, as the agent protocol reports it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Meter => "meter",
            Self::Zone => "zone",
            Self::Release => "release",
            Self::Manual => "manual",
        }
    }
}

/// One main-view frame's exposure, from [`ZoneExposure::report`].
///
/// All values are EV100 with exposure compensation applied, the units the post
/// stack exposes with.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ExposureReport {
    /// EV100 the frame was exposed at.
    pub ev: f32,
    /// EV100 the eye was chasing. In [`ZoneExposureMode::Meter`] mode this is the
    /// newest histogram's EV, which the smoothed meter is heading for. In
    /// [`ZoneExposureMode::Manual`] mode it equals [`Self::ev`].
    pub goal: f32,
    /// EV100 plain auto exposure would have applied (the smoothed meter).
    pub meter_ev: f32,
    /// What drove the frame.
    pub mode: ZoneExposureMode,
    /// The zone target in effect, if any.
    pub target: Option<EvTarget>,
}

impl ExposureReport {
    /// |[`Self::ev`] − [`Self::goal`]| is at most `tolerance_ev`.
    #[must_use]
    pub fn settled(&self, tolerance_ev: f32) -> bool {
        (self.ev - self.goal).abs() <= tolerance_ev
    }

    /// A zone (or its release) is still moving the eye by more than
    /// [`EXPOSURE_SETTLED_TOLERANCE_EV`].
    #[must_use]
    pub fn adapting(&self) -> bool {
        matches!(self.mode, ZoneExposureMode::Zone | ZoneExposureMode::Release)
            && !self.settled(EXPOSURE_SETTLED_TOLERANCE_EV)
    }
}

/// The auto-exposure readings [`ZoneExposure::step`] works from, EV100 with
/// compensation applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MeterReading {
    /// What plain auto exposure applies this frame: the smoothed meter, or the
    /// initial EV before the first histogram.
    pub ev: f32,
    /// The newest histogram alone, unsmoothed. Equal to [`Self::ev`] when there is
    /// no histogram yet.
    pub latest_ev: f32,
    /// The view's exposure compensation (already subtracted from both values).
    pub compensation: f32,
}

impl MeterReading {
    /// A reading whose smoothed and newest values are both `ev`, without
    /// compensation (tests and synthetic drives).
    #[must_use]
    pub fn steady(ev: f32) -> Self {
        Self {
            ev,
            latest_ev: ev,
            compensation: 0.0,
        }
    }
}

/// The main view's zone-driven eye.
///
/// `FrameRenderer` owns one. The host sets the target each frame
/// ([`Self::set_target`], through `FrameRenderer::set_exposure_target`), and the
/// post stack calls [`Self::apply`] with the frame's meter. Without a target,
/// and with no eye left to hand back, `apply` returns the meter's EV untouched,
/// so the frame is exactly the plain auto-exposure frame.
///
/// With a target, the eye starts from the EV the frame was about to apply (no
/// jump when the camera enters a blend band) and takes one [`adapt_ev`] step
/// toward [`zone_goal_ev`] with the target's speeds. When the target clears, the
/// eye walks back toward the smoothed meter with the default speeds and hands
/// over once it is within [`ZONE_RELEASE_TOLERANCE_EV`]. A camera cut that snaps
/// exposure starts the next zone frame on its goal.
///
/// `dt` comes from [`Self::set_frame_dt`] when the host gave one for this frame,
/// otherwise it is 0 and the eye holds: a host that never sets a step (no `Time`
/// resource, a bare `FrameRenderer`) gets a deterministic hold, not wall-clock
/// adaptation (audit G, render5 determinism).
///
/// The opt-in log ([`Self::start_log`]) records `(time, ev, goal)` for every
/// main-view frame, zone or not. Its time is the sum of the frame steps since the
/// log started.
#[derive(Clone, Debug, Default)]
pub struct ZoneExposure {
    target: Option<EvTarget>,
    host_dt: Option<f32>,
    eye: Option<f32>,
    snap: bool,
    report: Option<ExposureReport>,
    log: Option<ExposureLog>,
    logging: bool,
    log_time: Option<f32>,
    /// A frame without a host step holds at dt 0 ([`Self::set_hold_on_missing_dt`]).
    hold_on_missing_dt: bool,
}

impl ZoneExposure {
    /// Sets (or with `None`, clears) the zone target for the next frames.
    ///
    /// A target with a non-finite value is sanitized: the range becomes finite
    /// and ordered and a non-finite speed holds (0).
    pub fn set_target(&mut self, target: Option<EvTarget>) {
        self.target = target.map(sanitize_target);
    }

    /// The zone target in effect.
    #[must_use]
    pub fn target(&self) -> Option<EvTarget> {
        self.target
    }

    /// The host's step for the next main-view frame, in seconds.
    ///
    /// Consumed by that frame. `None`, or a frame without a call, holds the eye
    /// for that frame (step 0): measuring the wall clock instead made a replay
    /// that presents the same sim frames at a different speed adapt to a
    /// different exposure (audit G, render5 determinism). A negative or
    /// non-finite value holds the eye too.
    pub fn set_frame_dt(&mut self, seconds: Option<f32>) {
        self.host_dt = seconds;
    }

    /// Records the explicit hold flag used by complete-frame capture.
    ///
    /// Frames without a host step always hold at `dt` 0, including interactive
    /// frames. This compatibility flag remains available for a capture to save
    /// and restore its caller's state; clearing it does not enable wall-clock
    /// adaptation. An explicit [`Self::set_frame_dt`] step adapts either way.
    pub fn set_hold_on_missing_dt(&mut self, hold: bool) {
        self.hold_on_missing_dt = hold;
    }

    /// The explicit compatibility hold flag recorded by [`Self::set_hold_on_missing_dt`].
    /// Frames without a host step also hold when this flag is false.
    #[must_use]
    pub fn holds_on_missing_dt(&self) -> bool {
        self.hold_on_missing_dt
    }

    /// The exposure of the last main-view frame; `None` before the first.
    #[must_use]
    pub fn report(&self) -> Option<ExposureReport> {
        self.report
    }

    /// The eye is under zone control: a target is set or it is still handing back.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.target.is_some() || self.eye.is_some()
    }

    /// Camera cut with a snapped exposure: the eye is dropped and the next zone
    /// frame starts on its goal.
    pub fn camera_cut(&mut self) {
        self.eye = None;
        self.snap = true;
    }

    /// Starts a fresh log of at most `cap` samples (0 becomes 1), dropping any
    /// earlier one.
    pub fn start_log(&mut self, cap: usize) {
        let cap = cap.max(1);
        self.log = Some(ExposureLog {
            samples: Vec::with_capacity(cap.min(EXPOSURE_LOG_DEFAULT_CAP)),
            cap,
        });
        self.logging = true;
        self.log_time = None;
    }

    /// Stops recording. The samples stay readable through [`Self::log`].
    pub fn stop_log(&mut self) {
        self.logging = false;
    }

    /// Frames are being recorded.
    #[must_use]
    pub fn is_logging(&self) -> bool {
        self.logging
    }

    /// The current or last log; `None` when no log was started.
    #[must_use]
    pub fn log(&self) -> Option<&ExposureLog> {
        self.log.as_ref()
    }

    /// One auto-exposure frame of the main view at the wall-clock (or host) step.
    ///
    /// Returns the EV100 to expose with. With no target, no eye to hand back and
    /// no log running, this returns `meter.ev` without reading the clock.
    pub fn apply(&mut self, meter: MeterReading) -> f32 {
        let dt = self.frame_dt();
        self.step(meter, dt)
    }

    /// [`Self::manual_frame`] at the wall-clock (or host) step.
    pub fn apply_manual(&mut self, ev: f32) {
        let dt = self.frame_dt();
        self.manual_frame(ev, dt);
    }

    /// The host's step when it gave one, else 0 (the eye and the log hold for
    /// that frame — never the wall clock: audit G, render5 determinism).
    fn frame_dt(&mut self) -> f32 {
        self.host_dt.take().unwrap_or(0.0)
    }

    /// One auto-exposure frame at an explicit step of `dt` seconds.
    ///
    /// Deterministic: the drive tests call this with a fixed `dt`. See the type
    /// docs for the rules.
    pub fn step(&mut self, meter: MeterReading, dt: f32) -> f32 {
        let snap = std::mem::take(&mut self.snap);
        let (ev, goal, mode) = match (self.target, self.eye) {
            (Some(target), eye) => {
                let goal = zone_goal_ev(meter.latest_ev, target, meter.compensation);
                let start = match eye {
                    _ if snap => goal,
                    Some(eye) => eye,
                    None => meter.ev,
                };
                let next = adapt_ev(start, goal, dt, target.adapt_up_speed, target.adapt_down_speed);
                if next.is_finite() {
                    self.eye = Some(next);
                    (next, goal, ZoneExposureMode::Zone)
                } else {
                    self.eye = None;
                    (meter.ev, meter.ev, ZoneExposureMode::Meter)
                }
            }
            (None, Some(eye)) => {
                let next = adapt_ev(
                    eye,
                    meter.ev,
                    dt,
                    DEFAULT_ADAPT_UP_SPEED,
                    DEFAULT_ADAPT_DOWN_SPEED,
                );
                if next.is_finite() && (next - meter.ev).abs() > ZONE_RELEASE_TOLERANCE_EV {
                    self.eye = Some(next);
                    (next, meter.ev, ZoneExposureMode::Release)
                } else {
                    self.eye = None;
                    (meter.ev, meter.ev, ZoneExposureMode::Meter)
                }
            }
            (None, None) => (meter.ev, meter.latest_ev, ZoneExposureMode::Meter),
        };
        self.finish(ev, goal, meter.ev, mode, dt);
        ev
    }

    /// A frame of the main view with auto exposure off, exposed at `ev`.
    ///
    /// Zones are ignored and the eye is dropped, so turning auto exposure back
    /// on starts from the meter.
    pub fn manual_frame(&mut self, ev: f32, dt: f32) {
        self.eye = None;
        self.snap = false;
        self.finish(ev, ev, ev, ZoneExposureMode::Manual, dt);
    }

    fn finish(&mut self, ev: f32, goal: f32, meter_ev: f32, mode: ZoneExposureMode, dt: f32) {
        self.report = Some(ExposureReport {
            ev,
            goal,
            meter_ev,
            mode,
            target: self.target,
        });
        if !self.logging {
            return;
        }
        let step = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        let time = self.log_time.map_or(0.0, |time| time + step);
        self.log_time = Some(time);
        if let Some(log) = self.log.as_mut() {
            // A non-finite sample is refused by the log; nothing else to do with it.
            let _recorded = log.record(time, ev, goal);
        }
    }
}

fn sanitize_target(target: EvTarget) -> EvTarget {
    let range = target.range().sanitized();
    let speed = |s: f32| if s.is_finite() { s.max(0.0) } else { 0.0 };
    EvTarget {
        ev_min: range.ev_min,
        ev_max: range.ev_max,
        adapt_up_speed: speed(target.adapt_up_speed),
        adapt_down_speed: speed(target.adapt_down_speed),
    }
}

/// Orthonormal entity axes (columns) and the positive scale that was removed.
struct WorldFrame {
    axes: Mat3,
    scale: Vec3,
    translation: Vec3,
}

fn world_frame(world_from_local: &GlobalTransform) -> Option<WorldFrame> {
    let affine = world_from_local.0;
    let translation = Vec3::from(affine.translation);
    if !translation.is_finite() {
        return None;
    }
    let (axes, scale) = orthonormal_axes(
        Vec3::from(affine.matrix3.x_axis),
        Vec3::from(affine.matrix3.y_axis),
        Vec3::from(affine.matrix3.z_axis),
    )?;
    Some(WorldFrame {
        axes,
        scale,
        translation,
    })
}

/// Gram-Schmidt. A translation-rotation-scale matrix is already orthogonal, so
/// the axes stay put and `scale` is the column lengths.
fn orthonormal_axes(x_axis: Vec3, y_axis: Vec3, z_axis: Vec3) -> Option<(Mat3, Vec3)> {
    let sx = axis_len(x_axis)?;
    let rx = x_axis / sx;
    let y_orth = y_axis - rx * y_axis.dot(rx);
    let sy = axis_len(y_orth)?;
    let ry = y_orth / sy;
    let z_orth = z_axis - rx * z_axis.dot(rx) - ry * z_axis.dot(ry);
    let sz = axis_len(z_orth)?;
    let rz = z_orth / sz;
    if !rx.is_finite() || !ry.is_finite() || !rz.is_finite() {
        return None;
    }
    Some((Mat3::from_cols(rx, ry, rz), Vec3::new(sx, sy, sz)))
}

fn axis_len(axis: Vec3) -> Option<f32> {
    let len = axis.length();
    if len.is_finite() && len >= AXIS_EPS {
        Some(len)
    } else {
        None
    }
}

/// Inigo Quilez's box: `length(max(q, 0)) + min(max(q.x, q.y, q.z), 0)`.
fn sd_box(point: Vec3, half: Vec3) -> f32 {
    let q = point.abs() - half.abs();
    q.max(Vec3::ZERO).length() + q.max_element().min(0.0)
}

/// Capsule on Y. `half_height` is the inner segment; the caps add `radius`.
fn sd_capsule_y(point: Vec3, half_height: f32, radius: f32) -> f32 {
    let along = point.y.clamp(-half_height, half_height);
    (point - Vec3::new(0.0, along, 0.0)).length() - radius
}

fn hermite(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

fn all_finite(values: &[f32]) -> bool {
    values.iter().copied().all(f32::is_finite)
}

fn contributes(weight: f32, zone: &ExposureZone) -> bool {
    weight.is_finite()
        && weight > 0.0
        && all_finite(&[
            zone.ev_min,
            zone.ev_max,
            zone.adapt_up_speed,
            zone.adapt_down_speed,
        ])
}

fn winning_priority<Z: Borrow<ExposureZone>>(zones: &[(f32, Z)]) -> Option<i32> {
    let mut best: Option<i32> = None;
    for (weight, zone) in zones {
        let (weight, zone): (f32, &ExposureZone) = (*weight, zone.borrow());
        if !contributes(weight, zone) {
            continue;
        }
        best = Some(best.map_or(zone.priority, |priority| priority.max(zone.priority)));
    }
    best
}

fn finite_target(ev_min: f32, ev_max: f32, adapt_up: f32, adapt_down: f32, fallback: EvTarget) -> EvTarget {
    if !all_finite(&[ev_min, ev_max, adapt_up, adapt_down]) {
        return fallback;
    }
    let (ev_min, ev_max) = if ev_min <= ev_max {
        (ev_min, ev_max)
    } else {
        (ev_max, ev_min)
    };
    EvTarget {
        ev_min,
        ev_max,
        adapt_up_speed: adapt_up.max(0.0),
        adapt_down_speed: adapt_down.max(0.0),
    }
}

fn json_f32(value: f32) -> String {
    if value.is_finite() {
        format!("{value:.6}")
    } else {
        String::from("null")
    }
}

#[cfg(test)]
mod tests {
    use regia_core::math::{Affine3A, Quat, Vec3};
    use regia_scene::{GlobalTransform, Transform};

    use super::*;

    const SUN_EV: f32 = 14.5;
    const TUBE_EV: f32 = 8.5;
    const TUNNEL_FPS: f32 = 30.0;
    /// 40 s at 30 fps.
    const TUNNEL_FRAMES: usize = 40 * 30;
    const TUNNEL_SPEED_M_S: f32 = 20.0;
    /// 160 m before the mouth at z = -200, so the 25 m shell and both portals
    /// fall inside the 800 m drive.
    const TUNNEL_START_Z: f32 = -360.0;

    fn near(left: f32, right: f32) -> bool {
        (left - right).abs() <= 1.0e-4
    }

    fn identity() -> GlobalTransform {
        GlobalTransform::IDENTITY
    }

    fn placed(transform: Transform) -> GlobalTransform {
        GlobalTransform(transform.compute_affine())
    }

    fn zone_ev(ev: f32, priority: i32) -> ExposureZone {
        ExposureZone {
            ev_min: ev,
            ev_max: ev,
            priority,
            ..ExposureZone::default()
        }
    }

    fn tunnel_zone() -> ExposureZone {
        ExposureZone {
            shape: ExposureZoneShape::Box {
                half_extents: [8.0, 4.0, 200.0],
            },
            ev_min: TUBE_EV,
            ev_max: TUBE_EV,
            blend_distance: 25.0,
            adapt_up_speed: DEFAULT_ADAPT_UP_SPEED,
            adapt_down_speed: DEFAULT_ADAPT_DOWN_SPEED,
            priority: 1,
        }
    }

    #[test]
    fn box_distance_at_center_face_and_corner() {
        let shape = ExposureZoneShape::Box {
            half_extents: [1.0, 2.0, 3.0],
        };
        let world = identity();
        assert!(near(zone_signed_distance(shape, &world, [0.0, 0.0, 0.0]), -1.0));
        assert!(near(zone_signed_distance(shape, &world, [1.0, 0.0, 0.0]), 0.0));
        assert!(near(zone_signed_distance(shape, &world, [2.0, 0.0, 0.0]), 1.0));
        let corner = zone_signed_distance(shape, &world, [2.0, 3.0, 4.0]);
        assert!(near(corner, 3.0_f32.sqrt()));
    }

    #[test]
    fn negative_half_extents_match_the_absolute_box() {
        let world = identity();
        let point = [0.2, -0.4, 1.0];
        let positive = ExposureZoneShape::Box {
            half_extents: [1.0, 2.0, 3.0],
        };
        let negative = ExposureZoneShape::Box {
            half_extents: [-1.0, -2.0, -3.0],
        };
        assert!(near(
            zone_signed_distance(positive, &world, point),
            zone_signed_distance(negative, &world, point),
        ));
    }

    #[test]
    fn rigid_motion_preserves_distance() {
        let shape = ExposureZoneShape::Capsule {
            radius: 1.0,
            half_height: 2.0,
        };
        let local = [0.4, 3.0, -0.2];
        let local_sd = zone_signed_distance(shape, &identity(), local);
        let transform = Transform {
            translation: Vec3::new(10.0, -2.0, 4.0),
            rotation: Quat::from_rotation_y(0.7) * Quat::from_rotation_z(-0.4),
            scale: Vec3::ONE,
        };
        let world_point = transform.transform_point(Vec3::from_array(local)).to_array();
        let world_sd = zone_signed_distance(shape, &placed(transform), world_point);
        assert!(near(local_sd, world_sd), "{local_sd} vs {world_sd}");
    }

    #[test]
    fn uniform_scale_scales_box_distance() {
        let shape = ExposureZoneShape::Box {
            half_extents: [1.0, 2.0, 3.0],
        };
        let local = [4.0, -1.0, 0.5];
        let local_sd = zone_signed_distance(shape, &identity(), local);
        let scale = 2.5;
        let transform = Transform::from_scale(scale);
        let world_point = transform.transform_point(Vec3::from_array(local)).to_array();
        let world_sd = zone_signed_distance(shape, &placed(transform), world_point);
        assert!(
            near(world_sd, local_sd * scale),
            "{local_sd} * {scale} vs {world_sd}"
        );
    }

    #[test]
    fn nonuniform_box_scale_is_euclidean() {
        let shape = ExposureZoneShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        };
        let world = placed(Transform {
            translation: Vec3::ZERO,
            rotation: Quat::IDENTITY,
            scale: Vec3::new(2.0, 3.0, 4.0),
        });
        assert!(near(zone_signed_distance(shape, &world, [0.0, 0.0, 0.0]), -2.0));
        assert!(near(zone_signed_distance(shape, &world, [5.0, 0.0, 0.0]), 3.0));
    }

    #[test]
    fn capsule_distance_along_the_segment_and_the_radius() {
        let shape = ExposureZoneShape::Capsule {
            radius: 1.0,
            half_height: 2.0,
        };
        let world = identity();
        assert!(near(zone_signed_distance(shape, &world, [0.0, 0.0, 0.0]), -1.0));
        assert!(near(zone_signed_distance(shape, &world, [1.0, 0.0, 0.0]), 0.0));
        assert!(near(zone_signed_distance(shape, &world, [0.0, 5.0, 0.0]), 2.0));
        assert!(near(zone_signed_distance(shape, &world, [0.0, 2.0, 0.0]), -1.0));
    }

    #[test]
    fn zero_size_shapes_are_points_and_stay_finite() {
        let world = identity();
        let flat = ExposureZoneShape::Box {
            half_extents: [0.0, 0.0, 0.0],
        };
        assert!(near(zone_signed_distance(flat, &world, [3.0, 0.0, 0.0]), 3.0));
        assert!(near(zone_signed_distance(flat, &world, [0.0, 0.0, 0.0]), 0.0));
        let point = ExposureZoneShape::Capsule {
            radius: 0.0,
            half_height: 0.0,
        };
        assert!(near(zone_signed_distance(point, &world, [3.0, 4.0, 0.0]), 5.0));
        let at_three = zone_weight(3.0, 10.0);
        assert!(at_three.is_finite() && at_three > 0.0 && at_three < 1.0);
        assert!(zone_weight(9.0, 10.0) < at_three);
        assert!(zone_weight(0.0, 0.0) == 1.0);
        assert!(zone_weight(0.001, 0.0) == 0.0);
    }

    #[test]
    fn degenerate_transform_and_non_finite_point_are_outside() {
        let shape = ExposureZoneShape::Box {
            half_extents: [1.0, 1.0, 1.0],
        };
        let vanished = GlobalTransform(Affine3A::ZERO);
        assert!(zone_signed_distance(shape, &vanished, [0.0, 0.0, 0.0]) > 1.0e5);
        let broken = placed(Transform::from_translation(Vec3::new(f32::NAN, 0.0, 0.0)));
        assert!(zone_signed_distance(shape, &broken, [0.0, 0.0, 0.0]) > 1.0e5);
        assert!(zone_signed_distance(shape, &identity(), [f32::NAN, 0.0, 0.0]) > 1.0e5);
        assert!(zone_weight(f32::NAN, 10.0) == 0.0);
    }

    #[test]
    fn weight_fades_across_the_outside_shell() {
        assert!(zone_weight(25.0, 25.0) == 0.0);
        assert!(zone_weight(12.5, 25.0) == 0.5);
        assert!(zone_weight(0.0, 25.0) == 1.0);
        assert!(zone_weight(-4.0, 25.0) == 1.0);
        assert!(zone_weight(40.0, 25.0) == 0.0);
        assert!(zone_weight(-1.0, 0.0) == 1.0);
        assert!(zone_weight(0.1, 0.0) == 0.0);
        assert!(zone_weight(-1.0, -5.0) == 1.0);
        assert!(zone_weight(1.0, f32::INFINITY) == 0.0);
        assert!(zone_weight(-1.0, f32::NAN) == 1.0);
        let zone = ExposureZone {
            blend_distance: 25.0,
            ..ExposureZone::default()
        };
        let distance = zone.signed_distance(&identity(), [0.0, 0.0, 0.0]);
        assert!(near(
            zone.weight_at(&identity(), [0.0, 0.0, 0.0]),
            zone_weight(distance, 25.0)
        ));
    }

    #[test]
    fn empty_and_unusable_zones_return_the_default() {
        let none: &[(f32, &ExposureZone)] = &[];
        let sun = combine_zones(none, EvRange::point(SUN_EV));
        assert!(near(sun.target_ev(), SUN_EV));
        assert!(near(sun.adapt_up_speed, DEFAULT_ADAPT_UP_SPEED));
        assert!(near(sun.adapt_down_speed, DEFAULT_ADAPT_DOWN_SPEED));

        let nan_default = combine_zones(
            none,
            EvRange {
                ev_min: f32::NAN,
                ev_max: f32::NAN,
            },
        );
        assert!(near(nan_default.target_ev(), 0.0));
        assert!(nan_default.target_ev().is_finite());

        let mut broken = zone_ev(8.0, 1);
        broken.ev_max = f32::NAN;
        let ignored = combine_zones(&[(1.0, &broken)], EvRange::point(3.0));
        assert!(near(ignored.target_ev(), 3.0));

        let quiet = zone_ev(8.0, 1);
        let negative = combine_zones(&[(-1.0, &quiet), (0.0, &quiet)], EvRange::point(3.0));
        assert!(near(negative.target_ev(), 3.0));
    }

    #[test]
    fn higher_priority_wins_and_equal_priorities_blend() {
        let low = zone_ev(8.0, 0);
        let high = zone_ev(4.0, 5);
        let won = combine_zones(&[(1.0, &low), (0.5, &high)], EvRange::point(14.0));
        assert!(near(won.target_ev(), 9.0));

        let mut a = zone_ev(8.0, 1);
        let mut b = zone_ev(12.0, 1);
        a.adapt_up_speed = 4.0;
        b.adapt_up_speed = 8.0;
        let blended = combine_zones(&[(0.25, &a), (0.25, &b)], EvRange::point(16.0));
        assert!(near(blended.target_ev(), 13.0));
        let up = 0.25 * 4.0 + 0.25 * 8.0 + 0.5 * DEFAULT_ADAPT_UP_SPEED;
        assert!(near(blended.adapt_up_speed, up));

        let both = combine_zones(&[(1.0, &a), (1.0, &b)], EvRange::point(16.0));
        assert!(near(both.target_ev(), 10.0));
        assert!(near(both.adapt_up_speed, 6.0));

        let saturated = combine_zones(&[(2.0, &high)], EvRange::point(14.0));
        assert!(near(saturated.target_ev(), 4.0));

        let lower = zone_ev(1.0, -5);
        let higher = zone_ev(2.0, -1);
        let negatives = combine_zones(&[(1.0, &lower), (1.0, &higher)], EvRange::point(0.0));
        assert!(near(negatives.target_ev(), 2.0));
    }

    #[test]
    fn swapped_range_and_negative_speeds_stay_finite() {
        let ordered = EvRange::new(10.0, 6.0);
        assert!(near(ordered.ev_min, 6.0));
        assert!(near(ordered.ev_max, 10.0));
        assert!(near(ordered.target_ev(), 8.0));
        let patched = EvRange::new(f32::NAN, 4.0);
        assert!(near(patched.ev_min, 0.0));
        assert!(near(patched.ev_max, 4.0));

        let mut zone = zone_ev(0.0, 1);
        zone.ev_min = 10.0;
        zone.ev_max = 6.0;
        zone.adapt_up_speed = -4.0;
        zone.adapt_down_speed = -4.0;
        let target = combine_zones(&[(1.0, &zone)], EvRange::point(0.0));
        assert!(near(target.ev_min, 6.0));
        assert!(near(target.ev_max, 10.0));
        assert!(near(target.target_ev(), 8.0));
        assert!(near(target.adapt_up_speed, 0.0));
        assert!(near(target.adapt_down_speed, 0.0));
    }

    #[test]
    fn adapt_ev_holds_asymmetry_and_the_dt_cap() {
        assert!(adapt_ev(4.0, 9.0, 0.0, 3.0, 1.0) == 4.0);
        assert!(adapt_ev(4.0, 9.0, -0.5, 3.0, 1.0) == 4.0);
        assert!(adapt_ev(3.0, 9.0, 0.1, -5.0, 5.0) == 3.0);
        assert!(adapt_ev(9.0, 3.0, 0.1, 5.0, -5.0) == 9.0);
        assert!(adapt_ev(4.0, f32::NAN, 0.016, 1.0, 1.0) == 4.0);
        assert!(adapt_ev(4.0, 10.0, f32::INFINITY, 1.0, 1.0) == 4.0);
        assert!(adapt_ev(f32::NAN, 10.0, 0.016, 1.0, 1.0).is_nan());

        let dt = 0.05;
        let up = adapt_ev(1.0, 5.0, dt, 3.0, 0.5);
        let k_up = 1.0 - (-3.0_f32 * dt).exp();
        assert!(near(up, 1.0 + 4.0 * k_up));
        let down = adapt_ev(5.0, 1.0, dt, 3.0, 0.5);
        let k_down = 1.0 - (-0.5_f32 * dt).exp();
        assert!(near(down, 5.0 - 4.0 * k_down));
        assert!(up - 1.0 > 5.0 - down);

        let hitch = adapt_ev(0.0, 8.0, 5.0, 1.0, 1.0);
        let capped = adapt_ev(0.0, 8.0, ADAPT_DT_CAP_SECONDS, 1.0, 1.0);
        assert!(near(hitch, capped));
        let k = 1.0 - (-ADAPT_DT_CAP_SECONDS).exp();
        assert!(near(hitch, 8.0 * k));

        let landed = adapt_ev(0.0, 2.0, 0.1, 1.0e8, 1.0);
        assert!(landed <= 2.0 + 1.0e-4);
        assert!(landed >= 2.0 - 1.0e-3);
    }

    /// Determinism audit (render1, finding 4): with the hold on (complete frames),
    /// a frame without a host step keeps the eye; an explicit step still adapts by
    /// exactly the [`adapt_ev`] step on every run.
    #[test]
    fn hold_on_missing_dt_holds_the_eye_and_an_explicit_step_still_adapts() {
        let target = EvTarget::from_range(EvRange::new(8.0, 10.0));
        let meter = MeterReading::steady(5.0);
        let mut zones = ZoneExposure::default();
        zones.set_target(Some(target));
        zones.set_hold_on_missing_dt(true);
        let first = zones.apply(meter);
        let second = zones.apply(meter);
        assert_eq!(first, second, "no host step under the hold: the eye holds");
        assert_eq!(second, 5.0, "the held eye stays at the meter's EV: {second}");
        // An explicit step adapts, by exactly the adapt_ev step, every run.
        let mut stepped = ZoneExposure::default();
        stepped.set_target(Some(target));
        stepped.set_hold_on_missing_dt(true);
        stepped.set_frame_dt(Some(1.0 / 30.0));
        let a = stepped.apply(meter);
        stepped.set_frame_dt(Some(1.0 / 30.0));
        let b = stepped.apply(meter);
        let goal = zone_goal_ev(5.0, target, 0.0);
        assert_eq!(
            a,
            adapt_ev(
                5.0,
                goal,
                1.0 / 30.0,
                target.adapt_up_speed,
                target.adapt_down_speed
            )
        );
        assert_eq!(
            b,
            adapt_ev(
                a,
                goal,
                1.0 / 30.0,
                target.adapt_up_speed,
                target.adapt_down_speed
            )
        );
    }

    #[test]
    fn adapt_ev_never_overshoots_a_finite_step() {
        let currents = [-4.0, 0.0, TUBE_EV, SUN_EV];
        let targets = [-4.0, 0.0, TUBE_EV, SUN_EV, 20.0];
        let steps = [0.0, 1.0 / 30.0, ADAPT_DT_CAP_SECONDS, 1.0, -0.5];
        let speeds = [0.0, 0.5, DEFAULT_ADAPT_UP_SPEED, 100.0];
        for current in currents {
            for target in targets {
                for dt in steps {
                    for up in speeds {
                        for down in speeds {
                            let next = adapt_ev(current, target, dt, up, down);
                            assert!(next.is_finite(), "{current} -> {target} via {next}");
                            let lo = current.min(target) - 1.0e-4;
                            let hi = current.max(target) + 1.0e-4;
                            assert!(next >= lo && next <= hi, "{current} -> {target} landed on {next}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn log_drops_the_oldest_and_writes_json() {
        assert_eq!(ExposureLog::new(0).cap, 1);
        assert_eq!(ExposureLog::default().cap, EXPOSURE_LOG_DEFAULT_CAP);
        assert!(ExposureLog::new(4).is_empty());
        assert_eq!(ExposureLog::new(4).to_json(), "{\"samples\":[]}");

        let mut log = ExposureLog::new(2);
        assert!(log.record(0.0, 1.0, 1.5));
        assert!(log.record(1.0, 2.0, 2.5));
        assert!(log.record(2.0, 3.0, 3.5));
        assert_eq!(log.len(), 2);
        assert!(near(log.samples()[0].time, 1.0));
        assert!(near(log.samples()[1].ev, 3.0));
        assert!(!log.record(f32::NAN, 1.0, 1.0));
        assert!(!log.record(1.0, f32::INFINITY, 1.0));
        assert_eq!(log.len(), 2);
        let expected = concat!(
            "{\"samples\":[",
            "{\"time\":1.000000,\"ev\":2.000000,\"target\":2.500000},",
            "{\"time\":2.000000,\"ev\":3.000000,\"target\":3.500000}",
            "]}"
        );
        assert_eq!(log.to_json(), expected);
    }

    #[test]
    fn tunnel_mouth_blend_is_25_m_outside_the_face() {
        let zone = tunnel_zone();
        let world = identity();
        // Mouth plane z = -200. The eye at y = 1.5 is inside the 8 m × 4 m cross-section,
        // so the distance to the box is the distance to that plane.
        assert!(near(zone.weight_at(&world, [0.0, 1.5, -225.0]), 0.0));
        assert!(near(zone.weight_at(&world, [0.0, 1.5, -212.5]), 0.5));
        assert!(near(zone.weight_at(&world, [0.0, 1.5, -200.0]), 1.0));
        assert!(near(zone.weight_at(&world, [0.0, 1.5, -199.0]), 1.0));
    }

    /// Drive +Z at 20 m/s, 30 fps, for 40 s (1200 frames).
    ///
    /// The tube is a box from z = -200 to z = 200. Sun target EV 14.5 outside,
    /// tube target EV 8.5 inside, 25 m mouth blend. The chased EV is the
    /// combined range's midpoint, so it stays inside that range. The adapted EV
    /// stays inside the sun-to-tube envelope [8.5, 14.5]: clamping it to a
    /// collapsed instantaneous range would snap the frame to black. Adaptation
    /// is what keeps the step under [`MAX_TUNNEL_FRAME_DELTA_EV`].
    #[test]
    fn scripted_tunnel_drive_has_no_flash_or_blackout() {
        let log = drive_tunnel();
        let samples = log.samples();
        assert_eq!(samples.len(), TUNNEL_FRAMES);
        assert_eq!(log.len(), TUNNEL_FRAMES);
        assert!(near(samples[0].ev, SUN_EV));
        assert!(near(samples[0].time, 0.0));

        let worst = adapt_ev(
            0.0,
            SUN_EV - TUBE_EV,
            1.0 / TUNNEL_FPS,
            DEFAULT_ADAPT_UP_SPEED,
            DEFAULT_ADAPT_DOWN_SPEED,
        );
        assert!(worst < MAX_TUNNEL_FRAME_DELTA_EV);
        assert_steps(samples, worst);

        let end = valley_end(samples);
        assert!(end > 30, "valley at frame {end}");
        assert!(end + 30 < samples.len(), "valley at frame {end}");
        assert_non_increasing(samples, end);
        assert_non_decreasing(samples, end);

        let mut min_ev = f32::MAX;
        for sample in samples {
            min_ev = min_ev.min(sample.ev);
        }
        assert!(min_ev >= TUBE_EV - 1.0e-3, "min {min_ev}");
        assert!(min_ev < TUBE_EV + 0.05, "min {min_ev}");
        let last = samples[samples.len() - 1].ev;
        assert!(near(last, SUN_EV), "last {last}");

        // t = 18 s is the middle of the tube (z = 0).
        assert!(near(samples[540].time, 18.0));
        assert!(near(samples[540].target, TUBE_EV));
        assert!(near(samples[540].ev, TUBE_EV));

        let json = log.to_json();
        assert!(json.starts_with("{\"samples\":["));
        assert!(json.ends_with("]}"));
        assert_eq!(json.matches("\"time\":").count(), TUNNEL_FRAMES);
        assert!(!json.contains("NaN"));
        assert!(!json.contains("inf"));
    }

    fn drive_tunnel() -> ExposureLog {
        let zone = tunnel_zone();
        let world = identity();
        let sun = EvRange::point(SUN_EV);
        let mut ev = SUN_EV;
        let mut log = ExposureLog::new(TUNNEL_FRAMES);
        for frame in 0..TUNNEL_FRAMES {
            let time = frame as f32 / TUNNEL_FPS;
            let point = [0.0, 1.5, TUNNEL_START_Z + TUNNEL_SPEED_M_S * time];
            let weight = zone.weight_at(&world, point);
            let blended = combine_zones(&[(weight, &zone)], sun);
            let goal = blended.target_ev();
            assert!(
                goal + 1.0e-4 >= blended.ev_min && goal <= blended.ev_max + 1.0e-4,
                "frame {frame} target {goal} left [{}, {}]",
                blended.ev_min,
                blended.ev_max,
            );
            ev = adapt_ev(
                ev,
                goal,
                1.0 / TUNNEL_FPS,
                blended.adapt_up_speed,
                blended.adapt_down_speed,
            );
            assert!(log.record(time, ev, goal), "frame {frame} was not recorded");
        }
        log
    }

    fn assert_steps(samples: &[ExposureSample], worst: f32) {
        let envelope = (TUBE_EV - 1.0e-3)..=(SUN_EV + 1.0e-3);
        for index in 1..samples.len() {
            let prev = samples[index - 1].ev;
            let next = samples[index].ev;
            let target = samples[index].target;
            let delta = next - prev;
            assert!(
                delta.abs() <= MAX_TUNNEL_FRAME_DELTA_EV,
                "frame {index} jumped by {delta}"
            );
            assert!(
                delta.abs() <= worst + 1.0e-3,
                "frame {index} jumped by {delta}, bound {worst}"
            );
            let lo = prev.min(target) - 1.0e-3;
            let hi = prev.max(target) + 1.0e-3;
            assert!(
                next >= lo && next <= hi,
                "frame {index} ev {next} overshot {target} from {prev}"
            );
            assert!(envelope.contains(&next), "frame {index} ev {next}");
            assert!(envelope.contains(&target), "frame {index} target {target}");
            assert!(near(samples[index].time, index as f32 / TUNNEL_FPS));
        }
    }

    fn valley_end(samples: &[ExposureSample]) -> usize {
        let mut min_ev = f32::MAX;
        let mut end = 0;
        for (index, sample) in samples.iter().enumerate() {
            if sample.ev < min_ev - 1.0e-4 {
                min_ev = sample.ev;
                end = index;
            } else if (sample.ev - min_ev).abs() <= 1.0e-4 {
                end = index;
            }
        }
        end
    }

    fn assert_non_increasing(samples: &[ExposureSample], end: usize) {
        for index in 1..=end {
            let prev = samples[index - 1].ev;
            let next = samples[index].ev;
            assert!(
                next <= prev + 1.0e-4,
                "frame {index} rose from {prev} to {next} on the way in"
            );
        }
    }

    fn assert_non_decreasing(samples: &[ExposureSample], start: usize) {
        for index in start + 1..samples.len() {
            let prev = samples[index - 1].ev;
            let next = samples[index].ev;
            assert!(
                next + 1.0e-4 >= prev,
                "frame {index} fell from {prev} to {next} on the way out"
            );
        }
    }

    // --- The renderer's zone-driven eye (ADR-322) ---

    /// Synthetic meter of the zoned drive: sunlit ground outside the box, a dark
    /// bore (EV 5, well under the tube's EV 8.5) inside it.
    const METER_DARK_EV: f32 = 5.0;
    const DT: f32 = 1.0 / TUNNEL_FPS;

    fn metered_at(z: f32) -> f32 {
        if z.abs() <= 200.0 { METER_DARK_EV } else { SUN_EV }
    }

    /// What `regia_framework::exposure_zones` hands the renderer at `point`.
    fn framework_target(zone: &ExposureZone, point: [f32; 3]) -> Option<EvTarget> {
        let weight = zone.weight_at(&identity(), point);
        (weight > 0.0).then(|| combine_weighted_zones(&[(weight, *zone)], ZONE_OPEN_RANGE))
    }

    /// The renderer-side scripted drive: +Z at 20 m/s and a fixed 30 fps step from open
    /// ground, through the 25 m mouth, down the 400 m tube and back out. The meter jumps
    /// from EV 14.5 to EV 5 at the portal; the zone holds the eye at EV 8.5.
    #[test]
    fn zoned_tunnel_drive_stays_on_the_segment_and_settles() {
        let zone = tunnel_zone();
        let mut eye = ZoneExposure::default();
        eye.start_log(TUNNEL_FRAMES);
        let mut prev = SUN_EV;
        let mut entered: Option<usize> = None;
        let mut exited: Option<usize> = None;
        let settle = adapt_settle_steps(
            SUN_EV - TUBE_EV,
            EXPOSURE_SETTLED_TOLERANCE_EV,
            DEFAULT_ADAPT_DOWN_SPEED,
            DT,
        )
        .expect("finite settle bound");
        assert_eq!(settle, 96, "documented in book/agents/exposure-zones.md");
        let mut saw_zone = false;
        let mut saw_release = false;
        for frame in 0..TUNNEL_FRAMES {
            let z = TUNNEL_START_Z + TUNNEL_SPEED_M_S * frame as f32 * DT;
            let point = [0.0, 1.5, z];
            eye.set_target(framework_target(&zone, point));
            let ev = eye.step(MeterReading::steady(metered_at(z)), DT);
            let report = eye.report().expect("a report per frame");
            assert!(ev.to_bits() == report.ev.to_bits());
            let goal = report.goal;
            let lo = prev.min(goal) - 1.0e-4;
            let hi = prev.max(goal) + 1.0e-4;
            assert!(ev >= lo && ev <= hi, "frame {frame}: {ev} left [{prev}, {goal}]");
            assert!(
                (ev - prev).abs() <= MAX_TUNNEL_FRAME_DELTA_EV,
                "frame {frame} jumped {prev} -> {ev}"
            );
            assert!(
                (TUBE_EV - 1.0e-3..=SUN_EV + 1.0e-3).contains(&ev),
                "frame {frame}: {ev} left the sun-to-tube envelope (the EV 5 meter leaked through)"
            );
            match report.mode {
                ZoneExposureMode::Zone => saw_zone = true,
                ZoneExposureMode::Release => saw_release = true,
                ZoneExposureMode::Meter => {}
                ZoneExposureMode::Manual => panic!("frame {frame} reported manual"),
            }
            if z.abs() <= 200.0 {
                let first = *entered.get_or_insert(frame);
                if frame >= first + settle as usize {
                    assert!(
                        (ev - TUBE_EV).abs() <= EXPOSURE_SETTLED_TOLERANCE_EV,
                        "frame {frame}: {ev} not settled {settle} steps after the portal"
                    );
                    assert!(report.settled(EXPOSURE_SETTLED_TOLERANCE_EV));
                    assert!(!report.adapting());
                }
            } else if entered.is_some() && exited.is_none() {
                exited = Some(frame);
            }
            prev = ev;
        }
        assert!(saw_zone && saw_release, "zone {saw_zone}, release {saw_release}");
        let entered = entered.expect("the drive entered the tube");
        let exited = exited.expect("the drive left the tube");
        assert!(
            exited > entered + settle as usize,
            "the tube is long enough to settle"
        );
        let last = eye.report().expect("report");
        assert_eq!(last.mode, ZoneExposureMode::Meter, "handed back to the meter");
        assert!(
            last.ev.to_bits() == SUN_EV.to_bits(),
            "handed back exactly: {}",
            last.ev
        );
        assert!(!eye.is_active());

        let log = eye.log().expect("log");
        assert_eq!(log.len(), TUNNEL_FRAMES);
        for (index, sample) in log.samples().iter().enumerate() {
            assert!(
                // 1200 f32 additions of 1/30: the rounding stays well under 2 ms.
                (sample.time - index as f32 * DT).abs() <= 2.0e-3,
                "sample {index} at {}",
                sample.time
            );
        }
    }

    #[test]
    fn without_a_target_the_meter_passes_through_bit_for_bit() {
        let mut eye = ZoneExposure::default();
        let readings = [
            MeterReading::steady(14.5),
            MeterReading {
                ev: 3.25,
                latest_ev: 9.0,
                compensation: 0.75,
            },
            MeterReading::steady(-2.0),
            MeterReading::steady(15.0),
            MeterReading::steady(0.1 + 0.2),
            MeterReading::steady(f32::NAN),
        ];
        for (index, reading) in readings.iter().enumerate() {
            let ev = if index.is_multiple_of(2) {
                eye.step(*reading, DT)
            } else {
                eye.apply(*reading)
            };
            assert!(
                ev.to_bits() == reading.ev.to_bits(),
                "{index}: {ev} vs {}",
                reading.ev
            );
            let report = eye.report().expect("report");
            assert_eq!(report.mode, ZoneExposureMode::Meter);
            assert_eq!(report.target, None);
            assert!(!report.adapting());
        }
        assert!(!eye.is_active());
        assert!(eye.log().is_none(), "no log unless started");
    }

    #[test]
    fn manual_exposure_ignores_zones_and_drops_the_eye() {
        let tube = EvTarget::from_range(EvRange::point(TUBE_EV));
        let mut eye = ZoneExposure::default();
        eye.set_target(Some(tube));
        let first = eye.step(MeterReading::steady(SUN_EV), DT);
        assert!(first < SUN_EV, "the zone started pulling the eye down");
        eye.manual_frame(12.0, DT);
        let report = eye.report().expect("report");
        assert_eq!(report.mode, ZoneExposureMode::Manual);
        assert!(report.ev == 12.0 && report.goal == 12.0);
        assert!(!report.adapting());
        assert_eq!(
            report.target,
            Some(tube),
            "the target stays set for the next auto frame"
        );
        eye.set_target(None);
        let back = eye.step(MeterReading::steady(SUN_EV), DT);
        assert!(
            back.to_bits() == SUN_EV.to_bits(),
            "no release after a manual frame"
        );
        assert_eq!(eye.report().map(|r| r.mode), Some(ZoneExposureMode::Meter));
    }

    #[test]
    fn overlapping_zones_follow_the_priority_rule() {
        let cave = ExposureZone {
            shape: ExposureZoneShape::Box {
                half_extents: [50.0, 20.0, 50.0],
            },
            ev_min: 6.0,
            ev_max: 7.0,
            priority: 0,
            ..ExposureZone::default()
        };
        let lit_room = ExposureZone {
            shape: ExposureZoneShape::Box {
                half_extents: [5.0, 3.0, 5.0],
            },
            ev_min: 11.0,
            ev_max: 12.0,
            priority: 2,
            ..ExposureZone::default()
        };
        let world = identity();
        let in_room = [0.0, 1.0, 0.0];
        let in_cave = [30.0, 1.0, 0.0];
        let gather = |point: [f32; 3]| -> Vec<(f32, ExposureZone)> {
            [cave, lit_room]
                .into_iter()
                .map(|zone| (zone.weight_at(&world, point), zone))
                .filter(|(weight, _)| *weight > 0.0)
                .collect()
        };
        let room = combine_weighted_zones(&gather(in_room), ZONE_OPEN_RANGE);
        assert_eq!(room.range(), EvRange::new(11.0, 12.0), "priority 2 wins");
        let by_ref = combine_zones(&[(1.0, &cave), (1.0, &lit_room)], ZONE_OPEN_RANGE);
        assert_eq!(room, by_ref, "by value and by reference agree");
        assert!(zone_goal_ev(METER_DARK_EV, room, 0.0) == 11.0);
        assert!(zone_goal_ev(13.0, room, 0.0) == 12.0);
        assert!(
            zone_goal_ev(11.5, room, 0.0) == 11.5,
            "inside the range the meter rules"
        );

        let cave_only = combine_weighted_zones(&gather(in_cave), ZONE_OPEN_RANGE);
        assert_eq!(cave_only.range(), EvRange::new(6.0, 7.0));
        let mut eye = ZoneExposure::default();
        eye.set_target(Some(cave_only));
        eye.camera_cut();
        let ev = eye.step(MeterReading::steady(SUN_EV), DT);
        assert!(ev == 7.0, "a snapped cut starts on the goal: {ev}");

        let mut twin = lit_room;
        twin.priority = 0;
        twin.ev_min = 8.0;
        twin.ev_max = 8.0;
        let blended = combine_weighted_zones(&[(1.0, cave), (1.0, twin)], ZONE_OPEN_RANGE);
        assert!(
            near(blended.ev_min, 7.0) && near(blended.ev_max, 7.5),
            "{blended:?}"
        );
    }

    #[test]
    fn compensation_shifts_the_zone_range() {
        let tube = EvTarget::from_range(EvRange::new(8.0, 9.0));
        assert!(zone_goal_ev(5.0, tube, 1.0) == 7.0);
        assert!(zone_goal_ev(12.0, tube, -1.0) == 10.0);
        assert!(zone_goal_ev(f32::NAN, tube, 0.0) == 8.5);
        assert!(zone_goal_ev(7.5, tube, f32::NAN) == 8.0);
    }

    #[test]
    fn settle_steps_match_a_stepped_eye() {
        for (gap, speed) in [
            (6.0_f32, DEFAULT_ADAPT_DOWN_SPEED),
            (6.5, DEFAULT_ADAPT_UP_SPEED),
            (1.0, 0.5),
        ] {
            let steps = adapt_settle_steps(gap, EXPOSURE_SETTLED_TOLERANCE_EV, speed, DT).expect("bound");
            let mut ev = 0.0_f32;
            let mut taken = 0_u32;
            while (gap - ev).abs() > EXPOSURE_SETTLED_TOLERANCE_EV {
                ev = adapt_ev(ev, gap, DT, speed, speed);
                taken += 1;
                assert!(
                    taken <= steps + 1,
                    "{gap} EV at {speed} EV/s overran {steps} steps"
                );
            }
            assert!(taken + 1 >= steps, "{gap} EV at {speed} EV/s: {taken} vs {steps}");
        }
        assert_eq!(
            adapt_settle_steps(6.0, 0.05, 1.5, 5.0),
            adapt_settle_steps(6.0, 0.05, 1.5, 0.1)
        );
        assert_eq!(adapt_settle_steps(6.0, 0.0, 1.5, DT), None);
        assert_eq!(adapt_settle_steps(f32::NAN, 0.05, 1.5, DT), None);
    }

    #[test]
    fn host_step_is_consumed_once_and_matches_step() {
        let tube = EvTarget::from_range(EvRange::point(TUBE_EV));
        let mut hosted = ZoneExposure::default();
        let mut stepped = ZoneExposure::default();
        hosted.set_target(Some(tube));
        stepped.set_target(Some(tube));
        for _ in 0..10 {
            hosted.set_frame_dt(Some(DT));
            let a = hosted.apply(MeterReading::steady(SUN_EV));
            let b = stepped.step(MeterReading::steady(SUN_EV), DT);
            assert!(a.to_bits() == b.to_bits());
        }
        // A paused host (step 0) holds the eye.
        let held = hosted.report().expect("report").ev;
        hosted.set_frame_dt(Some(0.0));
        assert!(hosted.apply(MeterReading::steady(SUN_EV)) == held);
    }

    /// Audit G finding 5 (render5 determinism): a frame without a host step holds the
    /// eye. Pre-fix, `apply`/`apply_manual` measured the wall clock between main-view
    /// frames, so a replay presenting the same sim frames at a different speed adapted
    /// to a different exposure, and the log time advanced on wall time.
    #[test]
    fn without_a_host_step_the_eye_and_the_log_hold() {
        let tube = EvTarget::from_range(EvRange::point(TUBE_EV));
        let mut zones = ZoneExposure::default();
        zones.set_target(Some(tube));
        let meter = MeterReading::steady(SUN_EV);
        let first = zones.apply(meter);
        let second = zones.apply(meter);
        assert!(
            second.to_bits() == first.to_bits(),
            "two host-step-less frames hold the eye, got {second} after {first}"
        );
        zones.start_log(8);
        let _ = zones.apply(meter);
        let _ = zones.apply(meter);
        let log = zones.log().expect("log");
        assert!(
            log.samples()
                .iter()
                .all(|sample| sample.time.to_bits() == 0.0f32.to_bits()),
            "the log time does not advance without a host step: {:?}",
            log.samples()
        );
        zones.apply_manual(11.0);
        assert!(
            zones
                .log()
                .expect("log")
                .samples()
                .last()
                .expect("sample")
                .time
                .to_bits()
                == 0.0f32.to_bits(),
            "manual frames do not advance the log on wall time either"
        );
    }

    #[test]
    fn the_log_starts_stops_and_keeps_its_samples() {
        let mut eye = ZoneExposure::default();
        assert!(!eye.is_logging());
        eye.start_log(3);
        for index in 0..5 {
            let _ev = eye.step(MeterReading::steady(10.0 + index as f32), DT);
        }
        let log = eye.log().expect("log");
        assert_eq!(log.len(), 3, "capped");
        assert!(near(log.samples()[0].time, 2.0 * DT));
        assert!(near(log.samples()[2].ev, 14.0));
        eye.stop_log();
        let _ev = eye.step(MeterReading::steady(1.0), DT);
        assert_eq!(eye.log().map(ExposureLog::len), Some(3), "stopped");
        assert!(!eye.is_logging());
        eye.start_log(0);
        assert_eq!(eye.log().map(ExposureLog::len), Some(0), "a fresh log");
    }
}
