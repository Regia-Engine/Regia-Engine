// SPDX-License-Identifier: MPL-2.0
//! The Vehicle Tuning panel's plots (ADR-9760): pure functions over a `VehicleConfig`, built on the
//! runtime's own math so a plot shows what the car does. No GPU, no egui: the panel draws the points
//! these return, and the tests compare them with the runtime at sample points.
//!
//! - [`torque_power`]: full-throttle torque (`EngineConfig::torque_at`) and power against rpm.
//! - [`wheel_force_per_gear`]: the total driven-wheel force against road speed in each forward gear,
//!   with the clutch closed and no slip: `torque · efficiency · total_ratio(gear) / radius` at
//!   `speed = rpm / total_ratio(gear) · radius` (the drivetrain's own lockup relation).
//! - [`tire_side_force`]: a slip tire's side force against slip angle (`SlipTire::lateral_force`,
//!   the same numbers `physics.vehicle_tire_curve` reports).
//! - [`suspension_travel`]: a wheel's spring force against compression and where it settles (the
//!   rapier spring scaled by the chassis mass that `vehicle.rs` documents).

use regia::physics::vehicle::VehicleConfig;
use regia::physics::vehicle_drivetrain::{Drivetrain, EngineConfig};
use regia::physics::vehicle_tire::SlipTire;

/// Engine rpm per rad/s.
pub const RPM_PER_RAD_S: f32 = 60.0 / std::f32::consts::TAU;

/// Gravity the suspension plot settles under, m/s² (the proving ground's).
pub const GRAVITY: f32 = regia::physics::vehicle_proving::PROVING_GRAVITY;

/// The speed range of a `Simple` drivetrain's flat force line, km/h.
const SIMPLE_TOP_KMH: f32 = 200.0;

/// One point of the torque and power plot.
#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct TorquePoint {
    /// Engine speed, rpm.
    pub rpm: f32,
    /// Full-throttle torque, newton-meters.
    pub torque_nm: f32,
    /// Full-throttle power, kilowatts.
    pub power_kw: f32,
}

/// `points` samples (at least 2) of the engine from idle to redline.
#[must_use]
pub fn torque_power(engine: &EngineConfig, points: usize) -> Vec<TorquePoint> {
    let points = points.max(2);
    (0..points)
        .map(|index| {
            let share = index as f32 / (points - 1) as f32;
            let rpm = engine.idle_rpm + (engine.redline_rpm - engine.idle_rpm) * share;
            let torque_nm = engine.torque_at(rpm);
            TorquePoint {
                rpm,
                torque_nm,
                power_kw: torque_nm * rpm / RPM_PER_RAD_S / 1000.0,
            }
        })
        .collect()
}

/// One forward gear's force line.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct GearCurve {
    /// The gear, 1 and up (1 for the `Simple` drivetrain's single line).
    pub gear: i32,
    /// The engine-to-wheel ratio, final drive included (0 for `Simple`).
    pub total_ratio: f32,
    /// `(road speed km/h, total driven-wheel force N)`, rising speed.
    pub points: Vec<(f32, f32)>,
}

/// The mean radius of the driven wheels: the drivetrain's driven axles, or the wheels with
/// `drive` for the `Simple` drivetrain. `None` when no wheel is driven.
#[must_use]
pub fn driven_radius(config: &VehicleConfig) -> Option<f32> {
    let radii: Vec<f32> = match &config.drivetrain {
        Drivetrain::Geared(drivetrain) => drivetrain
            .driven_axles()
            .into_iter()
            .flatten()
            .flat_map(|(left, right)| [left, right])
            .filter_map(|index| config.wheels.get(index).map(|wheel| wheel.radius))
            .collect(),
        Drivetrain::Simple => config
            .wheels
            .iter()
            .filter(|wheel| wheel.drive)
            .map(|wheel| wheel.radius)
            .collect(),
    };
    (!radii.is_empty()).then(|| radii.iter().sum::<f32>() / radii.len() as f32)
}

/// Wheel force against speed in every forward gear, `points` samples (at least 2) per gear from idle
/// to redline. A `Simple` drivetrain has one flat line: `max_engine_force` per driven wheel.
#[must_use]
pub fn wheel_force_per_gear(config: &VehicleConfig, points: usize) -> Vec<GearCurve> {
    let points = points.max(2);
    let Some(radius) = driven_radius(config) else {
        return Vec::new();
    };
    match &config.drivetrain {
        Drivetrain::Simple => {
            let driven = config.wheels.iter().filter(|wheel| wheel.drive).count() as f32;
            let force = config.max_engine_force * driven;
            vec![GearCurve {
                gear: 1,
                total_ratio: 0.0,
                points: (0..points)
                    .map(|index| (SIMPLE_TOP_KMH * index as f32 / (points - 1) as f32, force))
                    .collect(),
            }]
        }
        Drivetrain::Geared(drivetrain) => {
            let engine = &drivetrain.engine;
            let gearbox = &drivetrain.gearbox;
            (1..=drivetrain.gearbox.forward_ratios.len())
                .map(|gear| {
                    let gear = i32::try_from(gear).unwrap_or(i32::MAX);
                    let ratio = gearbox.total_ratio(gear);
                    let points = torque_power(engine, points)
                        .into_iter()
                        .map(|point| {
                            let wheel_speed = point.rpm / RPM_PER_RAD_S / ratio;
                            let force = point.torque_nm * gearbox.efficiency * ratio / radius;
                            (wheel_speed * radius * 3.6, force)
                        })
                        .collect();
                    GearCurve {
                        gear,
                        total_ratio: ratio,
                        points,
                    }
                })
                .collect()
        }
    }
}

/// A slip tire's side force against slip angle at `load` newtons: `points` samples (at least 2)
/// from 0 to `max_angle` radians, as `(slip angle, force N)`.
#[must_use]
pub fn tire_side_force(tire: &SlipTire, load: f32, max_angle: f32, points: usize) -> Vec<(f32, f32)> {
    let points = points.max(2);
    (0..points)
        .map(|index| {
            let angle = max_angle * index as f32 / (points - 1) as f32;
            (angle, tire.lateral_force(angle, load))
        })
        .collect()
}

/// A wheel's suspension: spring force against compression, and where it settles.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct SuspensionPlot {
    /// The wheel.
    pub wheel: usize,
    /// Rest length, meters.
    pub rest_length: f32,
    /// How far it may compress, meters.
    pub travel: f32,
    /// Where it settles with the chassis' weight shared evenly by every wheel, meters (clamped to
    /// the travel).
    pub static_compression: f32,
    /// `(compression m, spring force N)` from 0 to the travel, capped at `max_suspension_force`.
    pub points: Vec<(f32, f32)>,
}

/// Wheel `index`'s suspension plot, `points` samples (at least 2); `None` for a wheel that does not
/// exist. The spring is rapier's: `stiffness · compression · chassis_mass` newtons, so `n` equal
/// wheels settle at `g / (n · stiffness)`.
#[must_use]
pub fn suspension_travel(config: &VehicleConfig, index: usize, points: usize) -> Option<SuspensionPlot> {
    let wheel = config.wheels.get(index)?;
    let points = points.max(2);
    let count = config.wheels.len() as f32;
    let static_compression = if wheel.suspension_stiffness > 0.0 {
        (GRAVITY / (count * wheel.suspension_stiffness)).min(wheel.suspension_travel)
    } else {
        wheel.suspension_travel
    };
    Some(SuspensionPlot {
        wheel: index,
        rest_length: wheel.suspension_rest_length,
        travel: wheel.suspension_travel,
        static_compression,
        points: (0..points)
            .map(|step| {
                let compression = wheel.suspension_travel * step as f32 / (points - 1) as f32;
                let force = (wheel.suspension_stiffness * compression * config.chassis_mass)
                    .min(wheel.max_suspension_force);
                (compression, force)
            })
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia::physics::vehicle_asset::VehicleAsset;
    use regia::physics::vehicle_drivetrain::DrivetrainConfig;
    use regia::physics::vehicle_proving::{ProvingScript, run_proving};
    use regia::physics::vehicle_tire::TireModel;

    const LAB_CAR: &str = include_str!("../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron");

    fn lab_car() -> VehicleConfig {
        VehicleAsset::from_ron(LAB_CAR).expect("lab car").into_config()
    }

    fn geared(config: &VehicleConfig) -> &DrivetrainConfig {
        match &config.drivetrain {
            Drivetrain::Geared(drivetrain) => drivetrain,
            Drivetrain::Simple => panic!("geared"),
        }
    }

    #[test]
    fn the_torque_plot_is_the_engine_curve() {
        let config = lab_car();
        let engine = &geared(&config).engine;
        let plot = torque_power(engine, 25);
        assert_eq!(plot.len(), 25);
        assert_eq!(plot[0].rpm, engine.idle_rpm);
        assert_eq!(plot[24].rpm, engine.redline_rpm);
        for point in &plot {
            assert_eq!(point.torque_nm.to_bits(), engine.torque_at(point.rpm).to_bits());
            let watts = point.torque_nm * point.rpm * std::f32::consts::TAU / 60.0;
            assert!((point.power_kw * 1000.0 - watts).abs() <= watts.abs() * 1e-5);
        }
        // At a curve point the plot reads the curve's own torque.
        let (rpm, torque) = engine.torque_curve[2];
        assert_eq!(engine.torque_at(rpm), torque);
    }

    #[test]
    fn the_gear_plot_follows_the_ratios_and_halves_with_a_doubled_ratio_speed() {
        let config = lab_car();
        let drivetrain = geared(&config);
        let curves = wheel_force_per_gear(&config, 9);
        assert_eq!(curves.len(), drivetrain.gearbox.forward_ratios.len());
        let radius = driven_radius(&config).expect("driven");
        assert_eq!(radius, config.wheels[2].radius);
        for curve in &curves {
            assert_eq!(curve.total_ratio, drivetrain.gearbox.total_ratio(curve.gear));
            for (point, torque) in curve.points.iter().zip(torque_power(&drivetrain.engine, 9)) {
                let force = torque.torque_nm * drivetrain.gearbox.efficiency * curve.total_ratio / radius;
                assert_eq!(point.1.to_bits(), force.to_bits());
                // Speed: the engine turns `total_ratio` times per wheel turn.
                let wheel_rpm = point.0 / 3.6 / radius * RPM_PER_RAD_S;
                assert!((wheel_rpm * curve.total_ratio - torque.rpm).abs() < 0.5);
            }
        }
        // First gear pulls harder and tops out slower than second.
        assert!(curves[0].points[4].1 > curves[1].points[4].1);
        assert!(curves[0].points[8].0 < curves[1].points[8].0);
        // A 20 % higher final drive: 20 % more force at 1/1.2 of the speed.
        let mut tuned = config.clone();
        if let Drivetrain::Geared(d) = &mut tuned.drivetrain {
            d.gearbox.final_drive *= 1.2;
        }
        let tuned_curves = wheel_force_per_gear(&tuned, 9);
        let (a, b) = (curves[0].points[4], tuned_curves[0].points[4]);
        assert!((b.1 / a.1 - 1.2).abs() < 1e-4 && (a.0 / b.0 - 1.2).abs() < 1e-4);
    }

    #[test]
    fn the_simple_drivetrain_has_one_flat_line() {
        let config = VehicleConfig::default();
        let curves = wheel_force_per_gear(&config, 5);
        assert_eq!(curves.len(), 1);
        assert!(
            curves[0]
                .points
                .iter()
                .all(|p| p.1 == config.max_engine_force * 2.0)
        );
    }

    #[test]
    fn the_tire_plot_equals_the_runtime_curve() {
        let tire = SlipTire::default();
        let plot = tire_side_force(&tire, 4000.0, 0.5, 41);
        assert_eq!(plot.len(), 41);
        for (angle, force) in &plot {
            assert_eq!(force.to_bits(), tire.lateral_force(*angle, 4000.0).to_bits());
        }
        // The same numbers as `physics.vehicle_tire_curve` at its default 41 points.
        let mut config = VehicleConfig::default();
        config.wheels[0].tire_model = TireModel::Slip(tire);
        let text = VehicleAsset::new("t", config).to_ron().expect("ron");
        let mut world = regia_ecs::prelude::World::new();
        let answer = regia_agent::panel_calls::call(
            &mut world,
            "physics.vehicle_tire_curve",
            &serde_json::json!({ "ron": text, "load": 4000.0 }),
        )
        .expect("tire curve");
        let points = answer["points"].as_array().expect("points");
        for ((angle, force), point) in plot.iter().zip(points) {
            assert_eq!(point["slip_angle"].as_f64(), Some(f64::from(*angle)));
            assert_eq!(point["lateral_force"].as_f64(), Some(f64::from(*force)));
        }
    }

    #[test]
    fn the_suspension_plot_settles_where_the_runtime_car_settles() {
        let config = VehicleConfig::default();
        let plot = suspension_travel(&config, 0, 11).expect("wheel 0");
        let wheel = &config.wheels[0];
        assert_eq!(plot.points.len(), 11);
        for (compression, force) in &plot.points {
            let spring = wheel.suspension_stiffness * compression * config.chassis_mass;
            assert_eq!(force.to_bits(), spring.min(wheel.max_suspension_force).to_bits());
        }
        // g / (4 · 30) = 0.08175 m; the settled runtime car sits there (the damper, the box's
        // pitch and rapier's substeps leave a few millimeters).
        assert!((plot.static_compression - 9.81 / 120.0).abs() < 1e-6);
        let run = run_proving(&config, ProvingScript::Accel, 0.1).expect("run");
        // The run's first sample is 0.1 s into a launch: weight moves rearward, but the four springs
        // still carry the whole car, so their mean compression is the static one.
        let first = &run.samples[0];
        let mean = first.wheels.iter().map(|w| w.compression).sum::<f32>() / first.wheels.len() as f32;
        assert!(
            (mean - plot.static_compression).abs() < 0.005,
            "{mean} vs {}",
            plot.static_compression
        );
        assert!(suspension_travel(&config, 9, 3).is_none());
    }
}
