// SPDX-License-Identifier: MPL-2.0
//! `Vehicle::set_config` (ADR-9760): live retuning keeps the car where and how fast it is, follows
//! the stated keep / reset / clamp rules, refuses an invalid config without a change, and stays
//! deterministic. Also the proving ground's scripted runs (`vehicle_proving`). Headless, fixed steps.

use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput, WheelConfig};
use regia_physics::vehicle_asset::VehicleAsset;
use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig, GearboxConfig};
use regia_physics::vehicle_proving::{
    PROVING_DT, ProvingRetune, ProvingScript, add_proving_ground, run_proving, run_proving_retuned,
    spawn_height, step_proving,
};
use regia_physics::vehicle_tire::{SlipTire, TireModel};

/// The vehicle lab's car, as the sample ships it.
const LAB_CAR: &str = include_str!("../../../samples/vehicle_lab/assets/rwd_car.vehicle.ron");

fn lab_car() -> VehicleConfig {
    VehicleAsset::from_ron(LAB_CAR)
        .expect("lab car loads")
        .into_config()
}

fn gearbox_mut(config: &mut VehicleConfig) -> &mut GearboxConfig {
    match &mut config.drivetrain {
        Drivetrain::Geared(drivetrain) => &mut drivetrain.gearbox,
        Drivetrain::Simple => panic!("the lab car is geared"),
    }
}

fn with_final_drive_scaled(mut config: VehicleConfig, scale: f32) -> VehicleConfig {
    let gearbox = gearbox_mut(&mut config);
    gearbox.final_drive *= scale;
    config
}

fn spawned(config: VehicleConfig) -> (PhysicsWorld, Vehicle) {
    let mut physics = PhysicsWorld::default();
    add_proving_ground(&mut physics);
    let height = spawn_height(&config) + 0.02;
    let vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, height, 0.0), Quat::IDENTITY).expect("spawn");
    (physics, vehicle)
}

fn drive(physics: &mut PhysicsWorld, vehicle: &mut Vehicle, input: VehicleInput, steps: usize) {
    for _ in 0..steps {
        vehicle.update(physics, input, PROVING_DT).expect("update");
        step_proving(physics);
    }
}

fn full_throttle() -> VehicleInput {
    VehicleInput {
        throttle: 1.0,
        ..VehicleInput::default()
    }
}

/// The chassis' pose and velocities, bit for bit.
fn motion_bits(physics: &PhysicsWorld, vehicle: &Vehicle) -> Vec<u32> {
    let state = vehicle.state(physics).expect("state");
    let r = state.rotation;
    [state.translation, state.linear_velocity, state.angular_velocity]
        .into_iter()
        .flat_map(|v| [v.x, v.y, v.z])
        .chain([r.x, r.y, r.z, r.w])
        .map(f32::to_bits)
        .collect()
}

#[test]
fn set_config_keeps_position_and_velocity_and_applies_the_new_gearing() {
    let (mut physics, mut vehicle) = spawned(lab_car());
    drive(&mut physics, &mut vehicle, VehicleInput::default(), 60);
    drive(&mut physics, &mut vehicle, full_throttle(), 180);
    let before = motion_bits(&physics, &vehicle);
    let steps = vehicle.steps();
    let gear = vehicle.drivetrain().expect("geared").gear();
    assert!(vehicle.state(&physics).expect("state").forward_speed > 5.0);

    let tuned = with_final_drive_scaled(lab_car(), 1.2);
    let change = vehicle.set_config(&mut physics, tuned.clone()).expect("retune");
    assert_eq!(motion_bits(&physics, &vehicle), before, "pose and velocity kept");
    assert_eq!(vehicle.steps(), steps, "the step counter is kept");
    assert_eq!(vehicle.config(), &tuned);
    assert_eq!(vehicle.config_revision(), 1);
    assert!(change.wheels_kept && change.drivetrain_kept);
    assert!(!change.chassis_updated, "the chassis fields did not change");
    assert_eq!(
        vehicle.drivetrain().expect("geared").gear(),
        gear,
        "the gear is kept"
    );
    // The next update drives with the new final drive: more wheel torque in the same gear.
    drive(&mut physics, &mut vehicle, full_throttle(), 1);
    assert_eq!(vehicle.config().digest(), tuned.digest());
}

#[test]
fn set_config_with_the_same_config_changes_nothing() {
    let (mut physics, mut vehicle) = spawned(lab_car());
    let (mut physics_b, mut vehicle_b) = spawned(lab_car());
    drive(&mut physics, &mut vehicle, full_throttle(), 200);
    drive(&mut physics_b, &mut vehicle_b, full_throttle(), 200);
    let drivetrain = vehicle.drivetrain().cloned();
    vehicle.set_config(&mut physics, lab_car()).expect("same config");
    assert_eq!(
        vehicle.drivetrain().cloned(),
        drivetrain,
        "every drivetrain field carried"
    );
    drive(&mut physics, &mut vehicle, full_throttle(), 120);
    drive(&mut physics_b, &mut vehicle_b, full_throttle(), 120);
    assert_eq!(vehicle.state_digest(), vehicle_b.state_digest());
    assert_eq!(physics.state_digest(), physics_b.state_digest());
}

#[test]
fn an_invalid_config_is_refused_with_its_code_and_changes_nothing() {
    let (mut physics, mut vehicle) = spawned(lab_car());
    drive(&mut physics, &mut vehicle, full_throttle(), 90);
    let before = motion_bits(&physics, &vehicle);
    let digest = vehicle.state_digest();
    let mut bad = lab_car();
    gearbox_mut(&mut bad).final_drive = -1.0;
    let error = vehicle.set_config(&mut physics, bad).expect_err("refused");
    assert_eq!(error.code(), "vehicle_invalid_config");
    assert!(error.to_string().contains("drivetrain.gearbox.final_drive"));
    let mut no_wheels = lab_car();
    no_wheels.wheels.clear();
    assert_eq!(
        vehicle.set_config(&mut physics, no_wheels).map_err(|e| e.code()),
        Err("vehicle_no_wheels")
    );
    assert_eq!(motion_bits(&physics, &vehicle), before);
    assert_eq!(vehicle.state_digest(), digest);
    assert_eq!(vehicle.config(), &lab_car());
    assert_eq!(vehicle.config_revision(), 0);
}

#[test]
fn a_gear_above_the_new_top_gear_is_clamped() {
    let (mut physics, mut vehicle) = spawned(lab_car());
    drive(&mut physics, &mut vehicle, full_throttle(), 60 * 20);
    let gear = vehicle.drivetrain().expect("geared").gear();
    assert!(gear >= 3, "the car reached a high gear: {gear}");
    let mut short = lab_car();
    gearbox_mut(&mut short).forward_ratios = vec![3.4, 2.1];
    let change = vehicle.set_config(&mut physics, short).expect("retune");
    assert!(change.gear_clamped);
    assert_eq!(vehicle.drivetrain().expect("geared").gear(), 2);
    drive(&mut physics, &mut vehicle, full_throttle(), 60);
}

#[test]
fn chassis_mass_and_wheel_count_follow_the_rules() {
    let (mut physics, mut vehicle) = spawned(VehicleConfig::default());
    drive(&mut physics, &mut vehicle, full_throttle(), 120);
    let handle = vehicle.chassis();
    let heavy = VehicleConfig {
        chassis_mass: 1800.0,
        ..VehicleConfig::default()
    };
    let before = motion_bits(&physics, &vehicle);
    let change = vehicle.set_config(&mut physics, heavy).expect("heavier");
    assert!(change.chassis_updated);
    assert_eq!(motion_bits(&physics, &vehicle), before);
    let mass = physics.bodies.get(handle.to_handle()).expect("body").mass();
    assert!(
        (mass - 1800.0).abs() < 1e-2,
        "the body's mass is the new one: {mass}"
    );

    // Six wheels: the wheels are rebuilt, the simple drivetrain has no state to keep.
    let mut six = VehicleConfig::default();
    six.wheels.push(WheelConfig {
        position: Vec3::new(-0.8, -0.2, 0.0),
        ..WheelConfig::default()
    });
    six.wheels.push(WheelConfig {
        position: Vec3::new(0.8, -0.2, 0.0),
        ..WheelConfig::default()
    });
    let change = vehicle.set_config(&mut physics, six).expect("six wheels");
    assert!(!change.wheels_kept && !change.drivetrain_kept);
    drive(&mut physics, &mut vehicle, full_throttle(), 30);
    assert_eq!(vehicle.wheel_states(&physics).expect("wheels").len(), 6);
    assert_eq!(vehicle.config_revision(), 2);
}

#[test]
fn slip_tires_keep_their_state_when_the_wheel_keeps_a_slip_tire() {
    let mut config = VehicleConfig::default();
    for wheel in &mut config.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire::default());
    }
    let (mut physics, mut vehicle) = spawned(config.clone());
    drive(&mut physics, &mut vehicle, full_throttle(), 90);
    let mut stiffer = config.clone();
    for wheel in &mut stiffer.wheels {
        wheel.suspension_stiffness = 40.0;
    }
    stiffer.wheels[3].tire_model = TireModel::Damper;
    let change = vehicle.set_config(&mut physics, stiffer).expect("retune");
    assert_eq!(change.tires_kept, 3);
    drive(&mut physics, &mut vehicle, full_throttle(), 30);
}

#[test]
fn a_retune_mid_run_is_deterministic() {
    let retune = ProvingRetune {
        step: 90,
        config: with_final_drive_scaled(lab_car(), 1.2),
    };
    let a = run_proving_retuned(&lab_car(), ProvingScript::Accel, 4.0, Some(&retune)).expect("run a");
    let b = run_proving_retuned(&lab_car(), ProvingScript::Accel, 4.0, Some(&retune)).expect("run b");
    assert_eq!(a.digest, b.digest, "two runs give the same telemetry hash");
    assert_eq!(a, b);
    let plain = run_proving(&lab_car(), ProvingScript::Accel, 4.0).expect("plain");
    assert_ne!(plain.digest, a.digest, "the retune changed the run");
}

#[test]
fn a_higher_final_drive_moves_the_zero_to_100_time() {
    let base = run_proving(&lab_car(), ProvingScript::Accel, 20.0).expect("base");
    let tuned = run_proving(
        &with_final_drive_scaled(lab_car(), 1.2),
        ProvingScript::Accel,
        20.0,
    )
    .expect("tuned");
    let (Some(base_t), Some(tuned_t)) = (base.zero_to_100_s, tuned.zero_to_100_s) else {
        panic!(
            "both reach 100 km/h: {:?} {:?}",
            base.zero_to_100_s, tuned.zero_to_100_s
        );
    };
    eprintln!(
        "0-100: base {base_t:.3} s, final drive +20 % {tuned_t:.3} s; top {:.1} / {:.1} km/h",
        base.top_speed_kmh, tuned.top_speed_kmh
    );
    assert!(
        tuned_t < base_t,
        "a shorter gearing accelerates harder: {tuned_t} vs {base_t}"
    );
    assert!(base.top_speed_kmh > 100.0);
}

#[test]
fn the_skidpad_holds_a_lateral_acceleration() {
    let run = run_proving(&lab_car(), ProvingScript::Skidpad, 20.0).expect("skidpad");
    eprintln!(
        "skidpad: {:.3} g, top {:.1} km/h",
        run.max_lateral_g, run.top_speed_kmh
    );
    assert!(
        run.max_lateral_g > 0.3 && run.max_lateral_g < 2.0,
        "{}",
        run.max_lateral_g
    );
    assert_eq!(run.samples.len(), 200);
    assert_eq!(run.samples[0].wheels.len(), 4);
    assert_eq!(ProvingScript::from_name("skidpad"), Some(ProvingScript::Skidpad));
    assert_eq!(ProvingScript::from_name("drift"), None);
    assert!(run_proving(&lab_car(), ProvingScript::Accel, 0.0).is_err());
    assert!(run_proving(&lab_car(), ProvingScript::Accel, 1.0e6).is_err());
}

#[test]
fn a_geared_to_simple_switch_starts_fresh() {
    let (mut physics, mut vehicle) = spawned(lab_car());
    drive(&mut physics, &mut vehicle, full_throttle(), 60);
    let mut simple = lab_car();
    simple.drivetrain = Drivetrain::Simple;
    simple.aids = Default::default();
    let change = vehicle.set_config(&mut physics, simple).expect("simple");
    assert!(!change.drivetrain_kept);
    assert!(vehicle.drivetrain().is_none());
    let mut geared = vehicle.config().clone();
    geared.drivetrain = Drivetrain::Geared(DrivetrainConfig::default());
    vehicle.set_config(&mut physics, geared).expect("geared again");
    let state = vehicle.drivetrain().expect("fresh");
    assert_eq!((state.gear(), state.upshifts()), (1, 0));
}
