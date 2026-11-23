// SPDX-License-Identifier: MPL-2.0
//! The camera as a physical actor (ADR-1502): a kinematic body with a radius and a speed that
//! the world reacts to as it flies past.
//!
//! [`CameraBody`] follows the camera's path. With a [`ShotPlayer`] sequence loaded it reads the
//! path at the player's clock; otherwise it follows the entity marked [`CameraBodyFollow`]. Each
//! frame it keeps the body's position and velocity (now and the frame before). From that state
//! the closed-form wake of [`regia_core::math::wake`] drives four reactions:
//!
//! 1. **Air wake.** [`CameraBody::wake_at`] gives the gust at any point. Hosts hand
//!    [`CameraBody::wake_words`] to the grass (`regia_terrain::grass::GrassWake`) so blades
//!    flick as the front passes.
//! 2. **Spray trail.** [`spray`] lifts dust or snow puffs from the ground where the front
//!    arrives with a gust over the lift threshold. Each ground cell lifts at most once per pass,
//!    on the frame the front reaches it.
//! 3. **Proximity.** [`proximity`] answers "what is within R of the camera path" over a time
//!    window, for any list of points (scatter, props, creatures). Agent call:
//!    `camera_body.proximity`.
//! 4. **Clearance guard.** [`clearance`] sweeps the planned path against the ground and against
//!    tree trunks and crowns, and reports violations. It moves nothing unless asked: with
//!    [`clearance::lift_shot`] it proposes raised keys. Agent call: `camera_body.clearance`.
//!
//! Loose props get the same wake as a drag impulse (`push`): every dynamic `regia_physics`
//! body in range receives `½ ρ C_d A |u|² û · dt` per fixed step, as an
//! [`ExternalImpulse`](regia_physics::ExternalImpulse).
//!
//! Everything is a pure function of the path and the clock: no wall clock and no unseeded
//! randomness. The same render-out gives the same frames.

pub mod clearance;
pub mod proximity;
mod push;
pub mod spray;

use regia_app::prelude::*;
use regia_core::math::Vec3;
use regia_core::math::wake::{WAKE_GPU_ROWS, WakeBody, WakeSample, WakeShape};
use regia_scene::GlobalTransform;
use serde::{Deserialize, Serialize};

use crate::shots::{ShotDriveSet, ShotPlayer, ShotSequence};

pub use push::{PushSpec, reference_radius};
pub use spray::{SprayGround, SpraySpec, SpraySurface};

/// Format version of a camera-body spec document (`camera_body.json`).
pub const CAMERA_BODY_SPEC_VERSION: u32 = 1;

/// Default time step for differentiating a path into a velocity, seconds.
pub const DEFAULT_VELOCITY_STEP_S: f64 = 1.0 / 240.0;

/// Everything the camera body does. Every field has a default; a spec document may name only
/// what it changes.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CameraBodySpec {
    /// Format version ([`CAMERA_BODY_SPEC_VERSION`]).
    pub version: u32,
    /// The body exists at all. Off: no wake, no spray, no pushes.
    pub enabled: bool,
    /// The wake's shape and strength (radius, speed threshold, range, pulse).
    pub wake: WakeShape,
    /// Loose rigid bodies get the wake as a drag impulse.
    pub push: PushSpec,
    /// Dust or snow lifted from the ground behind fast low passes.
    pub spray: SpraySpec,
    /// Time step for the path's velocity (central difference), seconds.
    pub velocity_step_s: f64,
}

impl Default for CameraBodySpec {
    fn default() -> Self {
        Self {
            version: CAMERA_BODY_SPEC_VERSION,
            enabled: true,
            wake: WakeShape::default(),
            push: PushSpec::default(),
            spray: SpraySpec::default(),
            velocity_step_s: DEFAULT_VELOCITY_STEP_S,
        }
    }
}

/// Why a camera-body spec was refused.
#[derive(Debug, thiserror::Error)]
pub enum CameraBodySpecError {
    /// The document is not valid JSON for the spec.
    #[error("camera body spec: {0}")]
    Parse(String),
    /// The version is newer than this build reads.
    #[error("camera body spec version {found} is newer than {CAMERA_BODY_SPEC_VERSION}")]
    Version {
        /// The version in the document.
        found: u32,
    },
}

impl CameraBodySpec {
    /// Parses a JSON document (unknown fields are refused) and clamps every value into range.
    ///
    /// # Errors
    ///
    /// [`CameraBodySpecError`] when the text does not parse or its version is newer.
    pub fn from_json(text: &str) -> Result<Self, CameraBodySpecError> {
        let spec: Self = serde_json::from_str(text).map_err(|e| CameraBodySpecError::Parse(e.to_string()))?;
        if spec.version > CAMERA_BODY_SPEC_VERSION {
            return Err(CameraBodySpecError::Version { found: spec.version });
        }
        Ok(spec.clamped())
    }

    /// Every value clamped into its documented range.
    #[must_use]
    pub fn clamped(mut self) -> Self {
        self.version = CAMERA_BODY_SPEC_VERSION;
        self.wake = self.wake.clamped();
        self.push = self.push.clamped();
        self.spray = self.spray.clamped();
        self.velocity_step_s = if self.velocity_step_s.is_finite() {
            self.velocity_step_s.clamp(1.0e-4, 0.1)
        } else {
            DEFAULT_VELOCITY_STEP_S
        };
        self
    }
}

/// A camera path the body can follow: positions over a span of time.
pub trait PathSampler {
    /// The path's position at `time` seconds (`None` outside a path that has no pose there).
    fn position(&self, time: f64) -> Option<Vec3>;

    /// The span of time the path covers, seconds.
    fn span(&self) -> (f64, f64);

    /// Whether `a` and `b` lie on one continuous stretch of the path (no cut between them).
    fn continuous(&self, a: f64, b: f64) -> bool {
        let _ = (a, b);
        true
    }

    /// Position and velocity at `time`: a central difference over `step` seconds, one-sided at
    /// the ends of the span and at cuts.
    fn body(&self, time: f64, step: f64) -> Option<WakeBody> {
        let (start, end) = self.span();
        let time = time.clamp(start, end.max(start));
        let position = self.position(time)?;
        let step = step.max(1.0e-6);
        let mut lo = (time - step).max(start);
        let mut hi = (time + step).min(end);
        if !self.continuous(lo, time) {
            lo = time;
        }
        if !self.continuous(time, hi) {
            hi = time;
        }
        let velocity = if hi > lo {
            match (self.position(lo), self.position(hi)) {
                (Some(a), Some(b)) => (b - a) / (hi - lo) as f32,
                _ => Vec3::ZERO,
            }
        } else {
            Vec3::ZERO
        };
        Some(WakeBody { position, velocity })
    }
}

impl PathSampler for ShotSequence {
    fn position(&self, time: f64) -> Option<Vec3> {
        self.sample(time).map(|s| s.eye)
    }

    fn span(&self) -> (f64, f64) {
        (0.0, self.duration())
    }

    fn continuous(&self, a: f64, b: f64) -> bool {
        match (self.locate(a), self.locate(b)) {
            (Some((sa, _)), Some((sb, _))) => sa == sb,
            _ => false,
        }
    }
}

/// A path given as a function of time over a span: for hosts and tools whose camera is not a
/// [`ShotSequence`] (a continuous flight compiled elsewhere, a test).
pub struct FnPath<F: Fn(f64) -> Vec3> {
    /// Position at a time.
    pub position: F,
    /// Covered span, seconds.
    pub span: (f64, f64),
}

impl<F: Fn(f64) -> Vec3> PathSampler for FnPath<F> {
    fn position(&self, time: f64) -> Option<Vec3> {
        Some((self.position)(time))
    }

    fn span(&self) -> (f64, f64) {
        self.span
    }
}

/// Marks the entity the body follows when no shot sequence is loaded (velocity from the change
/// of its world position over the frame's time step).
#[derive(Component, Clone, Copy, Debug, Default)]
pub struct CameraBodyFollow;

/// Where the body's state came from this frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CameraBodySource {
    /// No state yet (or the body is disabled).
    #[default]
    None,
    /// The loaded shot sequence at the player's clock.
    Sequence,
    /// An entity marked [`CameraBodyFollow`].
    Follow,
    /// Set by the host with [`CameraBody::drive`].
    Host,
}

/// What the body did last frame and step: for `camera_body.get` and tests.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct CameraBodyReport {
    /// Where the state came from.
    pub source: CameraBodySource,
    /// The clock of the state, seconds.
    pub time_s: f64,
    /// Position now, metres.
    pub position: [f32; 3],
    /// Velocity now, m/s.
    pub velocity: [f32; 3],
    /// Speed now, m/s.
    pub speed_m_s: f32,
    /// Mach number now.
    pub mach: f32,
    /// The gust at the body now (`gain · ramp · U`), m/s: 0 below the speed threshold.
    pub body_gust_m_s: f32,
    /// Frames the body has been updated.
    pub frames: u64,
    /// Rigid bodies pushed on the last fixed step.
    pub pushed_bodies: u32,
    /// Total impulse handed out on the last fixed step, N·s.
    pub pushed_impulse: f32,
    /// Spray puffs lifted last frame.
    pub spray_puffs: u32,
    /// Spray puffs lifted since the body was created.
    pub spray_total: u64,
}

/// The camera body (see the module docs). Insert it (or add [`CameraBodyPlugin`]) and set its
/// spec; it follows the shot player or a [`CameraBodyFollow`] entity.
#[derive(Resource, Clone, Debug, Default)]
pub struct CameraBody {
    spec: CameraBodySpec,
    current: Option<WakeBody>,
    previous: Option<WakeBody>,
    time: f64,
    host_driven: bool,
    report: CameraBodyReport,
    /// Clock of the last spray frame, and whether there was one since the last reset.
    spray_time: f64,
    spray_primed: bool,
}

impl CameraBody {
    /// A body with this spec (clamped).
    #[must_use]
    pub fn new(spec: CameraBodySpec) -> Self {
        Self {
            spec: spec.clamped(),
            ..Self::default()
        }
    }

    /// The spec in effect.
    #[must_use]
    pub fn spec(&self) -> &CameraBodySpec {
        &self.spec
    }

    /// Replaces the spec (clamped). The state is kept.
    pub fn set_spec(&mut self, spec: CameraBodySpec) {
        self.spec = spec.clamped();
    }

    /// The body now (`None` before the first update or while disabled).
    #[must_use]
    pub fn state(&self) -> Option<WakeBody> {
        self.current.filter(|_| self.spec.enabled)
    }

    /// The body the frame before (the current state on the first frame).
    #[must_use]
    pub fn previous(&self) -> Option<WakeBody> {
        self.previous.or(self.current).filter(|_| self.spec.enabled)
    }

    /// The clock of the current state, seconds.
    #[must_use]
    pub fn time(&self) -> f64 {
        self.time
    }

    /// What the body did last frame.
    #[must_use]
    pub fn report(&self) -> &CameraBodyReport {
        &self.report
    }

    /// Moves the body to `body` at `time` seconds, as the host's own camera driver; the
    /// previous state becomes the frame before. While host-driven the body ignores the shot
    /// player and [`CameraBodyFollow`]; [`Self::release`] hands it back.
    pub fn drive(&mut self, time: f64, body: WakeBody) {
        self.host_driven = true;
        self.advance(time, body, CameraBodySource::Host);
    }

    /// Ends [`Self::drive`]: the body follows the shot player or the marked entity again.
    pub fn release(&mut self) {
        self.host_driven = false;
    }

    /// Drops the state (a cut, a seek backwards): the next update starts fresh, with no
    /// previous frame.
    pub fn reset(&mut self) {
        self.current = None;
        self.previous = None;
        self.spray_primed = false;
        self.report.source = CameraBodySource::None;
    }

    fn advance(&mut self, time: f64, body: WakeBody, source: CameraBodySource) {
        self.previous = self.current.or(Some(body));
        self.current = Some(body);
        self.time = time;
        let speed = body.velocity.length();
        self.report.source = source;
        self.report.time_s = time;
        self.report.position = body.position.to_array();
        self.report.velocity = body.velocity.to_array();
        self.report.speed_m_s = speed;
        self.report.mach = speed / self.spec.wake.speed_of_sound_m_s;
        self.report.body_gust_m_s = self.spec.wake.body_gust(speed);
        self.report.frames += 1;
    }

    /// The wake at `point` now (zero gust while there is no state).
    #[must_use]
    pub fn wake_at(&self, point: Vec3) -> WakeSample {
        match self.state() {
            Some(body) => self.spec.wake.sample(&body, point),
            None => WakeSample::default(),
        }
    }

    /// The wake packed for shaders ([`WakeShape::gpu_words`]): `None` while there is no state
    /// or the body makes no wake now or the frame before.
    #[must_use]
    pub fn wake_words(&self) -> Option<[[f32; 4]; WAKE_GPU_ROWS]> {
        let (now, before) = (self.state()?, self.previous()?);
        let words = self.spec.wake.gpu_words(&now, &before);
        (words[5][2] > 0.0).then_some(words)
    }

    /// The body's state at `time` on `path` with this spec's velocity step.
    #[must_use]
    pub fn sample_path(&self, path: &dyn PathSampler, time: f64) -> Option<WakeBody> {
        path.body(time, self.spec.velocity_step_s)
    }
}

/// The system set of the body's update (`PostUpdate`, after the shot player has moved the
/// camera). Order readers `.after(CameraBodySet)`.
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub struct CameraBodySet;

/// Follows the shot player's clock (or the marked entity) into [`CameraBody`].
#[tracing::instrument(skip_all, target = "regia_framework", name = "camera_body_update")]
fn update_camera_body(
    time: Res<Time>,
    player: Option<Res<ShotPlayer>>,
    mut body: ResMut<CameraBody>,
    follow: Query<&GlobalTransform, With<CameraBodyFollow>>,
) {
    if !body.spec.enabled || body.host_driven {
        return;
    }
    if let Some(player) = player.as_deref()
        && let Some(sequence) = player.sequence()
    {
        let t = player.time();
        if t < body.time && body.report.source == CameraBodySource::Sequence {
            // Seek backwards: no previous frame to blend from.
            body.reset();
        }
        if let Some(state) = sequence.body(t, body.spec.velocity_step_s) {
            body.advance(t, state, CameraBodySource::Sequence);
        }
        return;
    }
    let Some(transform) = follow.iter().next() else {
        return;
    };
    let position = transform.translation();
    let dt = f64::from(time.delta_secs());
    let velocity = match body.current {
        Some(prev) if dt > 0.0 && body.report.source == CameraBodySource::Follow => {
            (position - prev.position) / dt as f32
        }
        Some(prev) if dt <= 0.0 => prev.velocity,
        _ => Vec3::ZERO,
    };
    let t = body.time + dt;
    body.advance(t, WakeBody { position, velocity }, CameraBodySource::Follow);
}

/// Adds [`CameraBody`] (default spec unless one is inserted first), its update after the shot
/// player, the spray trail and the rigid-body push.
pub struct CameraBodyPlugin;

impl Plugin for CameraBodyPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<CameraBody>()
            .init_resource::<SprayGround>()
            .configure_sets(PostUpdate, CameraBodySet.after(ShotDriveSet))
            .add_systems(
                PostUpdate,
                (update_camera_body, spray::emit_spray)
                    .chain()
                    .in_set(CameraBodySet)
                    .before(PostUpdateSet::TransformPropagate),
            )
            .add_systems(
                FixedUpdate,
                push::push_rigid_bodies.before(regia_physics::PhysicsStep),
            );
    }
}

#[cfg(test)]
mod tests;
