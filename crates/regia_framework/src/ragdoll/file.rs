// SPDX-License-Identifier: MPL-2.0
//! Ragdoll profile files (`*.ragdoll.ron`, `format_version` 4, spec 93 § 3 and § 10) and the
//! humanoid joint-limit table.
//!
//! Version 2 adds the per-bone `shape` (capsule, sphere or box, ADR-2133 § 6). A version 1
//! file has no `shape` and every bone is a capsule: [`RagdollProfile::from_ron`] reads it as
//! such. Version 3 adds the optional `active` block (PD motors, group strength, hit response,
//! balance and get-up clips, ADR-2498); a version 1 or 2 file has none and loads with zero
//! gains, so it behaves as before. Version 4 adds the `Sole` body shape and the optional `active.recovery` block
//! (stepping, landing, moving supports, ADR-2963); a version 3 file has none and loads with recovery off.
//! [`migrate_ragdoll`] rewrites any older file as version 4 (`regia upgrade-project`).

use regia_core::math::{Quat, Vec3};
use serde::{Deserialize, Serialize};

use super::{
    RagdollActiveProfile, RagdollBalance, RagdollBalanceRole, RagdollBone, RagdollFacing, RagdollFlex,
    RagdollGetUp, RagdollGroup, RagdollGroupStrength, RagdollHitResponse, RagdollLimits, RagdollMotor,
    RagdollProfile, RagdollShape,
};

/// Current `.ragdoll.ron` format version.
pub const RAGDOLL_FORMAT_VERSION: u32 = 4;

/// Errors reading a ragdoll profile file. [`RagdollFileError::code`] is the stable protocol
/// code.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum RagdollFileError {
    /// The file is a newer version than this build reads.
    #[error("ragdoll profile version {found} is newer than supported version {expected}; upgrade the engine")]
    Version {
        /// Version in the file.
        found: u32,
        /// Version this build reads.
        expected: u32,
    },
    /// RON syntax or shape error (the text carries line and column).
    #[error("ragdoll profile parse error: {0}")]
    Parse(String),
    /// A bone has a non-positive or non-finite radius or mass, or an empty joint name.
    #[error("ragdoll bone {index} ({joint:?}) is invalid: {message}")]
    BadBone {
        /// Index in `bones`.
        index: usize,
        /// The bone's joint name.
        joint: String,
        /// What is wrong.
        message: String,
    },
    /// The v3 `active` block is invalid (a motor on a joint with no bone, a negative gain, a strength outside
    /// `0..=1`, a non-finite number, an empty get-up clip name).
    #[error("ragdoll active block is invalid: {0}")]
    BadActive(String),
}

impl RagdollFileError {
    /// The stable lowercase protocol code of this error.
    pub fn code(&self) -> &'static str {
        match self {
            RagdollFileError::Version { .. } => "ragdoll_format_version",
            RagdollFileError::Parse(_) => "ragdoll_parse",
            RagdollFileError::BadBone { .. } => "ragdoll_bad_bone",
            RagdollFileError::BadActive(_) => "ragdoll_bad_active",
        }
    }
}

/// The root of a `.ragdoll.ron` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollProfileFile {
    /// Format version ([`RAGDOLL_FORMAT_VERSION`]).
    pub format_version: u32,
    /// Rig-space direction the character faces.
    pub forward: Vec3,
    /// Simulated bones.
    pub bones: Vec<RagdollBoneFile>,
    /// Active-ragdoll block (v3); absent in v1/v2 files and when every gain is zero.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<RagdollActiveFile>,
}

/// The `active` block of a v3 `.ragdoll.ron` file (spec 93 § 10).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollActiveFile {
    /// PD motors, at most one per bone.
    pub motors: Vec<RagdollMotorFile>,
    /// Strength per group, `0..=1` (default 1 each).
    #[serde(default)]
    pub strength: RagdollStrengthFile,
    /// Hit response.
    #[serde(default)]
    pub hit: RagdollHitFile,
    /// Balance tuning.
    #[serde(default)]
    pub balance: RagdollBalanceFile,
    /// Get-up clips.
    #[serde(default)]
    pub get_up: Vec<RagdollGetUpFile>,
    /// Stepping, landing and moving supports (v4, ADR-2963); absent: all off.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recovery: Option<RagdollRecoveryFile>,
}

/// [`RagdollRecovery`](super::RagdollRecovery) in a file (v4). Every field left out takes the value of
/// `RagdollRecovery::full()`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RagdollRecoveryFile {
    /// Largest number of steps in one recovery (0: never step).
    pub max_steps: u32,
    /// Capture-point distance outside the polygon that starts a step, m.
    pub trigger_margin: f32,
    /// Distance the step lands beyond the capture point, m.
    pub overshoot: f32,
    /// Longest step, m.
    pub max_step: f32,
    /// Least sideways distance between the feet, m.
    pub min_width: f32,
    /// Seconds of a swing.
    pub swing_time: f32,
    /// Swing foot lift, m.
    pub lift: f32,
    /// Land on the feet from the air.
    pub landing: bool,
    /// Knee bend while airborne (fraction of the leg's length).
    pub pre_bend: f32,
    /// Landing crouch depth, m.
    pub crouch: f32,
    /// Seconds the crouch takes to rise.
    pub absorb_time: f32,
    /// Seconds airborne before falling.
    pub max_air_time: f32,
    /// Plant on a single body that can move.
    pub moving_support: bool,
}

impl From<super::RagdollRecovery> for RagdollRecoveryFile {
    fn from(r: super::RagdollRecovery) -> Self {
        RagdollRecoveryFile {
            max_steps: r.max_steps,
            trigger_margin: r.trigger_margin,
            overshoot: r.overshoot,
            max_step: r.max_step,
            min_width: r.min_width,
            swing_time: r.swing_time,
            lift: r.lift,
            landing: r.landing,
            pre_bend: r.pre_bend,
            crouch: r.crouch,
            absorb_time: r.absorb_time,
            max_air_time: r.max_air_time,
            moving_support: r.moving_support,
        }
    }
}

impl From<RagdollRecoveryFile> for super::RagdollRecovery {
    fn from(r: RagdollRecoveryFile) -> Self {
        super::RagdollRecovery {
            max_steps: r.max_steps,
            trigger_margin: r.trigger_margin,
            overshoot: r.overshoot,
            max_step: r.max_step,
            min_width: r.min_width,
            swing_time: r.swing_time,
            lift: r.lift,
            landing: r.landing,
            pre_bend: r.pre_bend,
            crouch: r.crouch,
            absorb_time: r.absorb_time,
            max_air_time: r.max_air_time,
            moving_support: r.moving_support,
        }
    }
}

impl Default for RagdollRecoveryFile {
    fn default() -> Self {
        super::RagdollRecovery::full().into()
    }
}

/// One PD motor in a file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollMotorFile {
    /// The bone's joint name (must be one of `bones`).
    pub joint: String,
    /// Strength group.
    pub group: RagdollGroupFile,
    /// Stiffness, N·m/rad (`>= 0`).
    pub kp: f32,
    /// Damping, N·m·s/rad (`>= 0`).
    pub kd: f32,
    /// Largest torque, N·m (`> 0`).
    pub max_torque: f32,
    /// Balance role (default `None`).
    #[serde(default)]
    pub balance: RagdollBalanceRoleFile,
}

/// [`RagdollGroup`] in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RagdollGroupFile {
    /// Pelvis, spine, head.
    Spine,
    /// Arms.
    Arms,
    /// Legs.
    Legs,
}

/// [`RagdollBalanceRole`] in a file.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum RagdollBalanceRoleFile {
    /// Not part of balance.
    #[default]
    None,
    /// A hip.
    Hip,
    /// An ankle (the foot is a support body).
    Ankle,
}

/// [`RagdollGroupStrength`] in a file.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RagdollStrengthFile {
    /// Spine group strength.
    pub spine: f32,
    /// Arms group strength.
    pub arms: f32,
    /// Legs group strength.
    pub legs: f32,
}

impl Default for RagdollStrengthFile {
    fn default() -> Self {
        let s = RagdollGroupStrength::default();
        RagdollStrengthFile {
            spine: s.spine,
            arms: s.arms,
            legs: s.legs,
        }
    }
}

/// [`RagdollHitResponse`] in a file.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RagdollHitFile {
    /// Impulse (kg·m/s) that drops the hit group to zero strength.
    pub full_impulse: f32,
    /// Seconds to recover full strength.
    pub recover_time: f32,
}

impl Default for RagdollHitFile {
    fn default() -> Self {
        let h = RagdollHitResponse::default();
        RagdollHitFile {
            full_impulse: h.full_impulse,
            recover_time: h.recover_time,
        }
    }
}

/// [`RagdollBalance`] in a file.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RagdollBalanceFile {
    /// Lean per metre of offset, rad/m.
    pub cd: f32,
    /// Lean per m/s of velocity, rad/(m/s).
    pub cv: f32,
    /// Largest lean, rad.
    pub max_tilt: f32,
    /// Distance outside the support polygon before falling, m.
    pub fall_margin: f32,
    /// Seconds outside before falling.
    pub fall_time: f32,
    /// Motor strength while falling and lying.
    pub fall_strength: f32,
}

impl Default for RagdollBalanceFile {
    fn default() -> Self {
        let b = RagdollBalance::default();
        RagdollBalanceFile {
            cd: b.cd,
            cv: b.cv,
            max_tilt: b.max_tilt,
            fall_margin: b.fall_margin,
            fall_time: b.fall_time,
            fall_strength: b.fall_strength,
        }
    }
}

/// [`RagdollFacing`] in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RagdollFacingFile {
    /// On the back.
    FaceUp,
    /// On the front.
    FaceDown,
    /// On the left side.
    SideLeft,
    /// On the right side.
    SideRight,
}

/// One get-up clip in a file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollGetUpFile {
    /// Clip name (non-empty).
    pub clip: String,
    /// The facing it starts from.
    pub facing: RagdollFacingFile,
    /// Key bones of its first frame (may be empty: the facing alone decides).
    #[serde(default)]
    pub pose: Vec<RagdollKeyRotationFile>,
}

/// One key bone of a get-up clip: the joint's rotation relative to the root joint, `(x, y, z, w)`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollKeyRotationFile {
    /// Joint name.
    pub joint: String,
    /// Quaternion `(x, y, z, w)`; normalized on load.
    pub rotation: (f32, f32, f32, f32),
}

/// One bone in a `.ragdoll.ron` file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollBoneFile {
    /// Skeleton joint name.
    pub joint: String,
    /// Capsule radius, metres.
    pub radius: f32,
    /// Body mass, kg.
    pub mass: f32,
    /// Capsule length (metres); `None` measures it from the skeleton.
    pub length: Option<f32>,
    /// Joint limits; `None` is an unlimited ball joint.
    pub limits: Option<RagdollLimitsFile>,
    /// Collision shape (v2); absent in v1 files, where every bone is a capsule.
    #[serde(default)]
    pub shape: RagdollShapeFile,
}

/// [`RagdollShape`] as stored in a file (v2).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub enum RagdollShapeFile {
    /// A capsule of the bone's radius.
    #[default]
    Capsule,
    /// A sphere of the bone's radius.
    Sphere,
    /// A box: full side-to-side `width` and front-to-back `depth`, metres.
    Box {
        /// Side-to-side extent, metres.
        width: f32,
        /// Front-to-back extent, metres.
        depth: f32,
    },
    /// A flat foot (v4, ADR-2963): full side-to-side `width` and `height` (thickness), metres.
    Sole {
        /// Side-to-side extent, metres.
        width: f32,
        /// Thickness, metres.
        height: f32,
    },
}

/// Joint limits in a `.ragdoll.ron` file.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RagdollLimitsFile {
    /// Which way the limb flexes.
    pub flex: RagdollFlexFile,
    /// Min/max flexion (rad).
    pub bend: (f32, f32),
    /// Min/max sideways swing (rad).
    pub side: (f32, f32),
    /// Min/max twist about the bone (rad).
    pub twist: (f32, f32),
}

/// [`RagdollFlex`] as stored in a file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RagdollFlexFile {
    /// Towards the profile's forward.
    Forward,
    /// Away from the profile's forward.
    Backward,
}

/// `forward` as a unit vector. A vector that already is one is kept: normalizing again moves it by an ulp, so a profile
/// would change every time it is loaded and saved (ADR-1917).
fn unit_forward(forward: Vec3) -> Vec3 {
    if (forward.length_squared() - 1.0).abs() <= 4.0 * f32::EPSILON {
        forward
    } else {
        forward.normalize_or(Vec3::NEG_Z)
    }
}

impl RagdollProfile {
    /// Parses a `.ragdoll.ron` text (spec 93 § 3).
    pub fn from_ron(text: &str) -> Result<RagdollProfile, RagdollFileError> {
        #[derive(Deserialize)]
        struct Header {
            format_version: u32,
        }
        // Read the version first so a newer file reports its version, not a shape error.
        let options = ron::Options::default();
        let header: Header = options
            .from_str(text)
            .map_err(|e| RagdollFileError::Parse(e.to_string()))?;
        if header.format_version > RAGDOLL_FORMAT_VERSION {
            return Err(RagdollFileError::Version {
                found: header.format_version,
                expected: RAGDOLL_FORMAT_VERSION,
            });
        }
        let file: RagdollProfileFile = options
            .from_str(text)
            .map_err(|e| RagdollFileError::Parse(e.to_string()))?;
        let mut bones = Vec::with_capacity(file.bones.len());
        for (index, bone) in file.bones.into_iter().enumerate() {
            let bad = |message: &str| RagdollFileError::BadBone {
                index,
                joint: bone.joint.clone(),
                message: message.to_string(),
            };
            if bone.joint.is_empty() {
                return Err(bad("the joint name is empty"));
            }
            if !(bone.radius.is_finite() && bone.radius > 0.0) {
                return Err(bad("radius must be a positive number"));
            }
            if !(bone.mass.is_finite() && bone.mass > 0.0) {
                return Err(bad("mass must be a positive number"));
            }
            let shape = match bone.shape {
                RagdollShapeFile::Capsule => RagdollShape::Capsule,
                RagdollShapeFile::Sphere => RagdollShape::Sphere,
                RagdollShapeFile::Box { width, depth } => {
                    if !(width.is_finite() && width > 0.0 && depth.is_finite() && depth > 0.0) {
                        return Err(bad("box width and depth must be positive numbers"));
                    }
                    RagdollShape::Box { width, depth }
                }
                RagdollShapeFile::Sole { width, height } => {
                    if !(width.is_finite() && width > 0.0 && height.is_finite() && height > 0.0) {
                        return Err(bad("sole width and height must be positive numbers"));
                    }
                    RagdollShape::Sole { width, height }
                }
            };
            bones.push(RagdollBone {
                joint: bone.joint.clone(),
                radius: bone.radius,
                mass: bone.mass,
                length: bone.length,
                limits: bone.limits.map(|l| RagdollLimits {
                    flex: match l.flex {
                        RagdollFlexFile::Forward => RagdollFlex::Forward,
                        RagdollFlexFile::Backward => RagdollFlex::Backward,
                    },
                    bend: l.bend,
                    side: l.side,
                    twist: l.twist,
                }),
                shape,
            });
        }
        let active = match file.active {
            None => RagdollActiveProfile::default(),
            Some(active) => active_from_file(active, &bones)?,
        };
        Ok(RagdollProfile {
            bones,
            forward: unit_forward(file.forward),
            active,
        })
    }

    /// Writes this profile as `.ragdoll.ron` text (the current format version).
    pub fn to_ron(&self) -> Result<String, RagdollFileError> {
        let file = RagdollProfileFile {
            format_version: RAGDOLL_FORMAT_VERSION,
            forward: self.forward,
            bones: self
                .bones
                .iter()
                .map(|b| RagdollBoneFile {
                    joint: b.joint.clone(),
                    radius: b.radius,
                    mass: b.mass,
                    length: b.length,
                    limits: b.limits.map(|l| RagdollLimitsFile {
                        flex: match l.flex {
                            RagdollFlex::Forward => RagdollFlexFile::Forward,
                            RagdollFlex::Backward => RagdollFlexFile::Backward,
                        },
                        bend: l.bend,
                        side: l.side,
                        twist: l.twist,
                    }),
                    shape: match b.shape {
                        RagdollShape::Capsule => RagdollShapeFile::Capsule,
                        RagdollShape::Sphere => RagdollShapeFile::Sphere,
                        RagdollShape::Box { width, depth } => RagdollShapeFile::Box { width, depth },
                        RagdollShape::Sole { width, height } => RagdollShapeFile::Sole { width, height },
                    },
                })
                .collect(),
            active: (self.active != RagdollActiveProfile::default()).then(|| active_to_file(&self.active)),
        };
        ron::ser::to_string_pretty(&file, regia_core::ron_pretty())
            .map_err(|e| RagdollFileError::Parse(e.to_string()))
    }
}

/// Migration steps registered for `.ragdoll.ron` (the versions they migrate from): 1 → 2, 2 → 3 and 3 → 4.
pub fn ragdoll_migration_sources() -> Vec<u32> {
    vec![1, 2, 3]
}

/// `q` as a unit quaternion; one that already is one is kept bit for bit (see `unit_forward`).
fn unit_rotation(q: Quat) -> Option<Quat> {
    let squared = q.length_squared();
    // Components this large overflow the squared length: normalizing them would give NaN.
    if !q.is_finite() || !squared.is_finite() || squared <= 1e-12 {
        return None;
    }
    if (squared - 1.0).abs() <= 4.0 * f32::EPSILON {
        Some(q)
    } else {
        Some(q.normalize())
    }
}

/// The runtime active profile of a file's `active` block, validated against the profile's bones.
fn active_from_file(
    file: RagdollActiveFile,
    bones: &[RagdollBone],
) -> Result<RagdollActiveProfile, RagdollFileError> {
    let bad = |m: String| Err(RagdollFileError::BadActive(m));
    let finite_non_negative = |v: f32| v.is_finite() && v >= 0.0;
    let unit_range = |v: f32| v.is_finite() && (0.0..=1.0).contains(&v);
    let mut motors: Vec<RagdollMotor> = Vec::with_capacity(file.motors.len());
    for (index, m) in file.motors.into_iter().enumerate() {
        if !bones.iter().any(|b| b.joint == m.joint) {
            return bad(format!(
                "motor {index} names joint {:?}, which has no bone",
                m.joint
            ));
        }
        if motors.iter().any(|o| o.joint == m.joint) {
            return bad(format!("motor {index}: joint {:?} has two motors", m.joint));
        }
        if !(finite_non_negative(m.kp) && finite_non_negative(m.kd)) {
            return bad(format!(
                "motor {index} ({:?}): kp and kd must be non-negative numbers",
                m.joint
            ));
        }
        if !(m.max_torque.is_finite() && m.max_torque > 0.0) {
            return bad(format!(
                "motor {index} ({:?}): max_torque must be a positive number",
                m.joint
            ));
        }
        motors.push(RagdollMotor {
            joint: m.joint,
            group: match m.group {
                RagdollGroupFile::Spine => RagdollGroup::Spine,
                RagdollGroupFile::Arms => RagdollGroup::Arms,
                RagdollGroupFile::Legs => RagdollGroup::Legs,
            },
            kp: m.kp,
            kd: m.kd,
            max_torque: m.max_torque,
            balance: match m.balance {
                RagdollBalanceRoleFile::None => RagdollBalanceRole::None,
                RagdollBalanceRoleFile::Hip => RagdollBalanceRole::Hip,
                RagdollBalanceRoleFile::Ankle => RagdollBalanceRole::Ankle,
            },
        });
    }
    let s = file.strength;
    if !(unit_range(s.spine) && unit_range(s.arms) && unit_range(s.legs)) {
        return bad("strength values must be numbers in 0..=1".into());
    }
    let h = file.hit;
    if !(h.full_impulse.is_finite() && h.full_impulse > 0.0 && finite_non_negative(h.recover_time)) {
        return bad("hit: full_impulse must be positive and recover_time non-negative".into());
    }
    let b = file.balance;
    if !(b.cd.is_finite()
        && b.cv.is_finite()
        && b.max_tilt.is_finite()
        && (0.0..=1.5).contains(&b.max_tilt)
        && finite_non_negative(b.fall_margin)
        && finite_non_negative(b.fall_time)
        && unit_range(b.fall_strength))
    {
        return bad("balance: cd and cv must be numbers, max_tilt in 0..=1.5, fall_margin and fall_time non-negative, fall_strength in 0..=1".into());
    }
    let mut get_up = Vec::with_capacity(file.get_up.len());
    for (index, g) in file.get_up.into_iter().enumerate() {
        if g.clip.is_empty() {
            return bad(format!("get_up {index}: the clip name is empty"));
        }
        let mut pose = Vec::with_capacity(g.pose.len());
        for key in g.pose {
            let (x, y, z, w) = key.rotation;
            let Some(rotation) = unit_rotation(Quat::from_xyzw(x, y, z, w)) else {
                return bad(format!(
                    "get_up {index} ({:?}): rotation of {:?} is not a usable quaternion",
                    g.clip, key.joint
                ));
            };
            pose.push((key.joint, rotation));
        }
        get_up.push(RagdollGetUp {
            clip: g.clip,
            facing: match g.facing {
                RagdollFacingFile::FaceUp => RagdollFacing::FaceUp,
                RagdollFacingFile::FaceDown => RagdollFacing::FaceDown,
                RagdollFacingFile::SideLeft => RagdollFacing::SideLeft,
                RagdollFacingFile::SideRight => RagdollFacing::SideRight,
            },
            pose,
        });
    }
    Ok(RagdollActiveProfile {
        motors,
        strength: RagdollGroupStrength {
            spine: s.spine,
            arms: s.arms,
            legs: s.legs,
        },
        hit: RagdollHitResponse {
            full_impulse: h.full_impulse,
            recover_time: h.recover_time,
        },
        balance: RagdollBalance {
            cd: b.cd,
            cv: b.cv,
            max_tilt: b.max_tilt,
            fall_margin: b.fall_margin,
            fall_time: b.fall_time,
            fall_strength: b.fall_strength,
        },
        get_up,
        recovery: match file.recovery {
            None => super::RagdollRecovery::default(),
            Some(r) => {
                let r = super::RagdollRecovery::from(r);
                if !r.valid() {
                    return bad(
                        "recovery: max_steps at most 16, lengths and times non-negative numbers, pre_bend in 0..=0.5".into(),
                    );
                }
                r
            }
        },
    })
}

/// The file form of an active profile.
fn active_to_file(active: &RagdollActiveProfile) -> RagdollActiveFile {
    RagdollActiveFile {
        motors: active
            .motors
            .iter()
            .map(|m| RagdollMotorFile {
                joint: m.joint.clone(),
                group: match m.group {
                    RagdollGroup::Spine => RagdollGroupFile::Spine,
                    RagdollGroup::Arms => RagdollGroupFile::Arms,
                    RagdollGroup::Legs => RagdollGroupFile::Legs,
                },
                kp: m.kp,
                kd: m.kd,
                max_torque: m.max_torque,
                balance: match m.balance {
                    RagdollBalanceRole::None => RagdollBalanceRoleFile::None,
                    RagdollBalanceRole::Hip => RagdollBalanceRoleFile::Hip,
                    RagdollBalanceRole::Ankle => RagdollBalanceRoleFile::Ankle,
                },
            })
            .collect(),
        strength: RagdollStrengthFile {
            spine: active.strength.spine,
            arms: active.strength.arms,
            legs: active.strength.legs,
        },
        hit: RagdollHitFile {
            full_impulse: active.hit.full_impulse,
            recover_time: active.hit.recover_time,
        },
        balance: RagdollBalanceFile {
            cd: active.balance.cd,
            cv: active.balance.cv,
            max_tilt: active.balance.max_tilt,
            fall_margin: active.balance.fall_margin,
            fall_time: active.balance.fall_time,
            fall_strength: active.balance.fall_strength,
        },
        get_up: active
            .get_up
            .iter()
            .map(|g| RagdollGetUpFile {
                clip: g.clip.clone(),
                facing: match g.facing {
                    RagdollFacing::FaceUp => RagdollFacingFile::FaceUp,
                    RagdollFacing::FaceDown => RagdollFacingFile::FaceDown,
                    RagdollFacing::SideLeft => RagdollFacingFile::SideLeft,
                    RagdollFacing::SideRight => RagdollFacingFile::SideRight,
                },
                pose: g
                    .pose
                    .iter()
                    .map(|(joint, q)| RagdollKeyRotationFile {
                        joint: joint.clone(),
                        rotation: (q.x, q.y, q.z, q.w),
                    })
                    .collect(),
            })
            .collect(),
        recovery: (active.recovery != super::RagdollRecovery::default()).then(|| active.recovery.into()),
    }
}

/// Rewrites a `.ragdoll.ron` text of any supported version as the current version. v1 → v2
/// only adds `shape: Capsule` to every bone; v2 → 3 only changes the version (no `active` block:
/// zero gains), and so does v3 → 4 (no `recovery` block: recovery off). The profile loads to the same bones.
pub fn migrate_ragdoll(text: &str) -> Result<String, RagdollFileError> {
    RagdollProfile::from_ron(text)?.to_ron()
}

/// The serde shape fingerprint of `.ragdoll.ron` (hashed by `cargo xtask check-formats`).
pub fn ragdoll_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<RagdollProfileFile>(),
        serde_field_names::<RagdollBoneFile>(),
        serde_field_names::<RagdollLimitsFile>(),
        serde_variant_names::<RagdollFlexFile>(),
        serde_variant_names::<RagdollShapeFile>(),
        serde_field_names::<RagdollActiveFile>(),
        serde_field_names::<RagdollMotorFile>(),
        serde_variant_names::<RagdollGroupFile>(),
        serde_variant_names::<RagdollBalanceRoleFile>(),
        serde_field_names::<RagdollStrengthFile>(),
        serde_field_names::<RagdollHitFile>(),
        serde_field_names::<RagdollBalanceFile>(),
        serde_variant_names::<RagdollFacingFile>(),
        serde_field_names::<RagdollGetUpFile>(),
        serde_field_names::<RagdollKeyRotationFile>(),
        serde_field_names::<RagdollRecoveryFile>(),
    ])
}

/// Humanoid joint limits by role (rad; spec 93 § 2.3), rounded from clinical range-of-motion
/// tables (Norkin & White, *Measurement of Joint Motion*): `(flex, bend, side, twist)`. Hinge
/// joints (elbow, knee) keep some side and twist slack: a near-zero range fights floor contacts
/// and the limb never comes to rest.
pub(super) fn humanoid_limits(role: regia_animation::retarget::HumanoidBone) -> Option<RagdollLimits> {
    use regia_animation::retarget::HumanoidBone as B;
    let limits = |flex, bend, side, twist| {
        Some(RagdollLimits {
            flex,
            bend,
            side,
            twist,
        })
    };
    match role {
        B::Spine | B::Chest => limits(RagdollFlex::Forward, (-0.35, 0.7), (-0.35, 0.35), (-0.5, 0.5)),
        B::Head => limits(RagdollFlex::Forward, (-0.6, 0.8), (-0.6, 0.6), (-1.0, 1.0)),
        B::LeftUpperArm | B::RightUpperArm => {
            limits(RagdollFlex::Forward, (-0.8, 1.6), (-1.5, 1.5), (-1.0, 1.0))
        }
        B::LeftLowerArm | B::RightLowerArm => {
            limits(RagdollFlex::Forward, (0.0, 2.4), (-0.35, 0.35), (-1.2, 1.2))
        }
        B::LeftUpperLeg | B::RightUpperLeg => {
            limits(RagdollFlex::Forward, (-0.4, 1.8), (-0.7, 0.7), (-0.6, 0.6))
        }
        B::LeftLowerLeg | B::RightLowerLeg => {
            limits(RagdollFlex::Backward, (0.0, 2.3), (-0.15, 0.15), (-0.3, 0.3))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ADR-1917 fuzz finding: normalizing a unit vector again moves it by an ulp, so each load and save of a
    /// diagonal `forward` changed the file.
    #[test]
    fn loading_and_saving_a_diagonal_forward_is_stable() {
        let text = "(format_version: 1, forward: (-1.0, 0.0, -1.0), \
                    bones: [(joint: \"hips\", radius: 0.1, mass: 1.0, length: None, limits: None)])";
        let first = RagdollProfile::from_ron(text)
            .expect("loads")
            .to_ron()
            .expect("saves");
        let second = RagdollProfile::from_ron(&first)
            .expect("loads")
            .to_ron()
            .expect("saves");
        assert_eq!(first, second);
        // Not a unit vector, or not a usable one: normalized, or the default, as before.
        assert_eq!(unit_forward(Vec3::new(0.0, 0.0, -2.0)), Vec3::NEG_Z);
        assert_eq!(unit_forward(Vec3::ZERO), Vec3::NEG_Z);
    }

    fn sample() -> RagdollProfile {
        RagdollProfile {
            bones: vec![
                RagdollBone {
                    joint: "hips".into(),
                    radius: 0.12,
                    mass: 10.6,
                    length: None,
                    limits: None,
                    shape: RagdollShape::Capsule,
                },
                RagdollBone {
                    joint: "chest".into(),
                    radius: 0.14,
                    mass: 26.6,
                    length: Some(0.35),
                    limits: None,
                    shape: RagdollShape::Box {
                        width: 0.36,
                        depth: 0.22,
                    },
                },
                RagdollBone {
                    joint: "head".into(),
                    radius: 0.1,
                    mass: 6.1,
                    length: None,
                    limits: None,
                    shape: RagdollShape::Sphere,
                },
                RagdollBone {
                    joint: "calf_l".into(),
                    radius: 0.05,
                    mass: 4.6,
                    length: Some(0.4),
                    limits: Some(RagdollLimits {
                        flex: RagdollFlex::Backward,
                        bend: (0.0, 2.3),
                        side: (-0.05, 0.05),
                        twist: (-0.15, 0.15),
                    }),
                    shape: RagdollShape::Capsule,
                },
            ],
            forward: Vec3::NEG_Z,
            active: RagdollActiveProfile::default(),
        }
    }

    fn sample_active() -> RagdollProfile {
        let mut profile = sample();
        profile.active = RagdollActiveProfile {
            motors: vec![
                RagdollMotor {
                    joint: "chest".into(),
                    group: RagdollGroup::Spine,
                    kp: 300.0,
                    kd: 30.0,
                    max_torque: 400.0,
                    balance: RagdollBalanceRole::None,
                },
                RagdollMotor {
                    joint: "calf_l".into(),
                    group: RagdollGroup::Legs,
                    kp: 500.0,
                    kd: 40.0,
                    max_torque: 600.0,
                    balance: RagdollBalanceRole::Ankle,
                },
            ],
            strength: RagdollGroupStrength {
                spine: 1.0,
                arms: 0.5,
                legs: 0.75,
            },
            hit: RagdollHitResponse {
                full_impulse: 150.0,
                recover_time: 0.8,
            },
            balance: RagdollBalance {
                cd: 1.5,
                ..RagdollBalance::default()
            },
            get_up: vec![RagdollGetUp {
                clip: "getup_back".into(),
                facing: RagdollFacing::FaceUp,
                pose: vec![("chest".into(), Quat::from_rotation_x(0.3))],
            }],
            recovery: super::super::RagdollRecovery::default(),
        };
        profile
    }

    /// v3: the active block round-trips; v2 text loads with zero gains and migrates to v3 with no active block.
    #[test]
    fn the_active_block_round_trips_and_v2_loads_with_zero_gains() {
        let profile = sample_active();
        let text = profile.to_ron().expect("write");
        assert!(text.contains("active: Some("), "{text}");
        assert!(text.contains("balance: Ankle"), "{text}");
        let back = RagdollProfile::from_ron(&text).expect("read");
        assert_eq!(back, profile);
        assert_eq!(back.to_ron().expect("write again"), text, "stable text");
        let v2 = sample()
            .to_ron()
            .expect("write")
            .replace("format_version: 4", "format_version: 2");
        let loaded = RagdollProfile::from_ron(&v2).expect("v2 loads");
        assert_eq!(loaded.active, RagdollActiveProfile::default());
        assert!(!loaded.active.drives(), "v2 files are passive");
        let migrated = migrate_ragdoll(&v2).expect("v2 migrates");
        assert!(
            migrated.contains("format_version: 4") && !migrated.contains("active"),
            "{migrated}"
        );
        // Missing sub-blocks take their defaults.
        let minimal = r#"(format_version: 3, forward: (0.0, 0.0, -1.0), bones: [
            (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None),
            (joint: "chest", radius: 0.14, mass: 26.6, length: None, limits: None),
        ], active: Some((motors: [(joint: "chest", group: Spine, kp: 100.0, kd: 10.0, max_torque: 200.0)])))"#;
        let loaded = RagdollProfile::from_ron(minimal).expect("minimal v3");
        assert_eq!(loaded.active.strength, RagdollGroupStrength::default());
        assert_eq!(loaded.active.balance, RagdollBalance::default());
        assert_eq!(loaded.active.motors[0].balance, RagdollBalanceRole::None);
        assert!(loaded.active.drives());
    }

    /// Found by the `ragdoll` fuzz target: components so large their squared length overflows normalized to NaN, so the
    /// file loaded and its re-saved text did not.
    #[test]
    fn a_quaternion_too_large_to_normalize_is_refused() {
        assert_eq!(unit_rotation(Quat::from_xyzw(1e30, 0.0, 0.0, 1e30)), None);
        assert_eq!(unit_rotation(Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)), None);
        let large = unit_rotation(Quat::from_xyzw(0.0, 0.0, 0.0, 1e10)).expect("still normalizes");
        assert_eq!(large, Quat::IDENTITY);
    }

    #[test]
    fn a_bad_active_block_has_its_code() {
        type Edit = (&'static str, fn(&mut RagdollActiveProfile));
        let edits: [Edit; 8] = [
            ("motor on a joint with no bone", |a| {
                a.motors[0].joint = "knee".into()
            }),
            ("two motors on one joint", |a| a.motors[1].joint = "chest".into()),
            ("negative kp", |a| a.motors[0].kp = -1.0),
            ("zero max torque", |a| a.motors[0].max_torque = 0.0),
            ("strength above 1", |a| a.strength.arms = 1.5),
            ("zero full impulse", |a| a.hit.full_impulse = 0.0),
            ("empty clip", |a| a.get_up[0].clip.clear()),
            ("zero quaternion", |a| {
                a.get_up[0].pose[0].1 = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)
            }),
        ];
        for (what, edit) in edits {
            let mut profile = sample_active();
            edit(&mut profile.active);
            let text = profile.to_ron().expect("write");
            let err = RagdollProfile::from_ron(&text).expect_err(what);
            assert_eq!(err.code(), "ragdoll_bad_active", "{what}: {err}");
        }
    }

    #[test]
    fn profile_file_round_trips() {
        let profile = sample();
        let text = profile.to_ron().expect("write");
        assert!(text.contains("format_version: 4"), "{text}");
        assert!(text.contains("shape: Sphere"), "{text}");
        assert!(
            !text.contains("active"),
            "zero gains write no active block: {text}"
        );
        assert_eq!(RagdollProfile::from_ron(&text).expect("read"), profile);
    }

    #[test]
    fn newer_version_is_rejected_with_its_code() {
        let text = sample()
            .to_ron()
            .expect("write")
            .replace("format_version: 4", "format_version: 5");
        let err = RagdollProfile::from_ron(&text).expect_err("future version");
        assert_eq!(err.code(), "ragdoll_format_version");
        assert!(matches!(
            err,
            RagdollFileError::Version {
                found: 5,
                expected: 4
            }
        ));
    }

    /// v4 (ADR-2963): a `Sole` foot and a `recovery` block round-trip; a v3 file loads with recovery off and migrates to
    /// v4 with no `recovery` block; bad recovery numbers and sole extents are refused with their codes.
    #[test]
    fn v4_sole_and_recovery_round_trip_and_v3_migrates() {
        let mut profile = sample_active();
        profile.bones[0].shape = RagdollShape::Sole {
            width: 0.1,
            height: 0.06,
        };
        profile.active.recovery = super::super::RagdollRecovery {
            max_steps: 2,
            ..super::super::RagdollRecovery::full()
        };
        let text = profile.to_ron().expect("write");
        assert!(
            text.contains("Sole(") && text.contains("recovery: Some("),
            "{text}"
        );
        let back = RagdollProfile::from_ron(&text).expect("read");
        assert_eq!(back, profile);
        assert_eq!(back.to_ron().expect("write again"), text, "stable text");
        // An empty recovery block is `full()`.
        let empty = text.replace(
            &text[text.find("recovery: Some(").expect("block")..],
            "recovery: Some(()))))",
        );
        let loaded = RagdollProfile::from_ron(&empty).expect("empty block");
        assert_eq!(loaded.active.recovery, super::super::RagdollRecovery::full());
        // v3: no recovery block, recovery off; the migrator writes v4 and adds none.
        let mut v3_profile = sample_active();
        v3_profile.active.recovery = super::super::RagdollRecovery::default();
        let v3 = v3_profile
            .to_ron()
            .expect("write")
            .replace("format_version: 4", "format_version: 3");
        let loaded = RagdollProfile::from_ron(&v3).expect("v3 loads");
        assert_eq!(loaded.active.recovery, super::super::RagdollRecovery::default());
        let migrated = migrate_ragdoll(&v3).expect("v3 migrates");
        assert!(
            migrated.contains("format_version: 4") && !migrated.contains("recovery"),
            "{migrated}"
        );
        let bad = text.replace("max_steps: 2", "max_steps: 40");
        assert_eq!(
            RagdollProfile::from_ron(&bad).expect_err("bad").code(),
            "ragdoll_bad_active"
        );
        let bad = text.replace("width: 0.1,", "width: -0.1,");
        assert_eq!(
            RagdollProfile::from_ron(&bad).expect_err("bad").code(),
            "ragdoll_bad_bone"
        );
    }

    #[test]
    fn unknown_fields_are_parse_errors() {
        let text = sample()
            .to_ron()
            .expect("write")
            .replace("radius: 0.12", "radius: 0.12, radious: 1.0");
        assert_eq!(
            RagdollProfile::from_ron(&text).expect_err("typo").code(),
            "ragdoll_parse"
        );
    }

    #[test]
    fn bad_bones_and_bad_text_have_codes() {
        let text = sample()
            .to_ron()
            .expect("write")
            .replace("radius: 0.05", "radius: -1.0");
        assert_eq!(
            RagdollProfile::from_ron(&text).expect_err("bad radius").code(),
            "ragdoll_bad_bone"
        );
        assert_eq!(
            RagdollProfile::from_ron("(nope").expect_err("bad text").code(),
            "ragdoll_parse"
        );
    }

    #[test]
    fn the_spec_example_parses() {
        let text = r#"(
            format_version: 1,
            forward: (0.0, 0.0, -1.0),
            bones: [
                (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None),
                (joint: "calf_l", radius: 0.05, mass: 4.6, length: None,
                 limits: Some((flex: Backward, bend: (0.0, 2.3), side: (-0.05, 0.05), twist: (-0.15, 0.15)))),
            ],
        )"#;
        let profile = RagdollProfile::from_ron(text).expect("spec 93 § 3 example");
        assert_eq!(profile.bones.len(), 2);
        assert_eq!(
            profile.bones[1].limits.map(|l| l.flex),
            Some(RagdollFlex::Backward)
        );
    }

    /// A v1 file (no `shape`) loads with every bone a capsule; the migrator writes v2 with the
    /// same bones, and migrating that again changes nothing.
    #[test]
    fn a_v1_file_loads_as_capsules_and_migrates_to_v2() {
        let v1 = r#"(format_version: 1, forward: (0.0, 0.0, -1.0), bones: [
            (joint: "hips", radius: 0.12, mass: 10.6, length: None, limits: None),
            (joint: "head", radius: 0.1, mass: 6.1, length: Some(0.25), limits: None),
        ])"#;
        let profile = RagdollProfile::from_ron(v1).expect("v1 loads");
        assert!(profile.bones.iter().all(|b| b.shape == RagdollShape::Capsule));
        let v4 = migrate_ragdoll(v1).expect("migrates");
        assert!(v4.contains("format_version: 4"), "{v4}");
        assert!(v4.contains("shape: Capsule"), "{v4}");
        assert_eq!(RagdollProfile::from_ron(&v4).expect("v4 loads"), profile);
        assert_eq!(migrate_ragdoll(&v4).expect("idempotent"), v4);
        assert_eq!(ragdoll_migration_sources(), vec![1, 2, 3]);
        assert_eq!(
            migrate_ragdoll("(nope").expect_err("bad text").code(),
            "ragdoll_parse"
        );
    }

    #[test]
    fn a_box_needs_a_positive_width_and_depth() {
        let text = sample().to_ron().expect("write");
        for bad in ["width: 0.0", "width: -0.36"] {
            let edited = text.replace("width: 0.36", bad);
            assert_ne!(edited, text, "the sample writes the box width");
            let err = RagdollProfile::from_ron(&edited).expect_err(bad);
            assert_eq!(err.code(), "ragdoll_bad_bone", "{bad}");
        }
        let edited = text.replace("depth: 0.22", "depth: 0.0");
        assert_eq!(
            RagdollProfile::from_ron(&edited).expect_err("zero depth").code(),
            "ragdoll_bad_bone"
        );
    }

    #[test]
    fn shape_extents_and_leaf_lengths() {
        assert_eq!(RagdollShape::Capsule.extent(0.1), 0.1);
        assert_eq!(RagdollShape::Sphere.leaf_length(0.1), 0.2);
        assert_eq!(RagdollShape::Capsule.leaf_length(0.1), 0.4);
        let chest = RagdollShape::Box {
            width: 0.36,
            depth: 0.22,
        };
        assert_eq!(chest.extent(0.01), 0.18);
        assert_eq!(chest.leaf_length(0.01), 0.72);
        assert_eq!(
            [
                RagdollShape::Capsule.name(),
                RagdollShape::Sphere.name(),
                chest.name()
            ],
            ["capsule", "sphere", "box"]
        );
    }
}
