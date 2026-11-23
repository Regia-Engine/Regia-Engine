// SPDX-License-Identifier: MPL-2.0
//! Saving and restoring a vehicle (ADR-3080): a saved-then-restored vehicle matches one that never
//! saved, bit for bit, and the isolated world steps exactly like the shared one.

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_core::math::{Quat, Vec3};
use regia_physics::PhysicsWorld;
use regia_physics::convert::to_rapier_vec3;
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput};
use regia_physics::vehicle_drivetrain::{Drivetrain, DrivetrainConfig};
use regia_physics::vehicle_snapshot::{
    VEHICLE_SNAPSHOT_FORMAT_VERSION, VEHICLE_SNAPSHOT_MAX_BYTES, VehicleSimSettings, VehicleSimWorld,
    VehicleSnapshot, VehicleSnapshotError,
};
use regia_physics::vehicle_tire::{SlipTire, TireModel};

const DT: f32 = 1.0 / 60.0;

fn step(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: DT,
        num_solver_iterations: 4,
        max_ccd_substeps: 1,
        ..Default::default()
    };
    let PhysicsWorld {
        pipeline,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        ..
    } = physics;
    pipeline.step(
        to_rapier_vec3(Vec3::new(0.0, -9.81, 0.0)),
        &integration,
        islands,
        broad_phase,
        narrow_phase,
        bodies,
        colliders,
        impulse_joints,
        multibody_joints,
        soft_bodies,
        ccd_solver,
        &(),
        &(),
    );
}

/// Flat ground with a kicker ramp ahead, so a driven car leaves the ground: a wheel in the air
/// coasts on the private rotation delta the snapshot has to keep.
fn level() -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    let ground =
        ColliderBuilder::cuboid(400.0, 1.0, 400.0).translation(to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)));
    physics.colliders.insert(ground);
    let ramp = RigidBodyBuilder::fixed()
        .translation(to_rapier_vec3(Vec3::new(0.0, 0.25, -60.0)))
        .rotation(to_rapier_vec3(Vec3::new(0.25, 0.0, 0.0)))
        .build();
    let ramp = physics.bodies.insert(ramp);
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    colliders.insert_with_parent(ColliderBuilder::cuboid(6.0, 0.5, 6.0), ramp, bodies);
    step(&mut physics);
    physics
}

fn geared() -> VehicleConfig {
    VehicleConfig {
        drivetrain: Drivetrain::Geared(DrivetrainConfig::default()),
        ..VehicleConfig::default()
    }
}

/// Full throttle at the ramp, with a weave after it.
fn course(step: u32) -> VehicleInput {
    VehicleInput {
        throttle: if step < 500 { 1.0 } else { 0.0 },
        brake: if step >= 500 { 0.4 } else { 0.0 },
        steer: if step > 330 {
            ((step as f32) * 0.03).sin() * 0.4
        } else {
            0.0
        },
    }
}

fn slip() -> VehicleConfig {
    let mut config = VehicleConfig::default();
    for wheel in &mut config.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire::default());
    }
    config
}

fn transient_slip() -> VehicleConfig {
    let mut config = slip();
    for wheel in &mut config.wheels {
        wheel.tire_model = TireModel::Slip(SlipTire {
            relaxation_length: 0.6,
            pneumatic_trail: 0.04,
            camber_stiffness: 4000.0,
            static_camber: -0.02,
            ..SlipTire::default()
        });
    }
    config
}

/// Something else entirely, to leave the saved state far behind.
fn detour(step: u32) -> VehicleInput {
    VehicleInput {
        throttle: -1.0,
        brake: 0.0,
        steer: if step.is_multiple_of(2) { 1.0 } else { -1.0 },
    }
}

struct Car {
    physics: PhysicsWorld,
    vehicle: Vehicle,
}

fn car(config: VehicleConfig) -> Car {
    let mut physics = level();
    let vehicle =
        Vehicle::spawn(&mut physics, config, Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY).expect("spawn");
    Car { physics, vehicle }
}

impl Car {
    fn advance(&mut self, input: VehicleInput) {
        self.vehicle.update(&mut self.physics, input, DT).expect("update");
        step(&mut self.physics);
    }

    fn digest(&self) -> (u64, u64) {
        (
            self.vehicle.snapshot(&self.physics).expect("snapshot").digest(),
            self.physics.state_digest(),
        )
    }
}

/// `save_at`, a detour of `detour_steps`, a restore, and on to `end`, against a car that never saved.
fn same_with_and_without_a_save(config: VehicleConfig, save_at: u32, detour_steps: u32, end: u32) {
    let mut plain = car(config.clone());
    let mut saved = car(config);
    for t in 0..save_at {
        plain.advance(course(t));
        saved.advance(course(t));
    }
    let snapshot = saved.vehicle.snapshot(&saved.physics).expect("snapshot");
    assert!(!snapshot.chassis_in_contact, "this rig's chassis touches nothing");
    for t in 0..detour_steps {
        saved.advance(detour(t));
    }
    assert_ne!(
        saved.digest().0,
        plain.digest().0,
        "the detour left the saved state behind"
    );
    saved
        .vehicle
        .restore(&mut saved.physics, &snapshot)
        .expect("restore");
    assert_eq!(
        saved.digest().0,
        plain.digest().0,
        "the restore is the state at the save"
    );
    for t in save_at..end {
        plain.advance(course(t));
        saved.advance(course(t));
        assert_eq!(
            saved.digest(),
            plain.digest(),
            "step {t} after a save at {save_at}"
        );
    }
}

/// On the wheels on flat ground.
#[test]
fn a_restore_matches_never_saving_while_rolling() {
    same_with_and_without_a_save(VehicleConfig::default(), 200, 40, 460);
}

/// Saved on the ramp's far side with wheels in the air: the private rotation delta matters here.
#[test]
fn a_restore_matches_never_saving_in_the_air() {
    let mut probe = car(VehicleConfig::default());
    let mut airborne = None;
    for t in 0..460 {
        probe.advance(course(t));
        let state = probe.vehicle.state(&probe.physics).expect("state");
        if state.grounded_wheels == 0 {
            airborne = Some(t + 1);
            break;
        }
    }
    let at = airborne.expect("the ramp throws the car");
    same_with_and_without_a_save(VehicleConfig::default(), at, 40, at + 200);
}

/// A geared car, mid-acceleration, with shifts on both sides of the save.
#[test]
fn a_restore_matches_never_saving_with_a_geared_drivetrain() {
    same_with_and_without_a_save(geared(), 150, 40, 460);
}

/// The same with the save taken in the middle of a gear shift: the shift clock is part of the state.
#[test]
fn a_restore_matches_never_saving_in_the_middle_of_a_shift() {
    let mut probe = car(geared());
    let mut shifting = None;
    for t in 0..400 {
        probe.advance(course(t));
        if probe.vehicle.drivetrain().expect("geared").is_shifting() {
            shifting = Some(t + 1);
            break;
        }
    }
    let at = shifting.expect("the car shifts");
    same_with_and_without_a_save(geared(), at, 40, at + 200);
}

/// Slip tires, rolling: the tires' state is part of the digest.
#[test]
fn a_restore_matches_never_saving_with_slip_tires() {
    same_with_and_without_a_save(slip(), 200, 40, 460);
}

const HELD: VehicleInput = VehicleInput {
    throttle: 0.0,
    brake: 1.0,
    steer: 0.0,
};

/// A slope a car can hold on, with the car spawned above it. `parented` puts the ground on a fixed
/// body, so the stick anchors are in that body's frame; otherwise they are in world space.
fn parked(parented: bool) -> Car {
    let mut physics = PhysicsWorld::default();
    let tilt = to_rapier_vec3(Vec3::new(0.12, 0.0, 0.0));
    let ground = ColliderBuilder::cuboid(100.0, 1.0, 100.0);
    if parented {
        let body = physics.bodies.insert(
            RigidBodyBuilder::fixed()
                .translation(to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)))
                .rotation(tilt)
                .build(),
        );
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(ground, body, bodies);
    } else {
        physics.colliders.insert(
            ground
                .translation(to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)))
                .rotation(tilt),
        );
    }
    step(&mut physics);
    let vehicle =
        Vehicle::spawn(&mut physics, slip(), Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY).expect("spawn");
    let mut car = Car { physics, vehicle };
    for _ in 0..240 {
        car.advance(HELD);
    }
    car
}

/// The stick anchors are what the next step reads on a held car: losing one moves the car. Saved
/// held on a slope, driven off, restored, and through the text form on the way.
#[test]
fn a_restore_keeps_a_held_cars_stick_anchors() {
    for parented in [false, true] {
        let mut plain = parked(parented);
        let mut saved = parked(parented);
        let snapshot = saved.vehicle.snapshot(&saved.physics).expect("snapshot");
        assert!(!snapshot.chassis_in_contact, "the wheels hold the chassis up");
        assert_eq!(snapshot.tires.len(), 4);
        assert!(
            snapshot
                .tires
                .iter()
                .all(|tire| tire.stuck && tire.anchor.is_some()),
            "a held car's tires are stuck: {:?}",
            snapshot.tires
        );
        assert_eq!(
            snapshot.tires.iter().all(|tire| tire.anchor_body.is_some()),
            parented,
            "the anchors are in the ground body's frame only when there is one"
        );
        let read = VehicleSnapshot::from_ron(&snapshot.to_ron().expect("write")).expect("read");
        assert_eq!(read, snapshot);
        for t in 0..40 {
            saved.advance(detour(t));
        }
        assert_ne!(saved.digest().0, plain.digest().0);
        saved.vehicle.restore(&mut saved.physics, &read).expect("restore");
        assert_eq!(saved.digest().0, plain.digest().0, "the restore is the save");
        for t in 0..180 {
            plain.advance(HELD);
            saved.advance(HELD);
            assert_eq!(saved.digest(), plain.digest(), "step {t}, parented {parented}");
        }
    }
}

/// A correction moves the chassis, so an anchor kept across it would pull the car back to where it
/// was. It sticks afresh instead, and holds where the correction put it.
#[test]
fn a_correction_lets_go_of_the_stick_anchors() {
    let mut car = parked(false);
    let mut core = car.vehicle.core(&car.physics).expect("core");
    core.translation.x += 0.05;
    let corrected = core.translation.x;
    car.vehicle
        .apply_core(&mut car.physics, &core)
        .expect("correction");
    let after = car.vehicle.snapshot(&car.physics).expect("snapshot");
    assert!(after.tires.iter().all(|tire| tire.anchor.is_none()));
    for _ in 0..120 {
        car.advance(HELD);
    }
    let held = car.vehicle.snapshot(&car.physics).expect("snapshot");
    assert!(
        held.tires.iter().all(|tire| tire.stuck && tire.anchor.is_some()),
        "stuck again: {:?}",
        held.tires
    );
    let moved = (held.translation.x - corrected).abs();
    assert!(moved < 0.005, "the car moved {moved} m after the correction");
}

/// A tire state that cannot be this car's, or is not a number, changes nothing.
#[test]
fn bad_tire_states_are_refused() {
    let mut a = car(slip());
    for t in 0..120 {
        a.advance(course(t));
    }
    let good = a.vehicle.snapshot(&a.physics).expect("snapshot");
    let before = a.digest();
    let mut short = good.clone();
    short.tires.pop();
    let code = a
        .vehicle
        .restore(&mut a.physics, &short)
        .expect_err("short")
        .code();
    assert_eq!(code, "vehicle_snapshot_config_mismatch");
    let mut nan = good.clone();
    nan.tires[0].anchor = Some(Vec3::new(f32::NAN, 0.0, 0.0));
    let code = a.vehicle.restore(&mut a.physics, &nan).expect_err("nan").code();
    assert_eq!(code, "vehicle_snapshot_not_finite");
    let mut nan = good.clone();
    nan.tires[1].load = f32::INFINITY;
    assert_eq!(
        nan.validate().expect_err("inf").code(),
        "vehicle_snapshot_not_finite"
    );
    assert_eq!(a.digest(), before, "a refused restore changes nothing");
    let mut damper = car(VehicleConfig::default());
    let code = damper
        .vehicle
        .restore(&mut damper.physics, &good)
        .expect_err("another car")
        .code();
    assert_eq!(code, "vehicle_snapshot_config_mismatch");
}

/// The text form carries every bit: a car restored from the file goes on exactly like the original.
#[test]
fn the_text_form_keeps_every_bit() {
    for config in [VehicleConfig::default(), geared(), slip(), transient_slip()] {
        let transients = config
            .wheels
            .iter()
            .any(|w| w.tire_model.slip().is_some_and(SlipTire::has_transients));
        let mut plain = car(config.clone());
        let mut saved = car(config);
        for t in 0..240 {
            plain.advance(course(t));
            saved.advance(course(t));
        }
        let snapshot = saved.vehicle.snapshot(&saved.physics).expect("snapshot");
        if transients {
            assert!(
                snapshot.tires.iter().any(|t| t.lag != 0.0),
                "lag history is exercised"
            );
            assert!(
                snapshot.tires.iter().any(|t| t.camber_thrust != 0.0),
                "camber state is exercised"
            );
        }
        let text = snapshot.to_ron().expect("write");
        let read = VehicleSnapshot::from_ron(&text).expect("read");
        assert_eq!(read, snapshot);
        for t in 0..30 {
            saved.advance(detour(t));
        }
        saved.vehicle.restore(&mut saved.physics, &read).expect("restore");
        for t in 240..300 {
            plain.advance(course(t));
            saved.advance(course(t));
            assert_eq!(saved.digest(), plain.digest(), "step {t}");
        }
    }
}

fn code_of(text: &str) -> &'static str {
    VehicleSnapshot::from_ron(text).expect_err("refused").code()
}

/// Every refusal has its documented code, and a refused restore changes nothing.
#[test]
fn bad_snapshots_are_refused_with_their_codes() {
    let mut a = car(VehicleConfig::default());
    for t in 0..120 {
        a.advance(course(t));
    }
    let good = a.vehicle.snapshot(&a.physics).expect("snapshot");
    let text = good.to_ron().expect("write");

    let before = a.digest();
    let mut wrong = good.clone();
    wrong.wheels.pop();
    assert_eq!(
        a.vehicle
            .restore(&mut a.physics, &wrong)
            .expect_err("count")
            .code(),
        "vehicle_snapshot_wheel_count"
    );
    let mut wrong = good.clone();
    wrong.wheels[0].radius = 9.0;
    assert_eq!(
        a.vehicle
            .restore(&mut a.physics, &wrong)
            .expect_err("radius")
            .code(),
        "vehicle_snapshot_config_mismatch"
    );
    let mut wrong = good.clone();
    wrong.translation.x = f32::NAN;
    assert_eq!(
        a.vehicle.restore(&mut a.physics, &wrong).expect_err("nan").code(),
        "vehicle_snapshot_not_finite"
    );
    let geared_car = car(geared());
    let geared_snapshot = geared_car
        .vehicle
        .snapshot(&geared_car.physics)
        .expect("snapshot");
    assert_eq!(
        a.vehicle
            .restore(&mut a.physics, &geared_snapshot)
            .expect_err("model")
            .code(),
        "vehicle_snapshot_config_mismatch"
    );
    assert_eq!(a.digest(), before, "a refused restore changes nothing");

    assert_eq!(code_of("(nonsense"), "vehicle_snapshot_parse");
    let other = text.replacen(
        &format!("format_version:{VEHICLE_SNAPSHOT_FORMAT_VERSION}"),
        "format_version:99",
        1,
    );
    assert_ne!(other, text);
    assert_eq!(code_of(&other), "vehicle_snapshot_unsupported_version");
    let padded = format!("{text}{}", " ".repeat(VEHICLE_SNAPSHOT_MAX_BYTES));
    assert_eq!(code_of(&padded), "vehicle_snapshot_too_large");
    // A NaN in the one wheel field a reader cannot see through the public API.
    let hostile = text.replacen("delta_rotation:", "delta_rotation:NaN,zz:", 1);
    assert_ne!(hostile, text);
    assert!(
        matches!(
            VehicleSnapshot::from_ron(&hostile),
            Err(VehicleSnapshotError::Parse(_) | VehicleSnapshotError::NotFinite { .. })
        ),
        "{hostile}"
    );
}

/// ADR-3080's claim: the private world steps exactly like the shared one, with other bodies moving
/// in the shared one, so a client's prediction needs no correction.
#[test]
fn the_private_world_steps_like_the_shared_one() {
    for config in [VehicleConfig::default(), geared(), slip()] {
        let mut shared = car(config.clone());
        // A falling crate far from the course: the shared world has more going on than the private one.
        let body = shared.physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::new(200.0, 40.0, 200.0)))
                .build(),
        );
        {
            let PhysicsWorld {
                bodies, colliders, ..
            } = &mut shared.physics;
            colliders.insert_with_parent(ColliderBuilder::cuboid(1.0, 1.0, 1.0), body, bodies);
        }
        let statics = level();
        let mut private = VehicleSimWorld::new(
            config,
            &statics,
            VehicleSimSettings::default(),
            (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
            DT,
        )
        .expect("private");
        let mut fastest = 0.0_f32;
        for t in 0..600 {
            shared.advance(course(t));
            private.step(course(t), DT, None).expect("step");
            let (a, b) = (
                shared.vehicle.snapshot(&shared.physics).expect("shared"),
                private.snapshot().expect("private"),
            );
            assert_eq!(
                (a.translation, a.rotation, a.linear_velocity, a.angular_velocity),
                (b.translation, b.rotation, b.linear_velocity, b.angular_velocity),
                "tick {t}"
            );
            fastest = fastest.max(b.linear_velocity.length());
        }
        assert!(fastest > 15.0, "it drove: {fastest} m/s");
    }
}

/// With the chassis body against a wall, the plain snapshot says so; a checkpoint of the private
/// world goes back exactly anyway.
#[test]
fn a_checkpoint_is_exact_with_the_chassis_in_contact() {
    let mut statics = level();
    statics.colliders.insert(
        ColliderBuilder::cuboid(10.0, 3.0, 0.5).translation(to_rapier_vec3(Vec3::new(0.0, 3.0, -30.0))),
    );
    step(&mut statics);
    let new = || {
        VehicleSimWorld::new(
            VehicleConfig::default(),
            &statics,
            VehicleSimSettings::default(),
            (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
            DT,
        )
        .expect("private")
    };
    let (mut plain, mut saved) = (new(), new());
    let drive = VehicleInput {
        throttle: 1.0,
        ..VehicleInput::default()
    };
    let mut touching_at = None;
    for t in 0..600 {
        plain.step(drive, DT, None).expect("step");
        saved.step(drive, DT, None).expect("step");
        if saved.snapshot().expect("snapshot").chassis_in_contact {
            touching_at = Some(t);
            break;
        }
    }
    let touching_at = touching_at.expect("the car reaches the wall");
    assert!(touching_at > 10, "it drove there first: {touching_at}");
    let checkpoint = saved.checkpoint().expect("checkpoint");
    assert!(
        checkpoint.snapshot().chassis_in_contact,
        "the plain snapshot flags it"
    );
    for t in 0..30 {
        saved.step(detour(t), DT, None).expect("detour");
    }
    saved.rewind(&checkpoint).expect("rewind");
    for t in 0..120 {
        plain.step(drive, DT, None).expect("step");
        saved.step(drive, DT, None).expect("step");
        assert_eq!(
            saved.snapshot().expect("a").digest(),
            plain.snapshot().expect("b").digest(),
            "step {t} after the rewind"
        );
    }
}

/// A wall streamed in after the private world was built stops the car; an unchanged shared world
/// costs no rebuild, and the car stays where it was.
#[test]
fn a_static_collider_added_later_reaches_the_private_world() {
    let mut shared = level();
    let mut private = VehicleSimWorld::new(
        VehicleConfig::default(),
        &shared,
        VehicleSimSettings::default(),
        (Vec3::new(0.0, 1.0, 0.0), Quat::IDENTITY),
        DT,
    )
    .expect("private");
    for _ in 0..60 {
        private.step(VehicleInput::default(), DT, None).expect("step");
    }
    assert!(!private.sync_statics(&shared, DT).expect("no change"));
    let before = private.snapshot().expect("before");
    let colliders = private.collider_count();
    shared.colliders.insert(
        ColliderBuilder::cuboid(10.0, 3.0, 0.5).translation(to_rapier_vec3(Vec3::new(0.0, 3.0, -10.0))),
    );
    assert!(private.sync_statics(&shared, DT).expect("change"));
    assert_eq!(private.collider_count(), colliders + 1);
    let after = private.snapshot().expect("after");
    assert_eq!(after.translation, before.translation);
    assert_eq!(after.linear_velocity, before.linear_velocity);
    for _ in 0..600 {
        private
            .step(
                VehicleInput {
                    throttle: 1.0,
                    ..VehicleInput::default()
                },
                DT,
                None,
            )
            .expect("step");
    }
    let end = private.snapshot().expect("end");
    assert!(
        (-10.0..-5.0).contains(&end.translation.z),
        "the car drove to the wall and the wall stopped it: {}",
        end.translation
    );
}

/// The readers name what the brief lists: spin, suspension, rpm, gear, clutch.
#[test]
fn the_snapshot_reads_what_the_agent_shows() {
    let mut a = car(geared());
    for t in 0..120 {
        a.advance(course(t));
    }
    let snapshot = a.vehicle.snapshot(&a.physics).expect("snapshot");
    let state = a.vehicle.state(&a.physics).expect("state");
    assert_eq!(snapshot.rpm(), state.rpm);
    assert_eq!(snapshot.gear(), state.gear);
    assert_eq!(snapshot.spin_angle(0), Some(state.wheels[0].spin_angle));
    assert_eq!(
        snapshot.suspension_length(1),
        Some(state.wheels[1].suspension_length)
    );
    assert_eq!(
        snapshot.clutch_torque(),
        a.vehicle.drivetrain().expect("geared").clutch_torque()
    );
    assert_eq!(snapshot.spin_angle(99), None);
}

/// Frozen v1 saves acquire zero transient state and become current v2 snapshots on read.
#[test]
fn v1_tire_snapshots_upgrade_without_changing_the_saved_state() {
    let a = car(slip());
    let expected = a.vehicle.snapshot(&a.physics).expect("snapshot");
    let mut old = expected.to_ron().expect("write").replace(
        &format!("format_version:{VEHICLE_SNAPSHOT_FORMAT_VERSION}"),
        "format_version:1",
    );
    for field in [
        "lag",
        "lagged_slip_angle",
        "aligning_torque",
        "camber",
        "camber_thrust",
    ] {
        // A freshly spawned car has no tire history; remove the v2 fields to model a v1 writer.
        let member = format!(",{field}:0.0");
        assert!(old.contains(&member), "missing fixture field {field}");
        old = old.replace(&member, "");
    }
    let read = VehicleSnapshot::from_ron(&old).expect("v1 reads");
    assert_eq!(read.format_version, 2);
    assert_eq!(
        read, expected,
        "only the header version changes; tire history defaults to zero"
    );
    let upgraded = regia_physics::vehicle_snapshot::migrate_vehicle_snapshot(&old).expect("migrate");
    assert_eq!(VehicleSnapshot::from_ron(&upgraded).expect("v2 reads"), expected);
    let commented = format!("// Keep current-file comments\n{upgraded}");
    assert_eq!(
        regia_physics::vehicle_snapshot::migrate_vehicle_snapshot(&commented).expect("current"),
        commented
    );
    assert_eq!(
        regia_physics::vehicle_snapshot::vehicle_snapshot_migration_sources(),
        [1]
    );
}

#[test]
fn snapshot_schema_tracks_nested_tire_fields() {
    let schema = regia_physics::vehicle_snapshot::vehicle_snapshot_schema_fingerprint();
    for field in [
        "lag",
        "lagged_slip_angle",
        "aligning_torque",
        "camber",
        "camber_thrust",
    ] {
        assert!(schema.contains(field), "schema omitted {field}: {schema}");
    }
}
