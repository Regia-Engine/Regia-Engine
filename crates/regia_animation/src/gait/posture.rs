// SPDX-License-Identifier: MPL-2.0
//! Body height and attitude that follow the feet (ADR-8706).
//!
//! The body height target is a weighted mean of every foot's floor: a planted foot counts
//! at its plant height with weight 1, a foot in the air at the ground line under it (its
//! lift-off height blended to its landing height by swing progress) with weight
//! [`GaitPosture::swing_weight`]. The body eases toward the target with time constant
//! [`GaitPosture::height_tau`] and moves at most [`GaitPosture::height_slew`] per second.
//!
//! Pitch and roll come from a least-squares plane `y = a * right + b * forward + c`
//! through the planted feet in the stepper's frame (the centered 2x2 reduction of the 3x3
//! normal equations; Eberly, "Least Squares Fitting of Data by Linear or Quadratic
//! Structures", Geometric Tools). When the planted feet are collinear the fit takes every
//! foot with ground (feet in the air at their ground line); when those are collinear too it
//! keeps only the slope along their line (the minimum-norm solution); with every foot at
//! one spot the attitude holds. Angles are clamped by [`GaitPosture::max_pitch`] and
//! [`GaitPosture::max_roll`], scaled by [`GaitPosture::attitude_weight`] and eased with
//! [`GaitPosture::attitude_tau`]. Original code; presentation only, never a simulation
//! checksum input.

use super::{FootMode, Frame, GaitMode, GaitParams, GaitState, MAX_LEGS};
use crate::num::{det_atan2, det_exp};
use regia_core::math::detmath::det_sin_cos;
use regia_ecs::prelude::{Reflect, bevy_reflect};

/// How the body follows the ground under the feet (ADR-8706): height smoothing and
/// pitch/roll. Part of [`GaitParams::posture`]; [`GaitPosture::UPRIGHT`] is the stepper of
/// ADR-8060 bit for bit (height is the plain mean of the planted feet plus `clearance`, the
/// body never tilts).
#[derive(Clone, Copy, Debug, PartialEq, Reflect)]
pub struct GaitPosture {
    /// Weight of a foot in the air in the body height target, in `[0, 1]` (a planted foot
    /// weighs 1). The foot counts at the ground line under it: its lift-off height blended
    /// to its landing height by swing progress. 1 keeps the target continuous through
    /// lift-off and touchdown; 0 is the plain mean of the planted feet.
    pub swing_weight: f32,
    /// Time constant in seconds of the exponential ease of body height toward its target,
    /// finite and nonnegative; 0 follows the target at once.
    pub height_tau: f32,
    /// Most the body height may change per second, in meters; positive, infinity for no
    /// limit.
    pub height_slew: f32,
    /// Fraction of the ground's pitch and roll the body takes, in `[0, 1]`; 0 keeps the body
    /// upright ([`GaitState::attitude`] stays the identity).
    pub attitude_weight: f32,
    /// Largest pitch in radians (before `attitude_weight`), in `[0, pi/2)`.
    pub max_pitch: f32,
    /// Largest roll in radians (before `attitude_weight`), in `[0, pi/2)`.
    pub max_roll: f32,
    /// Time constant in seconds of the exponential ease of pitch and roll, finite and
    /// nonnegative; 0 follows the ground at once.
    pub attitude_tau: f32,
}

impl GaitPosture {
    /// The upright stepper of ADR-8060: no swing weight, no smoothing, no tilt.
    pub const UPRIGHT: Self = Self {
        swing_weight: 0.0,
        height_tau: 0.0,
        height_slew: f32::INFINITY,
        attitude_weight: 0.0,
        max_pitch: 0.0,
        max_roll: 0.0,
        attitude_tau: 0.0,
    };

    /// Whether every setting is in its documented range.
    #[must_use]
    pub fn is_valid(&self) -> bool {
        let unit = 0.0..=1.0;
        let angle = |a: f32| (0.0..core::f32::consts::FRAC_PI_2).contains(&a);
        let tau = |t: f32| t.is_finite() && t >= 0.0;
        unit.contains(&self.swing_weight)
            && unit.contains(&self.attitude_weight)
            && tau(self.height_tau)
            && tau(self.attitude_tau)
            && !self.height_slew.is_nan()
            && self.height_slew > 0.0
            && angle(self.max_pitch)
            && angle(self.max_roll)
    }
}

/// The identity quaternion `[x, y, z, w]`.
pub(super) const IDENTITY: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// One step of the exponential ease from `from` toward `to`; `tau == 0` returns `to`.
fn ease(from: f32, to: f32, tau: f32, dt: f32) -> f32 {
    if tau > 0.0 {
        from + (to - from) * (1.0 - det_exp(-dt / tau))
    } else {
        to
    }
}

/// A plane fit's slopes `[a, b]` (rise per meter along the local right and forward axes).
enum Fit {
    /// The points span a plane.
    Plane([f64; 2]),
    /// The points lie on a line: only the slope along it is known.
    Line([f64; 2]),
}

/// Least-squares slopes of `y = a * right + b * forward + c` through `points`
/// (`[right, forward, y]`), or `None` with every point at one spot (or fewer than two).
fn fit(points: &[[f32; 3]]) -> Option<Fit> {
    let n = points.len();
    if n < 2 {
        return None;
    }
    let count = n as f64;
    let (mut mr, mut mf) = (0.0_f64, 0.0_f64);
    for p in points {
        mr += f64::from(p[0]);
        mf += f64::from(p[1]);
    }
    mr /= count;
    mf /= count;
    // Heights relative to the first point: only the regressors need centering, and equal
    // heights give exactly zero sums (level ground stays level bit for bit).
    let y0 = f64::from(points[0][2]);
    let (mut srr, mut sff, mut srf, mut sry, mut sfy) = (0.0_f64, 0.0, 0.0, 0.0, 0.0);
    let mut level = true;
    for p in points {
        let r = f64::from(p[0]) - mr;
        let f = f64::from(p[1]) - mf;
        let dy = f64::from(p[2]) - y0;
        level &= dy == 0.0;
        srr += r * r;
        sff += f * f;
        srf += r * f;
        sry += r * dy;
        sfy += f * dy;
    }
    let spread = srr + sff;
    if spread.is_nan() || spread <= 1e-12 {
        return None;
    }
    if level {
        return Some(Fit::Plane([0.0, 0.0]));
    }
    let det = srr * sff - srf * srf;
    // det / spread^2 is about the ratio of the two spreads' squares: below 1e-4 the points
    // are within about 1 % of a line and the cross slope is noise.
    if det > 1e-4 * spread * spread {
        let a = (sry * sff - sfy * srf) / det;
        let b = (sfy * srr - sry * srf) / det;
        return Some(Fit::Plane([a, b]));
    }
    // Principal direction of the spread (eigenvector of the larger eigenvalue).
    let half = 0.5 * (srr - sff);
    let big = 0.5 * spread + (half * half + srf * srf).sqrt();
    let (u, v) = (srf, big - srr);
    let (p, q) = (big - sff, srf);
    let (dr, df) = if u * u + v * v >= p * p + q * q {
        (u, v)
    } else {
        (p, q)
    };
    let len = (dr * dr + df * df).sqrt();
    if len.is_nan() || len <= 0.0 {
        // Diagonal spread matrix with srf == 0: the line runs along one axis.
        return Some(if srr >= sff {
            Fit::Line([sry / srr, 0.0])
        } else {
            Fit::Line([0.0, sfy / sff])
        });
    }
    let (dr, df) = (dr / len, df / len);
    let along = dr * dr * srr + 2.0 * dr * df * srf + df * df * sff;
    if along.is_nan() || along <= 0.0 {
        return None;
    }
    let slope = (dr * sry + df * sfy) / along;
    Some(Fit::Line([slope * dr, slope * df]))
}

impl GaitState {
    /// Floor height (the weighted mean of the feet's floors, see the module docs), or
    /// `None` with no foot planted and no foot in the air over ground.
    fn floor_target(&self, params: &GaitParams) -> Option<f32> {
        let weight = params.posture.swing_weight;
        let mut sum = 0.0;
        let mut count = 0.0;
        for i in 0..self.leg_count() {
            let foot = &self.feet[i];
            if foot.planted {
                sum += foot.position[1];
                count += 1.0;
            } else if weight > 0.0 && self.over_ground(i) {
                sum += weight * self.swing_floor[i];
                count += weight;
            }
        }
        (count > 0.0).then(|| sum / count)
    }

    /// Whether foot `i` is in the air (pattern swing or forced step) toward ground.
    fn over_ground(&self, i: usize) -> bool {
        !self.feet[i].planted
            && self.swing_grounded[i]
            && (self.swinging[i] || self.foot_modes[i] == FootMode::Forced)
    }

    /// Body Y follows the floor target plus clearance (see the module docs): at once when
    /// `ease_from` is `None` (construction, replant), else eased and slew-limited from its
    /// `(start height, dt)`. With no foot planted and none in the air over ground it holds.
    /// Calling it again in the same step with the same start height is idempotent.
    pub(super) fn follow_stance(&mut self, params: &GaitParams, ease_from: Option<(f32, f32)>) {
        let Some(floor) = self.floor_target(params) else {
            return;
        };
        let target = floor + params.clearance;
        self.body_height_target = target;
        self.position[1] = match ease_from {
            None => target,
            Some((start, dt)) => {
                let posture = &params.posture;
                let mut y = ease(start, target, posture.height_tau, dt);
                let limit = posture.height_slew * dt;
                let change = y - start;
                if change.abs() > limit {
                    y = start + limit.copysign(change);
                }
                y
            }
        };
    }

    /// Pitch, roll and [`Self::attitude`] from the plane of the feet (see the module docs):
    /// at once when `dt` is `None`, else eased. The stored angles are clamped again after the
    /// ease to `max_pitch * attitude_weight` and `max_roll * attitude_weight` (ADR-10053), so a
    /// lowered limit or a stored angle past it holds on the next step, and weight 0 writes the
    /// identity at once.
    pub(super) fn follow_attitude(&mut self, params: &GaitParams, frame: Frame, dt: Option<f32>) {
        let posture = &params.posture;
        if posture.attitude_weight <= 0.0 {
            self.pitch = 0.0;
            self.roll = 0.0;
            self.attitude = IDENTITY;
            return;
        }
        let (pitch, roll) = match self.ground_angles(frame) {
            Some((pitch, roll)) => (
                pitch.clamp(-posture.max_pitch, posture.max_pitch) * posture.attitude_weight,
                roll.clamp(-posture.max_roll, posture.max_roll) * posture.attitude_weight,
            ),
            None => (self.pitch, self.roll),
        };
        let (pitch, roll) = match dt {
            None => (pitch, roll),
            Some(dt) => (
                ease(self.pitch, pitch, posture.attitude_tau, dt),
                ease(self.roll, roll, posture.attitude_tau, dt),
            ),
        };
        let pitch_limit = posture.max_pitch * posture.attitude_weight;
        let roll_limit = posture.max_roll * posture.attitude_weight;
        self.pitch = pitch.clamp(-pitch_limit, pitch_limit);
        self.roll = roll.clamp(-roll_limit, roll_limit);
        self.attitude = attitude(self.mode, self.pitch, self.roll);
    }

    /// Ground pitch and roll in radians under the feet, or `None` when they do not tell.
    fn ground_angles(&self, frame: Frame) -> Option<(f32, f32)> {
        let mut points = [[0.0_f32; 3]; MAX_LEGS];
        let mut n = 0;
        for foot in self.feet() {
            if foot.planted {
                points[n] = self.local(frame, foot.position, foot.position[1]);
                n += 1;
            }
        }
        let slopes = match fit(&points[..n]) {
            Some(Fit::Plane(slopes)) => slopes,
            _ => {
                // Collinear planted feet: add the feet in the air at their ground line.
                for i in 0..self.leg_count() {
                    if self.over_ground(i) {
                        points[n] = self.local(frame, self.feet[i].position, self.swing_floor[i]);
                        n += 1;
                    }
                }
                match fit(&points[..n])? {
                    Fit::Plane(slopes) | Fit::Line(slopes) => slopes,
                }
            }
        };
        let [a, b] = slopes.map(|s| s as f32);
        if !a.is_finite() || !b.is_finite() {
            return None;
        }
        // Pitch tilts the forward axis into the plane; roll then turns about that axis
        // until the right axis lies in the plane too: tan(roll) = a * cos(pitch).
        let pitch = det_atan2(b, 1.0);
        let (_, cos) = det_sin_cos(pitch);
        let roll = det_atan2(a * cos, 1.0);
        Some((pitch, roll))
    }

    /// `[right, forward, y]` of a world point relative to the body center.
    fn local(&self, frame: Frame, point: [f32; 3], y: f32) -> [f32; 3] {
        let dx = point[0] - self.position[0];
        let dz = point[2] - self.position[2];
        [
            dx * frame.right[0] + dz * frame.right[1],
            dx * frame.forward[0] + dz * frame.forward[1],
            y,
        ]
    }
}

/// The body tilt quaternion `[x, y, z, w]` in the stepper's local frame (+X the frame's
/// right, +Y up; +Z forward in integrating mode, back in driven mode): pitch about +X
/// (positive raises the nose), then roll about the pitched forward axis (positive raises
/// the +X side). Exactly the identity when both angles are zero.
fn attitude(mode: GaitMode, pitch: f32, roll: f32) -> [f32; 4] {
    if pitch == 0.0 && roll == 0.0 {
        return IDENTITY;
    }
    // Nose up: -Z forward (driven) turns positively about +X, +Z forward negatively.
    let nose = match mode {
        GaitMode::Driven => pitch,
        GaitMode::Integrating => -pitch,
    };
    let (sx, cx) = det_sin_cos(nose * 0.5);
    let (sz, cz) = det_sin_cos(roll * 0.5);
    // q = qx(nose) * qz(roll).
    [cz * sx, 0.0 - sx * sz, cx * sz, cx * cz]
}

#[cfg(test)]
mod tests {
    use super::{Fit, fit};

    #[test]
    fn plane_line_and_point_fits() {
        // y = 0.5 r - 0.25 f + 2 on four points.
        let plane = [
            [0.0, 0.0, 2.0],
            [1.0, 0.0, 2.5],
            [0.0, 1.0, 1.75],
            [1.0, 1.0, 2.25],
        ];
        match fit(&plane) {
            Some(Fit::Plane([a, b])) => {
                assert!((a - 0.5).abs() < 1e-12 && (b + 0.25).abs() < 1e-12);
            }
            _ => panic!("four corners span a plane"),
        }
        // Two points on a diagonal: only the slope along it.
        match fit(&[[-1.0, -1.0, 0.0], [1.0, 1.0, 2.0]]) {
            Some(Fit::Line([a, b])) => {
                assert!((a - 0.5).abs() < 1e-12 && (b - 0.5).abs() < 1e-12);
            }
            _ => panic!("two points are a line"),
        }
        // Along one axis.
        match fit(&[[0.0, -1.0, 0.0], [0.0, 1.0, 1.0], [0.0, 0.0, 0.5]]) {
            Some(Fit::Line([a, b])) => {
                assert_eq!(a, 0.0);
                assert!((b - 0.5).abs() < 1e-12);
            }
            _ => panic!("points on the forward axis are a line"),
        }
        assert!(fit(&[[1.0, 1.0, 3.0], [1.0, 1.0, 4.0]]).is_none());
        assert!(fit(&[[1.0, 1.0, 3.0]]).is_none());
        match fit(&[[0.0, 0.0, 0.1], [1.0, 0.0, 0.1], [0.0, 1.0, 0.1]]) {
            Some(Fit::Plane([a, b])) => assert_eq!((a.to_bits(), b.to_bits()), (0, 0)),
            _ => panic!("level points are a level plane"),
        }
    }
}
