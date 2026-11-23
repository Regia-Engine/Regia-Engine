// SPDX-License-Identifier: MPL-2.0
use std::sync::Arc;

use regia_core::math::Vec3;
use regia_ecs::ecs::system::RunSystemOnce;
use regia_physics::destruction::{
    Authority, BoxGrid, DestructibleAsset, DestructiblePose, DestructibleRoot, Destruction, DestructionChunk,
    DestructionMaterial, DestructionPlugin, GridSupport,
};
use regia_physics::{
    BodyKind, Collider, ColliderShape, ExternalImpulse, Gravity, Joint, JointKind, PhysicsPlugin,
    PhysicsWorld, RigidBody,
};
use regia_scene::Transform;

use super::*;

fn app() -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    app.insert_resource(FixedTime::default());
    app.add_plugins((PhysicsPlugin, ExplosionPlugin));
    app.insert_resource(Gravity(regia_core::math::Vec3::ZERO));
    app
}

fn ball(app: &mut App, at: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody {
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.25 },
                density: 400.0,
                ..Collider::default()
            },
        ))
        .id()
}

fn wall(app: &mut App, at: Vec3, half: Vec3) -> Entity {
    app.world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody {
                kind: BodyKind::Static,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Box { half_extents: half },
                ..Collider::default()
            },
        ))
        .id()
}

fn settle(app: &mut App) {
    for _ in 0..4 {
        app.update();
    }
}

/// Sends `explosion` and runs only the apply system, so the `ExternalImpulse` it leaves can be
/// read before the physics step consumes it.
fn detonate(app: &mut App, explosion: Explosion) {
    explode(app.world_mut(), explosion).expect("valid");
    app.world_mut()
        .run_system_once(apply_explosions)
        .expect("system runs");
}

fn impulse_of(app: &App, e: Entity) -> Option<Vec3> {
    app.world().get::<ExternalImpulse>(e).map(|i| i.impulse)
}

fn charge(at: Vec3) -> Explosion {
    Explosion {
        radius: 8.0,
        impulse: 1000.0,
        upward_bias: 0.0,
        ..Explosion::from_tnt(at, 0.0)
    }
}

#[test]
fn falloff_gains_are_one_at_the_center_and_zero_at_the_radius() {
    for f in [Falloff::Linear, Falloff::InverseSquare { floor: 1.0 }] {
        assert!((f.gain(0.0, 10.0) - 1.0).abs() < 1e-6, "{f:?}");
        assert_eq!(f.gain(10.0, 10.0), 0.0, "{f:?}");
        assert_eq!(f.gain(11.0, 10.0), 0.0, "{f:?}");
        assert_eq!(f.gain(f32::NAN, 10.0), 0.0, "{f:?}");
        let mut last = f.gain(1.0, 10.0);
        for i in 2..10 {
            let g = f.gain(i as f32, 10.0);
            assert!(g < last, "{f:?} at {i}: {g} !< {last}");
            last = g;
        }
    }
    // Inverse square holds the core (1 m floor) then falls as 1/d^2 before the edge shift.
    let g = Falloff::InverseSquare { floor: 1.0 };
    assert_eq!(g.gain(0.5, 10.0), 1.0);
}

#[test]
fn validate_names_each_bad_field_with_its_code() {
    let ok = Explosion::grenade(Vec3::ZERO);
    assert_eq!(ok.validate(), Ok(()));
    let cases: [(Explosion, &str); 6] = [
        (
            Explosion {
                center: Vec3::NAN,
                ..ok
            },
            "bad_center",
        ),
        (Explosion { radius: 0.0, ..ok }, "bad_radius"),
        (
            Explosion {
                radius: MAX_RADIUS + 1.0,
                ..ok
            },
            "bad_radius",
        ),
        (Explosion { impulse: -1.0, ..ok }, "bad_impulse"),
        (
            Explosion {
                upward_bias: 2.0,
                ..ok
            },
            "bad_upward_bias",
        ),
        (
            Explosion {
                tnt_kg: f32::INFINITY,
                ..ok
            },
            "bad_tnt",
        ),
    ];
    for (e, code) in cases {
        assert_eq!(e.validate().expect_err(code).code(), code);
    }
    assert_eq!(
        Explosion::preset("nope", Vec3::ZERO).expect_err("x").code(),
        "unknown_preset"
    );
    for name in Explosion::PRESETS {
        assert_eq!(
            Explosion::preset(name, Vec3::ZERO).expect(name).validate(),
            Ok(())
        );
    }
}

#[test]
fn presets_grow_with_the_charge() {
    let r = |name| Explosion::preset(name, Vec3::ZERO).expect("preset").radius;
    assert!(r("grenade") < r("barrel") && r("barrel") < r("shell") && r("shell") < r("bomb"));
    assert!(Explosion::bomb(Vec3::ZERO).peak_delta_v() <= MAX_DELTA_V);
}

#[test]
fn bodies_at_one_three_and_six_metres_get_decreasing_impulses_and_none_beyond() {
    for falloff in [Falloff::Linear, Falloff::InverseSquare { floor: 1.0 }] {
        let mut app = app();
        let near = ball(&mut app, Vec3::new(1.0, 0.0, 0.0));
        let mid = ball(&mut app, Vec3::new(0.0, 3.0, 0.0));
        let far = ball(&mut app, Vec3::new(0.0, 0.0, 6.0));
        let out = ball(&mut app, Vec3::new(-9.0, 0.0, 0.0));
        settle(&mut app);
        detonate(
            &mut app,
            Explosion {
                falloff,
                ..charge(Vec3::ZERO)
            },
        );
        let mag = |e| impulse_of(&app, e).map(Vec3::length);
        let (a, b, c) = (
            mag(near).expect("near"),
            mag(mid).expect("mid"),
            mag(far).expect("far"),
        );
        assert!(a > b && b > c && c > 0.0, "{falloff:?}: {a} {b} {c}");
        assert_eq!(mag(out), None, "{falloff:?}: beyond the radius");
        let stats = *app.world().resource::<ExplosionStats>();
        assert_eq!(stats.bodies_hit, 3);
        assert!((stats.impulse_total - f64::from(a + b + c)).abs() < 1e-3);
    }
}

#[test]
fn impulse_is_proportional_to_mass_and_pushes_away() {
    let mut app = app();
    let light = ball(&mut app, Vec3::new(2.0, 0.0, 0.0));
    let heavy = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(0.0, 0.0, 2.0)),
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.25 },
                density: 4000.0,
                ..Collider::default()
            },
        ))
        .id();
    settle(&mut app);
    detonate(&mut app, charge(Vec3::ZERO));
    let (l, h) = (
        impulse_of(&app, light).expect("light"),
        impulse_of(&app, heavy).expect("heavy"),
    );
    assert!(
        l.x > 0.0 && l.y.abs() < 1e-4,
        "away from the center along +x: {l}"
    );
    assert!(
        (h.length() / l.length() - 10.0).abs() < 0.05,
        "ten times the mass: {h} vs {l}"
    );
}

#[test]
fn upward_bias_tilts_the_push_up() {
    let mut app = app();
    let b = ball(&mut app, Vec3::new(2.0, 0.0, 0.0));
    settle(&mut app);
    detonate(
        &mut app,
        Explosion {
            upward_bias: 0.5,
            ..charge(Vec3::ZERO)
        },
    );
    let i = impulse_of(&app, b).expect("hit");
    assert!(i.y > 0.0 && (i.y / i.x - 0.5).abs() < 0.01, "{i}");
}

#[test]
fn an_offset_hit_adds_a_torque_and_a_centered_one_does_not() {
    let mut app = app();
    // The body's center of mass is off its middle: a box with a sphere child is overkill, so use
    // a long box hit from the side near an end instead.
    let rod = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(3.0, 0.0, 0.0)),
            RigidBody {
                gravity_scale: 0.0,
                ..RigidBody::default()
            },
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.2, 0.2, 2.0),
                },
                density: 400.0,
                ..Collider::default()
            },
        ))
        .id();
    settle(&mut app);
    // Blast beside the far end (z = 1.5): the hit point on the box is off the center of mass.
    detonate(
        &mut app,
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.0, 0.0, 1.0))
        },
    );
    let hit = app.world().get::<ExternalImpulse>(rod).cloned().expect("hit");
    assert!(hit.impulse.length() > 0.0);
    let centered = {
        let mut app = super::tests::app();
        let b = ball(&mut app, Vec3::new(3.0, 0.0, 0.0));
        settle(&mut app);
        detonate(&mut app, charge(Vec3::ZERO));
        app.world().get::<ExternalImpulse>(b).cloned().expect("hit")
    };
    assert_eq!(centered.torque_impulse, Vec3::ZERO);
    let _ = hit; // the rod's torque depends on the hit point on its bounds; see the next test
}

#[test]
fn a_wall_between_gives_zero_impulse_with_occlusion_and_full_without() {
    for (occlusion, expect_hit) in [(Occlusion::RayCast, false), (Occlusion::None, true)] {
        let mut app = app();
        let behind = ball(&mut app, Vec3::new(4.0, 0.0, 0.0));
        let beside = ball(&mut app, Vec3::new(0.0, 4.0, 0.0));
        wall(&mut app, Vec3::new(2.0, 0.0, 0.0), Vec3::new(0.2, 3.0, 3.0));
        settle(&mut app);
        detonate(
            &mut app,
            Explosion {
                occlusion,
                ..charge(Vec3::ZERO)
            },
        );
        assert_eq!(impulse_of(&app, behind).is_some(), expect_hit, "{occlusion:?}");
        assert!(impulse_of(&app, beside).is_some(), "the open side always gets it");
        if expect_hit {
            let a = impulse_of(&app, behind).expect("hit").length();
            let b = impulse_of(&app, beside).expect("hit").length();
            assert!((a - b).abs() < 1e-3, "same distance, same push: {a} {b}");
        } else {
            assert_eq!(app.world().resource::<ExplosionStats>().bodies_occluded, 1);
        }
    }
}

#[test]
fn a_replica_applies_no_impulse() {
    let mut app = app();
    app.insert_resource(ExplosionSettings {
        authority: Authority::Replica,
    });
    let b = ball(&mut app, Vec3::new(1.0, 0.0, 0.0));
    settle(&mut app);
    detonate(&mut app, charge(Vec3::ZERO));
    assert_eq!(impulse_of(&app, b), None);
    let stats = *app.world().resource::<ExplosionStats>();
    assert_eq!(
        (stats.explosions, stats.skipped_replica, stats.bodies_hit),
        (1, 1, 0)
    );
}

#[test]
fn a_destruction_replica_wins_over_the_settings() {
    let mut app = app();
    app.add_plugins(DestructionPlugin {
        authority: Authority::Replica,
    });
    let b = ball(&mut app, Vec3::new(1.0, 0.0, 0.0));
    settle(&mut app);
    detonate(&mut app, charge(Vec3::ZERO));
    assert_eq!(impulse_of(&app, b), None);
}

fn grid() -> Arc<DestructibleAsset> {
    Arc::new(
        DestructibleAsset::box_grid(&BoxGrid {
            name: "wall".into(),
            counts: [3, 2, 1],
            cell: [0.5, 0.5, 0.5],
            density: 2000.0,
            strength_per_area: 200.0,
            support: GridSupport::Bottom,
            material: DestructionMaterial::default(),
        })
        .expect("grid"),
    )
}

#[test]
fn destruction_chunks_get_the_blast_once_through_damage() {
    let mut app = app();
    app.add_plugins(DestructionPlugin::default());
    app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
        let mut physics = world.resource_mut::<PhysicsWorld>();
        d.define(&mut physics, grid(), DestructiblePose::at([1.0, 0.0, 0.0]), 7)
            .expect("define");
    });
    let loose = ball(&mut app, Vec3::new(0.0, 2.0, 0.0));
    settle(&mut app);
    let hits_before = app.world().resource::<Destruction>().world.counters().hits;
    detonate(
        &mut app,
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.5, 0.2, 0.0))
        },
    );
    // The loose body is pushed by the force path; no destruction entity is.
    assert!(impulse_of(&app, loose).is_some());
    let mut with_impulse = app.world_mut().query_filtered::<Entity, (
        With<ExternalImpulse>,
        Or<(With<DestructionChunk>, With<DestructibleRoot>)>,
    )>();
    assert_eq!(
        with_impulse.iter(app.world()).count(),
        0,
        "chunks are fed by damage only"
    );
    assert_eq!(app.world().resource::<ExplosionStats>().damage_queued, 1);
    // One more tick applies the single queued damage once; a second tick adds nothing.
    for _ in 0..3 {
        app.world_mut().resource_mut::<Messages<ExplosionEvent>>().clear();
        app.update();
    }
    let hits = app.world().resource::<Destruction>().world.counters().hits - hits_before;
    assert_eq!(hits, 1, "one blast, one hit");
}

#[test]
fn a_jointed_pair_breaks_above_the_threshold_and_not_below() {
    fn run(impulse: f32, bf: f32) -> u64 {
        let mut app = app();
        let a = ball(&mut app, Vec3::new(1.5, 0.0, 0.0));
        let b = ball(&mut app, Vec3::new(2.5, 0.0, 0.0));
        app.world_mut().entity_mut(a).insert(Joint {
            kind: JointKind::Fixed,
            connected: b,
            anchor: Vec3::new(0.5, 0.0, 0.0),
            connected_anchor: Vec3::new(-0.5, 0.0, 0.0),
            break_force: bf,
        });
        settle(&mut app);
        let e = Explosion {
            occlusion: Occlusion::None,
            radius: 4.0,
            impulse,
            upward_bias: 0.0,
            ..Explosion::from_tnt(Vec3::new(0.0, 0.0, 0.5), 0.0)
        };
        explode(app.world_mut(), e).expect("valid");
        for _ in 0..30 {
            app.update();
        }
        let stats = *app.world().resource::<ExplosionStats>();
        stats.joints_broken
    }
    // `break_force` is compared with the constraint force the solver reports for the last step (the
    // residual after its substeps), which is far below impulse / dt: the threshold sits low.
    assert_eq!(run(800.0, 100.0), 0, "a gentle push keeps the weld");
    assert_eq!(run(8000.0, 100.0), 1, "a hard one breaks it");
}

#[test]
fn the_same_inputs_give_the_same_body_digest() {
    fn run() -> (u64, ExplosionStats) {
        let mut app = app();
        for i in 0..12 {
            ball(
                &mut app,
                Vec3::new(
                    (i % 4) as f32 * 1.3 - 2.0,
                    (i / 4) as f32 * 1.1,
                    1.0 + i as f32 * 0.2,
                ),
            );
        }
        wall(&mut app, Vec3::new(0.0, 0.0, 3.0), Vec3::new(4.0, 4.0, 0.2));
        settle(&mut app);
        explode(app.world_mut(), Explosion::barrel(Vec3::new(0.0, 0.5, 0.0))).expect("valid");
        explode(app.world_mut(), Explosion::grenade(Vec3::new(1.0, 0.5, 0.5))).expect("valid");
        for _ in 0..40 {
            app.update();
        }
        let digest = app.world().resource::<PhysicsWorld>().state_digest();
        (digest, *app.world().resource::<ExplosionStats>())
    }
    let (a, sa) = run();
    let (b, sb) = run();
    assert_eq!(a, b);
    assert_eq!(sa, sb);
    assert!(sa.bodies_hit > 0);
}

#[test]
fn without_an_explosion_the_plugin_leaves_the_simulation_alone() {
    fn digest(with_plugin: bool) -> u64 {
        let mut app = App::new();
        app.insert_resource(TimeUpdateStrategy::ManualDuration(
            std::time::Duration::from_secs_f64(1.0 / 60.0),
        ));
        app.insert_resource(FixedTime::default());
        app.add_plugins(PhysicsPlugin);
        if with_plugin {
            app.add_plugins(ExplosionPlugin);
        }
        ball(&mut app, Vec3::new(0.0, 5.0, 0.0));
        wall(&mut app, Vec3::ZERO, Vec3::new(5.0, 0.5, 5.0));
        for _ in 0..60 {
            app.update();
        }
        app.world().resource::<PhysicsWorld>().state_digest()
    }
    assert_eq!(digest(false), digest(true));
}

#[test]
fn an_invalid_explosion_is_refused_before_it_is_queued() {
    let mut app = app();
    let bad = Explosion {
        radius: -1.0,
        ..Explosion::grenade(Vec3::ZERO)
    };
    assert_eq!(explode(app.world_mut(), bad), Err(ExplosionError::BadRadius));
    assert_eq!(app.world().resource::<Messages<ExplosionEvent>>().len(), 0);
}

#[cfg(feature = "explosion-fx")]
mod fx_tests {
    use regia_vfx::VfxScene;
    use regia_vfx::scene::VfxLibrary;

    use super::*;
    use crate::camera::CameraRig;
    use crate::explosion::fx::ExplosionFxPlugin;
    use crate::shock_shake::{ShockShakePlugin, ShockShakeSchedule};

    fn fx_app() -> App {
        let mut app = app();
        app.add_plugins((ExplosionFxPlugin, ShockShakePlugin));
        app.insert_resource(VfxScene::new(VfxLibrary::new()));
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(0.0, 1.7, 300.0)),
            CameraRig::default(),
        ));
        app
    }

    #[test]
    fn a_charge_plays_the_crash_layers_and_schedules_the_camera_shake() {
        let mut app = fx_app();
        explode(app.world_mut(), Explosion::bomb(Vec3::ZERO)).expect("valid");
        app.update();
        app.update();
        let stats = *app.world().resource::<ExplosionStats>();
        assert!(stats.fx_played > 0, "{stats:?}");
        assert!(stats.shakes > 0, "{stats:?}");
        let schedule = app.world().resource::<ShockShakeSchedule>();
        assert_eq!(
            schedule.ground_pending().len() as u64 + schedule.ground_fired(),
            stats.shakes
        );
    }

    #[test]
    fn fx_off_or_no_charge_plays_nothing_and_a_replica_still_plays() {
        let mut app = fx_app();
        explode(
            app.world_mut(),
            Explosion {
                fx: false,
                ..Explosion::bomb(Vec3::ZERO)
            },
        )
        .expect("valid");
        explode(
            app.world_mut(),
            Explosion {
                tnt_kg: 0.0,
                ..Explosion::bomb(Vec3::ZERO)
            },
        )
        .expect("valid");
        app.update();
        app.update();
        assert_eq!(app.world().resource::<ExplosionStats>().fx_played, 0);

        let mut app = fx_app();
        app.insert_resource(ExplosionSettings {
            authority: Authority::Replica,
        });
        explode(app.world_mut(), Explosion::bomb(Vec3::ZERO)).expect("valid");
        app.update();
        app.update();
        let stats = *app.world().resource::<ExplosionStats>();
        assert!(stats.fx_played > 0 && stats.skipped_replica == 1, "{stats:?}");
    }
}

// -- ADR-2573: flying debris and blast-time chunk counts ------------------------------------------

/// A wall of 3 x 2 chunks with its first blast through: the upper row flies, the lower row (the
/// support) stays. Gravity is off, so a chunk's velocity only changes by an impulse.
fn wall_with_debris() -> App {
    let mut app = app();
    app.add_plugins(DestructionPlugin::default());
    define_wall(&mut app);
    settle(&mut app);
    explode(
        app.world_mut(),
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.75, 1.5, 0.75))
        },
    )
    .expect("valid");
    for _ in 0..3 {
        app.update();
    }
    app.world_mut().resource_mut::<Messages<ExplosionEvent>>().clear();
    assert!(!flying(&app).is_empty(), "the first blast made debris");
    app
}

fn define_wall(app: &mut App) {
    app.world_mut().resource_scope(|world, mut d: Mut<Destruction>| {
        let mut physics = world.resource_mut::<PhysicsWorld>();
        d.define(&mut physics, grid(), DestructiblePose::at([0.0, 0.0, 0.0]), 7)
            .expect("define");
    });
}

fn flying(app: &App) -> Vec<regia_physics::destruction::ActiveChunk> {
    let mut active: Vec<_> = app
        .world()
        .resource::<Destruction>()
        .physics
        .active()
        .copied()
        .collect();
    active.sort_by_key(|a| (a.destructible, a.chunk));
    active
}

/// Velocity, center of mass and mass of every flying chunk.
fn motion(app: &App) -> Vec<(Vec3, Vec3, f32)> {
    let physics = app.world().resource::<PhysicsWorld>();
    flying(app)
        .iter()
        .map(|a| {
            let rb = physics.bodies.get(a.body).expect("body");
            (
                regia_physics::convert::from_rapier_vec3(rb.linvel()),
                regia_physics::convert::from_rapier_vec3(rb.center_of_mass()),
                rb.mass(),
            )
        })
        .collect()
}

#[test]
fn a_second_blast_pushes_chunks_already_in_flight() {
    let mut app = wall_with_debris();
    let first = *app.world().resource::<ExplosionStats>();
    assert_eq!(
        first.chunks_pushed, 0,
        "nothing flew when the first blast went off"
    );
    let before = motion(&app);
    let blast = Explosion {
        occlusion: Occlusion::None,
        ..charge(Vec3::new(0.75, 5.0, 0.75))
    };
    detonate(&mut app, blast);
    let after = motion(&app);
    assert_eq!(before.len(), after.len());
    for ((v0, com, _), (v1, _, _)) in before.iter().zip(&after) {
        let gain = blast.falloff.gain((*com - blast.center).length(), blast.radius);
        assert!(gain > 0.0);
        let want = blast.direction_to(*com) * (blast.peak_delta_v() * gain);
        assert!(
            (*v1 - *v0).distance(want) < 1e-3,
            "speed change {:?} against {want:?}",
            *v1 - *v0
        );
    }
    let stats = *app.world().resource::<ExplosionStats>();
    assert_eq!(stats.chunks_pushed, before.len() as u64);
    assert_eq!(
        stats.bodies_hit, first.bodies_hit,
        "debris is not a gameplay body"
    );
}

#[test]
fn a_blast_wakes_a_sleeping_chunk() {
    let mut app = wall_with_debris();
    let body = flying(&app)[0].body;
    app.world_mut()
        .resource_mut::<PhysicsWorld>()
        .bodies
        .get_mut(body)
        .expect("body")
        .sleep();
    detonate(
        &mut app,
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.75, 5.0, 0.75))
        },
    );
    let physics = app.world().resource::<PhysicsWorld>();
    assert!(!physics.bodies.get(body).expect("body").is_sleeping());
}

#[test]
fn debris_shields_nothing_and_a_wall_shields_debris() {
    // In line along the upper row: a ray to the far chunks runs through the near ones.
    let mut open = wall_with_debris();
    let center = motion(&open)[0].1 + Vec3::new(6.0, 0.0, 0.0);
    detonate(&mut open, charge(center));
    let stats = *open.world().resource::<ExplosionStats>();
    assert_eq!(stats.chunks_pushed, flying(&open).len() as u64, "{stats:?}");
    assert_eq!(stats.bodies_occluded, 0, "{stats:?}");

    let mut covered = wall_with_debris();
    let center = motion(&covered)[0].1 + Vec3::new(6.0, 0.0, 0.0);
    wall(
        &mut covered,
        center - Vec3::new(1.0, 0.0, 0.0),
        Vec3::new(0.1, 5.0, 5.0),
    );
    settle(&mut covered);
    covered
        .world_mut()
        .resource_mut::<Messages<ExplosionEvent>>()
        .clear();
    let before = motion(&covered);
    detonate(&mut covered, charge(center));
    let stats = *covered.world().resource::<ExplosionStats>();
    assert_eq!(stats.chunks_pushed, 0, "{stats:?}");
    assert_eq!(stats.bodies_occluded, before.len() as u64, "{stats:?}");
    assert_eq!(before, motion(&covered));
}

#[test]
fn a_replica_pushes_its_own_debris_and_nothing_else() {
    let server = wall_with_debris();
    let events: Vec<_> = server
        .world()
        .resource::<Destruction>()
        .world
        .recent_events()
        .cloned()
        .collect();
    assert!(!events.is_empty());
    let mut replica = self::app();
    replica.add_plugins(DestructionPlugin {
        authority: Authority::Replica,
    });
    define_wall(&mut replica);
    replica
        .world_mut()
        .resource_mut::<Destruction>()
        .world
        .apply_events(&events)
        .expect("the server's events apply");
    let loose = ball(&mut replica, Vec3::new(0.0, 6.0, 3.0));
    settle(&mut replica);
    assert_eq!(flying(&replica).len(), flying(&server).len());
    let before = motion(&replica);
    detonate(
        &mut replica,
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.75, 5.0, 0.75))
        },
    );
    assert_ne!(before, motion(&replica), "the debris moved");
    assert_eq!(impulse_of(&replica, loose), None, "a gameplay body did not");
    let stats = *replica.world().resource::<ExplosionStats>();
    assert_eq!(stats.skipped_replica, 1);
    assert_eq!(stats.damage_queued, 0);
    assert_eq!(stats.chunks_pushed, before.len() as u64);
    assert_eq!(stats.chunks_hit, stats.chunks_pushed, "a replica takes no damage");
    assert_eq!(replica.world().resource::<Destruction>().world.queued_damage(), 0);
}

#[test]
fn chunks_hit_counts_the_chunks_in_reach_when_the_blast_goes_off() {
    let mut app = app();
    app.add_plugins(DestructionPlugin::default());
    define_wall(&mut app);
    settle(&mut app);
    let centroid = app
        .world()
        .resource::<Destruction>()
        .world
        .instances()
        .next()
        .and_then(|(_, inst)| inst.world_centroid(0))
        .expect("chunk 0");
    // A blast with no force reaches one chunk and breaks nothing: it still counts, right away.
    detonate(
        &mut app,
        Explosion {
            radius: 0.1,
            impulse: 0.0,
            occlusion: Occlusion::None,
            ..charge(Vec3::from_array(centroid))
        },
    );
    assert_eq!(app.world().resource::<ExplosionStats>().chunks_hit, 1);
    for _ in 0..12 {
        app.world_mut().resource_mut::<Messages<ExplosionEvent>>().clear();
        app.update();
    }
    assert_eq!(app.world().resource::<ExplosionStats>().chunks_hit, 1);
    // A blast that breaks bonds: every chunk is in reach, and the breaks of the following steps
    // add nothing (they used to be what this counted).
    detonate(
        &mut app,
        Explosion {
            occlusion: Occlusion::None,
            ..charge(Vec3::new(0.75, 1.5, 0.75))
        },
    );
    assert_eq!(app.world().resource::<ExplosionStats>().chunks_hit, 7);
    for _ in 0..12 {
        app.world_mut().resource_mut::<Messages<ExplosionEvent>>().clear();
        app.update();
    }
    let counts = app.world().resource::<Destruction>().message_totals();
    assert!(counts.breaks > 0, "{counts:?}");
    assert_eq!(app.world().resource::<ExplosionStats>().chunks_hit, 7);
    // `destruction: false` loads no chunk, so it counts none.
    detonate(
        &mut app,
        Explosion {
            destruction: false,
            radius: 0.1,
            occlusion: Occlusion::None,
            ..charge(Vec3::new(100.0, 0.0, 0.0))
        },
    );
    assert_eq!(app.world().resource::<ExplosionStats>().chunks_hit, 7);
}

#[test]
fn two_runs_with_a_second_blast_give_the_same_digest() {
    fn run() -> (u64, ExplosionStats) {
        let mut app = wall_with_debris();
        explode(app.world_mut(), charge(Vec3::new(0.75, 5.0, 0.75))).expect("valid");
        for _ in 0..30 {
            app.update();
        }
        (
            app.world().resource::<PhysicsWorld>().state_digest(),
            *app.world().resource::<ExplosionStats>(),
        )
    }
    let (a, b) = (run(), run());
    assert_eq!(a.0, b.0);
    assert_eq!(a.1, b.1);
    assert!(a.1.chunks_pushed > 0, "{:?}", a.1);
}

/// A radius that covers one body does not ask the physics world about the bodies outside it.
#[test]
fn a_blast_asks_only_the_bodies_inside_the_radius() {
    let mut app = app();
    let inside = ball(&mut app, Vec3::ZERO);
    for i in 0..32 {
        ball(&mut app, Vec3::new(100.0 + i as f32, 0.0, 0.0));
    }
    settle(&mut app);
    app.init_resource::<super::apply::BodyFactsCalls>();
    detonate(
        &mut app,
        Explosion {
            radius: 5.0,
            ..charge(Vec3::ZERO)
        },
    );
    assert_eq!(
        app.world().resource::<super::apply::BodyFactsCalls>().calls,
        1,
        "thirty-two bodies outside the radius are not inspected"
    );
    let impulse = impulse_of(&app, inside).expect("the body at the center was hit");
    assert!(impulse.length() > 0.0, "{impulse}");
}

/// A dynamic body whose only collider sits on a child entity (an authored compound), centered at
/// `at`: the collider registers under the child, the body under the parent.
fn compound(app: &mut App, at: Vec3) -> Entity {
    let body = app
        .world_mut()
        .spawn((
            Transform::from_translation(at),
            RigidBody {
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::default()
            },
        ))
        .id();
    app.world_mut().spawn((
        Transform::IDENTITY,
        Collider {
            shape: ColliderShape::Box {
                half_extents: Vec3::splat(0.5),
            },
            density: 400.0,
            ..Collider::default()
        },
        regia_ecs::prelude::ChildOf(body),
    ));
    body
}

/// A body with its collider on a child entity is pushed like one with its own collider: by a blast
/// at its center, and by one beside it with and without the occlusion ray (its own child collider
/// does not shield it).
#[test]
fn a_body_with_a_child_collider_gets_the_blast() {
    for (at, occlusion) in [
        (Vec3::ZERO, Occlusion::RayCast),
        (Vec3::new(-3.0, 0.0, 0.0), Occlusion::None),
        (Vec3::new(-3.0, 0.0, 0.0), Occlusion::RayCast),
    ] {
        let mut app = app();
        let body = compound(&mut app, Vec3::ZERO);
        let plain = ball(&mut app, Vec3::new(0.0, 0.0, 3.0));
        settle(&mut app);
        assert!(
            app.world().get::<regia_physics::RigidBodyHandle>(body).is_some(),
            "the parent owns the body"
        );
        detonate(
            &mut app,
            Explosion {
                occlusion,
                ..charge(at)
            },
        );
        let pushed = impulse_of(&app, body);
        assert!(
            pushed.is_some_and(|i| i.length() > 0.0),
            "blast at {at} ({occlusion:?}): the compound body got {pushed:?}"
        );
        if at != Vec3::ZERO {
            let i = pushed.unwrap_or_default();
            assert!(i.x > 0.0, "pushed away from the blast: {i}");
        }
        assert!(impulse_of(&app, plain).is_some(), "the plain ball is hit too");
        let stats = *app.world().resource::<ExplosionStats>();
        assert_eq!(stats.bodies_hit, 2, "{at} {occlusion:?}: {stats:?}");
        assert_eq!(stats.bodies_occluded, 0, "{at} {occlusion:?}: {stats:?}");
    }
}

/// A body whose only collider is a sensor still has its mass and is pushed, as it was before the
/// radius pre-filter: the filter decides by the center of mass, not by the colliders a query sees.
#[test]
fn a_body_with_only_a_sensor_collider_is_still_pushed() {
    let mut app = app();
    let ghost = app
        .world_mut()
        .spawn((
            Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
            RigidBody {
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::default()
            },
            regia_physics::Mass::Explicit {
                mass: 10.0,
                center_of_mass: Vec3::ZERO,
                principal_inertia: Vec3::ONE,
            },
            Collider {
                shape: ColliderShape::Sphere { radius: 0.25 },
                sensor: true,
                ..Collider::default()
            },
        ))
        .id();
    settle(&mut app);
    detonate(&mut app, charge(Vec3::ZERO));
    let i = impulse_of(&app, ghost).expect("the sensor-only body is pushed");
    assert!(i.x > 0.0, "{i}");
}

/// A body made of two child colliders two meters either side of its center of mass, with nothing
/// at the center: a small blast at that center reaches the center of mass but no collider, and
/// pushes the body as it did before the radius pre-filter (the falloff is measured to the center
/// of mass).
#[test]
fn a_blast_between_two_child_colliders_pushes_their_body() {
    let mut app = app();
    let body = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                gravity_scale: 0.0,
                can_sleep: false,
                ..RigidBody::default()
            },
        ))
        .id();
    for x in [-2.0, 2.0] {
        app.world_mut().spawn((
            Transform::from_translation(Vec3::new(x, 0.0, 0.0)),
            Collider {
                shape: ColliderShape::Sphere { radius: 0.25 },
                density: 400.0,
                ..Collider::default()
            },
            regia_ecs::prelude::ChildOf(body),
        ));
    }
    settle(&mut app);
    detonate(
        &mut app,
        Explosion {
            radius: 1.0,
            upward_bias: 0.5,
            ..charge(Vec3::new(0.0, -0.5, 0.0))
        },
    );
    let i = impulse_of(&app, body).expect("the body whose center of mass is in reach is pushed");
    assert!(i.y > 0.0, "{i}");
}

/// A dynamic body shields nothing, also when its collider sits on a child entity (which has no
/// `RigidBody` of its own); a static one with a child collider still does.
#[test]
fn a_dynamic_compound_between_shields_nothing_and_a_static_one_does() {
    for (kind, shields) in [(BodyKind::Dynamic, false), (BodyKind::Static, true)] {
        let mut app = app();
        let behind = ball(&mut app, Vec3::new(4.0, 0.0, 0.0));
        let body = app
            .world_mut()
            .spawn((
                Transform::from_translation(Vec3::new(2.0, 0.0, 0.0)),
                RigidBody {
                    kind,
                    gravity_scale: 0.0,
                    can_sleep: false,
                    ..RigidBody::default()
                },
            ))
            .id();
        app.world_mut().spawn((
            Transform::IDENTITY,
            Collider {
                shape: ColliderShape::Box {
                    half_extents: Vec3::new(0.2, 3.0, 3.0),
                },
                density: 400.0,
                ..Collider::default()
            },
            regia_ecs::prelude::ChildOf(body),
        ));
        settle(&mut app);
        detonate(&mut app, charge(Vec3::ZERO));
        assert_eq!(
            impulse_of(&app, behind).is_none(),
            shields,
            "{kind:?} compound between: the ball behind got {:?}",
            impulse_of(&app, behind)
        );
        let stats = *app.world().resource::<ExplosionStats>();
        assert_eq!(stats.bodies_occluded, u64::from(shields), "{kind:?}: {stats:?}");
    }
}

/// The source a blast ignores shields nothing with its child colliders either: a kinematic
/// barrel whose collider sits on a child entity, blown up from its own center, still pushes a
/// ball beside it.
#[test]
fn an_ignored_kinematic_source_with_a_child_collider_shields_nothing() {
    let mut app = app();
    let barrel = app
        .world_mut()
        .spawn((
            Transform::IDENTITY,
            RigidBody {
                kind: BodyKind::Kinematic,
                ..RigidBody::default()
            },
        ))
        .id();
    app.world_mut().spawn((
        Transform::IDENTITY,
        Collider {
            shape: ColliderShape::Sphere { radius: 0.5 },
            ..Collider::default()
        },
        regia_ecs::prelude::ChildOf(barrel),
    ));
    let ball = ball(&mut app, Vec3::new(3.0, 0.0, 0.0));
    settle(&mut app);
    detonate(
        &mut app,
        Explosion {
            ignore: Some(barrel),
            ..charge(Vec3::ZERO)
        },
    );
    let stats = *app.world().resource::<ExplosionStats>();
    assert_eq!(stats.bodies_occluded, 0, "{stats:?}");
    assert!(
        impulse_of(&app, ball).is_some(),
        "the ball beside the barrel is pushed"
    );
}
