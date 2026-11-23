// SPDX-License-Identifier: MPL-2.0
//! Ragdolls: physics bodies on an animated rig, from hit boxes to a powered hit reaction to a
//! limp fall (`docs/specs/17-animation.md` § 4, `docs/specs/15-physics.md` § 2 joints).
//!
//! A [`Ragdoll`] on a rig entity (one with `SkinnedMesh` + `Animator`) builds one
//! body (a capsule, sphere or box: [`RagdollShape`]) per [`RagdollBone`] the first frame the animator has a pose, connected to its
//! nearest simulated ancestor by a ball joint (contacts between the two disabled with
//! `regia_physics::JointNoContacts`). Then, per [`RagdollMode`]:
//!
//! - **Kinematic**: every body follows the animation (hit boxes, ragdoll ready to go).
//! - **Powered**: the root bone (pelvis) follows the animation; every other body is dynamic
//!   and *tracks* the animated pose by velocity: each fixed step its velocity moves
//!   `strength` of the way towards the velocity that would close the pose error in
//!   `tracking_time`. Gravity, contacts and hits push it off the animation; it recovers.
//!   Only `blend` (and per-bone hit weights) of the simulation shows on the mesh.
//! - **Limp**: every body is dynamic and untracked; the blend ramps to 1 (death, knock-down).
//!
//! A [`RagdollHit`] message applies an impulse to the bone nearest the named joint and
//! raises that bone's — and its descendants' — physics weight to `reaction`, decaying to
//! zero over `recover` seconds: a **partial** blend (the arm that was shot flails, the legs
//! keep walking). The blended pose reaches the joint entities and the skin palette through
//! `regia_animation::PhysicsPoseBlend`; the animator's own pose stays the animation's.
//!
//! **Joint limits** (spec 93 § 2.3, ADR-1693): a bone with [`RagdollBone::limits`] gets a
//! `regia_physics::JointKind::SwingTwist` joint whose frame comes from the bind pose: twist
//! along the bone, bend towards the limb's [`RagdollFlex`] direction (relative to
//! [`RagdollProfile::forward`]), ranges centred on the bind pose. Bones without limits (and
//! every bone while [`RagdollSettings::joint_limits`] is off) get unlimited ball joints.
//! [`RagdollProfile::humanoid_limited`] fills a humanoid table; `.ragdoll.ron` files
//! ([`mod@file`]) author profiles as data.
//!
//! **Settle and budget**: a limp ragdoll whose bodies stay slow for [`Ragdoll::settle_time`]
//! is settled ([`RagdollSettled`], face up or down for the get-up clip). When more ragdolls
//! simulate than [`RagdollSettings::max_simulated`], the ones that went simulated first and
//! have settled are frozen where they lie (kinematic bodies, the mesh keeps the pose).
//!
//! **Mode switches** keep the motion the eye expects:
//!
//! - Going limp (death) **preserves momentum**: a body that stopped being kinematic starts
//!   dynamic with the animated velocity it had on the previous fixed step (a run carries
//!   into the fall). A seed faster than the cap (50 m/s, 50 rad/s) is dropped to zero rather than
//!   clamped: it can only come from a rig teleported in the same frame, which must not fire its
//!   bodies away at the cap either.
//! - Recovering (Limp → Kinematic/Powered, the get-up) teleports every body onto the
//!   animation with a [`Teleport`] marker and zero velocity, moves
//!   the rig origin (x/z + yaw, [`Ragdoll::recover_snap`]) to where the root body lies so a
//!   get-up continues where the character fell (the height stays the animation's business),
//!   and **freezes** the last simulated pose, easing it out over `blend_time` so the mesh
//!   blends from the fallen pose into the animation instead of popping.
//!
//! Provenance (`docs/provenance.md`): blending a simulated ragdoll with motion capture and
//! tracking the animation with a controller after an impact follows Zordan & Hodgins,
//! "Motion capture-driven simulations that hit and react", SCA 2002, and Zordan, Majkowska,
//! Chiu & Fast, "Dynamic response for motion capture animation", SIGGRAPH 2005. Velocity
//! tracking instead of PD torques (unconditionally stable at the fixed step, no inertia
//! tuning) and the per-bone decaying hit weight are Regia Engine's choices. Fyrox's `Ragdoll`
//! node was reviewed for the kinematic/dynamic switch; no code was ported. The momentum
//! hand-off and the frozen-pose recover are Regia Engine's.

use std::sync::Arc;

use regia_animation::retarget::{HumanoidBone, RetargetProfile};
use regia_animation::{Animator, PhysicsPoseBlend, SkinnedMesh};
use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec3, det_yaw_of, det_yaw_quat};
use regia_physics::{
    BodyKind, Collider, ColliderShape, ExternalImpulse, Joint, JointKind, JointNoContacts, Mass, PhysicsPose,
    RigidBody, ShapeAxis, Teleport, Velocity,
};
use regia_scene::Transform;

#[cfg(test)]
thread_local! {
    /// Joint-global composes `drive_ragdolls` ran on this thread (tests run the system directly
    /// on the test thread, so the count is race-free under parallel tests).
    static GLOBALS_COMPOSES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one joint-global compose (tests).
#[cfg(test)]
fn note_compose() {
    GLOBALS_COMPOSES.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
thread_local! {
    /// Bench A/B switch: true turns the steady kinematic skip off on this thread (tests only).
    static STEADY_SKIP_OFF: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub mod active;
pub mod file;
pub mod preview;
pub mod stepping;

pub use active::{
    RagdollActiveProfile, RagdollActiveState, RagdollBalance, RagdollBalanceRole, RagdollFacing,
    RagdollGetUp, RagdollGroup, RagdollGroupStrength, RagdollHitResponse, RagdollMotor, RagdollPhase,
};
pub use stepping::{RagdollFallCause, RagdollRecovery, RagdollStepPhase};

/// One simulated bone: a body (capsule by default) from `joint` towards its child.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollBone {
    /// Skeleton joint the body follows.
    pub joint: String,
    /// Capsule or sphere radius, metres (a box ignores it except as a size hint).
    pub radius: f32,
    /// Body mass, kg.
    pub mass: f32,
    /// Body length along the bone; `None` measures the bind distance to the nearest
    /// simulated (or first) child joint, and leaves (head, hands, feet) use
    /// [`RagdollShape::leaf_length`] along the parent bone's direction.
    pub length: Option<f32>,
    /// Limits of the joint to the nearest simulated ancestor; `None` is an unlimited ball
    /// joint (the root bone has no joint, so its limits are ignored).
    pub limits: Option<RagdollLimits>,
    /// The body's collision shape (`.ragdoll.ron` v2; v1 bones are capsules).
    pub shape: RagdollShape,
}

/// The collision shape of one ragdoll body (ADR-2133 § 6). Every shape sits along the bone,
/// from the joint towards the child, and is `length` long (a sphere is centred halfway).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum RagdollShape {
    /// A capsule of the bone's `radius`, cap tip to cap tip `length` long.
    #[default]
    Capsule,
    /// A sphere of the bone's `radius`, centred halfway along the bone.
    Sphere,
    /// A box `length` long; `width` (side to side) and `depth` (front to back, along the
    /// profile's `forward`) are full extents in metres.
    Box {
        /// Side-to-side extent, metres.
        width: f32,
        /// Front-to-back extent, metres.
        depth: f32,
    },
    /// A flat foot (ADR-2963): a box lying level when the ragdoll is built, `length` long from a quarter of its length
    /// behind the joint forward along the level direction from the joint to its child (the profile's `forward` when
    /// the bone is vertical), `width` across and `height` thick, its underside at the lower of the joint and the child.
    /// Foot bones of imported rigs point down to the toes; a box along such a bone stands on an edge.
    Sole {
        /// Side-to-side extent, metres.
        width: f32,
        /// Thickness, metres.
        height: f32,
    },
}

impl RagdollShape {
    /// The shape's protocol name (`capsule`, `sphere`, `box`, `sole`).
    pub fn name(&self) -> &'static str {
        match self {
            RagdollShape::Capsule => "capsule",
            RagdollShape::Sphere => "sphere",
            RagdollShape::Box { .. } => "box",
            RagdollShape::Sole { .. } => "sole",
        }
    }

    /// Half the largest cross-section extent: the radius of a capsule or sphere, half the
    /// larger side of a box.
    pub fn extent(&self, radius: f32) -> f32 {
        match *self {
            RagdollShape::Capsule | RagdollShape::Sphere => radius,
            RagdollShape::Box { width, depth } => width.max(depth) * 0.5,
            RagdollShape::Sole { width, height } => width.max(height) * 0.5,
        }
    }

    /// The length of a leaf body (no child joint) whose `length` is `None`: four extents for a
    /// capsule or box, one diameter for a sphere (so it touches its joint).
    pub fn leaf_length(&self, radius: f32) -> f32 {
        match self {
            RagdollShape::Sphere => 2.0 * radius,
            _ => 4.0 * self.extent(radius),
        }
    }
}

/// Which way a limb flexes, relative to [`RagdollProfile::forward`] (spec 93 § 2.3).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollFlex {
    /// Towards the character's front (hips, elbows, spine, neck).
    #[default]
    Forward,
    /// Towards the character's back (knees).
    Backward,
}

/// Joint limits of one ragdoll bone, relative to its bind pose (rad; spec 93 § 2.3).
///
/// The joint frame: twist along the bone, bend about `bone × flex direction` (positive bend
/// is flexion), side about the third axis. Each range is `(min, max)`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollLimits {
    /// Which way the limb flexes.
    pub flex: RagdollFlex,
    /// Flexion range (positive = towards the flex direction).
    pub bend: (f32, f32),
    /// Sideways swing range.
    pub side: (f32, f32),
    /// Twist range about the bone.
    pub twist: (f32, f32),
}

/// The bones of a ragdoll.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollProfile {
    /// Simulated bones, any order.
    pub bones: Vec<RagdollBone>,
    /// Rig-space direction the character faces (default `-Z`, the Regia character
    /// convention; many imported rigs face `+Z`). Orients joint limits and the settled
    /// face-up test.
    pub forward: Vec3,
    /// Active-ragdoll motors, strength, balance and get-up clips (`.ragdoll.ron` v3, spec 93 § 10); empty (zero
    /// gains, passive) by default and in v1/v2 files.
    pub active: RagdollActiveProfile,
}

impl Default for RagdollProfile {
    fn default() -> Self {
        RagdollProfile {
            bones: Vec::new(),
            forward: Vec3::NEG_Z,
            active: RagdollActiveProfile::default(),
        }
    }
}

impl RagdollProfile {
    /// [`RagdollProfile::humanoid`] with joint limits for every bone below the hips, from the
    /// humanoid range-of-motion table (spec 93 § 2.3). `forward` is the rig-space direction
    /// the character faces.
    pub fn humanoid_limited(skeleton: &regia_animation::Skeleton, mass: f32, forward: Vec3) -> Self {
        let roles = RetargetProfile::guess_humanoid(skeleton);
        let mut profile = Self::humanoid(skeleton, mass);
        profile.forward = forward.normalize_or(Vec3::NEG_Z);
        for bone in &mut profile.bones {
            let role = HumanoidBone::ALL
                .iter()
                .copied()
                .find(|role| roles.roles.get(role.name()) == Some(&bone.joint));
            bone.limits = role.and_then(file::humanoid_limits);
        }
        profile
    }

    /// An 11-bone humanoid (hips, chest, head, upper/lower arms, upper/lower legs) found by
    /// [`RetargetProfile::guess_humanoid`]; missing roles are skipped. Masses follow a 75 kg
    /// body (segment fractions after Winter, *Biomechanics and Motor Control of Human
    /// Movement*, table 4.1) times `mass / 75`.
    pub fn humanoid(skeleton: &regia_animation::Skeleton, mass: f32) -> Self {
        let roles = RetargetProfile::guess_humanoid(skeleton);
        let k = mass / 75.0;
        let chest = if roles.roles.contains_key(HumanoidBone::Chest.name()) {
            HumanoidBone::Chest
        } else {
            HumanoidBone::Spine
        };
        let table = [
            (HumanoidBone::Hips, 0.12, 0.142),
            (chest, 0.14, 0.355),
            (HumanoidBone::Head, 0.10, 0.081),
            (HumanoidBone::LeftUpperArm, 0.05, 0.028),
            (HumanoidBone::LeftLowerArm, 0.04, 0.022),
            (HumanoidBone::RightUpperArm, 0.05, 0.028),
            (HumanoidBone::RightLowerArm, 0.04, 0.022),
            (HumanoidBone::LeftUpperLeg, 0.07, 0.100),
            (HumanoidBone::LeftLowerLeg, 0.05, 0.061),
            (HumanoidBone::RightUpperLeg, 0.07, 0.100),
            (HumanoidBone::RightLowerLeg, 0.05, 0.061),
        ];
        RagdollProfile {
            bones: table
                .iter()
                .filter_map(|(role, radius, fraction)| {
                    roles.roles.get(role.name()).map(|joint| RagdollBone {
                        joint: joint.clone(),
                        radius: *radius,
                        mass: 75.0 * fraction * k,
                        length: None,
                        limits: None,
                        shape: RagdollShape::Capsule,
                    })
                })
                .collect(),
            forward: Vec3::NEG_Z,
            active: RagdollActiveProfile::default(),
        }
    }
}

impl RagdollProfile {
    /// [`RagdollProfile::humanoid`] plus a [`RagdollShape::Sole`] foot on each foot joint the skeleton has (ADR-2963):
    /// a level box 15 % of the rig's height long, 5.5 % wide and 3.5 % thick (the height is the bind pose's vertical
    /// extent times 1.1, for the skull above the head joint), mass 1.45 % of the body each (Winter, table 4.1). A rig
    /// without foot joints gets exactly [`RagdollProfile::humanoid`].
    pub fn humanoid_with_feet(skeleton: &regia_animation::Skeleton, mass: f32) -> Self {
        let roles = RetargetProfile::guess_humanoid(skeleton);
        let mut profile = Self::humanoid(skeleton, mass);
        let stature = rig_stature(skeleton);
        for role in [HumanoidBone::LeftFoot, HumanoidBone::RightFoot] {
            if let Some(joint) = roles.roles.get(role.name()) {
                profile.bones.push(RagdollBone {
                    joint: joint.clone(),
                    radius: 0.02 * stature,
                    mass: 0.0145 * mass,
                    length: Some(0.15 * stature),
                    limits: None,
                    shape: RagdollShape::Sole {
                        width: 0.055 * stature,
                        height: 0.035 * stature,
                    },
                });
            }
        }
        profile
    }

    /// An active humanoid (ADR-2963): [`RagdollProfile::humanoid_with_feet`] facing `forward` (rig space) without arm
    /// bodies (their mass goes to the chest: on rigs whose shoulders start at the chest joint a chest body would point
    /// at a shoulder), motors on every body with the gains of the tested biped scaled by `mass / 67` (hips and knees
    /// `kp` 3600, `kd` 12; ankles, chest and head 2700 and 9), balance roles on the hips and the feet, and `recovery`.
    /// Without foot joints there is no ankle and no balance: the rig stands only while nothing pushes it.
    pub fn humanoid_active(
        skeleton: &regia_animation::Skeleton,
        mass: f32,
        forward: Vec3,
        recovery: RagdollRecovery,
    ) -> Self {
        let roles = RetargetProfile::guess_humanoid(skeleton);
        let mut profile = Self::humanoid_with_feet(skeleton, mass);
        profile.forward = forward.normalize_or(Vec3::NEG_Z);
        let k = if mass.is_finite() && mass > 0.0 {
            mass / 67.0
        } else {
            1.0
        };
        let role_of = |joint: &str| {
            HumanoidBone::ALL
                .iter()
                .copied()
                .find(|role| roles.roles.get(role.name()).is_some_and(|j| j == joint))
        };
        use HumanoidBone as B;
        let arm = |role: Option<HumanoidBone>| {
            matches!(
                role,
                Some(
                    B::LeftUpperArm
                        | B::RightUpperArm
                        | B::LeftLowerArm
                        | B::RightLowerArm
                        | B::LeftHand
                        | B::RightHand
                        | B::LeftShoulder
                        | B::RightShoulder
                )
            )
        };
        let arm_mass: f32 = profile
            .bones
            .iter()
            .filter(|b| arm(role_of(&b.joint)))
            .map(|b| b.mass)
            .sum();
        profile.bones.retain(|b| !arm(role_of(&b.joint)));
        if let Some(chest) = profile
            .bones
            .iter_mut()
            .find(|b| matches!(role_of(&b.joint), Some(B::Chest | B::Spine | B::UpperChest)))
        {
            chest.mass += arm_mass;
        }
        let mut motors = Vec::new();
        for bone in &profile.bones {
            let (group, kp, kd, balance) = match role_of(&bone.joint) {
                Some(B::Hips) | None => continue,
                Some(B::LeftUpperLeg | B::RightUpperLeg) => {
                    (RagdollGroup::Legs, 3600.0, 12.0, RagdollBalanceRole::Hip)
                }
                Some(B::LeftLowerLeg | B::RightLowerLeg) => {
                    (RagdollGroup::Legs, 3600.0, 12.0, RagdollBalanceRole::None)
                }
                Some(B::LeftFoot | B::RightFoot) => {
                    (RagdollGroup::Legs, 2700.0, 9.0, RagdollBalanceRole::Ankle)
                }
                Some(_) => (RagdollGroup::Spine, 2700.0, 9.0, RagdollBalanceRole::None),
            };
            motors.push(RagdollMotor {
                joint: bone.joint.clone(),
                group,
                kp: kp * k,
                kd: kd * k,
                max_torque: 600.0 * k,
                balance,
            });
        }
        profile.active.motors = motors;
        profile.active.recovery = recovery;
        profile
    }
}

/// The rig's height for proportions: the bind pose's vertical joint extent times 1.1 (the skull above the head joint),
/// at least 0.1 m.
fn rig_stature(skeleton: &regia_animation::Skeleton) -> f32 {
    let (low, high) = skeleton
        .bind_globals()
        .iter()
        .map(|m| m.w_axis.y)
        .filter(|y| y.is_finite())
        .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), y| {
            (lo.min(y), hi.max(y))
        });
    let extent = (high - low) * 1.1;
    if extent.is_finite() { extent.max(0.1) } else { 1.75 }
}

/// How a ragdoll's bodies move (module docs).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollMode {
    /// Every body follows the animation.
    #[default]
    Kinematic,
    /// The root bone follows the animation; the rest track it by velocity.
    Powered,
    /// Every body is dynamic and untracked; the blend ramps to 1.
    Limp,
    /// Every body is dynamic; the profile's PD motors drive each joint towards the animated pose and the balance
    /// controller keeps it standing (spec 93 § 10, [`active`]). The blend ramps to 1. With no driving motor it
    /// behaves exactly like `Limp`.
    Active,
}

/// A ragdoll on a rig entity (module docs). Runtime state (the bodies) lives in
/// [`RagdollBodies`], added when the ragdoll is built.
#[derive(Component, Clone, Debug)]
pub struct Ragdoll {
    /// The simulated bones.
    pub profile: RagdollProfile,
    /// How the bodies move.
    pub mode: RagdollMode,
    /// Physics weight shown on every simulated bone in `Powered` mode (`Limp` uses 1,
    /// `Kinematic` 0). Hit reactions add per-bone weight on top.
    pub blend: f32,
    /// Seconds for the shown weight to reach a new target.
    pub blend_time: f32,
    /// `Powered`: fraction of the tracking velocity mixed into each body per fixed step
    /// (`0..=1`; higher holds the pose harder).
    pub strength: f32,
    /// `Powered`: seconds in which the tracking velocity would close the pose error.
    pub tracking_time: f32,
    /// Limp → Kinematic/Powered (the get-up): move the rig origin so the animation's root
    /// lands where the fallen root body lies (x/z and yaw; the height stays the
    /// animation's, so a get-up clip rises by itself). `false` keeps the rig where it was.
    pub recover_snap: bool,
    /// `Limp`: every body slower than this (m/s) counts as at rest. Spin is not counted: a
    /// capsule rocking in place barely moves the pose, and settling damps it out.
    pub settle_speed: f32,
    /// `Limp`: seconds at rest (counted in fixed steps) before the ragdoll is settled.
    pub settle_time: f32,
}

impl Ragdoll {
    /// A kinematic ragdoll with the given bones and default tuning.
    pub fn new(profile: RagdollProfile) -> Self {
        Ragdoll {
            profile,
            mode: RagdollMode::Kinematic,
            blend: 0.0,
            blend_time: 0.25,
            strength: 0.35,
            tracking_time: 0.08,
            recover_snap: true,
            settle_speed: 0.25,
            settle_time: 0.5,
        }
    }
}

/// One built body.
#[derive(Clone, Debug)]
pub struct RagdollBody {
    /// Skeleton joint.
    pub joint: u32,
    /// The body entity.
    pub entity: Entity,
    /// Index of the nearest simulated ancestor in [`RagdollBodies::bodies`].
    pub parent: Option<usize>,
    /// Current hit-reaction weight.
    pub hit: f32,
    /// Hit weight lost per second.
    pub hit_decay: f32,
}

/// The built bodies of a [`Ragdoll`] (runtime; added by the build system).
#[derive(Component, Clone, Debug, Default)]
pub struct RagdollBodies {
    /// Bodies, parents before children.
    pub bodies: Vec<RagdollBody>,
    /// The weight currently shown on every bone (eases towards the mode's target).
    pub weight: f32,
    /// The weight of the frozen recover pose, easing to zero over `blend_time` while it
    /// holds the last simulated pose over the animation (module docs).
    frozen_weight: f32,
    /// World-space poses captured when a limp ragdoll recovered, one per body; read while
    /// `frozen_weight > 0`.
    frozen: Vec<Option<(Vec3, Quat)>>,
    /// The animated world pose each body followed on the previous fixed step while it was
    /// kinematic; seeds its velocity when it turns dynamic (momentum on death).
    prev_targets: Vec<Option<(Vec3, Quat)>>,
    /// The mode the bodies' `RigidBody` kinds were last set for.
    applied: Option<RagdollMode>,
    /// The root body's forward axis in its local space (the profile's `forward` at build).
    root_forward: Vec3,
    /// Consecutive fixed steps every body was at rest (limp only).
    still_steps: u32,
    /// Settled: at rest for `settle_time` (limp only).
    settled: bool,
    /// The root body's forward pointed up when the ragdoll settled.
    face_up: bool,
    /// The fixed step at which the ragdoll last went simulated (Powered or Limp).
    simulated_since: Option<u64>,
    /// Frozen by the budget: kinematic where it lies until the mode changes.
    budget_frozen: bool,
    /// The root body's up axis in its local space (rig up at build; the get-up facing).
    root_up: Vec3,
    /// The active controller's state (spec 93 § 10).
    active: RagdollActiveState,
    /// Reused joint-world matrices of the animated pose, one buffer per rig.
    target_globals: Vec<Mat4>,
    /// What the last kinematic compose read (audit framework4 finding 5): a step whose pose, rig
    /// transform and skeleton match, and whose bodies still sit where it put them, skips the
    /// compose. Kept buffers, rewritten in place.
    steady: KinematicStamp,
}

/// The inputs of a ragdoll's last kinematic compose ([`RagdollBodies`]).
#[derive(Clone, Debug, Default)]
struct KinematicStamp {
    /// Set after a kinematic compose; cleared by any other mode.
    valid: bool,
    /// The evaluated pose composed.
    pose: regia_animation::Pose,
    /// The rig's translation, rotation and scale (bit-equal compare).
    rig: (Vec3, Quat, Vec3),
    /// The skeleton composed against.
    skeleton: Option<Arc<regia_animation::Skeleton>>,
}

impl KinematicStamp {
    /// Records a compose of `pose` on `rig` with `skeleton` (no heap request once warm).
    fn record(
        &mut self,
        pose: &regia_animation::Pose,
        rig: &Transform,
        skeleton: &Arc<regia_animation::Skeleton>,
    ) {
        self.pose.translations.clone_from(&pose.translations);
        self.pose.rotations.clone_from(&pose.rotations);
        self.pose.scales.clone_from(&pose.scales);
        self.rig = (rig.translation, rig.rotation, rig.scale);
        if !self.skeleton.as_ref().is_some_and(|s| Arc::ptr_eq(s, skeleton)) {
            self.skeleton = Some(Arc::clone(skeleton));
        }
        self.valid = true;
    }

    /// Whether a compose of these inputs would rebuild exactly the last one.
    fn matches(
        &self,
        pose: &regia_animation::Pose,
        rig: &Transform,
        skeleton: &Arc<regia_animation::Skeleton>,
    ) -> bool {
        self.valid
            && self.rig == (rig.translation, rig.rotation, rig.scale)
            && self.skeleton.as_ref().is_some_and(|s| Arc::ptr_eq(s, skeleton))
            && self.pose == *pose
    }
}

impl RagdollBodies {
    /// The physics weight shown on body `index` this frame.
    pub fn bone_weight(&self, index: usize) -> f32 {
        let hit = self.bodies.get(index).map_or(0.0, |b| b.hit);
        (self.weight + hit).clamp(0.0, 1.0)
    }

    /// Whether a recover blend is still easing the last simulated pose out.
    pub fn recovering(&self) -> bool {
        self.frozen_weight > 0.0
    }

    /// Whether the limp ragdoll has come to rest ([`RagdollSettled`]).
    pub fn settled(&self) -> bool {
        self.settled
    }

    /// Whether the settled ragdoll lies face up (its front points up); false until settled.
    pub fn face_up(&self) -> bool {
        self.settled && self.face_up
    }

    /// Whether the budget froze this ragdoll ([`RagdollSettings::max_simulated`]).
    pub fn frozen(&self) -> bool {
        self.budget_frozen
    }

    /// Whether the ragdoll counts against the budget: simulated and not frozen.
    pub fn simulating(&self) -> bool {
        self.simulated_since.is_some() && !self.budget_frozen
    }

    /// The active controller's state: phase, group hit strength, center of mass and support, chosen get-up clip
    /// (spec 93 § 10).
    pub fn active(&self) -> &RagdollActiveState {
        &self.active
    }
}

/// A limp ragdoll came to rest (spec 93 § 2.3); written once per fall.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct RagdollSettled {
    /// The rig entity with the [`Ragdoll`].
    pub rig: Entity,
    /// The pelvis' front points up (pick a get-up-from-back clip).
    pub face_up: bool,
    /// World position of the root body.
    pub root_position: Vec3,
}

/// Ragdoll quality switches and budget (spec 93 § 2.3).
#[derive(Resource, Clone, Copy, Debug, PartialEq)]
pub struct RagdollSettings {
    /// Build joint limits (read when a ragdoll's bodies are built); off builds unlimited
    /// ball joints.
    pub joint_limits: bool,
    /// Most ragdolls simulated at once; settled ones beyond it are frozen, oldest first.
    pub max_simulated: usize,
}

impl RagdollSettings {
    /// The defaults for a quality tier: MobileLow 2, MobileHigh 4, Deck 6, Low 8, Medium 12,
    /// High 16, Ultra 24 simulated ragdolls; joint limits on.
    #[cfg(feature = "client")]
    pub fn for_tier(tier: regia_render::QualityTier) -> Self {
        use regia_render::QualityTier as T;
        let max_simulated = match tier {
            T::MobileLow => 2,
            T::MobileHigh => 4,
            T::Deck => 6,
            T::Low | T::VeryLow => 8,
            T::Medium => 12,
            T::High => 16,
            T::Ultra => 24,
        };
        RagdollSettings {
            joint_limits: true,
            max_simulated,
        }
    }
}

impl Default for RagdollSettings {
    /// The High tier's numbers; a dedicated server has no quality tier.
    fn default() -> Self {
        RagdollSettings {
            joint_limits: true,
            max_simulated: 16,
        }
    }
}

/// Whether a body is kinematic in `mode` (the powered ragdoll's root follows the animation,
/// everything else is simulated).
fn mode_kinematic(mode: RagdollMode, parent: Option<usize>) -> bool {
    match mode {
        RagdollMode::Kinematic => true,
        RagdollMode::Powered => parent.is_none(),
        RagdollMode::Limp | RagdollMode::Active => false,
    }
}

/// Whether every body is dynamic in `mode` (`Limp` and `Active`).
fn fully_dynamic(mode: RagdollMode) -> bool {
    matches!(mode, RagdollMode::Limp | RagdollMode::Active)
}

/// The damping of a ragdoll body while it moves.
const BODY_LINEAR_DAMPING: f32 = 0.1;
const BODY_ANGULAR_DAMPING: f32 = 0.5;
/// The damping of a settled ragdoll's bodies: capsules roll on the floor (no rolling
/// friction), so a limb can rock in place for ever; this stops it (spec 93 § 2.3).
const REST_LINEAR_DAMPING: f32 = 2.0;
const REST_ANGULAR_DAMPING: f32 = 8.0;

/// Fastest velocity (m/s, `MAX_SEED_SPIN` rad/s) a kinematic → dynamic switch seeds from the
/// animated motion: a rig teleported in the same frame it goes limp must not fire its
/// bodies away (real motion is far below both). A faster seed is zeroed, not clamped.
const MAX_SEED_SPEED: f32 = 50.0;
const MAX_SEED_SPIN: f32 = 50.0;

/// Hit a ragdoll: an impulse on the bone nearest `joint` plus a decaying partial blend.
#[derive(Message, Clone, Debug, PartialEq)]
pub struct RagdollHit {
    /// The rig entity with the [`Ragdoll`].
    pub rig: Entity,
    /// Joint name; the nearest simulated bone at or above it takes the hit.
    pub joint: String,
    /// World-space impulse, kg·m/s.
    pub impulse: Vec3,
    /// Peak physics weight on the hit bone and its descendants, `0..=1`.
    pub reaction: f32,
    /// Seconds for the reaction weight to decay to zero.
    pub recover: f32,
}

/// Registers the ragdoll message and systems: build and hits/pose sync in `Update` (after
/// animation evaluates), the weight decays and the drive in `FixedUpdate` before the physics step
/// (the decays run per fixed step, so a hitched frame cannot change when a get-up starts). Requires
/// `regia_physics::PhysicsPlugin` and `regia_animation::AnimationPlugin`.
pub struct RagdollPlugin;

impl Plugin for RagdollPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<RagdollMode>()
            .init_resource::<RagdollSettings>()
            .add_message::<RagdollHit>()
            .add_message::<RagdollSettled>()
            .add_systems(
                Update,
                (build_ragdolls, apply_hits, sync_ragdoll_pose)
                    .chain()
                    .after(regia_animation::systems::evaluate_animators),
            )
            .add_systems(
                FixedUpdate,
                (decay_ragdoll_weights, drive_ragdolls)
                    .chain()
                    .before(regia_physics::PhysicsStep),
            );
        // Ragdoll caps follow the active quality tier (ADR-2424). Not in the fixed schedule.
        #[cfg(feature = "client")]
        app.add_systems(PreUpdate, apply_ragdoll_tier);
    }
}

/// Sets [`RagdollSettings`] from the active quality tier (`PreUpdate`, `client` builds): the tier is
/// the preset of the app's `RenderSettings` ([`regia_render::RenderSettings::preset`]), and
/// [`RagdollSettings::for_tier`] gives its caps, so a phone no longer simulates 16 ragdolls.
///
/// It acts when the tier first shows and again whenever it changes, never every frame, so an explicit
/// change to the settings (the agent's `anim.ragdoll_settings`, a game's own tuning) sticks until
/// the tier moves. On the first sight it leaves settings alone that are no longer the default, because
/// a host that inserted its own numbers before the first frame chose them. A profile that matches no
/// preset keeps the last tier's numbers. With no `RenderSettings` (a headless or server app) nothing
/// happens.
#[cfg(feature = "client")]
fn apply_ragdoll_tier(
    render: Option<Res<regia_render::RenderSettings>>,
    mut settings: ResMut<RagdollSettings>,
    mut seen: Local<Option<regia_render::QualityTier>>,
) {
    let Some(preset) = render.and_then(|r| r.preset()) else {
        return;
    };
    let tier = regia_render::QualityTier::of_preset(preset);
    if *seen == Some(tier) {
        return;
    }
    let first = seen.is_none();
    *seen = Some(tier);
    if first && *settings != RagdollSettings::default() {
        return;
    }
    let wanted = RagdollSettings::for_tier(tier);
    if *settings != wanted {
        *settings = wanted;
    }
}

fn rig_matrix(transform: &Transform) -> Mat4 {
    Mat4::from_scale_rotation_translation(transform.scale, transform.rotation, transform.translation)
}

/// `q` scaled to unit length, or `None` when its length is zero or not finite (glam 0.33's `Quat`
/// has no `try_normalize`). It is `Quat::normalize` itself, checked afterwards: `Vec4::try_normalize`
/// multiplies by the reciprocal length and rounds differently, which moved the determinism pins.
fn unit_quat(q: Quat) -> Option<Quat> {
    let unit = q.normalize();
    unit.is_finite().then_some(unit)
}

fn decompose(m: Mat4) -> (Vec3, Quat) {
    let (_, rotation, translation) = m.to_scale_rotation_translation();
    (translation, unit_quat(rotation).unwrap_or(Quat::IDENTITY))
}

/// [`decompose`] when the matrix has a usable rotation: a zero scale axis (a rig or bone scaled to
/// 0) makes the extracted quaternion NaN, which must never reach a body (audit v033b).
fn try_decompose(m: Mat4) -> Option<(Vec3, Quat)> {
    let (_, rotation, translation) = m.to_scale_rotation_translation();
    let rotation = unit_quat(rotation)?;
    (translation.is_finite() && rotation.is_finite()).then_some((translation, rotation))
}

/// A rig scale no pose can be decomposed from: an axis at (or near) zero, or non-finite.
fn degenerate_scale(scale: Vec3) -> bool {
    !scale.is_finite() || scale.abs().min_element() < 1e-6
}

/// Spawn the bodies of every ragdoll that has none yet (once the animator has a pose).
#[tracing::instrument(skip_all, target = "regia_framework")]
fn build_ragdolls(
    mut commands: Commands,
    rigs: Query<(Entity, &Ragdoll, &SkinnedMesh, &Animator, &Transform), Without<RagdollBodies>>,
    settings: Option<Res<RagdollSettings>>,
) {
    let limits_on = settings.is_none_or(|s| s.joint_limits);
    for (rig, ragdoll, binding, animator, transform) in &rigs {
        let skeleton = &binding.skeleton;
        let pose = animator.evaluated_pose();
        // A rig scaled to zero has no body poses yet: build once it is back.
        if pose.len() != skeleton.len() || degenerate_scale(transform.scale) {
            continue;
        }
        let globals = skeleton.globals(pose, Some(rig_matrix(transform)));
        // Joint frames come from the bind pose (rig space), so limits centre on it.
        let bind = skeleton.bind_globals();
        let forward = ragdoll.profile.forward.normalize_or(Vec3::NEG_Z);
        // Resolve bones to joints, parents first (joint ids are topologically sorted).
        let mut bones: Vec<(u32, &RagdollBone)> = ragdoll
            .profile
            .bones
            .iter()
            .filter_map(|b| match skeleton.joint_index(&b.joint) {
                Some(j) => Some((j, b)),
                None => {
                    tracing::warn!(target: "regia_framework", "ragdoll bone {:?} is not a joint of the rig", b.joint);
                    None
                }
            })
            .collect();
        bones.sort_by_key(|(j, _)| *j);
        bones.dedup_by_key(|(j, _)| *j);
        let simulated: Vec<u32> = bones.iter().map(|(j, _)| *j).collect();
        let nearest_ancestor = |joint: u32| -> Option<usize> {
            let mut current = skeleton.parents[joint as usize];
            while let Some(p) = current {
                if let Some(index) = simulated.iter().position(|&s| s == p) {
                    return Some(index);
                }
                current = skeleton.parents[p as usize];
            }
            None
        };

        let mut bodies = Vec::with_capacity(bones.len());
        for (index, &(joint, bone)) in bones.iter().enumerate() {
            let world = globals[joint as usize];
            let (position, rotation) = decompose(world);
            // Bone direction (world): towards the nearest simulated descendant, else the
            // first child joint, else along the parent bone.
            let child = simulated
                .iter()
                .copied()
                .filter(|&c| nearest_ancestor(c) == Some(index))
                .min()
                .or_else(|| {
                    skeleton
                        .parents
                        .iter()
                        .position(|p| *p == Some(joint))
                        .map(|c| c as u32)
                });
            let parent_position =
                skeleton.parents[joint as usize].map(|p| globals[p as usize].w_axis.truncate());
            let (direction, measured) = match child {
                Some(c) => {
                    let d = globals[c as usize].w_axis.truncate() - position;
                    (d.normalize_or(Vec3::Y), d.length())
                }
                None => (
                    parent_position.map_or(Vec3::Y, |p| (position - p).normalize_or(Vec3::Y)),
                    bone.shape.leaf_length(bone.radius),
                ),
            };
            let local_direction = (rotation.inverse() * direction).normalize_or(Vec3::Y);
            let local_forward = rotation.inverse() * (transform.rotation * forward);
            let collider = match bone.shape {
                RagdollShape::Sole { width, height } => sole_collider(
                    bone,
                    (width, height),
                    bone.length.unwrap_or(measured),
                    (local_direction * measured, local_forward),
                    rotation.inverse() * Vec3::Y,
                ),
                _ => body_collider(
                    bone,
                    bone.length.unwrap_or(measured),
                    local_direction,
                    local_forward,
                ),
            };
            let parent = nearest_ancestor(joint);
            let mut body = commands.spawn((
                Transform {
                    translation: position,
                    rotation,
                    scale: Vec3::ONE,
                },
                RigidBody {
                    kind: BodyKind::Kinematic,
                    linear_damping: BODY_LINEAR_DAMPING,
                    angular_damping: BODY_ANGULAR_DAMPING,
                    ..RigidBody::default()
                },
                collider,
                Mass::Auto,
                Velocity::default(),
            ));
            if let Some(p) = parent {
                // The parent body's frame is the parent joint's unscaled world frame.
                let (parent_position, parent_rotation) = decompose(globals[bones[p].0 as usize]);
                let anchor = parent_rotation.inverse() * (position - parent_position);
                let kind = match bone.limits.filter(|_| limits_on) {
                    Some(limits) => limited_joint(&bind, skeleton, joint, bones[p].0, child, forward, limits),
                    None => JointKind::Spherical { cone_limit: None },
                };
                body.insert((
                    Joint {
                        kind,
                        connected: bodies_entity(&bodies, p),
                        anchor: Vec3::ZERO,
                        connected_anchor: anchor,
                        break_force: 0.0,
                    },
                    JointNoContacts,
                ));
            }
            let entity = body.id();
            bodies.push(RagdollBody {
                joint,
                entity,
                parent,
                hit: 0.0,
                hit_decay: 0.0,
            });
        }
        let len = bones.len();
        let root_forward = bones
            .first()
            .map_or(forward, |(j, _)| {
                decompose(globals[*j as usize]).1.inverse() * (transform.rotation * forward)
            })
            .normalize_or(Vec3::NEG_Z);
        let root_up = bones
            .first()
            .map_or(Vec3::Y, |(j, _)| {
                decompose(globals[*j as usize]).1.inverse() * (transform.rotation * Vec3::Y)
            })
            .normalize_or(Vec3::Y);
        commands.entity(rig).insert((
            RagdollBodies {
                bodies,
                weight: 0.0,
                frozen_weight: 0.0,
                frozen: vec![None; len],
                prev_targets: vec![None; len],
                applied: None,
                root_forward,
                still_steps: 0,
                settled: false,
                face_up: false,
                simulated_since: None,
                budget_frozen: false,
                root_up,
                active: RagdollActiveState::default(),
                target_globals: Vec::new(),
                steady: KinematicStamp::default(),
            },
            PhysicsPoseBlend::new(skeleton.len()),
        ));
    }
}

/// The swing-twist joint of a limited bone (module docs): frame from the bind pose in rig
/// space, twist along the bone towards `child` (else away from the parent joint), bend
/// towards the flex direction, rest = the bind relative rotation.
fn limited_joint(
    bind: &[Mat4],
    skeleton: &regia_animation::Skeleton,
    joint: u32,
    parent_joint: u32,
    child: Option<u32>,
    forward: Vec3,
    limits: RagdollLimits,
) -> JointKind {
    let (position, rotation) = decompose(bind[joint as usize]);
    let (_, parent_rotation) = decompose(bind[parent_joint as usize]);
    let direction = match child {
        Some(c) => bind[c as usize].w_axis.truncate() - position,
        None => skeleton.parents[joint as usize]
            .map_or(Vec3::Y, |p| position - bind[p as usize].w_axis.truncate()),
    }
    .normalize_or(Vec3::Y);
    let toward = match limits.flex {
        RagdollFlex::Forward => forward,
        RagdollFlex::Backward => -forward,
    };
    let bend = direction
        .cross(toward)
        .try_normalize()
        .unwrap_or_else(|| direction.any_orthonormal_vector());
    JointKind::SwingTwist {
        twist_axis: rotation.inverse() * direction,
        bend_axis: rotation.inverse() * bend,
        bend: limits.bend,
        side: limits.side,
        twist: limits.twist,
        rest: Some((parent_rotation.inverse() * rotation).normalize()),
    }
}

/// `value` clamped to `[0, 1]`, or `fallback` when it is not finite (`f32::clamp` passes NaN
/// through).
fn unit_or(value: f32, fallback: f32) -> f32 {
    if value.is_finite() {
        value.clamp(0.0, 1.0)
    } else {
        fallback
    }
}

/// The collider of one body in its joint's frame: the shape along `direction` (joint-local
/// unit bone direction), `length` long (before the per-shape minimum), with the density that
/// gives the bone's mass. A box's depth axis is `forward` (joint-local) made perpendicular to
/// the bone. The capsule arm is the v1 code, unchanged, so capsule ragdolls replay bit-exactly.
fn body_collider(bone: &RagdollBone, length: f32, direction: Vec3, forward: Vec3) -> Collider {
    // A non-finite mass would give an infinite or NaN density (audit-v035-numerics L2); it takes
    // the floor like a tiny one does.
    let mass = if bone.mass.is_finite() {
        bone.mass.max(0.1)
    } else {
        0.1
    };
    if !(bone.mass.is_finite() && bone.radius.is_finite() && bone.radius > 0.0) {
        tracing::warn!(
            target: "regia_framework",
            joint = %bone.joint,
            mass = bone.mass,
            radius = bone.radius,
            "ragdoll bone with a non-finite mass or a radius that is not positive; using the floors"
        );
    }
    match bone.shape {
        RagdollShape::Capsule => {
            // Floored like the sphere's radius: a zero, negative or NaN radius built a capsule
            // the physics seam rejects, and the bone spawned without a collider (L2). Radii of
            // 5 mm and more are unchanged, so capsule ragdolls replay bit-exactly.
            let radius = if bone.radius.is_finite() {
                bone.radius.max(0.005)
            } else {
                0.005
            };
            let length = length.max(radius * 2.0);
            Collider {
                shape: ColliderShape::Capsule {
                    half_height: (length * 0.5 - radius).max(0.01),
                    radius,
                    axis: ShapeAxis::Y,
                },
                offset: Transform {
                    translation: direction * (length * 0.5),
                    rotation: Quat::from_rotation_arc(Vec3::Y, direction),
                    scale: Vec3::ONE,
                },
                density: mass / capsule_volume(length, radius),
                ..Collider::default()
            }
        }
        RagdollShape::Sphere => {
            let radius = bone.radius.max(0.005);
            let volume = 4.0 / 3.0 * std::f32::consts::PI * radius.powi(3);
            Collider {
                shape: ColliderShape::Sphere { radius },
                offset: Transform {
                    translation: direction * (length.max(0.0) * 0.5),
                    rotation: Quat::IDENTITY,
                    scale: Vec3::ONE,
                },
                density: mass / volume,
                ..Collider::default()
            }
        }
        // `build_ragdolls` builds a sole with `sole_collider`; here it is a box along the bone.
        RagdollShape::Box { width, depth } | RagdollShape::Sole { width, height: depth } => {
            let half = Vec3::new(width.max(0.01), length.max(0.02), depth.max(0.01)) * 0.5;
            let depth_axis = (forward - direction * forward.dot(direction))
                .try_normalize()
                .unwrap_or_else(|| direction.any_orthonormal_vector());
            let width_axis = direction.cross(depth_axis);
            let rotation = Quat::from_mat3(&regia_core::math::Mat3::from_cols(
                width_axis, direction, depth_axis,
            ));
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                offset: Transform {
                    translation: direction * half.y,
                    rotation: rotation.normalize(),
                    scale: Vec3::ONE,
                },
                density: mass / (8.0 * half.x * half.y * half.z),
                ..Collider::default()
            }
        }
    }
}

/// The collider of a `Sole` body in its joint's frame (`RagdollShape::Sole`): `child` is the child joint's offset (joint
/// local), `up` the world up (joint local). Non-finite or degenerate input falls back to sensible axes and the floors.
fn sole_collider(
    bone: &RagdollBone,
    (width, height): (f32, f32),
    length: f32,
    (child, forward): (Vec3, Vec3),
    up: Vec3,
) -> Collider {
    let floor = |v: f32, min: f32| if v.is_finite() { v.max(min) } else { min };
    let mass = floor(bone.mass, 0.1);
    let up = up.try_normalize().unwrap_or(Vec3::Y);
    let level = |v: Vec3| (v - up * v.dot(up)).try_normalize();
    let ahead = level(child)
        .or_else(|| level(forward))
        .unwrap_or_else(|| up.any_orthonormal_vector());
    let half = Vec3::new(floor(width, 0.01), floor(length, 0.02), floor(height, 0.01)) * 0.5;
    let low = if child.is_finite() {
        child.dot(up).min(0.0)
    } else {
        0.0
    };
    let rotation = Quat::from_mat3(&regia_core::math::Mat3::from_cols(ahead.cross(up), ahead, up));
    Collider {
        shape: ColliderShape::Box { half_extents: half },
        offset: Transform {
            // From a quarter of the length behind the joint to three quarters ahead; underside at `low`.
            translation: ahead * (half.y * 0.5) + up * (low + half.z),
            rotation: rotation.normalize(),
            scale: Vec3::ONE,
        },
        density: mass / (8.0 * half.x * half.y * half.z),
        ..Collider::default()
    }
}

/// Volume of a capsule of total `length` (cap tip to cap tip) and `radius`.
fn capsule_volume(length: f32, radius: f32) -> f32 {
    let cylinder = (length - 2.0 * radius).max(0.02);
    std::f32::consts::PI * radius * radius * cylinder + 4.0 / 3.0 * std::f32::consts::PI * radius.powi(3)
}

fn bodies_entity(bodies: &[RagdollBody], index: usize) -> Entity {
    bodies.get(index).map_or(Entity::PLACEHOLDER, |b| b.entity)
}

/// Turn [`RagdollHit`] messages into impulses and per-bone reaction weights.
#[tracing::instrument(skip_all, target = "regia_framework")]
fn apply_hits(
    mut commands: Commands,
    mut hits: MessageReader<RagdollHit>,
    mut rigs: Query<(&SkinnedMesh, &mut RagdollBodies, &Ragdoll)>,
    fixed: Option<Res<FixedTime>>,
) {
    let dt = fixed.map_or(1.0 / 60.0, |f| f.step().as_secs_f32().max(1e-4));
    for hit in hits.read() {
        let Ok((binding, mut ragdoll, profile)) = rigs.get_mut(hit.rig) else {
            continue;
        };
        let skeleton = &binding.skeleton;
        let Some(mut joint) = skeleton.joint_index(&hit.joint) else {
            tracing::warn!(target: "regia_framework", "ragdoll hit on unknown joint {:?}", hit.joint);
            continue;
        };
        // Nearest simulated bone at or above the joint.
        let index = loop {
            if let Some(i) = ragdoll.bodies.iter().position(|b| b.joint == joint) {
                break Some(i);
            }
            match skeleton.parents[joint as usize] {
                Some(p) => joint = p,
                None => break None,
            }
        };
        let Some(index) = index else { continue };
        // Active ragdolls: the hit bone's motor group weakens and recovers (spec 93 § 10).
        let hit_joint = ragdoll.bodies[index].joint;
        if let Some(motor) = profile
            .profile
            .active
            .motors
            .iter()
            .find(|m| skeleton.joint_index(&m.joint) == Some(hit_joint))
        {
            let response = profile.profile.active.hit;
            ragdoll
                .active
                .hit(motor.group, hit.impulse.length(), &response, dt);
        }
        commands
            .entity(ragdoll.bodies[index].entity)
            .insert(ExternalImpulse {
                impulse: hit.impulse,
                torque_impulse: Vec3::ZERO,
            });
        let reaction = hit.reaction.clamp(0.0, 1.0);
        let decay = reaction / hit.recover.max(1e-3);
        // The hit bone and every body below it react.
        for i in 0..ragdoll.bodies.len() {
            let mut cursor = Some(i);
            let mut below = false;
            while let Some(c) = cursor {
                if c == index {
                    below = true;
                    break;
                }
                cursor = ragdoll.bodies[c].parent;
            }
            if below && ragdoll.bodies[i].hit <= reaction {
                ragdoll.bodies[i].hit = reaction;
                ragdoll.bodies[i].hit_decay = decay;
            }
        }
    }
}

/// Ease the shown weight, decay recover and hit weights: per fixed step, not per frame. A hitched
/// frame (a long `delta` that `max_substeps` caps into a few sim steps) must ease these by the same
/// amount as a fixed replay of those steps, or a get-up montage starts on a different tick than the
/// replay. `sync_ragdoll_pose` (the shown blend sample) reads what this writes and changes none of
/// it.
#[tracing::instrument(skip_all, target = "regia_framework")]
fn decay_ragdoll_weights(fixed: Res<FixedTime>, mut rigs: Query<(&Ragdoll, &mut RagdollBodies)>) {
    let dt = fixed.step().as_secs_f32();
    for (ragdoll, mut bodies) in &mut rigs {
        let target = match ragdoll.mode {
            RagdollMode::Kinematic => 0.0,
            RagdollMode::Powered => unit_or(ragdoll.blend, 0.0),
            RagdollMode::Limp | RagdollMode::Active => 1.0,
        };
        let rate = if ragdoll.blend_time > 0.0 {
            dt / ragdoll.blend_time
        } else {
            1.0
        };
        // A non-finite shown weight (from an earlier NaN `blend`) restarts at the target instead
        // of easing from NaN forever (audit-v035-numerics M4).
        let w = if bodies.weight.is_finite() {
            bodies.weight
        } else {
            target
        };
        bodies.weight = w + (target - w).clamp(-rate, rate);
        // A recover eases the frozen last-simulated pose out over the same time; while it
        // lasts it holds the fallen pose over the animation (module docs).
        if fully_dynamic(ragdoll.mode) {
            bodies.frozen_weight = 0.0;
        } else if bodies.frozen_weight > 0.0 {
            bodies.frozen_weight = (bodies.frozen_weight - rate).max(0.0);
            if bodies.frozen_weight <= 0.0 {
                bodies.frozen = vec![None; bodies.bodies.len()];
            }
        }
        for body in &mut bodies.bodies {
            body.hit = (body.hit - body.hit_decay * dt).max(0.0);
            if ragdoll.mode == RagdollMode::Kinematic {
                body.hit = 0.0;
            }
        }
    }
}

/// Publish the simulated pose for blending; reads the weights `decay_ragdoll_weights` eased this
/// step and changes none of them.
#[tracing::instrument(skip_all, target = "regia_framework")]
fn sync_ragdoll_pose(
    mut rigs: Query<(&Ragdoll, &mut RagdollBodies, &mut PhysicsPoseBlend, &Transform)>,
    poses: Query<(Option<&PhysicsPose>, &Transform)>,
) {
    for (_ragdoll, bodies, mut blend, rig_transform) in &mut rigs {
        blend.joints.fill(None);
        if degenerate_scale(rig_transform.scale) {
            // No rig space to publish the bodies in: the animation shows alone this frame.
            continue;
        }
        let inverse_rig = rig_matrix(rig_transform).inverse();
        for (index, body) in bodies.bodies.iter().enumerate() {
            let Ok((physics, transform)) = poses.get(body.entity) else {
                continue;
            };
            let frozen_pose = bodies
                .frozen
                .get(index)
                .copied()
                .flatten()
                .filter(|_| bodies.frozen_weight > 0.0 && body.hit <= 0.0);
            let world = match frozen_pose {
                Some((position, rotation)) => Mat4::from_rotation_translation(rotation, position),
                None => match physics {
                    Some(p) => Mat4::from_rotation_translation(p.rotation, p.translation),
                    None => Mat4::from_rotation_translation(transform.rotation, transform.translation),
                },
            };
            let joint = body.joint as usize;
            if joint < blend.joints.len() {
                blend.joints[joint] = try_decompose(inverse_rig * world);
                blend.weights[joint] = if frozen_pose.is_some() {
                    bodies.frozen_weight.clamp(0.0, 1.0)
                } else {
                    bodies.bone_weight(index)
                };
            }
        }
    }
}

/// Where the rig origin moves on a recover with `recover_snap` (module docs): the yaw of the
/// fallen-vs-animated root offset applied about +Y, and the translation that lands the
/// animation's root on the fallen root body (the caller keeps its height). The yaw is extracted
/// and rebuilt with the deterministic kernel (spec 15 § 5), not `to_euler`/`from_rotation_y`: the
/// snapped root is the character root the rest of the fixed step reads, and both lockstep peers
/// must write the same bits whatever their platform libm says.
fn recover_snap_origin(
    rig: &Transform,
    root_position: Vec3,
    root_rotation: Quat,
    target_position: Vec3,
    target_rotation: Quat,
) -> (Quat, Vec3) {
    let local = rig_matrix(rig).inverse().transform_point3(target_position);
    let offset = root_rotation * target_rotation.inverse();
    let yaw = det_yaw_of(offset);
    let rotation = (det_yaw_quat(if yaw.is_finite() { yaw } else { 0.0 }) * rig.rotation).normalize();
    let translation = root_position - rotation * local;
    (rotation, translation)
}

/// Per fixed step: set body kinds for the mode, move kinematic bodies onto the animation and
/// steer powered bodies towards it. A mode switch keeps the motion continuous (module docs):
/// bodies turning dynamic are seeded with the animated velocity they followed, bodies turning
/// kinematic are teleported onto the animation, and a limp → kinematic/powered recover also
/// moves the rig to the fallen root body and freezes the last simulated pose for the blend
/// out.
#[tracing::instrument(skip_all, target = "regia_framework")]
#[allow(clippy::too_many_arguments)] // ADR-2498: a system; the physics world for the active drives made it 8
fn drive_ragdolls(
    mut commands: Commands,
    mut rigs: RagdollRigQuery,
    mut bodies_q: RagdollBodyQuery,
    fixed: Res<FixedTime>,
    settings: Option<Res<RagdollSettings>>,
    mut settled_messages: MessageWriter<RagdollSettled>,
    mut clock: Local<u64>,
    (mut scratch, mut budget): (Local<active::ActiveScratch>, Local<BudgetScratch>),
    (mut physics, physics_settings): (
        Option<ResMut<regia_physics::PhysicsWorld>>,
        Option<Res<regia_physics::PhysicsSettings>>,
    ),
) {
    let dt = fixed.step().as_secs_f32().max(1e-4);
    *clock += 1;
    let step = *clock;
    budget_freezes(
        &rigs,
        settings.map_or(usize::MAX, |s| s.max_simulated),
        step,
        &mut budget,
    );
    let freeze = budget.freeze.as_slice();
    for (rig, ragdoll, mut bodies, binding, animator, mut rig_transform) in &mut rigs {
        let pose = animator.evaluated_pose();
        // A rig scaled to zero: every target would be NaN. The bodies keep their last pose until
        // the scale is back (audit v033b).
        if pose.len() != binding.skeleton.len() || degenerate_scale(rig_transform.scale) {
            continue;
        }
        let old_mode = bodies.applied;
        let switching = old_mode.is_some_and(|old| old != ragdoll.mode);
        if old_mode != Some(ragdoll.mode) {
            // A mode change leaves the freeze and restarts the settle watch.
            if bodies.settled {
                set_damping(&bodies, &mut bodies_q, false);
            }
            bodies.budget_frozen = false;
            bodies.still_steps = 0;
            bodies.settled = false;
            bodies.simulated_since = match ragdoll.mode {
                RagdollMode::Kinematic => None,
                _ => bodies.simulated_since.or(Some(step)),
            };
        }
        if freeze.contains(&rig) {
            // Budget: kinematic where the bodies lie; the mesh keeps the fallen pose.
            bodies.budget_frozen = true;
            for body in &bodies.bodies {
                if let Ok((mut rigid, mut transform, mut velocity, physics)) = bodies_q.get_mut(body.entity) {
                    if let Some(p) = physics {
                        transform.translation = p.translation;
                        transform.rotation = p.rotation;
                    }
                    velocity.linear = Vec3::ZERO;
                    velocity.angular = Vec3::ZERO;
                    rigid.kind = BodyKind::Kinematic;
                }
            }
        }
        if bodies.budget_frozen {
            continue;
        }
        if ragdoll.mode == RagdollMode::Kinematic
            && !switching
            && kinematic_steady(&bodies, &bodies_q, pose, &rig_transform, &binding.skeleton)
        {
            // Same pose, rig and skeleton, every body where the last compose put it: the compose
            // and the body writes would repeat that step bit for bit (audit framework4 finding 5).
            let targets = std::mem::take(&mut bodies.target_globals);
            drive_active(
                &mut bodies,
                &bodies_q,
                ragdoll,
                &binding.skeleton,
                &targets,
                &mut physics,
                physics_settings
                    .as_ref()
                    .map_or((4, 9.81), |s| (s.solver_iterations.max(1), s.gravity.length())),
                dt,
                false,
                false,
                &mut scratch,
            );
            active::tick_hits(&mut bodies.active, dt);
            bodies.target_globals = targets;
            continue;
        }
        let recovering = switching && old_mode.is_some_and(fully_dynamic) && !fully_dynamic(ragdoll.mode);
        // The body poses before this step touches them (the fallen pose, for the freeze).
        let world_poses: Vec<Option<(Vec3, Quat)>> = if switching {
            bodies
                .bodies
                .iter()
                .map(|body| {
                    bodies_q.get(body.entity).ok().map(|(_, transform, _, physics)| {
                        physics.map_or((transform.translation, transform.rotation), |p| {
                            (p.translation, p.rotation)
                        })
                    })
                })
                .collect()
        } else {
            Vec::new()
        };
        let mut targets = std::mem::take(&mut bodies.target_globals);
        #[cfg(test)]
        note_compose();
        binding
            .skeleton
            .write_globals(pose, Some(rig_matrix(&rig_transform)), &mut targets);
        if recovering && ragdoll.recover_snap {
            // Move the rig origin (x/z + yaw) so the animation's root lands where the fallen
            // root body lies; the height is the animation's (a get-up clip rises by itself).
            if let Some(root) = bodies.bodies.iter().position(|b| b.parent.is_none())
                && let Some((root_position, root_rotation)) = world_poses[root]
            {
                let root_joint = bodies.bodies[root].joint as usize;
                let (target_position, target_rotation) = decompose(targets[root_joint]);
                let (rotation, translation) = recover_snap_origin(
                    &rig_transform,
                    root_position,
                    root_rotation,
                    target_position,
                    target_rotation,
                );
                rig_transform.rotation = rotation;
                rig_transform.translation.x = translation.x;
                rig_transform.translation.z = translation.z;
                #[cfg(test)]
                note_compose();
                binding
                    .skeleton
                    .write_globals(pose, Some(rig_matrix(&rig_transform)), &mut targets);
            }
        }
        if recovering {
            // Hold the last simulated pose over the animation while the shown weight eases
            // back down (`sync_ragdoll_pose` publishes it).
            bodies.frozen = world_poses.clone();
            bodies.frozen_weight = bodies.weight;
        } else if fully_dynamic(ragdoll.mode) {
            bodies.frozen = Vec::new();
            bodies.frozen_weight = 0.0;
        }
        let set_kinds = old_mode != Some(ragdoll.mode);
        let RagdollBodies {
            bodies: body_list,
            prev_targets,
            ..
        } = &mut *bodies;
        for (index, body) in body_list.iter().enumerate() {
            let (joint, entity, parent) = (body.joint, body.entity, body.parent);
            let Ok((mut rigid, mut transform, mut velocity, physics)) = bodies_q.get_mut(entity) else {
                continue;
            };
            let was_kinematic = old_mode.is_some_and(|old| mode_kinematic(old, parent));
            let kinematic = mode_kinematic(ragdoll.mode, parent);
            let kind = if kinematic {
                BodyKind::Kinematic
            } else {
                BodyKind::Dynamic
            };
            if set_kinds && rigid.kind != kind {
                rigid.kind = kind;
            }
            // A bone the animation scaled to zero has no usable target this step: the body keeps
            // its pose (audit v033b).
            let Some((target_position, target_rotation)) = try_decompose(targets[joint as usize]) else {
                continue;
            };
            if switching && was_kinematic && !kinematic {
                // Momentum: start dynamic with the animated velocity of the previous fixed
                // step. A seed beyond the clamp is a same-frame teleport, not motion.
                if let Some((previous_position, previous_rotation)) =
                    prev_targets.get(index).copied().flatten()
                {
                    let linear = (target_position - previous_position) / dt;
                    let mut error = target_rotation * previous_rotation.inverse();
                    if error.w < 0.0 {
                        error = -error;
                    }
                    let (axis, angle) = error.to_axis_angle();
                    let angular = if angle.is_finite() {
                        axis * (angle / dt)
                    } else {
                        Vec3::ZERO
                    };
                    velocity.linear = if linear.length() <= MAX_SEED_SPEED {
                        linear
                    } else {
                        Vec3::ZERO
                    };
                    velocity.angular = if angular.length() <= MAX_SEED_SPIN {
                        angular
                    } else {
                        Vec3::ZERO
                    };
                }
            }
            if switching && !was_kinematic && kinematic {
                // Recover: pin the body onto the animation with no velocity and no
                // interpolation smear across the jump.
                velocity.linear = Vec3::ZERO;
                velocity.angular = Vec3::ZERO;
                commands.entity(entity).insert(Teleport);
            }
            if kinematic {
                transform.translation = target_position;
                transform.rotation = target_rotation;
                if let Some(slot) = prev_targets.get_mut(index) {
                    *slot = Some((target_position, target_rotation));
                }
                continue;
            }
            if ragdoll.mode != RagdollMode::Powered {
                continue;
            }
            // Velocity tracking towards the animated pose (module docs).
            let (position, rotation) = physics.map_or((transform.translation, transform.rotation), |p| {
                (p.translation, p.rotation)
            });
            let tau = ragdoll.tracking_time.max(dt);
            let linear = (target_position - position) / tau;
            let mut error = target_rotation * rotation.inverse();
            if error.w < 0.0 {
                error = -error;
            }
            let (axis, angle) = error.to_axis_angle();
            let angular = if angle.is_finite() {
                axis * (angle / tau)
            } else {
                Vec3::ZERO
            };
            // A NaN strength tracks nothing rather than writing NaN velocities, which the
            // physics seam drops and counts every frame (audit-v035-numerics M4).
            let s = unit_or(ragdoll.strength, 0.0);
            velocity.linear = velocity.linear.lerp(linear, s);
            velocity.angular = velocity.angular.lerp(angular, s);
        }
        bodies.applied = Some(ragdoll.mode);
        if ragdoll.mode == RagdollMode::Kinematic {
            bodies.steady.record(pose, &rig_transform, &binding.skeleton);
        } else {
            bodies.steady.valid = false;
        }
        let is_active = ragdoll.mode == RagdollMode::Active;
        let entered_active = is_active && old_mode != Some(RagdollMode::Active);
        // An active ragdoll that stands is held up by its motors: it does not settle (spec 93 § 10).
        let holding = is_active && !entered_active && bodies.active.holding();
        if fully_dynamic(ragdoll.mode) && !switching && !holding {
            watch_settle(
                rig,
                ragdoll,
                &mut bodies,
                &mut bodies_q,
                dt,
                &mut settled_messages,
            );
        }
        drive_active(
            &mut bodies,
            &bodies_q,
            ragdoll,
            &binding.skeleton,
            &targets,
            &mut physics,
            physics_settings
                .as_ref()
                .map_or((4, 9.81), |s| (s.solver_iterations.max(1), s.gravity.length())),
            dt,
            entered_active,
            is_active && old_mode == Some(RagdollMode::Active),
            &mut scratch,
        );
        // After the motors sampled the hit strength: a hit parked at step 0 by `apply_hits` drives one fixed step at
        // its full drop, even when it recovers within that step (audit framework5 #15).
        active::tick_hits(&mut bodies.active, dt);
        bodies.target_globals = targets;
    }
}

/// The active-ragdoll part of [`drive_ragdolls`] for one rig (spec 93 § 10, [`active`]): the phase, the get-up
/// choice when it settles, and the motors and balance of an `Active` ragdoll. The caller advances the hit recovery
/// clocks after it, so the motors see a new hit's strength first.
#[allow(clippy::too_many_arguments)] // ADR-2498: one call site, the drive system's own locals
fn drive_active(
    bodies: &mut RagdollBodies,
    bodies_q: &RagdollBodyQuery,
    ragdoll: &Ragdoll,
    skeleton: &regia_animation::Skeleton,
    targets: &[Mat4],
    physics: &mut Option<ResMut<regia_physics::PhysicsWorld>>,
    (solver_iterations, gravity): (usize, f32),
    dt: f32,
    entered_active: bool,
    drive: bool,
    scratch: &mut active::ActiveScratch,
) {
    let profile = &ragdoll.profile.active;
    let (settled, recovering) = (bodies.settled, bodies.recovering());
    active::update_phase(
        &mut bodies.active,
        ragdoll.mode,
        entered_active,
        profile,
        settled,
        recovering,
    );
    let choose = settled && bodies.active.facing().is_none();
    if ragdoll.mode != RagdollMode::Active {
        release_drives(bodies, physics);
    }
    if ragdoll.mode != RagdollMode::Active && !choose {
        return;
    }
    scratch.list.clear();
    for body in &bodies.bodies {
        let (position, rotation) = bodies_q.get(body.entity).ok().map_or(
            (Vec3::ZERO, Quat::IDENTITY),
            |(_, transform, _, physics)| {
                physics.map_or((transform.translation, transform.rotation), |p| {
                    (p.translation, p.rotation)
                })
            },
        );
        let animated = targets.get(body.joint as usize).and_then(|m| try_decompose(*m));
        scratch.list.push(active::ActiveBody {
            parent: body.parent,
            joint: body.joint,
            entity: body.entity,
            position,
            rotation,
            target: animated.map(|(_, r)| r),
            target_position: animated.map_or(Vec3::ZERO, |(p, _)| p),
        });
    }
    if choose {
        let (forward, up) = (bodies.root_forward, bodies.root_up);
        active::choose_get_up(&mut bodies.active, profile, skeleton, &scratch.list, forward, up);
    }
    if ragdoll.mode != RagdollMode::Active {
        return;
    }
    let active::ActiveScratch { list, work } = scratch;
    let step = active::ActiveStep {
        profile,
        skeleton,
        bodies: list,
        physics: physics.as_deref(),
        dt,
        solver_iterations,
        gravity,
        drive,
        root_forward: bodies.root_forward,
        root_up: bodies.root_up,
    };
    active::step_active(&mut bodies.active, &step, work);
    if !work.drives.iter().any(Option::is_some) {
        release_drives(bodies, physics);
        return;
    }
    let Some(world) = physics.as_deref_mut() else {
        return;
    };
    let weight = bodies.active.foot_weight;
    for (index, ((body, drive), lock)) in list
        .iter()
        .zip(work.drives.iter().copied())
        .zip(work.foot_locks.iter().copied())
        .enumerate()
    {
        regia_physics::pd::set_joint_drive(world, body.entity, drive);
        match bodies.active.anchors.get(index).copied().flatten() {
            Some(anchor) if lock => follow_support(world, body.entity, &anchor, weight, dt),
            _ => regia_physics::pd::set_body_locked(world, body.entity, lock),
        }
    }
    bodies.active.drive_on = true;
}

/// Fraction of a planted foot's drift from its anchor pose corrected per fixed step (`stepping` module docs).
const ANCHOR_CORRECTION: f32 = 0.5;

/// Moves a foot planted on a body that can move with that body (`stepping` module docs): locked, with the velocity of
/// the support at the foot plus a correction back onto its anchor pose; a dynamic support gets the foot's weight share.
fn follow_support(
    world: &mut regia_physics::PhysicsWorld,
    foot: Entity,
    anchor: &active::FootAnchor,
    weight: f32,
    dt: f32,
) {
    use regia_physics::pd;
    let (Some((support_at, support_rotation)), Some((at, rotation)), Some(dynamics)) = (
        pd::body_pose(world, anchor.support),
        pd::body_pose(world, foot),
        pd::body_dynamics(world, foot),
    ) else {
        pd::set_body_locked(world, foot, true);
        return;
    };
    let want_at = support_at + support_rotation * anchor.position;
    let want_rotation = (support_rotation * anchor.rotation).normalize();
    let support = pd::body_dynamics(world, anchor.support);
    let spin = support.map_or(Vec3::ZERO, |s| s.angular);
    let carried = pd::point_velocity(world, anchor.support, dynamics.center_of_mass).unwrap_or(Vec3::ZERO);
    let correction = ANCHOR_CORRECTION / dt.max(1e-4);
    let linear = carried + (want_at - at) * correction;
    let angular = spin + pd::rotation_error(rotation, want_rotation) * correction;
    pd::set_body_locked_moving(world, foot, linear, angular);
    if anchor.dynamic && weight > 0.0 {
        pd::apply_impulse_at_point(world, anchor.support, Vec3::NEG_Y * (weight * dt), at);
    }
}

/// Clears a rig's joint drives and foot locks, if it has any (a rig that never drove is not touched).
fn release_drives(bodies: &mut RagdollBodies, physics: &mut Option<ResMut<regia_physics::PhysicsWorld>>) {
    if !bodies.active.drive_on {
        return;
    }
    let Some(world) = physics.as_deref_mut() else {
        return;
    };
    for body in &bodies.bodies {
        regia_physics::pd::set_joint_drive(world, body.entity, None);
        regia_physics::pd::set_body_locked(world, body.entity, false);
    }
    bodies.active.drive_on = false;
}

/// The rig query of [`drive_ragdolls`].
type RagdollRigQuery<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Ragdoll,
        &'static mut RagdollBodies,
        &'static SkinnedMesh,
        &'static Animator,
        &'static mut Transform,
    ),
>;

/// The body query of [`drive_ragdolls`].
type RagdollBodyQuery<'w, 's> = Query<
    'w,
    's,
    (
        &'static mut RigidBody,
        &'static mut Transform,
        &'static mut Velocity,
        Option<&'static PhysicsPose>,
    ),
    Without<Ragdoll>,
>;

/// Count a limp ragdoll's steps at rest; settle it (one [`RagdollSettled`]) after
/// `settle_time`, and unsettle it when a body moves again.
fn watch_settle(
    rig: Entity,
    ragdoll: &Ragdoll,
    bodies: &mut RagdollBodies,
    bodies_q: &mut RagdollBodyQuery,
    dt: f32,
    messages: &mut MessageWriter<RagdollSettled>,
) {
    // A settled ragdoll has to move clearly (twice the limit) to unsettle, so the rest
    // damping does not flicker on and off.
    let limit = ragdoll.settle_speed.max(0.0) * if bodies.settled { 2.0 } else { 1.0 };
    let still = bodies.bodies.iter().all(|body| {
        bodies_q
            .get(body.entity)
            .is_ok_and(|(_, _, v, _)| v.linear.length() <= limit)
    });
    if !still {
        if bodies.settled {
            set_damping(bodies, bodies_q, false);
        }
        bodies.still_steps = 0;
        bodies.settled = false;
        return;
    }
    bodies.still_steps = bodies.still_steps.saturating_add(1);
    if bodies.settled || (bodies.still_steps as f32) * dt < ragdoll.settle_time.max(0.0) {
        return;
    }
    let Some(root) = bodies.bodies.iter().find(|b| b.parent.is_none()) else {
        return;
    };
    let Ok((_, transform, _, physics)) = bodies_q.get(root.entity) else {
        return;
    };
    let (position, rotation) = physics.map_or((transform.translation, transform.rotation), |p| {
        (p.translation, p.rotation)
    });
    bodies.settled = true;
    bodies.face_up = (rotation * bodies.root_forward).y > 0.0;
    set_damping(bodies, bodies_q, true);
    messages.write(RagdollSettled {
        rig,
        face_up: bodies.face_up,
        root_position: position,
    });
}

/// Switch a ragdoll's bodies between the moving and the rest damping.
fn set_damping(bodies: &RagdollBodies, bodies_q: &mut RagdollBodyQuery, rest: bool) {
    let (linear, angular) = if rest {
        (REST_LINEAR_DAMPING, REST_ANGULAR_DAMPING)
    } else {
        (BODY_LINEAR_DAMPING, BODY_ANGULAR_DAMPING)
    };
    for body in &bodies.bodies {
        if let Ok((mut rigid, ..)) = bodies_q.get_mut(body.entity) {
            rigid.linear_damping = linear;
            rigid.angular_damping = angular;
        }
    }
}

/// Whether a kinematic ragdoll's compose this step would repeat its last one exactly: the
/// stamp matches the pose, rig and skeleton, and every body still has the pose that compose
/// wrote (`prev_targets`), so nothing moved a body in between.
fn kinematic_steady(
    bodies: &RagdollBodies,
    bodies_q: &RagdollBodyQuery,
    pose: &regia_animation::Pose,
    rig: &Transform,
    skeleton: &Arc<regia_animation::Skeleton>,
) -> bool {
    #[cfg(test)]
    if STEADY_SKIP_OFF.with(std::cell::Cell::get) {
        return false;
    }
    bodies.applied == Some(RagdollMode::Kinematic)
        && bodies.steady.matches(pose, rig, skeleton)
        && bodies.bodies.iter().enumerate().all(|(index, body)| {
            match bodies.prev_targets.get(index).copied().flatten() {
                Some((position, rotation)) => bodies_q.get(body.entity).is_ok_and(|(_, transform, ..)| {
                    transform.translation == position && transform.rotation == rotation
                }),
                // No usable target last compose: that body was not written then either.
                None => true,
            }
        })
}

/// The budget's per-step lists, kept by [`drive_ragdolls`] so a step makes no heap request.
#[derive(Default)]
struct BudgetScratch {
    /// Simulated candidates: (simulated since, entity bits, entity, settled).
    candidates: Vec<(u64, u64, Entity, bool)>,
    /// The rigs frozen this step.
    freeze: Vec<Entity>,
}

/// The rigs the budget freezes this step (module docs), into `scratch.freeze`: simulated
/// ragdolls ordered by the step they went simulated, then entity bits; while more than `max`
/// simulate, the oldest settled ones (in a stable mode) freeze. Unsettled ragdolls never freeze.
fn budget_freezes(rigs: &RagdollRigQuery, max: usize, step: u64, scratch: &mut BudgetScratch) {
    let BudgetScratch { candidates, freeze } = scratch;
    candidates.clear();
    freeze.clear();
    candidates.extend(
        rigs.iter()
            .filter(|(_, ragdoll, bodies, ..)| {
                ragdoll.mode != RagdollMode::Kinematic
                    && !(bodies.budget_frozen && bodies.applied == Some(ragdoll.mode))
            })
            .map(|(entity, ragdoll, bodies, ..)| {
                let stable = bodies.applied == Some(ragdoll.mode);
                (
                    bodies.simulated_since.filter(|_| stable).unwrap_or(step),
                    entity.to_bits(),
                    entity,
                    stable && bodies.settled,
                )
            }),
    );
    if candidates.len() <= max {
        return;
    }
    candidates.sort_by_key(|&(since, bits, ..)| (since, bits));
    let mut excess = candidates.len() - max;
    for &(_, _, entity, settled) in candidates.iter() {
        if excess == 0 {
            break;
        }
        if settled {
            freeze.push(entity);
            excess -= 1;
        }
    }
}

/// Despawn a rig's ragdoll bodies and remove its ragdoll components (the rig returns to pure
/// animation).
pub fn remove_ragdoll(world: &mut World, rig: Entity) {
    let bodies = world
        .get::<RagdollBodies>(rig)
        .map(|b| b.bodies.iter().map(|b| b.entity).collect::<Vec<_>>())
        .unwrap_or_default();
    for body in bodies {
        if let Ok(entity) = world.get_entity_mut(body) {
            entity.despawn();
        }
    }
    if let Ok(mut entity) = world.get_entity_mut(rig) {
        entity.remove::<(Ragdoll, RagdollBodies, PhysicsPoseBlend)>();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use regia_animation::components::spawn_skeleton;
    use regia_animation::pose::translated;
    use regia_animation::{AnimationClip, AnimationPlugin, Skeleton};

    use super::*;

    const NAMES: [&str; 17] = [
        "hips",
        "spine",
        "chest",
        "neck",
        "head",
        "upperarm_l",
        "lowerarm_l",
        "hand_l",
        "upperarm_r",
        "lowerarm_r",
        "hand_r",
        "thigh_l",
        "calf_l",
        "foot_l",
        "thigh_r",
        "calf_r",
        "foot_r",
    ];

    fn humanoid() -> Skeleton {
        let parents = vec![
            None,
            Some(0),
            Some(1),
            Some(2),
            Some(3),
            Some(2),
            Some(5),
            Some(6),
            Some(2),
            Some(8),
            Some(9),
            Some(0),
            Some(11),
            Some(12),
            Some(0),
            Some(14),
            Some(15),
        ];
        let t = |x: f32, y: f32| translated(Vec3::new(x, y, 0.0));
        let binds = vec![
            t(0.0, 1.0),
            t(0.0, 0.15),
            t(0.0, 0.25),
            t(0.0, 0.25),
            t(0.0, 0.1),
            t(0.2, 0.2),
            t(0.3, 0.0),
            t(0.27, 0.0),
            t(-0.2, 0.2),
            t(-0.3, 0.0),
            t(-0.27, 0.0),
            t(0.1, -0.05),
            t(0.0, -0.45),
            t(0.0, -0.42),
            t(-0.1, -0.05),
            t(0.0, -0.45),
            t(0.0, -0.42),
        ];
        Skeleton::from_local_bind(NAMES.iter().map(|n| n.to_string()).collect(), parents, binds)
            .expect("skeleton")
    }

    /// A limp humanoid with `blend_time` 0.5 on a floor, one 1/60 update per frame and up to 5
    /// fixed steps per frame.
    fn recovering_scene() -> (App, Entity) {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        fixed.max_substeps = 5;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((AnimationPlugin, RagdollPlugin));
        // Floor: top face at y = 0.
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(20.0, 0.5, 20.0),
                },
                ..Collider::default()
            },
        ));
        let skeleton = Arc::new(humanoid());
        let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
        ragdoll.mode = RagdollMode::Limp;
        ragdoll.blend_time = 0.5;
        let binding = spawn_skeleton(app.world_mut(), skeleton);
        let rig = app
            .world_mut()
            .spawn((
                Transform::default(),
                binding,
                Animator::single(AnimationClip::empty("idle", 1.0), true),
                ragdoll,
            ))
            .id();
        (app, rig)
    }

    #[test]
    fn mode_kinematic_truth_table() {
        // In Kinematic every body follows the animation; in Powered only the root does; in
        // Limp nothing does.
        assert!(mode_kinematic(RagdollMode::Kinematic, Some(3)));
        assert!(mode_kinematic(RagdollMode::Kinematic, None));
        assert!(mode_kinematic(RagdollMode::Powered, None));
        assert!(!mode_kinematic(RagdollMode::Powered, Some(0)));
        assert!(!mode_kinematic(RagdollMode::Limp, None));
        assert!(!mode_kinematic(RagdollMode::Limp, Some(2)));
        // Active: every body is dynamic, like Limp.
        assert!(!mode_kinematic(RagdollMode::Active, None));
        assert!(!mode_kinematic(RagdollMode::Active, Some(1)));
        assert!(fully_dynamic(RagdollMode::Active) && fully_dynamic(RagdollMode::Limp));
        assert!(!fully_dynamic(RagdollMode::Powered) && !fully_dynamic(RagdollMode::Kinematic));
    }

    /// A kinematic ragdoll's second fixed step reuses its joint buffer.
    #[test]
    fn kinematic_ragdoll_reuses_its_target_buffer() {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        fixed.max_substeps = 1;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((AnimationPlugin, RagdollPlugin));
        let skeleton = Arc::new(humanoid());
        let ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
        assert_eq!(ragdoll.mode, RagdollMode::Kinematic);
        let binding = spawn_skeleton(app.world_mut(), skeleton);
        let rig = app
            .world_mut()
            .spawn((
                Transform::default(),
                binding,
                Animator::single(AnimationClip::empty("idle", 1.0), true),
                ragdoll,
            ))
            .id();
        app.update();
        assert!(app.world().get::<RagdollBodies>(rig).is_some(), "bodies built");
        // `run_system_cached` spends a `resource_scope` on every call. The count is the
        // initialized system, which is what a schedule runs after the first frame.
        use regia_ecs::ecs::system::{IntoSystem, System};
        let mut system = IntoSystem::into_system(drive_ragdolls);
        {
            let world = app.world_mut();
            system.initialize(world);
            system.run((), world).expect("warmup step");
        }
        let allocated = crate::test_alloc::count(|| {
            system.run((), app.world_mut()).expect("steady step");
        });
        assert_eq!(
            allocated, 0,
            "the second kinematic step allocated {allocated} times"
        );
    }

    #[test]
    fn new_ragdoll_recovers_where_it_fell_by_default() {
        let ragdoll = Ragdoll::new(RagdollProfile::default());
        assert_eq!(ragdoll.mode, RagdollMode::Kinematic);
        assert!(ragdoll.recover_snap, "the get-up continues where the body lies");
        assert!((ragdoll.blend_time - 0.25).abs() < 1e-6);
    }

    /// The get-up phase and the recover blend follow the fixed step, not the frame clock: a 250 ms
    /// frame (`max_substeps` 5, the rest of the accumulated time dropped) must ease
    /// `frozen_weight` and flip `RagdollPhase` exactly like five 1/60 frames, or a get-up montage
    /// starts on a different tick than a fixed replay.
    #[test]
    fn get_up_phase_follows_fixed_steps_not_frame_delta() {
        let step = Duration::from_secs_f64(1.0 / 60.0);
        let (mut steady, steady_rig) = recovering_scene();
        let (mut hitched, hitched_rig) = recovering_scene();

        // Limp until the shown weight saturates at 1, identically in both apps, then the
        // limp-to-kinematic recover switch.
        for _ in 0..40 {
            steady.update();
            hitched.update();
        }
        steady
            .world_mut()
            .get_mut::<Ragdoll>(steady_rig)
            .expect("ragdoll")
            .mode = RagdollMode::Kinematic;
        hitched
            .world_mut()
            .get_mut::<Ragdoll>(hitched_rig)
            .expect("ragdoll")
            .mode = RagdollMode::Kinematic;

        // The hitched app's next frame is 250 ms: five fixed steps in one frame (the accumulated
        // excess is dropped), then it steps at 1/60 like the steady app. Five steady frames run
        // the same five fixed steps.
        hitched.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(250)));
        hitched.update();
        hitched.insert_resource(TimeUpdateStrategy::ManualDuration(step));
        for _ in 0..5 {
            steady.update();
        }
        let state = |app: &mut App, rig: Entity| {
            let bodies = app.world().get::<RagdollBodies>(rig).expect("bodies");
            (bodies.frozen_weight, bodies.active().phase())
        };
        assert_eq!(
            state(&mut steady, steady_rig),
            state(&mut hitched, hitched_rig),
            "after 5 fixed steps the recover blend or phase disagrees"
        );

        // Continue both to 30 fixed steps: blend_time 0.5 s at 60 Hz, where the blend ends.
        for _ in 0..25 {
            steady.update();
            hitched.update();
        }
        assert_eq!(
            state(&mut steady, steady_rig),
            state(&mut hitched, hitched_rig),
            "after 30 fixed steps the recover blend or phase disagrees"
        );
    }

    /// The recover snap extracts the fallen-vs-animated offset's yaw and rebuilds the root
    /// rotation with the deterministic kernel (spec 15 § 5), not `to_euler`/`from_rotation_y`:
    /// the snapped rig is the character root the rest of the fixed step reads, and both lockstep
    /// peers must write the same bits whatever their platform libm says. This fallen root's yaw
    /// (0.5000002 rad) is one whose kernel half-angle sine is one ulp off glibc's, so the libm
    /// rebuild fails the pin on glibc.
    #[test]
    fn recover_snap_yaw_bits_are_pinned() {
        let rig = Transform::from_rotation(Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize());
        // A fallen root body yawed 0.5000002 rad about +Y (bit-exact, built without the kernel so
        // the test compiles against the unfixed code too).
        let fallen = Quat::from_array([0.0, f32::from_bits(0x3e7d_577c), 0.0, f32::from_bits(0x3f78_0aa5)]);
        let (rotation, _) = recover_snap_origin(
            &rig,
            Vec3::new(0.5, 0.1, -0.25),
            fallen,
            Vec3::new(0.0, 1.0, 0.0),
            Quat::IDENTITY,
        );
        // The rig above is normalized by glam, whose SIMD dot groups the lanes differently on SSE2 and
        // NEON (ADR-5176), so the last bit of `z` is one value per instruction set (ADR-5550). The
        // aarch64 value is the M5's with the kernel; the libm-rebuild check is proven on glibc only.
        let bits = rotation.to_array().map(f32::to_bits);
        #[cfg(any(target_arch = "x86_64", target_arch = "x86"))]
        assert_eq!(
            bits,
            [0x3e33_c564, 0x3edac270, 0x3e8b_b1ee, 0x3f58_0a65],
            "the snapped root rotation came from the platform libm, not the deterministic kernel"
        );
        #[cfg(target_arch = "aarch64")]
        assert_eq!(
            bits,
            [0x3e33_c564, 0x3eda_c270, 0x3e8b_b1ed, 0x3f58_0a65],
            "the snapped root rotation came from the platform libm, not the deterministic kernel"
        );
    }

    /// A prone or supine fallen root (pitched ±π/2 about X) is the gimbal pose `det_yaw_of`
    /// documents as ill-conditioned: the extracted yaw is whatever f32 rounding of near-zero
    /// terms produces. framework4 review low 4: pin the chosen behavior so a kernel change
    /// cannot silently alter the facing a prone recover snaps to, and to lock that the value is
    /// deterministic across platforms (same bits everywhere, by construction of the kernel).
    #[test]
    fn recover_snap_yaw_bits_are_pinned_at_the_gimbal_pose() {
        let rig = Transform::from_rotation(Quat::from_xyzw(0.1, 0.2, 0.3, 0.9).normalize());
        for (name, pitch_bits) in [("prone", 0x3fc9_0fdbu32 + 8), ("supine", 0x3fc9_0fdbu32 - 8)] {
            // A fallen root pitched a few ulps off ±π/2 about X (bit-exact input, built without
            // the kernel so the test compiles against the unfixed code too).
            let half = f32::from_bits(pitch_bits) * 0.5;
            let fallen = Quat::from_array([half.sin(), 0.0, 0.0, half.cos()]);
            let (rotation, translation) = recover_snap_origin(
                &rig,
                Vec3::new(0.5, 0.1, -0.25),
                fallen,
                Vec3::new(0.0, 1.0, 0.0),
                Quat::IDENTITY,
            );
            assert!(
                rotation.is_finite() && translation.is_finite(),
                "{name}: the snap stays finite at the gimbal pose"
            );
            // Pinned from the deterministic kernel's output: at the gimbal pose the extracted
            // yaw is f32-rounding noise of near-zero terms — arbitrary, but these exact bits
            // hold on every x86_64 target, whatever its libm says (framework4 review low 4).
            // The rig is glam-normalized and the snap composes glam `Quat` products, whose lanes
            // group differently on SSE2 and NEON (ADR-5176), so aarch64 has its own bits,
            // measured on the M5 (ADR-5860), as in `recover_snap_yaw_bits_are_pinned`.
            #[cfg(not(target_arch = "aarch64"))]
            let (want_rotation, want_translation): ([u32; 4], [u32; 3]) = match name {
                "prone" => (
                    [0x3e9d_971e, 0x3f6c_62ab, 0xbdd2_1ed4, 0xbe52_1ed5],
                    [0x3eff_fffc, 0xbf66_666a, 0xbe7f_fffe],
                ),
                _ => (
                    [0x3dd2_1ed2, 0x3e52_1ed2, 0x3e9d_971e, 0x3f6c_62ab],
                    [0x3f00_0001, 0xbf66_666a, 0xbe80_0000],
                ),
            };
            #[cfg(target_arch = "aarch64")]
            let (want_rotation, want_translation): ([u32; 4], [u32; 3]) = match name {
                "prone" => (
                    [0x3e9d_971d, 0x3f6c_62ac, 0xbdd2_1ed4, 0xbe52_1ed5],
                    [0x3eff_ffff, 0xbf66_6668, 0xbe80_0000],
                ),
                _ => (
                    [0x3dd2_1ed2, 0x3e52_1ed2, 0x3e9d_971d, 0x3f6c_62ac],
                    [0x3f00_0000, 0xbf66_6668, 0xbe7f_ffff],
                ),
            };
            assert_eq!(
                rotation.to_array().map(f32::to_bits),
                want_rotation,
                "{name}: the snapped rotation changed (kernel regression?)"
            );
            assert_eq!(
                translation.to_array().map(f32::to_bits),
                want_translation,
                "{name}: the snapped translation changed (kernel regression?)"
            );
        }
    }

    fn composes() -> u64 {
        GLOBALS_COMPOSES.with(std::cell::Cell::get)
    }

    /// An app with `rigs` humanoid ragdolls in `mode` (one fixed step per 1/60 frame), on a
    /// floor, after `frames` updates.
    fn ragdoll_scene(rigs: usize, mode: RagdollMode, frames: usize) -> (App, Vec<Entity>) {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        fixed.max_substeps = 1;
        app.insert_resource(fixed)
            .add_plugins(regia_physics::PhysicsPlugin)
            .add_plugins((AnimationPlugin, RagdollPlugin));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(50.0, 0.5, 50.0),
                },
                ..Collider::default()
            },
        ));
        let skeleton = Arc::new(humanoid());
        let mut out = Vec::with_capacity(rigs);
        for index in 0..rigs {
            let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
            ragdoll.mode = mode;
            let binding = spawn_skeleton(app.world_mut(), Arc::clone(&skeleton));
            let rig = app
                .world_mut()
                .spawn((
                    Transform::from_translation(Vec3::new(index as f32 * 2.0, 0.2, 0.0)),
                    binding,
                    Animator::single(AnimationClip::empty("idle", 1.0), true),
                    ragdoll,
                ))
                .id();
            out.push(rig);
        }
        for _ in 0..frames {
            app.update();
        }
        (app, out)
    }

    /// `drive_ragdolls` as a schedule runs it after its first frame (initialized once).
    fn drive_system(app: &mut App) -> impl regia_ecs::ecs::system::System<In = (), Out = ()> + use<> {
        use regia_ecs::ecs::system::{IntoSystem, System};
        let mut system = IntoSystem::into_system(drive_ragdolls);
        system.initialize(app.world_mut());
        system
    }

    fn run_drive(system: &mut impl regia_ecs::ecs::system::System<In = (), Out = ()>, app: &mut App) {
        system.run((), app.world_mut()).expect("drive step");
    }

    fn body_pose(app: &App, body: Entity) -> (Vec3, Quat) {
        let transform = app.world().get::<Transform>(body).expect("body transform");
        (transform.translation, transform.rotation)
    }

    /// Audit framework4 finding 5: a kinematic ragdoll whose evaluated pose and rig transform did
    /// not change since its last fixed step does not compose its joint globals again (the bodies
    /// already sit on that pose).
    #[test]
    fn a_steady_kinematic_ragdoll_does_not_recompose_its_globals() {
        let (mut app, rigs) = ragdoll_scene(1, RagdollMode::Kinematic, 4);
        let rig = rigs[0];
        assert_eq!(
            app.world().get::<RagdollBodies>(rig).expect("bodies").applied,
            Some(RagdollMode::Kinematic)
        );
        let mut system = drive_system(&mut app);
        run_drive(&mut system, &mut app);
        let before = composes();
        for _ in 0..3 {
            run_drive(&mut system, &mut app);
        }
        assert_eq!(
            composes() - before,
            0,
            "the same kinematic pose and rig composed its joint globals again"
        );
    }

    /// The steady skip keeps the kinematic contract: a body something else moved is put back on
    /// the animation, and a moved rig carries every body with it, on the next fixed step.
    #[test]
    fn a_steady_kinematic_ragdoll_still_follows_its_rig_and_snaps_a_moved_body() {
        let (mut app, rigs) = ragdoll_scene(1, RagdollMode::Kinematic, 4);
        let rig = rigs[0];
        let bodies: Vec<Entity> = app
            .world()
            .get::<RagdollBodies>(rig)
            .expect("bodies")
            .bodies
            .iter()
            .map(|b| b.entity)
            .collect();
        assert!(bodies.len() > 2, "the humanoid profile built its bodies");
        let mut system = drive_system(&mut app);
        run_drive(&mut system, &mut app);
        run_drive(&mut system, &mut app);
        let rest: Vec<(Vec3, Quat)> = bodies.iter().map(|b| body_pose(&app, *b)).collect();
        // Something else moves one body: the next step puts it back on the animation.
        app.world_mut()
            .get_mut::<Transform>(bodies[2])
            .expect("body")
            .translation += Vec3::new(0.5, 0.25, 0.0);
        run_drive(&mut system, &mut app);
        assert_eq!(
            body_pose(&app, bodies[2]),
            rest[2],
            "the moved body went back on the pose"
        );
        // The rig moves 1 m along x: every body follows by the same offset.
        app.world_mut()
            .get_mut::<Transform>(rig)
            .expect("rig")
            .translation
            .x += 1.0;
        run_drive(&mut system, &mut app);
        for (body, (position, rotation)) in bodies.iter().zip(&rest) {
            let (now, now_rotation) = body_pose(&app, *body);
            assert!(
                (now - (*position + Vec3::X)).length() < 1e-5,
                "body {body:?} at {now} did not follow the rig from {position}"
            );
            assert!(now_rotation.angle_between(*rotation) < 1e-4);
        }
    }

    /// Audit framework4 finding 5 (budget): a fixed step with simulated ragdolls does not
    /// allocate the budget's candidate list once the system's buffers are warm.
    #[test]
    fn simulated_ragdolls_drive_without_allocating() {
        let (mut app, _) = ragdoll_scene(3, RagdollMode::Limp, 2);
        let mut system = drive_system(&mut app);
        run_drive(&mut system, &mut app);
        run_drive(&mut system, &mut app);
        let allocated = crate::test_alloc::count(|| run_drive(&mut system, &mut app));
        assert_eq!(
            allocated, 0,
            "a limp step with three ragdolls allocated {allocated} times"
        );
    }

    /// Timing of `drive_ragdolls` for 16 steady kinematic and 16 animated-pose rigs (run by
    /// hand with `--ignored --nocapture`; prints, asserts nothing about time).
    #[test]
    #[ignore = "manual bench: cargo test -p regia_framework --lib ragdoll::tests::bench_ -- --ignored --nocapture"]
    fn bench_drive_ragdolls_kinematic() {
        let (mut app, rigs) = ragdoll_scene(16, RagdollMode::Kinematic, 4);
        let mut system = drive_system(&mut app);
        for _ in 0..50 {
            run_drive(&mut system, &mut app);
        }
        let steps = 4000u32;
        let start = std::time::Instant::now();
        for _ in 0..steps {
            run_drive(&mut system, &mut app);
        }
        let steady = start.elapsed();
        // Moving rigs: the pose stamp changes every step, so every rig composes.
        let start = std::time::Instant::now();
        for step in 0..steps {
            for rig in &rigs {
                app.world_mut()
                    .get_mut::<Transform>(*rig)
                    .expect("rig")
                    .translation
                    .y = 0.2 + (step % 7) as f32 * 1e-3;
            }
            run_drive(&mut system, &mut app);
        }
        let moving = start.elapsed();
        eprintln!(
            "bench drive_ragdolls 16 rigs: steady {:.3} us/step, moving {:.3} us/step, allocs/step steady {}",
            steady.as_secs_f64() * 1e6 / f64::from(steps),
            moving.as_secs_f64() * 1e6 / f64::from(steps),
            crate::test_alloc::count(|| run_drive(&mut system, &mut app)),
        );
        // Same binary, same thread: five interleaved rounds of the steady case with the skip on
        // and off (`STEADY_SKIP_OFF`, tests only).
        for round in 0..5 {
            for off in [false, true] {
                STEADY_SKIP_OFF.with(|c| c.set(off));
                let start = std::time::Instant::now();
                for _ in 0..steps {
                    run_drive(&mut system, &mut app);
                }
                eprintln!(
                    "bench drive_ragdolls A/B round {round} skip {}: {:.3} us/step",
                    if off { "off" } else { "on" },
                    start.elapsed().as_secs_f64() * 1e6 / f64::from(steps)
                );
            }
        }
        STEADY_SKIP_OFF.with(|c| c.set(false));
    }

    /// Spec 93 § 5: ragdolls that went simulated on the same step freeze in entity-bits order,
    /// not in the order the query visits them. The lower-bits rig is moved to the end of its table
    /// (a component removed and inserted again), so iteration order and bits order disagree.
    #[test]
    fn budget_ties_break_by_entity_bits_not_query_order() {
        use regia_ecs::ecs::system::SystemState;
        let mut world = World::new();
        let skeleton = Arc::new(humanoid());
        let spawn_settled = |world: &mut World| {
            let mut ragdoll = Ragdoll::new(RagdollProfile::humanoid(&skeleton, 75.0));
            ragdoll.mode = RagdollMode::Limp;
            let bodies = RagdollBodies {
                applied: Some(RagdollMode::Limp),
                settled: true,
                simulated_since: Some(10),
                ..RagdollBodies::default()
            };
            let binding = spawn_skeleton(world, skeleton.clone());
            world
                .spawn((
                    Transform::default(),
                    binding,
                    Animator::single(AnimationClip::empty("idle", 1.0), true),
                    ragdoll,
                    bodies,
                ))
                .id()
        };
        let first = spawn_settled(&mut world);
        let second = spawn_settled(&mut world);
        let (low, high) = if first.to_bits() < second.to_bits() {
            (first, second)
        } else {
            (second, first)
        };
        let animator = world.entity_mut(low).take::<Animator>().expect("animator");
        world.entity_mut(low).insert(animator);

        let mut state: SystemState<RagdollRigQuery> = SystemState::new(&mut world);
        let rigs = state.get_mut(&mut world).expect("the rig query");
        let order: Vec<Entity> = rigs.iter().map(|(e, ..)| e).collect();
        assert_eq!(order, vec![high, low], "the query visits the higher bits first");
        let mut scratch = BudgetScratch::default();
        let mut freezes = |max: usize| -> Vec<Entity> {
            budget_freezes(&rigs, max, 20, &mut scratch);
            scratch.freeze.clone()
        };
        assert_eq!(freezes(1), vec![low], "the lower bits freeze first");
        assert_eq!(freezes(2), Vec::<Entity>::new(), "within budget");
        assert_eq!(freezes(0), vec![low, high]);
    }
}
