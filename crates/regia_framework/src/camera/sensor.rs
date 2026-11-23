// SPDX-License-Identifier: MPL-2.0
//! Per-camera shake sensors (ADR-1619, `docs/specs/14-scene-and-framework.md` § B5a).
//!
//! Shake sources and the cameras that feel them are separate. A [`ShakeSensor`] on a camera says
//! which channels it listens to (a bitmask), how strongly ([`ShakeSensor::gain`]), how several
//! shakes combine ([`ShakeCombine`]: summed, or only the strongest), whether a shake's offset is in
//! the camera's or the world's axes ([`ShakeAxes`]) and which clock plays it ([`ShakeClock`]).
//!
//! A camera without a sensor does not shake, with two defaults that keep older projects working:
//! a [`CameraRig`] camera, and a camera a cinematic player drives (the [`ShotPlayer`]'s camera, or
//! any [`CinematicCamera`], which a sequence takeover adds; ADR-1614), feel [`ShakeSensor::default`]
//! until they carry a sensor of their own. Plain `Camera` entities (UI, picture-in-picture, a
//! drone feed) never shake unless they are given a sensor. [`ShakeSensor::off`] silences a rig.
//!
//! # Sources
//!
//! - [`ShakeEmit`] messages: an optional origin, a channel mask, a [`ShakePreset`] (a noise
//!   [`Shake`], the blast law of [`ShockShake`] or the ground-shock law of [`GroundShock`], with
//!   their numbers unchanged), a falloff radius and an optional travel speed. Every listening
//!   camera gets its own copy, scaled by its distance and delayed by the travel time. The delay is
//!   counted in fixed steps ([`ShakeField`]) with the law of [`ShockShake::arrival_frame`], so a
//!   replay shakes on the same frames.
//! - The [`CameraDirector`]: its shakes are on [`ShakeSensor::DEFAULT_CHANNELS`] and every
//!   listening camera feels them alike (they have no position).
//! - [`ContactShake`]: a physics body that emits a noise shake when one of its contacts reports a
//!   force (`regia_physics::ContactForce`), scaled by the force and filtered by the other body's
//!   collision layer.
//!
//! # Order in `PostUpdate`
//!
//! [`ShakeRestoreSet`] puts back each camera's unshaken pose, when nothing moved the camera since
//! the shake was written. The rigs ([`CameraSet::Update`]) and the cinematic drivers
//! ([`CameraSet::Cinematic`]: the shot player's [`ShotDriveSet`] and a sequence takeover) then move it, and [`CameraSet::Director`] receives emits, starts the shakes that arrived and adds
//! the result on top. A shake therefore shows under a playing shot, and never accumulates into the
//! pose a rig eases from.
//!
//! # Clocks
//!
//! A running shake advances on its sensor's clock: gameplay time (pause and time scale apply) or
//! the unscaled frame clock (a pause menu keeps shaking). Neither reads the wall clock itself: both
//! are the engine's per-frame deltas, which are manual under `TimeUpdateStrategy::ManualDuration`.
//! Travel delays always count fixed steps, which stop while the game is paused.

use std::collections::HashMap;

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec2, Vec3};
use regia_physics::{Collider, ContactForce};
use regia_scene::{GlobalTransform, Transform};

use super::blend::PlayerCamera;
use super::{
    CameraDirector, CameraRig, CameraRuntime, CameraSet, CinematicCamera, Shake, mount_strength, noise_pose,
    noise_strength,
};
use crate::ground_shock::{GroundShock, MountShock, ShakePose, WaveKind};
use crate::player::PlayerManager;
use crate::shock_shake::{ShockShake, frame_after};
use crate::shots::{ShotDriveSet, ShotPlayer};

/// Amplitude below which a planned noise shake is dropped (metres), as for shock shakes.
const MIN_AMPLITUDE: f32 = 1.0e-4;

/// Most entries the contact cooldown table keeps before it forgets old ones.
const CONTACT_TABLE_LIMIT: usize = 4096;

/// Fixed steps after which a contact cooldown entry is forgotten.
const CONTACT_FORGET_STEPS: u64 = 3600;

// ---------------------------------------------------------------------------
// The sensor
// ---------------------------------------------------------------------------

/// How a sensor combines the shakes it feels at once.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShakeCombine {
    /// The poses add up (the director's behavior before sensors).
    #[default]
    Add,
    /// Only the strongest shake plays: the one with the largest envelope now (metres for a noise
    /// shake's amplitude, radians or metres for a mount oscillation, whichever is larger). Ties
    /// go to the shake that started first, the director's shakes before the camera's own.
    Max,
}

impl ShakeCombine {
    /// The name used by the agent protocol (`add`, `max`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Max => "max",
        }
    }

    /// The rule for one of [`name`](Self::name)'s values (`strongest` is accepted for `max`).
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "add" => Some(Self::Add),
            "max" | "strongest" => Some(Self::Max),
            _ => None,
        }
    }
}

/// Which axes a shake's positional offset is in. Rotations are always about the camera's own
/// right, up and view axes.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShakeAxes {
    /// The offset is in world axes (the director's behavior before sensors): a vertical shake
    /// moves the camera up whichever way it looks.
    #[default]
    World,
    /// The offset is in the camera's axes (x right, y up, z back): a vertical shake moves the
    /// picture up and down on screen.
    Camera,
}

impl ShakeAxes {
    /// The name used by the agent protocol (`world`, `camera`).
    pub fn name(self) -> &'static str {
        match self {
            Self::World => "world",
            Self::Camera => "camera",
        }
    }

    /// The axes for one of [`name`](Self::name)'s values.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "world" => Some(Self::World),
            "camera" | "local" => Some(Self::Camera),
            _ => None,
        }
    }
}

/// Which clock plays a sensor's shakes once they have arrived.
#[derive(Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ShakeClock {
    /// Gameplay time ([`Time::delta`]): pause and time scale apply.
    #[default]
    Gameplay,
    /// The unscaled frame clock ([`RealTime::delta`](regia_app::RealTime::delta)): the shake keeps
    /// playing while the game is paused, for a pause-menu or photo-mode camera.
    Unscaled,
}

impl ShakeClock {
    /// The name used by the agent protocol (`gameplay`, `unscaled`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Gameplay => "gameplay",
            Self::Unscaled => "unscaled",
        }
    }

    /// The clock for one of [`name`](Self::name)'s values.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "gameplay" | "game" => Some(Self::Gameplay),
            "unscaled" | "real" => Some(Self::Unscaled),
            _ => None,
        }
    }
}

/// Makes a camera feel shakes (see the module docs). Without one, only [`CameraRig`] cameras and
/// the shot player's camera shake, as if they had [`ShakeSensor::default`].
///
/// # Example
///
/// ```
/// use regia_framework::camera::sensor::{ShakeAxes, ShakeCombine, ShakeSensor};
///
/// // A drone feed that only feels channel 2 shakes, at half strength, in its own axes.
/// let drone = ShakeSensor {
///     channels: 1 << 2,
///     gain: 0.5,
///     combine: ShakeCombine::Max,
///     axes: ShakeAxes::Camera,
///     ..ShakeSensor::default()
/// };
/// assert!(drone.listens(1 << 2));
/// assert!(!drone.listens(ShakeSensor::DEFAULT_CHANNELS));
/// assert!(!ShakeSensor::off().listens(ShakeSensor::ALL_CHANNELS));
/// ```
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct ShakeSensor {
    /// Channels this camera listens to: it feels an emit when `channels & emit.channels != 0`.
    /// `0` feels nothing.
    pub channels: u32,
    /// Multiplies every offset and angle. Negative, NaN or infinite reads as `0`; values above
    /// [`ShakeSensor::MAX_GAIN`] are clamped.
    pub gain: f32,
    /// How simultaneous shakes combine.
    pub combine: ShakeCombine,
    /// Which axes the positional offset is in.
    pub axes: ShakeAxes,
    /// Which clock plays the shakes.
    pub clock: ShakeClock,
}

impl Default for ShakeSensor {
    fn default() -> Self {
        Self {
            channels: Self::DEFAULT_CHANNELS,
            gain: 1.0,
            combine: ShakeCombine::Add,
            axes: ShakeAxes::World,
            clock: ShakeClock::Gameplay,
        }
    }
}

impl ShakeSensor {
    /// Channel 0, bit `1`: what a default sensor hears, where [`CameraDirector`] shakes play and
    /// what a [`ShakeEmit`] targets unless told otherwise.
    pub const DEFAULT_CHANNELS: u32 = 1;
    /// Every channel.
    pub const ALL_CHANNELS: u32 = u32::MAX;
    /// Largest gain honored.
    pub const MAX_GAIN: f32 = 100.0;

    /// A sensor that feels nothing: put it on a [`CameraRig`] camera that must stay steady.
    pub fn off() -> Self {
        Self {
            channels: 0,
            ..Self::default()
        }
    }

    /// A default sensor listening to `channels`.
    pub fn on_channels(channels: u32) -> Self {
        Self {
            channels,
            ..Self::default()
        }
    }

    /// Whether an emit on `channels` reaches this sensor.
    pub fn listens(&self, channels: u32) -> bool {
        self.channels & channels != 0
    }

    /// The gain actually applied: [`gain`](Self::gain) clamped into `0..=MAX_GAIN`, `0` when not
    /// finite.
    pub fn effective_gain(&self) -> f32 {
        if self.gain.is_finite() {
            self.gain.clamp(0.0, Self::MAX_GAIN)
        } else {
            0.0
        }
    }

    /// [`Self::effective_gain`] multiplied by the player's shake scale.
    ///
    /// `player_scale` is clamped into `0..=1`. A non-finite scale reads as `0`, so a bad setting
    /// cannot amplify the authored gain. [`Self::gain`] is not written (ADR-3464).
    #[must_use]
    pub fn effective_gain_scaled(&self, player_scale: f32) -> f32 {
        let scale = if player_scale.is_finite() {
            player_scale.clamp(0.0, 1.0)
        } else {
            0.0
        };
        self.effective_gain() * scale
    }
}

/// The sensor a camera feels with: its own [`ShakeSensor`], else [`ShakeSensor::default`] for a
/// [`CameraRig`] camera (`rig`) or a camera a cinematic player drives (`shot_driven`: the
/// [`ShotPlayer`]'s camera or a [`CinematicCamera`]), else `None` (it does not shake).
pub fn effective_sensor(explicit: Option<&ShakeSensor>, rig: bool, shot_driven: bool) -> Option<ShakeSensor> {
    match explicit {
        Some(sensor) => Some(*sensor),
        None if rig || shot_driven => Some(ShakeSensor::default()),
        None => None,
    }
}

// ---------------------------------------------------------------------------
// Emits
// ---------------------------------------------------------------------------

/// Names of the noise presets [`named_shake`] knows, in a stable order.
pub const NAMED_SHAKES: &[&str] = &["bump", "impact", "rumble", "explosion"];

/// A named noise shake: `bump` (a light knock), `impact` (a hit), `rumble` (a long low shake),
/// `explosion` ([`Shake::default`]). Unknown names give `None`.
pub fn named_shake(name: &str) -> Option<Shake> {
    let (amplitude, frequency_hz, duration_secs, falloff) = match name {
        "bump" => (0.04, 14.0, 0.25, 2.0),
        "impact" => (0.12, 10.0, 0.45, 2.0),
        "rumble" => (0.03, 5.0, 2.0, 0.5),
        "explosion" => return Some(Shake::default()),
        _ => return None,
    };
    Some(Shake {
        amplitude,
        frequency_hz,
        duration_secs,
        falloff,
    })
}

/// What an emit plays.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShakePreset {
    /// A noise shake, its amplitude scaled by the falloff.
    Noise(Shake),
    /// The blast law: amplitude by distance and arrival at the shock speed, as
    /// [`ShockShake::to_shake`] and [`ShockShake::arrival_frame`] give them. A travel speed on
    /// the emit replaces the law's speed.
    Blast(ShockShake),
    /// The ground-shock law: the seismic wave and the airblast as two mount oscillations, each on
    /// its own arrival (the emit's travel speed is not used: each wave has its own).
    Ground(GroundShock),
}

/// A shake sent into the world: every camera whose sensor listens to one of `channels` feels it
/// (read in `PostUpdate`, [`CameraSet::Director`]).
///
/// # Example
///
/// ```
/// use regia_core::math::Vec3;
/// use regia_framework::camera::Shake;
/// use regia_framework::camera::sensor::{ShakeEmit, ShakePreset};
///
/// // A truck hit 40 m away: full strength at the origin, gone at 80 m, travelling at 343 m/s.
/// let emit = ShakeEmit::at(Vec3::new(0.0, 0.0, -40.0), ShakePreset::Noise(Shake::default()))
///     .with_falloff(80.0)
///     .with_travel(343.0);
/// assert_eq!(emit.falloff_factor(40.0), 0.25);
/// let planned = emit.plan(40.0, 1);
/// assert_eq!(planned.len(), 1);
/// assert!((planned[0].delay_s - 40.0 / 343.0).abs() < 1e-9);
/// ```
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct ShakeEmit {
    /// Where the shake comes from. `None` reaches every listening camera at full strength and at
    /// once (falloff and travel need a distance).
    pub origin: Option<Vec3>,
    /// The channels it is sent on.
    pub channels: u32,
    /// What plays.
    pub preset: ShakePreset,
    /// Distance in metres at which the strength reaches zero, falling as `(1 − d / r)²`.
    /// Non-positive, NaN or infinite means no falloff (the preset's own law still applies).
    pub falloff_radius_m: f32,
    /// Speed in m/s the shake travels from the origin; `None` (or a non-positive value) arrives
    /// at once. See [`ShakePreset`] for the blast and ground laws.
    pub travel_speed_m_s: Option<f32>,
}

impl ShakeEmit {
    /// A shake everywhere at once on the default channel.
    pub fn everywhere(shake: Shake) -> Self {
        Self {
            origin: None,
            channels: ShakeSensor::DEFAULT_CHANNELS,
            preset: ShakePreset::Noise(shake),
            falloff_radius_m: f32::INFINITY,
            travel_speed_m_s: None,
        }
    }

    /// A preset from `origin` on the default channel, with no falloff and no travel beyond the
    /// preset's own law.
    pub fn at(origin: Vec3, preset: ShakePreset) -> Self {
        Self {
            origin: Some(origin),
            channels: ShakeSensor::DEFAULT_CHANNELS,
            preset,
            falloff_radius_m: f32::INFINITY,
            travel_speed_m_s: None,
        }
    }

    /// The same emit on `channels`.
    #[must_use]
    pub fn with_channels(self, channels: u32) -> Self {
        Self { channels, ..self }
    }

    /// The same emit with a falloff radius in metres.
    #[must_use]
    pub fn with_falloff(self, radius_m: f32) -> Self {
        Self {
            falloff_radius_m: radius_m,
            ..self
        }
    }

    /// The same emit travelling at `speed_m_s`.
    #[must_use]
    pub fn with_travel(self, speed_m_s: f32) -> Self {
        Self {
            travel_speed_m_s: Some(speed_m_s),
            ..self
        }
    }

    /// The strength factor at `distance_m` from the origin: `(1 − d / r)²` clamped into `0..=1`,
    /// `1` without a radius or an origin, `0` for a NaN distance.
    pub fn falloff_factor(&self, distance_m: f32) -> f32 {
        let radius = self.falloff_radius_m;
        if self.origin.is_none() || !(radius.is_finite() && radius > 0.0) {
            return 1.0;
        }
        if distance_m.is_nan() {
            return 0.0;
        }
        let x = (distance_m.max(0.0) / radius).min(1.0);
        (1.0 - x) * (1.0 - x)
    }

    /// The travel speed actually used, when one is set and positive.
    fn travel(&self) -> Option<f32> {
        self.travel_speed_m_s.filter(|v| v.is_finite() && *v > 0.0)
    }

    /// What a camera `distance_m` from the origin feels: up to two shakes (the ground preset has a
    /// seismic wave and an airblast), each with its delay in seconds. Empty when it is negligible
    /// there, out of the falloff radius, or the preset is malformed (a non-finite field, a
    /// non-positive duration). `seed` seeds mount oscillations (`seed`, `seed + 1`); the same
    /// emit, distance and seed always plan the same shakes.
    pub fn plan(&self, distance_m: f32, seed: u64) -> Vec<PlannedShake> {
        let mut out = Vec::new();
        if distance_m.is_nan() {
            return out;
        }
        let distance_m = distance_m.max(0.0);
        let factor = self.falloff_factor(distance_m);
        if factor.is_nan() || factor <= 0.0 {
            return out;
        }
        match self.preset {
            ShakePreset::Noise(shake) => {
                let valid = shake.amplitude.is_finite()
                    && shake.frequency_hz.is_finite()
                    && shake.falloff.is_finite()
                    && shake.duration_secs.is_finite()
                    && shake.duration_secs > 0.0;
                let amplitude = shake.amplitude * factor;
                if !valid || amplitude.abs() < MIN_AMPLITUDE {
                    return out;
                }
                let delay_s = self
                    .travel()
                    .map_or(0.0, |speed| f64::from(distance_m) / f64::from(speed));
                out.push(PlannedShake {
                    delay_s,
                    distance_m,
                    kind: ShakeKind::Noise(Shake { amplitude, ..shake }),
                });
            }
            ShakePreset::Blast(mut blast) => {
                if let Some(speed) = self.travel() {
                    blast.speed_m_s = speed;
                }
                if let Some((_, mut shake)) = blast.to_shake(distance_m) {
                    if factor < 1.0 {
                        shake.amplitude *= factor;
                    }
                    if shake.amplitude >= MIN_AMPLITUDE {
                        out.push(PlannedShake {
                            delay_s: blast.delay_f64(distance_m),
                            distance_m,
                            kind: ShakeKind::Noise(shake),
                        });
                    }
                }
            }
            ShakePreset::Ground(ground) => {
                let waves = [
                    (WaveKind::Seismic, ground.seismic_delay_f64(distance_m)),
                    (WaveKind::Airblast, ground.air_delay_f64(distance_m)),
                ];
                for (offset, (wave, delay_s)) in (0u64..).zip(waves) {
                    let Some(mut shock) = ground.mount_shock(wave, distance_m, seed.wrapping_add(offset))
                    else {
                        continue;
                    };
                    if factor < 1.0 {
                        shock = MountShock::new(
                            shock.angle_rad * factor,
                            shock.translation_m * factor,
                            shock.decay_s,
                            shock.seed,
                        );
                    }
                    out.push(PlannedShake {
                        delay_s,
                        distance_m,
                        kind: ShakeKind::Mount(shock),
                    });
                }
            }
        }
        out
    }
}

/// One shake a camera will feel, before it arrives.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlannedShake {
    /// Seconds from the emit to the arrival.
    pub delay_s: f64,
    /// Distance from the origin to the camera (metres; `0` without an origin).
    pub distance_m: f32,
    /// What plays on arrival.
    pub kind: ShakeKind,
}

/// The two shapes a shake can take on a camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ShakeKind {
    /// Smooth value noise with a decaying envelope (the director's [`Shake`]).
    Noise(Shake),
    /// A damped mount oscillation (the ground-shock waves).
    Mount(MountShock),
}

impl ShakeKind {
    /// The kind's name in agent reports: `noise` or `mount`.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Noise(_) => "noise",
            Self::Mount(_) => "mount",
        }
    }

    /// The pose `elapsed` seconds in, or `None` once it has ended.
    fn pose(&self, elapsed: f32, seed: f32) -> Option<ShakePose> {
        match self {
            Self::Noise(shake) => noise_pose(shake, elapsed, seed),
            Self::Mount(shock) => (elapsed < shock.duration_s()).then(|| shock.pose(elapsed)),
        }
    }

    /// The envelope used by [`ShakeCombine::Max`] `elapsed` seconds in.
    fn strength(&self, elapsed: f32) -> f32 {
        match self {
            Self::Noise(shake) => noise_strength(shake, elapsed),
            Self::Mount(shock) => mount_strength(shock, elapsed),
        }
    }

    /// The peak amplitude: metres for noise, the peak pitch in radians for a mount oscillation.
    pub fn peak(&self) -> f32 {
        match self {
            Self::Noise(shake) => shake.amplitude,
            Self::Mount(shock) => shock.angle_rad,
        }
    }

    /// How long it plays once it has arrived (seconds).
    pub fn duration_s(&self) -> f32 {
        match self {
            Self::Noise(shake) => shake.duration_secs,
            Self::Mount(shock) => shock.duration_s(),
        }
    }
}

/// A shake on its way to a camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PendingShake {
    /// The [`ShakeField`] step it was emitted on.
    pub emitted_frame: u64,
    /// The step it starts on.
    pub arrival_frame: u64,
    /// Distance from the origin (metres).
    pub distance_m: f32,
    /// What plays.
    pub kind: ShakeKind,
}

/// A shake playing on a camera.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ActiveShake {
    /// What plays.
    pub kind: ShakeKind,
    /// Seconds played, on the sensor's clock.
    pub elapsed_s: f32,
    /// The noise seed (as the director picks them: one more than the noise shakes already
    /// playing when it started).
    pub seed: f32,
}

// ---------------------------------------------------------------------------
// Per-camera state
// ---------------------------------------------------------------------------

/// The unshaken pose a shake was added to, and the pose written.
#[derive(Debug, Clone, Copy, PartialEq)]
struct AppliedPose {
    base_translation: Vec3,
    base_rotation: Quat,
    shaken_translation: Vec3,
    shaken_rotation: Quat,
}

/// A camera's shakes: those on their way, those playing, and the pose last added. Inserted on
/// every camera that has (or defaults to) a sensor; not authored data.
#[derive(Component, Debug, Clone, Default, PartialEq)]
pub struct ShakeReceiver {
    pending: Vec<PendingShake>,
    active: Vec<ActiveShake>,
    applied: Option<AppliedPose>,
    pose: ShakePose,
    received: u64,
    fired: u64,
}

impl ShakeReceiver {
    /// Shakes on their way, in emit order.
    pub fn pending(&self) -> &[PendingShake] {
        &self.pending
    }

    /// Shakes playing, in start order.
    pub fn active(&self) -> &[ActiveShake] {
        &self.active
    }

    /// The pose added this frame, after gain (in the sensor's axes before the camera rotation).
    pub fn pose(&self) -> ShakePose {
        self.pose
    }

    /// Shakes received from emits so far.
    pub fn received(&self) -> u64 {
        self.received
    }

    /// Shakes that arrived and started so far.
    pub fn fired(&self) -> u64 {
        self.fired
    }

    /// Whether a shake is pending or playing.
    pub fn is_busy(&self) -> bool {
        !self.pending.is_empty() || !self.active.is_empty()
    }

    /// Drops every pending and playing shake (the pose already added is taken off next frame).
    pub fn clear(&mut self) {
        self.pending.clear();
        self.active.clear();
    }

    /// Puts back the shakes of `earlier` (those on their way and those playing, with their
    /// counts) and keeps the pose this receiver last added, so taking that pose off the camera
    /// next frame still matches the camera (a render-out job with the world held, ADR-2687).
    pub fn restore_shakes(&mut self, earlier: &ShakeReceiver) {
        self.pending.clone_from(&earlier.pending);
        self.active.clone_from(&earlier.active);
        self.received = earlier.received;
        self.fired = earlier.fired;
    }

    /// Queues `planned` to start on `arrival_frame`.
    pub fn push(&mut self, planned: PlannedShake, emitted_frame: u64, arrival_frame: u64) {
        self.received += 1;
        self.pending.push(PendingShake {
            emitted_frame,
            arrival_frame,
            distance_m: planned.distance_m,
            kind: planned.kind,
        });
    }

    /// Starts `kind` now. A noise shake's seed is one more than the noise shakes playing, as the
    /// [`CameraDirector`] numbers its own.
    pub fn start(&mut self, kind: ShakeKind) {
        let noise = self
            .active
            .iter()
            .filter(|a| matches!(a.kind, ShakeKind::Noise(_)))
            .count();
        self.fired += 1;
        self.active.push(ActiveShake {
            kind,
            elapsed_s: 0.0,
            seed: noise as f32 + 1.0,
        });
    }

    /// Starts every pending shake whose arrival is at or before `frame`, in emit order.
    pub fn fire_due(&mut self, frame: u64) {
        if self.pending.iter().all(|p| p.arrival_frame > frame) {
            return;
        }
        let pending = std::mem::take(&mut self.pending);
        for shake in pending {
            if shake.arrival_frame <= frame {
                self.start(shake.kind);
            } else {
                self.pending.push(shake);
            }
        }
    }

    /// Advances the playing shakes by `delta` seconds, drops the ended ones and returns the
    /// combined pose with `director` (the director's pose this frame and its strength) as the
    /// first source. No gain is applied.
    pub fn sample(
        &mut self,
        delta: f32,
        combine: ShakeCombine,
        director: Option<(ShakePose, f32)>,
    ) -> ShakePose {
        let delta = if delta.is_finite() && delta > 0.0 {
            delta
        } else {
            0.0
        };
        let mut combiner = Combiner::default();
        if let Some((pose, strength)) = director
            && !pose.is_idle()
        {
            combiner.push(pose, strength);
        }
        self.active.retain_mut(|shake| {
            shake.elapsed_s += delta;
            match shake.kind.pose(shake.elapsed_s, shake.seed) {
                Some(pose) => {
                    combiner.push(pose, shake.kind.strength(shake.elapsed_s));
                    true
                }
                None => false,
            }
        });
        combiner.finish(combine)
    }
}

/// Sums poses and keeps the strongest.
#[derive(Default)]
struct Combiner {
    sum: ShakePose,
    best: Option<(f32, ShakePose)>,
}

impl Combiner {
    fn push(&mut self, pose: ShakePose, strength: f32) {
        self.sum.offset += pose.offset;
        self.sum.pitch += pose.pitch;
        self.sum.yaw += pose.yaw;
        self.sum.roll += pose.roll;
        let strength = if strength.is_finite() { strength } else { 0.0 };
        if self.best.is_none_or(|(best, _)| strength > best) {
            self.best = Some((strength, pose));
        }
    }

    fn finish(self, combine: ShakeCombine) -> ShakePose {
        match combine {
            ShakeCombine::Add => self.sum,
            ShakeCombine::Max => self.best.map(|(_, pose)| pose).unwrap_or_default(),
        }
    }
}

/// `pose` with every term multiplied by `gain` (unchanged for a gain of exactly 1).
fn scaled(pose: ShakePose, gain: f32) -> ShakePose {
    if gain == 1.0 {
        return pose;
    }
    ShakePose {
        offset: pose.offset * gain,
        pitch: pose.pitch * gain,
        yaw: pose.yaw * gain,
        roll: pose.roll * gain,
    }
}

/// `pose`, or the idle pose when any term is not finite.
fn finite_pose(pose: ShakePose) -> ShakePose {
    if pose.offset.is_finite() && pose.pitch.is_finite() && pose.yaw.is_finite() && pose.roll.is_finite() {
        pose
    } else {
        ShakePose::default()
    }
}

// ---------------------------------------------------------------------------
// The field: step counter, seeds, counters
// ---------------------------------------------------------------------------

/// The fixed-step clock emits are delayed on, the seed counter of mount oscillations, and what
/// has been sent. Installed by [`CameraPlugin`](super::CameraPlugin).
#[derive(Resource, Debug, Clone)]
pub struct ShakeField {
    frame: u64,
    fixed_hz: f32,
    next_seed: u64,
    emitted: u64,
    delivered: u64,
    contact_emitted: u64,
    contact_last: HashMap<Entity, u64>,
}

impl Default for ShakeField {
    fn default() -> Self {
        Self {
            frame: 0,
            fixed_hz: 60.0,
            next_seed: 1,
            emitted: 0,
            delivered: 0,
            contact_emitted: 0,
            contact_last: HashMap::new(),
        }
    }
}

impl ShakeField {
    /// Fixed steps run so far.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// The fixed rate delays are converted at (the app's `FixedTime`).
    pub fn fixed_hz(&self) -> f32 {
        self.fixed_hz
    }

    /// The seed the next emit's mount oscillations start from.
    pub fn next_seed(&self) -> u64 {
        self.next_seed
    }

    /// Starts the emit seeds over, as in a new field, so a shock emitted next oscillates the same
    /// way in every render-out job (ADR-2687). Call only when no camera has a shake on its way or
    /// playing.
    pub fn restart_seeds(&mut self) {
        self.next_seed = Self::default().next_seed;
    }

    /// Puts the emit seed back where [`Self::next_seed`] read it: a render-out job with the world
    /// held leaves the next emit's oscillation where it found it (ADR-2814).
    pub fn restore_next_seed(&mut self, seed: u64) {
        self.next_seed = seed;
    }

    /// Emits read so far.
    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Shakes handed to cameras so far (an emit felt by two cameras counts twice).
    pub fn delivered(&self) -> u64 {
        self.delivered
    }

    /// Emits raised by [`ContactShake`] bodies so far.
    pub fn contact_emitted(&self) -> u64 {
        self.contact_emitted
    }

    /// The step a shake `delay_s` seconds after now starts on: now plus the delay in steps,
    /// rounded up so it never arrives early (the law of [`ShockShake::arrival_frame`]).
    pub fn arrival_frame(&self, delay_s: f64) -> u64 {
        frame_after(delay_s, self.fixed_hz, self.frame)
    }

    /// Steps from the emit to the arrival for a delay of `delay_s` seconds.
    pub fn delay_steps(&self, delay_s: f64) -> u64 {
        frame_after(delay_s, self.fixed_hz, 0)
    }
}

// ---------------------------------------------------------------------------
// Contact emitter
// ---------------------------------------------------------------------------

/// Makes a physics body shake listening cameras when it is hit: each `regia_physics::ContactForce`
/// report on this body above [`min_force_n`](Self::min_force_n) emits [`shake`](Self::shake)
/// from the body's position, its amplitude scaled by `force / reference_force_n` (at most
/// [`max_scale`](Self::max_scale)).
///
/// The collider must raise contact-force events: `Collider::active_events.contact_force` on and
/// `contact_force_threshold` at or below `min_force_n`. Opt in per body; nothing emits by default.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component, Default)]
pub struct ContactShake {
    /// Channels the shake is sent on.
    pub channels: u32,
    /// The shake at the reference force.
    pub shake: Shake,
    /// Force in newtons at which the shake plays at its own amplitude. Non-positive or not finite
    /// reads as 1 N.
    pub reference_force_n: f32,
    /// Forces below this (newtons) emit nothing.
    pub min_force_n: f32,
    /// Largest amplitude multiplier. Negative or not finite reads as 1.
    pub max_scale: f32,
    /// Collision layers of the other body that trigger it, as a bitmask over
    /// `CollisionLayer::index` (`u32::MAX`: any layer, including indices above 31).
    pub layers: u32,
    /// Falloff radius of the emitted shake (metres).
    pub falloff_radius_m: f32,
    /// Fixed steps after an emit during which this body emits nothing more (a resting contact
    /// reports a force every step).
    pub cooldown_steps: u32,
}

impl Default for ContactShake {
    fn default() -> Self {
        Self {
            channels: ShakeSensor::DEFAULT_CHANNELS,
            shake: Shake {
                amplitude: 0.05,
                frequency_hz: 12.0,
                duration_secs: 0.35,
                falloff: 2.0,
            },
            reference_force_n: 5_000.0,
            min_force_n: 500.0,
            max_scale: 3.0,
            layers: ShakeSensor::ALL_CHANNELS,
            falloff_radius_m: 60.0,
            cooldown_steps: 6,
        }
    }
}

impl ContactShake {
    /// Whether a contact with a body on collision layer `layer` triggers it.
    pub fn matches_layer(&self, layer: u16) -> bool {
        self.layers == u32::MAX || (layer < 32 && self.layers & (1u32 << layer) != 0)
    }

    /// The amplitude multiplier for a contact of `force_n` newtons with a body on `layer`, or
    /// `None` when it emits nothing (below the minimum, another layer, a non-finite force).
    pub fn scale(&self, force_n: f32, layer: u16) -> Option<f32> {
        if !force_n.is_finite() || force_n < self.min_force_n || !self.matches_layer(layer) {
            return None;
        }
        let reference = if self.reference_force_n.is_finite() && self.reference_force_n > 0.0 {
            self.reference_force_n
        } else {
            1.0
        };
        let max = if self.max_scale.is_finite() && self.max_scale >= 0.0 {
            self.max_scale
        } else {
            1.0
        };
        let scale = (force_n / reference).min(max);
        (scale > 0.0).then_some(scale)
    }
}

// ---------------------------------------------------------------------------
// Systems
// ---------------------------------------------------------------------------

/// Puts the unshaken pose back before rigs and shots run. Holds the
/// `restore_unshaken` system; ordered after `PostUpdateSet::LateScripts` and before
/// [`CameraSet::Update`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ShakeRestoreSet;

/// Registers the sensor types and systems (called by [`CameraPlugin`](super::CameraPlugin)).
pub(super) fn install(app: &mut App) {
    app.register_type::<ShakeSensor>()
        .register_type::<ShakeCombine>()
        .register_type::<ShakeAxes>()
        .register_type::<ShakeClock>()
        .register_type::<ContactShake>()
        .register_type::<Shake>()
        .init_resource::<ShakeField>()
        .add_message::<ShakeEmit>()
        .add_message::<ContactForce>()
        .configure_sets(
            PostUpdate,
            ShakeRestoreSet
                .after(PostUpdateSet::LateScripts)
                .before(CameraSet::Update),
        )
        .configure_sets(PostUpdate, CameraSet::Director.after(ShotDriveSet))
        .add_systems(FixedUpdate, count_shake_steps)
        .add_systems(PostUpdate, restore_unshaken.in_set(ShakeRestoreSet))
        .add_systems(
            PostUpdate,
            (
                ensure_receivers,
                emit_contact_shakes,
                receive_shake_emits,
                apply_shakes,
            )
                .chain()
                .in_set(CameraSet::Director)
                .before(PostUpdateSet::TransformPropagate),
        );
}

/// `FixedUpdate`: one more step on the field's clock, at the app's fixed rate.
fn count_shake_steps(fixed: Option<Res<FixedTime>>, mut field: ResMut<ShakeField>) {
    if let Some(fixed) = fixed {
        let step = fixed.step().as_secs_f64();
        if step > 0.0 {
            field.fixed_hz = (1.0 / step) as f32;
        }
    }
    field.frame = field.frame.saturating_add(1);
}

/// [`ShakeRestoreSet`]: takes last frame's shake off every camera that still holds the shaken
/// pose. A camera something else moved since keeps the new pose: that is the new unshaken pose.
pub(super) fn restore_unshaken(mut cameras: Query<(&mut Transform, &mut ShakeReceiver)>) {
    for (mut transform, mut receiver) in &mut cameras {
        let Some(applied) = receiver.applied.take() else {
            continue;
        };
        if transform.translation == applied.shaken_translation
            && transform.rotation == applied.shaken_rotation
        {
            transform.translation = applied.base_translation;
            transform.rotation = applied.base_rotation;
        }
    }
}

/// Cameras that have or default to a sensor and have no [`ShakeReceiver`] yet.
type NewSensorCameras<'w, 's> = Query<
    'w,
    's,
    Entity,
    (
        With<Transform>,
        Without<ShakeReceiver>,
        Or<(
            With<CameraRig>,
            With<ShakeSensor>,
            With<CinematicCamera>,
            With<PlayerCamera>,
        )>,
    ),
>;

/// Every camera with a [`ShakeReceiver`]: pose, state, sensor, rig marker, rig runtime, cinematic
/// marker, local player.
type ShakeCameras<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static mut Transform,
        &'static mut ShakeReceiver,
        Option<&'static ShakeSensor>,
        Has<CameraRig>,
        Option<&'static mut CameraRuntime>,
        Has<CinematicCamera>,
        Option<&'static PlayerCamera>,
    ),
>;

/// Adds a [`ShakeReceiver`] to every camera that has or defaults to a sensor.
fn ensure_receivers(
    mut commands: Commands,
    player: Option<Res<ShotPlayer>>,
    cameras: NewSensorCameras,
    bare: Query<(), (With<Transform>, Without<ShakeReceiver>)>,
) {
    for entity in &cameras {
        commands.entity(entity).try_insert(ShakeReceiver::default());
    }
    if let Some(entity) = player.and_then(|p| p.driven_camera())
        && bare.contains(entity)
        && !cameras.contains(entity)
    {
        commands.entity(entity).try_insert(ShakeReceiver::default());
    }
}

/// Turns `ContactForce` reports on [`ContactShake`] bodies into [`ShakeEmit`]s.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_shake_contacts")]
fn emit_contact_shakes(
    mut contacts: MessageReader<ContactForce>,
    emitters: Query<(&ContactShake, Option<&GlobalTransform>, Option<&Transform>)>,
    colliders: Query<&Collider>,
    mut field: ResMut<ShakeField>,
    mut emits: MessageWriter<ShakeEmit>,
) {
    let frame = field.frame;
    for contact in contacts.read() {
        for (source, other) in [
            (contact.entity_a, contact.entity_b),
            (contact.entity_b, contact.entity_a),
        ] {
            if source == other {
                continue;
            }
            let Ok((emitter, global, local)) = emitters.get(source) else {
                continue;
            };
            let layer = colliders.get(other).map_or(0, |collider| collider.layer.index);
            let Some(scale) = emitter.scale(contact.total_force, layer) else {
                continue;
            };
            if let Some(&last) = field.contact_last.get(&source)
                && frame.saturating_sub(last) < u64::from(emitter.cooldown_steps)
            {
                continue;
            }
            field.contact_last.insert(source, frame);
            field.contact_emitted += 1;
            let origin = global
                .map(GlobalTransform::translation)
                .or_else(|| local.map(|t| t.translation))
                .unwrap_or(Vec3::ZERO);
            emits.write(ShakeEmit {
                origin: Some(origin),
                channels: emitter.channels,
                preset: ShakePreset::Noise(Shake {
                    amplitude: emitter.shake.amplitude * scale,
                    ..emitter.shake
                }),
                falloff_radius_m: emitter.falloff_radius_m,
                travel_speed_m_s: None,
            });
        }
    }
    trim_contact_table(&mut field.contact_last, frame, CONTACT_TABLE_LIMIT);
}

/// Forgets contact cooldown entries older than `CONTACT_FORGET_STEPS` once the table holds more
/// than `limit`; when every entry is still young, keeps only the newest `limit` (ties by entity
/// bits, so the trim is deterministic). The table never stays above `limit` (ADR-1809).
fn trim_contact_table(table: &mut HashMap<Entity, u64>, frame: u64, limit: usize) {
    if table.len() <= limit {
        return;
    }
    table.retain(|_, last| frame.saturating_sub(*last) < CONTACT_FORGET_STEPS);
    if table.len() <= limit {
        return;
    }
    let Some(last_kept) = limit.checked_sub(1) else {
        table.clear();
        return;
    };
    let mut entries: Vec<(u64, u64)> = table
        .iter()
        .map(|(entity, last)| (*last, entity.to_bits()))
        .collect();
    entries.sort_unstable_by(|a, b| b.cmp(a));
    let cut = entries[last_kept];
    table.retain(|entity, last| (*last, entity.to_bits()) >= cut);
}

/// Cameras that may hear an emit: pose, state, sensor, rig marker, cinematic marker.
type ListeningCameras<'w, 's> = Query<
    'w,
    's,
    (
        Entity,
        &'static Transform,
        &'static mut ShakeReceiver,
        Option<&'static ShakeSensor>,
        Has<CameraRig>,
        Has<CinematicCamera>,
    ),
>;

/// Hands every [`ShakeEmit`] to the listening cameras, measured from their unshaken pose.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_shake_emits")]
fn receive_shake_emits(
    mut emits: MessageReader<ShakeEmit>,
    mut field: ResMut<ShakeField>,
    player: Option<Res<ShotPlayer>>,
    mut cameras: ListeningCameras,
) {
    let driven = player.and_then(|p| p.driven_camera());
    for emit in emits.read() {
        let seed = field.next_seed;
        field.next_seed = field.next_seed.wrapping_add(2);
        field.emitted += 1;
        for (entity, transform, mut receiver, explicit, rig, cinematic) in &mut cameras {
            let Some(sensor) = effective_sensor(explicit, rig, cinematic || driven == Some(entity)) else {
                continue;
            };
            if !sensor.listens(emit.channels) {
                continue;
            }
            let distance = emit
                .origin
                .map_or(0.0, |origin| (transform.translation - origin).length());
            for planned in emit.plan(distance, seed) {
                let arrival = field.arrival_frame(planned.delay_s);
                receiver.push(planned, field.frame, arrival);
                field.delivered += 1;
            }
        }
    }
}

/// [`CameraSet::Director`]: samples the shared director and each local player's director once,
/// starts arrived shakes, and adds each camera's combined shake to the pose the rigs and the
/// shot player left. A [`PlayerCamera`] camera hears its player's director only; every other
/// camera hears the shared one (spec 14 § B5, ADR-7263).
#[tracing::instrument(skip_all, target = "regia_framework", name = "cameras_director")]
pub(super) fn apply_shakes(
    time: Res<Time>,
    real: Option<Res<regia_app::RealTime>>,
    field: Res<ShakeField>,
    (director, players): (Option<ResMut<CameraDirector>>, Option<ResMut<PlayerManager>>),
    player: Option<Res<ShotPlayer>>,
    motion: Option<Res<crate::settings::SceneMotion>>,
    mut cameras: ShakeCameras,
) {
    let director = director.map(|mut director| {
        let pose = director.sample_pose(time.delta_secs());
        (pose, director.strength())
    });
    // Every player director advances once a frame, whether or not a camera hears it.
    let player_directors: Vec<(u8, (ShakePose, f32))> = players
        .map(|mut players| {
            players
                .directors
                .iter_mut()
                .map(|(index, director)| {
                    let pose = director.sample_pose(time.delta_secs());
                    (*index, (pose, director.strength()))
                })
                .collect()
        })
        .unwrap_or_default();
    let unscaled = real.map_or(0.0, |real| real.delta().as_secs_f32());
    let player_scale = motion.map(|motion| motion.shake_scale).unwrap_or(1.0);
    let driven = player.and_then(|p| p.driven_camera());
    for (entity, mut transform, mut receiver, explicit, rig, runtime, cinematic, player_camera) in
        &mut cameras
    {
        receiver.fire_due(field.frame);
        let sensor = effective_sensor(
            explicit,
            rig || player_camera.is_some(),
            cinematic || driven == Some(entity),
        );
        // A player's camera hears that player's director; no director (a remote or departed
        // player) means no director shakes.
        let source = match player_camera {
            Some(PlayerCamera(index)) => player_directors
                .iter()
                .find(|(at, _)| at == index)
                .map(|(_, sampled)| *sampled),
            None => director,
        };
        let (pose, axes) = match sensor {
            Some(sensor) => {
                let delta = match sensor.clock {
                    ShakeClock::Gameplay => time.delta_secs(),
                    ShakeClock::Unscaled => unscaled,
                };
                let heard = source.filter(|_| sensor.listens(ShakeSensor::DEFAULT_CHANNELS));
                let pose = receiver.sample(delta, sensor.combine, heard);
                (
                    finite_pose(scaled(pose, sensor.effective_gain_scaled(player_scale))),
                    sensor.axes,
                )
            }
            None => {
                receiver.clear();
                (ShakePose::default(), ShakeAxes::World)
            }
        };
        receiver.pose = pose;
        let offset = if pose.is_idle() {
            receiver.applied = None;
            Vec3::ZERO
        } else {
            let base_translation = transform.translation;
            let base_rotation = transform.rotation;
            let offset = match axes {
                ShakeAxes::World => pose.offset,
                ShakeAxes::Camera => base_rotation * pose.offset,
            };
            transform.translation = base_translation + offset;
            transform.rotation = base_rotation * pose.rotation();
            receiver.applied = Some(AppliedPose {
                base_translation,
                base_rotation,
                shaken_translation: transform.translation,
                shaken_rotation: transform.rotation,
            });
            offset
        };
        if let Some(mut runtime) = runtime {
            let tilt = Vec2::new(pose.pitch, pose.yaw);
            if runtime.shake_offset != offset || runtime.shake_roll != pose.roll || runtime.shake_tilt != tilt
            {
                runtime.shake_offset = offset;
                runtime.shake_roll = pose.roll;
                runtime.shake_tilt = tilt;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use regia_app::enable_fixed_step;
    use regia_render::Camera;

    use super::*;
    use crate::camera::{CameraMode, CameraPlugin, Free};
    use crate::shock_shake::{BlastShock, ShockShakePlugin, ShockShakeSchedule};
    use crate::shots::ShotPlugin;
    use crate::shots::asset::fixtures::{sequence, shot};

    #[test]
    fn the_contact_table_never_stays_above_its_limit() {
        let mut table: HashMap<Entity, u64> = HashMap::new();
        let mut world = World::new();
        for step in 0..10_u64 {
            table.insert(world.spawn_empty().id(), 100 + step);
        }
        // All young: only the newest four survive.
        trim_contact_table(&mut table, 110, 4);
        assert_eq!(table.len(), 4);
        assert!(table.values().all(|last| *last >= 106), "{table:?}");
        // Old entries go first.
        let old = world.spawn_empty().id();
        table.insert(old, 0);
        trim_contact_table(&mut table, CONTACT_FORGET_STEPS + 10, 4);
        assert!(!table.contains_key(&old));
        assert_eq!(table.len(), 4);
    }

    /// A strong, clearly visible noise shake.
    fn hard_shake() -> Shake {
        Shake {
            amplitude: 0.5,
            frequency_hz: 9.0,
            duration_secs: 0.5,
            falloff: 1.0,
        }
    }

    /// A headless app with the camera plugin at a fixed rate of `hz`, one frame per step.
    fn app(hz: f64) -> App {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin));
        enable_fixed_step(&mut app, hz);
        app
    }

    /// A free-flying rig with no input at `at`: it stays put, so any motion is the shake.
    fn spawn_rig(app: &mut App, at: Vec3) -> Entity {
        app.world_mut()
            .spawn((
                Transform::from_translation(at),
                CameraRig {
                    mode: CameraMode::Free(Free::default()),
                    ..CameraRig::default()
                },
            ))
            .id()
    }

    /// A plain render camera (no rig) at `at`.
    fn spawn_plain(app: &mut App, at: Vec3) -> Entity {
        app.world_mut()
            .spawn((Transform::from_translation(at), Camera::default()))
            .id()
    }

    fn translation(app: &App, entity: Entity) -> Vec3 {
        app.world()
            .get::<Transform>(entity)
            .expect("transform")
            .translation
    }

    fn rotation(app: &App, entity: Entity) -> Quat {
        app.world().get::<Transform>(entity).expect("transform").rotation
    }

    fn emit(app: &mut App, emit: ShakeEmit) {
        app.world_mut().write_message(emit);
    }

    /// Largest distance `entity` moves from `at` over `frames` updates.
    fn max_offset(app: &mut App, entity: Entity, at: Vec3, frames: usize) -> f32 {
        (0..frames)
            .map(|_| {
                app.update();
                (translation(app, entity) - at).length()
            })
            .fold(0.0, f32::max)
    }

    /// Spec 14 § B5 (ADR-7263): each local player's director owns its shakes. With two players a
    /// shake routed to player 1 moves only player 1's camera; the shared director's shakes reach
    /// the cameras that belong to no player, not the player cameras.
    #[test]
    fn a_shake_routed_to_one_player_moves_only_that_players_camera() {
        use crate::camera::blend::PlayerCamera;
        use crate::player::{PlayerKind, PlayerManager, PlayerPlugin};
        let mut app = app(60.0);
        app.add_plugins(PlayerPlugin);
        let mut manager = app
            .world_mut()
            .remove_resource::<PlayerManager>()
            .expect("manager");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p0");
        manager
            .spawn_player(app.world_mut(), PlayerKind::Local)
            .expect("p1");
        app.world_mut().insert_resource(manager);
        let (at0, at1, at_shared) = (Vec3::ZERO, Vec3::new(10.0, 0.0, 0.0), Vec3::new(20.0, 0.0, 0.0));
        let cam0 = spawn_rig(&mut app, at0);
        let cam1 = spawn_rig(&mut app, at1);
        let shared = spawn_rig(&mut app, at_shared);
        app.world_mut().entity_mut(cam0).insert(PlayerCamera(0));
        app.world_mut().entity_mut(cam1).insert(PlayerCamera(1));
        app.update();

        let run = |app: &mut App, frames: usize| {
            let mut most = [0.0f32; 3];
            for _ in 0..frames {
                app.update();
                for (slot, (entity, at)) in
                    most.iter_mut()
                        .zip([(cam0, at0), (cam1, at1), (shared, at_shared)])
                {
                    *slot = slot.max((translation(app, entity) - at).length());
                }
            }
            most
        };
        app.world_mut()
            .resource_mut::<PlayerManager>()
            .camera_director_mut(1)
            .expect("p1 director")
            .shake(hard_shake());
        let [p0, p1, rest] = run(&mut app, 20);
        assert!(p1 > 0.01, "player 1's camera shakes: {p1}");
        assert_eq!(p0, 0.0, "player 0's camera does not feel player 1's shake");
        assert_eq!(rest, 0.0, "a camera of no player does not feel it either");
        // Let it end, then shake the shared director: only the non-player camera moves.
        run(&mut app, 40);
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        let [p0, p1, rest] = run(&mut app, 20);
        assert!(
            rest > 0.01,
            "the shared director shakes the non-player camera: {rest}"
        );
        assert_eq!(
            (p0, p1),
            (0.0, 0.0),
            "player cameras feel only their own director"
        );
        assert!(
            !app.world()
                .resource::<PlayerManager>()
                .camera_director(1)
                .expect("p1")
                .is_shaking(),
            "the player shake ran down"
        );
    }

    #[test]
    fn channel_masks_isolate_two_cameras() {
        let mut app = app(60.0);
        let a_at = Vec3::ZERO;
        let b_at = Vec3::new(10.0, 0.0, 0.0);
        let a = spawn_rig(&mut app, a_at);
        let b = spawn_rig(&mut app, b_at);
        app.world_mut()
            .entity_mut(a)
            .insert(ShakeSensor::on_channels(1 << 1));
        app.world_mut()
            .entity_mut(b)
            .insert(ShakeSensor::on_channels(1 << 2));

        emit(
            &mut app,
            ShakeEmit::everywhere(hard_shake()).with_channels(1 << 2),
        );
        let mut moved_a = 0.0f32;
        let mut moved_b = 0.0f32;
        for _ in 0..40 {
            app.update();
            moved_a = moved_a.max((translation(&app, a) - a_at).length());
            moved_b = moved_b.max((translation(&app, b) - b_at).length());
        }
        assert_eq!(moved_a, 0.0, "channel 1 does not hear channel 2");
        assert!(moved_b > 0.05, "channel 2 shakes: {moved_b}");
        assert_eq!(translation(&app, b), b_at, "settled exactly back");

        emit(
            &mut app,
            ShakeEmit::everywhere(hard_shake()).with_channels(1 << 1),
        );
        let mut moved_b = 0.0f32;
        let mut moved_a = 0.0f32;
        for _ in 0..40 {
            app.update();
            moved_a = moved_a.max((translation(&app, a) - a_at).length());
            moved_b = moved_b.max((translation(&app, b) - b_at).length());
        }
        assert!(moved_a > 0.05, "channel 1 shakes now: {moved_a}");
        assert_eq!(moved_b, 0.0, "and channel 2 stays still");

        // The director plays on the default channel: neither camera listens to it.
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        assert_eq!(max_offset(&mut app, a, a_at, 40), 0.0);
        assert_eq!(max_offset(&mut app, b, b_at, 40), 0.0);
    }

    #[test]
    fn plain_cameras_need_a_sensor_and_rigs_default_on() {
        let mut app = app(60.0);
        let rig_at = Vec3::new(0.0, 2.0, 0.0);
        let pip_at = Vec3::new(5.0, 2.0, 0.0);
        let rig = spawn_rig(&mut app, rig_at);
        let pip = spawn_plain(&mut app, pip_at);

        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        let mut rig_moved = 0.0f32;
        let mut pip_moved = 0.0f32;
        for _ in 0..40 {
            app.update();
            rig_moved = rig_moved.max((translation(&app, rig) - rig_at).length());
            pip_moved = pip_moved.max((translation(&app, pip) - pip_at).length());
        }
        assert!(
            rig_moved > 0.05,
            "a rig feels the director by default: {rig_moved}"
        );
        assert_eq!(pip_moved, 0.0, "a plain camera without a sensor never shakes");
        assert!(app.world().get::<ShakeReceiver>(pip).is_none());

        // Opting in: the plain camera gets a sensor; opting out: the rig is switched off.
        app.world_mut().entity_mut(pip).insert(ShakeSensor::default());
        app.world_mut().entity_mut(rig).insert(ShakeSensor::off());
        emit(&mut app, ShakeEmit::everywhere(hard_shake()));
        let mut rig_moved = 0.0f32;
        let mut pip_moved = 0.0f32;
        for _ in 0..40 {
            app.update();
            rig_moved = rig_moved.max((translation(&app, rig) - rig_at).length());
            pip_moved = pip_moved.max((translation(&app, pip) - pip_at).length());
        }
        assert!(pip_moved > 0.05, "the sensor makes it shake: {pip_moved}");
        assert_eq!(rig_moved, 0.0, "ShakeSensor::off silences a rig");
    }

    #[test]
    fn add_sums_and_max_keeps_the_strongest() {
        let dt = 1.0 / 60.0;
        let strong = Shake {
            amplitude: 1.0,
            frequency_hz: 9.0,
            duration_secs: 1.0,
            falloff: 4.0,
        };
        // A flat envelope: weaker at first, stronger once (1 − t)^4 < 0.2, after t ≈ 0.33 s.
        let weak = Shake {
            amplitude: 0.2,
            frequency_hz: 4.0,
            duration_secs: 1.0,
            falloff: 0.0,
        };
        let mut both = ShakeReceiver::default();
        both.start(ShakeKind::Noise(strong));
        both.start(ShakeKind::Noise(weak));
        assert_eq!(both.active()[1].seed, 2.0, "seeds number the noise shakes");
        let mut max = both.clone();
        let mut only_strong = ShakeReceiver::default();
        only_strong.start(ShakeKind::Noise(strong));
        let mut only_weak = ShakeReceiver::default();
        only_weak.active.push(ActiveShake {
            kind: ShakeKind::Noise(weak),
            elapsed_s: 0.0,
            seed: 2.0,
        });

        let (mut strong_won, mut weak_won) = (0, 0);
        for frame in 0..55 {
            let s = only_strong.sample(dt, ShakeCombine::Add, None);
            let w = only_weak.sample(dt, ShakeCombine::Add, None);
            let sum = both.sample(dt, ShakeCombine::Add, None);
            let strongest = max.sample(dt, ShakeCombine::Max, None);
            assert!(
                (sum.offset - (s.offset + w.offset)).length() < 1.0e-6
                    && (sum.roll - (s.roll + w.roll)).abs() < 1.0e-6,
                "frame {frame}: add is the sum"
            );
            if strongest == s && strongest != w {
                strong_won += 1;
            } else if strongest == w && strongest != s {
                weak_won += 1;
            } else {
                panic!("frame {frame}: max is neither source alone: {strongest:?}");
            }
        }
        assert!(strong_won > 10 && weak_won > 10, "{strong_won} / {weak_won}");
        // The strong shake leads while its envelope is above 0.2, then the weak one takes over.
        let t_switch = 1.0 - 0.2f32.powf(0.25);
        assert!(
            ((strong_won as f32 + 1.0) * dt - t_switch).abs() < 2.0 * dt,
            "switched at frame {strong_won}"
        );
    }

    #[test]
    fn a_max_camera_with_two_emits_matches_the_strong_one_alone() {
        let mut app = app(60.0);
        let max_cam = spawn_rig(&mut app, Vec3::ZERO);
        let strong_cam = spawn_rig(&mut app, Vec3::ZERO);
        app.world_mut().entity_mut(max_cam).insert(ShakeSensor {
            channels: 0b11,
            combine: ShakeCombine::Max,
            ..ShakeSensor::default()
        });
        app.world_mut()
            .entity_mut(strong_cam)
            .insert(ShakeSensor::on_channels(0b01));
        emit(&mut app, ShakeEmit::everywhere(hard_shake()).with_channels(0b01));
        emit(
            &mut app,
            ShakeEmit::everywhere(Shake {
                amplitude: 0.1,
                ..hard_shake()
            })
            .with_channels(0b10),
        );
        let mut moved = false;
        for frame in 0..40 {
            app.update();
            assert_eq!(
                translation(&app, max_cam),
                translation(&app, strong_cam),
                "frame {frame}"
            );
            moved |= translation(&app, max_cam) != Vec3::ZERO;
        }
        assert!(moved);
        assert_eq!(
            app.world()
                .get::<ShakeReceiver>(max_cam)
                .expect("receiver")
                .fired(),
            2
        );
    }

    /// The camera's offset from `at` on every frame of a run that `setup` starts.
    fn trace(hz: f64, frames: usize, setup: impl FnOnce(&mut App)) -> (Vec<Vec3>, Vec<Quat>) {
        let mut app = app(hz);
        app.add_plugins(ShockShakePlugin);
        let rig = spawn_rig(&mut app, Vec3::ZERO);
        setup(&mut app);
        let mut offsets = Vec::with_capacity(frames);
        let mut rotations = Vec::with_capacity(frames);
        for _ in 0..frames {
            app.update();
            offsets.push(translation(&app, rig));
            rotations.push(rotation(&app, rig));
        }
        (offsets, rotations)
    }

    #[test]
    fn the_blast_preset_keeps_the_shock_law_numbers_and_frames() {
        let blast = ShockShake::showcase();
        let at = Vec3::new(0.0, 0.0, -343.0);
        let (legacy, _) = trace(60.0, 300, |app| {
            app.world_mut().write_message(BlastShock {
                position: at,
                shock: blast,
            });
        });
        let (sensor, _) = trace(60.0, 300, |app| {
            emit(app, ShakeEmit::at(at, ShakePreset::Blast(blast)))
        });
        assert_eq!(
            legacy, sensor,
            "the same shake on the same frames as the director path"
        );
        let first = sensor.iter().position(|o| *o != Vec3::ZERO).expect("it shakes");
        assert_eq!(
            first as u64,
            blast.arrival_frame(343.0, 60.0, 0),
            "343 m is 60 steps"
        );
    }

    #[test]
    fn the_ground_preset_keeps_the_ground_shock_numbers_and_frames() {
        let ground = GroundShock::default();
        let (_, legacy) = trace(60.0, 700, |app| {
            app.world_mut()
                .resource_mut::<ShockShakeSchedule>()
                .schedule_ground(&ground, 3300.0, 0);
        });
        let (_, sensor) = trace(60.0, 700, |app| {
            emit(
                app,
                ShakeEmit::at(Vec3::new(3300.0, 0.0, 0.0), ShakePreset::Ground(ground)),
            );
        });
        assert_eq!(legacy, sensor, "seismic wave and airblast as before");
        let first = sensor
            .iter()
            .position(|r| *r != Quat::IDENTITY)
            .expect("it tilts");
        assert_eq!(first, 36, "the P wave arrives 0.6 s after the blast");
    }

    #[test]
    fn arrival_frames_follow_the_fixed_rate_and_replay_identically() {
        let at = Vec3::new(0.0, 0.0, -686.0);
        let run = |hz: f64, frames: usize| {
            trace(hz, frames, |app| {
                emit(app, ShakeEmit::at(at, ShakePreset::Blast(ShockShake::default())))
            })
            .0
        };
        for (hz, frames, expected) in [(30.0, 120, 60usize), (60.0, 240, 120)] {
            let first = run(hz, frames);
            let again = run(hz, frames);
            assert_eq!(first, again, "{hz} Hz replays the same shake");
            let arrival = first.iter().position(|o| *o != Vec3::ZERO).expect("it shakes");
            assert_eq!(arrival, expected, "686 m is 2 s at {hz} Hz");
            assert!(
                first[..arrival].iter().all(|o| *o == Vec3::ZERO),
                "still before the front"
            );
        }
    }

    #[test]
    fn falloff_and_travel_plan_distance_and_delay() {
        let emit = ShakeEmit::at(Vec3::ZERO, ShakePreset::Noise(hard_shake()))
            .with_falloff(100.0)
            .with_travel(100.0);
        let near = emit.plan(50.0, 1);
        assert_eq!(near.len(), 1);
        assert!((near[0].delay_s - 0.5).abs() < 1.0e-12);
        assert_eq!(near[0].kind.peak(), 0.5 * 0.25, "a quarter at half the radius");
        assert!(emit.plan(100.0, 1).is_empty(), "nothing at the radius");
        assert!(emit.plan(150.0, 1).is_empty(), "nothing beyond it");
        assert!(emit.plan(f32::NAN, 1).is_empty());
        assert_eq!(
            emit.plan(0.0, 1)[0].kind.peak(),
            0.5,
            "full strength at the origin"
        );

        let everywhere = ShakeEmit::everywhere(hard_shake())
            .with_falloff(1.0)
            .with_travel(1.0);
        assert_eq!(everywhere.falloff_factor(1.0e6), 1.0, "no origin, no falloff");
        assert_eq!(everywhere.plan(0.0, 1)[0].delay_s, 0.0, "and no travel");

        for bad in [
            Shake {
                duration_secs: 0.0,
                ..hard_shake()
            },
            Shake {
                amplitude: f32::NAN,
                ..hard_shake()
            },
            Shake {
                frequency_hz: f32::INFINITY,
                ..hard_shake()
            },
        ] {
            assert!(ShakeEmit::everywhere(bad).plan(0.0, 1).is_empty(), "{bad:?}");
        }
        let slow = ShakeEmit::at(Vec3::ZERO, ShakePreset::Blast(ShockShake::default())).with_travel(686.0);
        assert!(
            (slow.plan(343.0, 1)[0].delay_s - 0.5).abs() < 1.0e-12,
            "travel replaces the shock speed"
        );
    }

    #[test]
    fn travel_delays_a_noise_shake_by_whole_steps() {
        let mut app = app(60.0);
        let at = Vec3::ZERO;
        let rig = spawn_rig(&mut app, at);
        emit(
            &mut app,
            ShakeEmit::at(Vec3::new(200.0, 0.0, 0.0), ShakePreset::Noise(hard_shake())).with_travel(100.0),
        );
        app.update();
        let pending = app
            .world()
            .get::<ShakeReceiver>(rig)
            .expect("receiver")
            .pending()
            .to_vec();
        assert_eq!(pending.len(), 1);
        assert_eq!(
            pending[0].arrival_frame - pending[0].emitted_frame,
            120,
            "2 s at 60 Hz"
        );
        let mut first = None;
        for frame in 1..200 {
            app.update();
            if first.is_none() && translation(&app, rig) != at {
                first = Some(frame);
            }
        }
        assert_eq!(first, Some(120));
    }

    #[test]
    fn gain_scales_and_camera_axes_follow_the_view() {
        let mut app = app(60.0);
        let yaw = std::f32::consts::FRAC_PI_2;
        let spawn = |app: &mut App, sensor: ShakeSensor| {
            app.world_mut()
                .spawn((
                    Transform::default(),
                    CameraRig {
                        mode: CameraMode::Free(Free::default()),
                        yaw,
                        ..CameraRig::default()
                    },
                    sensor,
                ))
                .id()
        };
        let world_axes = spawn(&mut app, ShakeSensor::default());
        let doubled = spawn(
            &mut app,
            ShakeSensor {
                gain: 2.0,
                ..ShakeSensor::default()
            },
        );
        let camera_axes = spawn(
            &mut app,
            ShakeSensor {
                axes: ShakeAxes::Camera,
                ..ShakeSensor::default()
            },
        );
        let silent = spawn(
            &mut app,
            ShakeSensor {
                gain: f32::NAN,
                ..ShakeSensor::default()
            },
        );
        emit(&mut app, ShakeEmit::everywhere(hard_shake()));
        let view = Quat::from_rotation_y(yaw);
        let mut moved = false;
        for frame in 0..30 {
            app.update();
            let w = translation(&app, world_axes);
            assert_eq!(
                translation(&app, doubled),
                w * 2.0,
                "frame {frame}: gain 2 doubles"
            );
            let c = translation(&app, camera_axes);
            assert!(
                (c - view * w).length() < 1.0e-5,
                "frame {frame}: {c:?} vs {:?}",
                view * w
            );
            assert_eq!(translation(&app, silent), Vec3::ZERO, "a NaN gain is silent");
            moved |= w != Vec3::ZERO;
        }
        assert!(moved);
    }

    #[test]
    fn player_shake_scale_scales_the_pose_and_leaves_authored_gain() {
        let sensor = ShakeSensor {
            gain: 2.0,
            ..ShakeSensor::default()
        };
        assert_eq!(sensor.effective_gain(), 2.0);
        assert_eq!(sensor.effective_gain_scaled(0.5), 1.0);
        assert_eq!(sensor.effective_gain_scaled(0.0), 0.0);
        assert_eq!(sensor.effective_gain_scaled(f32::NAN), 0.0);
        assert_eq!(sensor.effective_gain_scaled(4.0), 2.0);
        assert_eq!(sensor.gain, 2.0);

        let mut plain = app(60.0);
        let mut scaled = app(60.0);
        scaled.insert_resource(crate::settings::SceneMotion {
            shake_scale: 0.5,
            bob_scale: 1.0,
            fov_override_deg: None,
        });
        let spawn = |app: &mut App| {
            app.world_mut()
                .spawn((
                    Transform::default(),
                    CameraRig {
                        mode: CameraMode::Free(Free::default()),
                        ..CameraRig::default()
                    },
                    ShakeSensor {
                        gain: 2.0,
                        ..ShakeSensor::default()
                    },
                ))
                .id()
        };
        let full = spawn(&mut plain);
        let half = spawn(&mut scaled);
        emit(&mut plain, ShakeEmit::everywhere(hard_shake()));
        emit(&mut scaled, ShakeEmit::everywhere(hard_shake()));
        let mut moved = false;
        for _ in 0..30 {
            plain.update();
            scaled.update();
            let authored = translation(&plain, full);
            let felt = translation(&scaled, half);
            assert!(
                (felt - authored * 0.5).length() < 1.0e-4,
                "{felt:?} vs {authored:?}"
            );
            moved |= authored != Vec3::ZERO;
        }
        assert!(moved, "the shake never moved the camera");
        assert_eq!(
            scaled.world().get::<ShakeSensor>(half).expect("sensor").gain,
            2.0,
            "the authored gain stays on the camera"
        );
    }

    #[test]
    fn the_unscaled_clock_keeps_shaking_while_paused() {
        let mut app = app(60.0);
        let game = spawn_rig(&mut app, Vec3::ZERO);
        let menu = spawn_rig(&mut app, Vec3::ZERO);
        app.world_mut().entity_mut(menu).insert(ShakeSensor {
            clock: ShakeClock::Unscaled,
            ..ShakeSensor::default()
        });
        emit(&mut app, ShakeEmit::everywhere(hard_shake()));
        for _ in 0..5 {
            app.update();
        }
        let gap = (translation(&app, game) - translation(&app, menu)).length();
        assert!(gap < 1.0e-6, "the same clock while running: {gap}");
        app.world_mut().resource_mut::<Time>().paused = true;
        app.update();
        let frozen = translation(&app, game);
        let mut menu_moves = 0;
        let mut last = translation(&app, menu);
        for _ in 0..10 {
            app.update();
            assert_eq!(translation(&app, game), frozen, "gameplay time stops");
            let now = translation(&app, menu);
            if now != last {
                menu_moves += 1;
            }
            last = now;
        }
        assert!(menu_moves >= 9, "the unscaled clock keeps playing: {menu_moves}");
        for _ in 0..40 {
            app.update();
        }
        assert_eq!(translation(&app, menu), Vec3::ZERO, "it ends on its own clock");
        assert_eq!(translation(&app, game), frozen, "the paused one waits");
    }

    #[test]
    fn a_shake_shows_under_a_playing_shot_and_never_accumulates() {
        let mut app = app(60.0);
        app.add_plugins(ShotPlugin);
        let camera = spawn_plain(&mut app, Vec3::ZERO);
        {
            let mut player = app.world_mut().resource_mut::<ShotPlayer>();
            player.load(sequence(vec![shot("a", 10.0)]));
            player.play();
        }
        app.update();
        assert_eq!(app.world().resource::<ShotPlayer>().driven_camera(), Some(camera));
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        let eye = |app: &App| app.world().resource::<ShotPlayer>().sample().expect("sample").eye;
        let mut shaken = 0;
        for _ in 0..20 {
            app.update();
            if translation(&app, camera) != eye(&app) {
                shaken += 1;
            }
        }
        assert!(
            shaken > 15,
            "the director's shake is visible during the shot: {shaken} frames"
        );

        // Paused mid-shake the shot stops writing; the shake keeps its own pose and ends cleanly.
        app.world_mut().resource_mut::<ShotPlayer>().pause();
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        for _ in 0..60 {
            app.update();
        }
        assert_eq!(
            translation(&app, camera),
            eye(&app),
            "back on the shot's pose, nothing left over"
        );
        app.world_mut().resource_mut::<ShotPlayer>().play();
        for _ in 0..10 {
            app.update();
            assert_eq!(translation(&app, camera), eye(&app), "the shot moves on unshaken");
        }
    }

    #[test]
    fn a_shot_camera_with_an_off_sensor_stays_on_the_shot() {
        let mut app = app(60.0);
        app.add_plugins(ShotPlugin);
        let camera = spawn_plain(&mut app, Vec3::ZERO);
        app.world_mut().entity_mut(camera).insert(ShakeSensor::off());
        {
            let mut player = app.world_mut().resource_mut::<ShotPlayer>();
            player.load(sequence(vec![shot("a", 10.0)]));
            player.play();
        }
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        for _ in 0..20 {
            app.update();
            let eye = app.world().resource::<ShotPlayer>().sample().expect("sample").eye;
            assert_eq!(translation(&app, camera), eye);
        }
    }

    #[test]
    fn a_pose_moved_by_someone_else_becomes_the_new_base() {
        let mut app = app(60.0);
        let camera = spawn_plain(&mut app, Vec3::ZERO);
        app.world_mut().entity_mut(camera).insert(ShakeSensor::default());
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .shake(hard_shake());
        for _ in 0..5 {
            app.update();
        }
        // A teleport (an agent, a script) between frames.
        let target = Vec3::new(100.0, 0.0, 0.0);
        app.world_mut()
            .get_mut::<Transform>(camera)
            .expect("transform")
            .translation = target;
        for _ in 0..60 {
            app.update();
        }
        assert_eq!(
            translation(&app, camera),
            target,
            "the teleport is kept, the shake removed"
        );
    }

    #[test]
    fn contact_shakes_scale_with_force_filter_layers_and_cool_down() {
        let mut app = app(60.0);
        let rig = spawn_rig(&mut app, Vec3::ZERO);
        let body = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, 0.0, -10.0)),
                ContactShake {
                    shake: Shake {
                        amplitude: 0.1,
                        ..hard_shake()
                    },
                    reference_force_n: 1_000.0,
                    min_force_n: 100.0,
                    max_scale: 3.0,
                    layers: 1 << 2,
                    falloff_radius_m: f32::INFINITY,
                    cooldown_steps: 6,
                    ..ContactShake::default()
                },
            ))
            .id();
        let collider = |layer: u16| Collider {
            layer: regia_physics::CollisionLayer::new(layer),
            ..Collider::default()
        };
        let ground = app.world_mut().spawn((Transform::default(), collider(2))).id();
        let wall = app.world_mut().spawn((Transform::default(), collider(3))).id();
        let contact = |a: Entity, b: Entity, total_force: f32| ContactForce {
            entity_a: a,
            entity_b: b,
            total_force,
        };
        app.world_mut().write_message(contact(wall, body, 5_000.0));
        app.world_mut().write_message(contact(body, ground, 50.0));
        app.update();
        assert_eq!(
            app.world().resource::<ShakeField>().contact_emitted(),
            0,
            "wrong layer, too soft"
        );

        app.world_mut().write_message(contact(body, ground, 2_000.0));
        app.update();
        let field = app.world().resource::<ShakeField>();
        assert_eq!(field.contact_emitted(), 1);
        let receiver = app.world().get::<ShakeReceiver>(rig).expect("receiver");
        assert_eq!(receiver.active().len(), 1);
        assert_eq!(
            receiver.active()[0].kind.peak(),
            0.1 * 2.0,
            "twice the reference force"
        );
        assert_ne!(translation(&app, rig), Vec3::ZERO);

        // A resting contact reports every step: the cooldown keeps it to one shake.
        for _ in 0..5 {
            app.world_mut().write_message(contact(ground, body, 2_000.0));
            app.update();
        }
        assert_eq!(app.world().resource::<ShakeField>().contact_emitted(), 1);
        app.world_mut().write_message(contact(ground, body, 1.0e9));
        app.update();
        assert_eq!(app.world().resource::<ShakeField>().contact_emitted(), 2);
        let receiver = app.world().get::<ShakeReceiver>(rig).expect("receiver");
        assert_eq!(
            receiver.active().last().map(|a| a.kind.peak()),
            Some(0.1 * 3.0),
            "capped at max_scale"
        );
    }

    #[test]
    fn a_body_landing_on_the_ground_shakes_the_camera() {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, regia_physics::PhysicsPlugin));
        enable_fixed_step(&mut app, 60.0);
        let rig = spawn_rig(&mut app, Vec3::new(0.0, 2.0, 6.0));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            regia_physics::RigidBody::static_body(),
            Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::new(5.0, 0.5, 5.0),
                },
                ..Collider::default()
            },
        ));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 3.0, 0.0)),
            regia_physics::RigidBody::dynamic(),
            Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                active_events: regia_physics::ActiveEvents::ALL,
                contact_force_threshold: 1.0,
                ..Collider::default()
            },
            regia_physics::Velocity::default(),
            ContactShake {
                min_force_n: 1.0,
                reference_force_n: 10.0,
                ..ContactShake::default()
            },
        ));
        let mut moved = 0.0f32;
        for _ in 0..120 {
            app.update();
            moved = moved.max((translation(&app, rig) - Vec3::new(0.0, 2.0, 6.0)).length());
        }
        assert!(
            app.world().resource::<ShakeField>().contact_emitted() > 0,
            "the landing emitted"
        );
        assert!(moved > 1.0e-3, "and the camera felt it: {moved}");
    }

    #[test]
    fn sensor_option_names_round_trip() {
        for combine in [ShakeCombine::Add, ShakeCombine::Max] {
            assert_eq!(ShakeCombine::from_name(combine.name()), Some(combine));
        }
        for axes in [ShakeAxes::World, ShakeAxes::Camera] {
            assert_eq!(ShakeAxes::from_name(axes.name()), Some(axes));
        }
        for clock in [ShakeClock::Gameplay, ShakeClock::Unscaled] {
            assert_eq!(ShakeClock::from_name(clock.name()), Some(clock));
        }
        assert_eq!(ShakeCombine::from_name("strongest"), Some(ShakeCombine::Max));
        assert!(ShakeCombine::from_name("mul").is_none());
        for name in NAMED_SHAKES {
            assert!(named_shake(name).is_some(), "{name}");
        }
        assert!(named_shake("earthquake").is_none());
        assert_eq!(ShakeSensor::default().effective_gain(), 1.0);
        assert_eq!(
            ShakeSensor {
                gain: 1.0e9,
                ..ShakeSensor::default()
            }
            .effective_gain(),
            ShakeSensor::MAX_GAIN
        );
        assert_eq!(
            ShakeSensor {
                gain: -1.0,
                ..ShakeSensor::default()
            }
            .effective_gain(),
            0.0
        );
    }
}
