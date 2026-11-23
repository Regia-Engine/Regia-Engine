// SPDX-License-Identifier: MPL-2.0
//! Retargeting: play animation authored for one skeleton on another.
//!
//! Specification: `docs/specs/17-animation.md` § 4 (retargeting, humanoid mapping). Two rigs
//! are paired joint-to-joint through **roles**: a [`RetargetProfile`] names, per skeleton,
//! which joint plays which role (`"hips"`, `"leftUpperArm"`, or any custom role such as a
//! walker's `"leg_fl_upper"`). [`RetargetProfile::guess_humanoid`] fills the
//! [`HumanoidBone`] roles from common naming conventions; [`RetargetProfile::by_name`]
//! pairs joints with identical names (rig variants exported from one source).
//!
//! A [`Retargeter`] then transfers poses and bakes clips:
//!
//! - **Rotation**: each mapped target joint takes the source joint's object-space rotation
//!   *delta from its bind pose*, applied after a **rest-pose correction** that turns the
//!   target's bind bone direction onto the source's (an A-pose rig drives a T-pose rig and the
//!   arms still point where the source's arms point).
//! - **Translation**: bone lengths come from the target's bind pose. The skeleton root and
//!   the `hips` role carry the source's translation offset scaled by the hip-height ratio,
//!   so a short rig takes proportionally shorter steps and root motion scales the same way.
//! - Unmapped target joints keep their bind local rotation (fingers on a mitten rig).
//! - **IK goals** ([`RetargetOptions::ik_goals`], [`crate::retarget_goals`]): at bake, chains
//!   named by roles put their end where the source put it (scaled), so planted feet and hands on
//!   a prop stay put on a rig of other proportions.
//!
//! Both skeletons must share Regia Engine's object-space convention (+Y up, −Z forward), which the
//! glTF importer establishes.
//!
//! Provenance (`docs/provenance.md`): bind-relative rotation transfer with per-bone alignment
//! of rest directions and hip-height scaling of the root is the standard retargeting setup
//! described by Monzani, Baerlocher, Boulic & Thalmann, "Using an Intermediate Skeleton and
//! Inverse Kinematics for Motion Retargeting", Computer Graphics Forum 19(3), 2000, and
//! Gleicher, "Retargetting Motion to New Characters", SIGGRAPH 1998. Humanoid role names
//! follow the VRM 1.0 humanoid specification (vrm-c/vrm-specification, public).

use std::collections::BTreeMap;

use regia_core::math::{Mat4, Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::clip::{AnimationClip, JointTrack, RootMotionTrack, TrackCurve};
use crate::num::{det_rotation_y, det_sin_cos};
use crate::pose::Pose;
use crate::retarget_goals::{GoalRigs, GoalScratch, ResolvedGoal, resolve_goals};
use crate::skeleton::Skeleton;

pub use crate::retarget_goals::{IkGoalMode, IkGoalSpec};

/// The humanoid roles (VRM 1.0 humanoid bone names).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum HumanoidBone {
    /// Pelvis; carries the body's translation.
    Hips,
    /// First spine joint above the hips.
    Spine,
    /// Second spine joint.
    Chest,
    /// Third spine joint (optional).
    UpperChest,
    /// Neck.
    Neck,
    /// Head.
    Head,
    /// Left clavicle.
    LeftShoulder,
    /// Left upper arm.
    LeftUpperArm,
    /// Left forearm.
    LeftLowerArm,
    /// Left hand.
    LeftHand,
    /// Right clavicle.
    RightShoulder,
    /// Right upper arm.
    RightUpperArm,
    /// Right forearm.
    RightLowerArm,
    /// Right hand.
    RightHand,
    /// Left thigh.
    LeftUpperLeg,
    /// Left shin.
    LeftLowerLeg,
    /// Left foot.
    LeftFoot,
    /// Left toes.
    LeftToes,
    /// Right thigh.
    RightUpperLeg,
    /// Right shin.
    RightLowerLeg,
    /// Right foot.
    RightFoot,
    /// Right toes.
    RightToes,
}

impl HumanoidBone {
    /// Every role, hips first.
    pub const ALL: [HumanoidBone; 22] = [
        HumanoidBone::Hips,
        HumanoidBone::Spine,
        HumanoidBone::Chest,
        HumanoidBone::UpperChest,
        HumanoidBone::Neck,
        HumanoidBone::Head,
        HumanoidBone::LeftShoulder,
        HumanoidBone::LeftUpperArm,
        HumanoidBone::LeftLowerArm,
        HumanoidBone::LeftHand,
        HumanoidBone::RightShoulder,
        HumanoidBone::RightUpperArm,
        HumanoidBone::RightLowerArm,
        HumanoidBone::RightHand,
        HumanoidBone::LeftUpperLeg,
        HumanoidBone::LeftLowerLeg,
        HumanoidBone::LeftFoot,
        HumanoidBone::LeftToes,
        HumanoidBone::RightUpperLeg,
        HumanoidBone::RightLowerLeg,
        HumanoidBone::RightFoot,
        HumanoidBone::RightToes,
    ];

    /// The role name used in a [`RetargetProfile`] (VRM spelling, e.g. `"leftUpperArm"`).
    pub fn name(self) -> &'static str {
        match self {
            HumanoidBone::Hips => "hips",
            HumanoidBone::Spine => "spine",
            HumanoidBone::Chest => "chest",
            HumanoidBone::UpperChest => "upperChest",
            HumanoidBone::Neck => "neck",
            HumanoidBone::Head => "head",
            HumanoidBone::LeftShoulder => "leftShoulder",
            HumanoidBone::LeftUpperArm => "leftUpperArm",
            HumanoidBone::LeftLowerArm => "leftLowerArm",
            HumanoidBone::LeftHand => "leftHand",
            HumanoidBone::RightShoulder => "rightShoulder",
            HumanoidBone::RightUpperArm => "rightUpperArm",
            HumanoidBone::RightLowerArm => "rightLowerArm",
            HumanoidBone::RightHand => "rightHand",
            HumanoidBone::LeftUpperLeg => "leftUpperLeg",
            HumanoidBone::LeftLowerLeg => "leftLowerLeg",
            HumanoidBone::LeftFoot => "leftFoot",
            HumanoidBone::LeftToes => "leftToes",
            HumanoidBone::RightUpperLeg => "rightUpperLeg",
            HumanoidBone::RightLowerLeg => "rightLowerLeg",
            HumanoidBone::RightFoot => "rightFoot",
            HumanoidBone::RightToes => "rightToes",
        }
    }

    /// Roles a humanoid profile needs for full-body retargeting.
    pub fn required(self) -> bool {
        !matches!(
            self,
            HumanoidBone::UpperChest
                | HumanoidBone::Neck
                | HumanoidBone::LeftShoulder
                | HumanoidBone::RightShoulder
                | HumanoidBone::LeftToes
                | HumanoidBone::RightToes
        )
    }
}

/// Which joint of one skeleton plays which role.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RetargetProfile {
    /// Role name → joint name.
    pub roles: BTreeMap<String, String>,
}

/// Errors building a [`Retargeter`].
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum RetargetError {
    /// A profile names a joint the skeleton does not have.
    #[error("role {role:?} names joint {joint:?}, which is not in the skeleton")]
    MissingJoint {
        /// The role.
        role: String,
        /// The joint name.
        joint: String,
    },
    /// The two profiles share no role.
    #[error("the source and target profiles share no role")]
    NoCommonRoles,
    /// A skeleton's parent list does not match its joints: a different length, or a parent
    /// index past the last joint (audit wave 5; the fields are public, so a caller can build one).
    #[error("{side} skeleton: {detail}")]
    BadSkeleton {
        /// `source` or `target`.
        side: &'static str,
        /// What is wrong.
        detail: String,
    },
}

/// Refuses a skeleton whose parent list the pairing would index past: [`Retargeter::with_options`]
/// walks `parents` against `joint_names.len()` entries.
fn check_parents(skeleton: &Skeleton, side: &'static str) -> Result<(), RetargetError> {
    let n = skeleton.len();
    if skeleton.parents.len() != n {
        return Err(RetargetError::BadSkeleton {
            side,
            detail: format!("{} parents for {n} joints", skeleton.parents.len()),
        });
    }
    if let Some((joint, parent)) = skeleton
        .parents
        .iter()
        .enumerate()
        .find_map(|(j, p)| p.filter(|p| *p as usize >= n).map(|p| (j, p)))
    {
        return Err(RetargetError::BadSkeleton {
            side,
            detail: format!("joint {joint} names parent {parent}, past the last joint"),
        });
    }
    Ok(())
}

/// Side of a joint name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Side {
    Centre,
    Left,
    Right,
}

/// Split a joint name into lower-case tokens: at every non-alphanumeric character, at a
/// lower-to-upper case change (`lShldrBend` → `l shldr bend`), before the last capital of a
/// capital run followed by a lower-case letter (`LShoulder` → `l shoulder`) and at letter/digit
/// changes (`Bip01` → `bip 01`).
fn name_tokens(name: &str) -> Vec<String> {
    let chars: Vec<char> = name.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    for (i, &c) in chars.iter().enumerate() {
        if !c.is_ascii_alphanumeric() {
            if !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
            continue;
        }
        if let Some(&p) = i.checked_sub(1).and_then(|k| chars.get(k))
            && p.is_ascii_alphanumeric()
        {
            let next_lower = chars.get(i + 1).is_some_and(char::is_ascii_lowercase);
            let split = (p.is_ascii_lowercase() && c.is_ascii_uppercase())
                || (p.is_ascii_uppercase() && c.is_ascii_uppercase() && next_lower)
                || (p.is_ascii_digit() != c.is_ascii_digit());
            if split && !current.is_empty() {
                tokens.push(std::mem::take(&mut current));
            }
        }
        current.push(c.to_ascii_lowercase());
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Tokens that mark a joint that never plays a humanoid role (twist, IK, end and helper joints).
const EXCLUDED_TOKENS: &[&str] = &[
    "twist",
    "roll",
    "ik",
    "pole",
    "end",
    "nub",
    "tip",
    "target",
    "helper",
    "socket",
    "attach",
    "prop",
    "weapon",
    "corrective",
];

/// Tokens dropped before matching (namespaces, exporter prefixes, deform markers).
const NOISE_TOKENS: &[&str] = &[
    "mixamorig",
    "def",
    "org",
    "mch",
    "bip",
    "bn",
    "jnt",
    "joint",
    "skeleton",
    "skel",
    "bone",
    "base",
    "cc",
    "bend",
    "drv",
    "deform",
];

/// Classify a joint name: its side and core (the remaining tokens joined), or `None` for an
/// excluded joint (twist, IK, end, helper).
fn classify_tokens(name: &str) -> Option<(Side, String)> {
    let tokens = name_tokens(name);
    if tokens.iter().any(|t| EXCLUDED_TOKENS.contains(&t.as_str())) {
        return None;
    }
    let mut side = Side::Centre;
    let mut core = String::new();
    for token in &tokens {
        let as_side = match token.as_str() {
            "l" | "left" | "lf" | "lft" => Some(Side::Left),
            "r" | "right" | "rt" | "rgt" => Some(Side::Right),
            _ => None,
        };
        if let Some(s) = as_side {
            if side == Side::Centre {
                side = s;
            }
            continue;
        }
        if token.chars().all(|c| c.is_ascii_digit()) || NOISE_TOKENS.contains(&token.as_str()) {
            continue;
        }
        core.push_str(token);
    }
    // All-lower-case names glue the side on (`leftupleg`).
    if side == Side::Centre {
        for (prefix, s) in [("left", Side::Left), ("right", Side::Right)] {
            if let Some(rest) = core.strip_prefix(prefix)
                && !rest.is_empty()
            {
                side = s;
                core = rest.to_string();
                break;
            }
        }
    }
    Some((side, core))
}

/// Split a joint name into (side, core) (see `classify_tokens`); excluded joints get an empty core.
#[cfg(test)]
fn classify_name(name: &str) -> (Side, String) {
    classify_tokens(name).unwrap_or((Side::Centre, String::new()))
}

/// Specific role keywords per limb (core name after `classify_tokens`). The bare cores `arm`
/// and `leg` are generic and fill a limb in chain order (`ARM_CHAIN`, `LEG_CHAIN`).
const LIMB_KEYWORDS: &[(&[&str], [HumanoidBone; 2])] = &[
    (
        &["shoulder", "clavicle", "collar", "collarbone", "clav"],
        [HumanoidBone::LeftShoulder, HumanoidBone::RightShoulder],
    ),
    (
        &["upperarm", "humerus", "uparm", "shldr"],
        [HumanoidBone::LeftUpperArm, HumanoidBone::RightUpperArm],
    ),
    (
        &["forearm", "lowerarm", "elbow", "loarm"],
        [HumanoidBone::LeftLowerArm, HumanoidBone::RightLowerArm],
    ),
    (
        &["hand", "wrist"],
        [HumanoidBone::LeftHand, HumanoidBone::RightHand],
    ),
    (
        &["upleg", "upperleg", "thigh", "femur"],
        [HumanoidBone::LeftUpperLeg, HumanoidBone::RightUpperLeg],
    ),
    (
        &["lowerleg", "calf", "shin", "knee", "loleg"],
        [HumanoidBone::LeftLowerLeg, HumanoidBone::RightLowerLeg],
    ),
    (
        &["foot", "ankle"],
        [HumanoidBone::LeftFoot, HumanoidBone::RightFoot],
    ),
    (
        &["toebase", "toe", "toes", "ball"],
        [HumanoidBone::LeftToes, HumanoidBone::RightToes],
    ),
];

/// Arm roles in chain order, per side (shoulder first; it is filled only from a chain of four).
const ARM_CHAIN: [[HumanoidBone; 4]; 2] = [
    [
        HumanoidBone::LeftShoulder,
        HumanoidBone::LeftUpperArm,
        HumanoidBone::LeftLowerArm,
        HumanoidBone::LeftHand,
    ],
    [
        HumanoidBone::RightShoulder,
        HumanoidBone::RightUpperArm,
        HumanoidBone::RightLowerArm,
        HumanoidBone::RightHand,
    ],
];

/// Leg roles in chain order, per side.
const LEG_CHAIN: [[HumanoidBone; 4]; 2] = [
    [
        HumanoidBone::LeftUpperLeg,
        HumanoidBone::LeftLowerLeg,
        HumanoidBone::LeftFoot,
        HumanoidBone::LeftToes,
    ],
    [
        HumanoidBone::RightUpperLeg,
        HumanoidBone::RightLowerLeg,
        HumanoidBone::RightFoot,
        HumanoidBone::RightToes,
    ],
];

/// Fill the missing roles of one limb from its generic joints (depth order). The arm's
/// shoulder joins only when there are at least four generic joints.
fn fill_chain(
    chain: &[HumanoidBone; 4],
    is_arm: bool,
    generic: &[u32],
    best: &mut BTreeMap<HumanoidBone, u32>,
) {
    let used: Vec<u32> = best.values().copied().collect();
    let joints: Vec<u32> = generic.iter().copied().filter(|j| !used.contains(j)).collect();
    if joints.is_empty() {
        return;
    }
    let roles: &[HumanoidBone] = if is_arm && joints.len() < 4 {
        &chain[1..]
    } else {
        &chain[..]
    };
    let missing: Vec<HumanoidBone> = roles.iter().copied().filter(|r| !best.contains_key(r)).collect();
    for (role, joint) in missing.into_iter().zip(joints) {
        best.insert(role, joint);
    }
}

/// The deepest joint that is an ancestor of (or equal to) both `a` and `b`.
fn common_ancestor(skeleton: &Skeleton, a: u32, b: u32) -> Option<u32> {
    let mut chain = vec![a];
    let mut current = skeleton.parents.get(a as usize).copied().flatten();
    // At most one step per joint: a hand-built skeleton may hold a parent cycle.
    let steps = skeleton.parents.len();
    for _ in 0..steps {
        let Some(p) = current else { break };
        chain.push(p);
        current = skeleton.parents.get(p as usize).copied().flatten();
    }
    let mut current = Some(b);
    for _ in 0..=steps {
        let j = current?;
        if chain.contains(&j) {
            return Some(j);
        }
        current = skeleton.parents.get(j as usize).copied().flatten();
    }
    None
}

fn depths(skeleton: &Skeleton) -> Vec<u32> {
    let mut depth = vec![0u32; skeleton.len()];
    // A parent list longer than the joints is not walked past them (audit wave 5).
    for (i, parent) in skeleton.parents.iter().enumerate().take(depth.len()) {
        if let Some(p) = parent {
            depth[i] = depth.get(*p as usize).copied().unwrap_or(0) + 1;
        }
    }
    depth
}

pub(crate) fn is_ancestor(skeleton: &Skeleton, ancestor: u32, joint: u32) -> bool {
    let mut current = skeleton.parents.get(joint as usize).copied().flatten();
    // At most one step per joint: a hand-built skeleton may hold a parent cycle.
    for _ in 0..skeleton.parents.len() {
        let Some(p) = current else {
            return false;
        };
        if p == ancestor {
            return true;
        }
        current = skeleton.parents.get(p as usize).copied().flatten();
    }
    false
}

impl RetargetProfile {
    /// Assign a role to a joint.
    pub fn with(mut self, role: impl Into<String>, joint: impl Into<String>) -> Self {
        self.roles.insert(role.into(), joint.into());
        self
    }

    /// A profile in which every joint plays the role of its own name; two `by_name` profiles
    /// pair identically named joints.
    pub fn by_name(skeleton: &Skeleton) -> Self {
        RetargetProfile {
            roles: skeleton
                .joint_names
                .iter()
                .map(|n| (n.clone(), n.clone()))
                .collect(),
        }
    }

    /// Guess the [`HumanoidBone`] roles from joint names (`docs/specs/85-retarget-crowds.md` §
    /// 2.1): Mixamo `mixamorig:LeftUpLeg`, `thigh_l`, `L_Thigh`, `upper_arm.L`, `Bip01 L Thigh`,
    /// `DEF-thigh.L`, `lShldrBend`, VRM names, and generic chains such as `leg_joint_L_1..5`.
    ///
    /// Names are split into tokens; twist, IK, end and helper joints never take a role. When
    /// several joints match a role the shallowest wins. The bare cores `arm` and `leg` fill a
    /// limb's missing roles in chain order. Spine roles are the centre spine/chest/torso joints
    /// between the hips and the neck, bottom up. Without a hips name the hips are the common
    /// ancestor of the upper legs; without a head name the head is the deepest joint of a neck
    /// chain of two or more.
    pub fn guess_humanoid(skeleton: &Skeleton) -> Self {
        let depth = depths(skeleton);
        let mut best: BTreeMap<HumanoidBone, u32> = BTreeMap::new();
        let consider = |role: HumanoidBone, joint: u32, best: &mut BTreeMap<HumanoidBone, u32>| {
            let replace = best
                .get(&role)
                .is_none_or(|&current| depth[joint as usize] < depth[current as usize]);
            if replace {
                best.insert(role, joint);
            }
        };
        let mut spine: Vec<u32> = Vec::new();
        let mut necks: Vec<u32> = Vec::new();
        // Generic limb joints per side: [left, right] × [arm, leg].
        let mut generic: [[Vec<u32>; 2]; 2] = Default::default();
        for (i, name) in skeleton.joint_names.iter().enumerate() {
            let joint = i as u32;
            let Some((side, core)) = classify_tokens(name) else {
                continue;
            };
            match side {
                Side::Centre => {
                    let tokens = name_tokens(name);
                    let has = |words: &[&str]| tokens.iter().any(|t| words.contains(&t.as_str()));
                    match core.as_str() {
                        "hips" | "pelvis" | "hip" => consider(HumanoidBone::Hips, joint, &mut best),
                        "head" => consider(HumanoidBone::Head, joint, &mut best),
                        _ if has(&["neck"]) => necks.push(joint),
                        _ if has(&["spine", "chest", "upperchest", "torso", "abdomen"]) => spine.push(joint),
                        _ => {}
                    }
                }
                Side::Left | Side::Right => {
                    let k = usize::from(side == Side::Right);
                    match core.as_str() {
                        "arm" => generic[k][0].push(joint),
                        "leg" => generic[k][1].push(joint),
                        _ => {
                            for (words, roles) in LIMB_KEYWORDS {
                                if words.contains(&core.as_str()) {
                                    consider(roles[k], joint, &mut best);
                                }
                            }
                        }
                    }
                }
            }
        }
        // Generic chains fill the limbs' missing roles in depth order.
        for (k, limbs) in generic.iter_mut().enumerate() {
            for (l, joints) in limbs.iter_mut().enumerate() {
                joints.sort_by_key(|&j| (depth[j as usize], j));
                let chain = if l == 0 { &ARM_CHAIN[k] } else { &LEG_CHAIN[k] };
                fill_chain(chain, l == 0, joints, &mut best);
            }
        }
        // Hips from the legs when no joint is named for them.
        if !best.contains_key(&HumanoidBone::Hips)
            && let (Some(&l), Some(&r)) = (
                best.get(&HumanoidBone::LeftUpperLeg),
                best.get(&HumanoidBone::RightUpperLeg),
            )
            && let Some(hips) = common_ancestor(skeleton, l, r)
            && hips != l
            && hips != r
        {
            best.insert(HumanoidBone::Hips, hips);
            spine.retain(|&j| j != hips);
        }
        // Neck: the shallowest neck joint; with no head, the deepest of a chain of two or more.
        necks.sort_by_key(|&j| (depth[j as usize], j));
        if let Some(&neck) = necks.first() {
            best.insert(HumanoidBone::Neck, neck);
            if !best.contains_key(&HumanoidBone::Head)
                && let Some(&head) = necks.last()
                && head != neck
                && is_ancestor(skeleton, neck, head)
            {
                best.insert(HumanoidBone::Head, head);
            }
        }
        // Spine: joints under the hips (and above the neck/head when known), bottom up.
        let hips = best.get(&HumanoidBone::Hips).copied();
        let top = best
            .get(&HumanoidBone::Neck)
            .or_else(|| best.get(&HumanoidBone::Head))
            .copied();
        spine.retain(|&j| {
            hips.is_none_or(|h| is_ancestor(skeleton, h, j))
                && top.is_none_or(|t| is_ancestor(skeleton, j, t))
        });
        spine.sort_by_key(|&j| (depth[j as usize], j));
        let spine_roles: &[HumanoidBone] = match spine.len() {
            0 => &[],
            1 => &[HumanoidBone::Spine],
            2 => &[HumanoidBone::Spine, HumanoidBone::Chest],
            _ => &[HumanoidBone::Spine, HumanoidBone::Chest, HumanoidBone::UpperChest],
        };
        // With more spine joints than roles, the top one is the upper chest (it carries the
        // shoulders); the ones in between stay unmapped and keep their bind rotation.
        for (k, role) in spine_roles.iter().enumerate() {
            let joint = if k + 1 == spine_roles.len() {
                spine[spine.len() - 1]
            } else {
                spine[k]
            };
            best.insert(*role, joint);
        }
        RetargetProfile {
            roles: best
                .into_iter()
                .map(|(role, joint)| {
                    (
                        role.name().to_string(),
                        skeleton.joint_names[joint as usize].clone(),
                    )
                })
                .collect(),
        }
    }

    /// Required humanoid roles this profile lacks.
    pub fn missing_humanoid_roles(&self) -> Vec<HumanoidBone> {
        HumanoidBone::ALL
            .into_iter()
            .filter(|r| r.required() && !self.roles.contains_key(r.name()))
            .collect()
    }

    fn resolve(&self, skeleton: &Skeleton) -> Result<BTreeMap<String, u32>, RetargetError> {
        self.roles
            .iter()
            .map(|(role, joint)| {
                skeleton
                    .joint_index(joint)
                    .map(|j| (role.clone(), j))
                    .ok_or_else(|| RetargetError::MissingJoint {
                        role: role.clone(),
                        joint: joint.clone(),
                    })
            })
            .collect()
    }
}

/// How a mapped target joint's translation follows the source (`.retarget.ron` `roles`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TranslationMode {
    /// Keep the target's bind translation (bone lengths stay the target's).
    Bind,
    /// Copy the source joint's local translation unchanged (props, IK helpers in the same units).
    Animated,
    /// Move by the source joint's object-space offset from its bind position times the
    /// translation scale (the default for the skeleton root and the hips).
    Scaled,
}

/// Per-role retarget settings (`.retarget.ron` `roles`).
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoleSettings {
    /// Leave the role unmapped: the target joint keeps its bind pose.
    #[serde(default)]
    pub skip: bool,
    /// Translation mode; `None` keeps the default (`Scaled` for the root and hips, else `Bind`).
    #[serde(default)]
    pub translation: Option<TranslationMode>,
    /// Extra rotation of the target bone's rest orientation before the source's motion is
    /// applied: XYZ Euler angles in degrees, object space. The manual rest-pose fix.
    #[serde(default)]
    pub rest_offset_deg: Vec3,
}

/// The translation scale between the rigs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum ScaleRule {
    /// Target hip height over source hip height (default).
    #[default]
    HipHeight,
    /// A fixed factor.
    Fixed(f32),
}

/// How baked clips scale root motion.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RootMotionScale {
    /// The translation scale (default).
    #[default]
    Auto,
    /// A fixed factor on the root-motion joint's translation offset.
    Fixed(f32),
    /// Drop root motion from baked clips.
    Off,
}

/// Build options of a [`Retargeter`] (the runtime side of a `.retarget.ron` setup).
#[derive(Clone, Debug, PartialEq)]
pub struct RetargetOptions {
    /// Per-role settings, keyed by role name.
    pub roles: BTreeMap<String, RoleSettings>,
    /// Translation scale rule.
    pub translation_scale: ScaleRule,
    /// Root-motion scale rule.
    pub root_motion: RootMotionScale,
    /// Turn the source by the facing yaw between the rigs (snapped to 90°) so a rig exported
    /// facing +Z and one facing −Z move the same way.
    pub align_facing: bool,
    /// IK goals solved at bake ([`crate::retarget_goals`]); empty (the default) bakes the plain
    /// pose transfer.
    pub ik_goals: Vec<IkGoalSpec>,
}

impl Default for RetargetOptions {
    fn default() -> Self {
        RetargetOptions {
            roles: BTreeMap::new(),
            translation_scale: ScaleRule::HipHeight,
            root_motion: RootMotionScale::Auto,
            align_facing: true,
            ik_goals: Vec::new(),
        }
    }
}

/// The most frames [`Retargeter::retarget_clip`] bakes from one clip (about 9.7 hours at
/// 30 Hz); longer clips are baked at a lower rate.
pub const MAX_BAKE_FRAMES: usize = 1 << 20;

/// Transfers poses and clips from a source skeleton to a target skeleton (module docs).
#[derive(Clone, Debug)]
pub struct Retargeter {
    pub(crate) source: Skeleton,
    pub(crate) target: Skeleton,
    /// Per target joint: the source joint driving it.
    pub(crate) source_of: Vec<Option<u32>>,
    /// Per target joint: how its translation follows the source.
    translation: Vec<TranslationMode>,
    /// Per target joint: object-space rest correction (target bind bone dir → source's).
    correction: Vec<Quat>,
    /// Per target joint: the manual rest offset (`RoleSettings::rest_offset_deg`).
    offset: Vec<Quat>,
    pub(crate) source_bind: Vec<Mat4>,
    pub(crate) target_bind: Vec<Mat4>,
    /// Target hip height / source hip height.
    scale: f32,
    /// Root-motion scale of baked clips; `None` drops root motion.
    root_motion_scale: Option<f32>,
    /// Yaw turning the source's object space onto the target's (identity when they agree).
    pub(crate) facing: Quat,
    /// The snapped yaw that constructed `facing`, kept so reporting needs no inverse trig.
    facing_yaw: f32,
    /// Per target joint: the role it plays, when mapped through a role.
    role_of: Vec<Option<String>>,
    /// Roles only the source or only the target plays.
    unmatched: (Vec<String>, Vec<String>),
    /// Source joint name → target joint name for the shared roles (plus the implicit root
    /// pairing). This is the name view of `source_of`, used by [`crate::ik_retarget`] to move
    /// IK references (chains, sockets, targets, masks) between the two skeletons.
    name_map: BTreeMap<String, String>,
    /// The IK goals, resolved against both rigs (`RetargetOptions::ik_goals`).
    pub(crate) goals: Vec<ResolvedGoal>,
}

pub(crate) fn rotation_of(m: Mat4) -> Quat {
    m.to_scale_rotation_translation().1.normalize()
}

pub(crate) fn position_of(m: Mat4) -> Vec3 {
    m.w_axis.truncate()
}

/// Hip height of a bind pose: the hips' height above the origin for rigs standing on it, or
/// above the lowest joint for rigs whose origin sits at the pelvis or below the floor (their
/// hips are at or under `y = 0`, and a ratio of those heights would shrink or mirror the
/// motion). `None` when neither is a usable positive height.
pub(crate) fn hip_height(binds: &[Mat4], hips: usize) -> Option<f32> {
    let y = position_of(*binds.get(hips)?).y;
    if y.is_finite() && y > 1e-4 {
        return Some(y);
    }
    let floor = binds
        .iter()
        .map(|m| position_of(*m).y)
        .filter(|v| v.is_finite())
        .fold(f32::INFINITY, f32::min);
    let height = y - floor;
    (height.is_finite() && height > 1e-4).then_some(height)
}

/// A rig's forward direction on the ground plane from its bind pose: `left × up`, where `left`
/// runs from the right upper leg (or upper arm, or shoulder) to the left one. `None` when the
/// rig does not have both sides of any of these pairs.
fn facing_of(binds: &[Mat4], roles: &BTreeMap<String, u32>) -> Option<Vec3> {
    let pairs = [
        (HumanoidBone::LeftUpperLeg, HumanoidBone::RightUpperLeg),
        (HumanoidBone::LeftUpperArm, HumanoidBone::RightUpperArm),
        (HumanoidBone::LeftShoulder, HumanoidBone::RightShoulder),
    ];
    pairs.into_iter().find_map(|(l, r)| {
        let left = position_of(*binds.get(*roles.get(l.name())? as usize)?);
        let right = position_of(*binds.get(*roles.get(r.name())? as usize)?);
        let forward = (left - right).cross(Vec3::Y);
        let forward = Vec3::new(forward.x, 0.0, forward.z);
        (forward.is_finite() && forward.length_squared() > 1e-8).then(|| forward.normalize())
    })
}

/// The yaw from `from` to `to` about +Y, snapped to the nearest multiple of 90°.
fn snapped_yaw(from: Vec3, to: Vec3) -> f32 {
    use std::f32::consts::{FRAC_PI_2, PI};

    let y = from.cross(to).y;
    let x = from.dot(to);
    if x.is_nan() || y.is_nan() {
        return f32::NAN;
    }
    if y == 0.0 {
        return if x.is_sign_negative() { PI.copysign(y) } else { y };
    }
    // Rounding to the nearest quarter turns at the 45- and 135-degree bisectors. Exact
    // bisectors round away from zero, as `f32::round` did, but no approximate angle is involved.
    let ay = y.abs();
    let quarters = if x > ay {
        0.0
    } else if x < 0.0 && ay <= -x {
        2.0
    } else {
        1.0
    };
    let sign = if y.is_sign_negative() { -1.0 } else { 1.0 };
    sign * quarters * FRAC_PI_2
}

pub(crate) fn euler_degrees(v: Vec3) -> Quat {
    if v == Vec3::ZERO || !v.is_finite() {
        return Quat::IDENTITY;
    }
    // Keep glam's XYZ composition and operation order, replacing only its half-angle trig.
    let (si, ci) = det_sin_cos(v.x.to_radians() * 0.5);
    let (sj, cj) = det_sin_cos(-v.y.to_radians() * 0.5);
    let (sh, ch) = det_sin_cos(v.z.to_radians() * 0.5);
    let (cc, cs, sc, ss) = (ci * ch, ci * sh, si * ch, si * sh);
    Quat::from_xyzw(
        cj * sc - sj * cs,
        -(cj * ss + sj * cc),
        cj * cs - sj * sc,
        cj * cc + sj * ss,
    )
}

impl Retargeter {
    /// Pair the skeletons through their profiles (roles present in both), with the default
    /// [`RetargetOptions`].
    ///
    /// # Errors
    ///
    /// [`RetargetError::MissingJoint`] when a profile names an absent joint;
    /// [`RetargetError::NoCommonRoles`] when nothing pairs.
    pub fn new(
        source: &Skeleton,
        source_profile: &RetargetProfile,
        target: &Skeleton,
        target_profile: &RetargetProfile,
    ) -> Result<Self, RetargetError> {
        Self::with_options(
            source,
            source_profile,
            target,
            target_profile,
            &RetargetOptions::default(),
        )
    }

    /// Pair the skeletons through their profiles with explicit per-role settings, scale rules
    /// and facing alignment (`docs/specs/85-retarget-crowds.md` § 2).
    ///
    /// # Errors
    ///
    /// [`RetargetError::MissingJoint`] when a profile names an absent joint;
    /// [`RetargetError::NoCommonRoles`] when nothing pairs.
    pub fn with_options(
        source: &Skeleton,
        source_profile: &RetargetProfile,
        target: &Skeleton,
        target_profile: &RetargetProfile,
        options: &RetargetOptions,
    ) -> Result<Self, RetargetError> {
        check_parents(source, "source")?;
        check_parents(target, "target")?;
        let source_roles = source_profile.resolve(source)?;
        let target_roles = target_profile.resolve(target)?;
        let skipped = |role: &str| options.roles.get(role).is_some_and(|r| r.skip);
        let mut source_of = vec![None; target.len()];
        let mut role_of = vec![None; target.len()];
        for (role, &t) in &target_roles {
            if let Some(&s) = source_roles.get(role)
                && !skipped(role)
            {
                source_of[t as usize] = Some(s);
                role_of[t as usize] = Some(role.clone());
            }
        }
        if source_of.iter().all(Option::is_none) {
            return Err(RetargetError::NoCommonRoles);
        }
        let unmatched = (
            source_roles
                .keys()
                .filter(|r| !target_roles.contains_key(*r))
                .cloned()
                .collect(),
            target_roles
                .keys()
                .filter(|r| !source_roles.contains_key(*r))
                .cloned()
                .collect(),
        );
        // The skeleton roots pair implicitly so clip root motion has somewhere to go.
        let source_root = source.parents.iter().position(Option::is_none).map(|r| r as u32);
        let target_root = target.parents.iter().position(Option::is_none);
        if let (Some(sr), Some(tr)) = (source_root, target_root)
            && source_of[tr].is_none()
            && !source_of.contains(&Some(sr))
        {
            source_of[tr] = Some(sr);
        }

        // Source joint name → target joint name for the roles both skeletons play, plus the
        // implicit root pairing. This is the inverse-name view of `source_of` that IK
        // retargeting consumes; it never invents a mapping for a role only one side has.
        let mut name_map = BTreeMap::new();
        for (role, &t) in &target_roles {
            if let Some(&s) = source_roles.get(role)
                && !skipped(role)
            {
                name_map.insert(
                    source.joint_names[s as usize].clone(),
                    target.joint_names[t as usize].clone(),
                );
            }
        }
        if let (Some(sr), Some(tr)) = (source_root, target_root)
            && source_of[tr] == Some(sr)
        {
            name_map
                .entry(source.joint_names[sr as usize].clone())
                .or_insert_with(|| target.joint_names[tr].clone());
        }

        let source_bind = source.globals(&source.bind_pose_pose(), None);
        let target_bind = target.globals(&target.bind_pose_pose(), None);

        let mut translation = vec![TranslationMode::Bind; target.len()];
        if let Some(tr) = target_root
            && source_of[tr].is_some()
        {
            translation[tr] = TranslationMode::Scaled;
        }
        let hips = HumanoidBone::Hips.name();
        let mut scale = 1.0;
        if let (Some(&sh), Some(&th)) = (source_roles.get(hips), target_roles.get(hips)) {
            if source_of[th as usize].is_some() {
                translation[th as usize] = TranslationMode::Scaled;
            }
            if let (Some(sy), Some(ty)) = (
                hip_height(&source_bind, sh as usize),
                hip_height(&target_bind, th as usize),
            ) {
                scale = ty / sy;
            }
        }
        if let ScaleRule::Fixed(k) = options.translation_scale
            && k.is_finite()
            && k > 0.0
        {
            scale = k;
        }
        let mut offset = vec![Quat::IDENTITY; target.len()];
        for (role, settings) in &options.roles {
            let Some(&t) = target_roles.get(role) else {
                continue;
            };
            if source_of[t as usize].is_none() {
                continue;
            }
            if let Some(mode) = settings.translation {
                translation[t as usize] = mode;
            }
            offset[t as usize] = euler_degrees(settings.rest_offset_deg);
        }
        let root_motion_scale = match options.root_motion {
            RootMotionScale::Auto => Some(scale),
            RootMotionScale::Fixed(k) if k.is_finite() => Some(k),
            RootMotionScale::Fixed(_) => Some(scale),
            RootMotionScale::Off => None,
        };

        // Facing: the yaw that turns the source's forward onto the target's, snapped to 90°.
        let (facing, facing_yaw) = match (
            options.align_facing,
            facing_of(&source_bind, &source_roles),
            facing_of(&target_bind, &target_roles),
        ) {
            (true, Some(fs), Some(ft)) => {
                let yaw = snapped_yaw(fs, ft);
                (det_rotation_y(yaw), yaw)
            }
            _ => (Quat::IDENTITY, 0.0),
        };

        // Rest correction per mapped joint: align the bone towards the nearest mapped
        // descendant pair; leaves inherit their nearest mapped ancestor's correction.
        let mut correction = vec![Quat::IDENTITY; target.len()];
        let mut has_own = vec![false; target.len()];
        let target_depth = depths(target);
        for t in 0..target.len() {
            let Some(s) = source_of[t] else { continue };
            let child = (0..target.len())
                .filter(|&c| {
                    source_of[c].is_some_and(|sc| is_ancestor(source, s, sc))
                        && is_ancestor(target, t as u32, c as u32)
                })
                // Equal-depth candidates (a chest's two upper arms) tie-break by joint name, not
                // storage order, so a legal reorder of the target skeleton keeps the rest
                // correction (tests/retarget_joint_reindex.rs, changed_branch_rest_*).
                .min_by(|&a, &b| {
                    target_depth[a]
                        .cmp(&target_depth[b])
                        .then_with(|| target.joint_names[a].cmp(&target.joint_names[b]))
                });
            if let Some(c) = child {
                let sc = source_of[c].unwrap_or(s) as usize;
                let dt = (position_of(target_bind[c]) - position_of(target_bind[t])).normalize_or_zero();
                let ds = facing
                    * (position_of(source_bind[sc]) - position_of(source_bind[s as usize]))
                        .normalize_or_zero();
                if dt.length_squared() > 0.5 && ds.length_squared() > 0.5 {
                    correction[t] = Quat::from_rotation_arc(dt, ds);
                    has_own[t] = true;
                }
            }
        }
        for t in 0..target.len() {
            if source_of[t].is_some() && !has_own[t] {
                let mut parent = target.parents.get(t).copied().flatten();
                // One step per joint at most: a hand-built skeleton may hold a cycle.
                for _ in 0..target.len() {
                    let Some(p) = parent.map(|p| p as usize).filter(|p| *p < target.len()) else {
                        break;
                    };
                    if has_own[p] {
                        correction[t] = correction[p];
                        break;
                    }
                    parent = target.parents.get(p).copied().flatten();
                }
            }
        }

        let goals = resolve_goals(
            &options.ik_goals,
            &GoalRigs {
                target,
                target_roles: &target_roles,
                source_of: &source_of,
                source_bind: &source_bind,
                target_bind: &target_bind,
                scale,
            },
        );

        Ok(Retargeter {
            source: source.clone(),
            target: target.clone(),
            source_of,
            translation,
            correction,
            offset,
            source_bind,
            target_bind,
            scale,
            root_motion_scale,
            facing,
            facing_yaw,
            role_of,
            unmatched,
            name_map,
            goals,
        })
    }

    /// Humanoid retargeting: both profiles guessed with [`RetargetProfile::guess_humanoid`].
    ///
    /// # Errors
    ///
    /// As [`Retargeter::new`].
    pub fn humanoid(source: &Skeleton, target: &Skeleton) -> Result<Self, RetargetError> {
        Self::new(
            source,
            &RetargetProfile::guess_humanoid(source),
            target,
            &RetargetProfile::guess_humanoid(target),
        )
    }

    /// Target hip height over source hip height (the translation scale).
    pub fn scale(&self) -> f32 {
        self.scale
    }

    /// The root-motion scale of baked clips; `None` when root motion is dropped
    /// ([`RootMotionScale::Off`]).
    pub fn root_motion_scale(&self) -> Option<f32> {
        self.root_motion_scale
    }

    /// The facing yaw (radians, about +Y, a multiple of 90°) that turns the source's object space
    /// onto the target's; 0 when both rigs face the same way or a side is missing.
    pub fn facing_yaw(&self) -> f32 {
        self.facing_yaw
    }

    /// The role target joint `target_joint` plays, when it is mapped through a role (the
    /// implicit root pairing has none).
    pub fn role_of(&self, target_joint: u32) -> Option<&str> {
        self.role_of.get(target_joint as usize).and_then(|r| r.as_deref())
    }

    /// Roles only the source profile names, and roles only the target profile names.
    pub fn unmatched_roles(&self) -> (&[String], &[String]) {
        (&self.unmatched.0, &self.unmatched.1)
    }

    /// The rest correction of target joint `target_joint` in degrees: the angle between its bind
    /// bone direction and the source's, after the facing turn (0 for unmapped joints).
    pub fn rest_correction_degrees(&self, target_joint: u32) -> f32 {
        self.correction
            .get(target_joint as usize)
            .map_or(0.0, |q| q.normalize().angle_between(Quat::IDENTITY).to_degrees())
    }

    /// The IK goals of this retargeter, in solve order (`RetargetOptions::ik_goals`).
    pub fn ik_goals(&self) -> impl Iterator<Item = &IkGoalSpec> {
        self.goals.iter().map(|g| &g.spec)
    }

    /// Number of target joints driven by a source joint.
    pub fn mapped_joints(&self) -> usize {
        self.source_of.iter().filter(|s| s.is_some()).count()
    }

    /// The source joint driving target joint `target_joint`, if any.
    pub fn source_of(&self, target_joint: u32) -> Option<u32> {
        self.source_of.get(target_joint as usize).copied().flatten()
    }

    /// The target joint that plays the role of source joint `source_joint` (the inverse of
    /// [`Retargeter::source_of`]), if any.
    pub fn target_of(&self, source_joint: u32) -> Option<u32> {
        self.source_of
            .iter()
            .position(|&s| s == Some(source_joint))
            .map(|t| t as u32)
    }

    /// The target joint name driven by the source joint named `source_name`, when that joint's
    /// role is shared. The name view of the role map (see [`Retargeter::target_of`]).
    pub fn retarget_joint_name(&self, source_name: &str) -> Option<&str> {
        self.name_map.get(source_name).map(String::as_str)
    }

    /// The target joint id for a source joint name, when that joint's role maps.
    pub fn retarget_joint(&self, source_name: &str) -> Option<u32> {
        let name = self.name_map.get(source_name)?;
        self.target.joint_index(name)
    }

    /// A direction written in the frame of source joint `source_name` (its rest orientation:
    /// a `BodyAim` axis, a hinge limit's axis), expressed in the frame of the paired target joint
    /// so it names the same anatomical direction on the target rig, whatever the two rigs' bone
    /// rolls. The pose transfer turns the target joint by exactly this rest-frame difference, so
    /// a retargeted pose carries the source's direction onto the returned one:
    /// `R_target_bind⁻¹ · correction⁻¹ · rest_offset⁻¹ · facing · R_source_bind · direction`
    /// (rotations of the joints' global bind transforms, the rest correction and the manual rest
    /// offset of the target joint, the facing turn). `None` when the joint does not map or a bind
    /// transform has no usable rotation.
    pub fn retarget_joint_direction(&self, source_name: &str, direction: Vec3) -> Option<Vec3> {
        let s = self.source.joint_index(source_name)? as usize;
        let t = self.retarget_joint(source_name)? as usize;
        let source_bind = rotation_of(*self.source_bind.get(s)?);
        let target_bind = rotation_of(*self.target_bind.get(t)?);
        let correction = *self.correction.get(t)?;
        let offset = *self.offset.get(t)?;
        let turn =
            (target_bind.inverse() * correction.inverse() * offset.inverse() * self.facing * source_bind)
                .normalize();
        let out = turn * direction;
        out.is_finite().then_some(out)
    }

    /// The source skeleton this retargeter reads poses from.
    pub fn source(&self) -> &Skeleton {
        &self.source
    }

    /// The target skeleton this retargeter writes poses onto.
    pub fn target(&self) -> &Skeleton {
        &self.target
    }

    /// Retarget a three-joint IK chain `[root, mid, tip]` (joint names) through the role map;
    /// `None` when any joint does not map.
    pub fn retarget_chain(&self, chain: &[String; 3]) -> Option<[String; 3]> {
        Some([
            self.name_map.get(&chain[0])?.clone(),
            self.name_map.get(&chain[1])?.clone(),
            self.name_map.get(&chain[2])?.clone(),
        ])
    }

    /// Retarget a joint-name list (a layer mask): names whose role does not map are dropped.
    pub fn retarget_mask(&self, joints: &[String]) -> Vec<String> {
        joints
            .iter()
            .filter_map(|n| self.name_map.get(n).cloned())
            .collect()
    }

    /// Retarget a source local pose onto `out` (resized to the target skeleton).
    pub fn retarget_pose(&self, source_pose: &Pose, out: &mut Pose) {
        self.retarget_pose_scaled(source_pose, out, None);
    }

    /// `retarget_pose` with the root-motion joint's travel scaled by its own factor (a baked clip
    /// under [`RootMotionScale::Fixed`]): `joint_scale = (target root-motion joint, k)`. The
    /// joint moves by its source offset times `k`; `Scaled` joints under it follow that travel
    /// and scale only their offset from it, so the body goes where the root goes.
    pub(crate) fn retarget_pose_scaled(
        &self,
        source_pose: &Pose,
        out: &mut Pose,
        joint_scale: Option<(usize, f32)>,
    ) {
        if out.len() != self.target.len() {
            *out = self.target.bind_pose_pose();
        }
        let source_globals = self.source.globals(source_pose, None);
        // The root-motion joint's source travel, and its factor.
        let root_travel = joint_scale.and_then(|(joint, k)| {
            let s = self.source_of.get(joint).copied().flatten()? as usize;
            let travel = position_of(*source_globals.get(s)?) - position_of(*self.source_bind.get(s)?);
            Some((joint, travel, k))
        });
        let mut target_globals = vec![Mat4::IDENTITY; self.target.len()];
        let facing_inverse = self.facing.inverse();
        // A hand-built target may have shorter arrays than names; `validate` refuses those on load.
        let joints = self.target.len().min(self.target.bind_pose.len()).min(out.len());
        for t in 0..joints {
            let bind = &self.target.bind_pose[t];
            let parent = self
                .target
                .parents
                .get(t)
                .copied()
                .flatten()
                .and_then(|p| target_globals.get(p as usize).copied())
                .unwrap_or(Mat4::IDENTITY);
            let parent_rotation = rotation_of(parent);
            let mut rotation = bind.rotation;
            let mut translation = bind.translation;
            let source = self.source_of[t].map(|s| s as usize);
            if let Some((s, (&source_global, &source_bind))) =
                source.and_then(|s| Some((s, source_globals.get(s).zip(self.source_bind.get(s))?)))
            {
                let delta = self.facing
                    * rotation_of(source_global)
                    * rotation_of(source_bind).inverse()
                    * facing_inverse;
                let want = (delta * self.offset[t] * self.correction[t] * rotation_of(self.target_bind[t]))
                    .normalize();
                rotation = (parent_rotation.inverse() * want).normalize();
                match self.translation[t] {
                    TranslationMode::Bind => {}
                    TranslationMode::Animated => {
                        if let Some(&local) = source_pose.translations.get(s) {
                            translation = local;
                        }
                    }
                    TranslationMode::Scaled => {
                        let source_offset = position_of(source_global) - position_of(source_bind);
                        let offset = match root_travel {
                            Some((joint, travel, k))
                                if joint == t || is_ancestor(&self.target, joint as u32, t as u32) =>
                            {
                                (source_offset - travel) * self.scale + travel * k
                            }
                            _ => source_offset * self.scale,
                        };
                        let offset = self.facing * offset;
                        let want_position = position_of(self.target_bind[t]) + offset;
                        translation = parent.inverse().transform_point3(want_position);
                    }
                }
            }
            // A zero quaternion key or a zero-scale parent leaves nothing to transfer: the joint
            // keeps its bind value rather than baking NaN into every frame.
            if !rotation.is_finite() {
                rotation = bind.rotation;
            }
            if !translation.is_finite() {
                translation = bind.translation;
            }
            out.translations[t] = translation;
            out.rotations[t] = rotation;
            out.scales[t] = bind.scale;
            target_globals[t] =
                parent * Mat4::from_scale_rotation_translation(bind.scale, rotation, translation);
        }
    }

    /// Bake `clip` (authored for the source) into a clip for the target, sampled at the clip's
    /// `sample_rate` (at least 2 keys). Notifies, curves and property tracks carry over; morph
    /// tracks are dropped (they index the source mesh's targets). Root motion moves to the
    /// target joint driven by the source's root-motion joint, scaled like translations. IK goals
    /// ([`RetargetOptions::ik_goals`]) are solved on every baked frame after the transfer.
    pub fn retarget_clip(&self, clip: &AnimationClip) -> AnimationClip {
        // Malformed clips (an infinite duration or rate) bake a bounded number of frames instead
        // of overflowing the frame count.
        let duration = crate::num::finite_or(clip.duration, 0.0).max(0.0);
        let rate = if clip.sample_rate.is_finite() && clip.sample_rate > 0.0 {
            clip.sample_rate
        } else {
            30.0
        };
        let frames = ((duration * rate).ceil().min(MAX_BAKE_FRAMES as f32) as usize).max(1) + 1;
        let times: Vec<f32> = (0..frames)
            .map(|k| (k as f32 / (frames - 1) as f32) * duration)
            .collect();
        let driven: Vec<usize> = (0..self.target.len())
            .filter(|&t| self.source_of[t].is_some())
            .collect();
        let mut rotations: Vec<Vec<Quat>> = vec![Vec::with_capacity(frames); driven.len()];
        let mut translations: Vec<Vec<Vec3>> = vec![Vec::with_capacity(frames); driven.len()];
        let source_bind_pose = self.source.bind_pose_pose();
        let mut source_pose = source_bind_pose.clone();
        let mut target_pose = self.target.bind_pose_pose();
        // The target joint carrying root motion, and its scale when it differs from the
        // translation scale (`RootMotionScale::Fixed`).
        let root_motion_target = clip
            .root_motion
            .as_ref()
            .and_then(|rm| self.source_of.iter().position(|s| *s == Some(rm.joint)));
        let joint_scale = match (root_motion_target, self.root_motion_scale) {
            (Some(t), Some(k)) if (k - self.scale).abs() > f32::EPSILON => Some((t, k)),
            _ => None,
        };
        // IK goals: per-frame targets from the whole clip first (contacts need every frame).
        let goal_frames = self
            .has_active_goals()
            .then(|| self.goal_frames(clip, &times, joint_scale));
        let mut goal_scratch = GoalScratch::default();
        let mut residuals = Vec::new();
        for (frame, &time) in times.iter().enumerate() {
            source_pose.copy_from(&source_bind_pose);
            clip.sample_pose(time, &mut source_pose);
            self.retarget_pose_scaled(&source_pose, &mut target_pose, joint_scale);
            if let Some(goals) = &goal_frames {
                self.apply_goals(goals, frame, &mut target_pose, &mut goal_scratch, &mut residuals);
            }
            for (k, &t) in driven.iter().enumerate() {
                let previous = rotations[k].last().copied();
                let mut q = target_pose.rotations[t];
                // Keep the hemisphere continuous so nlerp sampling never takes the long way.
                if previous.is_some_and(|p: Quat| p.dot(q) < 0.0) {
                    q = -q;
                }
                rotations[k].push(q);
                translations[k].push(target_pose.translations[t]);
            }
        }
        let mut out = AnimationClip::empty(clip.name.clone(), duration);
        out.sample_rate = rate;
        for (k, &t) in driven.iter().enumerate() {
            let translation = if self.translation[t] != TranslationMode::Bind {
                TrackCurve::Animated {
                    times: times.clone(),
                    values: std::mem::take(&mut translations[k]),
                    exact_rotation: false,
                }
            } else {
                TrackCurve::Static(self.target.bind_pose[t].translation)
            };
            out.tracks.push(JointTrack {
                joint: t as u32,
                translations: translation,
                rotations: TrackCurve::Animated {
                    times: times.clone(),
                    values: std::mem::take(&mut rotations[k]),
                    exact_rotation: false,
                },
                scales: TrackCurve::Static(self.target.bind_pose[t].scale),
            });
        }
        out.notifies = clip.notifies.clone();
        out.curves = clip.curves.clone();
        out.property_tracks = clip.property_tracks.clone();
        out.root_motion = clip.root_motion.as_ref().and_then(|rm| {
            self.root_motion_scale?;
            let Some(joint) = root_motion_target else {
                // Losing root motion makes a walk play in place: say so.
                tracing::warn!(
                    target: "regia_animation",
                    "retarget {:?}: root-motion joint {} drives no target joint; the clip loses its root motion",
                    clip.name,
                    rm.joint
                );
                return None;
            };
            Some(RootMotionTrack {
                joint: joint as u32,
                mode: rm.mode,
            })
        });
        out
    }

    /// Retarget every clip of a library (names unchanged).
    pub fn retarget_library(&self, clips: &[AnimationClip]) -> Vec<AnimationClip> {
        clips.iter().map(|c| self.retarget_clip(c)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clip::RootMotionMode;
    use crate::pose::translated;
    use crate::skeleton::LocalBind;

    /// A small humanoid. `arm_down` rotates the upper arms 45° down (an A-pose rest);
    /// `size` scales every bone.
    fn humanoid(names: &[&str; 12], size: f32, arm_down: bool) -> Skeleton {
        // root, hips, spine, chest, neck, head, l_upperarm, l_forearm, l_hand, l_thigh, l_calf, l_foot
        let parents = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(4),
            Some(3),
            Some(6),
            Some(7),
            Some(1),
            Some(9),
            Some(10),
        ];
        let arm = if arm_down {
            Vec3::new(0.3, -0.3, 0.0).normalize() * 0.3
        } else {
            Vec3::new(0.3, 0.0, 0.0)
        };
        let s = |v: Vec3| translated(v * size);
        let binds: Vec<LocalBind> = vec![
            LocalBind::default(),
            s(Vec3::new(0.0, 1.0, 0.0)),
            s(Vec3::new(0.0, 0.15, 0.0)),
            s(Vec3::new(0.0, 0.2, 0.0)),
            s(Vec3::new(0.0, 0.25, 0.0)),
            s(Vec3::new(0.0, 0.1, 0.0)),
            s(Vec3::new(0.15, 0.2, 0.0)),
            s(arm),
            s(arm),
            s(Vec3::new(0.1, -0.05, 0.0)),
            s(Vec3::new(0.0, -0.45, 0.0)),
            s(Vec3::new(0.0, -0.45, 0.0)),
        ];
        Skeleton::from_local_bind(names.iter().map(|n| n.to_string()).collect(), parents, binds)
            .expect("valid skeleton")
    }

    const MIXAMO: [&str; 12] = [
        "Armature",
        "mixamorig:Hips",
        "mixamorig:Spine",
        "mixamorig:Spine1",
        "mixamorig:Neck",
        "mixamorig:Head",
        "mixamorig:LeftArm",
        "mixamorig:LeftForeArm",
        "mixamorig:LeftHand",
        "mixamorig:LeftUpLeg",
        "mixamorig:LeftLeg",
        "mixamorig:LeftFoot",
    ];
    const GAME: [&str; 12] = [
        "root",
        "pelvis",
        "spine_01",
        "spine_02",
        "neck_01",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "thigh_l",
        "calf_l",
        "foot_l",
    ];

    /// ADR-4477 / audit animation1-determinism H6: a vector just below 135 degrees snaps to
    /// 90, even when the host's rounded `atan2` lands exactly on the 135-degree bisector.
    #[test]
    fn facing_snap_uses_the_arithmetic_bisectors() {
        let from = Vec3::NEG_Z;
        let to = Vec3::new(-f32::from_bits(0x3f35_04f3), 0.0, f32::from_bits(0x3f35_04f2)).normalize();
        assert_eq!(
            snapped_yaw(from, to).to_bits(),
            std::f32::consts::FRAC_PI_2.to_bits()
        );
        let mirrored = Vec3::new(-to.x, 0.0, to.z);
        assert_eq!(
            snapped_yaw(from, mirrored).to_bits(),
            (-std::f32::consts::FRAC_PI_2).to_bits()
        );
        for (to, expected) in [
            (Vec3::NEG_Z, 0.0),
            (
                Vec3::new(-1.0, 0.0, -1.0).normalize(),
                std::f32::consts::FRAC_PI_2,
            ),
            (Vec3::X, -std::f32::consts::FRAC_PI_2),
            (Vec3::new(1.0, 0.0, 1.0).normalize(), -std::f32::consts::PI),
        ] {
            assert_eq!(snapped_yaw(from, to).to_bits(), expected.to_bits(), "{to:?}");
        }
    }

    /// ADR-4477 / audit animation1-determinism H6: rest offsets are rounded from the portable
    /// half-angle series once, not from the host's quaternion trigonometry.
    #[test]
    fn rest_offset_quaternion_uses_the_portable_trig_oracle() {
        let q = euler_degrees(Vec3::new(26.0, 0.0, 0.0));
        assert_eq!(
            q.to_array().map(f32::to_bits),
            [0x3e66_5991, 0x8000_0000, 0x0000_0000, 0x3f79_7051]
        );
        let (sin, cos) = crate::num::det_sin_cos(26.0f32.to_radians() * 0.5);
        assert_eq!(q.x.to_bits(), sin.to_bits());
        assert_eq!(q.w.to_bits(), cos.to_bits());
        assert_eq!(euler_degrees(Vec3::ZERO), Quat::IDENTITY);
        assert_eq!(euler_degrees(Vec3::NAN), Quat::IDENTITY);
    }

    /// Audit wave 5 (terrain-g2b M): a skeleton whose parent list is longer than its joints, or
    /// names a parent past the last joint, is refused with `BadSkeleton` instead of panicking in
    /// the depth walk or the root pairing; guessing roles on it does not panic either.
    #[test]
    fn a_skeleton_whose_parents_do_not_match_its_joints_is_refused() {
        let good = humanoid(&MIXAMO, 1.0, true);
        let bad = |parents: Vec<Option<u32>>| Skeleton {
            joint_names: vec!["Hips".to_owned()],
            parents,
            ..Skeleton::default()
        };
        for parents in [vec![None, Some(0)], vec![Some(0), None], vec![Some(5)]] {
            let sk = bad(parents.clone());
            let _ = RetargetProfile::guess_humanoid(&sk);
            for (source, target) in [(&sk, &good), (&good, &sk)] {
                let result = Retargeter::humanoid(source, target);
                assert!(
                    matches!(result, Err(RetargetError::BadSkeleton { .. })),
                    "{parents:?}: {:?}",
                    result.err()
                );
            }
        }
        assert!(Retargeter::humanoid(&good, &good).is_ok());
    }

    #[test]
    fn humanoid_roles_are_guessed_from_both_conventions() {
        for names in [&MIXAMO, &GAME] {
            let sk = humanoid(names, 1.0, false);
            let profile = RetargetProfile::guess_humanoid(&sk);
            let expect = [
                (HumanoidBone::Hips, 1),
                (HumanoidBone::Spine, 2),
                (HumanoidBone::Chest, 3),
                (HumanoidBone::Neck, 4),
                (HumanoidBone::Head, 5),
                (HumanoidBone::LeftUpperArm, 6),
                (HumanoidBone::LeftLowerArm, 7),
                (HumanoidBone::LeftHand, 8),
                (HumanoidBone::LeftUpperLeg, 9),
                (HumanoidBone::LeftLowerLeg, 10),
                (HumanoidBone::LeftFoot, 11),
            ];
            for (role, joint) in expect {
                assert_eq!(
                    profile.roles.get(role.name()).map(String::as_str),
                    Some(names[joint]),
                    "{role:?} on {names:?}"
                );
            }
        }
        assert_eq!(classify_name("upper_arm.L").1, "upperarm");
        assert!(matches!(classify_name("upperarm_l_01"), (Side::Left, ref c) if c == "upperarm"));
        assert!(matches!(classify_name("L_Thigh").0, Side::Left));
        assert!(matches!(classify_name("RightToeBase").0, Side::Right));
    }

    /// Spec 85 § 2.1, word by word: every listed exclusion token keeps a joint out of every
    /// role, every listed noise token (and a digit run) is dropped before matching, and the
    /// side aliases and capital-run split hold. The lists are spelled out here, not read from
    /// the constants, so the spec and the code cannot drift apart unseen.
    #[test]
    fn every_spec_exclusion_and_noise_token_is_honoured() {
        for word in [
            "twist",
            "roll",
            "ik",
            "pole",
            "end",
            "nub",
            "tip",
            "target",
            "helper",
            "socket",
            "attach",
            "prop",
            "weapon",
            "corrective",
        ] {
            assert_eq!(classify_tokens(&format!("thigh_{word}_l")), None, "{word}");
            assert_eq!(classify_tokens(&format!("{word}_thigh_l")), None, "{word}");
        }
        for word in [
            "mixamorig",
            "def",
            "org",
            "mch",
            "bip",
            "bn",
            "jnt",
            "joint",
            "skeleton",
            "skel",
            "bone",
            "base",
            "cc",
            "bend",
            "drv",
            "deform",
            "01",
            "7",
        ] {
            let (side, core) = classify_name(&format!("{word}_thigh_l"));
            assert!(matches!(side, Side::Left), "{word}: {side:?}");
            assert_eq!(core, "thigh", "{word}");
        }
        for (name, left) in [
            ("lf_thigh", true),
            ("lft_thigh", true),
            ("rt_thigh", false),
            ("rgt_thigh", false),
            ("thigh_R", false),
            ("leftupleg", true),
        ] {
            let (side, _) = classify_name(name);
            let expected = if left { Side::Left } else { Side::Right };
            assert_eq!(side, expected, "{name}");
        }
        assert_eq!(name_tokens("LShoulder"), ["l", "shoulder"]);
        assert_eq!(name_tokens("lShldrBend"), ["l", "shldr", "bend"]);
        assert_eq!(name_tokens("Bip01 L Thigh"), ["bip", "01", "l", "thigh"]);
    }

    fn bone_dir(sk: &Skeleton, pose: &Pose, joint: usize, child: usize) -> Vec3 {
        let g = sk.globals(pose, None);
        (position_of(g[child]) - position_of(g[joint])).normalize()
    }

    #[test]
    fn a_parent_cycle_does_not_hang_the_ancestor_walk() {
        let mut sk = humanoid(&MIXAMO, 1.0, false);
        sk.parents[1] = Some(2);
        sk.parents[2] = Some(1);
        assert!(!is_ancestor(&sk, 0, 2));
        assert!(!is_ancestor(&sk, 9, 1));
        // A malformed parent is a root to the compressor's walk, not an index panic.
        sk.parents[3] = Some(99);
        assert_eq!(sk.max_descendant_distance().len(), sk.len());
    }

    #[test]
    fn a_parent_cycle_in_the_target_does_not_hang_the_build() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let mut target = humanoid(&MIXAMO, 1.0, false);
        target.parents[1] = Some(2);
        target.parents[2] = Some(1);
        let _ = Retargeter::humanoid(&source, &target);
    }

    #[test]
    fn a_short_hand_built_target_does_not_panic_the_retarget() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let mut target = humanoid(&MIXAMO, 1.2, false);
        let r = Retargeter::humanoid(&source, &target).expect("retargeter");
        target.bind_pose.truncate(4);
        let short = Retargeter { target, ..r };
        let mut out = Pose::default();
        short.retarget_pose(&source.bind_pose_pose(), &mut out);
        assert_eq!(out.len(), 4);
    }

    #[test]
    fn a_zero_quaternion_key_bakes_the_bind_value_not_nan() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let target = humanoid(&MIXAMO, 1.3, false);
        let r = Retargeter::humanoid(&source, &target).expect("retargeter");
        let mut clip = AnimationClip::empty("broken", 1.0);
        clip.tracks.push(JointTrack {
            joint: 2,
            translations: TrackCurve::Static(source.bind_pose[2].translation),
            rotations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Quat::from_xyzw(0.0, 0.0, 0.0, 0.0); 2],
                exact_rotation: false,
            },
            scales: TrackCurve::Static(Vec3::ONE),
        });
        let baked = r.retarget_clip(&clip);
        let mut pose = target.bind_pose_pose();
        baked.sample_pose(0.5, &mut pose);
        assert!(pose.rotations.iter().all(|q| q.is_finite()));
        assert!(pose.translations.iter().all(|t| t.is_finite()));
    }

    #[test]
    fn rest_pose_correction_makes_bones_point_the_same_way() {
        // Source rests in a T-pose, target in an A-pose and 1.5× bigger.
        let source = humanoid(&MIXAMO, 1.0, false);
        let target = humanoid(&GAME, 1.5, true);
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
        assert!((rt.scale() - 1.5).abs() < 1e-4);
        assert!(rt.mapped_joints() >= 11);

        // The source raises its arm 60° about −Z (forward) and bends the elbow.
        let mut pose = source.bind_pose_pose();
        pose.rotations[6] = Quat::from_rotation_z(1.05);
        pose.rotations[7] = Quat::from_rotation_y(0.8);
        pose.rotations[9] = Quat::from_rotation_x(-0.6);
        let mut out = Pose::default();
        rt.retarget_pose(&pose, &mut out);
        for (joint, child) in [(6usize, 7usize), (7, 8), (9, 10), (10, 11)] {
            let ds = bone_dir(&source, &pose, joint, child);
            let dt = bone_dir(&target, &out, joint, child);
            assert!(ds.angle_between(dt) < 1e-3, "joint {joint}: {ds} vs {dt}");
        }
        // Bind pose maps to bind pose (rest correction only aligns, adds no motion).
        let mut rest = Pose::default();
        rt.retarget_pose(&source.bind_pose_pose(), &mut rest);
        for (joint, child) in [(6usize, 7usize), (9, 10)] {
            let ds = bone_dir(&source, &source.bind_pose_pose(), joint, child);
            let dt = bone_dir(&target, &rest, joint, child);
            assert!(ds.angle_between(dt) < 1e-3);
        }
        // Bone lengths stay the target's.
        let g = target.globals(&out, None);
        let upper = (position_of(g[7]) - position_of(g[6])).length();
        assert!((upper - 0.45).abs() < 1e-4, "{upper}");
    }

    #[test]
    fn clips_bake_with_scaled_hips_translation_and_root_motion() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let target = humanoid(&GAME, 0.5, false);
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
        let mut clip = AnimationClip::empty("walk", 1.0);
        clip.tracks.push(JointTrack {
            joint: 1,
            translations: TrackCurve::Animated {
                times: vec![0.0, 1.0],
                values: vec![Vec3::new(0.0, 1.0, 0.0), Vec3::new(0.0, 1.0, -2.0)],
                exact_rotation: false,
            },
            rotations: TrackCurve::Static(Quat::IDENTITY),
            scales: TrackCurve::Static(Vec3::ONE),
        });
        clip.root_motion = Some(RootMotionTrack {
            joint: 1,
            mode: RootMotionMode::XZ,
        });
        clip.notifies.push(crate::clip::Notify {
            time: 0.5,
            name: "Footstep".into(),
            payload: None,
        });
        let baked = rt.retarget_clip(&clip);
        assert_eq!(baked.root_motion.as_ref().map(|r| r.joint), Some(1));
        assert_eq!(baked.notifies.len(), 1);
        let mut pose = target.bind_pose_pose();
        baked.sample_pose(1.0, &mut pose);
        let hips = position_of(target.globals(&pose, None)[1]);
        assert!((hips - Vec3::new(0.0, 0.5, -1.0)).length() < 1e-4, "{hips}");
        let (delta, _) = baked.root_motion_delta(0.0, 1.0, false).expect("root motion");
        assert!(
            (delta.z + 1.0).abs() < 1e-3,
            "half-size rig takes half the travel: {delta}"
        );
    }

    #[test]
    fn by_name_profiles_and_errors() {
        let a = humanoid(&GAME, 1.0, false);
        let b = humanoid(&GAME, 1.2, true);
        let rt = Retargeter::new(
            &a,
            &RetargetProfile::by_name(&a),
            &b,
            &RetargetProfile::by_name(&b),
        )
        .expect("by name");
        assert_eq!(rt.mapped_joints(), 12);
        let bad = RetargetProfile::default().with("hips", "nope");
        assert!(matches!(
            Retargeter::new(&a, &bad, &b, &RetargetProfile::by_name(&b)),
            Err(RetargetError::MissingJoint { .. })
        ));
        let none = RetargetProfile::default().with("tail", "head");
        assert_eq!(
            Retargeter::new(&a, &none, &b, &RetargetProfile::default().with("wing", "head")).err(),
            Some(RetargetError::NoCommonRoles)
        );
        assert!(
            RetargetProfile::guess_humanoid(&a).missing_humanoid_roles().len() >= 4,
            "no right side"
        );
    }

    #[test]
    fn ik_references_retarget_by_name() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let target = humanoid(&GAME, 1.5, true);
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");

        // Names map through the shared roles.
        assert_eq!(rt.retarget_joint_name("mixamorig:LeftArm"), Some("upperarm_l"));
        assert_eq!(
            rt.retarget_joint("mixamorig:LeftFoot"),
            target.joint_index("foot_l")
        );

        // A three-joint chain retargets joint by joint.
        let chain = rt
            .retarget_chain(&[
                "mixamorig:LeftArm".to_string(),
                "mixamorig:LeftForeArm".to_string(),
                "mixamorig:LeftHand".to_string(),
            ])
            .expect("arm chain maps");
        assert_eq!(chain, ["upperarm_l", "lowerarm_l", "hand_l"]);
        // An unmapped joint drops the whole chain.
        assert!(
            rt.retarget_chain(&[
                "tail".to_string(),
                "mixamorig:LeftForeArm".to_string(),
                "mixamorig:LeftHand".to_string(),
            ])
            .is_none()
        );

        // A mask retarget drops unmapped names and keeps mapped ones.
        assert_eq!(
            rt.retarget_mask(&["mixamorig:LeftArm".to_string(), "unknown".to_string()]),
            ["upperarm_l"]
        );

        // `target_of` inverts `source_of`.
        let arm_target = target.joint_index("upperarm_l").expect("joint exists");
        let arm_source = source.joint_index("mixamorig:LeftArm").expect("joint exists");
        assert_eq!(rt.source_of(arm_target), Some(arm_source));
        assert_eq!(rt.target_of(arm_source), Some(arm_target));
        // The implicit root pairing is present in the name map too.
        assert_eq!(rt.retarget_joint_name("Armature"), Some("root"));
    }

    /// `humanoid` with the origin moved to the pelvis: hips at `y = 0`, feet below it.
    fn pelvis_origin(names: &[&str; 12], size: f32) -> Skeleton {
        let base = humanoid(names, size, false);
        let mut binds = base.bind_pose.clone();
        binds[0].translation = Vec3::new(0.0, -size, 0.0);
        Skeleton::from_local_bind(base.joint_names.clone(), base.parents.clone(), binds)
            .expect("valid skeleton")
    }

    #[test]
    fn pelvis_origin_rigs_still_scale_by_leg_height() {
        // Hips at y = 0 on both rigs: the old ratio needed |y| > 0.1 mm on both and fell back
        // to 1, so a half-size rig took full-size steps.
        let source = pelvis_origin(&MIXAMO, 1.0);
        let target = pelvis_origin(&GAME, 0.5);
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
        assert!((rt.scale() - 0.5).abs() < 1e-3, "{}", rt.scale());
        // One rig on the floor, one at the pelvis: still the height ratio, never negative.
        let floor = humanoid(&MIXAMO, 1.0, false);
        let rt = Retargeter::humanoid(&floor, &target).expect("retargeter");
        assert!(rt.scale() > 0.0, "{}", rt.scale());
        assert!((rt.scale() - 0.5).abs() < 0.05, "{}", rt.scale());
    }

    #[test]
    fn malformed_clip_durations_bake_without_overflow() {
        let source = humanoid(&MIXAMO, 1.0, false);
        let target = humanoid(&GAME, 1.0, false);
        let rt = Retargeter::humanoid(&source, &target).expect("retargeter");
        for (duration, rate) in [
            (f32::INFINITY, 30.0),
            (f32::NAN, 30.0),
            (1.0, f32::INFINITY),
            (-2.0, 0.0),
        ] {
            let mut clip = AnimationClip::empty("bad", duration);
            clip.sample_rate = rate;
            let baked = rt.retarget_clip(&clip);
            assert!(
                baked.duration.is_finite() && baked.duration >= 0.0,
                "{duration} {rate}"
            );
            let keys = baked.tracks.first().map_or(0, |t| t.rotations.key_count());
            assert!(
                (2..=MAX_BAKE_FRAMES + 1).contains(&keys),
                "{duration} {rate}: {keys} keys"
            );
        }
    }
}
