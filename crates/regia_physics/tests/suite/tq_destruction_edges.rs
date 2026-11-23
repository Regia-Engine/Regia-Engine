// SPDX-License-Identifier: MPL-2.0
//! Test-quality lane: validation, migration and API edge cases of destruction level 2 that
//! `destruction.rs` and the unit tests do not reach.

use std::sync::Arc;

use regia_physics::destruction::{
    Authority, BoxGrid, BreakEvent, BreakKind, Damage, DamageKind, DestructibleAsset, DestructibleError,
    DestructibleId, DestructiblePose, DestructionError, DestructionMaterial, DestructionWorld, GridSupport,
    MAX_DESTRUCTIBLE_CHUNKS, MAX_HULL_POINTS, MAX_HULLS_PER_CHUNK, destructible_migration_sources,
    migrate_destructible,
};

fn grid(counts: [u32; 3]) -> BoxGrid {
    BoxGrid {
        name: "g".into(),
        counts,
        cell: [1.0; 3],
        density: 1.0,
        strength_per_area: 1.0,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    }
}

fn wall() -> DestructibleAsset {
    DestructibleAsset::box_grid(&grid([4, 3, 1])).expect("wall")
}

fn invalid(asset: &DestructibleAsset) -> String {
    match asset.validate() {
        Err(DestructibleError::Invalid { reason, .. }) => reason,
        other => panic!("expected an Invalid error, got {other:?}"),
    }
}

#[test]
fn every_validation_rule_names_what_it_found() {
    let mut a = wall();
    a.name.clear();
    assert!(invalid(&a).contains("name"), "empty name");

    let mut a = wall();
    a.chunks[0].bounds_min[1] = a.chunks[0].bounds_max[1] + 1.0;
    assert!(invalid(&a).contains("bounds"), "inverted bounds");

    let mut a = wall();
    a.chunks[0].centroid[2] = f32::INFINITY;
    assert!(invalid(&a).contains("non-finite"), "infinite centroid");

    let mut a = wall();
    a.chunks[0].hulls = vec![a.chunks[0].hulls[0].clone(); MAX_HULLS_PER_CHUNK + 1];
    assert!(invalid(&a).contains("hulls"), "too many hulls");

    let mut a = wall();
    a.chunks[0].hulls[0] = vec![[0.5; 3]; MAX_HULL_POINTS + 1];
    assert!(invalid(&a).contains("points"), "too many hull points");

    let mut a = wall();
    a.chunks[0].hulls[0][2][1] = f32::NAN;
    assert!(invalid(&a).contains("non-finite"), "nan hull point");

    let mut a = wall();
    a.chunks[0].mass = -1.0;
    assert!(invalid(&a).contains("mass"), "negative mass");

    let mut a = wall();
    a.bonds[0].break_impulse = f32::NAN;
    assert!(invalid(&a).contains("break_impulse"), "nan break impulse");

    let mut a = wall();
    a.bonds[0].area = f32::NAN;
    assert!(invalid(&a).contains("non-finite or negative"), "nan area");

    let mut a = wall();
    a.bonds[0].area = -0.5;
    assert!(invalid(&a).contains("non-finite or negative"), "negative area");

    let mut a = wall();
    a.bonds[0].normal[0] = f32::INFINITY;
    assert!(invalid(&a).contains("non-finite or negative"), "infinite normal");

    let mut a = wall();
    let n = a.chunks.len() as u32;
    a.bonds[0].b = n; // one past the last chunk
    assert!(invalid(&a).contains("bond 0"), "bond past the chunk count");

    let mut a = wall();
    a.bonds[0].b = a.bonds[0].a; // a == b
    assert!(invalid(&a).contains("a < b"), "self bond");

    let mut a = wall();
    a.material.min_impulse = f32::NAN;
    assert!(invalid(&a).contains("min_impulse"), "nan material");

    let mut a = wall();
    a.chunks[1].lifetime_secs = Some(f32::NAN);
    assert!(invalid(&a).contains("lifetime_secs"), "nan lifetime");

    let mut a = wall();
    a.chunks[1].edge_heat = Some(f32::NAN);
    assert!(invalid(&a).contains("edge_heat"), "nan edge heat");
}

/// The audit's coplanar quad is a collider on parry 0.31. `SharedShape::convex_hull` builds a
/// double-sided flat mesh (`try_get_initial_mesh` dimension 2) and returns `Some`, and that shape
/// penetrates a box (`coplanar_quad_builds_a_shared_shape_and_contacts_a_box`). `validate` and
/// `define` therefore accept the quad, alone or beside a solid hull. A tetrahedron stays valid.
#[test]
fn a_coplanar_chunk_hull_is_a_collider() {
    let mut flat = wall();
    flat.chunks[1].hulls = vec![vec![
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [1.0, 1.0, 0.0],
    ]];
    flat.validate().expect("a coplanar quad is a flat convex");
    DestructionWorld::new(Authority::Server)
        .define(Arc::new(flat), DestructiblePose::default(), 1)
        .expect("a flat hull is placed");

    let mut mixed = wall();
    let solid = mixed.chunks[1].hulls[0].clone();
    mixed.chunks[1].hulls = vec![
        solid,
        vec![[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [1.0, 1.0, 0.0]],
    ];
    mixed
        .validate()
        .expect("a compound with one flat hull still builds");

    let mut tetra = wall();
    tetra.chunks[1].hulls = vec![vec![
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.0, 1.0, 0.0],
        [0.0, 0.0, 1.0],
    ]];
    tetra.validate().expect("a tetrahedron is a convex volume");
}

#[test]
fn the_chunk_cap_is_inclusive_and_checked_before_any_allocation() {
    // 4096 cells is allowed; 4097 is refused; a count product that overflows u32 is refused too.
    let ok = DestructibleAsset::box_grid(&grid([16, 16, 16])).expect("4096 cells");
    assert_eq!(ok.chunks.len(), MAX_DESTRUCTIBLE_CHUNKS);
    for counts in [[16, 16, 17], [u32::MAX, u32::MAX, u32::MAX], [65_536, 65_536, 1]] {
        let err = DestructibleAsset::box_grid(&grid(counts)).expect_err("too many cells");
        assert_eq!(err.code(), "destruction_invalid_asset", "{counts:?}");
    }
    let mut over = ok;
    over.chunks.push(over.chunks[0].clone());
    over.chunks.last_mut().expect("chunk").name = "extra".into();
    assert!(invalid(&over).contains("chunks"));
}

#[test]
fn box_grid_rejects_non_finite_and_non_positive_parameters() {
    for mutate in [
        (|g: &mut BoxGrid| g.cell[1] = f32::NAN) as fn(&mut BoxGrid),
        |g| g.cell[0] = f32::INFINITY,
        |g| g.cell[2] = 0.0,
        |g| g.cell[0] = -1.0,
        |g| g.density = 0.0,
        |g| g.density = f32::NAN,
        |g| g.strength_per_area = -2.0,
        |g| g.strength_per_area = f32::INFINITY,
    ] {
        let mut g = grid([2, 2, 1]);
        mutate(&mut g);
        assert_eq!(
            DestructibleAsset::box_grid(&g).expect_err("bad grid").code(),
            "destruction_invalid_asset"
        );
    }
}

#[test]
fn box_grid_bonds_join_face_neighbours_only() {
    // 3x2x2: face-neighbour bonds = (nx-1)*ny*nz + nx*(ny-1)*nz + nx*ny*(nz-1).
    let a = DestructibleAsset::box_grid(&grid([3, 2, 2])).expect("grid");
    assert_eq!(a.chunks.len(), 12);
    assert_eq!(a.bonds.len(), 20 /* 8 along x + 6 along y + 6 along z */);
    // Every bond is ordered and unique (the asset validates), and the single-cell grid has none.
    a.validate().expect("valid");
    let one = DestructibleAsset::box_grid(&grid([1, 1, 1])).expect("one cell");
    assert!(one.bonds.is_empty());
    assert_eq!(one.support_chunks(), vec![0], "the lone bottom cell is a support");
}

#[test]
fn migration_reads_the_version_field_strictly() {
    assert_eq!(
        destructible_migration_sources(),
        vec![1],
        "v1 migrates to v2 (ADR-2734)"
    );
    let text = wall().to_ron().expect("ron");
    assert_eq!(migrate_destructible(&text).expect("current"), text);
    for (src, code) in [
        ("", "destruction_parse"),
        ("(", "destruction_parse"),
        ("[1, 2]", "destruction_parse"),
        ("(name: \"x\")", "destruction_parse"),
        ("(format_version: \"1\")", "destruction_parse"),
        ("(format_version: -1)", "destruction_parse"),
        ("(format_version: 4294967296)", "destruction_parse"),
        ("(format_version: 0)", "destruction_version"),
        ("(format_version: 3)", "destruction_version"),
        ("(format_version: 4294967295)", "destruction_version"),
    ] {
        let err = migrate_destructible(src).expect_err(src);
        assert_eq!(err.code(), code, "{src:?}");
        assert_eq!(
            DestructibleAsset::from_ron(src).expect_err(src).code(),
            code,
            "from_ron {src:?}"
        );
    }
}

#[test]
fn from_ron_validates_after_parsing() {
    let mut a = wall();
    a.chunks[0].mass = 0.0;
    let text = a.to_ron().expect("ron");
    assert_eq!(
        DestructibleAsset::from_ron(&text).expect_err("mass").code(),
        "destruction_invalid_asset"
    );
    // A field the shape does not have (unknown keys are ignored by serde) still loads; a missing
    // required field is a parse error.
    let missing = wall().to_ron().expect("ron").replace("mass:", "weight:");
    assert_eq!(
        DestructibleAsset::from_ron(&missing).expect_err("missing").code(),
        "destruction_parse"
    );
}

fn world_with_wall() -> (DestructionWorld, DestructibleId) {
    let mut w = DestructionWorld::new(Authority::Server);
    let id = w
        .define(Arc::new(wall()), DestructiblePose::at([0.0; 3]), 7)
        .expect("define");
    (w, id)
}

#[test]
fn damage_parameters_are_checked_one_by_one() {
    let (mut w, id) = world_with_wall();
    let explosion = |center: [f32; 3], radius: f32, impulse: f32| Damage {
        target: None,
        kind: DamageKind::Explosion {
            center,
            radius,
            impulse,
        },
    };
    let projectile = |origin, direction, impulse, radius, max_distance| Damage {
        target: None,
        kind: DamageKind::Projectile {
            origin,
            direction,
            impulse,
            radius,
            max_distance,
        },
    };
    let bad = [
        explosion([f32::NAN, 0.0, 0.0], 1.0, 1.0),
        explosion([0.0; 3], f32::INFINITY, 1.0),
        explosion([0.0; 3], 1.0, -1.0),
        explosion([0.0; 3], 1.0, f32::NAN),
        projectile([0.0; 3], [0.0; 3], 1.0, 0.0, 10.0),
        projectile([0.0; 3], [f32::NAN, 0.0, 1.0], 1.0, 0.0, 10.0),
        projectile([f32::INFINITY, 0.0, 0.0], [0.0, 0.0, 1.0], 1.0, 0.0, 10.0),
        projectile([0.0; 3], [0.0, 0.0, 1.0], -1.0, 0.0, 10.0),
        projectile([0.0; 3], [0.0, 0.0, 1.0], 1.0, -0.1, 10.0),
        projectile([0.0; 3], [0.0, 0.0, 1.0], 1.0, 0.0, 0.0),
        projectile([0.0; 3], [0.0, 0.0, 1.0], 1.0, 0.0, f32::NAN),
        Damage {
            target: None,
            kind: DamageKind::Impulse {
                chunk: 0,
                impulse: [1.0; 3],
            },
        },
        Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 0,
                impulse: [0.0, f32::NAN, 0.0],
            },
        },
    ];
    for d in bad {
        let err = w.damage(d.clone()).expect_err(&format!("{d:?}"));
        assert_eq!(err.code(), "destruction_invalid_damage", "{d:?}");
    }
    assert_eq!(w.queued_damage(), 0, "nothing bad was queued");
    // Zero impulse and zero spread radius are allowed.
    assert!(w.damage(explosion([0.0; 3], 1.0, 0.0)).is_ok());
    assert!(
        w.damage(projectile([0.0; 3], [0.0, 0.0, 1.0], 0.0, 0.0, 1.0))
            .is_ok()
    );
    assert_eq!(w.queued_damage(), 2);
}

#[test]
fn queue_numbers_count_up_and_a_step_drains_the_queue() {
    let (mut w, id) = world_with_wall();
    let hit = Damage {
        target: Some(id),
        kind: DamageKind::Impulse {
            chunk: 5,
            impulse: [0.0, 1.0, 0.0],
        },
    };
    assert_eq!(w.damage(hit.clone()).expect("n0"), 0);
    assert_eq!(w.damage(hit.clone()).expect("n1"), 1);
    assert_eq!(w.queued_damage(), 2);
    let report = w.step(1.0 / 60.0);
    assert_eq!(report.hits, 2);
    assert_eq!(w.queued_damage(), 0);
    assert_eq!(
        w.damage(hit).expect("n2"),
        2,
        "numbers keep counting across steps"
    );
}

#[test]
fn bad_step_durations_neither_heal_nor_poison() {
    let mut a = wall();
    a.material.decay_per_second = 1000.0;
    let hit = |id| Damage {
        target: Some(id),
        kind: DamageKind::Impulse {
            chunk: 0,
            impulse: [0.01, 0.0, 0.0],
        },
    };
    let mut w = DestructionWorld::new(Authority::Server);
    let id = w
        .define(Arc::new(a), DestructiblePose::default(), 3)
        .expect("define");
    w.damage(hit(id)).expect("queued");
    w.step(1.0 / 60.0);
    let before: Vec<f32> = w.instance(id).expect("inst").bond_damage().to_vec();
    assert!(before.iter().any(|d| *d > 0.0), "the hit damaged a bond");
    for dt in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -5.0, 0.0] {
        w.step(dt);
        let after = w.instance(id).expect("inst").bond_damage();
        assert_eq!(after, before.as_slice(), "dt {dt}");
        assert!(after.iter().all(|d| d.is_finite()), "dt {dt}");
    }
    // A real step heals (decay 1000/s over one second is far more than the damage).
    w.step(1.0);
    assert!(
        w.instance(id)
            .expect("inst")
            .bond_damage()
            .iter()
            .all(|d| *d == 0.0)
    );
}

#[test]
fn server_and_replica_refuse_each_others_calls() {
    let (mut server, id) = world_with_wall();
    assert_eq!(
        server.apply_events(&[]).expect_err("server").code(),
        "destruction_not_replica"
    );
    let mut replica = DestructionWorld::new(Authority::Replica);
    let rid = replica
        .define(Arc::new(wall()), DestructiblePose::at([0.0; 3]), 7)
        .expect("define");
    assert_eq!(rid, id, "same definition order, same ids");
    let hit = Damage {
        target: Some(id),
        kind: DamageKind::Impulse {
            chunk: 0,
            impulse: [1.0; 3],
        },
    };
    assert_eq!(
        replica.damage(hit).expect_err("replica").code(),
        "destruction_replica"
    );
    // Replica with no events: apply nothing, step only advances the tick.
    assert!(replica.apply_events(&[]).expect("empty").events.is_empty());
    let t = replica.tick();
    assert!(replica.step(0.1).events.is_empty());
    assert_eq!(replica.tick(), t + 1);
}

fn bond_event(seq: u64, id: DestructibleId, bond: u32) -> BreakEvent {
    BreakEvent {
        seq,
        tick: 1,
        destructible: id,
        kind: BreakKind::BondBroken {
            bond,
            a: 0,
            b: 1,
            position: [0.0; 3],
        },
        seed: 1,
    }
}

#[test]
fn replica_rejects_bad_events_and_keeps_the_ones_before_them() {
    let mut replica = DestructionWorld::new(Authority::Replica);
    let id = replica
        .define(Arc::new(wall()), DestructiblePose::default(), 7)
        .expect("define");
    // Out of order, unknown instance, unknown bond: each is a BadEvent.
    for (event, why) in [
        (bond_event(5, id, 0), "seq"),
        (bond_event(0, DestructibleId(77), 0), "instance"),
        (bond_event(0, id, 9999), "bond"),
    ] {
        let err = replica.apply_events(&[event]).expect_err(why);
        assert_eq!(err.code(), "destruction_bad_event", "{why}");
    }
    assert_eq!(replica.event_count(), 0, "failed events changed nothing");
    // One good event, then a repeat of the same bond: the first stays applied.
    let err = replica
        .apply_events(&[bond_event(0, id, 0), bond_event(1, id, 0)])
        .expect_err("already broken");
    assert!(
        matches!(err, DestructionError::BadEvent { seq: 1, .. }),
        "{err:?}"
    );
    assert_eq!(replica.event_count(), 1);
    assert!(replica.instance(id).expect("inst").bond_broken()[0]);
    // The next expected number is 1 again, and a fresh bond is accepted.
    replica.apply_events(&[bond_event(1, id, 1)]).expect("seq 1");
    assert_eq!(replica.event_count(), 2);
    // A detached-chunk event for a chunk out of range or already detached is refused.
    let detach = |seq, chunk| BreakEvent {
        seq,
        tick: 2,
        destructible: id,
        kind: BreakKind::ChunkDetached {
            chunk,
            cause: regia_physics::destruction::DetachCause::Damage,
            position: [0.0; 3],
            linvel: [0.0; 3],
            angvel: [0.0; 3],
        },
        seed: 2,
    };
    assert!(replica.apply_events(&[detach(2, 9999)]).is_err());
    replica.apply_events(&[detach(2, 3)]).expect("detach");
    assert!(replica.apply_events(&[detach(3, 3)]).is_err(), "already detached");
    assert_eq!(replica.pending_spawns(), 1);
    assert_eq!(replica.take_spawns(0).len(), 1);
    assert_eq!(replica.pending_spawns(), 0);
}

#[test]
fn removing_an_instance_drops_its_queued_spawns_and_unknown_ids_are_safe() {
    let (mut w, id) = world_with_wall();
    assert!(!w.remove(DestructibleId(99)));
    assert_eq!(
        w.set_pose(DestructibleId(99), DestructiblePose::default())
            .expect_err("unknown")
            .code(),
        "destruction_unknown_id"
    );
    assert_eq!(
        w.set_pose(
            id,
            DestructiblePose {
                translation: [f32::NAN, 0.0, 0.0],
                rotation: [0.0, 0.0, 0.0, 1.0],
            }
        )
        .expect_err("nan")
        .code(),
        "destruction_invalid_pose"
    );
    // Blow it up so chunks detach, then remove: the spawn queue empties.
    w.damage(Damage {
        target: Some(id),
        kind: DamageKind::Explosion {
            center: [2.0, 1.0, 0.0],
            radius: 20.0,
            impulse: 1.0e9,
        },
    })
    .expect("queued");
    w.step(1.0 / 60.0);
    assert!(w.pending_spawns() > 0, "the blast detached chunks");
    assert!(w.remove(id));
    assert_eq!(w.pending_spawns(), 0);
    assert!(w.instance(id).is_none());
    assert!(!w.remove(id), "removing twice reports false");
}

#[test]
fn two_worlds_given_the_same_script_agree_bit_for_bit() {
    fn run() -> (u64, u64, u64) {
        let (mut w, id) = world_with_wall();
        for tick in 0..30u32 {
            w.damage(Damage {
                target: Some(id),
                kind: DamageKind::Projectile {
                    origin: [tick as f32 * 0.13, 1.5, -5.0],
                    direction: [0.0, 0.0, 1.0],
                    impulse: 40.0,
                    radius: 0.8,
                    max_distance: 20.0,
                },
            })
            .expect("queued");
            w.step(1.0 / 60.0);
        }
        (w.structure_hash(), w.state_hash(), w.event_count())
    }
    assert_eq!(run(), run());
}
