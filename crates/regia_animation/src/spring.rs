// SPDX-License-Identifier: MPL-2.0
//! Spring bones: secondary motion for hair, cloth strips, tails and antennae (C3).
//!
//! A [`SpringBones`] component on a rig runs after the animation is evaluated (and after any
//! ragdoll blend) and before the pose reaches the joint entities and the skin palette: it
//! reads the animated pose, simulates the chain points and writes **joint rotations** back
//! into the pose. Translations never change, so bone lengths stay the animator's.
//!
//! The solver follows the behaviour of the VRM `springBone` extension (a chain hangs from an
//! animated parent; each joint is a point that is pulled back to its animated pose, damped,
//! pushed by gravity, kept at its bone length and pushed out of sphere and capsule colliders
//! on body bones), implemented as position-based Verlet integration:
//!
//! 1. free motion `next = cur + (cur - prev) * (1 - damping) + gravity * dt²`;
//! 2. shape matching: `next = lerp(next, target, stiffness)`, where `target` is the pose the
//!    animation (plus the already-moved parent joints) asks for, so `stiffness = 1` keeps the
//!    animated pose and `0` leaves the chain free;
//! 3. a bone-length constraint, collider push-out, and the length constraint again.
//!
//! The simulation runs in **world space** (rig `Transform` times the pose), so moving the rig
//! drags the chain and a turn makes it swing, with a **fixed internal step**
//! ([`SpringBoneSetup::step_hz`], 60 Hz by default): a frame accumulates its delta and runs
//! `floor(accumulated / step)` substeps (at most [`MAX_SUBSTEPS`]; the rest of the backlog
//! stays in the accumulator and drains over later frames, clamped at one second or one step,
//! whichever is longer), all against that frame's animated pose. The result depends on the
//! number of steps, not on the frame rate. The step counts are exact, so two runs of one frame
//! sequence are bit-identical.
//!
//! Level of detail ([`SpringLod`], distance to [`crate::AnimLodCamera`]): beyond
//! `reduced_distance` the step drops to `reduced_step_hz`; beyond `freeze_distance` the rig
//! shows the plain animation and the chain re-seeds from the pose when it comes back.
//!
//! Authoring: a `.spring.ron` document ([`SpringBoneSetup`], `docs/formats/spring-ron.md`).
//! Joints are named, so one setup binds to any skeleton that has the joints.

use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::*;
use regia_scene::Transform;
use serde::{Deserialize, Serialize};

use crate::pose::Pose;
use crate::skeleton::Skeleton;

/// The `.spring.ron` format version. Version 1 is the first freeze.
pub const SPRING_FORMAT_VERSION: u32 = 1;

/// The most substeps one frame runs; the rest of a longer hitch stays in the accumulator and
/// drains over later frames (clamped at one second or one step, whichever is longer).
pub const MAX_SUBSTEPS: u32 = 8;

fn default_stiffness() -> f32 {
    0.2
}
fn default_damping() -> f32 {
    0.1
}
fn default_gravity_dir() -> Vec3 {
    Vec3::NEG_Y
}
fn default_gravity_power() -> f32 {
    0.0
}
fn default_hit_radius() -> f32 {
    0.0
}
fn default_true() -> bool {
    true
}
fn default_step_hz() -> f32 {
    60.0
}
fn default_reduced_hz() -> f32 {
    30.0
}

/// The collision shape of a [`SpringCollider`], in the space of its joint.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ColliderShape {
    /// A sphere at the collider's offset.
    Sphere {
        /// Radius in metres.
        radius: f32,
    },
    /// A capsule from the offset to `tail` (both in the joint's local frame).
    Capsule {
        /// Radius in metres.
        radius: f32,
        /// The other end of the capsule's axis, joint-local.
        tail: Vec3,
    },
}

/// A collider on a body bone that chain joints are pushed out of (VRM springBone collider).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpringCollider {
    /// Name chains refer to.
    pub name: String,
    /// The joint the collider rides (animated; use a body bone, not a simulated one).
    pub joint: String,
    /// Offset of the shape's origin in the joint's local frame, metres.
    #[serde(default)]
    pub offset: Vec3,
    /// The shape.
    pub shape: ColliderShape,
}

/// One spring chain: `joints` run root to tip, each the child of the one before. The root keeps
/// its animated position (it hangs from the animated parent) and is rotated to follow the
/// next joint; every later joint is simulated.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpringChain {
    /// Chain name (the agent addresses chains by it).
    pub name: String,
    /// Joint names, root first. At least two, each the child of the previous.
    pub joints: Vec<String>,
    /// Fraction `0..=1` of the distance to the animated pose recovered per fixed step. `1`
    /// keeps the pose exactly; `0` is a free rope.
    #[serde(default = "default_stiffness")]
    pub stiffness: f32,
    /// Fraction `0..=1` of the velocity lost per fixed step (VRM `dragForce`).
    #[serde(default = "default_damping")]
    pub damping: f32,
    /// World direction gravity pulls in (normalized on use).
    #[serde(default = "default_gravity_dir")]
    pub gravity_dir: Vec3,
    /// Gravity acceleration in m/s² (9.81 is Earth's; `0` disables gravity). VRM's
    /// `gravityPower` is not in these units: scale on import.
    #[serde(default = "default_gravity_power")]
    pub gravity_power: f32,
    /// Radius of each simulated joint for collisions, metres (VRM `hitRadius`).
    #[serde(default = "default_hit_radius")]
    pub hit_radius: f32,
    /// Names of the [`SpringCollider`]s this chain collides with.
    #[serde(default)]
    pub colliders: Vec<String>,
    /// A disabled chain shows its animation.
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for SpringChain {
    fn default() -> Self {
        SpringChain {
            name: String::new(),
            joints: Vec::new(),
            stiffness: default_stiffness(),
            damping: default_damping(),
            gravity_dir: default_gravity_dir(),
            gravity_power: default_gravity_power(),
            hit_radius: default_hit_radius(),
            colliders: Vec::new(),
            enabled: true,
        }
    }
}

/// Distance tiers for a rig's spring bones (distance from the rig to [`crate::AnimLodCamera`];
/// no camera resource means full rate).
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpringLod {
    /// Beyond this distance the step rate drops to `reduced_step_hz`. `0` disables the tier.
    #[serde(default)]
    pub reduced_distance: f32,
    /// Fixed step rate of the reduced tier.
    #[serde(default = "default_reduced_hz")]
    pub reduced_step_hz: f32,
    /// Beyond this distance the chains freeze (the animation shows). `0` disables the tier.
    #[serde(default)]
    pub freeze_distance: f32,
}

impl Default for SpringLod {
    fn default() -> Self {
        SpringLod {
            reduced_distance: 0.0,
            reduced_step_hz: default_reduced_hz(),
            freeze_distance: 0.0,
        }
    }
}

/// The `.spring.ron` document: chains, colliders and the shared step settings.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SpringBoneSetup {
    /// Format version of this file.
    #[serde(default)]
    pub format_version: u32,
    /// Fixed internal step rate in Hz.
    #[serde(default = "default_step_hz")]
    pub step_hz: f32,
    /// Distance tiers.
    #[serde(default)]
    pub lod: SpringLod,
    /// The chains.
    #[serde(default)]
    pub chains: Vec<SpringChain>,
    /// The colliders chains may refer to.
    #[serde(default)]
    pub colliders: Vec<SpringCollider>,
}

impl Default for SpringBoneSetup {
    fn default() -> Self {
        SpringBoneSetup {
            format_version: SPRING_FORMAT_VERSION,
            step_hz: default_step_hz(),
            lod: SpringLod::default(),
            chains: Vec::new(),
            colliders: Vec::new(),
        }
    }
}

/// Errors loading a `.spring.ron`.
#[derive(Debug, thiserror::Error)]
pub enum SpringError {
    /// RON syntax or shape error.
    #[error("spring RON: {0}")]
    Ron(#[from] ron::error::SpannedError),
    /// RON write error.
    #[error("spring RON write: {0}")]
    RonWrite(#[from] ron::Error),
    /// The file is newer than this engine.
    #[error("spring format version {0} is unknown (current is {SPRING_FORMAT_VERSION})")]
    UnknownVersion(u32),
    /// The document is malformed.
    #[error("invalid spring document: {0}")]
    Invalid(String),
}

/// `from` versions of the registered migrations (none: v1 is the first version).
#[must_use]
pub fn spring_migration_sources() -> Vec<u32> {
    Vec::new()
}

/// The serde field-list fingerprint of the format (for `cargo xtask check-formats`).
#[must_use]
pub fn spring_schema_fingerprint() -> String {
    use regia_core::schema::{join_field_lists, serde_field_names, serde_variant_names};
    join_field_lists(&[
        serde_field_names::<SpringBoneSetup>(),
        serde_field_names::<SpringChain>(),
        serde_field_names::<SpringCollider>(),
        serde_field_names::<SpringLod>(),
        serde_variant_names::<ColliderShape>(),
    ])
}

impl SpringBoneSetup {
    /// Parses `.spring.ron` text: peeks the version (newer is rejected), validates.
    ///
    /// # Errors
    ///
    /// Syntax errors, unknown versions, structurally invalid chains or colliders.
    pub fn from_ron_str(text: &str) -> Result<Self, SpringError> {
        let version = regia_core::version::peek_ron_version(text)
            .map_err(|e| SpringError::Invalid(format!("cannot read format_version: {e}")))?;
        if version > SPRING_FORMAT_VERSION {
            return Err(SpringError::UnknownVersion(version));
        }
        let mut setup: SpringBoneSetup = ron::from_str(text)?;
        setup.format_version = SPRING_FORMAT_VERSION;
        setup.validate()?;
        Ok(setup)
    }

    /// Serializes at the current version.
    ///
    /// # Errors
    ///
    /// Serialization failure.
    pub fn to_ron_string(&self) -> Result<String, SpringError> {
        let mut setup = self.clone();
        setup.format_version = SPRING_FORMAT_VERSION;
        let pretty = regia_core::ron_pretty();
        Ok(ron::ser::to_string_pretty(&setup, pretty)?)
    }

    /// Checks names, counts and ranges that do not need a skeleton.
    ///
    /// # Errors
    ///
    /// The first problem: a chain with fewer than two joints or no name, duplicate chain or
    /// collider names, a chain referring to a collider that is not defined, a non-finite or
    /// non-positive step rate.
    pub fn validate(&self) -> Result<(), SpringError> {
        let bad = |m: String| Err(SpringError::Invalid(m));
        let positive = |hz: f32| hz.is_finite() && hz > 0.0;
        if !positive(self.step_hz) || !positive(self.lod.reduced_step_hz) {
            return bad("step rates must be finite and positive".into());
        }
        for (i, c) in self.colliders.iter().enumerate() {
            if c.name.is_empty() || self.colliders[..i].iter().any(|o| o.name == c.name) {
                return bad(format!("collider name {:?} is empty or duplicated", c.name));
            }
        }
        for (i, chain) in self.chains.iter().enumerate() {
            if chain.name.is_empty() || self.chains[..i].iter().any(|o| o.name == chain.name) {
                return bad(format!("chain name {:?} is empty or duplicated", chain.name));
            }
            if !chain.stiffness.is_finite() || !chain.damping.is_finite() {
                return bad(format!(
                    "chain {:?} stiffness and damping must be finite",
                    chain.name
                ));
            }
            if chain.joints.len() < 2 {
                return bad(format!("chain {:?} needs at least two joints", chain.name));
            }
            for name in &chain.colliders {
                if !self.colliders.iter().any(|c| &c.name == name) {
                    return bad(format!(
                        "chain {:?} refers to unknown collider {name:?}",
                        chain.name
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Runtime state of one chain.
#[derive(Clone, Debug, Default)]
struct ChainRuntime {
    /// Resolved joint ids, root first (empty when the chain could not be bound).
    joints: Vec<u32>,
    /// Resolved collider indices into the setup.
    colliders: Vec<usize>,
    /// Current and previous world positions of every chain joint (index 0 is the root and is
    /// overwritten by the animated position each frame).
    cur: Vec<Vec3>,
    prev: Vec<Vec3>,
    seeded: bool,
    /// The root's world velocity between its last two simulated positions, m/s: a root moving
    /// fast but steadily is not a teleport.
    root_velocity: Vec3,
    /// Seconds since the root was last simulated (`cur[0]` written).
    since_root: f32,
}

/// A collider placed in world space for this frame.
#[derive(Clone, Copy, Debug)]
struct WorldCollider {
    a: Vec3,
    b: Vec3,
    radius: f32,
}

/// A hash of the skeleton's joint names and parents, or `None` when `parents` is shorter than
/// the joint list or a parent is not an earlier joint (such a skeleton cannot be simulated).
fn skeleton_fingerprint(skeleton: &Skeleton) -> Option<u64> {
    use std::hash::{Hash, Hasher};
    if skeleton.parents.len() != skeleton.joint_names.len() {
        return None;
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    for (i, (name, parent)) in skeleton.joint_names.iter().zip(&skeleton.parents).enumerate() {
        if parent.is_some_and(|p| p as usize >= i) {
            return None;
        }
        name.hash(&mut hasher);
        parent.hash(&mut hasher);
    }
    Some(hasher.finish())
}

/// Spring bones on a rig. Add it next to the rig's `SkinnedMesh` and `Animator`.
#[derive(Component, Clone, Debug)]
pub struct SpringBones {
    setup: SpringBoneSetup,
    /// Master switch; a disabled component shows the plain animation.
    pub enabled: bool,
    runtime: Vec<ChainRuntime>,
    problems: Vec<String>,
    resolved_for: Option<u64>,
    accumulator: f64,
    frozen: bool,
    reduced: bool,
    steps_last_frame: u32,
    /// Scratch reused across frames. Not part of the setup.
    globals: Vec<Mat4>,
    world_pos: Vec<Vec3>,
    world_rot: Vec<Quat>,
    collider_world: Vec<Option<WorldCollider>>,
    /// Collider joint ids resolved with the chains. `None` when the joint name is missing.
    collider_joints: Vec<Option<u32>>,
    out_rot: Vec<(u32, Quat)>,
    deltas: Vec<Quat>,
}

impl SpringBones {
    /// Spring bones running `setup`.
    pub fn new(setup: SpringBoneSetup) -> Self {
        SpringBones {
            setup,
            enabled: true,
            runtime: Vec::new(),
            problems: Vec::new(),
            resolved_for: None,
            accumulator: 0.0,
            frozen: false,
            reduced: false,
            steps_last_frame: 0,
            globals: Vec::new(),
            world_pos: Vec::new(),
            world_rot: Vec::new(),
            collider_world: Vec::new(),
            collider_joints: Vec::new(),
            out_rot: Vec::new(),
            deltas: Vec::new(),
        }
    }

    /// The setup.
    pub fn setup(&self) -> &SpringBoneSetup {
        &self.setup
    }

    /// The setup, mutably. Chains whose joints did not change keep their simulation state.
    pub fn setup_mut(&mut self) -> &mut SpringBoneSetup {
        self.resolved_for = None;
        &mut self.setup
    }

    /// Forget the simulation: every chain re-seeds from the animated pose on the next frame
    /// (use after a teleport or a cut).
    pub fn reset(&mut self) {
        for chain in &mut self.runtime {
            chain.seeded = false;
        }
        self.accumulator = 0.0;
    }

    /// Why chains could not be bound to the skeleton (missing joints, a joint that is not a
    /// child of the previous one, unknown colliders); empty when everything bound.
    pub fn problems(&self) -> &[String] {
        &self.problems
    }

    /// True when the rig is beyond the freeze distance (the animation shows).
    pub fn frozen(&self) -> bool {
        self.frozen
    }

    /// True when the rig runs the reduced step rate.
    pub fn reduced(&self) -> bool {
        self.reduced
    }

    /// Substeps run by the last [`SpringBones::update`].
    pub fn steps_last_frame(&self) -> u32 {
        self.steps_last_frame
    }

    /// True when the named chain bound to the skeleton (false before the first update).
    pub fn chain_bound(&self, chain: &str) -> bool {
        self.setup
            .chains
            .iter()
            .position(|c| c.name == chain)
            .and_then(|i| self.runtime.get(i))
            .is_some_and(|rt| !rt.joints.is_empty())
    }

    /// The world-space positions of a chain's joints, root first (empty before the first
    /// update, while frozen, or for an unbound chain).
    pub fn chain_positions(&self, chain: &str) -> Vec<Vec3> {
        let Some(index) = self.setup.chains.iter().position(|c| c.name == chain) else {
            return Vec::new();
        };
        match self.runtime.get(index) {
            Some(rt) if rt.seeded => rt.cur.clone(),
            _ => Vec::new(),
        }
    }

    /// Whether `resolve` last ran for exactly this skeleton (test hook).
    #[cfg(test)]
    fn is_resolved_for(&self, skeleton: &Skeleton) -> bool {
        self.resolved_for.is_some() && self.resolved_for == skeleton_fingerprint(skeleton)
    }

    fn resolve(&mut self, skeleton: &Skeleton) {
        let old = std::mem::take(&mut self.runtime);
        self.problems.clear();
        for chain in &self.setup.chains {
            let mut rt = ChainRuntime::default();
            let mut ok = true;
            let mut joints = Vec::with_capacity(chain.joints.len());
            for name in &chain.joints {
                match skeleton.joint_index(name) {
                    Some(j) => joints.push(j),
                    None => {
                        self.problems.push(format!(
                            "chain {:?}: joint {name:?} is not in the skeleton",
                            chain.name
                        ));
                        ok = false;
                    }
                }
            }
            if ok {
                for pair in joints.windows(2) {
                    if skeleton.parents[pair[1] as usize] != Some(pair[0]) {
                        self.problems.push(format!(
                            "chain {:?}: {:?} is not a child of {:?}",
                            chain.name,
                            skeleton.joint_names[pair[1] as usize],
                            skeleton.joint_names[pair[0] as usize]
                        ));
                        ok = false;
                    }
                }
            }
            for name in &chain.colliders {
                match self.setup.colliders.iter().position(|c| &c.name == name) {
                    Some(i) if skeleton.joint_index(&self.setup.colliders[i].joint).is_some() => {
                        rt.colliders.push(i);
                    }
                    Some(i) => self.problems.push(format!(
                        "chain {:?}: collider {name:?} rides {:?}, which is not in the skeleton",
                        chain.name, self.setup.colliders[i].joint
                    )),
                    None => self
                        .problems
                        .push(format!("chain {:?}: unknown collider {name:?}", chain.name)),
                }
            }
            if ok {
                rt.joints = joints;
                // Keep the state of a chain that kept its joints.
                if let Some(previous) = old.iter().find(|o| o.joints == rt.joints && o.seeded) {
                    rt.cur.clone_from(&previous.cur);
                    rt.prev.clone_from(&previous.prev);
                    rt.seeded = true;
                    rt.root_velocity = previous.root_velocity;
                    rt.since_root = previous.since_root;
                }
            }
            self.runtime.push(rt);
        }
        self.collider_joints.clear();
        self.collider_joints.extend(
            self.setup
                .colliders
                .iter()
                .map(|c| skeleton.joint_index(&c.joint)),
        );
        self.resolved_for = skeleton_fingerprint(skeleton);
        // `resolve` runs once per skeleton change, so this logs each binding problem once.
        for problem in &self.problems {
            tracing::warn!(target: "regia_animation", "spring bones: {problem}");
        }
    }

    /// Run the springs over the animated `pose` (local, in place): step the simulation by
    /// `delta_secs` and write the resulting joint rotations. `rig` is the rig's world
    /// transform; `camera_distance` the distance to the LOD camera, if there is one.
    pub fn update(
        &mut self,
        skeleton: &Skeleton,
        pose: &mut Pose,
        rig: &Transform,
        delta_secs: f32,
        camera_distance: Option<f32>,
    ) {
        let _span = tracing::debug_span!("spring_bones").entered();
        self.steps_last_frame = 0;
        if !self.enabled || self.setup.chains.is_empty() || pose.len() != skeleton.len() {
            return;
        }
        let lod = self.setup.lod;
        let distance = camera_distance.unwrap_or(0.0);
        self.frozen = lod.freeze_distance > 0.0 && distance > lod.freeze_distance;
        self.reduced = !self.frozen && lod.reduced_distance > 0.0 && distance > lod.reduced_distance;
        if self.frozen {
            for chain in &mut self.runtime {
                chain.seeded = false;
            }
            self.accumulator = 0.0;
            return;
        }
        // A hand-built or mutated skeleton can have short or out-of-order `parents`; skip it.
        let Some(fingerprint) = skeleton_fingerprint(skeleton) else {
            return;
        };
        // Joint indices are resolved by name, so a different skeleton of the same joint
        // count must re-resolve too.
        if self.resolved_for != Some(fingerprint) {
            self.resolve(skeleton);
        }

        // The animated pose in world space. A zero or non-finite rig or pose rotation (a
        // defaulted buffer) normalizes to NaN; it stands in as identity / the parent's rotation
        // so one bad quaternion cannot NaN every spring joint (audit-v035-numerics M3).
        let rig_rotation = unit_or(rig.rotation, Quat::IDENTITY);
        let rig_matrix = Mat4::from_scale_rotation_translation(rig.scale, rig_rotation, rig.translation);
        let joint_count = skeleton.len();
        reserve_spring(&mut self.globals, joint_count);
        skeleton.globals_into(pose, None, &mut self.globals);
        reserve_spring(&mut self.world_pos, joint_count);
        self.world_pos.clear();
        self.world_pos.extend(
            self.globals
                .iter()
                .map(|g| rig_matrix.transform_point3(g.w_axis.truncate())),
        );
        reserve_spring(&mut self.world_rot, joint_count);
        self.world_rot.clear();
        for (parent_of, rotation) in skeleton.parents.iter().zip(&pose.rotations).take(joint_count) {
            let parent = match *parent_of {
                Some(p) => self.world_rot[p as usize],
                None => rig_rotation,
            };
            self.world_rot.push(unit_or(parent * *rotation, parent));
        }
        if self.collider_joints.len() != self.setup.colliders.len() {
            self.collider_joints.clear();
            self.collider_joints.extend(
                self.setup
                    .colliders
                    .iter()
                    .map(|c| skeleton.joint_index(&c.joint)),
            );
        }
        reserve_spring(&mut self.collider_world, self.setup.colliders.len());
        self.collider_world.clear();
        for (c, joint) in self.setup.colliders.iter().zip(&self.collider_joints) {
            let Some(joint) = joint.map(|j| j as usize) else {
                self.collider_world.push(None);
                continue;
            };
            let at = |p: Vec3| self.world_pos[joint] + self.world_rot[joint] * (p * rig.scale);
            let (b, radius) = match c.shape {
                ColliderShape::Sphere { radius } => (c.offset, radius),
                ColliderShape::Capsule { radius, tail } => (tail, radius),
            };
            self.collider_world.push(Some(WorldCollider {
                a: at(c.offset),
                b: at(b),
                radius: radius * rig.scale.abs().max_element(),
            }));
        }

        let hz = if self.reduced {
            lod.reduced_step_hz
        } else {
            self.setup.step_hz
        };
        // A zero or non-finite rate would poison the accumulator; fall back to the default.
        let hz = if hz.is_finite() && hz > 0.0 {
            hz
        } else {
            default_step_hz()
        };
        let dt = 1.0 / f64::from(hz);
        // A hitch is caught up in later steps, not dropped: the excess stays in the
        // accumulator and drains at MAX_SUBSTEPS per call, so one 0.25 s frame and fifteen
        // 1/60 s frames run the same total number of substeps and reach the same pose when the
        // animated target did not move over the window (audit-v040 M4). Every substep in one
        // call integrates against that call's single animated pose, so a target that moved
        // during the hitch is fast-forwarded against its newest value — the substep count
        // matches never-hitched play, the exact pose need not (fixrev-06-k L7). The backlog is
        // clamped so a sustained overload cannot grow it without bound; the clamp (one second
        // at any step rate) only bites under a multi-second stall, where dropping time is the
        // intended behavior. It is never below one step: a rate under 1 Hz (which `validate`
        // accepts) never reached a whole step under a flat one-second clamp and stopped
        // simulating (audit corr-animation).
        self.accumulator = (self.accumulator + f64::from(delta_secs.max(0.0))).min(dt.max(1.0));
        let mut steps = (self.accumulator / dt).floor() as u32;
        if steps > MAX_SUBSTEPS {
            steps = MAX_SUBSTEPS;
        }
        self.accumulator -= f64::from(steps) * dt;
        self.steps_last_frame = steps;
        let dt = dt as f32;

        let world_pos = std::mem::take(&mut self.world_pos);
        let world_rot = std::mem::take(&mut self.world_rot);
        let collider_world = std::mem::take(&mut self.collider_world);
        let mut deltas = std::mem::take(&mut self.deltas);
        let mut out_rot = std::mem::take(&mut self.out_rot);
        out_rot.clear();
        for (index, chain) in self.setup.chains.iter().enumerate() {
            let rt = &mut self.runtime[index];
            if !chain.enabled || rt.joints.is_empty() {
                continue;
            }
            let ctx = ChainContext {
                chain,
                joints: &rt.joints,
                world_pos: &world_pos,
                parent_world_rot: match skeleton.parents[rt.joints[0] as usize] {
                    Some(p) => world_rot[p as usize],
                    None => rig_rotation,
                },
                collider_index: &rt.colliders,
                colliders: &collider_world,
            };
            if !rt.seeded {
                rt.cur = rt.joints.iter().map(|&j| world_pos[j as usize]).collect();
                rt.prev.clone_from(&rt.cur);
                rt.seeded = true;
            }
            let root = world_pos[rt.joints[0] as usize];
            rt.since_root += delta_secs.max(0.0);
            // A root that lands further than the chain is long both from where it was and from
            // where its recent velocity puts it is a teleport or an animation cut: reseed every
            // point, or the correction would be integrated as velocity. A root that moves that
            // far every frame (a fast vehicle, a low frame rate), or stops dead, keeps its
            // simulation and its lag.
            let reach: f32 = rt
                .joints
                .windows(2)
                .map(|w| (world_pos[w[1] as usize] - world_pos[w[0] as usize]).length())
                .sum();
            let last_root = rt.cur[0];
            let predicted = last_root + rt.root_velocity * rt.since_root;
            let jump = (root - last_root).length().min((root - predicted).length());
            if !jump.is_finite() || jump > reach.max(1e-3) {
                rt.cur = rt.joints.iter().map(|&j| world_pos[j as usize]).collect();
                rt.prev.clone_from(&rt.cur);
            }
            // The root follows the rig only on frames that simulate; a zero-substep frame aims
            // from the last simulated state, so the pose does not depend on the render rate.
            if steps > 0 {
                // Measured across a reseed too: a rig that starts moving fast reseeds once, then
                // its velocity predicts it.
                let velocity = (root - last_root) / rt.since_root;
                rt.root_velocity = if rt.since_root > 0.0 && velocity.is_finite() {
                    velocity
                } else {
                    Vec3::ZERO
                };
                rt.since_root = 0.0;
                rt.cur[0] = root;
                rt.prev[0] = root;
            }
            for _ in 0..steps {
                ctx.step(&mut rt.cur, &mut rt.prev, Some(dt), &mut deltas);
            }
            ctx.step(&mut rt.cur, &mut rt.prev, None, &mut deltas);
            // New local rotations: parent's new world rotation is the previous joint's.
            let mut parent_new = ctx.parent_world_rot;
            for (k, &joint) in rt.joints.iter().enumerate() {
                let new_world = (deltas[k] * world_rot[joint as usize]).normalize();
                let local = (parent_new.inverse() * new_world).normalize();
                if out_rot.len() == out_rot.capacity() {
                    crate::hot_count::count_spring_buffer();
                }
                out_rot.push((joint, local));
                parent_new = new_world;
            }
        }
        self.world_pos = world_pos;
        self.world_rot = world_rot;
        self.collider_world = collider_world;
        self.deltas = deltas;
        // A chain whose simulation went non-finite (a NaN rig translation or scale) leaves its
        // joints at the animated pose rather than writing NaN into it (M3).
        for (joint, rotation) in &out_rot {
            if rotation.is_finite() {
                pose.rotations[*joint as usize] = *rotation;
            }
        }
        self.out_rot = out_rot;
    }
}

/// Grow `buf` to hold `n` elements, counting only when the allocation is new.
fn reserve_spring<T>(buf: &mut Vec<T>, n: usize) {
    if buf.capacity() < n {
        crate::hot_count::count_spring_buffer();
        buf.reserve(n - buf.capacity());
    }
}

/// `q` normalized, or `fallback` when that is not finite (a zero or non-finite quaternion).
fn unit_or(q: Quat, fallback: Quat) -> Quat {
    let unit = q.normalize();
    if unit.is_finite() { unit } else { fallback }
}

/// Everything one chain's solve reads from the animated frame.
struct ChainContext<'a> {
    chain: &'a SpringChain,
    joints: &'a [u32],
    world_pos: &'a [Vec3],
    /// World rotation of the animated parent of the chain root.
    parent_world_rot: Quat,
    /// Indexes into `colliders` for this chain.
    collider_index: &'a [usize],
    colliders: &'a [Option<WorldCollider>],
}

impl ChainContext<'_> {
    /// One pass down the chain. With `dt` it integrates every simulated point first; without it
    /// only measures. Writes the world swing `delta` of every joint into `deltas` (new world
    /// rotation = `delta * animated world rotation`).
    fn step(&self, cur: &mut [Vec3], prev: &mut [Vec3], dt: Option<f32>, deltas: &mut Vec<Quat>) {
        let n = self.joints.len();
        reserve_spring(deltas, n);
        deltas.clear();
        deltas.resize(n, Quat::IDENTITY);
        let gravity = self.chain.gravity_dir.normalize_or_zero() * self.chain.gravity_power;
        let stiffness = crate::num::unit_weight(self.chain.stiffness);
        let keep = 1.0 - crate::num::unit_weight(self.chain.damping);
        let mut inherited = Quat::IDENTITY;
        for k in 0..n - 1 {
            let head = cur[k];
            let anim_head = self.world_pos[self.joints[k] as usize];
            let anim_tail = self.world_pos[self.joints[k + 1] as usize];
            let anim_offset = anim_tail - anim_head;
            let length = anim_offset.length();
            let anim_dir = if length > 1e-9 {
                anim_offset / length
            } else {
                Vec3::Y
            };
            if let Some(dt) = dt {
                let target = head + inherited * anim_dir * length;
                let free = cur[k + 1] + (cur[k + 1] - prev[k + 1]) * keep + gravity * (dt * dt);
                let mut next = free.lerp(target, stiffness);
                next = constrain(head, next, length, target);
                for &index in self.collider_index {
                    if let Some(collider) = self.colliders.get(index).and_then(Option::as_ref) {
                        next = push_out(next, collider, self.chain.hit_radius);
                    }
                }
                next = constrain(head, next, length, target);
                prev[k + 1] = cur[k + 1];
                cur[k + 1] = next;
            }
            let sim_offset = cur[k + 1] - cur[k];
            let sim_dir = if sim_offset.length_squared() > 1e-18 {
                sim_offset.normalize()
            } else {
                anim_dir
            };
            // Inherit the parent's swing, then swing the rest of the way to the simulated tail.
            let swing = Quat::from_rotation_arc(inherited * anim_dir, sim_dir);
            let delta = (swing * inherited).normalize();
            deltas[k] = delta;
            inherited = delta;
        }
        deltas[n - 1] = inherited;
    }
}

/// Place `p` at `length` from `head`, along `p - head` (or towards `fallback` when degenerate).
fn constrain(head: Vec3, p: Vec3, length: f32, fallback: Vec3) -> Vec3 {
    let d = p - head;
    let l = d.length();
    if l > 1e-9 {
        head + d * (length / l)
    } else {
        let f = (fallback - head).normalize_or_zero();
        head + f * length
    }
}

/// Push `p` (radius `hit`) out of a sphere/capsule collider.
fn push_out(p: Vec3, c: &WorldCollider, hit: f32) -> Vec3 {
    let axis = c.b - c.a;
    let len2 = axis.length_squared();
    let t = if len2 > 1e-12 {
        ((p - c.a).dot(axis) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let closest = c.a + axis * t;
    let d = p - closest;
    let dist = d.length();
    let min = c.radius + hit.max(0.0);
    if dist >= min {
        return p;
    }
    // Inside the core: push along +Y deterministically.
    let n = if dist > 1e-9 { d / dist } else { Vec3::Y };
    closest + n * min
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skeleton::LocalBind;

    /// root (the spring root's parent, at the origin) -> c0 -> c1 -> c2 -> c3, each 1 m along +X.
    fn rope() -> Skeleton {
        // The chain root c0 hangs from the anchor at x = 1; the rest runs along +X.
        let names: Vec<String> = ["anchor", "c0", "c1", "c2", "c3"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let parents = vec![None, Some(0), Some(1), Some(2), Some(3)];
        let binds = (0..5)
            .map(|i| LocalBind {
                translation: if i == 0 { Vec3::ZERO } else { Vec3::X },
                ..LocalBind::default()
            })
            .collect();
        Skeleton::from_local_bind(names, parents, binds).expect("valid skeleton")
    }

    fn chain(stiffness: f32, gravity: f32) -> SpringChain {
        SpringChain {
            name: "rope".into(),
            joints: ["c0", "c1", "c2", "c3"].iter().map(|s| s.to_string()).collect(),
            stiffness,
            damping: 0.15,
            gravity_power: gravity,
            ..SpringChain::default()
        }
    }

    fn setup(chain: SpringChain) -> SpringBoneSetup {
        SpringBoneSetup {
            chains: vec![chain],
            ..SpringBoneSetup::default()
        }
    }

    fn run(springs: &mut SpringBones, skeleton: &Skeleton, seconds: f32, fps: f32) -> Pose {
        let mut pose = Pose::default();
        let frames = (seconds * fps).round() as u32;
        for frame in 0..frames {
            pose = skeleton.bind_pose_pose();
            // Half a step of lead on the first frame keeps every frame rate on the same whole
            // number of substeps at the end (no floor() flapping at the boundary).
            let lead = if frame == 0 { 0.5 / 60.0 } else { 0.0 };
            springs.update(skeleton, &mut pose, &Transform::default(), 1.0 / fps + lead, None);
        }
        pose
    }

    fn tips(skeleton: &Skeleton, pose: &Pose) -> Vec<Vec3> {
        skeleton
            .globals(pose, None)
            .iter()
            .map(|g| g.w_axis.truncate())
            .collect()
    }

    #[test]
    fn a_malformed_skeleton_is_skipped_not_indexed() {
        let mut skeleton = rope();
        skeleton.parents.truncate(3);
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        let n = skeleton.len();
        let mut p = Pose {
            translations: vec![Vec3::ZERO; n],
            rotations: vec![Quat::IDENTITY; n],
            scales: vec![Vec3::ONE; n],
        };
        springs.update(&skeleton, &mut p, &Transform::default(), 0.1, None);
        // A forward parent reference is rejected too.
        let mut skeleton = rope();
        skeleton.parents[1] = Some(3);
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &Transform::default(), 0.1, None);
    }

    #[test]
    fn a_same_length_skeleton_swap_re_resolves() {
        let a = rope();
        let mut names = a.joint_names.clone();
        names.swap(1, 2);
        let b = Skeleton::from_local_bind(
            names,
            vec![None, Some(0), Some(1), Some(2), Some(3)],
            (0..5).map(|_| LocalBind::default()).collect(),
        )
        .expect("valid skeleton");
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        let mut pose = a.bind_pose_pose();
        springs.update(&a, &mut pose, &Transform::default(), 0.1, None);
        assert!(springs.is_resolved_for(&a));
        let mut pose = b.bind_pose_pose();
        springs.update(&b, &mut pose, &Transform::default(), 0.1, None);
        assert!(springs.is_resolved_for(&b) && !springs.is_resolved_for(&a));
    }

    #[test]
    fn a_free_chain_settles_hanging_down() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        let pose = run(&mut springs, &skeleton, 8.0, 60.0);
        let p = tips(&skeleton, &pose);
        for (i, expected) in [(2, -1.0), (3, -2.0), (4, -3.0)] {
            assert!(
                (p[i].x - 1.0).abs() < 0.02 && (p[i].y - expected).abs() < 0.02,
                "joint {i} at {:?}",
                p[i]
            );
        }
        let sim = springs.chain_positions("rope");
        assert!((sim[3].y + 3.0).abs() < 0.02, "simulated tip {:?}", sim[3]);
    }

    #[test]
    fn stiffness_one_keeps_the_animated_pose() {
        let skeleton = rope();
        // Gravity and a moving rig would bend a soft chain; a stiff one must not move.
        let mut springs = SpringBones::new(setup(chain(1.0, 9.81)));
        let pose = run(&mut springs, &skeleton, 2.0, 60.0);
        let p = tips(&skeleton, &pose);
        for (i, v) in p.iter().enumerate() {
            assert!(
                (*v - Vec3::new(i as f32, 0.0, 0.0)).length() < 1e-4,
                "joint {i} at {v:?}"
            );
        }
    }

    #[test]
    fn a_collider_pushes_the_chain_out() {
        let skeleton = rope();
        // A sphere sits in the way of the hanging tip.
        let mut s = setup(chain(0.0, 9.81));
        s.chains[0].hit_radius = 0.05;
        s.chains[0].colliders = vec!["ball".into()];
        s.colliders.push(SpringCollider {
            name: "ball".into(),
            joint: "anchor".into(),
            offset: Vec3::new(0.0, -2.6, 0.0),
            shape: ColliderShape::Sphere { radius: 0.4 },
        });
        let mut springs = SpringBones::new(s);
        run(&mut springs, &skeleton, 6.0, 60.0);
        assert!(springs.problems().is_empty(), "{:?}", springs.problems());
        let sim = springs.chain_positions("rope");
        for p in &sim[1..] {
            let d = (*p - Vec3::new(0.0, -2.6, 0.0)).length();
            assert!(d >= 0.45 - 1e-3, "{p:?} is {d} from the ball centre");
        }
        // Bone lengths survive the push.
        for w in sim.windows(2) {
            assert!(((w[1] - w[0]).length() - 1.0).abs() < 1e-3);
        }
    }

    #[test]
    fn a_capsule_collider_pushes_out_along_its_axis() {
        let c = WorldCollider {
            a: Vec3::new(0.0, 0.0, 0.0),
            b: Vec3::new(0.0, 2.0, 0.0),
            radius: 0.5,
        };
        let out = push_out(Vec3::new(0.2, 1.0, 0.0), &c, 0.1);
        assert!((out - Vec3::new(0.6, 1.0, 0.0)).length() < 1e-5, "{out:?}");
        let untouched = Vec3::new(2.0, 1.0, 0.0);
        assert_eq!(push_out(untouched, &c, 0.1), untouched);
    }

    #[test]
    fn the_result_does_not_depend_on_the_frame_rate() {
        let skeleton = rope();
        let mut reference = Vec::new();
        for fps in [30.0, 60.0, 120.0, 240.0] {
            let mut springs = SpringBones::new(setup(chain(0.05, 9.81)));
            // Half a second: the chain is still swinging, so a missed step would show.
            reference.push(tips(&skeleton, &run(&mut springs, &skeleton, 0.5, fps)));
        }
        assert!(reference[0][4].y < -0.05, "the chain has started to fall");
        for other in &reference[1..] {
            for (a, b) in reference[0].iter().zip(other) {
                assert!((*a - *b).length() < 1e-5, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn two_runs_are_bit_identical() {
        let skeleton = rope();
        let go = || {
            let mut springs = SpringBones::new(setup(chain(0.1, 9.81)));
            tips(&skeleton, &run(&mut springs, &skeleton, 1.5, 144.0))
        };
        assert_eq!(go(), go());
    }

    #[test]
    fn a_moving_rig_drags_the_chain() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.02, 0.0)));
        let mut rig = Transform::default();
        let mut pose = skeleton.bind_pose_pose();
        for _ in 0..120 {
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
            pose = skeleton.bind_pose_pose();
            rig.translation.z += 0.05;
        }
        let tip = springs.chain_positions("rope")[3];
        // The tip trails behind the rig (smaller z than the root).
        assert!(
            tip.z < rig.translation.z - 0.1,
            "tip z {} vs rig z {}",
            tip.z,
            rig.translation.z
        );
    }

    #[test]
    fn a_teleport_reseeds_the_whole_chain() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        let mut rig = Transform::default();
        for _ in 0..60 {
            let mut pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
        }
        rig.translation = Vec3::new(50.0, 0.0, 0.0);
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
        for p in springs.chain_positions("rope") {
            assert!((p.x - 50.0).abs() < 5.0, "{p:?} was left behind by the teleport");
        }
    }

    /// Review w11 #8: a root moving further than the chain's length every frame (here 4 m a
    /// frame against a 3 m chain) is fast motion, not a teleport: the chain keeps simulating and
    /// trails behind (it used to reseed every frame and stand rigid). A jump off that motion is
    /// still a teleport.
    #[test]
    fn fast_steady_motion_keeps_the_simulation() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.02, 0.0)));
        let mut rig = Transform::default();
        for _ in 0..30 {
            let mut pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
            rig.translation.z += 4.0;
        }
        let root_z = springs.chain_positions("rope")[0].z;
        let tip = springs.chain_positions("rope")[3];
        assert!(
            tip.z < root_z - 1.0,
            "the tip trails: tip z {} vs root z {root_z}",
            tip.z
        );
        // A 50 m jump sideways off the motion reseeds.
        rig.translation.x = 50.0;
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
        for p in springs.chain_positions("rope") {
            assert!((p.x - 50.0).abs() < 5.0, "{p:?} was left behind by the teleport");
        }
    }

    #[test]
    fn a_zero_substep_frame_keeps_the_last_simulated_aim() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.0, 0.0)));
        let mut rig = Transform::default();
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
        let aim = pose.rotations.clone();
        rig.translation.y = 0.2;
        pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &rig, 1.0 / 240.0, None);
        assert_eq!(springs.steps_last_frame(), 0);
        for (a, b) in aim.iter().zip(&pose.rotations) {
            assert!(a.angle_between(*b) < 1e-5);
        }
    }

    /// Audit (corr-animation): the one-second backlog clamp kept a 0.5 Hz chain below one step
    /// forever.
    #[test]
    fn a_step_rate_under_one_hertz_still_steps() {
        let skeleton = rope();
        let mut s = setup(chain(0.0, 9.81));
        s.step_hz = 0.5;
        let mut springs = SpringBones::new(s);
        let mut total = 0;
        for _ in 0..10 {
            let mut pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &Transform::default(), 1.0, None);
            total += springs.steps_last_frame();
        }
        assert_eq!(total, 5);
    }

    #[test]
    fn a_zero_step_rate_does_not_poison_the_accumulator() {
        let skeleton = rope();
        let mut s = setup(chain(0.0, 9.81));
        s.step_hz = 0.0;
        let mut springs = SpringBones::new(s);
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &Transform::default(), 1.0 / 60.0, None);
        springs.setup_mut().step_hz = 60.0;
        for _ in 0..4 {
            pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &Transform::default(), 1.0 / 60.0, None);
        }
        assert!(springs.steps_last_frame() >= 1, "the accumulator is NaN");
    }

    #[test]
    fn a_negative_rig_scale_keeps_the_collider_radius() {
        let skeleton = rope();
        let mut s = setup(chain(0.0, 9.81));
        s.chains[0].colliders = vec!["ball".into()];
        s.colliders.push(SpringCollider {
            name: "ball".into(),
            joint: "anchor".into(),
            offset: Vec3::new(0.0, -2.6, 0.0),
            shape: ColliderShape::Sphere { radius: 0.4 },
        });
        let mut springs = SpringBones::new(s);
        let rig = Transform {
            scale: Vec3::splat(-1.0),
            ..Transform::default()
        };
        let mut pose = Pose::default();
        for _ in 0..360 {
            pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
        }
        let ball = rig.translation + Vec3::new(0.0, 2.6, 0.0);
        let _ = pose;
        for p in &springs.chain_positions("rope")[1..] {
            assert!((*p - ball).length() >= 0.4 - 1e-3, "{p:?} is inside the collider");
        }
    }

    #[test]
    fn lod_freezes_and_reduces() {
        let skeleton = rope();
        let mut s = setup(chain(0.0, 9.81));
        s.lod = SpringLod {
            reduced_distance: 10.0,
            reduced_step_hz: 30.0,
            freeze_distance: 30.0,
        };
        let mut springs = SpringBones::new(s);
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &Transform::default(), 0.034, Some(15.0));
        assert!(springs.reduced() && !springs.frozen());
        assert_eq!(springs.steps_last_frame(), 1);
        let mut pose = skeleton.bind_pose_pose();
        springs.update(&skeleton, &mut pose, &Transform::default(), 0.034, Some(50.0));
        assert!(springs.frozen());
        assert_eq!(pose, skeleton.bind_pose_pose(), "frozen shows the animation");
        assert!(springs.chain_positions("rope").is_empty());
    }

    #[test]
    fn unbound_chains_are_reported_not_fatal() {
        let skeleton = rope();
        let mut c = chain(0.0, 9.81);
        c.joints = vec!["c0".into(), "c2".into()];
        let mut other = chain(0.0, 9.81);
        other.name = "ghost".into();
        other.joints = vec!["nope".into(), "c1".into()];
        let mut springs = SpringBones::new(SpringBoneSetup {
            chains: vec![c, other],
            ..SpringBoneSetup::default()
        });
        let pose = run(&mut springs, &skeleton, 0.5, 60.0);
        assert_eq!(springs.problems().len(), 2, "{:?}", springs.problems());
        assert_eq!(pose, skeleton.bind_pose_pose());
    }

    #[test]
    fn ron_round_trips_and_validates() {
        let text = r#"(
            format_version: 1,
            chains: [(name: "hair", joints: ["a", "b"], stiffness: 0.5, colliders: ["head"])],
            colliders: [(name: "head", joint: "neck", shape: Sphere(radius: 0.1))],
        )"#;
        let setup = SpringBoneSetup::from_ron_str(text).expect("parses");
        assert!((setup.step_hz - 60.0).abs() < f32::EPSILON);
        assert_eq!(setup.chains[0].damping, 0.1);
        let again = SpringBoneSetup::from_ron_str(&setup.to_ron_string().expect("writes")).expect("reparses");
        assert_eq!(setup, again);
        assert!(SpringBoneSetup::from_ron_str("(format_version: 2)").is_err());
        let bad = r#"(format_version: 1, chains: [(name: "x", joints: ["a"])])"#;
        assert!(SpringBoneSetup::from_ron_str(bad).is_err());
        let unknown = r#"(format_version: 1, chains: [(name: "x", joints: ["a", "b"], colliders: ["z"])])"#;
        assert!(SpringBoneSetup::from_ron_str(unknown).is_err());
        let fp = spring_schema_fingerprint();
        for field in ["gravity_power", "hit_radius", "freeze_distance", "Capsule"] {
            assert!(fp.contains(field), "{field} missing from {fp}");
        }
    }

    #[test]
    fn non_finite_stiffness_or_damping_is_rejected() {
        for (s, d) in [(f32::NAN, 0.1), (0.5, f32::INFINITY)] {
            let mut c = chain(0.5, 0.0);
            c.name = "hair".into();
            c.joints = vec!["a".into(), "b".into()];
            c.stiffness = s;
            c.damping = d;
            let setup = SpringBoneSetup {
                chains: vec![c],
                ..SpringBoneSetup::default()
            };
            assert!(setup.validate().is_err(), "{s} {d}");
        }
    }

    /// audit-v035-numerics M3 (fix-audit-wave12): a zero rig rotation (a defaulted buffer)
    /// normalized to NaN and every spring joint's pose rotation became NaN that frame. It now
    /// stands in as identity: the chain simulates as under an identity rig.
    #[test]
    fn a_zero_rig_rotation_does_not_nan_the_chain() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        let rig = Transform {
            rotation: Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
            ..Transform::default()
        };
        for _ in 0..30 {
            let mut pose = skeleton.bind_pose_pose();
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
            assert!(
                pose.rotations.iter().all(|q| q.is_finite()),
                "{:?}",
                pose.rotations
            );
        }
    }

    /// M3: a zero rotation in the animated pose (one joint) no longer poisons the joints below
    /// it, and a NaN rig translation leaves the animated rotations in place instead of NaN.
    #[test]
    fn a_zero_pose_rotation_or_a_nan_rig_does_not_write_nan() {
        let skeleton = rope();
        let mut springs = SpringBones::new(setup(chain(0.0, 9.81)));
        for frame in 0..30 {
            let mut pose = skeleton.bind_pose_pose();
            pose.rotations[1] = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
            let rig = if frame % 2 == 0 {
                Transform::default()
            } else {
                Transform::from_translation(Vec3::NAN)
            };
            springs.update(&skeleton, &mut pose, &rig, 1.0 / 60.0, None);
            for (i, q) in pose.rotations.iter().enumerate() {
                assert!(i == 1 || q.is_finite(), "frame {frame} joint {i}: {q:?}");
            }
        }
    }
}
