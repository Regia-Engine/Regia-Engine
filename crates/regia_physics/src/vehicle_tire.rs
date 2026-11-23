// SPDX-License-Identifier: MPL-2.0
// Source: Pacejka, "Tire and Vehicle Dynamics" (3rd ed.): the slip angle tan α = v_side / |v_along|,
// the brush model with a parabolic pressure distribution (ch. 3: F = µFz (1 − (1 − z)³) with
// z = Cα |tan α| / (3 µ Fz), saturated at z = 1) and the Magic Formula (ch. 4:
// D sin(C atan(Bx − E(Bx − atan Bx))), BCD the cornering stiffness). Milliken and Milliken, "Race Car
// Vehicle Dynamics" (ch. 2: tire load sensitivity, the friction ellipse for combined slip). The stick
// state anchors the contact where it stuck (the bristle idea of elasto-plastic friction) and solves it
// as a velocity constraint with a Baumgarte position term (textbook constraint stabilization). The implementation and the stability
// limit on the side impulse are Regia Engine's own (ADR-2719).
// Transients (ADR-3276): the relaxation length (Pacejka ch. 7, the single contact point model: the
// lateral deflection lags the slip over a rolling distance σ, dα'/dt = (|Vx|/σ)(α − α')), the
// brush model's aligning moment (ch. 3: pneumatic trail (a/3)(1 − z)³ / (1 − z + z²/3), zero once the
// patch slides), a cosine trail after the Magic Formula's (ch. 4; here in the slip angle over the
// peak's, original design) and the Magic Formula's combined-slip weighting
// cos(C atan(Bκ)) (ch. 4), and the linear camber thrust C_γ γ (ch. 1 and 4). The implicit coupling
// of the lag to the contact's mass, the deflection damping sized to critical damping, the clamp of
// the deflection at the slide and the friction-circle bound are Regia Engine's own (original design).

//! The slip-angle tire model of a ray-cast vehicle wheel (ADR-2719).
//!
//! [`TireModel::Damper`] (the default) keeps rapier's sideways velocity damper. [`TireModel::Slip`]
//! switches rapier's side force off for that wheel and has Regia apply a lateral force computed from
//! the contact's slip angle on a [`LateralCurve`], scaled by load sensitivity and combined with the
//! drivetrain's lengthwise force on a friction ellipse. Below [`SlipTire::stick_speed`] the tire
//! sticks, so a held brake on a slope inside the tire's grip holds the car still.
//!
//! The curve functions on [`SlipTire`] are pure, so a tire is checked without a world
//! (`physics.vehicle_tire_curve` reads them over the agent protocol).

use regia_core::math::{
    det_asin, det_atan, det_atan2, det_cos, det_exp, det_exp_f64, det_powf, det_sin, det_sin_cos, det_tan,
};
use serde::{Deserialize, Serialize};

/// The share of the stick displacement corrected per step (Baumgarte β). Lower is softer; the
/// held car's resting offset is about `g sin θ dt² / β`.
pub const STICK_BIAS: f32 = 0.3;

/// The largest friction coefficient a tire may have.
const MAX_FRICTION: f32 = 10.0;

/// The largest size of a tire setting (newtons, newtons per radian, m/s).
const MAX_SETTING: f32 = 1.0e9;

/// The load factor is kept inside `[1 / LOAD_FACTOR_LIMIT, LOAD_FACTOR_LIMIT]`, so a near-zero or
/// enormous load cannot blow the curve up.
const LOAD_FACTOR_LIMIT: f32 = 10.0;

/// Below this load (newtons) a tire makes no force.
const MIN_LOAD: f32 = 1.0e-3;

/// Below this lengthwise speed (m/s) the slip angle reads as fully sideways.
const MIN_ALONG_SPEED: f32 = 1.0e-3;

/// The longest relaxation length a tire may have, meters.
const MAX_RELAXATION_LENGTH: f32 = 10.0;

/// The longest pneumatic trail a tire may have, meters.
const MAX_TRAIL: f32 = 1.0;

/// The largest static camber a wheel may have, radians (about 29 degrees).
const MAX_STATIC_CAMBER: f32 = 0.5;

/// The lagged slip's tangent never goes past this (89 degrees), so a contact sliding sideways at
/// a standstill keeps a finite state.
const MAX_LAG_TAN: f32 = 57.29;

/// The Magic Formula trail is `cos(π/2 · α / α_peak)` up to this share of the peak slip angle and
/// stays there beyond it: 0 at the peak, `cos(1.05 · π/2)` ≈ −0.08 of its zero-slip value past it.
const TRAIL_TAIL: f32 = 1.05;

/// How a wheel's tire makes its sideways (cornering) force.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum TireModel {
    /// rapier's sideways velocity damper, limited by `friction_slip` and scaled by
    /// `side_friction_stiffness`: the model of every `.vehicle.ron` v1 and v2 file, bit for bit.
    #[default]
    Damper,
    /// A slip-angle curve computed by Regia and applied at the contact; rapier's side force is off
    /// for this wheel.
    Slip(SlipTire),
}

impl TireModel {
    /// The slip tire, if this is one.
    #[must_use]
    pub fn slip(&self) -> Option<&SlipTire> {
        match self {
            Self::Damper => None,
            Self::Slip(tire) => Some(tire),
        }
    }

    /// `"damper"` or `"slip"` (agent protocol, logs).
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self {
            Self::Damper => "damper",
            Self::Slip(_) => "slip",
        }
    }
}

/// The shape of a slip tire's force against slip angle. Both shapes rise with the authored
/// cornering stiffness at a slip angle of 0 and peak at `peak_friction · load`.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum LateralCurve {
    /// The brush model (parabolic contact pressure): rises, rounds over and stays flat at the peak
    /// once the whole contact patch slides.
    #[default]
    Brush,
    /// Pacejka's Magic Formula: peaks, then falls toward `sin(shape · π/2)` of the peak.
    MagicFormula {
        /// The shape factor C, in (0, 2): about 1.3 for a car's side force. Above 1 the curve has a
        /// peak and falls past it.
        shape: f32,
        /// The curvature factor E, at most 1: below 0 the peak is sharper, toward 1 flatter.
        curvature: f32,
    },
}

/// How a slip tire shares its grip between the lengthwise and the side force (ADR-3276).
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum CombinedSlip {
    /// The friction ellipse (ADR-2719, the default): the side grip left is
    /// `sqrt(1 − (Fx / Fx_max)²)` of the pure one.
    #[default]
    Ellipse,
    /// The Magic Formula's combined-slip weighting: the side force is the pure one times
    /// `cos(shape · atan(stiffness · κ))`, κ the wheel's slip ratio (0 with the `Simple`
    /// drivetrain, whose wheels roll with the chassis), and the resultant with the lengthwise force
    /// never exceeds `µ · Fz` (the friction circle).
    SlipWeighted {
        /// The weighting's shape factor, in (0, 1]: 1 lets a fully spinning or locked wheel lose all
        /// its side force, lower keeps `cos(shape · π/2)` of it.
        shape: f32,
        /// How fast the side force falls with slip ratio, in (0, 1000]: about 10 for a road tire.
        stiffness: f32,
    },
}

/// A slip-angle tire. Every field is optional in `.vehicle.ron`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SlipTire {
    /// The curve's shape.
    pub curve: LateralCurve,
    /// The sideways friction coefficient at `nominal_load`: the peak force is
    /// `peak_friction · load` there. In (0, 10].
    pub peak_friction: f32,
    /// Cornering stiffness at `nominal_load`, newtons per radian of slip angle (> 0): the slope of
    /// the curve at a slip angle of 0. It grows in proportion to the load (times the load factor).
    pub cornering_stiffness: f32,
    /// The load the friction and stiffness are quoted at, newtons (> 0).
    pub nominal_load: f32,
    /// Load sensitivity: the grip and the stiffness per newton of load scale by
    /// `(load / nominal_load)^(−load_exponent)`, in [0, 1). 0 is no sensitivity; road tires are
    /// about 0.1 to 0.3.
    pub load_exponent: f32,
    /// Where the side force acts, in [0, 1]: 1 (the default) at the contact, so load moves to
    /// the outer wheels as the geometry says; 0 at the center of mass's height (no roll moment).
    pub roll_influence: f32,
    /// Below this contact speed (m/s) the tire sticks: sideways always, lengthwise while the
    /// wheel is held by its brake. In [0, 10]; 0 turns the hold off.
    pub stick_speed: f32,
    /// Relaxation length, meters, in [0, 10] (`.vehicle.ron` v4, ADR-3276): the side force builds
    /// over this much rolling, with the time constant `relaxation_length / speed`. 0 (the default)
    /// is no lag: the force follows the slip angle at once, as in v3. Road tires are about 0.3 to
    /// 1 m.
    pub relaxation_length: f32,
    /// Pneumatic trail at a slip angle of 0, meters, in [0, 1] (v4): how far behind the contact the
    /// side force acts, which gives the self-aligning torque. It falls to 0 at the curve's peak
    /// (slightly below 0 past it for the Magic Formula). 0 (the default) gives no aligning torque.
    /// About a sixth of the contact patch's length: 0.02 to 0.05 m for a car.
    pub pneumatic_trail: f32,
    /// Camber stiffness at `nominal_load`, newtons per radian, in [0, 1e9] (v4): the side force a
    /// cambered wheel makes with no slip. It grows with the load like the cornering stiffness.
    /// 0 (the default) gives no camber thrust. About a tenth of the cornering stiffness for a car.
    pub camber_stiffness: f32,
    /// Static camber, radians, in [−0.5, 0.5] (v4): negative leans the wheel's top toward the
    /// chassis' center line, as on a setup sheet (a wheel at x = 0 counts as a right wheel). Body
    /// roll and the ground's slope add to it. Only `camber_stiffness` reads it.
    pub static_camber: f32,
    /// How the side and the lengthwise force share the grip (v4): the friction ellipse (the
    /// default) or the slip-ratio weighting.
    pub combined_slip: CombinedSlip,
}

impl Default for SlipTire {
    /// A road tire on a 1,200 kg car: µ 1.0 and 45 kN/rad at 3 kN, brush curve, no load
    /// sensitivity, side force at the contact, static hold below 0.3 m/s.
    fn default() -> Self {
        Self {
            curve: LateralCurve::Brush,
            peak_friction: 1.0,
            cornering_stiffness: 45_000.0,
            nominal_load: 3_000.0,
            load_exponent: 0.0,
            roll_influence: 1.0,
            stick_speed: 0.3,
            relaxation_length: 0.0,
            pneumatic_trail: 0.0,
            camber_stiffness: 0.0,
            static_camber: 0.0,
            combined_slip: CombinedSlip::Ellipse,
        }
    }
}

/// `value` is finite and in `[lo, hi]`.
fn within(value: f32, lo: f32, hi: f32) -> bool {
    value.is_finite() && (lo..=hi).contains(&value)
}

impl SlipTire {
    /// The first field out of range, named as in `.vehicle.ron` (`tire_model.peak_friction`).
    #[must_use]
    pub fn invalid_field(&self) -> Option<&'static str> {
        let curve_ok = match self.curve {
            LateralCurve::Brush => true,
            LateralCurve::MagicFormula { shape, curvature } => {
                shape.is_finite() && shape > 0.0 && shape < 2.0 && within(curvature, -MAX_SETTING, 1.0)
            }
        };
        let checks = [
            (curve_ok, "tire_model.curve"),
            (
                self.peak_friction.is_finite()
                    && self.peak_friction > 0.0
                    && self.peak_friction <= MAX_FRICTION,
                "tire_model.peak_friction",
            ),
            (
                self.cornering_stiffness.is_finite()
                    && self.cornering_stiffness > 0.0
                    && self.cornering_stiffness <= MAX_SETTING,
                "tire_model.cornering_stiffness",
            ),
            (
                self.nominal_load.is_finite()
                    && self.nominal_load > MIN_LOAD
                    && self.nominal_load <= MAX_SETTING,
                "tire_model.nominal_load",
            ),
            (
                self.load_exponent.is_finite() && (0.0..1.0).contains(&self.load_exponent),
                "tire_model.load_exponent",
            ),
            (within(self.roll_influence, 0.0, 1.0), "tire_model.roll_influence"),
            (within(self.stick_speed, 0.0, 10.0), "tire_model.stick_speed"),
            (
                within(self.relaxation_length, 0.0, MAX_RELAXATION_LENGTH),
                "tire_model.relaxation_length",
            ),
            (
                within(self.pneumatic_trail, 0.0, MAX_TRAIL),
                "tire_model.pneumatic_trail",
            ),
            (
                within(self.camber_stiffness, 0.0, MAX_SETTING),
                "tire_model.camber_stiffness",
            ),
            (
                within(self.static_camber, -MAX_STATIC_CAMBER, MAX_STATIC_CAMBER),
                "tire_model.static_camber",
            ),
            (
                match self.combined_slip {
                    CombinedSlip::Ellipse => true,
                    CombinedSlip::SlipWeighted { shape, stiffness } => {
                        shape.is_finite()
                            && shape > 0.0
                            && shape <= 1.0
                            && stiffness.is_finite()
                            && stiffness > 0.0
                            && stiffness <= 1000.0
                    }
                },
                "tire_model.combined_slip",
            ),
        ];
        checks.into_iter().find(|(ok, _)| !ok).map(|(_, field)| field)
    }

    /// The load factor `(load / nominal_load)^(−load_exponent)`, kept within 0.1 to 10; 1 for a
    /// load that is not finite and positive.
    #[must_use]
    pub fn load_factor(&self, load: f32) -> f32 {
        if self.load_exponent == 0.0 || !(load.is_finite() && load > MIN_LOAD) {
            return 1.0;
        }
        let factor = det_powf(load / self.nominal_load, -self.load_exponent);
        if factor.is_finite() {
            factor.clamp(1.0 / LOAD_FACTOR_LIMIT, LOAD_FACTOR_LIMIT)
        } else {
            1.0
        }
    }

    /// The effective friction coefficient at `load`: `peak_friction · load_factor`.
    #[must_use]
    pub fn friction_at_load(&self, load: f32) -> f32 {
        self.peak_friction * self.load_factor(load)
    }

    /// The peak side force at `load`, newtons; 0 for a load that is not finite and positive.
    #[must_use]
    pub fn peak_force(&self, load: f32) -> f32 {
        if !(load.is_finite() && load > MIN_LOAD) {
            return 0.0;
        }
        self.friction_at_load(load) * load
    }

    /// The cornering stiffness at `load`, newtons per radian; 0 for a load that is not finite and
    /// positive.
    #[must_use]
    pub fn cornering_stiffness_at(&self, load: f32) -> f32 {
        if !(load.is_finite() && load > MIN_LOAD) {
            return 0.0;
        }
        self.cornering_stiffness * (load / self.nominal_load) * self.load_factor(load)
    }

    /// The pure-slip side force's size for a slip whose tangent is `slip_tan` (≥ 0, may be
    /// infinite), at `load`. 0 for anything not finite or a load at or below zero.
    fn force_size(&self, slip_tan: f32, load: f32) -> f32 {
        let peak = self.peak_force(load);
        let stiffness = self.cornering_stiffness_at(load);
        if peak <= 0.0 || stiffness <= 0.0 || slip_tan.is_nan() {
            return 0.0;
        }
        let slip_tan = slip_tan.abs();
        let force = match self.curve {
            LateralCurve::Brush => {
                let z = (stiffness * slip_tan / (3.0 * peak)).min(1.0);
                let rest = 1.0 - z;
                peak * (1.0 - rest * rest * rest)
            }
            LateralCurve::MagicFormula { shape, curvature } => {
                // atan of an infinite tangent is π/2: a contact sliding straight sideways.
                // The deterministic kernels keep the force bit-stable across platform libms
                // (ADR-4773).
                let alpha = det_atan(slip_tan);
                let b = stiffness / (shape * peak);
                let x = b * alpha;
                let inner = x - curvature * (x - det_atan(x));
                det_sin(shape * det_atan(inner)) * peak
            }
        };
        if force.is_finite() { force.max(0.0) } else { 0.0 }
    }

    /// The pure-slip side force at slip angle `slip_angle` (radians, positive when the contact
    /// moves toward the wheel's right) and `load` (newtons), along the wheel's right: it opposes
    /// the slip, so a positive slip angle gives a negative force. 0 for a load at or below zero
    /// or any input that is not finite.
    #[must_use]
    pub fn lateral_force(&self, slip_angle: f32, load: f32) -> f32 {
        if !slip_angle.is_finite() {
            return 0.0;
        }
        let angle = slip_angle.clamp(-std::f32::consts::FRAC_PI_2, std::f32::consts::FRAC_PI_2);
        // `tan = sin/cos` on the deterministic kernels: same bits on every target (ADR-4773).
        let tan = if angle.abs() >= std::f32::consts::FRAC_PI_2 {
            f32::INFINITY
        } else {
            let (sin, cos) = det_sin_cos(angle);
            sin / cos
        };
        -self.force_size(tan, load).copysign(slip_angle)
    }

    /// The slip angle of the curve's peak at `load`, radians: where the brush model starts to
    /// slide fully, or the Magic Formula's maximum (`π/2` when it has none).
    #[must_use]
    pub fn peak_slip_angle(&self, load: f32) -> f32 {
        #[cfg(test)]
        PEAK_SLIP_EVALS.with(|c| c.set(c.get() + 1));
        let peak = self.peak_force(load);
        let stiffness = self.cornering_stiffness_at(load);
        if peak <= 0.0 || stiffness <= 0.0 {
            return 0.0;
        }
        match self.curve {
            LateralCurve::Brush => det_atan(3.0 * peak / stiffness),
            LateralCurve::MagicFormula { shape, curvature } => {
                if shape <= 1.0 {
                    return std::f32::consts::FRAC_PI_2;
                }
                // C·atan(u) = π/2 at u = tan(π/(2C)); solve x − E(x − atan x) = u for x by bisection
                // (the left side rises for E ≤ 1). Deterministic tan/atan (ADR-4773).
                let target = {
                    let (sin, cos) = det_sin_cos(std::f32::consts::FRAC_PI_2 / shape);
                    sin / cos
                };
                let b = stiffness / (shape * peak);
                let (mut lo, mut hi) = (0.0_f32, std::f32::consts::FRAC_PI_2 * b);
                let inner = |x: f32| x - curvature * (x - det_atan(x));
                if inner(hi) < target {
                    return std::f32::consts::FRAC_PI_2;
                }
                for _ in 0..48 {
                    let mid = 0.5 * (lo + hi);
                    if inner(mid) < target {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                0.5 * (lo + hi) / b
            }
        }
    }
}

impl SlipTire {
    /// Some v4 transient field is not at its default: the tire needs `.vehicle.ron` v4.
    #[must_use]
    pub fn has_transients(&self) -> bool {
        self.relaxation_length != 0.0
            || self.pneumatic_trail != 0.0
            || self.camber_stiffness != 0.0
            || self.static_camber != 0.0
            || self.combined_slip != CombinedSlip::Ellipse
    }

    /// The pneumatic trail at `slip_angle` (radians) and `load` (newtons), meters. The brush curve
    /// gives the brush model's trail, `pneumatic_trail · (1 − z)³ / (1 − z + z²/3)` with z the
    /// share of the patch that slides, so it is 0 from the peak slip angle on; the Magic Formula
    /// gives `pneumatic_trail · cos(π/2 · min(α / α_peak, 1.05))`, so it is 0 at the peak and
    /// slightly negative (−0.08 of the zero-slip trail) past it. 0 with no trail or a load or angle that is not finite.
    #[must_use]
    pub fn pneumatic_trail_at(&self, slip_angle: f32, load: f32) -> f32 {
        self.trail_at(slip_angle, load, None)
    }

    /// `cached_peak` is `peak_slip_angle(load)` when the caller already computed it for this load.
    fn trail_at(&self, slip_angle: f32, load: f32, cached_peak: Option<f32>) -> f32 {
        if self.pneumatic_trail <= 0.0 || !slip_angle.is_finite() {
            return 0.0;
        }
        let peak = self.peak_force(load);
        let stiffness = self.cornering_stiffness_at(load);
        if peak <= 0.0 || stiffness <= 0.0 {
            return 0.0;
        }
        let angle = slip_angle.abs().min(std::f32::consts::FRAC_PI_2);
        let trail = match self.curve {
            LateralCurve::Brush => {
                let tan = if angle >= std::f32::consts::FRAC_PI_2 {
                    f32::INFINITY
                } else {
                    det_tan(angle)
                };
                let z = (stiffness * tan / (3.0 * peak)).min(1.0);
                let rest = 1.0 - z;
                rest * rest * rest / (1.0 - z + z * z / 3.0)
            }
            LateralCurve::MagicFormula { .. } => {
                let at_peak = cached_peak
                    .unwrap_or_else(|| self.peak_slip_angle(load))
                    .max(1.0e-6);
                det_cos(std::f32::consts::FRAC_PI_2 * (angle / at_peak).min(TRAIL_TAIL))
            }
        };
        let trail = self.pneumatic_trail * trail;
        if trail.is_finite() { trail } else { 0.0 }
    }

    /// The pure-slip self-aligning torque at `slip_angle` and `load`, newton-meters about the
    /// contact normal, positive turning the wheel left (the sign of `steer_angle`): the trail times
    /// the side force. A positive slip angle (the contact moving toward the wheel's right) gives a
    /// negative torque, which turns the wheel toward where it is going.
    #[must_use]
    pub fn aligning_torque(&self, slip_angle: f32, load: f32) -> f32 {
        let torque = self.pneumatic_trail_at(slip_angle, load) * self.lateral_force(slip_angle, load);
        if torque.is_finite() { torque } else { 0.0 }
    }

    /// The camber stiffness at `load`, newtons per radian: `camber_stiffness` scaled like the
    /// cornering stiffness (in proportion to the load, times the load factor); 0 for a load that is
    /// not finite and positive.
    #[must_use]
    pub fn camber_stiffness_at(&self, load: f32) -> f32 {
        if !(load.is_finite() && load > MIN_LOAD) {
            return 0.0;
        }
        self.camber_stiffness * (load / self.nominal_load) * self.load_factor(load)
    }

    /// The camber thrust at camber `camber` (radians, positive leaning the wheel's top toward its
    /// right) and `load`, newtons along the wheel's right: `camber_stiffness_at(load) · camber`,
    /// toward the side the wheel leans. 0 for anything not finite.
    #[must_use]
    pub fn camber_thrust(&self, camber: f32, load: f32) -> f32 {
        if !camber.is_finite() {
            return 0.0;
        }
        let thrust = self.camber_stiffness_at(load) * camber;
        if thrust.is_finite() { thrust } else { 0.0 }
    }

    /// The share of the pure side force left at slip ratio `slip_ratio` by the slip-weighted
    /// combined model, in [0, 1]; 1 for the ellipse (which limits by force instead) or a slip ratio
    /// that is not finite.
    #[must_use]
    pub fn combined_weight(&self, slip_ratio: f32) -> f32 {
        match self.combined_slip {
            CombinedSlip::Ellipse => 1.0,
            CombinedSlip::SlipWeighted { shape, stiffness } => {
                if !slip_ratio.is_finite() {
                    return 1.0;
                }
                det_cos(shape * det_atan(stiffness * slip_ratio.abs())).clamp(0.0, 1.0)
            }
        }
    }

    /// The tangent of a peak slip angle, kept at most `MAX_LAG_TAN`.
    fn peak_slip_tan_from(angle: f32) -> f32 {
        if angle >= std::f32::consts::FRAC_PI_2 - 1.0e-3 {
            MAX_LAG_TAN
        } else {
            det_tan(angle).clamp(0.0, MAX_LAG_TAN)
        }
    }

    /// Parameters of [`Self::peak_slip_angle`]. The load is mixed in by the caller. Load and
    /// `load_exponent` cancel in the peak-to-stiffness ratio on paper, but not in the rounded
    /// bits, so `load_exponent` is mixed in too. The low bit is set so a key is never 0.
    fn peak_cache_key(&self) -> u64 {
        let mut mixed = 0xcbf2_9ce4_8422_2325u64;
        let mut mix = |bits: u32| {
            mixed = mixed
                .wrapping_mul(0x9e37_79b9_7f4a_7c15)
                .wrapping_add(u64::from(bits));
        };
        match self.curve {
            LateralCurve::Brush => mix(1),
            LateralCurve::MagicFormula { shape, curvature } => {
                mix(2);
                mix(shape.to_bits());
                mix(curvature.to_bits());
            }
        }
        mix(self.peak_friction.to_bits());
        mix(self.cornering_stiffness.to_bits());
        mix(self.nominal_load.to_bits());
        mix(self.load_exponent.to_bits());
        mixed | 1
    }
}

/// The camber a contact sees, radians, positive leaning the wheel's top toward its right: the
/// static camber (negative toward the chassis' center line, so it flips sign on the left side) plus
/// the lean of the wheel against the contact normal, `asin(−axle · normal)` (body roll, slope).
/// `wheel_x` is the wheel's mount `x` in chassis space; `axle_dot_normal` the world axle (the
/// wheel's right) dotted with the contact normal.
#[must_use]
pub fn contact_camber(static_camber: f32, wheel_x: f32, axle_dot_normal: f32) -> f32 {
    let side = if wheel_x < 0.0 { -1.0 } else { 1.0 };
    let lean = det_asin((-axle_dot_normal).clamp(-1.0, 1.0));
    let camber = static_camber * side + lean;
    if camber.is_finite() { camber } else { 0.0 }
}

/// What a wheel's tire did on the last update.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct TireForces {
    /// The tire is a [`TireModel::Slip`] tire (otherwise rapier's damper).
    pub slip: bool,
    /// Mean side force over the step along the wheel's right (axle), newtons; rapier's side
    /// impulse over the step for a damper tire.
    pub lateral_force: f32,
    /// Mean lengthwise force over the step along the rolling direction, newtons: the drivetrain's
    /// or rapier's, plus the static hold.
    pub longitudinal_force: f32,
    /// The tire's load (suspension and anti-roll force), newtons; 0 in the air.
    pub load: f32,
    /// The slip angle the tire saw this step, radians: `atan(v_side / |v_along|)` at the contact,
    /// positive toward the wheel's right; 0 in the air or for a damper tire.
    pub slip_angle: f32,
    /// The tire stuck this step (static hold, sideways or lengthwise).
    pub stuck: bool,
    /// The slip angle the side force used, radians (ADR-3276): the lagged one with a relaxation
    /// length, else `slip_angle`.
    pub lagged_slip_angle: f32,
    /// The self-aligning torque, newton-meters about the contact normal, positive turning the wheel
    /// left (the sign of `steer_angle`): the pneumatic trail at `lagged_slip_angle` times the
    /// slip part of `lateral_force`. 0 without a trail, while stuck and for a damper tire.
    pub aligning_torque: f32,
    /// The camber the tire saw, radians, positive leaning its top toward its right; 0 without a
    /// camber stiffness.
    pub camber: f32,
    /// The camber thrust inside `lateral_force`, newtons along the wheel's right; 0 without a
    /// camber stiffness and while stuck.
    pub camber_thrust: f32,
}

/// A steered axle's aligning torque (ADR-3276): what the tires push back into the steering.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AxleSteeringTorque {
    /// The axle's mount `z` in chassis space, meters (wheels within 1 cm share an axle).
    pub position_z: f32,
    /// How many steered wheels the axle has.
    pub wheels: usize,
    /// The sum of their aligning torques, newton-meters, positive turning the wheels left.
    pub torque: f32,
}

/// The aligning torques of the steered wheels, summed per axle, in the order the axles first
/// appear in `config.wheels`, into `out` (cleared first; reuse it every frame to keep it
/// allocation free). `forces` is `Vehicle::tire_forces`.
pub fn steering_torques_into(
    config: &crate::vehicle::VehicleConfig,
    forces: &[TireForces],
    out: &mut Vec<AxleSteeringTorque>,
) {
    out.clear();
    for (wheel, force) in config.wheels.iter().zip(forces).filter(|(wheel, _)| wheel.steer) {
        let torque = finite_or_zero(force.aligning_torque);
        match out
            .iter_mut()
            .find(|axle| (axle.position_z - wheel.position.z).abs() < 0.01)
        {
            Some(axle) => {
                axle.wheels += 1;
                axle.torque += torque;
            }
            None => out.push(AxleSteeringTorque {
                position_z: wheel.position.z,
                wheels: 1,
                torque,
            }),
        }
    }
}

/// The sum of every steered wheel's aligning torque, newton-meters, positive turning the wheels
/// left: what a force-feedback wheel would feel before the steering ratio.
#[must_use]
pub fn total_steering_torque(config: &crate::vehicle::VehicleConfig, forces: &[TireForces]) -> f32 {
    config
        .wheels
        .iter()
        .zip(forces)
        .filter(|(wheel, _)| wheel.steer)
        .map(|(_, force)| finite_or_zero(force.aligning_torque))
        .sum()
}

/// One slip tire's live state.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct TireState {
    /// Where the contact stuck: in the ground body's frame when the ground has a body, in world
    /// space otherwise; `None` while the tire is not stuck. A measured position, not integrated
    /// velocity: rapier's end-of-step velocity carries a substep artifact that would drift.
    pub anchor: Option<rapier3d::math::Vector>,
    /// The body whose frame `anchor` is in.
    pub anchor_body: Option<rapier3d::dynamics::RigidBodyHandle>,
    /// The lagged slip's tangent (the contact's lateral deflection over the relaxation length);
    /// 0 without a relaxation length (ADR-3276).
    pub lag: f32,
    /// The readout of the last step.
    pub forces: TireForces,
    /// [`SlipTire::peak_slip_angle`] for the load in `peak_key`. Not part of the digest:
    /// it is a cache of a pure function of the tire and that load.
    pub(crate) peak_angle: f32,
    /// Key of the tire and load `peak_angle` was computed for. 0 means the cache is empty.
    pub(crate) peak_key: u64,
}

/// One slip tire's contact for a step, measured after rapier's pass and the drivetrain's impulse.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SlipContact {
    /// Contact speed against the ground along the rolling direction (m/s).
    pub along_speed: f32,
    /// Contact speed against the ground along the axle (m/s).
    pub side_speed: f32,
    /// The tire's load (newtons).
    pub load: f32,
    /// The lengthwise impulse already applied this step (newton-seconds).
    pub longitudinal_impulse: f32,
    /// The lengthwise friction coefficient at nominal load (the ellipse's other axis).
    pub longitudinal_friction: f32,
    /// The wheel is held by its brake: the most lengthwise hold force (newtons), 0 when not held.
    pub hold_force: f32,
    /// The mass share the contact moves along the axle (kg).
    pub side_mass: f32,
    /// The mass share the contact moves along the rolling direction (kg).
    pub along_mass: f32,
    /// The contact's distance from its stick anchor along the rolling direction (m; 0 without one).
    pub along_offset: f32,
    /// The contact's distance from its stick anchor along the axle (m; 0 without one).
    pub side_offset: f32,
    /// The camber the contact sees (radians, positive leaning the top toward the wheel's right;
    /// [`contact_camber`]).
    pub camber: f32,
    /// The wheel's slip ratio (0 with the `Simple` drivetrain).
    pub slip_ratio: f32,
}

/// What a slip tire applies for a step.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub(crate) struct SlipImpulse {
    /// Along the axle (newton-seconds).
    pub side: f32,
    /// Along the rolling direction, on top of the impulse already applied (newton-seconds).
    pub along_extra: f32,
    /// The tire held lengthwise this step (keep the anchor's lengthwise place).
    pub along_stuck: bool,
    /// The tire held sideways this step (keep the anchor's sideways place).
    pub side_stuck: bool,
}

/// `value`, or 0 when it is not finite.
fn finite_or_zero(value: f32) -> f32 {
    if value.is_finite() { value } else { 0.0 }
}

impl TireState {
    /// The tire in the air or on a bad contact: no force, no anchor. The peak-angle cache stays:
    /// the next grounded step of the same tire must not rerun the Magic Formula search.
    pub(crate) fn release(&mut self) {
        let peak_angle = self.peak_angle;
        let peak_key = self.peak_key;
        *self = Self::default();
        self.forces.slip = true;
        self.peak_angle = peak_angle;
        self.peak_key = peak_key;
    }

    /// `peak_slip_angle(load)`, recomputed only when `tire` or that load's bits change.
    ///
    /// The peak-to-stiffness ratio cancels load on paper. The 48-step search does not:
    /// a different load is not the same bits, and the tire digest pins those bits.
    fn cached_peak_angle(&mut self, tire: &SlipTire, load: f32) -> f32 {
        let key = tire
            .peak_cache_key()
            .wrapping_mul(0x9e37_79b9_7f4a_7c15)
            .wrapping_add(u64::from(load.to_bits()))
            | 1;
        if self.peak_key == key {
            return self.peak_angle;
        }
        let angle = tire.peak_slip_angle(load);
        self.peak_angle = angle;
        self.peak_key = key;
        angle
    }

    /// One step of a slip tire on its contact: the static hold below the stick speed, the curve
    /// above it (through the relaxation lag when the tire has one), the camber thrust, the
    /// combined-slip limit after the lengthwise force, and the limit that keeps the side impulse
    /// from reversing the slide. Non-finite or zero-load contacts give no impulse.
    pub(crate) fn solve(&mut self, tire: &SlipTire, contact: &SlipContact, dt: f32) -> SlipImpulse {
        let values = [
            contact.along_speed,
            contact.side_speed,
            contact.load,
            contact.longitudinal_impulse,
            contact.longitudinal_friction,
            contact.hold_force,
            contact.side_mass,
            contact.along_mass,
            contact.along_offset,
            contact.side_offset,
            contact.camber,
            contact.slip_ratio,
            dt,
        ];
        if values.iter().any(|v| !v.is_finite())
            || contact.load <= MIN_LOAD
            || dt <= 0.0
            || contact.side_mass <= 0.0
            || contact.along_mass <= 0.0
        {
            self.release();
            return SlipImpulse::default();
        }
        let load = contact.load;
        let factor = tire.load_factor(load);
        let side_cap_force = tire.peak_force(load);
        let along_cap_force = contact.longitudinal_friction.max(0.0) * factor * load;
        let (vx, vy) = (contact.along_speed, contact.side_speed);
        let slow = tire.stick_speed > 0.0 && vx * vx + vy * vy < tire.stick_speed * tire.stick_speed;
        let (mut along_stuck, mut side_stuck) = (false, false);

        // Lengthwise hold: only a braked wheel, and only inside both the brake and the grip.
        let mut along_extra = 0.0;
        if slow && contact.hold_force > 0.0 {
            let want = -contact.along_mass * (vx + STICK_BIAS * contact.along_offset / dt);
            let cap = along_cap_force.min(contact.hold_force) * dt;
            if (contact.longitudinal_impulse + want).abs() <= cap {
                along_extra = want;
                along_stuck = true;
            }
        }
        let along_total = contact.longitudinal_impulse + along_extra;

        // The combined slip: what the lengthwise force leaves of the side grip. The ellipse
        // scales the curve and the cap alike; the slip-weighted model scales the curve by the slip
        // ratio's weight and caps the resultant at the friction circle.
        let used = if along_cap_force > 0.0 {
            (along_total.abs() / (along_cap_force * dt)).min(1.0)
        } else if along_total != 0.0 {
            1.0
        } else {
            0.0
        };
        let ellipse = (1.0 - used * used).max(0.0).sqrt();
        let (weight, side_cap) = match tire.combined_slip {
            CombinedSlip::Ellipse => (ellipse, side_cap_force * ellipse * dt),
            CombinedSlip::SlipWeighted { .. } => {
                let along_force = along_total / dt;
                let circle = (side_cap_force * side_cap_force - along_force * along_force).max(0.0);
                (tire.combined_weight(contact.slip_ratio), circle.sqrt() * dt)
            }
        };

        let mut camber_impulse = 0.0;
        let mut curve_angle = 0.0;
        let side = if slow {
            let want = -contact.side_mass * (vy + STICK_BIAS * contact.side_offset / dt);
            let side = if want.abs() <= side_cap {
                side_stuck = true;
                want
            } else {
                // The hold cannot stop the slide: the patch slides, so the aligning trail is
                // the brush trail at the slip angle it sees (past the peak it is ~0), the same
                // angle the rolling path reports — not the full pneumatic trail at angle 0.
                curve_angle = det_atan2(vy, vx.abs());
                side_cap.copysign(want)
            };
            if tire.relaxation_length > 0.0 {
                // The deflection the held force stands for, so the lag starts from it.
                let stiffness = tire.cornering_stiffness_at(load) * weight;
                let limit = SlipTire::peak_slip_tan_from(self.cached_peak_angle(tire, load));
                self.lag = if stiffness > 0.0 {
                    finite_or_zero(-side / (dt * stiffness)).clamp(-limit, limit)
                } else {
                    0.0
                };
            }
            side
        } else {
            let curve = if tire.relaxation_length > 0.0 {
                let curve = self.relaxed_side(tire, contact, weight, dt);
                curve_angle = det_atan(self.lag);
                curve
            } else {
                let slip_tan = if vx.abs() < MIN_ALONG_SPEED {
                    f32::INFINITY
                } else {
                    vy / vx.abs()
                };
                let size = tire.force_size(slip_tan, load) * weight * dt;
                // Never more than the impulse that stops the slide: an explicit slip force is a
                // damper of slope Cα/|vx| and would overshoot at low speed.
                let stop = vy.abs() * contact.side_mass;
                curve_angle = if vx * vx + vy * vy > 0.0 {
                    det_atan2(vy, vx.abs())
                } else {
                    0.0
                };
                -size.min(stop).copysign(vy)
            };
            // The slip-weighted model scales the curve by the slip ratio's weight only, so it must
            // meet the friction-circle cap on this path too: without it a full lengthwise force
            // left the whole side force on top (corr-physics audit). The ellipse already scales
            // the curve by the cap's own factor, so its bits stay as they were.
            let weighted = matches!(tire.combined_slip, CombinedSlip::SlipWeighted { .. });
            if tire.camber_stiffness > 0.0 {
                camber_impulse = tire.camber_thrust(contact.camber, load) * weight * dt;
                let total = curve + camber_impulse;
                let clamped = total.clamp(-side_cap, side_cap);
                if clamped != total && total != 0.0 {
                    // The grip cap cut the pair: scale the camber part with the clamp, so the
                    // `camber_thrust` readout stays inside the `lateral_force` that contains it
                    // and the aligning torque charges the slip part only its own share.
                    camber_impulse *= clamped / total;
                }
                clamped
            } else if tire.relaxation_length > 0.0 || weighted {
                curve.clamp(-side_cap, side_cap)
            } else {
                curve
            }
        };
        let side = finite_or_zero(side);
        let along_extra = finite_or_zero(along_extra);
        let slip_angle = if vx * vx + vy * vy > 0.0 {
            det_atan2(vy, vx.abs())
        } else {
            0.0
        };
        let stuck = along_stuck || side_stuck;
        let camber_thrust = finite_or_zero(camber_impulse / dt);
        let aligning_torque = if stuck || tire.pneumatic_trail <= 0.0 {
            0.0
        } else {
            let slip_force = side / dt - camber_thrust;
            let peak = self.cached_peak_angle(tire, load);
            finite_or_zero(tire.trail_at(curve_angle, load, Some(peak)) * slip_force)
        };
        self.forces = TireForces {
            slip: true,
            lateral_force: side / dt,
            longitudinal_force: finite_or_zero(along_total / dt),
            load,
            slip_angle,
            stuck,
            lagged_slip_angle: if tire.relaxation_length > 0.0 {
                det_atan(self.lag)
            } else {
                slip_angle
            },
            aligning_torque,
            camber: if tire.camber_stiffness > 0.0 {
                contact.camber
            } else {
                0.0
            },
            camber_thrust: if stuck { 0.0 } else { camber_thrust },
        };
        SlipImpulse {
            side,
            along_extra,
            along_stuck,
            side_stuck,
        }
    }

    /// The side impulse of a tire with a relaxation length, and its new lagged slip (ADR-3276).
    ///
    /// The lag is the contact's lateral deflection over the relaxation length σ, as a slip
    /// tangent t: `σ dt/dτ = v_side − |v_along| t`. Over the step it is integrated with the exact
    /// exponential, with the side velocity taken after this impulse (implicit in the velocity), so
    /// a stiff tire on a light contact cannot overshoot; at `v_along` = 0 nothing decays and the
    /// lag only follows the side motion. A deflection damping sized to critical damping of the
    /// deflection mode, `max(0, 2√(m σ C) − m |v_along|)`, keeps a slow rolling tire from
    /// oscillating; it fades out with speed. The deflection never goes past the steady slip or the
    /// curve's peak: a sliding patch stops deflecting.
    fn relaxed_side(&mut self, tire: &SlipTire, contact: &SlipContact, weight: f32, dt: f32) -> f32 {
        let (vx, vy, load, side_mass) = (
            contact.along_speed,
            contact.side_speed,
            contact.load,
            contact.side_mass,
        );
        let sigma = tire.relaxation_length;
        let speed = vx.abs();
        let rate = speed / sigma;
        let x = rate * dt;
        // Deterministic kernels (ADR-5128): e^(−x), and 1 − e^(−x) in f64 so the small-x
        // cancellation keeps f32 precision.
        let decay = det_exp(-x);
        // (1 − e^(−x)) / rate, which is dt at rate 0.
        let gain = if x > 1.0e-6 {
            (1.0 - det_exp_f64(-f64::from(x))) as f32 / rate
        } else {
            dt
        };
        let stiffness = tire.cornering_stiffness_at(load) * weight;
        let mass = side_mass;
        let damping = (2.0 * (mass * sigma * stiffness).sqrt() - mass * speed).max(0.0);
        let coupling = gain / (sigma * mass);
        let before = self.lag;
        let lag = (before * (decay + coupling * damping) + gain * vy / sigma)
            / (1.0 + coupling * (dt * stiffness + damping));
        let steady = if speed >= MIN_ALONG_SPEED {
            (vy / speed).abs().min(MAX_LAG_TAN)
        } else {
            0.0
        };
        let limit = steady.max(SlipTire::peak_slip_tan_from(self.cached_peak_angle(tire, load)));
        let lag = finite_or_zero(lag).clamp(-limit, limit);
        self.lag = lag;
        let size = tire.force_size(lag.abs(), load) * weight * dt;
        finite_or_zero(-size.copysign(lag) - damping * (lag - before))
    }

    /// The state's bits for a digest, in a fixed order.
    pub(crate) fn digest_words(&self) -> [u32; 8] {
        let anchor = self.anchor.unwrap_or_default();
        // The lag is 0 without a relaxation length (bits 0), so a v3 tire keeps its words.
        [
            self.forces.slip_angle.to_bits() ^ self.lag.to_bits().rotate_left(13),
            anchor.x.to_bits(),
            anchor.y.to_bits(),
            anchor.z.to_bits(),
            self.forces.lateral_force.to_bits(),
            self.forces.longitudinal_force.to_bits(),
            self.forces.load.to_bits(),
            u32::from(self.forces.stuck),
        ]
    }
}

/// A tuning hint about a vehicle's tires: the configuration is valid but probably not what the
/// author meant (`physics.vehicle_config_check` reports these).
#[derive(Debug, Clone, PartialEq)]
pub struct TireWarning {
    /// A stable identifier: lowercase ASCII, digits and `_`.
    pub code: &'static str,
    /// The wheel the hint is about, if it is about one.
    pub wheel: Option<usize>,
    /// What is odd, in words.
    pub message: String,
}

/// The slide angle past which a tire reads as vague: about 20 degrees.
const VAGUE_SLIDE_ANGLE: f32 = 0.35;

/// Tuning hints for the tires of `config`, wheel by wheel in order, then the axles.
///
/// Codes: `tire_mixed_axle` (two wheels side by side with different tire models),
/// `tire_roll_influence_low` (a slip tire's side force acts below half the contact's lever, so
/// load transfer is understated), `tire_load_far_from_nominal` (the static load is under a third or
/// over three times `nominal_load`, so the load factor extrapolates), `tire_slide_angle_large`
/// (the curve peaks past about 20 degrees at the static load: the stiffness is low for the grip),
/// `tire_no_static_hold` (`stick_speed` 0: a held brake creeps on a slope) and
/// `tire_side_friction_ignored` (a slip tire with `side_friction_stiffness` other than 1, which only
/// rapier's damper reads).
#[must_use]
pub fn tire_warnings(config: &crate::vehicle::VehicleConfig) -> Vec<TireWarning> {
    let mut warnings = Vec::new();
    let count = config.wheels.len().max(1) as f32;
    let static_load = config.chassis_mass * 9.81 / count;
    for (index, wheel) in config.wheels.iter().enumerate() {
        let Some(tire) = wheel.tire_model.slip() else {
            continue;
        };
        let mut warn = |code: &'static str, message: String| {
            warnings.push(TireWarning {
                code,
                wheel: Some(index),
                message,
            });
        };
        if tire.roll_influence < 0.5 {
            warn(
                "tire_roll_influence_low",
                format!(
                    "roll_influence {} puts the side force near the center of mass's height: load transfer is {} of the geometry's",
                    tire.roll_influence, tire.roll_influence
                ),
            );
        }
        let share = static_load / tire.nominal_load;
        if !(1.0 / 3.0..=3.0).contains(&share) {
            warn(
                "tire_load_far_from_nominal",
                format!(
                    "the static load is about {static_load:.0} N, {share:.2} times nominal_load {}",
                    tire.nominal_load
                ),
            );
        }
        let slide = tire.peak_slip_angle(static_load);
        if slide > VAGUE_SLIDE_ANGLE {
            warn(
                "tire_slide_angle_large",
                format!(
                    "the curve peaks at {:.1} degrees of slip at the static load: cornering_stiffness is low for peak_friction",
                    slide.to_degrees()
                ),
            );
        }
        if tire.stick_speed == 0.0 {
            warn(
                "tire_no_static_hold",
                "stick_speed is 0: a held brake creeps on a slope".to_string(),
            );
        }
        if wheel.side_friction_stiffness != 1.0 {
            warn(
                "tire_side_friction_ignored",
                format!(
                    "side_friction_stiffness {} is only read by the damper tire",
                    wheel.side_friction_stiffness
                ),
            );
        }
    }
    for (index, wheel) in config.wheels.iter().enumerate() {
        let partner = config
            .wheels
            .iter()
            .enumerate()
            .skip(index + 1)
            .find(|(_, other)| {
                (other.position.z - wheel.position.z).abs() < 0.01
                    && (other.position.x - wheel.position.x).abs() > 0.01
                    && other.tire_model.name() != wheel.tire_model.name()
            });
        if let Some((other, _)) = partner {
            warnings.push(TireWarning {
                code: "tire_mixed_axle",
                wheel: Some(index),
                message: format!(
                    "wheels {index} and {other} share an axle but one has the {} tire and the other the {}",
                    wheel.tire_model.name(),
                    config.wheels[other].tire_model.name()
                ),
            });
        }
    }
    warnings
}

// Counts `SlipTire::peak_slip_angle` on this thread. A steady tire pays the Magic
// Formula search once, not once per grounded step.
#[cfg(test)]
thread_local! {
    static PEAK_SLIP_EVALS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mf() -> SlipTire {
        SlipTire {
            curve: LateralCurve::MagicFormula {
                shape: 1.4,
                curvature: -0.5,
            },
            ..SlipTire::default()
        }
    }

    #[test]
    fn both_curves_rise_with_the_authored_stiffness_and_peak_at_mu_load() {
        for tire in [SlipTire::default(), mf()] {
            let load = tire.nominal_load;
            let small = 1.0e-4;
            let slope = -tire.lateral_force(small, load) / small;
            assert!(
                (slope - tire.cornering_stiffness).abs() < 0.01 * tire.cornering_stiffness,
                "{:?}: slope {slope}",
                tire.curve
            );
            let peak = (0..2000)
                .map(|i| -tire.lateral_force(i as f32 * 0.0007, load))
                .fold(0.0_f32, f32::max);
            let want = tire.peak_friction * load;
            assert!(
                (peak - want).abs() < 0.002 * want,
                "{:?}: peak {peak}",
                tire.curve
            );
            let at = tire.peak_slip_angle(load);
            assert!(
                (-tire.lateral_force(at, load) - want).abs() < 0.002 * want,
                "{:?}: force at the peak angle {at}",
                tire.curve
            );
        }
    }

    #[test]
    fn the_brush_curve_is_flat_past_full_sliding_and_the_magic_formula_falls() {
        let brush = SlipTire::default();
        let load = brush.nominal_load;
        let slide = brush.peak_slip_angle(load);
        assert!((slide - (3.0 * load / brush.cornering_stiffness).atan()).abs() < 1.0e-6);
        for angle in [slide, slide * 1.5, 1.2] {
            assert_eq!(-brush.lateral_force(angle, load), load, "{angle}");
        }
        let tire = mf();
        let peak = tire.peak_slip_angle(load);
        assert!(-tire.lateral_force(peak * 3.0, load) < -tire.lateral_force(peak, load));
        assert!(-tire.lateral_force(1.5, load) > 0.0);
    }

    #[test]
    fn the_force_opposes_the_slip_and_is_odd() {
        for tire in [SlipTire::default(), mf()] {
            for angle in [0.01_f32, 0.05, 0.2, 0.6] {
                let right = tire.lateral_force(angle, 3000.0);
                assert!(right < 0.0);
                assert_eq!(tire.lateral_force(-angle, 3000.0), -right);
            }
        }
    }

    #[test]
    fn load_sensitivity_scales_grip_by_the_load_factor() {
        let tire = SlipTire {
            load_exponent: 0.2,
            nominal_load: 1000.0,
            ..SlipTire::default()
        };
        let load = 3000.0_f32;
        let factor = 3.0_f32.powf(-0.2);
        assert!((tire.load_factor(load) - factor).abs() < 1.0e-6);
        assert!((tire.peak_force(load) - factor * load).abs() < 1.0e-2);
        assert_eq!(tire.load_factor(1000.0), 1.0);
        // The grip per newton falls with load, the force still rises.
        assert!(tire.friction_at_load(6000.0) < tire.friction_at_load(3000.0));
        assert!(tire.peak_force(6000.0) > tire.peak_force(3000.0));
        assert_eq!(SlipTire::default().load_factor(1.0e8), 1.0);
    }

    #[test]
    fn non_finite_and_zero_load_inputs_give_zero_force() {
        for tire in [SlipTire::default(), mf()] {
            for (angle, load) in [
                (0.1, 0.0),
                (0.1, -5.0),
                (0.1, f32::NAN),
                (0.1, f32::INFINITY),
                (f32::NAN, 3000.0),
                (f32::INFINITY, 3000.0),
            ] {
                let force = tire.lateral_force(angle, load);
                assert_eq!(force, 0.0, "{angle} {load}");
            }
            assert_eq!(tire.peak_slip_angle(0.0), 0.0);
        }
        let mut state = TireState::default();
        let good = SlipContact {
            along_speed: 20.0,
            side_speed: 1.0,
            load: 3000.0,
            longitudinal_impulse: 0.0,
            longitudinal_friction: 1.0,
            hold_force: 0.0,
            side_mass: 300.0,
            along_mass: 300.0,
            along_offset: 0.0,
            side_offset: 0.0,
            camber: 0.0,
            slip_ratio: 0.0,
        };
        let mut bad_ones = Vec::new();
        for field in 0..12 {
            for value in [f32::NAN, f32::INFINITY] {
                let mut contact = good;
                let slot = match field {
                    0 => &mut contact.along_speed,
                    1 => &mut contact.side_speed,
                    2 => &mut contact.load,
                    3 => &mut contact.longitudinal_impulse,
                    4 => &mut contact.longitudinal_friction,
                    5 => &mut contact.hold_force,
                    6 => &mut contact.side_mass,
                    7 => &mut contact.along_mass,
                    8 => &mut contact.along_offset,
                    9 => &mut contact.side_offset,
                    10 => &mut contact.camber,
                    _ => &mut contact.slip_ratio,
                };
                *slot = value;
                bad_ones.push(contact);
            }
        }
        bad_ones.push(SlipContact { load: 0.0, ..good });
        for contact in bad_ones {
            let out = state.solve(&SlipTire::default(), &contact, 1.0 / 60.0);
            assert_eq!(out, SlipImpulse::default(), "{contact:?}");
            assert_eq!(state.forces.lateral_force, 0.0);
            assert!(
                state
                    .digest_words()
                    .iter()
                    .all(|w| f32::from_bits(*w).is_finite())
            );
        }
        assert_eq!(
            state.solve(&SlipTire::default(), &good, f32::NAN),
            SlipImpulse::default()
        );
        // The good contact gives the curve's force.
        let out = state.solve(&SlipTire::default(), &good, 1.0 / 60.0);
        let want = SlipTire::default().lateral_force((1.0_f32 / 20.0).atan(), 3000.0) / 60.0;
        assert!(
            (out.side - want).abs() < 1.0e-3 * want.abs(),
            "{} vs {want}",
            out.side
        );
    }

    #[test]
    fn the_ellipse_takes_the_side_grip_the_lengthwise_force_uses() {
        let tire = SlipTire::default();
        let dt = 1.0 / 60.0;
        let contact = |fx: f32| SlipContact {
            along_speed: 20.0,
            side_speed: 10.0, // far past the peak: the side force is the cap
            load: 3000.0,
            longitudinal_impulse: fx * dt,
            longitudinal_friction: 1.0,
            hold_force: 0.0,
            side_mass: 1.0e6,
            along_mass: 1.0e6,
            along_offset: 0.0,
            side_offset: 0.0,
            camber: 0.0,
            slip_ratio: 0.0,
        };
        let mut state = TireState::default();
        let free = state.solve(&tire, &contact(0.0), dt).side / dt;
        assert!((free + 3000.0).abs() < 0.5, "{free}");
        for share in [0.3_f32, 0.6, 0.9, 1.0] {
            let side = state.solve(&tire, &contact(-share * 3000.0), dt).side / dt;
            let want = -3000.0 * (1.0 - share * share).sqrt();
            assert!((side - want).abs() < 0.5, "share {share}: {side} vs {want}");
        }
    }

    #[test]
    fn the_side_impulse_never_reverses_the_slide() {
        let tire = SlipTire {
            stick_speed: 0.0,
            ..SlipTire::default()
        };
        let mut state = TireState::default();
        let out = state.solve(
            &tire,
            &SlipContact {
                along_speed: 0.5,
                side_speed: 0.2,
                load: 3000.0,
                longitudinal_impulse: 0.0,
                longitudinal_friction: 1.0,
                hold_force: 0.0,
                side_mass: 50.0,
                along_mass: 50.0,
                along_offset: 0.0,
                side_offset: 0.0,
                camber: 0.0,
                slip_ratio: 0.0,
            },
            1.0 / 60.0,
        );
        assert_eq!(out.side, -0.2 * 50.0);
    }

    #[test]
    fn a_slow_tire_sticks_inside_its_grip_and_slides_past_it() {
        let tire = SlipTire::default();
        let dt = 1.0 / 60.0;
        let contact = SlipContact {
            along_speed: 0.05,
            side_speed: 0.02,
            load: 3000.0,
            longitudinal_impulse: 0.0,
            longitudinal_friction: 1.0,
            hold_force: 3000.0,
            side_mass: 300.0,
            along_mass: 300.0,
            along_offset: 0.002,
            side_offset: -0.001,
            camber: 0.0,
            slip_ratio: 0.0,
        };
        let mut state = TireState::default();
        let out = state.solve(&tire, &contact, dt);
        assert!(state.forces.stuck && out.along_stuck && out.side_stuck);
        // The anchor pulls back: v + β·offset/dt is what the impulse removes.
        let want = -300.0 * (0.05 + STICK_BIAS * 0.002 / dt);
        assert!(
            (out.along_extra - want).abs() < 1.0e-3,
            "{} vs {want}",
            out.along_extra
        );
        assert!(out.along_extra < 0.0 && out.side < 0.0);
        // Not held: no lengthwise hold, the side still sticks.
        let mut free = TireState::default();
        let out = free.solve(
            &tire,
            &SlipContact {
                hold_force: 0.0,
                ..contact
            },
            dt,
        );
        assert_eq!(out.along_extra, 0.0);
        assert!(free.forces.stuck && out.side_stuck && !out.along_stuck);
        // A huge mass asks for more than the grip: the tire slides at the cap.
        let mut sliding = TireState::default();
        let out = sliding.solve(
            &tire,
            &SlipContact {
                side_mass: 1.0e9,
                along_mass: 1.0e9,
                ..contact
            },
            dt,
        );
        assert_eq!(out.along_extra, 0.0);
        assert!((out.side + 3000.0 * dt).abs() < 1.0e-3, "{}", out.side);
        assert!(!out.along_stuck && !out.side_stuck && !sliding.forces.stuck);
    }

    #[test]
    fn validation_names_the_field() {
        let bad = |tire: SlipTire| tire.invalid_field();
        assert_eq!(bad(SlipTire::default()), None);
        assert_eq!(bad(mf()), None);
        assert_eq!(
            bad(SlipTire {
                peak_friction: 0.0,
                ..SlipTire::default()
            }),
            Some("tire_model.peak_friction")
        );
        assert_eq!(
            bad(SlipTire {
                load_exponent: 1.0,
                ..SlipTire::default()
            }),
            Some("tire_model.load_exponent")
        );
        assert_eq!(
            bad(SlipTire {
                curve: LateralCurve::MagicFormula {
                    shape: 2.0,
                    curvature: 0.0
                },
                ..SlipTire::default()
            }),
            Some("tire_model.curve")
        );
        assert_eq!(
            bad(SlipTire {
                roll_influence: f32::NAN,
                ..SlipTire::default()
            }),
            Some("tire_model.roll_influence")
        );
        assert_eq!(
            bad(SlipTire {
                nominal_load: 0.0,
                ..SlipTire::default()
            }),
            Some("tire_model.nominal_load")
        );
    }

    #[test]
    fn warnings_name_odd_tunings_with_stable_codes() {
        use crate::vehicle::VehicleConfig;
        let mut config = VehicleConfig::default();
        assert!(tire_warnings(&config).is_empty());
        for wheel in &mut config.wheels {
            wheel.tire_model = TireModel::Slip(SlipTire::default());
        }
        assert!(tire_warnings(&config).is_empty(), "{:?}", tire_warnings(&config));
        config.wheels[1].tire_model = TireModel::Damper;
        config.wheels[2].tire_model = TireModel::Slip(SlipTire {
            roll_influence: 0.1,
            nominal_load: 100.0,
            cornering_stiffness: 200.0,
            stick_speed: 0.0,
            ..SlipTire::default()
        });
        config.wheels[3].side_friction_stiffness = 2.0;
        let codes: Vec<(&str, Option<usize>)> =
            tire_warnings(&config).iter().map(|w| (w.code, w.wheel)).collect();
        assert_eq!(
            codes,
            vec![
                ("tire_roll_influence_low", Some(2)),
                ("tire_load_far_from_nominal", Some(2)),
                ("tire_slide_angle_large", Some(2)),
                ("tire_no_static_hold", Some(2)),
                ("tire_side_friction_ignored", Some(3)),
                ("tire_mixed_axle", Some(0)),
            ]
        );
        for warning in tire_warnings(&config) {
            assert!(warning.code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'));
            assert!(!warning.message.is_empty());
        }
    }

    // --- Transients (ADR-3276) ---

    /// A sliding contact at `vx`, `vy` with no lengthwise force.
    fn moving(vx: f32, vy: f32, side_mass: f32) -> SlipContact {
        SlipContact {
            along_speed: vx,
            side_speed: vy,
            load: 3000.0,
            longitudinal_impulse: 0.0,
            longitudinal_friction: 1.0,
            hold_force: 0.0,
            side_mass,
            along_mass: side_mass,
            along_offset: 0.0,
            side_offset: 0.0,
            camber: 0.0,
            slip_ratio: 0.0,
        }
    }

    #[test]
    fn the_defaults_have_no_transients_and_the_new_fields_are_validated() {
        assert!(!SlipTire::default().has_transients());
        assert!(!mf().has_transients());
        let bad = |tire: SlipTire| tire.invalid_field();
        for (tire, field) in [
            (
                SlipTire {
                    relaxation_length: -0.1,
                    ..SlipTire::default()
                },
                "tire_model.relaxation_length",
            ),
            (
                SlipTire {
                    pneumatic_trail: 2.0,
                    ..SlipTire::default()
                },
                "tire_model.pneumatic_trail",
            ),
            (
                SlipTire {
                    camber_stiffness: f32::NAN,
                    ..SlipTire::default()
                },
                "tire_model.camber_stiffness",
            ),
            (
                SlipTire {
                    static_camber: 0.6,
                    ..SlipTire::default()
                },
                "tire_model.static_camber",
            ),
            (
                SlipTire {
                    combined_slip: CombinedSlip::SlipWeighted {
                        shape: 1.5,
                        stiffness: 10.0,
                    },
                    ..SlipTire::default()
                },
                "tire_model.combined_slip",
            ),
        ] {
            assert!(tire.has_transients());
            assert_eq!(bad(tire), Some(field));
        }
    }

    #[test]
    fn a_step_steer_at_20_m_s_builds_the_force_with_time_constant_sigma_over_v() {
        let sigma = 1.0_f32;
        let speed = 20.0_f32;
        let dt = 1.0 / 60.0;
        let tau = sigma / speed;
        let tire = SlipTire {
            relaxation_length: sigma,
            ..SlipTire::default()
        };
        let angle = 0.002_f32;
        let contact = moving(speed, speed * angle.tan(), 1.0e9);
        let steady = tire.lateral_force(angle, 3000.0);
        let mut state = TireState::default();
        for n in 1..=12 {
            state.solve(&tire, &contact, dt);
            let t = n as f32 * dt;
            let share = 1.0 - (-t / tau).exp();
            // The lag is the exact first-order response.
            let lag = state.forces.lagged_slip_angle.tan() / angle.tan();
            assert!((lag - share).abs() < 1.0e-3, "step {n}: lag {lag} vs {share}");
            // The force's time constant, read back from each sample, is σ / v within 5 %.
            let ratio = state.forces.lateral_force / steady;
            if ratio < 0.99 {
                let measured = -t / (1.0 - ratio).ln();
                assert!(
                    (measured - tau).abs() < 0.05 * tau,
                    "step {n}: time constant {measured} vs {tau}"
                );
            }
        }
        // The steady force is the curve's, as without a lag.
        for _ in 0..60 {
            state.solve(&tire, &contact, dt);
        }
        assert!((state.forces.lateral_force - steady).abs() < 1.0e-3 * steady.abs());
    }

    #[test]
    fn a_relaxed_tire_at_walking_speed_neither_oscillates_nor_grows() {
        let dt = 1.0 / 60.0;
        for stiffness in [45_000.0_f32, 1.0e6] {
            for sigma in [0.05_f32, 0.3, 1.0] {
                for mass in [20.0_f32, 300.0, 1.0e4] {
                    for stick_speed in [0.0_f32, 0.3] {
                        let tire = SlipTire {
                            cornering_stiffness: stiffness,
                            relaxation_length: sigma,
                            stick_speed,
                            ..SlipTire::default()
                        };
                        let mut state = TireState::default();
                        let mut vy = 0.3_f32;
                        let (mut flips, mut first_half, mut second_half) = (0, 0.0_f32, 0.0_f32);
                        let mut last_sign = 1.0_f32;
                        for step in 0..600 {
                            let out = state.solve(&tire, &moving(0.5, vy, mass), dt);
                            vy += out.side / mass;
                            assert!(vy.is_finite() && state.lag.is_finite());
                            if vy.abs() > 1.0e-6 && vy.signum() != last_sign {
                                flips += 1;
                                last_sign = vy.signum();
                            }
                            if step < 300 {
                                first_half = first_half.max(vy.abs());
                            } else {
                                second_half = second_half.max(vy.abs());
                            }
                        }
                        let case = format!("C {stiffness} σ {sigma} m {mass} stick {stick_speed}");
                        assert!(flips <= 1, "{case}: side velocity changed sign {flips} times");
                        assert!(
                            second_half <= first_half,
                            "{case}: grew {first_half} -> {second_half}"
                        );
                        assert!(vy.abs() < 0.3, "{case}: did not settle ({vy})");
                    }
                }
            }
        }
    }

    #[test]
    fn the_lag_holds_at_zero_speed_and_follows_the_side_motion() {
        let tire = SlipTire {
            relaxation_length: 0.5,
            stick_speed: 0.0,
            ..SlipTire::default()
        };
        let dt = 1.0 / 60.0;
        let mut state = TireState {
            lag: 0.01,
            ..TireState::default()
        };
        for _ in 0..600 {
            state.solve(&tire, &moving(0.0, 0.0, 1.0e9), dt);
            assert_eq!(state.lag, 0.01);
        }
        // Standing still and pushed sideways, the deflection grows by v_side · dt / σ.
        state.solve(&tire, &moving(0.0, 0.003, 1.0e9), dt);
        assert!(
            (state.lag - (0.01 + 0.003 * dt / 0.5)).abs() < 1.0e-6,
            "{}",
            state.lag
        );
        // In the air it lets go.
        state.release();
        assert_eq!(state.lag, 0.0);
    }

    #[test]
    fn aligning_torque_turns_the_wheel_toward_its_path_and_falls_past_the_peak() {
        for curve in [SlipTire::default().curve, mf().curve] {
            let tire = SlipTire {
                curve,
                pneumatic_trail: 0.04,
                ..SlipTire::default()
            };
            let load = tire.nominal_load;
            assert_eq!(tire.pneumatic_trail_at(0.0, load), 0.04);
            let peak = tire.peak_slip_angle(load);
            let torque = |angle: f32| tire.aligning_torque(angle, load);
            // A contact moving right: the torque turns the wheel right (negative), back to its path.
            assert!(torque(0.02) < 0.0 && torque(-0.02) > 0.0);
            let (max_angle, max_torque) = (1..400)
                .map(|i| peak * i as f32 / 200.0)
                .map(|angle| (angle, -torque(angle)))
                .fold((0.0, 0.0), |best, row| if row.1 > best.1 { row } else { best });
            assert!(
                max_angle < peak,
                "{curve:?}: the torque peaks at {max_angle}, the force at {peak}"
            );
            assert!(
                -torque(peak) < 0.05 * max_torque,
                "{curve:?}: {} at the peak",
                -torque(peak)
            );
            let far = torque(peak * 2.0);
            match curve {
                LateralCurve::Brush => assert_eq!(far, 0.0),
                LateralCurve::MagicFormula { .. } => {
                    // Slightly past zero: the trail is negative, the torque turns the wheel out.
                    assert!(far > 0.0 && far < 0.2 * max_torque, "{far}");
                }
            }
            assert_eq!(SlipTire::default().aligning_torque(0.05, load), 0.0);
            assert_eq!(tire.aligning_torque(f32::NAN, load), 0.0);
            assert_eq!(tire.aligning_torque(0.05, 0.0), 0.0);
        }
    }

    /// corr-low P3: when the slip force and the camber thrust together pass the grip, the side
    /// force is clamped, but the `camber_thrust` readout stayed the unclamped thrust. It read more
    /// than the `lateral_force` that holds it, and the aligning torque charged the whole clamp
    /// to the slip part (here flipping its sign). The clamp now scales both parts alike.
    #[test]
    fn a_clamped_side_force_scales_the_camber_thrust_readout_with_it() {
        let tire = SlipTire {
            camber_stiffness: 200_000.0,
            pneumatic_trail: 0.04,
            ..SlipTire::default()
        };
        let mut state = TireState::default();
        // The slide pushes toward +right (vy < 0) and the camber leans the same way.
        let contact = SlipContact {
            camber: 0.1,
            ..moving(12.0, -0.9, 300.0)
        };
        state.solve(&tire, &contact, 1.0 / 60.0);
        let forces = state.forces;
        assert!(!forces.stuck);
        let cap = tire.peak_force(contact.load);
        assert!(
            (forces.lateral_force - cap).abs() < 1.0e-2,
            "the side force is at the grip: {} vs {cap}",
            forces.lateral_force
        );
        assert!(
            forces.camber_thrust > 0.0 && forces.camber_thrust <= forces.lateral_force,
            "camber thrust {} inside a side force of {}",
            forces.camber_thrust,
            forces.lateral_force
        );
        let trail = tire.pneumatic_trail_at(det_atan2(-0.9, 12.0), contact.load);
        assert!(
            forces.aligning_torque >= 0.0 && forces.aligning_torque <= trail * forces.lateral_force,
            "aligning torque {} from a slip part of the same sign as the side force",
            forces.aligning_torque
        );
    }

    /// corr-low P4: a tire sliding below the stick speed (the hold could not stop it) used the
    /// trail at a slip angle of 0, the full pneumatic trail, though the patch slides. It now uses
    /// the slip angle it saw, as the rolling path does: past the peak a brush trail is 0.
    #[test]
    fn a_slow_sliding_tire_uses_the_trail_at_its_slip_angle() {
        let tire = SlipTire {
            pneumatic_trail: 0.04,
            ..SlipTire::default()
        };
        let mut state = TireState::default();
        // Under the 0.3 m/s stick speed, but far too heavy a side mass for the grip to hold.
        state.solve(&tire, &moving(0.01, 0.05, 1.0e6), 1.0 / 60.0);
        let forces = state.forces;
        assert!(!forces.stuck, "the hold cannot stop a 1,000 t side mass");
        assert!(forces.lateral_force.abs() > 0.0);
        let trail = tire.pneumatic_trail_at(forces.slip_angle, forces.load);
        assert!(
            forces.aligning_torque.abs() <= trail.abs() * forces.lateral_force.abs() + 1.0e-3,
            "aligning torque {} vs the trail {trail} at the slip angle {}",
            forces.aligning_torque,
            forces.slip_angle
        );
    }

    #[test]
    fn camber_thrust_matches_the_closed_form() {
        let tire = SlipTire {
            camber_stiffness: 4500.0,
            load_exponent: 0.2,
            ..SlipTire::default()
        };
        let load = 4000.0_f32;
        let gamma = 0.05_f32;
        let want = 4500.0 * (load / 3000.0) * (load / 3000.0).powf(-0.2) * gamma;
        assert!((tire.camber_thrust(gamma, load) - want).abs() < 1.0e-3 * want);
        assert_eq!(tire.camber_thrust(-gamma, load), -tire.camber_thrust(gamma, load));
        assert_eq!(SlipTire::default().camber_thrust(gamma, load), 0.0);
        // Negative static camber leans both tops toward the center line.
        assert!((contact_camber(-0.05, -0.8, 0.0) - 0.05).abs() < 1.0e-7);
        assert!((contact_camber(-0.05, 0.8, 0.0) + 0.05).abs() < 1.0e-7);
        // The axle's right end dipping below the ground plane leans the top right.
        assert!((contact_camber(0.0, 0.8, -0.1_f32.sin()) - 0.1).abs() < 1.0e-6);
        assert_eq!(contact_camber(0.0, 0.8, f32::NAN), 0.0);
        // Rolling straight with no slip, the tire pushes exactly the thrust toward its lean.
        let mut state = TireState::default();
        let contact = SlipContact {
            load,
            camber: gamma,
            ..moving(20.0, 0.0, 300.0)
        };
        let out = state.solve(&tire, &contact, 1.0 / 60.0);
        assert!(
            (out.side * 60.0 - want).abs() < 1.0e-2 * want,
            "{}",
            out.side * 60.0
        );
        assert!((state.forces.camber_thrust - out.side * 60.0).abs() < 1.0e-3 * want);
        assert_eq!(state.forces.camber, gamma);
    }

    #[test]
    fn the_combined_models_never_exceed_mu_fz() {
        let dt = 1.0 / 60.0;
        let weighted = CombinedSlip::SlipWeighted {
            shape: 1.0,
            stiffness: 10.0,
        };
        for combined_slip in [CombinedSlip::Ellipse, weighted] {
            for relaxation_length in [0.0_f32, 0.5] {
                let tire = SlipTire {
                    combined_slip,
                    relaxation_length,
                    camber_stiffness: 20_000.0,
                    curve: LateralCurve::MagicFormula {
                        shape: 1.4,
                        curvature: -0.5,
                    },
                    ..SlipTire::default()
                };
                let mut state = TireState::default();
                let limit = tire.peak_force(3000.0);
                for i in 0..400 {
                    let share = (i % 11) as f32 / 10.0;
                    let contact = SlipContact {
                        longitudinal_impulse: share * limit * dt * if i % 2 == 0 { 1.0 } else { -1.0 },
                        camber: ((i % 7) as f32 - 3.0) * 0.1,
                        slip_ratio: ((i % 5) as f32 - 2.0) * 0.2,
                        ..moving(15.0, ((i % 13) as f32 - 6.0) * 2.0, 400.0)
                    };
                    let out = state.solve(&tire, &contact, dt);
                    let fx = state.forces.longitudinal_force;
                    let fy = out.side / dt;
                    let total = (fx * fx + fy * fy).sqrt();
                    assert!(
                        total <= limit * 1.0001,
                        "{combined_slip:?} σ {relaxation_length} step {i}: {total} > {limit}"
                    );
                }
            }
        }
        let tire = SlipTire {
            combined_slip: weighted,
            ..SlipTire::default()
        };
        assert_eq!(tire.combined_weight(0.0), 1.0);
        assert!(tire.combined_weight(0.1) < 1.0);
        assert!(tire.combined_weight(1.0e6) < 1.0e-3);
        assert_eq!(SlipTire::default().combined_weight(0.5), 1.0);
    }

    #[test]
    fn the_slip_weighted_model_keeps_the_circle_without_camber_or_lag() {
        // Corr-physics audit: with no camber stiffness and no relaxation length the side force
        // left the solve without the friction-circle cap, so at slip ratio 0 (the `Simple`
        // drivetrain) a full lengthwise force came with the full side force: √2 · µ Fz.
        let dt = 1.0 / 60.0;
        let tire = SlipTire {
            combined_slip: CombinedSlip::SlipWeighted {
                shape: 1.0,
                stiffness: 10.0,
            },
            ..SlipTire::default()
        };
        assert_eq!(tire.camber_stiffness, 0.0);
        assert_eq!(tire.relaxation_length, 0.0);
        let limit = tire.peak_force(3000.0);
        for share in [0.5_f32, 0.9, 1.0] {
            let mut state = TireState::default();
            let contact = SlipContact {
                longitudinal_impulse: share * limit * dt,
                ..moving(15.0, 6.0, 400.0)
            };
            let out = state.solve(&tire, &contact, dt);
            let fx = state.forces.longitudinal_force;
            let fy = out.side / dt;
            let total = (fx * fx + fy * fy).sqrt();
            assert!(total <= limit * 1.0001, "share {share}: {total} > {limit}");
            assert!(
                fy.abs() > 0.0 || share >= 1.0,
                "share {share}: the side force vanished"
            );
        }
    }

    #[test]
    fn steering_torque_sums_the_steered_wheels_per_axle() {
        use crate::vehicle::VehicleConfig;
        let config = VehicleConfig::default();
        let forces: Vec<TireForces> = (0..4)
            .map(|i| TireForces {
                aligning_torque: (i + 1) as f32,
                ..TireForces::default()
            })
            .collect();
        let mut axles = Vec::new();
        steering_torques_into(&config, &forces, &mut axles);
        assert_eq!(
            axles,
            vec![AxleSteeringTorque {
                position_z: -1.3,
                wheels: 2,
                torque: 3.0
            }]
        );
        assert_eq!(total_steering_torque(&config, &forces), 3.0);
        assert_eq!(total_steering_torque(&config, &[]), 0.0);
    }

    // --- ADR-4773 determinism pins (audit G finding 7): the tire forces, the load factor,
    // the stored slip angle and the digest words must be bit-identical on every host, so they
    // come from the deterministic kernels, never platform powf/atan/sin/atan2.
    const LOAD_FACTOR_HALF_BITS: u32 = 1_066_600_588;
    const LOAD_FACTOR_0741_BITS: u32 = 1_065_871_501;
    const MF_FORCE_BITS: u32 = 1_161_445_155;
    const BRUSH_FORCE_BITS: u32 = 1_161_527_296;
    const MF_LATERAL_BITS: u32 = 3_308_877_019;
    const MF_PEAK_ANGLE_BITS: u32 = 1_042_656_785;
    const BRUSH_PEAK_ANGLE_BITS: u32 = 1_045_045_776;
    const SOLVE_SLIP_ANGLE_BITS: u32 = 1_041_133_354;

    #[test]
    fn load_factor_and_curve_forces_are_deterministic() {
        let tire = SlipTire {
            load_exponent: 0.2,
            nominal_load: 3000.0,
            ..SlipTire::default()
        };
        // The audit's case: 1500 N on a 3000 N nominal tire, road-tire load exponent.
        assert_eq!(
            tire.load_factor(1500.0).to_bits(),
            LOAD_FACTOR_HALF_BITS,
            "0.5^-0.2 through the software pow"
        );
        // A ratio where common libms disagree with the kernel in the last ulp: the kernel's
        // bits are the contract.
        assert_eq!(
            tire.load_factor(2223.0).to_bits(),
            LOAD_FACTOR_0741_BITS,
            "0.741^-0.2 through the software pow"
        );
        let mf_tire = mf();
        assert_eq!(
            mf_tire.force_size(0.2, 3000.0).to_bits(),
            MF_FORCE_BITS,
            "the Magic Formula's atan/sin must be the deterministic kernels"
        );
        let brush = SlipTire::default();
        assert_eq!(
            brush.force_size(0.2, 3000.0).to_bits(),
            BRUSH_FORCE_BITS,
            "the brush curve shares the load factor path"
        );
        assert_eq!(
            mf_tire.lateral_force(0.13, 3000.0).to_bits(),
            MF_LATERAL_BITS,
            "tan = det_sin/det_cos feeds the curve"
        );
        assert_eq!(
            mf_tire.peak_slip_angle(3000.0).to_bits(),
            MF_PEAK_ANGLE_BITS,
            "the bisection's atan/tan must be the deterministic kernels"
        );
        assert_eq!(
            brush.peak_slip_angle(3000.0).to_bits(),
            BRUSH_PEAK_ANGLE_BITS,
            "the brush peak angle's atan must be the deterministic kernel"
        );
    }

    /// glibc `cosf` of `0x3d001e53` is `0x3f7fdff2`; [`det_cos`] is `0x3f7fdff1`.
    /// `tanf` of that angle round-trips through `atanf`, so this slip ratio makes
    /// `combined_weight` evaluate that cosine. The kernel bits are the contract (ADR-4773).
    #[test]
    fn combined_weight_uses_the_deterministic_atan_and_cos() {
        let slip = f32::from_bits(0x3d00_2906);
        let tire = SlipTire {
            combined_slip: CombinedSlip::SlipWeighted {
                shape: 1.0,
                stiffness: 1.0,
            },
            ..SlipTire::default()
        };
        assert_eq!(
            tire.combined_weight(slip).to_bits(),
            0x3f7f_dff1,
            "slip-weighted combined weight must be det_cos(det_atan), not the platform libm"
        );
    }

    /// Platform `tan` of slip angle `0x3aa3db80` gives brush trail `0x3d4a3ebd`.
    /// `det_tan` gives `0x3d4a3ebe`. The trail follows the kernel (ADR-4773).
    #[test]
    fn brush_trail_uses_the_deterministic_tan() {
        use regia_core::math::det_tan;
        let angle = f32::from_bits(0x3aa3_db80);
        let tire = SlipTire {
            pneumatic_trail: 0.05,
            ..SlipTire::default()
        };
        let peak = tire.peak_force(3000.0);
        let stiffness = tire.cornering_stiffness_at(3000.0);
        let from_tan = |tan: f32| {
            let z = (stiffness * tan / (3.0 * peak)).min(1.0);
            let rest = 1.0 - z;
            tire.pneumatic_trail * (rest * rest * rest / (1.0 - z + z * z / 3.0))
        };
        let kernel = from_tan(det_tan(angle));
        assert_eq!(
            tire.pneumatic_trail_at(angle, 3000.0).to_bits(),
            kernel.to_bits(),
            "brush trail {:08x} vs det_tan {:08x}",
            tire.pneumatic_trail_at(angle, 3000.0).to_bits(),
            kernel.to_bits()
        );
    }

    #[test]
    fn solve_slip_angle_and_digest_are_deterministic() {
        let tire = mf();
        let mut state = TireState::default();
        let contact = SlipContact {
            along_speed: 2.5,
            side_speed: 0.35,
            load: 3000.0,
            longitudinal_impulse: 0.0,
            longitudinal_friction: 1.0,
            hold_force: 0.0,
            side_mass: 300.0,
            along_mass: 300.0,
            along_offset: 0.0,
            side_offset: 0.0,
            camber: 0.0,
            slip_ratio: 0.0,
        };
        state.solve(&tire, &contact, 1.0 / 60.0);
        assert_eq!(
            state.forces.slip_angle.to_bits(),
            SOLVE_SLIP_ANGLE_BITS,
            "vy.atan2(|vx|) must be the deterministic kernel"
        );
        assert_eq!(
            state.digest_words()[0],
            SOLVE_SLIP_ANGLE_BITS,
            "the digest's first word is the slip angle's bits; a rollback checksum must agree"
        );
    }

    // ADR-5128: the transient-tire paths (ADR-3276) merged after ADR-4773 called platform libm
    // (tan, cos, atan, atan2, asin, exp, exp_m1). Each check below compares the bits with the
    // deterministic kernels; under a libm that rounds differently (another platform, or the 1-ulp
    // LD_PRELOAD shim on Linux) a platform call fails them.

    #[test]
    fn transient_readouts_use_the_deterministic_kernels() {
        for i in 0..64 {
            let a = -0.9 + 1.8 * i as f32 / 63.0;
            assert_eq!(
                contact_camber(0.0, 1.0, a).to_bits(),
                det_asin(-a).to_bits(),
                "camber lean at {a}"
            );
        }
        let brush = SlipTire {
            pneumatic_trail: 0.04,
            ..SlipTire::default()
        };
        let magic = SlipTire {
            pneumatic_trail: 0.04,
            ..mf()
        };
        let load = 3000.0;
        for i in 1..48 {
            let angle = 0.004 * i as f32;
            let peak = brush.peak_force(load);
            let z = (brush.cornering_stiffness_at(load) * det_tan(angle) / (3.0 * peak)).min(1.0);
            let rest = 1.0 - z;
            let want = 0.04 * (rest * rest * rest / (1.0 - z + z * z / 3.0));
            assert_eq!(
                brush.pneumatic_trail_at(angle, load).to_bits(),
                want.to_bits(),
                "brush trail at {angle}"
            );
            let at_peak = magic.peak_slip_angle(load).max(1.0e-6);
            let want = 0.04 * det_cos(std::f32::consts::FRAC_PI_2 * (angle / at_peak).min(TRAIL_TAIL));
            assert_eq!(
                magic.pneumatic_trail_at(angle, load).to_bits(),
                want.to_bits(),
                "magic-formula trail at {angle}"
            );
        }
        let weighted = SlipTire {
            combined_slip: CombinedSlip::SlipWeighted {
                shape: 1.1,
                stiffness: 9.0,
            },
            ..SlipTire::default()
        };
        for i in 0..40 {
            let slip = -0.5 + i as f32 / 39.0;
            let want = det_cos(1.1 * det_atan(9.0 * slip.abs())).clamp(0.0, 1.0);
            assert_eq!(
                weighted.combined_weight(slip).to_bits(),
                want.to_bits(),
                "combined weight at {slip}"
            );
        }
        for tire in [SlipTire::default(), mf()] {
            let angle = tire.peak_slip_angle(load);
            assert_eq!(
                SlipTire::peak_slip_tan_from(angle).to_bits(),
                det_tan(angle).clamp(0.0, MAX_LAG_TAN).to_bits(),
                "{:?}: peak slip tangent",
                tire.curve
            );
        }
    }

    #[test]
    fn transient_solves_use_the_deterministic_kernels() {
        let dt = 1.0 / 60.0;
        // No relaxation: the aligning torque's trail reads the slip angle det_atan2(vy, |vx|).
        let tire = SlipTire {
            pneumatic_trail: 0.04,
            ..mf()
        };
        for vy in [0.05_f32, 0.35, 0.9, -1.7] {
            let mut state = TireState::default();
            state.solve(&tire, &moving(12.0, vy, 300.0), dt);
            let angle = det_atan2(vy, 12.0_f32.abs());
            let want = tire.pneumatic_trail_at(angle, 3000.0) * state.forces.lateral_force;
            assert_eq!(
                state.forces.aligning_torque.to_bits(),
                want.to_bits(),
                "aligning torque at vy {vy}"
            );
        }
        // Relaxation: the lagged slip angle is det_atan of the lag, and the lag integrates with the
        // deterministic exponential (two hosts agree on every step's bits, checked by the vehicle
        // digest; here the readout).
        let tire = SlipTire {
            relaxation_length: 0.5,
            pneumatic_trail: 0.04,
            ..mf()
        };
        let mut state = TireState::default();
        for _ in 0..30 {
            state.solve(&tire, &moving(15.0, 0.6, 300.0), dt);
            assert_eq!(
                state.forces.lagged_slip_angle.to_bits(),
                det_atan(state.lag).to_bits(),
                "lagged slip angle"
            );
        }
    }

    /// A grounded Magic Formula tire with relaxation and trail shares one 48-step search
    /// across a steady load. A different load's bits recompute it.
    #[test]
    fn a_steady_magic_formula_tire_evaluates_its_peak_once() {
        let tire = SlipTire {
            relaxation_length: 0.5,
            pneumatic_trail: 0.04,
            ..mf()
        };
        assert_eq!(
            tire.peak_slip_angle(tire.nominal_load).to_bits(),
            tire.peak_slip_angle(8_000.0).to_bits(),
            "peak slip angle changed with load"
        );
        let sensitive = SlipTire {
            load_exponent: 0.2,
            ..tire.clone()
        };
        assert_eq!(
            sensitive.peak_slip_angle(sensitive.nominal_load).to_bits(),
            sensitive.peak_slip_angle(8_000.0).to_bits(),
            "load exponent changed the peak angle"
        );
        let contact = moving(15.0, 0.6, 300.0);
        let dt = 1.0 / 60.0;
        let mut first = TireState::default();
        let mut second = TireState::default();
        let impulse_a = first.solve(&tire, &contact, dt);
        let impulse_b = second.solve(&tire, &contact, dt);
        assert_eq!(impulse_a, impulse_b);
        assert_eq!(first.forces, second.forces);
        assert_eq!(first.lag.to_bits(), second.lag.to_bits());

        PEAK_SLIP_EVALS.with(|c| c.set(0));
        // A fresh state: the pair above already filled `first`'s cache.
        let mut measured = TireState::default();
        for _ in 0..32 {
            measured.solve(&tire, &contact, dt);
        }
        let evals = PEAK_SLIP_EVALS.with(|c| c.get());
        assert_eq!(evals, 1, "peak_slip_angle ran {evals} times over 32 steady steps");

        let other = SlipTire {
            peak_friction: 1.2,
            ..tire.clone()
        };
        measured.solve(&other, &contact, dt);
        assert_eq!(
            PEAK_SLIP_EVALS.with(|c| c.get()),
            2,
            "a different tire reused the cached peak"
        );
        measured.release();
        measured.solve(&other, &contact, dt);
        assert_eq!(
            PEAK_SLIP_EVALS.with(|c| c.get()),
            2,
            "release dropped the cached peak"
        );
    }

    #[test]
    fn the_relaxation_step_uses_the_deterministic_exponential() {
        // One relaxed step from a small lag at constant side speed: the lag is exactly the closed
        // form the code evaluates, with e^(−x) from det_exp and (1 − e^(−x)) from the f64 kernel.
        let tire = SlipTire {
            relaxation_length: 0.5,
            ..SlipTire::default()
        };
        let contact = moving(15.0, 0.6, 300.0);
        let dt = 1.0 / 60.0;
        let before = 0.01_f32;
        let mut state = TireState {
            lag: before,
            ..TireState::default()
        };
        let _ = state.relaxed_side(&tire, &contact, 1.0, dt);
        let sigma = 0.5_f32;
        let rate = 15.0 / sigma;
        let x = rate * dt;
        let decay = det_exp(-x);
        let gain = (1.0 - det_exp_f64(-f64::from(x))) as f32 / rate;
        let stiffness = tire.cornering_stiffness_at(3000.0);
        let mass = 300.0_f32;
        let damping = (2.0 * (mass * sigma * stiffness).sqrt() - mass * 15.0).max(0.0);
        let coupling = gain / (sigma * mass);
        let want = (before * (decay + coupling * damping) + gain * 0.6 / sigma)
            / (1.0 + coupling * (dt * stiffness + damping));
        let steady = (0.6_f32 / 15.0).abs().min(MAX_LAG_TAN);
        let limit = steady.max(SlipTire::peak_slip_tan_from(tire.peak_slip_angle(3000.0)));
        let want = want.clamp(-limit, limit);
        assert_eq!(state.lag.to_bits(), want.to_bits(), "lag {} vs {want}", state.lag);
    }
}
