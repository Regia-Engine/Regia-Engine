// SPDX-License-Identifier: MPL-2.0
//! First-person handheld camera: head sway, breathing and a walk bob that follows footfalls
//! (ADR-2449, `book/agents/camera-handheld.md`).
//!
//! A [`Handheld`] on a camera adds a small, smooth, deterministic pose on top of the pose the rig
//! or the shot player wrote, and takes it off again before the next frame (the same restore
//! discipline as the shake sensors, so it never builds up in the pose a rig eases from).
//!
//! # The model
//!
//! - **Sway**: low-amplitude correlated noise on position and rotation. The noise is smooth value
//!   noise (cubic interpolation between hashed lattice values, two octaves), the standard
//!   construction from the public literature on gradient and value noise (Perlin, "An Image
//!   Synthesizer", SIGGRAPH 1985; the lattice-hash variant is described in many public texts).
//!   The lattice values are a pure function of the seed, the channel and the cell, so a replay
//!   from the same seed and the same time steps gives bit-identical poses.
//! - **Breathing**: a slow sinusoid in height and a small pitch, at about 0.22 Hz (13 breaths per
//!   minute).
//! - **Walk bob**: the step phase is the horizontal distance the camera travelled divided by
//!   [`HandheldProfile::stride_m`], so the bob follows footfalls and not the clock: a faster walk
//!   steps faster, standing still does not bob, and a footfall lands at each whole step. The head
//!   is lowest at the footfall, and sways sideways and rolls with a period of two steps. The bob
//!   fades in between [`HandheldProfile::walk_speed_min`] and [`HandheldProfile::walk_speed_full`].
//! - **Intensity and steady**: [`HandheldProfile::intensity`] scales every term;
//!   [`HandheldProfile::steady`] is a stabilized mount (sway and bob reduced, no roll).
//!
//! The pose is a pure function of the profile, the time and the step phase
//! ([`HandheldProfile::sample`]); the component and its system only feed it.

use regia_app::prelude::*;
use regia_core::math::{Quat, Vec3, det_quat_from_yaw_pitch_roll, det_sin_cos};
use regia_scene::Transform;

use super::{CameraSet, sensor::ShakeRestoreSet};

/// Largest intensity that is honored; above it the value is clamped.
pub const MAX_INTENSITY: f32 = 4.0;
/// Sway and bob scale of the steady option.
pub const STEADY_SCALE: f32 = 0.2;
/// Roll of the steady option relative to the unsteady roll (none).
const STEADY_ROLL: f32 = 0.0;

/// The look of a handheld camera. Distances are metres, angles degrees, rates hertz.
///
/// # Example
///
/// ```
/// use regia_framework::camera::handheld::HandheldProfile;
///
/// let profile = HandheldProfile::default();
/// let standing = profile.sample(1.0, 0.0, 0.0);
/// let walking = profile.sample(1.0, 0.5, 1.0);
/// assert!(walking.offset.length() > standing.offset.length());
/// // The same inputs give the same pose.
/// assert_eq!(profile.sample(1.0, 0.5, 1.0), walking);
/// ```
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Default, Clone)]
pub struct HandheldProfile {
    /// Scales every term; `1` is the default look, `0` is off. Negative or NaN reads as `0`,
    /// values above [`MAX_INTENSITY`] are clamped.
    pub intensity: f32,
    /// A stabilized mount: sway and bob at [`STEADY_SCALE`], no roll. Breathing stays.
    pub steady: bool,
    /// Seed of the sway noise.
    pub seed: u64,
    /// Sway position amplitude, metres.
    pub sway_position_m: f32,
    /// Sway rotation amplitude (pitch, yaw, roll), degrees.
    pub sway_rotation_deg: f32,
    /// Sway frequency, hertz (how fast the noise moves).
    pub sway_hz: f32,
    /// Breathing rate, hertz.
    pub breath_hz: f32,
    /// Breathing height amplitude, metres.
    pub breath_position_m: f32,
    /// Breathing pitch amplitude, degrees.
    pub breath_pitch_deg: f32,
    /// Distance covered by one step, metres.
    pub stride_m: f32,
    /// Walk bob height amplitude (peak to peak), metres.
    pub bob_vertical_m: f32,
    /// Walk bob sideways amplitude, metres.
    pub bob_lateral_m: f32,
    /// Walk bob roll amplitude, degrees.
    pub bob_roll_deg: f32,
    /// Walk bob pitch amplitude, degrees.
    pub bob_pitch_deg: f32,
    /// Horizontal speed below which there is no bob, metres per second.
    pub walk_speed_min: f32,
    /// Horizontal speed from which the bob is at full strength, metres per second.
    pub walk_speed_full: f32,
}

impl Default for HandheldProfile {
    fn default() -> Self {
        Self {
            intensity: 1.0,
            steady: false,
            seed: 0x4e41_4d45,
            sway_position_m: 0.006,
            sway_rotation_deg: 0.35,
            sway_hz: 0.4,
            breath_hz: 0.22,
            breath_position_m: 0.004,
            breath_pitch_deg: 0.15,
            stride_m: 0.78,
            bob_vertical_m: 0.025,
            bob_lateral_m: 0.012,
            bob_roll_deg: 0.35,
            bob_pitch_deg: 0.2,
            walk_speed_min: 0.3,
            walk_speed_full: 1.5,
        }
    }
}

/// The pose a handheld camera adds, in the camera's own axes (x right, y up, z back).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct HandheldPose {
    /// Position offset, metres, camera axes.
    pub offset: Vec3,
    /// Pitch, radians (positive looks up).
    pub pitch: f32,
    /// Yaw, radians (positive looks left).
    pub yaw: f32,
    /// Roll, radians.
    pub roll: f32,
}

impl HandheldPose {
    /// The rotation to multiply onto the camera's: yaw, then pitch, then roll, from the pinned
    /// [`det_quat_from_yaw_pitch_roll`] euler construction.
    pub fn rotation(&self) -> Quat {
        det_quat_from_yaw_pitch_roll(self.yaw, self.pitch, self.roll)
    }

    /// Whether the pose adds nothing.
    pub fn is_idle(&self) -> bool {
        self.offset == Vec3::ZERO && self.pitch == 0.0 && self.yaw == 0.0 && self.roll == 0.0
    }
}

/// splitmix64 finalizer: a well-mixed 64-bit hash of `x`.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

/// The lattice value at `cell` of `channel`, in `[-1, 1]`.
fn lattice(seed: u64, channel: u32, cell: i64) -> f32 {
    let h = mix(seed ^ mix(u64::from(channel).wrapping_add(0x1000)) ^ mix(cell as u64));
    // 24 bits of the hash: exact in f32.
    ((h >> 40) as f32 / 8_388_607.5) - 1.0
}

/// Smooth value noise in `[-1, 1]` at `t` (cells are one unit long).
fn value_noise(seed: u64, channel: u32, t: f32) -> f32 {
    let floor = t.floor();
    let cell = floor as i64;
    let f = t - floor;
    // Smootherstep: zero first and second derivative at the lattice, so the signal is smooth.
    let u = f * f * f * (f * (f * 6.0 - 15.0) + 10.0);
    let a = lattice(seed, channel, cell);
    let b = lattice(seed, channel, cell.wrapping_add(1));
    a + (b - a) * u
}

/// Two octaves of value noise, normalized to `[-1, 1]`.
fn fractal(seed: u64, channel: u32, t: f32) -> f32 {
    (0.65 * value_noise(seed, channel, t)
        + 0.35 * value_noise(seed, channel.wrapping_add(64), t * 2.17 + 11.3))
    .clamp(-1.0, 1.0)
}

fn smoothstep(edge0: f32, edge1: f32, x: f32) -> f32 {
    if edge1 <= edge0 {
        return if x >= edge1 { 1.0 } else { 0.0 };
    }
    let t = ((x - edge0) / (edge1 - edge0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl HandheldProfile {
    /// A walking first-person look at the default intensity.
    pub fn walking() -> Self {
        Self::default()
    }

    /// A stabilized mount: sway and bob reduced, no roll.
    pub fn steady_mount() -> Self {
        Self {
            steady: true,
            ..Self::default()
        }
    }

    /// The intensity actually used: negative and NaN read as `0`, clamped to [`MAX_INTENSITY`].
    pub fn effective_intensity(&self) -> f32 {
        if self.intensity.is_finite() {
            self.intensity.clamp(0.0, MAX_INTENSITY)
        } else {
            0.0
        }
    }

    /// The pose `time_s` seconds in, with the walk at step phase `step_phase` (one unit per
    /// step, a footfall at each whole number) and `walk_weight` (`0..=1`, how much of the bob
    /// shows). Pure and deterministic.
    pub fn sample(&self, time_s: f32, step_phase: f32, walk_weight: f32) -> HandheldPose {
        let intensity = self.effective_intensity();
        if intensity == 0.0 || !time_s.is_finite() || !step_phase.is_finite() {
            return HandheldPose::default();
        }
        let weight = if walk_weight.is_finite() {
            walk_weight.clamp(0.0, 1.0)
        } else {
            0.0
        };
        let (sway_scale, bob_scale, roll_scale) = if self.steady {
            (STEADY_SCALE, STEADY_SCALE, STEADY_ROLL)
        } else {
            (1.0, 1.0, 1.0)
        };
        let t = time_s * self.sway_hz.max(0.0);
        let sway = |channel: u32| fractal(self.seed, channel, t);
        let deg = f32::to_radians;
        let sp = self.sway_position_m * sway_scale;
        let sr = deg(self.sway_rotation_deg) * sway_scale;
        let mut offset = Vec3::new(sway(1) * sp, sway(2) * sp, sway(3) * sp * 0.5);
        let mut pitch = sway(4) * sr;
        let yaw = sway(5) * sr;
        let mut roll = sway(6) * sr * 0.5 * roll_scale;

        // Breathing. The sinusoid is the pinned `det_sin_cos` series, so the pose is bit-identical
        // across platforms for the same seed and time.
        let (breath, _) = det_sin_cos(std::f32::consts::TAU * self.breath_hz.max(0.0) * time_s);
        offset.y += breath * self.breath_position_m;
        pitch += breath * deg(self.breath_pitch_deg);

        // Walk bob. The head is lowest at the footfall (phase = whole number), and the sideways
        // sway and the roll have a period of two steps.
        let step = std::f32::consts::TAU * step_phase;
        let (half_sin, _) = det_sin_cos(step * 0.5);
        let (_, step_cos) = det_sin_cos(step);
        let bs = bob_scale * weight;
        offset.y -= 0.5 * self.bob_vertical_m * bs * step_cos;
        offset.x += self.bob_lateral_m * bs * half_sin;
        roll += deg(self.bob_roll_deg) * bs * roll_scale * half_sin;
        pitch += deg(self.bob_pitch_deg) * bs * (-step_cos) * 0.5;

        HandheldPose {
            offset: offset * intensity,
            pitch: pitch * intensity,
            yaw: yaw * intensity,
            roll: roll * intensity,
        }
    }

    /// How much of the walk bob shows at horizontal speed `speed_m_s`.
    pub fn walk_weight(&self, speed_m_s: f32) -> f32 {
        if speed_m_s.is_finite() {
            smoothstep(
                self.walk_speed_min,
                self.walk_speed_full.max(self.walk_speed_min),
                speed_m_s,
            )
        } else {
            0.0
        }
    }
}

/// The pose written last frame, to take off again.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Applied {
    base_translation: Vec3,
    base_rotation: Quat,
    written_translation: Vec3,
    written_rotation: Quat,
}

/// A first-person handheld camera: add it to a camera to give it head sway, breathing and a walk
/// bob that follows the camera's own horizontal travel (see the module docs).
///
/// # Example
///
/// ```
/// use regia_framework::camera::handheld::{Handheld, HandheldProfile};
///
/// let mut handheld = Handheld::new(HandheldProfile::steady_mount());
/// assert!(handheld.profile.steady);
/// assert_eq!(handheld.footfalls(), 0);
/// handheld.profile.intensity = 0.5;
/// ```
#[derive(Component, Debug, Clone, PartialEq)]
pub struct Handheld {
    /// The look.
    pub profile: HandheldProfile,
    time_s: f32,
    distance_m: f32,
    footfalls: u64,
    last_base: Option<Vec3>,
    pose: HandheldPose,
    applied: Option<Applied>,
}

impl Default for Handheld {
    fn default() -> Self {
        Self::new(HandheldProfile::default())
    }
}

impl Handheld {
    /// A handheld camera with `profile`.
    pub fn new(profile: HandheldProfile) -> Self {
        Self {
            profile,
            time_s: 0.0,
            distance_m: 0.0,
            footfalls: 0,
            last_base: None,
            pose: HandheldPose::default(),
            applied: None,
        }
    }

    /// Footfalls so far: whole steps of the walk phase. Audio can follow this counter.
    pub fn footfalls(&self) -> u64 {
        self.footfalls
    }

    /// The step phase now (one unit per step).
    pub fn step_phase(&self) -> f32 {
        self.distance_m / self.profile.stride_m.max(0.05)
    }

    /// Seconds of handheld time so far.
    pub fn time_s(&self) -> f32 {
        self.time_s
    }

    /// The pose added this frame.
    pub fn pose(&self) -> HandheldPose {
        self.pose
    }

    /// Advances by `dt` seconds with the camera at `base` (its pose before this module adds
    /// anything), and returns the pose to add. A jump longer than `MAX_TELEPORT_M` in one step
    /// is a teleport: it does not count as walking.
    pub fn advance(&mut self, dt: f32, base: Vec3) -> HandheldPose {
        let dt = if dt.is_finite() { dt.max(0.0) } else { 0.0 };
        self.time_s += dt;
        let mut speed = 0.0;
        if let Some(last) = self.last_base
            && dt > 0.0
        {
            let flat = Vec3::new(base.x - last.x, 0.0, base.z - last.z);
            let step = flat.length();
            if step.is_finite() && step <= MAX_TELEPORT_M {
                speed = step / dt;
                self.distance_m += step;
            }
        }
        self.last_base = Some(base);
        let phase = self.step_phase();
        self.footfalls = phase.floor().max(0.0) as u64;
        let weight = self.profile.walk_weight(speed);
        self.pose = self.profile.sample(self.time_s, phase, weight);
        self.pose
    }

    /// Scales the pose [`Self::advance`] stored. `1` leaves it. Reduced motion passes `0`.
    fn scale_stored_pose(&mut self, scale: f32) {
        if scale == 1.0 {
            return;
        }
        let scale = if scale.is_finite() { scale } else { 0.0 };
        self.pose.offset *= scale;
        self.pose.pitch *= scale;
        self.pose.yaw *= scale;
        self.pose.roll *= scale;
    }
}

/// A single-step jump longer than this is treated as a teleport and not counted as walking.
pub const MAX_TELEPORT_M: f32 = 5.0;

impl Handheld {
    /// Takes the pose written last frame off `transform`, when the camera still holds it (a
    /// camera something else moved since keeps the new pose: that is its new base). Call it
    /// before removing the component, or the last pose stays on the camera.
    pub fn restore(&mut self, transform: &mut Transform) {
        let Some(applied) = self.applied.take() else {
            return;
        };
        if transform.translation == applied.written_translation
            && transform.rotation == applied.written_rotation
        {
            transform.translation = applied.base_translation;
            transform.rotation = applied.base_rotation;
        }
    }
}

/// `ShakeRestoreSet`: takes last frame's handheld pose off every camera that still holds it.
fn restore_handheld(mut cameras: Query<(&mut Transform, &mut Handheld)>) {
    for (mut transform, mut handheld) in &mut cameras {
        if handheld.applied.is_some() {
            handheld.restore(&mut transform);
        }
    }
}

/// `CameraSet::Director`, before the shakes: advances every handheld and adds its pose.
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_handheld")]
fn apply_handheld(
    time: Res<Time>,
    motion: Option<Res<crate::settings::SceneMotion>>,
    mut cameras: Query<(&mut Transform, &mut Handheld)>,
) {
    let dt = time.delta_secs();
    let bob_scale = motion.map(|motion| motion.bob_scale).unwrap_or(1.0);
    for (mut transform, mut handheld) in &mut cameras {
        let base_translation = transform.translation;
        let base_rotation = transform.rotation;
        handheld.advance(dt, base_translation);
        if bob_scale != 1.0 {
            handheld.scale_stored_pose(bob_scale);
        }
        let pose = handheld.pose();
        if pose.is_idle() {
            handheld.applied = None;
            continue;
        }
        transform.translation = base_translation + base_rotation * pose.offset;
        transform.rotation = base_rotation * pose.rotation();
        handheld.applied = Some(Applied {
            base_translation,
            base_rotation,
            written_translation: transform.translation,
            written_rotation: transform.rotation,
        });
    }
}

/// Registers the component and systems (called by [`CameraPlugin`](super::CameraPlugin)).
pub(super) fn install(app: &mut App) {
    app.add_systems(
        PostUpdate,
        // The shake restore runs first (it holds the shaken pose), then this one.
        restore_handheld
            .in_set(ShakeRestoreSet)
            .after(super::sensor::restore_unshaken),
    );
    app.add_systems(
        PostUpdate,
        apply_handheld
            .in_set(CameraSet::Director)
            .before(super::sensor::apply_shakes)
            .before(PostUpdateSet::TransformPropagate),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(profile: HandheldProfile, steps: u32, speed: f32) -> Vec<HandheldPose> {
        let mut handheld = Handheld::new(profile);
        let mut x = 0.0;
        let mut out = Vec::new();
        for _ in 0..steps {
            x += speed / 60.0;
            out.push(handheld.advance(1.0 / 60.0, Vec3::new(x, 1.7, 0.0)));
        }
        out
    }

    #[test]
    fn the_same_seed_gives_the_same_poses_and_another_seed_does_not() {
        let a = run(HandheldProfile::default(), 600, 1.4);
        let b = run(HandheldProfile::default(), 600, 1.4);
        assert_eq!(a, b);
        let c = run(
            HandheldProfile {
                seed: 7,
                ..HandheldProfile::default()
            },
            600,
            1.4,
        );
        assert_ne!(a, c);
    }

    #[test]
    fn standing_still_sways_and_breathes_but_does_not_bob() {
        let poses = run(HandheldProfile::default(), 1200, 0.0);
        let profile = HandheldProfile::default();
        let max_off = poses.iter().map(|p| p.offset.length()).fold(0.0_f32, f32::max);
        assert!(max_off > 0.0005, "moves: {max_off}");
        // Sway + breathing amplitude bound, far below a bob.
        assert!(
            max_off < 3.0 * (profile.sway_position_m + profile.breath_position_m),
            "{max_off}"
        );
        let mut h = Handheld::default();
        for _ in 0..600 {
            h.advance(1.0 / 60.0, Vec3::ZERO);
        }
        assert_eq!(h.footfalls(), 0);
    }

    #[test]
    fn sway_is_low_amplitude_and_smooth() {
        let poses = run(HandheldProfile::default(), 3600, 0.0);
        let max_angle = poses
            .iter()
            .map(|p| p.pitch.abs().max(p.yaw.abs()))
            .fold(0.0_f32, f32::max);
        // Sway plus breathing stay under one degree.
        assert!(max_angle < 1.0_f32.to_radians(), "{max_angle}");
        // No frame-to-frame jump above a millimetre: the noise is correlated, not white.
        let max_step = poses
            .windows(2)
            .map(|w| (w[1].offset - w[0].offset).length())
            .fold(0.0_f32, f32::max);
        assert!(max_step < 0.001, "{max_step}");
    }

    #[test]
    fn footfalls_follow_distance_not_time() {
        let profile = HandheldProfile::default();
        let mut slow = Handheld::new(profile);
        let mut fast = Handheld::new(profile);
        let (mut xs, mut xf) = (0.0, 0.0);
        for _ in 0..600 {
            xs += 1.0 / 60.0;
            xf += 2.0 / 60.0;
            slow.advance(1.0 / 60.0, Vec3::new(xs, 0.0, 0.0));
            fast.advance(1.0 / 60.0, Vec3::new(xf, 0.0, 0.0));
        }
        // 10 s at 1 m/s and at 2 m/s with a 0.78 m stride: 10 / 0.78 = 12 and 20 / 0.78 = 25.
        assert_eq!(slow.footfalls(), 12);
        assert_eq!(fast.footfalls(), 25);
        // The same distance in half the time gives the same step count.
        let mut quick = Handheld::new(profile);
        let mut x = 0.0;
        for _ in 0..300 {
            x += 2.0 / 60.0;
            quick.advance(1.0 / 60.0, Vec3::new(x, 0.0, 0.0));
        }
        assert_eq!(quick.footfalls(), slow.footfalls());
    }

    #[test]
    fn the_head_is_lowest_at_a_footfall() {
        let profile = HandheldProfile {
            sway_position_m: 0.0,
            sway_rotation_deg: 0.0,
            breath_position_m: 0.0,
            breath_pitch_deg: 0.0,
            ..HandheldProfile::default()
        };
        let at_foot = profile.sample(1.0, 3.0, 1.0).offset.y;
        let mid_step = profile.sample(1.0, 3.5, 1.0).offset.y;
        assert!(at_foot < mid_step, "{at_foot} {mid_step}");
        assert!((at_foot + 0.5 * profile.bob_vertical_m).abs() < 1e-6);
    }

    #[test]
    fn slow_drift_does_not_bob() {
        let profile = HandheldProfile::default();
        assert_eq!(profile.walk_weight(0.1), 0.0);
        assert_eq!(profile.walk_weight(2.0), 1.0);
        let mid = profile.walk_weight(0.9);
        assert!(mid > 0.0 && mid < 1.0);
        assert_eq!(profile.walk_weight(f32::NAN), 0.0);
    }

    #[test]
    fn steady_reduces_the_motion_and_removes_roll() {
        let normal = run(HandheldProfile::default(), 1200, 1.4);
        let steady = run(HandheldProfile::steady_mount(), 1200, 1.4);
        let peak = |poses: &[HandheldPose]| poses.iter().map(|p| p.offset.length()).fold(0.0_f32, f32::max);
        assert!(
            peak(&steady) < 0.5 * peak(&normal),
            "{} {}",
            peak(&steady),
            peak(&normal)
        );
        let roll = |poses: &[HandheldPose]| poses.iter().map(|p| p.roll.abs()).fold(0.0_f32, f32::max);
        assert_eq!(roll(&steady), 0.0);
        assert!(roll(&normal) > 0.0);
    }

    #[test]
    fn intensity_scales_and_zero_is_off() {
        let base = HandheldProfile::default().sample(2.0, 0.3, 1.0);
        let half = HandheldProfile {
            intensity: 0.5,
            ..HandheldProfile::default()
        }
        .sample(2.0, 0.3, 1.0);
        assert!((half.offset - base.offset * 0.5).length() < 1e-7);
        for bad in [0.0, -1.0, f32::NAN, f32::INFINITY * 0.0] {
            let off = HandheldProfile {
                intensity: bad,
                ..HandheldProfile::default()
            };
            assert!(off.sample(2.0, 0.3, 1.0).is_idle(), "{bad}");
        }
        let huge = HandheldProfile {
            intensity: 1.0e9,
            ..HandheldProfile::default()
        };
        assert_eq!(huge.effective_intensity(), MAX_INTENSITY);
    }

    #[test]
    fn a_teleport_is_not_walking() {
        let mut h = Handheld::default();
        h.advance(1.0 / 60.0, Vec3::ZERO);
        h.advance(1.0 / 60.0, Vec3::new(500.0, 0.0, 0.0));
        assert_eq!(h.footfalls(), 0);
        assert_eq!(h.step_phase(), 0.0);
    }

    #[test]
    fn the_noise_is_bounded_and_not_constant() {
        let mut lo = f32::MAX;
        let mut hi = f32::MIN;
        for i in 0..2000 {
            let v = fractal(1, 1, i as f32 * 0.05);
            assert!((-1.0..=1.0).contains(&v));
            lo = lo.min(v);
            hi = hi.max(v);
        }
        assert!(hi - lo > 0.8, "{lo} {hi}");
    }

    /// Pin: the default look. A change here changes every handheld clip.
    #[test]
    fn default_profile_pin() {
        let p = HandheldProfile::default();
        assert_eq!(p.intensity, 1.0);
        assert!(!p.steady);
        assert_eq!(
            (p.sway_position_m, p.sway_rotation_deg, p.sway_hz),
            (0.006, 0.35, 0.4)
        );
        assert_eq!((p.stride_m, p.bob_vertical_m), (0.78, 0.025));
    }

    /// Pin: the default look's pose bits for a fixed input, so a change to the deterministic
    /// series (or a regression to the platform libm) is caught on every platform.
    #[test]
    fn default_profile_sample_is_pinned_across_platforms() {
        let p = HandheldProfile::default();
        let pose = p.sample(1.0, 0.5, 1.0);
        assert_eq!(pose.offset.x.to_bits(), 0x3c64_47f4);
        assert_eq!(pose.offset.y.to_bits(), 0x3c8c_8924);
        assert_eq!(pose.offset.z.to_bits(), 0x39fe_cefe);
        assert_eq!(pose.pitch.to_bits(), 0x39b9_ba68);
        assert_eq!(pose.yaw.to_bits(), 0x3b2a_c884);
        assert_eq!(pose.roll.to_bits(), 0x3bbc_2291);
        // Still the same documented behavior: a walking pose moves more than a standing one.
        let standing = p.sample(1.0, 0.0, 0.0);
        assert!(pose.offset.length() > standing.offset.length());
    }

    mod app_tests {
        use regia_app::enable_fixed_step;

        use super::super::*;
        use crate::camera::CameraPlugin;

        fn app() -> App {
            let mut app = App::new();
            app.add_plugins((MinimalPlugins, CameraPlugin));
            enable_fixed_step(&mut app, 60.0);
            app
        }

        fn translation(app: &App, e: Entity) -> Vec3 {
            app.world().get::<Transform>(e).expect("transform").translation
        }

        #[test]
        fn the_system_adds_the_pose_and_never_accumulates_it() {
            let mut app = app();
            let base = Vec3::new(2.0, 1.7, -3.0);
            let cam = app
                .world_mut()
                .spawn((Transform::from_translation(base), Handheld::default()))
                .id();
            let mut moved = 0.0_f32;
            for _ in 0..600 {
                app.update();
                let d = (translation(&app, cam) - base).length();
                moved = moved.max(d);
                // Sway plus breathing stay within a centimetre however long it runs.
                assert!(d < 0.02, "{d}");
            }
            assert!(moved > 0.0005, "the camera never moved: {moved}");
        }

        #[test]
        fn a_walking_camera_counts_footfalls() {
            let mut app = app();
            let cam = app
                .world_mut()
                .spawn((Transform::from_translation(Vec3::ZERO), Handheld::default()))
                .id();
            let mut x = 0.0;
            for _ in 0..300 {
                x += 1.4 / 60.0;
                app.world_mut().get_mut::<Transform>(cam).expect("t").translation = Vec3::new(x, 1.7, 0.0);
                app.update();
            }
            let steps = app.world().get::<Handheld>(cam).expect("handheld").footfalls();
            // 5 s at 1.4 m/s is 7 m: 7 / 0.78 = 8 steps.
            assert!((8..=9).contains(&steps), "{steps}");
        }

        #[test]
        fn restore_before_removal_leaves_the_base_pose() {
            let mut app = app();
            let base = Vec3::new(0.0, 1.7, 0.0);
            let cam = app
                .world_mut()
                .spawn((Transform::from_translation(base), Handheld::default()))
                .id();
            for _ in 0..90 {
                app.update();
            }
            assert_ne!(translation(&app, cam), base);
            let mut entity = app.world_mut().entity_mut(cam);
            let mut handheld = entity.take::<Handheld>().expect("handheld");
            let mut transform = entity.get_mut::<Transform>().expect("t");
            handheld.restore(&mut transform);
            assert_eq!(transform.translation, base);
        }

        #[test]
        fn reduced_motion_scale_stops_handheld_sway_and_bob() {
            let mut app = app();
            app.insert_resource(crate::settings::SceneMotion {
                shake_scale: 0.0,
                bob_scale: 0.0,
                fov_override_deg: None,
            });
            let cam = app
                .world_mut()
                .spawn((Transform::from_translation(Vec3::ZERO), Handheld::default()))
                .id();
            let mut x = 0.0;
            for _ in 0..120 {
                x += 1.4 / 60.0;
                let base = Vec3::new(x, 1.7, 0.0);
                {
                    let mut transform = app.world_mut().get_mut::<Transform>(cam).expect("transform");
                    transform.translation = base;
                    transform.rotation = Quat::IDENTITY;
                }
                app.update();
                let transform = app.world().get::<Transform>(cam).expect("transform");
                assert_eq!(transform.translation, base);
                assert_eq!(transform.rotation, Quat::IDENTITY);
            }
        }
    }
}
