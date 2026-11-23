// SPDX-License-Identifier: MPL-2.0
//! Seeded property tests of the destruction break model (ADR-1597): for random grids, supports and
//! hits, after every tick (1) every attached chunk of an object with supports has a path of intact
//! bonds to a support, (2) every detached chunk has no intact bond, (3) two runs agree bit for bit,
//! and (4) a replica replaying the events matches the structure hash.

use std::sync::Arc;

use regia_physics::destruction::{
    Authority, BoxGrid, ChunkState, Damage, DamageKind, DestructibleAsset, DestructiblePose,
    DestructionBudget, DestructionMaterial, DestructionWorld, GridSupport,
};

/// SplitMix64: a tiny seeded generator (no dependency).
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    /// Uniform in `[lo, hi)`.
    fn range(&mut self, lo: f32, hi: f32) -> f32 {
        lo + (hi - lo) * ((self.next() >> 40) as f32 / 16_777_216.0)
    }
}

fn random_asset(rng: &mut Rng) -> DestructibleAsset {
    let counts = [
        1 + rng.below(6) as u32,
        1 + rng.below(6) as u32,
        1 + rng.below(3) as u32,
    ];
    let total = counts[0] * counts[1] * counts[2];
    let support = match rng.below(4) {
        0 => GridSupport::None,
        1 => GridSupport::Bottom,
        2 => GridSupport::Ends,
        _ => GridSupport::Explicit((0..total).filter(|_| rng.below(5) == 0).collect()),
    };
    let material = DestructionMaterial {
        min_impulse: rng.range(0.0, 20.0),
        decay_per_second: if rng.below(2) == 0 {
            0.0
        } else {
            rng.range(0.0, 50.0)
        },
        ..DestructionMaterial::default()
    };
    DestructibleAsset::box_grid(&BoxGrid {
        name: "prop".into(),
        counts,
        cell: [rng.range(0.2, 1.0), rng.range(0.2, 1.0), rng.range(0.2, 1.0)],
        density: rng.range(100.0, 3000.0),
        strength_per_area: rng.range(50.0, 2000.0),
        support,
        material,
    })
    .expect("random grid")
}

fn random_hit(rng: &mut Rng, chunks: u32) -> Damage {
    let p = [rng.range(-4.0, 4.0), rng.range(-1.0, 5.0), rng.range(-4.0, 4.0)];
    let kind = match rng.below(3) {
        0 => DamageKind::Explosion {
            center: p,
            radius: rng.range(0.1, 3.0),
            impulse: rng.range(0.0, 3000.0),
        },
        1 => DamageKind::Projectile {
            origin: [p[0], p[1], 10.0],
            direction: [rng.range(-0.2, 0.2), rng.range(-0.2, 0.2), -1.0],
            impulse: rng.range(0.0, 3000.0),
            radius: rng.range(0.0, 1.0),
            max_distance: 30.0,
        },
        _ => DamageKind::Impulse {
            chunk: rng.below(u64::from(chunks)) as u32,
            impulse: [
                rng.range(-500.0, 500.0),
                rng.range(-500.0, 500.0),
                rng.range(-500.0, 500.0),
            ],
        },
    };
    Damage { target: None, kind }
}

/// Every attached chunk of an object with supports reaches a support over intact bonds; every
/// detached chunk has no intact bond.
fn check_invariants(world: &DestructionWorld, seed: u64, tick: u64) {
    for (_, inst) in world.instances() {
        let asset = inst.asset();
        let states = inst.chunk_states();
        let broken = inst.bond_broken();
        for (i, b) in asset.bonds.iter().enumerate() {
            let detached =
                states[b.a as usize] == ChunkState::Detached || states[b.b as usize] == ChunkState::Detached;
            assert!(
                !detached || broken[i],
                "seed {seed} tick {tick}: bond {i} intact on a detached chunk"
            );
        }
        if asset.support_chunks().is_empty() {
            continue;
        }
        let n = asset.chunks.len();
        let mut reached = vec![false; n];
        let mut stack: Vec<usize> = (0..n)
            .filter(|&c| asset.chunks[c].support && states[c] == ChunkState::Attached)
            .collect();
        for &c in &stack {
            reached[c] = true;
        }
        while let Some(c) = stack.pop() {
            for (i, b) in asset.bonds.iter().enumerate() {
                if broken[i] {
                    continue;
                }
                let other = if b.a as usize == c {
                    b.b as usize
                } else if b.b as usize == c {
                    b.a as usize
                } else {
                    continue;
                };
                if !reached[other] && states[other] == ChunkState::Attached {
                    reached[other] = true;
                    stack.push(other);
                }
            }
        }
        for (c, (state, ok)) in states.iter().zip(&reached).enumerate() {
            assert!(
                *state != ChunkState::Attached || *ok,
                "seed {seed} tick {tick}: chunk {c} attached without support"
            );
        }
    }
}

/// Runs one seeded scenario; returns the final state hash, every tick's structure hash and the
/// number of break events.
fn run(seed: u64, check: bool) -> (u64, Vec<u64>, u64) {
    let mut rng = Rng(seed);
    let mut server = DestructionWorld::new(Authority::Server);
    let mut replica = DestructionWorld::new(Authority::Replica);
    server.set_budget(DestructionBudget {
        max_breaks_per_tick: rng.below(8) as u32,
        ..DestructionBudget::default()
    });
    let objects = 1 + rng.below(3);
    let mut chunks = Vec::new();
    for o in 0..objects {
        let asset = Arc::new(random_asset(&mut rng));
        chunks.push(asset.chunks.len() as u32);
        let pose = DestructiblePose::at([o as f32 * 3.0 - 3.0, 0.0, 0.0]);
        server
            .define(asset.clone(), pose, seed ^ o)
            .expect("server define");
        replica.define(asset, pose, seed ^ o).expect("replica define");
    }
    let mut hashes = Vec::new();
    for tick in 1..=40u64 {
        for _ in 0..rng.below(3) {
            let target = rng.below(objects);
            let mut hit = random_hit(&mut rng, chunks[target as usize]);
            if matches!(hit.kind, DamageKind::Impulse { .. }) {
                hit.target = Some(regia_physics::destruction::DestructibleId(target as u32));
            }
            server.damage(hit).expect("valid random hit");
        }
        let report = server.step(1.0 / 30.0);
        replica.step(1.0 / 30.0);
        replica.apply_events(&report.events).expect("replay");
        server.take_spawns(0);
        replica.take_spawns(0);
        assert_eq!(
            server.structure_hash(),
            replica.structure_hash(),
            "seed {seed} tick {tick}"
        );
        if check {
            check_invariants(&server, seed, tick);
        }
        hashes.push(server.structure_hash());
    }
    (server.state_hash(), hashes, server.event_count())
}

#[test]
fn random_scenarios_keep_the_invariants_and_replay_exactly() {
    let mut events = 0u64;
    let mut scenarios_with_breaks = 0u32;
    for seed in 0..150u64 {
        let a = run(seed, true);
        let b = run(seed, false);
        assert_eq!(a, b, "seed {seed}: two runs differ");
        events += a.2;
        scenarios_with_breaks += u32::from(a.2 > 0);
    }
    // The scenarios exercise the model, not just idle objects.
    assert!(
        events > 1000 && scenarios_with_breaks > 75,
        "{events} events, {scenarios_with_breaks} scenarios"
    );
}
