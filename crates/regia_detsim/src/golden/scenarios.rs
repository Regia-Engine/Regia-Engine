// SPDX-License-Identifier: MPL-2.0
//! The engine's own determinism scenarios (ADR-8046): a steering slalom, sleds following slopes,
//! discs colliding in a box, and the AI arena replayed from a recorded input log (DC-3.7b).
//!
//! Each is a small lockstep simulation written to the A-4 rules (f32, IEEE `+ - * / sqrt` in a fixed
//! order, transcendentals through [`crate::math`], no SIMD, no hash containers, inputs only from the
//! seed). Between them they exercise `sin`, `cos`, `atan2` and `sqrt` on a running state, a
//! branchy pair loop, and clamps, so a target whose float results differ shows up as a diverging
//! tick. Their goldens live in this crate's `goldens/` directory.
//!
//! [`Vehicle`] is also the engine's [`RateModel`]: the 60 Hz vs 120 Hz
//! gates run on it.

use crate::float::{det_clamp, det_min};
use crate::math;
use crate::tick::TickRate;

use super::rate_gate::RateModel;
use super::{DetRun, DetScenario, InputLogScenario, ScenarioFactory, StateHasher};
use crate::arena::{ArenaOrder, ArenaScenario, ArenaSettings};
use crate::replay::{self, InputLog, ReplayError, TickInputs};
use crate::state::{self, DetReadLimits, DetReader, DetState, DetStateError, DetWriter};

/// SplitMix64: the seeded input stream of the engine scenarios (integer only, so every target
/// draws the same numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SplitMix64(u64);

impl SplitMix64 {
    /// A stream from `seed`.
    pub const fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    /// The next 64 bits.
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// A value in `[0, 1)` from the top 24 bits (exact in f32).
    pub fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / 16_777_216.0
    }

    /// A value in `[-1, 1)`.
    pub fn signed(&mut self) -> f32 {
        self.unit() * 2.0 - 1.0
    }
}

/// Tuning of the engine's reference vehicle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VehicleParams {
    /// Forward acceleration at full throttle (m/s²).
    pub accel: f32,
    /// Sideways acceleration at full strafe (m/s²).
    pub strafe_accel: f32,
    /// Speed cap (m/s).
    pub max_speed: f32,
    /// Rate at which sideways velocity is removed (1/s).
    pub grip: f32,
    /// Linear drag (1/s).
    pub drag: f32,
    /// Yaw rate at full steer (rad/s).
    pub turn_rate: f32,
    /// Rate at which the yaw rate follows the steer input (1/s).
    pub turn_response: f32,
}

impl Default for VehicleParams {
    fn default() -> Self {
        VehicleParams {
            accel: 18.0,
            strafe_accel: 10.0,
            max_speed: 24.0,
            grip: 6.0,
            drag: 0.15,
            turn_rate: 2.4,
            turn_response: 10.0,
        }
    }
}

/// A planar vehicle: throttle, strafe and steer in, position, velocity and heading out. Heading 0
/// faces +z; positive yaw turns toward +x.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vehicle {
    /// Tuning.
    pub params: VehicleParams,
    /// Position `[x, z]` (m).
    pub pos: [f32; 2],
    /// Velocity `[x, z]` (m/s).
    pub vel: [f32; 2],
    /// Heading (rad, not wrapped).
    pub yaw: f32,
    /// Yaw rate (rad/s).
    pub yaw_rate: f32,
}

impl Vehicle {
    /// A vehicle at rest at the origin.
    pub fn new(params: VehicleParams) -> Self {
        Vehicle {
            params,
            ..Vehicle::default()
        }
    }

    /// Advance by `dt` with `input = [throttle, strafe, steer]`, each clamped to `[-1, 1]`.
    pub fn step(&mut self, input: [f32; 3], dt: f32) {
        let p = self.params;
        let throttle = det_clamp(input[0], -1.0, 1.0);
        let strafe = det_clamp(input[1], -1.0, 1.0);
        let steer = det_clamp(input[2], -1.0, 1.0);

        let target_rate = steer * p.turn_rate;
        let k = det_min(p.turn_response * dt, 1.0);
        self.yaw_rate += (target_rate - self.yaw_rate) * k;
        self.yaw += self.yaw_rate * dt;

        let s = math::sin(self.yaw);
        let c = math::cos(self.yaw);
        let fwd = [s, c];
        let right = [c, -s];

        let fa = throttle * p.accel * dt;
        let sa = strafe * p.strafe_accel * dt;
        self.vel[0] += fwd[0] * fa + right[0] * sa;
        self.vel[1] += fwd[1] * fa + right[1] * sa;

        let lateral = self.vel[0] * right[0] + self.vel[1] * right[1];
        let g = det_min(p.grip * dt, 1.0);
        self.vel[0] -= right[0] * lateral * g;
        self.vel[1] -= right[1] * lateral * g;

        let d = 1.0 / (1.0 + p.drag * dt);
        self.vel[0] *= d;
        self.vel[1] *= d;

        let speed2 = self.vel[0] * self.vel[0] + self.vel[1] * self.vel[1];
        if speed2 > p.max_speed * p.max_speed {
            let scale = p.max_speed / math::sqrt(speed2);
            self.vel[0] *= scale;
            self.vel[1] *= scale;
        }

        self.pos[0] += self.vel[0] * dt;
        self.pos[1] += self.vel[1] * dt;
    }

    fn hash(&self, h: &mut StateHasher) {
        h.f32("pos_x", self.pos[0]);
        h.f32("pos_z", self.pos[1]);
        h.f32("vel_x", self.vel[0]);
        h.f32("vel_z", self.vel[1]);
        h.f32("yaw", self.yaw);
        h.f32("yaw_rate", self.yaw_rate);
    }

    /// The tuning, in [`VehicleParams`] declaration order (what `step` reads besides the pose).
    fn hash_params(&self, h: &mut StateHasher) {
        let p = &self.params;
        h.f32("accel", p.accel);
        h.f32("strafe_accel", p.strafe_accel);
        h.f32("max_speed", p.max_speed);
        h.f32("grip", p.grip);
        h.f32("drag", p.drag);
        h.f32("turn_rate", p.turn_rate);
        h.f32("turn_response", p.turn_response);
    }
}

impl RateModel for Vehicle {
    fn reset(&mut self) {
        *self = Vehicle::new(self.params);
    }

    fn step(&mut self, input: [f32; 3], dt: f32) {
        Vehicle::step(self, input, dt);
    }

    fn velocity_xz(&self) -> [f32; 2] {
        self.vel
    }

    fn yaw(&self) -> f32 {
        self.yaw
    }
}

/// `steer_slalom`: four vehicles with seeded throttle, strafe and steer inputs that change every
/// half second, 20 s at 60 Hz.
#[derive(Debug, Clone)]
pub struct SteerScenario {
    cars: Vec<Vehicle>,
    inputs: Vec<[f32; 3]>,
    rng: SplitMix64,
    dt: f32,
}

impl SteerScenario {
    /// Vehicles in the scenario.
    pub const CARS: usize = 4;
    /// Ticks between input changes.
    pub const INPUT_PERIOD: u32 = 30;

    /// The scenario before its first reset.
    pub fn new() -> Self {
        let mut s = SteerScenario {
            cars: Vec::new(),
            inputs: Vec::new(),
            rng: SplitMix64::new(0),
            dt: TickRate::DEFAULT.dt(),
        };
        s.reset(&s.default_run());
        s
    }

    /// The vehicles.
    pub fn cars(&self) -> &[Vehicle] {
        &self.cars
    }
}

impl Default for SteerScenario {
    fn default() -> Self {
        SteerScenario::new()
    }
}

impl DetScenario for SteerScenario {
    fn name(&self) -> &str {
        "steer_slalom"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0001, 1200, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        self.cars = (0..Self::CARS)
            .map(|i| {
                let mut v = Vehicle::new(VehicleParams::default());
                v.pos[0] = i as f32 * 4.0 - 6.0;
                v
            })
            .collect();
        self.inputs = vec![[0.0; 3]; Self::CARS];
    }

    fn step(&mut self, tick: u32) {
        if tick.is_multiple_of(Self::INPUT_PERIOD) {
            for input in &mut self.inputs {
                let throttle = self.rng.unit() * 1.5 - 0.5;
                let strafe = self.rng.signed() * 0.3;
                let steer = self.rng.signed();
                *input = [throttle, strafe, steer];
            }
        }
        for (car, input) in self.cars.iter_mut().zip(&self.inputs) {
            car.step(*input, self.dt);
        }
    }

    // H2 (review det-core-review-c): every field the snapshot body stores and `step` reads: the pose
    // (first, so `flip_bit` still flips the first hashed field), the tuning, the held inputs, the
    // input stream's position and `dt`. Before, only the pose was hashed, so two input streams with
    // the same cars shared a fingerprint until the next resample (up to 29 ticks).
    fn hash_state(&self, h: &mut StateHasher) {
        for (i, car) in self.cars.iter().enumerate() {
            h.item("car", i as u32);
            car.hash(h);
            car.hash_params(h);
        }
        for (i, input) in self.inputs.iter().enumerate() {
            h.item("input", i as u32);
            h.f32("throttle", input[0]);
            h.f32("strafe", input[1]);
            h.f32("steer", input[2]);
        }
        h.item("run", 0);
        h.u64("rng", self.rng.0);
        h.f32("dt", self.dt);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.cars.first_mut() {
            Some(car) => {
                car.pos[0] = f32::from_bits(car.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}

/// An analytic terrain: rolling hills on a tilt, `h = amp * sin(kx * x) * cos(kz * z) + tilt * x`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Terrain {
    /// Hill height (m).
    pub amp: f32,
    /// Wave number along x (rad/m).
    pub kx: f32,
    /// Wave number along z (rad/m).
    pub kz: f32,
    /// Tilt along x (m/m).
    pub tilt: f32,
}

impl Default for Terrain {
    fn default() -> Self {
        Terrain {
            amp: 3.0,
            kx: 0.21,
            kz: 0.17,
            tilt: 0.08,
        }
    }
}

impl Terrain {
    /// Height and gradient `(h, dh/dx, dh/dz)` at `(x, z)`.
    pub fn sample(&self, x: f32, z: f32) -> (f32, f32, f32) {
        let sx = math::sin(self.kx * x);
        let cx = math::cos(self.kx * x);
        let sz = math::sin(self.kz * z);
        let cz = math::cos(self.kz * z);
        let h = self.amp * sx * cz + self.tilt * x;
        let gx = self.amp * self.kx * cx * cz + self.tilt;
        let gz = -self.amp * self.kz * sx * sz;
        (h, gx, gz)
    }
}

/// One sled of [`SlopeScenario`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Sled {
    pos: [f32; 3],
    vel: [f32; 2],
    slope: f32,
}

/// `slope_follow`: eight sleds released on rolling hills, sliding under gravity along the surface
/// with friction, kept in a 60 m square; 15 s at 60 Hz.
#[derive(Debug, Clone)]
pub struct SlopeScenario {
    terrain: Terrain,
    sleds: Vec<Sled>,
    dt: f32,
}

impl SlopeScenario {
    /// Sleds in the scenario.
    pub const SLEDS: usize = 8;
    /// Half the side of the square the sleds stay in (m).
    pub const HALF_EXTENT: f32 = 30.0;
    /// Gravity (m/s²).
    pub const GRAVITY: f32 = 9.81;
    /// Friction (1/s).
    pub const FRICTION: f32 = 0.4;

    /// The scenario before its first reset.
    pub fn new() -> Self {
        let mut s = SlopeScenario {
            terrain: Terrain::default(),
            sleds: Vec::new(),
            dt: TickRate::DEFAULT.dt(),
        };
        s.reset(&s.default_run());
        s
    }
}

impl Default for SlopeScenario {
    fn default() -> Self {
        SlopeScenario::new()
    }
}

impl DetScenario for SlopeScenario {
    fn name(&self) -> &str {
        "slope_follow"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0002, 900, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let mut rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        let terrain = self.terrain;
        self.sleds = (0..Self::SLEDS)
            .map(|_| {
                let x = rng.signed() * 20.0;
                let z = rng.signed() * 20.0;
                let (y, _, _) = terrain.sample(x, z);
                Sled {
                    pos: [x, y, z],
                    vel: [rng.signed(), rng.signed()],
                    slope: 0.0,
                }
            })
            .collect();
    }

    fn step(&mut self, _tick: u32) {
        let dt = self.dt;
        let terrain = self.terrain;
        let e = Self::HALF_EXTENT;
        for s in &mut self.sleds {
            let (_, gx, gz) = terrain.sample(s.pos[0], s.pos[2]);
            let n2 = 1.0 + gx * gx + gz * gz;
            s.vel[0] += -Self::GRAVITY * gx / n2 * dt;
            s.vel[1] += -Self::GRAVITY * gz / n2 * dt;
            let f = 1.0 / (1.0 + Self::FRICTION * dt);
            s.vel[0] *= f;
            s.vel[1] *= f;
            s.pos[0] += s.vel[0] * dt;
            s.pos[2] += s.vel[1] * dt;
            for (axis, v) in [(0, 0), (2, 1)] {
                if s.pos[axis] > e {
                    s.pos[axis] = e;
                    s.vel[v] = -s.vel[v] * 0.5;
                } else if s.pos[axis] < -e {
                    s.pos[axis] = -e;
                    s.vel[v] = -s.vel[v] * 0.5;
                }
            }
            let (h, gx, gz) = terrain.sample(s.pos[0], s.pos[2]);
            s.pos[1] = h;
            s.slope = math::atan2(math::sqrt(gx * gx + gz * gz), 1.0);
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, s) in self.sleds.iter().enumerate() {
            h.item("sled", i as u32);
            h.f32("pos_x", s.pos[0]);
            h.f32("pos_y", s.pos[1]);
            h.f32("pos_z", s.pos[2]);
            h.f32("vel_x", s.vel[0]);
            h.f32("vel_z", s.vel[1]);
            h.f32("slope", s.slope);
        }
        // The terrain and `dt` the next step reads (the snapshot body stores both; review
        // det-core-review-c nit beside H2/H3).
        h.item("run", 0);
        h.f32("amp", self.terrain.amp);
        h.f32("kx", self.terrain.kx);
        h.f32("kz", self.terrain.kz);
        h.f32("tilt", self.terrain.tilt);
        h.f32("dt", self.dt);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.sleds.first_mut() {
            Some(s) => {
                s.pos[0] = f32::from_bits(s.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}

/// One disc of [`CollideScenario`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
struct Disc {
    pos: [f32; 2],
    vel: [f32; 2],
    radius: f32,
}

/// `disc_collide`: 24 discs of seeded radius and velocity bouncing in a 24 m box, pushed apart and
/// exchanging impulses pair by pair in `(i, j)` order; 15 s at 60 Hz.
#[derive(Debug, Clone)]
pub struct CollideScenario {
    discs: Vec<Disc>,
    contacts: u32,
    dt: f32,
}

impl CollideScenario {
    /// Discs in the scenario.
    pub const DISCS: usize = 24;
    /// Half the side of the box (m).
    pub const HALF_EXTENT: f32 = 12.0;
    /// Restitution of disc and wall contacts.
    pub const RESTITUTION: f32 = 0.95;

    /// The scenario before its first reset.
    pub fn new() -> Self {
        let mut s = CollideScenario {
            discs: Vec::new(),
            contacts: 0,
            dt: TickRate::DEFAULT.dt(),
        };
        s.reset(&s.default_run());
        s
    }

    /// Disc contacts resolved since the reset.
    pub fn contacts(&self) -> u32 {
        self.contacts
    }
}

impl Default for CollideScenario {
    fn default() -> Self {
        CollideScenario::new()
    }
}

impl DetScenario for CollideScenario {
    fn name(&self) -> &str {
        "disc_collide"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0003, 900, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let mut rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        self.contacts = 0;
        let e = Self::HALF_EXTENT - 1.0;
        self.discs = (0..Self::DISCS)
            .map(|_| Disc {
                pos: [rng.signed() * e, rng.signed() * e],
                vel: [rng.signed() * 4.0, rng.signed() * 4.0],
                radius: 0.4 + rng.unit() * 0.6,
            })
            .collect();
    }

    fn step(&mut self, _tick: u32) {
        let dt = self.dt;
        let e = Self::HALF_EXTENT;
        let rest = Self::RESTITUTION;
        for d in &mut self.discs {
            let lim = e - d.radius;
            for (pos, vel) in d.pos.iter_mut().zip(d.vel.iter_mut()) {
                *pos += *vel * dt;
                if *pos > lim {
                    *pos = lim;
                    *vel = -*vel * rest;
                } else if *pos < -lim {
                    *pos = -lim;
                    *vel = -*vel * rest;
                }
            }
        }
        let n = self.discs.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let (left, right) = self.discs.split_at_mut(j);
                let a = &mut left[i];
                let b = &mut right[0];
                let dx = b.pos[0] - a.pos[0];
                let dz = b.pos[1] - a.pos[1];
                let d2 = dx * dx + dz * dz;
                let rs = a.radius + b.radius;
                if d2 >= rs * rs {
                    continue;
                }
                self.contacts = self.contacts.wrapping_add(1);
                let dist = math::sqrt(d2);
                let (nx, nz) = if dist > 1.0e-6 {
                    (dx / dist, dz / dist)
                } else {
                    (1.0, 0.0)
                };
                let ma = a.radius * a.radius;
                let mb = b.radius * b.radius;
                let inv = 1.0 / (ma + mb);
                let overlap = rs - dist;
                a.pos[0] -= nx * overlap * mb * inv;
                a.pos[1] -= nz * overlap * mb * inv;
                b.pos[0] += nx * overlap * ma * inv;
                b.pos[1] += nz * overlap * ma * inv;
                let rel = (b.vel[0] - a.vel[0]) * nx + (b.vel[1] - a.vel[1]) * nz;
                if rel < 0.0 {
                    let j_imp = -(1.0 + rest) * rel / (1.0 / ma + 1.0 / mb);
                    a.vel[0] -= nx * j_imp / ma;
                    a.vel[1] -= nz * j_imp / ma;
                    b.vel[0] += nx * j_imp / mb;
                    b.vel[1] += nz * j_imp / mb;
                }
            }
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, d) in self.discs.iter().enumerate() {
            h.item("disc", i as u32);
            h.f32("pos_x", d.pos[0]);
            h.f32("pos_z", d.pos[1]);
            h.f32("vel_x", d.vel[0]);
            h.f32("vel_z", d.vel[1]);
            // H3 (review det-core-review-c): the wall limit and the pair test read the radius.
            h.f32("radius", d.radius);
        }
        h.item("run", 0);
        h.u32("contacts", self.contacts);
        h.f32("dt", self.dt);
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        match self.discs.first_mut() {
            Some(d) => {
                d.pos[0] = f32::from_bits(d.pos[0].to_bits() ^ (1 << (bit % 32)));
                true
            }
            None => false,
        }
    }
}

// --- DetState (DC-3.7a): every field a later tick reads, raw float bits, fixed order. ---

/// Saves the stream position (one `u64`).
impl DetState for SplitMix64 {
    const STATE_TAG: [u8; 8] = *b"splitmix";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.u64(self.0);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(SplitMix64(r.u64()?))
    }
}

/// Saves the seven tuning values in declaration order.
impl DetState for VehicleParams {
    const STATE_TAG: [u8; 8] = *b"vehparam";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        for v in [
            self.accel,
            self.strafe_accel,
            self.max_speed,
            self.grip,
            self.drag,
            self.turn_rate,
            self.turn_response,
        ] {
            w.f32(v);
        }
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(VehicleParams {
            accel: r.f32()?,
            strafe_accel: r.f32()?,
            max_speed: r.f32()?,
            grip: r.f32()?,
            drag: r.f32()?,
            turn_rate: r.f32()?,
            turn_response: r.f32()?,
        })
    }
}

/// Saves the tuning, then position, velocity, yaw and yaw rate.
impl DetState for Vehicle {
    const STATE_TAG: [u8; 8] = *b"vehicle\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        self.params.save(w);
        for v in [
            self.pos[0],
            self.pos[1],
            self.vel[0],
            self.vel[1],
            self.yaw,
            self.yaw_rate,
        ] {
            w.f32(v);
        }
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Vehicle {
            params: VehicleParams::load(r)?,
            pos: [r.f32()?, r.f32()?],
            vel: [r.f32()?, r.f32()?],
            yaw: r.f32()?,
            yaw_rate: r.f32()?,
        })
    }
}

/// Saves the four shape values in declaration order.
impl DetState for Terrain {
    const STATE_TAG: [u8; 8] = *b"terrain\0";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        for v in [self.amp, self.kx, self.kz, self.tilt] {
            w.f32(v);
        }
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(Terrain {
            amp: r.f32()?,
            kx: r.f32()?,
            kz: r.f32()?,
            tilt: r.f32()?,
        })
    }
}

/// Saves the vehicles, the held inputs, the input stream and `dt`. `load` refuses a different
/// number of inputs and vehicles.
impl DetState for SteerScenario {
    const STATE_TAG: [u8; 8] = *b"steerslm";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.seq(&self.cars, |w, car| car.save(w));
        w.seq(&self.inputs, |w, input| {
            for v in input {
                w.f32(*v);
            }
        });
        self.rng.save(w);
        w.f32(self.dt);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        let cars = r.seq(Vehicle::load)?;
        let inputs = r.seq(|r| Ok([r.f32()?, r.f32()?, r.f32()?]))?;
        if cars.len() != inputs.len() {
            return Err(DetStateError::bad_value(format_args!(
                "{} cars with {} inputs",
                cars.len(),
                inputs.len()
            )));
        }
        Ok(SteerScenario {
            cars,
            inputs,
            rng: SplitMix64::load(r)?,
            dt: r.f32()?,
        })
    }
}

/// Saves the terrain, every sled (position, velocity, slope) and `dt`.
impl DetState for SlopeScenario {
    const STATE_TAG: [u8; 8] = *b"slopefol";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        self.terrain.save(w);
        w.seq(&self.sleds, |w, s| {
            for v in [s.pos[0], s.pos[1], s.pos[2], s.vel[0], s.vel[1], s.slope] {
                w.f32(v);
            }
        });
        w.f32(self.dt);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(SlopeScenario {
            terrain: Terrain::load(r)?,
            sleds: r.seq(|r| {
                Ok(Sled {
                    pos: [r.f32()?, r.f32()?, r.f32()?],
                    vel: [r.f32()?, r.f32()?],
                    slope: r.f32()?,
                })
            })?,
            dt: r.f32()?,
        })
    }
}

/// Saves every disc (position, velocity, radius), the contact count and `dt`.
impl DetState for CollideScenario {
    const STATE_TAG: [u8; 8] = *b"disccoll";
    const STATE_VERSION: u32 = 1;
    fn save(&self, w: &mut DetWriter) {
        w.seq(&self.discs, |w, d| {
            for v in [d.pos[0], d.pos[1], d.vel[0], d.vel[1], d.radius] {
                w.f32(v);
            }
        });
        w.u32(self.contacts);
        w.f32(self.dt);
    }
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
        Ok(CollideScenario {
            discs: r.seq(|r| {
                Ok(Disc {
                    pos: [r.f32()?, r.f32()?],
                    vel: [r.f32()?, r.f32()?],
                    radius: r.f32()?,
                })
            })?,
            contacts: r.u32()?,
            dt: r.f32()?,
        })
    }
}

/// Factories for the engine scenarios, in registration order: `steer_slalom`, `slope_follow`,
/// `disc_collide`, `hover_libm_1000` ([`crate::hover::HoverLibm1000`], ADR-8079), `hover_bad_input`
/// ([`crate::hover::HoverBadInput`], ADR-8309), `math_rng_fixed_vectors` ([`super::vectors`]), `hover_mixed`
/// ([`crate::hover::HoverMixed`]), `lowg_ballistic_mixed` ([`crate::hover::LowgBallisticMixed`]), `hover_fleet_terrain`
/// ([`crate::hover::HoverFleetTerrain`], ADR-8508), `projectile_mix`
/// ([`crate::projectile::ProjectileMix`], ADR-8509), `input_log_arena`,
/// `contact_pile` ([`crate::contact::ContactPile`], ADR-8563), `ground_vehicle_mix`
/// ([`crate::ground_vehicle::GroundVehicleMix`], ADR-8551).
pub fn engine_scenarios() -> Vec<ScenarioFactory> {
    vec![
        Box::new(|| Box::new(SteerScenario::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(SlopeScenario::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(CollideScenario::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::hover::HoverLibm1000::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::hover::HoverBadInput::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(super::vectors::MathRngFixedScenario::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::hover::HoverMixed::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::hover::LowgBallisticMixed::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::hover::HoverFleetTerrain::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::projectile::ProjectileMix::new()) as Box<dyn DetScenario>),
        Box::new(|| input_log_arena_scenario()),
        Box::new(|| Box::new(crate::contact::ContactPile::new()) as Box<dyn DetScenario>),
        Box::new(|| Box::new(crate::ground_vehicle::GroundVehicleMix::new()) as Box<dyn DetScenario>),
    ]
}

/// Name of the engine's input-log golden scenario.
pub const INPUT_LOG_ARENA: &str = "input_log_arena";

/// The committed input log of [`INPUT_LOG_ARENA`] (`goldens/input_log_arena.detlog`): the AI arena,
/// 6 units a team, snapshot carried at tick 40, then 600 ticks of seeded team orders with the hashes
/// recorded every 20th tick. [`record_input_log_arena`] is its recipe; a test holds the two equal.
pub const INPUT_LOG_ARENA_LOG: &[u8] = include_bytes!("../../goldens/input_log_arena.detlog");

/// Record the [`INPUT_LOG_ARENA`] fixture from its recipe: the arena (6 units a team, seed 7) steps
/// 40 ticks with no orders, is snapshotted, then steps 600 ticks of seeded orders (each seat present
/// 7 times in 8, holding 1 in 6, sprinting 1 in 4) through [`crate::replay::step_recorded`], keeping
/// the hashes of every 20th tick. Returns the encoded log.
pub fn record_input_log_arena() -> Result<Vec<u8>, ReplayError> {
    let settings = ArenaSettings {
        units_per_team: 6,
        ..ArenaSettings::default()
    };
    let mut sim = ArenaScenario::new(settings)
        .map_err(|e| ReplayError::State(DetStateError::bad_value(format_args!("arena settings: {e}"))))?;
    DetScenario::reset(&mut sim, &DetRun::new(7, 640, TickRate::DEFAULT));
    for tick in 0..40u64 {
        replay::advance(&mut sim, &TickInputs::new(tick));
    }
    let mut log = InputLog::for_sim::<ArenaScenario, 3>(40);
    log.set_start_snapshot(state::snapshot(&sim, 40)?)?;
    let mut rng = SplitMix64::new(0x1e9_7ec0_7d1d);
    for tick in 40..640u64 {
        let mut inputs = TickInputs::new(tick);
        for seat in 0..2u16 {
            let roll = rng.next_u64();
            let order = (!roll.is_multiple_of(8)).then_some(ArenaOrder {
                hold: (roll >> 8).is_multiple_of(6),
                sprint: (roll >> 16).is_multiple_of(4),
            });
            inputs.push(seat, order)?;
        }
        replay::step_recorded(&mut sim, inputs, &mut log, tick % 20 == 19)?;
    }
    Ok(log.encode()?)
}

/// The [`INPUT_LOG_ARENA`] scenario from the committed log. A log that does not decode (a codec
/// change without a re-recorded fixture) gives a scenario whose every tick hashes as `broken`, so
/// its golden check fails and says so.
pub fn input_log_arena_scenario() -> Box<dyn DetScenario> {
    match InputLogScenario::<ArenaScenario, 3>::from_bytes(
        INPUT_LOG_ARENA,
        None,
        INPUT_LOG_ARENA_LOG,
        DetReadLimits::DEFAULT,
    ) {
        Ok(scenario) => Box::new(scenario),
        Err(error) => Box::new(BrokenScenario {
            name: INPUT_LOG_ARENA,
            code: error.code(),
        }),
    }
}

/// Stands in for a scenario whose fixture does not load: hashes its error code every tick.
struct BrokenScenario {
    name: &'static str,
    code: &'static str,
}

impl DetScenario for BrokenScenario {
    fn name(&self) -> &str {
        self.name
    }
    fn default_run(&self) -> DetRun {
        DetRun::new(0, 1, TickRate::DEFAULT)
    }
    fn reset(&mut self, _run: &DetRun) {}
    fn step(&mut self, _tick: u32) {}
    fn hash_state(&self, h: &mut StateHasher) {
        h.u64("broken", 1);
        for b in self.code.bytes() {
            h.u32("code", u32::from(b));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fingerprint(s: &dyn DetScenario) -> u64 {
        let mut h = StateHasher::new();
        s.hash_state(&mut h);
        h.finish()
    }

    #[test]
    fn steer_fingerprint_covers_the_input_rng() {
        // H2: two seeds place the same cars with the same (zero) inputs; only the stream differs.
        let mut a = SteerScenario::new();
        let mut b = SteerScenario::new();
        a.reset(&DetRun::new(1, 60, TickRate::DEFAULT));
        b.reset(&DetRun::new(2, 60, TickRate::DEFAULT));
        assert_eq!(a.cars, b.cars);
        assert_eq!(a.inputs, b.inputs);
        assert_ne!(fingerprint(&a), fingerprint(&b));
        // Tick 0 draws; one tick later the held inputs differ too, and so do the fingerprints.
        a.step(0);
        b.step(0);
        assert_ne!(fingerprint(&a), fingerprint(&b));
        // The held inputs alone, the tuning alone and `dt` alone move it.
        let mut c = a.clone();
        c.inputs[2][1] = -c.inputs[2][1] + 0.25;
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let mut c = a.clone();
        c.cars[1].params.grip += 1.0;
        assert_ne!(fingerprint(&a), fingerprint(&c));
        let mut c = a.clone();
        c.dt = TickRate::new(120).map_or(c.dt * 0.5, |r| r.dt());
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }

    #[test]
    fn collide_fingerprint_covers_radius() {
        // H3: one disc at rest at the origin; a radius of 0.4 and of 0.5 step identically (no wall
        // contact, no pair), so only the radius tells them apart.
        let mut a = CollideScenario::new();
        a.discs.clear();
        a.discs.push(Disc {
            pos: [0.0, 0.0],
            vel: [0.0, 0.0],
            radius: 0.4,
        });
        let mut b = a.clone();
        b.discs[0].radius = 0.5;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        a.step(0);
        b.step(0);
        assert_eq!(a.discs[0].pos, b.discs[0].pos);
        assert_ne!(fingerprint(&a), fingerprint(&b));
        let mut c = a.clone();
        c.dt *= 0.5;
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }

    #[test]
    fn slope_fingerprint_covers_terrain_and_dt() {
        let a = SlopeScenario::new();
        let mut b = a.clone();
        b.terrain.kz += 0.01;
        assert_ne!(fingerprint(&a), fingerprint(&b));
        let mut c = a.clone();
        c.dt *= 0.5;
        assert_ne!(fingerprint(&a), fingerprint(&c));
    }

    #[test]
    fn flip_bit_still_flips_the_first_hashed_field() {
        // `DetScenario::flip_bit` flips the first hashed field; the new fields come after it.
        for (mut s, first) in [
            (
                Box::new(SteerScenario::new()) as Box<dyn DetScenario>,
                "car[0].pos_x",
            ),
            (Box::new(SlopeScenario::new()), "sled[0].pos_x"),
            (Box::new(CollideScenario::new()), "disc[0].pos_x"),
        ] {
            let mut h = StateHasher::with_log();
            s.hash_state(&mut h);
            let before = h.into_fields();
            assert_eq!(before[0].name, first);
            assert!(s.flip_bit(0));
            let mut h = StateHasher::with_log();
            s.hash_state(&mut h);
            let after = h.into_fields();
            let changed: Vec<&str> = before
                .iter()
                .zip(&after)
                .filter(|(x, y)| x.bits != y.bits)
                .map(|(x, _)| x.name.as_str())
                .collect();
            assert_eq!(changed, [first]);
        }
    }
}
