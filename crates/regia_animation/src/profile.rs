// SPDX-License-Identifier: MPL-2.0
//! Skeleton profile assets (Tincal C7): the canonical joint list of a rig family.
//!
//! A [`SkeletonProfile`] (`.skelprofile.ron`, format v1, `docs/formats/skelprofile-ron.md`)
//! names the joints a rig family is expected to have, their hierarchy, the **role** each plays
//! (the VRM 1.0 humanoid bone names that [`crate::retarget`] uses, or any custom role), the
//! alternate names imports use for them, and the rest-pose rule (T-pose or A-pose).
//!
//! - [`SkeletonProfile::validate`] checks the profile itself.
//! - [`SkeletonProfile::check_skeleton`] checks a real skeleton against it: missing and extra
//!   joints, hierarchy, and the measured arm elevation against the rest-pose rule.
//! - [`SkeletonProfile::retarget_profile`] turns the match into a [`RetargetProfile`] for the
//!   retargeter.
//! - [`SkeletonProfile::rest_pose_normalization`] builds the bind pose rotated into the other
//!   stance (A to T or back).
//!
//! Built-ins: [`SkeletonProfile::generic_humanoid`] (the 22 retarget roles) and
//! [`SkeletonProfile::vrm_humanoid`] (adds eyes, jaw and fingers with VRM names). A project
//! profile (such as DO27's 67-joint rig) is a `.skelprofile.ron` file owned by that project.
//!
//! Provenance (`docs/provenance.md`): role names and the required/optional split follow the
//! VRM 1.0 humanoid specification (vrm-c/vrm-specification, public); the rest-pose
//! measurement (elevation of the upper-arm to lower-arm direction below horizontal) is plain
//! vector geometry.

use std::collections::{HashMap, HashSet};

use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use crate::pose::Pose;
use crate::retarget::{HumanoidBone, RetargetProfile};
use crate::skeleton::Skeleton;

/// The `.skelprofile.ron` format version. Version 1 is the first freeze.
pub const SKELPROFILE_FORMAT_VERSION: u32 = 1;

fn yes() -> bool {
    true
}

/// The stance a rig's bind pose is expected to be in.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RestPoseRule {
    /// No rule.
    #[default]
    Any,
    /// Arms straight out to the sides.
    TPose {
        /// Allowed deviation of the arm elevation from horizontal, degrees.
        tolerance_deg: f32,
    },
    /// Arms angled down.
    APose {
        /// Arm elevation below horizontal, degrees (45 is the usual A-pose).
        angle_deg: f32,
        /// Allowed deviation, degrees.
        tolerance_deg: f32,
    },
}

impl RestPoseRule {
    /// The expected arm elevation below horizontal and its tolerance, degrees.
    #[must_use]
    pub fn target(self) -> Option<(f32, f32)> {
        match self {
            RestPoseRule::Any => None,
            RestPoseRule::TPose { tolerance_deg } => Some((0.0, tolerance_deg)),
            RestPoseRule::APose {
                angle_deg,
                tolerance_deg,
            } => Some((angle_deg, tolerance_deg)),
        }
    }
}

/// One expected joint of a profile.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProfileJoint {
    /// The canonical joint name.
    pub name: String,
    /// The role this joint plays in retargeting (a VRM humanoid bone name or a custom role).
    #[serde(default)]
    pub role: Option<String>,
    /// The canonical parent joint, listed before this joint. `None` for roots.
    #[serde(default)]
    pub parent: Option<String>,
    /// Alternate names importers map to this joint (`"mixamorig:Hips"`).
    #[serde(default)]
    pub aliases: Vec<String>,
    /// Whether a skeleton must have this joint to pass.
    #[serde(default = "yes")]
    pub required: bool,
}

/// The `.skelprofile.ron` document.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SkeletonProfile {
    /// Format version of this file.
    pub format_version: u32,
    /// Profile name (`"generic-humanoid"`, `"vrm"`, `"do27-67"`).
    pub name: String,
    /// The expected joints; parents come before children.
    pub joints: Vec<ProfileJoint>,
    /// The expected bind-pose stance.
    #[serde(default)]
    pub rest_pose: RestPoseRule,
    /// Whether a skeleton may have joints the profile does not list (twist joints, props).
    #[serde(default = "yes")]
    pub allow_extra_joints: bool,
    /// Whether the required humanoid roles must all be present.
    #[serde(default = "yes")]
    pub humanoid: bool,
}

/// Errors loading a `.skelprofile.ron`.
#[derive(Debug, thiserror::Error)]
pub enum ProfileError {
    /// RON parse error.
    #[error("skelprofile RON parse error: {0}")]
    RonParse(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("skelprofile RON write error: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file's version is not the current one.
    #[error("skelprofile format version {0} is unknown (current is {SKELPROFILE_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed (missing version field).
    #[error("invalid skelprofile document: {0}")]
    Invalid(String),
}

/// The serde field-list fingerprint of the format.
#[must_use]
pub fn skelprofile_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SkeletonProfile>(),
        serde_field_names::<ProfileJoint>(),
        serde_variant_names::<RestPoseRule>(),
    ])
}

/// `from` versions of the registered migrations (for `cargo xtask check-formats`).
#[must_use]
pub fn skelprofile_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The measured arm elevation of one side.
#[derive(Clone, Debug, PartialEq)]
pub struct ArmElevation {
    /// `"left"` or `"right"`.
    pub side: &'static str,
    /// Degrees below horizontal (negative: raised above it).
    pub degrees: f32,
}

/// The rest-pose comparison of a [`ProfileReport`].
#[derive(Clone, Debug, PartialEq)]
pub struct RestPoseReport {
    /// The measured arm elevations (empty when the arms could not be found).
    pub arms: Vec<ArmElevation>,
    /// The expected elevation and tolerance, degrees.
    pub expected: (f32, f32),
    /// Whether every measured arm is within tolerance (`false` when none could be measured).
    pub ok: bool,
}

/// The result of checking a skeleton against a profile.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProfileReport {
    /// Profile name.
    pub profile: String,
    /// Profile joints matched to skeleton joints.
    pub matched: usize,
    /// Required profile joints with no match.
    pub missing_required: Vec<String>,
    /// Optional profile joints with no match.
    pub missing_optional: Vec<String>,
    /// Skeleton joints no profile joint matched.
    pub extra_joints: Vec<String>,
    /// Profile joints matched by a name guess instead of the name or an alias (role names).
    pub guessed: Vec<String>,
    /// Hierarchy violations: a joint whose profile parent is not among its ancestors.
    pub hierarchy: Vec<String>,
    /// Required humanoid roles missing (only for humanoid profiles).
    pub missing_roles: Vec<String>,
    /// Extra joints, when the profile forbids them.
    pub forbidden_extra: Vec<String>,
    /// The rest-pose comparison, when the profile has a rule.
    pub rest_pose: Option<RestPoseReport>,
}

impl ProfileReport {
    /// Whether the skeleton conforms to the profile.
    #[must_use]
    pub fn ok(&self) -> bool {
        self.missing_required.is_empty()
            && self.hierarchy.is_empty()
            && self.missing_roles.is_empty()
            && self.forbidden_extra.is_empty()
            && self.rest_pose.as_ref().is_none_or(|r| r.ok)
    }
}

/// Is `ancestor` an ancestor of `joint` (strictly) in the skeleton.
fn is_ancestor(skeleton: &Skeleton, ancestor: u32, joint: u32) -> bool {
    let mut current = skeleton.parents.get(joint as usize).copied().flatten();
    while let Some(p) = current {
        if p == ancestor {
            return true;
        }
        current = skeleton.parents.get(p as usize).copied().flatten();
    }
    false
}

/// Arm elevation below horizontal of the direction from `upper` to `lower`, degrees. `None` when
/// either joint is outside `globals` (a malformed skeleton whose inverse bind matrices do not
/// cover every joint; cooked loads validate that, a hand-built one may not).
fn elevation_below_horizontal(globals: &[regia_core::math::Mat4], upper: u32, lower: u32) -> Option<f32> {
    let direction =
        globals.get(lower as usize)?.w_axis.truncate() - globals.get(upper as usize)?.w_axis.truncate();
    Some((-direction.y).atan2(direction.x.hypot(direction.z)).to_degrees())
}

impl SkeletonProfile {
    /// Parses `.skelprofile.ron` text.
    ///
    /// # Errors
    ///
    /// Syntax errors and versions other than the current one.
    pub fn from_ron_str(text: &str) -> Result<Self, ProfileError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| ProfileError::Invalid(format!("cannot read format_version: {e}")))?;
        if version != SKELPROFILE_FORMAT_VERSION {
            return Err(ProfileError::UnknownVersion(version));
        }
        Ok(ron::from_str(text)?)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, ProfileError> {
        let mut document = self.clone();
        document.format_version = SKELPROFILE_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&document, pretty)?)
    }

    /// A built-in profile by name: `"generic-humanoid"` or `"vrm"`.
    #[must_use]
    pub fn builtin(name: &str) -> Option<Self> {
        match name {
            "generic-humanoid" => Some(Self::generic_humanoid()),
            "vrm" => Some(Self::vrm_humanoid()),
            _ => None,
        }
    }

    /// The built-in profile names.
    pub const BUILTINS: [&str; 2] = ["generic-humanoid", "vrm"];

    fn joint(name: &str, parent: Option<&str>, required: bool) -> ProfileJoint {
        ProfileJoint {
            name: name.to_string(),
            role: Some(name.to_string()),
            parent: parent.map(str::to_string),
            aliases: Vec::new(),
            required,
        }
    }

    fn humanoid_joints() -> Vec<ProfileJoint> {
        use HumanoidBone as H;
        let parent_of = |bone: HumanoidBone| -> Option<HumanoidBone> {
            Some(match bone {
                H::Hips => return None,
                H::Spine | H::LeftUpperLeg | H::RightUpperLeg => H::Hips,
                H::Chest => H::Spine,
                H::UpperChest => H::Chest,
                H::Neck | H::LeftShoulder | H::RightShoulder => H::UpperChest,
                H::Head => H::Neck,
                H::LeftUpperArm => H::LeftShoulder,
                H::LeftLowerArm => H::LeftUpperArm,
                H::LeftHand => H::LeftLowerArm,
                H::RightUpperArm => H::RightShoulder,
                H::RightLowerArm => H::RightUpperArm,
                H::RightHand => H::RightLowerArm,
                H::LeftLowerLeg => H::LeftUpperLeg,
                H::LeftFoot => H::LeftLowerLeg,
                H::LeftToes => H::LeftFoot,
                H::RightLowerLeg => H::RightUpperLeg,
                H::RightFoot => H::RightLowerLeg,
                H::RightToes => H::RightFoot,
            })
        };
        let mixamo = |bone: HumanoidBone| -> Option<&'static str> {
            Some(match bone {
                H::Hips => "mixamorig:Hips",
                H::Spine => "mixamorig:Spine",
                H::Chest => "mixamorig:Spine1",
                H::UpperChest => "mixamorig:Spine2",
                H::Neck => "mixamorig:Neck",
                H::Head => "mixamorig:Head",
                H::LeftShoulder => "mixamorig:LeftShoulder",
                H::LeftUpperArm => "mixamorig:LeftArm",
                H::LeftLowerArm => "mixamorig:LeftForeArm",
                H::LeftHand => "mixamorig:LeftHand",
                H::RightShoulder => "mixamorig:RightShoulder",
                H::RightUpperArm => "mixamorig:RightArm",
                H::RightLowerArm => "mixamorig:RightForeArm",
                H::RightHand => "mixamorig:RightHand",
                H::LeftUpperLeg => "mixamorig:LeftUpLeg",
                H::LeftLowerLeg => "mixamorig:LeftLeg",
                H::LeftFoot => "mixamorig:LeftFoot",
                H::LeftToes => "mixamorig:LeftToeBase",
                H::RightUpperLeg => "mixamorig:RightUpLeg",
                H::RightLowerLeg => "mixamorig:RightLeg",
                H::RightFoot => "mixamorig:RightFoot",
                H::RightToes => "mixamorig:RightToeBase",
            })
        };
        // The UpperChest and Chest are optional, so a child whose canonical parent is absent
        // is checked against the nearest present ancestor by `check_skeleton`.
        HumanoidBone::ALL
            .into_iter()
            .map(|bone| {
                let mut joint = Self::joint(
                    bone.name(),
                    parent_of(bone).map(HumanoidBone::name),
                    bone.required(),
                );
                joint.aliases.extend(mixamo(bone).map(str::to_string));
                joint
            })
            .collect()
    }

    /// The generic humanoid: the 22 retarget roles with VRM bone names, Mixamo aliases and an
    /// unconstrained stance.
    #[must_use]
    pub fn generic_humanoid() -> Self {
        SkeletonProfile {
            format_version: SKELPROFILE_FORMAT_VERSION,
            name: "generic-humanoid".to_string(),
            joints: Self::humanoid_joints(),
            rest_pose: RestPoseRule::Any,
            allow_extra_joints: true,
            humanoid: true,
        }
    }

    /// The VRM 1.0 humanoid: the generic joints plus the eyes, the jaw and the 30 finger bones
    /// (all optional, VRM bone names), in a T-pose within 10 degrees (VRM's rest pose).
    #[must_use]
    pub fn vrm_humanoid() -> Self {
        let mut profile = Self::generic_humanoid();
        profile.name = "vrm".to_string();
        profile.rest_pose = RestPoseRule::TPose { tolerance_deg: 10.0 };
        for side in ["left", "right"] {
            profile
                .joints
                .push(Self::joint(&format!("{side}Eye"), Some("head"), false));
            let hand = format!("{side}Hand");
            for (finger, segments) in [
                ("Thumb", ["Metacarpal", "Proximal", "Distal"]),
                ("Index", ["Proximal", "Intermediate", "Distal"]),
                ("Middle", ["Proximal", "Intermediate", "Distal"]),
                ("Ring", ["Proximal", "Intermediate", "Distal"]),
                ("Little", ["Proximal", "Intermediate", "Distal"]),
            ] {
                let mut parent = hand.clone();
                for segment in segments {
                    let name = format!("{side}{finger}{segment}");
                    profile.joints.push(Self::joint(&name, Some(&parent), false));
                    parent = name;
                }
            }
        }
        profile.joints.push(Self::joint("jaw", Some("head"), false));
        profile
    }

    /// Check the profile itself: names, parents, roles, rest-pose numbers.
    ///
    /// # Errors
    ///
    /// One entry per problem.
    pub fn validate(&self) -> Result<(), Vec<String>> {
        let mut problems = Vec::new();
        let mut names: HashSet<&str> = HashSet::new();
        let mut taken: HashMap<&str, &str> = HashMap::new();
        let mut roles: HashSet<&str> = HashSet::new();
        if self.joints.is_empty() {
            problems.push("the profile lists no joints".to_string());
        }
        for joint in &self.joints {
            if joint.name.is_empty() {
                problems.push("a joint has an empty name".to_string());
                continue;
            }
            if !names.insert(joint.name.as_str()) {
                problems.push(format!("duplicate joint {:?}", joint.name));
            }
            if let Some(parent) = &joint.parent
                && !names.contains(parent.as_str())
            {
                problems.push(format!(
                    "joint {:?}: parent {parent:?} is not listed before it",
                    joint.name
                ));
            }
            if let Some(role) = &joint.role
                && !roles.insert(role.as_str())
            {
                problems.push(format!("role {role:?} is given to more than one joint"));
            }
            for alias in &joint.aliases {
                if let Some(owner) = taken.insert(alias.as_str(), joint.name.as_str()) {
                    problems.push(format!(
                        "alias {alias:?} belongs to both {owner:?} and {:?}",
                        joint.name
                    ));
                }
            }
        }
        for joint in &self.joints {
            for alias in &joint.aliases {
                if names.contains(alias.as_str()) && alias != &joint.name {
                    problems.push(format!(
                        "alias {alias:?} of {:?} is another joint's name",
                        joint.name
                    ));
                }
            }
        }
        if self.humanoid {
            for bone in HumanoidBone::ALL {
                if bone.required() && !roles.contains(bone.name()) {
                    problems.push(format!("humanoid role {:?} is not assigned", bone.name()));
                }
            }
        }
        if let Some((angle, tolerance)) = self.rest_pose.target()
            && (!angle.is_finite()
                || !tolerance.is_finite()
                || tolerance < 0.0
                || !(-90.0..=90.0).contains(&angle))
        {
            problems.push(
                "the rest pose needs a finite angle in -90..=90 and a tolerance of at least 0".to_string(),
            );
        }
        if problems.is_empty() {
            Ok(())
        } else {
            Err(problems)
        }
    }

    /// Match profile joints to skeleton joints: the name, then an alias, then a
    /// case-insensitive name, then (for joints with a humanoid role) the name guess of
    /// [`RetargetProfile::guess_humanoid`]. Returns the skeleton joint per profile joint and
    /// the profile joints matched by the guess.
    #[must_use]
    pub fn match_joints(&self, skeleton: &Skeleton) -> (Vec<Option<u32>>, Vec<String>) {
        let exact: HashMap<&str, u32> = skeleton
            .joint_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), i as u32))
            .rev()
            .collect();
        let lower: HashMap<String, u32> = skeleton
            .joint_names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.to_ascii_lowercase(), i as u32))
            .rev()
            .collect();
        let guess = RetargetProfile::guess_humanoid(skeleton);
        let mut used: HashSet<u32> = HashSet::new();
        let mut guessed = Vec::new();
        let mut map = Vec::with_capacity(self.joints.len());
        // Names and aliases first, so a guess never steals a joint another profile joint names.
        for joint in &self.joints {
            let found = std::iter::once(joint.name.as_str())
                .chain(joint.aliases.iter().map(String::as_str))
                .find_map(|n| exact.get(n).copied())
                .or_else(|| {
                    std::iter::once(joint.name.as_str())
                        .chain(joint.aliases.iter().map(String::as_str))
                        .find_map(|n| lower.get(&n.to_ascii_lowercase()).copied())
                });
            if let Some(found) = found {
                used.insert(found);
            }
            map.push(found);
        }
        for (joint, slot) in self.joints.iter().zip(&mut map) {
            if slot.is_some() {
                continue;
            }
            let Some(role) = &joint.role else { continue };
            let Some(name) = guess.roles.get(role) else {
                continue;
            };
            if let Some(&index) = exact.get(name.as_str())
                && used.insert(index)
            {
                *slot = Some(index);
                guessed.push(joint.name.clone());
            }
        }
        (map, guessed)
    }

    /// Check `skeleton` against the profile.
    #[must_use]
    pub fn check_skeleton(&self, skeleton: &Skeleton) -> ProfileReport {
        let (map, guessed) = self.match_joints(skeleton);
        let by_name: HashMap<&str, usize> = self
            .joints
            .iter()
            .enumerate()
            .map(|(i, j)| (j.name.as_str(), i))
            .collect();
        let mut report = ProfileReport {
            profile: self.name.clone(),
            guessed,
            ..ProfileReport::default()
        };
        for (joint, found) in self.joints.iter().zip(&map) {
            match found {
                Some(_) => report.matched += 1,
                None if joint.required => report.missing_required.push(joint.name.clone()),
                None => report.missing_optional.push(joint.name.clone()),
            }
        }
        let matched: HashSet<u32> = map.iter().flatten().copied().collect();
        report.extra_joints = skeleton
            .joint_names
            .iter()
            .enumerate()
            .filter(|(i, _)| !matched.contains(&(*i as u32)))
            .map(|(_, n)| n.clone())
            .collect();
        if !self.allow_extra_joints {
            report.forbidden_extra.clone_from(&report.extra_joints);
        }
        // Hierarchy: the nearest listed ancestor that exists in the skeleton must be an
        // ancestor of the joint (extra joints in between are fine).
        for (i, joint) in self.joints.iter().enumerate() {
            let Some(skeleton_joint) = map[i] else { continue };
            let mut parent = joint.parent.as_deref();
            while let Some(name) = parent {
                let Some(&pi) = by_name.get(name) else { break };
                if let Some(parent_joint) = map[pi] {
                    if !is_ancestor(skeleton, parent_joint, skeleton_joint) {
                        report.hierarchy.push(format!(
                            "{:?} should descend from {:?}",
                            joint.name, self.joints[pi].name
                        ));
                    }
                    break;
                }
                parent = self.joints[pi].parent.as_deref();
            }
        }
        if self.humanoid {
            let roles = self.retarget_profile_from(&map, skeleton);
            report.missing_roles = roles
                .missing_humanoid_roles()
                .into_iter()
                .map(|r| r.name().to_string())
                .collect();
        }
        if let Some(expected) = self.rest_pose.target() {
            let arms = self.measure_arms(&map, skeleton);
            let ok = !arms.is_empty() && arms.iter().all(|a| (a.degrees - expected.0).abs() <= expected.1);
            report.rest_pose = Some(RestPoseReport { arms, expected, ok });
        }
        report
    }

    fn role_joint(&self, map: &[Option<u32>], role: &str) -> Option<u32> {
        self.joints
            .iter()
            .zip(map)
            .find(|(j, _)| j.role.as_deref() == Some(role))
            .and_then(|(_, m)| *m)
    }

    fn arm_joints(&self, map: &[Option<u32>]) -> Vec<(&'static str, u32, u32)> {
        let mut arms = Vec::new();
        for (side, upper, lower) in [
            ("left", HumanoidBone::LeftUpperArm, HumanoidBone::LeftLowerArm),
            ("right", HumanoidBone::RightUpperArm, HumanoidBone::RightLowerArm),
        ] {
            if let (Some(u), Some(l)) = (
                self.role_joint(map, upper.name()),
                self.role_joint(map, lower.name()),
            ) {
                arms.push((side, u, l));
            }
        }
        arms
    }

    fn measure_arms(&self, map: &[Option<u32>], skeleton: &Skeleton) -> Vec<ArmElevation> {
        let globals = skeleton.bind_globals();
        // An arm that cannot be measured (a joint outside the skeleton's matrices) makes the whole
        // measurement empty, so the rest-pose check fails instead of passing on the other arm.
        self.arm_joints(map)
            .into_iter()
            .map(|(side, upper, lower)| {
                elevation_below_horizontal(&globals, upper, lower)
                    .map(|degrees| ArmElevation { side, degrees })
            })
            .collect::<Option<Vec<_>>>()
            .unwrap_or_default()
    }

    fn retarget_profile_from(&self, map: &[Option<u32>], skeleton: &Skeleton) -> RetargetProfile {
        let mut profile = RetargetProfile::default();
        for (joint, found) in self.joints.iter().zip(map) {
            if let (Some(role), Some(index)) = (&joint.role, found) {
                profile
                    .roles
                    .insert(role.clone(), skeleton.joint_names[*index as usize].clone());
            }
        }
        profile
    }

    /// The [`RetargetProfile`] of `skeleton` under this profile: role → the skeleton's joint.
    #[must_use]
    pub fn retarget_profile(&self, skeleton: &Skeleton) -> RetargetProfile {
        let (map, _) = self.match_joints(skeleton);
        self.retarget_profile_from(&map, skeleton)
    }

    /// The bind pose of `skeleton` with both upper arms rotated so the arm elevation matches
    /// `target` (an A-pose rig into a T-pose, or back). `None` when the arms are not found or
    /// the target is [`RestPoseRule::Any`]. The rotation is taken about the horizontal axis
    /// perpendicular to the arm, so the arm stays in its vertical plane.
    #[must_use]
    pub fn rest_pose_normalization(&self, skeleton: &Skeleton, target: RestPoseRule) -> Option<Pose> {
        let (wanted, _) = target.target()?;
        let (map, _) = self.match_joints(skeleton);
        let arms = self.arm_joints(&map);
        if arms.is_empty() {
            return None;
        }
        let mut pose = skeleton.bind_pose_pose();
        let globals = skeleton.bind_globals();
        for (_, upper, lower) in arms {
            // A joint outside the skeleton's matrices or pose (a malformed skeleton that skipped
            // cooked validation) leaves that arm as it is, like a zero-length arm below.
            let parent = skeleton.parent_of(upper as usize);
            let in_range =
                (upper as usize) < pose.rotations.len() && parent.is_none_or(|p| p < globals.len());
            let (Some(upper_global), Some(lower_global)) =
                (globals.get(upper as usize), globals.get(lower as usize))
            else {
                continue;
            };
            if !in_range {
                continue;
            }
            let direction =
                (lower_global.w_axis.truncate() - upper_global.w_axis.truncate()).normalize_or_zero();
            let horizontal = Vec3::new(direction.x, 0.0, direction.z).normalize_or_zero();
            if direction == Vec3::ZERO || horizontal == Vec3::ZERO {
                continue;
            }
            let wanted = wanted.to_radians();
            let new_direction = horizontal * wanted.cos() - Vec3::Y * wanted.sin();
            let global_delta = Quat::from_rotation_arc(direction, new_direction);
            let parent_rotation = parent.map_or(Quat::IDENTITY, |p| Quat::from_mat4(&globals[p]).normalize());
            let local = pose.rotations[upper as usize];
            pose.rotations[upper as usize] =
                (parent_rotation.inverse() * global_delta * parent_rotation * local).normalize();
        }
        Some(pose)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::LocalBind;

    /// A humanoid whose arms hang at `arm_deg` below horizontal, with joint names from `names`.
    fn humanoid(arm_deg: f32, rename: impl Fn(&str) -> String) -> Skeleton {
        let profile = SkeletonProfile::generic_humanoid();
        let mut names = Vec::new();
        let mut parents = Vec::new();
        let mut binds = Vec::new();
        let index: HashMap<&str, u32> = profile
            .joints
            .iter()
            .enumerate()
            .map(|(i, j)| (j.name.as_str(), i as u32))
            .collect();
        let rad = arm_deg.to_radians();
        for joint in &profile.joints {
            names.push(rename(&joint.name));
            parents.push(joint.parent.as_deref().map(|p| index[p]));
            let name = joint.name.as_str();
            let side = if name.starts_with("left") { 1.0 } else { -1.0 };
            let translation = match name {
                "hips" => Vec3::new(0.0, 1.0, 0.0),
                "spine" | "chest" | "upperChest" => Vec3::new(0.0, 0.15, 0.0),
                "neck" => Vec3::new(0.0, 0.1, 0.0),
                "head" => Vec3::new(0.0, 0.1, 0.0),
                "leftShoulder" | "rightShoulder" => Vec3::new(side * 0.05, 0.05, 0.0),
                "leftUpperArm" | "rightUpperArm" => Vec3::new(side * 0.1, 0.0, 0.0),
                "leftLowerArm" | "rightLowerArm" => Vec3::new(side * rad.cos(), -rad.sin(), 0.0) * 0.3,
                "leftHand" | "rightHand" => Vec3::new(side * rad.cos(), -rad.sin(), 0.0) * 0.25,
                "leftUpperLeg" | "rightUpperLeg" => Vec3::new(side * 0.1, -0.05, 0.0),
                "leftLowerLeg" | "rightLowerLeg" => Vec3::new(0.0, -0.45, 0.0),
                "leftFoot" | "rightFoot" => Vec3::new(0.0, -0.45, 0.0),
                _ => Vec3::new(0.0, 0.0, 0.1),
            };
            binds.push(LocalBind {
                translation,
                ..LocalBind::default()
            });
        }
        Skeleton::from_local_bind(names, parents, binds).expect("valid")
    }

    #[test]
    fn the_builtins_are_self_consistent() {
        for name in SkeletonProfile::BUILTINS {
            let profile = SkeletonProfile::builtin(name).expect("exists");
            assert_eq!(profile.validate(), Ok(()), "{name}");
        }
        assert_eq!(SkeletonProfile::generic_humanoid().joints.len(), 22);
        // 22 + 2 eyes + 30 fingers + jaw.
        assert_eq!(SkeletonProfile::vrm_humanoid().joints.len(), 55);
        assert!(SkeletonProfile::builtin("nope").is_none());
    }

    #[test]
    fn a_conforming_skeleton_passes_and_extra_joints_are_listed() {
        let profile = SkeletonProfile::generic_humanoid();
        let skeleton = humanoid(0.0, str::to_string);
        let report = profile.check_skeleton(&skeleton);
        assert!(report.ok(), "{report:?}");
        assert_eq!(report.matched, 22);
        assert!(report.extra_joints.is_empty() && report.guessed.is_empty());
    }

    #[test]
    fn mixamo_names_match_through_aliases() {
        let profile = SkeletonProfile::generic_humanoid();
        let mixamo = |role: &str| {
            profile
                .joints
                .iter()
                .find(|j| j.name == role)
                .and_then(|j| j.aliases.first().cloned())
                .unwrap_or_else(|| role.to_string())
        };
        let skeleton = humanoid(0.0, mixamo);
        let report = profile.check_skeleton(&skeleton);
        assert!(report.ok(), "{report:?}");
        let retarget = profile.retarget_profile(&skeleton);
        assert_eq!(retarget.roles["leftUpperArm"], "mixamorig:LeftArm");
        assert_eq!(retarget.roles.len(), 22);
    }

    #[test]
    fn unfamiliar_names_fall_back_to_the_guess_and_say_so() {
        let profile = SkeletonProfile::generic_humanoid();
        let skeleton = humanoid(0.0, |n| match n {
            "leftUpperArm" => "upper_arm.L".to_string(),
            other => other.to_string(),
        });
        let report = profile.check_skeleton(&skeleton);
        assert!(report.ok(), "{report:?}");
        assert_eq!(report.guessed, ["leftUpperArm"]);
    }

    #[test]
    fn missing_joints_and_broken_hierarchy_fail() {
        let profile = SkeletonProfile::generic_humanoid();
        let skeleton = humanoid(0.0, |n| if n == "leftHand" { "paw".into() } else { n.into() });
        let report = profile.check_skeleton(&skeleton);
        assert!(!report.ok());
        assert_eq!(report.missing_required, ["leftHand"]);
        assert_eq!(report.missing_roles, ["leftHand"]);
        assert_eq!(report.extra_joints, ["paw"]);
        // Optional joints (upperChest, shoulders, toes) may be absent.
        let mut skeleton = humanoid(0.0, str::to_string);
        // Re-parent the head under the hips: the neck chain no longer contains it.
        skeleton.parents[5] = Some(0);
        let report = profile.check_skeleton(&skeleton);
        assert!(report.hierarchy.iter().any(|h| h.contains("head")), "{report:?}");
    }

    #[test]
    fn the_rest_pose_rule_measures_the_arms() {
        let mut profile = SkeletonProfile::generic_humanoid();
        profile.rest_pose = RestPoseRule::TPose { tolerance_deg: 5.0 };
        assert!(profile.check_skeleton(&humanoid(2.0, str::to_string)).ok());
        let a_pose = profile.check_skeleton(&humanoid(45.0, str::to_string));
        assert!(!a_pose.ok());
        let rest = a_pose.rest_pose.expect("measured");
        assert_eq!(rest.arms.len(), 2);
        assert!((rest.arms[0].degrees - 45.0).abs() < 0.1, "{rest:?}");
        profile.rest_pose = RestPoseRule::APose {
            angle_deg: 45.0,
            tolerance_deg: 5.0,
        };
        assert!(profile.check_skeleton(&humanoid(45.0, str::to_string)).ok());
    }

    #[test]
    fn normalizing_a_pose_to_t_puts_the_arms_horizontal() {
        let profile = SkeletonProfile::generic_humanoid();
        let skeleton = humanoid(45.0, str::to_string);
        let pose = profile
            .rest_pose_normalization(&skeleton, RestPoseRule::TPose { tolerance_deg: 1.0 })
            .expect("arms found");
        let globals = skeleton.globals(&pose, None);
        let upper = skeleton.joint_index("leftUpperArm").expect("joint");
        let lower = skeleton.joint_index("leftLowerArm").expect("joint");
        let degrees = elevation_below_horizontal(&globals, upper, lower).expect("joints in range");
        assert!(degrees.abs() < 0.1, "{degrees}");
        let right = elevation_below_horizontal(
            &globals,
            skeleton.joint_index("rightUpperArm").expect("joint"),
            skeleton.joint_index("rightLowerArm").expect("joint"),
        )
        .expect("joints in range");
        assert!(right.abs() < 0.1, "{right}");
        // And back to 45.
        let back = profile
            .rest_pose_normalization(
                &skeleton,
                RestPoseRule::APose {
                    angle_deg: 30.0,
                    tolerance_deg: 1.0,
                },
            )
            .expect("arms found");
        let globals = skeleton.globals(&back, None);
        assert!(
            (elevation_below_horizontal(&globals, upper, lower).expect("joints in range") - 30.0).abs() < 0.1
        );
        assert!(
            profile
                .rest_pose_normalization(&skeleton, RestPoseRule::Any)
                .is_none()
        );
    }

    /// A skeleton whose inverse bind matrices stop short of the arm joints (a hand-built one that
    /// skipped cooked validation) is reported, not indexed out of bounds (w10-v051-smalls 5).
    #[test]
    fn a_skeleton_with_short_bind_matrices_does_not_panic() {
        let mut profile = SkeletonProfile::generic_humanoid();
        profile.rest_pose = RestPoseRule::TPose { tolerance_deg: 5.0 };
        let mut skeleton = humanoid(2.0, str::to_string);
        let upper = skeleton.joint_index("leftUpperArm").expect("joint") as usize;
        skeleton.inverse_bind_matrices.truncate(upper);
        let report = profile.check_skeleton(&skeleton);
        let rest = report.rest_pose.as_ref().expect("rest pose measured");
        assert!(
            rest.arms.is_empty(),
            "an unmeasurable arm empties the measurement: {rest:?}"
        );
        assert!(!rest.ok && !report.ok());
        // The normalization leaves the bind pose alone instead of panicking.
        let pose = profile
            .rest_pose_normalization(&skeleton, RestPoseRule::TPose { tolerance_deg: 1.0 })
            .expect("arms are named");
        assert_eq!(pose.rotations, skeleton.bind_pose_pose().rotations);
    }

    #[test]
    fn the_arm_elevation_of_a_joint_outside_the_matrices_is_none() {
        let skeleton = humanoid(2.0, str::to_string);
        let globals = skeleton.bind_globals();
        let n = globals.len() as u32;
        assert!(elevation_below_horizontal(&globals, 0, n - 1).is_some());
        assert!(elevation_below_horizontal(&globals, 0, n).is_none());
        assert!(elevation_below_horizontal(&globals, u32::MAX, 0).is_none());
    }

    #[test]
    fn validate_reports_every_profile_problem() {
        let mut profile = SkeletonProfile::generic_humanoid();
        profile.joints[1].name = "hips".into();
        profile.joints[3].parent = Some("later".into());
        profile.joints[4].role = Some("hips".into());
        profile.joints[5].aliases.push("hips".into());
        profile.rest_pose = RestPoseRule::APose {
            angle_deg: 200.0,
            tolerance_deg: 1.0,
        };
        let problems = profile.validate().expect_err("invalid");
        assert!(problems.len() >= 5, "{problems:?}");
        assert!(
            SkeletonProfile {
                joints: Vec::new(),
                ..SkeletonProfile::generic_humanoid()
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn ron_round_trips_and_other_versions_are_rejected() {
        let profile = SkeletonProfile::vrm_humanoid();
        let text = profile.to_ron_string().expect("writes");
        assert_eq!(SkeletonProfile::from_ron_str(&text).expect("reads"), profile);
        let minimal = r#"(format_version: 1, name: "tiny", joints: [(name: "root", role: Some("hips"))],
            humanoid: false)"#;
        let tiny = SkeletonProfile::from_ron_str(minimal).expect("defaults fill in");
        assert!(tiny.allow_extra_joints && tiny.joints[0].required && tiny.validate().is_ok());
        assert!(matches!(
            SkeletonProfile::from_ron_str("(format_version: 7)"),
            Err(ProfileError::UnknownVersion(7))
        ));
    }

    #[test]
    fn fingerprint_lists_the_fields() {
        let fp = skelprofile_schema_fingerprint();
        for field in ["allow_extra_joints", "aliases", "APose"] {
            assert!(fp.contains(field), "{field} missing from {fp}");
        }
        assert!(skelprofile_migration_sources().is_empty());
    }
}
