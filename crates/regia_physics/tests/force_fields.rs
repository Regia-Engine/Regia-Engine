// SPDX-License-Identifier: MPL-2.0
//! Force fields (ADR-3348): closed-form checks of every effect, the layer mask, impulse pads,
//! and a pinned digest of a 500-body, four-field run.
//!
//! Provenance: original tests; the closed forms are Newton's inverse-square law and the
//! terminal velocity of linear drag, `m g / k`.

use std::time::Duration;

use regia_app::{App, FixedTime, FixedUpdate, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_ecs::prelude::{ChildOf, Entity, IntoScheduleConfigs, Query, With};
use regia_physics::{
    BodySample, Collider, ColliderShape, CollisionLayer, Falloff, FieldKind, FieldMode, FieldPose,
    FieldVolume, ForceField, Mass, PhysicsPlugin, PhysicsSettings, PhysicsWorld, RigidBody, RigidBodyHandle,
    Velocity, probe_fields,
};
use regia_scene::Transform;

const DT: f32 = 1.0 / 60.0;
const G: f32 = 9.81;

fn app_with_gravity(gravity: Vec3) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(PhysicsSettings {
        gravity,
        ..PhysicsSettings::default()
    });
    app.add_plugins(PhysicsPlugin);
    app
}

/// A point-like dynamic body of `mass` kg that collides with nothing (a massless sensor sphere).
fn spawn_mass(app: &mut App, at: Vec3, mass: f32, velocity: Vec3, layer: u16) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::dynamic(),
            Mass::Explicit {
                mass,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(0.5),
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                sensor: true,
                density: 0.0,
                layer: CollisionLayer::new(layer),
                ..Collider::default()
            },
            Velocity {
                linear: velocity,
                angular: Vec3::ZERO,
            },
        ))
        .id()
}

fn spawn_field(app: &mut App, field: ForceField, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((field, Transform::from_translation(at)))
        .id()
}

/// A capsule whose straight-section length is not finite used to panic in `f32::clamp`
/// (`NaN <= NaN` is false) on every physics step that tested the volume.
#[test]
fn a_non_finite_capsule_contains_nothing() {
    let pose = FieldPose::IDENTITY;
    let nan_length = FieldVolume::Capsule {
        half_height: f32::NAN,
        radius: 1.0,
    };
    assert!(!nan_length.contains(&pose, Vec3::ZERO));
    assert!(!nan_length.contains(&pose, Vec3::new(0.0, 3.0, 0.0)));
    let nan_radius = FieldVolume::Capsule {
        half_height: 1.0,
        radius: f32::NAN,
    };
    assert!(!nan_radius.contains(&pose, Vec3::ZERO));
    let infinite = FieldVolume::Capsule {
        half_height: f32::INFINITY,
        radius: f32::INFINITY,
    };
    assert!(!infinite.contains(&pose, Vec3::ZERO));
}

fn state(app: &App, e: Entity) -> (Vec3, Vec3) {
    let handle = app
        .world()
        .get::<RigidBodyHandle>(e)
        .expect("body handle")
        .to_handle();
    let physics = app.world().resource::<PhysicsWorld>();
    let rb = physics.bodies.get(handle).expect("rapier body");
    let p = rb.translation();
    let v = rb.linvel();
    (Vec3::new(p.x, p.y, p.z), Vec3::new(v.x, v.y, v.z))
}

fn sample(position: Vec3, velocity: Vec3, mass: f32) -> BodySample {
    BodySample {
        position,
        velocity,
        angular_velocity: Vec3::ZERO,
        mass,
        inertia: 0.5,
    }
}

#[test]
fn radial_inverse_square_matches_closed_form_at_three_radii() {
    let field = ForceField::new(
        FieldKind::Radial {
            strength: 40.0,
            falloff: Falloff::InverseSquare { min_distance: 0.1 },
        },
        FieldVolume::Global,
    );
    let entity = Entity::from_raw_u32(1).expect("entity");
    for r in [1.0f32, 2.0, 5.0] {
        let probe = probe_fields(
            [(entity, &field, FieldPose::IDENTITY)],
            &sample(Vec3::new(r, 0.0, 0.0), Vec3::ZERO, 3.0),
            0,
            Vec3::ZERO,
        );
        let expected = 40.0 / (r * r);
        // Attraction: toward the center, i.e. along -x for a body at +x.
        assert!(
            (probe.force.x + expected).abs() < 1e-5,
            "r={r}: {:?}",
            probe.force
        );
        assert!(probe.force.y.abs() < 1e-6 && probe.force.z.abs() < 1e-6);
    }
    // Stepped: one step of a 2 kg body at 2 m from a 40 N m² well changes v by F / m * dt.
    let mut app = app_with_gravity(Vec3::ZERO);
    spawn_field(&mut app, field, Vec3::ZERO);
    let body = spawn_mass(&mut app, Vec3::new(2.0, 0.0, 0.0), 2.0, Vec3::ZERO, 0);
    app.update();
    app.update();
    let (_, v) = state(&app, body);
    // Two updates: the first creates the body, so at least one and at most two field steps ran.
    let per_step = 40.0 / 4.0 / 2.0 * DT;
    assert!(v.x < 0.0 && (v.x.abs() > per_step * 0.99), "v = {v:?}");
}

#[test]
fn repel_and_falloffs() {
    let entity = Entity::from_raw_u32(1).expect("entity");
    let repel = ForceField::new(
        FieldKind::Radial {
            strength: -10.0,
            falloff: Falloff::Linear { range: 4.0 },
        },
        FieldVolume::Sphere { radius: 4.0 },
    );
    let at = |r: f32| {
        probe_fields(
            [(entity, &repel, FieldPose::IDENTITY)],
            &sample(Vec3::new(0.0, r, 0.0), Vec3::ZERO, 1.0),
            0,
            Vec3::ZERO,
        )
        .force
    };
    assert!((at(1.0).y - 7.5).abs() < 1e-5);
    assert!((at(3.0).y - 2.5).abs() < 1e-5);
    assert_eq!(at(4.5), Vec3::ZERO, "outside the sphere");
    let curve = Falloff::Curve(vec![(0.0, 1.0), (2.0, 0.5), (4.0, 0.0)]);
    assert!((curve.factor(1.0) - 0.75).abs() < 1e-6);
    assert!((curve.factor(3.0) - 0.25).abs() < 1e-6);
    assert_eq!(curve.factor(9.0), 0.0);
    assert_eq!(curve.factor(-1.0), 1.0);
}

#[test]
fn vortex_orbits_with_bounded_tangential_speed() {
    let mut app = app_with_gravity(Vec3::ZERO);
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Vortex {
                tangential: 20.0,
                inward: 4.0,
                axis: Vec3::Y,
                max_speed: 5.0,
                falloff: Falloff::None,
            },
            FieldVolume::Sphere { radius: 50.0 },
        ),
        Vec3::ZERO,
    );
    let body = spawn_mass(&mut app, Vec3::new(3.0, 0.0, 0.0), 1.0, Vec3::ZERO, 0);
    app.update();
    let mut angle = 0.0f32;
    let mut last = 0.0f32;
    let mut max_tangential = 0.0f32;
    for _ in 0..900 {
        app.update();
        let (p, v) = state(&app, body);
        let radial = Vec3::new(p.x, 0.0, p.z).normalize_or_zero();
        let tangent = Vec3::Y.cross(radial);
        max_tangential = max_tangential.max(v.dot(tangent));
        let a = p.z.atan2(p.x);
        let mut d = a - last;
        if d > std::f32::consts::PI {
            d -= std::f32::consts::TAU;
        } else if d < -std::f32::consts::PI {
            d += std::f32::consts::TAU;
        }
        angle += d;
        last = a;
        assert!(p.y.abs() < 1e-4, "the swirl stays in its plane: {p:?}");
    }
    assert!(max_tangential <= 5.0 * 1.1, "tangential speed {max_tangential}");
    assert!(max_tangential > 4.0, "reaches the limit, got {max_tangential}");
    assert!(angle.abs() > std::f32::consts::TAU, "orbited {angle} rad");
}

#[test]
fn gravity_override_falls_along_it_and_restores_outside() {
    let mut app = app_with_gravity(Vec3::new(0.0, -G, 0.0));
    // Inside y in [5, 15]: gravity points along +x at 5 m/s² and none along y.
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Gravity {
                gravity: Vec3::new(5.0, 0.0, 0.0),
            },
            FieldVolume::Box {
                half_extents: Vec3::new(100.0, 5.0, 100.0),
            },
        ),
        Vec3::new(0.0, 10.0, 0.0),
    );
    let body = spawn_mass(&mut app, Vec3::new(0.0, 20.0, 0.0), 1.0, Vec3::ZERO, 0);
    app.update();
    let (mut prev_p, mut prev) = state(&app, body);
    let mut seen_inside = 0;
    let mut seen_after = 0;
    let mut seen_before = 0;
    for _ in 0..200 {
        app.update();
        let (next_p, v) = state(&app, body);
        let dv = v - prev;
        // The field saw the body where the previous step left it.
        let p = prev_p;
        prev = v;
        prev_p = next_p;
        if p.y > 15.2 {
            assert!(
                (dv.y + G * DT).abs() < 1e-4 && dv.x.abs() < 1e-6,
                "before: {dv:?}"
            );
            seen_before += 1;
        } else if (5.2..14.8).contains(&p.y) {
            assert!(dv.y.abs() < 1e-4, "inside: no y gravity, got {dv:?}");
            assert!((dv.x - 5.0 * DT).abs() < 1e-5, "inside: +x at 5 m/s², got {dv:?}");
            seen_inside += 1;
        } else if p.y < 4.8 {
            assert!((dv.y + G * DT).abs() < 1e-4 && dv.x.abs() < 1e-6, "after: {dv:?}");
            seen_after += 1;
        }
    }
    assert!(
        seen_before > 5 && seen_inside > 20 && seen_after > 5,
        "{seen_before} {seen_inside} {seen_after}"
    );
}

#[test]
fn gravity_override_priority_highest_wins() {
    let entity_a = Entity::from_raw_u32(1).expect("entity");
    let entity_b = Entity::from_raw_u32(2).expect("entity");
    let mut low = ForceField::new(FieldKind::Gravity { gravity: Vec3::X }, FieldVolume::Global);
    low.priority = 1;
    let mut high = ForceField::new(FieldKind::Gravity { gravity: Vec3::Z }, FieldVolume::Global);
    high.priority = 5;
    let probe = probe_fields(
        [
            (entity_a, &high, FieldPose::IDENTITY),
            (entity_b, &low, FieldPose::IDENTITY),
        ],
        &sample(Vec3::ZERO, Vec3::ZERO, 2.0),
        0,
        Vec3::new(0.0, -G, 0.0),
    );
    assert_eq!(probe.gravity, Some((Vec3::Z - Vec3::new(0.0, -G, 0.0)) * 2.0));
}

#[test]
fn linear_drag_reaches_terminal_velocity() {
    let (m, k) = (2.0f32, 4.0f32);
    let mut app = app_with_gravity(Vec3::new(0.0, -G, 0.0));
    spawn_field(
        &mut app,
        ForceField::new(FieldKind::LinearDrag { coefficient: k }, FieldVolume::Global),
        Vec3::ZERO,
    );
    let body = spawn_mass(&mut app, Vec3::new(0.0, 1000.0, 0.0), m, Vec3::ZERO, 0);
    for _ in 0..900 {
        app.update();
    }
    let terminal = m * G / k;
    let v = -state(&app, body).1.y;
    assert!(
        (v / terminal - 1.0).abs() < 0.02,
        "v = {v}, terminal = {terminal}"
    );
}

#[test]
fn acceleration_mode_drag_ignores_mass() {
    let mut app = app_with_gravity(Vec3::new(0.0, -G, 0.0));
    let mut field = ForceField::new(FieldKind::LinearDrag { coefficient: 2.0 }, FieldVolume::Global);
    field.mode = FieldMode::Acceleration;
    spawn_field(&mut app, field, Vec3::ZERO);
    let light = spawn_mass(&mut app, Vec3::new(-5.0, 1000.0, 0.0), 0.5, Vec3::ZERO, 0);
    let heavy = spawn_mass(&mut app, Vec3::new(5.0, 1000.0, 0.0), 50.0, Vec3::ZERO, 0);
    for _ in 0..900 {
        app.update();
    }
    let terminal = G / 2.0;
    for e in [light, heavy] {
        let v = -state(&app, e).1.y;
        assert!((v / terminal - 1.0).abs() < 0.02, "v = {v}");
    }
}

#[test]
fn layer_mask_excludes() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let mut field = ForceField::new(
        FieldKind::Directional {
            vector: Vec3::new(10.0, 0.0, 0.0),
        },
        FieldVolume::Global,
    );
    field.mask = 1 << 2;
    spawn_field(&mut app, field, Vec3::ZERO);
    let hit = spawn_mass(&mut app, Vec3::new(0.0, 0.0, 0.0), 1.0, Vec3::ZERO, 2);
    let miss0 = spawn_mass(&mut app, Vec3::new(0.0, 5.0, 0.0), 1.0, Vec3::ZERO, 0);
    let miss1 = spawn_mass(&mut app, Vec3::new(0.0, 10.0, 0.0), 1.0, Vec3::ZERO, 1);
    for _ in 0..60 {
        app.update();
    }
    assert!(state(&app, hit).1.x > 5.0, "layer 2 is pushed");
    assert_eq!(state(&app, miss0).1, Vec3::ZERO);
    assert_eq!(state(&app, miss1).1, Vec3::ZERO);
}

#[test]
fn impulse_pad_changes_velocity_by_j_over_m_once_per_entry() {
    let mut app = app_with_gravity(Vec3::ZERO);
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::ImpulsePad {
                impulse: Vec3::new(0.0, 6.0, 0.0),
            },
            FieldVolume::Box {
                half_extents: Vec3::new(1.0, 500.0, 500.0),
            },
        ),
        Vec3::new(1.0, 0.0, 0.0),
    );
    let body = spawn_mass(
        &mut app,
        Vec3::new(-1.0, 0.0, 0.0),
        2.0,
        Vec3::new(1.0, 0.0, 0.0),
        0,
    );
    for _ in 0..240 {
        app.update();
    }
    let (p, v) = state(&app, body);
    assert_eq!(
        app.world().resource::<regia_physics::FieldScratch>().pad_entries,
        1
    );
    assert!(p.x > 2.5, "passed through the pad: {p:?}");
    assert!((v.y - 3.0).abs() < 1e-5, "J / m = 3, got {}", v.y);
    // A second entry, from the other side.
    let handle = app.world().get::<RigidBodyHandle>(body).copied().expect("handle");
    let _ = handle;
    app.world_mut()
        .get_mut::<Velocity>(body)
        .expect("velocity")
        .linear = Vec3::new(-1.0, 3.0, 0.0);
    for _ in 0..240 {
        app.update();
    }
    let (p, v) = state(&app, body);
    assert!(p.x < -0.5, "came back out: {p:?}");
    assert!(
        (v.y - 6.0).abs() < 1e-5,
        "second entry adds J / m once more, got {}",
        v.y
    );
}

#[test]
fn rotated_box_and_capsule_volumes() {
    let pose = FieldPose {
        position: Vec3::new(0.0, 5.0, 0.0),
        rotation: Quat::from_rotation_z(std::f32::consts::FRAC_PI_2),
    };
    // A long thin box along local X becomes vertical after a quarter turn about Z.
    let tall = FieldVolume::Box {
        half_extents: Vec3::new(4.0, 0.5, 0.5),
    };
    assert!(tall.contains(&pose, Vec3::new(0.0, 8.5, 0.0)));
    assert!(!tall.contains(&pose, Vec3::new(3.0, 5.0, 0.0)));
    let capsule = FieldVolume::Capsule {
        half_height: 2.0,
        radius: 1.0,
    };
    assert!(capsule.contains(&FieldPose::IDENTITY, Vec3::new(0.0, 2.9, 0.0)));
    assert!(!capsule.contains(&FieldPose::IDENTITY, Vec3::new(0.0, 3.1, 0.0)));
    assert!(capsule.contains(&FieldPose::IDENTITY, Vec3::new(0.9, 0.0, 0.0)));
}

#[test]
fn disabled_field_and_non_dynamic_bodies_are_untouched() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let mut off = ForceField::new(
        FieldKind::Directional {
            vector: Vec3::X * 10.0,
        },
        FieldVolume::Global,
    );
    off.enabled = false;
    spawn_field(&mut app, off, Vec3::ZERO);
    let body = spawn_mass(&mut app, Vec3::ZERO, 1.0, Vec3::ZERO, 0);
    for _ in 0..30 {
        app.update();
    }
    assert_eq!(state(&app, body).1, Vec3::ZERO);
}

/// Fields as a scene would save them: the components reflect.
#[test]
fn field_components_are_registered_for_reflection() {
    let app = app_with_gravity(Vec3::ZERO);
    let registry = app
        .world()
        .resource::<regia_ecs::prelude::AppTypeRegistry>()
        .read();
    for name in [
        "regia_physics::fields::ForceField",
        "regia_physics::fields::FieldKind",
        "regia_physics::fields::FieldVolume",
        "regia_physics::fields::FieldMode",
        "regia_physics::fields::Falloff",
    ] {
        assert!(
            registry.get_with_type_path(name).is_some(),
            "{name} not registered"
        );
    }
}

// ---------------------------------------------------------------------------
// The pinned run
// ---------------------------------------------------------------------------

/// 500 sensor-sphere bodies in a 10 x 10 x 5 grid, four fields, 600 steps; folds every step's
/// state digest (FNV-1a).
fn run_pinned() -> u64 {
    let mut app = app_with_gravity(Vec3::new(0.0, -G, 0.0));
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Directional {
                vector: Vec3::new(3.0, 1.0, 0.0),
            },
            FieldVolume::Box {
                half_extents: Vec3::new(6.0, 10.0, 6.0),
            },
        ),
        Vec3::new(-6.0, 5.0, 0.0),
    );
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Radial {
                strength: 30.0,
                falloff: Falloff::InverseSquare { min_distance: 1.0 },
            },
            FieldVolume::Global,
        ),
        Vec3::new(0.0, 4.0, 0.0),
    );
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Vortex {
                tangential: 6.0,
                inward: 1.0,
                axis: Vec3::Y,
                max_speed: 4.0,
                falloff: Falloff::Linear { range: 12.0 },
            },
            FieldVolume::Sphere { radius: 12.0 },
        ),
        Vec3::new(8.0, 4.0, 0.0),
    );
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::LinearDrag { coefficient: 0.5 },
            FieldVolume::Capsule {
                half_height: 6.0,
                radius: 8.0,
            },
        ),
        Vec3::new(0.0, 4.0, 0.0),
    );
    for i in 0..10 {
        for j in 0..10 {
            for k in 0..5 {
                let at = Vec3::new(i as f32 * 1.5 - 7.0, k as f32 * 2.0 + 0.5, j as f32 * 1.5 - 7.0);
                let v = Vec3::new((i as f32 - 5.0) * 0.1, (k as f32) * 0.05, (j as f32 - 5.0) * 0.1);
                spawn_mass(&mut app, at, 1.0 + (i + j + k) as f32 * 0.1, v, 0);
            }
        }
    }
    let mut fold = 0xcbf2_9ce4_8422_2325u64;
    for _ in 0..600 {
        app.update();
        let digest = app.world().resource::<PhysicsWorld>().state_digest();
        fold ^= digest;
        fold = fold.wrapping_mul(0x0000_0100_0000_01b3);
    }
    fold
}

/// Digest pin for the 500-body run (Linux x86_64 only, like the other pins).
#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
const PINNED_DIGEST: u64 = 0xaaaf_0cf8_c367_d65f;

#[test]
fn five_hundred_bodies_four_fields_give_a_fixed_digest() {
    let a = run_pinned();
    let b = run_pinned();
    assert_eq!(a, b, "two runs in one process must agree");
    eprintln!("FORCE_FIELD_DIGEST {a:#018x}");
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    assert_eq!(a, PINNED_DIGEST, "new pin: {a:#018x}");
}

/// Corr-physics audit: fields wrote the velocity of disabled dynamic bodies too (frozen by
/// streaming, parked chassis), so a frozen body piled up `a dt` every step and flew off when it
/// came back.
#[test]
fn a_frozen_body_gains_no_velocity_while_it_waits() {
    use regia_scene::streaming::StreamFrozen;
    let mut app = app_with_gravity(Vec3::ZERO);
    let mut wind = ForceField::new(
        FieldKind::Directional {
            vector: Vec3::new(5.0, 0.0, 0.0),
        },
        FieldVolume::Global,
    );
    wind.mode = FieldMode::Acceleration;
    spawn_field(&mut app, wind, Vec3::ZERO);
    let body = spawn_mass(&mut app, Vec3::ZERO, 1.0, Vec3::ZERO, 0);
    app.update();
    app.update();
    app.world_mut().entity_mut(body).insert(StreamFrozen);
    app.update();
    let (_, frozen_at) = state(&app, body);
    for _ in 0..60 {
        app.update();
    }
    let (_, after) = state(&app, body);
    assert_eq!(after, frozen_at, "a frozen body's velocity moved");
    app.world_mut().entity_mut(body).remove::<StreamFrozen>();
    app.update();
    let (_, back) = state(&app, body);
    assert!(
        back.x <= frozen_at.x + 2.0 * 5.0 * DT,
        "came back at {back:?} after freezing at {frozen_at:?}"
    );
}

/// Corr-physics audit: the vortex speed limit measured along the counterclockwise tangent, so a
/// clockwise swirl (negative `tangential`) already past `max_speed` was still pushed faster.
#[test]
fn a_clockwise_vortex_keeps_its_speed_limit() {
    let field = |tangential: f32| {
        let mut f = ForceField::new(
            FieldKind::Vortex {
                tangential,
                inward: 0.0,
                axis: Vec3::Y,
                max_speed: 3.0,
                falloff: Falloff::None,
            },
            FieldVolume::Global,
        );
        f.mode = FieldMode::Acceleration;
        f
    };
    // At (1, 0, 0) the counterclockwise tangent (Y × X) is -Z.
    let ccw_tangent = Vec3::new(0.0, 0.0, -1.0);
    for sign in [1.0f32, -1.0] {
        let f = field(5.0 * sign);
        let swirl = ccw_tangent * sign;
        // Twice the limit along the swirl: the push reverses.
        let fast = f
            .effect(&FieldPose::IDENTITY, &sample(Vec3::X, swirl * 6.0, 1.0))
            .expect("inside");
        assert!(
            fast.accel.dot(swirl) < 0.0,
            "sign {sign}: {:?} speeds it up",
            fast.accel
        );
        // At rest: full push along the swirl.
        let rest = f
            .effect(&FieldPose::IDENTITY, &sample(Vec3::X, Vec3::ZERO, 1.0))
            .expect("inside");
        assert!(
            (rest.accel.dot(swirl) - 5.0).abs() < 1e-5,
            "sign {sign}: {:?}",
            rest.accel
        );
    }
}

/// Corr-physics audit: fields were folded in entity-id order, which parallel spawning makes
/// thread-timing dependent (ADR-1347); the fold (and the gravity tie-break) follows the order the
/// fields arrived, latest first, whatever ids they sit on.
#[test]
fn equal_priority_gravity_ties_go_to_the_latest_spawned_field() {
    let run = |first_on_lower_id: bool| {
        let mut app = app_with_gravity(Vec3::ZERO);
        // Two entities first, the fields inserted later: arrival order is insertion order, so
        // it can disagree with id order on purpose.
        let a = app.world_mut().spawn(Transform::IDENTITY).id();
        let b = app.world_mut().spawn(Transform::IDENTITY).id();
        let (lower, higher) = if a.to_bits() < b.to_bits() { (a, b) } else { (b, a) };
        let (first, second) = if first_on_lower_id {
            (lower, higher)
        } else {
            (higher, lower)
        };
        app.world_mut().entity_mut(first).insert(ForceField::new(
            FieldKind::Gravity { gravity: Vec3::X },
            FieldVolume::Global,
        ));
        app.world_mut().entity_mut(second).insert(ForceField::new(
            FieldKind::Gravity { gravity: Vec3::Z },
            FieldVolume::Global,
        ));
        let body = spawn_mass(&mut app, Vec3::ZERO, 1.0, Vec3::ZERO, 0);
        app.update();
        app.update();
        state(&app, body).1
    };
    // By id, `true` folds the first field first and `false` the second: the old order gave
    // different winners. By arrival both give the latest field.
    let a = run(true);
    let b = run(false);
    assert!(a.z > 0.0 && a.x == 0.0, "the latest field wins: {a:?}");
    assert_eq!(a, b, "the winner followed entity ids");
}

/// A compound body: a dynamic body whose only collider sits on a child entity.
fn spawn_compound_body(app: &mut App, at: Vec3, child_layer: u16) -> Entity {
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody::dynamic(),
            Mass::Explicit {
                mass: 2.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(0.5),
            },
            Velocity {
                linear: Vec3::ZERO,
                angular: Vec3::ZERO,
            },
        ))
        .id();
    app.world_mut().spawn((
        Transform::IDENTITY,
        ChildOf(body),
        Collider {
            shape: ColliderShape::Sphere { radius: 0.1 },
            density: 0.0,
            layer: CollisionLayer::new(child_layer),
            ..Collider::default()
        },
    ));
    body
}

#[test]
fn compound_bodies_follow_child_collider_layers() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let body = spawn_compound_body(&mut app, Vec3::ZERO, 2);
    // The layer-2 field pulls +x; the layer-0 field pushes -x. Only the layer-2 field may
    // touch a body whose (child) collider is on layer 2.
    let mut on_layer2 = ForceField::new(
        FieldKind::Directional {
            vector: Vec3::new(10.0, 0.0, 0.0),
        },
        FieldVolume::Global,
    );
    on_layer2.mask = 1 << 2;
    spawn_field(&mut app, on_layer2, Vec3::new(0.0, 100.0, 0.0));
    let mut on_layer0 = ForceField::new(
        FieldKind::Directional {
            vector: Vec3::new(-10.0, 0.0, 0.0),
        },
        FieldVolume::Global,
    );
    on_layer0.mask = 1 << 0;
    spawn_field(&mut app, on_layer0, Vec3::new(0.0, -100.0, 0.0));
    // One warm-up step: a body spawned this frame has no mass in the rapier world until the
    // deferred property sync lands, and Force-mode fields need it.
    app.update();
    app.update();
    let v = state(&app, body).1;
    assert!(
        (v.x - 10.0 / 2.0 * DT).abs() < 1e-5,
        "the layer-2 field pushes the compound body: {v:?}"
    );
    assert_eq!(v.y, 0.0);
    assert_eq!(v.z, 0.0);
}

fn move_root(mut roots: Query<&mut Transform, With<regia_scene::components::Name>>) {
    for mut t in &mut roots {
        t.translation.x += 2.0;
    }
}

#[test]
fn a_field_on_a_child_uses_the_current_fixed_step_pose() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let root = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::ZERO),
            regia_scene::components::Name::new("moving_root"),
        ))
        .id();
    app.add_systems(FixedUpdate, move_root.before(regia_physics::apply_force_fields));
    // The field lives on a child 5 m to the right of a root that moves +2 m in x every
    // fixed step before the fields apply.
    app.world_mut().spawn((
        ForceField::new(
            FieldKind::Radial {
                strength: 40.0,
                falloff: Falloff::InverseSquare { min_distance: 0.1 },
            },
            FieldVolume::Global,
        ),
        Transform::from_translation(Vec3::new(5.0, 0.0, 0.0)),
        ChildOf(root),
    ));
    let body = spawn_mass(&mut app, Vec3::new(10.0, 0.0, 0.0), 1.0, Vec3::ZERO, 0);
    // Warm-up step: a body spawned this frame has no mass in the rapier world until the
    // deferred property sync lands, and Force-mode fields need it. The root moves 0 -> 2.
    app.update();
    // This step moves the root 2 -> 4 before the fields apply, so the child's composed center
    // is at x = 9: exactly 1 m from the body (x = 10), attracting it toward -x with the full
    // 40 N. A stale pose (last frame's GlobalTransform, or the child's local Transform alone)
    // puts the center elsewhere and gives a different pull.
    app.update();
    let v = state(&app, body).1;
    assert!(
        (v.x - (-40.0 * DT)).abs() < 1e-4,
        "the field acts at the current composed pose: v = {v:?}"
    );
    assert_eq!(v.y, 0.0);
    assert_eq!(v.z, 0.0);
}

#[test]
fn angular_drag_matches_closed_form_in_both_modes_and_decays_stepwise() {
    // Probe: torque = -k ω (Force) and -k ω I (Acceleration).
    let spinning = BodySample {
        position: Vec3::ZERO,
        velocity: Vec3::ZERO,
        angular_velocity: Vec3::new(0.0, 2.0, 0.0),
        mass: 3.0,
        inertia: 0.5,
    };
    for (mode, expected_torque) in [
        (FieldMode::Force, -1.0f32),     // -k ω = -0.5 * 2, inertia cancels
        (FieldMode::Acceleration, -0.5), // -k ω I = -0.5 * 2 * 0.5
    ] {
        let mut field = ForceField::new(FieldKind::AngularDrag { coefficient: 0.5 }, FieldVolume::Global);
        field.mode = mode;
        let entity = Entity::from_raw_u32(1).expect("entity");
        let probe = probe_fields([(entity, &field, FieldPose::IDENTITY)], &spinning, 0, Vec3::ZERO);
        assert!(
            (probe.torque.y - expected_torque).abs() < 1e-5,
            "{mode:?}: torque = {:?}",
            probe.torque
        );
    }
    // Stepped: in Acceleration mode ω shrinks by (1 - k dt) every fixed step.
    let mut app = app_with_gravity(Vec3::ZERO);
    let mut field = ForceField::new(FieldKind::AngularDrag { coefficient: 0.5 }, FieldVolume::Global);
    field.mode = FieldMode::Acceleration;
    spawn_field(&mut app, field, Vec3::ZERO);
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 0.0)),
            RigidBody::dynamic(),
            Mass::Explicit {
                mass: 1.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(0.5),
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.05 },
                sensor: true,
                density: 0.0,
                layer: CollisionLayer::new(0),
                ..Collider::default()
            },
            Velocity {
                linear: Vec3::ZERO,
                angular: Vec3::new(0.0, 3.0, 0.0),
            },
        ))
        .id();
    for _ in 0..10 {
        app.update();
    }
    let factor = 1.0 - 0.5 * DT;
    let expected = 3.0 * factor.powi(10);
    let handle = app
        .world()
        .get::<RigidBodyHandle>(body)
        .expect("body handle")
        .to_handle();
    let w = app
        .world()
        .resource::<PhysicsWorld>()
        .bodies
        .get(handle)
        .expect("rapier body")
        .angvel();
    assert!(
        (w.y - expected).abs() < 1e-4,
        "ω decays by (1 - k dt) per step: got {:?}, expected {expected}",
        w
    );
}

#[test]
fn gravity_override_with_non_finite_level_gravity_replaces_the_last_finite() {
    let mut app = app_with_gravity(Vec3::ZERO);
    // A NaN level gravity makes the step keep its last finite gravity (the -9.81 default);
    // the override field must cancel exactly that, not zero.
    app.insert_resource(regia_scene::level::LevelSettings {
        gravity: Vec3::new(f32::NAN, f32::NAN, f32::NAN),
        ..regia_scene::level::LevelSettings::default()
    });
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Gravity {
                gravity: Vec3::new(0.0, 2.0, 0.0),
            },
            FieldVolume::Global,
        ),
        Vec3::ZERO,
    );
    let body = spawn_mass(&mut app, Vec3::ZERO, 1.0, Vec3::ZERO, 0);
    app.update();
    let v = state(&app, body).1;
    assert!(
        (v.y - 2.0 * DT).abs() < 1e-5,
        "the body feels exactly the override gravity: {v:?}"
    );
    assert_eq!(v.x, 0.0);
    assert_eq!(v.z, 0.0);
}

#[test]
fn a_readded_impulse_pad_forgets_previous_occupancy() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let field = ForceField::new(
        FieldKind::ImpulsePad {
            impulse: Vec3::new(0.0, 6.0, 0.0),
        },
        FieldVolume::Sphere { radius: 2.0 },
    );
    let body = spawn_mass(&mut app, Vec3::ZERO, 2.0, Vec3::ZERO, 0);
    // Warm-up step so the body's mass properties exist in the rapier world (a pad in Force
    // mode divides by mass) — the pad is added after it.
    app.update();
    let pad = spawn_field(&mut app, field.clone(), Vec3::ZERO);
    app.update();
    assert_eq!(
        app.world().resource::<regia_physics::FieldScratch>().pad_entries,
        1
    );
    app.world_mut().entity_mut(pad).remove::<ForceField>();
    app.update();
    app.world_mut().entity_mut(pad).insert(field);
    app.update();
    assert_eq!(
        app.world().resource::<regia_physics::FieldScratch>().pad_entries,
        2,
        "the re-added pad fires its entry impulse again"
    );
    let (_, v) = state(&app, body);
    assert!((v.y - 6.0).abs() < 1e-5, "J / m applied twice: {v:?}");
}

/// Review rev-kimi-20 finding 1: removing an unrelated field must not make a pad forget that a
/// body is still standing in it (every jump pad in a level used to re-launch its occupant when
/// any field was despawned or unloaded).
#[test]
fn removing_another_field_does_not_refire_a_pad() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let body = spawn_mass(&mut app, Vec3::ZERO, 2.0, Vec3::ZERO, 0);
    app.update();
    let pad = ForceField::new(
        FieldKind::ImpulsePad {
            impulse: Vec3::new(0.0, 0.12, 0.0),
        },
        FieldVolume::Sphere { radius: 2.0 },
    );
    spawn_field(&mut app, pad, Vec3::ZERO);
    let other = spawn_field(
        &mut app,
        ForceField::new(FieldKind::Directional { vector: Vec3::ZERO }, FieldVolume::Global),
        Vec3::new(50.0, 0.0, 0.0),
    );
    let third = spawn_field(
        &mut app,
        ForceField::new(FieldKind::Directional { vector: Vec3::ZERO }, FieldVolume::Global),
        Vec3::new(-50.0, 0.0, 0.0),
    );
    app.update();
    let entries = |app: &App| app.world().resource::<regia_physics::FieldScratch>().pad_entries;
    assert_eq!(entries(&app), 1);
    app.update();
    assert_eq!(entries(&app), 1);
    app.world_mut().entity_mut(other).remove::<ForceField>();
    app.update();
    let (_, v) = state(&app, body);
    assert_eq!(
        entries(&app),
        1,
        "removing another field re-fired the pad: v = {v:?}"
    );
    assert!((v.y - 0.06).abs() < 1e-5, "J / m applied once: {v:?}");
    // Despawning another field's entity is a removal too.
    app.world_mut().despawn(third);
    app.update();
    assert_eq!(entries(&app), 1, "despawning another field re-fired the pad");
}

/// Review rev-kimi-20 finding 4: a body whose only collider is on a layer index past 31 is on
/// no field layer (like `ForceField::affects_layer`), not on the no-collider fallback layer 0.
#[test]
fn a_collider_past_layer_31_is_not_the_no_collider_fallback() {
    let mut app = app_with_gravity(Vec3::ZERO);
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::Directional {
                vector: Vec3::new(10.0, 0.0, 0.0),
            },
            FieldVolume::Global,
        ),
        Vec3::ZERO,
    );
    let out_of_range = spawn_mass(&mut app, Vec3::ZERO, 1.0, Vec3::ZERO, 40);
    // Control: a body with no collider at all still falls back to layer 0 and is pushed.
    let bare = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 5.0, 0.0)),
            RigidBody::dynamic(),
            Mass::Explicit {
                mass: 1.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::splat(0.5),
            },
        ))
        .id();
    for _ in 0..30 {
        app.update();
    }
    assert!(
        state(&app, bare).1.x > 1.0,
        "a body without colliders is on layer 0"
    );
    assert_eq!(
        state(&app, out_of_range).1,
        Vec3::ZERO,
        "a collider on layer 40 matches no field mask"
    );
}

/// rc1 physics review, finding 10: pad occupancy was only kept for dynamic bodies, so a body that
/// stood in a pad while kinematic and was switched to dynamic there got the entry impulse on its
/// first dynamic step (a cutscene prop or a ragdoll going limp on a jump pad was launched). A
/// non-dynamic body's occupancy is tracked by containment now; the switch is not an entry, and a
/// body that leaves and comes back still gets its impulse.
#[test]
fn a_kinematic_to_dynamic_switch_inside_a_pad_is_not_an_entry() {
    let mut app = app_with_gravity(Vec3::ZERO);
    let body = spawn_mass(&mut app, Vec3::ZERO, 2.0, Vec3::ZERO, 0);
    app.update();
    *app.world_mut().get_mut::<RigidBody>(body).expect("body") = RigidBody::kinematic();
    app.update();
    spawn_field(
        &mut app,
        ForceField::new(
            FieldKind::ImpulsePad {
                impulse: Vec3::new(0.0, 6.0, 0.0),
            },
            FieldVolume::Sphere { radius: 2.0 },
        ),
        Vec3::ZERO,
    );
    for _ in 0..2 {
        app.update();
    }
    let entries = |app: &App| app.world().resource::<regia_physics::FieldScratch>().pad_entries;
    assert_eq!(entries(&app), 0, "a kinematic body gets no impulse");
    *app.world_mut().get_mut::<RigidBody>(body).expect("body") = RigidBody::dynamic();
    for _ in 0..3 {
        app.update();
    }
    let (_, v) = state(&app, body);
    assert_eq!(
        entries(&app),
        0,
        "switching to dynamic inside the pad is not an entry: v = {v:?}"
    );
    assert_eq!(v, Vec3::ZERO);
    // Leave the pad while kinematic, come back dynamic under its own speed: that is an entry.
    *app.world_mut().get_mut::<RigidBody>(body).expect("body") = RigidBody::kinematic();
    app.world_mut()
        .get_mut::<Transform>(body)
        .expect("transform")
        .translation = Vec3::new(10.0, 0.0, 0.0);
    for _ in 0..2 {
        app.update();
    }
    let (p, _) = state(&app, body);
    assert!(
        (p.x - 10.0).abs() < 1e-4,
        "the kinematic body left the pad: {p:?}"
    );
    *app.world_mut().get_mut::<RigidBody>(body).expect("body") = RigidBody::dynamic();
    *app.world_mut().get_mut::<Velocity>(body).expect("velocity") = Velocity {
        linear: Vec3::new(-30.0, 0.0, 0.0),
        angular: Vec3::ZERO,
    };
    for _ in 0..30 {
        app.update();
    }
    let (_, v) = state(&app, body);
    assert_eq!(entries(&app), 1, "coming back is an entry: v = {v:?}");
    assert!((v.y - 3.0).abs() < 1e-5, "J / m once: {v:?}");
}
