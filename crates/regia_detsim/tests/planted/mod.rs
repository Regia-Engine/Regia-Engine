// SPDX-License-Identifier: MPL-2.0
//! The planted cross-architecture divergence of the runners (ADR-8081, plan row F-03).
//!
//! [`PlantedSimd`] breaks the lockstep rules on purpose: its bodies are glam `Vec3A` and `Vec4`
//! values stepped with glam's SIMD operations, the very code ADR-8012 A-4 bans from lockstep
//! crates. glam picks SSE2 on x86-64 and NEON on aarch64, and those two do not round alike
//! (ADR-5176):
//!
//! - `Vec3A::cross` is two products and a subtraction on SSE2, but a product and one fused
//!   multiply-subtract (`vfmsq_f32`, one rounding) on NEON;
//! - `Vec4::dot` sums the lane products as `(x + z) + (y + w)` on SSE2 and as `(x + y) + (z + w)`
//!   on NEON (`vaddvq_f32`).
//!
//! `Vec3A::dot` on its own is no plant: glam 0.33 computes `(x * x' + y * y') + z * z'` on both
//! backends (`vec3a_dot_has_the_scalar_order_on_every_target` proves it per target). The
//! scenario uses it anyway, beside the two operations that do differ.
//!
//! Its golden keeps only the x86-64 Linux hash and trace. The runners must report it as differing
//! on every aarch64 leg, with a first diverging tick, and as equal on every x86-64 leg; a leg
//! that reports it equal on aarch64 is not running what it claims to (glam's scalar path, or an
//! x86-64 binary). [`probe`] counts, per operation, how many seeded inputs leave the scalar order.

use glam::{Quat, Vec3A, Vec4};
use regia_detsim::golden::scenarios::SplitMix64;
use regia_detsim::golden::{DetRun, DetScenario, StateHasher};
use regia_detsim::math;
use regia_detsim::tick::TickRate;

/// The planted scenario's name (its golden is `planted_simd_vec3a.detgold`).
pub const NAME: &str = "planted_simd_vec3a";

const BODIES: usize = 12;
const ARENA: f32 = 40.0;

/// Whether this build's glam runs a SIMD backend. glam's scalar path (the `scalar-math` feature,
/// ADR-5176) gives `Quat` 4-byte alignment; SSE2, NEON and wasm SIMD give it 16.
pub fn glam_uses_simd() -> bool {
    core::mem::align_of::<Quat>() == 16
}

#[derive(Debug, Clone, Copy)]
struct Body {
    pos: Vec3A,
    vel: Vec3A,
    goal: Vec3A,
    align: f32,
    energy: f32,
}

/// Bodies steering towards seeded goals with glam SIMD math (see the module docs).
#[derive(Debug, Clone)]
pub struct PlantedSimd {
    bodies: Vec<Body>,
    rng: SplitMix64,
    dt: f32,
}

impl PlantedSimd {
    /// An empty scenario; [`DetScenario::reset`] fills it.
    pub fn new() -> Self {
        PlantedSimd {
            bodies: Vec::new(),
            rng: SplitMix64::new(0),
            dt: 0.0,
        }
    }

    fn goal(rng: &mut SplitMix64) -> Vec3A {
        Vec3A::new(rng.signed() * ARENA, rng.signed() * 8.0, rng.signed() * ARENA)
    }
}

impl DetScenario for PlantedSimd {
    fn name(&self) -> &str {
        NAME
    }

    fn planted(&self) -> bool {
        true
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_f003, 600, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        self.rng = SplitMix64::new(run.seed);
        self.dt = run.rate.dt();
        self.bodies.clear();
        for _ in 0..BODIES {
            let pos = Self::goal(&mut self.rng);
            let vel = Vec3A::new(self.rng.signed(), self.rng.signed(), self.rng.signed()) * 3.0;
            let goal = Self::goal(&mut self.rng);
            self.bodies.push(Body {
                pos,
                vel,
                goal,
                align: 0.0,
                energy: 0.0,
            });
        }
    }

    fn step(&mut self, tick: u32) {
        let dt = self.dt;
        for (i, b) in self.bodies.iter_mut().enumerate() {
            let to_goal = b.goal - b.pos;
            // Same bits on SSE2 and NEON.
            b.align = b.vel.dot(to_goal);
            // Fused on NEON, not on SSE2.
            let swirl = b.vel.cross(to_goal);
            // Lane sums grouped differently on SSE2 and NEON.
            let v4 = Vec4::new(b.vel.x, b.vel.y, b.vel.z, b.align * 0.01);
            b.energy = v4.dot(v4);
            let dist2 = to_goal.dot(to_goal) + 1.0;
            let pull = to_goal * (6.0 / math::sqrt(dist2));
            let damp = b.vel * (0.2 + b.energy * 0.0005);
            b.vel += (pull + swirl * 0.004 - damp) * dt;
            b.pos += b.vel * dt;
            if dist2 < 4.0 || (tick + i as u32).is_multiple_of(240) {
                b.goal = Self::goal(&mut self.rng);
            }
        }
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (i, b) in self.bodies.iter().enumerate() {
            h.item("body", i as u32);
            h.f32("pos_x", b.pos.x);
            h.f32("pos_y", b.pos.y);
            h.f32("pos_z", b.pos.z);
            h.f32("vel_x", b.vel.x);
            h.f32("vel_y", b.vel.y);
            h.f32("vel_z", b.vel.z);
            h.f32("align", b.align);
            h.f32("energy", b.energy);
        }
    }
}

/// Per glam operation, how many of `samples` seeded inputs give other bits than the scalar
/// order written out by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Probe {
    /// Inputs per operation.
    pub samples: u32,
    /// `Vec3A::dot` against `(x * x' + y * y') + z * z'`.
    pub vec3a_dot: u32,
    /// `Vec3A::cross` against two products and a subtraction per lane.
    pub vec3a_cross: u32,
    /// `Vec4::dot` against `(x * x' + y * y') + (z * z' + w * w')` (the NEON grouping).
    pub vec4_dot_xy_zw: u32,
    /// `Vec4::dot` against `(x * x' + z * z') + (y * y' + w * w')` (the SSE2 grouping).
    pub vec4_dot_xz_yw: u32,
}

impl Probe {
    /// The probe as one line of JSON for the runners (`regia-det-probe {json}`).
    pub fn to_json(self, target: &str) -> String {
        format!(
            "{{\"target\":\"{target}\",\"glam_simd\":{},\"samples\":{},\"vec3a_dot\":{},\"vec3a_cross\":{},\
             \"vec4_dot_xy_zw\":{},\"vec4_dot_xz_yw\":{}}}",
            glam_uses_simd(),
            self.samples,
            self.vec3a_dot,
            self.vec3a_cross,
            self.vec4_dot_xy_zw,
            self.vec4_dot_xz_yw
        )
    }
}

fn differs(a: f32, b: f32) -> bool {
    regia_detsim::golden::canonical_f32_bits(a) != regia_detsim::golden::canonical_f32_bits(b)
}

/// Run every probed operation on `samples` seeded inputs.
pub fn probe(samples: u32) -> Probe {
    let mut rng = SplitMix64::new(0x5eed_9b0b);
    let mut p = Probe {
        samples,
        vec3a_dot: 0,
        vec3a_cross: 0,
        vec4_dot_xy_zw: 0,
        vec4_dot_xz_yw: 0,
    };
    let mut v = || rng.signed() * 100.0;
    for _ in 0..samples {
        let (a, b) = (Vec4::new(v(), v(), v(), v()), Vec4::new(v(), v(), v(), v()));
        let (a3, b3) = (Vec3A::new(a.x, a.y, a.z), Vec3A::new(b.x, b.y, b.z));

        let dot3 = (a.x * b.x + a.y * b.y) + a.z * b.z;
        p.vec3a_dot += u32::from(differs(a3.dot(b3), dot3));

        let c = a3.cross(b3);
        let cx = a.y * b.z - a.z * b.y;
        let cy = a.z * b.x - a.x * b.z;
        let cz = a.x * b.y - a.y * b.x;
        p.vec3a_cross += u32::from(differs(c.x, cx) || differs(c.y, cy) || differs(c.z, cz));

        let d4 = a.dot(b);
        let xy_zw = (a.x * b.x + a.y * b.y) + (a.z * b.z + a.w * b.w);
        let xz_yw = (a.x * b.x + a.z * b.z) + (a.y * b.y + a.w * b.w);
        p.vec4_dot_xy_zw += u32::from(differs(d4, xy_zw));
        p.vec4_dot_xz_yw += u32::from(differs(d4, xz_yw));
    }
    p
}
