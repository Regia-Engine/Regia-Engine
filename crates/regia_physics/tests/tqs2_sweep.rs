// SPDX-License-Identifier: MPL-2.0
//! Test-quality sweep 2: deterministic tests for public `regia_physics` functions no test called
//! before (cloth budget cost, pose binding, inverse masses, wind and config setters; capsule
//! headroom; collider removal; the fixed clock; destruction counts and cooked files; 2D gravity).

use std::sync::Arc;
use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_core::math::Vec3;
use regia_ecs::prelude::Entity;
use regia_physics::cloth::lod::budget_cost;
use regia_physics::cloth::{ClothConfig, ClothError, ClothSet, ClothSolver, ClothState, LOD_OFF, PoseMaps};
use regia_physics::destruction::sample::{bridge_asset, wall_asset};
use regia_physics::destruction::{
    Authority, DESTRUCTIBLE_TYPE_PATH, Damage, DamageKind, DestructibleAsset, DestructiblePose,
    DestructionWorld,
};
use regia_physics::{Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, RigidBody, capsule_has_headroom};
use regia_scene::Transform;

// ---------------------------------------------------------------------------------------------
// Cloth
// ---------------------------------------------------------------------------------------------

/// Budget cost: LOD 0 counts every particle, 1 and 2 round half and quarter up, off counts none.
#[test]
fn cloth_budget_cost_per_level() {
    for particles in [0usize, 1, 2, 3, 5, 8, 1001] {
        assert_eq!(budget_cost(particles, 0), particles);
        assert_eq!(budget_cost(particles, 1), particles.div_ceil(2));
        assert_eq!(budget_cost(particles, 2), particles.div_ceil(4));
        assert_eq!(budget_cost(particles, LOD_OFF), 0);
        assert_eq!(budget_cost(particles, u8::MAX), 0);
    }
    assert_eq!(budget_cost(3, 1), 2);
    assert_eq!(budget_cost(5, 2), 2);
    // Monotone: a lower level never costs more.
    for level in 0..LOD_OFF {
        assert!(budget_cost(97, level + 1) <= budget_cost(97, level));
    }
}

/// A free three-particle solver (no constraints) with the first particle pinned.
fn three_particles() -> ClothSolver {
    let mut s = ClothSolver::new(ClothConfig::default()).expect("config");
    s.add_particle(Vec3::new(0.0, 2.0, 0.0), 0.0).expect("pinned");
    s.add_particle(Vec3::new(1.0, 2.0, 0.0), 1.0).expect("free");
    s.add_particle(Vec3::new(2.0, 2.0, 0.0), 2.0).expect("free");
    s
}

/// `reset_to_pose` without a pose returns free particles to where they were added, zeroes every
/// velocity and restarts the blend ramp in `Settling`.
#[test]
fn cloth_reset_to_pose_without_targets_restores_rest_positions() {
    let mut s = three_particles();
    let rest: Vec<Vec3> = s.particles().iter().map(|p| p.position).collect();
    for _ in 0..10 {
        s.step(1.0 / 60.0).expect("step");
    }
    assert!(
        s.particles()[1].position.y < rest[1].y,
        "gravity moved the free particle"
    );
    s.reset_to_pose();
    for (p, r) in s.particles().iter().zip(&rest) {
        assert_eq!(p.position, *r);
        assert_eq!(p.velocity, Vec3::ZERO);
    }
    assert_eq!(s.state(), ClothState::Settling);
    assert_eq!(s.blend(), 0.0);
    assert_eq!(s.min_self_pair_distance(), None, "no self-collision pairs");
}

/// `set_inv_mass` refuses bad indices and values without change; zero pins and stops the particle.
#[test]
fn cloth_set_inv_mass_checks_and_pins() {
    let mut s = three_particles();
    for _ in 0..3 {
        s.step(1.0 / 60.0).expect("step");
    }
    assert!(matches!(
        s.set_inv_mass(3, 1.0),
        Err(ClothError::ParticleOutOfRange { .. })
    ));
    assert!(matches!(
        s.set_inv_mass(1, -1.0),
        Err(ClothError::Negative { .. })
    ));
    assert!(matches!(
        s.set_inv_mass(1, f32::NAN),
        Err(ClothError::NonFinite { .. })
    ));
    assert!(matches!(
        s.set_inv_mass(1, f32::INFINITY),
        Err(ClothError::NonFinite { .. })
    ));
    assert_eq!(s.particles()[1].inv_mass, 1.0);
    assert_ne!(s.particles()[1].velocity, Vec3::ZERO);
    s.set_inv_mass(1, 0.0).expect("pin");
    assert_eq!(s.particles()[1].inv_mass, 0.0);
    assert_eq!(s.particles()[1].velocity, Vec3::ZERO);
    let held = s.particles()[1].position;
    s.step(1.0 / 60.0).expect("step");
    assert_eq!(s.particles()[1].position, held, "a pinned particle never moves");
}

/// Pose maps scale inverse masses (`reach == 0` pins, `mass_scale` divides); later `set_inv_mass`
/// calls update the unscaled base; `clear_pose` restores the base and runs free at full blend.
#[test]
fn cloth_pose_maps_scale_masses_and_clear_pose_restores_them() {
    let mut s = three_particles();
    assert!(s.pose_maps().is_none());
    let maps = PoseMaps {
        reach: vec![100.0, 0.0, 1.0],
        mass_scale: vec![1.0, 1.0, 4.0],
        ..PoseMaps::default()
    };
    s.set_pose_maps(maps.clone()).expect("maps");
    assert_eq!(s.pose_maps(), Some(&maps));
    let inv: Vec<f32> = s.particles().iter().map(|p| p.inv_mass).collect();
    assert_eq!(inv, vec![0.0, 0.0, 0.5]);
    assert_eq!(s.state(), ClothState::Settling);

    s.set_inv_mass(2, 8.0).expect("set");
    assert_eq!(s.particles()[2].inv_mass, 2.0, "scaled by mass_scale 4");
    s.set_inv_mass(1, 3.0).expect("set");
    assert_eq!(s.particles()[1].inv_mass, 0.0, "reach 0 keeps it on its target");

    s.clear_pose();
    assert!(s.pose_maps().is_none());
    let inv: Vec<f32> = s.particles().iter().map(|p| p.inv_mass).collect();
    assert_eq!(inv, vec![0.0, 3.0, 8.0], "the unscaled values set while bound");
    assert_eq!(s.state(), ClothState::Running);
    assert_eq!(s.blend(), 1.0);

    // A map of the wrong length is refused and changes nothing.
    let bad = PoseMaps {
        reach: vec![1.0; 2],
        ..PoseMaps::default()
    };
    assert!(matches!(
        s.set_pose_maps(bad),
        Err(ClothError::MapSizeMismatch { .. })
    ));
    assert!(s.pose_maps().is_none());
}

/// `set_config` refuses an invalid config and keeps the old one; `set_field_wind` refuses
/// non-finite wind.
#[test]
fn cloth_set_config_and_field_wind_validate() {
    let mut s = three_particles();
    let good = ClothConfig {
        substeps: 3,
        ..ClothConfig::default()
    };
    s.set_config(good).expect("valid");
    assert_eq!(s.config().substeps, 3);
    let mut bad = good;
    bad.substeps = 0;
    assert!(s.set_config(bad).is_err());
    assert_eq!(s.config().substeps, 3);
    assert!(s.set_field_wind(Vec3::new(1.0, 0.0, 0.0)).is_ok());
    assert!(matches!(
        s.set_field_wind(Vec3::new(f32::NAN, 0.0, 0.0)),
        Err(ClothError::NonFinite { .. })
    ));
    assert!(matches!(
        s.set_field_wind(Vec3::new(0.0, f32::INFINITY, 0.0)),
        Err(ClothError::NonFinite { .. })
    ));
}

/// The set's global wind is stored only when finite; a refused value leaves the last one.
#[test]
fn cloth_set_global_wind_round_trip() {
    let mut set = ClothSet::new();
    assert_eq!(set.global_wind(), Vec3::ZERO);
    set.insert("a", three_particles());
    set.set_global_wind(Vec3::new(2.0, 0.0, -1.0)).expect("finite");
    assert_eq!(set.global_wind(), Vec3::new(2.0, 0.0, -1.0));
    assert!(set.set_global_wind(Vec3::new(0.0, f32::NAN, 0.0)).is_err());
    assert_eq!(set.global_wind(), Vec3::new(2.0, 0.0, -1.0));
    set.insert("b", three_particles());
    assert_eq!(set.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// Rigid bodies
// ---------------------------------------------------------------------------------------------

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

fn static_box(app: &mut App, half: Vec3, centre: Vec3, sensor: bool) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(centre),
            RigidBody::static_body(),
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                sensor,
                ..Collider::default()
            },
        ))
        .id()
}

fn settle(app: &mut App) {
    for _ in 0..5 {
        app.update();
    }
}

const RADIUS: f32 = 0.3;
const HALF: f32 = 0.6;

/// Headroom under a ceiling whose underside is at y = 2: a small rise fits, a large one does not;
/// a non-positive rise always fits; the floor the capsule rests on and sensors never block; an
/// excluded body does not block.
#[test]
fn capsule_headroom_under_a_ceiling() {
    let mut app = physics_app();
    static_box(
        &mut app,
        Vec3::new(10.0, 0.5, 10.0),
        Vec3::new(0.0, -0.5, 0.0),
        false,
    );
    let ceiling = static_box(
        &mut app,
        Vec3::new(10.0, 0.5, 10.0),
        Vec3::new(0.0, 2.5, 0.0),
        false,
    );
    static_box(
        &mut app,
        Vec3::new(10.0, 0.5, 10.0),
        Vec3::new(30.0, 1.5, 0.0),
        true,
    );
    settle(&mut app);
    let physics = app.world().resource::<PhysicsWorld>();
    // Capsule top at 1.0 + 0.6 + 0.3 = 1.9.
    let center = Vec3::new(0.0, 1.0, 0.0);
    let fits =
        |rise: f32, offset: f32| capsule_has_headroom(physics, RADIUS, HALF, center, rise, offset, None);
    assert!(fits(0.05, 0.02));
    assert!(!fits(0.2, 0.0));
    assert!(!fits(0.05, 0.1), "the skin offset counts");
    assert!(fits(0.0, 5.0));
    assert!(fits(-1.0, 5.0));
    // Resting slightly into the floor, standing up is not blocked by the floor.
    let on_floor = Vec3::new(0.0, HALF + RADIUS - 0.01, 0.0);
    assert!(capsule_has_headroom(
        physics, RADIUS, HALF, on_floor, 0.05, 0.0, None
    ));
    // A sensor slab right above does not block.
    let under_sensor = Vec3::new(30.0, 1.0 - 0.2, 0.0);
    assert!(capsule_has_headroom(
        physics,
        RADIUS,
        HALF,
        under_sensor,
        0.5,
        0.0,
        None
    ));
    // Excluding the ceiling's body lets the capsule rise through it.
    let handle = physics
        .bodies
        .iter()
        .map(|(h, _)| h)
        .find(|h| physics.entity_of_body(*h) == Some(ceiling))
        .expect("ceiling body");
    assert!(capsule_has_headroom(
        physics,
        RADIUS,
        HALF,
        center,
        0.5,
        0.0,
        Some(handle)
    ));
    // A crushed capsule (negative half height) is treated as a sphere.
    assert!(capsule_has_headroom(
        physics, RADIUS, -1.0, center, 0.5, 0.0, None
    ));
}

/// `remove_collider` drops the collider and its entity mapping; removing it again is a no-op.
#[test]
fn remove_collider_unmaps_and_is_idempotent() {
    let mut app = physics_app();
    let a = static_box(&mut app, Vec3::ONE, Vec3::ZERO, false);
    let b = static_box(&mut app, Vec3::ONE, Vec3::new(5.0, 0.0, 0.0), false);
    settle(&mut app);
    let mut physics = app.world_mut().resource_mut::<PhysicsWorld>();
    assert_eq!(physics.stats().colliders, 2);
    let handle = physics
        .colliders
        .iter()
        .map(|(h, _)| h)
        .find(|h| physics.entity_of_collider(*h) == Some(a))
        .expect("collider of a");
    physics.remove_collider(handle);
    assert_eq!(physics.stats().colliders, 1);
    assert_eq!(physics.entity_of_collider(handle), None);
    physics.remove_collider(handle);
    assert_eq!(physics.stats().colliders, 1);
    let rest: Vec<_> = physics
        .colliders
        .iter()
        .map(|(h, _)| physics.entity_of_collider(h))
        .collect();
    assert_eq!(rest, vec![Some(b)]);
}

/// The fixed clock sums whole fixed steps: elapsed time is exactly steps times the fixed step
/// length (a `Duration`, so 1/60 s rounds to whole nanoseconds), and starts at zero.
#[test]
fn fixed_elapsed_secs_counts_fixed_steps() {
    let mut app = physics_app();
    assert_eq!(app.world().resource::<PhysicsWorld>().fixed_elapsed_secs(), 0.0);
    for _ in 0..30 {
        app.update();
    }
    let step = app.world().resource::<FixedTime>().step();
    let physics = app.world().resource::<PhysicsWorld>();
    let steps = physics.stats().steps;
    assert!(steps >= 28, "steps {steps}");
    let expected = (step * u32::try_from(steps).expect("few steps")).as_secs_f64();
    assert_eq!(physics.fixed_elapsed_secs(), expected);
    assert!((expected - steps as f64 / 60.0).abs() < 1.0e-6);
}

/// 2D physics is behind the `physics2d` feature.
#[cfg(feature = "physics2d")]
mod physics2d {
    use regia_app::prelude::*;
    use regia_core::math::{Vec2, Vec3};
    use regia_physics::physics2d::{Collider2d, Physics2dPlugin, PhysicsWorld2d, RigidBody2d};
    use regia_scene::Transform;

    /// Non-positive and NaN length units are clamped to the smallest positive value; others are
    /// stored as given.
    #[test]
    fn physics2d_length_unit_is_clamped_positive() {
        let mut world = PhysicsWorld2d::default();
        for unit in [0.0, -4.0, f32::NAN, f32::NEG_INFINITY] {
            world.set_length_unit(unit);
            assert_eq!(
                world.world.integration_parameters.length_unit,
                f32::MIN_POSITIVE,
                "{unit}"
            );
        }
        world.set_length_unit(2.5);
        assert_eq!(world.world.integration_parameters.length_unit, 2.5);
    }

    fn height_after(gravity: Vec2, updates: usize) -> f32 {
        let mut app = App::new();
        app.add_plugins((MinimalPlugins, Physics2dPlugin));
        enable_fixed_step(&mut app, 60.0);
        app.world_mut()
            .resource_mut::<PhysicsWorld2d>()
            .set_gravity(gravity);
        assert_eq!(app.world().resource::<PhysicsWorld2d>().gravity(), gravity);
        let e = app
            .world_mut()
            .spawn((
                RigidBody2d::default(),
                Collider2d::ball(0.5),
                Transform::from_translation(Vec3::new(0.0, 10.0, 0.0)),
            ))
            .id();
        for _ in 0..updates {
            app.update();
        }
        app.world().get::<Transform>(e).expect("transform").translation.y
    }

    /// Gravity set on the 2D world is what moves bodies: zero gravity leaves a body in place.
    #[test]
    fn physics2d_set_gravity_drives_bodies() {
        assert_eq!(height_after(Vec2::ZERO, 20), 10.0);
        let fallen = height_after(Vec2::new(0.0, -9.81), 20);
        assert!(fallen < 10.0, "{fallen}");
        let risen = height_after(Vec2::new(0.0, 9.81), 20);
        assert!(risen > 10.0, "{risen}");
    }
}

// ---------------------------------------------------------------------------------------------
// Destruction
// ---------------------------------------------------------------------------------------------

/// The sample assets build and validate; a fresh instance has every chunk attached and no broken
/// bond, and a hard blast breaks bonds that `broken_count` then reports.
#[test]
fn destruction_sample_assets_and_broken_count() {
    let wall = wall_asset().expect("wall");
    let bridge = bridge_asset().expect("bridge");
    assert_eq!(wall.chunks.len(), 64);
    assert_eq!(bridge.chunks.len(), 24);
    wall.validate().expect("wall validates");
    bridge.validate().expect("bridge validates");

    let mut world = DestructionWorld::new(Authority::Server);
    let id = world
        .define(Arc::new(wall), DestructiblePose::at([0.0; 3]), 1)
        .expect("define");
    let inst = world.instance(id).expect("instance");
    assert_eq!(inst.broken_count(), 0);
    assert_eq!(inst.attached_count(), 64);
    world
        .damage(Damage {
            target: None,
            kind: DamageKind::Explosion {
                center: [0.0, 1.0, 0.0],
                radius: 1.5,
                impulse: 1.0e7,
            },
        })
        .expect("damage");
    world.step(1.0 / 60.0);
    let inst = world.instance(id).expect("instance");
    let broken = inst.bond_broken().iter().filter(|b| **b).count();
    assert!(broken > 0, "the blast broke bonds");
    assert_eq!(inst.broken_count(), broken);
    assert!(inst.attached_count() < 64);
}

fn cooked(type_path: &str, payload: &[u8]) -> Vec<u8> {
    let header = regia_asset::CookedHeader {
        format_version: regia_asset::COOKED_FORMAT_VERSION,
        type_tag: regia_core::id::TypePathHash::of(type_path),
        asset_id: regia_asset::AssetId(regia_core::id::Guid::from_u128(0x5eed)),
        importer_version: 1,
        dependencies: Vec::new(),
        payload_len: payload.len() as u64,
    };
    regia_asset::encode_cooked_file(&header, payload).expect("encode")
}

/// A cooked destructible reads back equal to its source; another type tag, a bad wrapper,
/// non-UTF-8 and malformed payloads are `Parse` errors, never panics.
#[test]
fn destructible_from_cooked_file_round_trip_and_refusals() {
    let wall = wall_asset().expect("wall");
    let text = wall.to_ron().expect("ron");
    let bytes = cooked(DESTRUCTIBLE_TYPE_PATH, text.as_bytes());
    assert_eq!(DestructibleAsset::from_cooked_file(&bytes).expect("decode"), wall);

    let other = cooked("regia_asset::CpuMesh", text.as_bytes());
    assert_eq!(
        DestructibleAsset::from_cooked_file(&other)
            .map_err(|e| e.code())
            .err(),
        Some("destruction_parse")
    );
    for bad in [
        Vec::new(),
        b"RGCK".to_vec(),
        bytes[..bytes.len() / 2].to_vec(),
        cooked(DESTRUCTIBLE_TYPE_PATH, &[0xff, 0xfe, 0x00]),
        cooked(DESTRUCTIBLE_TYPE_PATH, b"DestructibleAsset(nonsense"),
    ] {
        assert!(DestructibleAsset::from_cooked_file(&bad).is_err());
    }
}

// ---------------------------------------------------------------------------------------------
// Soft bodies
// ---------------------------------------------------------------------------------------------

mod soft {
    use regia_core::math::Vec3;
    use regia_physics::cloth::soft::PRESSURE_RANGE;
    use regia_physics::cloth::{ClothConfig, ClothError, SoftModel, SoftParams, SoftSet, SoftSolver};

    /// The unit corner tetrahedron (rest volume 1/6).
    const TET: [Vec3; 4] = [Vec3::ZERO, Vec3::X, Vec3::Y, Vec3::Z];
    /// Its four faces, counter-clockwise seen from outside.
    const FACES: [[usize; 3]; 4] = [[0, 2, 1], [0, 1, 3], [0, 3, 2], [1, 2, 3]];

    fn params() -> SoftParams {
        SoftParams {
            config: ClothConfig {
                gravity: Vec3::ZERO,
                ..ClothConfig::default()
            },
            ..SoftParams::default()
        }
    }

    fn pressure() -> SoftSolver {
        let model = SoftModel::Pressure {
            triangles: FACES.to_vec(),
            pressure: 1.0,
            volume_compliance: 0.0,
        };
        SoftSolver::new(&TET, model, params()).expect("closed tet surface")
    }

    fn tetra() -> SoftSolver {
        let model = SoftModel::Tetra {
            tets: vec![[0, 1, 2, 3]],
            volume_compliance: 0.0,
        };
        SoftSolver::new(&TET, model, params()).expect("positive tet")
    }

    fn shape_match() -> SoftSolver {
        let model = SoftModel::ShapeMatch {
            clusters: vec![vec![0, 1, 2, 3]],
            stiffness: 0.5,
            beta: 0.0,
        };
        SoftSolver::new(&TET, model, params()).expect("cluster")
    }

    /// Each model reports its stable name and its own queries: rest volume 1/6 for the surface and
    /// the tet, 0 for shape matching; a pressure only for the pressure model; a cluster rotation
    /// (identity at rest) only for shape matching; no inverted tets at rest.
    #[test]
    fn soft_models_name_and_model_specific_queries() {
        let (p, t, m) = (pressure(), tetra(), shape_match());
        assert_eq!(
            (p.model_name(), t.model_name(), m.model_name()),
            ("pressure", "tetra", "shape_match")
        );
        assert!(
            (p.rest_volume() - 1.0 / 6.0).abs() < 1.0e-6,
            "{}",
            p.rest_volume()
        );
        assert!(
            (t.rest_volume() - 1.0 / 6.0).abs() < 1.0e-6,
            "{}",
            t.rest_volume()
        );
        assert_eq!(m.rest_volume(), 0.0);
        assert!((p.volume() - p.rest_volume()).abs() < 1.0e-6);
        assert!((t.volume() - t.rest_volume()).abs() < 1.0e-6);
        assert_eq!(p.pressure(), Some(1.0));
        assert_eq!((t.pressure(), m.pressure()), (None, None));
        assert_eq!(
            (p.inverted_tets(), t.inverted_tets(), m.inverted_tets()),
            (0, 0, 0)
        );
        assert!(p.cluster_rotation(0).is_none() && t.cluster_rotation(0).is_none());
        assert!(m.cluster_rotation(1).is_none());
        let r = m.cluster_rotation(0).expect("cluster 0");
        assert!(r.abs_diff_eq(regia_core::math::Mat3::IDENTITY, 1.0e-5), "{r:?}");
    }

    /// `set_pressure` clamps to the documented range, refuses non-finite values, and is a
    /// `BadParam` on the other models.
    #[test]
    fn set_pressure_clamps_and_refuses() {
        let mut p = pressure();
        p.set_pressure(100.0).expect("clamped");
        assert_eq!(p.pressure(), Some(PRESSURE_RANGE.1));
        p.set_pressure(0.0).expect("clamped");
        assert_eq!(p.pressure(), Some(PRESSURE_RANGE.0));
        assert!(matches!(
            p.set_pressure(f32::NAN),
            Err(ClothError::NonFinite { .. })
        ));
        assert_eq!(p.pressure(), Some(PRESSURE_RANGE.0));
        assert!(matches!(
            tetra().set_pressure(1.0),
            Err(ClothError::BadParam { .. })
        ));
        assert!(matches!(
            shape_match().set_pressure(1.0),
            Err(ClothError::BadParam { .. })
        ));
    }

    /// `step_all` checks `dt` before stepping anything; a resting body without gravity keeps its
    /// volume and the set keeps insertion order with replace-by-name.
    #[test]
    fn soft_set_step_all_checks_dt_first() {
        let mut set = SoftSet::default();
        assert!(set.is_empty());
        set.insert("p", pressure());
        set.insert("t", tetra());
        assert!(set.insert("p", pressure()).is_some(), "same name replaces");
        assert_eq!(set.iter().map(|(n, _)| n).collect::<Vec<_>>(), vec!["p", "t"]);
        let before = set.get("t").expect("t").state_digest();
        assert!(matches!(set.step_all(-0.1), Err(ClothError::Negative { .. })));
        assert!(matches!(
            set.step_all(f32::NAN),
            Err(ClothError::NonFinite { .. })
        ));
        assert_eq!(set.get("t").expect("t").state_digest(), before, "nothing stepped");
        assert_eq!(set.step_all(0.0).expect("zero dt"), 0);
        let steps = set.step_all(1.0 / 30.0).expect("step");
        assert!(steps >= 2, "{steps}");
        for (name, s) in set.iter() {
            assert!((s.volume() - 1.0 / 6.0).abs() < 1.0e-3, "{name}: {}", s.volume());
        }
        assert!(set.remove("p").is_some());
        assert_eq!(set.len(), 1);
    }
}
