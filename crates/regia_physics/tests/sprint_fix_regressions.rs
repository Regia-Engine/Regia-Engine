// SPDX-License-Identifier: MPL-2.0
//! Regression tests for the resource-exhaustion findings of the physics review
//! (platformer box size, 2D polyline guard, cloth step counts, solver/CCD ceilings,
//! destruction damage queue). Re-lands the named B01 hunks of
//! lane/sprint-fix-auto-ferrite_physics (f0ee9cc29).

use regia_core::math::IVec2;
use regia_physics::cloth::{ClothConfig, ClothSolver, MAX_STEP_FIXED_COUNT};
use regia_physics::destruction::{
    Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructiblePose, DestructionError,
    DestructionMaterial, DestructionWorld, GridSupport, MAX_QUEUED_DAMAGE,
};
use regia_physics::platformer2d::{PlatformerWorld, Tile};

#[test]
fn huge_platformer_actor_size_is_clamped_and_returns() {
    let mut world = PlatformerWorld::new();
    world.add_static_tiled_layer(vec![Tile::Solid; 10], 8, 8, 10, IVec2::ZERO, 1);
    // A one-billion-pixel box used to visit ~10^9 cells in `over`; it is now clamped and the
    // scan is bounded by the layer.
    let actor = world.add_actor(IVec2::new(0, 100), 1_000_000_000, 1);
    let solid = world.add_solid(IVec2::new(0, 200), 1_000_000_000, 1_000_000_000);
    world.solid_move(solid, 1.0, 0.0);
    assert!(world.actor_pos(actor).is_some());
}

#[test]
fn cloth_step_fixed_count_is_capped() {
    let mut solver = ClothSolver::new(ClothConfig::default()).expect("config");
    assert_eq!(solver.step_fixed(u32::MAX), MAX_STEP_FIXED_COUNT);
    let config = ClothConfig {
        max_steps_per_call: MAX_STEP_FIXED_COUNT + 1,
        ..ClothConfig::default()
    };
    assert!(config.validate().is_err());
}

#[test]
fn physics_settings_iteration_ceilings_are_documented_constants() {
    assert_eq!(regia_physics::world::MAX_SOLVER_ITERATIONS, 64);
    assert_eq!(regia_physics::world::MAX_CCD_SUBSTEPS, 16);
}

#[test]
fn the_damage_queue_is_capped() {
    let mut world = DestructionWorld::new(Authority::Server);
    let asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "g".into(),
        counts: [2, 2, 1],
        cell: [1.0; 3],
        density: 1.0,
        strength_per_area: 1.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .expect("wall");
    world
        .define(std::sync::Arc::new(asset), DestructiblePose::at([0.0; 3]), 7)
        .expect("define");
    let hit = || Damage {
        target: None,
        kind: DamageKind::Explosion {
            center: [0.0; 3],
            radius: 1.0,
            impulse: 1.0,
        },
    };
    for _ in 0..MAX_QUEUED_DAMAGE {
        world.damage(hit()).expect("room");
    }
    assert!(matches!(
        world.damage(hit()),
        Err(DestructionError::InvalidDamage(_))
    ));
    assert_eq!(world.queued_damage(), MAX_QUEUED_DAMAGE);
}

/// The rest of lane/sprint-fix-auto-ferrite_physics (f0ee9cc29) that the B01 re-land left out:
/// the negative-zero cylinder, runtime cloth particle caps, destructible string caps and the
/// replica event-batch cap.
mod remainder {
    use regia_core::math::Vec3;
    use regia_physics::cloth::asset::MAX_ASSET_PARTICLES;
    use regia_physics::cloth::{ClothConfig, ClothError, ClothGrid, ClothRope, ClothSolver};
    use regia_physics::components::ColliderShape;
    use regia_physics::destruction::{
        Authority, BoxGrid, BreakEvent, BreakKind, DestructibleAsset, DestructionError, DestructionMaterial,
        DestructionWorld, GridSupport, MAX_DESTRUCTIBLE_STRING, MAX_EVENTS_PER_CALL,
    };
    use regia_physics::step::shape_to_shared_shape;

    #[test]
    fn negative_zero_cylinder_is_rejected_not_aborted() {
        let neg = ColliderShape::Cylinder {
            half_height: 0.5,
            radius: -0.0,
            axis: Default::default(),
        };
        assert!(shape_to_shared_shape(&neg).is_none());
        let neg = ColliderShape::Cylinder {
            half_height: -0.0,
            radius: 0.5,
            axis: Default::default(),
        };
        assert!(shape_to_shared_shape(&neg).is_none());
    }

    #[test]
    fn runtime_cloth_grid_and_rope_respect_the_particle_cap() {
        let mut s = ClothSolver::new(ClothConfig::default()).expect("config");
        let grid = ClothGrid {
            origin: Vec3::ZERO,
            u_axis: Vec3::X,
            v_axis: Vec3::Z,
            columns: 100_000,
            rows: 100_000,
            spacing: 0.1,
            particle_mass: 1.0,
            stretch_compliance: 0.0,
            shear_compliance: 0.0,
            bend_compliance: 0.0,
        };
        assert!(matches!(s.add_grid(&grid), Err(ClothError::Degenerate { .. })));
        let rope = ClothRope {
            start: Vec3::ZERO,
            end: Vec3::X,
            segments: MAX_ASSET_PARTICLES,
            particle_mass: 0.1,
            stretch_compliance: 0.0,
            bend_compliance: 0.0,
        };
        assert!(matches!(s.add_rope(&rope), Err(ClothError::Degenerate { .. })));
        assert_eq!(s.particles().len(), 0);
    }

    fn wall() -> DestructibleAsset {
        DestructibleAsset::box_grid(&BoxGrid {
            name: "g".into(),
            counts: [2, 2, 1],
            cell: [1.0; 3],
            density: 1.0,
            strength_per_area: 1.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("wall")
    }

    #[test]
    fn long_strings_are_rejected_by_validate() {
        let mut a = wall();
        a.material.fx_tag = "x".repeat(MAX_DESTRUCTIBLE_STRING + 1);
        assert!(a.validate().is_err());
        let mut a = wall();
        a.name = "x".repeat(MAX_DESTRUCTIBLE_STRING + 1);
        assert!(a.validate().is_err());
    }

    #[test]
    fn an_oversized_event_batch_is_rejected_before_any_applies() {
        let mut replica = DestructionWorld::new(Authority::Replica);
        let event = BreakEvent {
            seq: 0,
            tick: 0,
            destructible: regia_physics::destruction::DestructibleId(1),
            kind: BreakKind::BondBroken {
                bond: 0,
                a: 0,
                b: 1,
                position: [0.0; 3],
            },
            seed: 0,
        };
        let batch = vec![event; MAX_EVENTS_PER_CALL + 1];
        assert!(matches!(
            replica.apply_events(&batch),
            Err(DestructionError::BadEvent { .. })
        ));
    }
}

/// v0.5.0 rc1 physics review (sprint-deep-zai-physics): the single-hit queries, the
/// destruction detach velocity and the floating-origin rebase of queued damage.
mod rc1_physics {
    use std::sync::Arc;
    use std::time::Duration;

    use regia_app::{App, FixedTime, TimeUpdateStrategy};
    use regia_core::math::Vec3;
    use regia_physics::destruction::{
        Authority, BoxGrid, BreakKind, Damage, DamageKind, DestructibleAsset, DestructiblePose,
        DestructionMaterial, DestructionWorld, GridSupport, add_fixed_box,
    };
    use regia_physics::{
        Collider, ColliderShape, PhysicsPlugin, PhysicsWorld, QueryFilter, RigidBody, WorldQueries,
    };
    use regia_scene::Transform;

    /// Finding 1: a collider with no entity (a raw rapier collider, `add_fixed_box` ground in
    /// a headless server, a destruction collider before its entity spawns) closest to the
    /// query used to turn the whole single-hit query into a miss. It is skipped now, as
    /// `raycast_all` and `overlap` already skip it, and the mapped collider behind it is hit.
    #[test]
    fn an_unmapped_collider_in_front_does_not_swallow_single_hit_queries() {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / 60.0,
        )));
        let mut fixed = FixedTime::default();
        fixed.hz = 60.0;
        app.insert_resource(fixed).add_plugins(PhysicsPlugin);
        let ground = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(0.0, -0.5, 0.0)),
                RigidBody::static_body(),
                Collider {
                    shape: ColliderShape::Box {
                        half_extents: Vec3::new(5.0, 0.5, 5.0),
                    },
                    ..Collider::default()
                },
            ))
            .id();
        // An unmapped plate 5 m up, between every query below and the ground.
        add_fixed_box(
            &mut app.world_mut().resource_mut::<PhysicsWorld>(),
            [0.0, 5.0, 0.0],
            [5.0, 0.1, 5.0],
        );
        for _ in 0..6 {
            app.update();
        }
        let world = app.world().resource::<PhysicsWorld>();
        let queries = WorldQueries { world, layers: None };
        let ray = queries.raycast(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new());
        let ray = ray.expect("the ray reaches the mapped ground behind the unmapped plate");
        assert_eq!(ray.entity, ground);
        assert!((ray.distance - 10.0).abs() < 1e-3, "{}", ray.distance);
        let shape = ColliderShape::Sphere { radius: 0.25 };
        let cast = queries
            .shapecast(
                &shape,
                Transform::from_translation(Vec3::new(0.0, 10.0, 0.0)),
                Vec3::NEG_Y,
                20.0,
                QueryFilter::new(),
            )
            .expect("the shape cast reaches the mapped ground");
        assert_eq!(cast.entity, ground);
        // 5.1 m from the plate's top face, 4.9 m from its bottom face, 5.0 m from the ground.
        let closest = queries
            .closest_point(Vec3::new(0.0, 4.0, 0.0), 10.0, QueryFilter::new())
            .expect("the closest mapped collider is the ground");
        assert_eq!(closest.entity, ground);
        // The unmapped plate never shows up in the multi-hit queries either.
        let all = queries.raycast_all(Vec3::new(0.0, 10.0, 0.0), Vec3::NEG_Y, 20.0, QueryFilter::new());
        assert_eq!(all.iter().map(|h| h.entity).collect::<Vec<_>>(), vec![ground]);
    }

    fn wall() -> DestructibleAsset {
        DestructibleAsset::box_grid(&BoxGrid {
            name: "g".into(),
            counts: [2, 2, 1],
            cell: [1.0; 3],
            density: 1.0,
            strength_per_area: 1.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("wall")
    }

    fn detach_velocities(world: &mut DestructionWorld) -> Vec<[f32; 3]> {
        let report = world.step(1.0 / 60.0);
        report
            .events
            .iter()
            .filter_map(|e| match e.kind {
                BreakKind::ChunkDetached { linvel, .. } => Some(linvel),
                _ => None,
            })
            .collect()
    }

    /// Finding 4: a valid but tiny chunk mass (`1e-40`, so `1 / mass` is infinite) or an
    /// accumulated impulse that overflows `f32` used to put an infinite detach velocity in the
    /// break event: rapier got it on the server and a replica refused the event. The velocity
    /// is finite now, and a replica applies the server's events.
    #[test]
    fn a_detach_velocity_is_always_finite() {
        // Tiny mass.
        let mut asset = wall();
        for chunk in &mut asset.chunks {
            chunk.mass = 1.0e-40;
        }
        assert!(asset.validate().is_ok(), "the asset is valid");
        let mut server = DestructionWorld::new(Authority::Server);
        let id = server
            .define(Arc::new(asset.clone()), DestructiblePose::at([0.0; 3]), 7)
            .expect("define");
        server
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Impulse {
                    chunk: 3,
                    impulse: [1000.0, 0.0, 0.0],
                },
            })
            .expect("damage");
        let report = server.step(1.0 / 60.0);
        let velocities: Vec<[f32; 3]> = report
            .events
            .iter()
            .filter_map(|e| match e.kind {
                BreakKind::ChunkDetached { linvel, .. } => Some(linvel),
                _ => None,
            })
            .collect();
        assert!(!velocities.is_empty(), "the hit detaches the chunk");
        for v in &velocities {
            assert!(v.iter().all(|x| x.is_finite()), "detach velocity {v:?}");
        }
        let mut replica = DestructionWorld::new(Authority::Replica);
        replica
            .define_at(id, Arc::new(asset), DestructiblePose::at([0.0; 3]), 7)
            .expect("define replica");
        replica
            .apply_events(&report.events)
            .expect("the replica applies the server's own events");

        // Two huge hits on one chunk in one tick: the sum overflows.
        let mut server = DestructionWorld::new(Authority::Server);
        let id = server
            .define(Arc::new(wall()), DestructiblePose::at([0.0; 3]), 7)
            .expect("define");
        for _ in 0..2 {
            server
                .damage(Damage {
                    target: Some(id),
                    kind: DamageKind::Impulse {
                        chunk: 3,
                        impulse: [3.0e38, 0.0, 0.0],
                    },
                })
                .expect("damage");
        }
        let velocities = detach_velocities(&mut server);
        assert!(!velocities.is_empty(), "the hits detach the chunk");
        for v in &velocities {
            assert!(v.iter().all(|x| x.is_finite()), "detach velocity {v:?}");
        }
    }

    /// Finding 5: damage queued before a floating-origin rebase used to land at the old
    /// coordinates (a miss). Its world-space centre / origin moves with the rebase now.
    #[test]
    fn queued_damage_follows_a_floating_origin_rebase() {
        let asset = wall();
        let centroid = asset.chunks[0].centroid;
        let mut world = DestructionWorld::new(Authority::Server);
        let id = world
            .define(Arc::new(asset), DestructiblePose::at([100.0, 0.0, 0.0]), 7)
            .expect("define");
        let at = [centroid[0] + 100.0, centroid[1], centroid[2]];
        world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Explosion {
                    center: at,
                    radius: 0.25,
                    impulse: 0.001,
                },
            })
            .expect("blast");
        world
            .damage(Damage {
                target: Some(id),
                kind: DamageKind::Projectile {
                    origin: [at[0], at[1], at[2] - 5.0],
                    direction: [0.0, 0.0, 1.0],
                    impulse: 0.001,
                    radius: 0.0,
                    max_distance: 10.0,
                },
            })
            .expect("shot");
        world.shift_origin([100.0, 0.0, 0.0]);
        let report = world.step(1.0 / 60.0);
        assert_eq!(report.misses, 0, "both hits land after the rebase: {report:?}");
        assert_eq!(report.hits, 2, "{report:?}");
    }
}
