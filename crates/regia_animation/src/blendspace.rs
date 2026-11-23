// SPDX-License-Identifier: MPL-2.0
//! Blend spaces: 1D parameter-driven clip interpolation and 2D triangulated sample spaces.
//!
//! Specification: `docs/specs/17-animation.md` § 3. The 2D space is Delaunay-triangulated
//! (Bowyer–Watson) and sampled with barycentric weights, so the weights always sum to 1.
//! Queries outside the hull are projected onto the nearest triangle edge.
//!
//! Provenance: Bowyer–Watson incremental Delaunay construction (Bowyer 1981, Watson 1981;
//! public description in Paul Bourke's computational geometry notes); barycentric point-in-
//! triangle test (Ericson, *Real-Time Collision Detection*, 2005, § 3.4). See
//! `docs/provenance.md`.

use serde::{Deserialize, Serialize};

/// One 1D blend space sample: a parameter value and the clip it maps to.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSample1D {
    /// Parameter value at which this clip has full weight.
    pub value: f32,
    /// Clip name.
    pub clip: String,
}

/// One 2D blend space sample.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSample2D {
    /// Parameter point at which this clip has full weight.
    pub point: [f32; 2],
    /// Clip name.
    pub clip: String,
}

/// A 1D blend space definition.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSpace1D {
    /// The parameter driving the blend.
    pub param: String,
    /// Samples, sorted by `value` at build time.
    pub samples: Vec<BlendSample1D>,
}

/// A 2D blend space definition.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct BlendSpace2D {
    /// The parameter driving the horizontal axis.
    pub param_x: String,
    /// The parameter driving the vertical axis.
    pub param_y: String,
    /// Samples.
    pub samples: Vec<BlendSample2D>,
    /// Precomputed triangulation (indices into `samples`), rebuilt on deserialise if empty.
    pub triangles: Vec<[usize; 3]>,
}

impl BlendSpace1D {
    /// Weights for a parameter value: at most two neighbouring samples carry weight, the rest
    /// are zero. Weights sum to 1. Returns `(sample index, weight)` pairs with weight > 0.
    ///
    /// A non-finite value (a speed computed as `0 / 0`) is read as 0, and samples with a
    /// non-finite position are ignored, so the weights are always finite.
    pub fn weights(&self, value: f32) -> Vec<(usize, f32)> {
        let mut order = Vec::new();
        let mut out = Vec::new();
        self.weights_into(value, &mut order, &mut out);
        out
    }

    /// Write [`BlendSpace1D::weights`] into `out`.
    ///
    /// `order` is the finite sample indices sorted by value (stable on ties). It is rebuilt
    /// only when it no longer matches `samples`.
    pub fn weights_into(&self, value: f32, order: &mut Vec<usize>, out: &mut Vec<(usize, f32)>) {
        let value = crate::num::finite_or(value, 0.0);
        if !order_matches(&self.samples, order) {
            order.clear();
            order.extend((0..self.samples.len()).filter(|&i| self.samples[i].value.is_finite()));
            order.sort_by(|&a, &b| {
                self.samples[a]
                    .value
                    .partial_cmp(&self.samples[b].value)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        }
        out.clear();
        if order.is_empty() {
            return;
        }
        if order.len() == 1 || value <= self.samples[order[0]].value {
            out.push((order[0], 1.0));
            return;
        }
        let last = order.len() - 1;
        if value >= self.samples[order[last]].value {
            out.push((order[last], 1.0));
            return;
        }
        for w in order.windows(2) {
            let (a, b) = (w[0], w[1]);
            let (va, vb) = (self.samples[a].value, self.samples[b].value);
            if value >= va && value <= vb {
                let t = if vb > va { (value - va) / (vb - va) } else { 0.0 };
                // Drop the zero-weight partner so exact sample points yield one entry.
                if 1.0 - t > 1e-6 {
                    out.push((a, 1.0 - t));
                }
                if t > 1e-6 {
                    out.push((b, t));
                }
                return;
            }
        }
        out.push((order[0], 1.0));
    }
}

/// True when `order` is the stable sort of the finite sample indices.
fn order_matches(samples: &[BlendSample1D], order: &[usize]) -> bool {
    let finite = samples.iter().filter(|s| s.value.is_finite()).count();
    if order.len() != finite {
        return false;
    }
    if order
        .iter()
        .any(|&i| i >= samples.len() || !samples[i].value.is_finite())
    {
        return false;
    }
    order.windows(2).all(|w| {
        let (va, vb) = (samples[w[0]].value, samples[w[1]].value);
        va < vb || (va == vb && w[0] < w[1])
    })
}

impl BlendSpace2D {
    /// Build the Delaunay triangulation of the sample points (Bowyer–Watson on a
    /// super-triangle, then removal of super vertices).
    pub fn triangulate(&mut self) {
        self.triangles = delaunay(&self.samples.iter().map(|s| s.point).collect::<Vec<_>>());
    }

    /// Weights for a parameter point: barycentric weights of the containing triangle, or a
    /// projection onto the nearest triangle when outside the hull. Weights sum to 1.
    ///
    /// A non-finite coordinate is read as 0. A space built in code without
    /// [`BlendSpace2D::triangulate`] is triangulated on the fly (call it once to avoid the
    /// per-query cost); triangles that index past the samples are ignored.
    pub fn weights(&self, point: [f32; 2]) -> Vec<(usize, f32)> {
        let mut cached = Vec::new();
        let mut cached_samples = usize::MAX;
        let mut cached_points_hash = 0;
        let mut out = Vec::new();
        self.weights_into(
            point,
            &mut cached,
            &mut cached_samples,
            &mut cached_points_hash,
            &mut out,
        );
        out
    }

    /// Write [`BlendSpace2D::weights`] into `out`.
    ///
    /// When the asset has no usable triangulation and at least three samples, `cached` holds
    /// the triangulation built for the samples `*cached_samples` and `*cached_points_hash`
    /// fingerprint. It is rebuilt when the count or the fingerprint differs: code-built spaces
    /// can move samples in place without changing the count. A stored triangulation is used
    /// as-is and does not touch `cached`.
    pub fn weights_into(
        &self,
        point: [f32; 2],
        cached: &mut Vec<[usize; 3]>,
        cached_samples: &mut usize,
        cached_points_hash: &mut u64,
        out: &mut Vec<(usize, f32)>,
    ) {
        out.clear();
        let point = [
            crate::num::finite_or(point[0], 0.0),
            crate::num::finite_or(point[1], 0.0),
        ];
        let n = self.samples.len();
        if n == 0 {
            return;
        }
        if n == 1 {
            out.push((0, 1.0));
            return;
        }
        if n == 2 {
            write_segment(self.samples[0].point, self.samples[1].point, 0, 1, point, out);
            return;
        }
        let triangles_ok =
            !self.triangles.is_empty() && self.triangles.iter().all(|tri| tri.iter().all(|&i| i < n));
        if triangles_ok {
            write_from_triangles(&self.samples, &self.triangles, point, out);
            return;
        }
        let points_hash = self
            .samples
            .iter()
            .fold(0x9e37_79b9_7f4a_7c15u64, |hash, sample| {
                let hash = hash.wrapping_mul(0x0100_0000_01b3) ^ u64::from(sample.point[0].to_bits());
                hash.wrapping_mul(0x0100_0000_01b3) ^ u64::from(sample.point[1].to_bits())
            });
        let stale = *cached_samples != n
            || *cached_points_hash != points_hash
            || cached.is_empty()
            || cached.iter().any(|tri| tri.iter().any(|&i| i >= n));
        if stale {
            let points: Vec<[f32; 2]> = self.samples.iter().map(|s| s.point).collect();
            *cached = delaunay(&points);
            *cached_samples = n;
            *cached_points_hash = points_hash;
        }
        write_from_triangles(&self.samples, cached, point, out);
    }
}

fn write_segment(
    a: [f32; 2],
    b: [f32; 2],
    ia: usize,
    ib: usize,
    point: [f32; 2],
    out: &mut Vec<(usize, f32)>,
) {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let len_sq = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if len_sq > 0.0 {
        (((point[0] - a[0]) * ab[0] + (point[1] - a[1]) * ab[1]) / len_sq).clamp(0.0, 1.0)
    } else {
        0.0
    };
    out.push((ia, 1.0 - t));
    out.push((ib, t));
}

fn write_from_triangles(
    samples: &[BlendSample2D],
    triangles: &[[usize; 3]],
    point: [f32; 2],
    out: &mut Vec<(usize, f32)>,
) {
    if triangles.is_empty() {
        let a = samples[0].point;
        let b = samples.get(1).map(|s| s.point).unwrap_or(a);
        write_segment(a, b, 0, 1, point, out);
        return;
    }
    let point_of = |i: usize| samples[i].point;
    // Collinear samples (every triangle degenerate, from the fan fallback): interpolate along
    // the line like a 1D space. Projecting onto the fan's overlapping edges picked whichever
    // edge came first, often blending two samples that are not neighbours.
    let degenerate = triangles
        .iter()
        .all(|t| barycentric(point_of(t[0]), point_of(t[1]), point_of(t[2]), point).is_none());
    if degenerate {
        let points: Vec<[f32; 2]> = samples.iter().map(|s| s.point).collect();
        out.extend(collinear_weights(&points, point));
        return;
    }
    // Inside test first. A point on an edge shared by two triangles can test a hair outside
    // both in floating point; accept a small negative coordinate, keep the triangle the
    // point is deepest inside, and renormalise its clamped weights.
    let mut inside: Option<([usize; 3], [f32; 3], f32)> = None;
    for tri in triangles {
        if let Some(w) = barycentric(point_of(tri[0]), point_of(tri[1]), point_of(tri[2]), point) {
            let depth = w[0].min(w[1]).min(w[2]);
            if depth >= -1e-5 && inside.is_none_or(|(_, _, best)| depth > best) {
                inside = Some((*tri, w, depth));
            }
        }
    }
    if let Some((tri, w, _)) = inside {
        let w = [w[0].max(0.0), w[1].max(0.0), w[2].max(0.0)];
        let sum = w[0] + w[1] + w[2];
        if sum > 0.0 {
            out.push((tri[0], w[0] / sum));
            out.push((tri[1], w[1] / sum));
            out.push((tri[2], w[2] / sum));
            return;
        }
    }
    // Outside: project onto the closest point on any triangle edge.
    let mut best: Option<(usize, usize, f32, f32)> = None;
    for tri in triangles {
        for e in 0..3 {
            let ia = tri[e];
            let ib = tri[(e + 1) % 3];
            let (a, b) = (point_of(ia), point_of(ib));
            let ab = [b[0] - a[0], b[1] - a[1]];
            let len_sq = ab[0] * ab[0] + ab[1] * ab[1];
            let t = if len_sq > 0.0 {
                (((point[0] - a[0]) * ab[0] + (point[1] - a[1]) * ab[1]) / len_sq).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let proj = [a[0] + ab[0] * t, a[1] + ab[1] * t];
            let d_sq = (point[0] - proj[0]).powi(2) + (point[1] - proj[1]).powi(2);
            if best.is_none() || d_sq < best.map(|b| b.3).unwrap_or(f32::INFINITY) {
                best = Some((ia, ib, t, d_sq));
            }
        }
    }
    match best {
        Some((ia, ib, t, _)) => {
            out.push((ia, 1.0 - t));
            out.push((ib, t));
        }
        None => out.push((0, 1.0)),
    }
}

/// Weights for samples that all lie on one line: project the samples and the query onto the
/// line and blend the two neighbouring samples (clamped at the ends), as a 1D space does.
fn collinear_weights(points: &[[f32; 2]], point: [f32; 2]) -> Vec<(usize, f32)> {
    let origin = points[0];
    let offset = |p: [f32; 2]| [p[0] - origin[0], p[1] - origin[1]];
    let length_sq = |v: [f32; 2]| v[0] * v[0] + v[1] * v[1];
    let far = points
        .iter()
        .map(|&p| offset(p))
        .max_by(|a, b| length_sq(*a).total_cmp(&length_sq(*b)))
        .unwrap_or([0.0, 0.0]);
    let len = length_sq(far).sqrt();
    if len <= 1e-12 {
        // Every sample sits on one point.
        return vec![(0, 1.0)];
    }
    let dir = [far[0] / len, far[1] / len];
    let along = |p: [f32; 2]| {
        let v = offset(p);
        v[0] * dir[0] + v[1] * dir[1]
    };
    let mut order: Vec<(f32, usize)> = points.iter().enumerate().map(|(i, &p)| (along(p), i)).collect();
    order.sort_by(|a, b| a.0.total_cmp(&b.0));
    let q = along(point);
    let (first, last) = (order[0], order[order.len() - 1]);
    if q <= first.0 {
        return vec![(first.1, 1.0)];
    }
    if q >= last.0 {
        return vec![(last.1, 1.0)];
    }
    for pair in order.windows(2) {
        let ((sa, a), (sb, b)) = (pair[0], pair[1]);
        if q >= sa && q <= sb {
            let t = if sb > sa { (q - sa) / (sb - sa) } else { 0.0 };
            return vec![(a, 1.0 - t), (b, t)];
        }
    }
    vec![(first.1, 1.0)]
}

/// Barycentric coordinates of `p` in triangle `(a, b, c)`, or `None` for a degenerate
/// triangle. Unnormalised sign convention is not used: the returned coordinates sum to 1.
fn barycentric(a: [f32; 2], b: [f32; 2], c: [f32; 2], p: [f32; 2]) -> Option<[f32; 3]> {
    let v0 = [b[0] - a[0], b[1] - a[1]];
    let v1 = [c[0] - a[0], c[1] - a[1]];
    let v2 = [p[0] - a[0], p[1] - a[1]];
    let d00 = v0[0] * v0[0] + v0[1] * v0[1];
    let d01 = v0[0] * v1[0] + v0[1] * v1[1];
    let d11 = v1[0] * v1[0] + v1[1] * v1[1];
    let d20 = v2[0] * v0[0] + v2[1] * v0[1];
    let d21 = v2[0] * v1[0] + v2[1] * v1[1];
    let denom = d00 * d11 - d01 * d01;
    // Relative threshold, not an absolute 1e-12: `denom` has units of length⁴ (like `d00 *
    // d11`), so a millimetre-scale triangle (denom ~1e-16) read as degenerate and the space
    // fell back to the collinear path. A zero-size triangle is degenerate however it is scaled.
    let scale = d00 * d11;
    if scale == 0.0 || !denom.is_finite() || denom.abs() < 1e-12 * scale {
        return None;
    }
    let v = (d11 * d20 - d01 * d21) / denom;
    let w = (d00 * d21 - d01 * d20) / denom;
    let u = 1.0 - v - w;
    Some([u, v, w])
}

/// Bowyer–Watson Delaunay triangulation of 2D points. Collinear inputs return a fan
/// triangulation (the algorithm cannot triangulate them exactly; weights still sum to 1
/// because queries fall back to edge projection).
fn delaunay(points: &[[f32; 2]]) -> Vec<[usize; 3]> {
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }

    // Super-triangle around all points, in extended index space: n, n+1, n+2.
    let mut min = [f32::MAX, f32::MAX];
    let mut max = [f32::MIN, f32::MIN];
    for &p in points {
        for d in 0..2 {
            min[d] = min[d].min(p[d]);
            max[d] = max[d].max(p[d]);
        }
    }
    let span = (max[0] - min[0]).max(max[1] - min[1]).max(1.0) * 10.0;
    let mid = [(min[0] + max[0]) * 0.5, (min[1] + max[1]) * 0.5];
    let super_pts = [
        [mid[0] - span, mid[1] - span * 0.2],
        [mid[0] + span, mid[1] - span * 0.2],
        [mid[0], mid[1] + span],
    ];
    let pt = |i: usize| -> [f32; 2] { if i < n { points[i] } else { super_pts[i - n] } };

    let circumcircle = |a: usize, b: usize, c: usize| -> Option<([f32; 2], f32)> {
        let (pa, pb, pc) = (pt(a), pt(b), pt(c));
        let d = 2.0 * (pa[0] * (pb[1] - pc[1]) + pb[0] * (pc[1] - pa[1]) + pc[0] * (pa[1] - pb[1]));
        if d.abs() < 1e-12 {
            return None;
        }
        let a2 = pa[0] * pa[0] + pa[1] * pa[1];
        let b2 = pb[0] * pb[0] + pb[1] * pb[1];
        let c2 = pc[0] * pc[0] + pc[1] * pc[1];
        let ux = (a2 * (pb[1] - pc[1]) + b2 * (pc[1] - pa[1]) + c2 * (pa[1] - pb[1])) / d;
        let uy = (a2 * (pc[0] - pb[0]) + b2 * (pa[0] - pc[0]) + c2 * (pb[0] - pa[0])) / d;
        let center = [ux, uy];
        Some((center, (pa[0] - ux).powi(2) + (pa[1] - uy).powi(2)))
    };

    let mut triangles: Vec<[usize; 3]> = vec![[n, n + 1, n + 2]];
    for (i, point) in points.iter().enumerate().take(n) {
        let mut polygon: Vec<(usize, usize)> = Vec::new();
        let mut kept: Vec<[usize; 3]> = Vec::new();
        for &tri in &triangles {
            let inside = circumcircle(tri[0], tri[1], tri[2]).is_some_and(|(c, r2)| {
                let d2 = (point[0] - c[0]).powi(2) + (point[1] - c[1]).powi(2);
                d2 <= r2 * (1.0 + 1e-7)
            });
            if inside {
                polygon.push((tri[0], tri[1]));
                polygon.push((tri[1], tri[2]));
                polygon.push((tri[2], tri[0]));
            } else {
                kept.push(tri);
            }
        }
        // Boundary edges appear exactly once among the removed triangles' edges.
        let mut boundary: Vec<(usize, usize)> = Vec::new();
        'edge: for e in polygon {
            let rev = (e.1, e.0);
            if boundary.contains(&rev) {
                boundary.retain(|&x| x != rev);
            } else if boundary.contains(&e) {
                // duplicate in same orientation: keep once
                continue 'edge;
            } else {
                boundary.push(e);
            }
        }
        for (a, b) in boundary {
            kept.push([a, b, i]);
        }
        triangles = kept;
    }
    triangles.retain(|t| t[0] < n && t[1] < n && t[2] < n);
    if triangles.is_empty() {
        // Collinear or degenerate input: fan triangulation so queries still work.
        let mut fan = Vec::new();
        for i in 1..n - 1 {
            fan.push([0, i, i + 1]);
        }
        return fan;
    }
    triangles
}

#[cfg(test)]
mod tests {
    use super::*;

    fn space1d() -> BlendSpace1D {
        BlendSpace1D {
            param: "Speed".into(),
            samples: vec![
                BlendSample1D {
                    value: 0.0,
                    clip: "Idle".into(),
                },
                BlendSample1D {
                    value: 1.6,
                    clip: "Walk".into(),
                },
                BlendSample1D {
                    value: 6.0,
                    clip: "Run".into(),
                },
            ],
        }
    }

    #[test]
    fn blend1d_weights_sum_to_one() {
        let s = space1d();
        for step in 0..=100 {
            let v = -1.0 + step as f32 * 0.08;
            let w = s.weights(v);
            let sum: f32 = w.iter().map(|x| x.1).sum();
            assert!((sum - 1.0).abs() < 1e-5, "value {v} sum {sum}");
        }
    }

    #[test]
    fn blend1d_exact_at_samples() {
        let s = space1d();
        let w = s.weights(1.6);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0, 1);
        assert_eq!(w[0].1, 1.0);
    }

    #[test]
    fn blend1d_interpolates_between_neighbours() {
        let s = space1d();
        let w = s.weights(3.8); // halfway 1.6..6.0
        assert_eq!(w.len(), 2);
        assert!((w[0].1 - 0.5).abs() < 1e-4);
        assert!((w[1].1 - 0.5).abs() < 1e-4);
    }

    fn space2d() -> BlendSpace2D {
        let mut s = BlendSpace2D {
            param_x: "Speed".into(),
            param_y: "Direction".into(),
            samples: vec![
                BlendSample2D {
                    point: [0.0, 0.0],
                    clip: "Idle".into(),
                },
                BlendSample2D {
                    point: [6.0, 0.0],
                    clip: "RunF".into(),
                },
                BlendSample2D {
                    point: [0.0, 1.0],
                    clip: "StrafeL".into(),
                },
                BlendSample2D {
                    point: [6.0, 1.0],
                    clip: "RunL".into(),
                },
                BlendSample2D {
                    point: [3.0, 0.5],
                    clip: "Mid".into(),
                },
            ],
            triangles: Vec::new(),
        };
        s.triangulate();
        s
    }

    /// A code-built space whose samples are mutated in place (the count unchanged) used to
    /// keep the runtime triangulation cache: `weights_into` answered from the stale
    /// triangulation while `weights` (a fresh cache) answered from the moved points.
    #[test]
    fn blend2d_cache_rebuilds_when_a_sample_moves() {
        let mut s = space2d();
        s.triangles.clear();
        let point = [1.0, 0.2];
        let fresh = s.weights(point);
        let mut cached = Vec::new();
        let mut cached_samples = usize::MAX;
        let mut cached_points_hash = 0u64;
        let mut out = Vec::new();
        s.weights_into(
            point,
            &mut cached,
            &mut cached_samples,
            &mut cached_points_hash,
            &mut out,
        );
        assert_eq!(out, fresh, "a warm cache must agree with a fresh query");

        // Move the "Mid" sample far away; the sample count stays 5.
        s.samples[4].point = [30.0, 10.0];
        let fresh_moved = s.weights(point);
        s.weights_into(
            point,
            &mut cached,
            &mut cached_samples,
            &mut cached_points_hash,
            &mut out,
        );
        assert_eq!(
            out, fresh_moved,
            "the cache must rebuild after an in-place move: {out:?} vs {fresh_moved:?}"
        );
    }

    /// A space measured in millimetre-scale parameters (side 1e-4) used to be treated as
    /// fully degenerate: the barycentric denominator's absolute 1e-12 epsilon exceeds the
    /// triangle's area squared (~1e-16), every triangle read as degenerate and the query fell
    /// back to the two-sample collinear path.
    #[test]
    fn blend2d_tiny_space_still_triangulates() {
        let e = 1.0e-4_f32;
        let s = BlendSpace2D {
            param_x: "x".into(),
            param_y: "y".into(),
            samples: vec![
                BlendSample2D {
                    point: [0.0, 0.0],
                    clip: "A".into(),
                },
                BlendSample2D {
                    point: [e, 0.0],
                    clip: "B".into(),
                },
                BlendSample2D {
                    point: [0.0, e],
                    clip: "C".into(),
                },
                BlendSample2D {
                    point: [e, e],
                    clip: "D".into(),
                },
            ],
            triangles: Vec::new(),
        };
        // Off both diagonals: strictly inside the corner triangle whichever diagonal the
        // triangulation picks (the exact centre or quarter sits on one of them).
        let w = s.weights([e * 0.3, e * 0.2]);
        assert_eq!(w.len(), 3, "the centre must blend a whole triangle: {w:?}");
        assert!(
            w.iter().all(|&(_, weight)| weight > 0.0),
            "every corner carries weight: {w:?}"
        );
        let sum: f32 = w.iter().map(|x| x.1).sum();
        assert!((sum - 1.0).abs() < 1e-5, "{w:?}");
    }

    #[test]
    fn blend2d_weights_sum_to_one_everywhere() {
        let s = space2d();
        for ix in 0..=20 {
            for iy in 0..=20 {
                let p = [ix as f32 * 0.3 - 1.0, iy as f32 * 0.3 - 1.0];
                let w = s.weights(p);
                let sum: f32 = w.iter().map(|x| x.1).sum();
                assert!((sum - 1.0).abs() < 1e-4, "point {p:?} weights {w:?} sum {sum}");
            }
        }
    }

    #[test]
    fn blend2d_untriangulated_or_stale_triangles_do_not_panic() {
        let mut s = space2d();
        s.triangles.clear();
        let w = s.weights([3.0, 0.2]);
        let sum: f32 = w.iter().map(|x| x.1).sum();
        assert!((sum - 1.0).abs() < 1e-4, "{w:?}");
        assert!(w.len() >= 3, "all samples take part, not the first two: {w:?}");
        s.triangles = vec![[0, 1, 99]];
        let w = s.weights([3.0, 0.2]);
        let sum: f32 = w.iter().map(|x| x.1).sum();
        assert!((sum - 1.0).abs() < 1e-4, "{w:?}");
        assert!(w.iter().all(|(i, _)| *i < s.samples.len()));
    }

    #[test]
    fn blend2d_exact_at_vertices() {
        let s = space2d();
        for (i, sample) in s.samples.iter().enumerate() {
            let w = s.weights(sample.point);
            let total: f32 = w.iter().map(|x| x.1).sum();
            assert!((total - 1.0).abs() < 1e-4);
            let own = w.iter().find(|(idx, _)| *idx == i).map(|x| x.1).unwrap_or(0.0);
            assert!(own > 0.99, "vertex {i} own weight {own}");
        }
    }

    #[test]
    fn blend2d_inside_triangle_interpolates() {
        let s = space2d();
        // Midpoint between Idle (0,0) and RunF (6,0): whichever triangle contains it, Idle and
        // RunF must carry ~0.5 each (Mid and StrafeL are off the y=0 edge).
        let w = s.weights([3.0, 0.0]);
        let idle = w.iter().find(|(i, _)| *i == 0).map(|x| x.1).unwrap_or(0.0);
        let run = w.iter().find(|(i, _)| *i == 1).map(|x| x.1).unwrap_or(0.0);
        assert!((idle - 0.5).abs() < 1e-4, "idle {idle} weights {w:?}");
        assert!((run - 0.5).abs() < 1e-4, "run {run} weights {w:?}");
    }

    #[test]
    fn delaunay_covers_convex_hull_once() {
        let pts: Vec<[f32; 2]> = vec![
            [0.0, 0.0],
            [4.0, 0.0],
            [4.0, 4.0],
            [0.0, 4.0],
            [1.0, 2.0],
            [3.0, 1.0],
        ];
        let tris = delaunay(&pts);
        assert!(!tris.is_empty());
        // Every strictly-interior probe point (offset off the grid to avoid shared edges)
        // is inside exactly one triangle.
        for ix in 1..8 {
            for iy in 1..8 {
                let p = [ix as f32 * 0.5 + 0.13, iy as f32 * 0.5 + 0.29];
                let mut inside = 0;
                for t in &tris {
                    if let Some(w) = barycentric(pts[t[0]], pts[t[1]], pts[t[2]], p)
                        && w.iter().all(|c| *c >= -1e-5)
                    {
                        inside += 1;
                    }
                }
                assert_eq!(inside, 1, "point {p:?} inside {inside} triangles");
            }
        }
    }

    fn dense(weights: &[(usize, f32)], n: usize) -> Vec<f32> {
        let mut out = vec![0.0; n];
        for &(i, w) in weights {
            out[i] += w;
        }
        out
    }

    #[test]
    fn non_finite_inputs_give_finite_weights_that_sum_to_one() {
        let s1 = space1d();
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let w = s1.weights(v);
            let sum: f32 = w.iter().map(|x| x.1).sum();
            assert!(
                w.iter().all(|x| x.1.is_finite()) && (sum - 1.0).abs() < 1e-5,
                "{v}: {w:?}"
            );
        }
        let s2 = space2d();
        for p in [[f32::NAN, 0.5], [0.5, f32::INFINITY], [f32::NAN, f32::NAN]] {
            let w = s2.weights(p);
            let sum: f32 = w.iter().map(|x| x.1).sum();
            // The old projection fallback returned NaN weights for a NaN query.
            assert!(
                w.iter().all(|x| x.1.is_finite()) && (sum - 1.0).abs() < 1e-4,
                "{p:?}: {w:?}"
            );
        }
    }

    #[test]
    fn an_untriangulated_space_uses_every_sample() {
        // Built in code without `triangulate()` (graphs not loaded from RON). The old fallback
        // blended only the first two samples, so the far corner played the wrong clip.
        let mut s = space2d();
        s.triangles.clear();
        let w = s.weights([6.0, 1.0]);
        let corner = w.iter().find(|(i, _)| *i == 3).map(|x| x.1).unwrap_or(0.0);
        assert!(corner > 0.99, "RunL at its own point: {w:?}");
        // Stale triangles that index past the samples are ignored instead of panicking.
        let mut stale = space2d();
        stale.triangles.push([0, 1, 99]);
        let w = stale.weights([3.0, 0.5]);
        let sum: f32 = w.iter().map(|x| x.1).sum();
        assert!((sum - 1.0).abs() < 1e-4, "{w:?}");
    }

    /// The common locomotion layout: a 3 × 3 grid (direction × speed), with collinear hull
    /// points and co-circular quads.
    fn grid() -> BlendSpace2D {
        let mut samples = Vec::new();
        for y in [-1.0f32, 0.0, 1.0] {
            for x in [-1.0f32, 0.0, 1.0] {
                samples.push(BlendSample2D {
                    point: [x, y],
                    clip: format!("c{x}{y}"),
                });
            }
        }
        let mut s = BlendSpace2D {
            param_x: "X".into(),
            param_y: "Y".into(),
            samples,
            triangles: Vec::new(),
        };
        s.triangulate();
        s
    }

    #[test]
    fn a_grid_space_covers_its_hull_and_blends_continuously() {
        let s = grid();
        let n = s.samples.len();
        // Every vertex plays its own clip.
        for (i, sample) in s.samples.iter().enumerate() {
            let w = dense(&s.weights(sample.point), n);
            assert!(w[i] > 0.999, "vertex {i}: {w:?}");
        }
        // Every point inside the square lies in some triangle (no hull gaps from the
        // super-triangle), and the weights move continuously with the input.
        let steps = 40;
        let at = |ix: usize, iy: usize| {
            let p = [
                -1.0 + 2.0 * ix as f32 / steps as f32,
                -1.0 + 2.0 * iy as f32 / steps as f32,
            ];
            dense(&s.weights(p), n)
        };
        for iy in 0..=steps {
            for ix in 0..=steps {
                let p = [
                    -1.0 + 2.0 * ix as f32 / steps as f32,
                    -1.0 + 2.0 * iy as f32 / steps as f32,
                ];
                let covered = s.triangles.iter().any(|t| {
                    barycentric(
                        s.samples[t[0]].point,
                        s.samples[t[1]].point,
                        s.samples[t[2]].point,
                        p,
                    )
                    .is_some_and(|w| w.iter().all(|c| *c >= -1e-4))
                });
                assert!(covered, "{p:?} falls outside every triangle");
                let w = at(ix, iy);
                let sum: f32 = w.iter().sum();
                assert!((sum - 1.0).abs() < 1e-4, "{p:?}: {w:?}");
                if ix > 0 {
                    let change: f32 = w.iter().zip(at(ix - 1, iy)).map(|(a, b)| (a - b).abs()).sum();
                    // A 0.05 step in a unit cell moves at most ~0.1 of weight between samples.
                    assert!(change < 0.25, "jump of {change} between neighbours at {p:?}");
                }
            }
        }
    }

    #[test]
    fn collinear_samples_blend_their_neighbours_along_the_line() {
        // Four samples on the x axis, listed out of order: 0, 2, 1, 3. The fan triangulation of
        // collinear points has no edge between the samples at 2 and 3, so the old projection
        // blended the wrong pair.
        let mut s = BlendSpace2D {
            param_x: "X".into(),
            param_y: "Y".into(),
            samples: [0.0f32, 2.0, 1.0, 3.0]
                .iter()
                .map(|&x| BlendSample2D {
                    point: [x, 0.0],
                    clip: format!("c{x}"),
                })
                .collect(),
            triangles: Vec::new(),
        };
        s.triangulate();
        let w = dense(&s.weights([2.5, 0.0]), 4);
        assert!((w[1] - 0.5).abs() < 1e-5 && (w[3] - 0.5).abs() < 1e-5, "{w:?}");
        // Off the line: projected onto it.
        let w = dense(&s.weights([0.5, 3.0]), 4);
        assert!((w[0] - 0.5).abs() < 1e-5 && (w[2] - 0.5).abs() < 1e-5, "{w:?}");
        // Past the ends: the end sample.
        assert!(dense(&s.weights([-4.0, 0.0]), 4)[0] > 0.999);
        assert!(dense(&s.weights([9.0, 1.0]), 4)[3] > 0.999);
        // Every sample on one point: the first sample.
        let mut same = s.clone();
        for sample in &mut same.samples {
            sample.point = [1.0, 1.0];
        }
        same.triangulate();
        assert_eq!(same.weights([5.0, 5.0]), vec![(0, 1.0)]);
    }

    #[test]
    fn weights_are_identical_when_the_triangle_list_is_borrowed() {
        let space = BlendSpace1D {
            param: "Speed".into(),
            samples: [0.0, 1.0, 1.0, 2.0]
                .into_iter()
                .enumerate()
                .map(|(index, value)| BlendSample1D {
                    value,
                    clip: format!("c{index}"),
                })
                .collect(),
        };
        let mut order = Vec::new();
        let mut out = Vec::new();
        for value in [0.0, 0.25, 1.0, 1.5, 3.0] {
            space.weights_into(value, &mut order, &mut out);
            assert_eq!(out, space.weights(value), "1d {value}");
        }
        let order_cap = order.capacity();
        assert!(order_cap >= 4);
        space.weights_into(0.5, &mut order, &mut out);
        assert_eq!(order.capacity(), order_cap);
        assert_eq!(out, space.weights(0.5));

        let space = BlendSpace2D {
            param_x: "X".into(),
            param_y: "Y".into(),
            samples: [[0.0, 0.0], [1.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
                .into_iter()
                .enumerate()
                .map(|(index, point)| BlendSample2D {
                    point,
                    clip: format!("s{index}"),
                })
                .collect(),
            triangles: Vec::new(),
        };
        let mut cached = Vec::new();
        let mut cached_samples = usize::MAX;
        let mut cached_points_hash = 0u64;
        let points = [[0.0, 0.0], [0.25, 0.25], [0.5, 0.8], [2.0, -1.0]];
        for point in points {
            space.weights_into(
                point,
                &mut cached,
                &mut cached_samples,
                &mut cached_points_hash,
                &mut out,
            );
            assert_eq!(out, space.weights(point), "{point:?}");
        }
        assert_eq!(cached_samples, space.samples.len());
        assert!(!cached.is_empty());
        let tri_cap = cached.capacity();
        for point in points {
            space.weights_into(
                point,
                &mut cached,
                &mut cached_samples,
                &mut cached_points_hash,
                &mut out,
            );
            assert_eq!(out, space.weights(point), "reused {point:?}");
        }
        assert_eq!(cached.capacity(), tri_cap);
    }
}
