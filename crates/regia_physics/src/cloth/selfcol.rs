// SPDX-License-Identifier: MPL-2.0
// Source: M. Teschner, B. Heidelberger, M. Müller, D. Pomeranets, M. Gross, "Optimized Spatial
// Hashing for Collision Detection of Deformable Objects", VMV 2003 (uniform grid hashed with
// three large primes, cell size tied to the primitive size); M. Müller et al., "Position Based
// Dynamics", 2007 (pairwise distance projection split by inverse mass, position-level friction).
// Original code (spec 55 § 2.6).

//! Particle self-collision (`SelfCollision::Spheres`, spec 55 § 2.6).
//!
//! Once per substep a uniform grid (cell size `max(1.5 * reach, median rest edge)`) is built with a
//! counting sort of particles by hashed cell. Candidate pairs `(i, j)`, `i < j`, are collected
//! in ascending order; pairs within `exclude_rings` edge hops and pairs closer than `2r` at rest
//! are skipped. Each iteration pushes candidate pairs closer than `2r` apart along their
//! separation, split by inverse mass, with position-level friction. No hash-map order is
//! observable. At most 8 pairs per particle on average are kept per substep; the rest count
//! as dropped.

use regia_core::math::Vec3;

use super::{ClothParticle, EPSILON};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Candidate pushes in [`SelfCollider::broad_phase`]. `cargo test --lib` only.
#[cfg(test)]
pub(crate) static BROAD_PHASE_VISITS: AtomicUsize = AtomicUsize::new(0);

/// Pairs kept per particle on average per substep (`self_pairs_max`).
pub const SELF_PAIRS_PER_PARTICLE: usize = 8;

/// Candidate pairs are kept when closer than this factor times `2r` at the substep start, so
/// pairs that approach during the substep's iterations are still seen.
const CANDIDATE_MARGIN: f32 = 1.5;

/// Cell marker of a non-finite particle, which joins no bucket (ADR-3273).
const NO_CELL: [i32; 3] = [i32::MIN; 3];

#[derive(Debug, Clone, Default)]
pub(crate) struct SelfCollider {
    /// Exclusion lists in CSR form: particle `i`'s sorted neighbors within the ring count are
    /// `excluded[offsets[i]..offsets[i + 1]]`.
    offsets: Vec<u32>,
    excluded: Vec<u32>,
    /// Median positive rest length. Sizes the broad-phase cell so one long tether does not
    /// pull every particle into the same buckets. Exclusion rings stay a hop count.
    typical_edge: f32,
    /// The topology the exclusions were built for: (particles, edges, rings).
    built_for: Option<(usize, usize, u32)>,
    // Scratch, reused every substep.
    cell_of: Vec<[i32; 3]>,
    bucket_start: Vec<u32>,
    sorted: Vec<u32>,
    candidates: Vec<u32>,
    pub pairs: Vec<(u32, u32)>,
    /// Pairs over the cap, summed since the solver was made.
    pub dropped: u64,
    /// Pairs closer than `2r` after the last projection of the last substep (0 when settled).
    pub last_pairs: usize,
}

fn median_edge(edges: &[(usize, usize, f32)]) -> f32 {
    let mut lengths: Vec<f32> = edges
        .iter()
        .map(|edge| edge.2)
        .filter(|length| length.is_finite() && *length > 0.0)
        .collect();
    if lengths.is_empty() {
        return 0.0;
    }
    lengths.sort_by(f32::total_cmp);
    lengths[lengths.len() / 2]
}

pub(crate) fn cell_hash(c: [i32; 3], mask: usize) -> usize {
    let h =
        (c[0].wrapping_mul(92_837_111)) ^ (c[1].wrapping_mul(689_287_499)) ^ (c[2].wrapping_mul(283_923_481));
    (h as u32 as usize) & mask
}

impl SelfCollider {
    /// Whether the exclusion lists match this topology.
    pub fn is_built_for(&self, particles: usize, edges: usize, rings: u32) -> bool {
        self.built_for == Some((particles, edges, rings))
    }

    /// Marks the exclusion lists stale (topology changed).
    pub fn invalidate(&mut self) {
        self.built_for = None;
    }

    /// Rebuilds the exclusion lists from the edge list when the topology or the ring count
    /// changed. `edges` are `(a, b, rest)` triples.
    pub fn prepare(&mut self, particles: usize, edges: &[(usize, usize, f32)], rings: u32) {
        let key = (particles, edges.len(), rings);
        if self.built_for == Some(key) {
            return;
        }
        // Adjacency (sorted, deduplicated) in CSR form.
        let mut degree = vec![0u32; particles + 1];
        for &(a, b, _) in edges {
            degree[a] += 1;
            degree[b] += 1;
        }
        let mut start = vec![0u32; particles + 1];
        for i in 0..particles {
            start[i + 1] = start[i] + degree[i];
        }
        let mut fill = start.clone();
        let mut adjacency = vec![0u32; start[particles] as usize];
        for &(a, b, _) in edges {
            adjacency[fill[a] as usize] = b as u32;
            fill[a] += 1;
            adjacency[fill[b] as usize] = a as u32;
            fill[b] += 1;
        }
        self.typical_edge = median_edge(edges);
        // Breadth-first search to `rings` hops from each particle.
        self.offsets.clear();
        self.excluded.clear();
        self.offsets.push(0);
        let mut seen = vec![u32::MAX; particles];
        let mut frontier = Vec::new();
        let mut next = Vec::new();
        let mut found = Vec::new();
        for i in 0..particles {
            found.clear();
            frontier.clear();
            frontier.push(i as u32);
            seen[i] = i as u32;
            for _ in 0..rings {
                next.clear();
                for &f in &frontier {
                    let f = f as usize;
                    for &n in &adjacency[start[f] as usize..start[f + 1] as usize] {
                        if seen[n as usize] != i as u32 {
                            seen[n as usize] = i as u32;
                            next.push(n);
                            found.push(n);
                        }
                    }
                }
                std::mem::swap(&mut frontier, &mut next);
            }
            found.sort_unstable();
            self.excluded.extend_from_slice(&found);
            self.offsets.push(self.excluded.len() as u32);
        }
        self.built_for = Some(key);
    }

    /// Whether the exclusion lists were built for `particles` particles.
    pub fn covers(&self, particles: usize) -> bool {
        self.offsets.len() == particles + 1
    }

    /// Median positive rest length from the last [`Self::prepare`].
    pub fn typical_edge(&self) -> f32 {
        self.typical_edge
    }

    /// Whether `j` is within the ring count of `i`.
    pub fn is_excluded(&self, i: usize, j: u32) -> bool {
        let (a, b) = (self.offsets[i] as usize, self.offsets[i + 1] as usize);
        self.excluded[a..b].binary_search(&j).is_ok()
    }

    /// Builds the candidate pairs for this substep from the predicted positions. With
    /// `layers` (one per particle; empty for none) pairs on different layers are kept while
    /// closer than the margin times `2r + layer_gap`.
    pub fn broad_phase(
        &mut self,
        particles: &[ClothParticle],
        rest: &[Vec3],
        radius: f32,
        layers: &[u8],
        layer_gap: f32,
    ) {
        self.pairs.clear();
        let n = particles.len();
        if n < 2 || self.offsets.len() != n + 1 {
            return;
        }
        let contact = 2.0 * radius;
        // Layers widen the reach by the gap; without them the cell is the baseline's.
        let reach = if layers.is_empty() {
            contact
        } else {
            contact + layer_gap
        };
        // The ±1 stencil only sees pairs that can lie within one cell of each other, and a
        // kept candidate may be `CANDIDATE_MARGIN` times `reach` apart. The cell is at least
        // that distance. A single long edge used to set the cell and drop the hash to an
        // all-pairs scan; the median edge grows the cell only when the cloth itself is coarse.
        let cell = (reach * CANDIDATE_MARGIN).max(self.typical_edge).max(EPSILON);
        let inv_cell = 1.0 / cell;
        let table = (2 * n).next_power_of_two();
        let mask = table - 1;
        self.cell_of.clear();
        self.bucket_start.clear();
        self.bucket_start.resize(table + 1, 0);
        for p in particles {
            // A non-finite particle joins no cell (ADR-3273): `NaN as i32` is 0, so a blown-up
            // sheet would otherwise land in one cell and make the pair search quadratic.
            if !p.position.is_finite() {
                self.cell_of.push(NO_CELL);
                continue;
            }
            let c = (p.position * inv_cell).floor();
            let c = [c.x as i32, c.y as i32, c.z as i32];
            self.bucket_start[cell_hash(c, mask) + 1] += 1;
            self.cell_of.push(c);
        }
        for b in 0..table {
            self.bucket_start[b + 1] += self.bucket_start[b];
        }
        self.sorted.clear();
        self.sorted.resize(n, 0);
        // Counting sort: particles land in their bucket in ascending index order.
        {
            let mut fill: Vec<u32> = Vec::new();
            std::mem::swap(&mut fill, &mut self.candidates);
            fill.clear();
            fill.extend_from_slice(&self.bucket_start[..table]);
            for (i, c) in self.cell_of.iter().enumerate() {
                if *c == NO_CELL && !particles[i].position.is_finite() {
                    continue;
                }
                let b = cell_hash(*c, mask);
                self.sorted[fill[b] as usize] = i as u32;
                fill[b] += 1;
            }
            std::mem::swap(&mut fill, &mut self.candidates);
        }
        let cap = SELF_PAIRS_PER_PARTICLE * n;
        let keep = (reach * CANDIDATE_MARGIN).powi(2);
        let rest_min = contact * contact;
        for i in 0..n {
            self.candidates.clear();
            let c = self.cell_of[i];
            if c == NO_CELL && !particles[i].position.is_finite() {
                continue;
            }
            for dz in -1..=1 {
                for dy in -1..=1 {
                    for dx in -1..=1 {
                        let b = cell_hash(
                            [
                                c[0].wrapping_add(dx),
                                c[1].wrapping_add(dy),
                                c[2].wrapping_add(dz),
                            ],
                            mask,
                        );
                        let (s, e) = (self.bucket_start[b] as usize, self.bucket_start[b + 1] as usize);
                        for &j in &self.sorted[s..e] {
                            if j as usize > i {
                                self.candidates.push(j);
                            }
                        }
                    }
                }
            }
            self.candidates.sort_unstable();
            self.candidates.dedup();
            // Unique later particles in the stencil, not repeated bucket hits.
            #[cfg(test)]
            BROAD_PHASE_VISITS.fetch_add(self.candidates.len(), Ordering::Relaxed);
            for k in 0..self.candidates.len() {
                let j = self.candidates[k];
                let ju = j as usize;
                if particles[i].inv_mass == 0.0 && particles[ju].inv_mass == 0.0 {
                    continue;
                }
                if self.is_excluded(i, j) || rest[i].distance_squared(rest[ju]) < rest_min {
                    continue;
                }
                if particles[i].position.distance_squared(particles[ju].position) >= keep {
                    continue;
                }
                if self.pairs.len() < cap {
                    self.pairs.push((i as u32, j));
                } else {
                    self.dropped += 1;
                }
            }
        }
    }

    /// Pushes candidate pairs closer than `2r` apart, with position-level friction. A pair on
    /// two different `layers` keeps `2r + layer_gap` and only its outer particle moves (inner
    /// layers push outer layers, never the reverse).
    pub fn project(
        &mut self,
        particles: &mut [ClothParticle],
        previous: &[Vec3],
        radius: f32,
        friction: f32,
        layers: &[u8],
        layer_gap: f32,
    ) {
        let base_contact = 2.0 * radius;
        let mut touching = 0;
        for &(i, j) in &self.pairs {
            let (i, j) = (i as usize, j as usize);
            let (mut wi, mut wj) = (particles[i].inv_mass, particles[j].inv_mass);
            let mut contact = base_contact;
            if !layers.is_empty() {
                let li = layers.get(i).copied().unwrap_or(0);
                let lj = layers.get(j).copied().unwrap_or(0);
                if li != lj {
                    contact = base_contact + layer_gap;
                    if li < lj {
                        wi = 0.0;
                    } else {
                        wj = 0.0;
                    }
                }
            }
            let w = wi + wj;
            if w == 0.0 {
                continue;
            }
            let d = particles[i].position - particles[j].position;
            let len = d.length();
            if len >= contact {
                continue;
            }
            touching += 1;
            let n = if len > EPSILON { d / len } else { Vec3::Y };
            let push = contact - len;
            particles[i].position += n * (push * wi / w);
            particles[j].position -= n * (push * wj / w);
            if friction > 0.0 {
                let rel = (particles[i].position - previous[i]) - (particles[j].position - previous[j]);
                let tangent = rel - n * rel.dot(n);
                let t_len = tangent.length();
                if t_len > EPSILON {
                    let limit = friction * push;
                    let remove = if t_len <= limit {
                        tangent
                    } else {
                        tangent * (limit / t_len)
                    };
                    particles[i].position -= remove * (wi / w);
                    particles[j].position += remove * (wj / w);
                }
            }
        }
        self.last_pairs = touching;
    }

    /// Smallest current distance over candidate pairs (diagnostics and tests).
    #[must_use]
    pub fn min_pair_distance(&self, particles: &[ClothParticle]) -> Option<f32> {
        self.pairs
            .iter()
            .map(|&(i, j)| {
                particles[i as usize]
                    .position
                    .distance(particles[j as usize].position)
            })
            .reduce(f32::min)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use super::*;

    #[test]
    fn long_link_does_not_scan_every_particle() {
        let mut particles = Vec::new();
        let mut rest = Vec::new();
        let mut edges = Vec::new();
        for island in 0..2 {
            let base = island * 4;
            let origin = island as f32 * 20.0;
            for i in 0..4 {
                let p = Vec3::new(origin + i as f32 * 0.02, 0.0, 0.0);
                particles.push(ClothParticle {
                    position: p,
                    velocity: Vec3::ZERO,
                    inv_mass: 1.0,
                });
                rest.push(p);
                if i > 0 {
                    edges.push((base + i - 1, base + i, 0.02));
                }
            }
        }
        edges.push((3, 4, 20.0));
        let mut collider = SelfCollider::default();
        collider.prepare(particles.len(), &edges, 0);
        BROAD_PHASE_VISITS.store(0, Ordering::Relaxed);
        collider.broad_phase(&particles, &rest, 0.05, &[], 0.0);
        let visits = BROAD_PHASE_VISITS.load(Ordering::Relaxed);
        assert!(
            (1..=16).contains(&visits),
            "broad-phase visits {visits} (all pairs of 8 is 28)"
        );
        eprintln!("selfcol_unique_candidates {visits}");
        for &(i, j) in &collider.pairs {
            assert!((i < 4) == (j < 4), "a pair crossed the 20 m gap: {i}, {j}");
        }
    }
}
