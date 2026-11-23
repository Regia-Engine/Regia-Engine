// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/state/desync_fold.rs lines 307-350, commit
// d6ed37677, tag regia-absorb-1), ADR-8012: the shape of the per-system injection nudges (a 1/16 m
// position step, a 2^-10 gravity step, a sentinel map key, a counter bump), mark adapt (ADR-8047) onto a
// fixture state. The fixture sim itself is original test code.

//! A small deterministic lockstep sim for the ported desync-hash tests (it stands in for DO27's game
//! state, which does not come with the port). Thirteen systems, the same names the ported tests use.
//! Each system's state feeds only itself in the step, except bodies: a body's velocity (`physics`)
//! moves its position (`units`, `pilots`, `projectiles`, `buildings`), as in a real sim.

use serde::Serialize;

use super::{Mix, SystemFold, Systems};

pub(super) const SYSTEM_COUNT: usize = 13;

pub(super) const SYSTEMS: Systems<SYSTEM_COUNT> = Systems::new([
    "units",
    "physics",
    "weapons",
    "projectiles",
    "economy",
    "production",
    "ai",
    "pilots",
    "buildings",
    "terrain",
    "match",
    "rng",
    "training",
]);

const UNITS: usize = 0;
const PHYSICS: usize = 1;
const WEAPONS: usize = 2;
const PROJECTILES: usize = 3;
const ECONOMY: usize = 4;
const PRODUCTION: usize = 5;
const AI: usize = 6;
const PILOTS: usize = 7;
const BUILDINGS: usize = 8;
const TERRAIN: usize = 9;
const MATCH: usize = 10;
const RNG: usize = 11;
const TRAINING: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub(super) enum Kind {
    Unit,
    Pilot,
    Building,
    Projectile,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(super) enum Order {
    Hold,
    Move { to: [i32; 2] },
    Attack(u32),
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct Body {
    pub(super) entity_id: u32,
    pub(super) kind: Kind,
    pub(super) position: [f32; 3],
    pub(super) velocity: [f32; 3],
    pub(super) yaw: f32,
    pub(super) health: i32,
    pub(super) team: u8,
    pub(super) ammo: u16,
    pub(super) cargo: Option<u32>,
    pub(super) label: String,
}

/// The fixture sim.
#[derive(Clone, Debug)]
pub(super) struct TestSim {
    pub(super) tick: u64,
    pub(super) runtime_entities: Vec<Body>,
    gravity: f32,
    stocks: std::collections::BTreeMap<u32, u16>,
    shot_seq: u32,
    scrap: [i32; 4],
    queue: Vec<(u8, u16)>,
    pods_ready: [u32; 4],
    orders: std::collections::BTreeMap<u32, Order>,
    last_order_tick: [u64; 4],
    may_take: [[bool; 2]; 4],
    owners: std::collections::BTreeMap<u32, u8>,
    vents: Vec<[f32; 2]>,
    scores: [i32; 4],
    rng: u64,
    training_step: u32,
    training_target: [f32; 3],
}

/// Seed of a fixture boot.
pub(super) struct Bootstrap(u64);

pub(super) fn seed_to_bootstrap(seed: u64) -> Bootstrap {
    Bootstrap(seed)
}

impl TestSim {
    /// Twelve bodies of every kind, in id order, placed from the seed.
    pub(super) fn from_startup_bootstrap(boot: Bootstrap) -> Self {
        let seed = boot.0;
        let mut runtime_entities = Vec::new();
        let kinds = [Kind::Unit, Kind::Pilot, Kind::Building, Kind::Projectile];
        for i in 0..12u32 {
            let s = (seed as f32) * 0.25 + i as f32;
            runtime_entities.push(Body {
                entity_id: 10 + i * 3,
                kind: kinds[i as usize % 4],
                position: [s, 2.0 + s * 0.5, -s],
                velocity: if i % 4 == 2 {
                    [0.0; 3]
                } else {
                    [0.5, 1.0 + 0.125 * s, -0.25]
                },
                yaw: 0.0625 * i as f32,
                health: 100 + i as i32,
                team: (i % 2) as u8 + 1,
                ammo: 20 + i as u16,
                cargo: (i % 3 == 0).then_some(i),
                label: format!("body{i}"),
            });
        }
        let mut stocks = std::collections::BTreeMap::new();
        let mut orders = std::collections::BTreeMap::new();
        let mut owners = std::collections::BTreeMap::new();
        for body in &runtime_entities {
            stocks.insert(body.entity_id, 3);
            orders.insert(body.entity_id, Order::Hold);
            if body.kind == Kind::Building {
                owners.insert(body.entity_id, body.team);
            }
        }
        Self {
            tick: 0,
            runtime_entities,
            gravity: 9.8125,
            stocks,
            shot_seq: 1,
            scrap: [0, 40, 40, 0],
            queue: vec![(1, 30), (2, 45)],
            pods_ready: [0; 4],
            orders,
            last_order_tick: [0; 4],
            may_take: [[true, false]; 4],
            owners,
            vents: vec![[4.0, 8.0]],
            scores: [0; 4],
            rng: seed ^ 0x5eed,
            training_step: 0,
            training_target: [0.0; 3],
        }
    }

    /// One step. Plain f32 arithmetic in a fixed order.
    pub(super) fn advance_tick(&mut self) {
        self.tick += 1;
        let dt = 1.0f32 / 60.0;
        for body in &mut self.runtime_entities {
            body.velocity[1] -= self.gravity * dt;
            for axis in 0..3 {
                body.position[axis] += body.velocity[axis] * dt;
            }
            if body.kind == Kind::Projectile {
                body.yaw += 0.015625;
            }
        }
        if self.tick.is_multiple_of(4) {
            for (team, scrap) in self.scrap.iter_mut().enumerate() {
                *scrap += team as i32;
            }
        }
        for job in &mut self.queue {
            job.1 = job.1.saturating_sub(1);
        }
        for (id, order) in &mut self.orders {
            if self.tick.is_multiple_of(5) && id % 2 == 0 {
                *order = Order::Move {
                    to: [self.tick as i32, *id as i32],
                };
            } else if self.tick.is_multiple_of(9) {
                *order = Order::Attack(id.wrapping_add(1));
            }
        }
        if self.tick.is_multiple_of(5) {
            self.last_order_tick[2] = self.tick;
        }
        if self.tick.is_multiple_of(7) {
            self.shot_seq = self.shot_seq.wrapping_add(1);
            self.scores[2] += 1;
        }
        for stock in self.stocks.values_mut() {
            if self.tick.is_multiple_of(6) {
                *stock = stock.saturating_add(1);
            }
        }
        self.rng = self
            .rng
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        if self.training_step > 0 {
            self.training_step += 1;
        }
    }
}

impl SystemFold<SYSTEM_COUNT> for TestSim {
    const SYSTEMS: Systems<SYSTEM_COUNT> = SYSTEMS;

    fn fold_systems(&self, m: &mut [Mix; SYSTEM_COUNT]) {
        for e in &self.runtime_entities {
            let id = u64::from(e.entity_id);
            let own = match e.kind {
                Kind::Unit => UNITS,
                Kind::Pilot => PILOTS,
                Kind::Building => BUILDINGS,
                Kind::Projectile => PROJECTILES,
            };
            let row = &mut m[own];
            row.u64(id);
            row.fold(&e.kind);
            row.fold(&e.position);
            row.f32(e.yaw);
            row.fold(&e.health);
            row.fold(&e.team);
            row.fold(&e.cargo);
            row.fold(&e.label);
            m[PHYSICS].u64(id);
            m[PHYSICS].fold(&e.velocity);
            m[WEAPONS].u64(id);
            m[WEAPONS].fold(&e.ammo);
        }
        m[PHYSICS].f32(self.gravity);
        m[WEAPONS].fold(&self.stocks);
        m[PROJECTILES].fold(&self.shot_seq);
        m[ECONOMY].fold(&self.scrap);
        m[PRODUCTION].fold(&self.queue);
        m[PRODUCTION].fold(&self.pods_ready);
        m[AI].fold(&self.orders);
        m[AI].fold(&self.last_order_tick);
        m[PILOTS].fold(&self.may_take);
        m[BUILDINGS].fold(&self.owners);
        m[TERRAIN].fold(&self.vents);
        m[MATCH].fold(&self.scores);
        m[RNG].u64(self.rng);
        m[TRAINING].u64(u64::from(self.training_step));
        m[TRAINING].fold(&self.training_target);
    }

    fn inject(&mut self, system: usize) -> bool {
        match system {
            UNITS => {
                let Some(body) = self.runtime_entities.iter_mut().find(|e| e.kind == Kind::Unit) else {
                    return false;
                };
                // 1/16 m: exact in f32.
                body.position[0] += 0.0625;
            }
            PHYSICS => self.gravity += 0.0009765625,
            WEAPONS => {
                self.stocks.insert(0xFFFF_0001, 1);
            }
            PROJECTILES => self.shot_seq = self.shot_seq.wrapping_add(1),
            ECONOMY => self.scrap[1] = self.scrap[1].wrapping_add(7),
            PRODUCTION => self.pods_ready[1] = self.pods_ready[1].wrapping_add(1),
            AI => self.last_order_tick[1] = self.last_order_tick[1].wrapping_add(1),
            PILOTS => self.may_take[1][1] = !self.may_take[1][1],
            BUILDINGS => {
                self.owners.insert(0xFFFF_0004, 1);
            }
            TERRAIN => self.vents.push([1.5, 2.5]),
            MATCH => self.scores[1] = self.scores[1].wrapping_add(1),
            RNG => self.rng = self.rng.wrapping_add(1),
            TRAINING => {
                self.training_target = [1.0, 0.0, 2.0];
                self.training_step = 4;
            }
            _ => return false,
        }
        true
    }
}
