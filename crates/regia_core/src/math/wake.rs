// SPDX-License-Identifier: MPL-2.0
//! Air wake of a fast body in still air, in closed form (ADR-1502): the pressure front a moving
//! body drags behind it and the gust that front drives. Foliage, loose props, ground spray and
//! creatures read it to react to a camera that flies past.
//!
//! The body moves at speed `U` along the unit direction `d` through air with sound speed `c`.
//!
//! - **Front.** Above the speed of sound the front is the Mach cone behind the body, with half
//!   angle `μ = asin(c / U)`. Below it the front is the plane through the body normal to `d`
//!   (`μ = 90°`): a point is reached as the body passes it.
//! - **Arrival.** A point `x` metres behind the body along its axis and `r` metres off the axis
//!   lies `s = x sin μ − r cos μ` inside the front (`s < 0`: not reached yet). The front moves
//!   along its normal at `U sin μ` (`c` above Mach 1, `U` below), so the point was reached
//!   `s / (U sin μ)` seconds ago.
//! - **Strength.** The peak gust is `gain · ramp(U) · U · (R / (R + r))^n`, where `R` is the body
//!   radius and `n` defaults to ¾. That exponent is the far-field decay of a supersonic body's
//!   N-wave (Whitham 1952; Carlson, NASA TP-1122, 1978). The ramp is a smoothstep from
//!   `min_speed` to `full_speed`, and the gust fades to zero over the last 40 % of `range_m`.
//! - **Pulse.** In time the gust follows the Friedlander positive phase `(1 − ξ) e^(−ξ)`, with
//!   `ξ` the time since arrival over `pulse_s`. It is a sharp flick that eases back, with a small
//!   suction overshoot, and is cut after [`WAKE_TAIL_PULSES`] pulses.
//! - **Direction.** The gust points along the front normal `d sin μ + r̂ cos μ`: air behind a
//!   shock moves along the shock's normal. Below Mach 1 that normal is `d` (air dragged along).
//!   The radial share is held at least at `radial_share` so a slow pass also pushes things
//!   aside.
//!
//! A real boom's positive phase lasts milliseconds, far too short to move a blade of grass.
//! This model keeps the boom's geometry (arrival on the Mach cone, ¾-power decay, direction) and
//! stretches the pulse to `pulse_s`, a vegetation response time, so the reaction reads on screen.
//! It is a stylization with physical bones, and `gain` sets how strong it is. ADR-1502 records
//! the defaults and why.
//!
//! The straight-line front is exact for a body on a straight path and a close approximation on
//! the eased flights it serves: only the last [`WAKE_TAIL_PULSES`] × `pulse_s` seconds of travel
//! matter. [`WakeShape::gpu_words`] packs the same formula for shaders.
//! `regia_terrain`'s grass mirrors it in WGSL and keeps a CPU twin that is tested against this
//! module.

use glam::Vec3;
use serde::{Deserialize, Serialize};

/// Pulses after the arrival at which the gust is cut (the Friedlander tail is −1.2 % there).
pub const WAKE_TAIL_PULSES: f32 = 6.0;

/// `vec4` rows in [`WakeShape::gpu_words`].
pub const WAKE_GPU_ROWS: usize = 6;

/// Speeds below this are no motion at all (m/s): a hold has no direction.
const STILL_M_S: f32 = 1.0e-3;

/// The shape and strength of a body's wake. Every field has a physical or visual meaning; the
/// defaults suit a camera-sized body (ADR-1502).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WakeShape {
    /// Body radius `R`, metres: the distance at which the falloff halves the near-field gust
    /// (to `0.5^n`).
    pub radius_m: f32,
    /// Below this speed there is no wake at all, m/s.
    pub min_speed_m_s: f32,
    /// The wake reaches full strength at this speed (smoothstep from `min_speed_m_s`), m/s.
    pub full_speed_m_s: f32,
    /// Peak gust per metre per second of body speed at the body (`0..=1`).
    pub gain: f32,
    /// Exponent `n` of the lateral falloff `(R / (R + r))^n`; ¾ is the N-wave far-field law.
    pub falloff_exponent: f32,
    /// Lateral distance from the path at which the gust has faded to zero, metres. It fades out
    /// over the last 40 %.
    pub range_m: f32,
    /// Length of the gust pulse, seconds: the `T` of `(1 − τ/T) e^(−τ/T)`.
    pub pulse_s: f32,
    /// Ceiling of the gust, m/s.
    pub max_gust_m_s: f32,
    /// Speed of sound, m/s (343 m/s is dry air at 20 °C).
    pub speed_of_sound_m_s: f32,
    /// Smallest radial (outward) share of the gust direction, `0..=1`. Below Mach 1 the front's
    /// normal is the direction of travel; this keeps a slow pass pushing things aside too.
    pub radial_share: f32,
}

impl Default for WakeShape {
    fn default() -> Self {
        Self {
            radius_m: 1.5,
            min_speed_m_s: 8.0,
            full_speed_m_s: 40.0,
            gain: 0.12,
            falloff_exponent: 0.75,
            range_m: 150.0,
            pulse_s: 0.3,
            max_gust_m_s: 30.0,
            speed_of_sound_m_s: 343.0,
            radial_share: 0.6,
        }
    }
}

/// Where the body is and how it moves, at one instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WakeBody {
    /// Position, metres.
    pub position: Vec3,
    /// Velocity, metres per second.
    pub velocity: Vec3,
}

/// The wake at one point and instant.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize)]
pub struct WakeSample {
    /// Air velocity of the gust, m/s (zero ahead of the front, out of range or below the
    /// speed threshold).
    pub gust: Vec3,
    /// The gust's peak at this point (before the pulse shape), m/s.
    pub peak_m_s: f32,
    /// Seconds since the front reached the point; negative while it is still ahead.
    pub since_front_s: f32,
    /// Distance from the body's axis of travel, metres.
    pub lateral_m: f32,
    /// Distance behind the body along its axis, metres (negative: ahead of it).
    pub behind_m: f32,
}

/// The Friedlander positive-phase shape `(1 − x) e^(−x)` for `x = τ / T ≥ 0`, faded to zero
/// between 4 and [`WAKE_TAIL_PULSES`] so the cut is smooth. Zero before the arrival.
#[must_use]
pub fn friedlander_pulse(x: f32) -> f32 {
    if !(0.0..WAKE_TAIL_PULSES).contains(&x) {
        return 0.0;
    }
    (1.0 - x) * (-x).exp() * (1.0 - smoothstep(4.0, WAKE_TAIL_PULSES, x))
}

/// Hermite smoothstep of `x` from `lo` to `hi` (0 below, 1 above).
#[must_use]
pub fn smoothstep(lo: f32, hi: f32, x: f32) -> f32 {
    if hi <= lo {
        return if x >= hi { 1.0 } else { 0.0 };
    }
    let t = ((x - lo) / (hi - lo)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

impl WakeShape {
    /// Every field clamped into its documented range; non-finite values take the default.
    #[must_use]
    pub fn clamped(self) -> Self {
        let d = Self::default();
        let fix =
            |v: f32, lo: f32, hi: f32, default: f32| if v.is_finite() { v.clamp(lo, hi) } else { default };
        let min_speed = fix(self.min_speed_m_s, 0.0, 10_000.0, d.min_speed_m_s);
        Self {
            radius_m: fix(self.radius_m, 0.01, 100.0, d.radius_m),
            min_speed_m_s: min_speed,
            full_speed_m_s: fix(
                self.full_speed_m_s,
                min_speed,
                10_000.0,
                d.full_speed_m_s.max(min_speed),
            ),
            gain: fix(self.gain, 0.0, 1.0, d.gain),
            falloff_exponent: fix(self.falloff_exponent, 0.0, 4.0, d.falloff_exponent),
            range_m: fix(self.range_m, 0.0, 10_000.0, d.range_m),
            pulse_s: fix(self.pulse_s, 0.01, 10.0, d.pulse_s),
            max_gust_m_s: fix(self.max_gust_m_s, 0.0, 1_000.0, d.max_gust_m_s),
            speed_of_sound_m_s: fix(self.speed_of_sound_m_s, 1.0, 10_000.0, d.speed_of_sound_m_s),
            radial_share: fix(self.radial_share, 0.0, 1.0, d.radial_share),
        }
    }

    /// The speed ramp: 0 below `min_speed_m_s`, 1 from `full_speed_m_s` up.
    #[must_use]
    pub fn speed_ramp(&self, speed: f32) -> f32 {
        if speed < self.min_speed_m_s || speed <= STILL_M_S {
            return 0.0;
        }
        smoothstep(self.min_speed_m_s, self.full_speed_m_s, speed)
    }

    /// `sin μ` of the front: `c / U` above the speed of sound, 1 below.
    #[must_use]
    pub fn front_sin(&self, speed: f32) -> f32 {
        if speed <= self.speed_of_sound_m_s {
            1.0
        } else {
            self.speed_of_sound_m_s / speed
        }
    }

    /// The gust amplitude at the body for this speed, m/s: `gain · ramp(U) · U` (before the
    /// lateral falloff and the ceiling).
    #[must_use]
    pub fn body_gust(&self, speed: f32) -> f32 {
        self.gain * self.speed_ramp(speed) * speed
    }

    /// The fade of the gust toward `range_m`: 1 up to 60 % of the range, 0 at the range.
    #[must_use]
    pub fn range_fade(&self, lateral: f32) -> f32 {
        if lateral >= self.range_m {
            return 0.0;
        }
        1.0 - smoothstep(0.6 * self.range_m, self.range_m, lateral)
    }

    /// The peak gust `lateral` metres off the path of a body at `speed`, m/s.
    #[must_use]
    pub fn peak_gust(&self, speed: f32, lateral: f32) -> f32 {
        let lateral = lateral.max(0.0);
        let falloff = (self.radius_m / (self.radius_m + lateral)).powf(self.falloff_exponent);
        (self.body_gust(speed) * falloff * self.range_fade(lateral)).min(self.max_gust_m_s)
    }

    /// The wake of `body` at `point` (see the module docs). Pure: the same inputs give the same
    /// sample.
    #[must_use]
    pub fn sample(&self, body: &WakeBody, point: Vec3) -> WakeSample {
        let speed = body.velocity.length();
        let rel = point - body.position;
        if !speed.is_finite() || speed <= STILL_M_S {
            return WakeSample {
                lateral_m: rel.length(),
                since_front_s: f32::NEG_INFINITY,
                ..WakeSample::default()
            };
        }
        let dir = body.velocity / speed;
        let along = rel.dot(dir);
        let radial = rel - dir * along;
        let lateral = radial.length();
        let behind = -along;
        let sin_mu = self.front_sin(speed);
        let cos_mu = (1.0 - sin_mu * sin_mu).max(0.0).sqrt();
        let inside = behind * sin_mu - lateral * cos_mu;
        let since = inside / (speed * sin_mu);
        let peak = self.peak_gust(speed, lateral);
        let mut sample = WakeSample {
            gust: Vec3::ZERO,
            peak_m_s: peak,
            since_front_s: since,
            lateral_m: lateral,
            behind_m: behind,
        };
        if peak <= 0.0 || since < 0.0 {
            return sample;
        }
        let pulse = friedlander_pulse(since / self.pulse_s);
        if pulse == 0.0 {
            return sample;
        }
        let outward = if lateral > 1.0e-4 {
            radial / lateral
        } else {
            dir.any_orthonormal_vector()
        };
        let direction = (dir * sin_mu + outward * cos_mu.max(self.radial_share)).normalize_or_zero();
        sample.gust = direction * (peak * pulse);
        sample
    }

    /// Seconds from the body's position at `body` until its front reaches `point`, if it moves on
    /// in a straight line at its present velocity (0 when the point is already inside the
    /// front; `None` when the body is still or never reaches it).
    #[must_use]
    pub fn time_to_front(&self, body: &WakeBody, point: Vec3) -> Option<f32> {
        let speed = body.velocity.length();
        if !speed.is_finite() || speed <= STILL_M_S {
            return None;
        }
        let s = self.sample(body, point);
        if s.since_front_s >= 0.0 {
            return Some(0.0);
        }
        // Moving on at the same velocity, the inside distance grows at U sin μ per second.
        Some(-s.since_front_s)
    }

    /// The formula packed for a shader, [`WAKE_GPU_ROWS`] `vec4` rows:
    ///
    /// | row | x, y, z | w |
    /// |---|---|---|
    /// | 0 | body position now (m) | body gust now, `gain · ramp · U` (m/s) |
    /// | 1 | body velocity now (m/s) | `sin μ` now |
    /// | 2 | body position the frame before | body gust then |
    /// | 3 | body velocity the frame before | `sin μ` then |
    /// | 4 | radius, falloff exponent, range (m) | pulse (s) |
    /// | 5 | max gust (m/s), radial share, enabled (1/0) | 0 |
    ///
    /// The previous state lets a velocity pass move foliage between the two poses (TAA and
    /// motion blur see the flick). `enabled` is 0 when neither state makes a wake.
    #[must_use]
    pub fn gpu_words(&self, current: &WakeBody, previous: &WakeBody) -> [[f32; 4]; WAKE_GPU_ROWS] {
        let state = |b: &WakeBody| {
            let speed = b.velocity.length();
            let (gust, sin_mu) = if speed.is_finite() && speed > STILL_M_S {
                (self.body_gust(speed), self.front_sin(speed))
            } else {
                (0.0, 1.0)
            };
            (
                [b.position.x, b.position.y, b.position.z, gust],
                [b.velocity.x, b.velocity.y, b.velocity.z, sin_mu],
            )
        };
        let (c0, c1) = state(current);
        let (p0, p1) = state(previous);
        let enabled = if (c0[3] > 0.0 || p0[3] > 0.0) && self.max_gust_m_s > 0.0 && self.range_m > 0.0 {
            1.0
        } else {
            0.0
        };
        [
            c0,
            c1,
            p0,
            p1,
            [self.radius_m, self.falloff_exponent, self.range_m, self.pulse_s],
            [self.max_gust_m_s, self.radial_share, enabled, 0.0],
        ]
    }
}

/// The gust a shader computes from [`WakeShape::gpu_words`] for the state in rows `pos_row` and
/// `pos_row + 1` (0 = now, 2 = the frame before), on the CPU. Shader mirrors test against this:
/// it must equal [`WakeShape::sample`]'s gust for the same state.
#[must_use]
pub fn gust_from_words(words: &[[f32; 4]; WAKE_GPU_ROWS], pos_row: usize, point: Vec3) -> Vec3 {
    let [pos, vel, shape, misc] = [words[pos_row], words[pos_row + 1], words[4], words[5]];
    if misc[2] <= 0.0 || pos[3] <= 0.0 {
        return Vec3::ZERO;
    }
    let velocity = Vec3::new(vel[0], vel[1], vel[2]);
    let speed = velocity.length();
    if speed <= STILL_M_S {
        return Vec3::ZERO;
    }
    let dir = velocity / speed;
    let rel = point - Vec3::new(pos[0], pos[1], pos[2]);
    let along = rel.dot(dir);
    let radial = rel - dir * along;
    let lateral = radial.length();
    let (radius, falloff, range, pulse_s) = (shape[0], shape[1], shape[2], shape[3]);
    if lateral >= range {
        return Vec3::ZERO;
    }
    let sin_mu = vel[3];
    let cos_mu = (1.0 - sin_mu * sin_mu).max(0.0).sqrt();
    let since = (-along * sin_mu - lateral * cos_mu) / (speed * sin_mu);
    if since < 0.0 {
        return Vec3::ZERO;
    }
    let fade = 1.0 - smoothstep(0.6 * range, range, lateral);
    let peak = (pos[3] * (radius / (radius + lateral)).powf(falloff) * fade).min(misc[0]);
    let outward = if lateral > 1.0e-4 {
        radial / lateral
    } else {
        dir.any_orthonormal_vector()
    };
    let direction = (dir * sin_mu + outward * cos_mu.max(misc[1])).normalize_or_zero();
    direction * (peak * friedlander_pulse(since / pulse_s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(speed: f32) -> WakeBody {
        WakeBody {
            position: Vec3::new(0.0, 50.0, 0.0),
            velocity: Vec3::new(0.0, 0.0, -speed),
        }
    }

    /// First arrival of a sound disturbance emitted along a straight path `x(t) = p + v t`
    /// (`t ≤ 0`), by brute force: `min over τ ≤ 0 of τ + |P − x(τ)| / c`. Above Mach 1 its
    /// envelope is the Mach cone, so the closed form must agree.
    fn brute_arrival(b: &WakeBody, point: Vec3, c: f32) -> f32 {
        let (p, v, q) = (b.position.as_dvec3(), b.velocity.as_dvec3(), point.as_dvec3());
        let mut best = f64::INFINITY;
        for i in 0..=200_000u32 {
            let tau = -20.0 + f64::from(i) * 1.0e-4;
            best = best.min(tau + q.distance(p + v * tau) / f64::from(c));
        }
        best as f32
    }

    #[test]
    fn supersonic_front_is_the_mach_cone() {
        let shape = WakeShape::default();
        let b = body(750.0);
        for point in [
            Vec3::new(0.0, 0.0, 300.0),
            Vec3::new(40.0, 0.0, 600.0),
            Vec3::new(-20.0, 10.0, 150.0),
        ] {
            let s = shape.sample(&b, point);
            // The point was reached `since` seconds ago: the brute arrival is at -since.
            let brute = brute_arrival(&b, point, shape.speed_of_sound_m_s);
            assert!(
                (s.since_front_s + brute).abs() < 2.0e-3,
                "{point}: {} vs {brute}",
                s.since_front_s
            );
        }
    }

    #[test]
    fn subsonic_front_is_the_plane_through_the_body() {
        let shape = WakeShape::default();
        let b = body(40.0);
        // 20 m behind the body: passed half a second ago, whatever the lateral distance.
        for lateral in [0.0, 5.0, 60.0] {
            let s = shape.sample(&b, Vec3::new(lateral, 50.0, 20.0));
            assert!((s.since_front_s - 0.5).abs() < 1.0e-5);
        }
        // Ahead of the body: not reached.
        let ahead = shape.sample(&b, Vec3::new(3.0, 50.0, -10.0));
        assert!(ahead.since_front_s < 0.0);
        assert_eq!(ahead.gust, Vec3::ZERO);
    }

    #[test]
    fn no_wake_below_the_speed_threshold_or_out_of_range() {
        let shape = WakeShape::default();
        let near = Vec3::new(2.0, 50.0, 0.5);
        assert_eq!(shape.sample(&body(5.0), near).gust, Vec3::ZERO);
        assert_eq!(shape.sample(&body(0.0), near).gust, Vec3::ZERO);
        assert!(shape.sample(&body(30.0), near).gust.length() > 0.0);
        // Out of range: nothing, at any speed.
        let far = Vec3::new(shape.range_m + 1.0, 50.0, 400.0);
        assert_eq!(shape.sample(&body(750.0), far).gust, Vec3::ZERO);
    }

    #[test]
    fn peak_falls_with_distance_and_rises_with_speed() {
        let shape = WakeShape::default();
        let mut last = f32::INFINITY;
        for lateral in [0.0, 2.0, 10.0, 50.0, 100.0, 140.0] {
            let p = shape.peak_gust(300.0, lateral);
            assert!(p < last || p == shape.max_gust_m_s, "{lateral}: {p} after {last}");
            last = p;
        }
        assert_eq!(shape.peak_gust(300.0, shape.range_m), 0.0);
        let mut last = 0.0;
        for speed in [10.0, 20.0, 40.0, 100.0, 400.0, 750.0] {
            let p = shape.peak_gust(speed, 50.0);
            assert!(p > last, "{speed}: {p}");
            last = p;
        }
        // The showcase pass: 750 m/s, 50 m off the path, a few m/s.
        let p = shape.peak_gust(750.0, 50.0);
        assert!((3.0..12.0).contains(&p), "{p}");
    }

    #[test]
    fn gust_points_along_the_front_normal() {
        let shape = WakeShape::default();
        let b = body(750.0);
        // A point to the +X side, well inside the cone, right after the front.
        let sin_mu = shape.front_sin(750.0);
        let cos_mu = (1.0 - sin_mu * sin_mu).sqrt();
        let lateral = 30.0;
        let behind = lateral * cos_mu / sin_mu + 1.0;
        let s = shape.sample(&b, Vec3::new(lateral, 50.0, behind));
        assert!(s.since_front_s > 0.0 && s.gust.length() > 0.0);
        let g = s.gust.normalize();
        // Forward (-Z) by sin μ, outward (+X) by cos μ.
        assert!(
            (g.x - cos_mu).abs() < 1.0e-3 && (g.z + sin_mu).abs() < 1.0e-3,
            "{g}"
        );
    }

    #[test]
    fn pulse_is_a_flick_with_a_small_overshoot() {
        assert_eq!(friedlander_pulse(-0.1), 0.0);
        assert!((friedlander_pulse(0.0) - 1.0).abs() < 1.0e-6);
        assert!(friedlander_pulse(0.5) > 0.0);
        assert!(friedlander_pulse(2.0) < 0.0);
        assert!(friedlander_pulse(2.0) > -0.2);
        assert_eq!(friedlander_pulse(WAKE_TAIL_PULSES), 0.0);
        // Continuous through the tail fade.
        let mut prev = friedlander_pulse(3.9);
        let mut x = 3.9;
        while x < WAKE_TAIL_PULSES {
            x += 0.01;
            let v = friedlander_pulse(x);
            assert!((v - prev).abs() < 0.01);
            prev = v;
        }
    }

    #[test]
    fn gpu_words_give_the_same_gust_as_the_sample() {
        let shape = WakeShape::default();
        for speed in [20.0, 120.0, 750.0] {
            let b = body(speed);
            let words = shape.gpu_words(&b, &b);
            for point in [
                Vec3::new(3.0, 48.0, 2.0),
                Vec3::new(30.0, 0.0, 120.0),
                Vec3::new(-10.0, 45.0, 30.0),
                Vec3::new(0.0, 50.0, 5.0),
            ] {
                let a = shape.sample(&b, point).gust;
                let g = gust_from_words(&words, 0, point);
                assert!((a - g).length() < 1.0e-4, "{speed} {point}: {a} vs {g}");
            }
        }
    }

    #[test]
    fn words_are_disabled_for_a_hold() {
        let shape = WakeShape::default();
        let still = body(0.0);
        let words = shape.gpu_words(&still, &still);
        assert_eq!(words[5][2], 0.0);
        assert_eq!(gust_from_words(&words, 0, Vec3::new(1.0, 50.0, 1.0)), Vec3::ZERO);
    }

    #[test]
    fn time_to_front_counts_down() {
        let shape = WakeShape::default();
        let b = body(100.0);
        let point = Vec3::new(5.0, 50.0, -200.0);
        let t = shape.time_to_front(&b, point).unwrap_or(f32::NAN);
        assert!((t - 2.0).abs() < 1.0e-4, "{t}");
        assert_eq!(shape.time_to_front(&body(0.0), point), None);
    }

    #[test]
    fn clamped_fixes_bad_values() {
        let bad = WakeShape {
            radius_m: f32::NAN,
            gain: 7.0,
            full_speed_m_s: 1.0,
            min_speed_m_s: 10.0,
            ..WakeShape::default()
        }
        .clamped();
        assert_eq!(bad.radius_m, WakeShape::default().radius_m);
        assert_eq!(bad.gain, 1.0);
        assert!(bad.full_speed_m_s >= bad.min_speed_m_s);
    }
}
