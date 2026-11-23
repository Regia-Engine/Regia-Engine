// SPDX-License-Identifier: MPL-2.0
//! Two-way rigid-body coupling through `regia_physics` (feature `physics`, ADR-2577): the physics
//! step and the fluid step run in one app, the liquid's pressure impulses reach the body as
//! `ExternalImpulse`s. Fixed steps on a manual clock, no wall clock, no goldens.
#![cfg(feature = "physics")]

use std::time::Duration;

use glam::Vec3;
use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_ecs::prelude::Entity;
use regia_fluid::coupling::{FluidBody, FluidCoupling, FluidPhysicsPlugin};
use regia_fluid::sim::{Emitter, EmitterKind, FluidParams, FluidSim, Shape};
use regia_fluid::{FluidPlugin, FluidWorld, SolidMotion};
use regia_physics::{
    Collider, ColliderShape, LockedAxes, Mass, PhysicsPlugin, PhysicsPose, RigidBody, Velocity,
};
use regia_scene::Transform;

/// Tank: 0.6 m wide, 0.9 m tall, 3 cm deep (a slab: the box spans it wall to wall), 1 cm cells.
const SIZE: Vec3 = Vec3::new(0.6, 0.9, 0.03);
const DX: f32 = 0.01;
/// Box: 0.16 m wide, 0.6 m tall, spanning the slab. At 800 kg/m³ its Archimedes depth is 0.48 m,
/// 48 cells: the first-order free surface (ADR-2441, `p = 0` at air-cell centres) can bias the
/// pressure by up to about two cells of depth, so the depth must be large in cells for 5 %.
const HALF_W: f32 = 0.08;
const HALF_H: f32 = 0.3;
/// Initial fill height and the box's initial bottom (2 cm above its equilibrium, so it settles).
const FILL: f32 = 0.60;
const START_BOTTOM: f32 = 0.14;

fn app_with_fluid() -> (App, u32) {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins(PhysicsPlugin)
        .add_plugins(FluidPlugin)
        .add_plugins(FluidPhysicsPlugin);
    let mut params = FluidParams {
        bounds_min: Vec3::ZERO,
        bounds_max: SIZE,
        cell_size: DX,
        dt: 1.0 / 60.0,
        // The mobile cap: keeps the whole suite runnable on Android aarch64 (the digest pins
        // must be re-checked there). The fills stay far below it.
        particle_cap: 65_536,
        ..FluidParams::default()
    };
    params.whitewater.enabled = false;
    let mut sim = FluidSim::new(params).expect("tank");
    sim.add_emitter(Emitter {
        shape: Shape::Box {
            center: Vec3::new(SIZE.x * 0.5, FILL * 0.5, SIZE.z * 0.5),
            half_extents: Vec3::new(SIZE.x * 0.5, FILL * 0.5, SIZE.z * 0.5),
        },
        velocity: Vec3::ZERO,
        kind: EmitterKind::Fill,
        done: false,
    })
    .expect("fill");
    let mut fluids = FluidWorld::default();
    let id = fluids.insert(sim, None).expect("insert");
    fluids.get_mut(id).expect("entry").running = true;
    app.insert_resource(fluids);
    // A ground plate under the tank (top at y = 0) for the box that sinks.
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.3, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(2.0, 0.5, 2.0),
            },
            ..Collider::default()
        },
    ));
    (app, id)
}

/// A dynamic box of `density` (kg/m³ over its slab volume), upright (rotation and sideways motion
/// locked), coupled two-way.
fn spawn_box(app: &mut App, fluid: u32, density: f32) -> (Entity, f32) {
    let mass = density * (2.0 * HALF_W) * (2.0 * HALF_H) * SIZE.z;
    let body = RigidBody {
        linear_damping: 2.0,
        can_sleep: false,
        locked_axes: LockedAxes {
            translation_x: true,
            translation_z: true,
            rotation_x: true,
            rotation_y: true,
            rotation_z: true,
            ..LockedAxes::FREE
        },
        ..RigidBody::dynamic()
    };
    let entity = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.3, START_BOTTOM + HALF_H, SIZE.z * 0.5)),
            body,
            Collider {
                shape: ColliderShape::Box {
                    // Wider than the slab in z, so the liquid cannot pass beside it.
                    half_extents: Vec3::new(HALF_W, HALF_H, 0.1),
                },
                friction: 0.0,
                restitution: 0.0,
                ..Collider::default()
            },
            Mass::Explicit {
                mass,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(mass * 0.05),
            },
            Velocity::default(),
            FluidBody::new(fluid),
        ))
        .id();
    (entity, mass)
}

fn bottom_of(app: &App, body: Entity) -> f32 {
    let world = app.world();
    let y = world
        .get::<PhysicsPose>(body)
        .map(|p| p.translation.y)
        .or_else(|| world.get::<Transform>(body).map(|t| t.translation.y))
        .unwrap_or(f32::NAN);
    y - HALF_H
}

/// Mean free-surface height of the columns at least 4 cm from the box.
fn free_surface(app: &App, fluid: u32) -> f32 {
    let fluids = app.world().resource::<FluidWorld>();
    let sim = &fluids.get(fluid).expect("fluid").sim;
    let (skip_lo, skip_hi) = (0.3 - HALF_W - 0.04, 0.3 + HALF_W + 0.04);
    let d = *sim.dims();
    // The free surface of a column: its top particle plus a quarter cell (the 2 x 2 x 2 seeding puts
    // the top particle layer a quarter cell below the liquid's top). A particle count per cell
    // under-reads it, because surface cells settle with fewer than eight particles.
    let mut top = vec![f32::NEG_INFINITY; d.nx * d.nz];
    for p in sim.positions() {
        let Some((i, _, k)) = d.cell_of(*p) else {
            continue;
        };
        let t = &mut top[i + d.nx * k];
        *t = t.max(p.y);
    }
    let mut sum = 0.0;
    let mut n = 0;
    for k in 0..d.nz {
        for i in 0..d.nx {
            let x = d.origin.x + (i as f32 + 0.5) * d.dx;
            let t = top[i + d.nx * k];
            if (x > skip_lo && x < skip_hi) || !t.is_finite() {
                continue;
            }
            sum += t + 0.25 * d.dx;
            n += 1;
        }
    }
    sum / n.max(1) as f32
}

/// A box of half the liquid's density settles at the Archimedes depth `m / (rho A)` within 5 %:
/// the coupling's pressure impulses (applied as `ExternalImpulse`) carry its weight.
#[test]
fn floating_box_settles_at_archimedes_depth() {
    let (mut app, fluid) = app_with_fluid();
    let (body, mass) = spawn_box(&mut app, fluid, 800.0);
    let area = 2.0 * HALF_W * SIZE.z;
    let expected = mass / (1000.0 * area);
    let steps: u32 = 240;
    let window = 60;
    let (mut depth_sum, mut n) = (0.0f64, 0);
    let mut lowest = f32::MAX;
    for step in 0..steps {
        app.update();
        let bottom = bottom_of(&app, body);
        lowest = lowest.min(bottom);
        if step.is_multiple_of(20) {
            let coupling = app.world().resource::<FluidCoupling>();
            let b = coupling.bodies.iter().find(|b| b.entity == body);
            let v = app.world().get::<Velocity>(body).map(|v| v.linear);
            eprintln!(
                "float step {step}: bottom {bottom:.4} surface {:.4} impulse {:?} velocity {v:?}",
                free_surface(&app, fluid),
                b.map(|b| b.impulse)
            );
        }
        if step >= steps - window {
            depth_sum += f64::from(free_surface(&app, fluid) - bottom);
            n += 1;
        }
    }
    let depth = (depth_sum / f64::from(n)) as f32;
    let coupling = app.world().resource::<FluidCoupling>();
    let state = coupling
        .bodies
        .iter()
        .find(|b| b.entity == body)
        .expect("coupled");
    let error = (depth - expected).abs() / expected;
    eprintln!(
        "floating box: depth {depth:.4} m vs Archimedes {expected:.4} m ({:.2} %), bottom {:.4}, lowest {lowest:.4}, \
         last impulse {} N s (weight per step {:.4}), wetted faces {}",
        error * 100.0,
        bottom_of(&app, body),
        state.impulse,
        mass * 9.81 / 60.0,
        state.wetted_faces
    );
    assert_eq!(state.motion, SolidMotion::Dynamic);
    assert!(state.applied, "the impulse reached the body");
    assert!(state.wetted_faces > 0);
    assert!(error < 0.05, "depth {depth} vs Archimedes {expected}");
    // It floats: the bottom stays well above the tank floor.
    assert!(lowest > 0.05, "lowest bottom {lowest}");
}

/// A box of twice the liquid's density sinks to the ground, slower than in free fall (the
/// buoyancy pushes back), and the liquid's impulses on it point up.
#[test]
fn heavier_box_sinks() {
    let (mut app, fluid) = app_with_fluid();
    let (body, _) = spawn_box(&mut app, fluid, 2000.0);
    let mut up_impulse = 0.0f32;
    let start = bottom_of(&app, body);
    for _ in 0..240 {
        app.update();
        let coupling = app.world().resource::<FluidCoupling>();
        if let Some(b) = coupling.bodies.iter().find(|b| b.entity == body) {
            up_impulse += b.impulse.y;
        }
    }
    let end = bottom_of(&app, body);
    eprintln!("sinking box: bottom {start:.4} -> {end:.4}, summed up impulse {up_impulse:.4} N s");
    assert!(end < 0.03, "the heavy box reached the floor: bottom {end}");
    assert!(up_impulse > 0.0, "buoyancy pushed up: {up_impulse}");
}

/// A kinematic body moving through the liquid is a one-way solid: listed in the coupling state,
/// never given impulses, and its velocity comes from its `Transform` change.
#[test]
fn kinematic_body_is_one_way() {
    let (mut app, fluid) = app_with_fluid();
    let paddle = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.15, 0.3, SIZE.z * 0.5)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                ..Collider::default()
            },
            FluidBody::new(fluid),
        ))
        .id();
    for step in 0..20 {
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(paddle) {
            t.translation.x = 0.15 + 0.01 * step as f32;
        }
        app.update();
    }
    let coupling = app.world().resource::<FluidCoupling>();
    let state = coupling
        .bodies
        .iter()
        .find(|b| b.entity == paddle)
        .expect("listed");
    assert_eq!(state.motion, SolidMotion::Kinematic);
    assert!(!state.applied);
    let solid = state.solid.expect("sphere is supported");
    assert!(
        (solid.linear_velocity.x - 0.6).abs() < 1e-3,
        "{}",
        solid.linear_velocity
    );
    assert!(
        app.world()
            .get::<regia_physics::ExternalImpulse>(paddle)
            .is_none()
    );
}

/// (ADR-4753) The coupling feeds no platform trig into the solid list: the capsule axis turn is a
/// bit constant and the kinematic spin derives from the software log map, so both runs hash the
/// same on every platform. Pins captured on the reference host (see the lane report).
#[test]
fn capsule_axis_and_kinematic_spin_digest_bits_are_stable() {
    use glam::Quat;
    use regia_physics::ShapeAxis;

    // Part 1: an X-axis capsule FluidBody at rest. Its rest orientation is the axis-turn
    // quaternion; one platform ulp of `sin_cos` would move the SDF and fork the digest.
    let (mut app, fluid) = app_with_fluid();
    let capsule = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.3, 0.3, SIZE.z * 0.5)),
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Capsule {
                half_height: 0.1,
                radius: 0.05,
                axis: ShapeAxis::X,
            },
            ..Collider::default()
        },
        FluidBody::new(fluid),
    ));
    let capsule = capsule.id();
    for _ in 0..5 {
        app.update();
    }
    let capsule_digest = {
        let fluids = app.world().resource::<FluidWorld>();
        fluids.get(fluid).expect("fluid").sim.state_digest()
    };
    // The digest hashes each moving solid's tag, its body's `Entity::to_bits()`, and bevy keeps
    // every resource on an entity of its own: a resource added by `App::new` or a plugin moves the
    // body's index and the pin (ADR-5288). Printed so a moved pin shows whether the index moved.
    eprintln!(
        "x-axis capsule at rest: entity bits {:#x}, digest {capsule_digest:#018x}",
        capsule.to_bits()
    );

    // Part 2: a kinematic box given fixed yawed transforms a step apart. The spin between the
    // two poses goes through the software atan2; a platform ulp of `atan2` would fork the
    // angular velocity and everything the paddle stirs.
    let (mut app, fluid) = app_with_fluid();
    let paddle = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.3, 0.15, SIZE.z * 0.5)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.03, 0.08, 0.1),
                },
                ..Collider::default()
            },
            FluidBody::new(fluid),
        ))
        .id();
    // A half-degree yaw per fixed step, as literal bits (no platform trig in the test either).
    // 0x3c0ef9be = sin(0.5 deg), 0x3f7ffd81 = cos(0.5 deg), each the only f32 near the value.
    let yaw = Quat::from_xyzw(0.0, f32::from_bits(0x3c0e_f9be), 0.0, f32::from_bits(0x3f7f_fd81));
    for _ in 0..6 {
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(paddle) {
            // The product of unit quats is unit to an ulp; the coupling re-normalizes with the
            // software sqrt, so no platform float op may hide in the test either.
            t.rotation *= yaw;
        }
        app.update();
    }
    let digest = {
        let fluids = app.world().resource::<FluidWorld>();
        fluids.get(fluid).expect("fluid").sim.state_digest()
    };
    eprintln!(
        "yawing kinematic box: entity bits {:#x}, digest {digest:#018x}",
        paddle.to_bits()
    );
    // The constants checked in at v045b (0x414c_4d77_35e5_1bf8 and 0xacac_b936_c759_acea,
    // commit 751051832) do not reproduce with that solver. Phase 1 is those algorithms
    // plus counters. On the workstation x86_64 it hashes to 0xa2cbe5c46ff1750b and
    // 0x1d5b2bca15142c6d. This solver with `SEPARATE_CELL_CAP` lifted to 1_000_000
    // hashes to the same pair, so scratch reuse and the distance early-out are not
    // the move. The cap of 12 is: capsule 0xa2cbe5c46ff1750b -> 0xf2bd4a664f51644c,
    // yaw 0x1d5b2bca15142c6d -> 0x3b1085b3dec3e150. The lead owns both pins.
    // Both pins are asserted after both runs, so one run prints both digests.
    // integ-v047: fluid-perf's cell cap (12) and audfix-fluid's wall slide (0xa2cb_e5c4_6ff1_750b /
    // 0x1d5b_2bca_1514_2c6d alone) both move these digests; its pins were 0xf2bd_4a66_4f51_644c
    // and 0x3b10_85b3_dec3_e150.
    // integ-v048 (merge audit, ADR-5795; the test machine, x86_64): fixrev-09's software sqrt and digest
    // form (ADR-5495) move the uncapped pair to 0x3379_8cfd_fbf1_876d / 0x82e7_a52b_857f_3894,
    // integ-v048b's pins, and this tree with `SEPARATE_CELL_CAP` lifted to 1_000_000 hashes to
    // exactly those two. The cap of 12 then gives the pins below (integ-v048's tree, 565f3c4de).
    // integ-v049 (ADR-9230): two intended changes meet here. integ-v048c's fluid fixes give
    // 0x8cd8_32b4_f1cc_7edf / 0x01cf_4915_2cf4_30f2 (integ-v048e's line, d0d441fa2); the
    // `SolidField::normal` software sqrt (7fa2376c7, ADR-4753 class) gave 0xea0c_8cbb_4035_eafd /
    // 0x0519_1ded_fab5_26bf on integ-v049a, a line without those fixes. Together, from the v049a
    // merge eef8f45fd on, they hash to the pins below; that change alone on d0d441fa2 gives the
    // same pair, and the head with it reverted gives d0d441fa2's. Same bits on the build server (five runs)
    // and the test machine (x86_64).
    // v06-freeze-reds-k-core (ADR-8716): `PhysicsPlugin` initializes one more resource,
    // `ParentedBodyWarnings` (8c2f9c7f5, spec 15 § 1), so each body's entity row moved from 53
    // (bits 0xffffffca) to 54 (0xffffffc9) and the pins with it: 0xe924_b082_81f2_2163 /
    // 0xbd65_9117_36a9_e789 became the pair below. Proof: the base a9545d689 with that resource
    // removed hashes, at row 53, to exactly the old pair; with it, to this pair on the build server and on
    // the test machine. Nothing in the fluid or the coupling changed.
    assert_eq!(capsule_digest, 0xd6ba_6a69_8d6e_ed90, "capsule digest pin");
    assert_eq!(digest, 0x8ffa_4109_c024_a89a, "kinematic spin digest pin");
}

/// (Audit G-fluid #1) A paused simulation hands no impulse to its bodies: the coupling only applies
/// the impulse of a step that ran this tick, never the last one again.
#[test]
fn paused_fluid_does_not_reapply_last_impulse() {
    let (mut app, fluid) = app_with_fluid();
    let (body, _) = spawn_box(&mut app, fluid, 800.0);
    for _ in 0..20 {
        app.update();
    }
    let state = |app: &App| {
        app.world()
            .resource::<FluidCoupling>()
            .bodies
            .iter()
            .find(|b| b.entity == body)
            .map(|b| (b.applied, b.impulse))
            .expect("coupled")
    };
    let (applied, impulse) = state(&app);
    assert!(applied && impulse.y > 0.0, "running: {applied} {impulse}");
    app.world_mut()
        .resource_mut::<FluidWorld>()
        .get_mut(fluid)
        .expect("entry")
        .running = false;
    for tick in 0..5 {
        app.update();
        let (applied, impulse) = state(&app);
        assert!(
            !applied && impulse == Vec3::ZERO,
            "paused tick {tick}: applied {applied}, impulse {impulse}"
        );
    }
}

/// (Audit G-fluid #5) A kinematic body that spins up past half a turn per fixed step keeps the sign
/// of its spin: the pose difference is unwrapped against the previous step's spin instead of taking
/// the shortest arc. (Two poses alone cannot tell +270 degrees from -90 degrees; a spin-up can.)
#[test]
fn kinematic_spin_up_past_half_turn_keeps_sign() {
    use glam::Quat;
    let (mut app, fluid) = app_with_fluid();
    // Only the gather matters here: the liquid does not step.
    app.world_mut()
        .resource_mut::<FluidWorld>()
        .get_mut(fluid)
        .expect("entry")
        .running = false;
    let rotor = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.3, 0.3, SIZE.z * 0.5)),
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.05, 0.02, 0.1),
                },
                ..Collider::default()
            },
            FluidBody::new(fluid),
        ))
        .id();
    // One gather at rest first, so every later step has a previous pose.
    app.update();
    let dt = 1.0f32 / 60.0;
    let mut yaw = 0.0f32;
    let mut last_increment = 0.0f32;
    // The per-step yaw ramps 20, 40, ..., 260 degrees.
    for n in 1..=13 {
        last_increment = (20.0 * n as f32).to_radians();
        yaw += last_increment;
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(rotor) {
            t.rotation = Quat::from_rotation_y(yaw);
        }
        app.update();
        let spin = app
            .world()
            .resource::<FluidCoupling>()
            .bodies
            .iter()
            .find(|b| b.entity == rotor)
            .and_then(|b| b.solid)
            .map(|s| s.angular_velocity.y)
            .expect("rotor solid");
        let expected = last_increment / dt;
        assert!(
            (spin - expected).abs() <= 1e-3 * expected,
            "step {n}: spin {spin} rad/s, expected {expected} rad/s"
        );
    }
    assert!(last_increment > std::f32::consts::PI);
}

/// (Audit G-fluid #5, as it stands at this base) The software log map reads a pose difference
/// as a turn of up to a full revolution, chosen by the quaternion's sign. `q` and `-q` are the
/// same pose, so a sign flip between steps (here: a yaw angle wrapped into `[0, 2 pi)` as it
/// crosses 360 degrees, then an explicitly negated quaternion) must not turn a 2 degree step into
/// a 358 degree spin the other way.
#[test]
fn kinematic_spin_ignores_quaternion_sign() {
    use glam::Quat;
    let (mut app, fluid) = app_with_fluid();
    app.world_mut()
        .resource_mut::<FluidWorld>()
        .get_mut(fluid)
        .expect("entry")
        .running = false;
    let start = 350.0f32.to_radians();
    let rotor = app
        .world_mut()
        .spawn((
            Transform {
                rotation: Quat::from_rotation_y(start),
                ..Transform::from_translation(Vec3::new(0.3, 0.3, SIZE.z * 0.5))
            },
            RigidBody::kinematic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.05, 0.02, 0.1),
                },
                ..Collider::default()
            },
            FluidBody::new(fluid),
        ))
        .id();
    app.update();
    let dt = 1.0f32 / 60.0;
    let step = 2.0f32.to_radians();
    let expected = step / dt;
    for n in 1..=12 {
        let yaw = (start + step * n as f32).rem_euclid(std::f32::consts::TAU);
        let mut q = Quat::from_rotation_y(yaw);
        if n >= 9 && n % 2 == 1 {
            q = -q;
        }
        if let Some(mut t) = app.world_mut().get_mut::<Transform>(rotor) {
            t.rotation = q;
        }
        app.update();
        let spin = app
            .world()
            .resource::<FluidCoupling>()
            .bodies
            .iter()
            .find(|b| b.entity == rotor)
            .and_then(|b| b.solid)
            .map(|s| s.angular_velocity)
            .expect("rotor solid");
        assert!(
            (spin.y - expected).abs() <= 1e-2 * expected && spin.x.abs() < 1e-3 && spin.z.abs() < 1e-3,
            "step {n} (yaw {yaw}): spin {spin} rad/s, expected {expected} rad/s about +y"
        );
    }
}
