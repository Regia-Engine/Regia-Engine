// SPDX-License-Identifier: MPL-2.0
//! The `projectile_mix` determinism scenario (ADR-8509): seeded shots of four kinds (a fast point
//! shot, a bouncing fused grenade, a wind-blown shell, a bouncing ball with a range), against ground,
//! a slope, a thin wall, a crate and four body spheres in a ring, under changing wind, on four worlds.

use crate::golden::scenarios::SplitMix64;
use crate::golden::{DetRun, DetScenario, StateHasher};
use crate::math;
use crate::site_physics::WorldPhysics;
use crate::tick::TickRate;

use super::{
    HitBox, HitPlane, HitSphere, HitTarget, Impact, LinearScanHits, ProjectileParams, ProjectilePool,
    ProjectileState,
};

/// The worlds, 600 ticks each: `(gravity m/s², air density kg/m³, vacuum)`.
const WORLDS: [(f32, f32, bool); 4] = [
    (9.81, 1.2, false),
    (1.35, 5.3, false),
    (3.71, 0.6, false),
    (0.113, 0.0, true),
];

/// Ticks per world.
const WORLD_TICKS: u32 = 600;

/// Pool slots.
const CAPACITY: u16 = 48;

/// The four kinds, with their muzzle speeds (m/s).
const KINDS: [(ProjectileParams, f32); 4] = [
    (
        ProjectileParams {
            radius_m: 0.02,
            drag_per_m: 0.004,
            lifetime_s: 2.0,
            max_range_m: 400.0,
            ..ProjectileParams::DEFAULT
        },
        400.0,
    ),
    (
        ProjectileParams {
            radius_m: 0.05,
            fuse_s: Some(2.5),
            restitution: 0.45,
            friction: 0.3,
            max_bounces: 4,
            stop_speed_m_s: 1.0,
            wind_response: 0.2,
            ..ProjectileParams::DEFAULT
        },
        18.0,
    ),
    (
        ProjectileParams {
            radius_m: 0.1,
            drag_per_m: 0.001,
            linear_drag_per_s: 0.05,
            wind_response: 0.6,
            lifetime_s: 20.0,
            ..ProjectileParams::DEFAULT
        },
        60.0,
    ),
    (
        ProjectileParams {
            radius_m: 0.15,
            gravity_scale: 0.7,
            restitution: 0.8,
            friction: 0.1,
            max_bounces: 12,
            stop_speed_m_s: 0.5,
            max_range_m: 300.0,
            ..ProjectileParams::DEFAULT
        },
        25.0,
    ),
];

/// Seeded projectile shots stepped through a [`ProjectilePool`] against [`LinearScanHits`]
/// (ADR-8509). The `projectile_mix` golden pins its bits on every target.
#[derive(Debug, Clone)]
pub struct ProjectileMix {
    pool: ProjectilePool,
    hits: LinearScanHits,
    impacts: Vec<Impact>,
    rng: SplitMix64,
    wind: [f32; 3],
    dt: f32,
    total_impacts: u32,
    dropped: u32,
}

impl ProjectileMix {
    /// The scenario, reset for its default run.
    pub fn new() -> Self {
        let mut s = ProjectileMix {
            pool: ProjectilePool::new(CAPACITY),
            hits: LinearScanHits::default(),
            impacts: Vec::with_capacity(usize::from(CAPACITY)),
            rng: SplitMix64::new(0),
            wind: [0.0; 3],
            dt: TickRate::DEFAULT.dt(),
            total_impacts: 0,
            dropped: 0,
        };
        s.reset(&s.default_run());
        s
    }

    /// The live projectiles.
    pub fn pool(&self) -> &ProjectilePool {
        &self.pool
    }

    /// The impacts of the last tick, in slot order.
    pub fn impacts(&self) -> &[Impact] {
        &self.impacts
    }

    /// Impacts so far.
    pub fn total_impacts(&self) -> u32 {
        self.total_impacts
    }

    /// The world of `tick`.
    pub fn world_at(tick: u32) -> WorldPhysics {
        let (gravity_m_s2, air_density_kg_m3, vacuum) = WORLDS[(tick / WORLD_TICKS) as usize % WORLDS.len()];
        WorldPhysics {
            gravity_m_s2,
            air_density_kg_m3,
            vacuum,
            ..WorldPhysics::EARTH
        }
    }

    fn geometry() -> LinearScanHits {
        // A slope rising toward -Z: normal (0, 0.8, 0.6), through the point (0, 0, -30).
        let slope = HitPlane {
            normal: [0.0, 0.8, 0.6],
            offset: -18.0,
        };
        LinearScanHits {
            planes: vec![HitPlane::ground(0.0), slope],
            boxes: vec![
                // A 5 cm wall.
                HitBox {
                    min: [40.0, 0.0, -20.0],
                    max: [40.05, 6.0, 20.0],
                },
                HitBox {
                    min: [-12.0, 0.0, 8.0],
                    max: [-10.0, 2.0, 10.0],
                },
            ],
            // Four bodies in a ring 9 m around the launch area.
            spheres: [
                [9.0, 1.0, 0.0],
                [0.0, 1.5, 9.0],
                [-9.0, 1.0, 0.0],
                [0.0, 2.0, -9.0],
            ]
            .into_iter()
            .zip(1_u32..)
            .map(|(center, body)| HitSphere {
                center,
                radius_m: 1.5,
                body,
            })
            .collect(),
        }
    }
}

impl Default for ProjectileMix {
    fn default() -> Self {
        ProjectileMix::new()
    }
}

impl DetScenario for ProjectileMix {
    fn name(&self) -> &str {
        "projectile_mix"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_8509, WORLD_TICKS * WORLDS.len() as u32, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.pool = ProjectilePool::new(CAPACITY);
        self.hits = Self::geometry();
        self.impacts.clear();
        self.rng = SplitMix64::new(run.seed);
        self.wind = [0.0; 3];
        self.dt = run.rate.dt();
        self.total_impacts = 0;
        self.dropped = 0;
    }

    fn step(&mut self, tick: u32) {
        let rng = &mut self.rng;
        if tick.is_multiple_of(120) {
            self.wind = [rng.signed() * 8.0, rng.signed() * 0.5, rng.signed() * 8.0];
        }
        if tick.is_multiple_of(7) {
            let kind = (rng.next_u64() % KINDS.len() as u64) as usize;
            let (params, speed) = KINDS[kind];
            let pos = [rng.signed() * 5.0, 1.0 + rng.unit() * 2.0, rng.signed() * 5.0];
            let yaw = rng.signed() * core::f32::consts::PI;
            let pitch = 0.05 + rng.unit() * 1.1;
            let speed = speed * (0.75 + rng.unit() * 0.5);
            let flat = speed * math::cos(pitch);
            let vel = [
                flat * math::cos(yaw),
                speed * math::sin(pitch),
                flat * math::sin(yaw),
            ];
            let owner = ((rng.next_u64() & 1) == 1).then_some(1 + (kind as u32));
            if self
                .pool
                .spawn(params, ProjectileState::launch(pos, vel, owner))
                .is_none()
            {
                self.dropped += 1;
            }
        }
        self.impacts.clear();
        let world = Self::world_at(tick);
        self.pool
            .step_all(&world, self.wind, self.dt, &mut self.hits, &mut self.impacts);
        self.total_impacts += self.impacts.len() as u32;
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (slot, e) in self.pool.iter() {
            let s = &e.state;
            h.item("shot", u32::from(slot));
            h.f32("pos_x", s.pos[0]);
            h.f32("pos_y", s.pos[1]);
            h.f32("pos_z", s.pos[2]);
            h.f32("vel_x", s.vel[0]);
            h.f32("vel_y", s.vel[1]);
            h.f32("vel_z", s.vel[2]);
            h.u32("age_ticks", s.age_ticks);
            h.f32("travelled_m", s.travelled_m);
            h.u32("bounces", u32::from(s.bounces));
            h.bool("resting", s.resting);
        }
        for (i, imp) in self.impacts.iter().enumerate() {
            h.item("impact", i as u32);
            h.u32("slot", u32::from(imp.slot));
            h.u32("order", u32::from(imp.order));
            h.u32("reason", imp.reason as u32);
            h.f32("point_x", imp.point[0]);
            h.f32("point_y", imp.point[1]);
            h.f32("point_z", imp.point[2]);
            h.f32("normal_x", imp.normal[0]);
            h.f32("normal_y", imp.normal[1]);
            h.f32("normal_z", imp.normal[2]);
            h.u32(
                "target",
                match imp.target {
                    None => 0,
                    Some(HitTarget::World) => 1,
                    Some(HitTarget::Body(b)) => 2 + b,
                },
            );
            h.f32("speed", imp.speed);
            h.u32("age_ticks", imp.age_ticks);
        }
        h.item("totals", 0);
        h.u32("live", self.pool.len() as u32);
        h.u32("impacts", self.total_impacts);
        h.u32("dropped", self.dropped);
        h.f32("wind_x", self.wind[0]);
        h.f32("wind_y", self.wind[1]);
        h.f32("wind_z", self.wind[2]);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        let Some(slot) = self.pool.iter().next().map(|(slot, _)| slot) else {
            return false;
        };
        match self.pool.get_mut(slot) {
            Some(e) => {
                e.state.pos[0] = f32::from_bits(e.state.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}
