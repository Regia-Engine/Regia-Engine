// SPDX-License-Identifier: MPL-2.0
//! Secondary particles: foam, spray and air bubbles (Ihmsen, Akinci, Becker and Teschner, "Unified
//! Spray, Foam and Air Bubbles for Particle-Based Fluids", Computer Graphics Forum 2012).
//!
//! Every liquid particle gets three potentials from its neighbours inside one cell radius `h`:
//! trapped air (relative velocity of approaching neighbours), wave crest (convex surface moving
//! along its normal) and kinetic energy. Each potential is clamped to `[0, 1]` between a minimum and
//! a maximum; the particle emits `I_k (k_ta I_ta + k_wc I_wc) dt` secondary particles (the fraction
//! by a seeded draw) in a disc around it, moving with it. A secondary particle's type comes from the
//! liquid neighbour count around it: few is spray (ballistic), many is a bubble (buoyant, dragged
//! toward the liquid velocity), between is foam (carried by the liquid surface velocity, dissolves
//! over its lifetime). Each type has a cap; a particle that would push a type past its cap is not
//! made, or is removed when it changes into a full type.
//!
//! The SI thresholds here are this crate's own (the paper's are tied to its scene scale).

#[cfg(test)]
use std::cell::Cell;

use glam::Vec3;

use crate::grid::{Fnv1a, Rng};
use crate::math::len;
use crate::sim::{Bins, FluidSim};
use crate::tally::Tally;

// Particles whose neighbourhood `normals` walked. Tests read it; production builds leave it at zero.
#[cfg(test)]
thread_local! {
    static NORMAL_PARTICLE_SCANS: Cell<usize> = const { Cell::new(0) };
}

/// The type of a secondary particle (the `FluidFrame` type tag uses the same numbers).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum WhitewaterKind {
    /// Floating on the surface, carried by it.
    Foam = 1,
    /// In the air, ballistic.
    Spray = 2,
    /// Under the surface, rising.
    Bubble = 3,
}

impl WhitewaterKind {
    /// From the tag number.
    #[must_use]
    pub fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(Self::Foam),
            2 => Some(Self::Spray),
            3 => Some(Self::Bubble),
            _ => None,
        }
    }

    /// Lowercase name.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Foam => "foam",
            Self::Spray => "spray",
            Self::Bubble => "bubble",
        }
    }
}

/// One secondary particle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WhitewaterParticle {
    /// Position.
    pub position: Vec3,
    /// Velocity.
    pub velocity: Vec3,
    /// Type.
    pub kind: WhitewaterKind,
    /// Seconds since it was made.
    pub age: f32,
    /// Seconds of foam life left.
    pub lifetime: f32,
}

/// Whitewater settings (every field has a default).
#[derive(Debug, Clone, PartialEq)]
pub struct WhitewaterParams {
    /// Whether secondary particles are made.
    pub enabled: bool,
    /// Most foam particles.
    pub max_foam: usize,
    /// Most spray particles.
    pub max_spray: usize,
    /// Most bubbles.
    pub max_bubbles: usize,
    /// Trapped-air potential clamp (m/s).
    pub trapped_air: [f32; 2],
    /// Wave-crest potential clamp (dimensionless curvature).
    pub wave_crest: [f32; 2],
    /// Kinetic potential clamp (J/kg, `0.5 |v|²`).
    pub kinetic: [f32; 2],
    /// Particles per second at full trapped-air potential.
    pub rate_trapped_air: f32,
    /// Particles per second at full wave-crest potential.
    pub rate_wave_crest: f32,
    /// Foam lifetime in seconds (each particle draws 0.5 to 1 times this).
    pub foam_lifetime: f32,
    /// Bubble buoyancy (times gravity, upward).
    pub buoyancy: f32,
    /// Bubble drag toward the liquid velocity (0..=1 per step).
    pub drag: f32,
    /// Fewer liquid neighbours than this: spray.
    pub spray_neighbours: u32,
    /// More liquid neighbours than this: bubble.
    pub bubble_neighbours: u32,
}

impl Default for WhitewaterParams {
    fn default() -> Self {
        Self {
            enabled: true,
            max_foam: 20_000,
            max_spray: 20_000,
            max_bubbles: 10_000,
            trapped_air: [0.6, 4.0],
            wave_crest: [0.2, 1.0],
            kinetic: [0.5, 8.0],
            rate_trapped_air: 40.0,
            rate_wave_crest: 60.0,
            foam_lifetime: 3.0,
            buoyancy: 2.0,
            drag: 0.5,
            spray_neighbours: 6,
            bubble_neighbours: 20,
        }
    }
}

/// Hard cap on any whitewater type.
pub const MAX_WHITEWATER_PER_KIND: usize = 1_000_000;

impl WhitewaterParams {
    /// Checks the ranges.
    pub fn validate(&self) -> Result<(), String> {
        let cap_ok = |c: usize| c <= MAX_WHITEWATER_PER_KIND;
        if !cap_ok(self.max_foam) || !cap_ok(self.max_spray) || !cap_ok(self.max_bubbles) {
            return Err(format!(
                "whitewater caps must be at most {MAX_WHITEWATER_PER_KIND}"
            ));
        }
        for (name, [lo, hi]) in [
            ("trapped_air", self.trapped_air),
            ("wave_crest", self.wave_crest),
            ("kinetic", self.kinetic),
        ] {
            if !(lo.is_finite() && hi.is_finite() && lo >= 0.0 && hi > lo) {
                return Err(format!("whitewater {name} needs 0 <= min < max"));
            }
        }
        let rate_ok = |r: f32| r.is_finite() && (0.0..=10_000.0).contains(&r);
        if !rate_ok(self.rate_trapped_air) || !rate_ok(self.rate_wave_crest) {
            return Err("whitewater rates must be in [0, 10000]".into());
        }
        if !(self.foam_lifetime.is_finite() && self.foam_lifetime > 0.0 && self.foam_lifetime <= 600.0) {
            return Err("foam_lifetime must be in (0, 600]".into());
        }
        if !(self.buoyancy.is_finite() && (0.0..=100.0).contains(&self.buoyancy)) {
            return Err("buoyancy must be in [0, 100]".into());
        }
        if !(self.drag.is_finite() && (0.0..=1.0).contains(&self.drag)) {
            return Err("drag must be in [0, 1]".into());
        }
        if self.spray_neighbours > self.bubble_neighbours {
            return Err("spray_neighbours must be at most bubble_neighbours".into());
        }
        Ok(())
    }

    fn cap(&self, kind: WhitewaterKind) -> usize {
        match kind {
            WhitewaterKind::Foam => self.max_foam,
            WhitewaterKind::Spray => self.max_spray,
            WhitewaterKind::Bubble => self.max_bubbles,
        }
    }
}

/// The secondary particles of one simulation.
#[derive(Debug, Clone, PartialEq)]
pub struct Whitewater {
    params: WhitewaterParams,
    particles: Vec<WhitewaterParticle>,
    rng: Rng,
    generated: usize,
    counts: [usize; 4],
    /// Neighbour visits inside the liquid-normal pass.
    normal_neighbor_visits: Tally,
}

impl Default for Whitewater {
    fn default() -> Self {
        Self::new(WhitewaterParams::default(), 0)
    }
}

/// The Ihmsen clamp `(min(I, max) - min(I, min)) / (max - min)`.
fn clamp_potential(value: f32, [lo, hi]: [f32; 2]) -> f32 {
    (value.min(hi) - value.min(lo)) / (hi - lo)
}

/// The paper's radially symmetric weight `1 - r / h` inside `h`.
fn weight(r: f32, h: f32) -> f32 {
    if r < h { 1.0 - r / h } else { 0.0 }
}

/// Relative speed of two liquid particles, on the software length. One ulp of platform `sqrt`
/// changes the trapped-air potential and the spawn count (ADR-4753).
fn approach_speed(v: Vec3) -> f32 {
    len(v)
}

/// `v / |v|` or zero, like `Vec3::normalize_or_zero` but on the software sqrt.
fn normalize_or_zero(v: Vec3) -> Vec3 {
    let l = len(v);
    if l > 0.0 { v / l } else { Vec3::ZERO }
}

/// The wave-crest gate: whether the liquid particle moves out along its surface normal. Compared
/// on the squares in f64, so no `sqrt` decides it: the gate spawns foam and consumes `Rng`
/// draws, so one ulp of platform `sqrt` would fork a run (ADR-4753).
fn moves_out_along_normal(vi: Vec3, ni: Vec3) -> bool {
    if ni == Vec3::ZERO || vi == Vec3::ZERO {
        return false;
    }
    let along = f64::from(vi.dot(ni));
    if along <= 0.0 {
        return false;
    }
    // `along / |vi| >= 0.6` on the squares: `vi` is not zero here, so this is exact monotone
    // comparison of non-negative f64 values built from the same f32 bits everywhere.
    along * along >= 0.36 * f64::from(vi.length_squared())
}

impl Whitewater {
    /// No particles yet.
    #[must_use]
    pub fn new(params: WhitewaterParams, seed: u64) -> Self {
        Self {
            params,
            particles: Vec::new(),
            rng: Rng::new(seed ^ 0x005e_ed0f_f0a3),
            generated: 0,
            counts: [0; 4],
            normal_neighbor_visits: Tally::default(),
        }
    }

    /// Neighbour slots the liquid-normal pass walked (each liquid particle in a 3³ cell block).
    #[must_use]
    pub fn normal_neighbor_visits(&self) -> u64 {
        self.normal_neighbor_visits.get()
    }

    /// The particles, in creation order.
    #[must_use]
    pub fn particles(&self) -> &[WhitewaterParticle] {
        &self.particles
    }

    /// Counts by tag: `[_, foam, spray, bubble]`.
    #[must_use]
    pub fn counts(&self) -> [usize; 4] {
        self.counts
    }

    /// Particles made in the last step.
    #[must_use]
    pub fn generated_last_step(&self) -> usize {
        self.generated
    }

    /// The settings.
    #[must_use]
    pub fn params(&self) -> &WhitewaterParams {
        &self.params
    }

    pub(crate) fn hash_into(&self, h: &mut Fnv1a) {
        // The generator state: a sim that drew random numbers and then lost every particle must
        // not hash equal to one that never drew (ADR-4753).
        h.write(&self.rng.state().to_le_bytes());
        h.write(&(self.particles.len() as u64).to_le_bytes());
        for p in &self.particles {
            for x in p.position.to_array().into_iter().chain(p.velocity.to_array()) {
                h.write_f32(x);
            }
            h.write(&[p.kind as u8]);
            h.write_f32(p.age);
            h.write_f32(p.lifetime);
        }
    }

    fn recount(&mut self) {
        let mut counts = [0usize; 4];
        for p in &self.particles {
            counts[p.kind as usize] += 1;
        }
        self.counts = counts;
    }

    /// Liquid neighbours of `x` within `h`, and their weighted mean velocity.
    fn neighbourhood(sim: &FluidSim, bins: &Bins, x: Vec3, h: f32) -> (u32, Vec3) {
        let d = sim.dims();
        let (ci, cj, ck) = sim.cell_coords_clamped(x);
        let mut count = 0u32;
        let mut vsum = Vec3::ZERO;
        let mut wsum = 0.0f32;
        let pos = sim.positions();
        let vel = sim.velocities();
        for k in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
            for j in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                for i in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                    for &p in bins.cell(d.cell(i, j, k)) {
                        let r = len(pos[p as usize] - x);
                        let w = weight(r, h);
                        if w > 0.0 {
                            count += 1;
                            vsum += vel[p as usize] * w;
                            wsum += w;
                        }
                    }
                }
            }
        }
        let v = if wsum > 0.0 {
            vsum / wsum
        } else {
            sim.grid_velocity(x)
        };
        (count, v)
    }

    fn classify(&self, neighbours: u32) -> WhitewaterKind {
        if neighbours < self.params.spray_neighbours {
            WhitewaterKind::Spray
        } else if neighbours > self.params.bubble_neighbours {
            WhitewaterKind::Bubble
        } else {
            WhitewaterKind::Foam
        }
    }

    /// Advances the existing particles, then makes new ones from the liquid's potentials.
    pub(crate) fn step(&mut self, sim: &FluidSim, bins: &Bins, dt: f32) {
        self.generated = 0;
        if !self.params.enabled {
            if !self.particles.is_empty() {
                self.particles.clear();
                self.recount();
            }
            return;
        }
        let d = *sim.dims();
        let h = d.dx;
        let gravity = sim.params().gravity + sim.params().external_acceleration;
        let lo = d.origin;
        let hi = d.max();
        // 1. Advance.
        let mut kept = Vec::with_capacity(self.particles.len());
        let mut counts = [0usize; 4];
        let old = std::mem::take(&mut self.particles);
        for mut p in old {
            let (n, v_fluid) = Self::neighbourhood(sim, bins, p.position, h);
            let kind = self.classify(n);
            match kind {
                WhitewaterKind::Spray => {
                    p.velocity += gravity * dt;
                    p.position += p.velocity * dt;
                }
                WhitewaterKind::Foam => {
                    p.velocity = v_fluid;
                    p.position += v_fluid * dt;
                    p.lifetime -= dt;
                }
                WhitewaterKind::Bubble => {
                    p.velocity +=
                        -gravity * (self.params.buoyancy * dt) + (v_fluid - p.velocity) * self.params.drag;
                    p.position += p.velocity * dt;
                }
            }
            p.kind = kind;
            p.age += dt;
            let inside = p.position.cmpge(lo).all() && p.position.cmple(hi).all() && p.position.is_finite();
            let in_solid = sim.solid().distance_near(p.position, 0.0) < 0.0;
            if !inside || in_solid || p.lifetime <= 0.0 || counts[kind as usize] >= self.params.cap(kind) {
                continue;
            }
            counts[kind as usize] += 1;
            kept.push(p);
        }
        self.particles = kept;
        self.counts = counts;
        // 2. Generate.
        let pos = sim.positions();
        let vel = sim.velocities();
        if pos.is_empty() {
            return;
        }
        // Emission draws from the generator only after `amount > 0`. That needs a positive rate
        // and a kinetic potential above the floor, so a calm liquid (or rates of zero) leaves
        // the generator and skips the neighbour scan. Crest normals are read only when the
        // wave-crest rate is positive; trapped air does not use them.
        let can_emit = (self.params.rate_trapped_air > 0.0 || self.params.rate_wave_crest > 0.0)
            && vel.iter().any(|v| {
                let kinetic = clamp_potential(0.5 * v.length_squared(), self.params.kinetic);
                // Same particles the loop keeps: `kinetic <= 0` continues, and a NaN does not.
                kinetic > 0.0 || kinetic.is_nan()
            });
        if !can_emit {
            return;
        }
        let normals = if self.params.rate_wave_crest > 0.0 {
            // Kinetic potential is zero at or below the floor, so only a fast particle needs its
            // surface normal. Crest reads the normals of neighbours, so a fast particle also needs
            // the normals in the cells within Chebyshev distance 1.
            let kinetic_lo = self.params.kinetic[0];
            let mut want = vec![false; d.cells()];
            for (xi, vi) in pos.iter().zip(vel) {
                if 0.5 * vi.length_squared() <= kinetic_lo {
                    continue;
                }
                let (ci, cj, ck) = sim.cell_coords_clamped(*xi);
                for k in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
                    for j in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                        for i in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                            want[d.cell(i, j, k)] = true;
                        }
                    }
                }
            }
            Self::normals(&self.normal_neighbor_visits, sim, bins, h, &want)
        } else {
            Vec::new()
        };
        let p = self.params.clone();
        let radius = h * 0.25;
        for i in 0..pos.len() {
            let (xi, vi) = (pos[i], vel[i]);
            let kinetic = clamp_potential(0.5 * vi.length_squared(), p.kinetic);
            if kinetic <= 0.0 {
                continue;
            }
            let (ci, cj, ck) = sim.cell_coords_clamped(xi);
            let mut trapped = 0.0f32;
            let mut crest = 0.0f32;
            let ni = if normals.is_empty() {
                Vec3::ZERO
            } else {
                normals[i]
            };
            for k in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
                for j in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                    for ii in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                        for &q in bins.cell(d.cell(ii, j, k)) {
                            let q = q as usize;
                            if q == i {
                                continue;
                            }
                            let x_ij = xi - pos[q];
                            let r = len(x_ij);
                            let w = weight(r, h);
                            if w <= 0.0 || r <= 0.0 {
                                continue;
                            }
                            let v_ij = vi - vel[q];
                            let speed = approach_speed(v_ij);
                            if speed > 0.0 {
                                trapped += speed * (1.0 - (v_ij / speed).dot(x_ij / r)) * w;
                            }
                            if !normals.is_empty() && ni != Vec3::ZERO && (-x_ij / r).dot(ni) < 0.0 {
                                crest += (1.0 - ni.dot(normals[q])) * w;
                            }
                        }
                    }
                }
            }
            let moving_out = moves_out_along_normal(vi, ni);
            let i_ta = clamp_potential(trapped, p.trapped_air);
            let i_wc = if moving_out {
                clamp_potential(crest, p.wave_crest)
            } else {
                0.0
            };
            let amount = kinetic * (p.rate_trapped_air * i_ta + p.rate_wave_crest * i_wc) * dt;
            if amount <= 0.0 {
                continue;
            }
            let draw = self.rng.unit();
            let count = (amount + draw).floor() as usize;
            if count == 0 {
                continue;
            }
            let speed = len(vi);
            let axis = vi / speed;
            let helper = if axis.x.abs() < 0.9 { Vec3::X } else { Vec3::Y };
            let e1 = normalize_or_zero(axis.cross(helper));
            let e2 = axis.cross(e1);
            for _ in 0..count.min(64) {
                // A point in the unit disc by rejection (bounded, no trigonometry).
                let (mut a, mut b) = (0.0f32, 0.0f32);
                for _ in 0..8 {
                    let (x, y) = (self.rng.signed(), self.rng.signed());
                    if x * x + y * y <= 1.0 {
                        (a, b) = (x, y);
                        break;
                    }
                }
                let along = self.rng.unit() * speed * dt;
                // Inside the closed domain, like every other particle.
                let position = (xi + (e1 * a + e2 * b) * radius + axis * along).clamp(lo, hi);
                // The paper adds the disc offset to the velocity; here it moves with the liquid
                // particle, and the type's own motion takes over from the next step.
                let velocity = vi;
                let (n, _) = Self::neighbourhood(sim, bins, position, h);
                let kind = self.classify(n);
                if self.counts[kind as usize] >= p.cap(kind) {
                    continue;
                }
                let lifetime = p.foam_lifetime * (0.5 + 0.5 * self.rng.unit());
                self.particles.push(WhitewaterParticle {
                    position,
                    velocity,
                    kind,
                    age: 0.0,
                    lifetime,
                });
                self.counts[kind as usize] += 1;
                self.generated += 1;
            }
        }
    }

    /// Surface normals of the liquid particles: the normalised weighted sum of `x_i - x_j`, zero
    /// where the neighbourhood is balanced (the interior).
    fn normals(visits: &Tally, sim: &FluidSim, bins: &Bins, h: f32, want: &[bool]) -> Vec<Vec3> {
        let d = sim.dims();
        let pos = sim.positions();
        #[cfg(test)]
        NORMAL_PARTICLE_SCANS.with(|n| n.set(n.get() + pos.len()));
        let mut out = vec![Vec3::ZERO; pos.len()];
        // Counted locally and added once: one relaxed atomic per neighbour visit showed up next
        // to the kernel itself.
        let mut counted = 0u64;
        for (i, xi) in pos.iter().enumerate() {
            let (ci, cj, ck) = sim.cell_coords_clamped(*xi);
            if !want[d.cell(ci, cj, ck)] {
                continue;
            }
            let mut sum = Vec3::ZERO;
            let mut wsum = 0.0f32;
            for k in ck.saturating_sub(1)..=(ck + 1).min(d.nz - 1) {
                for j in cj.saturating_sub(1)..=(cj + 1).min(d.ny - 1) {
                    for ii in ci.saturating_sub(1)..=(ci + 1).min(d.nx - 1) {
                        for &q in bins.cell(d.cell(ii, j, k)) {
                            counted += 1;
                            if q as usize == i {
                                continue;
                            }
                            let x_ij = *xi - pos[q as usize];
                            let w = weight(len(x_ij), h);
                            sum += x_ij * w;
                            wsum += w;
                        }
                    }
                }
            }
            // A surface particle's neighbours sit on one side: the mean offset is a sizeable share of h.
            if wsum > 0.0 && len(sum / wsum) > 0.2 * h {
                out[i] = normalize_or_zero(sum);
            }
        }
        visits.add(counted);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One ulp up or down (the neighbours a platform `sqrt` could have rounded a boundary to).
    fn ulp(x: f32, dir: i32) -> f32 {
        f32::from_bits((x.to_bits() as i64 + i64::from(dir)) as u32)
    }

    #[test]
    fn crest_gate_pins_the_boundary() {
        // The 3-4-5 vector sits exactly on the 0.6 cosine; the gate must classify it and its
        // one-ulp neighbours deterministically (ADR-4753). Compared in f64 on the squares, the
        // results are stable across platforms by construction; the pins guard the construction.
        let up = Vec3::Y;
        for (v, want) in [
            (Vec3::new(0.0, 3.0, 4.0), true),
            (Vec3::new(0.0, 3.0, ulp(4.0, -1)), true),
            (Vec3::new(0.0, 3.0, ulp(4.0, 1)), false),
            (Vec3::new(0.0, ulp(3.0, -1), 4.0), false),
            (Vec3::new(0.0, ulp(3.0, 1), 4.0), true),
            (Vec3::new(0.0, -3.0, 4.0), false),
            (Vec3::new(0.0, 0.0, 4.0), false),
            (Vec3::ZERO, false),
        ] {
            assert_eq!(moves_out_along_normal(v, up), want, "{v:?}");
            assert!(!moves_out_along_normal(v, Vec3::ZERO));
        }
    }

    #[test]
    fn software_length_matches_the_platform_sqrt() {
        let mut worst = 0.0f64;
        let mut state = 0x2545_f491_4f6c_dd1du64;
        for _ in 0..20_000 {
            state ^= state >> 12;
            state ^= state << 25;
            state ^= state >> 27;
            let bits = state.wrapping_mul(0x2545_f491_4f6c_dd1d);
            // Random components across the normal range, signs included. Exponents -63..=63
            // keep `length_squared` inside the normal f32 range on both sides (the software
            // sqrt's documented domain), so the platform reference is meaningful.
            let component = |shift: u32, exponent_bits: u64| {
                let exponent = 64 + (exponent_bits % 126) as u32;
                f32::from_bits(
                    ((bits >> shift) as u32 & 0x007f_ffff) | (exponent << 23) | (((bits >> 63) as u32) << 31),
                )
            };
            let v = Vec3::new(
                component(9, bits >> 32),
                component(41, bits >> 40),
                component(20, bits >> 24),
            );
            let want = f64::from(v.length());
            let got = f64::from(len(v));
            worst = worst.max((got - want).abs() / want);
        }
        assert!(worst <= 1.5e-7, "len worst relative error {worst}");
    }

    /// Trapped-air speed must use the software length. These bits are a one-ulp split on the lane
    /// host: `Vec3::length` (platform `sqrt`) and `len` disagree, and the spawn count follows
    /// whichever one `approach_speed` returns (ADR-4753).
    #[test]
    fn approach_speed_is_the_software_length() {
        let v = Vec3::new(
            f32::from_bits(0x3ffe_2bfb),
            f32::from_bits(0x3ec0_6920),
            f32::from_bits(0x3fe1_569e),
        );
        assert_ne!(
            v.length(),
            len(v),
            "this host's libm and the software sqrt differ here"
        );
        assert_eq!(approach_speed(v), len(v));
    }

    fn still_tank() -> crate::sim::FluidSim {
        let params = crate::sim::FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.2),
            cell_size: 0.05,
            gravity: Vec3::ZERO,
            external_acceleration: Vec3::ZERO,
            separation: 0.0,
            reseed: false,
            ..crate::sim::FluidParams::default()
        };
        let mut sim = crate::sim::FluidSim::new(params).expect("tank");
        let particles: Vec<(Vec3, Vec3)> = (0..4)
            .map(|i| (Vec3::new(0.04 + i as f32 * 0.03, 0.1, 0.1), Vec3::ZERO))
            .collect();
        assert_eq!(sim.add_particles(&particles), 4);
        sim
    }

    /// Calm liquid (every speed under the kinetic floor) must not walk neighbour normals: the
    /// default whitewater rates would emit nothing, and the normal pass is the generation scan.
    #[test]
    fn calm_liquid_does_not_scan_normals() {
        let sim = still_tank();
        let bins = sim.bin();
        let mut ww = Whitewater::new(WhitewaterParams::default(), 1);
        NORMAL_PARTICLE_SCANS.with(|n| n.set(0));
        ww.step(&sim, &bins, 1.0 / 60.0);
        let scans = NORMAL_PARTICLE_SCANS.with(|n| n.get());
        assert_eq!(scans, 0, "normal pass walked {scans} particles");
        assert_eq!(ww.generated_last_step(), 0);
    }

    /// A particle above the kinetic floor still walks every liquid particle: crest emission
    /// reads the neighbour normals, not only the emitter's.
    #[test]
    fn a_fast_particle_still_scans_every_normal() {
        let mut sim = still_tank();
        let mut vel = sim.velocities().to_vec();
        vel[0] = Vec3::new(2.0, 0.0, 0.0);
        sim.set_particles(sim.positions().to_vec(), vel);
        let bins = sim.bin();
        let mut ww = Whitewater::new(WhitewaterParams::default(), 1);
        NORMAL_PARTICLE_SCANS.with(|n| n.set(0));
        ww.step(&sim, &bins, 1.0 / 60.0);
        let scans = NORMAL_PARTICLE_SCANS.with(|n| n.get());
        assert_eq!(
            scans,
            sim.positions().len(),
            "normal pass walked {scans} particles"
        );
    }

    /// Trapped-air emission with the crest rate at zero must not build surface normals.
    #[test]
    fn trapped_air_without_crest_skips_normals() {
        let params = crate::sim::FluidParams {
            bounds_min: Vec3::ZERO,
            bounds_max: Vec3::splat(0.2),
            cell_size: 0.05,
            gravity: Vec3::ZERO,
            external_acceleration: Vec3::ZERO,
            separation: 0.0,
            reseed: false,
            ..crate::sim::FluidParams::default()
        };
        let mut sim = crate::sim::FluidSim::new(params).expect("tank");
        let particles = [
            (Vec3::new(0.08, 0.1, 0.1), Vec3::new(4.0, 0.0, 0.0)),
            (Vec3::new(0.11, 0.1, 0.1), Vec3::new(-4.0, 0.0, 0.0)),
        ];
        assert_eq!(sim.add_particles(&particles), 2);
        let bins = sim.bin();
        let ww_params = WhitewaterParams {
            rate_wave_crest: 0.0,
            rate_trapped_air: 1_000.0,
            ..WhitewaterParams::default()
        };
        let mut ww = Whitewater::new(ww_params, 1);
        NORMAL_PARTICLE_SCANS.with(|n| n.set(0));
        ww.step(&sim, &bins, 1.0 / 60.0);
        let scans = NORMAL_PARTICLE_SCANS.with(|n| n.get());
        assert_eq!(scans, 0, "normal pass walked {scans} particles");
        assert!(ww.generated_last_step() > 0, "trapped air emitted nothing");
    }
}
