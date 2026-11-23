// SPDX-License-Identifier: MPL-2.0
//! Numeric-robustness regressions from the v0.2.14 candidate's numerics review
//! (audit-v035-numerics: H1 gravity, H2 collider material, M8 cloth compliance, M9 pose
//! quaternions; H3 joints and M6 character sweep as guards, not reproduced). Each regression
//! test fed the base a value it handed rapier (or the cloth solver) unchecked and failed there:
//! the solver state went non-finite or non-unit, or the body stopped responding.
//!
//! The rule under test is ADR-1507's: a non-finite or out-of-domain input is dropped or
//! substituted at the seam and counted, so the solver state stays finite. Every test steps a
//! fixed 60 Hz world a fixed number of times; no wall clock is read.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::Entity;
use regia_physics::cloth::{
    ClothConfig, ClothGrid, ClothSolver, ClothTuning, PoseMaps, PoseTarget, SoftModel, SoftParams, SoftSolver,
};
use regia_physics::{
    CharacterBody, Collider, ColliderHandle, ColliderShape, Joint, JointKind, JointMotorConfig, Mass,
    PhysicsPlugin, PhysicsSettings, PhysicsWorld, RigidBody, RigidBodyHandle, Teleport, Velocity,
};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;

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

fn steps(app: &mut App, count: usize) {
    for _ in 0..count {
        app.update();
    }
}

fn set_gravity(app: &mut App, gravity: Vec3) {
    app.insert_resource(PhysicsSettings {
        gravity,
        ..PhysicsSettings::default()
    });
}

fn ground(app: &mut App) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(10.0, 0.5, 10.0),
                },
                friction: 0.8,
                ..Collider::default()
            },
        ))
        .id()
}

fn dynamic_with(app: &mut App, translation: Vec3, collider: Collider) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(translation),
            RigidBody::dynamic(),
            collider,
            Velocity::default(),
        ))
        .id()
}

fn unit_box(app: &mut App, translation: Vec3) -> Entity {
    dynamic_with(app, translation, Collider::default())
}

fn rapier_body(app: &App, entity: Entity) -> &rapier3d::dynamics::RigidBody {
    let handle = app
        .world()
        .get::<RigidBodyHandle>(entity)
        .expect("body handle")
        .to_handle();
    app.world()
        .resource::<PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("rapier body")
}

fn rapier_collider(app: &App, entity: Entity) -> &rapier3d::geometry::Collider {
    let handle = app
        .world()
        .get::<ColliderHandle>(entity)
        .expect("collider handle")
        .to_handle();
    app.world()
        .resource::<PhysicsWorld>()
        .colliders
        .get(handle)
        .expect("rapier collider")
}

/// The solver-side state of a body: pose, velocities and mass all finite.
fn solver_state_finite(app: &App, entity: Entity) -> bool {
    let rb = rapier_body(app, entity);
    let t = rb.translation();
    let r = rb.rotation();
    let v = rb.linvel();
    let w = rb.angvel();
    [
        t.x,
        t.y,
        t.z,
        r.x,
        r.y,
        r.z,
        r.w,
        v.x,
        v.y,
        v.z,
        w.x,
        w.y,
        w.z,
        rb.mass(),
    ]
    .iter()
    .all(|c| c.is_finite())
}

fn rotation_length(app: &App, entity: Entity) -> f32 {
    let r = rapier_body(app, entity).rotation();
    (r.x * r.x + r.y * r.y + r.z * r.z + r.w * r.w).sqrt()
}

fn non_finite_inputs(app: &App) -> u64 {
    app.world().resource::<PhysicsWorld>().stats().non_finite_inputs
}

// ---------------------------------------------------------------------------
// H1: gravity
// ---------------------------------------------------------------------------

/// A NaN gravity reached rapier verbatim. On the base (rapier 0.36) the falling box stopped
/// falling while it was set (its solver state stayed finite). The step now keeps the last finite
/// gravity and counts the drop; a fixed gravity is followed again.
#[test]
fn a_nan_gravity_keeps_the_last_finite_gravity() {
    let mut app = physics_app();
    let body = unit_box(&mut app, Vec3::new(0.0, 50.0, 0.0));
    steps(&mut app, 3);
    set_gravity(&mut app, Vec3::new(0.0, f32::NAN, 0.0));
    steps(&mut app, 5);
    assert!(
        solver_state_finite(&app, body),
        "{:?}",
        rapier_body(&app, body).linvel()
    );
    assert!(
        rapier_body(&app, body).linvel().y < 0.0,
        "the body keeps falling under the last finite gravity: {:?}",
        rapier_body(&app, body).linvel()
    );
    assert!(non_finite_inputs(&app) > 0);
    set_gravity(&mut app, Vec3::new(f32::INFINITY, -9.81, 0.0));
    steps(&mut app, 5);
    assert!(solver_state_finite(&app, body));
    set_gravity(&mut app, Vec3::new(0.0, -9.81, 0.0));
    let y = rapier_body(&app, body).translation().y;
    steps(&mut app, 5);
    assert!(solver_state_finite(&app, body));
    assert!(rapier_body(&app, body).translation().y < y);
}

// ---------------------------------------------------------------------------
// H2: collider material
// ---------------------------------------------------------------------------

/// Density, friction and restitution reached rapier verbatim: a NaN or infinite density gave the
/// body NaN mass properties, a negative one a negative mass. Out-of-domain values are now
/// substituted with the `Collider` defaults and counted.
#[test]
fn collider_material_out_of_domain_is_substituted() {
    let mut app = physics_app();
    ground(&mut app);
    let sphere = |density: f32, friction: f32, restitution: f32| Collider {
        shape: ColliderShape::Sphere { radius: 0.5 },
        density,
        friction,
        restitution,
        ..Collider::default()
    };
    let nan_density = dynamic_with(&mut app, Vec3::new(-4.0, 0.6, 0.0), sphere(f32::NAN, 0.5, 0.0));
    let inf_density = dynamic_with(
        &mut app,
        Vec3::new(-2.0, 0.6, 0.0),
        sphere(f32::INFINITY, 0.5, 0.0),
    );
    let negative_density = dynamic_with(&mut app, Vec3::new(0.0, 0.6, 0.0), sphere(-5.0, 0.5, 0.0));
    let nan_friction = dynamic_with(
        &mut app,
        Vec3::new(2.0, 0.6, 0.0),
        sphere(1.0, f32::NAN, f32::NAN),
    );
    let negative_friction = dynamic_with(&mut app, Vec3::new(4.0, 0.6, 0.0), sphere(1.0, -1.0, -1.0));
    steps(&mut app, 20);
    for body in [
        nan_density,
        inf_density,
        negative_density,
        nan_friction,
        negative_friction,
    ] {
        assert!(solver_state_finite(&app, body), "{body:?}");
        assert!(
            rapier_body(&app, body).mass() > 0.0,
            "{body:?}: {}",
            rapier_body(&app, body).mass()
        );
        let collider = rapier_collider(&app, body);
        assert!(
            collider.friction().is_finite() && collider.friction() >= 0.0,
            "{body:?}: friction {}",
            collider.friction()
        );
        assert!(
            collider.restitution().is_finite() && collider.restitution() >= 0.0,
            "{body:?}: restitution {}",
            collider.restitution()
        );
    }
    assert!(non_finite_inputs(&app) >= 5);
    // A changed collider goes through the same seam.
    app.world_mut()
        .get_mut::<Collider>(nan_friction)
        .expect("collider")
        .density = f32::NAN;
    steps(&mut app, 5);
    assert!(solver_state_finite(&app, nan_friction));
    assert!(rapier_body(&app, nan_friction).mass() > 0.0);
}

/// A negative explicit mass, or a principal moment that is not positive, used to be added on
/// top of the collider mass. Parry stores `1/mass` for every non-zero value, so a mass of −10 kg
/// on the default 1 kg box made the body −9 kg, and a separating contact then accelerated it
/// into the contact. A zero moment stored inverse inertia 0, so a torque about that axis did
/// not turn the body. Those inputs are dropped and counted; a positive mass with three positive
/// moments is still added.
#[test]
fn explicit_mass_below_zero_or_with_a_non_positive_moment_is_not_pushed() {
    let mut app = physics_app();
    let spawn = |app: &mut App, x: f32, mass: f32, inertia: Vec3| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(x, 2.0, 0.0)),
                RigidBody::dynamic(),
                Collider::default(),
                Mass::Explicit {
                    mass,
                    center_of_mass: Vec3::ZERO,
                    principal_inertia: inertia,
                },
            ))
            .id()
    };
    // Default box: 1 m³ at density 1, so the collider alone is 1 kg.
    let negative = spawn(&mut app, 0.0, -10.0, Vec3::ONE);
    let zero_mass = spawn(&mut app, 3.0, 0.0, Vec3::ONE);
    let zero_moment = spawn(&mut app, 6.0, 10.0, Vec3::new(1.0, 0.0, 1.0));
    let negative_moment = spawn(&mut app, -3.0, 4.0, Vec3::new(1.0, -2.0, 1.0));
    let valid = spawn(&mut app, 9.0, 10.0, Vec3::ONE);
    steps(&mut app, 2);
    for body in [negative, zero_mass, zero_moment, negative_moment] {
        let mass = rapier_body(&app, body).mass();
        assert!(
            (mass - 1.0).abs() < 1e-3,
            "{body:?} should keep the 1 kg collider mass, got {mass}"
        );
        assert!(solver_state_finite(&app, body));
    }
    let valid_mass = rapier_body(&app, valid).mass();
    assert!(
        (valid_mass - 11.0).abs() < 1e-2,
        "a positive explicit mass is still added to the collider: {valid_mass}"
    );
    assert!(non_finite_inputs(&app) >= 4);

    // A later edit goes through `sync_body_properties`. Replacing a valid explicit mass with a
    // negative one is not pushed, so the body keeps the mass it already has.
    let before = non_finite_inputs(&app);
    app.world_mut().entity_mut(valid).insert(Mass::Explicit {
        mass: -10.0,
        center_of_mass: Vec3::ZERO,
        principal_inertia: Vec3::ONE,
    });
    steps(&mut app, 2);
    let after = rapier_body(&app, valid).mass();
    assert!(
        (after - valid_mass).abs() < 1e-2,
        "a negative edit is not pushed: {after} (was {valid_mass})"
    );
    assert!(non_finite_inputs(&app) > before);
    assert!(solver_state_finite(&app, valid));
}

/// Valid material numbers are handed over unchanged.
#[test]
fn valid_collider_material_is_unchanged() {
    let mut app = physics_app();
    let body = dynamic_with(
        &mut app,
        Vec3::new(0.0, 5.0, 0.0),
        Collider {
            density: 2.5,
            friction: 1.25,
            restitution: 0.75,
            contact_force_threshold: 3.0,
            ..Collider::default()
        },
    );
    steps(&mut app, 1);
    let collider = rapier_collider(&app, body);
    assert_eq!(collider.density(), 2.5);
    assert_eq!(collider.friction(), 1.25);
    assert_eq!(collider.restitution(), 0.75);
    assert_eq!(collider.contact_force_event_threshold(), 3.0);
    assert_eq!(non_finite_inputs(&app), 0);
}

// ---------------------------------------------------------------------------
// H3: joints
// ---------------------------------------------------------------------------

/// Two dynamic 1 m boxes side by side, jointed with `joint` (its `connected` filled in).
fn jointed_pair(app: &mut App, mut joint: Joint) -> (Entity, Entity) {
    let a = unit_box(app, Vec3::new(0.0, 5.0, 0.0));
    let b = unit_box(app, Vec3::new(1.5, 5.0, 0.0));
    joint.connected = b;
    app.world_mut().entity_mut(a).insert(joint);
    (a, b)
}

fn hinge(limits: Option<(f32, f32)>, motor: Option<JointMotorConfig>) -> JointKind {
    JointKind::Revolute {
        axis: Vec3::Z,
        limits,
        motor,
    }
}

/// Guard (audit-v035-numerics H3, not reproduced): the audit expected a NaN anchor, limit or
/// motor term to make both bodies' solver state NaN on the first step. On rapier 0.36 every case
/// below keeps both bodies finite on the base too; this pins that, so a rapier upgrade that
/// changes it fails here.
#[test]
fn joints_with_non_finite_numbers_keep_the_solver_finite() {
    let motor = |stiffness: f32, damping: f32, target: f32| JointMotorConfig {
        target_position: target,
        target_velocity: 0.0,
        stiffness,
        damping,
        max_force: 0.0,
    };
    let cases: Vec<(&str, Joint)> = vec![
        (
            "nan anchor",
            Joint {
                kind: JointKind::Spherical { cone_limit: None },
                anchor: Vec3::new(f32::NAN, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.75, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "infinite connected anchor",
            Joint {
                kind: JointKind::Fixed,
                anchor: Vec3::new(0.75, 0.0, 0.0),
                connected_anchor: Vec3::new(f32::INFINITY, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "nan hinge limit",
            Joint {
                kind: hinge(Some((f32::NAN, 0.3)), None),
                anchor: Vec3::new(0.75, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.75, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "nan motor stiffness",
            Joint {
                kind: hinge(None, Some(motor(f32::NAN, 1.0, 1.0))),
                anchor: Vec3::new(0.75, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.75, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "infinite motor target",
            Joint {
                kind: hinge(None, Some(motor(10.0, 1.0, f32::INFINITY))),
                anchor: Vec3::new(0.75, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.75, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "nan cone",
            Joint {
                kind: JointKind::Spherical {
                    cone_limit: Some(f32::NAN),
                },
                anchor: Vec3::new(0.75, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.75, 0.0, 0.0),
                ..Joint::default()
            },
        ),
        (
            "nan distance",
            Joint {
                kind: JointKind::Distance {
                    min: f32::NAN,
                    max: 1.0,
                },
                anchor: Vec3::new(0.5, 0.0, 0.0),
                connected_anchor: Vec3::new(-0.5, 0.0, 0.0),
                ..Joint::default()
            },
        ),
    ];
    for (name, joint) in cases {
        let mut app = physics_app();
        let (a, b) = jointed_pair(&mut app, joint);
        steps(&mut app, 10);
        assert!(
            solver_state_finite(&app, a),
            "{name}: {:?}",
            rapier_body(&app, a).translation()
        );
        assert!(
            solver_state_finite(&app, b),
            "{name}: {:?}",
            rapier_body(&app, b).translation()
        );
    }
}

/// A finite hinge motor drives its arm without a non-finite-input count.
#[test]
fn a_finite_hinge_motor_still_drives() {
    let mut app = physics_app();
    set_gravity(&mut app, Vec3::ZERO);
    let base = app
        .world_mut()
        .spawn((Transform::from_translation(Vec3::ZERO), RigidBody::static_body()))
        .id();
    let arm = unit_box(&mut app, Vec3::new(1.0, 0.0, 0.0));
    app.world_mut().entity_mut(base).insert(Joint {
        kind: hinge(
            None,
            Some(JointMotorConfig {
                target_position: 0.0,
                target_velocity: 2.0,
                stiffness: 0.0,
                damping: 50.0,
                max_force: 0.0,
            }),
        ),
        connected: arm,
        anchor: Vec3::ZERO,
        connected_anchor: Vec3::new(-1.0, 0.0, 0.0),
        break_force: 0.0,
    });
    steps(&mut app, 30);
    let spin = rapier_body(&app, arm).angvel().z.abs();
    assert!(spin > 0.5, "the motor turns the arm: {spin} rad/s");
    assert_eq!(non_finite_inputs(&app), 0);
}

// ---------------------------------------------------------------------------
// M9: pose quaternions at the kinematic and teleport seams
// ---------------------------------------------------------------------------

/// A kinematic body whose `Transform.rotation` is the zero quaternion (or not unit) was pushed
/// as rapier's next rotation verbatim, breaking rapier's unit-rotation invariant. A zero
/// rotation is dropped (the body keeps its last pose, counted); a non-unit one is normalized.
#[test]
fn kinematic_rotations_reach_rapier_as_unit_quaternions() {
    let mut app = physics_app();
    let spawn = |app: &mut App, x: f32| {
        app.world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(x, 1.0, 0.0)),
                RigidBody::kinematic(),
                Collider::default(),
            ))
            .id()
    };
    let zero = spawn(&mut app, 0.0);
    let scaled = spawn(&mut app, 3.0);
    steps(&mut app, 2);
    app.world_mut()
        .get_mut::<Transform>(zero)
        .expect("transform")
        .rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
    app.world_mut()
        .get_mut::<Transform>(scaled)
        .expect("transform")
        .rotation = Quat::from_xyzw(0.0, 0.0, 0.0, 2.0);
    let before = non_finite_inputs(&app);
    steps(&mut app, 3);
    for body in [zero, scaled] {
        let length = rotation_length(&app, body);
        assert!((length - 1.0).abs() < 1e-4, "{body:?}: rotation length {length}");
        assert!(solver_state_finite(&app, body));
    }
    assert!(non_finite_inputs(&app) > before);
}

/// A teleport to a zero rotation is dropped like a non-finite one; a non-unit rotation is
/// normalized.
#[test]
fn teleport_rotations_reach_rapier_as_unit_quaternions() {
    let mut app = physics_app();
    set_gravity(&mut app, Vec3::ZERO);
    let zero = unit_box(&mut app, Vec3::new(0.0, 1.0, 0.0));
    let scaled = unit_box(&mut app, Vec3::new(3.0, 1.0, 0.0));
    steps(&mut app, 2);
    for (body, rotation) in [
        (zero, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
        (scaled, Quat::from_xyzw(0.0, 0.0, 0.0, 2.0)),
    ] {
        let mut entity = app.world_mut().entity_mut(body);
        entity.get_mut::<Transform>().expect("transform").rotation = rotation;
        entity.insert(Teleport);
    }
    steps(&mut app, 3);
    for body in [zero, scaled] {
        let length = rotation_length(&app, body);
        assert!((length - 1.0).abs() < 1e-4, "{body:?}: rotation length {length}");
        assert!(solver_state_finite(&app, body));
        let rendered = app.world().get::<Transform>(body).expect("transform").rotation;
        assert!(
            (rendered.length() - 1.0).abs() < 1e-4,
            "{body:?}: rendered rotation {rendered:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// M6: character sweep inputs
// ---------------------------------------------------------------------------

/// Walks a capsule into a resting crate for half a second with `settings`, `radius` and `dt`,
/// stepping the world each frame. Returns the crate entity and the summed translations.
fn walk_into_crate(settings: &CharacterBody, radius: f32, dt: f32) -> (App, Entity, Vec3) {
    let mut app = physics_app();
    ground(&mut app);
    let crate_box = unit_box(&mut app, Vec3::new(0.0, 0.5, 0.0));
    steps(&mut app, 30);
    let mut position = Vec3::new(-1.0, 0.92, 0.0);
    let mut total = Vec3::ZERO;
    for _ in 0..30 {
        let moved = {
            let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
            regia_physics::move_character(
                &mut physics,
                settings,
                radius,
                0.6,
                (position, Quat::IDENTITY),
                Vec3::new(2.0 / 60.0, 0.0, 0.0),
                0.8,
                0.0,
                0.0,
                None,
                dt,
                dt,
            )
        };
        total += moved.translation;
        if moved.translation.is_finite() {
            position += moved.translation;
        }
        app.update();
    }
    (app, crate_box, total)
}

/// Guard (audit-v035-numerics M6, not reproduced): an infinite character mass was expected to
/// launch every pushed body to infinity. On rapier 0.36 the pushed crate stays finite on the
/// base; this pins it.
#[test]
fn an_infinite_character_mass_pushes_nothing() {
    let settings = CharacterBody {
        mass: f32::INFINITY,
        ..CharacterBody::default()
    };
    let (app, crate_box, total) = walk_into_crate(&settings, 0.3, DT);
    assert!(
        solver_state_finite(&app, crate_box),
        "{:?}",
        rapier_body(&app, crate_box).linvel()
    );
    assert!(total.is_finite());
}

/// Guard (M6, not reproduced): a NaN step length leaves the move and the pushed crate finite.
#[test]
fn a_nan_character_dt_moves_and_pushes_nothing() {
    let (app, crate_box, total) = walk_into_crate(&CharacterBody::default(), 0.3, f32::NAN);
    assert!(
        solver_state_finite(&app, crate_box),
        "{:?}",
        rapier_body(&app, crate_box).linvel()
    );
    assert!(total.is_finite(), "{total:?}");
}

/// Guard (M6, not reproduced): non-finite or negative capsule sizes and settings leave the move
/// and the pushed crate finite (parry accepts the capsule; the sweep reports a finite move).
#[test]
fn non_finite_character_sizes_and_settings_stay_finite() {
    let settings = CharacterBody {
        offset: f32::NAN,
        autostep_min_width: f32::NAN,
        min_slope_slide_angle: f32::NAN,
        ..CharacterBody::default()
    };
    for radius in [f32::NAN, -0.3, f32::INFINITY] {
        let (app, crate_box, total) = walk_into_crate(&settings, radius, DT);
        assert!(total.is_finite(), "radius {radius}: {total:?}");
        assert!(solver_state_finite(&app, crate_box), "radius {radius}");
    }
}

/// Guard (M6, not reproduced): a NaN climb angle still reports flat ground as ground.
#[test]
fn a_nan_climb_angle_still_finds_flat_ground() {
    let mut app = physics_app();
    ground(&mut app);
    steps(&mut app, 2);
    let moved = {
        let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
        regia_physics::move_character(
            &mut physics,
            &CharacterBody::default(),
            0.3,
            0.6,
            (Vec3::new(0.0, 0.92, 0.0), Quat::IDENTITY),
            Vec3::new(0.01, -0.05, 0.0),
            f32::NAN,
            0.0,
            0.0,
            None,
            DT,
            DT,
        )
    };
    assert!(moved.translation.is_finite(), "{:?}", moved.translation);
    assert!(
        moved.grounded,
        "flat ground is ground under the default climb angle"
    );
}

// ---------------------------------------------------------------------------
// M8: cloth compliance overflow
// ---------------------------------------------------------------------------

/// A huge but finite compliance passed validation, but `compliance / h²` overflowed to infinity
/// and `∞ · λ` (λ = 0 at each substep start) was NaN: the whole cloth went NaN in one step.
#[test]
fn a_huge_distance_compliance_keeps_the_cloth_finite() {
    let mut cloth = ClothSolver::new(ClothConfig::default()).expect("config");
    let a = cloth.add_particle(Vec3::new(0.0, 1.0, 0.0), 0.0).expect("pinned");
    let b = cloth.add_particle(Vec3::new(1.0, 1.0, 0.0), 1.0).expect("free");
    let c = cloth.add_particle(Vec3::new(2.0, 1.0, 0.0), 1.0).expect("free");
    cloth.add_distance(a, b, 1e34).expect("finite compliance");
    cloth.add_distance(b, c, 0.0).expect("rigid link");
    for _ in 0..10 {
        cloth.step(DT).expect("step");
    }
    for p in cloth.particles() {
        assert!(p.position.is_finite() && p.velocity.is_finite(), "{p:?}");
    }
    // The rigid link still holds its length; the infinitely soft one makes no correction.
    let link = (cloth.particles()[c].position - cloth.particles()[b].position).length();
    assert!((link - 1.0).abs() < 0.05, "rigid link length {link}");
}

/// The same overflow in the bending constraint.
#[test]
fn a_huge_bend_compliance_keeps_the_cloth_finite() {
    let mut cloth = ClothSolver::new(ClothConfig::default()).expect("config");
    let p = [
        Vec3::new(0.0, 1.0, 0.0),
        Vec3::new(0.0, 1.0, 1.0),
        Vec3::new(-1.0, 1.0, 0.5),
        Vec3::new(1.0, 1.2, 0.5),
    ];
    let ids: Vec<usize> = p
        .iter()
        .enumerate()
        .map(|(i, &x)| {
            cloth
                .add_particle(x, if i < 2 { 0.0 } else { 1.0 })
                .expect("particle")
        })
        .collect();
    cloth
        .add_bend(ids[0], ids[1], ids[2], ids[3], 1e34)
        .expect("finite compliance");
    for _ in 0..10 {
        cloth.step(DT).expect("step");
    }
    for p in cloth.particles() {
        assert!(p.position.is_finite() && p.velocity.is_finite(), "{p:?}");
    }
}

/// The same overflow in a soft body's tetra volume constraint.
#[test]
fn a_huge_tetra_volume_compliance_keeps_the_soft_body_finite() {
    let positions = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z].map(|p| p + Vec3::Y);
    let mut soft = SoftSolver::new(
        &positions,
        SoftModel::Tetra {
            tets: vec![[0, 1, 2, 3]],
            volume_compliance: 1e34,
        },
        SoftParams::default(),
    )
    .expect("soft body");
    for _ in 0..10 {
        soft.step(DT).expect("step");
    }
    for p in soft.cloth().particles() {
        assert!(p.position.is_finite() && p.velocity.is_finite(), "{p:?}");
    }
}

/// The same overflow in the pose binding's soft pull (`pull_compliance / pose_pull / h²`).
#[test]
fn a_huge_pose_pull_compliance_keeps_the_cloth_finite() {
    let mut cloth = ClothSolver::new(ClothConfig::default()).expect("config");
    cloth
        .add_grid(&ClothGrid {
            origin: Vec3::new(0.0, 1.0, 0.0),
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: 3,
            rows: 3,
            spacing: 0.1,
            particle_mass: 0.05,
            stretch_compliance: 0.0,
            shear_compliance: 1e-4,
            bend_compliance: 1e-2,
        })
        .expect("grid");
    let count = cloth.particles().len();
    // Targets a little below the rest grid, so every pull constraint has a length to correct.
    let targets: Vec<PoseTarget> = cloth
        .particles()
        .iter()
        .map(|p| PoseTarget {
            position: p.position - Vec3::Y * 0.05,
            normal: Vec3::Y,
        })
        .collect();
    cloth
        .set_pose_maps(PoseMaps {
            pose_pull: vec![1.0; count],
            ..PoseMaps::default()
        })
        .expect("maps");
    cloth.set_pose_targets(&targets, &targets).expect("targets");
    cloth
        .set_tuning(ClothTuning {
            pull_compliance: 1e34,
            ..ClothTuning::default()
        })
        .expect("finite pull compliance");
    for _ in 0..10 {
        cloth.step(DT).expect("step");
    }
    for p in cloth.particles() {
        assert!(p.position.is_finite() && p.velocity.is_finite(), "{p:?}");
    }
}
