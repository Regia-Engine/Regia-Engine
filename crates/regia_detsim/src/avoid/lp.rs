// SPDX-License-Identifier: MPL-2.0
//! The opt-in linear-program avoidance solver and static wall segments (ADR-8555).
//!
//! Original integer code after the public ORCA formulation: J. van den Berg, S. J. Guy,
//! M. Lin, D. Manocha, "Reciprocal n-Body Collision Avoidance" (ISRR 2009), and the
//! public description of its reference library (the incremental 2D linear program,
//! the 3D fallback that minimizes the largest penetration, and obstacle half-planes
//! with full responsibility). No floating point: every value is an integer.
//!
//! # Units and rounding rules
//!
//! - Positions and radii in millimeters; velocities inside the solver in
//!   `1/ORCA_LP_VELOCITY_SCALE` mm/s (256 sub-units per mm/s, "Q").
//! - Directions are integer vectors of length `ORCA_LP_UNIT` (2^24) within one unit,
//!   made by `unit`: the vector is shifted so its larger component lies in
//!   `[2^29, 2^30)` (exact for left shifts), the length is the floor root of the
//!   squared sum, and each component is `component * 2^24 / length` rounded half away
//!   from zero.
//! - `div_round` rounds half away from zero; `div_floor` / `div_ceil` round toward
//!   minus / plus infinity. Interval ends of the one-dimensional program use the
//!   rounding that keeps them inside every earlier constraint (floor for an upper end,
//!   ceil for a lower end) and the floor root for the circle.
//! - Every constraint is solved shifted inward by `MARGIN` (2 mm/s). The result in Q
//!   becomes mm/s by `div_round`; if that leaves the speed circle the components are
//!   truncated toward zero instead. Either moves the velocity by at most `sqrt(2)`
//!   mm/s (362 Q), less than the margin, so a feasible answer lies on the free side of
//!   every unshifted half-plane exactly (ADR-8555 has the bound). One exception
//!   (ADR-8703): when the wall planes conflict among themselves only through the
//!   margin, they are kept hard unshifted for the fallback, so the walls then hold
//!   to within the output rounding (1 mm/s) and the call still reports infeasible.
//! - Fallback ties (ADR-8703): where the least-penetration step finds a whole
//!   segment of equal penetration, it takes the point nearest the preference.
//!
//! The arithmetic domain (ADR-8555): bodies and settings inside the avoidance input
//! domain, segment endpoints within `ORCA_MAX_POSITION_MM`. Neighbors and wall pieces
//! whose velocity obstacle lies `2^22` mm/s beyond the relative velocity are skipped
//! (they cannot cut the speed circle), which bounds every velocity-space value by
//! 2^37 Q and every product by 2^113 in `i128`.

use super::{OrcaBody, OrcaHalfPlane, OrcaLpOutcome, OrcaLpStats, OrcaSegment, OrcaSettings, OrderKey};

/// Velocity sub-units per mm/s inside the solver.
pub(super) const Q: i64 = 256;
/// Length of a direction vector.
pub(super) const UNIT: i64 = 1 << 24;
/// Inward shift of every constraint, in Q (2 mm/s).
const MARGIN: i128 = 2 * Q as i128;
/// Relevance reach beyond the relative velocity, in Q (2^22 mm/s).
const REACH: i128 = (1 << 22) * Q as i128;
/// `|det(d_i, d_j)|` at or below this is parallel in the fallback (sin < 2^-14).
const PARALLEL: i128 = ((UNIT as i128) * (UNIT as i128)) >> 14;

type V = [i128; 2];

/// One constraint: `det(d, x - p) >= 0` (the free side is left of `d`).
#[derive(Debug, Clone, Copy)]
pub(super) struct Line {
    p: V,
    d: V,
}

/// Buffers and counters of the LP path, owned by `OrcaScratch`.
#[derive(Debug, Clone, Default)]
pub(super) struct LpScratch {
    /// The unshifted half-planes of the last solve, obstacles first.
    pub(super) planes: Vec<OrcaHalfPlane>,
    work: Vec<Line>,
    proj: Vec<Line>,
    pub(super) stats: OrcaLpStats,
    pub(super) last: Option<OrcaLpOutcome>,
}

impl LpScratch {
    pub(super) fn with_capacity(planes: usize) -> Self {
        Self {
            planes: Vec::with_capacity(planes),
            work: Vec::with_capacity(planes),
            proj: Vec::with_capacity(planes),
            stats: OrcaLpStats::default(),
            last: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Integer helpers.

#[inline]
fn dot(a: V, b: V) -> i128 {
    a[0] * b[0] + a[1] * b[1]
}

#[inline]
fn det(a: V, b: V) -> i128 {
    a[0] * b[1] - a[1] * b[0]
}

#[inline]
fn sub(a: V, b: V) -> V {
    [a[0] - b[0], a[1] - b[1]]
}

#[inline]
fn add(a: V, b: V) -> V {
    [a[0] + b[0], a[1] + b[1]]
}

/// Truncating quotient and remainder. The `i64` fast path gives the same integers as
/// the `i128` path (it is only taken when both operands fit), on hardware division.
#[inline]
fn qr(n: i128, d: i128) -> (i128, i128) {
    if let (Ok(n64), Ok(d64)) = (i64::try_from(n), i64::try_from(d))
        && n64 != i64::MIN
    {
        return (i128::from(n64 / d64), i128::from(n64 % d64));
    }
    let q = n / d;
    (q, n - q * d)
}

/// `n / d` rounded half away from zero; `d != 0`. A positive power-of-two `d` (the
/// direction length and Q) takes a shift with the same result.
#[inline]
pub(super) fn div_round(n: i128, d: i128) -> i128 {
    if d > 0 && d & (d - 1) == 0 {
        let k = d.trailing_zeros();
        // `m` is `2^127` (as `i128::MIN`) only for `n = i128::MIN, d = 1`, where the
        // wrapping negation gives back `n` exactly; every other `m` negates normally.
        let m = ((n.unsigned_abs() + (d as u128 >> 1)) >> k) as i128;
        return if n < 0 { m.wrapping_neg() } else { m };
    }
    let (q, r) = qr(n, d);
    if 2 * r.unsigned_abs() >= d.unsigned_abs() {
        if (n < 0) == (d < 0) { q + 1 } else { q - 1 }
    } else {
        q
    }
}

/// `n / d` rounded toward minus infinity; `d != 0`.
#[inline]
fn div_floor(n: i128, d: i128) -> i128 {
    let (q, r) = qr(n, d);
    if r != 0 && ((n < 0) != (d < 0)) { q - 1 } else { q }
}

/// `n / d` rounded toward plus infinity; `d != 0`.
#[inline]
fn div_ceil(n: i128, d: i128) -> i128 {
    let (q, r) = qr(n, d);
    if r != 0 && ((n < 0) == (d < 0)) { q + 1 } else { q }
}

#[inline]
fn scale(v: V, k: i128, d: i128) -> V {
    [div_round(v[0] * k, d), div_round(v[1] * k, d)]
}

/// Floor square root of a `u64`: descending Newton from a power of two at or above
/// the root.
fn isqrt64(v: u64) -> u64 {
    if v < 2 {
        return v;
    }
    let bits = 64 - v.leading_zeros();
    let mut x = 1u64 << bits.div_ceil(2);
    loop {
        let y = (x + v / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Floor square root. Above 2^64 the start is `(isqrt64(v >> 2k) + 1) << k` with `2k`
/// the even shift that brings `v` under 2^64: at or above the root and within 2^-31 of
/// it, so descending Newton ends at the floor in one or two steps.
pub(super) fn isqrt(v: u128) -> u128 {
    if let Ok(small) = u64::try_from(v) {
        return u128::from(isqrt64(small));
    }
    let bits = 128 - v.leading_zeros();
    let shift = (bits - 63) & !1;
    let hi = (v >> shift) as u64;
    let mut x = (u128::from(isqrt64(hi)) + 1) << (shift / 2);
    loop {
        let y = (x + v / x) / 2;
        if y >= x {
            return x;
        }
        x = y;
    }
}

/// Ceiling square root.
pub(super) fn isqrt_ceil(v: u128) -> u128 {
    let s = isqrt(v);
    if s * s < v { s + 1 } else { s }
}

/// Direction of `v` with length `UNIT` (see the module rounding rules); `None` for zero.
fn unit(v: V) -> Option<V> {
    let big = v[0].unsigned_abs().max(v[1].unsigned_abs());
    if big == 0 {
        return None;
    }
    let bits = 128 - big.leading_zeros() as i32;
    let w = if bits > 30 {
        let s = (bits - 30) as u32;
        [v[0] >> s, v[1] >> s]
    } else {
        let s = (30 - bits) as u32;
        [v[0] << s, v[1] << s]
    };
    let len = isqrt(dot(w, w) as u128) as i128;
    if len == 0 {
        return None;
    }
    Some([
        div_round(w[0] * UNIT as i128, len),
        div_round(w[1] * UNIT as i128, len),
    ])
}

/// Left normal of a direction: the free side of its line.
#[inline]
fn left(d: V) -> V {
    [-d[1], d[0]]
}

/// Direction whose free side is the normal `n`.
#[inline]
fn dir_of(n: V) -> V {
    [n[1], -n[0]]
}

// ---------------------------------------------------------------------------
// Velocity obstacles.

/// Signed angle order around `m` in `(-pi, pi]`: true when `b` lies counterclockwise of `a`.
fn ccw_of(m: V, a: V, b: V) -> bool {
    let class = |u: V| {
        let s = det(m, u);
        if s > 0 {
            1
        } else if s < 0 {
            -1
        } else if dot(m, u) > 0 {
            0
        } else {
            2
        }
    };
    let (ca, cb) = (class(a), class(b));
    if ca != cb {
        return cb > ca;
    }
    if ca == 0 || ca == 2 {
        return false;
    }
    det(a, b) > 0
}

/// A tangent ray from the origin to a disc: direction and the distance to the
/// tangent point in velocity space (Q).
#[derive(Clone, Copy)]
struct Tangent {
    u: V,
    start: i128,
}

/// The two tangents from the origin to the disc of radius `r` at `e` (mm, `|e| >= r`;
/// a point inside counts as on the circle).
fn tangents(e: V, r: i128, tau_ms: i128) -> Option<(Tangent, Tangent)> {
    // A rounded clip end may sit a fraction of a millimeter inside the disc: tangent there.
    let l2 = (dot(e, e) - r * r).max(0);
    // The tangent length and the radius in 2^-20 mm, so the directions keep their
    // precision when the tangent is short.
    let ls = isqrt((l2 as u128) << 40) as i128;
    let rs = r << 20;
    let l = unit([e[0] * ls - e[1] * rs, e[0] * rs + e[1] * ls])?;
    let rt = unit([e[0] * ls + e[1] * rs, -e[0] * rs + e[1] * ls])?;
    let start = div_round(ls * 1000 * Q as i128, tau_ms << 20);
    Some((Tangent { u: l, start }, Tangent { u: rt, start }))
}

/// Closest point of the segment `a b` (mm, relative) to the origin, rounded, and
/// whether the origin lies strictly within `r` of the segment (exact).
fn closest(a: V, b: V, r: i128) -> (V, bool) {
    let e = sub(b, a);
    let den = dot(e, e);
    let t = -dot(a, e);
    if den == 0 || t <= 0 {
        return (a, dot(a, a) < r * r);
    }
    if t >= den {
        return (b, dot(b, b) < r * r);
    }
    // Perpendicular distance squared is det(a, b)^2 / den; compare without the square
    // overflowing (r^2 * den < 2^106 in the domain, so a larger |det| is never touching).
    let c = det(a, b).unsigned_abs();
    let touching = c < (1 << 63) && c * c < (r * r) as u128 * den as u128;
    let p = [a[0] + div_round(e[0] * t, den), a[1] + div_round(e[1] * t, den)];
    (p, touching)
}

/// The point of the truncated velocity obstacle's boundary nearest to `w` (Q) and
/// the outward unit normal there. The obstacle is the capsule `a b` (mm, relative,
/// `a == b` for a disc) grown by `r`, over the horizon `tau_ms`; the origin is at
/// least `r` from it.
fn nearest_on_vo(a: V, b: V, m: V, r: i128, tau_ms: i128, w: V) -> Option<(V, V)> {
    let k = 1000 * Q as i128;
    let two = a != b;
    if !two {
        // A disc: when the nearest point of the cutoff circle is on its visible arc, it
        // is the nearest boundary point (each leg is tangent to that circle, so no leg
        // point is nearer), and the legs are not needed.
        let c = scale(a, k, tau_ms);
        let rho = div_round(r * k, tau_ms);
        let dv = sub(w, c);
        let e = if dv == [0, 0] {
            unit([-c[0], -c[1]])
        } else {
            unit(dv)
        };
        if let Some(e) = e
            && dot(c, e) <= -rho * UNIT as i128
        {
            return Some((add(c, scale(e, rho, UNIT as i128)), e));
        }
    }
    // Legs: the extreme tangents of the endpoint discs, seen from the direction `m`
    // of the capsule's closest point.
    let (la, ra) = tangents(a, r, tau_ms)?;
    let (mut lt, mut rt) = (la, ra);
    if two {
        let (lb, rb) = tangents(b, r, tau_ms)?;
        if ccw_of(m, lt.u, lb.u) {
            lt = lb;
        }
        if ccw_of(m, rb.u, rt.u) {
            rt = rb;
        }
    }
    let av = scale(a, k, tau_ms);
    let bv = scale(b, k, tau_ms);
    let rho = div_round(r * k, tau_ms);

    let mut best: Option<(i128, V, V)> = None;
    let mut offer = |q: V, n: V| {
        let d = sub(q, w);
        let d2 = dot(d, d);
        if best.is_none_or(|(b2, _, _)| d2 < b2) {
            best = Some((d2, q, n));
        }
    };
    // Left and right legs from their tangent points outward.
    for (t, n) in [(lt, left(lt.u)), (rt, [rt.u[1], -rt.u[0]])] {
        let mut s = div_round(dot(w, t.u), UNIT as i128);
        if s < t.start {
            s = t.start;
        }
        offer(scale(t.u, s, UNIT as i128), n);
    }
    // The cutoff: arcs of the end discs and the near side, where visible from the origin.
    let ends: &[(V, V)] = if two { &[(av, bv), (bv, av)] } else { &[(av, av)] };
    for &(c, other) in ends {
        let dv = sub(w, c);
        let e = if dv == [0, 0] {
            unit([-c[0], -c[1]])
        } else {
            unit(dv)
        };
        let Some(e) = e else { continue };
        if dot(c, e) > -rho * UNIT as i128 {
            continue;
        }
        if two && dot(e, sub(other, c)) > 0 {
            continue;
        }
        offer(add(c, scale(e, rho, UNIT as i128)), e);
    }
    if two {
        let ev = sub(bv, av);
        if let Some(mut ns) = unit(left(ev)) {
            let facing = dot(av, ns);
            if facing > 0 {
                ns = [-ns[0], -ns[1]];
            }
            if facing != 0 && dot(av, ns) + rho * UNIT as i128 <= 0 {
                let start = add(av, scale(ns, rho, UNIT as i128));
                let den = dot(ev, ev);
                let num = dot(sub(w, start), ev).clamp(0, den);
                offer(add(start, scale(ev, num, den)), ns);
            }
        }
    }
    best.map(|(_, q, n)| (q, n))
}

/// The reciprocal half-plane for one neighbor, or `None` when it cannot cut the
/// speed circle (skipped as far).
fn agent_line(me: &OrcaBody, v: V, o: &OrcaBody, cap: i128, settings: &OrcaSettings) -> Option<Line> {
    let q = Q as i128;
    let p = [
        i128::from(o.x_mm) - i128::from(me.x_mm),
        i128::from(o.z_mm) - i128::from(me.z_mm),
    ];
    let r = i128::from(me.r_mm) + i128::from(o.r_mm);
    let w = sub(v, [i128::from(o.vx_mm_s) * q, i128::from(o.vz_mm_s) * q]);
    let d2 = dot(p, p);
    let (point, n) = if d2 < r * r || d2 == 0 {
        // Overlap: leave the contact disc within `contact_ms`.
        let tau = i128::from(settings.contact_ms);
        let cv = scale(p, 1000 * q, tau);
        let rho = div_round(r * 1000 * q, tau);
        let dv = sub(w, cv);
        let e = unit(dv)
            .or_else(|| unit([-p[0], -p[1]]))
            .unwrap_or(if me.id < o.id {
                [-(UNIT as i128), 0]
            } else {
                [UNIT as i128, 0]
            });
        let target = add(cv, scale(e, rho, UNIT as i128));
        let u = sub(target, w);
        (add(v, [div_round(u[0], 2), div_round(u[1], 2)]), e)
    } else {
        // Range: no velocity in the speed circle meets the neighbor (at its current
        // velocity) within the horizon. Stronger than the 2^22 mm/s domain reach.
        let tau = i128::from(settings.horizon_ms);
        let gap = isqrt(d2 as u128) as i128 - r;
        let vo1 = i128::from(o.vx_mm_s).abs() + i128::from(o.vz_mm_s).abs();
        if gap * 1000 > tau * (cap + vo1) {
            return None;
        }
        let (target, n) = nearest_on_vo(p, p, p, r, tau, w)?;
        let u = sub(target, w);
        (add(v, [div_round(u[0], 2), div_round(u[1], 2)]), n)
    };
    Some(Line {
        p: point,
        d: dir_of(n),
    })
}

/// Liang-Barsky clip of `a b` to the box `[-l, l]^2`, endpoints rounded.
fn clip(a: V, b: V, l: i128) -> Option<(V, V)> {
    let e = sub(b, a);
    // Parameters as fractions num / den with den > 0.
    let (mut lo, mut hi) = ((0_i128, 1_i128), (1_i128, 1_i128));
    for axis in 0..2 {
        for (pk, qk) in [(-e[axis], a[axis] + l), (e[axis], l - a[axis])] {
            if pk == 0 {
                if qk < 0 {
                    return None;
                }
                continue;
            }
            // t = qk / pk
            let (num, den) = if pk < 0 { (-qk, -pk) } else { (qk, pk) };
            if pk < 0 {
                // entering: lo = max(lo, t)
                if num * lo.1 > lo.0 * den {
                    lo = (num, den);
                }
            } else if num * hi.1 < hi.0 * den {
                hi = (num, den);
            }
        }
    }
    if lo.0 * hi.1 > hi.0 * lo.1 {
        return None;
    }
    let at = |t: (i128, i128)| {
        [
            a[0] + div_round(e[0] * t.0, t.1),
            a[1] + div_round(e[1] * t.0, t.1),
        ]
    };
    Some((at(lo), at(hi)))
}

/// The half-plane of one wall segment (full responsibility), or `None` when it is
/// beyond reach or is a zero-length segment on the body center (ADR-8703).
fn obstacle_line(me: &OrcaBody, v: V, s: &OrcaSegment, cap: i128, settings: &OrcaSettings) -> Option<Line> {
    let q = Q as i128;
    // Canonical endpoint order, so `a b` and `b a` give the same plane.
    let (sa, sb) = if (s.a_mm[0], s.a_mm[1]) <= (s.b_mm[0], s.b_mm[1]) {
        (s.a_mm, s.b_mm)
    } else {
        (s.b_mm, s.a_mm)
    };
    let pos = [i128::from(me.x_mm), i128::from(me.z_mm)];
    let a = sub([i128::from(sa[0]), i128::from(sa[1])], pos);
    let b = sub([i128::from(sb[0]), i128::from(sb[1])], pos);
    let r = i128::from(me.r_mm);
    if a == [0, 0] && b == [0, 0] {
        // A zero-length segment on the body center has no outward direction: no plane
        // (ADR-8703), so the other planes decide, and the preference when there are none.
        return None;
    }
    let (c, touching) = closest(a, b, r);
    if touching || c == [0, 0] {
        // Overlap: move off the wall within `contact_ms`; never toward it.
        let tau = i128::from(settings.contact_ms);
        let dist = isqrt(dot(c, c) as u128) as i128;
        // `c` is the center only when the segment passes through it with nonzero
        // length (the zero-length case returned above), so `left(b - a)` is defined.
        let n = unit([-c[0], -c[1]])
            .or_else(|| unit(left(sub(b, a))))
            .unwrap_or([UNIT as i128, 0]);
        let push = div_round((r - dist).max(0) * 1000 * q, tau);
        return Some(Line {
            p: scale(n, push, UNIT as i128),
            d: dir_of(n),
        });
    }
    let tau = i128::from(settings.obstacle_horizon_ms);
    // Range: beyond `r + horizon * cap` no velocity in the speed circle reaches the wall
    // within the horizon (the rounded closest point is within 1 mm).
    let gap = isqrt(dot(c, c) as u128) as i128 - 1 - r;
    if gap * 1000 > tau * cap {
        return None;
    }
    let vl1 = v[0].abs() + v[1].abs();
    let reach = r + div_ceil(tau * (vl1 + REACH), 1000 * q) + 2;
    let (ca, cb) = clip(a, b, reach)?;
    let (m, _) = closest(ca, cb, r);
    let (point, n) = nearest_on_vo(ca, cb, m, r, tau, v)?;
    Some(Line {
        p: point,
        d: dir_of(n),
    })
}

// ---------------------------------------------------------------------------
// The linear programs.

/// Best point on line `i` inside the circle and every earlier line; `None` if none.
///
/// With `dir_opt`, the point furthest along `opt`; when `opt` is within the fallback's
/// parallel tolerance of perpendicular to the line (every point of the chord equally far
/// along it), the point nearest `tie` instead (ADR-8703), never an arbitrary chord end.
///
/// The line is re-anchored at its foot point `p0` (the point nearest the origin,
/// rounded), so the circle chord is `p0 ± t d / UNIT` with `t = floor(sqrt((R² - |p0|²)
/// UNIT² / |d|²))`: exact for the real length of `d`. `p0` lies within 0.71 Q of
/// the line.
fn lp1(lines: &[Line], i: usize, radius: i128, opt: V, dir_opt: bool, tie: V) -> Option<V> {
    let unit = UNIT as i128;
    let li = lines[i];
    let dd = dot(li.d, li.d);
    let p0 = sub(li.p, scale(li.d, dot(li.p, li.d), dd));
    let rest = radius * radius - dot(p0, p0);
    if rest < 0 {
        return None;
    }
    let s = isqrt((rest * unit * unit / dd) as u128) as i128;
    let (mut tl, mut tr) = (-s, s);
    for lj in &lines[..i] {
        let den = det(li.d, lj.d);
        let num = det(lj.d, sub(p0, lj.p));
        if den == 0 {
            if num < 0 {
                return None;
            }
            continue;
        }
        if den > 0 {
            tr = tr.min(div_floor(num * unit, den));
        } else {
            tl = tl.max(div_ceil(num * unit, den));
        }
        if tl > tr {
            return None;
        }
    }
    let nearest = |target: V| div_round(dot(sub(target, p0), li.d) * unit, dd).clamp(tl, tr);
    let t = if dir_opt {
        let along = dot(opt, li.d);
        // |cos(opt, d)| <= 2^-14, the same tolerance the fallback uses for parallel
        // pairs: `|along| * 2^14 <= |opt| |d|` (floor roots, so a tie is never missed
        // by more than the rounding of the two lengths).
        let span = isqrt(dot(opt, opt) as u128) * isqrt(dd as u128);
        if along.unsigned_abs() << 14 <= span {
            nearest(tie)
        } else if along > 0 {
            tr
        } else {
            tl
        }
    } else {
        nearest(opt)
    };
    Some(add(p0, scale(li.d, t, unit)))
}

/// Incremental 2D program: the point nearest `opt` (or furthest along the direction
/// `opt` when `dir_opt`, ties to the point nearest `tie`) in the circle and every line.
/// Returns the index of the first line that left no point (`lines.len()` on success)
/// and the result so far.
fn lp2(lines: &[Line], radius: i128, opt: V, dir_opt: bool, tie: V) -> (usize, V) {
    let mut result = if dir_opt || dot(opt, opt) > radius * radius {
        onto_circle(opt, radius)
    } else {
        opt
    };
    for (i, l) in lines.iter().enumerate() {
        if det(l.d, sub(result, l.p)) < 0 {
            match lp1(lines, i, radius, opt, dir_opt, tie) {
                Some(x) => result = x,
                None => return (i, result),
            }
        }
    }
    (lines.len(), result)
}

/// `v` scaled to length `radius`, rounded toward zero against the ceiling length, so
/// the result never leaves the circle.
fn onto_circle(v: V, radius: i128) -> V {
    let len = isqrt_ceil(dot(v, v) as u128) as i128;
    if len == 0 {
        return [0, 0];
    }
    [v[0] * radius / len, v[1] * radius / len]
}

/// Fallback from line `begin`: the point minimizing the largest penetration of the
/// soft lines (index `>= hard`), keeping the hard lines and the circle. Where that
/// minimum is a whole segment (parallel soft lines), the point nearest `tie` (the
/// preference) on it (ADR-8703).
fn lp3(
    lines: &[Line],
    proj: &mut Vec<Line>,
    hard: usize,
    begin: usize,
    radius: i128,
    mut result: V,
    tie: V,
) -> V {
    let unit = UNIT as i128;
    let mut distance = 0_i128;
    for i in begin..lines.len() {
        let li = lines[i];
        if det(li.d, sub(li.p, result)) <= distance {
            continue;
        }
        proj.clear();
        proj.extend_from_slice(&lines[..hard]);
        for lj in &lines[hard..i] {
            let den = det(li.d, lj.d);
            let point = if den.abs() <= PARALLEL {
                if dot(li.d, lj.d) > 0 {
                    continue;
                }
                [div_round(li.p[0] + lj.p[0], 2), div_round(li.p[1] + lj.p[1], 2)]
            } else {
                let t = div_round(det(lj.d, sub(li.p, lj.p)) * unit, den);
                add(li.p, scale(li.d, t, unit))
            };
            // The bisector direction, not normalized: `lp1` is exact for any length,
            // and every side test is a sign. Nonzero: parallel same-direction pairs left.
            proj.push(Line {
                p: point,
                d: sub(lj.d, li.d),
            });
        }
        let (fail, r) = lp2(proj, radius, left(li.d), true, tie);
        if fail == proj.len() {
            result = r;
        }
        distance = det(li.d, sub(li.p, result));
    }
    result
}

// ---------------------------------------------------------------------------
// Entry.

/// Solve with the LP: `prefs[0]` is the (head-on biased) preference in mm/s the program
/// steers toward, `prefs[1]` the host's preference that sizes the speed circle,
/// `order` the retained neighbors in `(distance_squared, id)` order.
pub(super) fn solve(
    lp: &mut LpScratch,
    me: &OrcaBody,
    prefs: [[i64; 2]; 2],
    order: &[OrderKey],
    bodies: &[OrcaBody],
    segments: &[OrcaSegment],
    settings: &OrcaSettings,
) -> [i64; 2] {
    let q = Q as i128;
    let unit = UNIT as i128;
    let LpScratch {
        planes,
        work,
        proj,
        stats,
        last,
    } = lp;
    let [pref, host_pref] = prefs;
    let host = [i128::from(host_pref[0]), i128::from(host_pref[1])];
    let cap = i128::from(settings.max_speed_mm_s).max(isqrt_ceil(dot(host, host) as u128) as i128);
    let v = [i128::from(me.vx_mm_s) * q, i128::from(me.vz_mm_s) * q];
    work.clear();
    for s in segments {
        match obstacle_line(me, v, s, cap, settings) {
            Some(l) => work.push(l),
            None => stats.skipped_far += 1,
        }
    }
    let hard = work.len();
    for (_, _, i) in order {
        match agent_line(me, v, &bodies[*i], cap, settings) {
            Some(l) => work.push(l),
            None => stats.skipped_far += 1,
        }
    }
    stats.obstacle_planes += hard as u64;
    stats.agent_planes += (work.len() - hard) as u64;
    planes.clear();
    for (k, l) in work.iter_mut().enumerate() {
        planes.push(OrcaHalfPlane {
            point_q: [l.p[0] as i64, l.p[1] as i64],
            direction: [l.d[0] as i64, l.d[1] as i64],
            obstacle: k < hard,
        });
        // Solve against the plane shifted inward by the margin.
        l.p = add(l.p, scale(left(l.d), MARGIN, unit));
    }

    let radius = (cap * q - 2).max(0);
    let opt = [i128::from(pref[0]) * q, i128::from(pref[1]) * q];
    let (fail, mut result) = lp2(work, radius, opt, false, opt);
    let feasible = fail == work.len();
    if !feasible && fail < hard {
        // The walls conflict among themselves. If they only conflict through the margin
        // (the unshifted walls still leave a point), keep them hard, unshifted, and let
        // the neighbors be the soft lines (ADR-8703). Otherwise every line is soft
        // (ADR-8555). The unshifted points are the recorded planes, exact in `i64`.
        for (l, p) in work[..hard].iter_mut().zip(planes.iter()) {
            l.p = [i128::from(p.point_q[0]), i128::from(p.point_q[1])];
        }
        let (walls_fail, _) = lp2(&work[..hard], radius, opt, false, opt);
        if walls_fail == hard {
            let (fail2, r2) = lp2(work, radius, opt, false, opt);
            result = if fail2 == work.len() {
                r2
            } else {
                lp3(work, proj, hard, fail2, radius, r2, opt)
            };
        } else {
            for l in &mut work[..hard] {
                l.p = add(l.p, scale(left(l.d), MARGIN, unit));
            }
            result = lp3(work, proj, 0, fail, radius, result, opt);
        }
    } else if !feasible {
        result = lp3(work, proj, hard, fail, radius, result, opt);
    }
    let mut out = [div_round(result[0], q), div_round(result[1], q)];
    if dot(out, out) > cap * cap {
        out = [result[0] / q, result[1] / q];
    }

    // Worst penetration of the unshifted planes, mm/s rounded up.
    let x = [out[0] * q, out[1] * q];
    let worst = planes
        .iter()
        .map(|p| {
            let pp = [i128::from(p.point_q[0]), i128::from(p.point_q[1])];
            let dd = [i128::from(p.direction[0]), i128::from(p.direction[1])];
            -det(dd, sub(x, pp))
        })
        .fold(0_i128, i128::max);
    let worst_mm_s = div_ceil(worst, unit * q) as i64;

    stats.calls += 1;
    if feasible {
        stats.feasible += 1;
    } else {
        stats.fallbacks += 1;
    }
    stats.worst_violation_mm_s = stats.worst_violation_mm_s.max(worst_mm_s as u64);
    *last = Some(OrcaLpOutcome {
        feasible,
        worst_violation_mm_s: worst_mm_s,
        planes: planes.len() as u32,
    });
    [out[0] as i64, out[1] as i64]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounding_helpers() {
        assert_eq!(div_round(5, 2), 3);
        assert_eq!(div_round(-5, 2), -3);
        assert_eq!(div_round(5, -2), -3);
        assert_eq!(div_round(4, 3), 1);
        assert_eq!(div_floor(-5, 2), -3);
        assert_eq!(div_floor(5, -2), -3);
        assert_eq!(div_floor(4, 2), 2);
        assert_eq!(div_ceil(5, 2), 3);
        assert_eq!(div_ceil(-5, 2), -2);
        assert_eq!(div_ceil(-5, -2), 3);
    }

    /// `n / 1` is exactly `n` for every `i128`, including `i128::MIN`, whose magnitude
    /// `2^127` does not fit `i128` (det-orca-walls verification nit: the shift path negated
    /// it). Other power-of-two divisors halve the magnitude first, so they never reach it.
    #[test]
    fn rounding_by_one_keeps_every_i128() {
        for n in [i128::MIN, i128::MIN + 1, -1, 0, 1, i128::MAX] {
            assert_eq!(div_round(n, 1), n, "{n} / 1");
        }
        assert_eq!(div_round(i128::MIN, 2), i128::MIN / 2);
        assert_eq!(div_round(i128::MIN, 1 << 126), -2);
    }

    #[test]
    fn power_of_two_rounding_matches_division() {
        let mut x = 0x2545_f491_4f6c_dd1d_u64;
        for _ in 0..20_000 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let n = (i128::from(x as i64) << (x % 40)) >> 3;
            for d in [1_i128, 2, 256, UNIT as i128, 1 << 40] {
                let q = n / d;
                let r = n - q * d;
                let want = if 2 * r.unsigned_abs() >= d.unsigned_abs() {
                    if n < 0 { q - 1 } else { q + 1 }
                } else {
                    q
                };
                assert_eq!(div_round(n, d), want, "{n} / {d}");
                assert_eq!(div_round(-n, d), -want, "-{n} / {d}");
            }
        }
    }

    /// Wide division, never the `i64` quotient. `None` when `n / d` overflows `i128`
    /// (`i128::MIN / -1`), which neither path can compute.
    fn wide_qr(n: i128, d: i128) -> Option<(i128, i128)> {
        if d == 0 || (n == i128::MIN && d == -1) {
            return None;
        }
        let q = n / d;
        Some((q, n - q * d))
    }

    fn wide_div_round(n: i128, d: i128) -> Option<i128> {
        let (q, r) = wide_qr(n, d)?;
        Some(if 2 * r.unsigned_abs() >= d.unsigned_abs() {
            if (n < 0) == (d < 0) { q + 1 } else { q - 1 }
        } else {
            q
        })
    }

    fn wide_div_floor(n: i128, d: i128) -> Option<i128> {
        let (q, r) = wide_qr(n, d)?;
        Some(if r != 0 && ((n < 0) != (d < 0)) { q - 1 } else { q })
    }

    fn wide_div_ceil(n: i128, d: i128) -> Option<i128> {
        let (q, r) = wide_qr(n, d)?;
        Some(if r != 0 && ((n < 0) == (d < 0)) { q + 1 } else { q })
    }

    /// The `i64` quotient and the power-of-two shift must match wide division on every
    /// input those paths can return, including the `i64` boundary the 20,000-draw
    /// corpus never reaches (`i64::MIN`, `i64::MAX + 1`, values past 2^63).
    #[test]
    fn fast_division_matches_wide_division_past_the_i64_boundary() {
        let mut values = vec![
            0,
            1,
            -1,
            2,
            -2,
            3,
            -3,
            i64::MAX as i128,
            i64::MIN as i128,
            i64::MAX as i128 + 1,
            i64::MIN as i128 - 1,
            i128::MAX,
            i128::MAX / 2,
            i128::MIN / 2,
            (1_i128 << 80) + 3,
            -((1_i128 << 80) + 3),
            (1_i128 << 100) - 1,
            1 - (1_i128 << 100),
        ];
        let mut x = 0x1234_5678_9abc_def0_u64;
        for _ in 0..4_000 {
            x = x
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let shift = (x % 90) as u32;
            let n = i128::from(x as i64).wrapping_shl(shift);
            values.push(n);
            values.push(n.wrapping_add(1));
            values.push(n.wrapping_sub(1));
        }
        let mut divisors = vec![
            1_i128,
            -1,
            2,
            -2,
            3,
            -3,
            256,
            -256,
            UNIT as i128,
            1 << 40,
            -(1 << 40),
            1 << 62,
            i64::MAX as i128,
            i64::MIN as i128,
            i64::MAX as i128 + 1,
            1 << 80,
            7,
            -11,
            1_000_003,
        ];
        for k in 0..60 {
            divisors.push(1_i128 << k);
            divisors.push(-(1_i128 << k));
        }
        for &n in &values {
            for &d in &divisors {
                // `div_round(i128::MIN, 1)` takes the power-of-two shift. The magnitude
                // 2^127 does not fit back in `i128`, and the following negation panics
                // in debug. Wide division returns `i128::MIN`. No in-domain call passes
                // this pair (denominators are 2, Q, UNIT, or a length at least 1 mm).
                if n == i128::MIN && d == 1 {
                    continue;
                }
                let Some(want) = wide_div_round(n, d) else {
                    continue;
                };
                assert_eq!(div_round(n, d), want, "div_round {n} / {d}");
                let Some(floor) = wide_div_floor(n, d) else {
                    continue;
                };
                assert_eq!(div_floor(n, d), floor, "div_floor {n} / {d}");
                let Some(ceil) = wide_div_ceil(n, d) else {
                    continue;
                };
                assert_eq!(div_ceil(n, d), ceil, "div_ceil {n} / {d}");
            }
        }
    }

    #[test]
    fn roots_are_exact_floors() {
        for v in [
            0_u128,
            1,
            2,
            3,
            4,
            15,
            16,
            17,
            u128::from(u64::MAX),
            (1 << 100) + 7,
            u128::MAX >> 2,
        ] {
            let s = isqrt(v);
            assert!(s * s <= v && (s + 1) * (s + 1) > v, "{v}");
            let c = isqrt_ceil(v);
            assert!(c * c >= v && (c == 0 || (c - 1) * (c - 1) < v), "{v}");
        }
    }

    #[test]
    fn units_have_unit_length() {
        for v in [
            [1, 0],
            [0, -3],
            [3, 4],
            [-7, 1_000_000_007],
            [1 << 60, -(1 << 59)],
            [5, 5],
        ] {
            let u = unit(v).expect("nonzero");
            let len2 = dot(u, u);
            let target = (UNIT as i128) * (UNIT as i128);
            assert!((len2 - target).abs() <= 4 * UNIT as i128, "{v:?} -> {u:?}");
            // Parallel to v within rounding: |det(u, v)| <= 2 |v| (one unit per component).
            let lenv = isqrt_ceil(dot(v, v) as u128);
            assert!(det(u, v).unsigned_abs() <= 2 * lenv, "{v:?}");
        }
        assert!(unit([0, 0]).is_none());
    }

    #[test]
    fn clip_keeps_the_part_inside_the_box() {
        let (a, b) = clip([-100, 5], [100, 5], 10).expect("crosses");
        assert_eq!((a, b), ([-10, 5], [10, 5]));
        assert!(clip([20, 20], [30, 40], 10).is_none());
        assert_eq!(clip([1, 1], [2, 2], 10), Some(([1, 1], [2, 2])));
    }

    #[test]
    fn disc_obstacle_matches_the_cutoff_and_legs() {
        // Disc 10 m ahead, radius 1 m, horizon 2 s: the cutoff disc is at 5 m/s with radius 0.5 m/s.
        let q = Q as i128;
        let p = [10_000, 0];
        // A relative velocity straight at it, slower than the cutoff: nearest point is on the cutoff arc.
        let w = [4_000 * q, 0];
        let (pt, n) = nearest_on_vo(p, p, p, 1_000, 2_000, w).expect("vo");
        assert_eq!(pt, [4_500 * q, 0]);
        assert_eq!(n, [-(UNIT as i128), 0]);
        // Far beyond the cutoff, straight at it: nearest point is on a leg.
        let w = [9_000 * q, 0];
        let (pt, n) = nearest_on_vo(p, p, p, 1_000, 2_000, w).expect("vo");
        assert!(pt[1] != 0 && n[1] != 0, "{pt:?} {n:?}");
    }
}
