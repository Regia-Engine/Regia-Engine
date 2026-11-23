// SPDX-License-Identifier: MPL-2.0
//! Heel and toe foot shape for the foot planter: stairs, curbs, ramps and toe bend
//! (`docs/specs/17-animation.md` § 6).
//!
//! [`FootPlanter::plan`](super::FootPlanter::plan) sees a foot as one ankle point plus a tilt
//! toward one ground normal, probed under the ankle only. A foot half over a stair nosing then
//! floats (the ankle probe hits the lower tread and the toe sinks into the riser) or hangs. With
//! a [`FootSole`] per leg, passed to
//! [`FootPlanter::plan_with_soles`](super::FootPlanter::plan_with_soles), the planter also knows
//! the ground under the heel and the toe:
//!
//! * **Pitch** comes from the heel and toe ground heights (clamped by `max_tilt`); roll still
//!   comes from the ground normal.
//! * **Stair rule.** When the heel and toe heights differ by more than `step_edge` and the ground
//!   between them is not a ramp, the foot rests flat on the higher tread when at least
//!   `min_support` of its length is over it. Otherwise it moves along its heel-to-toe axis onto
//!   the tread it mostly covers, never more than `max_edge_shift`. When that is too far, or would
//!   take the foot out of the leg's reach, it rests on the higher tread, so a sole never sinks into
//!   a riser.
//! * **Toe bend.** With a toe joint, a heel the clip lifts during stance exit turns the foot
//!   about the ball and the toe joint counter-rotates, so the toe stays flat on the ground until
//!   the ball lifts.
//!
//! Every point the planter places is checked against the ground profile it was given: a planted
//! heel, ball or toe is never left under it.
//!
//! [`probe_foot_sole`] builds a [`FootSole`] from any ground query (a physics raycast, an
//! analytic surface in tests), including a short bisection that finds the step edge between heel
//! and toe. [`foot_shape_from_bind`] derives the heel, ball and toe points from a skeleton.
//!
//! Provenance: heel and toe ground contacts and toe roll follow the procedural foot placement
//! approach in the public procedural-animation talks already cited for foot IK
//! (`docs/provenance.md`). The support profile, the edge bisection and the stair rule are
//! original Regia code.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;

use super::plant::{FootPlantSettings, align_to_normal};
use crate::num::{det_atan2, det_sin_cos_f64, finite_or};
use crate::skeleton::Skeleton;

/// One probed sole point of a foot.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SolePoint {
    /// The sole point minus the ankle, world space, in the clip pose (before any IK).
    pub offset: Vec3,
    /// The ground hit under the sole point, world space. The host probes at the ankle it probed
    /// for [`FootPlantLeg::ground`](super::FootPlantLeg::ground) (the pin while one holds the
    /// foot) plus `offset`.
    pub ground: Vec3,
}

/// Heel and toe ground samples for one leg. [`FootSole::default`] (no samples) plans the leg
/// exactly as [`FootPlanter::plan`](super::FootPlanter::plan) does.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct FootSole {
    /// The heel point and the ground under it. `None` (a probe that missed) ignores the shape.
    pub heel: Option<SolePoint>,
    /// The toe tip point and the ground under it. `None` ignores the shape.
    pub toe: Option<SolePoint>,
    /// The toe joint (ball) minus the ankle, world space, clip pose. `Some` turns on the toe
    /// bend; `None` treats the foot as rigid.
    pub ball: Option<Vec3>,
    /// Where a step edge crosses the heel-to-toe line, 0 at the heel probe, 1 at the toe probe.
    /// `Some` marks the ground between them as a step. `None` lets the planter decide from the
    /// heights (and the ankle sample), estimating the edge when it finds a step.
    pub edge: Option<f32>,
}

/// Heel, ball and toe points in the foot (ankle) joint's bind space, skeleton units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootShape {
    /// The back of the sole.
    pub heel: Vec3,
    /// The front of the sole (the toe tip).
    pub toe: Vec3,
    /// The toe joint, when the leg names one: the toe bends about it.
    pub ball: Option<Vec3>,
}

/// How [`FootPlacement`](super::FootPlacement) builds a [`FootShape`] per leg (lengths in
/// skeleton units, which are meters for a rig at scale 1).
#[derive(Clone, Copy, Debug, PartialEq, serde::Serialize, serde::Deserialize, Reflect)]
#[serde(default)]
#[reflect(Default)]
pub struct FootShapeSettings {
    /// Heel distance behind the ankle, along the foot. Default 0.06.
    pub heel_length: f32,
    /// Toe tip distance in front of the toe joint. Default 0.05.
    pub toe_length: f32,
    /// Toe tip distance in front of the ankle, for a leg without a toe joint. Default 0.17.
    pub front_length: f32,
    /// Bisection probes that locate a step edge between heel and toe (0 to 8). Default 5.
    pub edge_steps: u32,
}

impl Default for FootShapeSettings {
    fn default() -> Self {
        Self {
            heel_length: 0.06,
            toe_length: 0.05,
            front_length: 0.17,
            edge_steps: 5,
        }
    }
}

/// The most bisection probes [`probe_foot_sole`] issues for one edge.
pub const MAX_EDGE_STEPS: u32 = 8;

/// The largest toe bend, radians (60 degrees).
const MAX_TOE_BEND: f32 = std::f32::consts::FRAC_PI_3;

/// How far past a step edge a shifted foot is placed, meters.
const EDGE_MARGIN: f32 = 0.005;

/// The heel, ball and toe of `ankle`'s foot from the bind pose of `skeleton`.
///
/// The sole height is the toe joint's bind height (the rig origin, 0, without one). The foot
/// points along the bind ankle-to-toe direction (object −Z without a toe joint). The heel sits
/// `heel_length` behind the ankle, the toe tip `toe_length` in front of the toe joint, or
/// `front_length` in front of the ankle without one. `None` for an ankle that is not a joint.
pub fn foot_shape_from_bind(
    skeleton: &Skeleton,
    ankle: u32,
    toe: Option<u32>,
    settings: &FootShapeSettings,
) -> Option<FootShape> {
    let bind = skeleton.bind_globals();
    let foot = *bind.get(ankle as usize)?;
    let ankle_p = foot.w_axis.truncate();
    let ball_p = toe
        .and_then(|toe| bind.get(toe as usize))
        .map(|m| m.w_axis.truncate())
        .filter(|ball| ball.is_finite());
    let len = |v: f32, fallback: f32| finite_or(v, fallback).abs();
    let forward = ball_p
        .map(|ball| Vec3::new(ball.x - ankle_p.x, 0.0, ball.z - ankle_p.z))
        .filter(|d| d.length_squared() > 1.0e-6)
        .map_or(Vec3::NEG_Z, Vec3::normalize);
    let sole_y = ball_p.map_or(0.0, |ball| ball.y);
    let heel = Vec3::new(ankle_p.x, sole_y, ankle_p.z) - forward * len(settings.heel_length, 0.06);
    let tip = match ball_p {
        Some(ball) => ball + forward * len(settings.toe_length, 0.05),
        None => Vec3::new(ankle_p.x, sole_y, ankle_p.z) + forward * len(settings.front_length, 0.17),
    };
    let inverse = foot.inverse();
    if !inverse.is_finite() {
        return None;
    }
    Some(FootShape {
        heel: inverse.transform_point3(heel),
        toe: inverse.transform_point3(tip),
        ball: ball_p.map(|ball| inverse.transform_point3(ball)),
    })
}

/// Finds where the ground steps between `heel` and `toe` (world points whose ground heights are
/// `heel_y` and `toe_y`) by bisection: `steps` probes, each halving the bracket. `probe` returns
/// the ground height under a world point. The result is the edge fraction, 0 at `heel`, 1 at
/// `toe`. A probe that misses stops the search at the current bracket.
pub fn find_step_edge(
    heel: Vec3,
    toe: Vec3,
    heel_y: f32,
    toe_y: f32,
    steps: u32,
    mut probe: impl FnMut(Vec3) -> Option<f32>,
) -> f32 {
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..steps.min(MAX_EDGE_STEPS) {
        let mid = 0.5 * (lo + hi);
        let Some(y) = probe(heel.lerp(toe, mid)).filter(|y| y.is_finite()) else {
            break;
        };
        if (y - heel_y).abs() <= (y - toe_y).abs() {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    0.5 * (lo + hi)
}

/// Probes the heel and toe of a foot for
/// [`FootPlanter::plan_with_soles`](super::FootPlanter::plan_with_soles).
///
/// `foot` is the clip foot joint's world matrix; `anchor` is the world ankle point the host
/// probes under (the planter's pin while one holds the foot, else the clip ankle). `probe`
/// returns the ground hit (world point) under a world sole point. When the heel and toe heights
/// differ by more than `step_edge`, one probe halfway between them tells a ramp from a step; a
/// step is then located with `edge_steps` bisection probes ([`find_step_edge`]). At most
/// `3 + edge_steps` probes per foot, and two on level ground.
pub fn probe_foot_sole(
    foot: Mat4,
    anchor: Vec3,
    shape: &FootShape,
    step_edge: f32,
    edge_steps: u32,
    mut probe: impl FnMut(Vec3) -> Option<Vec3>,
) -> FootSole {
    let ankle = foot.w_axis.truncate();
    let offset = |p: Vec3| foot.transform_point3(p) - ankle;
    let heel_off = offset(shape.heel);
    let toe_off = offset(shape.toe);
    let ball = shape.ball.map(offset).filter(|b| b.is_finite());
    if !(anchor.is_finite() && heel_off.is_finite() && toe_off.is_finite()) {
        return FootSole::default();
    }
    let mut hit = |p: Vec3| probe(p).filter(|g| g.is_finite());
    let heel_at = anchor + heel_off;
    let toe_at = anchor + toe_off;
    let heel = hit(heel_at).map(|ground| SolePoint {
        offset: heel_off,
        ground,
    });
    let toe = hit(toe_at).map(|ground| SolePoint {
        offset: toe_off,
        ground,
    });
    let mut edge = None;
    if let (Some(h), Some(t)) = (heel, toe) {
        let (hy, ty) = (h.ground.y, t.ground.y);
        let step_edge = finite_or(step_edge, 0.04).abs();
        if (ty - hy).abs() > step_edge {
            let middle = hit(heel_at.lerp(toe_at, 0.5)).map(|g| g.y);
            let on_line = middle.is_some_and(|m| (m - 0.5 * (hy + ty)).abs() <= 0.5 * step_edge);
            if !on_line {
                let found = find_step_edge(heel_at, toe_at, hy, ty, edge_steps, |p| hit(p).map(|g| g.y));
                edge = Some(found);
            }
        }
    }
    FootSole {
        heel,
        toe,
        ball,
        edge,
    }
}

/// Rotation by `angle` radians about the unit `axis`, platform-independent.
pub(super) fn axis_angle(axis: Vec3, angle: f32) -> Quat {
    if !axis.is_finite() || !angle.is_finite() || angle == 0.0 || axis.length_squared() < 1.0e-12 {
        return Quat::IDENTITY;
    }
    let axis = axis.normalize();
    let (sin, cos) = det_sin_cos_f64(f64::from(angle) * 0.5);
    let sin = sin as f32;
    Quat::from_xyzw(axis.x * sin, axis.y * sin, axis.z * sin, cos as f32).normalize()
}

/// [`FootPlantSettings`] stair values made safe.
#[derive(Clone, Copy, Debug)]
pub(super) struct SoleTuning {
    step_edge: f32,
    min_support: f32,
    max_shift: f32,
    max_tilt: f32,
}

impl SoleTuning {
    pub(super) fn from(s: &FootPlantSettings, max_tilt: f32) -> Self {
        Self {
            step_edge: finite_or(s.step_edge, 0.04).abs(),
            min_support: finite_or(s.min_support, 0.5).clamp(0.0, 1.0),
            max_shift: finite_or(s.max_edge_shift, 0.12).abs(),
            max_tilt,
        }
    }
}

/// The ground under one foot along its heel-to-toe axis, object space.
#[derive(Clone, Copy, Debug)]
enum Profile {
    /// A plane: height `heel` at the heel probe, `toe` at the toe probe, linear between and past.
    Ramp { heel: f32, toe: f32 },
    /// Height `heel` before `at` (distance from the heel probe along the axis), `toe` from it.
    Step { heel: f32, toe: f32, at: f32 },
}

/// One leg's heel and toe data in object space.
#[derive(Clone, Copy, Debug)]
pub(super) struct SoleFrame {
    /// Horizontal unit heel-to-toe axis.
    forward: Vec3,
    /// Unit lateral axis (`forward × Y`): a positive turn about it raises the toe.
    lateral: Vec3,
    /// Heel, toe and ball minus the ankle, clip pose.
    heel: Vec3,
    toe: Vec3,
    ball: Option<Vec3>,
    /// The heel probe point; distances along the axis are measured from it.
    origin: Vec3,
    /// Heel to toe probe, along the axis.
    length: f32,
    profile: Profile,
}

impl SoleFrame {
    /// `None` unless both probes hit and the foot is at least 2 cm long.
    pub(super) fn build(
        sole: &FootSole,
        to_object: impl Fn(Vec3) -> Vec3,
        to_object_dir: impl Fn(Vec3) -> Vec3,
        ankle_ground: Option<Vec3>,
        t: &SoleTuning,
    ) -> Option<Self> {
        let (heel, toe) = (sole.heel?, sole.toe?);
        let heel_off = to_object_dir(heel.offset);
        let toe_off = to_object_dir(toe.offset);
        let heel_ground = to_object(heel.ground);
        let toe_ground = to_object(toe.ground);
        if !(heel_off.is_finite() && toe_off.is_finite() && heel_ground.is_finite() && toe_ground.is_finite())
        {
            return None;
        }
        let axis = Vec3::new(toe_off.x - heel_off.x, 0.0, toe_off.z - heel_off.z);
        let length = axis.length();
        if length.is_nan() || length < 0.02 {
            return None;
        }
        let forward = axis / length;
        let lateral = forward.cross(Vec3::Y).normalize_or_zero();
        let ball = sole.ball.map(&to_object_dir).filter(|b| b.is_finite());
        let (hg, tg) = (heel_ground.y, toe_ground.y);
        let origin = Vec3::new(heel_ground.x, 0.0, heel_ground.z);
        let step = match sole.edge.filter(|e| e.is_finite()) {
            Some(edge) => Some(edge.clamp(0.0, 1.0)),
            None if (tg - hg).abs() > t.step_edge => {
                // No edge from the host: a ramp when the ankle sample lies on the heel-toe line,
                // else a step whose edge is halfway into the bracket the ankle sample leaves.
                let ankle = ankle_ground
                    .map(&to_object)
                    .filter(|a| a.is_finite())
                    .map(|a| ((a - origin).dot(forward), a.y));
                match ankle {
                    Some((u, y)) => {
                        let line = hg + (tg - hg) * (u / length);
                        if (y - line).abs() <= 0.5 * t.step_edge {
                            None
                        } else {
                            let u = u.clamp(0.0, length);
                            let at = if (y - hg).abs() <= (y - tg).abs() {
                                0.5 * (u + length)
                            } else {
                                0.5 * u
                            };
                            Some(at / length)
                        }
                    }
                    None => Some(0.5),
                }
            }
            None => None,
        };
        let profile = match step {
            Some(edge) => Profile::Step {
                heel: hg,
                toe: tg,
                at: edge * length,
            },
            None => Profile::Ramp { heel: hg, toe: tg },
        };
        Some(Self {
            forward,
            lateral,
            heel: heel_off,
            toe: toe_off,
            ball,
            origin,
            length,
            profile,
        })
    }

    /// The horizontal heel-to-toe axis, object space.
    pub(super) fn forward(&self) -> Vec3 {
        self.forward
    }

    /// The ground height under object point `p`.
    fn ground(&self, p: Vec3) -> f32 {
        let u = (p - self.origin).dot(self.forward);
        match self.profile {
            Profile::Ramp { heel, toe } => heel + (toe - heel) * (u / self.length),
            Profile::Step { heel, toe, at } => {
                if u < at {
                    heel
                } else {
                    toe
                }
            }
        }
    }

    /// The ankle the host probed under: the heel probe minus the heel offset, horizontally.
    pub(super) fn anchor(&self) -> Vec3 {
        Vec3::new(self.origin.x - self.heel.x, 0.0, self.origin.z - self.heel.z)
    }

    /// The pitch the heel and toe heights ask for (0 on a step), clamped by `max_tilt`.
    pub(super) fn pitch(&self, t: &SoleTuning) -> f32 {
        match self.profile {
            Profile::Ramp { heel, toe } => det_atan2(toe - heel, self.length).clamp(-t.max_tilt, t.max_tilt),
            Profile::Step { .. } => 0.0,
        }
    }

    /// The stair rule's move along the foot axis (0 off a step, or when it would exceed
    /// `max_edge_shift`).
    pub(super) fn shift(&self, t: &SoleTuning) -> f32 {
        let Profile::Step { heel, toe, at } = self.profile else {
            return 0.0;
        };
        let edge = at / self.length;
        if toe > heel {
            // The toe end is on the higher tread.
            if 1.0 - edge >= t.min_support {
                return 0.0;
            }
            let back = (self.length - at) + EDGE_MARGIN;
            if back <= t.max_shift { -back } else { 0.0 }
        } else {
            if edge >= t.min_support {
                return 0.0;
            }
            let ahead = at + EDGE_MARGIN;
            if ahead <= t.max_shift { ahead } else { 0.0 }
        }
    }

    /// The ground normal the foot turns toward: `pitch` along the foot axis, the roll of the
    /// probed `normal` across it.
    pub(super) fn normal(&self, normal: Vec3, pitch: f32) -> Vec3 {
        let n = normal.normalize_or_zero();
        let across = if n.is_finite() && n.y > 1.0e-4 {
            n.dot(self.lateral) / n.y
        } else {
            0.0
        };
        let (sin, cos) = det_sin_cos_f64(f64::from(pitch));
        let slope = (sin / cos.max(1.0e-6)) as f32;
        (Vec3::Y - self.forward * slope + self.lateral * across).normalize_or_zero()
    }

    /// The tilt for `pitch` and the probed `normal`, at most `max_tilt`.
    pub(super) fn tilt(&self, normal: Vec3, pitch: f32, t: &SoleTuning) -> Quat {
        align_to_normal(self.normal(normal, pitch), t.max_tilt)
    }

    /// How far the clip lifts the heel above the ball, radians (0 for a rigid foot).
    pub(super) fn heel_lift(&self) -> f32 {
        let Some(ball) = self.ball else {
            return 0.0;
        };
        let d = ball - self.heel;
        let along = d.dot(self.forward);
        if along <= 1.0e-4 {
            return 0.0;
        }
        det_atan2(-d.y, along).clamp(0.0, MAX_TOE_BEND)
    }

    /// The toe tip minus the ankle with the toe bent up by `bend` radians about the ball.
    pub(super) fn tip(&self, bend: f32) -> Vec3 {
        match self.ball {
            Some(ball) if bend > 0.0 => ball + axis_angle(self.lateral, bend) * (self.toe - ball),
            _ => self.toe,
        }
    }

    /// The toe joint's bend rotation in object space after the foot `tilt`.
    pub(super) fn toe_rotation(&self, tilt: Quat, bend: f32) -> Quat {
        if self.ball.is_none() || bend.is_nan() || bend <= 0.0 {
            return Quat::IDENTITY;
        }
        axis_angle(tilt * self.lateral, bend)
    }

    /// The lowest clip sole point over the clip's floor (object height 0), from the clip ankle
    /// height `tip_y`.
    pub(super) fn clearance(&self, tip_y: f32) -> f32 {
        let mut low = (tip_y + self.heel.y).min(tip_y + self.toe.y);
        if let Some(ball) = self.ball {
            low = low.min(tip_y + ball.y);
        }
        low
    }

    /// The lowest ankle height at horizontal position `ankle` that keeps the heel, ball and the
    /// (bent) toe tip, turned by `tilt`, on or above the ground.
    pub(super) fn support(&self, tilt: Quat, ankle: Vec3, tip: Vec3) -> f32 {
        let lift = |offset: Vec3| {
            let o = tilt * offset;
            self.ground(Vec3::new(ankle.x + o.x, 0.0, ankle.z + o.z)) - o.y
        };
        let mut need = lift(self.heel).max(lift(tip));
        if let Some(ball) = self.ball {
            need = need.max(lift(ball));
        }
        need
    }

    /// Heel and toe tip heights over the surface under them, for an ankle placed at `target` and
    /// turned by `tilt`. On a step, a point at or above the higher tread's height is measured
    /// to that tread: a heel hanging past the nosing the sole rests on is resting, not floating.
    pub(super) fn gaps(&self, target: Vec3, tilt: Quat, tip: Vec3) -> (f32, f32) {
        let gap = |offset: Vec3| {
            let p = target + tilt * offset;
            let ground = match self.profile {
                Profile::Step { heel, toe, .. } if p.y >= heel.max(toe) - 1.0e-3 => heel.max(toe),
                _ => self.ground(p),
            };
            p.y - ground
        };
        (gap(self.heel), gap(tip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn flat_sole(heel_y: f32, toe_y: f32, edge: Option<f32>) -> FootSole {
        FootSole {
            heel: Some(SolePoint {
                offset: Vec3::new(0.0, -0.08, 0.06),
                ground: Vec3::new(0.0, heel_y, 0.06),
            }),
            toe: Some(SolePoint {
                offset: Vec3::new(0.0, -0.08, -0.17),
                ground: Vec3::new(0.0, toe_y, -0.17),
            }),
            ball: None,
            edge,
        }
    }

    fn tuning() -> SoleTuning {
        SoleTuning::from(&FootPlantSettings::default(), 0.436)
    }

    fn frame(sole: &FootSole, ankle: Option<Vec3>) -> SoleFrame {
        SoleFrame::build(sole, |p| p, |d| d, ankle, &tuning()).expect("frame")
    }

    #[test]
    fn a_missing_probe_or_a_tiny_foot_is_no_frame() {
        let mut sole = flat_sole(0.0, 0.0, None);
        sole.toe = None;
        assert!(SoleFrame::build(&sole, |p| p, |d| d, None, &tuning()).is_none());
        let mut tiny = flat_sole(0.0, 0.0, None);
        if let Some(toe) = &mut tiny.toe {
            toe.offset = Vec3::new(0.0, -0.08, 0.05);
        }
        assert!(SoleFrame::build(&tiny, |p| p, |d| d, None, &tuning()).is_none());
        assert!(SoleFrame::build(&FootSole::default(), |p| p, |d| d, None, &tuning()).is_none());
    }

    #[test]
    fn level_ground_needs_the_ankle_height_and_no_shift() {
        let f = frame(&flat_sole(0.25, 0.25, None), None);
        assert_eq!(f.pitch(&tuning()), 0.0);
        assert_eq!(f.shift(&tuning()), 0.0);
        let need = f.support(Quat::IDENTITY, f.anchor(), f.tip(0.0));
        assert!((need - 0.33).abs() < 1.0e-6, "{need}");
    }

    #[test]
    fn a_step_with_most_support_up_rests_on_the_higher_tread() {
        // Toe 0.18 higher; the edge at 30 % from the heel leaves 70 % of the foot up.
        let f = frame(&flat_sole(0.0, 0.18, Some(0.3)), None);
        assert_eq!(f.pitch(&tuning()), 0.0);
        assert_eq!(f.shift(&tuning()), 0.0);
        let need = f.support(Quat::IDENTITY, f.anchor(), f.tip(0.0));
        assert!((need - (0.18 + 0.08)).abs() < 1.0e-6);
    }

    #[test]
    fn a_step_with_little_support_up_moves_back_onto_the_lower_tread() {
        let t = tuning();
        let f = frame(&flat_sole(0.0, 0.18, Some(0.8)), None);
        let shift = f.shift(&t);
        // 20 % of 0.23 m is over the riser: move back past the edge.
        assert!((shift + (0.23 * 0.2 + EDGE_MARGIN)).abs() < 1.0e-5, "{shift}");
        let ankle = f.anchor() + f.forward() * shift;
        let need = f.support(Quat::IDENTITY, ankle, f.tip(0.0));
        assert!((need - 0.08).abs() < 1.0e-6, "on the lower tread: {need}");
        // Too far to move: it rests on the higher tread instead of sinking.
        let near = SoleTuning { max_shift: 0.01, ..t };
        assert_eq!(f.shift(&near), 0.0);
    }

    #[test]
    fn a_ramp_is_told_from_a_step_by_the_ankle_sample() {
        let t = tuning();
        let rise = 0.23 * 20f32.to_radians().tan();
        // The ankle probe at 6 cm from the heel lies on the heel-toe line: a ramp.
        let ankle = Vec3::new(0.0, rise * (0.06 / 0.23), 0.0);
        let ramp = frame(&flat_sole(0.0, rise, None), Some(ankle));
        assert!((ramp.pitch(&t).to_degrees() - 20.0).abs() < 0.01);
        assert_eq!(ramp.shift(&t), 0.0);
        // The ankle probe at the toe height: a step behind the ankle.
        let step = frame(&flat_sole(0.0, rise, None), Some(Vec3::new(0.0, rise, 0.0)));
        assert_eq!(step.pitch(&t), 0.0);
    }

    #[test]
    fn bisection_finds_a_step_edge() {
        let heel = Vec3::new(0.0, 0.0, 0.06);
        let toe = Vec3::new(0.0, 0.0, -0.17);
        // A riser at z = -0.1 (edge fraction 0.16 / 0.23).
        let probe = |p: Vec3| Some(if p.z < -0.1 { 0.18 } else { 0.0 });
        let edge = find_step_edge(heel, toe, 0.0, 0.18, 6, probe);
        assert!((edge - 0.16 / 0.23).abs() < 1.0 / 64.0 + 1.0e-6, "{edge}");
    }

    #[test]
    fn the_toe_bends_about_the_ball() {
        let mut sole = flat_sole(0.0, 0.0, None);
        // The clip lifted the heel 20 degrees about the ball 12 cm ahead of the ankle.
        let lift = 20f32.to_radians();
        let ball = Vec3::new(0.0, -0.08, -0.12);
        let turn = Quat::from_rotation_x(-lift);
        let about = |p: Vec3| ball + turn * (p - ball);
        if let (Some(heel), Some(toe)) = (&mut sole.heel, &mut sole.toe) {
            heel.offset = about(heel.offset);
            toe.offset = about(toe.offset);
        }
        sole.ball = Some(ball);
        let f = frame(&sole, None);
        assert!(
            (f.heel_lift().to_degrees() - 20.0).abs() < 0.05,
            "{}",
            f.heel_lift()
        );
        let tip = f.tip(f.heel_lift());
        assert!(
            (tip.y - ball.y).abs() < 1.0e-4,
            "the bent toe is level with the ball: {tip}"
        );
    }

    #[test]
    fn foot_shape_from_the_demo_rig() {
        let skeleton = crate::demo_rig::demo_humanoid(&crate::demo_rig::DemoRig::default()).expect("rig");
        let shape =
            foot_shape_from_bind(&skeleton, 14, Some(15), &FootShapeSettings::default()).expect("shape");
        // Identity bind frames: foot space is object space minus the ankle (-0.1, 0.08, 0).
        assert!(
            (shape.heel - Vec3::new(0.0, -0.08, 0.06)).length() < 1.0e-5,
            "{}",
            shape.heel
        );
        assert!(
            (shape.toe - Vec3::new(0.0, -0.08, -0.17)).length() < 1.0e-5,
            "{}",
            shape.toe
        );
        assert!((shape.ball.expect("ball") - Vec3::new(0.0, -0.08, -0.12)).length() < 1.0e-5);
        let no_toe = foot_shape_from_bind(&skeleton, 14, None, &FootShapeSettings::default()).expect("shape");
        assert!(no_toe.ball.is_none());
        assert!((no_toe.toe - Vec3::new(0.0, -0.08, -0.17)).length() < 1.0e-5);
        assert!(foot_shape_from_bind(&skeleton, 99, None, &FootShapeSettings::default()).is_none());
    }
}
