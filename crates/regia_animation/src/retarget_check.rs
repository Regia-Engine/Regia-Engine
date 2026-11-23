// SPDX-License-Identifier: MPL-2.0
//! Retarget quality report: is this retarget right, before anyone looks at it on screen?
//!
//! Specification: `docs/specs/85-retarget-crowds.md` § 2.4. [`Retargeter::check`] reports the
//! mapping (mapped, unmatched and missing roles, rest corrections, scales, facing) and, for a
//! clip, how faithfully the bake follows the source: the bone-direction error between the rigs,
//! root speed against the root-motion scale, and the planted foot's skate on both rigs. A clip
//! without root motion is measured against its stance feet instead (`stance_velocity`). With IK
//! goals (`RetargetOptions::ik_goals`) the clip is measured with the goals solved, as the bake
//! does, and each goal reports its residual and (for `Contacts` goals) its planted slide.
//!
//! Provenance (`docs/provenance.md`): bone-direction comparison and foot-skate measurement
//! (horizontal speed of a foot in ground contact, contact from a height threshold over the
//! foot's lowest point) are the standard retarget quality measures of Gleicher, "Retargetting
//! Motion to New Characters", SIGGRAPH 1998 (footplant constraints) and Kovar, Schreiner &
//! Gleicher, "Footskate Cleanup for Motion Capture Editing", SCA 2002; the thresholds are ours.

use regia_core::math::{Mat4, Vec3};
use serde::Serialize;

use crate::clip::AnimationClip;
use crate::pose::Pose;
use crate::retarget::{HumanoidBone, IkGoalMode, Retargeter, is_ancestor, position_of};
use crate::retarget_goals::{GoalScratch, runs};

/// The most frames [`Retargeter::check`] samples from one clip.
pub const CHECK_MAX_FRAMES: usize = 600;
/// Height above the foot's lowest point under which the source foot counts as planted, meters.
pub const CONTACT_HEIGHT: f32 = 0.02;
/// A planted source foot moves at most this fraction of the clip's root speed, plus
/// [`CONTACT_SPEED_FLOOR`] (the velocity half of the footplant test: a low foot that is still
/// swinging is not planted).
pub const CONTACT_SPEED: f32 = 0.25;
/// Source foot speed always allowed to a planted foot, m/s.
pub const CONTACT_SPEED_FLOOR: f32 = 0.1;
/// Rest corrections above this many degrees are reported as `large_rest_correction`.
pub const LARGE_REST_CORRECTION_DEG: f32 = 100.0;
/// Worst bone-direction errors above this many degrees are reported as `direction_error`.
pub const DIRECTION_ERROR_DEG: f32 = 5.0;
/// Allowed relative difference of the root speed ratio from the root-motion scale.
pub const SPEED_RATIO_TOLERANCE: f32 = 0.05;
/// Allowed relative difference of the stance speed ratio from the leg ratio. Stride also depends
/// on the hip joints' width and the knee bend, so this is a coarser check than the root's.
pub const STANCE_RATIO_TOLERANCE: f32 = 0.15;
/// Allowed extra target skate over the scaled source skate, m/s.
pub const SKATE_TOLERANCE: f32 = 0.05;
/// A full-weight IK goal whose end stays farther than this from its goal, meters, is reported as
/// `goal_unreached` (the chain is too short, or the goal folds it).
pub const GOAL_RESIDUAL_TOLERANCE: f32 = 0.01;
/// A `Contacts` goal whose end moves farther than this over a planted run, meters, is reported as
/// `goal_slide`.
pub const GOAL_SLIDE_TOLERANCE: f32 = 0.01;
/// Stance feet slower than this, m/s, mean the clip goes nowhere (an idle).
pub const STANCE_MIN_SPEED: f32 = 0.1;
/// The share of a foot's samples, lowest first, taken as its stance by [`stance_velocity`]. It
/// is below the planted share of a run, so swing frames are the minority the median drops.
const STANCE_SHARE: f32 = 0.3;

/// One mapped role.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct MappedRole {
    /// The role name.
    pub role: String,
    /// The source joint name.
    pub source_joint: String,
    /// The target joint name.
    pub target_joint: String,
    /// The rest correction, degrees.
    pub rest_correction_deg: f32,
}

/// One problem found by [`Retargeter::check`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetargetIssue {
    /// Stable code: `missing_role`, `large_rest_correction`, `direction_error`,
    /// `root_motion_lost`, `speed_ratio`, `foot_skate`, `facing_turned`, `goal_unmapped`,
    /// `goal_not_a_chain`, `goal_unreached` or `goal_slide`.
    pub code: &'static str,
    /// Human-readable detail.
    pub detail: String,
}

/// One IK goal of a checked clip (`RetargetOptions::ik_goals`, in order).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct GoalCheck {
    /// The goal's `[upper, lower, end]` roles.
    pub roles: [String; 3],
    /// The goal's mode.
    pub mode: IkGoalMode,
    /// The goal's weight.
    pub weight: f32,
    /// Whether the goal paired onto both rigs (otherwise an issue says why and the bake skips it).
    pub resolved: bool,
    /// The largest distance from the end to its goal after the solve, meters.
    pub max_residual: f32,
    /// The mean distance from the end to its goal after the solve, meters.
    pub mean_residual: f32,
    /// Frames in which the source end is planted.
    pub planted_frames: usize,
    /// `Contacts` goals: the largest horizontal distance the target end moves from where it stood
    /// at the start of a planted run, meters (`None` for `ScaledWorld` goals or without contact).
    pub planted_slide: Option<f32>,
}

/// The per-clip part of a [`RetargetReport`].
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct ClipCheck {
    /// Clip name.
    pub name: String,
    /// Frames sampled.
    pub frames: usize,
    /// Worst bone-direction error between source and target, degrees.
    pub max_direction_error_deg: f32,
    /// Mean bone-direction error, degrees.
    pub mean_direction_error_deg: f32,
    /// Role of the target bone with the worst error.
    pub worst_role: Option<String>,
    /// Clip time of the worst error, seconds.
    pub worst_time: f32,
    /// Source root-motion ground speed, m/s (`None` without root motion).
    pub source_root_speed: Option<f32>,
    /// Baked root-motion ground speed, m/s (`None` without root motion).
    pub target_root_speed: Option<f32>,
    /// Target speed over source speed: of the root motion, or of the stance feet for a clip
    /// without it. The feet of a retargeted clip stride with the legs, so a stance ratio follows
    /// [`RetargetReport::leg_ratio`], not the hip-height scale.
    pub speed_ratio: Option<f32>,
    /// Ground speed of a clip without root motion, from the source's stance feet, m/s (`None`
    /// with root motion, without both feet mapped, or when the feet do not slide: an idle, or a
    /// clip that travels on the hips).
    pub source_stance_speed: Option<f32>,
    /// The same from the bake, m/s.
    pub target_stance_speed: Option<f32>,
    /// Source planted-foot skate, m/s (`None` without both feet mapped or no contact).
    pub source_foot_skate: Option<f32>,
    /// Target planted-foot skate over the same frames, m/s.
    pub target_foot_skate: Option<f32>,
    /// Every IK goal of the retargeter, in order (empty without goals).
    pub goals: Vec<GoalCheck>,
}

/// What [`Retargeter::check`] found.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct RetargetReport {
    /// Mapped roles, largest rest correction first (ties by role name).
    pub mapped: Vec<MappedRole>,
    /// Roles only the source plays.
    pub unmatched_source_roles: Vec<String>,
    /// Roles only the target plays.
    pub unmatched_target_roles: Vec<String>,
    /// Required humanoid roles that are not mapped (only when the hips are mapped).
    pub missing_humanoid_roles: Vec<String>,
    /// Translation scale.
    pub scale: f32,
    /// Root-motion scale (`None`: root motion dropped).
    pub root_motion_scale: Option<f32>,
    /// Target leg length over source leg length (hip joint to knee to ankle, both sides; `None`
    /// without both legs mapped). When it differs from `scale`, root motion scaled by `scale`
    /// skates; `RootMotionScale::Fixed` with this value fixes that.
    pub leg_ratio: Option<f32>,
    /// Facing yaw between the rigs, degrees.
    pub facing_yaw_deg: f32,
    /// The clip part, when a clip was checked.
    pub clip: Option<ClipCheck>,
    /// Problems, in the order of the checks.
    pub issues: Vec<RetargetIssue>,
}

impl RetargetReport {
    /// Whether no issue other than the informational `facing_turned` was found.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.issues.iter().all(|i| i.code == "facing_turned")
    }
}

/// Bone pairs compared by the direction check: (target joint, target child, source joint,
/// source child).
fn bone_pairs(rt: &Retargeter) -> Vec<(usize, usize, usize, usize)> {
    let target = &rt.target;
    let mut out = Vec::new();
    for t in 0..target.len() {
        let Some(s) = rt.source_of[t] else { continue };
        let child = (0..target.len())
            .filter(|&c| {
                rt.source_of[c].is_some_and(|sc| is_ancestor(&rt.source, s, sc))
                    && is_ancestor(target, t as u32, c as u32)
            })
            .min_by_key(|&c| {
                let mut d = 0u32;
                let mut p = target.parents[c];
                while let Some(q) = p {
                    d += 1;
                    p = target.parents[q as usize];
                }
                d
            });
        if let Some(c) = child
            && let Some(sc) = rt.source_of[c]
        {
            out.push((t, c, s as usize, sc as usize));
        }
    }
    out
}

fn horizontal(v: Vec3) -> Vec3 {
    Vec3::new(v.x, 0.0, v.z)
}

fn median(values: &mut [f32]) -> f32 {
    values.sort_by(f32::total_cmp);
    values.get(values.len() / 2).copied().unwrap_or(0.0)
}

/// The ground velocity of a clip that plays in place, from `feet` (left and right at evenly
/// spaced times over one cycle, `dt` apart): on the ground a foot moves back at the gait speed,
/// so the clip moves forward at minus the stance velocity. Each foot's stance is its lowest
/// `STANCE_SHARE` of samples; the median velocity over them ignores the swing frames among
/// them; the two feet average, so a turn on the spot, where they disagree, measures nothing.
pub(crate) fn stance_velocity(feet: &[[Vec3; 2]], dt: f32) -> Vec3 {
    let n = feet.len();
    if n < 8 || dt <= 0.0 {
        return Vec3::ZERO;
    }
    let stance = ((n as f32 * STANCE_SHARE) as usize).max(3);
    let [left, right] = [0, 1].map(|side| {
        let mut order: Vec<usize> = (0..n).collect();
        order.sort_by(|&a, &b| feet[a][side].y.total_cmp(&feet[b][side].y).then(a.cmp(&b)));
        let velocity = |k: usize| {
            let before = feet[(k + n - 1) % n][side];
            let after = feet[(k + 1) % n][side];
            horizontal(after - before) / (2.0 * dt)
        };
        let mut x: Vec<f32> = order[..stance].iter().map(|&k| velocity(k).x).collect();
        let mut z: Vec<f32> = order[..stance].iter().map(|&k| velocity(k).z).collect();
        Vec3::new(median(&mut x), 0.0, median(&mut z))
    });
    (left + right) * -0.5
}

impl Retargeter {
    /// Check this retarget, and `clip` (authored for the source) when given. Pure and
    /// deterministic; samples at most [`CHECK_MAX_FRAMES`] frames.
    #[must_use]
    pub fn check(&self, clip: Option<&AnimationClip>) -> RetargetReport {
        let mut issues = Vec::new();
        let mut mapped: Vec<MappedRole> = (0..self.target.len())
            .filter_map(|t| {
                let role = self.role_of(t as u32)?;
                let s = self.source_of[t]?;
                Some(MappedRole {
                    role: role.to_string(),
                    source_joint: self.source.joint_names[s as usize].clone(),
                    target_joint: self.target.joint_names[t].clone(),
                    rest_correction_deg: self.rest_correction_degrees(t as u32),
                })
            })
            .collect();
        // Largest rest correction first (spec 85 § 2.4), ties by role name: a total order.
        mapped.sort_by(|a, b| {
            b.rest_correction_deg
                .total_cmp(&a.rest_correction_deg)
                .then_with(|| a.role.cmp(&b.role))
        });
        let is_mapped = |role: &str| mapped.iter().any(|m| m.role == role);
        let missing_humanoid_roles: Vec<String> = if is_mapped(HumanoidBone::Hips.name()) {
            HumanoidBone::ALL
                .into_iter()
                .filter(|r| r.required() && !is_mapped(r.name()))
                .map(|r| r.name().to_string())
                .collect()
        } else {
            Vec::new()
        };
        for role in &missing_humanoid_roles {
            issues.push(RetargetIssue {
                code: "missing_role",
                detail: format!("required humanoid role {role:?} is not mapped"),
            });
        }
        for m in &mapped {
            if m.rest_correction_deg > LARGE_REST_CORRECTION_DEG {
                issues.push(RetargetIssue {
                    code: "large_rest_correction",
                    detail: format!(
                        "role {:?} ({} → {}) needs a {:.0}° rest correction: probably a wrong mapping",
                        m.role, m.source_joint, m.target_joint, m.rest_correction_deg
                    ),
                });
            }
        }
        let facing_yaw_deg = self.facing_yaw().to_degrees();
        if facing_yaw_deg.abs() > 0.5 {
            issues.push(RetargetIssue {
                code: "facing_turned",
                detail: format!("the source is turned {facing_yaw_deg:.0}° to face the target's way"),
            });
        }
        for goal in &self.goals {
            if let Err((code, detail)) = &goal.chain {
                issues.push(RetargetIssue {
                    code,
                    detail: detail.clone(),
                });
            }
        }
        let (unmatched_source, unmatched_target) = self.unmatched_roles();
        let clip_check = clip.map(|clip| self.check_clip(clip, &mut issues));
        RetargetReport {
            mapped,
            unmatched_source_roles: unmatched_source.to_vec(),
            unmatched_target_roles: unmatched_target.to_vec(),
            missing_humanoid_roles,
            scale: self.scale(),
            root_motion_scale: self.root_motion_scale(),
            leg_ratio: self.leg_ratio(),
            facing_yaw_deg,
            clip: clip_check,
            issues,
        }
    }

    /// Target leg length over source leg length, both sides.
    fn leg_ratio(&self) -> Option<f32> {
        let (source, target) = (self.source.bind_globals(), self.target.bind_globals());
        let length = |globals: &[Mat4], a: usize, b: usize, c: usize| {
            let at = |j: usize| globals.get(j).map(|m| position_of(*m));
            Some((at(a)? - at(b)?).length() + (at(b)? - at(c)?).length())
        };
        let leg = |bones: [HumanoidBone; 3]| {
            let [a, b, c] = bones
                .map(|bone| (0..self.target.len()).find(|&t| self.role_of(t as u32) == Some(bone.name())));
            let (a, b, c) = (a?, b?, c?);
            let of = |t: usize| self.source_of[t].map(|s| s as usize);
            Some((
                length(&target, a, b, c)?,
                length(&source, of(a)?, of(b)?, of(c)?)?,
            ))
        };
        let (lt, ls) = leg([
            HumanoidBone::LeftUpperLeg,
            HumanoidBone::LeftLowerLeg,
            HumanoidBone::LeftFoot,
        ])?;
        let (rt, rs) = leg([
            HumanoidBone::RightUpperLeg,
            HumanoidBone::RightLowerLeg,
            HumanoidBone::RightFoot,
        ])?;
        (ls + rs > 1e-4).then_some((lt + rt) / (ls + rs))
    }

    fn check_clip(&self, clip: &AnimationClip, issues: &mut Vec<RetargetIssue>) -> ClipCheck {
        let duration = crate::num::finite_or(clip.duration, 0.0).max(0.0);
        let rate = if clip.sample_rate.is_finite() && clip.sample_rate > 0.0 {
            clip.sample_rate
        } else {
            30.0
        };
        let frames = (((duration * rate).ceil().min(CHECK_MAX_FRAMES as f32) as usize).max(1) + 1)
            .min(CHECK_MAX_FRAMES);
        let times: Vec<f32> = (0..frames)
            .map(|k| (k as f32 / (frames - 1).max(1) as f32) * duration)
            .collect();
        let root_motion_target = clip
            .root_motion
            .as_ref()
            .and_then(|rm| self.source_of.iter().position(|s| *s == Some(rm.joint)));
        let joint_scale = match (root_motion_target, self.root_motion_scale()) {
            (Some(t), Some(k)) if (k - self.scale()).abs() > f32::EPSILON => Some((t, k)),
            _ => None,
        };
        let pairs = bone_pairs(self);
        let feet = [HumanoidBone::LeftFoot, HumanoidBone::RightFoot].map(|role| {
            let t = (0..self.target.len()).find(|&t| self.role_of(t as u32) == Some(role.name()))?;
            Some((self.source_of[t]? as usize, t))
        });
        let source_bind_pose = self.source.bind_pose_pose();
        let mut source_pose = source_bind_pose.clone();
        let mut target_pose = Pose::default();
        let mut worst = (0.0f32, None::<usize>, 0.0f32);
        let mut sum = 0.0f64;
        let mut count = 0u64;
        // Per frame: foot positions [left, right] on both rigs.
        let mut source_feet: Vec<[Vec3; 2]> = Vec::with_capacity(frames);
        let mut target_feet: Vec<[Vec3; 2]> = Vec::with_capacity(frames);
        // IK goals, solved as the bake solves them; per goal: residuals and end positions.
        let goal_frames = self
            .has_active_goals()
            .then(|| self.goal_frames(clip, &times, joint_scale));
        let mut goal_scratch = GoalScratch::default();
        let mut residuals = Vec::new();
        let mut goal_residuals: Vec<Vec<f32>> = vec![Vec::new(); self.goals.len()];
        let mut goal_ends: Vec<Vec<Vec3>> = vec![Vec::new(); self.goals.len()];
        for (frame, &time) in times.iter().enumerate() {
            source_pose.copy_from(&source_bind_pose);
            clip.sample_pose(time, &mut source_pose);
            self.retarget_pose_scaled(&source_pose, &mut target_pose, joint_scale);
            if let Some(goals) = &goal_frames {
                self.apply_goals(goals, frame, &mut target_pose, &mut goal_scratch, &mut residuals);
            }
            let sg: Vec<Mat4> = self.source.globals(&source_pose, None);
            let tg: Vec<Mat4> = self.target.globals(&target_pose, None);
            for (k, goal) in self.goals.iter().enumerate() {
                if let Some(chain) = goal.active() {
                    goal_residuals[k].push(residuals.get(k).copied().unwrap_or(0.0));
                    goal_ends[k].push(
                        tg.get(chain.target.tip as usize)
                            .map_or(Vec3::ZERO, |m| position_of(*m)),
                    );
                }
            }
            for &(t, c, s, sc) in &pairs {
                let dt = (position_of(tg[c]) - position_of(tg[t])).normalize_or_zero();
                let ds = self.facing * (position_of(sg[sc]) - position_of(sg[s])).normalize_or_zero();
                if dt.length_squared() < 0.5 || ds.length_squared() < 0.5 {
                    continue;
                }
                let angle = dt.angle_between(ds).to_degrees();
                if !angle.is_finite() {
                    continue;
                }
                sum += f64::from(angle);
                count += 1;
                if angle > worst.0 {
                    worst = (angle, Some(t), time);
                }
            }
            if let [Some((sl, tl)), Some((sr, tr))] = feet {
                source_feet.push([
                    self.facing * position_of(sg[sl]),
                    self.facing * position_of(sg[sr]),
                ]);
                target_feet.push([position_of(tg[tl]), position_of(tg[tr])]);
            }
        }
        let mean = if count == 0 {
            0.0
        } else {
            (sum / count as f64) as f32
        };
        let worst_role = worst.1.and_then(|t| self.role_of(t as u32)).map(str::to_string);
        if worst.0 > DIRECTION_ERROR_DEG {
            issues.push(RetargetIssue {
                code: "direction_error",
                detail: format!(
                    "clip {:?}: bone {:?} points {:.1}° away from the source at {:.2} s",
                    clip.name,
                    worst_role.as_deref().unwrap_or("?"),
                    worst.0,
                    worst.2
                ),
            });
        }

        // Root speed: ground travel over one play of the clip.
        let baked = self.retarget_clip(clip);
        let speed = |c: &AnimationClip| {
            let (delta, _) = c.root_motion_delta_full(0.0, c.duration, false)?;
            (c.duration > 0.0).then(|| horizontal(delta).length() / c.duration)
        };
        let source_root_speed = speed(clip);
        let target_root_speed = speed(&baked);
        if clip.root_motion.is_some() && baked.root_motion.is_none() && self.root_motion_scale().is_some() {
            issues.push(RetargetIssue {
                code: "root_motion_lost",
                detail: format!(
                    "clip {:?}: the root-motion joint drives no target joint; the bake plays in place",
                    clip.name
                ),
            });
        }
        let root_ratio = match (source_root_speed, target_root_speed) {
            (Some(s), Some(t)) if s > 1e-4 => Some(t / s),
            _ => None,
        };
        // Without root motion the feet say how fast the clip goes: the planted foot slides back
        // on a treadmill, and that belt comes off the skate below.
        let belts = if root_ratio.is_none() && source_feet.len() > 1 {
            let dt = duration / (source_feet.len() - 1) as f32;
            let belt = |feet: &[[Vec3; 2]]| stance_velocity(&feet[1..], dt);
            [belt(&source_feet), belt(&target_feet)]
        } else {
            [Vec3::ZERO; 2]
        };
        let (source_stance_speed, target_stance_speed) = match belts.map(Vec3::length) {
            [s, t] if s >= STANCE_MIN_SPEED => (Some(s), Some(t)),
            _ => (None, None),
        };
        let stance_ratio = source_stance_speed.zip(target_stance_speed).map(|(s, t)| t / s);
        let speed_ratio = root_ratio.or(stance_ratio);
        let (expected, tolerance) = if root_ratio.is_some() {
            (self.root_motion_scale(), SPEED_RATIO_TOLERANCE)
        } else {
            (
                Some(self.leg_ratio().unwrap_or(self.scale())),
                STANCE_RATIO_TOLERANCE,
            )
        };
        if let (Some(ratio), Some(k)) = (speed_ratio, expected)
            && k > 1e-4
            && ((ratio / k) - 1.0).abs() > tolerance
        {
            issues.push(RetargetIssue {
                code: "speed_ratio",
                detail: format!(
                    "clip {:?}: speed ratio {ratio:.3} differs from the expected scale {k:.3}",
                    clip.name
                ),
            });
        }

        // Foot skate: the planted foot's horizontal speed over frames where the source foot is
        // within `CONTACT_HEIGHT` of its lowest point, below the other foot, and nearly still.
        let ground_speed = source_root_speed.unwrap_or(0.0).max(belts[0].length());
        let max_speed = CONTACT_SPEED * ground_speed + CONTACT_SPEED_FLOOR;
        let (source_foot_skate, target_foot_skate) =
            foot_skate(&source_feet, &target_feet, &times, max_speed, belts);
        if let (Some(source), Some(target)) = (source_foot_skate, target_foot_skate) {
            let allowed = source * stance_ratio.unwrap_or(self.scale()) + SKATE_TOLERANCE;
            if target > allowed {
                issues.push(RetargetIssue {
                    code: "foot_skate",
                    detail: format!(
                        "clip {:?}: the planted foot slides {target:.3} m/s on the target, {source:.3} m/s on the source (allowed {allowed:.3})",
                        clip.name
                    ),
                });
            }
        }
        let goals = self.goal_checks(clip, goal_frames.as_ref(), &goal_residuals, &goal_ends, issues);
        ClipCheck {
            name: clip.name.clone(),
            frames,
            max_direction_error_deg: worst.0,
            mean_direction_error_deg: mean,
            worst_role,
            worst_time: worst.2,
            source_root_speed,
            target_root_speed,
            speed_ratio,
            source_stance_speed,
            target_stance_speed,
            source_foot_skate,
            target_foot_skate,
            goals,
        }
    }

    /// The per-goal part of a clip check, with its `goal_unreached` and `goal_slide` issues.
    fn goal_checks(
        &self,
        clip: &AnimationClip,
        frames: Option<&crate::retarget_goals::GoalFrames>,
        residuals: &[Vec<f32>],
        ends: &[Vec<Vec3>],
        issues: &mut Vec<RetargetIssue>,
    ) -> Vec<GoalCheck> {
        self.goals
            .iter()
            .enumerate()
            .map(|(k, goal)| {
                let spec = &goal.spec;
                let residual = residuals.get(k).map_or(&[][..], Vec::as_slice);
                let max_residual = residual.iter().copied().fold(0.0f32, f32::max);
                let mean_residual = if residual.is_empty() {
                    0.0
                } else {
                    residual.iter().map(|&r| f64::from(r)).sum::<f64>() as f32 / residual.len() as f32
                };
                let planted = frames
                    .and_then(|f| f.planted.get(k))
                    .map_or(&[][..], Vec::as_slice);
                let end = ends.get(k).map_or(&[][..], Vec::as_slice);
                let planted_runs = runs(planted);
                let planted_slide = (spec.mode == IkGoalMode::Contacts
                    && !planted_runs.is_empty()
                    && end.len() >= planted.len())
                .then(|| {
                    planted_runs
                        .iter()
                        .flat_map(|run| {
                            let start = end[run.start];
                            end[run.clone()].iter().map(move |p| horizontal(*p - start).length())
                        })
                        .fold(0.0f32, f32::max)
                });
                let active = goal.active();
                if active.is_some_and(|c| c.weight >= 1.0) && max_residual > GOAL_RESIDUAL_TOLERANCE {
                    issues.push(RetargetIssue {
                        code: "goal_unreached",
                        detail: format!(
                            "clip {:?}: goal {:?} ends up to {max_residual:.3} m from its target (the chain cannot reach it)",
                            clip.name,
                            spec.end_role()
                        ),
                    });
                }
                if let Some(slide) = planted_slide
                    && active.is_some()
                    && slide > GOAL_SLIDE_TOLERANCE
                {
                    issues.push(RetargetIssue {
                        code: "goal_slide",
                        detail: format!(
                            "clip {:?}: goal {:?} slides {slide:.3} m over a planted run",
                            clip.name,
                            spec.end_role()
                        ),
                    });
                }
                GoalCheck {
                    roles: spec.roles.clone(),
                    mode: spec.mode,
                    weight: spec.weight,
                    resolved: goal.chain.is_ok(),
                    max_residual,
                    mean_residual,
                    planted_frames: planted.iter().filter(|&&p| p).count(),
                    planted_slide,
                }
            })
            .collect()
    }
}

/// Mean horizontal speed of the planted foot on both rigs, over consecutive frame pairs in which
/// the same source foot is planted: low (`CONTACT_HEIGHT`), below the other foot, and moving at
/// most `max_speed` on the source (Kovar, Schreiner & Gleicher 2002: height and velocity).
/// `belts` are the ground velocities of an in-place clip on each rig (zero otherwise), which a
/// planted foot moves back at: speeds are measured over the ground.
fn foot_skate(
    source: &[[Vec3; 2]],
    target: &[[Vec3; 2]],
    times: &[f32],
    max_speed: f32,
    belts: [Vec3; 2],
) -> (Option<f32>, Option<f32>) {
    if source.len() < 2 || source.len() != target.len() {
        return (None, None);
    }
    let lowest = [0, 1].map(|k| source.iter().map(|f| f[k].y).fold(f32::INFINITY, f32::min));
    let planted = |frame: &[Vec3; 2]| -> Option<usize> {
        let k = usize::from(frame[1].y < frame[0].y);
        (frame[k].y <= lowest[k] + CONTACT_HEIGHT).then_some(k)
    };
    let (mut source_sum, mut target_sum, mut time) = (0.0f32, 0.0f32, 0.0f32);
    for i in 1..source.len() {
        let (Some(a), Some(b)) = (planted(&source[i - 1]), planted(&source[i])) else {
            continue;
        };
        let dt = times[i] - times[i - 1];
        if a != b || dt <= 0.0 {
            continue;
        }
        let source_step = (horizontal(source[i][a] - source[i - 1][a]) + belts[0] * dt).length();
        if source_step > max_speed * dt {
            continue;
        }
        source_sum += source_step;
        target_sum += (horizontal(target[i][a] - target[i - 1][a]) + belts[1] * dt).length();
        time += dt;
    }
    if time <= 0.0 {
        return (None, None);
    }
    (Some(source_sum / time), Some(target_sum / time))
}
