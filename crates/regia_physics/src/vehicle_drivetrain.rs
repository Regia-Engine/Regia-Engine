// SPDX-License-Identifier: MPL-2.0
// Source: Pacejka, "Tire and Vehicle Dynamics" (the slip ratio (Ωr − Vx)/|Vx| and slip angle
// atan(Vy/|Vx|) definitions); Gillespie, "Fundamentals of Vehicle Dynamics" (engine torque through
// the gear ratios, drag and rolling resistance, torque flow through open and locked differentials);
// Milliken and Milliken, "Race Car Vehicle Dynamics" (limited-slip torque bias). Implicit Euler for
// stiff rotating inertias is textbook numerics. The implementation is Regia Engine's own and uses
// only + − × ÷ and sqrt, so a run's bits do not depend on a platform's libm.

//! The geared drivetrain of a ray-cast vehicle (ADR-2598): engine, clutch, gearbox, differentials,
//! wheel spin, tire longitudinal force and the driver aids.
//!
//! [`DrivetrainState::step`] is a pure fixed-step function of the configuration, the driver input
//! and each wheel's contact (load and ground speed), so its answers can be checked without a
//! world. [`Vehicle`](crate::vehicle::Vehicle) calls it after rapier's suspension pass and applies
//! the resulting tire impulses to the chassis.
//!
//! # Model
//!
//! Torque goes engine → slip clutch → gearbox and final drive → centre differential (AWD) → axle
//! differentials → wheels. Every wheel has its own angular speed and feels the differential's
//! torque, the brake, rolling resistance and the tire's force. The tire force follows the slip
//! ratio on a piecewise-linear curve: it rises to `peak_friction · load` at `peak_slip` and falls
//! to `slide_friction · load` at a slip of 1 (a locked or a fully spinning wheel).
//!
//! A wheel's inertia is small against the tire's slope, so a plain explicit step explodes. Each
//! fixed step runs [`DRIVETRAIN_SUBSTEPS`] substeps of a linearly implicit solve: every wheel is an
//! element `ω' = a·τ + w` in its input torque, differentials combine two elements into one, and the
//! clutch either locks (its torque comes from the combined element) or slips at its capacity.
//!
//! No hash map or other order-dependent container is involved: the same inputs give a
//! bit-identical [`DrivetrainState::digest`].

use serde::{Deserialize, Serialize};

use crate::vehicle::{VehicleError, WheelConfig};

/// Substeps of the drivetrain solve per fixed step.
pub const DRIVETRAIN_SUBSTEPS: u32 = 4;

/// The most forward gears one gearbox may have.
pub const MAX_FORWARD_GEARS: usize = 16;

/// Radians per second to revolutions per minute.
const RPM_PER_RAD_S: f32 = 60.0 / std::f32::consts::TAU;

/// Below this ground speed the slip ratio's denominator stops shrinking (m/s), so a standing wheel
/// has a finite slip and a stopped car does not divide by zero.
const SLIP_SPEED_FLOOR: f32 = 1.0;

/// Below this ground speed (m/s) a throttle against the direction of travel may change direction:
/// the automatic gearbox selects reverse or first instead of braking.
const DIRECTION_CHANGE_SPEED: f32 = 1.0;

/// Below this share of the peak friction the lateral grip does not fall, however hard the tire
/// works lengthwise, so a locked wheel still has some side force.
const MIN_LATERAL_SHARE: f32 = 0.2;

/// Time an automatic gearbox waits after a shift ends before the next decision (s).
const SHIFT_DWELL: f32 = 0.25;

/// A downshift must land this far under the up threshold, or the two thresholds would hunt.
const DOWNSHIFT_MARGIN: f32 = 0.98;

/// The largest size of a drivetrain, aid, bar or aero setting: far beyond any vehicle, and low enough
/// that products of settings stay finite in `f32`.
const MAX_SETTING: f32 = 1.0e9;

/// The smallest positive setting: below it, a division by the setting would leave `f32`.
const MIN_POSITIVE_SETTING: f32 = 1.0e-6;

/// A vehicle's drivetrain in `.vehicle.ron`.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub enum Drivetrain {
    /// `max_engine_force` per driven wheel, the rapier controller's own forward and brake
    /// impulses, and no engine state: the model every v1 file has, bit for bit.
    #[default]
    Simple,
    /// Engine, gearbox, differentials and per-wheel spin. `max_engine_force`,
    /// `rolling_resistance`, the wheels' `drive` flags and `friction_slip` are not used.
    Geared(DrivetrainConfig),
}

/// The engine: torque curve, speed limits, inertia and closed-throttle braking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EngineConfig {
    /// Full-throttle torque curve, `(rpm, newton-meters)`, rpm strictly rising, at least two
    /// points, some torque above zero. Linear between points, flat beyond the ends.
    pub torque_curve: Vec<(f32, f32)>,
    /// The engine never runs below this speed (rpm, > 0); it does not stall.
    pub idle_rpm: f32,
    /// Fuel is cut at this speed (rpm, > `idle_rpm`).
    pub redline_rpm: f32,
    /// Rotating inertia of the engine and flywheel (kg·m², > 0).
    pub inertia: f32,
    /// Closed-throttle braking torque at the redline (newton-meters, ≥ 0); linear in rpm.
    pub engine_braking: f32,
}

impl Default for EngineConfig {
    /// A 300 N·m engine that revs to 7,000 rpm.
    fn default() -> Self {
        Self {
            torque_curve: vec![
                (1000.0, 180.0),
                (2500.0, 260.0),
                (4500.0, 300.0),
                (6000.0, 270.0),
                (7000.0, 220.0),
            ],
            idle_rpm: 900.0,
            redline_rpm: 7000.0,
            inertia: 0.2,
            engine_braking: 60.0,
        }
    }
}

impl EngineConfig {
    /// Full-throttle torque at `rpm` (newton-meters): linear between the curve's points.
    #[must_use]
    pub fn torque_at(&self, rpm: f32) -> f32 {
        let curve = &self.torque_curve;
        let Some((&(first_rpm, first_torque), &(last_rpm, last_torque))) = curve.first().zip(curve.last())
        else {
            return 0.0;
        };
        if rpm <= first_rpm {
            return first_torque;
        }
        if rpm >= last_rpm {
            return last_torque;
        }
        for pair in curve.windows(2) {
            let (low_rpm, low_torque) = pair[0];
            let (high_rpm, high_torque) = pair[1];
            if rpm <= high_rpm {
                return low_torque + (high_torque - low_torque) * (rpm - low_rpm) / (high_rpm - low_rpm);
            }
        }
        last_torque
    }

    /// Net torque at `rpm` and `throttle` in 0..=1: the curve scaled by the throttle, less the
    /// closed-throttle braking for the part of the throttle that is shut. Above the redline the
    /// fuel is cut and only the braking acts.
    #[must_use]
    pub fn net_torque(&self, rpm: f32, throttle: f32) -> f32 {
        let braking = self.engine_braking * (rpm / self.redline_rpm).clamp(0.0, 1.0);
        if rpm >= self.redline_rpm {
            return -braking;
        }
        throttle * self.torque_at(rpm) - (1.0 - throttle) * braking
    }

    fn invalid_field(&self) -> Option<&'static str> {
        let curve_ok = self.torque_curve.len() >= 2
            && self
                .torque_curve
                .iter()
                .all(|(rpm, torque)| non_negative(*rpm) && non_negative(*torque))
            && self.torque_curve.windows(2).all(|pair| pair[0].0 < pair[1].0)
            && self.torque_curve.iter().any(|(_, torque)| *torque > 0.0);
        let checks = [
            (curve_ok, "drivetrain.engine.torque_curve"),
            (positive(self.idle_rpm), "drivetrain.engine.idle_rpm"),
            (
                positive(self.redline_rpm) && self.redline_rpm > self.idle_rpm,
                "drivetrain.engine.redline_rpm",
            ),
            (positive(self.inertia), "drivetrain.engine.inertia"),
            (
                non_negative(self.engine_braking),
                "drivetrain.engine.engine_braking",
            ),
        ];
        first_bad(checks)
    }
}

/// The gearbox: ratios, efficiency and the automatic's thresholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GearboxConfig {
    /// Forward ratios from first gear (engine turns per output turn, each > 0, each below the one
    /// before), 1 to [`MAX_FORWARD_GEARS`]. An upshift must land above `downshift_rpm`, or the
    /// automatic could never take the next gear.
    pub forward_ratios: Vec<f32>,
    /// Reverse ratio as a positive number (> 0).
    pub reverse_ratio: f32,
    /// Final drive ratio (> 0), multiplied into every gear.
    pub final_drive: f32,
    /// Share of the torque that reaches the differential, in (0, 1].
    pub efficiency: f32,
    /// Start in automatic: shift on `upshift_rpm` and `downshift_rpm`. A manual shift turns it off
    /// until [`DrivetrainState::set_automatic`] turns it on again.
    pub automatic: bool,
    /// The automatic shifts up at this engine speed (rpm, ≤ the redline).
    pub upshift_rpm: f32,
    /// The automatic shifts down at this engine speed (rpm, above the engine's idle, which is as
    /// slow as it runs, and below `upshift_rpm`).
    pub downshift_rpm: f32,
    /// How long a shift holds the clutch open with the throttle shut (seconds, ≥ 0).
    pub shift_time: f32,
}

impl Default for GearboxConfig {
    /// A five-speed automatic with a 3.7 final drive.
    fn default() -> Self {
        Self {
            forward_ratios: vec![3.4, 2.1, 1.45, 1.1, 0.88],
            reverse_ratio: 3.2,
            final_drive: 3.7,
            efficiency: 0.92,
            automatic: true,
            upshift_rpm: 6200.0,
            downshift_rpm: 2600.0,
            shift_time: 0.25,
        }
    }
}

impl GearboxConfig {
    /// The ratio from the engine to the differential in `gear`, with the final drive: negative in
    /// reverse (`gear` −1), 0 in neutral (`gear` 0) or for a gear that does not exist.
    #[must_use]
    pub fn total_ratio(&self, gear: i32) -> f32 {
        let box_ratio = match gear {
            -1 => -self.reverse_ratio,
            g if g >= 1 => self.forward_ratios.get((g - 1) as usize).copied().unwrap_or(0.0),
            _ => 0.0,
        };
        box_ratio * self.final_drive
    }

    fn invalid_field(&self) -> Option<&'static str> {
        let ratios_ok = !self.forward_ratios.is_empty()
            && self.forward_ratios.len() <= MAX_FORWARD_GEARS
            && self.forward_ratios.iter().all(|ratio| positive(*ratio))
            && self.forward_ratios.windows(2).all(|pair| pair[1] < pair[0]);
        let reachable = self
            .forward_ratios
            .windows(2)
            .all(|pair| self.upshift_rpm * pair[1] * DOWNSHIFT_MARGIN > self.downshift_rpm * pair[0]);
        let checks = [
            (ratios_ok, "drivetrain.gearbox.forward_ratios"),
            (positive(self.reverse_ratio), "drivetrain.gearbox.reverse_ratio"),
            (positive(self.final_drive), "drivetrain.gearbox.final_drive"),
            (
                positive(self.efficiency) && self.efficiency <= 1.0,
                "drivetrain.gearbox.efficiency",
            ),
            (positive(self.upshift_rpm), "drivetrain.gearbox.upshift_rpm"),
            (
                positive(self.downshift_rpm) && self.downshift_rpm < self.upshift_rpm,
                "drivetrain.gearbox.downshift_rpm",
            ),
            (non_negative(self.shift_time), "drivetrain.gearbox.shift_time"),
            (reachable, "drivetrain.gearbox.forward_ratios"),
        ];
        first_bad(checks)
    }
}

/// A slip clutch that closes with speed, so a stopped car idles and a launch slips.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClutchConfig {
    /// The most torque the closed clutch carries (newton-meters, > 0).
    pub max_torque: f32,
    /// The clutch is fully closed when the faster of its two sides reaches this speed, and open at
    /// the engine's idle speed (rpm, > `idle_rpm`).
    pub engage_rpm: f32,
}

impl Default for ClutchConfig {
    fn default() -> Self {
        Self {
            max_torque: 600.0,
            engage_rpm: 2000.0,
        }
    }
}

/// The tire's lengthwise curve and rolling resistance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TireConfig {
    /// Friction coefficient at the peak of the curve (> 0). Also the wheel's sideways limit.
    pub peak_friction: f32,
    /// Friction coefficient of a locked or fully spinning wheel, in (0, `peak_friction`].
    pub slide_friction: f32,
    /// The slip ratio of the peak, in (0, 1).
    pub peak_slip: f32,
    /// Rolling resistance as a share of the wheel's load (≥ 0).
    pub rolling_coefficient: f32,
}

impl Default for TireConfig {
    /// A road tire: µ 1.0 at 10 % slip, 0.7 locked, 1.5 % rolling resistance.
    fn default() -> Self {
        Self {
            peak_friction: 1.0,
            slide_friction: 0.7,
            peak_slip: 0.1,
            rolling_coefficient: 0.015,
        }
    }
}

impl TireConfig {
    /// The friction coefficient at slip ratio `slip`, signed like the slip: rises linearly to the
    /// peak, falls linearly to the slide value at a slip of 1, flat beyond.
    #[must_use]
    pub fn friction_at(&self, slip: f32) -> f32 {
        let size = slip.abs();
        let friction = if size <= self.peak_slip {
            self.peak_friction * size / self.peak_slip
        } else if size >= 1.0 {
            self.slide_friction
        } else {
            let along = (size - self.peak_slip) / (1.0 - self.peak_slip);
            self.peak_friction + (self.slide_friction - self.peak_friction) * along
        };
        friction.copysign(slip)
    }

    fn invalid_field(&self) -> Option<&'static str> {
        let checks = [
            (positive(self.peak_friction), "drivetrain.tire.peak_friction"),
            (
                positive(self.slide_friction) && self.slide_friction <= self.peak_friction,
                "drivetrain.tire.slide_friction",
            ),
            (
                positive(self.peak_slip) && self.peak_slip < 1.0,
                "drivetrain.tire.peak_slip",
            ),
            (
                non_negative(self.rolling_coefficient),
                "drivetrain.tire.rolling_coefficient",
            ),
        ];
        first_bad(checks)
    }
}

/// How a differential shares torque between its two sides.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum Differential {
    /// The torque split is fixed: the sides turn freely against each other.
    #[default]
    Open,
    /// The sides turn at one speed; the torque goes wherever it takes to keep them equal.
    Locked,
    /// Open until the slower side carries `torque_bias` times the faster side's torque (relative
    /// to the nominal split), locked below that.
    LimitedSlip {
        /// The torque-bias ratio (≥ 1; 1 is an open differential).
        torque_bias: f32,
    },
}

/// Which axles the engine drives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum DriveLayout {
    /// Rear axle only.
    #[default]
    Rwd,
    /// Front axle only.
    Fwd,
    /// Both axles, through the centre differential.
    Awd,
}

/// A geared drivetrain: see the module docs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DrivetrainConfig {
    /// The engine.
    pub engine: EngineConfig,
    /// The gearbox.
    pub gearbox: GearboxConfig,
    /// The clutch.
    pub clutch: ClutchConfig,
    /// The tires (one model for every wheel).
    pub tire: TireConfig,
    /// Which axles are driven.
    pub layout: DriveLayout,
    /// The front axle's `(left, right)` wheel indices.
    pub front_axle: (usize, usize),
    /// The rear axle's `(left, right)` wheel indices.
    pub rear_axle: (usize, usize),
    /// The front axle's differential.
    pub front_differential: Differential,
    /// The rear axle's differential.
    pub rear_differential: Differential,
    /// The centre differential (AWD only).
    pub centre_differential: Differential,
    /// The front axle's share of the torque through the centre differential, in [0, 1] (AWD only).
    pub front_torque_share: f32,
    /// One wheel's rotating inertia with its brake disc and half shaft (kg·m², > 0).
    pub wheel_inertia: f32,
}

impl Default for DrivetrainConfig {
    /// A rear-wheel drive car on the default car's wheel order, with open differentials.
    fn default() -> Self {
        Self {
            engine: EngineConfig::default(),
            gearbox: GearboxConfig::default(),
            clutch: ClutchConfig::default(),
            tire: TireConfig::default(),
            layout: DriveLayout::Rwd,
            front_axle: (0, 1),
            rear_axle: (2, 3),
            front_differential: Differential::Open,
            rear_differential: Differential::Open,
            centre_differential: Differential::Open,
            front_torque_share: 0.4,
            wheel_inertia: 1.2,
        }
    }
}

impl DrivetrainConfig {
    /// The axles the engine drives.
    #[must_use]
    pub fn driven_axles(&self) -> [Option<(usize, usize)>; 2] {
        match self.layout {
            DriveLayout::Rwd => [None, Some(self.rear_axle)],
            DriveLayout::Fwd => [Some(self.front_axle), None],
            DriveLayout::Awd => [Some(self.front_axle), Some(self.rear_axle)],
        }
    }

    /// How many wheels the engine drives.
    #[must_use]
    pub fn driven_wheel_count(&self) -> usize {
        self.driven_axles().into_iter().flatten().count() * 2
    }

    /// The first field that is out of range for a vehicle of `wheel_count` wheels, if any. The
    /// answer names the field as `drivetrain.<section>.<field>`.
    #[must_use]
    pub fn invalid_field(&self, wheel_count: usize) -> Option<&'static str> {
        if let Some(field) = self
            .engine
            .invalid_field()
            .or_else(|| self.gearbox.invalid_field())
            .or_else(|| self.tire.invalid_field())
        {
            return Some(field);
        }
        let axle_ok =
            |(left, right): (usize, usize)| left < wheel_count && right < wheel_count && left != right;
        let distinct = match self.driven_axles() {
            [Some(front), Some(rear)] => {
                front.0 != rear.0 && front.0 != rear.1 && front.1 != rear.0 && front.1 != rear.1
            }
            _ => true,
        };
        let checks = [
            (
                self.gearbox.upshift_rpm <= self.engine.redline_rpm,
                "drivetrain.gearbox.upshift_rpm",
            ),
            (
                self.gearbox.downshift_rpm > self.engine.idle_rpm,
                "drivetrain.gearbox.downshift_rpm",
            ),
            (positive(self.clutch.max_torque), "drivetrain.clutch.max_torque"),
            (
                positive(self.clutch.engage_rpm) && self.clutch.engage_rpm > self.engine.idle_rpm,
                "drivetrain.clutch.engage_rpm",
            ),
            (
                self.layout == DriveLayout::Rwd || axle_ok(self.front_axle),
                "drivetrain.front_axle",
            ),
            (
                self.layout == DriveLayout::Fwd || axle_ok(self.rear_axle),
                "drivetrain.rear_axle",
            ),
            (distinct, "drivetrain.layout"),
            (bias_ok(self.front_differential), "drivetrain.front_differential"),
            (bias_ok(self.rear_differential), "drivetrain.rear_differential"),
            (
                bias_ok(self.centre_differential),
                "drivetrain.centre_differential",
            ),
            (
                non_negative(self.front_torque_share) && self.front_torque_share <= 1.0,
                "drivetrain.front_torque_share",
            ),
            (positive(self.wheel_inertia), "drivetrain.wheel_inertia"),
        ];
        first_bad(checks)
    }
}

/// Anti-lock brakes: each wheel's brake is released while it locks and reapplied when it recovers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AbsConfig {
    /// The size of the braking slip ratio above which the brake is released, in (0, 1).
    pub slip_threshold: f32,
    /// How fast the brake is released (share of full brake per second, > 0).
    pub release_rate: f32,
    /// How fast the brake comes back (share of full brake per second, > 0).
    pub apply_rate: f32,
    /// Below this ground speed the wheels may lock (m/s, ≥ 0), so the car comes to rest.
    pub min_speed: f32,
}

impl Default for AbsConfig {
    fn default() -> Self {
        Self {
            slip_threshold: 0.14,
            release_rate: 24.0,
            apply_rate: 12.0,
            min_speed: 1.0,
        }
    }
}

/// Traction control: the throttle is cut while a driven wheel spins.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TractionControlConfig {
    /// The driving slip ratio above which the throttle is cut (> 0).
    pub slip_threshold: f32,
    /// How fast the throttle is cut (share of full throttle per second, > 0).
    pub cut_rate: f32,
    /// How fast the throttle comes back (share of full throttle per second, > 0).
    pub restore_rate: f32,
}

impl Default for TractionControlConfig {
    fn default() -> Self {
        Self {
            slip_threshold: 0.15,
            cut_rate: 3.0,
            restore_rate: 3.0,
        }
    }
}

/// The optional driver aids. Both need a [`Drivetrain::Geared`] drivetrain: only it has wheel spin.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AidsConfig {
    /// Anti-lock brakes.
    pub abs: Option<AbsConfig>,
    /// Traction control.
    pub traction_control: Option<TractionControlConfig>,
}

impl AidsConfig {
    /// `true` when an aid is switched on.
    #[must_use]
    pub fn any(&self) -> bool {
        self.abs.is_some() || self.traction_control.is_some()
    }

    /// The first field that is out of range, if any.
    #[must_use]
    pub fn invalid_field(&self) -> Option<&'static str> {
        if let Some(abs) = &self.abs {
            let checks = [
                (
                    positive(abs.slip_threshold) && abs.slip_threshold < 1.0,
                    "aids.abs.slip_threshold",
                ),
                (positive(abs.release_rate), "aids.abs.release_rate"),
                (positive(abs.apply_rate), "aids.abs.apply_rate"),
                (non_negative(abs.min_speed), "aids.abs.min_speed"),
            ];
            if let Some(field) = first_bad(checks) {
                return Some(field);
            }
        }
        if let Some(tc) = &self.traction_control {
            let checks = [
                (
                    positive(tc.slip_threshold),
                    "aids.traction_control.slip_threshold",
                ),
                (positive(tc.cut_rate), "aids.traction_control.cut_rate"),
                (positive(tc.restore_rate), "aids.traction_control.restore_rate"),
            ];
            if let Some(field) = first_bad(checks) {
                return Some(field);
            }
        }
        None
    }
}

/// `value` is 0 or a usable positive setting (never NaN).
fn non_negative(value: f32) -> bool {
    value == 0.0 || positive(value)
}

/// `value` is a usable positive setting (never NaN).
fn positive(value: f32) -> bool {
    (MIN_POSITIVE_SETTING..=MAX_SETTING).contains(&value)
}

/// `value` is a brake torque the solve can multiply safely (never NaN).
pub(crate) fn brake_torque_in_range(value: f32) -> bool {
    non_negative(value)
}

/// The field of the first check that failed.
fn first_bad<const N: usize>(checks: [(bool, &'static str); N]) -> Option<&'static str> {
    checks.into_iter().find(|(ok, _)| !ok).map(|(_, field)| field)
}

fn bias_ok(differential: Differential) -> bool {
    match differential {
        Differential::LimitedSlip { torque_bias } => (1.0..=MAX_SETTING).contains(&torque_bias),
        Differential::Open | Differential::Locked => true,
    }
}

/// An anti-roll bar between two wheels, usually an axle's left and right (Milliken and Milliken):
/// it pushes the more compressed wheel's contact up and the other's down with
/// `stiffness · (compression difference)`, so it adds roll stiffness and no bounce stiffness.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AntiRollBar {
    /// The left wheel's index.
    pub left: usize,
    /// The right wheel's index.
    pub right: usize,
    /// Newtons per meter of compression difference (≥ 0).
    pub stiffness: f32,
}

impl AntiRollBar {
    /// The force on the left contact (up is positive) for these two compressions; the right
    /// contact takes the opposite.
    #[must_use]
    pub fn force(&self, left_compression: f32, right_compression: f32) -> f32 {
        self.stiffness * (left_compression - right_compression)
    }

    /// The wheels exist and differ, and the stiffness is finite and ≥ 0.
    #[must_use]
    pub fn is_valid(&self, wheel_count: usize) -> bool {
        self.left < wheel_count
            && self.right < wheel_count
            && self.left != self.right
            && non_negative(self.stiffness)
    }
}

/// Aerodynamic drag and downforce (Gillespie).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AeroConfig {
    /// Air density (kg/m³, ≥ 0).
    pub air_density: f32,
    /// Drag coefficient `Cd` (≥ 0).
    pub drag_coefficient: f32,
    /// Frontal area `A` (m², ≥ 0).
    pub frontal_area: f32,
    /// Lift coefficient times area of the front axle (m², ≥ 0): the downforce there is
    /// `0.5 · density · this · forward speed²`.
    pub front_downforce: f32,
    /// The same for the rear axle.
    pub rear_downforce: f32,
}

impl Default for AeroConfig {
    /// A road car at sea level: `Cd` 0.32, 2.1 m², no downforce.
    fn default() -> Self {
        Self {
            air_density: 1.225,
            drag_coefficient: 0.32,
            frontal_area: 2.1,
            front_downforce: 0.0,
            rear_downforce: 0.0,
        }
    }
}

impl AeroConfig {
    /// The drag force on a chassis moving at `velocity`: `−0.5 · ρ · Cd · A · |v| · v`.
    #[must_use]
    pub fn drag_force(&self, velocity: regia_core::math::Vec3) -> regia_core::math::Vec3 {
        velocity * (-0.5 * self.air_density * self.drag_coefficient * self.frontal_area * velocity.length())
    }

    /// The downforce of one axle with this `coefficient` at this forward speed (newtons).
    #[must_use]
    pub fn downforce(&self, coefficient: f32, forward_speed: f32) -> f32 {
        0.5 * self.air_density * coefficient * forward_speed * forward_speed
    }

    /// The first field that is out of range, if any.
    #[must_use]
    pub fn invalid_field(&self) -> Option<&'static str> {
        first_bad([
            (non_negative(self.air_density), "aero.air_density"),
            (non_negative(self.drag_coefficient), "aero.drag_coefficient"),
            (non_negative(self.frontal_area), "aero.frontal_area"),
            (non_negative(self.front_downforce), "aero.front_downforce"),
            (non_negative(self.rear_downforce), "aero.rear_downforce"),
        ])
    }
}

/// A manual gear change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShiftRequest {
    /// One gear up: reverse to neutral to first and on.
    Up,
    /// One gear down: first to neutral to reverse.
    Down,
    /// This gear: −1 reverse, 0 neutral, 1 and up the forward gears.
    Gear(i32),
}

/// One wheel's contact for a step, as the vehicle measured it.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelContact {
    /// The wheel's ray hit ground.
    pub grounded: bool,
    /// The load on the tire (newtons, ≥ 0).
    pub load: f32,
    /// The contact point's speed over the ground along the wheel's rolling direction (m/s).
    pub ground_speed: f32,
}

/// The driver and the chassis for a step.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DrivetrainInput {
    /// The sanitized throttle: +1 full forward, −1 full reverse.
    pub throttle: f32,
    /// The sanitized brake, 0 to 1.
    pub brake: f32,
    /// The chassis' speed along its forward axis (m/s).
    pub forward_speed: f32,
    /// The chassis' mass (kg, > 0).
    pub chassis_mass: f32,
}

/// One wheel's spin and what the tire did on the last step.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct WheelSpin {
    /// Angular speed (rad/s), positive rolling forward.
    pub angular_speed: f32,
    /// Accumulated rolling angle (radians).
    pub spin_angle: f32,
    /// Slip ratio at the end of the step: `(ω·r − v) / max(|v|, 1 m/s)`, positive driving,
    /// negative braking; 0 in the air.
    pub slip_ratio: f32,
    /// Mean tire force over the step along the rolling direction (newtons).
    pub tire_force: f32,
    /// The tire's load on the step (newtons).
    pub load: f32,
    /// Torque the drivetrain put into this wheel on the last substep (newton-meters).
    pub drive_torque: f32,
    /// Impulse the tire applied over the step along the rolling direction (newton-seconds).
    pub tire_impulse: f32,
    /// The ABS factor on this wheel's brake: 1 is the full brake, lower is released.
    pub abs_factor: f32,
    /// Ground speed the substeps track through the step (m/s).
    ground_speed: f32,
    /// Slope of the element `ω' = a·τ + w`.
    element_a: f32,
    /// Offset of the element.
    element_w: f32,
    /// Tire force at the start of the substep.
    force_start: f32,
    /// Tire force slope against slip speed (newtons per m/s, ≥ 0).
    force_slope: f32,
    /// Implicit denominator for the chassis' response to the tire.
    implicit: f32,
    /// Brake and rolling torque that oppose the spin on this substep.
    resist: f32,
}

impl WheelSpin {
    fn at_rest() -> Self {
        Self {
            angular_speed: 0.0,
            spin_angle: 0.0,
            slip_ratio: 0.0,
            tire_force: 0.0,
            load: 0.0,
            drive_torque: 0.0,
            tire_impulse: 0.0,
            abs_factor: 1.0,
            ground_speed: 0.0,
            element_a: 0.0,
            element_w: 0.0,
            force_start: 0.0,
            force_slope: 0.0,
            implicit: 1.0,
            resist: 0.0,
        }
    }
}

/// The fixed wheel data the drivetrain needs.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
struct WheelSetup {
    radius: f32,
    /// Full-brake torque (newton-meters).
    brake_torque: f32,
}

/// An element of the driveline: `ω' = a·τ + w` in its input torque `τ`.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Branch {
    a: f32,
    w: f32,
}

/// How a differential turns its input torque into its two side torques.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Split {
    /// The nominal split plus this transfer to side 1.
    Fixed(f32),
    /// The transfer that keeps both sides at one speed, whatever its size.
    Equalize,
    /// The transfer that keeps both sides at one speed, within this limit.
    EqualizeWithin(f32),
}

/// The transfer to side 1 that gives both sides one speed with input torque `tau`.
fn equalizing_transfer(share: f32, one: Branch, two: Branch, tau: f32) -> f32 {
    (two.w - one.w + (two.a * (1.0 - share) - one.a * share) * tau) / (one.a + two.a)
}

/// Combines two branches through a differential with `share` of the nominal torque on side 1.
/// `tau_hint` (the torque on the previous substep) sets a limited-slip's limit.
fn combine(kind: Differential, share: f32, one: Branch, two: Branch, tau_hint: f32) -> (Branch, Split) {
    let (s1, s2) = (share, 1.0 - share);
    let open = |transfer: f32| Branch {
        a: s1 * s1 * one.a + s2 * s2 * two.a,
        w: s1 * one.w + s2 * two.w + transfer * (s1 * one.a - s2 * two.a),
    };
    let locked = Branch {
        a: one.a * two.a / (one.a + two.a),
        w: (one.w * two.a + two.w * one.a) / (one.a + two.a),
    };
    match kind {
        Differential::Open => (open(0.0), Split::Fixed(0.0)),
        Differential::Locked => (locked, Split::Equalize),
        Differential::LimitedSlip { torque_bias } => {
            let needed = equalizing_transfer(share, one, two, tau_hint);
            // The transfer makes one side heavy against its nominal share: under drive the side it
            // adds to, on overrun the side it takes from.
            // (s_heavy |τ| + |T|) / (s_light |τ| − |T|) = bias · s_heavy / s_light.
            let (heavy, light) = if (needed >= 0.0) == (tau_hint >= 0.0) {
                (s1, s2)
            } else {
                (s2, s1)
            };
            let limit = heavy * light * tau_hint.abs() * (torque_bias - 1.0) / (light + torque_bias * heavy);
            if needed.abs() <= limit {
                (locked, Split::EqualizeWithin(limit))
            } else {
                let transfer = limit.copysign(needed);
                (open(transfer), Split::Fixed(transfer))
            }
        }
    }
}

/// The two side torques of a differential for input torque `tau`.
fn side_torques(split: Split, share: f32, one: Branch, two: Branch, tau: f32) -> (f32, f32) {
    let transfer = match split {
        Split::Fixed(transfer) => transfer,
        Split::Equalize => equalizing_transfer(share, one, two, tau),
        Split::EqualizeWithin(limit) => equalizing_transfer(share, one, two, tau).clamp(-limit, limit),
    };
    (share * tau + transfer, (1.0 - share) * tau - transfer)
}

/// The driveline's combined element and the plan to split its torque.
#[derive(Debug, Clone, Copy)]
struct Plan {
    carrier: Branch,
    front: Option<(Branch, Split, Branch, Branch)>,
    rear: Option<(Branch, Split, Branch, Branch)>,
    centre: Option<Split>,
}

/// What `DrivetrainState::carried_into` made: the state and what it clamped (ADR-9760).
pub(crate) struct CarriedDrivetrain {
    /// The state on the new configuration.
    pub state: DrivetrainState,
    /// A gear was above the new top gear.
    pub gear_clamped: bool,
    /// The engine speed was outside the new idle to redline range.
    pub rpm_clamped: bool,
}

/// The live state of a [`DrivetrainConfig`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DrivetrainState {
    wheels: Vec<WheelSpin>,
    setup: Vec<WheelSetup>,
    /// Engine speed (rad/s).
    engine_speed: f32,
    /// The engaged gear (the old one until a shift ends).
    gear: i32,
    /// The gear a shift is moving to; equals `gear` when none is under way.
    target_gear: i32,
    /// Seconds left in the shift.
    shift_timer: f32,
    /// Seconds until the automatic may decide again.
    dwell: f32,
    automatic: bool,
    upshifts: u32,
    downshifts: u32,
    /// Engine speed when the last automatic or manual shift began (rpm).
    last_shift_rpm: f32,
    /// The clutch carried this torque on the last substep (newton-meters).
    clutch_torque: f32,
    /// The clutch's two sides turned at different speeds on the last substep.
    clutch_slipping: bool,
    /// The engine's net torque on the last substep (newton-meters).
    engine_torque: f32,
    /// The throttle that reached the engine on the last substep, 0 to 1.
    applied_throttle: f32,
    /// The traction control's factor on the throttle: 1 is no cut.
    tc_factor: f32,
    /// Input torques on the previous substep: front axle, rear axle, centre.
    torque_hints: [f32; 3],
}

impl DrivetrainState {
    /// A drivetrain at idle in first gear, every wheel at rest. `max_brake_force` and each wheel's
    /// `brake` flag and radius set its full-brake torque.
    #[must_use]
    pub fn new(config: &DrivetrainConfig, wheels: &[WheelConfig], max_brake_force: f32) -> Self {
        let setup = wheels
            .iter()
            .map(|wheel| WheelSetup {
                radius: wheel.radius,
                brake_torque: if wheel.brake {
                    max_brake_force * wheel.radius
                } else {
                    0.0
                },
            })
            .collect();
        Self {
            wheels: vec![WheelSpin::at_rest(); wheels.len()],
            setup,
            engine_speed: config.engine.idle_rpm / RPM_PER_RAD_S,
            gear: 1,
            target_gear: 1,
            shift_timer: 0.0,
            dwell: 0.0,
            automatic: config.gearbox.automatic,
            upshifts: 0,
            downshifts: 0,
            last_shift_rpm: 0.0,
            clutch_torque: 0.0,
            clutch_slipping: false,
            engine_torque: 0.0,
            applied_throttle: 0.0,
            tc_factor: 1.0,
            torque_hints: [0.0; 3],
        }
    }

    /// This live state moved onto a retuned `config` (ADR-9760, `Vehicle::set_config`): a fresh
    /// state for the new wheels and brakes that keeps the engine speed clamped into the new idle to
    /// redline range, the engaged and target gear clamped to the new top forward gear (reverse and
    /// neutral stay), the shift timer clamped to the new shift time, the automatic flag, the shift
    /// counts, the clutch and traction state and, wheel by wheel, the spin. `wheels` must have as many
    /// entries as this state has wheels; extra or missing wheels start at rest.
    pub(crate) fn carried_into(
        &self,
        config: &DrivetrainConfig,
        wheels: &[WheelConfig],
        max_brake_force: f32,
    ) -> CarriedDrivetrain {
        let mut state = Self::new(config, wheels, max_brake_force);
        for (spin, old) in state.wheels.iter_mut().zip(&self.wheels) {
            *spin = *old;
        }
        let rpm = self.rpm();
        let clamped_rpm = rpm.clamp(config.engine.idle_rpm, config.engine.redline_rpm);
        let top = i32::try_from(config.gearbox.forward_ratios.len()).unwrap_or(i32::MAX);
        let clamp_gear = |gear: i32| if gear > top { top } else { gear };
        state.engine_speed = if clamped_rpm == rpm {
            self.engine_speed
        } else {
            clamped_rpm / RPM_PER_RAD_S
        };
        state.gear = clamp_gear(self.gear);
        state.target_gear = clamp_gear(self.target_gear);
        state.shift_timer = self.shift_timer.min(config.gearbox.shift_time);
        state.dwell = self.dwell;
        state.automatic = self.automatic;
        state.upshifts = self.upshifts;
        state.downshifts = self.downshifts;
        state.last_shift_rpm = self.last_shift_rpm;
        state.clutch_torque = self.clutch_torque;
        state.clutch_slipping = self.clutch_slipping;
        state.engine_torque = self.engine_torque;
        state.applied_throttle = self.applied_throttle;
        state.tc_factor = self.tc_factor;
        state.torque_hints = self.torque_hints;
        CarriedDrivetrain {
            gear_clamped: state.gear != self.gear || state.target_gear != self.target_gear,
            rpm_clamped: clamped_rpm != rpm,
            state,
        }
    }

    /// Engine speed (rpm).
    #[must_use]
    pub fn rpm(&self) -> f32 {
        self.engine_speed * RPM_PER_RAD_S
    }

    /// The engaged gear: −1 reverse, 0 neutral, 1 and up forward. A shift keeps the old gear until
    /// it ends; [`Self::target_gear`] names the new one.
    #[must_use]
    pub fn gear(&self) -> i32 {
        self.gear
    }

    /// The gear a shift is moving to; [`Self::gear`] when no shift is under way.
    #[must_use]
    pub fn target_gear(&self) -> i32 {
        self.target_gear
    }

    /// A shift is under way: the clutch is open and the throttle shut.
    #[must_use]
    pub fn is_shifting(&self) -> bool {
        self.shift_timer > 0.0
    }

    /// The automatic is deciding the gears.
    #[must_use]
    pub fn is_automatic(&self) -> bool {
        self.automatic
    }

    /// Hands the gears to the automatic or takes them from it.
    pub fn set_automatic(&mut self, automatic: bool) {
        self.automatic = automatic;
    }

    /// Shifts up since the start.
    #[must_use]
    pub fn upshifts(&self) -> u32 {
        self.upshifts
    }

    /// Shifts down since the start.
    #[must_use]
    pub fn downshifts(&self) -> u32 {
        self.downshifts
    }

    /// Engine speed when the last shift began (rpm), 0 before the first.
    #[must_use]
    pub fn last_shift_rpm(&self) -> f32 {
        self.last_shift_rpm
    }

    /// The torque through the clutch on the last substep (newton-meters).
    #[must_use]
    pub fn clutch_torque(&self) -> f32 {
        self.clutch_torque
    }

    /// The clutch's two sides turned at different speeds on the last substep.
    #[must_use]
    pub fn is_clutch_slipping(&self) -> bool {
        self.clutch_slipping
    }

    /// The engine's net torque on the last substep (newton-meters).
    #[must_use]
    pub fn engine_torque(&self) -> f32 {
        self.engine_torque
    }

    /// The throttle that reached the engine on the last substep, 0 to 1.
    #[must_use]
    pub fn applied_throttle(&self) -> f32 {
        self.applied_throttle
    }

    /// The traction control's factor on the throttle: 1 is no cut.
    #[must_use]
    pub fn traction_factor(&self) -> f32 {
        self.tc_factor
    }

    /// Every wheel, in configuration order.
    #[must_use]
    pub fn wheels(&self) -> &[WheelSpin] {
        &self.wheels
    }

    /// The sideways friction coefficient left to wheel `index`: the peak, less what the lengthwise
    /// force uses of the friction circle on the last step.
    #[must_use]
    pub fn lateral_friction(&self, config: &DrivetrainConfig, index: usize) -> f32 {
        let peak = config.tire.peak_friction;
        let Some(wheel) = self.wheels.get(index) else {
            return peak;
        };
        if wheel.load <= 0.0 {
            return peak;
        }
        let used = (wheel.tire_force.abs() / (peak * wheel.load)).min(1.0);
        peak * (1.0 - used * used)
            .max(MIN_LATERAL_SHARE * MIN_LATERAL_SHARE)
            .sqrt()
    }

    /// A shift a driver asked for: validates the gear, turns the automatic off, starts the shift.
    /// Returns the gear it is moving to.
    ///
    /// # Errors
    /// [`VehicleError::InvalidGear`] for a gear the gearbox does not have.
    pub fn request_shift(
        &mut self,
        config: &DrivetrainConfig,
        request: ShiftRequest,
    ) -> Result<i32, VehicleError> {
        let from = self.target_gear;
        let top = config.gearbox.forward_ratios.len() as i32;
        let gear = match request {
            ShiftRequest::Up => from + 1,
            ShiftRequest::Down => from - 1,
            ShiftRequest::Gear(gear) => gear,
        };
        if !(-1..=top).contains(&gear) {
            return Err(VehicleError::InvalidGear { gear });
        }
        self.automatic = false;
        if gear != from {
            self.begin_shift(config, gear);
        }
        Ok(gear)
    }

    /// Starts a shift to `gear`, counting it as up or down.
    fn begin_shift(&mut self, config: &DrivetrainConfig, gear: i32) {
        if gear > self.target_gear {
            self.upshifts += 1;
        } else {
            self.downshifts += 1;
        }
        self.last_shift_rpm = self.rpm();
        self.target_gear = gear;
        self.shift_timer = config.gearbox.shift_time;
        if self.shift_timer <= 0.0 {
            self.gear = gear;
            self.dwell = SHIFT_DWELL;
        }
    }

    /// The differential input's weighted mean of `value` over the driven wheels.
    fn driven_mean(&self, config: &DrivetrainConfig, value: impl Fn(&WheelSpin, &WheelSetup) -> f32) -> f32 {
        let axle = |(left, right): (usize, usize)| {
            let of = |index: usize| {
                self.wheels
                    .get(index)
                    .zip(self.setup.get(index))
                    .map_or(0.0, |(wheel, setup)| value(wheel, setup))
            };
            0.5 * (of(left) + of(right))
        };
        match config.layout {
            DriveLayout::Rwd => axle(config.rear_axle),
            DriveLayout::Fwd => axle(config.front_axle),
            DriveLayout::Awd => {
                let share = config.front_torque_share;
                share * axle(config.front_axle) + (1.0 - share) * axle(config.rear_axle)
            }
        }
    }

    /// The carrier's speed now: what the differential's input turns at (rad/s).
    fn carrier_speed(&self, config: &DrivetrainConfig) -> f32 {
        self.driven_mean(config, |wheel, _| wheel.angular_speed)
    }

    /// The automatic's decision: one gear up at the up threshold, one down at the down threshold.
    /// It never shifts up while the ground speed is too low for the next gear, so a spinning wheel
    /// holds the gear instead of running up through the box, and it never shifts down into a gear
    /// that would cross the up threshold at once.
    fn automatic_decision(&mut self, config: &DrivetrainConfig) {
        if !self.automatic || self.is_shifting() || self.dwell > 0.0 || self.gear < 1 {
            return;
        }
        let rpm = self.rpm();
        let top = config.gearbox.forward_ratios.len() as i32;
        let box_config = &config.gearbox;
        if rpm >= box_config.upshift_rpm && self.gear < top {
            let rolling = self
                .driven_mean(config, |wheel, setup| wheel.ground_speed / setup.radius)
                .abs();
            let after = rolling * box_config.total_ratio(self.gear + 1) * RPM_PER_RAD_S;
            if after >= box_config.downshift_rpm {
                self.begin_shift(config, self.gear + 1);
            }
        } else if rpm <= box_config.downshift_rpm && self.gear > 1 {
            let carrier = self.carrier_speed(config).abs();
            let after = carrier * box_config.total_ratio(self.gear - 1) * RPM_PER_RAD_S;
            if after < box_config.upshift_rpm * DOWNSHIFT_MARGIN {
                self.begin_shift(config, self.gear - 1);
            }
        }
    }

    /// Maps the throttle's sign onto the engine and the brakes. Returns `(engine throttle, brake)`.
    ///
    /// A throttle against the direction of travel brakes. Near a stop the automatic takes reverse
    /// or first instead; a manual gearbox keeps its gear and keeps braking. A manual gearbox in
    /// neutral only revs the engine.
    fn map_input(&mut self, config: &DrivetrainConfig, input: &DrivetrainInput) -> (f32, f32) {
        let throttle = input.throttle;
        let size = throttle.abs();
        let brake = input.brake;
        if throttle == 0.0 {
            return (0.0, brake);
        }
        let wanted = if throttle > 0.0 { 1 } else { -1 };
        let engaged = self.target_gear.signum();
        if engaged == wanted || (engaged == 0 && !self.automatic) {
            return (size, brake);
        }
        let travel = input.forward_speed * wanted as f32;
        if self.automatic && travel > -DIRECTION_CHANGE_SPEED {
            self.begin_shift(config, wanted);
            (size, brake)
        } else {
            (0.0, brake.max(size))
        }
    }

    /// Advances one fixed step. `aids` must be the vehicle's; `contacts` has one entry per wheel.
    ///
    /// Each [`WheelSpin`] then holds the step's tire impulse, which the caller applies to the
    /// chassis.
    pub fn step(
        &mut self,
        config: &DrivetrainConfig,
        aids: &AidsConfig,
        input: &DrivetrainInput,
        contacts: &[WheelContact],
        dt: f32,
    ) {
        let (throttle, brake) = self.map_input(config, input);
        let substep = dt / DRIVETRAIN_SUBSTEPS as f32;
        let grounded = contacts.iter().filter(|contact| contact.grounded).count();
        let tire_mass = input.chassis_mass / grounded.max(1) as f32;
        for (wheel, contact) in self.wheels.iter_mut().zip(contacts) {
            wheel.tire_impulse = 0.0;
            wheel.ground_speed = contact.ground_speed;
            wheel.load = if contact.grounded {
                contact.load.max(0.0)
            } else {
                0.0
            };
        }
        for _ in 0..DRIVETRAIN_SUBSTEPS {
            self.substep(config, aids, (throttle, brake), contacts, tire_mass, substep);
        }
        for (wheel, contact) in self.wheels.iter_mut().zip(contacts) {
            wheel.tire_force = wheel.tire_impulse / dt;
            if !contact.grounded {
                wheel.slip_ratio = 0.0;
            }
        }
    }

    fn substep(
        &mut self,
        config: &DrivetrainConfig,
        aids: &AidsConfig,
        (throttle, brake): (f32, f32),
        contacts: &[WheelContact],
        tire_mass: f32,
        h: f32,
    ) {
        // The shift clock and the automatic.
        if self.shift_timer > 0.0 {
            self.shift_timer -= h;
            if self.shift_timer <= 0.0 {
                self.shift_timer = 0.0;
                self.gear = self.target_gear;
                self.dwell = SHIFT_DWELL;
            }
        } else if self.dwell > 0.0 {
            self.dwell -= h;
        }
        self.automatic_decision(config);

        // Traction control acts on the last substep's slip.
        if let Some(tc) = &aids.traction_control {
            let direction = self.gear.signum() as f32;
            let spin = config
                .driven_axles()
                .into_iter()
                .flatten()
                .flat_map(|(left, right)| [left, right])
                .filter_map(|index| self.wheels.get(index).zip(contacts.get(index)))
                .filter(|(_, contact)| contact.grounded)
                .fold(0.0_f32, |most, (wheel, _)| most.max(wheel.slip_ratio * direction));
            self.tc_factor = if spin > tc.slip_threshold && throttle > 0.0 {
                (self.tc_factor - tc.cut_rate * h).max(0.0)
            } else {
                (self.tc_factor + tc.restore_rate * h).min(1.0)
            };
        }

        // ABS and the wheel elements.
        for (index, wheel) in self.wheels.iter_mut().enumerate() {
            let setup = self.setup[index];
            let contact = contacts.get(index).copied().unwrap_or_default();
            if let Some(abs) = &aids.abs {
                let locking = -wheel.slip_ratio * wheel.ground_speed.signum();
                wheel.abs_factor = if !contact.grounded || wheel.ground_speed.abs() < abs.min_speed {
                    1.0
                } else if locking > abs.slip_threshold {
                    (wheel.abs_factor - abs.release_rate * h).max(0.0)
                } else {
                    (wheel.abs_factor + abs.apply_rate * h).min(1.0)
                };
            }
            let rolling = config.tire.rolling_coefficient * wheel.load * setup.radius;
            wheel.resist = setup.brake_torque * brake * wheel.abs_factor + rolling;
            let (force, slope) = if contact.grounded {
                tire_force(
                    &config.tire,
                    wheel.angular_speed * setup.radius - wheel.ground_speed,
                    wheel.ground_speed,
                    wheel.load,
                )
            } else {
                (0.0, 0.0)
            };
            wheel.force_start = force;
            wheel.force_slope = slope;
            wheel.implicit = 1.0 + slope * h / tire_mass;
            let lhs = config.wheel_inertia / h + setup.radius * setup.radius * slope / wheel.implicit;
            wheel.element_a = 1.0 / lhs;
            let free =
                -setup.radius * force / wheel.implicit - zero_signum(wheel.angular_speed) * wheel.resist;
            wheel.element_w = wheel.angular_speed + wheel.element_a * free;
            wheel.drive_torque = 0.0;
        }

        // The driveline: the combined element, the clutch and the torque back down to the wheels.
        let plan = self.plan(config);
        let ratio = if self.is_shifting() {
            0.0
        } else {
            config.gearbox.total_ratio(self.gear)
        };
        let engine_rpm = self.rpm();
        let gear_rpm = (self.carrier_speed(config) * ratio).abs() * RPM_PER_RAD_S;
        let shifted_throttle = if self.is_shifting() {
            0.0
        } else {
            throttle * self.tc_factor
        };
        self.applied_throttle = shifted_throttle;
        let net = config.engine.net_torque(engine_rpm, shifted_throttle);
        let inertia = config.engine.inertia;
        let efficiency = config.gearbox.efficiency;
        let clutch = if ratio == 0.0 {
            0.0
        } else {
            let closed = (engine_rpm.max(gear_rpm) - config.engine.idle_rpm)
                / (config.clutch.engage_rpm - config.engine.idle_rpm);
            let capacity = config.clutch.max_torque * closed.clamp(0.0, 1.0);
            let lock = (self.engine_speed + h * net / inertia - ratio * plan.carrier.w)
                / (h / inertia + plan.carrier.a * efficiency * ratio * ratio);
            self.clutch_slipping = lock.abs() > capacity;
            lock.clamp(-capacity, capacity)
        };
        if ratio == 0.0 {
            self.clutch_slipping = false;
        }
        self.clutch_torque = clutch;
        self.engine_torque = net;
        self.engine_speed =
            (self.engine_speed + h * (net - clutch) / inertia).max(config.engine.idle_rpm / RPM_PER_RAD_S);
        self.distribute(config, &plan, efficiency * ratio * clutch);

        // Each wheel with its drive torque: spin, tire force and the tracked ground speed.
        for (index, wheel) in self.wheels.iter_mut().enumerate() {
            let setup = self.setup[index];
            let contact = contacts.get(index).copied().unwrap_or_default();
            let before = wheel.angular_speed;
            let tire_torque = -setup.radius * wheel.force_start / wheel.implicit;
            let net = wheel.drive_torque + tire_torque;
            let a = wheel.element_a;
            let resist = wheel.resist;
            let after = if resist <= 0.0 {
                before + a * net
            } else if before != 0.0 {
                let moved = before + a * (net - zero_signum(before) * resist);
                if moved * before >= 0.0 {
                    moved
                } else if net.abs() <= resist {
                    0.0
                } else {
                    a * (net - zero_signum(net) * resist)
                }
            } else if net.abs() <= resist {
                0.0
            } else {
                a * (net - zero_signum(net) * resist)
            };
            wheel.angular_speed = after;
            wheel.spin_angle += after * h;
            if contact.grounded {
                let raw = (wheel.force_start + wheel.force_slope * setup.radius * (after - before))
                    / wheel.implicit;
                let limit = config.tire.peak_friction * wheel.load;
                let force = raw.clamp(-limit, limit);
                wheel.tire_impulse += force * h;
                wheel.ground_speed += force * h / tire_mass;
                wheel.slip_ratio = (after * setup.radius - wheel.ground_speed)
                    / wheel.ground_speed.abs().max(SLIP_SPEED_FLOOR);
            }
        }
    }

    /// Builds the driveline's combined element for the driven axles.
    fn plan(&self, config: &DrivetrainConfig) -> Plan {
        let branch = |index: usize| {
            self.wheels
                .get(index)
                .map_or(Branch { a: 0.0, w: 0.0 }, |wheel| Branch {
                    a: wheel.element_a,
                    w: wheel.element_w,
                })
        };
        let axle = |pair: (usize, usize), kind: Differential, hint: f32| {
            let (left, right) = (branch(pair.0), branch(pair.1));
            let (combined, split) = combine(kind, 0.5, left, right, hint);
            (combined, split, left, right)
        };
        match config.layout {
            DriveLayout::Rwd => {
                let rear = axle(config.rear_axle, config.rear_differential, self.torque_hints[1]);
                Plan {
                    carrier: rear.0,
                    front: None,
                    rear: Some(rear),
                    centre: None,
                }
            }
            DriveLayout::Fwd => {
                let front = axle(config.front_axle, config.front_differential, self.torque_hints[0]);
                Plan {
                    carrier: front.0,
                    front: Some(front),
                    rear: None,
                    centre: None,
                }
            }
            DriveLayout::Awd => {
                let front = axle(config.front_axle, config.front_differential, self.torque_hints[0]);
                let rear = axle(config.rear_axle, config.rear_differential, self.torque_hints[1]);
                let (carrier, centre) = combine(
                    config.centre_differential,
                    config.front_torque_share,
                    front.0,
                    rear.0,
                    self.torque_hints[2],
                );
                Plan {
                    carrier,
                    front: Some(front),
                    rear: Some(rear),
                    centre: Some(centre),
                }
            }
        }
    }

    /// Sends `tau` (the differential input torque) down to the wheels' `drive_torque`.
    fn distribute(&mut self, config: &DrivetrainConfig, plan: &Plan, tau: f32) {
        let (front_tau, rear_tau) = match (plan.centre, plan.front, plan.rear) {
            (Some(centre), Some(front), Some(rear)) => {
                side_torques(centre, config.front_torque_share, front.0, rear.0, tau)
            }
            (_, Some(_), None) => (tau, 0.0),
            _ => (0.0, tau),
        };
        self.torque_hints = [front_tau, rear_tau, tau];
        let mut send = |pair: (usize, usize), axle: (Branch, Split, Branch, Branch), axle_tau: f32| {
            let (_, split, left, right) = axle;
            let (left_tau, right_tau) = side_torques(split, 0.5, left, right, axle_tau);
            if let Some(wheel) = self.wheels.get_mut(pair.0) {
                wheel.drive_torque = left_tau;
            }
            if let Some(wheel) = self.wheels.get_mut(pair.1) {
                wheel.drive_torque = right_tau;
            }
        };
        if let Some(front) = plan.front {
            send(config.front_axle, front, front_tau);
        }
        if let Some(rear) = plan.rear {
            send(config.rear_axle, rear, rear_tau);
        }
    }

    /// FNV-1a over every state value's bits, in a fixed order: two runs with the same inputs give
    /// the same digest. Stable across builds of the same state layout.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut hash = 0xcbf2_9ce4_8422_2325_u64;
        let mut write = |word: u32| {
            for byte in word.to_le_bytes() {
                hash ^= u64::from(byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        };
        for value in [
            self.engine_speed,
            self.shift_timer,
            self.dwell,
            self.clutch_torque,
            self.engine_torque,
            self.applied_throttle,
            self.tc_factor,
            self.last_shift_rpm,
            self.torque_hints[0],
            self.torque_hints[1],
            self.torque_hints[2],
        ] {
            write(value.to_bits());
        }
        write(self.gear as u32);
        write(self.target_gear as u32);
        write(u32::from(self.automatic));
        write(self.upshifts);
        write(self.downshifts);
        for wheel in &self.wheels {
            for value in [
                wheel.angular_speed,
                wheel.spin_angle,
                wheel.slip_ratio,
                wheel.tire_impulse,
                wheel.load,
                wheel.drive_torque,
                wheel.abs_factor,
            ] {
                write(value.to_bits());
            }
        }
        hash
    }
}

/// −1, 0 or +1 (`f32::signum` gives ±1 at zero).
fn zero_signum(value: f32) -> f32 {
    if value > 0.0 {
        1.0
    } else if value < 0.0 {
        -1.0
    } else {
        0.0
    }
}

/// The tire's force and its slope against the slip speed `slip_speed = ω·r − v`.
fn tire_force(tire: &TireConfig, slip_speed: f32, ground_speed: f32, load: f32) -> (f32, f32) {
    let denominator = ground_speed.abs().max(SLIP_SPEED_FLOOR);
    let slip = slip_speed / denominator;
    let force = tire.friction_at(slip) * load;
    let slope = if slip.abs() < tire.peak_slip {
        load * tire.peak_friction / (tire.peak_slip * denominator)
    } else {
        0.0
    };
    (force, slope)
}

// ---------------------------------------------------------------------------
// Snapshots (ADR-3080)
// ---------------------------------------------------------------------------

/// What one wheel's next step reads from the last one, and its rolling angle.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WheelCore {
    /// Angular speed (rad/s), positive rolling forward.
    pub angular_speed: f32,
    /// Accumulated rolling angle (radians).
    pub spin_angle: f32,
    /// Slip ratio at the end of the last step (traction control reads it).
    pub slip_ratio: f32,
    /// Mean tire force over the last step (newtons; the friction circle reads it).
    pub tire_force: f32,
    /// The tire's load on the last step (newtons; the friction circle reads it).
    pub load: f32,
    /// The ABS factor on this wheel's brake.
    pub abs_factor: f32,
}

/// What a drivetrain's next step reads from the last one: enough to go on from a corrected state.
/// [`DrivetrainState`] holds this and the last substep's readouts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct DrivetrainCore {
    /// Engine speed (rad/s).
    pub engine_speed: f32,
    /// The engaged gear.
    pub gear: i32,
    /// The gear a shift is moving to.
    pub target_gear: i32,
    /// Seconds left in the shift.
    pub shift_timer: f32,
    /// Seconds until the automatic may decide again.
    pub dwell: f32,
    /// The automatic decides the gears.
    pub automatic: bool,
    /// The torque through the clutch on the last substep (newton-meters).
    pub clutch_torque: f32,
    /// The traction control's factor on the throttle.
    pub tc_factor: f32,
    /// The differentials' input torques on the last substep: front, rear, centre.
    pub torque_hints: [f32; 3],
    /// One entry per wheel.
    pub wheels: Vec<WheelCore>,
}

impl DrivetrainState {
    /// What the next step reads from this state.
    #[must_use]
    pub fn core(&self) -> DrivetrainCore {
        DrivetrainCore {
            engine_speed: self.engine_speed,
            gear: self.gear,
            target_gear: self.target_gear,
            shift_timer: self.shift_timer,
            dwell: self.dwell,
            automatic: self.automatic,
            clutch_torque: self.clutch_torque,
            tc_factor: self.tc_factor,
            torque_hints: self.torque_hints,
            wheels: self
                .wheels
                .iter()
                .map(|wheel| WheelCore {
                    angular_speed: wheel.angular_speed,
                    spin_angle: wheel.spin_angle,
                    slip_ratio: wheel.slip_ratio,
                    tire_force: wheel.tire_force,
                    load: wheel.load,
                    abs_factor: wheel.abs_factor,
                })
                .collect(),
        }
    }

    /// Goes on from `core`. Wheels past the end of `core` keep their values.
    pub fn set_core(&mut self, core: &DrivetrainCore) {
        self.engine_speed = core.engine_speed;
        self.gear = core.gear;
        self.target_gear = core.target_gear;
        self.shift_timer = core.shift_timer;
        self.dwell = core.dwell;
        self.automatic = core.automatic;
        self.clutch_torque = core.clutch_torque;
        self.tc_factor = core.tc_factor;
        self.torque_hints = core.torque_hints;
        for (wheel, saved) in self.wheels.iter_mut().zip(&core.wheels) {
            wheel.angular_speed = saved.angular_speed;
            wheel.spin_angle = saved.spin_angle;
            wheel.slip_ratio = saved.slip_ratio;
            wheel.tire_force = saved.tire_force;
            wheel.load = saved.load;
            wheel.abs_factor = saved.abs_factor;
        }
    }

    /// Whether every float this state holds is finite and its wheel count is `wheels`.
    pub(crate) fn is_sound(&self, wheels: usize) -> bool {
        let scalars = [
            self.engine_speed,
            self.shift_timer,
            self.dwell,
            self.last_shift_rpm,
            self.clutch_torque,
            self.engine_torque,
            self.applied_throttle,
            self.tc_factor,
            self.torque_hints[0],
            self.torque_hints[1],
            self.torque_hints[2],
        ];
        self.wheels.len() == wheels
            && self.setup.len() == wheels
            && scalars.iter().all(|value| value.is_finite())
            && self
                .setup
                .iter()
                .all(|setup| setup.radius.is_finite() && setup.radius > 0.0 && setup.brake_torque.is_finite())
            && self.wheels.iter().all(|wheel| {
                [
                    wheel.angular_speed,
                    wheel.spin_angle,
                    wheel.slip_ratio,
                    wheel.tire_force,
                    wheel.load,
                    wheel.drive_torque,
                    wheel.tire_impulse,
                    wheel.abs_factor,
                    wheel.ground_speed,
                    wheel.element_a,
                    wheel.element_w,
                    wheel.force_start,
                    wheel.force_slope,
                    wheel.implicit,
                    wheel.resist,
                ]
                .iter()
                .all(|value| value.is_finite())
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_torque_curve_is_linear_between_points_and_flat_beyond() {
        let engine = EngineConfig {
            torque_curve: vec![(1000.0, 100.0), (3000.0, 300.0), (5000.0, 200.0)],
            ..EngineConfig::default()
        };
        assert_eq!(engine.torque_at(500.0), 100.0);
        assert_eq!(engine.torque_at(1000.0), 100.0);
        assert_eq!(engine.torque_at(2000.0), 200.0);
        assert_eq!(engine.torque_at(4000.0), 250.0);
        assert_eq!(engine.torque_at(9000.0), 200.0);
    }

    #[test]
    fn closed_throttle_brakes_and_the_redline_cuts_the_fuel() {
        let engine = EngineConfig::default();
        assert_eq!(engine.net_torque(7000.0, 1.0), -engine.engine_braking);
        assert_eq!(engine.net_torque(3500.0, 0.0), -0.5 * engine.engine_braking);
        assert!(engine.net_torque(3500.0, 1.0) > 0.0);
    }

    #[test]
    fn the_tire_curve_peaks_then_falls_to_the_slide_value() {
        let tire = TireConfig::default();
        assert_eq!(tire.friction_at(0.0), 0.0);
        assert_eq!(tire.friction_at(tire.peak_slip), tire.peak_friction);
        assert_eq!(tire.friction_at(-tire.peak_slip), -tire.peak_friction);
        assert_eq!(tire.friction_at(1.0), tire.slide_friction);
        assert_eq!(tire.friction_at(-4.0), -tire.slide_friction);
        let middle = tire.friction_at(0.55);
        assert!(
            middle < tire.peak_friction && middle > tire.slide_friction,
            "{middle}"
        );
    }

    #[test]
    fn the_total_ratio_carries_the_final_drive_and_the_sign() {
        let gearbox = GearboxConfig {
            forward_ratios: vec![3.0, 2.0],
            reverse_ratio: 2.5,
            final_drive: 4.0,
            ..GearboxConfig::default()
        };
        assert_eq!(gearbox.total_ratio(1), 12.0);
        assert_eq!(gearbox.total_ratio(2), 8.0);
        assert_eq!(gearbox.total_ratio(-1), -10.0);
        assert_eq!(gearbox.total_ratio(0), 0.0);
        assert_eq!(gearbox.total_ratio(3), 0.0);
    }

    #[test]
    fn a_limited_slip_holds_its_bias_both_ways_on_an_unequal_split() {
        // The sides' speeds are far apart in both directions and the torque runs both ways: drive
        // and overrun. The heavier side, relative to its nominal share, carries the bias each time.
        let (share, bias) = (0.35_f32, 2.5_f32);
        let kind = Differential::LimitedSlip { torque_bias: bias };
        let branch = |w: f32| Branch { a: 1.0e-3, w };
        for (one, two) in [(branch(0.0), branch(50.0)), (branch(50.0), branch(0.0))] {
            for tau in [100.0_f32, -100.0] {
                let (_, split) = combine(kind, share, one, two, tau);
                let (first, second) = side_torques(split, share, one, two, tau);
                assert!((first + second - tau).abs() < 1.0e-3, "torque is conserved");
                let (first, second) = (first.abs() / share, second.abs() / (1.0 - share));
                let ratio = first.max(second) / first.min(second);
                assert!(
                    (ratio - bias).abs() < 1.0e-3,
                    "tau {tau}, w {} and {}: ratio {ratio} vs {bias}",
                    one.w,
                    two.w
                );
            }
        }
    }

    #[test]
    fn bad_fields_are_named() {
        let field = |change: fn(&mut DrivetrainConfig)| {
            let mut config = DrivetrainConfig::default();
            change(&mut config);
            config.invalid_field(4)
        };
        assert_eq!(field(|_| ()), None);
        assert_eq!(
            field(|c| c.engine.torque_curve = vec![(100.0, 1.0)]),
            Some("drivetrain.engine.torque_curve")
        );
        assert_eq!(
            field(|c| c.engine.torque_curve = vec![(100.0, 1.0), (100.0, 1.0)]),
            Some("drivetrain.engine.torque_curve")
        );
        assert_eq!(
            field(|c| c.engine.redline_rpm = 100.0),
            Some("drivetrain.engine.redline_rpm")
        );
        assert_eq!(
            field(|c| c.gearbox.forward_ratios.clear()),
            Some("drivetrain.gearbox.forward_ratios")
        );
        assert_eq!(
            field(|c| c.gearbox.downshift_rpm = c.gearbox.upshift_rpm),
            Some("drivetrain.gearbox.downshift_rpm")
        );
        assert_eq!(
            field(|c| c.tire.slide_friction = 2.0),
            Some("drivetrain.tire.slide_friction")
        );
        assert_eq!(field(|c| c.rear_axle = (2, 2)), Some("drivetrain.rear_axle"));
        assert_eq!(field(|c| c.rear_axle = (2, 9)), Some("drivetrain.rear_axle"));
        assert_eq!(
            field(|c| c.rear_differential = Differential::LimitedSlip { torque_bias: 0.5 }),
            Some("drivetrain.rear_differential")
        );
        assert_eq!(field(|c| c.wheel_inertia = 0.0), Some("drivetrain.wheel_inertia"));
        // An AWD car cannot drive a wheel through both axles.
        let mut awd = DrivetrainConfig {
            layout: DriveLayout::Awd,
            front_axle: (0, 1),
            rear_axle: (1, 2),
            ..DrivetrainConfig::default()
        };
        assert_eq!(awd.invalid_field(4), Some("drivetrain.layout"));
        awd.rear_axle = (2, 3);
        assert_eq!(awd.invalid_field(4), None);
    }
}
