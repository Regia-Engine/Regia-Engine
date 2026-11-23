// SPDX-License-Identifier: MPL-2.0
//! Foot IK for characters on slopes and stairs (`docs/specs/17-animation.md` § 6, M20).
//!
//! Two cooperating pieces:
//!
//! 1. [`plan_foot_ik`] — the *planner*: a physics-free function that turns ground-contact
//!    data (however the game obtained it — raycasts, the character controller's ground
//!    probe) into a pelvis adjustment plus one object-space IK target per leg. The caller
//!    feeds those targets to an anim graph's two-bone IK nodes through `IkTarget::Param`
//!    vec3 parameters, and offsets the rig's translation by the pelvis amount.
//! 2. [`solve_foot_ik`] — the *solver*: applies the plan directly to a [`Pose`] (root-joint
//!    translation plus [`solve_two_bone`] per leg). Games that post-process poses
//!    themselves use this; the sample drives the graph instead.
//!
//! The pelvis strategy is the classic lowest-ground plant: each grounded leg reports how far
//! the ground sits from its animated foot (positive = the foot would penetrate, the ground is
//! higher); the pelvis follows the **lowest** grounded foot (clamped), so no leg is ever asked
//! to extend beyond its animated length — legs on higher ground bend down to plant, and the
//! two-bone solver clamps anything still out of reach. Ungrounded legs (no ground hit) keep
//! their animated pose.
//!
//! For tilt, a world-space plant lock and frame-to-frame smoothing, keep a stateful
//! [`FootPlanter`] per character instead (module [`plant`]); it plans the same pelvis-and-targets
//! shape and [`FootPlant::ik_plan`] feeds the same graph nodes. [`FootPlacement`] (module
//! [`component`]) is that planter as a component: the engine probes, plans, applies and inserts
//! [`FootPlantReport`].
//!
//! Provenance: the pelvis-lowering foot-planting heuristic follows the procedural animation
//! approach described in David Rosen's GDC 2014 talk "Animation Bootcamp: An Indie Approach
//! to Procedural Animation" (public); the per-leg solve is the analytic two-bone IK already
//! in [`crate::ik`] (Aristidou & Lasenby 2011 survey, see `docs/provenance.md`).

use regia_core::math::{Quat, Vec3};

pub mod contact;
pub mod lock;
pub mod plant;
pub use lock::{PlantLock, PlantLockStep, PlantRelease};
pub use plant::{
    FootPlant, FootPlantInput, FootPlantLeg, FootPlantLegOut, FootPlantReport, FootPlantSettings,
    FootPlanter, apply_foot_plant,
};

pub mod component;
pub use component::{
    FootLegDesc, FootPlacement, FootPlacementOutput, FootPlacementRespawn, FootPlacementStatus, GroundHit,
    GroundProbe, GroundProbes,
};

pub mod shape;
pub use plant::apply_foot_plant_into;
pub use plant::{apply_foot_plant_with_toes, apply_toe_bend};
pub use shape::{
    FootShape, FootShapeSettings, FootSole, SolePoint, find_step_edge, foot_shape_from_bind, probe_foot_sole,
};

use crate::ik::{IkChain, solve_two_bone};
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// One leg of the foot-IK rig.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootIkLeg {
    /// The hip → knee → foot chain (joint ids).
    pub chain: IkChain,
    /// How far above its animated position a foot may be pulled (metres). Targets further
    /// than this are treated as unreachable and the leg keeps its animated pose.
    pub max_reach_offset: f32,
}

impl FootIkLeg {
    /// A leg with the given chain and the default `max_reach_offset` (0.35 m).
    pub fn new(chain: IkChain) -> Self {
        Self {
            chain,
            max_reach_offset: 0.35,
        }
    }
}

/// Tuning for [`plan_foot_ik`] / [`solve_foot_ik`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootIkSettings {
    /// How far the pelvis may sink below its animated height (metres, positive number).
    pub max_pelvis_drop: f32,
    /// How far the pelvis may rise above its animated height (metres, positive number).
    pub max_pelvis_rise: f32,
    /// Leg influence fades from 1 to 0 as the ground-to-foot distance grows from `0` to
    /// this (metres).
    pub fade_distance: f32,
}

impl Default for FootIkSettings {
    fn default() -> Self {
        Self {
            max_pelvis_drop: 0.25,
            max_pelvis_rise: 0.2,
            fade_distance: 0.5,
        }
    }
}

/// Ground-contact data for one leg, in **world** space.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FootGround {
    /// The ground point the foot should plant on.
    Contact(Vec3),
    /// No contact this frame (airborne, or the probe found nothing): the leg is not
    /// adjusted.
    None,
}

/// What [`plan_foot_ik`] decided for one leg.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FootIkLegPlan {
    /// Object-space (rig-local) IK target for the leg's tip joint.
    pub target: Vec3,
    /// Blend weight for the leg (`0` keeps the animated pose).
    pub weight: f32,
}

/// The plan: how the rig and its legs should be adjusted this frame.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FootIkPlan {
    /// World-space Y offset for the whole rig (negative = pelvis sinks). Apply it to the
    /// rig entity's translation — visual only, never to the character's capsule.
    pub pelvis_offset: f32,
    /// One entry per leg, in the order the legs were given.
    pub legs: Vec<FootIkLegPlan>,
}

/// The world-space pose the rig currently renders with, for planning.
#[derive(Clone, Copy, Debug)]
pub struct FootIkInput<'a> {
    /// The rig entity's world translation (its `Transform`).
    pub rig_translation: Vec3,
    /// The rig entity's world rotation.
    pub rig_rotation: Quat,
    /// Current animated foot-tip positions in **world** space, one per leg.
    pub feet_world: &'a [Vec3],
    /// Ground contact per leg (world space), one per leg.
    pub ground: &'a [FootGround],
}

/// Plans the foot adjustment for one frame (the module docs describe the strategy).
///
/// `legs` and `input.feet_world`/`input.ground` must have equal length (legs without probes
/// pass [`FootGround::None`]).
pub fn plan_foot_ik(input: &FootIkInput<'_>, legs: &[FootIkLeg], settings: &FootIkSettings) -> FootIkPlan {
    let count = legs.len().min(input.feet_world.len()).min(input.ground.len());
    let mut errors: Vec<f32> = Vec::with_capacity(count);
    for i in 0..count {
        // A probe or foot that is not a number plans no adjustment for the pelvis either, as for
        // its leg below: `f32::min` drops NaN, so an all-NaN list folded to +inf and lifted the
        // pelvis by the full `max_pelvis_rise` (audit-v035-numerics M7).
        if let FootGround::Contact(point) = input.ground[i] {
            let error = point.y - input.feet_world[i].y;
            if error.is_finite() {
                errors.push(error);
            }
        }
    }

    // Pelvis: follow the lowest grounded foot (the most negative error), clamped. The
    // lowest foot never needs to extend beyond its animated length; higher feet bend.
    let pelvis_offset = if errors.is_empty() {
        0.0
    } else {
        let lowest = errors.iter().copied().fold(f32::INFINITY, f32::min);
        // Ranges are magnitudes; a negative or NaN setting must not panic `clamp`.
        let max_drop = crate::num::finite_or(settings.max_pelvis_drop, 0.0).abs();
        let max_rise = crate::num::finite_or(settings.max_pelvis_rise, 0.0).abs();
        crate::num::clamp_between(lowest, -max_drop, max_rise)
    };

    let inverse_rotation = input.rig_rotation.inverse();
    let mut plans = Vec::with_capacity(count);
    for i in 0..count {
        let (target_world, error) = match input.ground[i] {
            FootGround::Contact(point) => (point, point.y - input.feet_world[i].y),
            FootGround::None => (input.feet_world[i], 0.0),
        };
        // The pelvis moves the whole rig; re-aim each leg at where the ground sits *after*
        // the adjustment so partial pelvis offsets do not over-bend the legs.
        let target_world = target_world - Vec3::Y * pelvis_offset;
        // A NaN reach reads as none; a probe or foot that is not a number plans no adjustment.
        let fade = crate::num::finite_or(settings.fade_distance, 0.0).abs();
        let reachable = crate::num::finite_or(legs.get(i).map_or(0.35, |leg| leg.max_reach_offset), 0.0);
        let weight = match input.ground[i] {
            FootGround::None => 0.0,
            FootGround::Contact(_) if !(error.is_finite() && target_world.is_finite()) => 0.0,
            FootGround::Contact(_) => {
                if error.abs() > reachable + fade {
                    0.0
                } else if error.abs() > reachable {
                    1.0 - (error.abs() - reachable) / fade.max(1.0e-6)
                } else {
                    1.0
                }
            }
        };
        let target_object = inverse_rotation * (target_world - input.rig_translation);
        plans.push(FootIkLegPlan {
            target: target_object,
            weight: weight.clamp(0.0, 1.0),
        });
    }

    FootIkPlan {
        pelvis_offset,
        legs: plans,
    }
}

/// Applies a plan directly to `pose`: the root joint translates by the plan's pelvis offset
/// (object space, +Y up) and each leg is solved towards its target with
/// [`solve_two_bone`] at the leg's weight. `pole` (object space) steers knees/ankles
/// (use the rig's forward); pass `None` to keep the current bend.
///
/// Use it *instead of* moving the rig entity by [`FootIkPlan::pelvis_offset`]: the plan's leg
/// targets assume the rig moved, so here they are raised by the offset again to land on the
/// ground while the rig stays put (for a rig that only yaws, which is what characters do).
///
/// This is the post-process form — games that drive an [`AnimGraph`](crate::graph::AnimGraph)
/// with `IkTarget::Param` nodes use [`plan_foot_ik`] alone.
pub fn solve_foot_ik(
    skeleton: &Skeleton,
    pose: &mut Pose,
    plan: &FootIkPlan,
    legs: &[FootIkLeg],
    pole: Option<Vec3>,
) {
    // Pelvis: offset the root joint (the only joint without a parent is the rig root).
    if plan.pelvis_offset != 0.0
        && let Some(root) = skeleton.parents.iter().position(|p| p.is_none())
        && root < pose.len()
    {
        pose.translations[root].y += plan.pelvis_offset;
    }
    for (leg, leg_plan) in legs.iter().zip(&plan.legs) {
        if leg_plan.weight <= 0.0 {
            continue;
        }
        // The plan aimed at the ground relative to a rig lowered by the pelvis offset; the rig
        // is not moved here (the root joint is), so aim at the ground itself.
        let target = leg_plan.target + Vec3::Y * plan.pelvis_offset;
        solve_two_bone(skeleton, pose, &leg.chain, target, pole, leg_plan.weight);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ik::IkChain;
    use crate::pose::translated;

    /// A simple biped: pelvis at (0, 1, 0), hips ±0.15 out, 0.45 m thighbones down, 0.45 m
    /// shins; feet end at y = 0.1.
    fn biped() -> Skeleton {
        let names: Vec<String> = ["Pelvis", "LHip", "LKnee", "LFoot", "RHip", "RKnee", "RFoot"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(0), Some(4), Some(5)];
        let bind = vec![
            translated(Vec3::new(0.0, 1.0, 0.0)),
            translated(Vec3::new(-0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.15, 0.0, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
            translated(Vec3::new(0.0, -0.45, 0.0)),
        ];
        Skeleton::from_local_bind(names, parents, bind).expect("valid biped")
    }

    fn legs() -> Vec<FootIkLeg> {
        vec![
            FootIkLeg::new(IkChain {
                root: 1,
                mid: 2,
                tip: 3,
            }),
            FootIkLeg::new(IkChain {
                root: 4,
                mid: 5,
                tip: 6,
            }),
        ]
    }

    fn foot_world(skeleton: &Skeleton, pose: &Pose, joint: u32) -> Vec3 {
        crate::ik::joint_position(skeleton, pose, joint)
    }

    #[test]
    fn flat_ground_is_a_no_op() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let feet = [foot_world(&sk, &pose, 3), foot_world(&sk, &pose, 6)];
        let ground = [FootGround::Contact(feet[0]), FootGround::Contact(feet[1])];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        assert!(
            plan.pelvis_offset.abs() < 1.0e-6,
            "flat ground: no pelvis change, got {}",
            plan.pelvis_offset
        );
        for (leg, foot) in plan.legs.iter().zip(feet) {
            assert!(leg.target.abs_diff_eq(foot, 1.0e-5), "target at the foot");
            assert!((leg.weight - 1.0).abs() < 1.0e-6, "full weight");
        }
    }

    #[test]
    fn a_step_up_plants_through_leg_bend_not_pelvis_rise() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        // A 0.1 m step under the LEFT foot only.
        let step = 0.1;
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y + step, left.z)),
            FootGround::Contact(right),
        ];
        let feet = [left, right];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        // The lowest grounded foot is flat: the pelvis stays, and the step leg bends.
        assert!(
            plan.pelvis_offset.abs() < 1.0e-6,
            "pelvis follows the lowest foot: {}",
            plan.pelvis_offset
        );
        // The left leg aims at the step top (object space = world here).
        let expected_left = Vec3::new(left.x, left.y + step, left.z);
        assert!(plan.legs[0].target.abs_diff_eq(expected_left, 1.0e-5));
        assert!((plan.legs[0].weight - 1.0).abs() < 1.0e-6);
        // The right leg aims at its own flat ground.
        assert!(plan.legs[1].target.abs_diff_eq(right, 1.0e-5));
    }

    #[test]
    fn a_downhill_slope_lowers_the_pelvis() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        // Both feet float 0.2 m above the ground (walking downhill, animation behind).
        let drop = 0.2;
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y - drop, left.z)),
            FootGround::Contact(Vec3::new(right.x, right.y - drop, right.z)),
        ];
        let feet = [left, right];
        let settings = FootIkSettings {
            max_pelvis_drop: 0.25,
            ..FootIkSettings::default()
        };
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &settings,
        );
        assert!(
            (plan.pelvis_offset + drop).abs() < 1.0e-6,
            "pelvis follows the slope down: {}",
            plan.pelvis_offset
        );
    }

    #[test]
    fn pelvis_drop_is_clamped() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let drop = 5.0; // a cliff
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y - drop, left.z)),
            FootGround::Contact(Vec3::new(right.x, right.y - drop, right.z)),
        ];
        let feet = [left, right];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        assert!((plan.pelvis_offset + 0.25).abs() < 1.0e-6, "clamped at max drop");
    }

    #[test]
    fn airborne_legs_are_untouched() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let feet = [left, right];
        let ground = [FootGround::None, FootGround::None];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        assert!(plan.pelvis_offset.abs() < 1.0e-6);
        assert!(plan.legs.iter().all(|leg| leg.weight == 0.0));
    }

    #[test]
    fn far_away_ground_fades_out() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        // Ground 2 m below the animated feet: beyond reach + fade.
        let far = 2.0;
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y - far, left.z)),
            FootGround::Contact(Vec3::new(right.x, right.y - far, right.z)),
        ];
        let feet = [left, right];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        // Pelvis clamps to the drop limit (the ground is 2 m down), and both legs fade to
        // zero weight.
        let settings = FootIkSettings::default();
        assert!(
            (plan.pelvis_offset + settings.max_pelvis_drop).abs() < 1.0e-6,
            "pelvis offset {} must clamp to -{}",
            plan.pelvis_offset,
            settings.max_pelvis_drop
        );
        assert!(plan.legs.iter().all(|leg| leg.weight <= 1.0e-6));
    }

    #[test]
    fn rig_rotation_targets_round_trip_to_world() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let feet = [left, right];
        let ground = [FootGround::Contact(left), FootGround::Contact(right)];
        // Rig rotated 180° about Y at (10, 0, 0): the object-space targets must map back
        // onto the world ground points under the rig transform.
        let yaw = Quat::from_rotation_y(std::f32::consts::PI);
        let rig_translation = Vec3::new(10.0, 0.0, 0.0);
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation,
                rig_rotation: yaw,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        for (leg, ground_point) in plan.legs.iter().zip(feet) {
            let back_to_world = yaw * leg.target + rig_translation;
            assert!(
                back_to_world.abs_diff_eq(ground_point, 1.0e-4),
                "object target {back_to_world:?} must round-trip to {ground_point:?}"
            );
        }
    }

    #[test]
    fn solver_plants_a_foot_on_a_step() {
        let sk = biped();
        let mut pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let step = 0.12;
        let feet = [left, right];
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y + step, left.z)),
            FootGround::Contact(right),
        ];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        solve_foot_ik(&sk, &mut pose, &plan, &legs(), Some(Vec3::NEG_Z));

        // The root joint moved up by the pelvis offset.
        let globals = sk.globals(&pose, None);
        let pelvis_y = globals[0].w_axis.truncate().y;
        assert!(
            (pelvis_y - (1.0 + plan.pelvis_offset)).abs() < 1.0e-4,
            "pelvis rose to {pelvis_y}"
        );
        // The left foot planted within 1 mm of its (re-based) target.
        let foot = globals[3].w_axis.truncate();
        assert!(
            foot.abs_diff_eq(plan.legs[0].target, 1.0e-3),
            "left foot {foot} vs target {}",
            plan.legs[0].target
        );
        // The right foot still reaches its ground point.
        let rfoot = globals[6].w_axis.truncate();
        assert!(rfoot.abs_diff_eq(plan.legs[1].target, 2.0e-3));
    }

    #[test]
    fn solver_with_zero_weights_changes_nothing() {
        let sk = biped();
        let mut pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let feet = [left, right];
        let ground = [FootGround::None, FootGround::None];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        let before = sk.globals(&pose, None);
        solve_foot_ik(&sk, &mut pose, &plan, &legs(), None);
        let after = sk.globals(&pose, None);
        for (b, a) in before.iter().zip(after.iter()) {
            let bt = b.w_axis.truncate();
            let at = a.w_axis.truncate();
            assert!(bt.abs_diff_eq(at, 1.0e-6), "airborne solve is a no-op");
        }
    }

    #[test]
    fn the_solver_plants_feet_on_lowered_ground() {
        // Both feet float 0.2 m above the ground: the pelvis drops 0.2 m. The solver moves the
        // root joint instead of the rig, so the feet must still land on the ground; aiming at
        // the plan's targets (made for a lowered rig) left them 0.2 m in the air.
        let sk = biped();
        let mut pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let drop = 0.2;
        let ground = [
            Vec3::new(left.x, left.y - drop, left.z),
            Vec3::new(right.x, right.y - drop, right.z),
        ];
        let feet = [left, right];
        let contacts = [FootGround::Contact(ground[0]), FootGround::Contact(ground[1])];
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &contacts,
            },
            &legs(),
            &FootIkSettings::default(),
        );
        assert!((plan.pelvis_offset + drop).abs() < 1e-6);
        solve_foot_ik(&sk, &mut pose, &plan, &legs(), Some(Vec3::NEG_Z));
        let globals = sk.globals(&pose, None);
        for (joint, ground_point) in [(3usize, ground[0]), (6, ground[1])] {
            let foot = globals[joint].w_axis.truncate();
            assert!(
                foot.abs_diff_eq(ground_point, 2e-3),
                "foot {joint} at {foot}, ground at {ground_point}"
            );
        }
    }

    #[test]
    fn negative_or_nan_pelvis_ranges_do_not_panic() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let left = foot_world(&sk, &pose, 3);
        let right = foot_world(&sk, &pose, 6);
        let feet = [left, right];
        let ground = [
            FootGround::Contact(Vec3::new(left.x, left.y - 0.5, left.z)),
            FootGround::Contact(right),
        ];
        for (drop, rise) in [(-0.1, 0.0), (0.1, -0.3), (f32::NAN, 0.2), (0.25, f32::NAN)] {
            let plan = plan_foot_ik(
                &FootIkInput {
                    rig_translation: Vec3::ZERO,
                    rig_rotation: Quat::IDENTITY,
                    feet_world: &feet,
                    ground: &ground,
                },
                &legs(),
                &FootIkSettings {
                    max_pelvis_drop: drop,
                    max_pelvis_rise: rise,
                    fade_distance: 0.5,
                },
            );
            assert!(plan.pelvis_offset.is_finite(), "{drop} {rise}");
            // Ranges are magnitudes: the drop never exceeds |max_pelvis_drop|.
            let limit = if drop.is_nan() { 0.0 } else { drop.abs() };
            assert!(
                plan.pelvis_offset >= -limit - 1e-6,
                "{drop}: {}",
                plan.pelvis_offset
            );
        }
    }

    #[test]
    fn negative_fade_distance_still_plants_a_perfect_foot() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let feet = [foot_world(&sk, &pose, 3), foot_world(&sk, &pose, 6)];
        let ground = [FootGround::Contact(feet[0]), FootGround::Contact(feet[1])];
        let settings = FootIkSettings {
            fade_distance: -0.5,
            ..FootIkSettings::default()
        };
        let plan = plan_foot_ik(
            &FootIkInput {
                rig_translation: Vec3::ZERO,
                rig_rotation: Quat::IDENTITY,
                feet_world: &feet,
                ground: &ground,
            },
            &legs(),
            &settings,
        );
        for leg in &plan.legs {
            assert!((leg.weight - 1.0).abs() < 1.0e-6, "weight {}", leg.weight);
        }
    }

    /// audit-v035-numerics M7 (fix-audit-wave12): a NaN probe on the only grounded leg folded the
    /// error list to +inf and lifted the pelvis by the full `max_pelvis_rise`. It plans no
    /// adjustment, as the leg's own weight already did; the finite leg still drives the pelvis.
    #[test]
    fn a_nan_ground_probe_does_not_lift_the_pelvis() {
        let sk = biped();
        let pose = sk.bind_pose_pose();
        let feet = [foot_world(&sk, &pose, 3), foot_world(&sk, &pose, 6)];
        let plan = |ground: &[FootGround]| {
            plan_foot_ik(
                &FootIkInput {
                    rig_translation: Vec3::ZERO,
                    rig_rotation: Quat::IDENTITY,
                    feet_world: &feet,
                    ground,
                },
                &legs(),
                &FootIkSettings::default(),
            )
        };
        let nan_only = plan(&[FootGround::Contact(Vec3::NAN), FootGround::None]);
        assert_eq!(nan_only.pelvis_offset, 0.0, "a NaN probe moves no pelvis");
        assert!(nan_only.legs.iter().all(|l| l.weight == 0.0));
        let lower = feet[1] - Vec3::Y * 0.1;
        let mixed = plan(&[FootGround::Contact(Vec3::NAN), FootGround::Contact(lower)]);
        assert!(
            (mixed.pelvis_offset + 0.1).abs() < 1e-5,
            "the finite leg drives the pelvis: {}",
            mixed.pelvis_offset
        );
    }
}
