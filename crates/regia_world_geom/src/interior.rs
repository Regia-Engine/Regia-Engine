// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (shell-regia/game/src/cam_clip.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 23-84, 47-55, 88-189, 195-480, 545-713, 732-847, mark: adapt (Stage A: presentation).
// Edits: tuning is CameraInteriorParams; [f32; 3]; det_* and regia_detsim::math; probe logs, drawn-wall grid and product names dropped; swept clip is segment plus push_disc.
// Portions copied from Direct Order 27 (ir-sim/src/tunnels.rs, commit c06c17084, tag regia-absorb-3), ADR-8012.
// Lines 89-96, 291-334, mark: adapt (Stage A: compat keeps its copies; this module is the presentation replacement).
// Edits: camera_floor keeps the highest Floor span inside the caller's reach; mesh_spans and the ray clip call the native queries.
// Requirements only (no source text): shell-regia/game/src/window_play.rs lines 7630-7679 and 11340-11377 (open-cut hole at the feet, world build).

//! Interior chase camera (presentation).
//!
//! The zone machine, the mouth blend and the boom length are pure functions of the frame clock.
//! `InteriorProfile::place` (feature `stage_b`) sweeps a sphere with `segment` and `push_disc` and
//! eases the boom at junctions. Nothing here is a lockstep tick input: the simulation keeps the
//! Stage A `camera_floor` / `camera_clip` copies, and this profile only moves the presented eye.
//!
//! Wall ease lives on `InteriorProfile::place`. The agent method steps the zone machine only.

use regia_detsim::float::{det_clamp, det_max, det_min};
use regia_detsim::math;

#[cfg(feature = "stage_b")]
use crate::WorldGeom;
#[cfg(feature = "stage_b")]
use crate::query::{Band, GeomState, SpanBuf, SurfaceKind};
#[cfg(feature = "stage_b")]
use crate::query_segment::FaceMask;

/// Presentation tuning for the interior chase camera.
///
/// The defaults are the documented chase profile: a 4.5 m exterior boom, a 2.5 m interior boom,
/// a 0.5 s mouth blend, a 0.5 m sphere, and a 12 m floor reach. A host may replace any of them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct CameraInteriorParams {
    /// Clear space kept from rock and ground, metres.
    pub cam_margin_m: f32,
    /// Feet this far below the lattice are under cover when a mesh floor is in reach, metres.
    pub cover_m: f32,
    /// Most the chase eye rises over the pivot to clear a rim, metres.
    pub rise_cap_m: f32,
    /// Most a first-person eye is lifted off a slope, metres.
    pub fp_lift_cap_m: f32,
    /// A boom shorter than this after a terrain pull-in is cramped, metres.
    pub cramped_boom_m: f32,
    /// Gap kept over the ground when the eye lifts over a rim, metres.
    pub rim_clearance_m: f32,
    /// Height kept over a tunnel floor, metres.
    pub floor_clearance_m: f32,
    /// Step of the open-ground boom march, metres.
    pub march_step_m: f32,
    /// Gap kept under a lid, metres.
    pub lid_margin_m: f32,
    /// Step of the boom march through interior space, metres.
    pub space_step_m: f32,
    /// Highest the pivot and the eye ride over the floor under a lid, metres.
    pub lid_eye_cap_m: f32,
    /// A hull this close to a floor is still in that floor's space, metres.
    pub hull_hover_m: f32,
    /// Open-ground boom the blend is measured against, metres.
    pub exterior_boom_m: f32,
    /// Boom length under rock, metres.
    pub interior_boom_m: f32,
    /// Mouth blend duration on the frame clock, seconds.
    pub mouth_blend_s: f32,
    /// Sphere radius of the interior boom, metres.
    pub interior_sphere_m: f32,
    /// Open samples before an interior zone becomes exterior.
    pub zone_exit_ticks: u32,
    /// Distance along an opening where the mouth blend runs, metres.
    pub mouth_band_m: f32,
    /// Look-ahead of the cover test, metres.
    pub lead_m: f32,
    /// How long cover is held after the body stops reading as covered, seconds.
    pub cover_exit_hold_s: f32,
    /// Time constant of a boom pulling in against rock, seconds.
    pub tau_in_s: f32,
    /// Time constant of a boom easing back out under cover, seconds.
    pub tau_out_cover_s: f32,
    /// Time constant of a boom easing back out in the open, seconds.
    pub tau_out_open_s: f32,
    /// Once a pull-in is already this close to the clip, it finishes. A longer gap follows
    /// `tau_in_s` and does not cross a blocked span in one frame.
    pub in_slack_m: f32,
    /// A pivot jump farther than this restarts the filter, metres.
    pub pivot_jump_m: f32,
    /// Open frames before a lid gap counts as leaving.
    pub exit_confirm_frames: u32,
    /// A clip shorter or longer than the previous boom by more than this is eased, metres.
    pub clip_ease_m: f32,
    /// Default reach of `camera_floor`: a floor farther below the eye is ignored, metres.
    pub floor_reach_m: f32,
}

impl Default for CameraInteriorParams {
    fn default() -> Self {
        Self {
            cam_margin_m: 0.35,
            cover_m: 3.0,
            rise_cap_m: 3.0,
            fp_lift_cap_m: 1.0,
            cramped_boom_m: 2.5,
            rim_clearance_m: 1.5,
            floor_clearance_m: 0.3,
            march_step_m: 0.5,
            lid_margin_m: 0.6,
            space_step_m: 0.25,
            lid_eye_cap_m: 5.0,
            hull_hover_m: 5.0,
            exterior_boom_m: 4.5,
            interior_boom_m: 2.5,
            mouth_blend_s: 0.5,
            interior_sphere_m: 0.5,
            zone_exit_ticks: 30,
            mouth_band_m: 18.0,
            lead_m: 2.0,
            cover_exit_hold_s: 0.5,
            tau_in_s: 0.06,
            tau_out_cover_s: 0.6,
            tau_out_open_s: 0.15,
            in_slack_m: 0.15,
            pivot_jump_m: 8.0,
            exit_confirm_frames: 6,
            clip_ease_m: 0.75,
            floor_reach_m: 12.0,
        }
    }
}

/// Where the chase camera is.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CamZone {
    /// Open ground, long boom.
    #[default]
    Exterior,
    /// Mouth band, or the hold after leaving cover.
    Mouth,
    /// Under a lid, short boom.
    Interior,
}

impl CamZone {
    /// Stable name: `exterior`, `mouth` or `interior`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exterior => "exterior",
            Self::Mouth => "mouth",
            Self::Interior => "interior",
        }
    }

    /// Parse [`Self::as_str`]. Any other text is `None`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "exterior" => Some(Self::Exterior),
            "mouth" => Some(Self::Mouth),
            "interior" => Some(Self::Interior),
            _ => None,
        }
    }
}

/// One centreline sample of an opening, for [`near_mouth`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RampSample {
    /// Floor point, metres.
    pub p: [f32; 3],
    /// Arc length from the passage start, metres.
    pub s: f32,
}

/// An opening the mouth test measures along.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RampSpan<'a> {
    /// Floor width, metres.
    pub width_m: f32,
    /// Centreline samples in order.
    pub samples: &'a [RampSample],
}

/// One frame of the chase rig.
#[cfg(feature = "stage_b")]
pub struct ChaseFrame<'a> {
    /// Body feet, metres.
    pub feet: [f32; 3],
    /// Look pivot, metres.
    pub pivot: [f32; 3],
    /// Rig eye before the interior profile, metres.
    pub desired: [f32; 3],
    /// Eye the open-ground camera would use this frame, metres.
    pub exterior: [f32; 3],
    /// Frame duration, seconds. The blend uses this clock.
    pub dt_s: f32,
    /// True inside an open cut: the heightfield is not a floor there.
    pub hole_at: &'a dyn Fn(f32, f32) -> bool,
    /// Openings for the mouth band. Empty when the caller has no layout.
    pub ramps: &'a [RampSpan<'a>],
}

/// The presented eye for one frame.
#[cfg(feature = "stage_b")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ChasePose {
    /// Eye position, metres.
    pub eye: [f32; 3],
    /// Look point, metres. The pivot when the rig was not lowered.
    pub look: [f32; 3],
    /// Distance from `look` to `eye`, metres.
    pub boom_m: f32,
    /// Zone after this frame.
    pub zone: CamZone,
    /// Mouth blend, 0 open to 1 interior.
    pub blend: f32,
    /// Cover flag after the exit hold.
    pub covered: bool,
}

/// Round-trip of the zone machine. Wall ease is not in this snapshot; it lives on `InteriorProfile::place`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct InteriorSnapshot {
    /// Zone after the step.
    pub zone: CamZone,
    /// Mouth blend, 0 to 1.
    pub blend: f32,
    /// Boom length for this blend and exterior arm, metres.
    pub boom_m: f32,
    /// Open samples counted in the mouth hold.
    pub open_ticks: u32,
    /// Cover flag after the exit hold.
    pub covered: bool,
    /// The body has been under a lid.
    pub seen_inside: bool,
    /// A confirmed lid gap is blending back out.
    pub exiting: bool,
    /// Open frames since the body left a lid.
    pub gap: u32,
    /// The next step is still the spawn cut.
    pub seed: bool,
    /// Seconds of raw-uncovered time accumulated toward releasing cover.
    pub cover_timer_s: f32,
    /// Steady open ground: the eye is the exterior eye.
    pub passthrough: bool,
}

/// Live third-person interior profile.
///
/// The mouth zone enters at once and leaves only after [`CameraInteriorParams::zone_exit_ticks`]
/// of open ground. The boom length eases between the exterior arm and the interior boom over
/// [`CameraInteriorParams::mouth_blend_s`].
#[derive(Clone, Debug)]
pub struct InteriorProfile {
    zone: CamZone,
    open_ticks: u32,
    covered: bool,
    cover_timer_s: f32,
    blend: f32,
    seen_inside: bool,
    exiting: bool,
    gap: u32,
    seed: bool,
    #[cfg(feature = "stage_b")]
    boom_len: Option<f32>,
    #[cfg(feature = "stage_b")]
    last_pivot: Option<[f32; 3]>,
    #[cfg(feature = "stage_b")]
    last_feet: Option<[f32; 3]>,
}

impl Default for InteriorProfile {
    fn default() -> Self {
        Self::new()
    }
}

impl InteriorProfile {
    /// Exterior zone, blend 0, seed set so the next step can be a cut.
    #[must_use]
    pub fn new() -> Self {
        Self {
            zone: CamZone::Exterior,
            open_ticks: 0,
            covered: false,
            cover_timer_s: 0.0,
            blend: 0.0,
            seen_inside: false,
            exiting: false,
            gap: 0,
            seed: true,
            #[cfg(feature = "stage_b")]
            boom_len: None,
            #[cfg(feature = "stage_b")]
            last_pivot: None,
            #[cfg(feature = "stage_b")]
            last_feet: None,
        }
    }

    /// Same as [`Self::new`].
    pub fn reset(&mut self) {
        *self = Self::new();
    }

    /// Zone after the last step.
    #[must_use]
    pub fn zone(&self) -> CamZone {
        self.zone
    }

    /// Blend after the last step.
    #[must_use]
    pub fn blend(&self) -> f32 {
        self.blend
    }

    /// Rebuild a profile from a snapshot. The wall-ease filter starts empty.
    #[must_use]
    pub fn restore(snap: &InteriorSnapshot) -> Self {
        Self {
            zone: snap.zone,
            open_ticks: snap.open_ticks,
            covered: snap.covered,
            cover_timer_s: snap.cover_timer_s,
            blend: det_clamp(snap.blend, 0.0, 1.0),
            seen_inside: snap.seen_inside,
            exiting: snap.exiting,
            gap: snap.gap,
            seed: snap.seed,
            #[cfg(feature = "stage_b")]
            boom_len: None,
            #[cfg(feature = "stage_b")]
            last_pivot: None,
            #[cfg(feature = "stage_b")]
            last_feet: None,
        }
    }

    /// Advance the zone and the blend by one frame.
    ///
    /// `dt_s` is frame time. A non-finite value counts as zero. `exterior_boom_m` is the arm the
    /// blend shortens. The first step is a cut when `under_lid` or `near_mouth` is already set:
    /// the blend snaps to the target. Later steps move at most `dt_s / mouth_blend_s`.
    pub fn step(
        &mut self,
        dt_s: f32,
        under_lid: bool,
        near_mouth: bool,
        exterior_boom_m: f32,
        params: &CameraInteriorParams,
    ) -> InteriorSnapshot {
        let dt = if dt_s.is_finite() { det_max(dt_s, 0.0) } else { 0.0 };
        self.zone_push(under_lid, near_mouth, params.zone_exit_ticks);
        let held = self.cover(under_lid, dt, params.cover_exit_hold_s);
        if under_lid {
            self.seen_inside = true;
            self.exiting = false;
            self.gap = 0;
        } else if self.seen_inside {
            self.gap = self.gap.saturating_add(1);
            if self.gap > params.exit_confirm_frames {
                self.exiting = true;
            }
        }
        let target = self.target(under_lid, near_mouth, held);
        if self.seed {
            self.seed = false;
            if under_lid || near_mouth {
                self.blend = target;
            }
        }
        let step = mouth_blend(dt, params.mouth_blend_s);
        let delta = det_clamp(target - self.blend, -step, step);
        self.blend = det_clamp(self.blend + delta, 0.0, 1.0);
        let passthrough = self.blend <= 1.0e-4 && !under_lid && !near_mouth;
        if passthrough {
            self.blend = 0.0;
            self.exiting = false;
            self.seen_inside = false;
            self.gap = 0;
        }
        let boom_m = if passthrough {
            exterior_boom_m
        } else {
            blended_boom(exterior_boom_m, self.blend, params)
        };
        self.snapshot(boom_m, passthrough)
    }

    /// The zone machine as it stands. Does not advance the frame clock.
    ///
    /// A fresh profile reports the exterior boom and `passthrough`. The wall-ease filter is not part
    /// of this view; it lives on `place`.
    #[must_use]
    pub fn view(&self, exterior_boom_m: f32, params: &CameraInteriorParams) -> InteriorSnapshot {
        let passthrough = self.blend <= 1.0e-4 && !self.seen_inside && self.zone == CamZone::Exterior;
        let boom_m = if passthrough {
            exterior_boom_m
        } else {
            blended_boom(exterior_boom_m, self.blend, params)
        };
        self.snapshot(boom_m, passthrough)
    }

    /// Presented eye for one frame.
    ///
    /// Fully outside, the result is `frame.exterior`. Under a lid or in a mouth the boom shortens,
    /// a lid lowers the rig, and a sphere sweep stops the eye short of rock. A wall that pulls the
    /// boom in by more than [`CameraInteriorParams::clip_ease_m`] eases in; a sudden opening eases out.
    #[cfg(feature = "stage_b")]
    pub fn place(
        &mut self,
        geom: &WorldGeom,
        state: &GeomState,
        frame: &ChaseFrame<'_>,
        params: &CameraInteriorParams,
    ) -> ChasePose {
        let _span = tracing::debug_span!("interior_camera").entered();
        let lead = self.lead(frame.feet, params);
        let raw = tunnel_body(geom, state, frame.feet, params) || tunnel_body(geom, state, lead, params);
        let near = near_mouth(frame.feet, frame.ramps, params) || near_mouth(lead, frame.ramps, params);
        let arm = len3(sub(frame.desired, frame.pivot));
        let snap = self.step(frame.dt_s, raw, near, arm, params);
        if snap.passthrough {
            self.sync(frame.pivot, frame.exterior);
            return ChasePose {
                eye: frame.exterior,
                look: frame.pivot,
                boom_m: len3(sub(frame.exterior, frame.pivot)),
                zone: snap.zone,
                blend: snap.blend,
                covered: snap.covered,
            };
        }
        let (look, shaped) = shape_interior(
            geom,
            state,
            frame.feet,
            frame.pivot,
            frame.desired,
            snap.blend,
            params,
        );
        let covered = raw || snap.covered;
        let clipped = clip_profile(geom, state, covered, frame, look, shaped, params);
        let clip_len = len3(sub(clipped, look));
        let prev = self.boom_len.unwrap_or(clip_len);
        let mut eye = if clip_len + params.clip_ease_m < prev {
            let _eased = self.ease(frame.dt_s, true, look, clipped, params);
            let want = self.boom_len.unwrap_or(clip_len);
            let kept = last_clear_length(geom, state, look, clipped, prev, want, params);
            self.boom_len = Some(kept);
            if clip_len < 1.0e-3 {
                clipped
            } else {
                add(look, scale(sub(clipped, look), kept / clip_len))
            }
        } else if prev + params.clip_ease_m < clip_len {
            self.ease(frame.dt_s, covered, look, clipped, params)
        } else {
            self.sync(look, clipped);
            clipped
        };
        if !in_camera_space(geom, state, eye, params)
            && in_camera_space(geom, state, clipped, params)
            && !sphere_blocked(geom, state, clipped, look[1], params.interior_sphere_m, params)
        {
            eye = clipped;
        }
        let separated = separate_sphere(geom, state, look, frame.desired, eye, params);
        if separated != eye {
            self.boom_len = Some(len3(sub(separated, look)));
            eye = separated;
        }
        ChasePose {
            boom_m: len3(sub(eye, look)),
            eye,
            look,
            zone: snap.zone,
            blend: snap.blend,
            covered,
        }
    }

    fn zone_push(&mut self, under_lid: bool, near_mouth: bool, exit_ticks: u32) {
        match (under_lid, near_mouth) {
            (true, near) => {
                self.zone = if near { CamZone::Mouth } else { CamZone::Interior };
                self.open_ticks = 0;
            }
            (false, true) => {
                self.zone = CamZone::Mouth;
                self.open_ticks = 0;
            }
            (false, false) => match self.zone {
                CamZone::Exterior => {}
                CamZone::Interior => {
                    self.zone = CamZone::Mouth;
                    self.open_ticks = 1;
                }
                CamZone::Mouth => {
                    self.open_ticks = self.open_ticks.saturating_add(1);
                    if self.open_ticks >= exit_ticks {
                        self.zone = CamZone::Exterior;
                        self.open_ticks = 0;
                    }
                }
            },
        }
    }

    fn cover(&mut self, raw: bool, dt: f32, hold_s: f32) -> bool {
        if raw {
            self.covered = true;
            self.cover_timer_s = 0.0;
        } else if self.covered {
            self.cover_timer_s += dt;
            if self.cover_timer_s >= hold_s {
                self.covered = false;
            }
        }
        self.covered
    }

    fn target(&self, under_lid: bool, near_mouth: bool, held: bool) -> f32 {
        let open = self.zone == CamZone::Exterior || self.blend <= 1.0e-4;
        if !under_lid && (self.exiting || (!near_mouth && open)) {
            0.0
        } else if under_lid || near_mouth || held || self.zone != CamZone::Exterior {
            1.0
        } else {
            0.0
        }
    }

    fn snapshot(&self, boom_m: f32, passthrough: bool) -> InteriorSnapshot {
        InteriorSnapshot {
            zone: self.zone,
            blend: self.blend,
            boom_m,
            open_ticks: self.open_ticks,
            covered: self.covered,
            seen_inside: self.seen_inside,
            exiting: self.exiting,
            gap: self.gap,
            seed: self.seed,
            cover_timer_s: self.cover_timer_s,
            passthrough,
        }
    }

    #[cfg(feature = "stage_b")]
    fn lead(&mut self, feet: [f32; 3], params: &CameraInteriorParams) -> [f32; 3] {
        let prev = self.last_feet.replace(feet);
        let Some(prev) = prev else {
            return feet;
        };
        let dx = feet[0] - prev[0];
        let dz = feet[2] - prev[2];
        let n = len_xz(dx, dz);
        if n < 1.0e-4 || n > params.pivot_jump_m {
            return feet;
        }
        let s = params.lead_m / n;
        [feet[0] + dx * s, feet[1], feet[2] + dz * s]
    }

    #[cfg(feature = "stage_b")]
    fn sync(&mut self, pivot: [f32; 3], eye: [f32; 3]) {
        self.last_pivot = Some(pivot);
        self.boom_len = Some(len3(sub(eye, pivot)));
    }

    #[cfg(feature = "stage_b")]
    fn ease(
        &mut self,
        dt: f32,
        covered: bool,
        pivot: [f32; 3],
        clipped: [f32; 3],
        params: &CameraInteriorParams,
    ) -> [f32; 3] {
        if self
            .last_pivot
            .is_some_and(|p| len3(sub(p, pivot)) > params.pivot_jump_m)
        {
            self.boom_len = None;
        }
        self.last_pivot = Some(pivot);
        let boom = sub(clipped, pivot);
        let target = len3(boom);
        if target < 1.0e-3 {
            self.boom_len = Some(0.0);
            return clipped;
        }
        let prev = self.boom_len.unwrap_or(target);
        let tau = if target < prev {
            params.tau_in_s
        } else if covered {
            params.tau_out_cover_s
        } else {
            params.tau_out_open_s
        };
        let dt = det_max(dt, 0.0);
        let a = if tau <= 1.0e-8 {
            1.0
        } else {
            1.0 - math::exp(-dt / tau)
        };
        let mut len = prev + (target - prev) * a;
        // Finish only a pull-in that is already inside the slack band. Clamping every
        // longer gap down to the clip in one frame is the snap the junction chase rejects.
        if target < prev && len <= target + params.in_slack_m {
            len = target;
        }
        len = det_min(len, det_max(target, prev));
        self.boom_len = Some(len);
        add(pivot, scale(boom, len / target))
    }
}

/// Mouth blend factor for `elapsed_s` of frame time. Zero when the duration is not positive.
#[must_use]
pub fn mouth_blend(elapsed_s: f32, mouth_blend_s: f32) -> f32 {
    if !elapsed_s.is_finite() || !mouth_blend_s.is_finite() || mouth_blend_s <= 0.0 || elapsed_s <= 0.0 {
        return 0.0;
    }
    det_clamp(elapsed_s / mouth_blend_s, 0.0, 1.0)
}

/// Boom length at `blend`: the exterior default shortens to the interior boom.
#[must_use]
pub fn boom_for(blend: f32, params: &CameraInteriorParams) -> f32 {
    let blend = det_clamp(blend, 0.0, 1.0);
    params.exterior_boom_m + (params.interior_boom_m - params.exterior_boom_m) * blend
}

/// Eye and look after the mouth blend, with no lid cap and no wall sweep.
///
/// Look stays at `pivot`. Blend 0 returns `desired`. This is the presentation shorten a caller
/// uses when it has no cooked world; `place` adds the lid cap and the sphere sweep.
#[must_use]
pub fn shaped_boom(
    pivot: [f32; 3],
    desired: [f32; 3],
    blend: f32,
    params: &CameraInteriorParams,
) -> ([f32; 3], [f32; 3]) {
    let blend = det_clamp(blend, 0.0, 1.0);
    if blend <= 0.0 || !finite3(pivot) || !finite3(desired) {
        return (pivot, desired);
    }
    let boom = [
        desired[0] - pivot[0],
        desired[1] - pivot[1],
        desired[2] - pivot[2],
    ];
    let len = math::sqrt(boom[0] * boom[0] + boom[1] * boom[1] + boom[2] * boom[2]);
    if len < 1.0e-4 {
        return (pivot, desired);
    }
    let short = blended_boom(len, blend, params);
    let k = short / len;
    (
        pivot,
        [
            pivot[0] + boom[0] * k,
            pivot[1] + boom[1] * k,
            pivot[2] + boom[2] * k,
        ],
    )
}

/// Shorten `exterior_len` toward the interior boom.
///
/// Blend 0 leaves `exterior_len` unchanged. An arm equal to the exterior default goes through
/// [`boom_for`]. A shorter arm shortens toward the smaller of the two lengths.
#[must_use]
pub fn blended_boom(exterior_len: f32, blend: f32, params: &CameraInteriorParams) -> f32 {
    let blend = det_clamp(blend, 0.0, 1.0);
    if (exterior_len - params.exterior_boom_m).abs() <= 1.0e-4 {
        return boom_for(blend, params);
    }
    let interior = det_min(params.interior_boom_m, exterior_len);
    exterior_len + (interior - exterior_len) * blend
}

/// Scale `pivot` and `eye` by one factor so the higher of the two stays within `cap_m` of `floor_y`.
#[must_use]
pub fn lid_capped(floor_y: f32, pivot: [f32; 3], eye: [f32; 3], cap_m: f32) -> ([f32; 3], [f32; 3]) {
    let top = det_max(pivot[1] - floor_y, eye[1] - floor_y);
    if !top.is_finite() || top <= cap_m || top <= 0.0 {
        return (pivot, eye);
    }
    let k = cap_m / top;
    let squash = |p: [f32; 3]| [p[0], floor_y + (p[1] - floor_y) * k, p[2]];
    (squash(pivot), squash(eye))
}

/// True in the mouth band: within the band along an opening, inside its half width, and within 8 m of the ramp height.
#[must_use]
pub fn near_mouth(p: [f32; 3], ramps: &[RampSpan<'_>], params: &CameraInteriorParams) -> bool {
    if !finite3(p) {
        return false;
    }
    for ramp in ramps {
        let half = ramp.width_m * 0.5;
        let Some(top) = ramp.samples.last() else {
            continue;
        };
        let top_s = top.s;
        if ramp.samples.len() < 2 {
            continue;
        }
        for w in ramp.samples.windows(2) {
            let (dist, t) = xz_seg_dist(p, w[0].p, w[1].p);
            if dist > half {
                continue;
            }
            let s = w[0].s + (w[1].s - w[0].s) * t;
            let y = w[0].p[1] + (w[1].p[1] - w[0].p[1]) * t;
            if (top_s - s).abs() <= params.mouth_band_m && (p[1] - y).abs() <= 8.0 {
                return true;
            }
        }
    }
    false
}

/// Highest `Floor` span at `(x, z)` with `floor_y <= y + 0.5` and `floor_y >= y - reach_m`.
///
/// `reach_m` is the caller's tolerance. A negative or non-finite reach returns `None`. Lattice
/// ground is not a floor. A chase on the plain uses a reach of 12 m so a tube 30 m below is ignored.
#[cfg(feature = "stage_b")]
#[must_use]
pub fn camera_floor(
    geom: &WorldGeom,
    state: &GeomState,
    x: f32,
    z: f32,
    y: f32,
    reach_m: f32,
) -> Option<f32> {
    if !x.is_finite() || !z.is_finite() || !y.is_finite() || !reach_m.is_finite() || reach_m < 0.0 {
        return None;
    }
    let mut buf = SpanBuf::default();
    geom.floors_at(state, x, z, &mut buf);
    let mut best: Option<f32> = None;
    for span in buf.spans() {
        if span.floor.kind != SurfaceKind::Floor {
            continue;
        }
        let fy = span.floor.y;
        if fy <= y + 0.5 && fy >= y - reach_m {
            best = Some(match best {
                Some(prev) => det_max(prev, fy),
                None => fy,
            });
        }
    }
    best
}

/// Mesh floor spans at `(x, z)`, bottom to top, as `(floor_y, lid_y)`.
///
/// Allocates. Presentation only; a tick uses the stack buffer inside [`camera_floor`].
#[cfg(feature = "stage_b")]
#[must_use]
pub fn mesh_spans(geom: &WorldGeom, state: &GeomState, x: f32, z: f32) -> Vec<(f32, Option<f32>)> {
    let mut buf = SpanBuf::default();
    geom.floors_at(state, x, z, &mut buf);
    buf.spans()
        .iter()
        .filter(|span| span.floor.kind == SurfaceKind::Floor)
        .map(|span| (span.floor.y, span.ceiling.map(|lid| lid.y)))
        .collect()
}

/// True when `p` is over a tunnel floor and under its lid, over an open floor, or in the air over the lattice.
///
/// A point under the lattice and not under a lid is rock.
#[cfg(feature = "stage_b")]
#[must_use]
pub fn in_camera_space(
    geom: &WorldGeom,
    state: &GeomState,
    p: [f32; 3],
    params: &CameraInteriorParams,
) -> bool {
    if !finite3(p) {
        return false;
    }
    let mut buf = SpanBuf::default();
    geom.floors_at(state, p[0], p[2], &mut buf);
    let mut lattice = None;
    let mut saw_floor = false;
    for span in buf.spans() {
        if span.floor.kind == SurfaceKind::Ground {
            lattice = Some(span.floor.y);
            continue;
        }
        if span.floor.kind != SurfaceKind::Floor {
            continue;
        }
        saw_floor = true;
        if p[1] < span.floor.y - 0.1 {
            continue;
        }
        match span.ceiling {
            Some(lid) if p[1] <= lid.y - params.lid_margin_m => return true,
            Some(_) => {}
            None => return true,
        }
    }
    let ground = lattice.unwrap_or_else(|| geom.support(state, p[0], p[2], p[1]).floor.y);
    if saw_floor && p[1] < ground + params.cam_margin_m {
        return false;
    }
    p[1] >= ground + params.cam_margin_m
}

/// True when a lid is over `p`, or the body is well under the lattice and a mesh floor is in reach.
#[cfg(feature = "stage_b")]
#[must_use]
pub fn tunnel_body(geom: &WorldGeom, state: &GeomState, p: [f32; 3], params: &CameraInteriorParams) -> bool {
    if !finite3(p) {
        return false;
    }
    if lid_over(geom, state, p) {
        return true;
    }
    let lattice = lattice_y(geom, state, p[0], p[2], p[1]);
    lattice - p[1] > params.cover_m
        && camera_floor(geom, state, p[0], p[2], p[1], params.floor_reach_m).is_some()
}

/// Memoryless segment pull-in along `pivot -> eye`, stopping `margin_m` short of the first wall or roof.
///
/// This is the single-ray camera the interior profile replaces. It has no blend and no ease, so a
/// wall that enters the ray moves the eye by the whole remaining boom in one frame.
#[cfg(feature = "stage_b")]
#[must_use]
pub fn single_ray_clip(
    geom: &WorldGeom,
    state: &GeomState,
    pivot: [f32; 3],
    eye: [f32; 3],
    margin_m: f32,
) -> [f32; 3] {
    if !finite3(pivot) || !finite3(eye) {
        return eye;
    }
    let boom = sub(eye, pivot);
    let len = len3(boom);
    if len < 1.0e-3 {
        return eye;
    }
    let Some(hit) = geom.segment(state, pivot, eye, FaceMask::NO_FLOORS) else {
        return eye;
    };
    let keep = det_max(hit.t * len - det_max(margin_m, 0.0), 0.0) / len;
    add(pivot, scale(boom, keep))
}

/// How far to lift a first-person eye off a slope.
///
/// Zero under cover and inside `hole_at`. Never more than [`CameraInteriorParams::fp_lift_cap_m`].
#[cfg(feature = "stage_b")]
#[must_use]
pub fn first_person_lift(
    geom: &WorldGeom,
    state: &GeomState,
    feet: [f32; 3],
    eye: [f32; 3],
    clearance_m: f32,
    hole_at: &dyn Fn(f32, f32) -> bool,
    params: &CameraInteriorParams,
) -> f32 {
    if tunnel_body(geom, state, feet, params) {
        return 0.0;
    }
    let Some(ground) = solid_ground(geom, state, hole_at, eye[0], eye[2], eye[1]) else {
        return 0.0;
    };
    det_clamp(ground + clearance_m - eye[1], 0.0, params.fp_lift_cap_m)
}

/// Feet a hull's chase camera uses: the floor under it when the hull is within the hover band.
#[cfg(feature = "stage_b")]
#[must_use]
pub fn hull_feet(
    geom: &WorldGeom,
    state: &GeomState,
    pos: [f32; 3],
    params: &CameraInteriorParams,
) -> [f32; 3] {
    match camera_floor(geom, state, pos[0], pos[2], pos[1], params.floor_reach_m) {
        Some(fy) if pos[1] - fy <= params.hull_hover_m && pos[1] >= fy => [pos[0], fy, pos[2]],
        _ => pos,
    }
}

#[cfg(feature = "stage_b")]
fn shape_interior(
    geom: &WorldGeom,
    state: &GeomState,
    feet: [f32; 3],
    pivot: [f32; 3],
    desired: [f32; 3],
    blend: f32,
    params: &CameraInteriorParams,
) -> ([f32; 3], [f32; 3]) {
    let blend = det_clamp(blend, 0.0, 1.0);
    if blend <= 0.0 {
        return (pivot, desired);
    }
    let boom = sub(desired, pivot);
    let len = len3(boom);
    if len < 1.0e-4 {
        return (pivot, desired);
    }
    let short = blended_boom(len, blend, params);
    let eye = add(pivot, scale(boom, short / len));
    let Some(floor_y) = camera_floor(geom, state, feet[0], feet[2], pivot[1], params.floor_reach_m) else {
        return (pivot, eye);
    };
    let (capped_pivot, capped_eye) = lid_capped(floor_y, pivot, eye, params.lid_eye_cap_m);
    let look = [
        pivot[0] + (capped_pivot[0] - pivot[0]) * blend,
        pivot[1] + (capped_pivot[1] - pivot[1]) * blend,
        pivot[2] + (capped_pivot[2] - pivot[2]) * blend,
    ];
    let eye = [
        eye[0] + (capped_eye[0] - eye[0]) * blend,
        eye[1] + (capped_eye[1] - eye[1]) * blend,
        eye[2] + (capped_eye[2] - eye[2]) * blend,
    ];
    (look, eye)
}

#[cfg(feature = "stage_b")]
fn clip_profile(
    geom: &WorldGeom,
    state: &GeomState,
    covered: bool,
    frame: &ChaseFrame<'_>,
    look: [f32; 3],
    shaped: [f32; 3],
    params: &CameraInteriorParams,
) -> [f32; 3] {
    if covered {
        tunnel_eye(geom, state, frame.feet, look, shaped, params)
    } else {
        open_eye(geom, state, look, shaped, frame.hole_at, params)
    }
}

#[cfg(feature = "stage_b")]
fn tunnel_eye(
    geom: &WorldGeom,
    state: &GeomState,
    feet: [f32; 3],
    pivot: [f32; 3],
    desired: [f32; 3],
    params: &CameraInteriorParams,
) -> [f32; 3] {
    let mut eye = desired;
    if let Some(floor) = camera_floor(geom, state, eye[0], eye[2], feet[1] + 0.25, params.floor_reach_m) {
        eye[1] = det_max(eye[1], floor + params.floor_clearance_m);
    }
    swept_clip(geom, state, pivot, eye, params.interior_sphere_m, params)
}

/// Move `eye` along the boom until `push_disc` reports no contact.
///
/// A sample that sits on the radius compares equal and is clear, but placing that length again
/// can land one ulp inside the sphere. Outward (toward `desired`) is tried first, then back
/// toward `look`.
#[cfg(feature = "stage_b")]
fn separate_sphere(
    geom: &WorldGeom,
    state: &GeomState,
    look: [f32; 3],
    desired: [f32; 3],
    eye: [f32; 3],
    params: &CameraInteriorParams,
) -> [f32; 3] {
    let radius = params.interior_sphere_m;
    let blocked = |p: [f32; 3]| sphere_blocked(geom, state, p, look[1], radius, params);
    if !blocked(eye) {
        return eye;
    }
    let outward = sub(desired, look);
    let span = len3(outward);
    let here = len3(sub(eye, look));
    if span > 1.0e-4 {
        for i in 1..=16 {
            let len = det_min(here + 0.01 * i as f32, span);
            let sample = add(look, scale(outward, len / span));
            if !blocked(sample) {
                return sample;
            }
        }
    }
    if here > 1.0e-4 {
        let back = sub(eye, look);
        for i in 1..=16 {
            let len = det_max(here - 0.01 * i as f32, 0.0);
            let sample = add(look, scale(back, len / here));
            if !blocked(sample) {
                return sample;
            }
        }
    }
    eye
}

/// Sphere sweep: march, `segment` against walls and roofs, then a fixed bisection while `push_disc` still moves.
#[cfg(feature = "stage_b")]
fn swept_clip(
    geom: &WorldGeom,
    state: &GeomState,
    pivot: [f32; 3],
    eye: [f32; 3],
    radius: f32,
    params: &CameraInteriorParams,
) -> [f32; 3] {
    if !in_camera_space(geom, state, pivot, params) {
        return eye;
    }
    let boom = sub(eye, pivot);
    let len = len3(boom);
    if len < 1.0e-3 || !len.is_finite() {
        return eye;
    }
    let radius = det_max(radius, 0.0);
    let margin = params.cam_margin_m + radius;
    let mut keep = len;
    let steps = step_count(len, params.space_step_m);
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let sample = add(pivot, scale(boom, t));
        if !sample_clear(geom, state, sample, pivot[1], params) {
            let traveled = (i - 1) as f32 / steps as f32 * len;
            keep = det_max(traveled - margin, 0.0);
            break;
        }
    }
    if let Some(hit) = geom.segment(state, pivot, eye, FaceMask::NO_FLOORS) {
        let along = det_max(hit.t * len - margin, 0.0);
        keep = det_min(keep, along);
    }
    if !sphere_blocked(
        geom,
        state,
        add(pivot, scale(boom, keep / len)),
        pivot[1],
        radius,
        params,
    ) {
        return add(pivot, scale(boom, keep / len));
    }
    let mut lo = 0.0_f32;
    let mut hi = keep;
    for _ in 0..8 {
        let mid = (lo + hi) * 0.5;
        let sample = add(pivot, scale(boom, mid / len));
        if sphere_blocked(geom, state, sample, pivot[1], radius, params) {
            hi = mid;
        } else {
            lo = mid;
        }
    }
    add(pivot, scale(boom, lo / len))
}

#[cfg(feature = "stage_b")]
fn sample_clear(
    geom: &WorldGeom,
    state: &GeomState,
    sample: [f32; 3],
    ref_y: f32,
    params: &CameraInteriorParams,
) -> bool {
    in_camera_space(geom, state, sample, params) && !lid_blocks(geom, state, sample, ref_y, params)
}

#[cfg(feature = "stage_b")]
fn lid_blocks(
    geom: &WorldGeom,
    state: &GeomState,
    sample: [f32; 3],
    ref_y: f32,
    params: &CameraInteriorParams,
) -> bool {
    let support = geom.support(state, sample[0], sample[2], ref_y);
    match support.ceiling {
        Some(lid) => sample[1] >= lid.y - params.lid_margin_m,
        None => false,
    }
}

#[cfg(feature = "stage_b")]
fn sphere_blocked(
    geom: &WorldGeom,
    state: &GeomState,
    sample: [f32; 3],
    ref_y: f32,
    radius: f32,
    params: &CameraInteriorParams,
) -> bool {
    if !sample_clear(geom, state, sample, ref_y, params) {
        return true;
    }
    if radius <= 0.0 {
        return false;
    }
    let band = Band {
        lo: sample[1] - radius,
        hi: sample[1] + radius,
    };
    geom.push_disc(state, sample[0], sample[2], radius, band).contacts > 0
}

/// Length closest to `to_len` whose samples from `from_len` stay clear.
///
/// Distances lie on the ray `origin -> clipped` and may extend past the clipped point: the
/// previous boom is that longer distance. Clamping it onto the clipped segment would read the
/// far eye as the near clip and walk the sphere through the wall. The step is at most half the
/// sphere radius, so a zero-thickness wall between samples still meets the sphere.
#[cfg(feature = "stage_b")]
fn last_clear_length(
    geom: &WorldGeom,
    state: &GeomState,
    origin: [f32; 3],
    clipped: [f32; 3],
    from_len: f32,
    to_len: f32,
    params: &CameraInteriorParams,
) -> f32 {
    let boom = sub(clipped, origin);
    let full = len3(boom);
    let delta = to_len - from_len;
    let radius = params.interior_sphere_m;
    if full < 1.0e-3 || !full.is_finite() || delta.abs() < 1.0e-4 {
        return to_len;
    }
    let at = |len: f32| add(origin, scale(boom, det_max(len, 0.0) / full));
    let clear = |len: f32| !sphere_blocked(geom, state, at(len), origin[1], radius, params);
    if !clear(from_len) {
        return from_len;
    }
    let step = det_max(radius, 0.05) * 0.5;
    let steps = step_count(delta.abs(), step);
    let mut last = from_len;
    for i in 1..=steps {
        let len = from_len + delta * (i as f32 / steps as f32);
        if !clear(len) {
            return last;
        }
        last = len;
    }
    last
}

#[cfg(feature = "stage_b")]
fn open_eye(
    geom: &WorldGeom,
    state: &GeomState,
    pivot: [f32; 3],
    desired: [f32; 3],
    hole_at: &dyn Fn(f32, f32) -> bool,
    params: &CameraInteriorParams,
) -> [f32; 3] {
    let boom = sub(desired, pivot);
    let len = len3(boom);
    if len < 1.0e-3 || !len.is_finite() {
        return desired;
    }
    let steps = step_count(len, params.march_step_m);
    let mut safe = 1.0_f32;
    for i in 1..=steps {
        let t = i as f32 / steps as f32;
        let p = add(pivot, scale(boom, t));
        if solid_ground(geom, state, hole_at, p[0], p[2], p[1])
            .is_some_and(|g| p[1] < g + params.cam_margin_m)
        {
            safe = (i - 1) as f32 / steps as f32;
            break;
        }
    }
    let pulled = add(pivot, scale(boom, safe));
    if safe * len >= params.cramped_boom_m {
        return pulled;
    }
    match lift_over_rim(geom, state, pivot, desired, hole_at, params) {
        Some(lifted) if len3(sub(lifted, pivot)) > len3(sub(pulled, pivot)) => lifted,
        _ => pulled,
    }
}

#[cfg(feature = "stage_b")]
fn lift_over_rim(
    geom: &WorldGeom,
    state: &GeomState,
    pivot: [f32; 3],
    desired: [f32; 3],
    hole_at: &dyn Fn(f32, f32) -> bool,
    params: &CameraInteriorParams,
) -> Option<[f32; 3]> {
    let ceiling = pivot[1] + params.rise_cap_m;
    let mut eye = desired;
    for _ in 0..24 {
        let Some(ground) = solid_ground(geom, state, hole_at, eye[0], eye[2], eye[1]) else {
            break;
        };
        if eye[1] >= ground + params.rim_clearance_m || eye[1] >= ceiling {
            break;
        }
        let shorter = add(pivot, scale(sub(eye, pivot), 0.92));
        eye = if len3(sub(shorter, pivot)) > 3.0 {
            add(shorter, [0.0, 0.6, 0.0])
        } else {
            add(eye, [0.0, 0.8, 0.0])
        };
    }
    let ground = solid_ground(geom, state, hole_at, eye[0], eye[2], eye[1])?;
    eye[1] = det_max(eye[1], det_min(ground + params.rim_clearance_m, ceiling));
    eye[1] = det_min(eye[1], ceiling);
    (eye[1] >= ground + params.cam_margin_m).then_some(eye)
}

#[cfg(feature = "stage_b")]
fn solid_ground(
    geom: &WorldGeom,
    state: &GeomState,
    hole_at: &dyn Fn(f32, f32) -> bool,
    x: f32,
    z: f32,
    ref_y: f32,
) -> Option<f32> {
    if hole_at(x, z) {
        return None;
    }
    Some(lattice_y(geom, state, x, z, ref_y))
}

#[cfg(feature = "stage_b")]
fn lattice_y(geom: &WorldGeom, state: &GeomState, x: f32, z: f32, ref_y: f32) -> f32 {
    let mut buf = SpanBuf::default();
    geom.floors_at(state, x, z, &mut buf);
    for span in buf.spans() {
        if span.floor.kind == SurfaceKind::Ground {
            return span.floor.y;
        }
    }
    geom.support(state, x, z, ref_y).floor.y
}

#[cfg(feature = "stage_b")]
fn lid_over(geom: &WorldGeom, state: &GeomState, p: [f32; 3]) -> bool {
    let mut buf = SpanBuf::default();
    geom.floors_at(state, p[0], p[2], &mut buf);
    buf.spans().iter().any(|span| {
        span.floor.kind == SurfaceKind::Floor
            && span.floor.y <= p[1] + 0.5
            && span.ceiling.is_some_and(|lid| lid.y > p[1])
    })
}

fn xz_seg_dist(p: [f32; 3], a: [f32; 3], b: [f32; 3]) -> (f32, f32) {
    let abx = b[0] - a[0];
    let abz = b[2] - a[2];
    let len2 = abx * abx + abz * abz;
    let t = if len2 < 1.0e-8 {
        0.0
    } else {
        det_clamp(((p[0] - a[0]) * abx + (p[2] - a[2]) * abz) / len2, 0.0, 1.0)
    };
    let dx = p[0] - (a[0] + abx * t);
    let dz = p[2] - (a[2] + abz * t);
    (len_xz(dx, dz), t)
}

#[cfg(feature = "stage_b")]
fn add(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

#[cfg(feature = "stage_b")]
fn sub(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

#[cfg(feature = "stage_b")]
fn scale(v: [f32; 3], s: f32) -> [f32; 3] {
    [v[0] * s, v[1] * s, v[2] * s]
}

#[cfg(feature = "stage_b")]
fn len3(v: [f32; 3]) -> f32 {
    math::sqrt(v[0] * v[0] + v[1] * v[1] + v[2] * v[2])
}

fn len_xz(x: f32, z: f32) -> f32 {
    math::sqrt(x * x + z * z)
}

fn finite3(p: [f32; 3]) -> bool {
    p[0].is_finite() && p[1].is_finite() && p[2].is_finite()
}

#[cfg(feature = "stage_b")]
fn step_count(len: f32, step: f32) -> usize {
    let step = det_max(step, 1.0e-3);
    let n = (len / step).ceil();
    if !n.is_finite() {
        return 1;
    }
    det_clamp(n, 1.0, 512.0) as usize
}
