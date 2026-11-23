// SPDX-License-Identifier: MPL-2.0
//! `contact_pile`: seeded bodies funneled into a pit through a ring of static posts, pushed apart
//! and exchanging impulses through [`super::resolve_all`] every tick (ADR-8563).

use crate::broadphase::{BandExtent, BroadBody, BroadphaseConfig, DetBroadphase, body_band};
use crate::golden::scenarios::SplitMix64;
use crate::golden::{DetRun, DetScenario, StateHasher, TickRate};
use crate::math;

use super::{ContactBody, ContactConfig, ContactStats, Ram, resolve_all, resolve_all_n2};

/// Inward pull toward the pit centre (m/s²).
const PULL: f32 = 4.0;
/// Linear drag (1/s).
const DRAG: f32 = 0.8;
/// Inside this radius the pull stops (m).
const PIT_CORE: f32 = 0.5;
/// Height of the upper floor (a deck over the pit) one body in seven drives on (m).
const DECK_Y: f32 = 3.0;
/// Band of every body around its origin.
const EXTENT: BandExtent = BandExtent {
    below: 0.5,
    above: 1.0,
};

/// The `contact_pile` scenario: `n` discs (one in twenty a static post on a ring around the pit,
/// one in seven on a deck above it, so the floor rule is exercised) of seeded radius, mass,
/// restitution and friction, pulled into a pit and resolved through [`resolve_all`] each tick. The
/// engine golden runs 300 bodies for 600 ticks at 60 Hz; [`ContactPile::with_bodies`] scales it
/// (the cost evidence uses 900).
#[derive(Debug, Clone)]
pub struct ContactPile {
    name: &'static str,
    n: usize,
    reference_loop: bool,
    bodies: Vec<BroadBody>,
    props: Vec<ContactBody>,
    vel: Vec<[f32; 3]>,
    grid: DetBroadphase,
    cfg: ContactConfig,
    rams: Vec<Ram>,
    totals: ContactStats,
    last: ContactStats,
    errors: u32,
    dt: f32,
}

impl ContactPile {
    /// Bodies in the engine golden.
    pub const BODIES: usize = 300;

    /// The engine golden's scenario (300 bodies).
    pub fn new() -> Self {
        ContactPile::with_bodies("contact_pile", Self::BODIES)
    }

    /// The same scenario with `n` bodies under `name` (lowercase ASCII, digits and `_`). The pit's
    /// spawn area grows with `sqrt(n)`.
    pub fn with_bodies(name: &'static str, n: usize) -> Self {
        let mut s = ContactPile {
            name,
            n,
            reference_loop: false,
            bodies: Vec::new(),
            props: Vec::new(),
            vel: Vec::new(),
            grid: DetBroadphase::new(BroadphaseConfig::default()),
            cfg: ContactConfig {
                ram_threshold_m_s: 2.5,
                bounce_min_speed_m_s: 0.5,
            },
            rams: Vec::new(),
            totals: ContactStats::default(),
            last: ContactStats::default(),
            errors: 0,
            dt: TickRate::DEFAULT.dt(),
        };
        s.reset(&s.default_run());
        s
    }

    /// Resolve through the all-pairs loop ([`resolve_all_n2`]) instead of the grid: the same bits.
    pub fn use_reference_loop(&mut self, on: bool) {
        self.reference_loop = on;
    }

    /// Counters summed since the reset.
    pub fn totals(&self) -> ContactStats {
        self.totals
    }

    /// Counters of the last tick.
    pub fn last_stats(&self) -> ContactStats {
        self.last
    }

    /// Rams of the last tick, in visit order.
    pub fn last_rams(&self) -> &[Ram] {
        &self.rams
    }

    /// The bodies as the broadphase sees them.
    pub fn bodies(&self) -> &[BroadBody] {
        &self.bodies
    }

    /// The velocities.
    pub fn velocities(&self) -> &[[f32; 3]] {
        &self.vel
    }

    /// The contact settings per body.
    pub fn props(&self) -> &[ContactBody] {
        &self.props
    }
}

impl Default for ContactPile {
    fn default() -> Self {
        ContactPile::new()
    }
}

impl DetScenario for ContactPile {
    fn name(&self) -> &str {
        self.name
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0c0e, 600, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let mut rng = SplitMix64::new(run.seed);
        let n = self.n;
        self.dt = run.rate.dt();
        self.totals = ContactStats::default();
        self.last = ContactStats::default();
        self.errors = 0;
        self.rams.clear();
        self.bodies.clear();
        self.props.clear();
        self.vel.clear();
        // 300 bodies spawn in a 60 m square; the side grows with sqrt(n).
        let half = 30.0 * math::sqrt(n as f32 / Self::BODIES as f32);
        let ring = half * (14.0 / 30.0);
        for i in 0..n {
            let radius = 0.3 + rng.unit() * 0.5;
            if i.is_multiple_of(20) {
                // A static post on the ring.
                let angle = i as f32 / n as f32 * core::f32::consts::TAU;
                let (x, z) = (math::cos(angle) * ring, math::sin(angle) * ring);
                self.bodies
                    .push(BroadBody::new([x, 0.0, z], radius, body_band(0.0, EXTENT)));
                self.props.push(ContactBody {
                    friction: rng.unit() * 0.8,
                    ..ContactBody::STATIC
                });
                self.vel.push([0.0; 3]);
                continue;
            }
            let y = if i % 7 == 3 { DECK_Y } else { 0.0 };
            let (x, z) = (rng.signed() * half, rng.signed() * half);
            self.bodies
                .push(BroadBody::new([x, y, z], radius, body_band(y, EXTENT)));
            let mass = 0.5 + rng.unit() * 3.0;
            self.props.push(ContactBody {
                inv_mass: 1.0 / mass,
                restitution: rng.unit() * 0.6,
                friction: rng.unit() * 0.8,
                max_push_m: 0.25,
            });
            self.vel.push([rng.signed() * 3.0, 0.0, rng.signed() * 3.0]);
        }
    }

    fn step(&mut self, _tick: u32) {
        let dt = self.dt;
        let damp = 1.0 / (1.0 + DRAG * dt);
        for ((b, v), p) in self.bodies.iter_mut().zip(self.vel.iter_mut()).zip(&self.props) {
            if !p.is_dynamic() {
                continue;
            }
            let (x, z) = (b.pos[0], b.pos[2]);
            let r = math::sqrt(x * x + z * z);
            if r > PIT_CORE {
                v[0] -= x / r * PULL * dt;
                v[2] -= z / r * PULL * dt;
            }
            v[0] *= damp;
            v[2] *= damp;
            b.pos[0] += v[0] * dt;
            b.pos[2] += v[2] * dt;
        }
        self.rams.clear();
        let result = if self.reference_loop {
            let rule = self.grid.config().floor_rule;
            resolve_all_n2(
                &mut self.bodies,
                &self.props,
                &mut self.vel,
                &self.cfg,
                rule,
                &mut self.rams,
            )
        } else {
            resolve_all(
                &mut self.grid,
                &mut self.bodies,
                &self.props,
                &mut self.vel,
                &self.cfg,
                &mut self.rams,
            )
        };
        match result {
            Ok(stats) => {
                self.last = stats;
                self.totals.add(&stats);
            }
            Err(_) => self.errors = self.errors.wrapping_add(1),
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, (b, v)) in self.bodies.iter().zip(&self.vel).enumerate() {
            h.item("body", i as u32);
            h.f32("pos_x", b.pos[0]);
            h.f32("pos_z", b.pos[2]);
            h.f32("vel_x", v[0]);
            h.f32("vel_z", v[2]);
        }
        h.u64("pairs", self.totals.pairs);
        h.u64("pushes", self.totals.pushes);
        h.u64("capped_pushes", self.totals.capped_pushes);
        h.u64("rams", self.totals.rams);
        for r in &self.rams {
            h.u32("ram_a", r.a as u32);
            h.u32("ram_b", r.b as u32);
            h.f32("ram_impulse", r.impulse);
        }
        h.u32("errors", self.errors);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.bodies.first_mut() {
            Some(b) => {
                b.pos[0] = f32::from_bits(b.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}
