// SPDX-License-Identifier: MPL-2.0
//! Structural load (ADR-2734): gravity relaxed through the bonds to the supports, overloaded bonds
//! fail, capped cascades break the same bonds in the same order, the server decides and a replica
//! replays. Fixed steps, no sockets, no rapier.

use std::sync::Arc;

use regia_physics::destruction::{
    Authority, BoxGrid, BreakEvent, BreakKind, Damage, DamageKind, DestructibleAsset, DestructibleBond,
    DestructibleChunk, DestructibleId, DestructiblePose, DestructionBudget, DestructionMaterial,
    DestructionWorld, GridSupport, LoadSettings,
};

const DT: f32 = 1.0 / 60.0;
const G: f32 = 9.81;

/// Unit boxes at grid cells `(x, y)` (centres at `x + 0.5`, `y + 0.5`), bonded to their face
/// neighbours; `supports` are cell indices.
fn cells(
    name: &str,
    at: &[(i32, i32)],
    supports: &[usize],
    density: f32,
    strength: f32,
) -> DestructibleAsset {
    let hull: Vec<[f32; 3]> = (0..8u32)
        .map(|c| {
            [
                if c & 1 == 0 { -0.5 } else { 0.5 },
                if c & 2 == 0 { -0.5 } else { 0.5 },
                if c & 4 == 0 { -0.5 } else { 0.5 },
            ]
        })
        .collect();
    let chunks = at
        .iter()
        .enumerate()
        .map(|(i, &(x, y))| {
            let c = [x as f32 + 0.5, y as f32 + 0.5, 0.0];
            DestructibleChunk {
                name: format!("{name}_{i}"),
                parent: None,
                centroid: c,
                mass: density,
                inertia: None,
                hulls: vec![hull.clone()],
                bounds_min: [c[0] - 0.5, c[1] - 0.5, -0.5],
                bounds_max: [c[0] + 0.5, c[1] + 0.5, 0.5],
                support: supports.contains(&i),
                lods: Vec::new(),
                fx_tag: None,
                small: false,
                lifetime_secs: None,
                edge_heat: None,
            }
        })
        .collect();
    let mut bonds = Vec::new();
    for (i, &(xi, yi)) in at.iter().enumerate() {
        for (j, &(xj, yj)) in at.iter().enumerate().skip(i + 1) {
            if (xi - xj).abs() + (yi - yj).abs() != 1 {
                continue;
            }
            let normal = [(xj - xi) as f32, (yj - yi) as f32, 0.0];
            bonds.push(DestructibleBond {
                a: i as u32,
                b: j as u32,
                area: 1.0,
                anchor: [
                    (xi as f32 + xj as f32) * 0.5 + 0.5,
                    (yi as f32 + yj as f32) * 0.5 + 0.5,
                    0.0,
                ],
                normal,
                break_impulse: strength,
                max_load: None,
            });
        }
    }
    let asset = DestructibleAsset {
        format_version: regia_physics::destruction::DESTRUCTIBLE_FORMAT_VERSION,
        name: name.into(),
        source: None,
        pattern_id: None,
        fracture_seed: None,
        replaces_stage: None,
        meshes: None,
        outer_material: None,
        inner_material: None,
        material: DestructionMaterial::default(),
        chunks,
        bonds,
        load: None,
        falling: None,
    };
    asset.validate().expect("fixture");
    asset
}

fn with_load(mut asset: DestructibleAsset, max_load_per_area: f32) -> DestructibleAsset {
    asset.load = Some(LoadSettings {
        max_load_per_area,
        ..LoadSettings::default()
    });
    asset
}

fn server(asset: DestructibleAsset) -> (DestructionWorld, DestructibleId) {
    let mut world = DestructionWorld::new(Authority::Server);
    let id = world
        .define(Arc::new(asset), DestructiblePose::default(), 7)
        .expect("define");
    (world, id)
}

/// A table: a 5-cell top on two 2-cell legs, the legs' feet held by the ground.
fn table() -> DestructibleAsset {
    // 0 1: left leg, 2 3: right leg, 4..=8: the top.
    cells(
        "table",
        &[
            (0, 0),
            (0, 1),
            (4, 0),
            (4, 1),
            (0, 2),
            (1, 2),
            (2, 2),
            (3, 2),
            (4, 2),
        ],
        &[0, 2],
        100.0,
        100.0,
    )
}

/// Knocks the right leg's upper cell out: an impulse on chunk 3 that breaks both of its bonds.
fn kick_leg(world: &mut DestructionWorld, id: DestructibleId) {
    world
        .damage(Damage {
            target: Some(id),
            kind: DamageKind::Impulse {
                chunk: 3,
                impulse: [1000.0, 0.0, 0.0],
            },
        })
        .expect("kick");
}

/// What a run broke, in order, without ticks or seeds: `(true, bond)` or `(false, chunk)`.
fn order(events: &[BreakEvent]) -> Vec<(bool, u32)> {
    events
        .iter()
        .map(|e| match e.kind {
            BreakKind::BondBroken { bond, .. } => (true, bond),
            BreakKind::ChunkDetached { chunk, .. } => (false, chunk),
        })
        .collect()
}

fn run(world: &mut DestructionWorld, ticks: u32) -> Vec<BreakEvent> {
    let mut events = Vec::new();
    for _ in 0..ticks {
        events.extend(world.step(DT).events);
    }
    events
}

#[test]
fn a_column_carries_the_weight_above_each_bond() {
    let column = DestructibleAsset::box_grid(&BoxGrid {
        name: "column".into(),
        counts: [1, 10, 1],
        cell: [1.0, 1.0, 1.0],
        density: 200.0,
        strength_per_area: 1.0e9,
        support: GridSupport::Bottom,
        material: DestructionMaterial::default(),
    })
    .expect("grid");
    let column = with_load(column, 1.0e9);
    let iterations = column.load.as_ref().expect("load").iterations;
    let (mut world, id) = server(column);
    world.step(DT);
    let inst = world.instance(id).expect("inst");
    let loads = inst.bond_loads().expect("load on");
    let weight = 200.0 * G;
    let asset = inst.asset().clone();
    assert_eq!(asset.bonds.len(), 9);
    for (b, spec) in asset.bonds.iter().enumerate() {
        // Bond (i, i + 1) holds chunks i + 1 ..= 9 up.
        let above = (9 - spec.a) as f32 * weight;
        let got = loads[b].abs();
        assert!(
            (got - above).abs() <= 0.02 * above,
            "bond {b} carries {got} N, the weight above is {above} N"
        );
    }
    let solution = inst.load_solution().expect("solution");
    assert!(solution.iterations <= iterations, "{solution:?}");
    assert_eq!(solution.reactions.len(), 1);
    assert!((solution.reactions[0].1 - 9.0 * weight).abs() <= 0.02 * 9.0 * weight);
    // Nothing is over its limit: no damage, no breaks.
    assert!(run(&mut world, 30).is_empty());
    assert!(
        world
            .instance(id)
            .expect("inst")
            .bond_damage()
            .iter()
            .all(|d| *d == 0.0)
    );
    assert_eq!(world.load_stats().solves, 1, "one solve: the bonds never changed");
}

#[test]
fn an_arch_shares_its_load_between_its_feet() {
    // A stepped arch, symmetric about x = 2.5: feet at cells 0 and 8.
    let arch = cells(
        "arch",
        &[
            (0, 0),
            (0, 1),
            (1, 1),
            (1, 2),
            (2, 2),
            (3, 2),
            (4, 2),
            (4, 1),
            (5, 1),
            (5, 0),
        ],
        &[0, 9],
        150.0,
        1.0e9,
    );
    let (world, id) = server(with_load(arch, 1.0e9));
    let report = world.load_report(id, 3).expect("report");
    assert!(report.enabled);
    assert_eq!(report.supports.len(), 2);
    let (left, right) = (report.supports[0].1, report.supports[1].1);
    let total = 8.0 * 150.0 * G;
    assert!((left + right - total).abs() <= 1e-3 * total, "{report:?}");
    assert!(
        (left - right).abs() <= 0.1 * total * 0.5,
        "left {left} N, right {right} N"
    );
    assert_eq!(report.top.len(), 3);
    assert!(report.top[0].load >= report.top[1].load);
    assert_eq!(report.problem, None);
}

#[test]
fn a_table_without_a_leg_collapses_under_load_alone() {
    // Intact, each leg carries 2.5 cells of top plus its upper cell: 3.5 W < 4 W at the foot.
    let w = 100.0 * G;
    let limit = 4.0 * w;
    let (mut world, id) = server(with_load(table(), limit));
    assert!(run(&mut world, 120).is_empty(), "the intact table stands");
    kick_leg(&mut world, id);
    let events = run(&mut world, 300);
    let inst = world.instance(id).expect("inst");
    // Only the two feet are left.
    assert_eq!(inst.attached_count(), 2, "{:?}", order(&events));
    // The kick broke the right leg's two bonds; everything after it was load.
    let kicked: Vec<_> = events.iter().filter(|e| e.tick == 121).collect();
    assert!(kicked.len() >= 2);
    assert!(
        events.iter().any(|e| e.tick > 121),
        "the collapse came after the kick"
    );

    // The same fixture with load off stays up as a cantilever on its left leg.
    let (mut world, id) = server(table());
    run(&mut world, 120);
    kick_leg(&mut world, id);
    let events = run(&mut world, 300);
    let inst = world.instance(id).expect("inst");
    assert_eq!(
        inst.attached_count(),
        8,
        "only the kicked cell fell: {:?}",
        order(&events)
    );
}

#[test]
fn a_capped_cascade_breaks_the_same_bonds_in_the_same_order() {
    let w = 100.0 * G;
    let collapse = |cap: u32| {
        let (mut world, id) = server(with_load(table(), 4.0 * w));
        world.set_budget(DestructionBudget {
            max_breaks_per_tick: cap,
            ..DestructionBudget::default()
        });
        run(&mut world, 10);
        kick_leg(&mut world, id);
        let events = run(&mut world, 400);
        let deferred = world.counters().deferred_breaks;
        (
            order(&events),
            deferred,
            world.instance(id).expect("inst").bond_broken().to_vec(),
        )
    };
    let (free, free_deferred, free_bonds) = collapse(0);
    let (capped, capped_deferred, capped_bonds) = collapse(1);
    assert_eq!(free_deferred, 0);
    assert!(capped_deferred > 0, "the cap held breaks back");
    assert!(free.len() > 4, "{free:?}");
    assert_eq!(capped, free);
    assert_eq!(capped_bonds, free_bonds);
}

#[test]
fn two_runs_and_a_replica_agree() {
    let w = 100.0 * G;
    let play = || {
        let (mut world, id) = server(with_load(table(), 4.0 * w));
        world.set_budget(DestructionBudget {
            max_breaks_per_tick: 2,
            ..DestructionBudget::default()
        });
        let mut per_tick = Vec::new();
        for t in 0..300 {
            if t == 20 {
                kick_leg(&mut world, id);
            }
            per_tick.push(world.step(DT).events);
        }
        (world, id, per_tick)
    };
    let (a, id, events) = play();
    let (b, _, _) = play();
    assert_eq!(a.state_hash(), b.state_hash());
    assert_eq!(a.structure_hash(), b.structure_hash());

    // A replica fed the server's events tick by tick: the same bonds, the same hash. Its own
    // step never decides a load break.
    let mut replica = DestructionWorld::new(Authority::Replica);
    let rid = replica
        .define(
            Arc::new(with_load(table(), 4.0 * w)),
            DestructiblePose::default(),
            7,
        )
        .expect("define");
    assert_eq!(rid, id);
    for tick in &events {
        replica.apply_events(tick).expect("apply");
        let report = replica.step(DT);
        assert_eq!(report.events.len(), tick.len());
    }
    assert_eq!(replica.structure_hash(), a.structure_hash());
    assert_eq!(
        replica.instance(rid).expect("inst").bond_broken(),
        a.instance(id).expect("inst").bond_broken()
    );
    // A replica refuses to be hit, so nothing it does can start a break.
    assert_eq!(
        replica
            .damage(Damage {
                target: Some(rid),
                kind: DamageKind::Impulse {
                    chunk: 3,
                    impulse: [1.0, 0.0, 0.0],
                },
            })
            .expect_err("replica")
            .code(),
        "destruction_replica"
    );
    assert_eq!(
        replica.load_stats().solves,
        0,
        "a replica never solves in its step"
    );
    // The on-demand report works on the replica's replayed bonds.
    let report = replica.load_report(rid, 4).expect("report");
    assert!(report.enabled);
}

#[test]
fn load_off_leaves_the_bonds_and_hashes_alone() {
    // The table without load settings, with and without a load-on twin in the same world: the
    // load-off instance's events are the same as in a world of its own.
    let alone = {
        let (mut world, id) = server(table());
        kick_leg(&mut world, id);
        order(&run(&mut world, 60))
    };
    let mut world = DestructionWorld::new(Authority::Server);
    let off = world
        .define(Arc::new(table()), DestructiblePose::default(), 7)
        .expect("off");
    let _on = world
        .define(
            Arc::new(with_load(table(), 1.0e9)),
            DestructiblePose::at([20.0, 0.0, 0.0]),
            8,
        )
        .expect("on");
    kick_leg(&mut world, off);
    let mixed: Vec<_> = run(&mut world, 60)
        .into_iter()
        .filter(|e| e.destructible == off)
        .collect();
    assert_eq!(order(&mixed), alone);
    assert!(world.instance(off).expect("off").bond_loads().is_none());
    // A load-off asset still gets an on-demand report, flagged as not enabled.
    let report = world.load_report(off, 2).expect("report");
    assert!(!report.enabled);
}

#[test]
fn edges_are_refused_or_reported_with_codes() {
    // A weight too large for f32 is refused when the asset loads.
    let mut heavy = with_load(table(), 1.0);
    heavy.load.as_mut().expect("load").gravity = f32::MAX;
    assert_eq!(
        heavy.validate().expect_err("weight").code(),
        "destruction_invalid_asset"
    );
    // Zero mass is refused by the format.
    let mut light = table();
    light.chunks[4].mass = 0.0;
    assert_eq!(
        light.validate().expect_err("mass").code(),
        "destruction_invalid_asset"
    );
    // Cycles are fine: a closed ring of cells on one support solves and conserves weight.
    let ring = cells(
        "ring",
        &[(0, 0), (1, 0), (2, 0), (2, 1), (2, 2), (1, 2), (0, 2), (0, 1)],
        &[1],
        10.0,
        1.0e9,
    );
    let (world, id) = server(with_load(ring, 1.0e9));
    let report = world.load_report(id, 16).expect("ring");
    assert_eq!(report.problem, None);
    assert!((report.supports[0].1 - 7.0 * 10.0 * G).abs() < 1e-2, "{report:?}");
    // Weights near the f32 limit make the solve non-finite: reported, no load, no breaks.
    let mut huge = cells("huge", &[(0, 0), (0, 1), (0, 2)], &[0], 3.0e38, 1.0);
    huge.load = Some(LoadSettings {
        gravity: 1.0,
        max_load_per_area: 1.0,
        ..LoadSettings::default()
    });
    let (mut world, id) = server(huge);
    assert!(run(&mut world, 5).is_empty());
    assert_eq!(world.load_stats().non_finite, 1);
    let report = world.load_report(id, 1).expect("report");
    assert_eq!(report.problem, Some("destruction_load_not_finite"));
    // Unknown ids answer their code.
    assert_eq!(
        world.load_report(DestructibleId(99), 1).expect_err("id").code(),
        "destruction_unknown_id"
    );
}
