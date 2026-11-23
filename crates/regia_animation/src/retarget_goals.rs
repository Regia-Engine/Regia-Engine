// SPDX-License-Identifier: MPL-2.0
//! IK goals at retarget bake: keep end effectors where the source put them (ADR-8476).
//!
//! A retargeted clip takes the source's rotations and the target's bone lengths, scaling only the
//! root and the hips by the translation scale ([`crate::retarget`]). On a target whose leg (or
//! arm) proportions differ from that scale, a planted foot then slides or sinks and two hands on
//! one prop drift apart. An [`IkGoalSpec`] names a three-joint chain by roles
//! (`[upper, lower, end]`, humanoid or custom) and, at bake, puts the chain's end on a goal
//! derived from the source with the analytic two-bone solver ([`crate::ik::solve_two_bone`]):
//!
//! - **Goal.** The source end effector's object-space position times the translation scale
//!   ([`crate::retarget::Retargeter::scale`]), turned by the facing yaw. Under
//!   [`crate::RootMotionScale::Fixed`] the root-motion joint's travel moves by its own factor, as
//!   the body does.
//! - **[`IkGoalMode::ScaledWorld`]** uses that goal every frame (hands on a prop, a weapon grip).
//! - **[`IkGoalMode::Contacts`]** is for ground contacts. The goal's height moves from the
//!   source's bind height of the end joint to the target's (the ankle height correction), and
//!   over every run of frames in which the source end is planted the goal holds at the run's mean,
//!   blending back to the free goal over [`CONTACT_BLEND_SECONDS`](crate::retarget_goals::CONTACT_BLEND_SECONDS)
//!   on both sides (a run whose source end travels more than
//!   [`CONTACT_HOLD_MAX_TRAVEL`](crate::retarget_goals::CONTACT_HOLD_MAX_TRAVEL), a treadmill stance, is not held).
//!   The planted
//!   frames are the clip's cooked contact curve `contact.<source end joint>` when it has one
//!   (`foot_ik::contact`, shared with the planter), else the motion-matching contact labels
//!   (`motion_matching::contacts::label_contacts`) of the source end over the baked frames.
//! - The solve keeps the current bend plane (no pole) and the end's object-space rotation, and
//!   blends by [`IkGoalSpec::weight`]. Goals solve in list order.
//!
//! Goals run in [`crate::retarget::Retargeter::retarget_clip`] (and in
//! [`crate::retarget::Retargeter::check`], which reports each goal's residual and planted slide):
//! `Contacts` needs the whole clip, so `retarget_pose` stays a plain pose transfer. Without goals
//! (or with weight 0) the bake is bit for bit the bake without this pass.
//!
//! Provenance (`docs/provenance.md`): keeping end effectors on source-derived constraints after a
//! retarget is the approach of Gleicher, "Retargetting Motion to New Characters", SIGGRAPH 1998;
//! holding a foot at one position over each detected contact, with the adjustment blended in and
//! out around it, follows Kovar, Schreiner & Gleicher, "Footskate Cleanup for Motion Capture
//! Editing", SCA 2002 (also cited for the crowd's foot lock). The run mean, the blend window and
//! the bind-height correction are original code.

use regia_core::math::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::clip::AnimationClip;
use crate::foot_ik::contact::contact_curve_name;
use crate::ik::{IkChain, solve_two_bone_into};
use crate::motion_matching::ContactSettings;
use crate::motion_matching::contacts::label_contacts;
use crate::pose::Pose;
use crate::retarget::{Retargeter, position_of, rotation_of};
use crate::skeleton::Skeleton;

/// How long a `Contacts` goal blends from the free goal into a held contact and back, seconds.
pub const CONTACT_BLEND_SECONDS: f32 = 0.1;

/// A planted run over which the source end travels farther than this on the ground (source
/// meters) is not held: the clip walks in place (a treadmill clip's stance foot slides back in
/// object space), and holding it would stop the foot under a body that does not move.
pub const CONTACT_HOLD_MAX_TRAVEL: f32 = 0.15;

/// How an IK goal's target follows the source (`.retarget.ron` v2 `ik_goals`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum IkGoalMode {
    /// The source end's object-space position times the translation scale, every frame.
    #[default]
    ScaledWorld,
    /// `ScaledWorld` with the end's bind height moved to the target's, held fixed over the frames
    /// the source end is planted.
    Contacts,
}

fn full_weight() -> f32 {
    1.0
}

/// One IK goal of a retarget: a chain named by roles and how its end follows the source.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IkGoalSpec {
    /// `[upper, lower, end]` roles (`"leftUpperLeg"`, `"leftLowerLeg"`, `"leftFoot"`, or custom
    /// roles). On the target the three joints must be a parent → child chain.
    pub roles: [String; 3],
    /// How the goal follows the source.
    #[serde(default)]
    pub mode: IkGoalMode,
    /// Blend of the solve, `0..=1` (default 1).
    #[serde(default = "full_weight")]
    pub weight: f32,
}

impl IkGoalSpec {
    /// A full-weight goal on the chain `roles`.
    #[must_use]
    pub fn new(roles: [&str; 3], mode: IkGoalMode) -> Self {
        IkGoalSpec {
            roles: roles.map(str::to_string),
            mode,
            weight: 1.0,
        }
    }

    /// `Contacts` goals on both humanoid legs (left, then right).
    #[must_use]
    pub fn humanoid_feet() -> Vec<Self> {
        vec![
            Self::new(["leftUpperLeg", "leftLowerLeg", "leftFoot"], IkGoalMode::Contacts),
            Self::new(
                ["rightUpperLeg", "rightLowerLeg", "rightFoot"],
                IkGoalMode::Contacts,
            ),
        ]
    }

    /// `ScaledWorld` goals on both humanoid arms (left, then right): a two-handed hold.
    #[must_use]
    pub fn humanoid_hands() -> Vec<Self> {
        vec![
            Self::new(
                ["leftUpperArm", "leftLowerArm", "leftHand"],
                IkGoalMode::ScaledWorld,
            ),
            Self::new(
                ["rightUpperArm", "rightLowerArm", "rightHand"],
                IkGoalMode::ScaledWorld,
            ),
        ]
    }

    /// The end role (the joint the goal moves).
    #[must_use]
    pub fn end_role(&self) -> &str {
        &self.roles[2]
    }

    /// Checks the values: three distinct, non-empty roles and a weight in `0..=1`.
    ///
    /// # Errors
    ///
    /// A message naming the bad value.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.weight.is_finite() && (0.0..=1.0).contains(&self.weight)) {
            return Err(format!(
                "goal {:?}: weight {} must be in 0..=1",
                self.end_role(),
                self.weight
            ));
        }
        if self.roles.iter().any(|r| r.trim().is_empty()) {
            return Err(format!("goal {:?}: a role name is empty", self.roles));
        }
        let [a, b, c] = &self.roles;
        if a == b || b == c || a == c {
            return Err(format!("goal {:?}: the three roles must differ", self.roles));
        }
        Ok(())
    }
}

/// A goal paired onto the two rigs.
#[derive(Clone, Debug)]
pub(crate) struct GoalChain {
    /// The target chain.
    pub(crate) target: IkChain,
    /// The source joint whose position drives the goal.
    pub(crate) source_end: u32,
    pub(crate) mode: IkGoalMode,
    /// The clamped weight.
    pub(crate) weight: f32,
    /// `Contacts`: target bind height of the end minus the scaled source bind height.
    pub(crate) height_fix: f32,
}

/// One [`IkGoalSpec`] resolved against the rigs: its chain, or why it has none.
#[derive(Clone, Debug)]
pub(crate) struct ResolvedGoal {
    pub(crate) spec: IkGoalSpec,
    pub(crate) chain: Result<GoalChain, (&'static str, String)>,
}

impl ResolvedGoal {
    /// The chain when the goal resolved and has a weight.
    pub(crate) fn active(&self) -> Option<&GoalChain> {
        self.chain.as_ref().ok().filter(|c| c.weight > 0.0)
    }
}

/// Inputs of [`resolve_goals`] that `Retargeter::with_options` already has.
pub(crate) struct GoalRigs<'a> {
    pub(crate) target: &'a Skeleton,
    pub(crate) target_roles: &'a std::collections::BTreeMap<String, u32>,
    pub(crate) source_of: &'a [Option<u32>],
    pub(crate) source_bind: &'a [Mat4],
    pub(crate) target_bind: &'a [Mat4],
    pub(crate) scale: f32,
}

/// Pair every goal onto the rigs. A goal whose roles do not all map on both rigs is
/// `goal_unmapped`; one whose target joints are not a parent → child chain is `goal_not_a_chain`.
/// Neither is an error: [`Retargeter::check`] reports them and the bake skips them.
pub(crate) fn resolve_goals(specs: &[IkGoalSpec], rigs: &GoalRigs<'_>) -> Vec<ResolvedGoal> {
    specs
        .iter()
        .map(|spec| ResolvedGoal {
            spec: spec.clone(),
            chain: resolve_goal(spec, rigs),
        })
        .collect()
}

fn resolve_goal(spec: &IkGoalSpec, rigs: &GoalRigs<'_>) -> Result<GoalChain, (&'static str, String)> {
    let mut joints = [0u32; 3];
    for (slot, role) in joints.iter_mut().zip(&spec.roles) {
        let t = rigs
            .target_roles
            .get(role)
            .copied()
            .filter(|&t| rigs.source_of.get(t as usize).copied().flatten().is_some())
            .ok_or_else(|| {
                (
                    "goal_unmapped",
                    format!(
                        "goal {:?}: role {role:?} is not mapped on both rigs",
                        spec.end_role()
                    ),
                )
            })?;
        *slot = t;
    }
    let [upper, lower, end] = joints;
    let parent = |j: u32| rigs.target.parents.get(j as usize).copied().flatten();
    if parent(lower) != Some(upper) || parent(end) != Some(lower) {
        return Err((
            "goal_not_a_chain",
            format!(
                "goal {:?}: on the target, {:?} → {:?} → {:?} is not a parent → child chain",
                spec.end_role(),
                rigs.target.joint_names[upper as usize],
                rigs.target.joint_names[lower as usize],
                rigs.target.joint_names[end as usize]
            ),
        ));
    }
    let source_end = rigs.source_of[end as usize].ok_or_else(|| {
        (
            "goal_unmapped",
            format!("goal {:?}: the end has no source joint", spec.end_role()),
        )
    })?;
    let height = |binds: &[Mat4], j: u32| binds.get(j as usize).map_or(0.0, |m| position_of(*m).y);
    let height_fix = match spec.mode {
        IkGoalMode::Contacts => {
            height(rigs.target_bind, end) - rigs.scale * height(rigs.source_bind, source_end)
        }
        IkGoalMode::ScaledWorld => 0.0,
    };
    Ok(GoalChain {
        target: IkChain {
            root: upper,
            mid: lower,
            tip: end,
        },
        source_end,
        mode: spec.mode,
        weight: crate::num::unit_weight(spec.weight),
        height_fix: if height_fix.is_finite() { height_fix } else { 0.0 },
    })
}

/// Per-frame goals of one clip: for every goal of the retargeter (inactive ones empty), its
/// object-space target position and the source end's planted label, frame by frame.
#[derive(Clone, Debug, Default)]
pub(crate) struct GoalFrames {
    pub(crate) positions: Vec<Vec<Vec3>>,
    pub(crate) planted: Vec<Vec<bool>>,
}

/// Reused buffers of the goal solve.
#[derive(Debug, Default)]
pub(crate) struct GoalScratch {
    globals: Vec<Mat4>,
}

/// Runs of `true` in `labels`, as half-open frame ranges.
pub(crate) fn runs(labels: &[bool]) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let mut start = None;
    for (i, &planted) in labels.iter().chain(std::iter::once(&false)).enumerate() {
        match (planted, start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            _ => {}
        }
    }
    out
}

impl Retargeter {
    /// Whether any goal changes the bake.
    pub(crate) fn has_active_goals(&self) -> bool {
        self.goals.iter().any(|g| g.active().is_some())
    }

    /// The goals of `clip` at `times` (see [`GoalFrames`]). `joint_scale` is the bake's
    /// root-motion joint and factor when it differs from the translation scale.
    pub(crate) fn goal_frames(
        &self,
        clip: &AnimationClip,
        times: &[f32],
        joint_scale: Option<(usize, f32)>,
    ) -> GoalFrames {
        let _span = tracing::trace_span!("retarget_goal_frames", goals = self.goals.len()).entered();
        let n = times.len();
        let mut frames = GoalFrames {
            positions: vec![Vec::new(); self.goals.len()],
            planted: vec![Vec::new(); self.goals.len()],
        };
        if n == 0 || !self.has_active_goals() {
            return frames;
        }
        // Source end positions per goal, and the root-motion joint's source travel.
        let source_bind_pose = self.source.bind_pose_pose();
        let mut pose = source_bind_pose.clone();
        let mut globals: Vec<Mat4> = Vec::new();
        let mut ends: Vec<Vec<Vec3>> = vec![Vec::with_capacity(n); self.goals.len()];
        let mut travel: Vec<Vec3> = Vec::with_capacity(n);
        let root_source = joint_scale.and_then(|(joint, _)| self.source_of.get(joint).copied().flatten());
        for &time in times {
            pose.copy_from(&source_bind_pose);
            clip.sample_pose(time, &mut pose);
            self.source.globals_into(&pose, None, &mut globals);
            let at = |j: u32| {
                let p = globals.get(j as usize).map_or(Vec3::ZERO, |m| position_of(*m));
                if p.is_finite() { p } else { Vec3::ZERO }
            };
            for (goal, slot) in self.goals.iter().zip(ends.iter_mut()) {
                if let Some(chain) = goal.active() {
                    slot.push(at(chain.source_end));
                }
            }
            let t = root_source.map_or(Vec3::ZERO, |s| {
                at(s)
                    - self
                        .source_bind
                        .get(s as usize)
                        .map_or(Vec3::ZERO, |m| position_of(*m))
            });
            travel.push(t);
        }
        let factor = joint_scale.map_or(0.0, |(_, k)| k - self.scale());
        for (k, goal) in self.goals.iter().enumerate() {
            let Some(chain) = goal.active() else {
                continue;
            };
            let source = &ends[k];
            let free: Vec<Vec3> = source
                .iter()
                .zip(&travel)
                .map(|(&p, &t)| {
                    let mut g = self.facing * (p * self.scale() + t * factor);
                    g.y += chain.height_fix;
                    g
                })
                .collect();
            let labels = self.source_labels(clip, times, chain.source_end, source);
            frames.positions[k] = match chain.mode {
                IkGoalMode::ScaledWorld => free,
                IkGoalMode::Contacts => hold_contacts(
                    &free,
                    &labels,
                    times,
                    CONTACT_HOLD_MAX_TRAVEL * self.scale().abs(),
                ),
            };
            frames.planted[k] = labels;
        }
        frames
    }

    /// The planted label of source joint `joint` per frame: the clip's contact curve for it
    /// when it has one, else the contact labels of `track` (its object-space positions).
    fn source_labels(&self, clip: &AnimationClip, times: &[f32], joint: u32, track: &[Vec3]) -> Vec<bool> {
        let name = self
            .source
            .joint_names
            .get(joint as usize)
            .map(|n| contact_curve_name(n));
        if let Some(curve) = name.and_then(|name| clip.curves.iter().find(|c| c.name == name)) {
            return times.iter().map(|&t| curve.curve.sample(t) >= 0.5).collect();
        }
        let n = track.len().min(times.len());
        let lowest = track.iter().map(|p| p.y).fold(f32::INFINITY, f32::min);
        let heights: Vec<f32> = track.iter().map(|p| p.y - lowest).collect();
        let speeds: Vec<f32> = (0..n)
            .map(|i| {
                let (a, b) = (i.saturating_sub(1), (i + 1).min(n - 1));
                let span = times[b] - times[a];
                if span > 0.0 {
                    (track[b] - track[a]).length() / span
                } else {
                    0.0
                }
            })
            .collect();
        label_contacts(&heights, &speeds, false, &ContactSettings::default())
    }

    /// Solve every active goal of frame `frame` on `pose` (target local pose, in place) and write
    /// each goal's residual (end-to-goal distance after the solve, meters; 0 for inactive goals)
    /// into `residuals`.
    pub(crate) fn apply_goals(
        &self,
        frames: &GoalFrames,
        frame: usize,
        pose: &mut Pose,
        scratch: &mut GoalScratch,
        residuals: &mut Vec<f32>,
    ) {
        residuals.clear();
        residuals.resize(self.goals.len(), 0.0);
        for (k, goal) in self.goals.iter().enumerate() {
            let (Some(chain), Some(&target)) =
                (goal.active(), frames.positions.get(k).and_then(|p| p.get(frame)))
            else {
                continue;
            };
            let (mid, tip) = (chain.target.mid as usize, chain.target.tip as usize);
            if tip >= pose.len() {
                continue;
            }
            self.target.globals_into(pose, None, &mut scratch.globals);
            let Some(&tip_global) = scratch.globals.get(tip) else {
                continue;
            };
            let end_rotation = rotation_of(tip_global);
            solve_two_bone_into(
                &self.target,
                pose,
                &chain.target,
                target,
                None,
                chain.weight,
                &mut scratch.globals,
            );
            // Keep the end's object-space rotation: the solve turned its parents.
            self.target.globals_into(pose, None, &mut scratch.globals);
            let mid_rotation = rotation_of(scratch.globals[mid]);
            let local: Quat = (mid_rotation.inverse() * end_rotation).normalize();
            if local.is_finite() && end_rotation.is_finite() {
                pose.rotations[tip] = local;
            }
            residuals[k] = (position_of(scratch.globals[tip]) - target).length();
        }
    }
}

/// `Contacts`: over each planted run the goal holds at the run's mean free goal; within
/// [`CONTACT_BLEND_SECONDS`] of a run it blends from the free goal towards the held one. A run
/// whose free goal travels more than `max_travel` on the ground is left free.
fn hold_contacts(free: &[Vec3], labels: &[bool], times: &[f32], max_travel: f32) -> Vec<Vec3> {
    let n = free.len().min(labels.len());
    let mut out = free.to_vec();
    let flat = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
    let planted: Vec<std::ops::Range<usize>> = runs(&labels[..n])
        .into_iter()
        .filter(|run| {
            let start = flat(free[run.start]);
            free[run.clone()]
                .iter()
                .all(|&p| (flat(p) - start).length() <= max_travel)
        })
        .collect();
    if planted.is_empty() {
        return out;
    }
    let dt = if times.len() > 1 { times[1] - times[0] } else { 0.0 };
    let blend = if dt > 0.0 {
        ((CONTACT_BLEND_SECONDS / dt).round() as usize).min(n)
    } else {
        0
    };
    // Per frame: the strongest hold reaching it (weight, position).
    let mut hold: Vec<(f32, Vec3)> = vec![(0.0, Vec3::ZERO); n];
    for run in &planted {
        let sum = free[run.clone()].iter().fold(Vec3::ZERO, |a, &p| a + p);
        let held = sum / run.len() as f32;
        for slot in &mut hold[run.clone()] {
            *slot = (1.0, held);
        }
        for d in 1..=blend {
            let w = 1.0 - d as f32 / (blend + 1) as f32;
            for i in [run.start.checked_sub(d), Some(run.end - 1 + d).filter(|&i| i < n)]
                .into_iter()
                .flatten()
            {
                if w > hold[i].0 {
                    hold[i] = (w, held);
                }
            }
        }
    }
    for (slot, &(w, held)) in out.iter_mut().zip(&hold) {
        if w > 0.0 {
            *slot = slot.lerp(held, w);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contacts_hold_at_the_run_mean_and_blend_out() {
        let free: Vec<Vec3> = (0..12).map(|i| Vec3::new(i as f32 * 0.1, 0.0, 0.0)).collect();
        let labels: Vec<bool> = (0..12).map(|i| (4..7).contains(&i)).collect();
        let times: Vec<f32> = (0..12).map(|i| i as f32 / 30.0).collect();
        let out = hold_contacts(&free, &labels, &times, 1.0);
        for p in &out[4..7] {
            assert!((p.x - 0.5).abs() < 1e-6, "{p:?}");
        }
        // A run that travels (a treadmill stance) is left free.
        assert_eq!(hold_contacts(&free, &labels, &times, 0.15), free);
        // 0.1 s at 30 Hz: three blend frames each side, weights 3/4, 2/4, 1/4.
        assert!((out[3].x - (0.3 + (0.5 - 0.3) * 0.75)).abs() < 1e-6);
        assert!((out[7].x - (0.7 + (0.5 - 0.7) * 0.75)).abs() < 1e-6);
        assert_eq!(out[0], free[0]);
        assert_eq!(out[11], free[11]);
        assert_eq!(runs(&[true, false, true, true]), vec![0..1, 2..4]);
    }

    #[test]
    fn goal_specs_validate() {
        assert!(IkGoalSpec::humanoid_feet().iter().all(|g| g.validate().is_ok()));
        assert!(IkGoalSpec::humanoid_hands().iter().all(|g| g.validate().is_ok()));
        let mut g = IkGoalSpec::new(["a", "b", "c"], IkGoalMode::Contacts);
        g.weight = f32::NAN;
        assert!(g.validate().is_err());
        g.weight = 1.0;
        g.roles[1] = "a".into();
        assert!(g.validate().is_err());
        g.roles[1] = " ".into();
        assert!(g.validate().is_err());
    }
}
