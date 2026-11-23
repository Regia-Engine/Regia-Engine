// SPDX-License-Identifier: MPL-2.0
//! Trailers (`regia_physics::vehicle_trailer`, ADR-3281), headless.
//!
//! Every test builds a bare [`PhysicsWorld`] (or an app with `PhysicsPlugin`, `VehiclePlugin`
//! and `TrailerPlugin`) and steps it a fixed number of times at 60 Hz with scripted inputs: a
//! car and trailer tracking straight, the steady-circle articulation against the kinematic
//! off-tracking geometry, trailer brakes shortening a stop, the jackknife stop in reverse, the
//! stand, the capture radius, a two-trailer train after a lane change, a tractor and semi-trailer,
//! a lost tow body, and a determinism pin. No GPU, window or wall clock.

use std::time::Duration;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::ColliderBuilder;
use regia_app::prelude::*;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_physics::convert::{from_rapier_pose, from_rapier_vec3, to_rapier_vec3};
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput, VehiclePlugin, spawn_vehicle};
use regia_physics::vehicle_trailer::{
    CouplingKind, TowHitch, Trailer, TrailerBrakes, TrailerConfig, TrailerController, TrailerError,
    TrailerPlugin, TrailerReadout, TrailerState, car_tow_hitch, couple_trailer, spawn_trailer,
    tractor_config, tractor_fifth_wheel, uncouple_trailer,
};
use regia_physics::{PhysicsPlugin, PhysicsWorld, RigidBodyHandle};

/// The fixed step every test runs at.
const DT: f32 = 1.0 / 60.0;

/// One degree in radians.
const DEG: f32 = std::f32::consts::PI / 180.0;

/// Steps the rapier world once, the way `step_physics` does (gravity, fixed dt).
fn step(physics: &mut PhysicsWorld) {
    let integration = IntegrationParameters {
        dt: DT,
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

/// A fixed box whose top face is the plane y = 0.
fn ground_box(physics: &mut PhysicsWorld, half: f32) {
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -1.0, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = physics;
    colliders.insert_with_parent(ColliderBuilder::cuboid(half, 1.0, half).build(), body, bodies);
}

/// A world with a 4 km square flat plane at y = 0.
fn flat_world() -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    ground_box(&mut physics, 2000.0);
    physics
}

/// Height the default car's chassis center rests at.
fn car_rest_height() -> f32 {
    let config = VehicleConfig::default();
    let wheel = &config.wheels[0];
    let compression = 9.81 / (config.wheels.len() as f32 * wheel.suspension_stiffness);
    -wheel.position.y + (wheel.suspension_rest_length - compression) + wheel.radius
}

/// A car and its trains, stepped together.
struct Rig {
    physics: PhysicsWorld,
    car: Vehicle,
    trailers: Vec<Trailer>,
}

impl Rig {
    /// A default car at the origin facing −Z with `configs` coupled behind it in a chain (each
    /// on the rear hitch of the one before), settled for 120 steps.
    fn new(configs: Vec<TrailerConfig>) -> Self {
        Self::with_tow(
            VehicleConfig::default(),
            car_tow_hitch(),
            car_rest_height(),
            configs,
        )
    }

    /// [`Rig::new`] with any tow vehicle, its hitch and its rest height.
    fn with_tow(tow: VehicleConfig, hitch: Vec3, rest: f32, configs: Vec<TrailerConfig>) -> Self {
        let mut physics = flat_world();
        let car = Vehicle::spawn(
            &mut physics,
            tow,
            Vec3::new(0.0, rest + 0.02, 0.0),
            Quat::IDENTITY,
        )
        .expect("spawn car");
        let mut trailers: Vec<Trailer> = Vec::new();
        for config in configs {
            let (body, point) = match trailers.last() {
                None => (car.chassis(), hitch),
                Some(front) => (
                    front.chassis(),
                    front
                        .config()
                        .rear_hitch
                        .expect("the front trailer has a rear hitch"),
                ),
            };
            trailers.push(Trailer::spawn_coupled(&mut physics, config, body, point).expect("couple"));
        }
        let mut rig = Self {
            physics,
            car,
            trailers,
        };
        rig.run(VehicleInput::default(), 120);
        rig
    }

    /// One fixed step: car, trailers front to back, world.
    fn step(&mut self, input: VehicleInput) {
        self.car.update(&mut self.physics, input, DT).expect("car update");
        let brake = input.sanitized().brake;
        for trailer in &mut self.trailers {
            trailer
                .update(&mut self.physics, brake, DT, None)
                .expect("trailer update");
        }
        step(&mut self.physics);
    }

    fn run(&mut self, input: VehicleInput, steps: usize) {
        for _ in 0..steps {
            self.step(input);
        }
    }

    fn readout(&self, index: usize) -> TrailerReadout {
        self.trailers[index].readout(&self.physics).expect("readout")
    }

    fn car_speed(&self) -> f32 {
        self.car.state(&self.physics).expect("state").forward_speed
    }

    /// Sets every body of the rig moving forward (−Z) at `speed`.
    fn launch(&mut self, speed: f32) {
        let mut bodies = vec![self.car.chassis()];
        bodies.extend(self.trailers.iter().map(Trailer::chassis));
        for handle in bodies {
            if let Some(body) = self.physics.bodies.get_mut(handle.to_handle()) {
                let (_, rotation) = from_rapier_pose(body.position());
                body.set_linvel(to_rapier_vec3(rotation * Vec3::NEG_Z * speed), true);
            }
        }
    }

    /// Throttle (or reverse throttle) holding the car at `target` m/s.
    fn hold(&self, target: f32, steer: f32) -> VehicleInput {
        let error = target - self.car_speed();
        let throttle = if target >= 0.0 {
            (error * 0.8).clamp(0.0, 1.0)
        } else {
            (error * 0.8).clamp(-1.0, 0.0)
        };
        VehicleInput {
            throttle,
            brake: 0.0,
            steer,
        }
    }
}

/// A point of a body (body space) in world space and its velocity.
fn point_motion(physics: &PhysicsWorld, body: RigidBodyHandle, local: Vec3) -> (Vec3, Vec3) {
    let body = &physics.bodies[body.to_handle()];
    let (t, r) = from_rapier_pose(body.position());
    let world = t + r * local;
    let velocity = from_rapier_vec3(body.velocity_at_point(to_rapier_vec3(world)));
    (world, velocity)
}

fn yaw_rate(physics: &PhysicsWorld, body: RigidBodyHandle) -> f32 {
    from_rapier_vec3(physics.bodies[body.to_handle()].angvel()).y
}

// ---------------------------------------------------------------------------
// Acceptance
// ---------------------------------------------------------------------------

#[test]
fn a_car_and_trailer_driving_straight_keep_the_articulation_under_half_a_degree() {
    let mut rig = Rig::new(vec![TrailerConfig::car_trailer()]);
    rig.launch(15.0);
    let start = rig.readout(0);
    assert!(start.coupled);
    let mut worst = 0.0_f32;
    let mut last = 0.0;
    for _ in 0..1200 {
        let input = rig.hold(15.0, 0.0);
        rig.step(input);
        let readout = rig.readout(0);
        worst = worst.max(readout.articulation_angle.abs());
        last = readout.articulation_angle;
        assert!(readout.coupling_gap < 0.02, "gap {}", readout.coupling_gap);
        assert!(!readout.jackknife_warning);
    }
    assert!(worst < 0.5 * DEG, "worst articulation {} deg", worst / DEG);
    // No drift: the trailer stays on the car's line.
    let (trailer_at, _) = point_motion(&rig.physics, rig.trailers[0].chassis(), Vec3::ZERO);
    let (car_at, _) = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO);
    assert!(
        trailer_at.x.abs() < 0.05 && car_at.x.abs() < 0.05,
        "{car_at} {trailer_at}"
    );
    assert!(last.abs() < 0.5 * DEG);
    assert!((rig.car_speed() - 15.0).abs() < 0.5, "{}", rig.car_speed());
    assert!(car_at.z < -200.0, "the car drove {car_at}");
}

#[test]
fn the_steady_circle_articulation_matches_the_kinematic_off_tracking_geometry() {
    let mut rig = Rig::new(vec![TrailerConfig::car_trailer()]);
    let rear_axle = Vec3::new(0.0, -0.2, 1.3);
    let hitch_behind_axle = car_tow_hitch().z - rear_axle.z;
    let config = TrailerConfig::car_trailer();
    let trailer_axle = Vec3::new(0.0, -0.15, config.chassis.wheels[0].position.z);
    let drawbar = trailer_axle.z - config.coupling.point.z;
    // A slow right turn, run until the circle is steady.
    for _ in 0..1800 {
        let input = rig.hold(3.0, 0.4);
        rig.step(input);
    }
    let (mut measured, mut radius, mut trailer_radius) = (0.0, 0.0, 0.0);
    let samples = 300;
    for _ in 0..samples {
        let input = rig.hold(3.0, 0.4);
        rig.step(input);
        measured += rig.readout(0).articulation_angle;
        let (_, axle_velocity) = point_motion(&rig.physics, rig.car.chassis(), rear_axle);
        radius += axle_velocity.length() / yaw_rate(&rig.physics, rig.car.chassis()).abs();
        let trailer = rig.trailers[0].chassis();
        let (_, trailer_velocity) = point_motion(&rig.physics, trailer, trailer_axle);
        trailer_radius += trailer_velocity.length() / yaw_rate(&rig.physics, trailer).abs();
    }
    let n = samples as f32;
    let (measured, radius, trailer_radius) = (measured / n, radius / n, trailer_radius / n);
    // Steady state with no tire slip: the hitch runs on sqrt(R² + c²); the trailer axle's radius
    // is perpendicular to the drawbar.
    let hitch_radius = (radius * radius + hitch_behind_axle * hitch_behind_axle).sqrt();
    let predicted = (hitch_behind_axle / radius).atan() + (drawbar / hitch_radius).asin();
    let predicted_radius = (hitch_radius * hitch_radius - drawbar * drawbar).sqrt();
    println!(
        "R {radius:.3} m, articulation {:.3} deg (kinematic {:.3}), trailer axle radius {trailer_radius:.3} m \
         (kinematic {predicted_radius:.3}), off-tracking {:.3} m (kinematic {:.3})",
        measured / DEG,
        predicted / DEG,
        radius - trailer_radius,
        radius - predicted_radius,
    );
    assert!(radius > 5.0 && radius < 40.0, "turn radius {radius}");
    assert!(
        measured > 0.0,
        "a right turn swings the trailer to the car's left: {measured}"
    );
    assert!(
        (measured - predicted).abs() <= 0.1 * predicted,
        "articulation {} deg vs kinematic {} deg",
        measured / DEG,
        predicted / DEG
    );
    let off_tracking = radius - trailer_radius;
    let predicted_off_tracking = radius - predicted_radius;
    assert!(
        (off_tracking - predicted_off_tracking).abs() <= 0.1 * predicted_off_tracking.abs(),
        "off-tracking {off_tracking} vs kinematic {predicted_off_tracking}"
    );
}

/// Distance the rig needs to stop from 20 m/s under full brake.
fn stopping_distance(brakes: TrailerBrakes) -> f32 {
    let mut rig = Rig::new(vec![TrailerConfig {
        brakes,
        ..TrailerConfig::car_trailer()
    }]);
    rig.launch(20.0);
    let (start, _) = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO);
    let brake = VehicleInput {
        throttle: 0.0,
        brake: 1.0,
        steer: 0.0,
    };
    let mut steps = 0;
    while rig.car_speed() > 0.05 && steps < 1200 {
        rig.step(brake);
        steps += 1;
    }
    assert!(steps < 1200, "the rig never stopped");
    let (end, _) = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO);
    // The brakes kept it straight.
    assert!(rig.readout(0).articulation_angle.abs() < 2.0 * DEG);
    start.distance(end)
}

#[test]
fn trailer_brakes_shorten_the_stop_from_20_meters_per_second() {
    let with = stopping_distance(TrailerBrakes::Follow {
        proportion: 1.0,
        delay: 0.1,
    });
    let without = stopping_distance(TrailerBrakes::None);
    let surge = stopping_distance(TrailerBrakes::Surge {
        threshold: 300.0,
        full_force: 3000.0,
        response: 0.15,
    });
    println!("stopping distance: follow {with:.2} m, surge {surge:.2} m, none {without:.2} m");
    assert!(with < without * 0.95, "with {with} m, without {without} m");
    assert!(surge < without * 0.95, "surge {surge} m, without {without} m");
}

#[test]
fn the_jackknife_stop_holds_under_hard_steering_in_reverse() {
    let limit = 50.0 * DEG;
    let mut config = TrailerConfig::car_trailer();
    config.coupling.kind = CouplingKind::TowBall {
        yaw_limit: limit,
        pitch_limit: 20.0 * DEG,
        roll_limit: 20.0 * DEG,
    };
    config.coupling.jackknife_warning_angle = 40.0 * DEG;
    let mut rig = Rig::new(vec![config]);
    let (mut worst, mut warned, mut worst_gap) = (0.0_f32, false, 0.0_f32);
    for _ in 0..900 {
        let input = rig.hold(-2.0, 1.0);
        rig.step(input);
        let readout = rig.readout(0);
        worst = worst.max(readout.articulation_angle.abs());
        worst_gap = worst_gap.max(readout.coupling_gap);
        warned |= readout.jackknife_warning;
        assert!(readout.articulation_angle.is_finite());
    }
    println!(
        "reverse: worst articulation {:.3} deg against a {:.1} deg stop, worst gap {worst_gap:.4} m",
        worst / DEG,
        limit / DEG
    );
    assert!(
        worst > limit - 3.0 * DEG,
        "never reached the stop: {} deg",
        worst / DEG
    );
    assert!(worst <= limit + 2.0 * DEG, "past the stop: {} deg", worst / DEG);
    assert!(warned, "the jackknife warning never fired");
    assert!(worst_gap < 0.05, "the coupling opened by {worst_gap} m");
}

/// The lowest point of the stand's foot, world y.
fn stand_bottom(physics: &PhysicsWorld, trailer: &Trailer) -> f32 {
    let stand = trailer.config().stand.expect("stand");
    let (foot, _) = point_motion(physics, trailer.chassis(), stand.foot);
    foot.y
}

/// The chassis' speed in the ground plane, m/s (a body resting on ray-cast springs reads a small
/// vertical velocity while it holds still; see `VehicleState::linear_velocity`).
fn horizontal_speed(physics: &PhysicsWorld, trailer: &Trailer) -> f32 {
    let velocity = from_rapier_vec3(physics.bodies[trailer.chassis().to_handle()].linvel());
    Vec3::new(velocity.x, 0.0, velocity.z).length()
}

#[test]
fn an_uncoupled_trailer_rests_on_its_stand() {
    // Spawned on its own, just above its coupled height.
    let mut physics = flat_world();
    let mut trailer = Trailer::spawn(
        &mut physics,
        TrailerConfig::car_trailer(),
        Vec3::new(0.0, 0.62, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    for _ in 0..600 {
        trailer.update(&mut physics, 1.0, DT, None).expect("update");
        step(&mut physics);
    }
    let readout = trailer.readout(&physics).expect("readout");
    assert!(!readout.coupled);
    assert_eq!(readout.brake, 1.0, "the parking brake holds");
    let foot = stand_bottom(&physics, &trailer);
    assert!(foot > -0.02 && foot < 0.05, "stand foot at {foot}");
    assert!(horizontal_speed(&physics, &trailer) < 0.01);
    let height = point_motion(&physics, trailer.chassis(), Vec3::ZERO).0.y;
    for _ in 0..120 {
        trailer.update(&mut physics, 1.0, DT, None).expect("update");
        step(&mut physics);
    }
    let later = point_motion(&physics, trailer.chassis(), Vec3::ZERO).0.y;
    assert!((later - height).abs() < 0.002, "{height} -> {later}");

    // Uncoupled from a car: the nose settles onto the stand, nothing falls through.
    let mut rig = Rig::new(vec![TrailerConfig::car_trailer()]);
    rig.trailers[0].uncouple(&mut rig.physics).expect("uncouple");
    assert_eq!(
        rig.trailers[0].uncouple(&mut rig.physics).map_err(|e| e.code()),
        Err("trailer_not_coupled")
    );
    // The car drives off.
    for _ in 0..300 {
        rig.step(VehicleInput {
            throttle: 0.5,
            brake: 0.0,
            steer: 0.0,
        });
    }
    for _ in 0..300 {
        rig.step(VehicleInput::default());
    }
    let foot = stand_bottom(&rig.physics, &rig.trailers[0]);
    assert!(foot > -0.02 && foot < 0.05, "stand foot at {foot}");
    assert!(horizontal_speed(&rig.physics, &rig.trailers[0]) < 0.01);
    let (car_at, _) = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO);
    assert!(car_at.z < -5.0, "the car left: {car_at}");
}

#[test]
fn coupling_outside_the_capture_radius_fails_with_its_code() {
    let mut physics = flat_world();
    let car = Vehicle::spawn(
        &mut physics,
        VehicleConfig::default(),
        Vec3::new(0.0, car_rest_height(), 0.0),
        Quat::IDENTITY,
    )
    .expect("car");
    let config = TrailerConfig::car_trailer();
    let lined_up = car_tow_hitch() - config.coupling.point + Vec3::new(0.0, car_rest_height(), 0.0);
    let mut far = Trailer::spawn(
        &mut physics,
        config.clone(),
        lined_up + Vec3::new(0.0, 0.0, 1.0),
        Quat::IDENTITY,
    )
    .expect("far trailer");
    let error = far
        .couple(&mut physics, car.chassis(), car_tow_hitch())
        .expect_err("out of reach");
    assert_eq!(error.code(), "trailer_out_of_reach");
    let TrailerError::OutOfReach { distance, .. } = error else {
        panic!("{error:?}");
    };
    assert!((distance - 1.0).abs() < 1.0e-3, "{distance}");
    assert!(!far.is_coupled());
    assert_eq!(physics.impulse_joints.len(), 0);
    assert!(far.remove(&mut physics));

    // 0.2 m off is within the default 0.3 m: the joint pulls the gap shut.
    let mut near = Trailer::spawn(
        &mut physics,
        config,
        lined_up + Vec3::new(0.15, 0.0, 0.13),
        Quat::IDENTITY,
    )
    .expect("near trailer");
    near.couple(&mut physics, car.chassis(), car_tow_hitch())
        .expect("within the capture radius");
    assert_eq!(
        near.couple(&mut physics, car.chassis(), car_tow_hitch())
            .map_err(|e| e.code()),
        Err("trailer_already_coupled")
    );
    assert_eq!(
        near.couple(&mut physics, near.chassis(), Vec3::ZERO)
            .map_err(|e| e.code()),
        Err("trailer_already_coupled")
    );
    for _ in 0..120 {
        near.update(&mut physics, 0.0, DT, None).expect("update");
        step(&mut physics);
    }
    let gap = near.readout(&physics).expect("readout").coupling_gap;
    assert!(gap < 0.02, "gap {gap}");
}

#[test]
fn a_two_trailer_train_damps_out_after_a_lane_change_at_25_meters_per_second() {
    let mut front = TrailerConfig::car_trailer();
    front.rear_hitch = Some(Vec3::new(0.0, 0.0, 1.5));
    let mut rig = Rig::new(vec![front, TrailerConfig::car_trailer()]);
    rig.launch(25.0);
    rig.run(rig.hold(25.0, 0.0), 30);
    let start_x = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO).0.x;
    // One full sine of steering over 2 s: a lane change of about 3.5 m (0.023 rad of wheel angle).
    let period = 120;
    for i in 0..period {
        let phase = i as f32 / period as f32 * std::f32::consts::TAU;
        let input = rig.hold(25.0, 0.046 * phase.sin());
        rig.step(input);
    }
    let mut history = [Vec::new(), Vec::new()];
    for _ in 0..600 {
        let input = rig.hold(25.0, 0.0);
        rig.step(input);
        for (index, angles) in history.iter_mut().enumerate() {
            angles.push(rig.readout(index).articulation_angle);
        }
    }
    let shift = point_motion(&rig.physics, rig.car.chassis(), Vec3::ZERO).0.x - start_x;
    println!("lane change: the car moved {shift:.2} m sideways");
    assert!(shift.abs() > 2.5 && shift.abs() < 5.0, "lane change of {shift} m");
    for (index, angles) in history.iter().enumerate() {
        // The envelope: the largest |angle| in each quarter second after the lane change.
        let envelope: Vec<f32> = angles
            .chunks(15)
            .map(|window| window.iter().fold(0.0_f32, |worst, a| worst.max(a.abs())))
            .collect();
        println!(
            "trailer {index}: articulation envelope after the lane change (deg) {:?}",
            envelope.iter().map(|p| p / DEG).collect::<Vec<_>>()
        );
        let first = envelope[0];
        assert!(
            first > 0.05 * DEG,
            "trailer {index} barely moved: {} deg",
            first / DEG
        );
        assert!(first < 10.0 * DEG, "trailer {index} swung {} deg", first / DEG);
        // Peaks decay: no window rises above the one before it (beyond a noise floor of 1 % of
        // the first), and the last second is under 2 % of the first window.
        let floor = 0.01 * first;
        for pair in envelope.windows(2) {
            assert!(
                pair[1] <= pair[0].max(floor) * 1.05,
                "trailer {index}: the swing grew: {:?}",
                envelope
            );
        }
        let tail = envelope[envelope.len() - 4..]
            .iter()
            .fold(0.0_f32, |worst, a| worst.max(*a));
        assert!(
            tail <= 0.02 * first,
            "trailer {index} still swings {} deg (first {} deg)",
            tail / DEG,
            first / DEG
        );
    }
}

#[test]
fn a_tractor_and_semi_trailer_track_straight_and_turn() {
    let tractor = tractor_config();
    let wheel = &tractor.wheels[0];
    // Six wheels carrying the tractor plus about a third of the semi-trailer.
    let rest = -wheel.position.y + wheel.suspension_rest_length - 0.06 + wheel.radius;
    let mut rig = Rig::with_tow(
        tractor,
        tractor_fifth_wheel(),
        rest,
        vec![TrailerConfig::semi_trailer()],
    );
    rig.launch(15.0);
    let mut worst = 0.0_f32;
    for _ in 0..600 {
        let input = rig.hold(15.0, 0.0);
        rig.step(input);
        worst = worst.max(rig.readout(0).articulation_angle.abs());
    }
    assert!(worst < 0.5 * DEG, "straight: {} deg", worst / DEG);
    let readout = rig.readout(0);
    assert!(readout.coupling_gap < 0.03, "gap {}", readout.coupling_gap);
    assert!(
        readout.relative_pitch.abs() < 5.0 * DEG,
        "pitch {}",
        readout.relative_pitch
    );
    let semi = rig.trailers[0].vehicle().state(&rig.physics).expect("state");
    assert_eq!(semi.grounded_wheels, 6);
    // Slow to 5 m/s, then a left turn swings the semi-trailer to the tractor's right (negative).
    let mut steps = 0;
    while rig.car_speed() > 5.0 && steps < 600 {
        rig.step(VehicleInput {
            throttle: 0.0,
            brake: 0.6,
            steer: 0.0,
        });
        steps += 1;
    }
    for _ in 0..360 {
        let input = rig.hold(5.0, -0.5);
        rig.step(input);
    }
    let tractor = rig.car.state(&rig.physics).expect("state");
    assert_eq!(tractor.grounded_wheels, 6, "the tractor stays on its wheels");
    let turning = rig.readout(0);
    println!(
        "semi-trailer: left turn at 5 m/s, articulation {:.2} deg",
        turning.articulation_angle / DEG
    );
    assert!(
        turning.articulation_angle < -5.0 * DEG,
        "left turn: {} deg",
        turning.articulation_angle / DEG
    );
    assert!(turning.articulation_angle.abs() < 80.0 * DEG);
}

#[test]
fn a_lost_tow_body_uncouples_the_trailer_onto_its_stand() {
    let mut rig = Rig::new(vec![TrailerConfig::car_trailer()]);
    let car = rig.car.chassis();
    rig.physics.remove_body(car.to_handle());
    let error = rig.trailers[0]
        .update(&mut rig.physics, 0.0, DT, None)
        .expect_err("the tow is gone");
    assert_eq!(error.code(), "trailer_tow_missing");
    assert!(!rig.trailers[0].is_coupled());
    step(&mut rig.physics);
    for _ in 0..300 {
        rig.trailers[0]
            .update(&mut rig.physics, 0.0, DT, None)
            .expect("uncoupled update");
        step(&mut rig.physics);
    }
    let foot = stand_bottom(&rig.physics, &rig.trailers[0]);
    assert!(foot > -0.02 && foot < 0.05, "stand foot at {foot}");
}

#[test]
fn a_semi_trailer_on_a_dolly_behind_a_semi_trailer_makes_a_road_train() {
    let tractor = tractor_config();
    let wheel = &tractor.wheels[0];
    let rest = -wheel.position.y + wheel.suspension_rest_length - 0.06 + wheel.radius;
    let mut lead = TrailerConfig::semi_trailer();
    lead.rear_hitch = Some(Vec3::new(0.0, -1.75, 7.1));
    let mut rig = Rig::with_tow(
        tractor,
        tractor_fifth_wheel(),
        rest,
        vec![lead, TrailerConfig::dolly(), TrailerConfig::semi_trailer()],
    );
    rig.launch(12.0);
    let mut worst = [0.0_f32; 3];
    for _ in 0..600 {
        let input = rig.hold(12.0, 0.0);
        rig.step(input);
        for (index, worst) in worst.iter_mut().enumerate() {
            let readout = rig.readout(index);
            assert!(readout.coupled);
            *worst = worst.max(readout.articulation_angle.abs());
        }
    }
    println!(
        "road train straight: worst articulation (deg) {:?}",
        worst.map(|w| w / DEG)
    );
    for (index, worst) in worst.iter().enumerate() {
        assert!(*worst < 1.0 * DEG, "trailer {index}: {} deg", worst / DEG);
        assert!(rig.readout(index).coupling_gap < 0.05);
    }
}

// ---------------------------------------------------------------------------
// Determinism
// ---------------------------------------------------------------------------

/// A scripted car-and-trailer run: settle, accelerate, turn, brake. Returns the world digest and
/// the trailer digest after it.
fn scripted_run() -> (u64, u64) {
    let mut rig = Rig::new(vec![TrailerConfig::car_trailer()]);
    for i in 0..600_usize {
        let input = VehicleInput {
            throttle: if i < 300 { 0.8 } else { 0.0 },
            brake: if i >= 450 { 0.7 } else { 0.0 },
            steer: if (200..350).contains(&i) { 0.5 } else { 0.0 },
        };
        rig.step(input);
    }
    (rig.physics.state_digest(), rig.trailers[0].state_digest())
}

#[test]
fn the_same_inputs_give_a_bit_identical_trailer_run() {
    let a = scripted_run();
    let b = scripted_run();
    assert_eq!(a, b);
    println!(
        "trailer determinism pin: world {:#018x}, trailer {:#018x}",
        a.0, a.1
    );
    // Pinned on Linux x86_64 (rapier's floats go through the platform's libm); other targets only
    // check the two runs agree.
    #[cfg(all(
        target_os = "linux",
        target_arch = "x86_64",
        not(feature = "physics-cross-platform-determinism")
    ))]
    assert_eq!(
        a,
        (0x342e_272e_5e48_8369, 0x4c2a_0511_3e97_1b4f),
        "the trailer pin moved"
    );
}

// ---------------------------------------------------------------------------
// ECS
// ---------------------------------------------------------------------------

fn trailer_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin, TrailerPlugin));
    {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        ground_box(&mut physics, 500.0);
    }
    app
}

#[test]
fn the_plugin_couples_brakes_and_reports_a_train() {
    let mut app = trailer_app();
    let car = spawn_vehicle(
        app.world_mut(),
        VehicleConfig::default(),
        Vec3::new(0.0, car_rest_height(), 0.0),
        Quat::IDENTITY,
    )
    .expect("car");
    app.world_mut().entity_mut(car).insert(TowHitch {
        point: car_tow_hitch(),
    });
    let mut front_config = TrailerConfig::car_trailer();
    front_config.rear_hitch = Some(Vec3::new(0.0, 0.0, 1.5));
    let front_at = car_tow_hitch() - front_config.coupling.point + Vec3::new(0.0, car_rest_height(), 0.0);
    let front =
        spawn_trailer(app.world_mut(), front_config.clone(), front_at, Quat::IDENTITY).expect("front");
    assert!(
        app.world().get::<TowHitch>(front).is_some(),
        "a rear hitch makes a TowHitch"
    );
    let back_at = front_at + Vec3::new(0.0, 0.0, 1.5) - front_config.coupling.point;
    let back = spawn_trailer(
        app.world_mut(),
        TrailerConfig::car_trailer(),
        back_at,
        Quat::IDENTITY,
    )
    .expect("back");

    let code = |result: Result<(), TrailerError>| result.map_err(|e| e.code());
    assert_eq!(
        code(couple_trailer(app.world_mut(), car, front)),
        Err("trailer_not_a_trailer")
    );
    assert_eq!(
        code(couple_trailer(app.world_mut(), front, front)),
        Err("trailer_self_coupling")
    );
    assert_eq!(
        code(couple_trailer(app.world_mut(), front, back)),
        Err("trailer_no_hitch")
    );
    assert_eq!(
        code(uncouple_trailer(app.world_mut(), front)),
        Err("trailer_not_coupled")
    );
    assert_eq!(code(couple_trailer(app.world_mut(), front, car)), Ok(()));
    assert_eq!(
        code(couple_trailer(app.world_mut(), back, car)),
        Err("trailer_hitch_occupied")
    );
    assert_eq!(code(couple_trailer(app.world_mut(), back, front)), Ok(()));

    for _ in 0..120 {
        app.update();
    }
    let state = *app.world().get::<TrailerState>(back).expect("state");
    assert!(state.readout.coupled);
    assert_eq!(state.tow, Some(front));
    assert_eq!(state.root, Some(car));
    assert_eq!(state.train_position, 2);
    assert_eq!(
        app.world()
            .get::<TrailerState>(front)
            .expect("state")
            .train_position,
        1
    );
    assert_eq!(
        app.world()
            .get::<TrailerController>(back)
            .expect("controller")
            .last_error,
        None
    );

    // The car's brake reaches the back trailer after the delay.
    app.world_mut().get_mut::<VehicleInput>(car).expect("input").brake = 1.0;
    for _ in 0..20 {
        app.update();
    }
    let braking = app
        .world()
        .get::<TrailerState>(back)
        .expect("state")
        .readout
        .brake;
    assert!((braking - 0.8).abs() < 1.0e-6, "brake {braking}");

    // Uncoupling the front trailer leaves the back one on it.
    assert_eq!(code(uncouple_trailer(app.world_mut(), front)), Ok(()));
    app.update();
    let front_state = *app.world().get::<TrailerState>(front).expect("state");
    assert!(!front_state.readout.coupled);
    assert_eq!(front_state.train_position, 0);
    assert_eq!(front_state.readout.brake, 1.0, "parking brake");
    let back_state = *app.world().get::<TrailerState>(back).expect("state");
    assert_eq!(
        back_state.root,
        Some(front),
        "the uncoupled trailer heads the rest"
    );
    // A loop is refused.
    assert_eq!(
        code(couple_trailer(app.world_mut(), front, back)),
        Err("trailer_no_hitch")
    );
    app.world_mut().entity_mut(back).insert(TowHitch {
        point: Vec3::new(0.0, 0.0, 1.5),
    });
    assert_eq!(
        code(couple_trailer(app.world_mut(), front, back)),
        Err("trailer_chain_cycle")
    );

    // Despawning the tow uncouples on the next step.
    app.world_mut().despawn(front);
    for _ in 0..3 {
        app.update();
    }
    let controller = app.world().get::<TrailerController>(back).expect("controller");
    assert!(!controller.trailer.is_coupled());
    assert_eq!(controller.tow, None);
}
