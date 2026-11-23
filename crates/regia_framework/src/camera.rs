// SPDX-License-Identifier: MPL-2.0
//! Cameras (`docs/specs/14-scene-and-framework.md` § B5).
//!
//! A [`CameraRig`] holds the active camera mode plus the orbit angles the input layer drives;
//! [`ChaseCamera`] links a camera entity to the entity it follows. [`CameraPlugin`] updates every
//! rig each frame in `PostUpdate` (after late scripts, before transform propagation), computing the
//! camera's desired pose from the mode and the target's [`Transform`], then easing towards it with
//! exponential lag, `current = lerp(current, desired, 1 − exp(−speed · Δt))`.
//!
//! # Modes (M9: what Direct Order 27 needs, `specs/25` § 4)
//!
//! - [`CameraMode::ThirdPerson`] — the chase/boom camera with position and rotation lag **and
//!   lean**: the camera rolls into turns in proportion to the rig's yaw velocity
//!   ("pilot chase with lag/lean").
//! - [`CameraMode::Overhead`] — the satellite camera: straight down (or tilted) above the target,
//!   north-up, with soft follow.
//! - [`CameraMode::FirstPerson`] — the pilot eye: welded to the target at eye height; rig yaw and
//!   pitch are look offsets.
//! - [`CameraMode::Free`] — the spectator/free camera: no target, flown directly by
//!   [`FreeLookInput`] (move, look, rise, sprint).
//!
//! `Fixed`, `Orbit` and `Spline` (§ B5) are not separate modes: inspection is [`CameraMode::Free`]
//! and a locked shot is a [`CameraMode::ThirdPerson`] whose lag speeds are zero. A camera that rides
//! a spline is the `Rail` node of a data-driven camera rig ([`rail`]), and a crane arm is its
//! `Crane` node ([`crane`]); a sequence keys both ([`rail_keys`]; ADR-3231). From M20 the
//! third-person boom shortens on collision: a sphere cast from the pivot along the boom, with
//! the eased length stored on [`CameraRuntime::boom`].
//!
//! # 2D: the ortho pixel-perfect mode
//!
//! [`ortho::Camera2d`] (with [`ortho::Camera2dPlugin`]) is the orthographic camera of the 2D
//! toolkit: pixels per unit, an optional pixel-perfect virtual resolution with integer scaling
//! and letterbox bars, snapping, follow with deadzone and lag, and screen ↔ world conversions
//! (`docs/specs/22-tier2.md` § T2.10).
//!
//! # Director
//!
//! [`CameraDirector`] adds transient effects on top of any rig: camera shakes (smooth value noise,
//! decaying) and an FOV kick (stored and decayed; `fov_kick_value` is what a game reads). The
//! camera driver writes the rig's `fov_y` and does not add the kick. DO27's event router maps
//! sim events to shakes (`specs/25` § 3).
//!
//! Which cameras feel a shake is up to their [`sensor::ShakeSensor`] (ADR-1619): a channel mask,
//! a gain, add or strongest, camera or world axes, and a clock. [`sensor::ShakeEmit`] messages
//! carry shakes with a position, a falloff and a travel speed, and [`sensor::ContactShake`] bodies
//! emit them on impact. Rig cameras and the shot player's camera feel the director by default; a
//! plain `Camera` (UI, picture-in-picture) shakes only with a sensor. The shake is added after the
//! rigs and the shot player have moved the camera, so it shows under a playing shot.
//!
//! # Coordinates
//!
//! Regia Engine is right-handed, +Y up, forward −Z, in metres (`docs/02-architecture.md` § 3). Yaw
//! rotates about world +Y (0 faces −Z; positive turns towards −X) and pitch is positive looking
//! up. With yaw 0, pitch 0 and zero offsets the camera sits `boom_length` metres towards +Z of
//! the target — behind it — looking towards −Z, at the target.

use std::f32::consts::FRAC_PI_2;

use regia_app::prelude::*;
use regia_core::math::{Isometry, Vec2, Vec3, det_exp, det_nlerp, det_quat_from_yaw_pitch_roll};
use regia_scene::Transform;

use crate::ground_shock::{MountShock, ShakePose};

pub mod asset;
pub mod blend;
pub mod crane;
pub mod debug;
pub mod director;
pub mod driver;
pub mod handheld;
pub mod load;
pub mod nodes;
pub mod ortho;
pub mod photographic;
pub mod presets;
pub mod rail;
pub mod rail_keys;
#[cfg(test)]
mod rail_tests;
pub mod runtime;
pub mod sensor;
pub mod walk_bob;

// ---------------------------------------------------------------------------
// System sets
// ---------------------------------------------------------------------------

/// Orders camera work inside [`PostUpdate`]: rigs update first, cinematic drivers (shot and
/// sequence players) write their poses second, director effects come last.
/// [`sensor::ShakeRestoreSet`] runs before all three; the shot player
/// ([`ShotDriveSet`](crate::shots::ShotDriveSet)) is in [`CameraSet::Cinematic`].
#[derive(SystemSet, Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CameraSet {
    /// Rig poses are computed (after [`PostUpdateSet::LateScripts`]).
    Update,
    /// Shot and sequence players write cinematic poses over the rigs (ADR-1614).
    Cinematic,
    /// Director effects (shakes) are applied on top of the rig or cinematic poses, after the
    /// shot player has written its pose (ADR-1619).
    Director,
}

// ---------------------------------------------------------------------------
// Components
// ---------------------------------------------------------------------------

/// Parameters of the third-person boom mode (§ B5) with lag and lean.
///
/// The camera hangs on the end of a rigid *boom*. The boom's pivot is the target's translation
/// plus [`socket_offset`](Self::socket_offset) rotated into world space, and the boom extends
/// [`boom_length`](Self::boom_length) metres back along the view direction given by the rig's
/// yaw and pitch. The camera aims at the *look-at point*: the target's translation plus
/// [`target_offset`](Self::target_offset) rotated into world space. With zero offsets the pivot
/// and the look-at point are the target itself, so the camera looks straight along its boom.
///
/// Lean: the camera rolls around its forward axis in proportion to how fast the rig's yaw
/// changes (`lean_gain`, radians of roll per radian/second of yaw, clamped to `max_lean`), then
/// relaxes with `lean_speed`. `lean_gain: 0` disables the effect.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct ThirdPerson {
    /// Distance in metres from the boom's pivot to the camera, along the view direction.
    pub boom_length: f32,
    /// Where the boom attaches, in the target's local space (added before the boom). For example
    /// `(0, 1.5, 0)` pivots the boom at head height.
    pub socket_offset: Vec3,
    /// The point the camera aims at, in the target's local space. For example `(0, 0.8, 0)`
    /// frames the target's chest.
    pub target_offset: Vec3,
    /// Exponential smoothing of the camera position: larger is snappier. `0` freezes the
    /// position; a very large value snaps to the desired position within one frame.
    pub lag_speed: f32,
    /// Exponential smoothing of the camera rotation, in the same units as
    /// [`lag_speed`](Self::lag_speed).
    pub rotation_lag: f32,
    /// Lowest allowed pitch in radians (looking down). Stored back clamped every frame.
    pub min_pitch: f32,
    /// Highest allowed pitch in radians (looking up). Stored back clamped every frame.
    pub max_pitch: f32,
    /// Maximum roll in radians the lean effect may apply (0 disables lean).
    pub max_lean: f32,
    /// Radians of roll per radian/second of yaw velocity (sign flips the lean direction).
    pub lean_gain: f32,
    /// How quickly the lean relaxes, in the same units as
    /// [`lag_speed`](Self::lag_speed).
    pub lean_speed: f32,
    /// Radius in metres of the sphere-cast probe that keeps the camera out of geometry
    /// (§ B5; 0 disables the probe). Requires `regia_physics::PhysicsPlugin` — without a
    /// physics world the boom never shortens.
    pub probe_radius: f32,
}

impl Default for ThirdPerson {
    fn default() -> Self {
        Self {
            boom_length: 5.0,
            socket_offset: Vec3::ZERO,
            target_offset: Vec3::ZERO,
            lag_speed: 10.0,
            rotation_lag: 10.0,
            min_pitch: -80.0_f32.to_radians(),
            max_pitch: 80.0_f32.to_radians(),
            max_lean: 0.18,
            lean_gain: 0.12,
            lean_speed: 6.0,
            probe_radius: 0.2,
        }
    }
}

/// Parameters of the satellite/overhead mode (§ B5 `TopDown`, DO27 "satellite overhead").
///
/// The camera hangs `height` metres above the target along the view direction given by the rig's
/// yaw and `pitch` (default straight down), aiming at the target. The rig's yaw spins the
/// satellite's orientation; the world stays north-up for the default yaw.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct Overhead {
    /// Distance in metres above the target along the view direction.
    pub height: f32,
    /// View pitch in radians; the default looks straight down. Shallower angles tilt the orbit.
    pub pitch: f32,
    /// Exponential smoothing of the position (see [`ThirdPerson::lag_speed`]); rotation follows
    /// the aim directly.
    pub lag_speed: f32,
}

impl Default for Overhead {
    fn default() -> Self {
        Self {
            height: 20.0,
            pitch: -std::f32::consts::FRAC_PI_2,
            lag_speed: 5.0,
        }
    }
}

/// Parameters of the first-person eye mode (§ B5).
///
/// The camera is welded to the target: at `eye_offset` (target-local) with the target's rotation
/// composed with the rig's yaw/pitch look offsets. No lag — the eye is rigid unless it walks:
/// `bob` above zero, or a [`walk_bob::WalkBob`] on the camera, adds a stride-matched walk bob
/// (ADR-2486).
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct FirstPerson {
    /// Eye position in the target's local space.
    pub eye_offset: Vec3,
    /// Walk bob vertical amplitude in metres (0, the default: no bob). Above zero the eye bobs with
    /// [`walk_bob::WalkBob::from_amplitude`]; a [`walk_bob::WalkBob`] on the camera overrides it.
    pub bob: f32,
}

impl Default for FirstPerson {
    fn default() -> Self {
        Self {
            eye_offset: Vec3::new(0.0, 1.6, 0.0),
            bob: 0.0,
        }
    }
}

/// Parameters of the free/spectator mode (§ B5, DO27 "spectator free camera").
///
/// The rig has no target: it flies itself. Every frame it consumes [`FreeLookInput`] (the game's
/// input layer or the agent protocol write that resource): `look` turns yaw/pitch, `move` flies
/// (x = strafe, y = forward), `rise` flies up/down, `sprint` multiplies the speed. Motion is
/// smoothed with `accel` (exponential, `0` = instant).
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub struct Free {
    /// Cruising speed in metres/second.
    pub speed: f32,
    /// Multiplier applied while sprinting.
    pub sprint_multiplier: f32,
    /// Exponential acceleration (higher = snappier; `0` = instant velocity).
    pub accel: f32,
}

impl Default for Free {
    fn default() -> Self {
        Self {
            speed: 10.0,
            sprint_multiplier: 4.0,
            accel: 10.0,
        }
    }
}

/// What a [`CameraRig`] does.
#[derive(Reflect, Clone, Copy, Debug, PartialEq)]
pub enum CameraMode {
    /// A boom camera that orbits the target and aims at it, with lag and lean.
    ThirdPerson(ThirdPerson),
    /// The satellite camera above the target.
    Overhead(Overhead),
    /// The pilot eye welded to the target.
    FirstPerson(FirstPerson),
    /// The spectator free camera, flown through [`FreeLookInput`].
    Free(Free),
}

impl Default for CameraMode {
    fn default() -> Self {
        CameraMode::ThirdPerson(ThirdPerson::default())
    }
}

impl CameraMode {
    /// A stable name for the mode, used by the agent protocol (`camera.set_mode`).
    pub fn name(&self) -> &'static str {
        match self {
            CameraMode::ThirdPerson(_) => "chase",
            CameraMode::Overhead(_) => "overhead",
            CameraMode::FirstPerson(_) => "first_person",
            CameraMode::Free(_) => "free",
        }
    }

    /// The mode for one of [`CameraMode::name`]'s values, with default parameters.
    /// Unknown names return `None`.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "chase" | "third_person" => Some(CameraMode::ThirdPerson(ThirdPerson::default())),
            "overhead" | "top_down" | "satellite" => Some(CameraMode::Overhead(Overhead::default())),
            "first_person" | "eye" => Some(CameraMode::FirstPerson(FirstPerson::default())),
            "free" | "spectator" => Some(CameraMode::Free(Free::default())),
            _ => None,
        }
    }
}

/// Per-camera rig state: the active mode and the orbit angles the input layer drives.
///
/// The camera system clamps [`pitch`](Self::pitch) into the active mode's pitch range and stores
/// the clamped value back, so input code and inspectors see the effective angle.
///
/// # Example
///
/// ```
/// use regia_framework::camera::{CameraMode, CameraRig};
///
/// let mut rig = CameraRig::default();
/// assert!(matches!(rig.mode, CameraMode::ThirdPerson(_)));
/// rig.yaw = std::f32::consts::FRAC_PI_2; // radians; positive turns towards −X
/// assert_eq!(rig.yaw, std::f32::consts::FRAC_PI_2);
/// ```
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component, Default)]
pub struct CameraRig {
    /// Active camera mode.
    pub mode: CameraMode,
    /// Orbit pitch in radians; positive looks up.
    pub pitch: f32,
    /// Orbit yaw in radians about world +Y; 0 faces −Z, positive turns towards −X.
    pub yaw: f32,
    /// Which pose of a predicted target the camera follows (spec 73 § 6.4): the smoothed pose
    /// (simulated + the correction offset, the default) or the simulated one. A target without a
    /// correction offset has one pose, so this matters only on a networked client.
    #[reflect(default)]
    pub follow: CameraFollow,
}

impl Default for CameraRig {
    fn default() -> Self {
        Self {
            mode: CameraMode::default(),
            pitch: 0.0,
            yaw: 0.0,
            follow: CameraFollow::default(),
        }
    }
}

/// The pose of a predicted target a [`CameraRig`] follows (spec 73 § 6.4, ADR-7213).
#[derive(Reflect, Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CameraFollow {
    /// The rendered pose: the simulated pose plus the target's correction offset
    /// (`regia_net::NetVisualOffset`), so a correction never pops the view.
    #[default]
    Smoothed,
    /// The simulated pose: the camera snaps with each correction (exact aim on the predicted
    /// state, at the cost of a visible pop).
    Simulated,
}

impl CameraFollow {
    /// A stable name: `smoothed` or `simulated` (agent protocol `camera.set_follow`).
    pub fn name(self) -> &'static str {
        match self {
            Self::Smoothed => "smoothed",
            Self::Simulated => "simulated",
        }
    }

    /// The value for one of [`Self::name`]'s names.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "smoothed" => Some(Self::Smoothed),
            "simulated" => Some(Self::Simulated),
            _ => None,
        }
    }
}

/// The correction offset a target may carry (`regia_net` builds only).
#[cfg(feature = "net")]
type FollowOffset = Option<&'static regia_net::NetVisualOffset>;
/// No correction offsets without `regia_net`.
#[cfg(not(feature = "net"))]
type FollowOffset = ();

/// The entities rigs follow: their pose and, with `regia_net`, their correction offset.
type FollowTargets<'w, 's> =
    Query<'w, 's, (&'static Transform, FollowOffset), (Without<CameraRig>, Without<ChaseCamera>)>;

/// The pose of `target` a rig following `follow` aims at.
#[cfg(feature = "net")]
fn followed_pose(
    target: &Transform,
    offset: Option<&regia_net::NetVisualOffset>,
    follow: CameraFollow,
) -> Transform {
    match (follow, offset) {
        (CameraFollow::Smoothed, Some(offset)) => offset.apply(target),
        _ => *target,
    }
}

/// The pose of `target` a rig following `follow` aims at (one pose without `regia_net`).
#[cfg(not(feature = "net"))]
fn followed_pose(target: &Transform, _offset: (), _follow: CameraFollow) -> Transform {
    *target
}

impl CameraRig {
    /// Switches the rig to `mode` (default parameters). Convenience for key binds and the agent
    /// protocol's `camera.set_mode`; the angles carry over so the view does not jump more than
    /// the mode change itself implies.
    pub fn switch_to(&mut self, mode: CameraMode) {
        self.mode = mode;
    }
}

/// Per-frame flight input for [`CameraMode::Free`] rigs. The game's input layer (or the agent
/// protocol) writes it; free cameras consume it in the camera update. `look` is a delta in
/// radians (yaw/pitch), `move` is a normalised axis pair (x = strafe, y = forward), `rise` is
/// vertical (positive = up), `sprint` boosts.
#[derive(Resource, Debug, Clone, Copy, Default, PartialEq)]
pub struct FreeLookInput {
    /// Look delta this frame: x = yaw (positive turns towards −X), y = pitch (positive looks up).
    pub look: Vec2,
    /// Movement axes this frame: x = strafe right, y = forward.
    pub move_axes: Vec2,
    /// Vertical movement (positive = up).
    pub rise: f32,
    /// Sprint boost.
    pub sprint: bool,
}

/// Marks a camera entity as a chase camera and names the entity it follows.
///
/// The camera entity also needs a [`CameraRig`] and a [`Transform`]. Frames whose target does not
/// exist (or has no [`Transform`]) are skipped: the target may be spawned later. A chase camera
/// cannot follow another chase camera. [`CameraMode::Free`] ignores the target.
#[derive(Component, Reflect, Clone, Copy, Debug, PartialEq)]
#[reflect(Component)]
pub struct ChaseCamera {
    /// The entity this camera follows.
    pub target: Entity,
}

/// Runtime smoothing state the camera system maintains per rig (not authored data).
///
/// Auto-inserted on first update; holds the previous yaw for the lean filter, the current lean,
/// the smoothed free-flight velocity, the collision-shortened boom length, and the shake offset
/// and angles added this frame (for reading: the unshaken pose is kept by the camera's
/// [`sensor::ShakeReceiver`], so a shake is never baked into the pose).
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq)]
#[reflect(Component, Default)]
pub struct CameraRuntime {
    /// The rig's yaw on the previous frame, for yaw-velocity lean.
    pub prev_yaw: f32,
    /// The current smoothed lean roll in radians.
    pub lean: f32,
    /// The smoothed free-flight velocity (metres/second), for [`CameraMode::Free`].
    pub velocity: Vec3,
    /// The current effective boom length (metres), including collision shortening. `0` on the
    /// first frame snaps to the desired length instead of easing from the origin.
    pub boom: f32,
    /// The shake offset added this frame (world axes).
    pub shake_offset: Vec3,
    /// The shake roll added this frame (radians).
    pub shake_roll: f32,
    /// The shake pitch and yaw added this frame (radians).
    pub shake_tilt: Vec2,
}

/// Marks a camera whose pose a cinematic player writes (the shot player, a sequence takeover).
/// It feels shakes like a [`CameraRig`] does (a default [`sensor::ShakeSensor`] unless it carries
/// its own), added after the cinematic pose (ADR-1614, ADR-1619). The players add and remove it.
#[derive(Component, Reflect, Debug, Clone, Copy, Default, PartialEq, Eq)]
#[reflect(Component, Default)]
pub struct CinematicCamera;

// ---------------------------------------------------------------------------
// Director
// ---------------------------------------------------------------------------

/// One camera shake request: an offset with amplitude decaying over `duration_secs`.
#[derive(Reflect, Debug, Clone, Copy, PartialEq)]
pub struct Shake {
    /// Position amplitude in metres (and, scaled down, rotational wobble).
    pub amplitude: f32,
    /// Wobble frequency in hertz.
    pub frequency_hz: f32,
    /// Total duration in seconds.
    pub duration_secs: f32,
    /// Amplitude decay exponent: `amplitude · (1 − t)^falloff` (`1` = linear).
    pub falloff: f32,
}

impl Default for Shake {
    fn default() -> Self {
        Self {
            amplitude: 0.3,
            frequency_hz: 8.0,
            duration_secs: 0.5,
            falloff: 1.0,
        }
    }
}

/// Transient camera effects on top of any rig (§ B5 `CameraDirector`): shakes and FOV kick.
///
/// The resource is the shared director; each local player also has one on the
/// [`PlayerManager`](crate::player::PlayerManager) ([`PlayerManager::camera_director_mut`](crate::player::PlayerManager::camera_director_mut)),
/// which blends that player's [`blend::PlayerCamera`] cameras ([`Self::blend_to`], ADR-7203).
/// [`Shake`] requests accumulate; each is smooth value noise on its own seed, decaying to zero.
/// The FOV kick decays exponentially. `fov_kick_value` returns the stored degrees. The camera
/// driver does not add them to the projection.
#[derive(Resource, Debug, Default, Clone)]
pub struct CameraDirector {
    shakes: Vec<(Shake, f32 /* elapsed */, f32 /* offset seed */)>,
    mounts: Vec<(MountShock, f32 /* seconds since arrival */)>,
    fov_kick_degrees: f32,
    /// Shakes added so far: each new shake's seed, so it never repeats a running shake's seed
    /// (the count of running shakes did once one had ended, audit-seqdet-muse L4).
    shakes_added: u32,
    /// The running or holding blend ([`blend`], ADR-7203).
    blend: Option<blend::ActiveBlend>,
}

impl CameraDirector {
    /// Adds a shake.
    pub fn shake(&mut self, params: Shake) {
        // Kept below 2^20 so the seed stays an exact `f32`; 2^20 shakes never run at once.
        let seed = (self.shakes_added % (1 << 20)) as f32 + 1.0;
        self.shakes_added = self.shakes_added.wrapping_add(1);
        self.shakes.push((params, 0.0, seed));
    }

    /// Starts the seeds of new shakes over when no shake is running, so a shake added next plays
    /// the same noise in every render-out job (ADR-2687). A running shake keeps the count going:
    /// a new shake never repeats a running one's seed. Returns whether it restarted.
    pub fn restart_idle_seeds(&mut self) -> bool {
        if !self.shakes.is_empty() {
            return false;
        }
        self.shakes_added = 0;
        true
    }

    /// Adds a damped mount oscillation (a seismic or airblast arrival, see
    /// [`crate::ground_shock`]); it plays from now.
    pub fn mount_shock(&mut self, shock: MountShock) {
        self.mounts.push((shock, 0.0));
    }

    /// Adds `degrees` to the FOV kick (decays exponentially; additive).
    pub fn fov_kick(&mut self, degrees: f32) {
        self.fov_kick_degrees += degrees;
    }

    /// Whether any shake is still running.
    pub fn is_shaking(&self) -> bool {
        !self.shakes.is_empty() || !self.mounts.is_empty()
    }

    /// The current FOV kick in degrees (positive = wider).
    pub fn fov_kick_value(&self) -> f32 {
        self.fov_kick_degrees
    }

    /// Advances every active shake by `delta` seconds and evaluates them: the combined
    /// positional offset and roll (in radians) now. Expired shakes drop out, and the FOV kick
    /// decays. [`CameraPlugin`] calls this once per frame for its rigs; a host that moves its
    /// camera another way calls it itself, once per frame.
    pub fn sample(&mut self, delta: f32) -> (Vec3, f32) {
        let pose = self.sample_pose(delta);
        (pose.offset, pose.roll)
    }

    /// Like [`sample`](Self::sample), and with the rotation about the camera's right and up axes
    /// that mount oscillations add. A frame is sampled once: call this or `sample`, not both.
    pub fn sample_pose(&mut self, delta: f32) -> ShakePose {
        let mut offset = Vec3::ZERO;
        let mut roll = 0.0;
        let mut pitch = 0.0;
        let mut yaw = 0.0;
        self.mounts.retain_mut(|(shock, elapsed)| {
            *elapsed += delta;
            if *elapsed >= shock.duration_s() {
                return false;
            }
            let pose = shock.pose(*elapsed);
            offset += pose.offset;
            pitch += pose.pitch;
            yaw += pose.yaw;
            roll += pose.roll;
            true
        });
        self.shakes.retain_mut(|(params, elapsed, seed)| {
            *elapsed += delta;
            match noise_pose(params, *elapsed, *seed) {
                Some(pose) => {
                    offset += pose.offset;
                    roll += pose.roll;
                    true
                }
                None => false,
            }
        });
        // The pinned decay: host-visible stored state (`fov_kick_value`), so it follows the
        // deterministic series like the rest of the shake envelope (spec 35 § 12.13).
        self.fov_kick_degrees *= det_exp(-4.0 * delta);
        if self.fov_kick_degrees.abs() < 1.0e-3 {
            self.fov_kick_degrees = 0.0;
        }
        ShakePose {
            offset,
            pitch,
            yaw,
            roll,
        }
    }

    /// How many shakes and mount oscillations are running.
    #[must_use]
    pub fn shake_count(&self) -> usize {
        self.shakes.len() + self.mounts.len()
    }

    /// The strongest running effect's envelope now (see [`sensor::ShakeCombine::Max`]); `0` when
    /// idle.
    #[must_use]
    pub fn strength(&self) -> f32 {
        let mounts = self
            .mounts
            .iter()
            .map(|(shock, elapsed)| mount_strength(shock, *elapsed));
        let shakes = self
            .shakes
            .iter()
            .map(|(params, elapsed, _)| noise_strength(params, *elapsed));
        mounts.chain(shakes).fold(0.0, f32::max)
    }
}

/// One noise shake's pose `elapsed` seconds in, or `None` once it has ended: smooth value noise on
/// three axes and the roll, under the envelope `amplitude · (1 − t)^falloff`.
fn noise_pose(params: &Shake, elapsed: f32, seed: f32) -> Option<ShakePose> {
    if elapsed >= params.duration_secs || params.duration_secs <= 0.0 {
        return None;
    }
    let t = elapsed / params.duration_secs;
    let envelope = (1.0 - t).max(0.0).powf(params.falloff.max(0.0));
    let phase = elapsed * params.frequency_hz;
    Some(ShakePose {
        offset: Vec3::new(
            smooth_noise(seed, phase) * params.amplitude * envelope,
            smooth_noise(seed + 31.7, phase) * params.amplitude * envelope,
            smooth_noise(seed + 77.3, phase) * params.amplitude * envelope,
        ),
        pitch: 0.0,
        yaw: 0.0,
        roll: smooth_noise(seed + 101.9, phase) * params.amplitude * 0.5 * envelope,
    })
}

/// A noise shake's envelope `elapsed` seconds in (metres): `|amplitude| · (1 − t)^falloff`, `0`
/// once it has ended.
fn noise_strength(params: &Shake, elapsed: f32) -> f32 {
    if elapsed >= params.duration_secs || params.duration_secs <= 0.0 {
        return 0.0;
    }
    let t = elapsed / params.duration_secs;
    (params.amplitude * (1.0 - t).max(0.0).powf(params.falloff.max(0.0))).abs()
}

/// A mount oscillation's envelope `elapsed` seconds in: the larger of its peak angle (radians)
/// and translation (metres), times the envelope; `0` once it has ended.
fn mount_strength(shock: &MountShock, elapsed: f32) -> f32 {
    if elapsed.is_nan() || elapsed < 0.0 || elapsed >= shock.duration_s() {
        return 0.0;
    }
    (shock.angle_rad.abs().max(shock.translation_m.abs()) * shock.envelope(elapsed)).abs()
}

/// Smooth 1D value noise in `[-1, 1]`: random lattice values at integer points, cosine-interpolated
/// (a compact stand-in for gradient noise; same construction Ken Perlin's improved noise uses for
/// its fade curve — see `docs/provenance.md`).
fn smooth_noise(seed: f32, x: f32) -> f32 {
    let lattice_value = |i: i64| -> f32 {
        let mut rng = regia_core::math::Rng::with_stream(i as u64, seed.to_bits() as u64);
        rng.f32() * 2.0 - 1.0
    };
    let i = x.floor() as i64;
    let f = x - x.floor();
    // Cosine fade (Perlin's fade for the 1D case): 0.5 − 0.5·cos(πf), from the pinned `det_cos`
    // series so the noise is bit-identical across platforms.
    let fade = 0.5 - regia_core::math::det_cos(std::f32::consts::PI * f) * 0.5;
    let a = lattice_value(i);
    let b = lattice_value(i + 1);
    a + (b - a) * fade
}

// ---------------------------------------------------------------------------
// Plugin and systems
// ---------------------------------------------------------------------------

/// Registers the camera types and updates every [`CameraRig`] each frame in `PostUpdate`, then
/// adds the shakes each camera's [`sensor::ShakeSensor`] feels (see [`sensor`]).
pub struct CameraPlugin;

impl Plugin for CameraPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<CameraRig>()
            .register_type::<CameraFollow>()
            .register_type::<CameraMode>()
            .register_type::<ThirdPerson>()
            .register_type::<Overhead>()
            .register_type::<FirstPerson>()
            .register_type::<Free>()
            .register_type::<ChaseCamera>()
            .register_type::<CameraRuntime>()
            .register_type::<CinematicCamera>()
            .init_resource::<FreeLookInput>()
            .init_resource::<CameraDirector>()
            .add_message::<driver::CameraLayerEvent>()
            .configure_sets(
                PostUpdate,
                (CameraSet::Update, CameraSet::Cinematic, CameraSet::Director).chain(),
            )
            .add_systems(
                PostUpdate,
                update_rigs
                    .in_set(CameraSet::Update)
                    .after(PostUpdateSet::LateScripts)
                    .before(PostUpdateSet::TransformPropagate),
            )
            .register_type::<blend::PlayerCamera>()
            .add_systems(
                PostUpdate,
                blend::blend_cameras
                    .after(CameraSet::Cinematic)
                    .before(CameraSet::Director)
                    .before(PostUpdateSet::TransformPropagate),
            )
            .add_systems(
                PostUpdate,
                driver::drive_cameras
                    .in_set(CameraSet::Update)
                    .after(PostUpdateSet::LateScripts)
                    .before(PostUpdateSet::TransformPropagate),
            );
        sensor::install(app);
        walk_bob::install(app);
        handheld::install(app);
        photographic::install(app);
        rail_keys::install(app);
        if !app.has_plugin(std::any::type_name::<load::CameraAssetsPlugin>()) {
            app.add_plugins(load::CameraAssetsPlugin);
        }
        app.add_systems(Update, load::bind_camera_sources);
        app.add_plugins(debug::CameraDebugPlugin);
    }
}

/// Writes the pose of every rig (the math is in the module docs).
///
/// Per camera, in order:
///
/// 1. Clamps [`CameraRig::pitch`] into the mode's range and stores the clamped value. A
///    non-finite pitch is read as level (`0`), a non-finite bound as unbounded, and crossed
///    bounds are swapped, so bad configuration can never panic or produce a NaN rotation. A
///    non-finite yaw is likewise read as level.
/// 2. Computes the mode's desired pose (see each variant). Modes with a target skip the frame
///    while the target is missing.
/// 3. Eases the current pose towards the desired pose: position by `lag_speed` and rotation by
///    `rotation_lag`, each with `current = lerp(current, desired, 1 − exp(−speed · Δt))`. Lean
///    (chase) and flight smoothing (free) run through [`CameraRuntime`].
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(skip_all, target = "regia_framework", name = "cameras_update")]
fn update_rigs(
    time: Res<Time>,
    mut commands: Commands,
    free_input: Res<FreeLookInput>,
    physics: Option<Res<regia_physics::PhysicsWorld>>,
    physics_layers: Option<Res<regia_physics::PhysicsLayers>>,
    mut cameras: Query<(Entity, &mut CameraRig, &mut Transform), Without<driver::CameraDriver>>,
    chases: Query<&ChaseCamera>,
    targets: FollowTargets,
    mut runtime: Query<&mut CameraRuntime>,
) {
    let delta = time.delta_secs();
    for (entity, mut rig, mut transform) in &mut cameras {
        // Ensure runtime state exists (first frame only).
        if runtime.get_mut(entity).is_err() {
            commands.entity(entity).try_insert(CameraRuntime::default());
        }

        // Target modes need a ChaseCamera naming a live, transformed target.
        let chase = chases.get(entity).ok();
        let follow = rig.follow;
        let target = chase.map(|chase| {
            targets
                .get(chase.target)
                .map(|(pose, offset)| followed_pose(pose, offset, follow))
        });
        match rig.mode {
            CameraMode::ThirdPerson(mode) => {
                let Some(target) = target else { continue };
                let Ok(target) = target else { continue };

                // 1. Clamp the stored pitch so input code and inspectors see the effective angle.
                let low = if mode.min_pitch.is_finite() {
                    mode.min_pitch
                } else {
                    f32::NEG_INFINITY
                };
                let high = if mode.max_pitch.is_finite() {
                    mode.max_pitch
                } else {
                    f32::INFINITY
                };
                let (low, high) = if low <= high { (low, high) } else { (high, low) };
                rig.pitch = finite_or_zero(rig.pitch).clamp(low, high);

                // 2. View rotation and desired position: pivot, then the boom back along the
                //    view direction. The boom shortens on collision (§ B5): a sphere of
                //    `probe_radius` is cast from the pivot along the boom before the pose is
                //    computed. Retraction snaps (never clip through a wall that just appeared);
                //    extension eases back at half the lag speed so the camera recovers smoothly.
                // The pinned euler construction (spec 35 § 12.13), matching the driver path.
                let view = det_quat_from_yaw_pitch_roll(finite_or_zero(rig.yaw), rig.pitch, 0.0);
                let forward = view * Vec3::NEG_Z;
                let pivot = target.translation + target.rotation * mode.socket_offset;
                let target_entity = chase.map_or(Entity::PLACEHOLDER, |chase| chase.target);
                let desired_boom = probe_boom_length(
                    physics.as_deref(),
                    physics_layers.as_deref(),
                    &mode,
                    pivot,
                    -forward,
                    target_entity,
                );
                let current_boom = runtime_state(&runtime, entity).boom;
                // Snap on first frame and when retracting; ease the extension back out.
                let boom = if current_boom <= 0.0 || desired_boom < current_boom {
                    desired_boom
                } else {
                    current_boom + (desired_boom - current_boom) * lag_factor(mode.lag_speed * 0.5, delta)
                };
                if let Ok(mut state) = runtime.get_mut(entity) {
                    state.boom = boom;
                }
                let desired_position = pivot - forward * boom;

                // 3. Aim at the look-at point; fall back to the view rotation when degenerate.
                let look_at = target.translation + target.rotation * mode.target_offset;
                let desired_rotation =
                    Isometry::looking_at(desired_position, look_at, Vec3::Y).map_or(view, |aim| aim.rotation);

                // 4. Exponential lag towards the desired pose, plus the lean roll.
                transform.translation = transform
                    .translation
                    .lerp(desired_position, lag_factor(mode.lag_speed, delta));
                let yaw_rate =
                    wrap_angle(rig.yaw - runtime_state(&runtime, entity).prev_yaw) / delta.max(1.0e-6);
                let lean_target = (yaw_rate * mode.lean_gain).clamp(-mode.max_lean, mode.max_lean);
                if let Ok(mut state) = runtime.get_mut(entity) {
                    state.prev_yaw = rig.yaw;
                    let factor = lag_factor(mode.lean_speed, delta);
                    state.lean = state.lean + (lean_target - state.lean) * factor;
                    // A frozen rotation (rotation_lag 0) freezes the lean too: no rotation at all.
                    let roll = if mode.rotation_lag > 0.0 { state.lean } else { 0.0 };
                    transform.rotation = det_nlerp(
                        transform.rotation,
                        desired_rotation,
                        lag_factor(mode.rotation_lag, delta),
                    ) * det_quat_from_yaw_pitch_roll(0.0, 0.0, roll);
                } else {
                    transform.rotation = det_nlerp(
                        transform.rotation,
                        desired_rotation,
                        lag_factor(mode.rotation_lag, delta),
                    );
                }
            }
            CameraMode::Overhead(mode) => {
                let Some(target) = target else { continue };
                let Ok(target) = target else { continue };
                let pitch = finite_or_zero(mode.pitch).clamp(-FRAC_PI_2, 0.0);
                rig.pitch = pitch;
                let view = det_quat_from_yaw_pitch_roll(finite_or_zero(rig.yaw), pitch, 0.0);
                let forward = view * Vec3::NEG_Z;
                let desired_position = target.translation - forward * mode.height;
                let desired_rotation = Isometry::looking_at(desired_position, target.translation, Vec3::Y)
                    .map_or(view, |aim| aim.rotation);
                transform.translation = transform
                    .translation
                    .lerp(desired_position, lag_factor(mode.lag_speed, delta));
                transform.rotation = desired_rotation;
            }
            CameraMode::FirstPerson(mode) => {
                let Some(target) = target else { continue };
                let Ok(target) = target else { continue };
                rig.pitch = finite_or_zero(rig.pitch).clamp(-FRAC_PI_2, FRAC_PI_2);
                let look = det_quat_from_yaw_pitch_roll(finite_or_zero(rig.yaw), rig.pitch, 0.0);
                transform.translation = target.translation + target.rotation * mode.eye_offset;
                transform.rotation = target.rotation * look;
            }
            CameraMode::Free(mode) => {
                // Look: yaw/pitch follow the free-look delta directly.
                rig.yaw = wrap_angle(finite_or_zero(rig.yaw) - free_input.look.x);
                rig.pitch = (finite_or_zero(rig.pitch) + free_input.look.y).clamp(-FRAC_PI_2, FRAC_PI_2);
                let view = det_quat_from_yaw_pitch_roll(rig.yaw, rig.pitch, 0.0);
                let forward = view * Vec3::NEG_Z;
                let right = view * Vec3::X;
                let speed = mode.speed
                    * if free_input.sprint {
                        mode.sprint_multiplier
                    } else {
                        1.0
                    };
                let desired_velocity = (forward * free_input.move_axes.y
                    + right * free_input.move_axes.x
                    + Vec3::Y * free_input.rise)
                    * speed;
                if let Ok(mut state) = runtime.get_mut(entity) {
                    // `accel <= 0` means instant velocity (lag_factor(0) would freeze it).
                    let factor = if mode.accel <= 0.0 {
                        1.0
                    } else {
                        lag_factor(mode.accel, delta)
                    };
                    state.velocity = state.velocity.lerp(desired_velocity, factor);
                    transform.translation += state.velocity * delta;
                } else {
                    transform.translation += desired_velocity * delta;
                }
                transform.rotation = view;
            }
        }
    }
}

/// Reads a rig's runtime state without borrowing conflicts (defaults when absent).
fn runtime_state(runtime: &Query<&mut CameraRuntime>, entity: Entity) -> CameraRuntime {
    runtime.get(entity).copied().unwrap_or_default()
}

/// Skin kept between the probe sphere and geometry (metres).
const PROBE_SKIN: f32 = 0.05;
/// Shortest boom the probe may produce (metres) — the camera never rides the target's head.
const PROBE_MIN_BOOM: f32 = 0.1;

/// The boom length the sphere-cast probe allows (§ B5): casts a sphere of
/// [`ThirdPerson::probe_radius`] from `pivot` along `direction` (unit, pivot → camera) and
/// returns the first hit's distance minus the radius and a small skin, clamped into
/// `[min_boom, mode.boom_length]`. Without a physics world, or when the probe is disabled,
/// the boom keeps its full length.
fn probe_boom_length(
    world: Option<&regia_physics::PhysicsWorld>,
    layers: Option<&regia_physics::PhysicsLayers>,
    mode: &ThirdPerson,
    pivot: Vec3,
    direction: Vec3,
    target: Entity,
) -> f32 {
    let Some(world) = world else {
        return mode.boom_length;
    };
    if !mode.probe_radius.is_finite() || mode.probe_radius <= 0.0 || direction.length_squared() < 1.0e-8 {
        return mode.boom_length;
    }
    let queries = regia_physics::queries::WorldQueries { world, layers };
    let excluded = [target];
    let filter = regia_physics::queries::QueryFilter::new()
        .exclude_sensors()
        .exclude_entities(&excluded);
    let hit = queries.shapecast(
        &regia_physics::ColliderShape::Sphere {
            radius: mode.probe_radius,
        },
        Transform {
            translation: pivot,
            ..Transform::default()
        },
        direction,
        mode.boom_length,
        filter,
    );
    match hit {
        // `time_of_impact` is the distance the sphere *centre* travels before touching, so
        // only the skin is subtracted (§ B5).
        Some(hit) => probe_hit_boom(hit.time_of_impact, mode.probe_radius, mode.boom_length),
        None => mode.boom_length,
    }
}

/// The boom a probe hit at `distance` allows: `distance` minus the skin, kept in
/// `[max(probe_radius, PROBE_MIN_BOOM), boom_length]`. A boom authored shorter than that minimum
/// (or negative, or NaN) caps the minimum instead of panicking in `clamp` (audit-gameplay zai M1,
/// ADR-1809).
fn probe_hit_boom(distance: f32, probe_radius: f32, boom_length: f32) -> f32 {
    let max = boom_length.max(0.0);
    let min = probe_radius.max(PROBE_MIN_BOOM).min(max);
    (distance - PROBE_SKIN).clamp(min, max)
}

/// The share of the remaining distance a lagging value covers in one frame:
/// `1 − exp(−speed · Δt)`, clamped into `[0, 1]`. Negative (or NaN) speeds and NaN deltas yield
/// `0` — the value does not move; an infinite speed yields `1` — it snaps. The exponential is the
/// pinned `det_exp` series, the same value the camera driver's `smoothing_factor` uses, so the
/// legacy rig and the driver stay in lockstep bit for bit (spec 35 § 12.13).
fn lag_factor(speed: f32, delta: f32) -> f32 {
    let t = 1.0 - regia_core::math::det_exp(-speed * delta);
    if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 }
}

/// `angle` when finite, otherwise `0` (level). Keeps NaN or infinite input from poisoning the
/// camera pose.
fn finite_or_zero(angle: f32) -> f32 {
    if angle.is_finite() { angle } else { 0.0 }
}

/// Wraps an angle difference into `(-π, π]` (shortest path).
fn wrap_angle(angle: f32) -> f32 {
    if !angle.is_finite() {
        return 0.0;
    }
    let wrapped = angle.rem_euclid(std::f32::consts::TAU);
    if wrapped > std::f32::consts::PI {
        wrapped - std::f32::consts::TAU
    } else {
        wrapped
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use regia_core::math::Quat;

    #[test]
    fn a_boom_shorter_than_the_probe_radius_never_panics() {
        // boom 0.5 with a 1.0 probe radius, and a hit at the pivot (zai M1).
        assert_eq!(probe_hit_boom(0.0, 1.0, 0.5), 0.5);
        // boom below PROBE_MIN_BOOM with the default radius.
        assert_eq!(probe_hit_boom(0.0, 0.2, 0.08), 0.08);
        assert_eq!(probe_hit_boom(0.3, 0.2, -1.0), 0.0);
        assert_eq!(probe_hit_boom(0.3, 0.2, f32::NAN), 0.0);
        // The normal case is unchanged.
        assert!((probe_hit_boom(2.05, 0.2, 4.0) - 2.0).abs() < 1e-6);
        assert_eq!(probe_hit_boom(0.0, 0.2, 4.0), 0.2);
        assert_eq!(probe_hit_boom(9.0, 0.2, 4.0), 4.0);
    }
    use std::f32::consts::FRAC_PI_2;
    use std::time::Duration;

    /// One 60 Hz frame.
    const STEP: Duration = Duration::from_millis(16);

    /// Third-person parameters with the given smoothing and everything else default.
    fn third_person(lag_speed: f32, rotation_lag: f32) -> ThirdPerson {
        ThirdPerson {
            boom_length: 5.0,
            lag_speed,
            rotation_lag,
            ..ThirdPerson::default()
        }
    }

    /// A headless app with one target at the origin and one camera following it.
    fn camera_app(rig: CameraRig, camera_transform: Transform, step: Duration) -> (App, Entity, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(step));
        let target = app.world_mut().spawn(Transform::default()).id();
        let camera = app
            .world_mut()
            .spawn((camera_transform, rig, ChaseCamera { target }))
            .id();
        (app, camera, target)
    }

    #[test]
    fn full_lag_reaches_the_desired_pose_in_one_frame() {
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(1.0e6, 1.0e6)),
            ..CameraRig::default()
        };
        let start = Transform {
            translation: Vec3::new(100.0, 100.0, 100.0),
            ..Transform::default()
        };
        let (mut app, camera, _) = camera_app(rig, start, STEP);
        app.update();

        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        assert!(
            pose.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 5.0), 1.0e-4),
            "camera should sit 5 m behind the target on +Z, got {:?}",
            pose.translation
        );
        assert!(
            pose.rotation.abs_diff_eq(Quat::IDENTITY, 1.0e-3),
            "camera should look along −Z at the target, got {:?}",
            pose.rotation
        );
    }

    #[test]
    fn pitch_is_clamped_into_the_mode_range() {
        let mode = CameraMode::ThirdPerson(ThirdPerson {
            min_pitch: -1.0,
            max_pitch: 1.0,
            lag_speed: 0.0,
            rotation_lag: 0.0,
            ..ThirdPerson::default()
        });
        let (mut app, camera, target) = camera_app(
            CameraRig {
                mode,
                pitch: 5.0,
                yaw: 0.0,
                ..CameraRig::default()
            },
            Transform::default(),
            STEP,
        );
        let second = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraRig {
                    mode,
                    pitch: -5.0,
                    yaw: 0.0,
                    ..CameraRig::default()
                },
                ChaseCamera { target },
            ))
            .id();
        app.update();

        let stored_high = app.world().get::<CameraRig>(camera).expect("camera rig").pitch;
        let stored_low = app.world().get::<CameraRig>(second).expect("camera rig").pitch;
        assert_eq!(
            stored_high, 1.0,
            "pitch above max_pitch must be stored as max_pitch"
        );
        assert_eq!(
            stored_low, -1.0,
            "pitch below min_pitch must be stored as min_pitch"
        );
    }

    #[test]
    fn zero_lag_freezes_the_camera() {
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(0.0, 0.0)),
            yaw: 1.0, // the desired pose differs from the initial one
            ..CameraRig::default()
        };
        let start = Transform {
            translation: Vec3::new(7.0, 8.0, 9.0),
            rotation: Quat::from_rotation_y(FRAC_PI_2),
            ..Transform::default()
        };
        let (mut app, camera, _) = camera_app(rig, start, STEP);
        for _ in 0..3 {
            app.update();
        }

        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        assert_eq!(
            pose.translation,
            Vec3::new(7.0, 8.0, 9.0),
            "lag_speed 0 must not move the camera"
        );
        assert!(
            pose.rotation
                .abs_diff_eq(Quat::from_rotation_y(FRAC_PI_2), 1.0e-5),
            "rotation_lag 0 must not rotate the camera, got {:?}",
            pose.rotation
        );
    }

    #[test]
    fn lag_covers_the_exponential_fraction_of_a_frame() {
        // Δt = 0.25 s and lag_speed = ln 2 / Δt give the exact fraction 1 − exp(−ln 2) = 0.5.
        let step = Duration::from_millis(250);
        let lag_speed = std::f32::consts::LN_2 / step.as_secs_f32();
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(lag_speed, 0.0)),
            ..CameraRig::default()
        };
        let start = Transform {
            translation: Vec3::new(0.0, 0.0, 15.0),
            ..Transform::default()
        };
        let (mut app, camera, _) = camera_app(rig, start, step);
        app.update();

        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        // Halfway between the start (0, 0, 15) and the desired pose (0, 0, 5).
        assert!(
            pose.translation.abs_diff_eq(Vec3::new(0.0, 0.0, 10.0), 1.0e-4),
            "expected the camera halfway along the boom, got {:?}",
            pose.translation
        );
    }

    #[test]
    fn missing_target_skips_the_frame_without_panicking() {
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(1.0e6, 1.0e6)),
            ..CameraRig::default()
        };
        let start = Transform {
            translation: Vec3::new(1.0, 2.0, 3.0),
            ..Transform::default()
        };
        let (mut app, camera, target) = camera_app(rig, start, STEP);
        app.world_mut().despawn(target);
        app.update();

        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        assert_eq!(pose.translation, Vec3::new(1.0, 2.0, 3.0), "no target, no update");
    }

    // --- new modes ---------------------------------------------------------

    #[test]
    fn chase_leans_into_yaw_velocity_and_relaxes() {
        // Lean off: snap lag so poses are exact; lean gain strong.
        let mode = ThirdPerson {
            lag_speed: 1.0e6,
            rotation_lag: 1.0e6,
            lean_gain: 0.5,
            max_lean: 0.4,
            lean_speed: 1.0e6, // lean snaps to its target each frame
            ..ThirdPerson::default()
        };
        let (mut app, camera, _) = camera_app(
            CameraRig {
                mode: CameraMode::ThirdPerson(mode),
                ..CameraRig::default()
            },
            Transform::default(),
            STEP,
        );

        // Frame 1: yaw 0 → no lean.
        app.update();
        let level = app.world().get::<Transform>(camera).expect("transform").rotation;

        // Turn hard: yaw rate = Δyaw / Δt = (π/4) / (1/60) ≈ 47 rad/s → lean clamps at max.
        let mut rig = app.world_mut().get_mut::<CameraRig>(camera).expect("rig");
        rig.yaw += std::f32::consts::FRAC_PI_4;
        app.update();

        let leaned = app.world().get::<Transform>(camera).expect("transform").rotation;
        // Reconstruct the roll component: the leaned rotation differs from the level one.
        assert!(
            !leaned.abs_diff_eq(level, 1.0e-3),
            "a hard turn must roll the camera (lean), got {leaned:?} vs {level:?}"
        );

        // Stop turning: the lean relaxes back to ~0.
        for _ in 0..120 {
            app.update();
        }
        let settled = app.world().get::<Transform>(camera).expect("transform").rotation;
        let state = app.world().get::<CameraRuntime>(camera).expect("runtime");
        assert!(state.lean.abs() < 1.0e-3, "lean must relax, got {}", state.lean);
        let _ = settled;
    }

    #[test]
    fn lean_disabled_with_zero_gain() {
        let mode = ThirdPerson {
            lag_speed: 1.0e6,
            rotation_lag: 1.0e6,
            lean_gain: 0.0,
            max_lean: 0.0,
            ..ThirdPerson::default()
        };
        let (mut app, camera, _) = camera_app(
            CameraRig {
                mode: CameraMode::ThirdPerson(mode),
                ..CameraRig::default()
            },
            Transform::default(),
            STEP,
        );
        app.update();
        let before = app.world().get::<Transform>(camera).expect("transform").rotation;
        app.world_mut().get_mut::<CameraRig>(camera).expect("rig").yaw += 1.0;
        app.update();
        let after = app.world().get::<Transform>(camera).expect("transform").rotation;
        // Only the yaw differs, no roll: rotation magnitudes stay 1; the camera looks elsewhere
        // but its up vector must stay world-up (no roll) → rotation is a pure yaw.
        let up = after * Vec3::Y;
        assert!(up.y > 0.99, "no-roll camera keeps up ≈ +Y, got {up:?}");
        let _ = before;
    }

    #[test]
    fn overhead_sits_above_the_target_looking_down() {
        let rig = CameraRig {
            mode: CameraMode::Overhead(Overhead {
                height: 20.0,
                pitch: -FRAC_PI_2,
                lag_speed: 1.0e6,
            }),
            ..CameraRig::default()
        };
        let (mut app, camera, target) = camera_app(rig, Transform::default(), STEP);
        app.world_mut()
            .get_mut::<Transform>(target)
            .expect("target transform")
            .translation = Vec3::new(5.0, 0.0, -3.0);
        app.update();

        let pose = app.world().get::<Transform>(camera).expect("transform");
        assert!(
            pose.translation.abs_diff_eq(Vec3::new(5.0, 20.0, -3.0), 1.0e-3),
            "overhead camera sits height above the target, got {:?}",
            pose.translation
        );
        // Looking straight down: forward ≈ −Y.
        let forward = pose.rotation * Vec3::NEG_Z;
        assert!(
            forward.abs_diff_eq(Vec3::NEG_Y, 1.0e-3),
            "overhead camera looks down, got forward {forward:?}"
        );
    }

    #[test]
    fn first_person_is_welded_to_the_target_with_look_offsets() {
        let rig = CameraRig {
            mode: CameraMode::FirstPerson(FirstPerson {
                eye_offset: Vec3::new(0.0, 2.0, 0.0),
                bob: 0.0,
            }),
            yaw: 0.0,
            pitch: 0.1,
            ..CameraRig::default()
        };
        let (mut app, camera, target) = camera_app(rig, Transform::default(), STEP);
        let target_rotation = Quat::from_rotation_y(1.0);
        let target_translation = Vec3::new(3.0, 1.0, 2.0);
        {
            let mut t = app.world_mut().get_mut::<Transform>(target).expect("target");
            t.rotation = target_rotation;
            t.translation = target_translation;
        }
        app.update();

        let pose = app.world().get::<Transform>(camera).expect("transform");
        assert!(
            pose.translation
                .abs_diff_eq(target_translation + Vec3::new(0.0, 2.0, 0.0), 1.0e-4),
            "eye welded at eye_offset, got {:?}",
            pose.translation
        );
        let expected = target_rotation * Quat::from_rotation_x(0.1);
        assert!(
            pose.rotation.abs_diff_eq(expected, 1.0e-3),
            "rotation = target rotation · look offsets, got {:?}",
            pose.rotation
        );
    }

    #[test]
    fn free_camera_flies_from_the_input_resource() {
        let rig = CameraRig {
            mode: CameraMode::Free(Free {
                speed: 10.0,
                sprint_multiplier: 2.0,
                accel: 0.0,
            }),
            ..Default::default()
        };
        let (mut app, camera, _) = camera_app(rig, Transform::default(), STEP);

        // One frame of input: forward at 10 m/s for 1/60 s.
        app.world_mut().resource_mut::<FreeLookInput>().move_axes = Vec2::new(0.0, 1.0);
        app.update();
        let pose = app.world().get::<Transform>(camera).expect("transform");
        let expected = Vec3::NEG_Z * 10.0 * STEP.as_secs_f32();
        assert!(
            pose.translation.abs_diff_eq(expected, 1.0e-4),
            "free camera flies forward, expected {expected:?}, got {:?}",
            pose.translation
        );

        // No target needed: despawn everything else, still fine.
        app.update();
        assert!(app.world().get::<Transform>(camera).is_some());
    }

    #[test]
    fn switching_modes_keeps_the_rig_on_the_same_entity() {
        let (mut app, camera, _) = camera_app(CameraRig::default(), Transform::default(), STEP);
        let mut rig = app.world_mut().get_mut::<CameraRig>(camera).expect("rig");
        rig.switch_to(CameraMode::from_name("overhead").expect("name"));
        assert_eq!(
            app.world().get::<CameraRig>(camera).expect("rig").mode.name(),
            "overhead"
        );
        app.update(); // runs the overhead path without panic
        let mut rig = app.world_mut().get_mut::<CameraRig>(camera).expect("rig");
        rig.switch_to(CameraMode::from_name("free").expect("name"));
        app.update(); // runs the free path without panic
        assert_eq!(
            app.world().get::<CameraRig>(camera).expect("rig").mode.name(),
            "free"
        );
    }

    #[test]
    fn mode_names_round_trip() {
        for name in ["chase", "overhead", "first_person", "free"] {
            let mode = CameraMode::from_name(name).expect("known name");
            assert_eq!(mode.name(), name);
        }
        assert!(CameraMode::from_name("nope").is_none());
        // DO27/spec aliases resolve to the same modes.
        assert_eq!(
            CameraMode::from_name("satellite"),
            CameraMode::from_name("overhead")
        );
        assert_eq!(CameraMode::from_name("spectator"), CameraMode::from_name("free"));
    }

    /// A shake added after an earlier one ended must not reuse the seed of a shake still running:
    /// two identical requests would stack one noise lattice twice (double amplitude, no
    /// independence) instead of adding two independent wobbles (audit-seqdet-muse L4).
    #[test]
    fn a_new_shake_never_reuses_a_running_shakes_seed() {
        let mut director = CameraDirector::default();
        let short = Shake {
            duration_secs: 0.1,
            ..Shake::default()
        };
        let long = Shake {
            duration_secs: 10.0,
            ..Shake::default()
        };
        director.shake(short);
        director.shake(long);
        // The short shake ends; the long one keeps running.
        director.sample(0.5);
        assert_eq!(director.shakes.len(), 1);
        director.shake(long);
        let seeds: Vec<f32> = director.shakes.iter().map(|(_, _, seed)| *seed).collect();
        assert_eq!(seeds.len(), 2);
        assert_ne!(seeds[0], seeds[1], "two running shakes share a seed: {seeds:?}");
    }

    /// Spec 14 B5: the FOV kick adds up, decays exponentially per sampled frame (`exp(-4 t)`)
    /// and snaps to zero once negligible; it never moves the shake offset.
    #[test]
    fn fov_kick_adds_up_and_decays_to_zero() {
        let mut director = CameraDirector::default();
        director.fov_kick(6.0);
        director.fov_kick(4.0);
        assert_eq!(director.fov_kick_value(), 10.0, "kicks add");
        let (offset, roll) = director.sample(0.25);
        assert_eq!((offset, roll), (Vec3::ZERO, 0.0), "a kick is not a shake");
        let expected = 10.0 * (-4.0f32 * 0.25).exp();
        assert!(
            (director.fov_kick_value() - expected).abs() < 1.0e-5,
            "one quarter second of decay: {} vs {expected}",
            director.fov_kick_value()
        );
        for _ in 0..40 {
            director.sample(0.1);
        }
        assert_eq!(director.fov_kick_value(), 0.0, "snaps to zero when negligible");
        assert!(!director.is_shaking(), "a kick alone never counts as shaking");
    }

    #[test]
    fn director_shake_offsets_and_decays() {
        // lag 0 keeps the rig pose pinned at the origin, so any translation is the shake.
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(0.0, 0.0)),
            ..CameraRig::default()
        };
        let (mut app, camera, _) = camera_app(rig, Transform::default(), STEP);
        app.world_mut().resource_mut::<CameraDirector>().shake(Shake {
            amplitude: 1.0,
            frequency_hz: 10.0,
            duration_secs: 0.5,
            falloff: 1.0,
        });

        app.update();
        let shaken = app
            .world()
            .get::<Transform>(camera)
            .expect("transform")
            .translation;
        assert!(
            shaken.length() > 0.1,
            "shake must offset the camera, got {shaken:?}"
        );

        // Wait past the duration: the shake ends and the offset disappears (pose returns to 0).
        for _ in 0..40 {
            app.update();
        }
        assert!(!app.world().resource::<CameraDirector>().is_shaking());
        let settled = app
            .world()
            .get::<Transform>(camera)
            .expect("transform")
            .translation;
        assert!(
            settled.abs_diff_eq(Vec3::ZERO, 1.0e-4),
            "camera returns to the un-shaken pose, got {settled:?}"
        );
    }

    #[test]
    fn a_mount_shock_tilts_the_rig_and_the_tilt_is_removed_when_it_ends() {
        let rig = CameraRig {
            mode: CameraMode::ThirdPerson(third_person(0.0, 0.0)),
            ..CameraRig::default()
        };
        let (mut app, camera, _) = camera_app(rig, Transform::default(), STEP);
        app.update();
        let rest = app.world().get::<Transform>(camera).expect("transform").rotation;
        app.world_mut()
            .resource_mut::<CameraDirector>()
            .mount_shock(MountShock::new(0.05, 0.0, 0.3, 3));
        let mut tilted = 0.0f32;
        for _ in 0..30 {
            app.update();
            let now = app.world().get::<Transform>(camera).expect("transform").rotation;
            tilted = tilted.max(rest.angle_between(now));
        }
        assert!(tilted > 0.005, "the rig tilted {tilted} rad");
        for _ in 0..200 {
            app.update();
        }
        assert!(!app.world().resource::<CameraDirector>().is_shaking());
        let settled = app.world().get::<Transform>(camera).expect("transform").rotation;
        assert!(rest.angle_between(settled) < 2.0e-3, "back at rest");
    }

    #[test]
    fn wrap_angle_takes_the_shortest_path() {
        assert!((wrap_angle(std::f32::consts::TAU)).abs() < 1.0e-6);
        assert!((wrap_angle(std::f32::consts::PI + 0.1) - (-std::f32::consts::PI + 0.1)).abs() < 1.0e-6);
        assert_eq!(wrap_angle(f32::NAN), 0.0);
    }

    // --- boom collision (§ B5, M20) ----------------------------------------

    /// A camera app with a live physics world: MinimalPlugins + CameraPlugin + PhysicsPlugin,
    /// one target at the origin, one chase camera with the given boom.
    fn physics_camera_app(boom: f32) -> (App, Entity) {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin, regia_physics::PhysicsPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(STEP));
        let target = app.world_mut().spawn(Transform::default()).id();
        let camera = app
            .world_mut()
            .spawn((
                Transform::default(),
                CameraRig {
                    mode: CameraMode::ThirdPerson(third_person(1.0e6, 1.0e6)),
                    ..CameraRig::default()
                },
                ChaseCamera { target },
            ))
            .id();
        // The boom under test.
        if let CameraMode::ThirdPerson(mode) =
            &mut app.world_mut().get_mut::<CameraRig>(camera).expect("rig").mode
        {
            mode.boom_length = boom;
        }
        (app, camera)
    }

    /// Spawns a static wall box centred at `centre` with the given half-extents.
    fn spawn_wall(app: &mut App, centre: Vec3, half_extents: Vec3) {
        app.world_mut().spawn((
            Transform {
                translation: centre,
                ..Transform::default()
            },
            regia_physics::RigidBody {
                kind: regia_physics::BodyKind::Static,
                ..Default::default()
            },
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box { half_extents },
                ..Default::default()
            },
        ));
    }

    /// With a wall between the target and the camera's rest position, the boom shortens to
    /// keep `probe_radius` plus skin between the camera and the wall.
    #[test]
    fn boom_shortens_on_collision() {
        let (mut app, camera) = physics_camera_app(5.0);
        // The camera rests at (0, 0, 5); a wall with its near face at z = 2.75.
        spawn_wall(&mut app, Vec3::new(0.0, 2.0, 3.0), Vec3::new(4.0, 4.0, 0.25));
        // A few updates for the collider to reach the physics world.
        for _ in 0..6 {
            app.update();
        }
        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        let mode = third_person(0.0, 0.0);
        // The sphere centre stops at wall − radius; the boom subtracts only the skin.
        let expected = 2.75 - mode.probe_radius - PROBE_SKIN;
        assert!(
            (pose.translation.z - expected).abs() < 1.0e-3,
            "boom must shorten to {expected}, camera at {:?}",
            pose.translation
        );
        // The runtime remembers the effective length.
        let state = app.world().get::<CameraRuntime>(camera).expect("runtime");
        assert!(
            (state.boom - expected).abs() < 1.0e-3,
            "runtime boom {}",
            state.boom
        );
    }

    /// Without geometry in the way the boom keeps its full length; the probe never fires.
    #[test]
    fn boom_keeps_full_length_in_the_open() {
        let (mut app, camera) = physics_camera_app(5.0);
        for _ in 0..3 {
            app.update();
        }
        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        assert!(
            (pose.translation.z - 5.0).abs() < 1.0e-3,
            "camera rests at the full boom, got {:?}",
            pose.translation
        );
    }

    /// After the obstruction disappears the boom eases back out (smoothed, not snapped).
    #[test]
    fn boom_recovers_smoothly_after_the_wall_is_gone() {
        let (mut app, camera) = physics_camera_app(5.0);
        // A moderate lag so the half-rate extension easing is visible frame to frame.
        {
            let mut rig = app.world_mut().get_mut::<CameraRig>(camera).expect("rig");
            if let CameraMode::ThirdPerson(mode) = &mut rig.mode {
                mode.lag_speed = 4.0;
                mode.rotation_lag = 4.0;
            }
        }
        let wall = app
            .world_mut()
            .spawn((
                Transform::default(),
                regia_physics::RigidBody {
                    kind: regia_physics::BodyKind::Static,
                    ..Default::default()
                },
                regia_physics::Collider {
                    shape: regia_physics::ColliderShape::Box {
                        half_extents: Vec3::new(4.0, 4.0, 0.25),
                    },
                    ..Default::default()
                },
            ))
            .id();
        // Sync the wall into the physics world first.
        for _ in 0..6 {
            app.update();
        }
        let shortened = app.world().get::<CameraRuntime>(camera).expect("runtime").boom;
        assert!(shortened < 3.0, "boom shortened: {shortened}");

        // Remove the wall; the boom extends gradually (half-rate easing), reaching the full
        // length within a reasonable window. The despawn itself needs a frame to land in
        // the physics world.
        app.world_mut().despawn(wall);
        let mut gradual = false;
        for _ in 0..240 {
            app.update();
            let boom = app.world().get::<CameraRuntime>(camera).expect("runtime").boom;
            if (3.2..4.9).contains(&boom) {
                gradual = true;
            }
            if (boom - 5.0).abs() < 1.0e-3 {
                break;
            }
        }
        assert!(gradual, "boom recovery must pass through intermediate lengths");
        let settled = app.world().get::<CameraRuntime>(camera).expect("runtime").boom;
        assert!((settled - 5.0).abs() < 5.0e-3, "boom fully recovered: {settled}");
    }

    /// The probe ignores sensor colliders (trigger volumes never block a camera).
    #[test]
    fn sensors_do_not_block_the_boom() {
        let (mut app, camera) = physics_camera_app(5.0);
        app.world_mut().spawn((
            Transform::default(),
            regia_physics::RigidBody {
                kind: regia_physics::BodyKind::Static,
                ..Default::default()
            },
            regia_physics::Collider {
                shape: regia_physics::ColliderShape::Box {
                    half_extents: Vec3::new(4.0, 4.0, 0.25),
                },
                sensor: true,
                ..Default::default()
            },
        ));
        for _ in 0..6 {
            app.update();
        }
        let pose = app.world().get::<Transform>(camera).expect("camera transform");
        assert!(
            (pose.translation.z - 5.0).abs() < 1.0e-3,
            "sensor must not shorten the boom, camera at {:?}",
            pose.translation
        );
    }
}

/// Spec 73 § 6.4 (ADR-7213): a rig follows the smoothed or the simulated pose of a predicted target.
#[cfg(all(test, feature = "net"))]
mod follow_tests {
    use super::*;
    use std::time::Duration;

    fn eye(follow: CameraFollow) -> Vec3 {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, CameraPlugin))
            .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
        let target = app
            .world_mut()
            .spawn((
                Transform::default(),
                regia_net::NetVisualOffset {
                    translation: Vec3::new(1.0, 0.0, 0.0),
                    ..regia_net::NetVisualOffset::default()
                },
            ))
            .id();
        let rig = CameraRig {
            mode: CameraMode::FirstPerson(FirstPerson {
                eye_offset: Vec3::ZERO,
                bob: 0.0,
            }),
            follow,
            ..CameraRig::default()
        };
        let camera = app
            .world_mut()
            .spawn((Transform::default(), rig, ChaseCamera { target }))
            .id();
        app.update();
        app.world().get::<Transform>(camera).expect("camera").translation
    }

    #[test]
    fn the_camera_follows_the_chosen_pose() {
        assert_eq!(
            CameraRig::default().follow,
            CameraFollow::Smoothed,
            "spec default"
        );
        assert!(eye(CameraFollow::Smoothed).abs_diff_eq(Vec3::new(1.0, 0.0, 0.0), 1.0e-6));
        assert!(eye(CameraFollow::Simulated).abs_diff_eq(Vec3::ZERO, 1.0e-6));
        for f in [CameraFollow::Smoothed, CameraFollow::Simulated] {
            assert_eq!(CameraFollow::from_name(f.name()), Some(f));
        }
    }
}
