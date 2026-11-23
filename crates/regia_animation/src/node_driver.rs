// SPDX-License-Identifier: MPL-2.0
//! Procedural hinges, spins and slews on plain nodes (ADR-8410).
//!
//! Portions copied from Direct Order 27 (shell-regia/game/src/world/rig.rs, commit c9513623f), ADR-8012.
//!
//! Named adaptations (see `docs/ported-sources.toml`): a hinge target is the named parameter in
//! degrees, clamped into the closed interval and approached at `rate` degrees per second, with no
//! ease and no one-degree floor; a spin speed is that parameter in degrees per second about the
//! caller axis, wrapped into `[0, 360)`; a slew limits yaw and pitch independently, and the pitch
//! parameter is `{input}.pitch`; the driven matrix is the clip-local matrix times the rotation,
//! with no import-frame conjugation. Sidecar loaders, catalogs and game stems are not ported.
//! There is no new file format. Rotations use `det_quat_from_axis_angle`. This is presentation
//! math, not a lockstep pin.
//!
//! Order, after skeletal `apply_animation`: `sample_plain_node_clips`, then `apply_node_drives`.
//! Drives run in `NodeDriver::drives` order. Later drives on the same node multiply on the right.
//! A joint (`PlainNodeXform::is_joint`) keeps its clip matrix; its angle still advances. A missing
//! parameter reads as `0`. Duplicate parameter names keep the first value.

use regia_core::math::detmath::det_quat_from_axis_angle;
use regia_core::math::{Mat4, Quat, Vec3};
use regia_ecs::prelude::{
    Commands, Component, Entity, Query, Reflect, ReflectComponent, Res, bevy_ecs, bevy_reflect,
};
use serde::{Deserialize, Serialize};

use crate::node_tracks::NodeAnimation;

const DEGREE: f32 = std::f32::consts::PI / 180.0;

/// One plain-node drive. `node` is the source index from the clip's node tracks.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
pub struct NodeDrive {
    /// Source-node index the rotation applies to.
    pub node: u32,
    /// Hinge, spin or slew.
    pub kind: DriveKind,
    /// Parameter name the game writes. A slew also reads `{input}.pitch`.
    pub input: String,
}

/// How a drive turns its node. Angles and rates are degrees and degrees per second.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
pub enum DriveKind {
    /// Rotate about `axis` toward the input, staying inside `[min, max]`.
    Hinge {
        /// Rotation axis in the node's parent frame. A zero axis contributes no rotation.
        axis: Vec3,
        /// Lower limit in degrees. Swapped with `max` when they are out of order.
        min: f32,
        /// Upper limit in degrees.
        max: f32,
        /// Maximum approach speed in degrees per second. Negative or non-finite means no motion.
        rate: f32,
    },
    /// Accumulate `input` degrees per second about `axis`.
    Spin {
        /// Rotation axis in the node's parent frame. A zero axis contributes no rotation.
        axis: Vec3,
    },
    /// Rate-limit yaw and pitch independently. Pitch reads `{input}.pitch`, or `0` when absent.
    Slew {
        /// Yaw axis. Applied before pitch.
        yaw_axis: Vec3,
        /// Pitch axis, in the frame after yaw.
        pitch_axis: Vec3,
        /// Shared maximum speed for each axis, in degrees per second.
        max_rate: f32,
        /// Closed limits for yaw and pitch, in degrees.
        limits: SlewLimits,
    },
}

/// Closed yaw and pitch limits for a [`DriveKind::Slew`], in degrees.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct SlewLimits {
    /// Lower yaw limit.
    pub yaw_min: f32,
    /// Upper yaw limit.
    pub yaw_max: f32,
    /// Lower pitch limit.
    pub pitch_min: f32,
    /// Upper pitch limit.
    pub pitch_max: f32,
}

/// Drives attached to one entity, applied in list order.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Component)]
pub struct NodeDriver {
    /// Drives in application order.
    pub drives: Vec<NodeDrive>,
}

/// One named value a game writes for the drives to read.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Reflect)]
pub struct DriveParam {
    /// Parameter name. The first copy of a name wins.
    pub name: String,
    /// Value in the drive's units (degrees, or degrees per second for a spin).
    pub value: f32,
}

/// Named parameters for [`NodeDriver`]. A linear list: the first matching name wins.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Component)]
pub struct NodeDriveParams {
    /// Parameters in insertion order.
    pub values: Vec<DriveParam>,
}

impl NodeDriveParams {
    /// Build a list without collapsing duplicates, so the first copy of a name wins.
    #[must_use]
    pub fn from_pairs<I, S>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (S, f32)>,
        S: Into<String>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(name, value)| DriveParam {
                    name: name.into(),
                    value,
                })
                .collect(),
        }
    }

    /// The first value named `name`, or `0` when none is present.
    #[must_use]
    pub fn get(&self, name: &str) -> f32 {
        self.values
            .iter()
            .find(|param| param.name == name)
            .map(|param| param.value)
            .unwrap_or(0.0)
    }

    /// Write `value` onto the first parameter named `name`, or append one.
    pub fn set(&mut self, name: impl Into<String>, value: f32) {
        let name = name.into();
        if let Some(param) = self.values.iter_mut().find(|param| param.name == name) {
            param.value = value;
        } else {
            self.values.push(DriveParam { name, value });
        }
    }
}

/// Pitch parameter for a slew whose yaw input is `input`: `{input}.pitch`.
#[must_use]
pub fn slew_pitch_param(input: &str) -> String {
    let mut name = String::with_capacity(input.len() + 6);
    name.push_str(input);
    name.push_str(".pitch");
    name
}

/// Stored angles for one drive, in degrees. `secondary` is the slew pitch.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
pub struct NodeDriveAngles {
    /// Hinge angle, spin angle, or slew yaw.
    pub primary: f32,
    /// Slew pitch. Unused drives leave this at zero.
    pub secondary: f32,
}

/// Angles parallel to [`NodeDriver::drives`].
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Component)]
pub struct NodeDriveState {
    /// One entry per drive. A length mismatch is resized with zeros on the next step.
    pub angles: Vec<NodeDriveAngles>,
}

impl NodeDriveState {
    /// Advance every drive by `dt` seconds. Non-finite inputs leave that angle unchanged.
    /// An angle outside its limit steps toward the clamped target and is not snapped.
    pub fn step(&mut self, driver: &NodeDriver, params: &NodeDriveParams, dt: f32) {
        if self.angles.len() != driver.drives.len() {
            self.angles
                .resize(driver.drives.len(), NodeDriveAngles::default());
        }
        for (drive, angles) in driver.drives.iter().zip(self.angles.iter_mut()) {
            match &drive.kind {
                DriveKind::Hinge { min, max, rate, .. } => {
                    angles.primary =
                        step_toward(angles.primary, params.get(&drive.input), *min, *max, *rate, dt);
                    angles.secondary = 0.0;
                }
                DriveKind::Spin { .. } => {
                    angles.primary = spin_angle(angles.primary, params.get(&drive.input), dt);
                    angles.secondary = 0.0;
                }
                DriveKind::Slew { max_rate, limits, .. } => {
                    let pitch = slew_pitch_param(&drive.input);
                    angles.primary = step_toward(
                        angles.primary,
                        params.get(&drive.input),
                        limits.yaw_min,
                        limits.yaw_max,
                        *max_rate,
                        dt,
                    );
                    angles.secondary = step_toward(
                        angles.secondary,
                        params.get(&pitch),
                        limits.pitch_min,
                        limits.pitch_max,
                        *max_rate,
                        dt,
                    );
                }
            }
        }
    }
}

/// The host's plain-node clip and the time, in seconds, at which to sample it.
/// The host owns the clock: these systems do not advance `time`.
#[derive(Component, Clone, Debug, Serialize, Deserialize)]
pub struct PlainNodePlayer {
    /// Node tracks to sample. Not reflected: the clip body is asset data.
    pub animation: NodeAnimation,
    /// Sample time in seconds, in the clip's source clock.
    pub time: f32,
}

/// One sampled node after the clip, and after the drives when it is not a joint.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize, Reflect)]
pub struct PlainNodeXform {
    /// Source-node index.
    pub index: u32,
    /// A skin joint. Drives do not change `driven` for a joint.
    pub is_joint: bool,
    /// Clip-local matrix for this sample. Drives do not write it.
    pub clip: Mat4,
    /// `clip` times each drive rotation on the right, or `clip` for a joint.
    pub driven: Mat4,
}

/// Pose written by [`sample_plain_node_clips`] and updated by [`apply_node_drives`].
///
/// `valid` stays false until a sample succeeds. A failed sample clears `nodes` and
/// leaves `valid` false so the drives do not step on that frame.
#[derive(Component, Clone, Debug, Default, PartialEq, Serialize, Deserialize, Reflect)]
#[reflect(Component)]
pub struct PlainNodePose {
    /// Whether `nodes` is a successful sample.
    pub valid: bool,
    /// Sampled nodes. Empty when the sample failed or the clip has none.
    pub nodes: Vec<PlainNodeXform>,
}

/// Rotation for the current angles, as a unit quaternion. A zero or non-finite axis is identity.
#[must_use]
pub fn drive_rotation(kind: &DriveKind, primary: f32, secondary: f32) -> Quat {
    match kind {
        DriveKind::Hinge { axis, .. } | DriveKind::Spin { axis } => axis_rotation(*axis, primary),
        DriveKind::Slew {
            yaw_axis, pitch_axis, ..
        } => axis_rotation(*yaw_axis, primary) * axis_rotation(*pitch_axis, secondary),
    }
}

/// `bind` times `rotation`, so a translated node origin stays fixed.
#[must_use]
pub fn bind_times_rotation(bind: Mat4, rotation: Quat) -> Mat4 {
    bind * Mat4::from_quat(rotation)
}

/// Sample each [`PlainNodePlayer`] into its [`PlainNodePose`].
/// A sample error clears that pose and marks it invalid. The player's time is not changed.
pub fn sample_plain_node_clips(mut players: Query<(&PlainNodePlayer, &mut PlainNodePose)>) {
    let _span = tracing::debug_span!("sample_plain_node_clips").entered();
    let mut samples = Vec::new();
    for (player, mut pose) in &mut players {
        match player.animation.sample(player.time, &mut samples) {
            Ok(()) => {
                pose.valid = true;
                pose.nodes.clear();
                pose.nodes
                    .extend(samples.iter().copied().map(|sample| PlainNodeXform {
                        index: sample.index,
                        is_joint: sample.is_joint,
                        clip: sample.local,
                        driven: sample.local,
                    }));
            }
            Err(_) => {
                pose.valid = false;
                pose.nodes.clear();
            }
        }
    }
}

/// One driven entity: the drive list, optional parameters and angles, and the sampled pose.
type DrivenParts = (
    Entity,
    &'static NodeDriver,
    Option<&'static NodeDriveParams>,
    Option<&'static mut NodeDriveState>,
    Option<&'static mut PlainNodePose>,
);

/// Step [`NodeDriveState`] and write `clip * R` onto non-joint nodes.
/// Runs after [`sample_plain_node_clips`]. An invalid pose skips the entity for the frame.
/// A missing state component is inserted after the same frame's pose write.
pub fn apply_node_drives(time: Res<regia_app::Time>, mut commands: Commands, mut driven: Query<DrivenParts>) {
    let _span = tracing::debug_span!("apply_node_drives").entered();
    let dt = time.delta_secs();
    for (entity, driver, params, state, mut pose) in &mut driven {
        if pose.as_ref().is_some_and(|pose| !pose.valid) {
            continue;
        }
        let owned = params.cloned().unwrap_or_default();
        let mut created = None;
        let angles = if let Some(mut state) = state {
            state.step(driver, &owned, dt);
            state.angles.clone()
        } else {
            let mut fresh = NodeDriveState::default();
            fresh.step(driver, &owned, dt);
            let angles = fresh.angles.clone();
            created = Some(fresh);
            angles
        };
        if let Some(pose) = pose.as_mut() {
            write_drives(driver, &angles, pose);
        }
        if let Some(state) = created {
            commands.entity(entity).insert(state);
        }
    }
}

fn write_drives(driver: &NodeDriver, angles: &[NodeDriveAngles], pose: &mut PlainNodePose) {
    for (drive, angle) in driver.drives.iter().zip(angles) {
        let Some(slot) = pose.nodes.iter().position(|node| node.index == drive.node) else {
            continue;
        };
        if pose.nodes[slot].is_joint {
            continue;
        }
        let rotation = drive_rotation(&drive.kind, angle.primary, angle.secondary);
        pose.nodes[slot].driven = bind_times_rotation(pose.nodes[slot].driven, rotation);
    }
}

fn axis_rotation(axis: Vec3, degrees: f32) -> Quat {
    if !degrees.is_finite() {
        return Quat::IDENTITY;
    }
    det_quat_from_axis_angle(axis, degrees * DEGREE)
}

fn finite_interval(min: f32, max: f32) -> Option<(f32, f32)> {
    if !min.is_finite() || !max.is_finite() {
        return None;
    }
    if min <= max {
        Some((min, max))
    } else {
        Some((max, min))
    }
}

fn clamp_to(value: f32, min: f32, max: f32) -> f32 {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

fn approach(current: f32, goal: f32, step: f32) -> f32 {
    if !current.is_finite() || !goal.is_finite() || !step.is_finite() || step < 0.0 {
        return current;
    }
    let delta = goal - current;
    if delta.abs() <= step {
        goal
    } else if delta > 0.0 {
        current + step
    } else {
        current - step
    }
}

fn step_toward(current: f32, target: f32, min: f32, max: f32, rate: f32, dt: f32) -> f32 {
    if !current.is_finite() || !target.is_finite() || !rate.is_finite() || !dt.is_finite() {
        return current;
    }
    let Some((min, max)) = finite_interval(min, max) else {
        return current;
    };
    let goal = clamp_to(target, min, max);
    let rate = if rate > 0.0 { rate } else { 0.0 };
    let dt = if dt > 0.0 { dt } else { 0.0 };
    approach(current, goal, rate * dt)
}

fn spin_angle(current: f32, speed: f32, dt: f32) -> f32 {
    if !current.is_finite() || !speed.is_finite() || !dt.is_finite() {
        return current;
    }
    let dt = if dt > 0.0 { dt } else { 0.0 };
    let next = current + speed * dt;
    if !next.is_finite() {
        return current;
    }
    next.rem_euclid(360.0)
}
