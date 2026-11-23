// SPDX-License-Identifier: MPL-2.0
//! Time the per-system desync fold over entity rows (prints, asserts nothing): `cargo run --release -p
//! regia_detsim --example hash_cost -- [rows=900] [ticks=2000] [raw]`.
//!
//! Each row is a typical lockstep unit (id, position, velocity, facing, health, team, an order enum
//! and an optional target), folded into four systems the way a game's `SystemFold` would: one pass
//! in id order feeding several mixers. `raw` also folds the raw-bits diagnose vector (the cost of
//! `SystemRing::with_raw`). The numbers are evidence for the lane report, never test assertions.

use std::time::Instant;

use regia_detsim::hash::{self, FloatRule, Mix, SystemFold, SystemRing, Systems};
use serde::Serialize;

#[derive(Clone, Serialize)]
enum Order {
    Idle,
    Move { to: [f32; 2] },
    Attack(u32),
}

#[derive(Clone, Serialize)]
struct Unit {
    id: u32,
    pos: [f32; 3],
    vel: [f32; 3],
    facing: f32,
    hp: i32,
    team: u8,
    order: Order,
    target: Option<u32>,
}

struct World {
    units: Vec<Unit>,
    gold: [u32; 8],
    tick: u64,
}

impl SystemFold<4> for World {
    const SYSTEMS: Systems<4> = Systems::new(["movement", "combat", "orders", "economy"]);
    fn fold_systems(&self, mix: &mut [Mix; 4]) {
        for unit in &self.units {
            mix[0].u64(u64::from(unit.id));
            mix[0].fold(&(unit.pos, unit.vel, unit.facing));
            mix[1].u64(u64::from(unit.id));
            mix[1].fold(&(unit.hp, unit.team, unit.target));
            mix[2].u64(u64::from(unit.id));
            mix[2].fold(&unit.order);
        }
        mix[3].fold(&self.gold);
        mix[3].u64(self.tick);
    }
}

fn world(rows: usize) -> World {
    let mut s: u64 = 0x5eed;
    let mut next = move || {
        s = s
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (s >> 40) as f32 * (1.0 / 16_777_216.0)
    };
    let units = (0..rows)
        .map(|i| Unit {
            id: i as u32 + 1,
            pos: [next() * 500.0, 0.0, next() * 500.0],
            vel: [next() - 0.5, 0.0, next() - 0.5],
            facing: next() * 6.0,
            hp: (next() * 100.0) as i32,
            team: (i % 8) as u8,
            order: match i % 3 {
                0 => Order::Idle,
                1 => Order::Move {
                    to: [next() * 500.0, next() * 500.0],
                },
                _ => Order::Attack((i as u32 + 7) % rows.max(1) as u32),
            },
            target: i.is_multiple_of(2).then_some(i as u32 / 2),
        })
        .collect();
    World {
        units,
        gold: [1000; 8],
        tick: 0,
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "host timing tool, not a lockstep path: command-line arguments and wall-clock timing of the fold (ADR-8367, ADR-8621)"
)]
fn main() {
    let mut args = std::env::args().skip(1);
    let rows: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(900);
    let ticks: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(2000).max(1);
    let raw = args.next().is_some_and(|a| a == "raw");
    let mut w = world(rows);
    let mut ring = if raw {
        SystemRing::<4>::with_raw()
    } else {
        SystemRing::<4>::new()
    };
    // Warm up (and fill the ring, so no row allocates).
    for tick in 0..300u64 {
        w.tick = tick;
        ring.record(tick, &w);
    }
    let start = Instant::now();
    for i in 0..ticks {
        let tick = 300 + u64::from(i);
        w.tick = tick;
        w.units[i as usize % rows.max(1)].hp ^= 1;
        ring.record(tick, &w);
    }
    let per_tick = start.elapsed().as_secs_f64() / f64::from(ticks);
    let mut sink = 0u64;
    let fold_start = Instant::now();
    for _ in 0..ticks {
        w.tick += 1;
        sink = sink.wrapping_add(hash::fold_system_hashes(&w, FloatRule::Canonical).hashes.words[0]);
    }
    let fold_only = fold_start.elapsed().as_secs_f64() / f64::from(ticks);
    println!(
        "hash_cost rows {rows} ticks {ticks} raw {raw}: record {:.4} ms per tick ({:.1} ns per row), canonical fold {:.4} ms per tick ({sink:x})",
        per_tick * 1e3,
        per_tick * 1e9 / rows.max(1) as f64,
        fold_only * 1e3,
    );
}
