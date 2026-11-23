// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/soak.rs, commit d6ed37677), ADR-8012.
// Mark: adapt (ADR-8148): circular seating (lines 139-159) and entity-id-ordered
// fingerprint shape (202-241). Game rosters, names, economy, health and layout are
// replaced with configurable neutral discs and seat inputs. F-02 owns the hasher;
// F-06 owns IDs and ordered contacts. This is not a port of the game's simulation.

//! Configurable CPU-only scale soak (ADR-8148), independent of game content.
//!
//! Defaults exercise 40 input seats and 900 moving discs at 60 Hz. Every tick
//! drives inputs, moves discs, recycles IDs at a fixed interval and resolves
//! contacts. [`ScaleScenario`] implements the golden harness without writing
//! golden files. [`ScaleScenario::step_observed`] brackets each pass for host
//! tracing spans and the standalone runner's measurements.

use crate::broadphase::{
    BroadBody, BroadphaseConfig, Contact, DetBroadphase, FloorRule, for_each_contact_n2,
};
use crate::float::det_clamp;
use crate::golden::scenarios::SplitMix64;
use crate::golden::{DetRun, DetScenario, StateHasher};
use crate::ids::{DetHandle, DetIdAllocator};
use crate::{math, tick::TickRate};

pub mod profile;

pub mod ai;

/// Settings for the synthetic load, not game limits or gameplay defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SoakConfig {
    /// Independent input seats, each owning at least one entity.
    pub seats: u16,
    /// Live discs; must be at least `seats`.
    pub entities: u16,
    /// Half-width of the bounded arena, meters; finite and in `1..=10000`.
    pub half_extent: f32,
    /// Disc radius, meters; finite, positive and smaller than `half_extent`.
    pub radius: f32,
    /// Desired speed in meters/second; finite and in `0..=1000`.
    pub speed: f32,
    /// Recycle one entity per seat every this many ticks; positive.
    pub recycle_interval: u32,
}

impl Default for SoakConfig {
    fn default() -> Self {
        Self {
            seats: 40,
            entities: 900,
            half_extent: 64.0,
            radius: 0.75,
            speed: 6.0,
            recycle_interval: 60,
        }
    }
}

/// Invalid scale settings. The stable code is [`SoakConfigError::code`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SoakConfigError;

impl SoakConfigError {
    /// Protocol error code.
    pub const fn code(self) -> &'static str {
        "invalid_soak_config"
    }
}

impl std::fmt::Display for SoakConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("invalid_soak_config: require 1 <= seats <= entities, finite geometry/speed and a positive recycle interval")
    }
}
impl std::error::Error for SoakConfigError {}

impl SoakConfig {
    /// Check settings before allocating or stepping.
    pub fn validate(self) -> Result<(), SoakConfigError> {
        if self.seats == 0
            || self.entities < self.seats
            || !self.half_extent.is_finite()
            || !(1.0..=10000.0).contains(&self.half_extent)
            || !self.radius.is_finite()
            || self.radius <= 0.0
            || self.radius >= self.half_extent
            || !self.speed.is_finite()
            || !(0.0..=1000.0).contains(&self.speed)
            || self.recycle_interval == 0
        {
            return Err(SoakConfigError);
        }
        Ok(())
    }
}

/// A profiled pass; `Step` encloses the four other passes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoakPass {
    /// All input seats.
    Input,
    /// Integration and boundary reflection of all entities.
    Movement,
    /// FIFO ID reuse and generation changes.
    Recycling,
    /// Grid construction, ordered contact queries and resolution.
    Contacts,
    /// The complete tick, excluding hashing and reporting.
    Step,
}

impl SoakPass {
    /// Report order, also used to index timing arrays.
    pub const ALL: [Self; 5] = [
        Self::Input,
        Self::Movement,
        Self::Recycling,
        Self::Contacts,
        Self::Step,
    ];
    /// Array index.
    pub const fn index(self) -> usize {
        self as usize
    }
    /// Stable lowercase report and tracing-span name.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Movement => "movement",
            Self::Recycling => "recycling",
            Self::Contacts => "contacts",
            Self::Step => "step",
        }
    }
}

/// Host profiling hook. Enter/exit a tracing span here or measure elapsed time.
/// No-op observers leave deterministic state unchanged.
pub trait SoakObserver {
    /// Enter a pass; `Step` nests all four inner passes.
    fn begin(&mut self, pass: SoakPass);
    /// Exit the matching pass.
    fn end(&mut self, pass: SoakPass);
}
impl SoakObserver for () {
    fn begin(&mut self, _: SoakPass) {}
    fn end(&mut self, _: SoakPass) {}
}

#[derive(Debug, Clone)]
struct Seat {
    phase: f32,
    input: [f32; 2],
}
#[derive(Debug, Clone)]
struct Entity {
    handle: DetHandle<u16>,
    seat: usize,
    velocity: [f32; 2],
}

/// Seeded load scenario. Construct, reset and step through [`DetScenario`].
/// The all-pairs path is an independent correctness oracle, never a benchmark substitute.
pub struct ScaleScenario {
    config: SoakConfig,
    seats: Vec<Seat>,
    entities: Vec<Entity>,
    bodies: Vec<BroadBody>,
    ids: DetIdAllocator<u16>,
    grid: DetBroadphase,
    dt: f32,
    tick: u32,
    contacts: u64,
    recycled: u64,
}

impl ScaleScenario {
    /// Validate settings and initialize the default seeded run.
    pub fn new(config: SoakConfig) -> Result<Self, SoakConfigError> {
        config.validate()?;
        let mut result = Self {
            config,
            seats: Vec::new(),
            entities: Vec::new(),
            bodies: Vec::new(),
            ids: DetIdAllocator::new(config.entities),
            grid: DetBroadphase::new(BroadphaseConfig::default()),
            dt: 0.0,
            tick: 0,
            contacts: 0,
            recycled: 0,
        };
        result.reset(&result.default_run());
        Ok(result)
    }
    /// Live entity count, constant throughout a run.
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }
    /// Number of independently driven input seats.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }
    /// Total contacts actually resolved since reset.
    pub fn contacts(&self) -> u64 {
        self.contacts
    }
    /// Total freed and reissued handles since reset.
    pub fn recycled(&self) -> u64 {
        self.recycled
    }
    /// Current immutable settings.
    pub fn config(&self) -> SoakConfig {
        self.config
    }
    /// Read-only bodies for end-to-end geometry assertions.
    pub fn bodies(&self) -> &[BroadBody] {
        &self.bodies
    }
    /// Snapshot digest, with stable entity-ID order, outside the timed step.
    pub fn fingerprint(&self) -> u64 {
        let mut hash = StateHasher::new();
        self.hash_state(&mut hash);
        hash.finish()
    }
    /// Step with the all-pairs oracle. Its operation order matches the grid's.
    pub fn step_reference(&mut self, tick: u32) {
        self.advance(tick, true, &mut ());
    }
    /// Step with nested profiling events. The observer cannot mutate scenario state.
    pub fn step_observed(&mut self, tick: u32, observer: &mut impl SoakObserver) {
        self.advance(tick, false, observer);
    }
    fn advance(&mut self, tick: u32, reference: bool, observer: &mut impl SoakObserver) {
        observer.begin(SoakPass::Step);
        observer.begin(SoakPass::Input);
        for (i, seat) in self.seats.iter_mut().enumerate() {
            let angle = seat.phase + (tick as f32 * self.dt) * (0.3 + i as f32 * 0.002);
            seat.input = [math::cos(angle), math::sin(angle)];
        }
        observer.end(SoakPass::Input);
        observer.begin(SoakPass::Movement);
        let edge = self.config.half_extent - self.config.radius;
        for (entity, body) in self.entities.iter_mut().zip(&mut self.bodies) {
            let input = self.seats[entity.seat].input;
            for (axis, component) in [0, 2].into_iter().enumerate() {
                // A restoring drive keeps the crowd in contact instead of settling after warmup.
                let target = input[axis] * self.config.speed - body.pos[component] * 0.25;
                entity.velocity[axis] += (target - entity.velocity[axis]) * self.dt;
                body.pos[component] += entity.velocity[axis] * self.dt;
                if body.pos[component].abs() > edge {
                    body.pos[component] = det_clamp(body.pos[component], -edge, edge);
                    entity.velocity[axis] = -entity.velocity[axis];
                }
            }
        }
        observer.end(SoakPass::Movement);
        observer.begin(SoakPass::Recycling);
        if tick.is_multiple_of(self.config.recycle_interval) {
            // Free in seat order, then allocate in that order: exercise the whole FIFO batch.
            let start = (tick / self.config.recycle_interval) as usize % self.entities.len();
            for offset in 0..self.seats.len() {
                let i = (start + offset) % self.entities.len();
                assert!(
                    self.ids.free_handle(self.entities[i].handle),
                    "live scenario handle"
                );
            }
            for offset in 0..self.seats.len() {
                let i = (start + offset) % self.entities.len();
                // The just-freed batch guarantees availability, independent of user input.
                self.entities[i].handle = self.ids.alloc_handle().expect("freed batch has an ID");
                self.recycled += 1;
            }
        }
        observer.end(SoakPass::Recycling);
        observer.begin(SoakPass::Contacts);
        let contacts = &mut self.contacts;
        let resolve = |bodies: &mut [BroadBody], contact: Contact| {
            *contacts += 1;
            separate(bodies, contact);
        };
        if reference {
            for_each_contact_n2(&mut self.bodies, FloorRule::Bands, resolve);
        } else {
            self.grid.for_each_contact(&mut self.bodies, resolve);
        }
        observer.end(SoakPass::Contacts);
        self.tick = tick.wrapping_add(1);
        observer.end(SoakPass::Step);
    }
}

impl DetScenario for ScaleScenario {
    fn name(&self) -> &str {
        "scale_soak"
    }
    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0900_0040, 600, TickRate::default())
    }
    fn reset(&mut self, run: &DetRun) {
        self.dt = run.rate.dt();
        self.tick = 0;
        self.contacts = 0;
        self.recycled = 0;
        self.ids = DetIdAllocator::new(self.config.entities);
        self.grid = DetBroadphase::new(BroadphaseConfig::default());
        self.seats.clear();
        self.entities.clear();
        self.bodies.clear();
        let mut rng = SplitMix64::new(run.seed);
        for _ in 0..self.config.seats {
            self.seats.push(Seat {
                phase: rng.unit() * std::f32::consts::TAU,
                input: [0.0; 2],
            });
        }
        for i in 0..usize::from(self.config.entities) {
            let seat = i % self.seats.len();
            let angle = seat as f32 / self.seats.len() as f32 * std::f32::consts::TAU;
            let distance = self.config.half_extent * (0.15 + rng.unit() * 0.55);
            let x = math::cos(angle) * distance;
            let z = math::sin(angle) * distance;
            // Two outdoor stacked floors plus an underground floor; no game heights copied.
            let floor = (i / self.seats.len()) % 3;
            let y = floor as f32 * self.config.radius * 8.0;
            let mut body = BroadBody::new(
                [x, y, z],
                self.config.radius,
                [y - self.config.radius, y + self.config.radius],
            );
            body.underground = floor == 0;
            self.bodies.push(body);
            self.entities.push(Entity {
                handle: self.ids.alloc_handle().expect("validated entity cap"),
                seat,
                velocity: [0.0; 2],
            });
        }
    }
    fn step(&mut self, tick: u32) {
        self.advance(tick, false, &mut ());
    }
    fn hash_state(&self, h: &mut StateHasher) {
        h.u32("tick", self.tick);
        h.f32("dt", self.dt);
        h.u32("seats", u32::from(self.config.seats));
        h.u32("entities", u32::from(self.config.entities));
        h.f32("half_extent", self.config.half_extent);
        h.f32("radius", self.config.radius);
        h.f32("speed", self.config.speed);
        h.u32("recycle_interval", self.config.recycle_interval);
        h.u64("contacts", self.contacts);
        h.u64("recycled", self.recycled);
        for (i, seat) in self.seats.iter().enumerate() {
            h.item("seat", i as u32);
            h.f32("phase", seat.phase);
            h.f32("input_x", seat.input[0]);
            h.f32("input_z", seat.input[1]);
        }
        // Recycling frees/reissues in the same order, so the original ascending ID order stays.
        // The allocator is full and its FIFO empty at every snapshot; handles encode all
        // generations. Grid caches are rebuilt each tick and do not affect the logical state.
        for (entity, body) in self.entities.iter().zip(&self.bodies) {
            h.item("entity", u32::from(entity.handle.id));
            h.u32("generation", entity.handle.generation);
            h.u32("seat", entity.seat as u32);
            h.f32("x", body.pos[0]);
            h.f32("y", body.pos[1]);
            h.f32("z", body.pos[2]);
            h.f32("radius", body.radius);
            h.f32("band_lo", body.band[0]);
            h.f32("band_hi", body.band[1]);
            h.bool("underground", body.underground);
            h.f32("velocity_x", entity.velocity[0]);
            h.f32("velocity_z", entity.velocity[1]);
        }
    }
    fn flip_bit(&mut self, bit: u32) -> bool {
        if bit >= 32 {
            return false;
        }
        self.tick ^= 1_u32 << bit;
        true
    }
}

fn separate(bodies: &mut [BroadBody], c: Contact) {
    let distance = math::sqrt(c.dist_sq);
    let (nx, nz) = if distance > 0.0001 {
        (c.dx / distance, c.dz / distance)
    } else {
        (1.0, 0.0)
    };
    let half = (c.min_dist - distance) * 0.5;
    bodies[c.a].pos[0] -= nx * half;
    bodies[c.a].pos[2] -= nz * half;
    bodies[c.b].pos[0] += nx * half;
    bodies[c.b].pos[2] += nz * half;
}
