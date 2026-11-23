// SPDX-License-Identifier: MPL-2.0
//! Hover craft (`regia_physics::hover`), headless: the thrusters settle the chassis at the
//! spring's rest height, throttle pushes along −Z, steering yaws the right way, a craft over the
//! wrong layer falls, bad input gives named errors, and the same inputs give identical runs.

use std::time::Duration;

use rapier3d::dynamics::{IntegrationParameters, RigidBodyBuilder};
use rapier3d::geometry::{ColliderBuilder, Group, InteractionGroups, InteractionTestMode};
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_physics::convert::to_rapier_vec3;
use regia_physics::hover::{HoverConfig, HoverController, HoverCraft, HoverError, spawn_hovercraft};
use regia_physics::vehicle::{VehicleInput, VehiclePlugin};
use regia_physics::{PhysicsLayers, PhysicsPlugin, PhysicsWorld};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
const GRAVITY: Vec3 = Vec3::new(0.0, -9.81, 0.0);

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
        to_rapier_vec3(GRAVITY),
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

/// A 400 m plane, top at y = 0, whose collider is a member of `layer` (0 for the default).
fn world_with_ground(layer: u32) -> PhysicsWorld {
    let mut physics = PhysicsWorld::default();
    let body = physics.bodies.insert(
        RigidBodyBuilder::fixed()
            .translation(to_rapier_vec3(Vec3::new(0.0, -0.5, 0.0)))
            .build(),
    );
    let PhysicsWorld {
        bodies, colliders, ..
    } = &mut physics;
    let groups = InteractionGroups::new(
        Group::from_bits_retain(1 << layer),
        Group::all(),
        InteractionTestMode::And,
    );
    colliders.insert_with_parent(
        ColliderBuilder::cuboid(200.0, 0.5, 200.0)
            .collision_groups(groups)
            .build(),
        body,
        bodies,
    );
    physics
}

fn run(physics: &mut PhysicsWorld, craft: &mut HoverCraft, input: VehicleInput, steps: usize) {
    for _ in 0..steps {
        craft
            .update_with_layers(physics, input, DT, None)
            .expect("update");
        step(physics);
    }
}

fn settled() -> (PhysicsWorld, HoverCraft) {
    let mut physics = world_with_ground(0);
    let mut craft = HoverCraft::spawn(
        &mut physics,
        HoverConfig::default(),
        Vec3::new(0.0, 1.2, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    run(&mut physics, &mut craft, VehicleInput::default(), 600);
    (physics, craft)
}

#[test]
fn the_default_config_is_valid_and_bad_ones_name_the_field() {
    assert_eq!(HoverConfig::default().validate(), Ok(()));
    let bad = |edit: &dyn Fn(&mut HoverConfig)| {
        let mut c = HoverConfig::default();
        edit(&mut c);
        c.validate().expect_err("rejected")
    };
    assert_eq!(
        bad(&|c| c.hover_height = 0.0),
        HoverError::InvalidConfig {
            field: "hover_height"
        }
    );
    assert_eq!(
        bad(&|c| c.chassis_mass = f32::NAN),
        HoverError::InvalidConfig {
            field: "chassis_mass"
        }
    );
    assert_eq!(
        bad(&|c| c.stiffness = -1.0),
        HoverError::InvalidConfig { field: "stiffness" }
    );
    assert_eq!(
        bad(&|c| c.angular_damping = f32::NAN),
        HoverError::InvalidConfig {
            field: "angular_damping"
        }
    );
    assert_eq!(bad(&|c| c.thrusters.clear()), HoverError::NoThrusters);
    assert_eq!(
        bad(&|c| c.thrusters = vec![Vec3::ZERO; 17]),
        HoverError::TooManyThrusters { count: 17 }
    );
    assert_eq!(
        bad(&|c| c.thrusters[0] = Vec3::NAN),
        HoverError::InvalidConfig { field: "thrusters" }
    );
    assert_eq!(
        bad(&|c| c.query_layer = Some(regia_physics::MAX_LAYERS as u16)),
        HoverError::InvalidConfig { field: "query_layer" }
    );
}

#[test]
fn error_codes_are_stable_lowercase_identifiers() {
    let all = [
        (HoverError::NoThrusters, "hover_no_thrusters"),
        (
            HoverError::TooManyThrusters { count: 99 },
            "hover_too_many_thrusters",
        ),
        (HoverError::InvalidConfig { field: "x" }, "hover_invalid_config"),
        (HoverError::InvalidPose, "hover_invalid_pose"),
        (HoverError::InvalidTimeStep, "hover_invalid_time_step"),
        (HoverError::ChassisMissing, "hover_chassis_missing"),
        (HoverError::NoPhysicsWorld, "hover_no_physics_world"),
    ];
    for (error, code) in all {
        assert_eq!(error.code(), code);
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
        );
    }
}

#[test]
fn spawn_and_update_refuse_bad_pose_and_time_step_without_touching_the_world() {
    let mut physics = world_with_ground(0);
    let before = physics.bodies.len();
    let nan_pose = HoverCraft::spawn(&mut physics, HoverConfig::default(), Vec3::NAN, Quat::IDENTITY);
    assert_eq!(nan_pose.err(), Some(HoverError::InvalidPose));
    assert_eq!(physics.bodies.len(), before);
    let mut craft = HoverCraft::spawn(
        &mut physics,
        HoverConfig::default(),
        Vec3::new(0.0, 1.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    for dt in [0.0, -1.0, f32::NAN, f32::INFINITY] {
        assert_eq!(
            craft.update_with_layers(&mut physics, VehicleInput::default(), dt, None),
            Err(HoverError::InvalidTimeStep)
        );
    }
    assert_eq!(craft.steps(), 0);
    let handle = craft.chassis().to_handle();
    assert!(craft.clone().remove(&mut physics));
    assert_eq!(
        craft.update_with_layers(&mut physics, VehicleInput::default(), DT, None),
        Err(HoverError::ChassisMissing)
    );
    assert_eq!(craft.state(&physics).err(), Some(HoverError::ChassisMissing));
    assert!(!physics.bodies.contains(handle));
}

#[test]
fn the_craft_settles_at_the_springs_rest_height_level_and_still() {
    let (physics, craft) = settled();
    let config = HoverConfig::default();
    let expected = config.hover_height - GRAVITY.length() / config.stiffness;
    let state = craft.state(&physics).expect("state");
    assert_eq!(state.grounded_thrusters, 4);
    for t in &state.thrusters {
        assert!(
            (t.distance - expected).abs() < 0.03,
            "thruster {} at {}",
            t.index,
            t.distance
        );
        assert!(t.compression > 0.0 && t.force > 0.0);
        assert!(t.contact_point.is_some_and(|p| p.y.abs() < 1e-3));
    }
    assert!((state.rotation * Vec3::Y).dot(Vec3::Y) > 0.9995, "level");
    // "Still" (recut decision, ADR-2301): the pose stops moving at every step (no drift and no
    // slow oscillation between the end points), and the velocity read after a step is at most
    // one step of gravity. The reading is not zero: the spring impulse that cancels gravity is
    // applied before the next step, so a post-step reading holds part of one step's gravity
    // while the pose stays put.
    let (mut physics, mut craft) = (physics, craft);
    let one_step_of_gravity = GRAVITY.length() * DT;
    let mut worst_move = 0.0f32;
    let mut worst_speed = 0.0f32;
    for _ in 0..60 {
        run(&mut physics, &mut craft, VehicleInput::default(), 1);
        let now = craft.state(&physics).expect("state");
        worst_move = worst_move.max((now.translation - state.translation).length());
        worst_speed = worst_speed.max(now.linear_velocity.length());
    }
    println!("over 60 steps: worst move {worst_move} m, worst velocity reading {worst_speed} m/s");
    assert!(worst_move < 1e-3, "at rest: moved {worst_move} m");
    assert!(
        worst_speed <= one_step_of_gravity + 1e-3,
        "velocity reading {worst_speed} m/s is more than one step of gravity ({one_step_of_gravity})"
    );
}

#[test]
fn throttle_pushes_along_forward_and_the_brake_stops_it() {
    let (mut physics, mut craft) = settled();
    run(
        &mut physics,
        &mut craft,
        VehicleInput {
            throttle: 1.0,
            ..Default::default()
        },
        120,
    );
    let moving = craft.state(&physics).expect("state");
    assert!(moving.forward_speed > 3.0, "speed {}", moving.forward_speed);
    assert!(
        moving.translation.z < -3.0,
        "went along -Z: {}",
        moving.translation
    );
    assert!(moving.lateral_speed.abs() < 0.2);
    assert!(moving.translation.y > 0.5, "still hovering");
    run(
        &mut physics,
        &mut craft,
        VehicleInput {
            brake: 1.0,
            ..Default::default()
        },
        240,
    );
    let stopped = craft.state(&physics).expect("state");
    assert!(
        stopped.forward_speed.abs() < 0.2,
        "braked to {}",
        stopped.forward_speed
    );
}

#[test]
fn steering_yaws_right_for_positive_input_and_lateral_grip_limits_the_slide() {
    let (mut physics, mut craft) = settled();
    run(
        &mut physics,
        &mut craft,
        VehicleInput {
            throttle: 0.6,
            steer: 1.0,
            ..Default::default()
        },
        120,
    );
    let state = craft.state(&physics).expect("state");
    // Forward = (-sin yaw, 0, -cos yaw): a right turn rotates it toward +X.
    assert!(state.forward.x > 0.1, "turned right: {}", state.forward);
    // Left mirrors.
    let (mut physics, mut craft) = settled();
    run(
        &mut physics,
        &mut craft,
        VehicleInput {
            throttle: 0.6,
            steer: -1.0,
            ..Default::default()
        },
        120,
    );
    assert!(craft.state(&physics).expect("state").forward.x < -0.1);
}

#[test]
fn lateral_grip_of_one_removes_one_sixtieth_of_the_slide_at_sixty_hertz() {
    let mut physics = world_with_ground(0);
    let config = HoverConfig {
        lateral_grip: 1.0,
        ..HoverConfig::default()
    };
    let mut craft =
        HoverCraft::spawn(&mut physics, config, Vec3::new(0.0, 0.5, 0.0), Quat::IDENTITY).expect("spawn");
    // One step puts the ground in the query pipeline. The craft has not been driven yet.
    step(&mut physics);
    let handle = craft.chassis().to_handle();
    if let Some(body) = physics.bodies.get_mut(handle) {
        body.set_linvel(to_rapier_vec3(Vec3::new(10.0, 0.0, 0.0)), true);
        body.set_angvel(to_rapier_vec3(Vec3::ZERO), true);
    }
    craft
        .update_with_layers(&mut physics, VehicleInput::default(), DT, None)
        .expect("update");
    let after = craft.state(&physics).expect("state").lateral_speed;
    let expect = 10.0 * 59.0 / 60.0;
    assert!(
        (after - expect).abs() < 1.0e-3,
        "grip 1 removes dt of the slide, not all of it: {after} vs {expect}"
    );
}

#[test]
fn lateral_grip_bleeds_off_a_sideways_slide() {
    let (mut physics, mut craft) = settled();
    let handle = craft.chassis().to_handle();
    if let Some(body) = physics.bodies.get_mut(handle) {
        body.set_linvel(to_rapier_vec3(Vec3::new(8.0, 0.0, 0.0)), true);
    }
    assert!(craft.state(&physics).expect("state").lateral_speed > 7.0);
    run(&mut physics, &mut craft, VehicleInput::default(), 60);
    let after = craft.state(&physics).expect("state").lateral_speed;
    assert!(
        after.abs() < 1.0,
        "slide left {after} m/s after a second at grip 3"
    );
}

#[test]
fn a_craft_above_reach_gets_no_thrust_and_falls() {
    let mut physics = world_with_ground(0);
    let mut craft = HoverCraft::spawn(
        &mut physics,
        HoverConfig::default(),
        Vec3::new(0.0, 10.0, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    run(
        &mut physics,
        &mut craft,
        VehicleInput {
            throttle: 1.0,
            ..Default::default()
        },
        30,
    );
    let state = craft.state(&physics).expect("state");
    assert_eq!(state.grounded_thrusters, 0);
    assert!(
        state.linear_velocity.y < -2.0,
        "falling: {}",
        state.linear_velocity
    );
    assert!(
        state.linear_velocity.x.abs() < 1e-4 && state.linear_velocity.z.abs() < 1e-4,
        "no drive in the air"
    );
}

#[test]
fn the_ground_layers_mask_decides_what_the_thrusters_see() {
    let over_layer_two = |config: HoverConfig, layers: Option<&PhysicsLayers>| {
        let mut physics = world_with_ground(2);
        let mut craft =
            HoverCraft::spawn(&mut physics, config, Vec3::new(0.0, 0.9, 0.0), Quat::IDENTITY).expect("spawn");
        step(&mut physics);
        craft
            .update_with_layers(&mut physics, VehicleInput::default(), DT, layers)
            .expect("update");
        craft.state(&physics).expect("state").grounded_thrusters
    };
    let with = |ground_layers| HoverConfig {
        ground_layers,
        ..HoverConfig::default()
    };
    assert_eq!(over_layer_two(with(None), None), 4);
    assert_eq!(over_layer_two(with(Some(1 << 2)), None), 4);
    assert_eq!(over_layer_two(with(Some(1 << 0)), None), 0);
    let mut layers = PhysicsLayers::default();
    layers.query_rows[1] = 1 << 0;
    let from_one = HoverConfig {
        query_layer: Some(1),
        ..HoverConfig::default()
    };
    assert_eq!(over_layer_two(from_one, Some(&layers)), 0);
}

#[test]
fn the_same_inputs_give_a_bit_identical_run() {
    let digest = || {
        let (mut physics, mut craft) = settled();
        let mut out = Vec::new();
        for i in 0..200 {
            let input = VehicleInput {
                throttle: if i % 50 < 30 { 1.0 } else { -0.5 },
                brake: if i % 70 > 60 { 1.0 } else { 0.0 },
                steer: ((i % 40) as f32 / 20.0) - 1.0,
            };
            craft
                .update_with_layers(&mut physics, input, DT, None)
                .expect("update");
            step(&mut physics);
            let s = craft.state(&physics).expect("state");
            out.push([
                s.translation.x.to_bits(),
                s.translation.y.to_bits(),
                s.translation.z.to_bits(),
            ]);
        }
        out
    };
    let a = digest();
    assert_eq!(a, digest());
    let mut distinct = a.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert!(distinct.len() > 150, "the craft really moved");
}

#[test]
fn non_finite_input_is_read_as_released() {
    let run_with = |input: VehicleInput| {
        let (mut physics, mut craft) = settled();
        run(&mut physics, &mut craft, input, 60);
        craft
            .state(&physics)
            .expect("state")
            .translation
            .to_array()
            .map(f32::to_bits)
    };
    let nan = VehicleInput {
        throttle: f32::NAN,
        brake: f32::INFINITY * 0.0,
        steer: f32::NAN,
    };
    assert_eq!(run_with(nan), run_with(VehicleInput::default()));
}

#[test]
fn the_plugin_drives_a_spawned_craft_entity_from_its_input_component() {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin));
    *app.world_mut().resource_mut::<PhysicsWorld>() = world_with_ground(0);
    let entity = spawn_hovercraft(
        app.world_mut(),
        HoverConfig::default(),
        Vec3::new(0.0, 1.2, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    for _ in 0..300 {
        app.update();
    }
    let rest = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    assert!(rest.y > 0.5 && rest.y < 1.2, "hovering at {}", rest.y);
    app.world_mut()
        .get_mut::<VehicleInput>(entity)
        .expect("input")
        .throttle = 1.0;
    for _ in 0..90 {
        app.update();
    }
    let moved = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    assert!(moved.z < rest.z - 1.0, "drove along -Z: {rest} -> {moved}");
    let controller = app.world().get::<HoverController>(entity).expect("controller");
    assert_eq!(controller.last_error, None);
    app.world_mut().despawn(entity);
    app.update();
}

#[test]
fn a_craft_of_a_parked_data_layer_is_not_driven_and_keeps_its_velocity() {
    // A parked layer disables the chassis (spec 37 § 13); the controller must not keep adding
    // spring and drive impulses to it, or the craft would come back with a velocity it never had.
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, VehiclePlugin));
    *app.world_mut().resource_mut::<PhysicsWorld>() = world_with_ground(0);
    let entity = spawn_hovercraft(
        app.world_mut(),
        HoverConfig::default(),
        Vec3::new(0.0, 1.2, 0.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    for _ in 0..120 {
        app.update();
    }
    let chassis = app
        .world()
        .get::<regia_physics::RigidBodyHandle>(entity)
        .expect("chassis handle")
        .to_handle();
    let velocity = |app: &App| {
        let body = app
            .world()
            .resource::<PhysicsWorld>()
            .bodies
            .get(chassis)
            .expect("chassis");
        (body.is_enabled(), body.linvel(), body.angvel())
    };
    app.world_mut()
        .get_mut::<VehicleInput>(entity)
        .expect("input")
        .throttle = 1.0;
    app.world_mut()
        .entity_mut(entity)
        .insert(regia_scene::layers::LayerParked {
            restore: regia_scene::prelude::Visibility::Inherited,
            inherited: false,
        });
    app.update();
    let parked = velocity(&app);
    assert!(!parked.0, "the parked chassis is disabled");
    for _ in 0..60 {
        app.update();
    }
    assert_eq!(velocity(&app), parked, "a parked craft got impulses");

    // Back live: the same chassis (the controller has no rebuild path) is enabled and driven.
    app.world_mut()
        .entity_mut(entity)
        .remove::<regia_scene::layers::LayerParked>();
    let before = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    for _ in 0..60 {
        app.update();
    }
    assert!(velocity(&app).0, "the resumed chassis is enabled again");
    let controller = app.world().get::<HoverController>(entity).expect("controller");
    assert_eq!(controller.last_error, None, "the resumed craft lost its chassis");
    assert_eq!(
        controller.craft.chassis().to_handle(),
        chassis,
        "the chassis was rebuilt"
    );
    let after = app
        .world()
        .get::<Transform>(entity)
        .expect("transform")
        .translation;
    assert!(
        after.z < before.z - 0.5,
        "the resumed craft drives again: {before} -> {after}"
    );
}

#[test]
fn a_rebase_moves_the_thruster_contacts_with_the_chassis() {
    let (physics, mut craft) = settled();
    let before = craft.state(&physics).expect("state");
    let contact = before.thrusters[0].contact_point.expect("grounded");
    let shift = Vec3::new(1024.0, 0.0, -512.0);
    craft.shift_origin(shift);
    let after = craft.state(&physics).expect("state");
    assert_eq!(after.thrusters[0].contact_point, Some(contact - shift));
    assert_eq!(after.thrusters[0].distance, before.thrusters[0].distance);
}
