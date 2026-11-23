// SPDX-License-Identifier: MPL-2.0
//! Destruction level 2 end to end (ADR-1597): the wall-and-bridge sample breaks the same way on
//! every run and every platform (pinned hashes), a replica that replays the server's events tick by
//! tick matches it, and inside an app the rapier side follows: intact colliders leave, pooled
//! debris bodies appear under the caps, despawn rules empty the pool, and the messages fire.

use std::time::Duration;

use regia_app::{App, FixedTime, TimeUpdateStrategy};
use regia_physics::destruction::sample::{
    SAMPLE_SEEDS, SAMPLE_TICKS, define_sample, run_sample_headless, sample_assets, sample_hits,
};
use regia_physics::destruction::{
    Authority, BreakKind, ChunkState, DestructiblePose, Destruction, DestructionBudget, DestructionChunk,
    DestructionPlugin, DestructionWorld, DetachCause,
};
use regia_physics::{PhysicsPlugin, PhysicsWorld};

/// `structure_hash` of the sample after `SAMPLE_TICKS` ticks, recorded on x86_64 Linux (the build server,
/// Rust 1.99). The break model uses only IEEE-exact `f32` operations on plain arrays, so every
/// platform (aarch64 Android, macOS, Windows) must produce the same value.
const PINNED_STRUCTURE_HASH: u64 = 0x4c7d_ecfd_e4c7_d135;
/// `state_hash` (adds bond damage and the tick) of the same run.
const PINNED_STATE_HASH: u64 = 0xdad9_b9e5_73c3_b4f7;

#[test]
fn sample_breaks_the_same_way_every_run_and_matches_the_pinned_hash() {
    let a = run_sample_headless(SAMPLE_TICKS).expect("run a");
    let b = run_sample_headless(SAMPLE_TICKS).expect("run b");
    assert_eq!(a, b, "two runs differ");
    println!(
        "DESTRUCTION_SAMPLE structure={:#018x} state={:#018x} events={} attached={:?}",
        a.structure_hash,
        a.state_hash,
        a.events.len(),
        a.attached
    );
    assert_eq!(a.structure_hash, PINNED_STRUCTURE_HASH, "structure hash moved");
    assert_eq!(a.state_hash, PINNED_STATE_HASH, "state hash moved");
}

#[test]
fn sample_script_does_what_it_says() {
    let run = run_sample_headless(SAMPLE_TICKS).expect("run");
    let wall_chunks = 64;
    let bridge_chunks = 24;
    // The wall lost bricks but still stands; the bridge kept both abutments and its right half.
    // The bottom row (8 supports) always stands.
    assert!(
        run.attached[0] < wall_chunks && run.attached[0] >= 8,
        "{:?}",
        run.attached
    );
    let detached_by = |id, cause| {
        run.events
            .iter()
            .filter(|e| e.destructible == id)
            .filter_map(|e| match e.kind {
                BreakKind::ChunkDetached { chunk, cause: c, .. } if c == cause => Some(chunk),
                _ => None,
            })
            .collect::<Vec<u32>>()
    };
    // Tick 10: the shot knocks out brick (x 4, y 4) = 4 + 8 * 4.
    let first_wall = run
        .events
        .iter()
        .find(|e| e.destructible == run.ids.wall && matches!(e.kind, BreakKind::ChunkDetached { .. }))
        .expect("the shot breaks a brick");
    assert_eq!(first_wall.tick, 10);
    assert!(
        matches!(first_wall.kind, BreakKind::ChunkDetached { chunk: 36, .. }),
        "{first_wall:?}"
    );
    // Tick 50: the deck's middle planks (x 5, 6 on both rows) blow out.
    let mut middle = detached_by(run.ids.bridge, DetachCause::Damage);
    middle.sort_unstable();
    assert_eq!(middle, vec![1, 5, 6, 13, 17, 18], "planks broken by hits");
    // Tick 70: the left half (x 2..=4 on both rows) collapses; abutments 0, 11, 12, 23 stay.
    let collapsed = detached_by(run.ids.bridge, DetachCause::Collapse);
    assert_eq!(collapsed, vec![2, 3, 4, 14, 15, 16]);
    assert_eq!(run.attached[1], bridge_chunks - 12);
    let tick_of = |chunk| {
        run.events
            .iter()
            .find(|e| {
                e.destructible == run.ids.bridge
                    && matches!(e.kind, BreakKind::ChunkDetached { chunk: c, .. } if c == chunk)
            })
            .map(|e| e.tick)
    };
    assert_eq!(tick_of(5), Some(50));
    assert_eq!(tick_of(2), Some(70));
    // Sequence numbers are dense and in order.
    for (i, e) in run.events.iter().enumerate() {
        assert_eq!(e.seq, i as u64);
    }
}

#[test]
fn replica_replaying_events_tick_by_tick_matches_the_server() {
    let run = run_sample_headless(SAMPLE_TICKS).expect("run");
    let mut server = DestructionWorld::new(Authority::Server);
    let mut client = DestructionWorld::new(Authority::Replica);
    let ids = define_sample(&mut server).expect("server");
    assert_eq!(define_sample(&mut client).expect("client"), ids);
    let mut sent = 0usize;
    for tick in 1..=SAMPLE_TICKS {
        for hit in sample_hits(tick, ids) {
            server.damage(hit).expect("queue");
        }
        let report = server.step(1.0 / 60.0);
        client.step(1.0 / 60.0);
        // Ship the tick's events as bytes (what a network layer would do) and replay them.
        let wire = ron::to_string(&report.events).expect("encode");
        let received: Vec<regia_physics::destruction::BreakEvent> = ron::from_str(&wire).expect("decode");
        client.apply_events(&received).expect("apply");
        sent += received.len();
        assert_eq!(server.structure_hash(), client.structure_hash(), "tick {tick}");
        assert_eq!(server.take_spawns(0), client.take_spawns(0), "tick {tick}");
    }
    assert_eq!(sent, run.events.len());
    assert_eq!(server.structure_hash(), run.structure_hash);
}

fn app(budget: DestructionBudget) -> App {
    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, DestructionPlugin::default()));
    app.world_mut()
        .resource_mut::<Destruction>()
        .world
        .set_budget(budget);
    app
}

/// Defines the sample in the app's resource and runs the script until `ticks` fixed ticks ran.
fn run_app(
    app: &mut App,
    ticks: u64,
    mut each_tick: impl FnMut(&mut App),
) -> regia_physics::destruction::sample::SampleIds {
    let (wall, bridge) = sample_assets().expect("assets");
    let ids = app
        .world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            let wall = d
                .define(
                    &mut physics,
                    wall,
                    DestructiblePose::at(regia_physics::destruction::sample::WALL_POSE),
                    SAMPLE_SEEDS[0],
                )
                .expect("wall");
            let bridge = d
                .define(
                    &mut physics,
                    bridge,
                    DestructiblePose::at(regia_physics::destruction::sample::BRIDGE_POSE),
                    SAMPLE_SEEDS[1],
                )
                .expect("bridge");
            regia_physics::destruction::sample::SampleIds { wall, bridge }
        });
    let mut guard = 0;
    loop {
        let tick = app.world().resource::<Destruction>().world.tick();
        if tick >= ticks {
            break;
        }
        // Hits queued now land on the next tick.
        for hit in sample_hits(tick + 1, ids) {
            app.world_mut()
                .resource_mut::<Destruction>()
                .world
                .damage(hit)
                .expect("queue");
        }
        // Exactly one fixed step may run per update; loop until the tick advances.
        while app.world().resource::<Destruction>().world.tick() == tick {
            app.update();
            guard += 1;
            assert!(guard < 10 * ticks as usize + 100, "fixed steps stopped");
        }
        each_tick(app);
    }
    ids
}

#[test]
fn app_decides_like_the_headless_run_and_rapier_follows() {
    let headless = run_sample_headless(SAMPLE_TICKS).expect("headless");
    let mut app = app(DestructionBudget::default());
    let mut start_colliders = None;
    let ids = run_app(&mut app, SAMPLE_TICKS, |app| {
        start_colliders.get_or_insert(app.world().resource::<PhysicsWorld>().colliders.len());
    });
    let d = app.world().resource::<Destruction>();
    // Rapier never feeds back into a decision: same structure as the headless run.
    assert_eq!(d.world.structure_hash(), headless.structure_hash);
    assert_eq!(
        start_colliders,
        Some(64 + 24),
        "one intact collider per chunk after tick 1"
    );
    let detached = headless
        .events
        .iter()
        .filter(|e| matches!(e.kind, BreakKind::ChunkDetached { .. }))
        .count();
    let stats = d.physics.stats().clone();
    assert_eq!(stats.colliders_removed_total, detached as u64, "{stats:?}");
    assert_eq!(stats.spawned_total, detached as u64, "{stats:?}");
    assert!(
        d.physics.intact_collider(ids.wall, 36).is_none(),
        "the shot brick's collider left"
    );
    assert!(
        d.physics.intact_collider(ids.wall, 0).is_some(),
        "a base brick stays"
    );
    let totals = d.message_totals();
    assert_eq!(totals.breaks, headless.events.len() as u64);
    assert!(totals.fx >= totals.breaks);
    assert!(
        totals.nav >= 4,
        "a nav region per tick that lost chunks: {totals:?}"
    );
    // Debris entities follow their bodies.
    let live = stats.active as usize;
    let mut q = app.world_mut().query::<&DestructionChunk>();
    assert_eq!(q.iter(app.world()).count(), live);
    assert!(live > 0);
}

#[test]
fn caps_recycle_the_oldest_and_rules_empty_the_pool() {
    let budget = DestructionBudget {
        max_active_chunks: 4,
        max_spawns_per_tick: 2,
        max_lifetime_secs: 0.5,
        sleep_despawn_secs: 0.0,
        ..DestructionBudget::default()
    };
    let mut app = app(budget);
    let mut worst_active = 0u32;
    let mut saw_pending = false;
    run_app(&mut app, SAMPLE_TICKS, |app| {
        let d = app.world().resource::<Destruction>();
        worst_active = worst_active.max(d.physics.stats().active);
        saw_pending |= d.world.pending_spawns() > 0;
        assert!(d.last_tick().spawned <= 2, "spawn cap: {:?}", d.last_tick());
    });
    assert!(worst_active <= 4, "active cap held: {worst_active}");
    assert!(saw_pending, "the blasts detach more than 2 chunks in a tick");
    // Let the queue drain and every body expire (0.5 s = 30 ticks).
    for _ in 0..240 {
        app.update();
    }
    let d = app.world().resource::<Destruction>();
    let stats = d.physics.stats().clone();
    assert_eq!(d.world.pending_spawns(), 0);
    assert_eq!(stats.active, 0, "{stats:?}");
    assert!(stats.evicted_total > 0, "{stats:?}");
    assert!(stats.pool_reuses > 0, "{stats:?}");
    assert!(stats.pool_free <= 4, "{stats:?}");
    let mut q = app.world_mut().query::<&DestructionChunk>();
    assert_eq!(q.iter(app.world()).count(), 0);
}

#[test]
fn zero_active_cap_removes_colliders_without_bodies() {
    let mut app = app(DestructionBudget {
        max_active_chunks: 0,
        ..DestructionBudget::default()
    });
    let ids = run_app(&mut app, 12, |_| {});
    let d = app.world().resource::<Destruction>();
    assert!(d.physics.intact_collider(ids.wall, 36).is_none());
    assert_eq!(d.physics.stats().active, 0);
    assert_eq!(d.physics.stats().spawned_total, 0);
    let inst = d.world.instance(ids.wall).expect("wall");
    assert_eq!(inst.chunk_states()[36], ChunkState::Detached);
}

#[test]
fn removing_an_instance_clears_its_bodies_and_entities() {
    let mut app = app(DestructionBudget::default());
    let ids = run_app(&mut app, 60, |_| {});
    let before = app.world().resource::<PhysicsWorld>().bodies.len();
    app.world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            assert!(d.remove(&mut physics, ids.bridge));
        });
    app.update();
    app.update();
    let d = app.world().resource::<Destruction>();
    assert!(d.world.instance(ids.bridge).is_none());
    assert!(d.root_entity(ids.bridge).is_none());
    assert!(d.physics.active().all(|a| a.destructible != ids.bridge));
    let after = app.world().resource::<PhysicsWorld>().bodies.len();
    assert!(
        after < before,
        "the bridge's fixed body left ({before} -> {after})"
    );
}

#[test]
fn replica_app_replays_the_server_app_and_spawns_the_same_debris() {
    let mut server = app(DestructionBudget::default());
    let mut client = App::new();
    client.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
        1.0 / 60.0,
    )));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    client.insert_resource(fixed).add_plugins((
        PhysicsPlugin,
        DestructionPlugin {
            authority: Authority::Replica,
        },
    ));
    // The client defines the same instances in the same order, so the ids agree.
    let (wall, bridge) = sample_assets().expect("assets");
    client
        .world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(
                &mut physics,
                wall,
                DestructiblePose::at(regia_physics::destruction::sample::WALL_POSE),
                SAMPLE_SEEDS[0],
            )
            .expect("wall");
            d.define(
                &mut physics,
                bridge,
                DestructiblePose::at(regia_physics::destruction::sample::BRIDGE_POSE),
                SAMPLE_SEEDS[1],
            )
            .expect("bridge");
        });
    let mut sent = 0u64;
    run_app(&mut server, SAMPLE_TICKS, |server| {
        // What the server's netcode would send this tick: every event past the last one sent.
        let events: Vec<_> = server
            .world()
            .resource::<Destruction>()
            .world
            .recent_events()
            .filter(|e| e.seq >= sent)
            .cloned()
            .collect();
        sent += events.len() as u64;
        client
            .world_mut()
            .resource_mut::<Destruction>()
            .world
            .apply_events(&events)
            .expect("apply");
        // The replica's next fixed tick spawns the debris.
        let target = client.world().resource::<Destruction>().world.tick() + 1;
        while client.world().resource::<Destruction>().world.tick() < target {
            client.update();
        }
        let s = server.world().resource::<Destruction>();
        let c = client.world().resource::<Destruction>();
        assert_eq!(s.world.structure_hash(), c.world.structure_hash());
        assert_eq!(s.physics.stats().spawned_total, c.physics.stats().spawned_total);
        assert_eq!(
            s.physics.stats().colliders_removed_total,
            c.physics.stats().colliders_removed_total
        );
    });
    let c = client.world().resource::<Destruction>();
    assert!(c.physics.stats().spawned_total > 0);
    assert_eq!(c.message_totals().breaks, sent);
    assert!(c.message_totals().nav > 0);
}

#[test]
fn chunk_hints_small_inertia_lifetime_fx_tag_and_compound_hulls() {
    use regia_physics::destruction::{
        BoxGrid, Damage, DamageKind, DestructibleAsset, DestructionMaterial, DestructionPhysics, FxKind,
        GridSupport, step_standalone,
    };
    // A row of four 1 m cubes; the first is held by the world.
    let mut asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "hints".into(),
        counts: [4, 1, 1],
        cell: [1.0; 3],
        density: 100.0,
        strength_per_area: 1.0,
        support: GridSupport::Explicit(vec![0]),
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    asset.chunks[1].small = true;
    asset.chunks[2].inertia = Some([2.0, 3.0, 4.0]);
    asset.chunks[2].lifetime_secs = Some(0.1);
    asset.chunks[3].fx_tag = Some("glass".into());
    let extra = asset.chunks[3].hulls[0].clone();
    asset.chunks[3].hulls.push(extra);
    asset.validate().expect("valid");
    let mut world = DestructionWorld::new(Authority::Server);
    let mut physics = PhysicsWorld::default();
    let mut bridge = DestructionPhysics::default();
    let id = world
        .define(
            std::sync::Arc::new(asset),
            DestructiblePose::at([0.0, 5.0, 0.0]),
            9,
        )
        .expect("define");
    bridge.attach(&mut physics, &world, id).expect("attach");
    world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Explosion {
                center: [1.0, 5.5, 0.0],
                radius: 10.0,
                impulse: 100.0,
            },
        })
        .expect("queue");
    let report = world.step(1.0 / 60.0);
    // Chunks 1..=3 detach; chunk 3's effect carries its own tag, the others the material's.
    let tags: Vec<(String, bool)> = report
        .fx
        .iter()
        .filter(|f| f.kind != FxKind::BondBreak)
        .map(|f| (f.fx_tag.clone(), f.fx_tag == "glass"))
        .collect();
    assert_eq!(tags.len(), 3, "{:?}", report.fx);
    assert_eq!(tags.iter().filter(|t| t.1).count(), 1, "{tags:?}");
    let sync = bridge.sync(&mut physics, &mut world, 1.0 / 60.0);
    // The small chunk gets no body; the others do.
    assert!(sync.despawned.contains(&(id, 1)), "{sync:?}");
    assert!(
        sync.spawned.contains(&(id, 2)) && sync.spawned.contains(&(id, 3)),
        "{sync:?}"
    );
    assert!(!sync.spawned.contains(&(id, 1)));
    // Given inertia: the body carries the chunk's mass; hull-derived: the collider does (rapier
    // folds mass changes in on the next step).
    step_standalone(&mut physics, 1.0 / 60.0, [0.0, -9.81, 0.0]);
    let body_of = |chunk: u32| {
        bridge
            .active()
            .find(|a| a.chunk == chunk)
            .map(|a| a.body)
            .expect("debris body")
    };
    let mass2 = physics
        .bodies
        .get(body_of(2))
        .map(|rb| rb.mass())
        .expect("body 2");
    let mass3 = physics
        .bodies
        .get(body_of(3))
        .map(|rb| rb.mass())
        .expect("body 3");
    assert!((mass2 - 100.0).abs() < 1e-3, "{mass2}");
    assert!((mass3 - 100.0).abs() < 1e-3, "{mass3}");
    // Chunk 2's own 0.1 s lifetime ends long before the budget's 30 s.
    for _ in 0..12 {
        step_standalone(&mut physics, 1.0 / 60.0, [0.0, -9.81, 0.0]);
        world.step(1.0 / 60.0);
        bridge.sync(&mut physics, &mut world, 1.0 / 60.0);
    }
    let alive: Vec<u32> = bridge.active().map(|a| a.chunk).collect();
    assert_eq!(alive, vec![3], "chunk 2 expired, chunk 3 lives");
}

#[test]
fn moving_the_root_moves_the_structure_and_where_hits_land() {
    use regia_physics::destruction::{Damage, DamageKind, DestructibleRoot};
    let mut app = app(DestructionBudget::default());
    let ids = run_app(&mut app, 2, |_| {});
    let root = app
        .world()
        .resource::<Destruction>()
        .root_entity(ids.bridge)
        .expect("root spawned");
    assert!(app.world().get::<DestructibleRoot>(root).is_some());
    // Lift the bridge 2 m by moving its root, as a game moves a structure on a vehicle.
    let lifted = regia_core::math::Vec3::new(0.0, 5.0, 5.0);
    app.world_mut()
        .get_mut::<regia_scene::Transform>(root)
        .expect("root transform")
        .translation = lifted;
    let target = app.world().resource::<Destruction>().world.tick() + 3;
    while app.world().resource::<Destruction>().world.tick() < target {
        app.update();
    }
    let d = app.world().resource::<Destruction>();
    assert_eq!(
        d.world.instance(ids.bridge).map(|i| i.pose().translation),
        Some([0.0, 5.0, 5.0])
    );
    let body = d.physics.intact_body(ids.bridge).expect("intact body");
    let rb = app
        .world()
        .resource::<PhysicsWorld>()
        .bodies
        .get(body)
        .expect("body");
    assert!(rb.is_kinematic(), "a moved structure is kinematic");
    assert!((rb.translation().y - 5.0).abs() < 1e-4, "{:?}", rb.translation());
    // The wall never moved and stays a fixed body.
    let wall = d.physics.intact_body(ids.wall).expect("wall body");
    assert!(
        app.world()
            .resource::<PhysicsWorld>()
            .bodies
            .get(wall)
            .is_some_and(|rb| rb.is_fixed())
    );
    // A blast at the old deck height reaches nothing now; one at the new height breaks planks.
    for (y, expect_hit) in [(3.15, false), (5.15, true)] {
        let before = app.world().resource::<Destruction>().world.counters().hits;
        app.world_mut()
            .resource_mut::<Destruction>()
            .world
            .damage(Damage {
                target: Some(ids.bridge),
                kind: DamageKind::Explosion {
                    center: [0.0, y, 5.0],
                    radius: 0.9,
                    impulse: 3000.0,
                },
            })
            .expect("queue");
        let target = app.world().resource::<Destruction>().world.tick() + 1;
        while app.world().resource::<Destruction>().world.tick() < target {
            app.update();
        }
        let after = app.world().resource::<Destruction>().world.counters().hits;
        assert_eq!(after > before, expect_hit, "blast at y {y}");
    }
}

#[test]
fn debris_of_a_moving_structure_keeps_its_velocity() {
    use regia_physics::destruction::{
        BoxGrid, Damage, DamageKind, DestructibleAsset, DestructionMaterial, DestructionPhysics, GridSupport,
        step_standalone,
    };
    let asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "cart".into(),
        counts: [2, 1, 1],
        cell: [1.0; 3],
        density: 100.0,
        strength_per_area: 1.0,
        support: GridSupport::Explicit(vec![0]),
        material: DestructionMaterial {
            detach_spin: 0.0,
            ..DestructionMaterial::default()
        },
    })
    .expect("grid");
    let mut world = DestructionWorld::new(Authority::Server);
    let mut physics = PhysicsWorld::default();
    let mut bridge = DestructionPhysics::default();
    let id = world
        .define(std::sync::Arc::new(asset), DestructiblePose::default(), 1)
        .expect("define");
    bridge.attach(&mut physics, &world, id).expect("attach");
    // Move the structure along +X at 6 m/s for a few ticks (0.1 m per 1/60 s).
    let dt = 1.0 / 60.0;
    for tick in 1..=5 {
        world
            .set_pose(id, DestructiblePose::at([0.1 * tick as f32, 0.0, 0.0]))
            .expect("move");
        bridge.follow_pose(&mut physics, &world, id).expect("follow");
        step_standalone(&mut physics, dt, [0.0; 3]);
    }
    // An impulse straight up on chunk 1 breaks its bond.
    world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 1,
                impulse: [0.0, 10.0, 0.0],
            },
        })
        .expect("queue");
    world.step(dt);
    let sync = bridge.sync(&mut physics, &mut world, dt);
    assert_eq!(sync.spawned, vec![(id, 1)]);
    let body = bridge.active().next().map(|a| a.body).expect("debris");
    let v = physics.bodies.get(body).map(|rb| rb.linvel()).expect("body");
    // Carried 6 m/s along X plus the decided 10 N s / 100 kg = 0.1 m/s up.
    assert!((v.x - 6.0).abs() < 0.05, "{v:?}");
    assert!((v.y - 0.1).abs() < 1e-3, "{v:?}");
}

/// Finding 2 of the Muse fixrev review: the intact structure and fresh debris bodies must
/// store the model's quaternion bits, not the axis-angle round trip (`atan2`, then rapier's
/// `sin`/`cos`). `PhysicsWorld::state_digest` hashes every rapier body on lockstep paths, and
/// the round trip's last digit differs per platform libm; the pooled spawn path already
/// bit-copied, so a fresh body used to disagree with a pooled one for the same chunk.
#[test]
fn destruction_spawns_store_the_model_rotation_bits() {
    use regia_core::math::Quat;
    use regia_physics::destruction::{
        BoxGrid, Damage, DamageKind, DestructibleAsset, DestructionMaterial, DestructionPhysics, GridSupport,
    };
    // Exact f32 bits of rotation-Y by 0.1023 (half-angle 0.05115): its axis-angle round trip
    // lands several ulps away in `y`, so a regression to `rotation(axis-angle)` fails here.
    // The literals are host-independent.
    let authored = Quat::from_xyzw(0.0, f32::from_bits(0x3d51_6b48), 0.0, f32::from_bits(0x3f7f_aa49));
    let asset = DestructibleAsset::box_grid(&BoxGrid {
        name: "rot".into(),
        counts: [4, 1, 1],
        cell: [1.0; 3],
        density: 100.0,
        strength_per_area: 1.0,
        support: GridSupport::Explicit(vec![0]),
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    let mut world = DestructionWorld::new(Authority::Server);
    let mut physics = PhysicsWorld::default();
    let mut bridge = DestructionPhysics::default();
    let id = world
        .define(
            std::sync::Arc::new(asset),
            DestructiblePose {
                translation: [0.0, 5.0, 0.0],
                rotation: authored.to_array(),
            },
            9,
        )
        .expect("define");
    bridge.attach(&mut physics, &world, id).expect("attach");
    // The model normalizes the pose on define; that normalized value is what rapier must store.
    let expected = Quat::from_array(world.instance(id).expect("instance").pose().rotation);
    let bits = |q: Quat| [q.x.to_bits(), q.y.to_bits(), q.z.to_bits(), q.w.to_bits()];
    let intact = bridge.intact_body(id).expect("intact body");
    let stored = physics.bodies.get(intact).expect("body").position().rotation;
    let stored = Quat::from_xyzw(stored.x, stored.y, stored.z, stored.w);
    assert_eq!(
        bits(stored),
        bits(expected),
        "the intact body must store the model pose bits, not the libm axis-angle round trip"
    );
    // Break a chunk loose: the fresh debris body must store the same bits.
    world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 1,
                impulse: [0.0, 10.0, 0.0],
            },
        })
        .expect("queue");
    world.step(1.0 / 60.0);
    let sync = bridge.sync(&mut physics, &mut world, 1.0 / 60.0);
    assert!(
        sync.spawned.contains(&(id, 1)),
        "the impulse broke chunk 1 loose: {sync:?}"
    );
    let debris = bridge
        .active()
        .find(|a| a.chunk == 1)
        .map(|a| a.body)
        .expect("chunk 1's debris body");
    let stored = physics.bodies.get(debris).expect("body").position().rotation;
    let stored = Quat::from_xyzw(stored.x, stored.y, stored.z, stored.w);
    assert_eq!(
        bits(stored),
        bits(expected),
        "fresh debris must store the model pose bits, not the libm axis-angle round trip"
    );
}

#[test]
fn an_origin_shift_moves_the_instance_poses_and_where_hits_land() {
    use regia_physics::destruction::{Damage, DamageKind};
    let mut app = app(DestructionBudget::default());
    let ids = run_app(&mut app, 2, |_| {});
    let before = app
        .world()
        .resource::<Destruction>()
        .world
        .instance(ids.bridge)
        .map(|i| i.pose().translation)
        .expect("bridge");
    let shift = [0.0, 0.0, 1024.0];
    app.world_mut()
        .resource_mut::<Destruction>()
        .world
        .shift_origin(shift);
    let after = app
        .world()
        .resource::<Destruction>()
        .world
        .instance(ids.bridge)
        .map(|i| i.pose().translation)
        .expect("bridge");
    for axis in 0..3 {
        assert_eq!(after[axis], before[axis] - shift[axis], "axis {axis}");
    }
    // The deck is at the same place in the shifted frame: a blast there breaks planks, one at the
    // old (now 1024 m away) local position reaches nothing.
    for (z, expect_hit) in [(5.0 - 1024.0, true), (5.0, false)] {
        let hits = app.world().resource::<Destruction>().world.counters().hits;
        app.world_mut()
            .resource_mut::<Destruction>()
            .world
            .damage(Damage {
                target: Some(ids.bridge),
                kind: DamageKind::Explosion {
                    center: [0.0, 3.15, z],
                    radius: 0.9,
                    impulse: 3000.0,
                },
            })
            .expect("queue");
        let target = app.world().resource::<Destruction>().world.tick() + 1;
        while app.world().resource::<Destruction>().world.tick() < target {
            app.update();
        }
        let now = app.world().resource::<Destruction>().world.counters().hits;
        assert_eq!(now > hits, expect_hit, "blast at z {z}");
    }
}
