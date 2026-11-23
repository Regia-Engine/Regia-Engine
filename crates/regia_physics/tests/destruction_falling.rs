// SPDX-License-Identifier: MPL-2.0
//! Falling sections and free bodies (ADR-2734): a section that lost its support falls as one body
//! with the chunks' summed mass properties, splits into islands on impact with its momentum kept,
//! and a free object falls from its first tick. Off by default: nothing changes.

use std::sync::Arc;

use regia_physics::destruction::{
    Authority, BoxGrid, Damage, DamageKind, DestructibleAsset, DestructibleId, DestructiblePose,
    DestructionMaterial, DestructionPhysics, DestructionWorld, FallingSettings, GridSupport, add_fixed_box,
    step_standalone,
};
use regia_physics::world::PhysicsWorld;

const DT: f32 = 1.0 / 60.0;
const GRAVITY: [f32; 3] = [0.0, -9.81, 0.0];

/// A column of four 1 m cells hanging from its top cell (the support); bond 2-3 is weak.
fn hanging(falling: bool) -> DestructibleAsset {
    let mut a = DestructibleAsset::box_grid(&BoxGrid {
        name: "hanging".into(),
        counts: [1, 4, 1],
        cell: [1.0, 1.0, 1.0],
        density: 100.0,
        strength_per_area: 100.0,
        support: GridSupport::Explicit(vec![3]),
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    a.bonds[2].break_impulse = 1.0;
    // Mass differs per chunk so the centre of mass is not the middle.
    a.chunks[0].mass = 300.0;
    if falling {
        a.falling = Some(FallingSettings::default());
    }
    a.validate().expect("valid");
    a
}

struct Scene {
    world: DestructionWorld,
    bridge: DestructionPhysics,
    physics: PhysicsWorld,
    id: DestructibleId,
}

fn scene(asset: DestructibleAsset, at: [f32; 3], authority: Authority) -> Scene {
    let mut world = DestructionWorld::new(authority);
    let mut physics = PhysicsWorld::default();
    add_fixed_box(&mut physics, [0.0, -0.5, 0.0], [20.0, 0.5, 20.0]);
    let id = world
        .define(Arc::new(asset), DestructiblePose::at(at), 3)
        .expect("define");
    let mut bridge = DestructionPhysics::default();
    bridge.attach(&mut physics, &world, id).expect("attach");
    Scene {
        world,
        bridge,
        physics,
        id,
    }
}

impl Scene {
    fn tick(&mut self) -> regia_physics::destruction::BridgeReport {
        self.world.step(DT);
        let report = self.bridge.sync(&mut self.physics, &mut self.world, DT);
        step_standalone(&mut self.physics, DT, GRAVITY);
        report
    }

    fn cut(&mut self) {
        // Chunk 2's bonds: 1-2 (strong) and 2-3 (weak). 2 N s shared by two bonds breaks the weak one.
        self.world
            .damage(Damage {
                target: Some(self.id),
                kind: DamageKind::Impulse {
                    chunk: 2,
                    impulse: [0.0, 2.0, 0.0],
                },
            })
            .expect("cut");
    }
}

#[test]
fn a_falling_section_is_one_body_with_the_chunks_mass_and_centre() {
    let mut s = scene(hanging(true), [0.0, 10.0, 0.0], Authority::Server);
    s.tick();
    s.cut();
    // Checked right after the bridge built it, before rapier moves it.
    s.world.step(DT);
    let report = s.bridge.sync(&mut s.physics, &mut s.world, DT);
    assert_eq!(report.sections_spawned, vec![(s.id, 0)], "{report:?}");
    assert_eq!(report.spawned.len(), 3);
    let sec = s.bridge.sections().next().expect("section").clone();
    assert_eq!(sec.chunks, vec![0, 1, 2]);
    assert_eq!(sec.bonds, vec![0, 1], "bonds 0-1 and 1-2 hold it");
    assert_eq!(s.bridge.stats().active, 1, "one body counts against the cap");
    let inst = s.world.instance(s.id).expect("inst");
    let pose = inst.pose();
    let (mut mass, mut moment) = (0.0f64, [0.0f64; 3]);
    for c in &sec.chunks {
        let chunk = &inst.asset().chunks[*c as usize];
        let w = pose.transform_point(chunk.centroid);
        mass += f64::from(chunk.mass);
        for (m, wk) in moment.iter_mut().zip(w) {
            *m += f64::from(chunk.mass) * f64::from(wk);
        }
    }
    let rb = s.physics.bodies.get(sec.body).expect("body");
    assert!(
        (f64::from(rb.mass()) - mass).abs() <= 1e-4 * mass,
        "{} vs {mass}",
        rb.mass()
    );
    let com = rb.center_of_mass();
    for (k, got) in [com.x, com.y, com.z].into_iter().enumerate() {
        let want = moment[k] / mass;
        assert!((f64::from(got) - want).abs() <= 1e-4, "axis {k}: {got} vs {want}");
    }
    // Each chunk's pose follows the section body.
    let (t, _) = s.bridge.debris_pose(&s.physics, s.id, 1).expect("chunk 1 pose");
    assert!((t.y - 11.5).abs() < 1e-3, "{t:?}");
}

#[test]
fn a_section_splits_on_impact_and_keeps_its_momentum() {
    let mut s = scene(hanging(true), [0.0, 6.0, 0.0], Authority::Server);
    s.tick();
    s.cut();
    let mut split = None;
    for _ in 0..240 {
        let report = s.tick();
        if let Some(sp) = report.splits.into_iter().next() {
            split = Some(sp);
            break;
        }
    }
    let split = split.expect("the section split when it landed");
    assert_eq!(split.islands, 3, "{split:?}");
    let before = split.momentum_before;
    let after = split.momentum_after;
    let size = (before.iter().map(|v| v * v).sum::<f32>()).sqrt().max(1.0);
    for (b, a) in before.iter().zip(&after) {
        assert!((b - a).abs() <= 0.01 * size, "momentum {before:?} → {after:?}");
    }
    assert_eq!(s.bridge.sections().count(), 0);
    assert_eq!(s.bridge.stats().active, 3, "three single debris bodies now");
    assert_eq!(s.bridge.stats().splits_total, 1);
}

#[test]
fn without_falling_the_section_breaks_into_chunks_as_before() {
    let mut s = scene(hanging(false), [0.0, 10.0, 0.0], Authority::Server);
    s.tick();
    s.cut();
    let report = s.tick();
    assert!(report.sections_spawned.is_empty());
    assert_eq!(report.spawned.len(), 3);
    assert_eq!(s.bridge.sections().count(), 0);
    assert_eq!(s.bridge.stats().active, 3);
}

#[test]
fn a_replica_groups_the_same_sections() {
    let mut server = DestructionWorld::new(Authority::Server);
    let mut replica = DestructionWorld::new(Authority::Replica);
    let asset = Arc::new(hanging(true));
    let a = server
        .define(asset.clone(), DestructiblePose::at([0.0, 10.0, 0.0]), 3)
        .expect("server");
    let b = replica
        .define(asset, DestructiblePose::at([0.0, 10.0, 0.0]), 3)
        .expect("replica");
    server
        .damage(Damage {
            target: Some(a),
            kind: DamageKind::Impulse {
                chunk: 2,
                impulse: [0.0, 2.0, 0.0],
            },
        })
        .expect("cut");
    let report = server.step(DT);
    replica.apply_events(&report.events).expect("apply");
    let groups = server.take_spawn_groups(0);
    assert_eq!(groups.len(), 1);
    assert_eq!(groups, replica.take_spawn_groups(0));
    assert_eq!(server.structure_hash(), replica.structure_hash());
    assert_eq!(
        server.instance(a).expect("a").section_bonds(0),
        replica.instance(b).expect("b").section_bonds(0)
    );
}

fn crate_box(falling: bool) -> DestructibleAsset {
    let mut a = DestructibleAsset::box_grid(&BoxGrid {
        name: "crate".into(),
        counts: [2, 2, 2],
        cell: [0.5, 0.5, 0.5],
        density: 400.0,
        strength_per_area: 1.0e6,
        support: GridSupport::None,
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    if falling {
        a.falling = Some(FallingSettings::default());
    }
    a
}

#[test]
fn a_free_object_falls_and_lands_and_stays_put_without_the_setting() {
    let mut s = scene(crate_box(true), [0.0, 5.0, 0.0], Authority::Server);
    assert!(s.bridge.is_free_body(s.id));
    assert_eq!(s.bridge.stats().free_bodies, 1);
    for _ in 0..240 {
        s.tick();
    }
    let y = s.world.instance(s.id).expect("inst").pose().translation[1];
    assert!(y.abs() < 0.05, "the crate rests on the ground: y = {y}");
    // Hits land where the crate is now.
    s.world
        .damage(Damage {
            target: None,
            kind: DamageKind::Projectile {
                origin: [0.0, 0.5, 5.0],
                direction: [0.0, 0.0, -1.0],
                impulse: 1.0,
                radius: 0.0,
                max_distance: 10.0,
            },
        })
        .expect("shot");
    let report = s.world.step(DT);
    assert_eq!(report.misses, 0, "the shot found the crate on the ground");

    let mut s = scene(crate_box(false), [0.0, 5.0, 0.0], Authority::Server);
    assert!(!s.bridge.is_free_body(s.id));
    for _ in 0..240 {
        s.tick();
    }
    assert_eq!(s.world.instance(s.id).expect("inst").pose().translation[1], 5.0);

    // A replica does not simulate it: it follows the pose its game replicates.
    let s = scene(crate_box(true), [0.0, 5.0, 0.0], Authority::Replica);
    assert!(!s.bridge.is_free_body(s.id));
}

/// Cost of a falling section against one body per chunk (ADR-2734): an 8 x 8 wall hanging from its
/// top row loses its second row from the top, so 48 bonded chunks fall 6 m onto the ground. Prints
/// bodies, contact pairs and the wall-clock of the bridge sync and the rapier step over 180 ticks.
/// Run: `cargo test -p regia_physics --test destruction_falling -- --ignored --nocapture`.
#[test]
#[ignore = "a measurement for the ADR, not a check"]
fn section_cost_against_one_body_per_chunk() {
    for falling in [false, true] {
        let mut asset = DestructibleAsset::box_grid(&BoxGrid {
            name: "panel".into(),
            counts: [8, 8, 1],
            cell: [0.5, 0.5, 0.25],
            density: 2000.0,
            strength_per_area: 4.0e4,
            support: GridSupport::Explicit((56..64).collect()),
            material: DestructionMaterial::default(),
        })
        .expect("grid");
        if falling {
            asset.falling = Some(FallingSettings::default());
        }
        let mut s = scene(asset, [0.0, 6.0, 0.0], Authority::Server);
        s.tick();
        for chunk in 48..56 {
            s.world
                .damage(Damage {
                    target: Some(s.id),
                    kind: DamageKind::Impulse {
                        chunk,
                        impulse: [0.0, 0.0, 1.0e6],
                    },
                })
                .expect("cut");
        }
        let (mut sync_us, mut step_us, mut max_bodies, mut max_pairs) = (0u128, 0u128, 0usize, 0usize);
        for _ in 0..180 {
            s.world.step(DT);
            let t = std::time::Instant::now();
            s.bridge.sync(&mut s.physics, &mut s.world, DT);
            sync_us += t.elapsed().as_micros();
            let t = std::time::Instant::now();
            step_standalone(&mut s.physics, DT, GRAVITY);
            step_us += t.elapsed().as_micros();
            max_bodies = max_bodies.max(s.bridge.stats().active as usize);
            max_pairs = max_pairs.max(
                s.physics
                    .narrow_phase
                    .contact_pairs()
                    .filter(|p| p.has_any_active_contact())
                    .count(),
            );
        }
        println!(
            "falling={falling}: debris bodies (max) {max_bodies}, active contact pairs (max) {max_pairs}, sync {sync_us} us, rapier step {step_us} us over 180 ticks, splits {}",
            s.bridge.stats().splits_total
        );
    }
}

/// Inside an app: each chunk of a falling section gets its `DestructionChunk` entity, and its
/// `Transform` follows the section body down.
#[test]
fn section_chunks_have_entities_that_follow_the_body() {
    use regia_app::{App, FixedTime, TimeUpdateStrategy};
    use regia_physics::PhysicsPlugin;
    use regia_physics::destruction::{Destruction, DestructionChunk, DestructionPlugin};
    use regia_scene::Transform;

    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed)
        .add_plugins((PhysicsPlugin, DestructionPlugin::default()));
    let id = app
        .world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(
                &mut physics,
                Arc::new(hanging(true)),
                DestructiblePose::at([0.0, 30.0, 0.0]),
                3,
            )
            .expect("define")
        });
    for _ in 0..3 {
        app.update();
    }
    app.world_mut()
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 2,
                impulse: [0.0, 2.0, 0.0],
            },
        })
        .expect("cut");
    for _ in 0..3 {
        app.update();
    }
    let poses = |app: &mut App| {
        let mut q = app.world_mut().query::<(&DestructionChunk, &Transform)>();
        let mut v: Vec<(u32, regia_core::math::Vec3)> = q
            .iter(app.world())
            .map(|(c, t)| (c.chunk, t.translation))
            .collect();
        v.sort_by_key(|(c, _)| *c);
        v
    };
    let first = poses(&mut app);
    assert_eq!(first.iter().map(|(c, _)| *c).collect::<Vec<_>>(), vec![0, 1, 2]);
    assert_eq!(
        app.world().resource::<Destruction>().physics.sections().count(),
        1
    );
    for _ in 0..20 {
        app.update();
    }
    let later = poses(&mut app);
    for ((c, p0), (_, p1)) in first.iter().zip(&later) {
        assert!(p1.y < p0.y, "chunk {c} fell: {p0} -> {p1}");
    }
    // The chunks keep their spacing: one rigid body.
    let gap0 = first[1].1.distance(first[0].1);
    let gap1 = later[1].1.distance(later[0].1);
    assert!(
        (gap0 - 1.0).abs() < 1e-3 && (gap1 - 1.0).abs() < 1e-3,
        "{gap0} vs {gap1}"
    );
}

/// `step_destruction` must copy the gravity the previous physics step applied. Without that,
/// `impact_sections` subtracts Earth gravity from a velocity change that was not caused by it,
/// so a free section in zero-g (no contact) accumulates bond damage and can split.
#[test]
fn a_section_in_zero_gravity_takes_no_impact_damage_without_a_hit() {
    use regia_app::{App, FixedTime, TimeUpdateStrategy};
    use regia_core::math::Vec3;
    use regia_physics::destruction::{Destruction, DestructionPlugin};
    use regia_physics::{PhysicsPlugin, PhysicsSettings};

    let mut app = App::new();
    app.insert_resource(TimeUpdateStrategy::ManualDuration(
        std::time::Duration::from_secs_f64(1.0 / 60.0),
    ));
    let mut fixed = FixedTime::default();
    fixed.hz = 60.0;
    app.insert_resource(fixed);
    app.insert_resource(PhysicsSettings {
        gravity: Vec3::ZERO,
        ..PhysicsSettings::default()
    });
    app.add_plugins((PhysicsPlugin, DestructionPlugin::default()));
    let id = app
        .world_mut()
        .resource_scope(|world, mut d: regia_ecs::prelude::Mut<Destruction>| {
            let mut physics = world.resource_mut::<PhysicsWorld>();
            d.define(
                &mut physics,
                Arc::new(hanging(true)),
                DestructiblePose::at([0.0, 30.0, 0.0]),
                3,
            )
            .expect("define")
        });
    // Let the physics step store zero gravity before the section exists.
    for _ in 0..3 {
        app.update();
    }
    app.world_mut()
        .resource_mut::<Destruction>()
        .world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 2,
                impulse: [0.0, 2.0, 0.0],
            },
        })
        .expect("cut");
    for _ in 0..8 {
        app.update();
    }
    let early = {
        let destruction = app.world().resource::<Destruction>();
        assert_eq!(destruction.physics.stats().splits_total, 0);
        let section = destruction
            .physics
            .sections()
            .next()
            .expect("the cut section is still one body");
        section.bond_damage.clone()
    };
    for _ in 0..22 {
        app.update();
    }
    let destruction = app.world().resource::<Destruction>();
    assert_eq!(
        destruction.physics.stats().splits_total,
        0,
        "zero-g free fall is not an impact"
    );
    let section = destruction
        .physics
        .sections()
        .next()
        .expect("the cut section is still one body");
    // The cut face starts overlapping the support, so the contact solver can apply one
    // separation impulse. Earth gravity wrongly subtracted every tick used to add tens of
    // N·s per bond and split the section; that damage must not keep growing.
    for (early_d, late_d) in early.iter().zip(&section.bond_damage) {
        assert!(
            (*late_d - *early_d).abs() < 1.0,
            "bond damage grew without a new hit: early {early:?} late {:?}",
            section.bond_damage
        );
    }
}

/// Corr-physics audit: a chunk whose hull rapier cannot build (collinear points pass `validate`)
/// was skipped when its section's colliders were built, so `colliders` ran short of `chunks`:
/// every later chunk got the wrong collider and the last one indexed past the end (a panic in
/// `debris_collider`, which the plugin calls for every spawned chunk).
#[test]
fn a_section_with_an_unbuildable_hull_keeps_its_colliders_aligned() {
    let mut asset = hanging(true);
    asset.chunks[1].hulls = vec![vec![
        [0.0, 0.0, 0.0],
        [0.1, 0.0, 0.0],
        [0.2, 0.0, 0.0],
        [0.3, 0.0, 0.0],
    ]];
    asset.validate().expect("collinear hulls still validate");
    let mut s = scene(asset, [0.0, 10.0, 0.0], Authority::Server);
    s.tick();
    s.cut();
    s.world.step(DT);
    let report = s.bridge.sync(&mut s.physics, &mut s.world, DT);
    assert_eq!(report.sections_spawned, vec![(s.id, 0)], "{report:?}");
    let sec = s.bridge.sections().next().expect("section").clone();
    assert_eq!(sec.chunks, vec![0, 1, 2]);
    assert_eq!(sec.colliders.len(), sec.chunks.len(), "one slot per chunk");
    assert_eq!(sec.colliders[1], None, "the collinear hull has no collider");
    assert_eq!(s.bridge.debris_collider(s.id, 1), None);
    let inst = s.world.instance(s.id).expect("inst");
    for (slot, chunk) in [(0usize, 0u32), (2, 2)] {
        let handle = s
            .bridge
            .debris_collider(s.id, chunk)
            .expect("a built hull has a collider");
        assert_eq!(sec.colliders[slot], Some(handle));
        let collider = s.physics.colliders.get(handle).expect("collider");
        assert_eq!(collider.parent(), Some(sec.body));
        let offset = collider.position_wrt_parent().expect("attached").translation;
        let want = inst.asset().chunks[chunk as usize].centroid;
        assert!(
            (offset.x - want[0]).abs() < 1e-5
                && (offset.y - want[1]).abs() < 1e-5
                && (offset.z - want[2]).abs() < 1e-5,
            "chunk {chunk} got the collider at {offset:?}, its centroid is {want:?}"
        );
    }
    // Releasing the section must not trip over the empty slot either.
    for _ in 0..1200 {
        s.tick();
    }
}

/// Corr-physics audit: the section impact test read the support a resting section gets each
/// step (`-g dt`) as a jolt, so a landed section took its weight times dt as bond damage every
/// tick and split by itself, however gently it landed.
#[test]
fn a_landed_section_does_not_split_by_resting() {
    let mut asset = hanging(true);
    asset.bonds[0].break_impulse = 2000.0;
    asset.bonds[1].break_impulse = 2000.0;
    asset.validate().expect("valid");
    // The column's bottom face 5 cm above the ground: a gentle landing (about 1 m/s).
    let mut s = scene(asset, [0.0, 0.05, 0.0], Authority::Server);
    let mut budget = s.world.budget().clone();
    budget.sleep_despawn_secs = 0.0;
    budget.max_lifetime_secs = 0.0;
    s.world.set_budget(budget);
    s.tick();
    s.cut();
    let mut damage_at_120 = None;
    for tick in 0..600 {
        s.tick();
        if tick == 120 {
            damage_at_120 = s.bridge.sections().next().map(|sec| sec.bond_damage.clone());
        }
    }
    assert_eq!(s.bridge.stats().splits_total, 0, "the resting section split");
    let sec = s.bridge.sections().next().expect("the section is still one body");
    let damage_at_120 = damage_at_120.expect("a section at tick 120");
    // Contact jitter while it settles may add a little; resting used to add about 41 N s per
    // bond per tick (about 20 000 N s over these 480 ticks).
    for (now, then) in sec.bond_damage.iter().zip(&damage_at_120) {
        assert!(
            now - then < 20.0,
            "resting kept adding bond damage: {then} -> {now}"
        );
    }
}
