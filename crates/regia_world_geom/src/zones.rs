// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/world/lighting.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1099-1175, mark: adapt (Stage A: unchanged).
// Edits: module paths; doc comments with DO27 file and engine names dropped; the wall-clock Instant and the 30 m eye-jump snap are gone (the ease takes the frame clock's dt and holds on dt <= 0); INTERIOR_EV and the outdoor EV are caller arguments, not defaults; the hole-mask cache is the caller's open_cut flag.
// Portions copied from Direct Order 27 (shell-regia/game/src/window_play.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 1689-1752 and 8860-8868, mark: adapt (Stage A: unchanged).
// Edits: the step flag is EaseSettings::depth_blend (on by default); the level-band floors are caller arguments; under a lid the weight follows the same 3-22 m blend when depth blend is on, so a portal crossing does not write the interior floor in one sample; the written floor is eased on the frame clock.

//! One exposure zone, one reverb zone and one interior volume per cooked section (plan row W-24, ADR-8384).
//!
//! [`section_volumes`] walks the cooked sections in order and never stops at four. The exposure ease
//! ([`ExposureEase`]) advances by the frame clock's `dt` and holds when that step is 0 or not finite.
//! [`EaseSettings::depth_blend`] is on by default, so the under-rock weight follows one smoothstep from
//! 3 m to 22 m of overburden, including under a lid.
//!
//! The outdoor and interior EV floors are arguments. This module does not pick a level's exposure.

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;

use crate::cooked::{CookedSection, CookedZone, CookedZoneShape};
use crate::units::mm_to_m;

/// Settings for the under-rock weight and the exposure ease (plan row W-24).
///
/// The depths and the half-life are the adapted shell's constants. The outdoor and interior EV
/// floors are not settings: each call passes them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EaseSettings {
    /// When on (the default), the weight is a smoothstep from [`Self::depth_blend_start_m`] to
    /// [`Self::depth_blend_end_m`], including under a lid. When off, the weight is the shell's step.
    pub depth_blend: bool,
    /// Ease half-life, seconds. A non-positive or non-finite value snaps to the target.
    pub half_life_s: f32,
    /// Overburden depth where the default blend starts, metres.
    pub depth_blend_start_m: f32,
    /// Overburden depth where the default blend reaches 1, metres.
    pub depth_blend_end_m: f32,
    /// With depth blend off, depths at or below this stay at weight 0, metres.
    pub step_depth_m: f32,
    /// Open-cut exposure drop below the outdoor floor, EV ([`interior_exposure_target`]).
    pub open_cut_drop_ev: f32,
    /// Open-cut drop blend start, metres.
    pub open_cut_start_m: f32,
    /// Open-cut drop blend end, metres.
    pub open_cut_end_m: f32,
    /// Covered interior blend start, metres.
    pub interior_blend_start_m: f32,
    /// Covered interior blend end, metres.
    pub interior_blend_end_m: f32,
}

impl Default for EaseSettings {
    fn default() -> Self {
        Self {
            depth_blend: true,
            half_life_s: 0.45,
            depth_blend_start_m: 3.0,
            depth_blend_end_m: 22.0,
            step_depth_m: 2.0,
            open_cut_drop_ev: 1.0,
            open_cut_start_m: 3.0,
            open_cut_end_m: 14.0,
            interior_blend_start_m: 2.0,
            interior_blend_end_m: 14.0,
        }
    }
}

/// One eye sample on a drive through a mouth.
///
/// `depth_m` is overburden: ground height minus the eye's height. A negative or non-finite depth
/// is outdoors. `open_cut` is a shaft or a trench with no lid. `under_lid` is rock over the ceiling.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EyeSample {
    /// Eye position, metres.
    pub position: [f32; 3],
    /// Ground height minus eye height, metres.
    pub depth_m: f32,
    /// The sample sits in an open cut.
    pub open_cut: bool,
    /// Rock covers the ceiling at this sample.
    pub under_lid: bool,
}

/// The exposure ease. It stores the last value and the settings. It does not read a clock.
#[derive(Clone, Debug, PartialEq)]
pub struct ExposureEase {
    settings: EaseSettings,
    value: Option<f32>,
}

impl Default for ExposureEase {
    fn default() -> Self {
        Self::new(EaseSettings::default())
    }
}

impl ExposureEase {
    /// An ease with no sample yet. The first [`Self::step`] takes the target.
    #[must_use]
    pub fn new(settings: EaseSettings) -> Self {
        Self {
            settings,
            value: None,
        }
    }

    /// The last eased value, if a finite target has been stepped or cut.
    #[must_use]
    pub fn value(&self) -> Option<f32> {
        self.value
    }

    /// The settings this ease was built with.
    #[must_use]
    pub fn settings(&self) -> EaseSettings {
        self.settings
    }

    /// Snap to `target` when it is finite. A drive does not call this: a host asks for the cut.
    pub fn cut(&mut self, target: f32) -> f32 {
        if target.is_finite() {
            self.value = Some(target);
        }
        self.value.unwrap_or(target)
    }

    /// One step toward `target` by `dt_s` seconds of the frame clock.
    ///
    /// The first finite target is taken as-is, including when `dt_s` is 0. After that, a
    /// non-positive or non-finite `dt_s` holds the previous value, and so does a non-finite
    /// target. `dt_s` is capped at 1 second. The factor is `1 - 0.5^(dt / half_life)`.
    pub fn step(&mut self, target: f32, dt_s: f32) -> f32 {
        if !target.is_finite() {
            return self.value.unwrap_or(target);
        }
        let Some(prev) = self.value else {
            self.value = Some(target);
            return target;
        };
        if !dt_s.is_finite() || dt_s <= 0.0 {
            return prev;
        }
        let dt = det_min(dt_s, 1.0);
        let half = self.settings.half_life_s;
        let k = if !half.is_finite() || half <= 0.0 {
            1.0
        } else {
            1.0 - math::powf(0.5, dt / half)
        };
        let k = if k.is_finite() {
            det_clamp(k, 0.0, 1.0)
        } else {
            0.0
        };
        let next = prev + (target - prev) * k;
        self.value = Some(next);
        next
    }
}

/// Hermite smoothstep from 0 at `edge0` to 1 at `edge1`.
///
/// A non-finite `x` returns 0. A non-positive or non-finite span is a step at `edge0`.
#[must_use]
pub fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if !x.is_finite() {
        return 0.0;
    }
    let span = edge1 - edge0;
    if !span.is_finite() || span <= 0.0 {
        return if x < edge0 { 0.0 } else { 1.0 };
    }
    let t = det_clamp((x - edge0) / span, 0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The under-rock weight in `0..=1` (adapted from the shell's interior band).
///
/// A non-finite or non-positive `depth_m` is 0. With [`EaseSettings::depth_blend`] on, the weight
/// is [`smoothstep`] from the blend start to the blend end and does not jump under a lid. With it
/// off, the weight is 0 at or below [`EaseSettings::step_depth_m`], 1 under a lid or outside an
/// open cut, and 0 on an open cut.
#[must_use]
pub fn under_rock_weight(depth_m: f32, open_cut: bool, under_lid: bool, settings: &EaseSettings) -> f32 {
    if !depth_m.is_finite() || depth_m <= 0.0 {
        return 0.0;
    }
    if !settings.depth_blend {
        if depth_m <= settings.step_depth_m {
            return 0.0;
        }
        if under_lid || !open_cut {
            return 1.0;
        }
        return 0.0;
    }
    smoothstep(settings.depth_blend_start_m, settings.depth_blend_end_m, depth_m)
}

/// `outdoor + (interior - outdoor) * weight`, with `weight` clamped to `0..=1`.
///
/// A non-finite weight contributes nothing (the outdoor floor).
#[must_use]
pub fn band_exposure(outdoor_ev: f32, interior_ev: f32, weight: f32) -> f32 {
    let w = if weight.is_finite() {
        det_clamp(weight, 0.0, 1.0)
    } else {
        0.0
    };
    outdoor_ev + (interior_ev - outdoor_ev) * w
}

/// The band target at one eye sample: the under-rock weight applied to the two floors.
#[must_use]
pub fn band_target(sample: &EyeSample, outdoor_ev: f32, interior_ev: f32, settings: &EaseSettings) -> f32 {
    let weight = under_rock_weight(sample.depth_m, sample.open_cut, sample.under_lid, settings);
    band_exposure(outdoor_ev, interior_ev, weight)
}

/// The interior exposure target (adapted from the shell's lighting ease).
///
/// A non-finite or non-positive depth returns `outdoor_ev`. An open cut drops
/// [`EaseSettings::open_cut_drop_ev`] below the outdoor floor across the open-cut blend. A covered
/// sample blends from the outdoor floor toward `min(interior_ev, outdoor_ev)`.
#[must_use]
pub fn interior_exposure_target(
    outdoor_ev: f32,
    interior_ev: f32,
    depth_m: f32,
    open_cut: bool,
    settings: &EaseSettings,
) -> f32 {
    if !depth_m.is_finite() || depth_m <= 0.0 {
        return outdoor_ev;
    }
    if open_cut {
        let t = smoothstep(settings.open_cut_start_m, settings.open_cut_end_m, depth_m);
        return outdoor_ev - settings.open_cut_drop_ev * t;
    }
    let t = smoothstep(
        settings.interior_blend_start_m,
        settings.interior_blend_end_m,
        depth_m,
    );
    let inner = det_min(interior_ev, outdoor_ev);
    outdoor_ev + (inner - outdoor_ev) * t
}

/// One cooked section's exposure band, reverb and interior box, in metres.
///
/// `interior_half_m` is the world-axis half extents of the zone (an oriented box's yaw about Y is
/// folded into the axis-aligned box; a capsule is the box around both ends plus the radius).
#[derive(Clone, Debug, PartialEq)]
pub struct SectionVolumes {
    /// The cooked section id.
    pub section: u16,
    /// Exposure band low and high, EV100.
    pub exposure_ev: [f32; 2],
    /// Blend distance, metres.
    pub blend_m: f32,
    /// Reverb preset name (`tube`, `gallery`, `open`, or empty when the section had no zone).
    pub reverb_preset: String,
    /// Reverb cavity size, metres.
    pub reverb_size_m: f32,
    /// Interior box centre, metres.
    pub interior_center_m: [f32; 3],
    /// Interior box half extents on the world axes, metres.
    pub interior_half_m: [f32; 3],
}

/// One [`SectionVolumes`] per section, in section order.
///
/// A section whose id matches a zone uses that zone (the first match). A section with no zone still
/// emits one volume, from its bounds, with a zero exposure band and an empty reverb name. The
/// result length is `sections.len()`. It is not capped at four.
#[must_use]
pub fn section_volumes(sections: &[CookedSection], zones: &[CookedZone]) -> Vec<SectionVolumes> {
    sections
        .iter()
        .map(
            |section| match zones.iter().find(|zone| zone.section == section.id) {
                Some(zone) => volume_from_zone(section, zone),
                None => volume_from_bounds(section),
            },
        )
        .collect()
}

fn volume_from_zone(section: &CookedSection, zone: &CookedZone) -> SectionVolumes {
    let (center, half) = match zone.shape {
        CookedZoneShape::OrientedBox {
            center_mm,
            half_mm,
            yaw,
        } => oriented_box_aabb(center_mm, half_mm, yaw),
        CookedZoneShape::Capsule {
            a_mm,
            b_mm,
            radius_mm,
        } => capsule_aabb(a_mm, b_mm, radius_mm),
    };
    SectionVolumes {
        section: section.id,
        exposure_ev: [zone.exposure[0], zone.exposure[1]],
        blend_m: zone.exposure[2],
        reverb_preset: zone.reverb_preset.clone(),
        reverb_size_m: zone.reverb_size_m,
        interior_center_m: center,
        interior_half_m: half,
    }
}

fn volume_from_bounds(section: &CookedSection) -> SectionVolumes {
    let (center, half) = bounds_aabb(section.bounds_mm);
    SectionVolumes {
        section: section.id,
        exposure_ev: [0.0, 0.0],
        blend_m: 0.0,
        reverb_preset: String::new(),
        reverb_size_m: 0.0,
        interior_center_m: center,
        interior_half_m: half,
    }
}

/// Axis-aligned half extents of a yaw-about-Y box. Extents are absolute.
fn oriented_box_aabb(center_mm: [i32; 3], half_mm: [i32; 3], yaw: f32) -> ([f32; 3], [f32; 3]) {
    let c = math::cos(yaw).abs();
    let s = math::sin(yaw).abs();
    let hx = mm_to_m(0, half_mm[0]).abs();
    let hy = mm_to_m(0, half_mm[1]).abs();
    let hz = mm_to_m(0, half_mm[2]).abs();
    let center = [
        mm_to_m(0, center_mm[0]),
        mm_to_m(0, center_mm[1]),
        mm_to_m(0, center_mm[2]),
    ];
    let half = [c * hx + s * hz, hy, s * hx + c * hz];
    (center, half)
}

fn capsule_aabb(a_mm: [i32; 3], b_mm: [i32; 3], radius_mm: i32) -> ([f32; 3], [f32; 3]) {
    let a = [mm_to_m(0, a_mm[0]), mm_to_m(0, a_mm[1]), mm_to_m(0, a_mm[2])];
    let b = [mm_to_m(0, b_mm[0]), mm_to_m(0, b_mm[1]), mm_to_m(0, b_mm[2])];
    let r = mm_to_m(0, radius_mm).abs();
    let center = [0, 1, 2].map(|i| {
        let lo = det_min(a[i], b[i]) - r;
        let hi = det_max(a[i], b[i]) + r;
        (lo + hi) * 0.5
    });
    let half = [0, 1, 2].map(|i| {
        let lo = det_min(a[i], b[i]) - r;
        let hi = det_max(a[i], b[i]) + r;
        (hi - lo) * 0.5
    });
    (center, half)
}

fn bounds_aabb(bounds_mm: [[i32; 3]; 2]) -> ([f32; 3], [f32; 3]) {
    let a = [
        mm_to_m(0, bounds_mm[0][0]),
        mm_to_m(0, bounds_mm[0][1]),
        mm_to_m(0, bounds_mm[0][2]),
    ];
    let b = [
        mm_to_m(0, bounds_mm[1][0]),
        mm_to_m(0, bounds_mm[1][1]),
        mm_to_m(0, bounds_mm[1][2]),
    ];
    let center = [0, 1, 2].map(|i| {
        let lo = det_min(a[i], b[i]);
        let hi = det_max(a[i], b[i]);
        (lo + hi) * 0.5
    });
    let half = [0, 1, 2].map(|i| {
        let lo = det_min(a[i], b[i]);
        let hi = det_max(a[i], b[i]);
        (hi - lo) * 0.5
    });
    (center, half)
}
