// SPDX-License-Identifier: MPL-2.0
//! The graph's locomotion warp nodes (`.animgraph.ron` v4, ADR-8540): `StrideWarp` and
//! `OrientationWarp`.
//!
//! Specification: `docs/specs/17-animation.md` § 3. Both nodes read the inner graph's root
//! motion of this frame (a blend space blends its samples' root motion with the pose weights),
//! so a blend-space locomotion graph gets the corrections the motion-matching player has
//! ([`crate::motion_matching::warp`], the same code):
//!
//! - `StrideWarp` compares the gameplay ground speed (a float parameter) with the clip speed
//!   (horizontal root motion over the frame's clip time) and either scales the inner graph's
//!   clock (`Rate`: sync groups inside keep their phase, because they all run on the scaled
//!   clock) or scales the stride with leg IK (`Pose`, [`apply_stride_pose_scratch`]).
//! - `OrientationWarp` turns the pelvis toward the desired movement heading (a float
//!   parameter) by the clipped difference to the root-motion heading and turns the spine back
//!   ([`apply_orientation_warp_with_globals`]).
//!
//! The per-node state (the smoothed ratio, resolved joints and buffers) lives in the graph
//! instance ([`crate::eval::GraphInstance::Warp`]), so the debug snapshot shows the ratio and the
//! turn of every node and a warm rig allocates nothing per frame.
//!
//! Provenance (`docs/provenance.md`): the warp math is `motion_matching::warp`'s (moved unchanged to a shared module from
//! the motion-matching player); reading the clip speed and heading from the blended root motion
//! of one evaluation and scaling the inner clock for `Rate` are original code here.

use regia_core::math::{Mat4, Vec3};

use crate::eval::{EvalError, first_warning};
use crate::foot_ik::FootIkLeg;
use crate::graph::{OrientationWarpNode, StrideWarpNode};
use crate::ik::IkChain;
use crate::motion_matching::warp::{
    OrientationWarpSettings, StrideMode, StrideRequest, StrideWarp, StrideWarpScratch,
    apply_orientation_warp_with_globals, apply_stride_pose_scratch, orientation_delta,
};
use crate::num::det_atan2;
use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// Below this horizontal root motion per frame (meters) the inner graph has no movement
/// direction: the orientation warp passes the pose.
const MIN_TRAVEL: f32 = 1e-6;

/// The heading of an object-space movement direction as the warp nodes measure it: radians
/// about +Y, 0 for the character's forward (−Z), positive counter-clockwise seen from above
/// (toward −X), in `(−π, π]`. The vertical part is ignored; a zero direction gives 0.
#[must_use]
pub fn movement_heading(direction: Vec3) -> f32 {
    if direction.x == 0.0 && direction.z == 0.0 {
        return 0.0;
    }
    det_atan2(-direction.x, -direction.z)
}

/// Runtime state of one `StrideWarp` or `OrientationWarp` node: the smoothed stride ratio, the
/// last measured speeds and turn, the joints resolved on the rig and the solver buffers.
#[derive(Clone, Debug, Default)]
pub struct WarpNodeState {
    stride: StrideWarp,
    clip_speed: f32,
    ground_speed: f32,
    pelvis_drop: f32,
    orientation_delta: f32,
    clip_heading: f32,
    /// Address of the skeleton the joints were resolved on (0 = not resolved).
    skeleton: usize,
    legs: Vec<FootIkLeg>,
    pelvis: u32,
    orientation: OrientationWarpSettings,
    scratch: StrideWarpScratch,
    globals: Vec<Mat4>,
    /// The no-root-motion warning was considered for this node (the process-wide once-per-key
    /// check then runs only once per node, not every frame).
    warned: bool,
}

impl WarpNodeState {
    /// The smoothed stride ratio (1 = no warp): the clock multiplier in `Rate` mode, the stride
    /// scale in `Pose` mode. Always 1 for an orientation node.
    #[must_use]
    pub fn ratio(&self) -> f32 {
        self.stride.ratio()
    }

    /// The inner graph's clip speed measured on the latest frame, m/s (0 without root motion).
    #[must_use]
    pub fn clip_speed(&self) -> f32 {
        self.clip_speed
    }

    /// The ground-speed parameter on the latest frame, m/s.
    #[must_use]
    pub fn ground_speed(&self) -> f32 {
        self.ground_speed
    }

    /// How far `Pose` mode lowered the pelvis on the latest frame, meters (0 or more).
    #[must_use]
    pub fn pelvis_drop(&self) -> f32 {
        self.pelvis_drop
    }

    /// The lower-body turn the orientation node applied on the latest frame, radians.
    #[must_use]
    pub fn orientation_delta(&self) -> f32 {
        self.orientation_delta
    }

    /// The inner graph's root-motion heading on the latest frame ([`movement_heading`]).
    #[must_use]
    pub fn clip_heading(&self) -> f32 {
        self.clip_heading
    }

    /// The clock multiplier a `StrideWarp` node in `mode` gives its inner graph this frame
    /// (the ratio smoothed up to the previous frame for `Rate`, 1 for `Pose`).
    #[must_use]
    pub(crate) fn rate(&self, mode: StrideMode) -> f32 {
        self.stride.rate_multiplier(mode)
    }
}

fn joint_named(skeleton: &Skeleton, joint: u32, name: &str) -> bool {
    skeleton
        .joint_names
        .get(joint as usize)
        .is_some_and(|n| n == name)
}

fn find(skeleton: &Skeleton, name: &str) -> Result<u32, EvalError> {
    skeleton
        .joint_index(name)
        .ok_or_else(|| EvalError::MissingJoint(name.to_string()))
}

fn warn_no_root_motion(node: &str, param: &str) {
    if first_warning(format!("warp no root motion {node} {param}")) {
        tracing::warn!(
            target: "regia_animation",
            "animgraph_warp_no_root_motion: the {node} node driven by {param:?} sees no root motion from its inner graph; it passes the pose (give the clips a root-motion track)"
        );
    }
}

/// The horizontal part of a root-motion translation.
fn ground(translation: Vec3) -> Vec3 {
    Vec3::new(translation.x, 0.0, translation.z)
}

/// One frame's numbers for [`stride_after_inner`].
#[derive(Clone, Copy, Debug)]
pub(crate) struct StrideFrame {
    /// The inner graph's root-motion translation this frame (object space).
    pub(crate) translation: Vec3,
    /// The inner graph's clock step this frame, seconds (the frame's delta times the rate).
    pub(crate) clip_dt: f32,
    /// The frame's delta, seconds (the ratio's spring advances by it).
    pub(crate) dt: f32,
    /// The ground-speed parameter, m/s.
    pub(crate) ground_speed: f32,
}

/// The stride node's work after its inner graph ran: measure the clip speed from the inner root
/// motion (translation over the inner clock step), advance the ratio by the frame's delta and,
/// in `Pose` mode, scale the stride of `pose`.
///
/// # Errors
///
/// [`EvalError::MissingJoint`] when a `Pose`-mode leg or pelvis joint is not in the skeleton.
pub(crate) fn stride_after_inner(
    def: &StrideWarpNode,
    state: &mut WarpNodeState,
    skeleton: &Skeleton,
    pose: &mut Pose,
    frame: StrideFrame,
) -> Result<(), EvalError> {
    let StrideFrame {
        translation,
        clip_dt,
        dt,
        ground_speed,
    } = frame;
    let _span = tracing::trace_span!("anim_stride_warp", legs = def.legs.len()).entered();
    let travel = ground(translation);
    let clip_speed = if clip_dt > 0.0 && travel.is_finite() {
        travel.length() / clip_dt
    } else {
        0.0
    };
    let settings = &def.settings;
    if travel == Vec3::ZERO
        && dt > 0.0
        && settings.strength > 0.0
        && ground_speed.is_finite()
        && ground_speed >= settings.min_speed
        && !state.warned
    {
        state.warned = true;
        warn_no_root_motion("stride_warp", &def.ground_speed_param);
    }
    state.clip_speed = clip_speed;
    state.ground_speed = ground_speed;
    // No clip speed: the target ratio is 1, so the spring glides back and nothing is warped.
    state.stride.update(settings, ground_speed, clip_speed, dt);
    state.pelvis_drop = 0.0;
    if settings.mode != StrideMode::Pose {
        return Ok(());
    }
    let ratio = state.stride.stride_ratio(StrideMode::Pose);
    if (ratio - 1.0).abs() < 1e-6 || travel.length_squared() < MIN_TRAVEL * MIN_TRAVEL {
        return Ok(());
    }
    resolve_legs(def, state, skeleton)?;
    let request = StrideRequest {
        hips: state.pelvis,
        movement_dir: travel,
        ratio,
        stride_length: def.stride_length,
    };
    state.pelvis_drop = apply_stride_pose_scratch(skeleton, pose, &state.legs, request, &mut state.scratch);
    Ok(())
}

/// Resolve the stride node's legs and pelvis on `skeleton` unless they still name the same
/// joints (an edited node or a reloaded skeleton resolves again).
fn resolve_legs(
    def: &StrideWarpNode,
    state: &mut WarpNodeState,
    skeleton: &Skeleton,
) -> Result<(), EvalError> {
    let key = std::ptr::from_ref(skeleton) as usize;
    let current = state.skeleton == key
        && state.legs.len() == def.legs.len()
        && joint_named(skeleton, state.pelvis, &def.pelvis)
        && state.legs.iter().zip(&def.legs).all(|(leg, names)| {
            joint_named(skeleton, leg.chain.root, &names[0])
                && joint_named(skeleton, leg.chain.mid, &names[1])
                && joint_named(skeleton, leg.chain.tip, &names[2])
        });
    if current {
        return Ok(());
    }
    state.skeleton = 0;
    state.pelvis = find(skeleton, &def.pelvis)?;
    state.legs.clear();
    for [hip, knee, ankle] in &def.legs {
        let chain = IkChain {
            root: find(skeleton, hip)?,
            mid: find(skeleton, knee)?,
            tip: find(skeleton, ankle)?,
        };
        state.legs.push(FootIkLeg::new(chain));
    }
    state.skeleton = key;
    Ok(())
}

/// The orientation node's work after its inner graph ran: the turn between `desired_heading`
/// and the heading of the inner root motion (`translation`), applied to `pose`.
///
/// # Errors
///
/// [`EvalError::MissingJoint`] when the pelvis or a spine joint is not in the skeleton.
pub(crate) fn orient_after_inner(
    def: &OrientationWarpNode,
    state: &mut WarpNodeState,
    skeleton: &Skeleton,
    pose: &mut Pose,
    translation: Vec3,
    desired_heading: f32,
) -> Result<(), EvalError> {
    let _span = tracing::trace_span!("anim_orientation_warp", spine = def.spine.len()).entered();
    let travel = ground(translation);
    state.orientation_delta = 0.0;
    if !(travel.is_finite() && travel.length_squared() >= MIN_TRAVEL * MIN_TRAVEL) {
        if travel == Vec3::ZERO && desired_heading.is_finite() && desired_heading != 0.0 && !state.warned {
            state.warned = true;
            warn_no_root_motion("orientation_warp", &def.heading_param);
        }
        return Ok(());
    }
    state.clip_heading = movement_heading(travel);
    resolve_spine(def, state, skeleton)?;
    let delta = orientation_delta(&state.orientation, desired_heading, state.clip_heading);
    if delta != 0.0 {
        apply_orientation_warp_with_globals(
            skeleton,
            pose,
            state.pelvis,
            &state.orientation,
            delta,
            &mut state.globals,
        );
    }
    state.orientation_delta = delta;
    Ok(())
}

/// Resolve the orientation node's pelvis and spine on `skeleton` (kept while they still name
/// the same joints) and copy its strength, limit and shares into the kept settings.
fn resolve_spine(
    def: &OrientationWarpNode,
    state: &mut WarpNodeState,
    skeleton: &Skeleton,
) -> Result<(), EvalError> {
    let key = std::ptr::from_ref(skeleton) as usize;
    let settings = &mut state.orientation;
    let current = state.skeleton == key
        && settings.spine_joints.len() == def.spine.len()
        && joint_named(skeleton, state.pelvis, &def.pelvis)
        && settings
            .spine_joints
            .iter()
            .zip(&def.spine)
            .all(|(&joint, (name, _))| joint_named(skeleton, joint, name));
    if !current {
        state.skeleton = 0;
        state.pelvis = find(skeleton, &def.pelvis)?;
        settings.spine_joints.clear();
        for (name, _) in &def.spine {
            settings.spine_joints.push(find(skeleton, name)?);
        }
        state.skeleton = key;
    }
    settings.strength = def.strength;
    settings.max_delta = def.max_delta;
    if settings.spine_weights.len() != def.spine.len()
        || settings
            .spine_weights
            .iter()
            .zip(&def.spine)
            .any(|(w, (_, share))| w.to_bits() != share.to_bits())
    {
        settings.spine_weights.clear();
        settings
            .spine_weights
            .extend(def.spine.iter().map(|(_, share)| *share));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movement_heading_measures_from_forward_counter_clockwise() {
        assert_eq!(movement_heading(Vec3::NEG_Z), 0.0);
        assert!((movement_heading(Vec3::NEG_X) - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((movement_heading(Vec3::X) + std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((movement_heading(Vec3::Z).abs() - std::f32::consts::PI).abs() < 1e-6);
        assert_eq!(movement_heading(Vec3::Y), 0.0, "vertical only: no heading");
        // A turn about +Y by `a` maps forward to the direction whose heading is `a`.
        for a in [-2.5f32, -1.0, 0.3, 1.9] {
            let dir = regia_core::math::Quat::from_rotation_y(a) * Vec3::NEG_Z;
            assert!((movement_heading(dir) - a).abs() < 1e-5, "{a}");
        }
    }
}
