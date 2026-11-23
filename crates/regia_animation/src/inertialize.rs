// SPDX-License-Identifier: MPL-2.0
//! Inertialized transitions: switch to the target state at once and decay the pose offset.
//!
//! A cross-fade evaluates the source and the target state for the whole transition and
//! blends them; a short cross-fade between two different poses still pops, because the blend
//! changes the pose fast. An *inertialized* transition instead evaluates **only the target
//! state**, from the first frame, at full weight. At the switch it records, per joint, the
//! offset between the pose that would have been shown (the "source") and the target's pose,
//! plus the offset's velocity, and adds that offset back on top of the target pose while
//! decaying it to zero over the transition's `duration`. Position and velocity carry over the
//! switch, and the transition costs one state evaluation instead of two.
//!
//! Two decays are available ([`DecayMethod`]):
//!
//! - **Quintic** (default, [`crate::state_machine::TransitionBlend::Inertialize`]): the method of
//!   David Bollo, "Inertialization: High-Performance Animation Transitions in *Gears of War*"
//!   (GDC 2018). Each channel's offset is reduced to a magnitude `x0` along a fixed direction
//!   (the rotation offset's axis, or the translation offset's direction) and its velocity
//!   projected onto that direction, `v0`. A fifth-order polynomial
//!   `x(t) = A t⁵ + B t⁴ + C t³ + ½ a0 t² + v0 t + x0` takes it to zero with zero velocity and
//!   acceleration at `t1`. Overshoot is prevented as in the talk: a velocity pointing *away*
//!   from the target (offset growing) is clamped to zero, `t1` is shortened to `−5 x0 / v0` so a
//!   fast approach cannot cross zero, and the initial acceleration
//!   `a0 = (−8 v0 t1 − 20 x0) / t1²` is clamped to be non-negative. The coefficients solve
//!   `x(t1) = x'(t1) = x''(t1) = 0` (re-derived; checked numerically in the tests).
//! - **Spring** ([`crate::state_machine::TransitionBlend::InertializeSpring`]): the offset of
//!   every channel decays as a critically damped spring, as in Daniel Holden's articles on
//!   spring-based inertialization (theorangeduck.com, "Spring-It-On" and "Dead Blending").
//!   The closed form `x(t) = (x0 + (v0 + y x0) t) e^(−y t)` is the textbook solution of
//!   `x'' = −2y x' − y² x` (re-derived, no code ported). It keeps the full 3D offset velocity,
//!   so velocity is continuous at every switch, at the cost of a possible small overshoot.
//!   `y = 7.5 / duration`, leaving < 0.5 % of the offset at `duration`, which is dropped.
//!
//! Rotations are offset on the left (parent space): `offset = source · target⁻¹`, as a scaled
//! axis-angle vector along the shortest arc; the offset's angular velocity is
//! `ω_source − offset · ω_target`. Translations and scales are plain vector differences.
//! Velocities come from the previous frame's output (source) and a side-effect-free
//! evaluation of the target state one frame earlier (target), see `eval.rs`.
//!
//! An interrupting inertialized transition simply starts again from the current output pose
//! (which already contains the running offset) and its velocity, so interruptions are as
//! continuous as first switches.
//!
//! Provenance: `docs/provenance.md` ("Inertialized animation transitions").
//!
//! Every transcendental (`exp`, `atan2`, `sin`, `cos`) goes through
//! [`regia_core::math::det`], so the transition output bits are identical on every
//! platform (audit-v040 M5).

use regia_core::math::{Quat, Vec3, det};

use crate::pose::Pose;

/// Offsets smaller than this (radians, metres, scale units) are treated as zero.
const EPSILON: f32 = 1e-6;

/// Spring decay rate per transition duration: `y = SPRING_RATE / duration`. With 7.5,
/// `(1 + 7.5) e^-7.5 ≈ 0.47 %` of a resting offset remains at `duration`.
pub const SPRING_RATE: f32 = 7.5;

/// How an inertialized offset decays to zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DecayMethod {
    /// Bollo's overshoot-safe quintic polynomial; reaches zero exactly at `duration`.
    #[default]
    Quintic,
    /// Holden-style critically damped spring; full 3D velocity continuity.
    Spring,
}

/// Bollo's quintic decay of a non-negative scalar offset magnitude.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct QuinticDecay {
    /// `[A, B, C, a0/2, v0, x0]`: coefficients of t⁵ … t⁰.
    coefficients: [f32; 6],
    /// Time at which the offset reaches zero (≤ the requested duration).
    t1: f32,
}

impl QuinticDecay {
    /// Plan the decay of magnitude `x0 ≥ 0` with rate `v0` (negative = shrinking) over
    /// `duration` seconds, with Bollo's overshoot clamps.
    #[must_use]
    pub fn new(x0: f32, v0: f32, duration: f32) -> Self {
        if x0 <= EPSILON || duration <= EPSILON || !x0.is_finite() {
            return Self::default();
        }
        // A velocity pointing away from the target would carry the offset further out first;
        // Bollo clamps it to zero.
        let v0 = if v0.is_finite() { v0.min(0.0) } else { 0.0 };
        let mut t1 = duration;
        if v0 < 0.0 {
            // Arriving fast: shorten the decay so the polynomial cannot cross zero.
            t1 = t1.min(-5.0 * x0 / v0);
        }
        let t1 = t1.max(EPSILON);
        let t1_2 = t1 * t1;
        let t1_3 = t1_2 * t1;
        let a0 = ((-8.0 * v0 * t1 - 20.0 * x0) / t1_2).max(0.0);
        let a = -(a0 * t1_2 + 6.0 * v0 * t1 + 12.0 * x0) / (2.0 * t1_3 * t1_2);
        let b = (3.0 * a0 * t1_2 + 16.0 * v0 * t1 + 30.0 * x0) / (2.0 * t1_2 * t1_2);
        let c = -(3.0 * a0 * t1_2 + 12.0 * v0 * t1 + 20.0 * x0) / (2.0 * t1_3);
        Self {
            coefficients: [a, b, c, 0.5 * a0, v0, x0],
            t1,
        }
    }

    /// The offset magnitude `t` seconds after the switch (zero from `t1` on).
    #[must_use]
    pub fn eval(&self, t: f32) -> f32 {
        if t >= self.t1 {
            return 0.0;
        }
        let t = t.max(0.0);
        let [a, b, c, d, e, f] = self.coefficients;
        ((((a * t + b) * t + c) * t + d) * t + e) * t + f
    }

    /// When the offset reaches zero.
    #[must_use]
    pub fn end(&self) -> f32 {
        self.t1
    }
}

/// One channel (a joint's rotation, translation or scale) of an inertialized offset.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
enum Channel {
    /// No offset and no offset velocity (most translation and scale channels): skipped.
    #[default]
    Zero,
    /// Bollo: magnitude decay along a fixed unit direction.
    Quintic { dir: Vec3, decay: QuinticDecay },
    /// Spring: `x(t) = (x0 + j t) e^(−y t)` with `j = v0 + y x0`.
    Spring { x0: Vec3, j: Vec3, y: f32 },
}

impl Channel {
    fn new(method: DecayMethod, offset: Vec3, velocity: Vec3, duration: f32) -> Self {
        match method {
            DecayMethod::Quintic => {
                let x0 = offset.length();
                if x0 <= EPSILON || !x0.is_finite() {
                    return Channel::default();
                }
                let dir = offset / x0;
                Channel::Quintic {
                    dir,
                    decay: QuinticDecay::new(x0, velocity.dot(dir), duration),
                }
            }
            DecayMethod::Spring => {
                if !offset.is_finite() {
                    return Channel::Zero;
                }
                if offset.length_squared() <= EPSILON * EPSILON
                    && velocity.length_squared() <= EPSILON * EPSILON
                {
                    return Channel::Zero;
                }
                let y = SPRING_RATE / duration.max(EPSILON);
                let velocity = if velocity.is_finite() {
                    velocity
                } else {
                    Vec3::ZERO
                };
                Channel::Spring {
                    x0: offset,
                    j: velocity + offset * y,
                    y,
                }
            }
        }
    }

    fn eval(&self, t: f32) -> Vec3 {
        match *self {
            Channel::Zero => Vec3::ZERO,
            Channel::Quintic { dir, decay } => dir * decay.eval(t),
            Channel::Spring { x0, j, y } => (x0 + j * t) * det::exp(-y * t),
        }
    }
}

/// The rotation `q` or `−q`, whichever has `w ≥ 0` (the shortest-arc representative).
fn shortest(q: Quat) -> Quat {
    if q.w < 0.0 { -q } else { q }
}

/// Angular velocity (parent space, rad/s) that takes `prev` to `cur` in `dt`.
fn angular_velocity(cur: Quat, prev: Quat, inv_dt: f32) -> Vec3 {
    det::quat_to_scaled_axis(shortest(cur * prev.inverse())) * inv_dt
}

/// What [`Inertializer::start`] needs to know about the switch.
#[derive(Clone, Copy, Debug)]
pub struct Switch<'a> {
    /// The pose that would be shown this frame without the switch (current output).
    pub source: &'a Pose,
    /// The previous frame's output, for the source velocity (`None` = at rest).
    pub source_prev: Option<&'a Pose>,
    /// The target state's pose this frame.
    pub target: &'a Pose,
    /// The target state's pose one frame earlier (`None` = at rest).
    pub target_prev: Option<&'a Pose>,
    /// Frame time between the `*_prev` poses and this frame, seconds.
    pub dt: f32,
}

/// Runtime state of an inertialized transition: the per-joint offset decays.
///
/// Usage per frame: [`Inertializer::advance`] by the frame delta, then
/// [`Inertializer::apply`] onto the freshly evaluated target pose. A transition calls
/// [`Inertializer::start`] on the switch frame, *after* the source output was computed.
#[derive(Clone, Debug, Default)]
pub struct Inertializer {
    method: DecayMethod,
    duration: f32,
    elapsed: f32,
    active: bool,
    rotations: Vec<Channel>,
    translations: Vec<Channel>,
    scales: Vec<Channel>,
    max_rotation: f32,
    max_translation: f32,
}

impl Inertializer {
    /// Begin (or restart, on interruption) an inertialized transition: records the offset
    /// `source ⊖ target` per joint and its velocity. Applying at elapsed 0 reproduces `source`.
    pub fn start(&mut self, method: DecayMethod, duration: f32, switch: Switch<'_>) {
        let n = switch.source.len().min(switch.target.len());
        self.method = method;
        // NaN and infinite durations cannot be decayed over: the switch is a plain cut.
        self.duration = if duration.is_finite() {
            duration.max(0.0)
        } else {
            0.0
        };
        self.elapsed = 0.0;
        self.active = self.duration > EPSILON && n > 0;
        self.rotations.clear();
        self.translations.clear();
        self.scales.clear();
        self.max_rotation = 0.0;
        self.max_translation = 0.0;
        if !self.active {
            return;
        }
        let inv_dt = if switch.dt > EPSILON { 1.0 / switch.dt } else { 0.0 };
        let source_prev = switch.source_prev.filter(|p| p.len() >= n && inv_dt > 0.0);
        let target_prev = switch.target_prev.filter(|p| p.len() >= n && inv_dt > 0.0);
        let (s, t) = (switch.source, switch.target);
        for i in 0..n {
            // Rotation, parent space: offset · target = source.
            let offset = shortest(s.rotations[i] * t.rotations[i].inverse());
            let w_source = source_prev.map_or(Vec3::ZERO, |p| {
                angular_velocity(s.rotations[i], p.rotations[i], inv_dt)
            });
            let w_target = target_prev.map_or(Vec3::ZERO, |p| {
                angular_velocity(t.rotations[i], p.rotations[i], inv_dt)
            });
            let w_offset = w_source - offset * w_target;
            self.rotations.push(Channel::new(
                method,
                det::quat_to_scaled_axis(offset),
                w_offset,
                self.duration,
            ));

            let v_source =
                source_prev.map_or(Vec3::ZERO, |p| (s.translations[i] - p.translations[i]) * inv_dt);
            let v_target =
                target_prev.map_or(Vec3::ZERO, |p| (t.translations[i] - p.translations[i]) * inv_dt);
            self.translations.push(Channel::new(
                method,
                s.translations[i] - t.translations[i],
                v_source - v_target,
                self.duration,
            ));

            let r_source = source_prev.map_or(Vec3::ZERO, |p| (s.scales[i] - p.scales[i]) * inv_dt);
            let r_target = target_prev.map_or(Vec3::ZERO, |p| (t.scales[i] - p.scales[i]) * inv_dt);
            self.scales.push(Channel::new(
                method,
                s.scales[i] - t.scales[i],
                r_source - r_target,
                self.duration,
            ));
        }
    }

    /// Advance the decay clock by `dt`; ends the transition at `duration`.
    pub fn advance(&mut self, dt: f32) {
        if !self.active {
            return;
        }
        self.elapsed += dt.max(0.0);
        if self.elapsed >= self.duration {
            self.stop();
        }
    }

    /// Drop any running offset.
    pub fn stop(&mut self) {
        self.active = false;
        self.max_rotation = 0.0;
        self.max_translation = 0.0;
    }

    /// Add the current offset onto `pose` (the target state's pose). No-op when inactive.
    pub fn apply(&mut self, pose: &mut Pose) {
        if !self.active {
            return;
        }
        let t = self.elapsed;
        let n = pose.len().min(self.rotations.len());
        let mut max_rotation = 0.0f32;
        let mut max_translation = 0.0f32;
        for i in 0..n {
            if !matches!(self.rotations[i], Channel::Zero) {
                let r = self.rotations[i].eval(t);
                let angle = r.length();
                if angle > EPSILON {
                    // Both factors are unit quaternions; the target pose is resampled every frame,
                    // so no drift accumulates and no renormalisation is needed. The angle is far
                    // above the near-zero cutoff, so building from the axis matches
                    // `Quat::from_scaled_axis` with the platform-stable det trig (audit-v040 M5).
                    pose.rotations[i] = det::quat_from_axis_angle(r / angle, angle) * pose.rotations[i];
                }
                max_rotation = max_rotation.max(angle);
            }
            if !matches!(self.translations[i], Channel::Zero) {
                let d = self.translations[i].eval(t);
                pose.translations[i] += d;
                max_translation = max_translation.max(d.length());
            }
            if !matches!(self.scales[i], Channel::Zero) {
                pose.scales[i] += self.scales[i].eval(t);
            }
        }
        self.max_rotation = max_rotation;
        self.max_translation = max_translation;
    }

    /// Whether an offset is still decaying.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Seconds since the (latest) switch.
    #[must_use]
    pub fn elapsed(&self) -> f32 {
        self.elapsed
    }

    /// Decay time of the running transition.
    #[must_use]
    pub fn duration(&self) -> f32 {
        self.duration
    }

    /// The decay in use.
    #[must_use]
    pub fn method(&self) -> DecayMethod {
        self.method
    }

    /// Largest joint rotation offset applied by the last [`Inertializer::apply`], radians
    /// (0 when inactive). Useful for `wait_until` checks: the transition has settled when this
    /// reaches 0.
    #[must_use]
    pub fn max_rotation_offset(&self) -> f32 {
        self.max_rotation
    }

    /// Largest joint translation offset applied by the last apply, metres.
    #[must_use]
    pub fn max_translation_offset(&self) -> f32 {
        self.max_translation
    }
}

/// Inertialized layer weight: when a layer's weight parameter changes, the weight actually
/// used eases to it with [`QuinticDecay`], carrying its current rate (an aim layer snapped
/// from 0 to 1 no longer pops the upper body). Enabled per layer by
/// [`crate::graph::LayerEntry::weight_inertialize`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct WeightInertia {
    output: f32,
    prev_output: f32,
    target: f32,
    sign: f32,
    decay: QuinticDecay,
    elapsed: f32,
    initialised: bool,
}

impl WeightInertia {
    /// The weight to use this frame for parameter value `target`, easing changes over
    /// `duration` seconds (0 = follow `target` directly). `dt` is the frame delta.
    pub fn update(&mut self, target: f32, duration: f32, dt: f32) -> f32 {
        // A weight computed as `0 / 0` follows as 0; a NaN or infinite duration follows directly.
        let target = crate::num::finite_or(target, 0.0);
        if !(duration > EPSILON && duration.is_finite()) || !self.initialised {
            self.initialised = true;
            self.output = target;
            self.prev_output = target;
            self.target = target;
            self.decay = QuinticDecay::default();
            return target;
        }
        self.elapsed += dt.max(0.0);
        if (target - self.target).abs() > 1e-4 {
            // Re-inertialize from last frame's output and rate; this frame is `dt` into it.
            let x0 = self.output - target;
            let rate = if dt > EPSILON {
                (self.output - self.prev_output) / dt
            } else {
                0.0
            };
            self.sign = if x0 < 0.0 { -1.0 } else { 1.0 };
            self.decay = QuinticDecay::new(x0.abs(), rate * self.sign, duration);
            self.elapsed = dt.max(0.0);
            self.target = target;
        }
        let out = (target + self.sign * self.decay.eval(self.elapsed)).clamp(0.0, 1.0);
        self.prev_output = self.output;
        self.output = out;
        out
    }

    /// The weight used last frame.
    #[must_use]
    pub fn output(&self) -> f32 {
        self.output
    }

    /// Words of a pose snapshot (`pose_eval`, ADR-2551): every field as its bits, so a restored
    /// layer eases exactly as the original.
    pub(crate) const WORDS: usize = 13;

    /// The state as [`Self::WORDS`] words.
    pub(crate) fn to_words(self) -> [u32; Self::WORDS] {
        let c = self.decay.coefficients;
        [
            self.output.to_bits(),
            self.prev_output.to_bits(),
            self.target.to_bits(),
            self.sign.to_bits(),
            c[0].to_bits(),
            c[1].to_bits(),
            c[2].to_bits(),
            c[3].to_bits(),
            c[4].to_bits(),
            c[5].to_bits(),
            self.decay.t1.to_bits(),
            self.elapsed.to_bits(),
            u32::from(self.initialised),
        ]
    }

    /// The state back from [`Self::to_words`].
    pub(crate) fn from_words(w: &[u32; Self::WORDS]) -> Self {
        let f = |i: usize| f32::from_bits(w[i]);
        WeightInertia {
            output: f(0),
            prev_output: f(1),
            target: f(2),
            sign: f(3),
            decay: QuinticDecay {
                coefficients: [f(4), f(5), f(6), f(7), f(8), f(9)],
                t1: f(10),
            },
            elapsed: f(11),
            initialised: w[12] != 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derivatives(d: &QuinticDecay, t: f32) -> (f32, f32, f32) {
        let h = 1e-3;
        let x = d.eval(t);
        let v = (d.eval(t + h) - d.eval(t - h)) / (2.0 * h);
        let a = (d.eval(t + h) - 2.0 * x + d.eval(t - h)) / (h * h);
        (x, v, a)
    }

    #[test]
    fn quintic_meets_start_and_end_conditions() {
        for &(x0, v0, d) in &[
            (1.0, 0.0, 0.3),
            (0.5, -2.0, 0.2),
            (2.0, -1.0, 0.5),
            (0.2, -50.0, 0.3),
        ] {
            let q = QuinticDecay::new(x0, v0, d);
            assert!((q.eval(0.0) - x0).abs() < 1e-5, "x(0) = x0");
            let h = 1e-4;
            let v_start = (q.eval(h) - q.eval(0.0)) / h;
            assert!(
                (v_start - v0).abs() < 0.02 * (1.0 + v0.abs()),
                "x'(0) = v0: {v_start} vs {v0}"
            );
            let t1 = q.end();
            assert!(t1 <= d + 1e-6);
            let (x, v, _) = derivatives(&q, t1 - 2e-3);
            assert!(x.abs() < 1e-4 && v.abs() < 0.05, "ends at rest: x {x} v {v}");
            assert_eq!(q.eval(t1), 0.0);
            // Overshoot-safe: never crosses zero.
            for i in 0..=200 {
                assert!(q.eval(t1 * i as f32 / 200.0) >= -1e-5);
            }
        }
    }

    #[test]
    fn quintic_clamps_velocity_pointing_away() {
        let away = QuinticDecay::new(1.0, 3.0, 0.3);
        let at_rest = QuinticDecay::new(1.0, 0.0, 0.3);
        assert_eq!(away, at_rest, "positive (growing) velocity is clamped to zero");
    }

    #[test]
    fn spring_starts_with_offset_and_velocity_and_settles() {
        let c = Channel::new(DecayMethod::Spring, Vec3::X, Vec3::new(0.0, 2.0, 0.0), 0.2);
        assert!((c.eval(0.0) - Vec3::X).length() < 1e-6);
        let h = 1e-4;
        let v = (c.eval(h) - c.eval(0.0)) / h;
        assert!((v.y - 2.0).abs() < 0.01, "full 3D velocity kept: {v:?}");
        assert!(c.eval(0.2).length() < 0.02);
    }

    #[test]
    fn inertializer_reproduces_source_then_reaches_target() {
        let mut source = Pose::from_identity(2);
        source.rotations[1] = Quat::from_rotation_z(0.8);
        source.translations[0] = Vec3::new(0.3, 0.0, 0.0);
        let target = Pose::from_identity(2);
        for method in [DecayMethod::Quintic, DecayMethod::Spring] {
            let mut inertia = Inertializer::default();
            inertia.start(
                method,
                0.25,
                Switch {
                    source: &source,
                    source_prev: None,
                    target: &target,
                    target_prev: None,
                    dt: 1.0 / 60.0,
                },
            );
            let mut pose = target.clone();
            inertia.apply(&mut pose);
            assert!(pose.rotations[1].angle_between(source.rotations[1]) < 1e-4);
            assert!((pose.translations[0] - source.translations[0]).length() < 1e-6);
            assert!((inertia.max_rotation_offset() - 0.8).abs() < 1e-4);
            inertia.advance(0.25);
            assert!(!inertia.is_active());
            let mut pose = target.clone();
            inertia.apply(&mut pose);
            assert_eq!(pose, target);
        }
    }

    #[test]
    fn weight_inertia_eases_a_snap() {
        let mut w = WeightInertia::default();
        let dt = 1.0 / 60.0;
        assert_eq!(w.update(0.0, 0.2, dt), 0.0);
        let mut last = 0.0;
        let mut steps = Vec::new();
        for _ in 0..20 {
            let v = w.update(1.0, 0.2, dt);
            assert!(v >= last - 1e-6 && v <= 1.0, "monotone, bounded");
            steps.push(v - last);
            last = v;
        }
        assert!((last - 1.0).abs() < 1e-5, "settles at the target");
        assert!(steps[0] < 0.05, "no pop on the first frame: {}", steps[0]);
        // Disabled = follows the parameter.
        let mut off = WeightInertia::default();
        off.update(0.0, 0.0, dt);
        assert_eq!(off.update(1.0, 0.0, dt), 1.0);
    }

    #[test]
    fn spring_channel_with_non_finite_offset_is_zero() {
        let c = Channel::new(
            DecayMethod::Spring,
            Vec3::new(f32::NAN, 0.0, 0.0),
            Vec3::ZERO,
            0.3,
        );
        assert_eq!(c, Channel::Zero);
    }
}
