// SPDX-License-Identifier: MPL-2.0
//! Spectator camera for replays and demos (`book/agents/demo.md`, ADR-1569): pure logic with no
//! world access. A caller feeds [`SpectatorInput`] and the frame time, and reads the pose back
//! with [`SpectatorCamera::pose`].
//!
//! # Modes
//!
//! - **Free-fly**: the camera moves along its own view axes (right, world up, forward) at the
//!   current speed and turns by the look deltas. Convention as everywhere in the engine: +Y up,
//!   forward is −Z at zero yaw and pitch, yaw turns about +Y (positive turns left), positive
//!   pitch looks up.
//! - **Follow**: the camera orbits a smoothed anchor that chases the followed point. The anchor
//!   moves by exponential smoothing, `alpha = 1 − exp(−dt / smoothing_secs)`, which gives the same
//!   path for one 20 ms frame as for two 10 ms frames (up to the target moving in between). Look
//!   deltas orbit the camera around the anchor; zoom changes the orbit distance within limits.
//! - **Orbit**: same look and zoom controls, with the anchor fixed to the current target.
//! - **POV**: target position plus a rotated local eye offset, preserving the target's full
//!   orientation. Movement, look and zoom input are ignored; a missing target holds the pose.
//! - **Overhead**: a vertical view over an independent focus, initially the target (or camera
//!   position). Movement pans along world +X/−Z at a configurable fraction of height per second;
//!   zoom changes height in meters. See [`SpectatorOverheadConfig`].
//!
//! # Speed
//!
//! Speed is `base_speed × speed_factor^level` with an integer level clamped to
//! `±MAX_SPEED_LEVEL`: one input step is a fixed ratio, so stepping up then down returns to the
//! same speed exactly.
//!
//! # Robustness
//!
//! Non-finite input components count as zero, a non-positive or non-finite `dt` is ignored, and a
//! long frame is clamped to [`MAX_STEP_SECS`] so a hitch cannot throw the camera across the map.
//!
//! # Driving a render camera
//!
//! [`SpectatorPlugin`] connects the logic to the scene (ADR-1878): every frame, in `PostUpdate`
//! before transform propagation, it composes the followed entity's current local transform chain
//! ([`SpectatorDrive::follow`], including a parent that moved this frame) and feeds that world
//! pose in as the follow target. It integrates [`SpectatorDrive::input`] over the wall-clock
//! frame time ([`RealTime`], so the camera still flies while a replay is paused), clears the
//! one-shot parts of the input, and writes the pose into the [`Transform`] of every entity marked
//! [`SpectatorView`] (scale untouched). Propagation then publishes that camera `GlobalTransform`
//! in the same frame, which is the pose rendering reads. A chain that cannot be composed (a
//! parent with no `Transform`, a cycle, or more than `LIVE_POSE_DEPTH` ancestors) falls back to
//! the last propagated world pose. Put [`SpectatorView`] on the render camera.
//! A followed entity that disappears is remembered ([`SpectatorDrive::lost`]); when a demo seek
//! brings it back under a new id ([`crate::demo_world::DemoRemap`]) the follow resumes.

use regia_app::RealTime;
use regia_app::prelude::*;
use regia_core::math::{EulerRot, Quat, Vec2, Vec3};
use regia_scene::{GlobalTransform, Transform};

/// Largest frame time one [`SpectatorCamera::update`] integrates.
pub const MAX_STEP_SECS: f32 = 0.25;
/// Speed level limit: the level is clamped to `-MAX_SPEED_LEVEL..=MAX_SPEED_LEVEL`.
pub const MAX_SPEED_LEVEL: i32 = 12;

/// Tuning for a [`SpectatorCamera`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectatorConfig {
    /// Free-fly speed in metres per second at speed level 0.
    pub base_speed: f32,
    /// Speed ratio of one speed level step (greater than 1).
    pub speed_factor: f32,
    /// Largest pitch magnitude in radians (below π/2 so the view never flips).
    pub pitch_limit: f32,
    /// Seconds for the follow anchor to close 63 % of the gap to its target; 0 follows rigidly.
    pub follow_smoothing_secs: f32,
    /// Orbit distance when follow starts, metres.
    pub follow_distance: f32,
    /// Closest orbit distance, metres.
    pub min_follow_distance: f32,
    /// Farthest orbit distance, metres.
    pub max_follow_distance: f32,
}

impl SpectatorConfig {
    /// This tuning with every unusable field replaced by the default's: a speed or a factor that
    /// is not finite and positive (the factor above 1), a pitch limit outside `0..π/2`, a negative
    /// smoothing, or follow distances that are not finite and `0 < min <= max` (both then take
    /// the defaults). [`SpectatorCamera::new`] applies it, so a bad field can neither panic in a
    /// clamp nor turn the pose NaN.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let d = Self::default();
        let pick = |value: f32, ok: bool, default: f32| if ok { value } else { default };
        let distances_ok = self.min_follow_distance.is_finite()
            && self.max_follow_distance.is_finite()
            && self.min_follow_distance > 0.0
            && self.min_follow_distance <= self.max_follow_distance;
        let (min_follow_distance, max_follow_distance) = if distances_ok {
            (self.min_follow_distance, self.max_follow_distance)
        } else {
            (d.min_follow_distance, d.max_follow_distance)
        };
        Self {
            base_speed: pick(
                self.base_speed,
                self.base_speed.is_finite() && self.base_speed > 0.0,
                d.base_speed,
            ),
            speed_factor: pick(
                self.speed_factor,
                self.speed_factor.is_finite() && self.speed_factor > 1.0,
                d.speed_factor,
            ),
            pitch_limit: pick(
                self.pitch_limit,
                self.pitch_limit > 0.0 && self.pitch_limit < std::f32::consts::FRAC_PI_2,
                d.pitch_limit,
            ),
            follow_smoothing_secs: pick(
                self.follow_smoothing_secs,
                self.follow_smoothing_secs.is_finite() && self.follow_smoothing_secs >= 0.0,
                d.follow_smoothing_secs,
            ),
            follow_distance: pick(
                self.follow_distance,
                self.follow_distance.is_finite(),
                d.follow_distance,
            ),
            min_follow_distance,
            max_follow_distance,
        }
    }
}

impl Default for SpectatorConfig {
    fn default() -> Self {
        Self {
            base_speed: 8.0,
            speed_factor: 1.25,
            pitch_limit: 1.55,
            follow_smoothing_secs: 0.2,
            follow_distance: 6.0,
            min_follow_distance: 1.0,
            max_follow_distance: 60.0,
        }
    }
}

/// Which behavior the camera has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SpectatorMode {
    /// Moves freely under the caller's input.
    #[default]
    FreeFly,
    /// Orbits a smoothed anchor that chases a followed point.
    Follow,
    /// Orbits the target directly, keeping it centered without follow lag.
    Orbit,
    /// Uses the target's full orientation and a target-local eye offset.
    Pov,
    /// Looks straight down at an independently pannable focus point.
    Overhead,
}

impl SpectatorMode {
    /// A stable lowercase name for the agent protocol.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::FreeFly => "free_fly",
            Self::Follow => "follow",
            Self::Orbit => "orbit",
            Self::Pov => "pov",
            Self::Overhead => "overhead",
        }
    }

    /// The mode named `name` (as given by [`SpectatorMode::name`]).
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "free_fly" => Some(Self::FreeFly),
            "follow" => Some(Self::Follow),
            "orbit" => Some(Self::Orbit),
            "pov" => Some(Self::Pov),
            "overhead" => Some(Self::Overhead),
            _ => None,
        }
    }

    /// Cycles follow, orbit, POV, free-fly, overhead, then follow.
    #[must_use]
    pub fn next(self) -> Self {
        match self {
            Self::Follow => Self::Orbit,
            Self::Orbit => Self::Pov,
            Self::Pov => Self::FreeFly,
            Self::FreeFly => Self::Overhead,
            Self::Overhead => Self::Follow,
        }
    }
}

/// Limits and movement tuning for the overhead spectator camera.
///
/// Separate from [`SpectatorConfig`] so existing configuration literals still compile.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpectatorOverheadConfig {
    /// Initial distance above the focus, in meters.
    pub height: f32,
    /// Minimum distance above the focus, in meters; strictly positive.
    pub min_height: f32,
    /// Maximum distance above the focus, in meters; at least `min_height`.
    pub max_height: f32,
    /// Horizontal travel per second, as a fraction of the current height.
    pub pan_speed: f32,
}

impl Default for SpectatorOverheadConfig {
    fn default() -> Self {
        Self {
            height: 30.0,
            min_height: 1.0,
            max_height: 1000.0,
            pan_speed: 1.0,
        }
    }
}

impl SpectatorOverheadConfig {
    /// Replaces invalid limits and nonfinite or negative speeds with defaults, then clamps height.
    #[must_use]
    pub fn sanitized(self) -> Self {
        let defaults = Self::default();
        let (min_height, max_height) = if self.min_height.is_finite()
            && self.max_height.is_finite()
            && self.min_height > 0.0
            && self.max_height >= self.min_height
        {
            (self.min_height, self.max_height)
        } else {
            (defaults.min_height, defaults.max_height)
        };
        Self {
            height: if self.height.is_finite() {
                self.height
            } else {
                defaults.height
            }
            .clamp(min_height, max_height),
            min_height,
            max_height,
            pan_speed: if self.pan_speed.is_finite() && self.pan_speed >= 0.0 {
                self.pan_speed
            } else {
                defaults.pan_speed
            },
        }
    }
}

/// One frame of spectator input.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct SpectatorInput {
    /// Free-fly movement along the camera's right (x), world up (y) and forward (z) axes, each in
    /// −1..=1; longer vectors are normalized so diagonals are not faster. Overhead uses x/z
    /// for world +X/−Z pan and ignores y. Follow, Orbit and POV ignore movement.
    pub movement: Vec3,
    /// Yaw and pitch change this frame, radians.
    pub look: Vec2,
    /// Speed level change (positive faster), applied before movement.
    pub speed_steps: i32,
    /// Follow/Orbit distance or Overhead height change this frame, meters (positive zooms out).
    pub zoom: f32,
}

/// The spectator camera state. Also a resource, so the agent protocol (`spectator.*`) can drive one
/// per world.
#[derive(Resource, Debug, Clone, PartialEq)]
pub struct SpectatorCamera {
    config: SpectatorConfig,
    mode: SpectatorMode,
    position: Vec3,
    yaw: f32,
    pitch: f32,
    roll: f32,
    rotation: Quat,
    speed_level: i32,
    anchor: Vec3,
    distance: f32,
    target: Option<Vec3>,
    target_rotation: Quat,
    pov_offset: Vec3,
    overhead_config: SpectatorOverheadConfig,
    overhead_focus: Vec3,
    overhead_height: f32,
}

fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

fn sanitize(v: Vec3) -> Vec3 {
    Vec3::new(finite_or_zero(v.x), finite_or_zero(v.y), finite_or_zero(v.z))
}

impl SpectatorCamera {
    /// A free-fly camera at `position` looking along −Z. `config` is [`SpectatorConfig::sanitized`]
    /// first.
    #[must_use]
    pub fn new(config: SpectatorConfig, position: Vec3) -> Self {
        let config = config.sanitized();
        let distance = config
            .follow_distance
            .clamp(config.min_follow_distance, config.max_follow_distance);
        Self {
            config,
            mode: SpectatorMode::FreeFly,
            position: sanitize(position),
            yaw: 0.0,
            pitch: 0.0,
            roll: 0.0,
            rotation: Quat::IDENTITY,
            speed_level: 0,
            anchor: Vec3::ZERO,
            distance,
            target: None,
            target_rotation: Quat::IDENTITY,
            pov_offset: Vec3::ZERO,
            overhead_config: SpectatorOverheadConfig::default(),
            overhead_focus: Vec3::ZERO,
            overhead_height: SpectatorOverheadConfig::default().height,
        }
    }

    /// The tuning.
    #[must_use]
    pub fn config(&self) -> &SpectatorConfig {
        &self.config
    }

    /// The current mode.
    #[must_use]
    pub fn mode(&self) -> SpectatorMode {
        self.mode
    }

    /// Camera position.
    #[must_use]
    pub fn position(&self) -> Vec3 {
        self.position
    }

    /// Yaw in radians.
    #[must_use]
    pub fn yaw(&self) -> f32 {
        self.yaw
    }

    /// Pitch in radians.
    #[must_use]
    pub fn pitch(&self) -> f32 {
        self.pitch
    }

    /// The speed level (0 is the base speed).
    #[must_use]
    pub fn speed_level(&self) -> i32 {
        self.speed_level
    }

    /// Free-fly speed in metres per second.
    #[must_use]
    pub fn speed(&self) -> f32 {
        self.config.base_speed * self.config.speed_factor.powi(self.speed_level)
    }

    /// The point Follow or Orbit circles (smoothed in Follow, exact in Orbit).
    #[must_use]
    pub fn anchor(&self) -> Vec3 {
        self.anchor
    }

    /// The current Follow/Orbit distance, in meters.
    #[must_use]
    pub fn follow_distance(&self) -> f32 {
        self.distance
    }

    /// Orientation, including target roll in POV and a vertical view in Overhead.
    #[must_use]
    pub fn rotation(&self) -> Quat {
        self.rotation
    }

    /// The unit view direction.
    #[must_use]
    pub fn forward(&self) -> Vec3 {
        self.rotation() * Vec3::NEG_Z
    }

    /// Position and orientation, for writing into a camera transform.
    #[must_use]
    pub fn pose(&self) -> (Vec3, Quat) {
        (self.position, self.rotation())
    }

    /// Sets the speed level (clamped).
    pub fn set_speed_level(&mut self, level: i32) {
        self.speed_level = level.clamp(-MAX_SPEED_LEVEL, MAX_SPEED_LEVEL);
    }

    /// Moves the camera to `position` and turns it to `yaw` and `pitch` (pitch clamped).
    /// Non-finite values are treated as zero.
    pub fn set_pose(&mut self, position: Vec3, yaw: f32, pitch: f32) {
        self.position = sanitize(position);
        self.yaw = finite_or_zero(yaw);
        self.pitch = finite_or_zero(pitch).clamp(-self.config.pitch_limit, self.config.pitch_limit);
        self.roll = 0.0;
        self.rotation = Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, self.roll);
        if self.mode == SpectatorMode::Follow {
            self.anchor = self.position + self.forward() * self.distance;
        }
    }

    /// Switches to free-fly, keeping the current pose.
    pub fn free_fly(&mut self) {
        self.mode = SpectatorMode::FreeFly;
    }

    /// Switches to follow mode. The anchor starts at the point the camera is looking at, so the
    /// view does not jump; it then eases toward the followed point set by
    /// [`SpectatorCamera::set_target`].
    pub fn follow(&mut self) {
        if self.mode != SpectatorMode::Follow {
            self.anchor = self.position + self.forward() * self.distance;
            self.mode = SpectatorMode::Follow;
        }
    }

    /// Selects a mode. Follow keeps its smooth entry; Orbit centers the target on the next
    /// update. Entering Overhead centers its focus on the current target (or camera position).
    /// Re-selecting the current mode leaves its focus and pose alone.
    pub fn set_mode(&mut self, mode: SpectatorMode) {
        if self.mode == mode {
            return;
        }
        match mode {
            SpectatorMode::FreeFly => self.free_fly(),
            SpectatorMode::Follow => self.follow(),
            SpectatorMode::Orbit => {
                self.anchor = self
                    .target
                    .unwrap_or(self.position + self.forward() * self.distance);
                self.mode = mode;
            }
            SpectatorMode::Pov => self.mode = mode,
            SpectatorMode::Overhead => {
                self.overhead_focus = self.target.unwrap_or(self.position);
                self.mode = mode;
            }
        }
    }

    /// Sets a target pose in engine coordinates. Rotation is normalized; a zero or nonfinite
    /// quaternion, or a nonfinite position, clears the target. No game-axis conversion is done.
    pub fn set_target_pose(&mut self, position: Vec3, rotation: Quat) {
        // Scale first so finite very large/small quaternion components normalize safely.
        let scale = rotation
            .to_array()
            .into_iter()
            .map(f32::abs)
            .fold(0.0_f32, f32::max);
        if !position.is_finite() || !rotation.is_finite() || scale == 0.0 {
            self.clear_target();
            return;
        }
        self.target_rotation =
            Quat::from_array(rotation.to_array().map(|component| component / scale)).normalize();
        self.target = Some(position);
    }

    /// The target's normalized orientation (identity until a pose is supplied).
    #[must_use]
    pub fn target_rotation(&self) -> Quat {
        self.target_rotation
    }

    /// Sets the POV eye offset in the target's local frame, in meters. Nonfinite components
    /// become zero. Scale on a followed entity does not scale this physical offset.
    pub fn set_pov_offset(&mut self, offset: Vec3) {
        self.pov_offset = sanitize(offset);
    }

    /// The target-local POV eye offset, in meters.
    #[must_use]
    pub fn pov_offset(&self) -> Vec3 {
        self.pov_offset
    }

    /// Replaces overhead tuning and resets the current height to its sanitized initial height.
    pub fn set_overhead_config(&mut self, config: SpectatorOverheadConfig) {
        self.overhead_config = config.sanitized();
        self.overhead_height = self.overhead_config.height;
    }

    /// Overhead tuning.
    #[must_use]
    pub fn overhead_config(&self) -> &SpectatorOverheadConfig {
        &self.overhead_config
    }

    /// The overhead camera's independent world-space focus.
    #[must_use]
    pub fn overhead_focus(&self) -> Vec3 {
        self.overhead_focus
    }

    /// Sets the overhead focus explicitly; nonfinite components become zero.
    pub fn set_overhead_focus(&mut self, focus: Vec3) {
        self.overhead_focus = sanitize(focus);
    }

    /// The current overhead height above the focus, in meters.
    #[must_use]
    pub fn overhead_height(&self) -> f32 {
        self.overhead_height
    }

    /// Sets the point Follow chases and Orbit centers (POV also uses it). Keeps the previous
    /// target orientation; use [`Self::set_target_pose`] when orientation is available.
    /// A nonfinite point clears the target. Overhead only reads it when entering the mode.
    pub fn set_target(&mut self, target: Vec3) {
        self.target = target.is_finite().then_some(target);
    }

    /// Stops tracking: Follow/Orbit hold their anchor and POV holds its last pose.
    pub fn clear_target(&mut self) {
        self.target = None;
    }

    /// The followed point, if any.
    #[must_use]
    pub fn target(&self) -> Option<Vec3> {
        self.target
    }

    /// Advances the camera by `dt` seconds under `input`. A POV eye transform or Overhead
    /// step whose result exceeds finite `f32` coordinates holds the previous pose.
    pub fn update(&mut self, input: &SpectatorInput, dt: f32) {
        self.set_speed_level(self.speed_level.saturating_add(input.speed_steps));
        if !(dt.is_finite() && dt > 0.0) {
            return;
        }
        let dt = dt.min(MAX_STEP_SECS);
        let look = Vec2::new(finite_or_zero(input.look.x), finite_or_zero(input.look.y));
        if !matches!(self.mode, SpectatorMode::Pov | SpectatorMode::Overhead) && look != Vec2::ZERO {
            self.yaw = (self.yaw + look.x).rem_euclid(std::f32::consts::TAU);
            self.pitch = (self.pitch + look.y).clamp(-self.config.pitch_limit, self.config.pitch_limit);
            self.rotation = Quat::from_euler(EulerRot::YXZ, self.yaw, self.pitch, self.roll);
        }
        match self.mode {
            SpectatorMode::FreeFly => {
                let mut movement = sanitize(input.movement).clamp(Vec3::splat(-1.0), Vec3::splat(1.0));
                if movement.length_squared() > 1.0 {
                    movement = movement.normalize();
                }
                let rotation = self.rotation();
                let direction = rotation * Vec3::X * movement.x
                    + Vec3::Y * movement.y
                    + rotation * Vec3::NEG_Z * movement.z;
                self.position += direction * self.speed() * dt;
            }
            SpectatorMode::Follow | SpectatorMode::Orbit => {
                self.distance = (self.distance + finite_or_zero(input.zoom))
                    .clamp(self.config.min_follow_distance, self.config.max_follow_distance);
                if let Some(target) = self.target {
                    let smoothing = self.config.follow_smoothing_secs;
                    let alpha = if self.mode == SpectatorMode::Follow && smoothing > 0.0 {
                        1.0 - (-dt / smoothing).exp()
                    } else {
                        1.0
                    };
                    if alpha == 1.0 {
                        self.anchor = target;
                    } else {
                        self.anchor += (target - self.anchor) * alpha;
                    }
                }
                self.position = self.anchor - self.forward() * self.distance;
            }
            SpectatorMode::Pov => {
                if let Some(target) = self.target {
                    let position = target + self.target_rotation * self.pov_offset;
                    if position.is_finite() {
                        self.position = position;
                        self.set_rotation(self.target_rotation);
                    }
                }
            }
            SpectatorMode::Overhead => {
                let height = (self.overhead_height + finite_or_zero(input.zoom))
                    .clamp(self.overhead_config.min_height, self.overhead_config.max_height);
                let mut movement =
                    Vec2::new(finite_or_zero(input.movement.x), finite_or_zero(input.movement.z))
                        .clamp(Vec2::splat(-1.0), Vec2::ONE);
                if movement.length_squared() > 1.0 {
                    movement = movement.normalize();
                }
                let pan = height * self.overhead_config.pan_speed * dt;
                let focus = self.overhead_focus + Vec3::new(movement.x, 0.0, -movement.y) * pan;
                let position = focus + Vec3::Y * height;
                if position.is_finite() {
                    self.overhead_height = height;
                    self.overhead_focus = focus;
                    self.position = position;
                    self.set_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2));
                }
            }
        }
    }

    fn set_rotation(&mut self, rotation: Quat) {
        self.rotation = rotation;
        (self.yaw, self.pitch, self.roll) = rotation.to_euler(EulerRot::YXZ);
    }
}

/// Marks the camera entity whose [`Transform`] follows the [`SpectatorCamera`] resource.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct SpectatorView;

/// What moves the spectator each frame (module docs, "Driving a render camera").
#[derive(Resource, Debug, Clone, Copy, PartialEq, Default)]
pub struct SpectatorDrive {
    /// Input for the coming frame. Movement and look persist until changed; `speed_steps` and
    /// `zoom` are one-shot and reset to zero after the frame applies them.
    pub input: SpectatorInput,
    /// Entity supplying the target world pose for Follow, Orbit and POV. Overhead reads its
    /// position only on mode entry. `None` keeps the last target pose.
    pub follow: Option<Entity>,
    /// The followed entity that disappeared (set when `follow` is cleared for that reason). If a
    /// demo seek brings it back under a new id ([`crate::demo_world::DemoRemap`]), the follow
    /// resumes on it.
    pub lost: Option<Entity>,
}

/// The [`SpectatorPlugin`] system set (`PostUpdate`, before transform propagation).
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct SpectatorSet;

/// Drives the [`SpectatorCamera`] resource each frame and writes its pose to [`SpectatorView`]
/// cameras.
pub struct SpectatorPlugin;

impl Plugin for SpectatorPlugin {
    fn build(&self, app: &mut App) {
        if app.world().get_resource::<SpectatorCamera>().is_none() {
            app.insert_resource(SpectatorCamera::new(SpectatorConfig::default(), Vec3::ZERO));
        }
        app.register_type::<SpectatorView>()
            .init_resource::<SpectatorDrive>()
            .add_systems(
                PostUpdate,
                drive_spectator
                    .in_set(SpectatorSet)
                    .before(PostUpdateSet::TransformPropagate),
            );
    }
}

/// Ancestors composed for a same-frame world pose. A longer chain falls back to the last
/// propagated [`GlobalTransform`].
const LIVE_POSE_DEPTH: usize = 64;

/// A followed entity as [`drive_spectator`] reads it: its propagated and local poses, and whether
/// it has a parent. Used when the live local chain cannot be composed.
type FollowTarget = (
    Option<&'static GlobalTransform>,
    Option<&'static Transform>,
    Has<ChildOf>,
);

/// Current local `Transform` of an entity that is not itself a spectator view, plus its parent
/// link. The view query mutably borrows `Transform`, so this one excludes those entities.
type LivePose = (&'static Transform, Option<&'static ChildOf>);

/// World pose from the current local chain, root first. `None` when an ancestor has no
/// `Transform`, the `ChildOf` links cycle, or the chain is longer than `LIVE_POSE_DEPTH`.
fn live_world_pose(start: Entity, locals: &Query<LivePose, Without<SpectatorView>>) -> Option<Transform> {
    let mut chain = [Entity::PLACEHOLDER; LIVE_POSE_DEPTH];
    let mut depth = 0usize;
    let mut current = start;
    loop {
        if depth >= LIVE_POSE_DEPTH || chain[..depth].contains(&current) {
            return None;
        }
        let (_, parent) = locals.get(current).ok()?;
        chain[depth] = current;
        depth += 1;
        match parent {
            Some(link) => current = link.parent(),
            None => break,
        }
    }
    let mut world = Transform::IDENTITY;
    for entity in chain[..depth].iter().rev() {
        let (local, _) = locals.get(*entity).ok()?;
        // Parent scale moves the child; the spectator eye offset stays unscaled later.
        world.translation = world.transform_point(local.translation);
        world.rotation *= local.rotation;
        world.scale *= local.scale;
    }
    Some(world)
}

/// `PostUpdate`: follow target, integrate, write the pose to the views.
#[tracing::instrument(skip_all, target = "regia_framework", name = "spectator_drive")]
fn drive_spectator(
    real: Option<Res<RealTime>>,
    camera: Option<ResMut<SpectatorCamera>>,
    mut drive: ResMut<SpectatorDrive>,
    remap: Option<Res<crate::demo_world::DemoRemap>>,
    targets: Query<FollowTarget, Without<SpectatorView>>,
    locals: Query<LivePose, Without<SpectatorView>>,
    mut views: Query<&mut Transform, With<SpectatorView>>,
) {
    let Some(mut camera) = camera else {
        return;
    };
    // A demo seek respawned entities under new ids: the followed (or lost) one follows them.
    if let Some(remap) = remap.filter(|remap| remap.is_changed())
        && let Some(moved) = drive.follow.or(drive.lost).and_then(|entity| remap.get(entity))
    {
        drive.follow = Some(moved);
        drive.lost = None;
    }
    if let Some(entity) = drive.follow {
        // Compose the current local chain so a parent that moved this frame is already in the
        // pose. This system still runs before propagation, and the camera `Transform` written
        // below is what that propagation publishes for rendering.
        if let Some(pose) = live_world_pose(entity, &locals) {
            camera.set_target_pose(pose.translation, pose.rotation);
        } else {
            match targets.get(entity) {
                // Last propagated pose when the live chain cannot be composed. A root that was
                // never propagated keeps an identity global; its local pose is the real one. A
                // child's identity global is the world origin, not "unset".
                Ok((Some(global), local, child))
                    if *global != GlobalTransform::IDENTITY || local.is_none() || child =>
                {
                    let pose = global.to_transform();
                    camera.set_target_pose(pose.translation, pose.rotation);
                }
                Ok((_, Some(local), _)) => {
                    camera.set_target_pose(local.translation, local.rotation);
                }
                // The followed entity is gone (or has no position): stop following it, keep the
                // last target, remember it for a seek that brings it back.
                _ => {
                    drive.lost = drive.follow.take();
                }
            }
        }
    }
    let dt = real.map_or(0.0, |real| real.delta().as_secs_f32());
    camera.update(&drive.input, dt);
    drive.input.speed_steps = 0;
    drive.input.zoom = 0.0;
    let (position, rotation) = camera.pose();
    for mut transform in &mut views {
        transform.translation = position;
        transform.rotation = rotation;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn orbit_mode_keeps_target_centered() {
        let mut cam = camera();
        cam.mode = SpectatorMode::parse("orbit").expect("orbit mode must exist");
        for frame in 0..120 {
            let target = Vec3::new(frame as f32 * 0.125, 3.0, -7.0);
            cam.set_target(target);
            cam.update(
                &SpectatorInput {
                    look: Vec2::new(0.07, -0.003),
                    zoom: 0.02,
                    ..SpectatorInput::default()
                },
                1.0 / 60.0,
            );
            assert_near(cam.anchor(), target, 1e-5);
            assert_near(
                cam.forward() * cam.follow_distance(),
                target - cam.position(),
                1e-4,
            );
        }
    }

    fn camera() -> SpectatorCamera {
        SpectatorCamera::new(SpectatorConfig::default(), Vec3::ZERO)
    }

    fn assert_near(a: Vec3, b: Vec3, eps: f32) {
        assert!((a - b).length() <= eps, "{a:?} vs {b:?}");
    }

    fn fly(movement: Vec3) -> SpectatorInput {
        SpectatorInput {
            movement,
            ..SpectatorInput::default()
        }
    }

    #[test]
    fn free_fly_moves_forward_at_the_base_speed() {
        let mut cam = camera();
        cam.update(&fly(Vec3::Z), 0.25);
        assert_near(cam.position(), Vec3::new(0.0, 0.0, -2.0), 1e-5);
    }

    #[test]
    fn movement_follows_the_view_direction() {
        let mut cam = camera();
        // Turn left a quarter turn: forward is now −X, right is −Z.
        cam.update(
            &SpectatorInput {
                look: Vec2::new(std::f32::consts::FRAC_PI_2, 0.0),
                ..SpectatorInput::default()
            },
            0.01,
        );
        assert_near(cam.forward(), Vec3::NEG_X, 1e-5);
        let before = cam.position();
        cam.update(&fly(Vec3::Z), 0.25);
        assert_near(cam.position() - before, Vec3::new(-2.0, 0.0, 0.0), 1e-4);
        let before = cam.position();
        cam.update(&fly(Vec3::X), 0.25);
        assert_near(cam.position() - before, Vec3::new(0.0, 0.0, -2.0), 1e-4);
    }

    #[test]
    fn vertical_movement_is_along_world_up_even_when_looking_down() {
        let mut cam = camera();
        cam.set_pose(Vec3::ZERO, 0.0, -1.0);
        cam.update(&fly(Vec3::Y), 0.25);
        assert_near(cam.position(), Vec3::new(0.0, 2.0, 0.0), 1e-5);
    }

    #[test]
    fn diagonals_are_not_faster() {
        let mut cam = camera();
        cam.update(&fly(Vec3::new(1.0, 0.0, 1.0)), 0.25);
        assert!(
            (cam.position().length() - 2.0).abs() < 1e-4,
            "{:?}",
            cam.position()
        );
        // Over-range components are clamped before that.
        let mut cam = camera();
        cam.update(&fly(Vec3::new(0.0, 0.0, 50.0)), 0.25);
        assert!((cam.position().length() - 2.0).abs() < 1e-4);
    }

    #[test]
    fn pitch_is_clamped_and_yaw_wraps() {
        let mut cam = camera();
        cam.update(
            &SpectatorInput {
                look: Vec2::new(7.0, 5.0),
                ..SpectatorInput::default()
            },
            0.01,
        );
        assert!((cam.pitch() - 1.55).abs() < 1e-6);
        assert!((0.0..std::f32::consts::TAU).contains(&cam.yaw()), "{}", cam.yaw());
        cam.update(
            &SpectatorInput {
                look: Vec2::new(0.0, -50.0),
                ..SpectatorInput::default()
            },
            0.01,
        );
        assert!((cam.pitch() + 1.55).abs() < 1e-6);
        assert!(cam.forward().is_finite());
    }

    #[test]
    fn speed_levels_scale_geometrically_and_clamp() {
        let mut cam = camera();
        assert!((cam.speed() - 8.0).abs() < 1e-6);
        cam.update(
            &SpectatorInput {
                speed_steps: 3,
                ..SpectatorInput::default()
            },
            0.0,
        );
        assert_eq!(
            cam.speed_level(),
            3,
            "speed changes apply even on a zero-length frame"
        );
        assert!((cam.speed() - 8.0 * 1.25f32.powi(3)).abs() < 1e-4);
        cam.update(
            &SpectatorInput {
                speed_steps: -3,
                ..SpectatorInput::default()
            },
            0.0,
        );
        assert!(
            (cam.speed() - 8.0).abs() < 1e-6,
            "up then down returns to the base speed"
        );
        cam.set_speed_level(1000);
        assert_eq!(cam.speed_level(), MAX_SPEED_LEVEL);
        cam.set_speed_level(-1000);
        assert_eq!(cam.speed_level(), -MAX_SPEED_LEVEL);
        cam.update(
            &SpectatorInput {
                speed_steps: i32::MAX,
                ..SpectatorInput::default()
            },
            0.0,
        );
        assert_eq!(cam.speed_level(), MAX_SPEED_LEVEL, "no overflow");
    }

    #[test]
    fn a_faster_level_covers_more_ground() {
        let mut slow = camera();
        let mut fast = camera();
        fast.set_speed_level(4);
        slow.update(&fly(Vec3::Z), 1.0);
        fast.update(&fly(Vec3::Z), 1.0);
        assert!(fast.position().length() > slow.position().length() * 2.0);
    }

    #[test]
    fn bad_input_is_ignored() {
        let mut cam = camera();
        let before = cam.clone();
        cam.update(&fly(Vec3::Z), 0.0);
        cam.update(&fly(Vec3::Z), -1.0);
        cam.update(&fly(Vec3::Z), f32::NAN);
        cam.update(&fly(Vec3::Z), f32::INFINITY);
        assert_eq!(cam, before, "unusable frame times do nothing");
        cam.update(
            &SpectatorInput {
                movement: Vec3::new(f32::NAN, f32::INFINITY, 0.0),
                look: Vec2::new(f32::NAN, f32::NEG_INFINITY),
                zoom: f32::NAN,
                ..SpectatorInput::default()
            },
            0.1,
        );
        assert_eq!(cam.position(), Vec3::ZERO);
        assert_eq!((cam.yaw(), cam.pitch()), (0.0, 0.0));
        cam.set_pose(Vec3::new(f32::NAN, 1.0, 2.0), f32::NAN, f32::INFINITY);
        assert_eq!(cam.position(), Vec3::new(0.0, 1.0, 2.0));
        assert_eq!(cam.pitch(), 0.0);
    }

    #[test]
    fn a_long_frame_is_clamped() {
        let mut cam = camera();
        cam.update(&fly(Vec3::Z), 100.0);
        assert!((cam.position().length() - 8.0 * MAX_STEP_SECS).abs() < 1e-4);
    }

    #[test]
    fn follow_starts_where_the_view_already_is() {
        let mut cam = camera();
        cam.set_pose(Vec3::new(0.0, 2.0, 10.0), 0.0, 0.0);
        cam.follow();
        assert_eq!(cam.mode(), SpectatorMode::Follow);
        // With no target the anchor holds and the camera does not move.
        cam.update(&SpectatorInput::default(), 0.1);
        assert_near(cam.position(), Vec3::new(0.0, 2.0, 10.0), 1e-4);
        assert_near(cam.anchor(), Vec3::new(0.0, 2.0, 4.0), 1e-4);
        // Following again does not reset the anchor.
        cam.follow();
        assert_near(cam.anchor(), Vec3::new(0.0, 2.0, 4.0), 1e-4);
    }

    #[test]
    fn follow_converges_on_the_target_and_keeps_its_distance() {
        let mut cam = camera();
        cam.follow();
        cam.set_target(Vec3::new(20.0, 0.0, -30.0));
        for _ in 0..600 {
            cam.update(&SpectatorInput::default(), 1.0 / 60.0);
        }
        assert_near(cam.anchor(), Vec3::new(20.0, 0.0, -30.0), 1e-3);
        assert!(((cam.position() - cam.anchor()).length() - 6.0).abs() < 1e-4);
        assert_near(cam.forward() * 6.0, cam.anchor() - cam.position(), 1e-4);
    }

    #[test]
    fn follow_smoothing_is_monotonic_and_frame_rate_independent() {
        let run = |steps: u32, dt: f32| {
            let mut cam = camera();
            cam.follow();
            cam.set_target(Vec3::new(10.0, 0.0, 0.0));
            let mut last = cam.anchor().distance(Vec3::new(10.0, 0.0, 0.0));
            for _ in 0..steps {
                cam.update(&SpectatorInput::default(), dt);
                let now = cam.anchor().distance(Vec3::new(10.0, 0.0, 0.0));
                assert!(now < last, "the gap shrinks every frame");
                last = now;
            }
            cam.anchor()
        };
        // 0.2 s of motion as 2 frames of 100 ms or 20 frames of 10 ms: the same anchor.
        assert_near(run(2, 0.1), run(20, 0.01), 1e-4);
        // After one time constant the gap has closed by 1 − 1/e.
        let anchor = run(1, 0.2);
        let initial_gap = Vec3::new(0.0, 0.0, -6.0).distance(Vec3::new(10.0, 0.0, 0.0));
        let closed = 1.0 - anchor.distance(Vec3::new(10.0, 0.0, 0.0)) / initial_gap;
        assert!((closed - (1.0 - (-1.0f32).exp())).abs() < 1e-4, "{closed}");
    }

    #[test]
    fn an_unusable_config_falls_back_per_field_instead_of_panicking() {
        // `pitch_limit: NaN` panicked in `f32::clamp` on the first update, swapped follow
        // distances already in `new` (ADR-1660).
        let d = SpectatorConfig::default();
        let bad = SpectatorConfig {
            base_speed: -1.0,
            speed_factor: 0.5,
            pitch_limit: f32::NAN,
            follow_smoothing_secs: -0.1,
            follow_distance: f32::INFINITY,
            min_follow_distance: 50.0,
            max_follow_distance: 5.0,
        };
        assert_eq!(bad.sanitized(), d);
        let mut cam = SpectatorCamera::new(bad, Vec3::ZERO);
        assert_eq!(cam.config(), &d);
        cam.set_pose(Vec3::ZERO, 0.0, 3.0);
        assert!((cam.pitch() - d.pitch_limit).abs() < 1e-6);
        cam.follow();
        cam.update(
            &SpectatorInput {
                look: Vec2::new(0.1, 0.1),
                zoom: 1.0,
                movement: Vec3::X,
                speed_steps: 1,
            },
            0.016,
        );
        assert!(cam.position().is_finite() && cam.speed().is_finite());
        // A usable config is kept as given.
        let good = SpectatorConfig {
            base_speed: 2.0,
            pitch_limit: 1.0,
            min_follow_distance: 2.0,
            max_follow_distance: 3.0,
            ..d
        };
        assert_eq!(good.sanitized(), good);
    }

    #[test]
    fn zero_smoothing_follows_rigidly() {
        let config = SpectatorConfig {
            follow_smoothing_secs: 0.0,
            ..SpectatorConfig::default()
        };
        let mut cam = SpectatorCamera::new(config, Vec3::ZERO);
        cam.follow();
        cam.set_target(Vec3::new(5.0, 1.0, 2.0));
        cam.update(&SpectatorInput::default(), 0.016);
        assert_near(cam.anchor(), Vec3::new(5.0, 1.0, 2.0), 1e-6);
    }

    #[test]
    fn look_orbits_the_anchor_and_zoom_is_limited() {
        let mut cam = camera();
        cam.follow();
        let anchor = cam.anchor();
        cam.update(
            &SpectatorInput {
                look: Vec2::new(1.2, 0.3),
                ..SpectatorInput::default()
            },
            0.016,
        );
        assert!(
            (cam.position().distance(cam.anchor()) - 6.0).abs() < 1e-4,
            "orbit radius is constant"
        );
        assert_near(cam.anchor(), anchor, 1e-5);
        cam.update(
            &SpectatorInput {
                zoom: 1000.0,
                ..SpectatorInput::default()
            },
            0.016,
        );
        assert_eq!(cam.follow_distance(), 60.0);
        cam.update(
            &SpectatorInput {
                zoom: -1000.0,
                ..SpectatorInput::default()
            },
            0.016,
        );
        assert_eq!(cam.follow_distance(), 1.0);
    }

    #[test]
    fn a_lost_target_holds_the_anchor_and_a_bad_one_clears_it() {
        let mut cam = camera();
        cam.follow();
        cam.set_target(Vec3::new(4.0, 0.0, 0.0));
        cam.update(&SpectatorInput::default(), 0.2);
        let held = cam.anchor();
        cam.clear_target();
        cam.update(&SpectatorInput::default(), 0.2);
        assert_eq!(cam.anchor(), held);
        cam.set_target(Vec3::X);
        assert_eq!(cam.target(), Some(Vec3::X));
        cam.set_target(Vec3::new(f32::NAN, 0.0, 0.0));
        assert_eq!(cam.target(), None);
    }

    #[test]
    fn leaving_follow_keeps_the_pose() {
        let mut cam = camera();
        cam.follow();
        cam.set_target(Vec3::new(3.0, 0.0, 0.0));
        cam.update(&SpectatorInput::default(), 0.5);
        let pose = cam.pose();
        cam.free_fly();
        assert_eq!(cam.mode(), SpectatorMode::FreeFly);
        assert_eq!(cam.pose(), pose);
    }

    #[test]
    fn identical_inputs_give_identical_poses() {
        let run = || {
            let mut cam = camera();
            for k in 0..200u32 {
                let t = k as f32 * 0.1;
                if k == 100 {
                    cam.follow();
                    cam.set_target(Vec3::new(5.0, 1.0, -5.0));
                }
                cam.update(
                    &SpectatorInput {
                        movement: Vec3::new(t.sin(), 0.2, t.cos()),
                        look: Vec2::new(0.01, 0.002 * t.sin()),
                        speed_steps: i32::from(k % 50 == 0),
                        zoom: 0.01,
                    },
                    1.0 / 60.0,
                );
            }
            cam
        };
        assert_eq!(run(), run());
    }

    #[test]
    fn mode_names_round_trip() {
        for mode in [
            SpectatorMode::FreeFly,
            SpectatorMode::Follow,
            SpectatorMode::Orbit,
            SpectatorMode::Pov,
            SpectatorMode::Overhead,
        ] {
            assert_eq!(SpectatorMode::parse(mode.name()), Some(mode));
        }
        assert_eq!(SpectatorMode::parse("unknown_mode"), None);
    }

    #[test]
    fn modes_cycle_through_all_five_and_back() {
        let mut mode = SpectatorMode::Follow;
        for expected in [
            SpectatorMode::Orbit,
            SpectatorMode::Pov,
            SpectatorMode::FreeFly,
            SpectatorMode::Overhead,
            SpectatorMode::Follow,
        ] {
            mode = mode.next();
            assert_eq!(mode, expected);
        }
    }

    #[test]
    fn pov_uses_target_orientation_and_local_eye_offset_without_input_drift() {
        let mut cam = camera();
        cam.set_mode(SpectatorMode::Pov);
        let offset = Vec3::new(0.3, 1.7, -0.2);
        cam.set_pov_offset(offset);
        for frame in 0..60 {
            let t = frame as f32 * 0.03;
            let position = Vec3::new(t, 2.0, -t);
            let rotation = Quat::from_euler(EulerRot::YXZ, t, 0.4, -0.3);
            cam.set_target_pose(position, rotation);
            cam.update(
                &SpectatorInput {
                    movement: Vec3::ONE,
                    look: Vec2::ONE,
                    zoom: 100.0,
                    ..SpectatorInput::default()
                },
                1.0 / 60.0,
            );
            assert_near(cam.position(), position + rotation * offset, 1e-5);
            assert_near(cam.forward(), rotation * Vec3::NEG_Z, 1e-5);
            assert_near(cam.rotation() * Vec3::Y, rotation * Vec3::Y, 1e-5);
        }
        let pose = cam.pose();
        cam.clear_target();
        cam.update(&fly(Vec3::ONE), 0.1);
        assert_eq!(cam.pose(), pose, "lost POV holds its last pose");
        cam.free_fly();
        cam.update(&SpectatorInput::default(), 0.1);
        assert_eq!(
            cam.pose(),
            pose,
            "leaving POV preserves roll without a look delta"
        );
    }

    #[test]
    fn overhead_pans_zooms_clamps_and_keeps_an_independent_focus() {
        let mut cam = camera();
        cam.set_overhead_config(SpectatorOverheadConfig {
            height: 24.0,
            min_height: 4.0,
            max_height: 48.0,
            pan_speed: 0.5,
        });
        let target = Vec3::new(11.0, 2.0, -13.0);
        cam.set_target(target);
        cam.set_mode(SpectatorMode::Overhead);
        cam.update(&fly(Vec3::Z), 0.25);
        assert_near(cam.overhead_focus(), target + Vec3::NEG_Z * 3.0, 1e-5);
        assert_near(cam.position(), cam.overhead_focus() + Vec3::Y * 24.0, 1e-5);
        assert_near(cam.forward(), Vec3::NEG_Y, 1e-5);
        assert_near(cam.rotation() * Vec3::Y, Vec3::NEG_Z, 1e-5);
        let focus = cam.overhead_focus();
        cam.set_target(Vec3::splat(99.0));
        cam.set_mode(SpectatorMode::Overhead);
        cam.update(
            &SpectatorInput {
                look: Vec2::ONE,
                zoom: -1000.0,
                ..SpectatorInput::default()
            },
            0.1,
        );
        assert_eq!(
            cam.overhead_focus(),
            focus,
            "target motion/reselection does not undo panning"
        );
        assert_eq!(cam.overhead_height(), 4.0);
        cam.update(
            &SpectatorInput {
                zoom: 1000.0,
                ..SpectatorInput::default()
            },
            0.1,
        );
        assert_eq!(cam.overhead_height(), 48.0);
        cam.free_fly();
        cam.set_mode(SpectatorMode::Overhead);
        assert_eq!(cam.overhead_focus(), Vec3::splat(99.0), "reentering recenters");
    }

    #[test]
    fn overhead_pan_is_rate_independent_and_diagonals_are_normalized() {
        let run = |steps, dt, movement| {
            let mut cam = camera();
            cam.set_mode(SpectatorMode::Overhead);
            for _ in 0..steps {
                cam.update(&fly(movement), dt);
            }
            cam.overhead_focus()
        };
        let straight = run(30, 1.0 / 30.0, Vec3::Z);
        assert_near(straight, run(120, 1.0 / 120.0, Vec3::Z), 1e-4);
        assert!((straight.length() - run(60, 1.0 / 60.0, Vec3::new(1.0, 0.0, 1.0)).length()).abs() < 1e-4);
    }

    #[test]
    fn new_modes_handle_missing_targets_invalid_poses_and_invalid_settings() {
        for mode in [SpectatorMode::Orbit, SpectatorMode::Pov, SpectatorMode::Overhead] {
            let mut cam = camera();
            cam.set_mode(mode);
            cam.set_overhead_config(SpectatorOverheadConfig {
                height: f32::NAN,
                min_height: -1.0,
                max_height: f32::INFINITY,
                pan_speed: f32::NAN,
            });
            assert_eq!(*cam.overhead_config(), SpectatorOverheadConfig::default());
            for rotation in [
                Quat::from_xyzw(0.0, 0.0, 0.0, 0.0),
                Quat::from_xyzw(f32::NAN, 0.0, 0.0, 1.0),
            ] {
                cam.set_target_pose(Vec3::ZERO, rotation);
                assert_eq!(cam.target(), None);
            }
            cam.set_target_pose(Vec3::ONE, Quat::from_xyzw(0.0, 0.0, 0.0, f32::MAX));
            assert_eq!(cam.target_rotation(), Quat::IDENTITY);
            cam.set_target_pose(Vec3::ONE, Quat::from_xyzw(0.0, 0.0, 0.0, f32::from_bits(1)));
            assert_eq!(cam.target_rotation(), Quat::IDENTITY);
            cam.clear_target();
            cam.set_pov_offset(Vec3::splat(f32::NAN));
            assert_eq!(cam.pov_offset(), Vec3::ZERO);
            let before = cam.clone();
            cam.update(&fly(Vec3::ONE), f32::NAN);
            assert_eq!(cam, before);
            cam.update(
                &SpectatorInput {
                    movement: Vec3::splat(f32::INFINITY),
                    look: Vec2::splat(f32::NAN),
                    zoom: f32::NAN,
                    ..SpectatorInput::default()
                },
                0.1,
            );
            assert!(cam.position().is_finite() && cam.rotation().is_finite());
        }
    }

    #[test]
    fn pov_plugin_reads_the_world_pose_of_a_parented_target() {
        let mut app = spectator_app();
        // Propagation gives the child a real world pose (the bare test app never propagates).
        app.add_plugins(regia_scene::ScenePlugin);
        let parent_pose = Transform::from_rotation(Quat::from_rotation_z(0.5));
        let parent = app.world_mut().spawn(parent_pose).id();
        let local = Transform::from_translation(Vec3::new(3.0, 1.0, 2.0));
        let target = app.world_mut().spawn((local, ChildOf(parent))).id();
        let view = app.world_mut().spawn((SpectatorView, Transform::IDENTITY)).id();
        app.world_mut().resource_mut::<SpectatorDrive>().follow = Some(target);
        {
            let mut cam = app.world_mut().resource_mut::<SpectatorCamera>();
            cam.set_mode(SpectatorMode::Pov);
            cam.set_pov_offset(Vec3::Y);
        }
        for _ in 0..3 {
            app.update();
        }
        let expected = parent_pose.transform_point(local.translation + Vec3::Y);
        let actual = app.world().get::<Transform>(view).expect("view");
        assert_near(actual.translation, expected, 1e-5);
        assert_near(actual.rotation * Vec3::Y, parent_pose.rotation * Vec3::Y, 1e-5);
        app.world_mut().despawn(target);
        let pose = app.world().resource::<SpectatorCamera>().pose();
        app.update();
        assert_eq!(app.world().resource::<SpectatorCamera>().pose(), pose);
        assert_eq!(app.world().resource::<SpectatorDrive>().lost, Some(target));
    }

    /// A parent that moves after the previous propagation must be the pose Orbit centers and POV
    /// aims, and the view's propagated `GlobalTransform` must already match that pose when the
    /// frame ends (that is the transform rendering reads).
    #[test]
    fn moving_parent_is_centered_and_aimed_in_the_same_frame() {
        let mut app = spectator_app();
        app.add_plugins(regia_scene::ScenePlugin);
        let parent = app.world_mut().spawn(Transform::IDENTITY).id();
        let child_local = Transform {
            translation: Vec3::new(0.0, 1.5, 0.25),
            rotation: Quat::from_rotation_z(0.35),
            ..Transform::IDENTITY
        };
        let child = app.world_mut().spawn((child_local, ChildOf(parent))).id();
        let view = app.world_mut().spawn((SpectatorView, Transform::IDENTITY)).id();
        app.world_mut().resource_mut::<SpectatorDrive>().follow = Some(child);
        app.world_mut()
            .resource_mut::<SpectatorCamera>()
            .set_mode(SpectatorMode::Orbit);

        let distance = SpectatorConfig::default().follow_distance;
        for step in 1..=4 {
            let parent_pose = Transform {
                translation: Vec3::new(step as f32 * 3.0, 0.4, -(step as f32)),
                rotation: Quat::from_rotation_y(step as f32 * 0.17),
                ..Transform::IDENTITY
            };
            *app.world_mut().get_mut::<Transform>(parent).expect("parent") = parent_pose;
            app.update();
            let world_pos = parent_pose.transform_point(child_local.transform_point(Vec3::ZERO));
            let expected = world_pos + Vec3::Z * distance;
            let rendered = app
                .world()
                .get::<GlobalTransform>(view)
                .expect("view global")
                .to_transform();
            assert_near(rendered.translation, expected, 1e-4);
            assert_near(rendered.rotation * Vec3::NEG_Z, Vec3::NEG_Z, 1e-4);
            assert_near(
                app.world().resource::<SpectatorCamera>().anchor(),
                world_pos,
                1e-4,
            );
        }

        let offset = Vec3::new(0.2, 1.0, -0.1);
        {
            let mut cam = app.world_mut().resource_mut::<SpectatorCamera>();
            cam.set_mode(SpectatorMode::Pov);
            cam.set_pov_offset(offset);
        }
        for step in 5..=8 {
            let parent_pose = Transform {
                translation: Vec3::new(-(step as f32), step as f32 * 0.2, step as f32 * 0.5),
                rotation: Quat::from_rotation_x(0.2) * Quat::from_rotation_z(step as f32 * 0.11),
                ..Transform::IDENTITY
            };
            *app.world_mut().get_mut::<Transform>(parent).expect("parent") = parent_pose;
            app.update();
            let world_rot = parent_pose.rotation * child_local.rotation;
            let world_pos = parent_pose.transform_point(child_local.transform_point(Vec3::ZERO));
            let expected = world_pos + world_rot * offset;
            let rendered = app
                .world()
                .get::<GlobalTransform>(view)
                .expect("view global")
                .to_transform();
            assert_near(rendered.translation, expected, 1e-4);
            assert_near(rendered.rotation * Vec3::Y, world_rot * Vec3::Y, 1e-4);
            assert_near(rendered.rotation * Vec3::X, world_rot * Vec3::X, 1e-4);
        }
    }

    #[test]
    fn unrepresentable_eye_offsets_and_overhead_steps_hold_the_last_pose() {
        let mut cam = camera();
        cam.set_target_pose(Vec3::splat(f32::MAX), Quat::IDENTITY);
        cam.set_pov_offset(Vec3::splat(f32::MAX));
        cam.set_mode(SpectatorMode::Pov);
        let pose = cam.pose();
        cam.update(&SpectatorInput::default(), 0.1);
        assert_eq!(cam.pose(), pose);
        cam.set_overhead_config(SpectatorOverheadConfig {
            pan_speed: f32::MAX,
            ..SpectatorOverheadConfig::default()
        });
        cam.set_mode(SpectatorMode::Overhead);
        let focus = cam.overhead_focus();
        cam.update(&fly(Vec3::X), 0.25);
        assert_eq!(cam.pose(), pose);
        assert_eq!(cam.overhead_focus(), focus);
    }

    fn spectator_app() -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins).add_plugins(SpectatorPlugin);
        enable_fixed_step(&mut app, 50.0);
        app
    }

    #[test]
    fn the_plugin_writes_the_pose_to_spectator_view_cameras() {
        let mut app = spectator_app();
        let view = app
            .world_mut()
            .spawn((
                SpectatorView,
                Transform {
                    scale: Vec3::splat(2.0),
                    ..Transform::IDENTITY
                },
            ))
            .id();
        let other = app.world_mut().spawn(Transform::IDENTITY).id();
        app.world_mut().resource_mut::<SpectatorDrive>().input.movement = Vec3::Z;
        for _ in 0..5 {
            app.update();
        }
        let (position, rotation) = app.world().resource::<SpectatorCamera>().pose();
        assert!(position.z < -0.5, "flew forward along -Z: {position:?}");
        let transform = app.world().get::<Transform>(view).copied().expect("view");
        assert_eq!(transform.translation, position);
        assert_eq!(transform.rotation, rotation);
        assert_eq!(transform.scale, Vec3::splat(2.0), "scale is left alone");
        assert_eq!(
            app.world().get::<Transform>(other).copied(),
            Some(Transform::IDENTITY),
            "only SpectatorView entities move"
        );
    }

    #[test]
    fn one_shot_input_applies_once_and_movement_persists() {
        let mut app = spectator_app();
        {
            let mut drive = app.world_mut().resource_mut::<SpectatorDrive>();
            drive.input.speed_steps = 2;
            drive.input.zoom = 1.0;
        }
        app.update();
        app.update();
        assert_eq!(app.world().resource::<SpectatorCamera>().speed_level(), 2);
        let drive = *app.world().resource::<SpectatorDrive>();
        assert_eq!((drive.input.speed_steps, drive.input.zoom), (0, 0.0));
    }

    #[test]
    fn follow_chases_the_followed_entity_and_forgets_a_despawned_one() {
        let mut app = spectator_app();
        let target = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(10.0, 0.0, 0.0)))
            .id();
        app.world_mut().resource_mut::<SpectatorCamera>().follow();
        app.world_mut().resource_mut::<SpectatorDrive>().follow = Some(target);
        for _ in 0..200 {
            app.update();
        }
        let anchor = app.world().resource::<SpectatorCamera>().anchor();
        assert!((anchor - Vec3::new(10.0, 0.0, 0.0)).length() < 0.05, "{anchor:?}");
        app.world_mut().despawn(target);
        app.update();
        assert_eq!(app.world().resource::<SpectatorDrive>().follow, None);
        assert_eq!(app.world().resource::<SpectatorDrive>().lost, Some(target));
        assert_eq!(
            app.world().resource::<SpectatorCamera>().target(),
            Some(Vec3::new(10.0, 0.0, 0.0)),
            "the last target is kept"
        );
    }

    /// A child whose propagated pose is the world origin (identity global) is followed at the
    /// origin, not at its local offset from the parent (audit framework5 #8).
    #[test]
    fn follow_uses_the_propagated_pose_of_a_child_at_the_world_origin() {
        let mut app = spectator_app();
        let parent = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)))
            .id();
        let child = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(-5.0, 0.0, 0.0)),
                GlobalTransform::IDENTITY,
                ChildOf(parent),
            ))
            .id();
        app.world_mut().resource_mut::<SpectatorCamera>().follow();
        app.world_mut().resource_mut::<SpectatorDrive>().follow = Some(child);
        for _ in 0..3 {
            app.update();
            let global = app.world().get::<GlobalTransform>(child).copied();
            assert_eq!(global, Some(GlobalTransform::IDENTITY), "parent + child = origin");
            assert_eq!(
                app.world().resource::<SpectatorCamera>().target(),
                Some(Vec3::ZERO),
                "the child's world position"
            );
        }
    }

    #[test]
    fn a_seek_that_brings_the_followed_entity_back_resumes_the_follow() {
        let mut app = spectator_app();
        let target = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(4.0, 0.0, 0.0)))
            .id();
        app.world_mut().resource_mut::<SpectatorCamera>().follow();
        app.world_mut().resource_mut::<SpectatorDrive>().follow = Some(target);
        app.update();
        app.world_mut().despawn(target);
        app.update();
        assert_eq!(app.world().resource::<SpectatorDrive>().lost, Some(target));
        // A restore spawned it again elsewhere and published the move.
        let back = app
            .world_mut()
            .spawn(Transform::from_translation(Vec3::new(-3.0, 0.0, 0.0)))
            .id();
        let mut moved = regia_ecs::ecs::entity::EntityHashMap::default();
        moved.insert(target, back);
        app.world_mut()
            .insert_resource(crate::demo_world::DemoRemap { moved });
        app.update();
        let drive = *app.world().resource::<SpectatorDrive>();
        assert_eq!((drive.follow, drive.lost), (Some(back), None));
        assert_eq!(
            app.world().resource::<SpectatorCamera>().target(),
            Some(Vec3::new(-3.0, 0.0, 0.0))
        );
    }
}
