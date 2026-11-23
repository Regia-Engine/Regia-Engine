// SPDX-License-Identifier: MPL-2.0
//! Opt-in swept CCD in the step (ADR-2423): a fast spinning thin bar against thin geometry
//! tunnels without `SweptCcd` and stops with it; the hook is deterministic at fixed steps,
//! and a `SweptCcd` body that hits nothing leaves the world bit-identical.
//!
//! The thin geometry is a kinematic body (a door, a platform, a vehicle panel). Rapier 0.36
//! already sweeps fast non-bullet bodies against *fixed* colliders on its own (its automatic
//! CCD tier), so a fixed wall would stop the bar with or without `SweptCcd`; against kinematic
//! and dynamic bodies only bullets (`RigidBody::ccd`) are swept by rapier.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::{
    Collider, ColliderShape, LockedAxes, PhysicsPlugin, PhysicsPose, PhysicsWorld, RigidBody, SweptCcd,
    Velocity,
};
use regia_scene::Transform;

/// Spin of the turnstile bar: 0.75 rad per 60 Hz step (rapier caps one step's rotation at
/// pi/4, so faster spins do not exist inside a step).
const SPIN: f32 = 45.0;

/// Polar angle of the thin blade the turnstile bar sweeps through.
const BLADE_ANGLE: f32 = 0.4;

fn physics_app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed).add_plugins(PhysicsPlugin);
    app
}

/// Runs updates until the physics world has taken `n` more steps (bounded).
fn step(app: &mut App, n: u64) {
    let target = app.world().resource::<PhysicsWorld>().stats().steps + n;
    for _ in 0..(n * 4 + 8) {
        if app.world().resource::<PhysicsWorld>().stats().steps >= target {
            return;
        }
        app.update();
    }
    panic!("physics did not take {n} steps");
}

fn weightless(locked_axes: LockedAxes) -> RigidBody {
    RigidBody {
        gravity_scale: 0.0,
        can_sleep: false,
        locked_axes,
        ..RigidBody::dynamic()
    }
}

/// A 2 m, 4 cm thin bar along X, pinned at its centre (translation and X/Y rotation locked),
/// spinning about Z; and a 1 cm thin kinematic blade, radial, at `BLADE_ANGLE`, from r = 0.45
/// to 0.95. In one step the bar turns 0.75 rad, from 0 to past the blade, without touching it
/// at either step boundary.
fn turnstile(app: &mut App, swept: bool) -> Entity {
    let locks = LockedAxes {
        translation_x: true,
        translation_y: true,
        translation_z: true,
        rotation_x: true,
        rotation_y: true,
        rotation_z: false,
    };
    let mut bar = app.world_mut().spawn((
        Transform::IDENTITY,
        weightless(locks),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(1.0, 0.02, 0.02),
            },
            ..Collider::default()
        },
        Velocity {
            linear: Vec3::ZERO,
            angular: Vec3::new(0.0, 0.0, SPIN),
        },
    ));
    if swept {
        bar.insert(SweptCcd::default());
    }
    let bar = bar.id();
    let blade_rotation = Quat::from_rotation_z(BLADE_ANGLE);
    app.world_mut().spawn((
        Transform {
            translation: blade_rotation * Vec3::new(0.7, 0.0, 0.0),
            rotation: blade_rotation,
            ..Transform::IDENTITY
        },
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.25, 0.005, 0.5),
            },
            ..Collider::default()
        },
    ));
    bar
}

/// The bar's polar angle in `(-pi, pi]` and its spin about Z.
fn bar_state(app: &App, bar: Entity) -> (f32, f32) {
    let world = app.world();
    // The simulated pose, not the interpolated render `Transform`.
    let rotation = world
        .get::<PhysicsPose>(bar)
        .map(|p| p.rotation)
        .unwrap_or(Quat::IDENTITY);
    let tip = rotation * Vec3::X;
    let spin = world
        .get::<Velocity>(bar)
        .map(|v| v.angular.z)
        .unwrap_or(f32::NAN);
    (tip.y.atan2(tip.x), spin)
}

#[test]
fn spinning_bar_tunnels_through_a_thin_blade_without_swept_ccd() {
    let mut app = physics_app();
    let bar = turnstile(&mut app, false);
    step(&mut app, 1);
    let (angle, spin) = bar_state(&app, bar);
    // One step turns the bar 0.75 rad: from 0 straight past the blade at 0.4, untouched.
    assert!((angle - 0.75).abs() < 0.02, "bar angle after one step: {angle}");
    assert!(
        (spin - SPIN).abs() < 0.5,
        "spin is unchanged, no contact was seen: {spin}"
    );
    step(&mut app, 2);
    let (_, spin) = bar_state(&app, bar);
    assert!((spin - SPIN).abs() < 0.5, "still spinning freely: {spin}");
}

#[test]
fn spinning_bar_stops_at_the_thin_blade_with_swept_ccd() {
    let mut app = physics_app();
    let bar = turnstile(&mut app, true);
    step(&mut app, 1);
    let (angle, spin) = bar_state(&app, bar);
    // The leading face meets the blade's near face first at its inner end (r = 0.45): about
    // 0.4 - 0.025 / 0.45 = 0.344 rad, less the skin.
    assert!(
        angle > 0.30 && angle < 0.36,
        "bar clamped short of the blade: {angle}"
    );
    assert!(spin.abs() < 1.0, "the impact removed the closing spin: {spin}");
    let state = app.world().get::<SweptCcd>(bar).copied().unwrap_or_default();
    assert_eq!(state.hits, 1, "{state:?}");
    assert_eq!(state.failures, 0, "{state:?}");
    let toi = state.last_toi.unwrap_or(f32::NAN);
    assert!(
        toi > 0.40 && toi < 0.46,
        "time of impact {toi} (about 0.33 / 0.75 of the step)"
    );
    // It stays on the near side from then on: the solver holds the contact.
    for _ in 0..30 {
        step(&mut app, 1);
        let (angle, _) = bar_state(&app, bar);
        assert!(
            angle > 0.3 && angle < BLADE_ANGLE,
            "bar passed the blade later: {angle}"
        );
    }
}

/// A 1 m bar thrown at 120 m/s (2 m per step) while spinning, at a 1 cm kinematic wall at
/// x = 5: it is clear of the wall at x = 4 and x = 6, so a discrete step never sees it.
fn thrown_bar(app: &mut App, swept: bool) -> Entity {
    let mut bar = app.world_mut().spawn((
        Transform::IDENTITY,
        weightless(LockedAxes::default()),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.5, 0.02, 0.02),
            },
            ..Collider::default()
        },
        Velocity {
            linear: Vec3::new(120.0, 0.0, 0.0),
            angular: Vec3::new(0.0, 0.0, 30.0),
        },
    ));
    if swept {
        bar.insert(SweptCcd::default());
    }
    let bar = bar.id();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.005, 3.0, 3.0),
            },
            ..Collider::default()
        },
    ));
    bar
}

fn bar_x(app: &App, bar: Entity) -> f32 {
    app.world()
        .get::<PhysicsPose>(bar)
        .map(|p| p.translation.x)
        .unwrap_or(f32::NAN)
}

#[test]
fn thrown_spinning_bar_tunnels_without_and_stops_with_swept_ccd() {
    let mut off = physics_app();
    let bar_off = thrown_bar(&mut off, false);
    step(&mut off, 10);
    assert!(
        bar_x(&off, bar_off) > 15.0,
        "no CCD: passed the wall, x = {}",
        bar_x(&off, bar_off)
    );

    let mut on = physics_app();
    let bar_on = thrown_bar(&mut on, true);
    step(&mut on, 10);
    let x = bar_x(&on, bar_on);
    assert!(x < 5.0, "swept CCD: stopped on the near side, x = {x}");
    let state = on.world().get::<SweptCcd>(bar_on).copied().unwrap_or_default();
    assert!(state.hits >= 1, "{state:?}");
}

/// Folds every step's state digest of a turnstile run.
fn turnstile_digests(swept: bool, steps: u64) -> Vec<u64> {
    let mut app = physics_app();
    turnstile(&mut app, swept);
    let mut out = Vec::new();
    for _ in 0..steps {
        step(&mut app, 1);
        out.push(app.world().resource::<PhysicsWorld>().state_digest());
    }
    out
}

#[test]
fn swept_ccd_is_deterministic_at_fixed_steps() {
    let a = turnstile_digests(true, 40);
    let b = turnstile_digests(true, 40);
    assert_eq!(a, b, "two runs of the same scene must match step for step");
    // And it is a different simulation from the CCD-off run (the hook really acted).
    assert_ne!(a, turnstile_digests(false, 40));
}

/// A stack of touching boxes on a floor plus one free body far away, with or without `SweptCcd` on that
/// body and on one pile body.
fn pile_digests(with_component: bool) -> Vec<u64> {
    let mut app = physics_app();
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
        RigidBody::static_body(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(10.0, 0.5, 10.0),
            },
            ..Collider::default()
        },
    ));
    for i in 0..6 {
        let mut body = app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.1 * i as f32, 0.5 + 1.0 * i as f32, 0.05 * i as f32)),
            RigidBody::dynamic(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::splat(0.5),
                },
                ..Collider::default()
            },
        ));
        // A slow body resting in the solver's contact range: the hook leaves it to the solver.
        if with_component && i == 0 {
            body.insert(SweptCcd::default());
        }
    }
    let mut free = app.world_mut().spawn((
        Transform::from_translation(Vec3::new(50.0, 20.0, 0.0)),
        weightless(LockedAxes::default()),
        Collider {
            shape: ColliderShape::Sphere { radius: 0.2 },
            ..Collider::default()
        },
        Velocity {
            linear: Vec3::new(30.0, 0.0, 0.0),
            angular: Vec3::new(5.0, 0.0, 0.0),
        },
    ));
    if with_component {
        free.insert(SweptCcd::default());
    }
    let mut out = Vec::new();
    for _ in 0..90 {
        step(&mut app, 1);
        out.push(app.world().resource::<PhysicsWorld>().state_digest());
    }
    out
}

#[test]
fn swept_ccd_without_an_impact_leaves_the_world_bit_identical() {
    assert_eq!(
        pile_digests(false),
        pile_digests(true),
        "a SweptCcd body that never hits must not perturb the simulation"
    );
}

/// Two 10 cm balls fired head-on at 200 m/s each (3.3 m per step each), from x = -5 and x = 5.
/// Rapier sweeps fast bodies against fixed geometry only, and bullets (`ccd`) never against
/// other bullets, so they pass through each other between two steps.
fn head_on(swept: bool, bullets: bool) -> (f32, f32) {
    let mut app = physics_app();
    let mut spawn = |x: f32, v: f32| {
        let mut ball = app.world_mut().spawn((
            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            RigidBody {
                ccd: bullets,
                ..weightless(LockedAxes::default())
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.1 },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(v, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
        ));
        if swept {
            ball.insert(SweptCcd::default());
        }
        ball.id()
    };
    let a = spawn(-5.0, 200.0);
    let b = spawn(5.0, -200.0);
    step(&mut app, 5);
    (bar_x(&app, a), bar_x(&app, b))
}

#[test]
fn head_on_fast_bodies_pass_through_each_other_without_and_meet_with_swept_ccd() {
    for bullets in [false, true] {
        let (a, b) = head_on(false, bullets);
        assert!(
            a > b,
            "bullets {bullets}, no SweptCcd: the balls crossed (a {a}, b {b})"
        );
        let (a, b) = head_on(true, bullets);
        assert!(
            a < b,
            "bullets {bullets}, SweptCcd: the balls met and stayed apart (a {a}, b {b})"
        );
    }
}

/// Two `SweptCcd` balls fired head-on, step-by-step digests. With `invert_ids`, both entity
/// ids are reserved first and the bodies inserted handle-first, so the entity-id order is
/// the reverse of the body-handle order — what parallel `Commands::spawn` produces on
/// another thread count (ADR-1347). `capture_starts` must sweep in body-handle order, so
/// both runs are bit-identical; the order matters because `resolve` clamps each body before
/// the next is swept, and the second sweep reads the first body's clamped pose.
fn head_on_digests(invert_ids: bool) -> Vec<u64> {
    let mut app = physics_app();
    let ball = |x: f32, v: f32| {
        (
            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            weightless(LockedAxes::default()),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.1 },
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::new(v, 0.0, 0.0),
                angular: Vec3::ZERO,
            },
            SweptCcd::default(),
        )
    };
    if invert_ids {
        // Reserve both ids (as a parallel system's earlier-recorded command queue would),
        // then give the lower body handle to the higher entity id.
        let low = app.world_mut().spawn_empty().id();
        let high = app.world_mut().spawn_empty().id();
        app.world_mut().entity_mut(high).insert(ball(-5.0, 200.0));
        app.world_mut().entity_mut(low).insert(ball(5.0, -200.0));
    } else {
        app.world_mut().spawn(ball(-5.0, 200.0));
        app.world_mut().spawn(ball(5.0, -200.0));
    }
    let mut out = Vec::new();
    for _ in 0..5 {
        step(&mut app, 1);
        out.push(app.world().resource::<PhysicsWorld>().state_digest());
    }
    out
}

#[test]
fn swept_ccd_order_follows_body_handles_not_entity_ids() {
    let natural = head_on_digests(false);
    let inverted = head_on_digests(true);
    assert_eq!(
        natural, inverted,
        "entity-id order must not change the sweep order (ADR-1347)"
    );
}

/// Corr-physics audit guard: a swept body clamped at a wall must not stop a later swept body that
/// passes near where the first one would have ended unclamped (the audit suspected a stale
/// collider pose there; this scene passes on that code, so the suspicion is listed as unverified
/// in the lane report).
#[test]
fn a_later_swept_body_does_not_meet_the_phantom_of_a_clamped_one() {
    let mut app = physics_app();
    let mut ball = |x: f32, v: f32| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
                weightless(LockedAxes::default()),
                Collider {
                    shape: ColliderShape::Sphere { radius: 0.5 },
                    ..Collider::default()
                },
                Velocity {
                    linear: Vec3::new(v, 0.0, 0.0),
                    angular: Vec3::ZERO,
                },
                SweptCcd::default(),
            ))
            .id()
    };
    // A: 10 m in one step toward a kinematic wall at x = 5 (rapier only sweeps against fixed
    // colliders on its own), so A is clamped at about x = 4.45.
    let a = ball(0.0, 600.0);
    // B: from 12 to 10.9, nowhere near A's real path, but within reach of A's unclamped end.
    let b = ball(12.0, -66.0);
    app.world_mut().spawn((
        Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        RigidBody::kinematic(),
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::new(0.05, 2.0, 2.0),
            },
            ..Collider::default()
        },
    ));
    step(&mut app, 1);
    let a_state = *app.world().get::<SweptCcd>(a).expect("swept");
    assert_eq!(a_state.hits, 1, "A stops at the wall: {a_state:?}");
    assert!(bar_x(&app, a) < 5.0, "A at {}", bar_x(&app, a));
    let b_state = *app.world().get::<SweptCcd>(b).expect("swept");
    assert_eq!(b_state.hits, 0, "B met nothing real: {b_state:?}");
    assert!(
        (bar_x(&app, b) - (12.0 - 66.0 / 60.0)).abs() < 1e-3,
        "B kept its motion: {}",
        bar_x(&app, b)
    );
}
