// SPDX-License-Identifier: MPL-2.0
//! Enclosed-space lighting: probe placement and a neutral ambient floor.
//!
//! Tunnels and cockpits were rendering near-black. Local lights carry the
//! direct term. This module places irradiance and reflection probes where a
//! few rays say the point is enclosed, and it supplies a neutral illuminance
//! so a surface the lights miss is not quantised to zero.
//!
//! ## Ambient floor
//!
//! [`INTERIOR_AMBIENT_FLOOR_LUX`] is 40 lux: a dim cabin (you can see the
//! consoles; an office is ~300 lux, full sun is ~100 klx). The forward shader
//! treats it as a neutral illuminance, `albedo * lux / π`, with no sky colour.
//! At [`INTERIOR_EXPOSURE_EV`] (EV 8) and albedo about 0.55, the corrected AgX
//! plus the default look prints that term near display luma 37: above black,
//! under a wash. EV 6 prints the same floor near luma 91, which reads as flat
//! grey. At a sunny EV 15 the same 40 lux is crushed to black — that is the
//! exposure, not a missing floor. Interior cameras should pin EV near 8 or
//! leave auto exposure on. Outdoor frames
//! leave [`crate::FrameQuality::interior_ambient`] off, so the lab goldens
//! keep the sky-tinted ambient.
//!
//! ## Enclosure
//!
//! Six cardinal rays. A point is enclosed when at least [`ENCLOSURE_MIN_HITS`]
//! of them hit the SDF within `reach`. A tunnel open at both ends scores 4
//! (floor, ceiling, two walls). An open field scores 1. The threshold is
//! Regia Engine's; the ray test is the cheap form of the sky-visibility check a
//! probe grid uses to know it is indoors (Majercik et al., JCGT 2019).

use regia_app::prelude::*;

use crate::gi::{
    GiError, GiField, MAX_PROBES, ProbeLocalLight, ProbeSun, update_irradiance_with_lights,
    update_irradiance_with_sky,
};
use crate::sdf::{SdfScene, SdfShape, sphere_trace};

/// Neutral interior illuminance, lux. See the module docs for the exposure.
pub const INTERIOR_AMBIENT_FLOOR_LUX: f32 = 40.0;
/// Multiplier on the one traced bounce.
///
/// The field stores a single bounce. In a closed diffuse room the remaining
/// bounces are the geometric series `1 / (1 − ρ)`. For a wall reflectance near
/// 0.6 that sum is 2.5, so the spill on a nearby wall is the full series and
/// not the first term alone. A ray that misses stays black; the gain does not
/// invent light.
pub const INTERIOR_BOUNCE_GAIN: f32 = 2.5;
/// Exposure, EV100, at which 40 lux is a visible floor under the corrected AgX.
///
/// With the default look, albedo ~0.55 prints near luma 37. EV 6 prints near
/// luma 91 (a wash). EV 15 prints 0.
pub const INTERIOR_EXPOSURE_EV: f32 = 8.0;
/// Cardinal hits required before a point counts as indoors.
pub const ENCLOSURE_MIN_HITS: u32 = 4;

const CARDINALS: [[f32; 3]; 6] = [
    [1.0, 0.0, 0.0],
    [-1.0, 0.0, 0.0],
    [0.0, 1.0, 0.0],
    [0.0, -1.0, 0.0],
    [0.0, 0.0, 1.0],
    [0.0, 0.0, -1.0],
];

/// SDF shell extraction tests when no [`InteriorVolume`] contains the camera.
///
/// A reach of zero disables the test. The scene should be a hollow shell
/// ([`hollow_box`]); a solid box reports the eye as buried, not enclosed.
#[derive(Resource, Clone, Debug, Default)]
pub struct EnclosureScene {
    /// Geometry the six cardinal rays march.
    pub scene: SdfScene,
    /// Ray length in metres. Non-positive skips the test.
    pub reach: f32,
}

/// An axis-aligned volume that marks an interior (tunnel, cockpit, room).
///
/// While the camera eye is inside one, extraction selects the interior shade:
/// surfaces *inside the box* drop the sky IBL and take a neutral illuminance,
/// [`Self::ambient_lux`] (or [`INTERIOR_AMBIENT_FLOOR_LUX`] when that field is 0),
/// plus probe bounce. Surfaces outside the box — the sunlit exterior and an open
/// trench seen through the mouth — keep the sky. (Before, the whole view lost
/// the sky, so shadows outside the mouth printed pitch black next to sunlit
/// ground and the frame clipped to black and white.)
#[derive(Component, Debug, Clone, Copy, PartialEq)]
pub struct InteriorVolume {
    /// World centre, metres.
    pub center: [f32; 3],
    /// Half extents, metres. A non-positive axis does not contain any point.
    pub half_extent: [f32; 3],
    /// Fill illuminance in lux. `0` selects [`INTERIOR_AMBIENT_FLOOR_LUX`].
    pub ambient_lux: f32,
}

impl InteriorVolume {
    /// A volume with the exposure-safe floor.
    #[must_use]
    pub fn with_floor(center: [f32; 3], half_extent: [f32; 3]) -> Self {
        Self {
            center,
            half_extent,
            ambient_lux: INTERIOR_AMBIENT_FLOOR_LUX,
        }
    }

    /// True when `point` is inside the box, edges included.
    #[must_use]
    pub fn contains(self, point: [f32; 3]) -> bool {
        if self
            .half_extent
            .iter()
            .any(|axis| !axis.is_finite() || *axis <= 0.0)
        {
            return false;
        }
        point.iter().enumerate().all(|(axis, value)| {
            value.is_finite() && (value - self.center[axis]).abs() <= self.half_extent[axis]
        })
    }

    /// The lux the shader should add. Zero in the asset means the floor constant.
    #[must_use]
    pub fn resolved_lux(self) -> f32 {
        if self.ambient_lux.is_finite() && self.ambient_lux > 0.0 {
            self.ambient_lux
        } else {
            INTERIOR_AMBIENT_FLOOR_LUX
        }
    }
}

impl Default for InteriorVolume {
    fn default() -> Self {
        Self::with_floor([0.0; 3], [1.0, 1.0, 1.0])
    }
}

/// What extraction writes when it has looked at the volumes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InteriorShade {
    /// The view takes the interior shade (neutral floor, probe bounce added).
    pub interior: bool,
    /// Neutral ambient illuminance, lux. Unchanged when `interior` is false.
    pub ambient_lux: f32,
    /// Image-based sky lighting for the view. An enclosure test without a box forces
    /// it off for the whole view; a volume keeps it, because the renderer drops the
    /// sky only for pixels inside [`Self::volume`].
    pub ibl: bool,
    /// The volume that contains the eye, when a volume (not the enclosure test)
    /// selected the shade.
    pub volume: Option<InteriorVolume>,
}

/// The first volume that contains `eye` wins.
#[must_use]
pub fn shade_for_eye(
    eye: [f32; 3],
    volumes: &[InteriorVolume],
    ambient_lux: f32,
    ibl: bool,
) -> InteriorShade {
    if let Some(volume) = volumes.iter().copied().find(|volume| volume.contains(eye)) {
        InteriorShade {
            interior: true,
            ambient_lux: volume.resolved_lux(),
            ibl,
            volume: Some(volume),
        }
    } else {
        InteriorShade {
            interior: false,
            ambient_lux,
            ibl,
            volume: None,
        }
    }
}

/// [`shade_for_eye`], then [`detect_enclosure`] when no volume contains the eye.
///
/// Extraction calls this so an `EnclosureScene` selects the same neutral shade
/// a volume would (ADR-034). A volume still wins, including its own lux.
#[must_use]
pub fn shade_for_view(
    eye: [f32; 3],
    volumes: &[InteriorVolume],
    enclosure: Option<&SdfScene>,
    reach: f32,
    ambient_lux: f32,
    ibl: bool,
) -> InteriorShade {
    let shade = shade_for_eye(eye, volumes, ambient_lux, ibl);
    if shade.interior {
        return shade;
    }
    let Some(scene) = enclosure else {
        return shade;
    };
    if detect_enclosure(scene, eye, reach) {
        InteriorShade {
            interior: true,
            ambient_lux: INTERIOR_AMBIENT_FLOOR_LUX,
            ibl: false,
            volume: None,
        }
    } else {
        shade
    }
}

/// Six thin walls on the faces of an AABB. The interior is air, not a solid box.
///
/// A solid box marks every probe inside it as buried, so a cabin traced that way
/// stores black. `thickness` is the full wall thickness, centred on each face.
#[must_use]
pub fn hollow_box(center: [f32; 3], half: [f32; 3], thickness: f32, albedo: [f32; 3]) -> SdfScene {
    let t = if thickness.is_finite() && thickness > 0.0 {
        thickness * 0.5
    } else {
        0.04
    };
    let wall = |at: [f32; 3], extent: [f32; 3]| SdfShape::Box {
        center: at,
        half: extent,
        albedo,
        emissive: [0.0; 3],
    };
    let mut scene = SdfScene::default();
    scene.shapes.push(wall(
        [center[0], center[1] - half[1], center[2]],
        [half[0] + t, t, half[2] + t],
    ));
    scene.shapes.push(wall(
        [center[0], center[1] + half[1], center[2]],
        [half[0] + t, t, half[2] + t],
    ));
    scene.shapes.push(wall(
        [center[0] - half[0], center[1], center[2]],
        [t, half[1] + t, half[2] + t],
    ));
    scene.shapes.push(wall(
        [center[0] + half[0], center[1], center[2]],
        [t, half[1] + t, half[2] + t],
    ));
    scene.shapes.push(wall(
        [center[0], center[1], center[2] - half[2]],
        [half[0] + t, half[1] + t, t],
    ));
    scene.shapes.push(wall(
        [center[0], center[1], center[2] + half[2]],
        [half[0] + t, half[1] + t, t],
    ));
    scene
}

/// True when at least [`ENCLOSURE_MIN_HITS`] cardinal rays hit `scene` within `reach`.
#[must_use]
pub fn detect_enclosure(scene: &SdfScene, point: [f32; 3], reach: f32) -> bool {
    if !point.iter().all(|c| c.is_finite()) || !reach.is_finite() || reach <= 0.0 {
        return false;
    }
    let hits = CARDINALS
        .iter()
        .filter(|dir| sphere_trace(scene, point, **dir, reach).is_some())
        .count();
    hits >= ENCLOSURE_MIN_HITS as usize
}

/// Probe grid and reflection-probe box for one interior volume.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InteriorCoverage {
    /// World position of probe `(0,0,0)`.
    pub origin: [f32; 3],
    /// Metres between probes.
    pub spacing: [f32; 3],
    /// Probe counts. The product is at most [`MAX_PROBES`].
    pub dims: [u32; 3],
    /// How many of those probes sit at an enclosed point.
    pub enclosed: u32,
    /// Reflection-probe centre (the volume centre).
    pub reflection_center: [f32; 3],
    /// Reflection-probe half extents (the volume's).
    pub reflection_half_extent: [f32; 3],
}

/// Places a probe grid inside `volume` and counts enclosed cells.
///
/// When the grid is larger than [`MAX_PROBES`], the longest axis is coarsened
/// and the short axes keep `spacing`. A tunnel then still has a cross-section
/// instead of a single line of probes. Each axis spans the volume inset by
/// 15 cm, so the outer probes sit just inside the shell instead of a full
/// cell back from it. Probes are not traced here; pass the grid to
/// [`build_interior_lighting`].
#[must_use]
pub fn plan_interior_coverage(
    volume: &InteriorVolume,
    scene: &SdfScene,
    spacing: f32,
    reach: f32,
) -> InteriorCoverage {
    let spacing = if spacing.is_finite() && spacing >= 0.25 {
        spacing
    } else {
        1.0
    };
    let half = [
        volume.half_extent[0].abs().max(spacing * 0.5),
        volume.half_extent[1].abs().max(spacing * 0.5),
        volume.half_extent[2].abs().max(spacing * 0.5),
    ];
    // Keep the outer probe this far inside the volume face. A larger inset
    // left the tunnel cap and the ceiling corner sampling a probe on the
    // far side of the shell.
    const FACE_INSET_M: f32 = 0.15;
    /// Probe count for `dims`: `None` when the product overflows a u64 (an
    /// absurd volume), which the planner reads as "must coarsen".
    fn probe_count(dims: [u32; 3]) -> Option<u64> {
        u64::from(dims[0])
            .checked_mul(u64::from(dims[1]))?
            .checked_mul(u64::from(dims[2]))
    }
    let mut step = [spacing; 3];
    let mut dims = [1u32; 3];
    for _ in 0..16 {
        for axis in 0..3 {
            let span = (half[axis] * 2.0 - FACE_INSET_M * 2.0).max(step[axis]);
            // u64 and saturating: a span past u32::MAX probes must grow the
            // step below, not wrap `gaps + 1` to 0 (a debug panic) and not
            // become a grid `GiField::new` rejects as empty.
            let gaps = (span / step[axis]).round().max(1.0) as u64;
            dims[axis] = gaps.saturating_add(1).min(u64::from(u32::MAX)) as u32;
            step[axis] = span / gaps.max(1) as f32;
        }
        if probe_count(dims).is_none_or(|count| count <= MAX_PROBES as u64) {
            break;
        }
        let axis = (0..3).max_by_key(|axis| dims[*axis]).unwrap_or(0);
        step[axis] *= 1.5;
    }
    while probe_count(dims).is_none_or(|count| count > MAX_PROBES as u64) {
        let axis = (0..3).max_by_key(|axis| dims[*axis]).unwrap_or(0);
        if dims[axis] <= 1 {
            break;
        }
        // Halve the longest axis toward the others (clamped to what the budget
        // allows), so an absurd cube coarsens to a cube-ish grid that still
        // spans the volume and not to a single line of probes — decrementing
        // one axis at a time would take millions of iterations.
        let others = u64::from(dims[(axis + 1) % 3]).saturating_mul(u64::from(dims[(axis + 2) % 3]));
        let needed = (MAX_PROBES as u64 / others.max(1)).max(1) as u32;
        dims[axis] = needed.max(dims[axis] / 2).clamp(1, dims[axis] - 1);
        let span = (half[axis] * 2.0 - FACE_INSET_M * 2.0).max(step[axis]);
        if dims[axis] > 1 {
            step[axis] = span / (dims[axis] - 1) as f32;
        }
    }
    let origin = [0, 1, 2].map(|axis| {
        let span = (dims[axis].saturating_sub(1)) as f32 * step[axis];
        volume.center[axis] - span * 0.5
    });
    let mut enclosed = 0u32;
    for z in 0..dims[2] {
        for y in 0..dims[1] {
            for x in 0..dims[0] {
                let point = [
                    origin[0] + x as f32 * step[0],
                    origin[1] + y as f32 * step[1],
                    origin[2] + z as f32 * step[2],
                ];
                if detect_enclosure(scene, point, reach) {
                    enclosed += 1;
                }
            }
        }
    }
    InteriorCoverage {
        origin,
        spacing: step,
        dims,
        enclosed,
        reflection_center: volume.center,
        reflection_half_extent: half,
    }
}

/// A probe grid traced for one interior, plus the reflection box from the plan.
#[derive(Clone, Debug)]
pub struct InteriorLighting {
    /// Placement. [`Self::field`] uses this origin, spacing and size.
    pub coverage: InteriorCoverage,
    /// Irradiance after one bounce of `lights`. Misses store black.
    pub field: GiField,
}

/// Runs [`plan_interior_coverage`] and traces that grid with `lights`.
///
/// The sun is off and a miss stores black, so the field is the local-light
/// bounce only, scaled by [`INTERIOR_BOUNCE_GAIN`] for the bounces a single
/// trace leaves out. The 40 lux floor stays in the forward shader. Bind
/// [`InteriorLighting::field`] with [`crate::GiMode::Probes`] and capture one
/// reflection probe at [`InteriorCoverage::reflection_center`].
///
/// # Errors
///
/// The planned grid is not a usable [`GiField`] (it is capped, so this is the
/// bad-spacing path).
#[tracing::instrument(skip_all)]
pub fn build_interior_lighting(
    volume: &InteriorVolume,
    scene: &SdfScene,
    lights: &[crate::lab_frame::LabLocalLight],
    spacing: f32,
    reach: f32,
    rays: u32,
) -> Result<InteriorLighting, GiError> {
    let coverage = plan_interior_coverage(volume, scene, spacing, reach);
    let mut field = GiField::new(coverage.origin, coverage.spacing, coverage.dims)?;
    field.normal_bias = 0.1;
    field.intensity = INTERIOR_BOUNCE_GAIN;
    let probes: Vec<ProbeLocalLight> = lights.iter().map(ProbeLocalLight::from_lab).collect();
    let sun = ProbeSun {
        direction: [0.0, -1.0, 0.0],
        color: [1.0; 3],
        illuminance: 0.0,
    };
    update_irradiance_with_lights(&mut field, scene, sun, &probes, false, rays, 0.0);
    Ok(InteriorLighting { coverage, field })
}

/// Updates [`build_daylit_interior`] runs: each one adds a bounce (hits read the
/// previous field), so sunlit floor → wall → shadowed floor arrives on the third.
pub const DAYLIT_INTERIOR_BOUNCES: u32 = 3;

#[cfg(test)]
thread_local! {
    static DAYLIT_BAKES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Daylit interior bakes this thread has started. Tests read the delta to show a
/// held fallback box does not retrace.
#[cfg(test)]
#[must_use]
pub(crate) fn daylit_bakes() -> u64 {
    DAYLIT_BAKES.with(std::cell::Cell::get)
}

#[cfg(test)]
fn note_daylit_bake() {
    DAYLIT_BAKES.with(|cell| cell.set(cell.get() + 1));
}

/// [`build_interior_lighting`] for an interior that daylight reaches: a bore with an
/// open end, a trench under trusses, a hangar with its doors open.
///
/// The grid is traced with the transmitted `sun` (shadow rays against `scene`), the
/// punctual `lights`, and `sky` — the radiance (cd/m²) an escaping ray sees, the
/// planet's sky above the horizon and its sunlit ground below — for
/// [`DAYLIT_INTERIOR_BOUNCES`] updates, so the field holds the light that comes in
/// through the openings and off the sunlit floor, walls and roof. Unlike the sealed
/// bake it applies no [`INTERIOR_BOUNCE_GAIN`]: the bounces are traced. The renderer
/// runs this itself when the eye is in an interior, the profile uses probe GI, the
/// game set no field of its own, and an [`EnclosureScene`] gives the geometry.
///
/// # Errors
///
/// The planned grid is not a usable [`GiField`].
#[tracing::instrument(skip_all)]
#[allow(clippy::too_many_arguments)] // each argument is an independent bake input
pub fn build_daylit_interior(
    volume: &InteriorVolume,
    scene: &SdfScene,
    lights: &[crate::lab_frame::LabLocalLight],
    sun: ProbeSun,
    sky: &dyn Fn([f32; 3]) -> [f32; 3],
    spacing: f32,
    reach: f32,
    rays: u32,
) -> Result<InteriorLighting, GiError> {
    #[cfg(test)]
    note_daylit_bake();
    let coverage = plan_interior_coverage(volume, scene, spacing, reach);
    let mut field = GiField::new(coverage.origin, coverage.spacing, coverage.dims)?;
    field.normal_bias = 0.1;
    let probes: Vec<ProbeLocalLight> = lights.iter().map(ProbeLocalLight::from_lab).collect();
    for _ in 0..DAYLIT_INTERIOR_BOUNCES {
        update_irradiance_with_sky(&mut field, scene, sun, &probes, sky, rays, 0.0);
    }
    Ok(InteriorLighting { coverage, field })
}

/// Interior volumes the outdoor composite reads: the froxel fog's and the aerial perspective's
/// four slots (ADR-4757), filled by [`aerial_interior_slots`]. A volume left out shades surfaces but
/// leaves the air outdoor.
pub const AERIAL_INTERIOR_SLOTS: usize = 4;

/// Which of `volumes` the composite's [`AERIAL_INTERIOR_SLOTS`] slots hold, as indices into
/// `volumes`, for a camera at `eye` (ADR-7661).
///
/// With at most four volumes, slot `i` holds volume `i` (an unusable box keeps its slot empty, as
/// before). With more, the slots hold the four usable volumes nearest `eye` (distance from the eye to
/// the box, 0 inside it; ties to the earlier volume), in list order, so the box the camera stands in
/// is always read. Before, the first four in list order were read: a map with one volume per storey
/// (DO27 ask 11, stacked floors) or several buildings could leave the camera's own box out, and the
/// air inside it took the sky's in-scatter. A non-finite `eye` keeps the first four.
#[must_use]
pub fn aerial_interior_slots(
    volumes: &[InteriorVolume],
    eye: [f32; 3],
) -> [Option<usize>; AERIAL_INTERIOR_SLOTS] {
    let mut slots = [None; AERIAL_INTERIOR_SLOTS];
    if volumes.len() <= AERIAL_INTERIOR_SLOTS || eye.iter().any(|v| !v.is_finite()) {
        for (slot, out) in slots.iter_mut().enumerate().take(volumes.len()) {
            *out = Some(slot);
        }
        return slots;
    }
    let usable = |v: &InteriorVolume| {
        v.half_extent.iter().all(|h| h.is_finite() && *h > 0.0) && v.center.iter().all(|c| c.is_finite())
    };
    // Squared distance from the eye to the box (0 inside).
    let dist2 = |v: &InteriorVolume| {
        (0..3)
            .map(|a| {
                let d = ((eye[a] - v.center[a]).abs() - v.half_extent[a]).max(0.0);
                d * d
            })
            .sum::<f32>()
    };
    // Selection without allocation (this runs every frame): the nearest not yet taken, four times.
    let mut taken = 0;
    while taken < AERIAL_INTERIOR_SLOTS {
        let mut best: Option<(f32, usize)> = None;
        for (i, v) in volumes.iter().enumerate() {
            if !usable(v) || slots[..taken].contains(&Some(i)) {
                continue;
            }
            let d = dist2(v);
            if best.is_none_or(|(bd, _)| d < bd) {
                best = Some((d, i));
            }
        }
        let Some((_, i)) = best else {
            break;
        };
        slots[taken] = Some(i);
        taken += 1;
    }
    slots[..taken].sort_unstable();
    slots
}

/// Squared distance from `eye` to the box of `volume` (0 inside it); `+inf` for a box that holds
/// no air (a non-positive or non-finite half extent, a non-finite centre).
fn eye_box_distance_sq(volume: &InteriorVolume, eye: [f32; 3]) -> f32 {
    if volume.half_extent.iter().any(|h| !h.is_finite() || *h <= 0.0)
        || volume.center.iter().any(|c| !c.is_finite())
    {
        return f32::INFINITY;
    }
    (0..3)
        .map(|a| {
            let d = ((eye[a] - volume.center[a]).abs() - volume.half_extent[a]).max(0.0);
            d * d
        })
        .sum()
}

/// The list indices of the volumes the aerial composite's [`AERIAL_INTERIOR_SLOTS`] slots hold, in
/// list order (ADR-7975). With up to that many volumes, all of them in order (the packing before
/// ADR-7975). With more, the ones nearest `eye` (distance from the eye to each box, 0 inside it;
/// ties by list order; a box that holds no air last), so a passage the eye stands in always takes
/// its air out, whatever its place in the list. A non-finite eye keeps the first ones in list order.
#[must_use]
pub fn aerial_interior_pick(eye: [f32; 3], volumes: &[InteriorVolume]) -> Vec<usize> {
    if volumes.len() <= AERIAL_INTERIOR_SLOTS || eye.iter().any(|v| !v.is_finite()) {
        return (0..volumes.len().min(AERIAL_INTERIOR_SLOTS)).collect();
    }
    let mut order: Vec<(f32, usize)> = volumes
        .iter()
        .enumerate()
        .map(|(i, v)| (eye_box_distance_sq(v, eye), i))
        .collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
    let mut picked: Vec<usize> = order
        .iter()
        .take(AERIAL_INTERIOR_SLOTS)
        .map(|(_, i)| *i)
        .collect();
    picked.sort_unstable();
    picked
}

/// Spans shorter than this (meters) are not taken out of the air: a ray grazing a box's face.
/// `INTERIOR_MIN_SPAN_M` in `outdoor_sky.wgsl`, the same value.
pub const AERIAL_INTERIOR_MIN_SPAN_M: f32 = 1.0e-3;

/// The stretches of one view ray that lie inside interior volumes, as the aerial composite takes
/// them out of the air ([`interior_spans_on_ray`]).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct InteriorRaySpans {
    /// Merged, sorted `[start, end]` stretches along the ray, meters from the eye.
    pub spans: Vec<[f32; 2]>,
    /// Their total length, meters.
    pub inside_m: f32,
    /// The ray's length outside every volume, meters.
    pub outside_m: f32,
}

impl InteriorRaySpans {
    /// The share of the ray's in-scatter the composite keeps while the surface lies inside the
    /// aerial table's first slice (2 km), where the in-scatter grows linearly with distance:
    /// `outside / distance`. 1 for a ray that crosses no volume.
    #[must_use]
    pub fn inscatter_share_near(&self) -> f32 {
        let total = self.inside_m + self.outside_m;
        if total > 0.0 { self.outside_m / total } else { 1.0 }
    }
}

/// The stretch `[t0, t1]` of the ray `eye + dir t`, `t` in `[0, dist]`, inside `volume` (slab
/// test, Kay & Kajiya 1986). `None` when the ray misses it or the span is shorter than
/// [`AERIAL_INTERIOR_MIN_SPAN_M`]. `dir` must be a unit direction for distances in meters.
/// Invalid boxes, non-finite inputs, zero directions and non-positive distances return `None`.
#[must_use]
pub fn interior_span(volume: &InteriorVolume, eye: [f32; 3], dir: [f32; 3], dist: f32) -> Option<[f32; 2]> {
    if volume.half_extent.iter().any(|h| !h.is_finite() || *h <= 0.0)
        || volume
            .center
            .iter()
            .chain(eye.iter())
            .chain(dir.iter())
            .any(|v| !v.is_finite())
        || dir.iter().all(|v| *v == 0.0)
        || !dist.is_finite()
        || dist <= 0.0
    {
        return None;
    }
    let (mut t0, mut t1) = (0.0f32, dist);
    for a in 0..3 {
        let lo = volume.center[a] - volume.half_extent[a] - eye[a];
        let hi = volume.center[a] + volume.half_extent[a] - eye[a];
        let d = dir[a];
        if d.abs() < 1.0e-9 {
            if lo > 0.0 || hi < 0.0 {
                return None;
            }
        } else {
            let (ta, tb) = (lo / d, hi / d);
            t0 = t0.max(ta.min(tb));
            t1 = t1.min(ta.max(tb));
        }
    }
    (t1 - t0 >= AERIAL_INTERIOR_MIN_SPAN_M).then_some([t0, t1])
}

/// The stretches of the view ray from `eye` along the unit `dir` to a surface `dist` away inside
/// the volumes [`aerial_interior_slots`] binds for `eye`, merged where boxes overlap: the CPU twin of
/// `interior_air_on_ray` in `outdoor_sky.wgsl`. The composite subtracts the in-scatter of each
/// stretch (the air there is under a roof) and keeps the transmittance (ADR-4757).
#[must_use]
pub fn interior_spans_on_ray(
    eye: [f32; 3],
    dir: [f32; 3],
    dist: f32,
    volumes: &[InteriorVolume],
) -> InteriorRaySpans {
    let dist = if dist.is_finite() { dist.max(0.0) } else { 0.0 };
    let mut raw: Vec<[f32; 2]> = aerial_interior_slots(volumes, eye)
        .iter()
        .flatten()
        .filter_map(|&i| interior_span(&volumes[i], eye, dir, dist))
        .collect();
    raw.sort_by(|a, b| a[0].total_cmp(&b[0]));
    let mut spans: Vec<[f32; 2]> = Vec::with_capacity(raw.len());
    for s in raw {
        match spans.last_mut() {
            Some(cur) if s[0] <= cur[1] => cur[1] = cur[1].max(s[1]),
            _ => spans.push(s),
        }
    }
    let inside_m: f32 = spans.iter().map(|s| s[1] - s[0]).sum();
    InteriorRaySpans {
        spans,
        inside_m,
        outside_m: (dist - inside_m).max(0.0),
    }
}

/// Linear scene luminance of a diffuse surface under the floor alone, cd/m².
///
/// `albedo * lux / π`. The shader multiplies by the exposure next.
#[must_use]
pub fn interior_floor_luminance(albedo: f32) -> f32 {
    albedo.max(0.0) * INTERIOR_AMBIENT_FLOOR_LUX / std::f32::consts::PI
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::post::exposure_from_ev100;
    use crate::sdf::SdfShape;
    use crate::tonemap::tonemap_agx;

    fn wall(center: [f32; 3], half: [f32; 3]) -> SdfShape {
        SdfShape::Box {
            center,
            half,
            albedo: [0.6, 0.58, 0.55],
            emissive: [0.0; 3],
        }
    }

    fn closed_room() -> SdfScene {
        let mut scene = SdfScene::default();
        scene.shapes.push(wall([0.0, -2.05, 0.0], [2.3, 0.08, 2.3]));
        scene.shapes.push(wall([0.0, 2.05, 0.0], [2.3, 0.08, 2.3]));
        scene.shapes.push(wall([-2.05, 0.0, 0.0], [0.08, 2.3, 2.3]));
        scene.shapes.push(wall([2.05, 0.0, 0.0], [0.08, 2.3, 2.3]));
        scene.shapes.push(wall([0.0, 0.0, -2.05], [2.3, 2.3, 0.08]));
        scene.shapes.push(wall([0.0, 0.0, 2.05], [2.3, 2.3, 0.08]));
        scene
    }

    fn open_tunnel() -> SdfScene {
        // Bore along X. The ends are open, so ±X miss when `reach` is short.
        let mut scene = SdfScene::default();
        scene.shapes.push(wall([0.0, -1.2, 0.0], [8.0, 0.1, 1.3]));
        scene.shapes.push(wall([0.0, 1.2, 0.0], [8.0, 0.1, 1.3]));
        scene.shapes.push(wall([0.0, 0.0, -1.2], [8.0, 1.3, 0.1]));
        scene.shapes.push(wall([0.0, 0.0, 1.2], [8.0, 1.3, 0.1]));
        scene
    }

    fn encode_srgb(linear: f32) -> f32 {
        linear.max(0.0).powf(1.0 / 2.2)
    }

    #[test]
    fn a_closed_room_and_an_open_tunnel_are_enclosed_and_a_field_is_not() {
        assert!(detect_enclosure(&closed_room(), [0.0, 0.0, 0.0], 4.0));
        assert!(detect_enclosure(&open_tunnel(), [0.0, 0.0, 0.0], 3.0));
        let mut field = SdfScene::default();
        field.shapes.push(wall([0.0, -1.0, 0.0], [20.0, 0.1, 20.0]));
        assert!(!detect_enclosure(&field, [0.0, 2.0, 0.0], 6.0));
    }

    #[test]
    fn coverage_puts_probes_and_a_reflection_box_inside_the_room() {
        let scene = closed_room();
        let volume = InteriorVolume::with_floor([0.0; 3], [1.6, 1.6, 1.6]);
        let plan = plan_interior_coverage(&volume, &scene, 1.2, 4.0);
        let count = plan.dims[0] as usize * plan.dims[1] as usize * plan.dims[2] as usize;
        assert!((1..=MAX_PROBES).contains(&count));
        assert!(
            plan.enclosed >= 1,
            "an enclosed room must keep at least one probe, plan {plan:?}"
        );
        assert_eq!(plan.reflection_center, volume.center);
        let field = crate::GiField::new(plan.origin, plan.spacing, plan.dims);
        assert!(field.is_ok(), "the planned grid must fit the GPU buffer");
    }

    #[test]
    fn a_long_bore_keeps_a_cross_section_instead_of_one_line_of_probes() {
        // 160 m × 3.2 m × 4.4 m at 1.2 m would be 133×2×3 = 798 probes.
        // Coarsening every axis drops the height to 1. Coarsening X keeps it.
        let volume = InteriorVolume::with_floor([0.0; 3], [80.0, 1.6, 2.2]);
        let plan = plan_interior_coverage(&volume, &open_tunnel(), 1.2, 4.0);
        let count = plan.dims[0] as usize * plan.dims[1] as usize * plan.dims[2] as usize;
        assert!(count <= MAX_PROBES, "plan {plan:?}");
        assert!(plan.dims[1] >= 2, "the bore must keep a height, {plan:?}");
        assert!(plan.dims[2] >= 2, "the bore must keep a width, {plan:?}");
        assert!(
            plan.dims[0] >= 8,
            "the long axis stays the one that is coarsened, {plan:?}"
        );
    }

    #[test]
    fn an_oversized_volume_coarsens_instead_of_overflowing() {
        let scene = closed_room();
        let probe_count = |dims: [u32; 3]| u64::from(dims[0]) * u64::from(dims[1]) * u64::from(dims[2]);
        let spacing_spans = |volume: &InteriorVolume, plan: &InteriorCoverage| {
            for axis in 0..3 {
                let span = (plan.dims[axis].saturating_sub(1)) as f64 * f64::from(plan.spacing[axis]);
                let want = f64::from((volume.half_extent[axis] * 2.0 - 0.15 * 2.0).max(plan.spacing[axis]));
                assert!(
                    span >= want * 0.99,
                    "axis {axis}: span {span} must still reach the volume ({want} m), plan {plan:?}"
                );
            }
        };
        // One axis past u32::MAX probes: the plan must grow the step, not panic
        // on `gaps + 1` or hand `GiField::new` a grid it rejects.
        let huge = InteriorVolume::with_floor([0.0; 3], [3.0e9, 1.0, 1.0]);
        let plan = plan_interior_coverage(&huge, &scene, 1.0, 4.0);
        assert!(
            (1..=MAX_PROBES as u64).contains(&probe_count(plan.dims)),
            "plan {plan:?}"
        );
        spacing_spans(&huge, &plan);
        let field = crate::GiField::new(plan.origin, plan.spacing, plan.dims);
        assert!(field.is_ok(), "the planned grid must fit the GPU buffer");
        // Three ~3-million-probe axes: the product overflows usize. Coarsen.
        let vast = InteriorVolume::with_floor([0.0; 3], [1.5e6, 1.5e6, 1.5e6]);
        let plan = plan_interior_coverage(&vast, &scene, 1.0, 4.0);
        assert!(
            (1..=MAX_PROBES as u64).contains(&probe_count(plan.dims)),
            "plan {plan:?}"
        );
        spacing_spans(&vast, &plan);
    }

    #[test]
    fn detect_enclosure_selects_the_floor_when_no_volume_contains_the_eye() {
        let scene = closed_room();
        let inside = shade_for_view([0.0, 0.2, 0.0], &[], Some(&scene), 4.0, 12_000.0, true);
        assert!(inside.interior);
        assert!(!inside.ibl);
        assert!((inside.ambient_lux - INTERIOR_AMBIENT_FLOOR_LUX).abs() < 1.0e-3);
        let outside = shade_for_view([20.0, 2.0, 0.0], &[], Some(&scene), 4.0, 12_000.0, true);
        assert!(!outside.interior);
        assert!(outside.ibl);
        let custom = InteriorVolume {
            center: [0.0; 3],
            half_extent: [1.0; 3],
            ambient_lux: 80.0,
        };
        let volume_wins = shade_for_view([0.0, 0.2, 0.0], &[custom], Some(&scene), 4.0, 12_000.0, true);
        assert!(volume_wins.interior);
        assert!((volume_wins.ambient_lux - 80.0).abs() < 1.0e-3);
    }

    #[test]
    fn the_floor_clears_agx_at_interior_ev_and_not_at_sunny_ev() {
        // Albedo 0.5, neutral illuminance, then EV100 exposure, then AgX + sRGB.
        let luminance = interior_floor_luminance(0.5);
        let at_interior = tonemap_agx([luminance * exposure_from_ev100(INTERIOR_EXPOSURE_EV); 3]);
        let byte = (encode_srgb(at_interior[0]) * 255.0).round();
        assert!(
            byte >= 1.0,
            "40 lux at EV {INTERIOR_EXPOSURE_EV} must not quantize to black, byte {byte}, lum {luminance}"
        );
        let at_sun = tonemap_agx([luminance * exposure_from_ev100(15.0); 3]);
        let sun_byte = (encode_srgb(at_sun[1]) * 255.0).round();
        assert_eq!(
            sun_byte, 0.0,
            "the same floor at EV 15 is crushed; interiors must not use sunny exposure"
        );
    }

    #[test]
    fn the_camera_inside_a_volume_takes_the_floor_inside_the_box_only() {
        let volume = InteriorVolume::with_floor([0.0, 1.0, 0.0], [2.0, 1.0, 2.0]);
        let inside = shade_for_eye([0.2, 1.1, -0.4], &[volume], 12_000.0, true);
        assert!(inside.interior);
        // The view keeps the sky for what lies outside the box (the exterior seen
        // through the mouth); the renderer drops it for pixels inside the box.
        assert!(inside.ibl);
        assert_eq!(inside.volume, Some(volume));
        let no_sky = shade_for_eye([0.2, 1.1, -0.4], &[volume], 12_000.0, false);
        assert!(!no_sky.ibl, "a view without IBL does not gain it");
        assert!((inside.ambient_lux - INTERIOR_AMBIENT_FLOOR_LUX).abs() < 1.0e-3);
        let outside = shade_for_eye([9.0, 1.0, 0.0], &[volume], 12_000.0, true);
        assert!(!outside.interior);
        assert!(outside.ibl);
        assert_eq!(outside.volume, None);
        assert!((outside.ambient_lux - 12_000.0).abs() < 1.0e-3);
        let custom = InteriorVolume {
            center: [0.0; 3],
            half_extent: [1.0; 3],
            ambient_lux: 80.0,
        };
        assert!((custom.resolved_lux() - 80.0).abs() < 1.0e-4);
        assert!(InteriorVolume::default().contains([0.0; 3]));
    }

    /// The cockpit side wall is outside the inset probe box. A one-ray 4×4
    /// depth cell made Chebyshev a step, so a patch of wall went to zero
    /// irradiance beside a neighbour the next cell still lit.
    #[test]
    fn cockpit_side_wall_bounce_has_no_zero_islands() {
        use crate::gi::sample_irradiance;
        let mut shell = hollow_box([0.0, 0.0, 0.0], [1.8, 1.1, 2.4], 0.08, [0.62, 0.58, 0.52]);
        shell.shapes.push(SdfShape::Box {
            center: [1.15, 0.15, 0.2],
            half: [0.35, 0.12, 0.5],
            albedo: [0.4, 0.46, 0.55],
            emissive: [0.0; 3],
        });
        let volume = cockpit_probe_volume();
        let lights = cockpit_spot();
        let lighting = build_interior_lighting(&volume, &shell, &lights, 0.72, 4.5, 32).expect("grid");
        let energy = |at: [f32; 3], normal: [f32; 3]| {
            let e = sample_irradiance(&lighting.field, at, normal);
            e[0] + e[1] + e[2]
        };
        // The measured island: z = -1.49 is black, 8 cm over is the bounced wall.
        let beside = energy([-1.78, 1.06, -1.57], [1.0, 0.0, 0.0]);
        let island = energy([-1.78, 1.06, -1.49], [1.0, 0.0, 0.0]);
        assert!(beside > 20.0, "the neighbor must be bounced, energy {beside}");
        assert!(
            island > beside * 0.5,
            "zero-GI island on the left wall: {island} beside {beside}"
        );
        let mut zeros = 0u32;
        for (x, normal) in [(1.78_f32, [-1.0, 0.0, 0.0]), (-1.78, [1.0, 0.0, 0.0])] {
            let mut y = 0.4_f32;
            while y < 1.08 {
                let mut z = -1.9_f32;
                while z < 1.2 {
                    if energy([x, y, z], normal) < 0.5 {
                        zeros += 1;
                    }
                    z += 0.08;
                }
                y += 0.08;
            }
        }
        assert_eq!(zeros, 0, "side-wall samples with no bounce");
    }

    /// Pixels (253,185) and (255,186) of the 960×540 cockpit sit on the outer
    /// ceiling corner (−1.80, 1.10, −2.34). A volume that stopped 30 cm short
    /// of the shell left that point 0.77 m outside the grid; the one probe
    /// that reached it had Chebyshev visibility 0.147 and was dropped, so the
    /// texel was the bare 40 lux floor beside a lit wall.
    #[test]
    fn cockpit_ceiling_corner_keeps_bounce() {
        use crate::gi::sample_irradiance;
        let mut shell = hollow_box([0.0, 0.0, 0.0], [1.8, 1.1, 2.4], 0.08, [0.62, 0.58, 0.52]);
        shell.shapes.push(SdfShape::Box {
            center: [1.15, 0.15, 0.2],
            half: [0.35, 0.12, 0.5],
            albedo: [0.4, 0.46, 0.55],
            emissive: [0.0; 3],
        });
        let lighting =
            build_interior_lighting(&cockpit_probe_volume(), &shell, &cockpit_spot(), 0.72, 4.5, 128)
                .expect("grid");
        for z in 0..lighting.field.dims[2] {
            for y in 0..lighting.field.dims[1] {
                for x in 0..lighting.field.dims[0] {
                    let at = lighting.field.probe_position(x, y, z);
                    let distance = shell.sample(at).distance;
                    assert!(
                        distance > 0.0,
                        "probe ({x},{y},{z}) at {at:?} is buried in the shell (sdf {distance})"
                    );
                }
            }
        }
        let energy = |at: [f32; 3], normal: [f32; 3]| {
            let e = sample_irradiance(&lighting.field, at, normal);
            e[0] + e[1] + e[2]
        };
        let mid = energy([0.0, 1.1, 0.0], [0.0, -1.0, 0.0]);
        let corner = energy([-1.795, 1.1, -2.338], [0.0, -1.0, 0.0]);
        assert!(mid > 1.0, "ceiling center has no bounce ({mid})");
        assert!(
            corner > mid * 0.35,
            "ceiling corner is a probe hole (corner {corner}, mid {mid}), plan {:?}",
            lighting.coverage
        );
    }

    /// ADR-4757: the aerial composite's interior stretches (the CPU twin of the shader's clip).
    #[test]
    fn interior_spans_follow_the_boxes_the_ray_crosses() {
        let room = InteriorVolume::with_floor([0.0, 2.0, 0.0], [5.0, 2.0, 5.0]);
        // A ray in the open, away from the room: nothing taken out, the old path.
        let open = interior_spans_on_ray([20.0, 2.0, 0.0], [1.0, 0.0, 0.0], 300.0, &[room]);
        assert!(open.spans.is_empty());
        assert_eq!(open.inside_m, 0.0);
        assert_eq!(open.inscatter_share_near(), 1.0);
        // Eye and surface inside: all of the air is under the roof.
        let closed = interior_spans_on_ray([0.0, 2.0, 0.0], [1.0, 0.0, 0.0], 4.0, &[room]);
        assert_eq!(closed.spans, vec![[0.0, 4.0]]);
        assert_eq!(closed.inscatter_share_near(), 0.0);
        // Eye inside, surface 95 m out through the door: the first 5 m only.
        let door = interior_spans_on_ray([0.0, 2.0, 0.0], [1.0, 0.0, 0.0], 100.0, &[room]);
        assert_eq!(door.spans, vec![[0.0, 5.0]]);
        assert!((door.outside_m - 95.0).abs() < 1.0e-4);
        // Looking in from outside: the stretch from the face to the back wall.
        let into = interior_spans_on_ray([-30.0, 2.0, 0.0], [1.0, 0.0, 0.0], 34.0, &[room]);
        assert_eq!(into.spans, vec![[25.0, 34.0]]);
        // A ray parallel to a face just outside it misses; one along the face inside it hits.
        assert!(interior_span(&room, [0.0, 4.5, 0.0], [1.0, 0.0, 0.0], 50.0).is_none());
        assert!(interior_span(&room, [0.0, 3.9, 0.0], [1.0, 0.0, 0.0], 50.0).is_some());
        // A grazing touch shorter than the minimum is no span.
        assert!(interior_span(&room, [5.0, 2.0, -9.0], [1.0, 0.0, 0.0], 50.0).is_none());
        // A degenerate box holds no air.
        let flat = InteriorVolume::with_floor([0.0; 3], [1.0, 0.0, 1.0]);
        assert!(interior_span(&flat, [0.0; 3], [1.0, 0.0, 0.0], 10.0).is_none());
    }

    #[test]
    fn past_four_volumes_the_slots_hold_the_four_nearest_the_eye() {
        let storey = |x: f32, k: f32| InteriorVolume::with_floor([x, 1.35 + 3.0 * k, 3.0], [3.0, 1.35, 3.0]);
        let broken = InteriorVolume::with_floor([0.0; 3], [0.0, 1.0, 1.0]);
        // Four or fewer: slot i holds volume i, an unusable box included (it keeps its slot).
        let four = [storey(3.0, 0.0), broken, storey(3.0, 1.0), storey(3.0, 2.0)];
        assert_eq!(
            aerial_interior_slots(&four, [3.0, 7.6, 3.0]),
            [Some(0), Some(1), Some(2), Some(3)]
        );
        assert_eq!(
            aerial_interior_slots(&four[..2], [3.0; 3]),
            [Some(0), Some(1), None, None]
        );
        // Two stacked blocks (six storeys) and a broken box: the eye on the far block's top storey.
        let mut many = vec![broken];
        for x in [3.0, 23.0] {
            for k in 0..3 {
                many.push(storey(x, k as f32));
            }
        }
        let eye = [21.0, 7.6, 3.0];
        let slots = aerial_interior_slots(&many, eye);
        // The far block's three storeys (indices 4..=6, distances 0, 1.9, 4.9) and the near block's top storey
        // (index 3, 15 m away); list order; the broken box is never chosen.
        assert_eq!(slots, [Some(3), Some(4), Some(5), Some(6)]);
        assert!(slots.iter().flatten().any(|&i| many[i].contains(eye)));
        // No usable eye: the first four, as before.
        assert_eq!(
            aerial_interior_slots(&many, [f32::NAN, 0.0, 0.0]),
            [Some(0), Some(1), Some(2), Some(3)]
        );
        // Fewer usable boxes than slots: the rest stay empty.
        let mostly_broken = [broken, broken, broken, broken, storey(3.0, 0.0)];
        assert_eq!(
            aerial_interior_slots(&mostly_broken, eye),
            [Some(4), None, None, None]
        );
    }

    #[test]
    fn overlapping_boxes_are_taken_out_once_and_only_four_are_read() {
        let a = InteriorVolume::with_floor([0.0, 0.0, 0.0], [4.0, 1.0, 1.0]);
        let b = InteriorVolume::with_floor([5.0, 0.0, 0.0], [3.0, 1.0, 1.0]);
        let c = InteriorVolume::with_floor([20.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let r = interior_spans_on_ray([-10.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0, &[c, b, a]);
        // a: [6, 14], b: [12, 18] overlap into [6, 18]; c: [29, 31].
        assert_eq!(r.spans, vec![[6.0, 18.0], [29.0, 31.0]]);
        assert!((r.inside_m - 14.0).abs() < 1.0e-4);
        // A fifth volume farther from the eye than four others is past the composite's slots.
        let far = InteriorVolume::with_floor([50.0, 0.0, 0.0], [1.0, 1.0, 1.0]);
        let filler = InteriorVolume::with_floor([0.0, 50.0, 0.0], [1.0, 1.0, 1.0]);
        let five = [filler, filler, filler, filler, far];
        assert!(
            interior_spans_on_ray([-10.0, 0.0, 0.0], [1.0, 0.0, 0.0], 100.0, &five)
                .spans
                .is_empty()
        );
        // The shader declares the same minimum span.
        let wgsl = include_str!("../shaders/outdoor_sky.wgsl");
        assert!(wgsl.contains("const INTERIOR_MIN_SPAN_M: f32 = 1.0e-3;"));
        assert!((AERIAL_INTERIOR_MIN_SPAN_M - 1.0e-3).abs() < f32::EPSILON);
    }

    /// ADR-7975: with more volumes than slots, the slots hold the ones nearest the eye. Five
    /// passages, the eye in the last one listed: the ray inside it takes no in-scatter (before, the
    /// first four in list order were read and the eye's passage glowed with sky light).
    #[test]
    fn the_eye_in_a_fifth_passage_takes_its_air_out() {
        let passage = |x: f32| InteriorVolume::with_floor([x, 2.0, 0.0], [4.0, 2.0, 4.0]);
        let five = [
            passage(-100.0),
            passage(-50.0),
            passage(50.0),
            passage(100.0),
            passage(200.0),
        ];
        let eye = [200.0, 2.0, 0.0];
        let inside = interior_spans_on_ray(eye, [0.0, 0.0, 1.0], 3.5, &five);
        assert_eq!(inside.spans, vec![[0.0, 3.5]]);
        assert_eq!(
            inside.inscatter_share_near(),
            0.0,
            "the eye's own passage, fifth in the list, takes its air out"
        );
        assert_eq!(aerial_interior_pick(eye, &five), vec![1, 2, 3, 4]);
        // The same ray with the passage first in the list: the same spans.
        let first = [five[4], five[0], five[1], five[2], five[3]];
        assert_eq!(interior_spans_on_ray(eye, [0.0, 0.0, 1.0], 3.5, &first), inside);
    }

    /// ADR-7975: up to four volumes the pick is the list itself (the slots before ADR-7975); past
    /// four it is the nearest by eye-to-box distance, ties by list order, a box with no air last.
    #[test]
    fn the_aerial_pick_is_the_list_up_to_four_and_the_nearest_past_it() {
        let at = |x: f32| InteriorVolume::with_floor([x, 0.0, 0.0], [1.0; 3]);
        let broken = InteriorVolume::with_floor([0.0; 3], [0.0, 1.0, 1.0]);
        let eye = [0.0, 0.0, 0.0];
        for n in 0..=AERIAL_INTERIOR_SLOTS {
            let list: Vec<InteriorVolume> = (0..n).map(|k| at(100.0 - 10.0 * k as f32)).collect();
            assert_eq!(aerial_interior_pick(eye, &list), (0..n).collect::<Vec<_>>());
        }
        // Distances 49, 29, 9, 9 (a tie: list order), 19, and a broken box at the eye.
        let list = [at(50.0), at(30.0), at(10.0), at(-10.0), at(20.0), broken];
        assert_eq!(aerial_interior_pick(eye, &list), vec![1, 2, 3, 4]);
        let tie = [at(10.0), at(10.0), at(10.0), at(10.0), at(10.0)];
        assert_eq!(aerial_interior_pick(eye, &tie), vec![0, 1, 2, 3]);
        // Inside a box counts as distance 0, wherever the eye is in it.
        let big = InteriorVolume::with_floor([500.0, 0.0, 0.0], [600.0, 1.0, 1.0]);
        let list = [at(50.0), at(30.0), at(10.0), at(-10.0), big];
        assert_eq!(aerial_interior_pick(eye, &list), vec![1, 2, 3, 4]);
        // A non-finite eye keeps the list's first four.
        assert_eq!(
            aerial_interior_pick([f32::NAN, 0.0, 0.0], &list),
            vec![0, 1, 2, 3]
        );
    }

    #[test]
    fn interior_clip_rejects_nonfinite_geometry_and_invalid_rays() {
        let room = InteriorVolume::with_floor([0.0; 3], [2.0; 3]);
        for distance in [f32::NAN, f32::INFINITY, -1.0, 0.0] {
            assert_eq!(interior_span(&room, [0.0; 3], [1.0, 0.0, 0.0], distance), None);
        }
        for direction in [[0.0; 3], [f32::NAN, 0.0, 0.0], [f32::INFINITY, 0.0, 0.0]] {
            assert_eq!(interior_span(&room, [0.0; 3], direction, 10.0), None);
        }
        let broken = InteriorVolume::with_floor([f32::NAN, 0.0, 0.0], [2.0; 3]);
        assert_eq!(interior_span(&broken, [0.0; 3], [1.0, 0.0, 0.0], 10.0), None);
        assert_eq!(
            interior_span(&room, [f32::INFINITY, 0.0, 0.0], [1.0, 0.0, 0.0], 10.0),
            None
        );
    }

    /// Walking out of a door: the air under the roof shrinks with the eye's step and reaches zero
    /// at the face, so the in-scatter the composite keeps has no jump (no pop).
    #[test]
    fn walking_out_of_a_door_changes_the_interior_air_smoothly() {
        let room = InteriorVolume::with_floor([0.0, 2.0, 0.0], [5.0, 2.0, 5.0]);
        let wall_x = 300.0;
        let step = 0.05;
        let mut last: Option<f32> = None;
        let mut x = 2.0f32;
        while x < 8.0 {
            let r = interior_spans_on_ray([x, 2.0, 0.0], [1.0, 0.0, 0.0], wall_x - x, &[room]);
            let inside = r.inside_m;
            assert!(
                (inside - (5.0 - x).max(0.0)).abs() < 2.0e-3,
                "x {x}: inside {inside}"
            );
            if let Some(prev) = last {
                assert!(
                    (prev - inside).abs() <= step + 2.0e-3,
                    "x {x}: the interior air jumped from {prev} to {inside}"
                );
            }
            last = Some(inside);
            x += step;
        }
        assert_eq!(last, Some(0.0), "outside the door the ray is the open air's");
    }

    fn cockpit_probe_volume() -> InteriorVolume {
        // Inner air of the hollow shell is |x| < 1.76, |y| < 1.06, |z| < 2.36
        // (8 cm walls on the outer box). The planner insets another 15 cm, so
        // these faces put the outer probes about 11 cm inside the air. The
        // previous box ([1.45, 0.85, 1.95] about y = 0.05) left the ceiling
        // corner on a probe 0.77 m away, past that probe's depth mean.
        InteriorVolume::with_floor([0.0, 0.0, 0.0], [1.80, 1.10, 2.35])
    }

    fn cockpit_spot() -> Vec<crate::lab_frame::LabLocalLight> {
        use crate::lab_frame::{LabLocalLight, LabSpotLight};
        vec![LabLocalLight::Spot(LabSpotLight {
            position: [0.15, 0.72, 1.15],
            direction: [-0.05, -0.15, -1.0],
            color: [0.72, 0.84, 1.0],
            intensity: 4_500.0,
            range: 6.0,
            inner_angle: 0.2,
            outer_angle: 0.48,
            cast_shadows: true,
            source_radius: None,
        })]
    }
}
