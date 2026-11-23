// SPDX-License-Identifier: MPL-2.0
// Source: R. Bridson, R. Fedkiw, J. Anderson, "Robust Treatment of Collisions, Contact and
// Friction for Cloth Animation", SIGGRAPH 2002 (§ 6.1: point-triangle proximity against a
// cloth thickness, the repulsion spread over the triangle's vertices by barycentric weights
// with the `1 + sum(w_i^2)` style denominator, friction limited by the normal correction);
// M. Teschner, B. Heidelberger, M. Müller, D. Pomeranets, M. Gross, "Optimized Spatial Hashing
// for Collision Detection of Deformable Objects", VMV 2003 (triangles entered into every cell
// their bounding box covers, points looked up in their own cells, three-prime hash); C. Ericson,
// "Real-Time Collision Detection", Morgan Kaufmann 2004 (§ 3.4 barycentric coordinates,
// § 5.1.5 closest point on a triangle). Position-level projection in the style of Müller et al.,
// "Position Based Dynamics", 2007. Original code (spec 55 § 2.6, ADR-2425).

//! Vertex-triangle self-collision (`SelfCollision::VertexTriangle`, spec 55 § 2.6) and the
//! one-sided rule for garment layers.
//!
//! Once per substep every visible sim triangle is entered into a hashed uniform grid in every
//! cell its bounding box covers (the box spans the triangle's substep-start and predicted
//! positions, grown by the contact distance), with a counting sort so each bucket lists its
//! triangles in ascending index. Each particle looks up the cells its own substep path covers and
//! keeps the triangles it may touch: not one of its own, no vertex within `exclude_rings` edge
//! hops, not closer than the contact distance at rest, inside the triangle's grown box, inside
//! its prism and closer than the margin on the side it keeps. That side is the one the particle
//! was on at the start of the fixed step, so a particle that crosses a triangle within a step is
//! pulled back (folds and layered garments).
//!
//! Each iteration projects the inequality `side * n·(x - x_a) >= contact` over the vertex and the
//! three triangle vertices by inverse mass and barycentric weight, with position-level friction.
//!
//! **Layers.** With per-particle layers, a pair whose vertex and triangle are on different layers
//! keeps an extra `layer_gap`, and only the outer one moves: the inner side acts as if it had
//! infinite mass, so inner layers push outer layers and never the reverse. A triangle's layer is
//! the lowest layer of its vertices.
//!
//! The candidate set depends only on geometry (the grid only prunes), pairs are visited by
//! ascending `(vertex, triangle)`, and no hash-map order is observable.

use regia_core::math::Vec3;

use super::selfcol::{SELF_PAIRS_PER_PARTICLE, SelfCollider, cell_hash};
use super::{ClothParticle, EPSILON};

#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};

/// Triangles whose grown box passed the particle test. `cargo test --lib` only.
#[cfg(test)]
static VT_CANDIDATES: AtomicUsize = AtomicUsize::new(0);

/// Candidates are kept when closer than this factor times the contact distance at the substep
/// start, so pairs that approach during the substep's iterations are still seen.
const CANDIDATE_MARGIN: f32 = 1.5;

/// Barycentric slack of the prism test, so a particle over a shared edge belongs to both sides.
const PRISM_TOLERANCE: f32 = 1e-3;

/// A pair this close (as a factor of its contact distance) after a substep counts as resting in
/// contact for the inelastic velocity pass.
const CONTACT_BAND: f32 = 1.05;

/// Most grid cells per axis one triangle box or particle path covers (the rest is clamped).
const MAX_SPAN: i32 = 4;

/// One vertex-triangle contact candidate of a substep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct VtPair {
    /// The particle.
    pub vertex: u32,
    /// The sim triangle (index into the solver's triangle list).
    pub triangle: u32,
    /// `+1` or `-1`: the side of the triangle's plane the particle keeps.
    pub side: f32,
    /// Contact distance: the radius, plus the layer gap across layers.
    pub contact: f32,
    /// Inverse-mass factor of the particle (0 when it is on an inner layer).
    pub vertex_scale: f32,
    /// Inverse-mass factor of the triangle's vertices (0 when the triangle is on an inner layer).
    pub triangle_scale: f32,
    /// Whether a projection pushed this pair during the running substep.
    pub touched: bool,
}

/// Inputs of one broad phase.
pub(crate) struct VtInput<'a> {
    /// Predicted positions (and inverse masses).
    pub particles: &'a [ClothParticle],
    /// Positions at the start of the substep.
    pub previous: &'a [Vec3],
    /// Positions at the start of the fixed step (the side reference).
    pub step_start: &'a [Vec3],
    /// Rest positions.
    pub rest: &'a [Vec3],
    /// Sim triangles.
    pub triangles: &'a [[usize; 3]],
    /// Torn triangles (skipped).
    pub hidden: &'a [bool],
    /// Per-particle layers (empty: all on layer 0).
    pub layers: &'a [u8],
    /// Contact distance in meters.
    pub radius: f32,
    /// Extra distance across layers.
    pub layer_gap: f32,
}

/// Scratch and results of the vertex-triangle pass, reused every substep.
#[derive(Debug, Clone, Default)]
pub(crate) struct VertexTriangleCollider {
    tri_min: Vec<Vec3>,
    tri_max: Vec<Vec3>,
    tri_lo: Vec<[i32; 3]>,
    tri_hi: Vec<[i32; 3]>,
    bucket_start: Vec<u32>,
    fill: Vec<u32>,
    sorted: Vec<u32>,
    candidates: Vec<u32>,
    /// Triangles whose box covers more than `MAX_SPAN` cells. Every particle tests them;
    /// the box test drops the ones it cannot reach. A long tether would otherwise force the
    /// cell up to that edge and put every triangle in one bucket.
    oversized: Vec<u32>,
    /// Candidate pairs of the running substep, ascending `(vertex, triangle)`.
    pub pairs: Vec<VtPair>,
    /// Pairs over the cap, summed since the solver was made.
    pub dropped: u64,
    /// Pairs in contact during the last iteration of the last substep.
    pub last_contacts: usize,
    /// Pairs whose particle is on the wrong side of its triangle after the last substep
    /// (0 when nothing crossed).
    pub crossings: usize,
}

/// Layer of particle `i` (0 without layers).
fn layer_of(layers: &[u8], i: usize) -> u8 {
    layers.get(i).copied().unwrap_or(0)
}

fn cell_of(p: Vec3, inv_cell: f32) -> [i32; 3] {
    let c = (p * inv_cell).floor();
    // `as` saturates far from the origin, which only makes cells coarser there.
    [c.x as i32, c.y as i32, c.z as i32]
}

/// Barycentric coordinates `(u, v, w)` of the projection of `p` onto the plane of `a, b, c`
/// (`p ≈ u a + v b + w c`), or `None` for a degenerate triangle (Ericson § 3.4).
pub(crate) fn barycentric(a: Vec3, b: Vec3, c: Vec3, p: Vec3) -> Option<(f32, f32, f32)> {
    let (v0, v1, v2) = (b - a, c - a, p - a);
    let d00 = v0.dot(v0);
    let d01 = v0.dot(v1);
    let d11 = v1.dot(v1);
    let d20 = v2.dot(v0);
    let d21 = v2.dot(v1);
    let denom = d00 * d11 - d01 * d01;
    if denom.abs() <= EPSILON * EPSILON {
        return None;
    }
    let v = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    Some((1.0 - v - w, v, w))
}

/// Squared distance from `p` to the closest point of triangle `a, b, c` (Ericson § 5.1.5).
fn triangle_distance_squared(p: Vec3, a: Vec3, b: Vec3, c: Vec3) -> f32 {
    let (ab, ac, ap) = (b - a, c - a, p - a);
    let (d1, d2) = (ab.dot(ap), ac.dot(ap));
    if d1 <= 0.0 && d2 <= 0.0 {
        return ap.length_squared();
    }
    let bp = p - b;
    let (d3, d4) = (ab.dot(bp), ac.dot(bp));
    if d3 >= 0.0 && d4 <= d3 {
        return bp.length_squared();
    }
    let vc = d1 * d4 - d3 * d2;
    if vc <= 0.0 && d1 >= 0.0 && d3 <= 0.0 {
        let t = d1 / (d1 - d3);
        return (p - (a + ab * t)).length_squared();
    }
    let cp = p - c;
    let (d5, d6) = (ab.dot(cp), ac.dot(cp));
    if d6 >= 0.0 && d5 <= d6 {
        return cp.length_squared();
    }
    let vb = d5 * d2 - d1 * d6;
    if vb <= 0.0 && d2 >= 0.0 && d6 <= 0.0 {
        let t = d2 / (d2 - d6);
        return (p - (a + ac * t)).length_squared();
    }
    let va = d3 * d6 - d5 * d4;
    if va <= 0.0 && (d4 - d3) >= 0.0 && (d5 - d6) >= 0.0 {
        let t = (d4 - d3) / ((d4 - d3) + (d5 - d6));
        return (p - (b + (c - b) * t)).length_squared();
    }
    let denom = va + vb + vc;
    if denom.abs() <= EPSILON {
        return ap
            .length_squared()
            .min(bp.length_squared())
            .min(cp.length_squared());
    }
    let (v, w) = (vb / denom, vc / denom);
    (p - (a + ab * v + ac * w)).length_squared()
}

/// Unit normal of `a, b, c` (counter-clockwise front), or `None` when degenerate.
fn unit_normal(a: Vec3, b: Vec3, c: Vec3) -> Option<Vec3> {
    let n = (b - a).cross(c - a);
    let len = n.length();
    (len > EPSILON).then(|| n / len)
}

/// Barycentric weights clamped into the triangle and renormalized (gradient weights).
fn clamped_weights(u: f32, v: f32, w: f32) -> [f32; 3] {
    let (u, v, w) = (u.max(0.0), v.max(0.0), w.max(0.0));
    let sum = u + v + w;
    if sum > EPSILON {
        [u / sum, v / sum, w / sum]
    } else {
        [1.0 / 3.0; 3]
    }
}

fn inside_prism(u: f32, v: f32, w: f32) -> bool {
    u >= -PRISM_TOLERANCE && v >= -PRISM_TOLERANCE && w >= -PRISM_TOLERANCE
}

impl VertexTriangleCollider {
    /// Builds the candidate pairs for this substep.
    pub fn broad_phase(&mut self, input: &VtInput<'_>, exclusions: &SelfCollider) {
        self.pairs.clear();
        let n = input.particles.len();
        let tris = input.triangles.len();
        if n < 4 || tris == 0 || !exclusions.covers(n) || input.step_start.len() != n {
            return;
        }
        let gap = if input.layers.is_empty() {
            0.0
        } else {
            input.layer_gap
        };
        let reach = (input.radius + gap) * CANDIDATE_MARGIN;
        // `2 * reach` keeps a particle and a triangle that can touch in neighboring cells.
        // Dividing a typical triangle box (an edge plus `reach` on both sides) by
        // `MAX_SPAN - 1` lets that box fit in the clamped grid; the edge alone left most boxes
        // one cell too wide, so they went to `oversized` and every particle tested them
        // (corr-physics audit). One long edge used to set the cell and collapse the hash.
        let fit = (exclusions.typical_edge() + 2.0 * reach) / (MAX_SPAN - 1) as f32;
        let cell = (2.0 * reach).max(fit).max(EPSILON);
        let inv_cell = 1.0 / cell;
        let pos = |i: usize| input.particles[i].position;

        // Triangle boxes (substep start and predicted, grown) and their cell ranges.
        self.tri_min.clear();
        self.tri_max.clear();
        self.tri_lo.clear();
        self.tri_hi.clear();
        self.oversized.clear();
        let mut entries = 0usize;
        for (t, tri) in input.triangles.iter().enumerate() {
            let mut lo = Vec3::splat(f32::INFINITY);
            let mut hi = Vec3::splat(f32::NEG_INFINITY);
            for &i in tri {
                for p in [pos(i), input.previous[i]] {
                    lo = lo.min(p);
                    hi = hi.max(p);
                }
            }
            lo -= Vec3::splat(reach);
            hi += Vec3::splat(reach);
            self.tri_min.push(lo);
            self.tri_max.push(hi);
            let c0 = cell_of(lo, inv_cell);
            let c1_raw = cell_of(hi, inv_cell);
            let too_wide = (0..3).any(|k| c1_raw[k].saturating_sub(c0[k]) >= MAX_SPAN);
            let hidden = input.hidden.get(t).copied().unwrap_or(false);
            if too_wide {
                // Empty range: the clamped cells would drop the far end of the box.
                self.tri_lo.push([0, 0, 0]);
                self.tri_hi.push([-1, -1, -1]);
                if !hidden {
                    self.oversized.push(t as u32);
                }
            } else {
                self.tri_lo.push(c0);
                self.tri_hi.push(c1_raw);
                if !hidden {
                    entries += (0..3)
                        .map(|k| (c1_raw[k] - c0[k] + 1).max(0) as usize)
                        .product::<usize>();
                }
            }
        }
        let table = (2 * entries.max(1)).next_power_of_two();
        let mask = table - 1;
        self.bucket_start.clear();
        self.bucket_start.resize(table + 1, 0);
        let visible = |t: usize| !input.hidden.get(t).copied().unwrap_or(false);
        for t in (0..tris).filter(|&t| visible(t)) {
            let (c0, c1) = (self.tri_lo[t], self.tri_hi[t]);
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        self.bucket_start[cell_hash([x, y, z], mask) + 1] += 1;
                    }
                }
            }
        }
        for b in 0..table {
            self.bucket_start[b + 1] += self.bucket_start[b];
        }
        self.fill.clear();
        self.fill.extend_from_slice(&self.bucket_start[..table]);
        self.sorted.clear();
        self.sorted.resize(self.bucket_start[table] as usize, 0);
        // Counting sort: triangles land in their buckets in ascending index order.
        for t in (0..tris).filter(|&t| visible(t)) {
            let (c0, c1) = (self.tri_lo[t], self.tri_hi[t]);
            for z in c0[2]..=c1[2] {
                for y in c0[1]..=c1[1] {
                    for x in c0[0]..=c1[0] {
                        let b = cell_hash([x, y, z], mask);
                        self.sorted[self.fill[b] as usize] = t as u32;
                        self.fill[b] += 1;
                    }
                }
            }
        }

        let cap = SELF_PAIRS_PER_PARTICLE * n;
        for i in 0..n {
            let x = pos(i);
            // A pair needs `x` inside the triangle's box (the test below), and a box is listed
            // in every cell it covers, so `x`'s own cell holds every candidate. The particle's
            // whole path, clamped to `MAX_SPAN` cells from its low corner, dropped the cells
            // around `x` for a particle moving fast toward +x/+y/+z (corr-physics audit).
            self.candidates.clear();
            {
                let b = cell_hash(cell_of(x, inv_cell), mask);
                let (s, e) = (self.bucket_start[b] as usize, self.bucket_start[b + 1] as usize);
                self.candidates.extend_from_slice(&self.sorted[s..e]);
            }
            self.candidates.extend_from_slice(&self.oversized);
            self.candidates.sort_unstable();
            self.candidates.dedup();
            #[cfg(test)]
            VT_CANDIDATES.fetch_add(self.candidates.len(), Ordering::Relaxed);
            let li = layer_of(input.layers, i);
            let wi = input.particles[i].inv_mass;
            for k in 0..self.candidates.len() {
                let t = self.candidates[k] as usize;
                let [a, b, c] = input.triangles[t];
                if i == a || i == b || i == c {
                    continue;
                }
                let (lo, hi) = (self.tri_min[t], self.tri_max[t]);
                if x.cmplt(lo).any() || x.cmpgt(hi).any() {
                    continue;
                }
                if exclusions.is_excluded(i, a as u32)
                    || exclusions.is_excluded(i, b as u32)
                    || exclusions.is_excluded(i, c as u32)
                {
                    continue;
                }
                let lt = layer_of(input.layers, a)
                    .min(layer_of(input.layers, b))
                    .min(layer_of(input.layers, c));
                let (vertex_scale, triangle_scale, contact) = if li == lt {
                    (1.0, 1.0, input.radius)
                } else if li > lt {
                    (1.0, 0.0, input.radius + gap)
                } else {
                    (0.0, 1.0, input.radius + gap)
                };
                let w_tri =
                    input.particles[a].inv_mass + input.particles[b].inv_mass + input.particles[c].inv_mass;
                if wi * vertex_scale + w_tri * triangle_scale == 0.0 {
                    continue;
                }
                let rest =
                    triangle_distance_squared(input.rest[i], input.rest[a], input.rest[b], input.rest[c]);
                if rest < contact * contact {
                    continue;
                }
                let (pa, pb, pc) = (pos(a), pos(b), pos(c));
                let Some(normal) = unit_normal(pa, pb, pc) else {
                    continue;
                };
                let Some((u, v, w)) = barycentric(pa, pb, pc, x) else {
                    continue;
                };
                if !inside_prism(u, v, w) {
                    continue;
                }
                let s = input.step_start;
                let start = (s[b] - s[a]).cross(s[c] - s[a]).dot(s[i] - s[a]);
                let side = if start.abs() > EPSILON {
                    start.signum()
                } else {
                    // No usable start plane: keep the side the particle is on now.
                    let now = normal.dot(x - pa);
                    if now >= 0.0 { 1.0 } else { -1.0 }
                };
                if side * normal.dot(x - pa) >= contact * CANDIDATE_MARGIN {
                    continue;
                }
                if self.pairs.len() < cap {
                    self.pairs.push(VtPair {
                        vertex: i as u32,
                        triangle: t as u32,
                        side,
                        contact,
                        vertex_scale,
                        triangle_scale,
                        touched: false,
                    });
                } else {
                    self.dropped += 1;
                }
            }
        }
    }

    /// One Gauss-Seidel pass over the candidate pairs, with position-level friction.
    pub fn project(
        &mut self,
        particles: &mut [ClothParticle],
        previous: &[Vec3],
        triangles: &[[usize; 3]],
        friction: f32,
    ) {
        let mut touching = 0;
        for pair in &mut self.pairs {
            let i = pair.vertex as usize;
            let [a, b, c] = triangles[pair.triangle as usize];
            let (pa, pb, pc, x) = (
                particles[a].position,
                particles[b].position,
                particles[c].position,
                particles[i].position,
            );
            let Some(normal) = unit_normal(pa, pb, pc) else {
                continue;
            };
            let Some((u, v, w)) = barycentric(pa, pb, pc, x) else {
                continue;
            };
            if !inside_prism(u, v, w) {
                continue;
            }
            let distance = pair.side * normal.dot(x - pa);
            let violation = pair.contact - distance;
            if violation <= 0.0 {
                continue;
            }
            touching += 1;
            pair.touched = true;
            let bw = clamped_weights(u, v, w);
            let wi = particles[i].inv_mass * pair.vertex_scale;
            let wt = [
                particles[a].inv_mass * pair.triangle_scale,
                particles[b].inv_mass * pair.triangle_scale,
                particles[c].inv_mass * pair.triangle_scale,
            ];
            let denom = wi + bw[0] * bw[0] * wt[0] + bw[1] * bw[1] * wt[1] + bw[2] * bw[2] * wt[2];
            if denom <= 0.0 {
                continue;
            }
            let dir = normal * pair.side;
            let step = violation / denom;
            particles[i].position += dir * (step * wi);
            for ((&p, &bk), &wk) in [a, b, c].iter().zip(&bw).zip(&wt) {
                particles[p].position -= dir * (step * bk * wk);
            }
            if friction > 0.0 {
                let point = |q: [Vec3; 3]| q[0] * bw[0] + q[1] * bw[1] + q[2] * bw[2];
                let now = point([
                    particles[a].position,
                    particles[b].position,
                    particles[c].position,
                ]);
                let then = point([previous[a], previous[b], previous[c]]);
                let rel = (particles[i].position - previous[i]) - (now - then);
                let tangent = rel - dir * rel.dot(dir);
                let t_len = tangent.length();
                if t_len > EPSILON {
                    let limit = friction * violation;
                    let remove = if t_len <= limit {
                        tangent
                    } else {
                        tangent * (limit / t_len)
                    };
                    let k = remove / denom;
                    particles[i].position -= k * wi;
                    for ((&p, &bk), &wk) in [a, b, c].iter().zip(&bw).zip(&wt) {
                        particles[p].position += k * (bk * wk);
                    }
                }
            }
        }
        self.last_contacts = touching;
    }

    /// Makes the contacts of this substep inelastic: for every pair a projection pushed, or that
    /// ends within [`CONTACT_BAND`] of its contact distance, the separating part of the relative
    /// normal velocity (which the position pushes created) is removed, spread like the push so
    /// momentum is kept (in the spirit of Bridson et al.'s inelastic repulsion impulse).
    /// Approaching pairs are left alone.
    pub fn inelastic(&self, particles: &mut [ClothParticle], triangles: &[[usize; 3]]) {
        for pair in &self.pairs {
            let i = pair.vertex as usize;
            let [a, b, c] = triangles[pair.triangle as usize];
            let (pa, pb, pc) = (
                particles[a].position,
                particles[b].position,
                particles[c].position,
            );
            let x = particles[i].position;
            let (Some(normal), Some((u, v, w))) = (unit_normal(pa, pb, pc), barycentric(pa, pb, pc, x))
            else {
                continue;
            };
            let in_contact = pair.side * normal.dot(x - pa) <= pair.contact * CONTACT_BAND;
            if !(pair.touched || in_contact) || !inside_prism(u, v, w) {
                continue;
            }
            let bw = clamped_weights(u, v, w);
            let wi = particles[i].inv_mass * pair.vertex_scale;
            let wt = [
                particles[a].inv_mass * pair.triangle_scale,
                particles[b].inv_mass * pair.triangle_scale,
                particles[c].inv_mass * pair.triangle_scale,
            ];
            let denom = wi + bw[0] * bw[0] * wt[0] + bw[1] * bw[1] * wt[1] + bw[2] * bw[2] * wt[2];
            if denom <= 0.0 {
                continue;
            }
            let dir = normal * pair.side;
            let point =
                particles[a].velocity * bw[0] + particles[b].velocity * bw[1] + particles[c].velocity * bw[2];
            let separating = (particles[i].velocity - point).dot(dir);
            if separating <= 0.0 {
                continue;
            }
            let k = separating / denom;
            particles[i].velocity -= dir * (k * wi);
            for ((&p, &bk), &wk) in [a, b, c].iter().zip(&bw).zip(&wt) {
                particles[p].velocity += dir * (k * bk * wk);
            }
        }
    }

    /// Counts candidate pairs whose particle ended on the wrong side of its triangle.
    pub fn measure(&mut self, particles: &[ClothParticle], triangles: &[[usize; 3]]) {
        self.crossings = self
            .pairs
            .iter()
            .filter(|pair| {
                let [a, b, c] = triangles[pair.triangle as usize];
                let (pa, pb, pc) = (
                    particles[a].position,
                    particles[b].position,
                    particles[c].position,
                );
                let x = particles[pair.vertex as usize].position;
                match (unit_normal(pa, pb, pc), barycentric(pa, pb, pc, x)) {
                    (Some(n), Some((u, v, w))) => inside_prism(u, v, w) && pair.side * n.dot(x - pa) < 0.0,
                    _ => false,
                }
            })
            .count();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn barycentric_recovers_the_weights() {
        let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Z);
        let p = a * 0.2 + b * 0.5 + c * 0.3 + Vec3::Y * 0.7;
        let (u, v, w) = barycentric(a, b, c, p).expect("triangle");
        assert!((u - 0.2).abs() < 1e-6 && (v - 0.5).abs() < 1e-6 && (w - 0.3).abs() < 1e-6);
        assert!(barycentric(a, b, a, p).is_none(), "degenerate");
    }

    #[test]
    fn closest_point_covers_every_region() {
        let (a, b, c) = (Vec3::ZERO, Vec3::X, Vec3::Z);
        let d = |p: Vec3| triangle_distance_squared(p, a, b, c).sqrt();
        assert!((d(Vec3::new(0.25, 0.5, 0.25)) - 0.5).abs() < 1e-6, "face");
        assert!(
            (d(Vec3::new(-1.0, 0.0, -1.0)) - 2f32.sqrt()).abs() < 1e-6,
            "vertex a"
        );
        assert!((d(Vec3::new(2.0, 0.0, 0.0)) - 1.0).abs() < 1e-6, "vertex b");
        assert!((d(Vec3::new(0.0, 0.0, 3.0)) - 2.0).abs() < 1e-6, "vertex c");
        assert!((d(Vec3::new(0.5, 0.0, -1.0)) - 1.0).abs() < 1e-6, "edge ab");
        assert!((d(Vec3::new(-1.0, 0.0, 0.5)) - 1.0).abs() < 1e-6, "edge ac");
        assert!(
            (d(Vec3::new(1.0, 0.0, 1.0)) - 0.5f32.sqrt()).abs() < 1e-6,
            "edge bc"
        );
    }

    #[test]
    fn one_sided_pairs_move_only_the_outer_side() {
        // A free particle 0.005 above a free triangle, contact 0.02: both move when on one layer;
        // the inner side stays put when the particle is on an outer layer.
        let particle = |p: Vec3| ClothParticle {
            position: p,
            velocity: Vec3::ZERO,
            inv_mass: 1.0,
        };
        let base = vec![
            particle(Vec3::new(0.0, 0.0, 0.0)),
            particle(Vec3::new(0.0, 0.0, 1.0)),
            particle(Vec3::new(1.0, 0.0, 0.0)),
            particle(Vec3::new(0.25, 0.005, 0.25)),
        ];
        let previous: Vec<Vec3> = base.iter().map(|p| p.position).collect();
        let triangles = [[0usize, 1, 2]];
        for (vertex_scale, triangle_scale) in [(1.0, 1.0), (1.0, 0.0), (0.0, 1.0)] {
            let mut ps = base.clone();
            let mut col = VertexTriangleCollider::default();
            col.pairs.push(VtPair {
                vertex: 3,
                triangle: 0,
                side: 1.0,
                contact: 0.02,
                vertex_scale,
                triangle_scale,
                touched: false,
            });
            col.project(&mut ps, &previous, &triangles, 0.0);
            assert_eq!(col.last_contacts, 1);
            let n = unit_normal(ps[0].position, ps[1].position, ps[2].position).expect("normal");
            let gap = n.dot(ps[3].position - ps[0].position);
            assert!(
                (gap - 0.02).abs() < 1e-5,
                "resolved to the contact distance: {gap}"
            );
            if triangle_scale == 0.0 {
                assert!(ps[..3].iter().zip(&base).all(|(p, b)| p.position == b.position));
            }
            if vertex_scale == 0.0 {
                assert_eq!(ps[3].position, base[3].position);
            }
        }
    }

    fn still(p: Vec3) -> ClothParticle {
        ClothParticle {
            position: p,
            velocity: Vec3::ZERO,
            inv_mass: 1.0,
        }
    }

    #[test]
    fn a_fast_particle_moving_toward_plus_x_still_finds_its_triangle() {
        // Corr-physics audit: the particle's path box was clamped to `MAX_SPAN` cells from its
        // low corner, so a particle that crossed more cells toward +x never looked at the cells
        // around where it ended; moving toward -x it did. Four 3 m links elsewhere make the typical
        // edge 3 m, so the cell is about 1 m and the 1 m triangle is not an oversized one (every
        // particle tests those, which hides the clamp). Starting at x = -8.5 the old clamped path
        // (cells -9..=-6) hashes into no bucket of the triangle's cells in the 64-bucket table, so
        // only the end point's own cell can find it (a bucket collision hid the bug at -4).
        let triangle = [
            Vec3::new(1.0, 0.0, 0.0),
            Vec3::new(1.0, 0.0, 1.0),
            Vec3::new(2.0, 0.0, 0.0),
        ];
        let far = [
            Vec3::new(0.0, 10.0, 0.0),
            Vec3::new(3.0, 10.0, 0.0),
            Vec3::new(3.0, 10.0, 3.0),
            Vec3::new(0.0, 10.0, 3.0),
        ];
        let end = Vec3::new(1.2, 0.005, 0.2);
        let triangles = [[0usize, 1, 2]];
        let edges = [
            (0usize, 1usize, 1.0f32),
            (1, 2, 2.0f32.sqrt()),
            (2, 0, 1.0),
            (4, 5, 3.0),
            (5, 6, 3.0),
            (6, 7, 3.0),
            (7, 4, 3.0),
        ];
        for from_x in [-8.5f32, 6.4] {
            let mut particles: Vec<ClothParticle> = triangle.iter().copied().map(still).collect();
            particles.push(still(end));
            particles.extend(far.iter().copied().map(still));
            let mut rest: Vec<Vec3> = triangle.to_vec();
            rest.push(Vec3::new(1.2, 0.5, 0.2));
            rest.extend(far);
            let mut previous: Vec<Vec3> = triangle.to_vec();
            previous.push(Vec3::new(from_x, 0.005, 0.2));
            previous.extend(far);
            let mut exclusions = SelfCollider::default();
            exclusions.prepare(particles.len(), &edges, 0);
            let input = VtInput {
                particles: &particles,
                previous: &previous,
                step_start: &previous,
                rest: &rest,
                triangles: &triangles,
                hidden: &[],
                layers: &[],
                radius: 0.01,
                layer_gap: 0.0,
            };
            let mut col = VertexTriangleCollider::default();
            col.broad_phase(&input, &exclusions);
            assert!(
                col.oversized.is_empty(),
                "from x = {from_x}: the triangle must use the grid"
            );
            assert!(
                col.pairs.iter().any(|p| p.vertex == 3 && p.triangle == 0),
                "from x = {from_x}: the particle above the triangle has no pair"
            );
        }
    }

    #[test]
    fn a_regular_grid_puts_no_triangle_in_the_oversized_list() {
        // Corr-physics audit: the cell was a third of the typical edge, but a triangle's box is
        // that edge plus `reach` on both sides, so most boxes spanned one cell too many and went
        // to `oversized`, which every particle tests (all pairs).
        let side = 10usize;
        let mut particles = Vec::new();
        for z in 0..side {
            for x in 0..side {
                particles.push(still(Vec3::new(x as f32 * 0.1, 0.0, z as f32 * 0.1)));
            }
        }
        let at = |x: usize, z: usize| z * side + x;
        let mut triangles = Vec::new();
        let mut edges = Vec::new();
        for z in 0..side - 1 {
            for x in 0..side - 1 {
                let (a, b, c, d) = (at(x, z), at(x + 1, z), at(x, z + 1), at(x + 1, z + 1));
                triangles.push([a, b, c]);
                triangles.push([b, d, c]);
                edges.push((a, b, 0.1f32));
                edges.push((a, c, 0.1f32));
                edges.push((b, c, 0.1f32 * 2.0f32.sqrt()));
            }
        }
        let rest: Vec<Vec3> = particles.iter().map(|p| p.position).collect();
        let mut exclusions = SelfCollider::default();
        exclusions.prepare(particles.len(), &edges, 0);
        let input = VtInput {
            particles: &particles,
            previous: &rest,
            step_start: &rest,
            rest: &rest,
            triangles: &triangles,
            hidden: &[],
            layers: &[],
            radius: 0.01,
            layer_gap: 0.0,
        };
        let mut col = VertexTriangleCollider::default();
        col.broad_phase(&input, &exclusions);
        assert!(
            col.oversized.len() * 10 < triangles.len(),
            "{} of {} triangles are oversized",
            col.oversized.len(),
            triangles.len()
        );
    }

    #[test]
    fn long_link_does_not_gather_every_triangle() {
        // Two islands 4 m apart. Eight short triangles and one 40 m rest edge. Before the
        // cell followed `longest_edge` (40 m), every particle gathered all eight triangles
        // (128 candidates). The median edge keeps the islands in different cells.
        let mut particles = Vec::new();
        let mut edges = Vec::new();
        let mut triangles = Vec::new();
        for island in 0..2 {
            let base = island * 8;
            let origin = if island == 0 { 0.08 } else { 4.0 };
            for i in 0..8 {
                let p = Vec3::new(origin + (i % 3) as f32 * 0.01, 0.0, (i / 3) as f32 * 0.01);
                particles.push(ClothParticle {
                    position: p,
                    velocity: Vec3::ZERO,
                    inv_mass: 1.0,
                });
            }
            for t in 0..4 {
                let tri = [base + t, base + t + 1, base + t + 2];
                triangles.push(tri);
                edges.push((tri[0], tri[1], 0.02));
                edges.push((tri[1], tri[2], 0.02));
                edges.push((tri[2], tri[0], 0.02));
            }
        }
        edges.push((0, 8, 40.0));
        let rest: Vec<Vec3> = particles.iter().map(|p| p.position).collect();
        let mut exclusions = SelfCollider::default();
        exclusions.prepare(particles.len(), &edges, 0);
        let input = VtInput {
            particles: &particles,
            previous: &rest,
            step_start: &rest,
            rest: &rest,
            triangles: &triangles,
            hidden: &[],
            layers: &[],
            radius: 0.05,
            layer_gap: 0.0,
        };
        let mut col = VertexTriangleCollider::default();
        VT_CANDIDATES.store(0, Ordering::Relaxed);
        col.broad_phase(&input, &exclusions);
        let visits = VT_CANDIDATES.load(Ordering::Relaxed);
        assert!(
            (1..96).contains(&visits),
            "vertex-triangle candidates {visits} (16 particles x 8 triangles is 128)"
        );
        eprintln!("vtcol_candidates {visits}");
        for pair in &col.pairs {
            let vertex_island = pair.vertex / 8;
            let tri = triangles[pair.triangle as usize];
            let tri_island = (tri[0] / 8) as u32;
            assert_eq!(
                vertex_island, tri_island,
                "pair crossed the gap: vertex {} triangle {}",
                pair.vertex, pair.triangle
            );
        }
    }
}
