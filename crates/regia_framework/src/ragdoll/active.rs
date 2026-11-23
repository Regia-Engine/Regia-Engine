// SPDX-License-Identifier: MPL-2.0
//! Active ragdolls (`docs/specs/93-character-physics.md` § 10, ADR-2498, ADR-2564): per-bone PD motors with
//! joint-space targets from the animated pose, hit reactions that weaken a body group, a standing balance controller
//! and the get-up clip choice.
//!
//! [`RagdollMode::Active`](super::RagdollMode::Active) makes every body dynamic, like `Limp`. Each fixed step, every
//! body with a [`RagdollMotor`] gets a **joint drive**: rapier's implicit angular joint motor
//! (`regia_physics::pd::set_joint_drive`) pulling its rotation *relative to its parent body* towards the animated
//! pose's relative rotation, with the stiffness `kp` and damping `kd` scaled by the bone's group strength
//! ([`RagdollGroupStrength`] times the hit multiplier) and `max_torque` per axis. The solver works out the torque
//! together with the joints and the contacts, so large gains stay stable at the fixed step, and it acts equally and
//! oppositely on the child and the parent: the motors never add angular momentum to the ragdoll. No solver change.
//!
//! **Balance** (SIMBICON-style feedback, Yin, Loken and van de Panne 2007): the center of mass `c` comes from the
//! bodies' rapier mass properties, the support polygon is the convex hull of the touching contact points of the
//! [`RagdollBalanceRole::Ankle`] bodies. With `d` the horizontal offset of `c` from the ankles, less the same offset
//! in the animated pose (zero at the authored pose, and unaffected by which contacts exist), and `v` the horizontal
//! velocity of `c`, the legs' target leans by `cd * d + cv * v` (rad, clamped to `max_tilt`).
//!
//! With two feet down the lean splits along the line between them. Across the line the ankles take it: the foot's
//! length can carry that torque. Along the line the hips take it, by shifting the load between the feet, and the ankle
//! only keeps the foot flat (the shin's rotation about that axis is left free). The hips of a standing leg hold the
//! pelvis' world rotation (SIMBICON's torso servo) instead of a rotation relative to a thigh that may be leaning.
//!
//! **Planted feet.** A foot touching only fixed bodies is locked in place while the ragdoll stands
//! (`regia_physics::pd::set_body_locked`). A light foot under a whole body is the hardest case for an iterative solver
//! (at rapier's default 4 iterations a free foot rocks and creeps across the floor and the ragdoll eventually falls,
//! ADR-2564); locked, the solver sees an infinitely heavy anchor, and the rig holds at the default iterations. A locked
//! foot cannot tip or slip by itself, so what would have tipped it is applied as limits:
//!
//! - the ankle's torque is limited to what the sole could carry, the foot's share of the weight times the lever from
//!   the ankle to its farthest contact point about that axis, and the hips' to the weight times the polygon's reach. A
//!   large shove therefore stays momentum in the body instead of vanishing into the anchored foot.
//! - the fall test reads the **capture point** (Pratt et al. 2006), `c + v * sqrt(height / g)`: when it is more than
//!   `fall_margin` outside the polygon for `fall_time`, or no foot touches anything for that long, or the center of mass
//!   has sunk below `COLLAPSE_HEIGHT` of its animated height over the ankles (legs too weak to hold it), the ragdoll
//!   **falls**: the feet unlock, its motors drop to `fall_strength` (default 0, passive) and it settles like a limp one.
//!
//! On a body that can move (a crate, a lift) a foot stays free: its weight has to reach that body. Raise
//! `PhysicsSettings::solver_iterations` for ragdolls that must stand there.
//!
//! **Get-up**: when a ragdoll settles, the pelvis' facing ([`RagdollFacing`]) and the smallest squared rotation
//! distance of the key bones (rotations relative to the pelvis body) pick one of the authored [`RagdollGetUp`] clips.
//! The game plays it and switches the mode back to `Kinematic` or `Powered`; the existing recover path blends the
//! fallen pose into the clip.
//!
//! Zero gains (every `.ragdoll.ron` v2 file) are passive: an `Active` ragdoll with no driven motor behaves bit for
//! bit like a `Limp` one (no joint is ever touched, the settle watch runs as for `Limp`).

use regia_app::prelude::*;
use regia_core::math::{Mat4, Quat, Vec2, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::pd::{
    BodyDynamics, FootSupport, JointDrive, body_dynamics, body_pose, foot_support, joint_drive_torque,
    on_fixed_ground, point_velocity, rotation_error, touching_contact_points,
};

use super::stepping::{
    self, Leg, RagdollFallCause, RagdollRecovery, RagdollStepPhase, Swing, leg_rotations, swing_point,
    two_bone_ik,
};
use crate::detmath;

/// The body group a motor belongs to; hit reactions and per-group strength act on groups.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RagdollGroup {
    /// Pelvis, spine, chest, neck and head.
    #[default]
    Spine,
    /// Shoulders, arms and hands.
    Arms,
    /// Thighs, shins and feet.
    Legs,
}

impl RagdollGroup {
    /// Every group, in index order.
    pub const ALL: [RagdollGroup; 3] = [RagdollGroup::Spine, RagdollGroup::Arms, RagdollGroup::Legs];

    /// The group's protocol name (`spine`, `arms`, `legs`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollGroup::Spine => "spine",
            RagdollGroup::Arms => "arms",
            RagdollGroup::Legs => "legs",
        }
    }

    /// The group with protocol name `name`.
    pub fn from_name(name: &str) -> Option<RagdollGroup> {
        RagdollGroup::ALL.into_iter().find(|g| g.name() == name)
    }

    fn index(self) -> usize {
        match self {
            RagdollGroup::Spine => 0,
            RagdollGroup::Arms => 1,
            RagdollGroup::Legs => 2,
        }
    }
}

/// What the balance controller does with a motor's joint.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollBalanceRole {
    /// Not part of balance.
    #[default]
    None,
    /// A hip (the joint between the pelvis and a thigh): keeps the pelvis upright over the leaning leg.
    Hip,
    /// An ankle (the joint between a shin and a foot): the foot is a support body and the lean acts here.
    Ankle,
}

impl RagdollBalanceRole {
    /// The role's protocol name (`none`, `hip`, `ankle`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollBalanceRole::None => "none",
            RagdollBalanceRole::Hip => "hip",
            RagdollBalanceRole::Ankle => "ankle",
        }
    }
}

/// A PD motor on the joint between one ragdoll bone and its nearest simulated ancestor.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollMotor {
    /// The bone's skeleton joint (one of the profile's bones; the root bone has no parent and is never driven).
    pub joint: String,
    /// Strength group.
    pub group: RagdollGroup,
    /// Stiffness, N·m/rad (`>= 0`).
    pub kp: f32,
    /// Damping, N·m·s/rad (`>= 0`).
    pub kd: f32,
    /// Largest torque, N·m (`> 0`).
    pub max_torque: f32,
    /// Balance role of the joint.
    pub balance: RagdollBalanceRole,
}

impl RagdollMotor {
    /// Whether the motor drives at all (a gain above zero).
    pub fn drives(&self) -> bool {
        self.kp > 0.0 || self.kd > 0.0
    }
}

/// Authored strength of each group, `0..=1` (multiplies the gains).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollGroupStrength {
    /// Pelvis, spine and head.
    pub spine: f32,
    /// Arms.
    pub arms: f32,
    /// Legs.
    pub legs: f32,
}

impl Default for RagdollGroupStrength {
    fn default() -> Self {
        RagdollGroupStrength {
            spine: 1.0,
            arms: 1.0,
            legs: 1.0,
        }
    }
}

impl RagdollGroupStrength {
    /// The strength of `group`.
    pub fn get(&self, group: RagdollGroup) -> f32 {
        match group {
            RagdollGroup::Spine => self.spine,
            RagdollGroup::Arms => self.arms,
            RagdollGroup::Legs => self.legs,
        }
    }

    /// Sets the strength of `group`.
    pub fn set(&mut self, group: RagdollGroup, value: f32) {
        match group {
            RagdollGroup::Spine => self.spine = value,
            RagdollGroup::Arms => self.arms = value,
            RagdollGroup::Legs => self.legs = value,
        }
    }
}

/// How a [`RagdollHit`](super::RagdollHit) weakens the hit bone's group.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollHitResponse {
    /// Impulse (kg·m/s) that drops the group's strength to zero; a smaller one drops it in proportion.
    pub full_impulse: f32,
    /// Seconds (counted in fixed steps) for a weakened group to return to full strength, however deep the drop.
    pub recover_time: f32,
}

impl Default for RagdollHitResponse {
    fn default() -> Self {
        RagdollHitResponse {
            full_impulse: 200.0,
            recover_time: 1.0,
        }
    }
}

/// Balance controller tuning (module docs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RagdollBalance {
    /// Lean per metre of center-of-mass offset from the support center, rad/m (SIMBICON `cd`).
    pub cd: f32,
    /// Lean per m/s of center-of-mass velocity, rad/(m/s) (SIMBICON `cv`).
    pub cv: f32,
    /// Largest lean, rad.
    pub max_tilt: f32,
    /// Distance (m) the capture point may leave the support polygon before the ragdoll counts as falling.
    pub fall_margin: f32,
    /// Seconds (counted in fixed steps) outside the margin, or with no foot contact, before it falls.
    pub fall_time: f32,
    /// Motor strength left while falling and lying (0 = passive).
    pub fall_strength: f32,
}

impl Default for RagdollBalance {
    fn default() -> Self {
        RagdollBalance {
            cd: 1.0,
            cv: 0.3,
            max_tilt: 0.35,
            fall_margin: 0.05,
            fall_time: 0.1,
            fall_strength: 0.0,
        }
    }
}

/// How a settled ragdoll's pelvis lies (from its forward and left axes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollFacing {
    /// On its back: the front points up.
    #[default]
    FaceUp,
    /// On its front: the front points down.
    FaceDown,
    /// On its left side (the left points down).
    SideLeft,
    /// On its right side (the left points up).
    SideRight,
}

impl RagdollFacing {
    /// The facing's protocol name (`face_up`, `face_down`, `side_left`, `side_right`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollFacing::FaceUp => "face_up",
            RagdollFacing::FaceDown => "face_down",
            RagdollFacing::SideLeft => "side_left",
            RagdollFacing::SideRight => "side_right",
        }
    }

    /// The facing of a pelvis whose forward and left axes (world space) are `forward` and `left`: the axis that
    /// points more nearly up or down decides (a tie goes to face up/down).
    pub fn classify(forward: Vec3, left: Vec3) -> RagdollFacing {
        if forward.y.abs() >= left.y.abs() {
            if forward.y > 0.0 {
                RagdollFacing::FaceUp
            } else {
                RagdollFacing::FaceDown
            }
        } else if left.y > 0.0 {
            RagdollFacing::SideRight
        } else {
            RagdollFacing::SideLeft
        }
    }
}

/// An authored get-up clip: its name (the game plays it), the facing it starts from and its first frame's key-bone
/// rotations relative to the pelvis (root) joint.
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollGetUp {
    /// Clip name.
    pub clip: String,
    /// The facing the clip starts from.
    pub facing: RagdollFacing,
    /// Key bones: joint name and the joint's world rotation relative to the root joint's (`root⁻¹ · joint`).
    pub pose: Vec<(String, Quat)>,
}

impl RagdollGetUp {
    /// Key-bone rotations relative to `root` from joint globals (for example the first frame of a clip:
    /// `skeleton.globals(&pose, None)`); joints that are not in the skeleton are skipped.
    pub fn key_pose(
        skeleton: &regia_animation::Skeleton,
        globals: &[Mat4],
        root: &str,
        joints: &[&str],
    ) -> Vec<(String, Quat)> {
        let rotation = |j: u32| {
            globals
                .get(j as usize)
                .and_then(|m| super::try_decompose(*m))
                .map(|(_, r)| r)
        };
        let Some(root) = skeleton.joint_index(root).and_then(rotation) else {
            return Vec::new();
        };
        joints
            .iter()
            .filter_map(|name| {
                let r = skeleton.joint_index(name).and_then(rotation)?;
                Some(((*name).to_string(), (root.inverse() * r).normalize()))
            })
            .collect()
    }
}

/// The squared pose distance: the sum over `clip`'s key bones found in `current` of the squared angle between the
/// two relative rotations. Bones missing from `current` are skipped.
pub fn get_up_distance(clip: &[(String, Quat)], current: &[(String, Quat)]) -> f32 {
    clip.iter()
        .filter_map(|(name, want)| {
            let have = current.iter().find(|(n, _)| n == name)?.1;
            Some(rotation_error(have, *want).length_squared())
        })
        .sum()
}

/// The get-up clip for a pose (module docs): among the clips authored for `facing` (all clips when none is), the one
/// with the smallest [`get_up_distance`]; ties go to the first authored. `None` when there are no clips.
pub fn pick_get_up(
    clips: &[RagdollGetUp],
    facing: RagdollFacing,
    current: &[(String, Quat)],
) -> Option<usize> {
    let any_facing = clips.iter().any(|c| c.facing == facing);
    let mut best: Option<(usize, f32)> = None;
    for (index, clip) in clips.iter().enumerate() {
        if any_facing && clip.facing != facing {
            continue;
        }
        let d = get_up_distance(&clip.pose, current);
        if best.is_none_or(|(_, b)| d < b) {
            best = Some((index, d));
        }
    }
    best.map(|(i, _)| i)
}

/// The active-ragdoll part of a profile (`.ragdoll.ron` `active` block since v3, `recovery` since v4; empty in
/// v1/v2 files: zero gains).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RagdollActiveProfile {
    /// Motors, at most one per bone.
    pub motors: Vec<RagdollMotor>,
    /// Authored strength per group.
    pub strength: RagdollGroupStrength,
    /// Hit reaction.
    pub hit: RagdollHitResponse,
    /// Balance tuning (balance runs when a motor has the `Ankle` role).
    pub balance: RagdollBalance,
    /// Get-up clips.
    pub get_up: Vec<RagdollGetUp>,
    /// Stepping, landing and planting on moving bodies (ADR-2963); all off by default.
    pub recovery: RagdollRecovery,
}

impl RagdollActiveProfile {
    /// Whether any motor drives (the ragdoll can stand); `false` means an `Active` ragdoll is passive.
    pub fn drives(&self) -> bool {
        self.motors.iter().any(RagdollMotor::drives)
    }

    /// The motor of `joint`, if any.
    pub fn motor(&self, joint: &str) -> Option<&RagdollMotor> {
        self.motors.iter().find(|m| m.joint == joint)
    }
}

/// What an active ragdoll is doing (`ragdoll.state`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum RagdollPhase {
    /// The animation drives it (`Kinematic` or `Powered`).
    #[default]
    Animated,
    /// `Active` and standing: motors and balance run.
    Active,
    /// Simulated and not at rest: an `Active` ragdoll that lost its balance (or has no driving motor), or a `Limp`
    /// one.
    Falling,
    /// Simulated and at rest (settled).
    Passive,
    /// Back to the animation and still blending the fallen pose out (the recover path).
    GettingUp,
}

impl RagdollPhase {
    /// The phase's protocol name (`animated`, `active`, `falling`, `passive`, `getting_up`).
    pub fn name(self) -> &'static str {
        match self {
            RagdollPhase::Animated => "animated",
            RagdollPhase::Active => "active",
            RagdollPhase::Falling => "falling",
            RagdollPhase::Passive => "passive",
            RagdollPhase::GettingUp => "getting_up",
        }
    }
}

/// Runtime state of the active controller, part of [`RagdollBodies`](super::RagdollBodies)
/// (`RagdollBodies::active`).
#[derive(Clone, Debug, PartialEq)]
pub struct RagdollActiveState {
    phase: RagdollPhase,
    /// Hit multiplier per group right after the last hit.
    hit_low: [f32; 3],
    /// Fixed steps since that hit (`None`: at full strength).
    hit_steps: [Option<u32>; 3],
    /// Seconds a weakened group takes to recover (from the profile at the hit).
    hit_recover: [f32; 3],
    /// Center of mass minus the support center (horizontal), last step.
    com_offset: Vec3,
    /// Center of mass, last step.
    center_of_mass: Vec3,
    /// Distance of the center of mass outside the support polygon (`<= 0` inside), `None` when unsupported or balance
    /// is off.
    support_distance: Option<f32>,
    /// Number of support contact points last step.
    support_points: usize,
    /// Consecutive steps outside the margin or unsupported.
    outside_steps: u32,
    /// The lean the balance controller applied last step (axis times angle, rad).
    tilt: Vec3,
    /// Motors that drove last step.
    driven: u32,
    /// Largest torque (N·m) applied last step.
    peak_torque: f32,
    /// Facing when it last settled.
    facing: Option<RagdollFacing>,
    /// The chosen get-up clip (index into the profile's `get_up`) and its name.
    get_up: Option<(usize, String)>,
    /// Feet locked to fixed ground in the last step.
    planted_feet: u32,
    /// Distance of the capture point outside the support polygon (`<= 0` inside), `None` when unsupported.
    capture_distance: Option<f32>,
    /// Whether joint drives are set in rapier.
    pub(super) drive_on: bool,
    /// Motor index per body, resolved for `motor_joints`.
    motor_of_body: Vec<Option<usize>>,
    motor_joints: Vec<String>,
    /// The rig's legs (thigh, shin, foot), resolved with the motors.
    legs: Vec<Leg>,
    /// What the recovery layer does.
    step_phase: RagdollStepPhase,
    /// Steps in the current recovery (back to 0 after a calm second).
    steps: u32,
    /// Steps since the ragdoll entered `Active`.
    total_steps: u32,
    /// The swing in progress.
    swing: Option<Swing>,
    /// The feet stand where steps or a landing put them (the stance comes from them, not from the animation).
    stepped: bool,
    /// Consecutive steps standing with the capture point inside the polygon.
    calm_steps: u32,
    /// Fixed steps airborne.
    air_steps: u32,
    /// Fixed steps since the landing touch.
    land_steps: u32,
    /// Why it last fell (cleared when it enters `Active`).
    fall_cause: Option<RagdollFallCause>,
    /// The body the first planted (or touching) foot stands on.
    support_body: Option<Entity>,
    /// Per body: the foot's anchor on a body that can move (support entity, foot pose in its frame, dynamic).
    pub(super) anchors: Vec<Option<FootAnchor>>,
    /// Weight (N) each planted foot carries, last step.
    pub(super) foot_weight: f32,
}

/// A foot planted on a body that can move (module docs of `stepping`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct FootAnchor {
    /// The support body's entity.
    pub support: Entity,
    /// The foot's position in the support's frame when it planted.
    pub position: Vec3,
    /// The foot's rotation in the support's frame when it planted.
    pub rotation: Quat,
    /// The support takes forces (it gets the foot's weight share).
    pub dynamic: bool,
}

impl Default for RagdollActiveState {
    fn default() -> Self {
        RagdollActiveState {
            phase: RagdollPhase::Animated,
            hit_low: [1.0; 3],
            hit_steps: [None; 3],
            hit_recover: [0.0; 3],
            com_offset: Vec3::ZERO,
            center_of_mass: Vec3::ZERO,
            support_distance: None,
            support_points: 0,
            outside_steps: 0,
            tilt: Vec3::ZERO,
            driven: 0,
            peak_torque: 0.0,
            facing: None,
            get_up: None,
            planted_feet: 0,
            capture_distance: None,
            drive_on: false,
            motor_of_body: Vec::new(),
            motor_joints: Vec::new(),
            legs: Vec::new(),
            step_phase: RagdollStepPhase::Stand,
            steps: 0,
            total_steps: 0,
            swing: None,
            stepped: false,
            calm_steps: 0,
            air_steps: 0,
            land_steps: 0,
            fall_cause: None,
            support_body: None,
            anchors: Vec::new(),
            foot_weight: 0.0,
        }
    }
}

impl RagdollActiveState {
    /// The current phase.
    pub fn phase(&self) -> RagdollPhase {
        self.phase
    }

    /// The hit multiplier of `group` (`0..=1`; 1 = full strength), at the last fixed step.
    pub fn hit_strength(&self, group: RagdollGroup, dt: f32) -> f32 {
        let g = group.index();
        match self.hit_steps[g] {
            None => 1.0,
            Some(steps) => {
                let recover = self.hit_recover[g];
                let age = steps as f32 * dt;
                if recover <= 0.0 || age >= recover {
                    1.0
                } else {
                    self.hit_low[g] + (1.0 - self.hit_low[g]) * (age / recover)
                }
            }
        }
    }

    /// Center of mass minus the support polygon's center, horizontal (m), at the last fixed step.
    pub fn com_offset(&self) -> Vec3 {
        self.com_offset
    }

    /// The world center of mass at the last fixed step (zero before the first active step).
    pub fn center_of_mass(&self) -> Vec3 {
        self.center_of_mass
    }

    /// How far the center of mass was outside the support polygon (`<= 0` inside); `None` with no support.
    pub fn support_distance(&self) -> Option<f32> {
        self.support_distance
    }

    /// How far the capture point was outside the support polygon (`<= 0` inside); `None` with no support.
    pub fn capture_distance(&self) -> Option<f32> {
        self.capture_distance
    }

    /// Feet locked to fixed ground in the last step.
    pub fn planted_feet(&self) -> u32 {
        self.planted_feet
    }

    /// Number of foot contact points in the last support polygon.
    pub fn support_points(&self) -> usize {
        self.support_points
    }

    /// The balance lean of the last step (axis times angle, rad).
    pub fn tilt(&self) -> Vec3 {
        self.tilt
    }

    /// Motors that drove in the last step.
    pub fn driven_motors(&self) -> u32 {
        self.driven
    }

    /// Largest motor torque of the last step, N·m.
    pub fn peak_torque(&self) -> f32 {
        self.peak_torque
    }

    /// How the pelvis lay when the ragdoll last settled.
    pub fn facing(&self) -> Option<RagdollFacing> {
        self.facing
    }

    /// The chosen get-up clip's name (set when the ragdoll settles; cleared when it moves again or the mode changes).
    pub fn get_up_clip(&self) -> Option<&str> {
        self.get_up.as_ref().map(|(_, n)| n.as_str())
    }

    /// What the recovery layer is doing (`Stand` whenever recovery is off).
    pub fn step_phase(&self) -> RagdollStepPhase {
        self.step_phase
    }

    /// Steps taken in the current recovery (back to 0 after a calm second of standing).
    pub fn steps(&self) -> u32 {
        self.steps
    }

    /// Steps taken since the ragdoll entered `Active`.
    pub fn total_steps(&self) -> u32 {
        self.total_steps
    }

    /// Where the swing foot's origin should touch down, while a foot swings.
    pub fn swing_target(&self) -> Option<Vec3> {
        self.swing.map(|s| s.target)
    }

    /// Why the ragdoll last fell (`None` while it has not fallen since it entered `Active`).
    pub fn fall_cause(&self) -> Option<RagdollFallCause> {
        self.fall_cause
    }

    /// The body the planted (or first touching) foot stands on, last step.
    pub fn support_body(&self) -> Option<Entity> {
        self.support_body
    }

    /// Weakens `group` after a hit of `impulse` kg·m/s (module docs).
    pub(super) fn hit(&mut self, group: RagdollGroup, impulse: f32, response: &RagdollHitResponse, dt: f32) {
        let g = group.index();
        let full = response.full_impulse.max(1e-3);
        let drop = (impulse.max(0.0) / full).min(1.0);
        let now = self.hit_strength(group, dt);
        let low = now.min(1.0 - drop);
        if low >= 1.0 {
            return;
        }
        self.hit_low[g] = low;
        self.hit_steps[g] = Some(0);
        self.hit_recover[g] = response.recover_time.max(0.0);
    }

    /// Back to standing with no step, swing, landing or anchor.
    fn reset_recovery(&mut self) {
        self.step_phase = RagdollStepPhase::Stand;
        self.steps = 0;
        self.swing = None;
        self.stepped = false;
        self.calm_steps = 0;
        self.air_steps = 0;
        self.land_steps = 0;
        self.anchors.clear();
        self.foot_weight = 0.0;
    }

    /// Whether the active controller holds the ragdoll up (settling is suspended).
    pub(super) fn holding(&self) -> bool {
        self.phase == RagdollPhase::Active
    }
}

/// Group strength times hit multiplier times the phase factor for a motor.
fn motor_strength(
    profile: &RagdollActiveProfile,
    state: &RagdollActiveState,
    motor: &RagdollMotor,
    dt: f32,
) -> f32 {
    let phase = match state.phase {
        RagdollPhase::Active => 1.0,
        RagdollPhase::Falling | RagdollPhase::Passive => profile.balance.fall_strength,
        RagdollPhase::Animated | RagdollPhase::GettingUp => 0.0,
    };
    let s = profile.strength.get(motor.group) * state.hit_strength(motor.group, dt) * phase;
    if s.is_finite() { s.max(0.0) } else { 0.0 }
}

/// Factor on the authored damping of the joints that carry the body while a recovery is under way (ADR-2963: the
/// landing stands with 2 to 4 from 24 solver iterations; 3 is the middle).
const RECOVERY_DAMPING: f32 = 3.0;

/// Largest drop (m) of the stance hip while the other foot swings (the stance knee bends so the swing leg can reach).
const STANCE_DROP: f32 = 0.05;

/// The ragdoll has collapsed when its center of mass is below this fraction of its animated height over the ankles.
const COLLAPSE_HEIGHT: f32 = 0.7;

/// Convex hull (counter-clockwise in x/z, seen from above) of `points` projected on the ground plane; duplicates
/// removed. Order independent: the points are sorted first.
pub fn support_hull(points: &[Vec3]) -> Vec<Vec2> {
    let mut projected = Vec::new();
    let mut hull = Vec::new();
    support_hull_into(points, &mut projected, &mut hull);
    hull
}

/// Fills `hull` the way [`support_hull`] does, reusing `projected` and `hull`.
fn support_hull_into(points: &[Vec3], projected: &mut Vec<Vec2>, hull: &mut Vec<Vec2>) {
    projected.clear();
    projected.extend(
        points
            .iter()
            .map(|v| Vec2::new(v.x, v.z))
            .filter(|v| v.is_finite()),
    );
    projected.sort_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
    projected.dedup_by(|a, b| (*a - *b).length_squared() < 1e-10);
    hull.clear();
    if projected.len() < 3 {
        hull.extend_from_slice(projected);
        return;
    }
    let cross = |o: Vec2, a: Vec2, b: Vec2| (a - o).perp_dot(b - o);
    for &q in projected.iter() {
        while hull.len() >= 2 && cross(hull[hull.len() - 2], hull[hull.len() - 1], q) <= 0.0 {
            hull.pop();
        }
        hull.push(q);
    }
    let lower = hull.len() + 1;
    for &q in projected.iter().rev().skip(1) {
        while hull.len() >= lower && cross(hull[hull.len() - 2], hull[hull.len() - 1], q) <= 0.0 {
            hull.pop();
        }
        hull.push(q);
    }
    hull.pop();
}

/// Signed distance of `point` from a convex counter-clockwise polygon: negative inside (minus the distance to the
/// nearest edge), positive outside. A one- or two-point polygon is a point or segment (distance, never negative).
/// `None` for an empty polygon.
pub fn support_distance(hull: &[Vec2], point: Vec2) -> Option<f32> {
    let segment = |a: Vec2, b: Vec2| {
        let ab = b - a;
        let t = if ab.length_squared() > 0.0 {
            ((point - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        (point - (a + ab * t)).length()
    };
    match hull.len() {
        0 => None,
        1 => Some((point - hull[0]).length()),
        2 => Some(segment(hull[0], hull[1])),
        n => {
            let mut nearest = f32::INFINITY;
            let mut inside = true;
            for (i, &a) in hull.iter().enumerate() {
                let b = hull[(i + 1) % n];
                if (b - a).perp_dot(point - a) < 0.0 {
                    inside = false;
                }
                nearest = nearest.min(segment(a, b));
            }
            Some(if inside { -nearest } else { nearest })
        }
    }
}

/// The point of a convex counter-clockwise polygon nearest to `point` (`point` itself inside); `None` for an empty one.
pub fn nearest_on_hull(hull: &[Vec2], point: Vec2) -> Option<Vec2> {
    if support_distance(hull, point)? <= 0.0 {
        return Some(point);
    }
    let n = hull.len();
    let mut best: Option<(f32, Vec2)> = None;
    for (i, &a) in hull.iter().enumerate() {
        let b = hull[(i + 1) % n];
        let ab = b - a;
        let t = if ab.length_squared() > 0.0 {
            ((point - a).dot(ab) / ab.length_squared()).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let q = a + ab * t;
        let d = (point - q).length_squared();
        if best.is_none_or(|(e, _)| d < e) {
            best = Some((d, q));
        }
    }
    best.map(|(_, q)| q)
}

/// Per-body input of one active step.
pub(super) struct ActiveBody {
    /// Index of the parent body.
    pub parent: Option<usize>,
    /// Skeleton joint.
    pub joint: u32,
    /// Body entity.
    pub entity: Entity,
    /// Current world position of the body's origin (its joint).
    pub position: Vec3,
    /// Current world rotation of the body.
    pub rotation: Quat,
    /// Animated world rotation (the target), when the animation has a usable one.
    pub target: Option<Quat>,
    /// Animated world position (the target's origin), with `target`.
    pub target_position: Vec3,
}

/// One rig's input to [`step_active`].
pub(super) struct ActiveStep<'a> {
    pub profile: &'a RagdollActiveProfile,
    pub skeleton: &'a regia_animation::Skeleton,
    pub bodies: &'a [ActiveBody],
    pub physics: Option<&'a PhysicsWorld>,
    pub dt: f32,
    /// `PhysicsSettings::solver_iterations` of the world.
    pub solver_iterations: usize,
    /// Length of the world's gravity, m/s².
    pub gravity: f32,
    /// The mode is `Active` and the bodies' kinds are already dynamic in rapier (not the switching step).
    pub drive: bool,
    /// The root body's forward axis in its own frame (`RagdollBodies::root_forward`).
    pub root_forward: Vec3,
    /// The root body's up axis in its own frame.
    pub root_up: Vec3,
}

/// Resolves the motor of every body (by joint name) when the motor list changed.
fn resolve_motors(
    state: &mut RagdollActiveState,
    profile: &RagdollActiveProfile,
    skeleton: &regia_animation::Skeleton,
    bodies: &[ActiveBody],
) {
    let same = state.motor_of_body.len() == bodies.len()
        && state.motor_joints.len() == profile.motors.len()
        && state
            .motor_joints
            .iter()
            .zip(&profile.motors)
            .all(|(a, m)| *a == m.joint);
    if same {
        return;
    }
    state.motor_joints = profile.motors.iter().map(|m| m.joint.clone()).collect();
    state.motor_of_body = bodies
        .iter()
        .map(|b| {
            profile
                .motors
                .iter()
                .position(|m| skeleton.joint_index(&m.joint) == Some(b.joint))
        })
        .collect();
    // Legs: every `Ankle` body whose parent (shin) has a parent (thigh) that is not the root.
    state.legs = (0..bodies.len())
        .filter(|&foot| {
            state.motor_of_body[foot].map(|m| profile.motors[m].balance) == Some(RagdollBalanceRole::Ankle)
        })
        .filter_map(|foot| {
            let shin = bodies[foot].parent?;
            let thigh = bodies[shin].parent?;
            bodies[thigh].parent?;
            // `target_position` is only meaningful with a `target` (it is zero when the animated
            // joint did not decompose): a body without one is measured where it is, which the joint
            // constraints keep a real bone length (rc1 physics review, finding 14).
            let origin = |b: &ActiveBody| {
                if b.target.is_some() {
                    b.target_position
                } else {
                    b.position
                }
            };
            let upper = (origin(&bodies[shin]) - origin(&bodies[thigh])).length();
            let lower = (origin(&bodies[foot]) - origin(&bodies[shin])).length();
            (upper.is_finite() && lower.is_finite() && upper > 1e-3 && lower > 1e-3).then_some(Leg {
                thigh,
                shin,
                foot,
                upper,
                lower,
            })
        })
        .collect();
}

/// Advances the hit recovery clocks by one fixed step.
pub(super) fn tick_hits(state: &mut RagdollActiveState, dt: f32) {
    let clocks = state
        .hit_steps
        .iter_mut()
        .zip(&mut state.hit_low)
        .zip(&state.hit_recover);
    for ((clock, low), recover) in clocks {
        if let Some(steps) = *clock {
            let next = steps.saturating_add(1);
            if next as f32 * dt >= *recover {
                *clock = None;
                *low = 1.0;
            } else {
                *clock = Some(next);
            }
        }
    }
}

/// Phase bookkeeping that every ragdoll gets each fixed step (module docs); `entered_active` is the step the mode
/// switched into `Active`.
pub(super) fn update_phase(
    state: &mut RagdollActiveState,
    mode: super::RagdollMode,
    entered_active: bool,
    profile: &RagdollActiveProfile,
    settled: bool,
    recovering: bool,
) {
    use super::RagdollMode as M;
    let before = state.phase;
    state.phase = match mode {
        M::Kinematic | M::Powered => {
            if recovering {
                RagdollPhase::GettingUp
            } else {
                RagdollPhase::Animated
            }
        }
        M::Limp => {
            if settled {
                RagdollPhase::Passive
            } else {
                RagdollPhase::Falling
            }
        }
        M::Active => {
            if entered_active {
                state.outside_steps = 0;
                state.reset_recovery();
                state.fall_cause = None;
                state.total_steps = 0;
                if profile.drives() {
                    RagdollPhase::Active
                } else {
                    RagdollPhase::Falling
                }
            } else {
                match state.phase {
                    RagdollPhase::Active => RagdollPhase::Active,
                    _ if settled => RagdollPhase::Passive,
                    _ => RagdollPhase::Falling,
                }
            }
        }
    };
    if state.phase != RagdollPhase::Passive && before == RagdollPhase::Passive {
        state.get_up = None;
        state.facing = None;
    }
    if matches!(state.phase, RagdollPhase::Animated | RagdollPhase::Active) {
        state.get_up = None;
        state.facing = None;
    }
    if !matches!(state.phase, RagdollPhase::Active) {
        state.tilt = Vec3::ZERO;
    }
    if mode != M::Active {
        state.driven = 0;
        state.peak_torque = 0.0;
        state.planted_feet = 0;
        state.capture_distance = None;
        state.support_body = None;
    }
    if state.phase != RagdollPhase::Active {
        state.reset_recovery();
    }
}

/// Picks the get-up clip of a ragdoll that just settled (module docs).
pub(super) fn choose_get_up(
    state: &mut RagdollActiveState,
    profile: &RagdollActiveProfile,
    skeleton: &regia_animation::Skeleton,
    bodies: &[ActiveBody],
    root_forward: Vec3,
    root_up: Vec3,
) {
    let Some(root) = bodies.iter().find(|b| b.parent.is_none()) else {
        return;
    };
    let forward = root.rotation * root_forward;
    let left = root.rotation * root_up.cross(root_forward);
    let facing = RagdollFacing::classify(forward, left);
    state.facing = Some(facing);
    let current: Vec<(String, Quat)> = bodies
        .iter()
        .filter(|b| b.parent.is_some())
        .filter_map(|b| {
            let name = skeleton.joint_names.get(b.joint as usize)?;
            Some((name.clone(), (root.rotation.inverse() * b.rotation).normalize()))
        })
        .collect();
    state.get_up =
        pick_get_up(&profile.get_up, facing, &current).map(|i| (i, profile.get_up[i].clip.clone()));
}

/// The rotation that leans a leg by `lean` (a horizontal vector, its length in rad): the leg's lower end moves along it.
fn lean_rotation(lean: Vec3) -> Quat {
    let angle = lean.length();
    let axis = lean.cross(Vec3::Y).normalize_or_zero();
    if angle.is_finite() && angle > 0.0 && axis != Vec3::ZERO {
        Quat::from_axis_angle(axis, angle)
    } else {
        Quat::IDENTITY
    }
}

/// The part of rotation `q` about the unit `axis` (the twist of a swing-twist split).
fn twist_about(q: Quat, axis: Vec3) -> Quat {
    let along = axis * Vec3::new(q.x, q.y, q.z).dot(axis);
    let twist = Quat::from_xyzw(along.x, along.y, along.z, q.w);
    if twist.length_squared() > 1e-8 {
        twist.normalize()
    } else {
        Quat::IDENTITY
    }
}

/// Whether body `j` is body `root` or below it.
fn in_subtree(bodies: &[ActiveBody], root: usize, j: usize) -> bool {
    let mut cursor = Some(j);
    while let Some(c) = cursor {
        if c == root {
            return true;
        }
        cursor = bodies.get(c).and_then(|b| b.parent);
    }
    false
}

/// Scratch buffers for one [`step_active`] call. `drive_ragdolls` keeps one per rig across fixed steps.
#[derive(Default)]
pub(super) struct ActiveWork {
    /// Joint drive per body, index-aligned with the step's bodies.
    pub(super) drives: Vec<Option<JointDrive>>,
    /// Whether each foot is locked this step.
    pub(super) foot_locks: Vec<bool>,
    dynamics: Vec<Option<BodyDynamics>>,
    own: Vec<Entity>,
    points: Vec<Vec3>,
    supported: Vec<bool>,
    levers: Vec<Vec3>,
    moving: Vec<Option<FootSupport>>,
    soles: Vec<Option<Vec3>>,
    touch: Vec<Vec3>,
    feet: Vec<Vec3>,
    leg_targets: Vec<Option<Quat>>,
    free_legs: Vec<usize>,
    projected: Vec<Vec2>,
    hull: Vec<Vec2>,
    legs: Vec<Leg>,
    down: Vec<usize>,
    planted: Vec<usize>,
}

/// The per-rig body list and [`ActiveWork`] reused between fixed steps.
#[derive(Default)]
pub(super) struct ActiveScratch {
    /// Bodies of the rig this step.
    pub(super) list: Vec<ActiveBody>,
    /// Motor and balance scratch.
    pub(super) work: ActiveWork,
}

/// One fixed step of the motors and the balance controller for one rig (module docs). Writes the joint drive per
/// body into `work.drives` (index-aligned with `step.bodies`); all `None` when nothing drives. `work.foot_locks`
/// matches that order.
#[tracing::instrument(skip_all, target = "regia_framework")]
pub(super) fn step_active(state: &mut RagdollActiveState, step: &ActiveStep, work: &mut ActiveWork) {
    let n = step.bodies.len();
    let ActiveWork {
        drives,
        foot_locks,
        dynamics,
        own,
        points,
        supported,
        levers,
        moving,
        soles,
        touch,
        feet,
        leg_targets,
        free_legs,
        projected,
        hull,
        legs,
        down,
        planted,
    } = work;
    drives.clear();
    drives.resize(n, None);
    foot_locks.clear();
    foot_locks.resize(n, false);
    state.driven = 0;
    state.peak_torque = 0.0;
    resolve_motors(state, step.profile, step.skeleton, step.bodies);
    let Some(physics) = step.physics else {
        return;
    };
    if !step.drive
        || !matches!(
            state.phase,
            RagdollPhase::Active | RagdollPhase::Falling | RagdollPhase::Passive
        )
    {
        return;
    }
    let substep_dt = step.dt / step.solver_iterations.max(1) as f32;
    let recovery = step.profile.recovery;
    dynamics.clear();
    dynamics.extend(step.bodies.iter().map(|b| body_dynamics(physics, b.entity)));
    let ankle_role = |state: &RagdollActiveState, index: usize| {
        state
            .motor_of_body
            .get(index)
            .copied()
            .flatten()
            .map(|m| step.profile.motors[m].balance)
            == Some(RagdollBalanceRole::Ankle)
    };
    // The swinging foot is not a support while it swings; its touch ends the swing.
    let swing_foot = state
        .swing
        .and_then(|s| state.legs.get(s.leg))
        .map(|leg| leg.foot);
    // Foot contacts: the support polygon, which feet stand on something and which of those plant (module docs).
    own.clear();
    own.extend(step.bodies.iter().map(|b| b.entity));
    points.clear();
    supported.clear();
    supported.resize(n, false);
    levers.clear();
    levers.resize(n, Vec3::ZERO);
    moving.clear();
    moving.resize(n, None);
    let mut swing_touch = false;
    let mut support_body = None;
    // Per foot: the centroid of its touching contact points (where its sole carries).
    soles.clear();
    soles.resize(n, None);
    for (index, body) in step.bodies.iter().enumerate() {
        if !ankle_role(state, index) {
            continue;
        }
        if swing_foot == Some(index) && state.phase == RagdollPhase::Active {
            touch.clear();
            touching_contact_points(physics, body.entity, own.as_slice(), touch);
            swing_touch = !touch.is_empty();
            continue;
        }
        let before = points.len();
        touching_contact_points(physics, body.entity, own.as_slice(), points);
        supported[index] = points.len() > before;
        if supported[index] {
            let touching = &points[before..];
            soles[index] = Some(touching.iter().copied().sum::<Vec3>() / touching.len() as f32);
        }
        let support = if recovery.any() && supported[index] {
            foot_support(physics, body.entity, own.as_slice())
        } else {
            None
        };
        if support_body.is_none() {
            support_body = support.and_then(|s| s.body);
        }
        let plants = state.phase == RagdollPhase::Active && supported[index];
        foot_locks[index] = plants && on_fixed_ground(physics, body.entity, own.as_slice());
        if plants
            && !foot_locks[index]
            && recovery.moving_support
            && let Some(s) = support.filter(|s| s.single && s.body.is_some())
        {
            foot_locks[index] = true;
            moving[index] = Some(s);
        }
        for point in &points[before..] {
            let r = *point - body.position;
            levers[index] = levers[index].max(Vec3::new(r.z.abs(), 0.0, r.x.abs()));
        }
    }
    state.support_body = support_body;
    // Anchors in the frame of a support that can move: kept while the foot stays planted on the same body.
    if recovery.moving_support {
        state.anchors.resize(n, None);
        for (index, support) in moving.iter().enumerate() {
            let Some(support) = support.and_then(|s| s.body.map(|e| (e, s.dynamic))) else {
                state.anchors[index] = None;
                continue;
            };
            if state.anchors[index].is_some_and(|a| a.support == support.0) {
                continue;
            }
            let body = &step.bodies[index];
            state.anchors[index] = body_pose(physics, support.0).map(|(p, r)| {
                let inverse = r.inverse();
                FootAnchor {
                    support: support.0,
                    position: inverse * (body.position - p),
                    rotation: (inverse * body.rotation).normalize(),
                    dynamic: support.1,
                }
            });
        }
    }

    // Balance (phase Active only): center of mass, support polygon, lean, fall test.
    // With two feet down the lean splits along the line between them (module docs).
    feet.clear();
    feet.extend((0..n).filter(|&j| supported[j]).map(|j| step.bodies[j].position));
    let stance = match feet.as_slice() {
        [first, .., last] => Some(Vec3::new(last.x - first.x, 0.0, last.z - first.z))
            .filter(|line| line.length_squared() > 1e-4)
            .map(Vec3::normalize),
        _ => None,
    };
    let mut lean_sideways = Quat::IDENTITY;
    let mut lean_forward = Quat::IDENTITY;
    let mut foot_force = 0.0;
    let mut hip_limit = f32::INFINITY;
    // Leg targets the recovery layer replaces (index-aligned with the bodies; `None` keeps the animated target).
    leg_targets.clear();
    leg_targets.resize(n, None);
    // Hips whose target is relative to the pelvis' current rotation (a swinging or reaching leg), and their feet.
    free_legs.clear();
    let balance_on = state.phase == RagdollPhase::Active
        && step
            .profile
            .motors
            .iter()
            .any(|m| m.balance == RagdollBalanceRole::Ankle);
    if balance_on {
        let _span = tracing::debug_span!(target: "regia_framework", "ragdoll_balance").entered();
        let mut mass = 0.0;
        let mut weighted = Vec3::ZERO;
        let mut momentum = Vec3::ZERO;
        for d in dynamics.iter().flatten() {
            mass += d.mass;
            weighted += d.center_of_mass * d.mass;
            momentum += d.linear * d.mass;
        }
        let (com, mut com_velocity) = if mass > 0.0 {
            (weighted / mass, momentum / mass)
        } else {
            (Vec3::ZERO, Vec3::ZERO)
        };
        // On a support that moves, the capture point reads the velocity relative to it.
        if recovery.moving_support {
            let (mut sum, mut count) = (Vec3::ZERO, 0.0_f32);
            for (index, anchor) in state.anchors.iter().enumerate() {
                if let Some(a) = anchor
                    && let Some(v) = point_velocity(physics, a.support, step.bodies[index].position)
                {
                    sum += v;
                    count += 1.0;
                }
            }
            if count > 0.0 {
                com_velocity -= sum / count;
            }
        }
        // The same center of mass and ankle center in the animated pose: the feedback is zero at the authored pose.
        let (mut ankles, mut animated_ankles, mut count) = (Vec3::ZERO, Vec3::ZERO, 0.0_f32);
        let (mut animated_com, mut animated_mass) = (Vec3::ZERO, 0.0_f32);
        for (index, body) in step.bodies.iter().enumerate() {
            let Some(target) = body.target else { continue };
            if let Some(d) = dynamics[index] {
                let local = body.rotation.inverse() * (d.center_of_mass - body.position);
                animated_com += (body.target_position + target * local) * d.mass;
                animated_mass += d.mass;
            }
            if ankle_role(state, index) {
                ankles += body.position;
                animated_ankles += body.target_position;
                count += 1.0;
            }
        }
        let rise = if count > 0.0 && animated_mass > 0.0 {
            Some((
                com - ankles / count,
                animated_com / animated_mass - animated_ankles / count,
            ))
        } else {
            None
        };
        support_hull_into(points, projected, hull);
        let at = Vec2::new(com.x, com.z);
        let distance = support_distance(hull, at);
        // The capture point (Pratt et al. 2006): where a linear inverted pendulum of this height would have to step to
        // stop. Outside the polygon no ankle torque can stop it, whatever the planted feet allow.
        let height = rise.map_or(com.y, |(current, _)| current.y);
        let capture = if step.gravity > 0.0 && height > 0.01 {
            at + Vec2::new(com_velocity.x, com_velocity.z) * (height / step.gravity).sqrt()
        } else {
            at
        };
        let capture_distance = support_distance(hull, capture);
        let centre = if hull.is_empty() {
            at
        } else {
            hull.iter().copied().sum::<Vec2>() / hull.len() as f32
        };
        let offset = Vec3::new(at.x - centre.x, 0.0, at.y - centre.y);
        state.center_of_mass = com;
        state.com_offset = offset;
        state.support_distance = distance;
        state.capture_distance = capture_distance;
        state.support_points = points.len();
        let balance = &step.profile.balance;
        let collapsed = rise.is_some_and(|(current, animated)| current.y < animated.y * COLLAPSE_HEIGHT);
        // The recovery layer (`stepping` module docs): it may suspend the capture test while a step or a landing is
        // under way, or end in a fall of its own.
        let mut busy = false;
        let mut missed = false;
        if recovery.any() {
            let frame = Frame {
                com,
                capture,
                pressure: nearest_on_hull(hull, capture).unwrap_or(capture),
                omega: if height > 0.01 && step.gravity > 0.0 {
                    (step.gravity / height).sqrt()
                } else {
                    0.0
                },
                animated_com: if animated_mass > 0.0 {
                    animated_com / animated_mass
                } else {
                    com
                },
                forward: step.root_forward,
                up: step.root_up,
            };
            legs.clear();
            legs.extend_from_slice(&state.legs);
            let out = recover(
                state,
                step,
                &recovery,
                &frame,
                supported,
                soles,
                swing_touch,
                capture_distance,
                collapsed,
                leg_targets,
                free_legs,
                legs,
                down,
                planted,
            );
            busy = out.busy;
            missed = out.missed;
        }
        let outside = collapsed
            || missed
            || (!busy && capture_distance.is_none_or(|d| d > balance.fall_margin.max(0.0)));
        state.outside_steps = if outside {
            state.outside_steps.saturating_add(1)
        } else {
            0
        };
        let fall_steps = (balance.fall_time.max(0.0) / step.dt).ceil().max(1.0) as u32;
        if state.outside_steps >= fall_steps || missed {
            state.phase = RagdollPhase::Falling;
            state.tilt = Vec3::ZERO;
            state.fall_cause = Some(if collapsed {
                RagdollFallCause::Collapse
            } else if missed {
                RagdollFallCause::MissedStep
            } else if points.is_empty() {
                RagdollFallCause::NoContact
            } else {
                RagdollFallCause::Capture
            });
            foot_locks.fill(false);
            state.anchors.clear();
            leg_targets.fill(None);
            free_legs.clear();
        } else if state.step_phase != RagdollStepPhase::Airborne {
            let feedback = rise.map_or(offset, |(current, animated)| {
                let d = current - animated;
                Vec3::new(d.x, 0.0, d.z)
            });
            let lean = feedback * balance.cd + Vec3::new(com_velocity.x, 0.0, com_velocity.z) * balance.cv;
            let angle = lean.length().min(balance.max_tilt.max(0.0));
            let axis = lean.cross(Vec3::Y).normalize_or_zero();
            state.tilt = if angle.is_finite() && angle > 0.0 && axis != Vec3::ZERO {
                let clamped = lean.normalize_or_zero() * angle;
                let sideways = stance.map_or(Vec3::ZERO, |line| line * clamped.dot(line));
                lean_sideways = lean_rotation(sideways);
                lean_forward = lean_rotation(clamped - sideways);
                axis * angle
            } else {
                Vec3::ZERO
            };
            // What the planted feet can carry (module docs).
            let planted_count = foot_locks.iter().filter(|locked| **locked).count();
            if planted_count > 0 && step.gravity > 0.0 {
                foot_force = mass * step.gravity / planted_count as f32;
                let reach = hull.iter().map(|p| (*p - centre).length()).fold(0.0, f32::max);
                hip_limit = foot_force * reach;
            }
        }
        state.foot_weight = if foot_force.is_finite() { foot_force } else { 0.0 };
    } else if state.phase != RagdollPhase::Active {
        state.support_distance = None;
        state.capture_distance = None;
        state.support_points = 0;
    }
    state.planted_feet = foot_locks.iter().filter(|locked| **locked).count() as u32;

    // Motors.
    let _span = tracing::debug_span!(target: "regia_framework", "ragdoll_motors").entered();
    let target_of = |j: usize| leg_targets[j].or(step.bodies[j].target);
    for (index, body) in step.bodies.iter().enumerate() {
        let Some(motor) = state
            .motor_of_body
            .get(index)
            .copied()
            .flatten()
            .map(|m| &step.profile.motors[m])
        else {
            continue;
        };
        let Some(parent) = body.parent else {
            continue;
        };
        let strength = motor_strength(step.profile, state, motor, step.dt);
        if strength <= 0.0 || !motor.drives() {
            continue;
        }
        let (Some(target), Some(parent_target)) = (target_of(index), target_of(parent)) else {
            continue;
        };
        if dynamics[index].is_none() || dynamics[parent].is_none() {
            continue;
        }
        // A free (swinging or reaching) leg: the hip tracks its world target against the pelvis as it is now, the
        // knee and the foot follow their targets plainly, and none of them takes the balance lean or the sole limits.
        let free = free_legs.contains(&index)
            || free_legs.contains(&parent)
            || body
                .parent
                .and_then(|p| step.bodies[p].parent)
                .is_some_and(|g| free_legs.contains(&g));
        let relative = if free {
            if free_legs.contains(&index) {
                step.bodies[parent].rotation.inverse() * target
            } else {
                parent_target.inverse() * target
            }
        } else {
            // Joint-space target: the child's animated rotation relative to its parent's, with the balance lean.
            let planted_below = (0..n).any(|j| supported[j] && in_subtree(step.bodies, index, j));
            match (motor.balance, stance) {
                // Two feet down, hips: the pelvis holds its world rotation; the lean along the feet's line is the
                // hips' to do (it shifts the load between the feet). Rotation about that line is left to the ankles.
                (RagdollBalanceRole::Hip, Some(line)) if planted_below => {
                    let free = twist_about(body.rotation * target.inverse(), line);
                    parent_target.inverse() * free * lean_sideways * target
                }
                // One foot down: the pelvis' world rotation only.
                (RagdollBalanceRole::Hip, None) if planted_below => parent_target.inverse() * body.rotation,
                // Two feet down, ankles: they lean across the line (the foot's length can take it). Along the line
                // the shin stays free and the ankle only keeps the foot flat: a foot's width cannot take that torque.
                (RagdollBalanceRole::Ankle, Some(line)) if planted_below => {
                    let shin = step.bodies[parent].rotation;
                    let free = twist_about(shin * parent_target.inverse(), line.cross(Vec3::Y));
                    (lean_forward * free * parent_target).inverse() * target
                }
                (RagdollBalanceRole::Ankle, _) => {
                    parent_target.inverse() * (lean_sideways * lean_forward).inverse() * target
                }
                _ => parent_target.inverse() * target,
            }
        };
        // While a recovery is under way (a swing, an airborne or landing phase, or the stance a step or landing left),
        // the joints that carry the body get `RECOVERY_DAMPING` times their authored damping (ADR-2963): at the
        // authored kd (about kp / 300) they ring after a touch-down. A swinging leg keeps its authored gains.
        let damping =
            if recovery.any() && !free && (state.stepped || state.step_phase != RagdollStepPhase::Stand) {
                RECOVERY_DAMPING
            } else {
                1.0
            };
        // A planted foot does not tip by itself: its ankle gets the torque limit the sole would set, and the hips the
        // limit of shifting the load between the feet.
        let authored = Vec3::splat(motor.max_torque);
        let max_torque = match motor.balance {
            _ if free => authored,
            RagdollBalanceRole::Ankle if foot_locks[index] && foot_force > 0.0 => {
                let sole = levers[index] * foot_force;
                Vec3::new(sole.x.min(authored.x), authored.y, sole.z.min(authored.z))
            }
            // While a foot swings, the stance hip carries the pelvis against the swing leg: its authored torque.
            RagdollBalanceRole::Hip
                if hip_limit.is_finite() && state.step_phase != RagdollStepPhase::Swing =>
            {
                authored.min(Vec3::splat(hip_limit))
            }
            _ => authored,
        };
        state.driven += 1;
        state.peak_torque = state
            .peak_torque
            .max(joint_drive_torque(physics, body.entity, substep_dt));
        drives[index] = Some(JointDrive {
            target: relative.normalize(),
            kp: motor.kp * strength,
            kd: motor.kd * strength * damping,
            max_torque,
        });
    }
}

/// The world frame the recovery layer works in, from the balance step.
struct Frame {
    /// Center of mass.
    com: Vec3,
    /// Capture point (x/z).
    capture: Vec2,
    /// The support polygon's point nearest to the capture point (where the center of pressure can go at best).
    pressure: Vec2,
    /// The pendulum's natural frequency `sqrt(g / h)`, 1/s (0 when unknown).
    omega: f32,
    /// Center of mass of the animated pose.
    animated_com: Vec3,
    /// The root body's forward axis in its own frame.
    forward: Vec3,
    /// The root body's up axis in its own frame.
    up: Vec3,
}

/// Exponential divergence of the capture point from the center of pressure, capped to the recovery horizon.
fn capture_growth(omega: f32, seconds: f32) -> f32 {
    detmath::exp_positive((omega * seconds.max(0.0)).min(1.5))
}

/// What [`recover`] tells the fall test.
struct RecoverOut {
    /// A step or a landing is under way: the capture point is not a fall reason this step.
    busy: bool,
    /// A swing foot did not touch down in time.
    missed: bool,
}

/// One step of the recovery layer (`stepping` module docs): the phase machine (stand, swing, airborne, landing) and
/// the leg targets it replaces. `leg_targets` gets world rotations for thigh and shin bodies; `free_legs` the thighs
/// whose targets are relative to the pelvis as it is.
#[allow(clippy::too_many_arguments)] // ADR-2963: one call site, the balance step's own locals
fn recover(
    state: &mut RagdollActiveState,
    step: &ActiveStep,
    recovery: &RagdollRecovery,
    frame: &Frame,
    supported: &[bool],
    soles: &[Option<Vec3>],
    swing_touch: bool,
    capture_distance: Option<f32>,
    collapsed: bool,
    leg_targets: &mut [Option<Quat>],
    free_legs: &mut Vec<usize>,
    legs: &[Leg],
    down: &mut Vec<usize>,
    planted: &mut Vec<usize>,
) -> RecoverOut {
    let bodies = step.bodies;
    let mut out = RecoverOut {
        busy: false,
        missed: false,
    };
    if legs.is_empty() {
        return out;
    }
    let Some(root) = bodies.iter().position(|b| b.parent.is_none()) else {
        return out;
    };
    let steps_of = |seconds: f32| (seconds.max(0.0) / step.dt).ceil().max(1.0) as u32;
    let horizontal = |v: Vec3| Vec3::new(v.x, 0.0, v.z);
    let pelvis = bodies[root].rotation;
    let forward = horizontal(pelvis * frame.forward).normalize_or(Vec3::NEG_Z);
    let left = horizontal(pelvis * frame.up.cross(frame.forward)).normalize_or(forward.cross(Vec3::NEG_Y));
    let any_down = legs.iter().any(|l| supported[l.foot]) || swing_touch;

    // Airborne and landing.
    if recovery.landing {
        if !any_down && state.swing.is_none() {
            if state.step_phase != RagdollStepPhase::Airborne {
                state.step_phase = RagdollStepPhase::Airborne;
                state.air_steps = 0;
            }
            state.air_steps = state.air_steps.saturating_add(1);
            out.busy = state.air_steps < steps_of(recovery.max_air_time);
        } else if state.step_phase == RagdollStepPhase::Airborne && any_down {
            state.step_phase = RagdollStepPhase::Landing;
            state.land_steps = 0;
            state.stepped = true;
        }
        if state.step_phase == RagdollStepPhase::Landing {
            state.land_steps = state.land_steps.saturating_add(1);
            if state.land_steps >= steps_of(recovery.absorb_time) {
                state.step_phase = RagdollStepPhase::Stand;
            } else {
                // The feet come down one after the other and rock onto their soles: the capture test waits for the
                // absorb to end (a collapse still falls).
                out.busy = true;
            }
        }
    }

    // The swing in progress.
    let swing_steps = steps_of(recovery.swing_time);
    if let Some(mut swing) = state.swing {
        swing.elapsed = swing.elapsed.saturating_add(1);
        let s = swing.elapsed as f32 / swing_steps as f32;
        if s >= 0.5 && swing_touch {
            state.swing = None;
            state.step_phase = RagdollStepPhase::Stand;
            state.steps = state.steps.saturating_add(1);
            state.total_steps = state.total_steps.saturating_add(1);
            state.stepped = true;
            state.calm_steps = 0;
        } else if swing.elapsed > swing_steps.saturating_mul(3) {
            state.swing = None;
            state.step_phase = RagdollStepPhase::Stand;
            out.missed = true;
            return out;
        } else {
            state.swing = Some(swing);
            out.busy = true;
        }
    }

    // The capture point `seconds` from now if the center of pressure stays at its best point (the capture point of a
    // linear inverted pendulum diverges from it as `exp(omega t)`): where a step that lands then has to go.
    let predicted = |seconds: f32| {
        let grow = capture_growth(frame.omega, seconds);
        let p = frame.pressure + (frame.capture - frame.pressure) * grow;
        if p.is_finite() { p } else { frame.capture }
    };
    // A swing foot target the leg can reach from its hip once the stance hip has dropped as far as it may: a target
    // past that leaves the foot hovering above the ground while the body falls.
    let within_reach = |leg: &Leg, target: Vec3| {
        stepping::within_reach(
            bodies[leg.thigh].position,
            leg.upper,
            leg.lower,
            STANCE_DROP,
            target,
        )
    };

    // A new step.
    let standing = state.step_phase == RagdollStepPhase::Stand;
    down.clear();
    down.extend((0..legs.len()).filter(|&l| supported[legs[l].foot]));
    if standing
        && state.swing.is_none()
        && state.steps < recovery.max_steps
        && !collapsed
        && down.len() >= 2
        && capture_distance.is_some_and(|d| {
            // A later step only when the standing controller cannot ride it out on the stepped stance.
            let margin = if state.steps > 0 {
                recovery.trigger_margin.max(step.profile.balance.fall_margin)
            } else {
                recovery.trigger_margin
            };
            d > margin
        })
    {
        let mid = down
            .iter()
            .map(|&l| horizontal(bodies[legs[l].foot].position))
            .sum::<Vec3>()
            / down.len() as f32;
        let to_capture = Vec3::new(frame.capture.x, 0.0, frame.capture.y) - mid;
        let lateral = to_capture.dot(left);
        let pick = if lateral.abs() > 0.25 * to_capture.length() {
            // The foot on the side the capture point left towards.
            down.iter().copied().max_by(|&a, &b| {
                let side =
                    |l: usize| (horizontal(bodies[legs[l].foot].position) - mid).dot(left) * lateral.signum();
                side(a).total_cmp(&side(b)).then(b.cmp(&a))
            })
        } else {
            // Straight ahead or back: the foot behind.
            let travel = to_capture.normalize_or_zero();
            down.iter().copied().min_by(|&a, &b| {
                let ahead = |l: usize| (horizontal(bodies[legs[l].foot].position) - mid).dot(travel);
                ahead(a).total_cmp(&ahead(b)).then(a.cmp(&b))
            })
        };
        if let Some(leg) = pick
            && let Some(&other) = down.iter().find(|&&l| l != leg)
        {
            let foot = bodies[legs[leg].foot].position;
            let stance = bodies[legs[other].foot].position;
            let side = {
                let s = (horizontal(foot) - horizontal(stance)).dot(left);
                if s.abs() > 1e-4 {
                    left * s.signum()
                } else {
                    horizontal(foot - stance).normalize_or(left)
                }
            };
            let authored = horizontal(bodies[legs[leg].foot].target_position - frame.animated_com);
            let target = stepping::step_target(
                predicted(swing_steps as f32 * step.dt),
                Vec2::new(frame.com.x, frame.com.z),
                Vec2::new(authored.x, authored.z),
                Vec2::new(stance.x, stance.z),
                Vec2::new(side.x, side.z),
                recovery,
            );
            let rotation = bodies[legs[leg].foot].rotation;
            let sole = soles[legs[leg].foot].map_or(Vec3::ZERO, |c| rotation.inverse() * (c - foot));
            let reach = horizontal(rotation * sole);
            state.swing = Some(Swing {
                leg,
                elapsed: 0,
                start: foot,
                target: within_reach(
                    &legs[leg],
                    Vec3::new(target.x - reach.x, stance.y, target.y - reach.z),
                ),
                sole,
            });
            state.step_phase = RagdollStepPhase::Swing;
            out.busy = true;
        }
    }
    if state.step_phase == RagdollStepPhase::Stand && state.swing.is_none() {
        if capture_distance.is_some_and(|d| d <= 0.0) {
            state.calm_steps = state.calm_steps.saturating_add(1);
            if state.calm_steps >= steps_of(1.0) {
                state.steps = 0;
            }
        } else {
            state.calm_steps = 0;
        }
    }

    // Leg targets.
    let axes = |leg: &Leg| {
        (
            bodies[leg.shin].target_position - bodies[leg.thigh].target_position,
            bodies[leg.foot].target_position - bodies[leg.shin].target_position,
        )
    };
    let animated = |leg: &Leg| match (bodies[leg.thigh].target, bodies[leg.shin].target) {
        (Some(a), Some(b)) => Some((a, b)),
        _ => None,
    };
    let solve = |leg: &Leg, hip: Vec3, ankle: Vec3, leg_targets: &mut [Option<Quat>]| {
        let Some(rest) = animated(leg) else { return };
        if let Some((knee, reached)) = two_bone_ik(hip, leg.upper, leg.lower, ankle, forward) {
            let (thigh, shin) = leg_rotations(rest, axes(leg), hip, knee, reached);
            leg_targets[leg.thigh] = Some(thigh);
            leg_targets[leg.shin] = Some(shin);
        }
    };
    match state.step_phase {
        RagdollStepPhase::Swing => {
            if let Some(mut swing) = state.swing {
                let s = swing.elapsed as f32 / swing_steps as f32;
                // The target follows the predicted capture point through most of the swing.
                if s < 0.7
                    && let Some(&other) = down.iter().find(|&&l| l != swing.leg)
                {
                    let stance = bodies[legs[other].foot].position;
                    let foot = bodies[legs[swing.leg].foot].position;
                    let side = {
                        let d = (horizontal(swing.start) - horizontal(stance)).dot(left);
                        if d.abs() > 1e-4 {
                            left * d.signum()
                        } else {
                            horizontal(foot - stance).normalize_or(left)
                        }
                    };
                    let authored =
                        horizontal(bodies[legs[swing.leg].foot].target_position - frame.animated_com);
                    let remaining = (swing_steps.saturating_sub(swing.elapsed)) as f32 * step.dt;
                    let target = stepping::step_target(
                        predicted(remaining),
                        Vec2::new(frame.com.x, frame.com.z),
                        Vec2::new(authored.x, authored.z),
                        Vec2::new(stance.x, stance.z),
                        Vec2::new(side.x, side.z),
                        recovery,
                    );
                    let reach = horizontal(bodies[legs[swing.leg].foot].rotation * swing.sole);
                    swing.target = within_reach(
                        &legs[swing.leg],
                        Vec3::new(target.x - reach.x, stance.y, target.y - reach.z),
                    );
                    state.swing = Some(swing);
                }
                let leg = legs[swing.leg];
                let want = swing_point(swing.start, swing.target, s, recovery.lift, 0.03);
                solve(&leg, bodies[leg.thigh].position, want, leg_targets);
                free_legs.push(leg.thigh);
                // The stance knee bends so the hip comes down far enough for the swing leg to reach the target.
                if let Some(&other) = down.iter().find(|&&l| l != swing.leg) {
                    let stance = legs[other];
                    let reach = (leg.upper + leg.lower) * 0.97;
                    let d = horizontal(swing.target - bodies[leg.thigh].position).length();
                    let needed = swing.target.y + (reach * reach - d * d).max(0.0).sqrt();
                    let animated = swing.target.y
                        + (bodies[leg.thigh].target_position.y - bodies[leg.foot].target_position.y);
                    let drop = (animated - needed).clamp(0.0, STANCE_DROP);
                    if drop > 0.0 {
                        let foot = bodies[stance.foot].position;
                        let mut hip = bodies[stance.thigh].position;
                        hip.y = foot.y
                            + (bodies[stance.thigh].target_position.y
                                - bodies[stance.foot].target_position.y)
                            - drop;
                        solve(&stance, hip, foot, leg_targets);
                    }
                }
            }
        }
        RagdollStepPhase::Airborne => {
            let capture = Vec3::new(frame.capture.x, 0.0, frame.capture.y);
            for leg in legs {
                let hip = bodies[leg.thigh].position;
                let authored = bodies[leg.foot].target_position;
                let drop = (bodies[leg.thigh].target_position.y - authored.y).max(0.0);
                let mut want = capture + horizontal(authored - frame.animated_com);
                want.y = hip.y - drop * (1.0 - recovery.pre_bend);
                solve(leg, hip, want, leg_targets);
                free_legs.push(leg.thigh);
            }
        }
        RagdollStepPhase::Stand | RagdollStepPhase::Landing if state.stepped => {
            // Stand in the stance the steps (or the landing) left: the pelvis over the planted feet as authored, the
            // landing crouch easing out.
            planted.clear();
            planted.extend(
                legs.iter()
                    .enumerate()
                    .filter(|(_, leg)| supported[leg.foot])
                    .map(|(index, _)| index),
            );
            if !planted.is_empty() {
                let count = planted.len() as f32;
                let mid = planted
                    .iter()
                    .map(|&index| bodies[legs[index].foot].position)
                    .sum::<Vec3>()
                    / count;
                let animated_mid = planted
                    .iter()
                    .map(|&index| bodies[legs[index].foot].target_position)
                    .sum::<Vec3>()
                    / count;
                let crouch = if state.step_phase == RagdollStepPhase::Landing {
                    let t = state.land_steps as f32 / steps_of(recovery.absorb_time) as f32;
                    recovery.crouch * (1.0 - t.clamp(0.0, 1.0))
                } else {
                    0.0
                };
                // Height only: the hips stay where they are across, the balance lean moves them (a horizontal target
                // here would fight the lean with the knees).
                let height = mid.y + (bodies[root].target_position.y - animated_mid.y) - crouch;
                let lift = height - bodies[root].position.y;
                for &index in planted.iter() {
                    let leg = &legs[index];
                    solve(
                        leg,
                        bodies[leg.thigh].position + Vec3::Y * lift,
                        bodies[leg.foot].position,
                        leg_targets,
                    );
                }
            }
        }
        _ => {}
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// rc1 physics review, finding 14: leg bone lengths were measured from `target_position`, which is
    /// zero for a body whose animated target did not decompose, so a degenerate frame during a motor
    /// change registered a leg with garbage lengths. Such a body is measured where it is now.
    #[test]
    fn leg_lengths_ignore_a_missing_animated_target() {
        use regia_animation::Skeleton;
        use regia_animation::skeleton::LocalBind;

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "thigh".into(), "shin".into(), "foot".into()],
            vec![None, Some(0), Some(1), Some(2)],
            vec![LocalBind::default(); 4],
        )
        .expect("skeleton");
        let mut profile = RagdollActiveProfile::default();
        profile.motors.push(RagdollMotor {
            joint: "foot".into(),
            group: RagdollGroup::Legs,
            kp: 40.0,
            kd: 4.0,
            max_torque: 80.0,
            balance: RagdollBalanceRole::Ankle,
        });
        let bodies = [
            (None, 0, 1.0),
            (Some(0), 1, 0.55),
            (Some(1), 2, 0.13),
            (Some(2), 3, 0.05),
        ]
        .map(|(parent, joint, y)| ActiveBody {
            parent,
            joint,
            entity: Entity::from_raw_u32(joint + 1).expect("entity"),
            position: Vec3::new(0.0, y, 0.0),
            rotation: Quat::IDENTITY,
            // The shin's animated joint did not decompose this frame.
            target: (joint != 2).then_some(Quat::IDENTITY),
            target_position: if joint == 2 {
                Vec3::ZERO
            } else {
                Vec3::new(0.0, y, 0.0)
            },
        });
        let mut state = RagdollActiveState::default();
        resolve_motors(&mut state, &profile, &skeleton, &bodies);
        assert_eq!(state.legs.len(), 1);
        let leg = &state.legs[0];
        assert!((leg.upper - 0.42).abs() < 1e-5, "upper {}", leg.upper);
        assert!((leg.lower - 0.08).abs() < 1e-5, "lower {}", leg.lower);
    }

    /// A standing active step reuses its scratch on the second call.
    #[test]
    fn standing_active_ragdoll_does_not_allocate_scratch_each_step() {
        use regia_animation::Skeleton;
        use regia_animation::skeleton::LocalBind;

        let skeleton = Skeleton::from_local_bind(
            vec!["root".into(), "thigh".into(), "shin".into(), "foot".into()],
            vec![None, Some(0), Some(1), Some(2)],
            vec![
                LocalBind::default(),
                LocalBind {
                    translation: Vec3::new(0.0, -0.45, 0.0),
                    ..LocalBind::default()
                },
                LocalBind {
                    translation: Vec3::new(0.0, -0.42, 0.0),
                    ..LocalBind::default()
                },
                LocalBind {
                    translation: Vec3::new(0.0, -0.08, 0.12),
                    ..LocalBind::default()
                },
            ],
        )
        .expect("skeleton");
        let mut profile = RagdollActiveProfile {
            recovery: RagdollRecovery::full(),
            ..RagdollActiveProfile::default()
        };
        profile.motors.push(RagdollMotor {
            joint: "foot".into(),
            group: RagdollGroup::Legs,
            kp: 40.0,
            kd: 4.0,
            max_torque: 80.0,
            balance: RagdollBalanceRole::Ankle,
        });
        let bodies = [
            (None, 0, 1.0),
            (Some(0), 1, 0.55),
            (Some(1), 2, 0.13),
            (Some(2), 3, 0.05),
        ]
        .map(|(parent, joint, y)| ActiveBody {
            parent,
            joint,
            entity: Entity::from_raw_u32(joint + 1).expect("entity"),
            position: Vec3::new(0.0, y, 0.0),
            rotation: Quat::IDENTITY,
            target: Some(Quat::IDENTITY),
            target_position: Vec3::new(0.0, y, 0.0),
        });
        let physics = PhysicsWorld::default();
        let step = ActiveStep {
            profile: &profile,
            skeleton: &skeleton,
            bodies: &bodies,
            physics: Some(&physics),
            dt: 1.0 / 60.0,
            solver_iterations: 4,
            gravity: 9.81,
            drive: true,
            root_forward: Vec3::NEG_Z,
            root_up: Vec3::Y,
        };
        let mut state = RagdollActiveState {
            phase: RagdollPhase::Active,
            ..RagdollActiveState::default()
        };
        let mut work = ActiveWork::default();
        step_active(&mut state, &step, &mut work);
        let allocated = crate::test_alloc::count(|| {
            step_active(&mut state, &step, &mut work);
        });
        assert_eq!(allocated, 0, "the second step allocated {allocated} times");
        assert_eq!(work.foot_locks.len(), bodies.len());
    }

    #[test]
    fn facing_follows_the_more_vertical_pelvis_axis() {
        // Regia rigs face -Z with +Y up: left = up x forward = -X.
        let (up, forward) = (Vec3::Y, Vec3::NEG_Z);
        let left = up.cross(forward);
        assert_eq!(left, Vec3::NEG_X);
        let lie = |q: Quat| RagdollFacing::classify(q * forward, q * left);
        assert_eq!(
            lie(Quat::from_rotation_x(std::f32::consts::FRAC_PI_2)),
            RagdollFacing::FaceUp
        );
        assert_eq!(
            lie(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
            RagdollFacing::FaceDown
        );
        // Rolled onto the left side: the left axis points down.
        assert_eq!(
            lie(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
            RagdollFacing::SideLeft
        );
        assert_eq!(
            lie(Quat::from_rotation_z(-std::f32::consts::FRAC_PI_2)),
            RagdollFacing::SideRight
        );
        assert_eq!(
            [RagdollFacing::FaceUp, RagdollFacing::SideRight].map(RagdollFacing::name),
            ["face_up", "side_right"]
        );
    }

    #[test]
    fn the_get_up_choice_filters_by_facing_then_takes_the_nearest_pose() {
        let clip = |name: &str, facing, bend: f32| RagdollGetUp {
            clip: name.into(),
            facing,
            pose: vec![("spine".into(), Quat::from_rotation_x(bend))],
        };
        let clips = vec![
            clip("back", RagdollFacing::FaceUp, 0.0),
            clip("front_flat", RagdollFacing::FaceDown, 0.0),
            clip("front_curled", RagdollFacing::FaceDown, 0.8),
        ];
        let curled = vec![("spine".to_string(), Quat::from_rotation_x(0.7))];
        assert_eq!(pick_get_up(&clips, RagdollFacing::FaceDown, &curled), Some(2));
        let flat = vec![("spine".to_string(), Quat::from_rotation_x(0.1))];
        assert_eq!(pick_get_up(&clips, RagdollFacing::FaceDown, &flat), Some(1));
        // Facing first: the face-up clip wins on its back even though a face-down pose is nearer.
        assert_eq!(pick_get_up(&clips, RagdollFacing::FaceUp, &curled), Some(0));
        // No clip for the facing: every clip competes; a tie goes to the first authored.
        assert_eq!(pick_get_up(&clips, RagdollFacing::SideLeft, &flat), Some(0));
        assert_eq!(pick_get_up(&[], RagdollFacing::FaceUp, &flat), None);
        assert!(
            get_up_distance(&clips[2].pose, &[]) == 0.0,
            "missing bones are skipped"
        );
    }

    #[test]
    fn the_support_hull_and_distance() {
        let foot = |x: f32| {
            [
                Vec3::new(x - 0.05, 0.0, -0.15),
                Vec3::new(x + 0.05, 0.0, -0.15),
                Vec3::new(x + 0.05, 0.0, 0.1),
                Vec3::new(x - 0.05, 0.0, 0.1),
                Vec3::new(x, 0.0, 0.0), // interior point
            ]
        };
        let mut points: Vec<Vec3> = foot(-0.1).into_iter().chain(foot(0.1)).collect();
        let hull = support_hull(&points);
        assert_eq!(hull.len(), 4, "{hull:?}");
        // Order independent.
        points.reverse();
        assert_eq!(support_hull(&points), hull);
        let inside = support_distance(&hull, Vec2::new(0.0, 0.0)).expect("hull");
        assert!(
            (inside + 0.1).abs() < 1e-5,
            "0.1 m from the nearest (back) edge: {inside}"
        );
        let outside = support_distance(&hull, Vec2::new(0.0, -0.35)).expect("hull");
        assert!((outside - 0.2).abs() < 1e-5, "{outside}");
        assert_eq!(support_distance(&[], Vec2::ZERO), None);
        let one = support_hull(&[Vec3::new(1.0, 0.0, 1.0)]);
        assert_eq!(support_distance(&one, Vec2::new(1.0, 2.0)), Some(1.0));
    }

    fn reference_exp(x: f32) -> f32 {
        let x = f64::from(x);
        let mut sum = 0.0;
        for n in (1..=24).rev() {
            sum = 1.0 + sum * x / f64::from(n);
        }
        sum as f32
    }

    #[test]
    fn capture_growth_uses_deterministic_exponential_bits() {
        for i in 1..10_000 {
            let seconds = i as f32 * 0.0001;
            assert_eq!(
                capture_growth(1.0, seconds).to_bits(),
                reference_exp(seconds).to_bits(),
                "seconds = {seconds}"
            );
        }
        // The audit's gravity/height/time case remains pinned too.
        assert_eq!(capture_growth(9.81_f32.sqrt(), 0.2).to_bits(), 0x3fef_7994);
        assert_eq!(capture_growth(3.0, 1.0).to_bits(), 0x408f_69ff);
    }

    #[test]
    fn a_hit_weakens_its_group_and_recovers_on_time() {
        let dt = 1.0 / 32.0; // exact in binary: 16 steps are exactly 0.5 s
        let mut state = RagdollActiveState::default();
        let response = RagdollHitResponse {
            full_impulse: 100.0,
            recover_time: 0.5,
        };
        state.hit(RagdollGroup::Arms, 75.0, &response, dt);
        assert!((state.hit_strength(RagdollGroup::Arms, dt) - 0.25).abs() < 1e-6);
        assert_eq!(state.hit_strength(RagdollGroup::Spine, dt), 1.0);
        // A smaller hit while weaker does not raise it.
        state.hit(RagdollGroup::Arms, 10.0, &response, dt);
        assert!((state.hit_strength(RagdollGroup::Arms, dt) - 0.25).abs() < 1e-6);
        for _ in 0..8 {
            tick_hits(&mut state, dt);
        }
        let mid = state.hit_strength(RagdollGroup::Arms, dt);
        assert!((mid - 0.625).abs() < 1e-6, "halfway: {mid}");
        for _ in 0..7 {
            tick_hits(&mut state, dt);
        }
        assert!(
            state.hit_strength(RagdollGroup::Arms, dt) < 1.0,
            "15 steps: not yet"
        );
        tick_hits(&mut state, dt);
        assert_eq!(
            state.hit_strength(RagdollGroup::Arms, dt),
            1.0,
            "16 steps of 1/32 s = 0.5 s"
        );
        assert_eq!(RagdollGroup::from_name("legs"), Some(RagdollGroup::Legs));
        assert_eq!(RagdollGroup::from_name("tail"), None);
    }
}
