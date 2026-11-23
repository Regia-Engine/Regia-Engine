// SPDX-License-Identifier: MPL-2.0
//! Impact damage (ADR-3272): contacts on an opted-in structure's intact colliders become hits.
//!
//! Every test runs a bare rapier world at 60 Hz in the order the plugin uses (destruction step and
//! bridge sync, then the physics step, then `DestructionImpact::convert`), with fixed inputs only:
//! a box dropped from low and high, a box resting for 600 steps, a ray-cast car at 15 m/s into a
//! chunked wall (twice, for the digest), the same crash on a replica world and with the feature
//! off, debris hits off and on, the per-tick cap, and the plugin's hook in an app. No GPU, window
//! or wall clock.

use std::sync::Arc;
use std::time::Duration;

use rapier3d::dynamics::{RigidBodyBuilder, RigidBodyHandle};
use rapier3d::geometry::ColliderBuilder;
use regia_app::prelude::*;
use regia_app::{App, TimeUpdateStrategy};
use regia_core::math::{Quat, Vec3};
use regia_physics::convert::to_rapier_vec3;
use regia_physics::destruction::{
    Authority, BoxGrid, BreakKind, ChunkState, DestructibleAsset, DestructibleId, DestructiblePose,
    Destruction, DestructionImpact, DestructionMaterial, DestructionPhysics, DestructionPlugin,
    DestructionWorld, DetachCause, GridSupport, ImpactHit, ImpactSettings, add_fixed_box, step_standalone,
};
use regia_physics::vehicle::{Vehicle, VehicleConfig, VehicleInput};
use regia_physics::{PhysicsPlugin, PhysicsWorld};

const DT: f32 = 1.0 / 60.0;
const GRAVITY: [f32; 3] = [0.0, -9.81, 0.0];

/// A world with ground (top at y = 0), a server or replica destruction world and its bridge.
struct Scene {
    physics: PhysicsWorld,
    world: DestructionWorld,
    bridge: DestructionPhysics,
    impact: DestructionImpact,
}

impl Scene {
    fn new(authority: Authority) -> Self {
        let mut physics = PhysicsWorld::default();
        add_fixed_box(&mut physics, [0.0, -0.5, 0.0], [100.0, 0.5, 100.0]);
        Self {
            physics,
            world: DestructionWorld::new(authority),
            bridge: DestructionPhysics::default(),
            impact: DestructionImpact::default(),
        }
    }

    fn define(&mut self, asset: DestructibleAsset, at: [f32; 3]) -> DestructibleId {
        let id = self
            .world
            .define(Arc::new(asset), DestructiblePose::at(at), 7)
            .expect("define");
        self.bridge
            .attach(&mut self.physics, &self.world, id)
            .expect("attach");
        id
    }

    /// One tick in the plugin's order; `before_physics` runs between the destruction step and the
    /// physics step (a vehicle update).
    fn tick(&mut self, before_physics: &mut dyn FnMut(&mut PhysicsWorld)) {
        self.world.step(DT);
        self.bridge.sync(&mut self.physics, &mut self.world, DT);
        before_physics(&mut self.physics);
        step_standalone(&mut self.physics, DT, GRAVITY);
        self.impact
            .convert(&self.physics, &self.bridge, &mut self.world, DT);
    }

    fn run(&mut self, ticks: usize) -> Vec<ImpactHit> {
        let mut hits = Vec::new();
        for _ in 0..ticks {
            self.tick(&mut |_| {});
            hits.extend_from_slice(self.impact.last_hits());
        }
        hits
    }

    fn bond_damage(&self, id: DestructibleId) -> Vec<f32> {
        self.world.instance(id).expect("instance").bond_damage().to_vec()
    }

    fn add_box(&mut self, centre: [f32; 3], half: f32, mass: f32) -> RigidBodyHandle {
        let body = self.physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::from_array(centre)))
                .build(),
        );
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut self.physics;
        colliders.insert_with_parent(
            ColliderBuilder::cuboid(half, half, half).mass(mass).build(),
            body,
            bodies,
        );
        body
    }
}

/// A wall `nx` x `ny` x 1 cells on the ground, bottom row supported.
fn wall(nx: u32, ny: u32, cell: [f32; 3], strength: f32, material: DestructionMaterial) -> DestructibleAsset {
    DestructibleAsset::box_grid(&BoxGrid {
        name: "wall".into(),
        counts: [nx, ny, 1],
        cell,
        density: 2400.0,
        strength_per_area: strength,
        support: GridSupport::Bottom,
        material,
    })
    .expect("wall")
}

/// The drop scene: a strong 6 x 4 wall of 1 m cells (top at y = 4) and a 50 kg box of 0.4 m
/// centred over chunk 20 (x 2, y 3), `height` metres above the top.
fn drop_scene(
    height: f32,
    material: DestructionMaterial,
    settings: ImpactSettings,
) -> (Scene, DestructibleId) {
    let mut s = Scene::new(Authority::Server);
    let id = s.define(wall(6, 4, [1.0; 3], 1.0e9, material), [0.0, 0.0, 0.0]);
    s.impact.set_settings(settings).expect("settings");
    s.impact.enable(&s.world, id).expect("enable");
    s.add_box([-0.5, 4.0 + 0.2 + height, 0.0], 0.2, 50.0);
    (s, id)
}

/// Chunk 20's bonds in the 6 x 4 wall.
fn bonds_of(s: &Scene, id: DestructibleId, chunk: u32) -> Vec<bool> {
    s.world
        .instance(id)
        .expect("instance")
        .asset()
        .bonds
        .iter()
        .map(|b| b.a == chunk || b.b == chunk)
        .collect()
}

#[test]
fn a_box_dropped_from_low_height_deals_no_damage() {
    let material = DestructionMaterial {
        min_impulse: 300.0,
        ..DestructionMaterial::default()
    };
    // A low speed gate, so the material's `min_impulse` is what stops the hit.
    let settings = ImpactSettings {
        min_speed: 0.1,
        ..ImpactSettings::default()
    };
    let (mut s, id) = drop_scene(0.1, material, settings);
    let hits = s.run(120);
    assert!(hits.is_empty(), "{hits:?}");
    assert!(s.bond_damage(id).iter().all(|d| *d == 0.0));
    let t = *s.impact.totals();
    assert!(
        t.below_min_impulse > 0,
        "the landing was measured and refused: {t:?}"
    );
    assert_eq!(t.hits, 0);
    assert_eq!(s.world.queued_damage(), 0);
}

#[test]
fn a_box_dropped_from_high_damages_the_contact_chunk_only() {
    let material = DestructionMaterial {
        min_impulse: 300.0,
        ..DestructionMaterial::default()
    };
    let (mut s, id) = drop_scene(8.0, material, ImpactSettings::default());
    let hits = s.run(150);
    println!("IMPACT_DROP_HIGH hits={hits:?}");
    assert!(
        !hits.is_empty(),
        "an 8 m drop of 50 kg hits: {:?}",
        s.impact.totals()
    );
    assert!(
        hits.iter().all(|h| h.chunk == 20 && h.destructible == id),
        "{hits:?}"
    );
    assert!(
        hits.iter().all(|h| h.direction[1] < -0.9),
        "pushes down: {hits:?}"
    );
    let incident = bonds_of(&s, id, 20);
    let damage = s.bond_damage(id);
    assert!(
        damage.iter().zip(&incident).any(|(d, i)| *i && *d > 0.0),
        "the contact chunk's bonds took damage: {damage:?}"
    );
    for (b, (d, i)) in damage.iter().zip(&incident).enumerate() {
        assert!(*i || *d == 0.0, "bond {b} is not chunk 20's but took {d}");
    }
    // One landing is one hit: the pair cools down while the box bounces and settles.
    assert_eq!(hits.len(), 1, "{hits:?}");
}

#[test]
fn a_resting_contact_held_for_600_steps_adds_no_damage() {
    // Default material (`min_impulse` 0) and settings: the speed gate is what refuses resting.
    let (mut s, id) = drop_scene(0.0, DestructionMaterial::default(), ImpactSettings::default());
    let settle = s.run(60);
    assert!(settle.is_empty(), "placing a box does not hit: {settle:?}");
    let before = s.bond_damage(id);
    let t0 = *s.impact.totals();
    let hits = s.run(600);
    assert!(hits.is_empty(), "{hits:?}");
    assert_eq!(s.bond_damage(id), before);
    let t = *s.impact.totals();
    assert!(
        t.below_speed - t0.below_speed >= 600,
        "the box touched the wall every step and was refused by speed: {t:?}"
    );
}

/// The crash: a 28 x 6 wall of 0.5 m cells (14 m wide, 3 m tall, 0.5 m thick) at z = 0 and the
/// default ray-cast car (1200 kg) settled at z = 9 facing it, then launched at 15 m/s.
struct Crash {
    scene: Scene,
    id: DestructibleId,
    hits: Vec<ImpactHit>,
}

const CRASH_CELL: f32 = 0.5;
const CRASH_SPEED: f32 = 15.0;

fn crash(authority: Authority, enabled: bool) -> Crash {
    let mut scene = Scene::new(authority);
    let id = scene.define(
        wall(28, 6, [CRASH_CELL; 3], 1000.0, DestructionMaterial::default()),
        [0.0, 0.0, 0.0],
    );
    if enabled {
        scene.impact.enable(&scene.world, id).expect("enable");
    }
    let config = VehicleConfig::default();
    let wheel = &config.wheels[0];
    let compression = 9.81 / (config.wheels.len() as f32 * wheel.suspension_stiffness);
    let rest = -wheel.position.y + (wheel.suspension_rest_length - compression) + wheel.radius;
    let mut car = Vehicle::spawn(
        &mut scene.physics,
        config,
        Vec3::new(0.0, rest + 0.05, 9.0),
        Quat::IDENTITY,
    )
    .expect("spawn");
    fn drive(scene: &mut Scene, car: &mut Vehicle, n: usize, hits: &mut Vec<ImpactHit>) {
        for _ in 0..n {
            scene.tick(&mut |physics| {
                car.update(physics, VehicleInput::default(), DT).expect("update");
            });
            hits.extend_from_slice(scene.impact.last_hits());
        }
    }
    let mut hits = Vec::new();
    drive(&mut scene, &mut car, 120, &mut hits);
    assert!(hits.is_empty(), "settling hits nothing");
    let chassis = car.chassis().to_handle();
    let rb = scene.physics.bodies.get_mut(chassis).expect("chassis");
    rb.enable_ccd(true);
    rb.set_linvel(to_rapier_vec3(Vec3::new(0.0, 0.0, -CRASH_SPEED)), true);
    drive(&mut scene, &mut car, 120, &mut hits);
    Crash { scene, id, hits }
}

/// Chunks detached by damage (`cause: Damage`) and by collapse, from the world's event log.
fn detached(c: &Crash) -> (Vec<u32>, Vec<u32>) {
    let mut damage = Vec::new();
    let mut collapse = Vec::new();
    for e in c.scene.world.recent_events() {
        if let BreakKind::ChunkDetached { chunk, cause, .. } = e.kind {
            match cause {
                DetachCause::Damage => damage.push(chunk),
                DetachCause::Collapse => collapse.push(chunk),
            }
        }
    }
    (damage, collapse)
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    Vec3::from_array(a).distance(Vec3::from_array(b))
}

#[test]
fn a_car_at_15_mps_breaks_chunks_near_the_contact_and_none_beyond() {
    let c = crash(Authority::Server, true);
    let (by_damage, by_collapse) = detached(&c);
    let inst = c.scene.world.instance(c.id).expect("wall");
    println!(
        "IMPACT_CRASH hits={} damage={by_damage:?} collapse={by_collapse:?} attached={} totals={:?}",
        c.hits.len(),
        inst.attached_count(),
        c.scene.impact.totals()
    );
    for h in &c.hits {
        println!("  hit {h:?}");
    }
    assert!(!c.hits.is_empty(), "the crash hit the wall");
    assert!(!by_damage.is_empty(), "the crash broke chunks loose");
    // The documented radius (book/agents/destruction.md, "Impact damage"): the colliding body's
    // bounding-sphere diameter plus one chunk's half-diagonal, around the first contact point.
    let half = VehicleConfig::default().chassis_half_extents;
    let radius = 2.0 * half.length() + (3.0f32).sqrt() * CRASH_CELL * 0.5;
    let first = c.hits[0].point;
    let mut beyond = 0;
    for chunk in 0..inst.asset().chunks.len() as u32 {
        let centre = inst.world_centroid(chunk).expect("centroid");
        let d = distance(centre, first);
        let state = inst.chunk_states()[chunk as usize];
        if d > radius {
            beyond += 1;
            assert_eq!(
                state,
                ChunkState::Attached,
                "chunk {chunk} at {d} m is beyond {radius} m"
            );
        }
    }
    assert!(
        beyond > 0,
        "the wall reaches beyond the radius, so the check means something"
    );
    // Tighter, for this head-on crash: every broken chunk lies across the car's front face, within
    // one chunk of its half-width.
    for chunk in by_damage.iter().chain(&by_collapse) {
        let centre = inst.world_centroid(*chunk).expect("centroid");
        assert!(
            centre[0].abs() <= half.x + CRASH_CELL,
            "chunk {chunk} at x {}",
            centre[0]
        );
    }
    // Every hit is on the face the car met, pushing into the wall.
    for h in &c.hits {
        assert!(h.direction[2] < -0.5, "{h:?}");
        assert!(h.speed >= ImpactSettings::default().min_speed);
    }
}

#[test]
fn two_crash_runs_give_the_same_breaks() {
    let a = crash(Authority::Server, true);
    let b = crash(Authority::Server, true);
    assert_eq!(a.hits, b.hits);
    assert_eq!(a.scene.world.event_digest(), b.scene.world.event_digest());
    assert_eq!(a.scene.world.structure_hash(), b.scene.world.structure_hash());
    assert_eq!(a.scene.world.state_hash(), b.scene.world.state_hash());
    assert!(a.scene.world.event_count() > 0);
    println!(
        "IMPACT_CRASH_DIGEST events={} digest={:#018x} structure={:#018x}",
        a.scene.world.event_count(),
        a.scene.world.event_digest(),
        a.scene.world.structure_hash()
    );
}

#[test]
fn a_replica_world_produces_no_local_damage() {
    let c = crash(Authority::Replica, true);
    assert!(c.hits.is_empty(), "{:?}", c.hits);
    assert!(c.scene.impact.last_report().replica);
    assert_eq!(
        c.scene.impact.totals().pairs,
        0,
        "a replica does not read the contacts"
    );
    assert_eq!(c.scene.world.queued_damage(), 0);
    assert_eq!(c.scene.world.event_count(), 0);
    let inst = c.scene.world.instance(c.id).expect("wall");
    assert_eq!(inst.attached_count(), inst.asset().chunks.len());
}

#[test]
fn with_the_feature_off_the_car_bounces_off_as_before() {
    let off = crash(Authority::Server, false);
    assert!(off.hits.is_empty());
    assert_eq!(off.scene.world.event_count(), 0);
    assert_eq!(*off.scene.impact.totals(), Default::default(), "nothing was read");
    let inst = off.scene.world.instance(off.id).expect("wall");
    assert!(inst.bond_damage().iter().all(|d| *d == 0.0));
    assert_eq!(inst.attached_count(), inst.asset().chunks.len());
}

/// A two-chunk column held at its top (chunk 1) hanging over the 6 x 4 wall's chunk 20: knocking
/// its bond loose drops chunk 0 (debris) 1.5 m onto the wall.
fn debris_scene(debris_hits: bool) -> (Scene, DestructibleId) {
    let mut s = Scene::new(Authority::Server);
    let material = DestructionMaterial {
        min_impulse: 1.0,
        ..DestructionMaterial::default()
    };
    let id = s.define(wall(6, 4, [1.0; 3], 1.0e9, material), [0.0, 0.0, 0.0]);
    let column = DestructibleAsset::box_grid(&BoxGrid {
        name: "column".into(),
        counts: [1, 2, 1],
        cell: [0.4; 3],
        density: 2400.0,
        strength_per_area: 10.0,
        support: GridSupport::Explicit(vec![1]),
        material: DestructionMaterial::default(),
    })
    .expect("column");
    let hanging = s.define(column, [-0.5, 5.5, 0.0]);
    s.impact
        .set_settings(ImpactSettings {
            debris_hits,
            min_speed: 0.5,
            ..ImpactSettings::default()
        })
        .expect("settings");
    s.impact.enable(&s.world, id).expect("enable");
    s.world
        .damage(regia_physics::destruction::Damage {
            target: Some(hanging),
            kind: regia_physics::destruction::DamageKind::Impulse {
                chunk: 0,
                impulse: [0.0, -100.0, 0.0],
            },
        })
        .expect("knock");
    (s, id)
}

#[test]
fn debris_hits_are_off_by_default_and_settable() {
    assert!(!ImpactSettings::default().debris_hits);
    let (mut off, id) = debris_scene(false);
    let hits = off.run(120);
    assert!(hits.is_empty(), "{hits:?}");
    assert!(
        off.impact.totals().debris_skipped > 0,
        "{:?}",
        off.impact.totals()
    );
    assert!(off.bond_damage(id).iter().all(|d| *d == 0.0));
    let (mut on, id) = debris_scene(true);
    let hits = on.run(120);
    assert!(!hits.is_empty(), "{:?}", on.impact.totals());
    assert!(hits.iter().all(|h| h.debris && h.destructible == id), "{hits:?}");
    // The landing is the first and strongest hit, on the chunk under the piece; a piece that tips
    // after landing may touch a neighbour once more (each pair once per cooldown).
    assert_eq!(hits[0].chunk, 20, "{hits:?}");
    assert!(hits.iter().all(|h| h.impulse <= hits[0].impulse), "{hits:?}");
}

#[test]
fn the_cap_keeps_the_strongest_hits_and_counts_the_rest() {
    let mut c = Scene::new(Authority::Server);
    let id = c.define(
        wall(6, 1, [1.0; 3], 1.0e9, DestructionMaterial::default()),
        [0.0, 0.0, 0.0],
    );
    c.impact
        .set_settings(ImpactSettings {
            max_hits_per_tick: 2,
            min_speed: 0.5,
            ..ImpactSettings::default()
        })
        .expect("settings");
    c.impact.enable(&c.world, id).expect("enable");
    // Six boxes of rising mass over the six chunks, dropped together from 3 m.
    for i in 0..6 {
        c.add_box(
            [-2.5 + i as f32, 1.0 + 0.2 + 3.0, 0.0],
            0.2,
            20.0 + 10.0 * i as f32,
        );
    }
    let mut capped_tick = None;
    for _ in 0..90 {
        c.tick(&mut |_| {});
        let r = *c.impact.last_report();
        if r.dropped_by_cap > 0 {
            capped_tick = Some((r, c.impact.last_hits().to_vec()));
            break;
        }
    }
    let (r, hits) = capped_tick.expect("the six landings came in one tick");
    assert_eq!(r.hits, 2);
    assert_eq!(r.dropped_by_cap, 4, "{r:?}");
    assert!(hits[0].impulse >= hits[1].impulse);
    // The heaviest boxes are over chunks 5 and 4.
    let mut chunks: Vec<u32> = hits.iter().map(|h| h.chunk).collect();
    chunks.sort_unstable();
    assert_eq!(chunks, vec![4, 5], "{hits:?}");
}

#[test]
fn the_plugin_converts_after_the_physics_step() {
    let mut app = App::new();
    app.add_plugins((MinimalPlugins, PhysicsPlugin, DestructionPlugin::default()))
        .insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_millis(16)));
    let id = {
        let world = app.world_mut();
        let mut physics = world.remove_resource::<PhysicsWorld>().expect("physics");
        let mut d = world.remove_resource::<Destruction>().expect("destruction");
        let id = d
            .define(
                &mut physics,
                Arc::new(wall(6, 4, [1.0; 3], 1.0e9, DestructionMaterial::default())),
                DestructiblePose::default(),
                3,
            )
            .expect("define");
        add_fixed_box(&mut physics, [0.0, -0.5, 0.0], [50.0, 0.5, 50.0]);
        let body = physics.bodies.insert(
            RigidBodyBuilder::dynamic()
                .translation(to_rapier_vec3(Vec3::new(-0.5, 9.0, 0.0)))
                .build(),
        );
        let PhysicsWorld {
            bodies, colliders, ..
        } = &mut physics;
        colliders.insert_with_parent(
            ColliderBuilder::cuboid(0.2, 0.2, 0.2).mass(50.0).build(),
            body,
            bodies,
        );
        world
            .resource_mut::<DestructionImpact>()
            .enable(&d.world, id)
            .expect("enable");
        world.insert_resource(d);
        world.insert_resource(physics);
        id
    };
    let mut guard = 0;
    while app.world().resource::<DestructionImpact>().totals().hits == 0 {
        app.update();
        guard += 1;
        assert!(
            guard < 600,
            "no hit: {:?}",
            app.world().resource::<DestructionImpact>().totals()
        );
    }
    // The hit is queued after the physics step and lands at the next destruction step.
    assert_eq!(app.world().resource::<Destruction>().world.queued_damage(), 1);
    let tick = app.world().resource::<Destruction>().world.tick();
    while app.world().resource::<Destruction>().world.tick() == tick {
        app.update();
    }
    let d = app.world().resource::<Destruction>();
    assert!(
        d.world
            .instance(id)
            .expect("wall")
            .bond_damage()
            .iter()
            .any(|v| *v > 0.0)
    );
    assert_eq!(d.last_tick().hits, 1);
}
