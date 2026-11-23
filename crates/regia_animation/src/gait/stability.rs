// SPDX-License-Identifier: MPL-2.0
//! Static stability margin (ADR-8653): the signed distance on XZ from the body center to the
//! support polygon of the planted feet (the convex hull of their XZ points), after McGhee
//! and Frank 1968. Positive inside the polygon (the distance to its nearest edge), negative
//! outside it (minus the distance to the polygon), zero on its boundary. With one or two
//! planted feet the "polygon" is a point or a segment, so the margin is never positive:
//! a trot, a pace or a bound is statically unstable by design and balances dynamically (a
//! bound also has flight phases with no planted foot; the stepper then reports the finite
//! `NO_SUPPORT_MARGIN`, ADR-8736).

use super::pattern::MAX_LEGS;

/// Signed support margin of `center` against the convex hull of `points` (XZ meters); at
/// most [`MAX_LEGS`] points are read. No point gives negative infinity (no support). No
/// allocation.
#[must_use]
pub fn support_margin(points: &[[f32; 2]], center: [f32; 2]) -> f32 {
    let n = points.len().min(MAX_LEGS);
    if n == 0 {
        return f32::NEG_INFINITY;
    }
    let mut sorted = [[0.0_f32; 2]; MAX_LEGS];
    sorted[..n].copy_from_slice(&points[..n]);
    // Insertion sort by x, then z.
    for k in 1..n {
        let mut m = k;
        while m > 0 && before(sorted[m], sorted[m - 1]) {
            sorted.swap(m - 1, m);
            m -= 1;
        }
    }
    // Andrew's monotone chain, counter-clockwise, collinear points dropped.
    let mut hull = [[0.0_f32; 2]; 2 * MAX_LEGS];
    let mut h = 0;
    for &p in &sorted[..n] {
        while h >= 2 && cross(hull[h - 2], hull[h - 1], p) <= 0.0 {
            h -= 1;
        }
        hull[h] = p;
        h += 1;
    }
    let lower = h + 1;
    for &p in sorted[..n.saturating_sub(1)].iter().rev() {
        while h >= lower && cross(hull[h - 2], hull[h - 1], p) <= 0.0 {
            h -= 1;
        }
        hull[h] = p;
        h += 1;
    }
    // The chain ends where it began; drop the repeat (one point stays one point).
    if h > 1 {
        h -= 1;
    }
    let hull = &hull[..h];
    if hull.len() < 3 {
        let a = hull[0];
        let b = hull[hull.len() - 1];
        return -segment_distance(a, b, center);
    }
    let mut inside = true;
    let mut nearest = f32::INFINITY;
    for (k, &a) in hull.iter().enumerate() {
        let b = hull[(k + 1) % hull.len()];
        inside &= cross(a, b, center) >= 0.0;
        nearest = nearest.min(segment_distance(a, b, center));
    }
    if inside { nearest } else { -nearest }
}

fn before(a: [f32; 2], b: [f32; 2]) -> bool {
    a[0] < b[0] || (a[0] == b[0] && a[1] < b[1])
}

/// Twice the signed area of `o, a, b`: positive when `b` is left of `o -> a`.
fn cross(o: [f32; 2], a: [f32; 2], b: [f32; 2]) -> f32 {
    (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
}

/// Distance from `p` to the segment `a b` (a point when `a == b`).
fn segment_distance(a: [f32; 2], b: [f32; 2], p: [f32; 2]) -> f32 {
    let (dx, dz) = (b[0] - a[0], b[1] - a[1]);
    let length2 = dx * dx + dz * dz;
    let t = if length2 > 0.0 {
        (((p[0] - a[0]) * dx + (p[1] - a[1]) * dz) / length2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let (ex, ez) = (a[0] + dx * t - p[0], a[1] + dz * t - p[1]);
    (ex * ex + ez * ez).sqrt()
}

#[cfg(test)]
mod tests {
    use super::support_margin;

    #[test]
    fn square_triangle_segment_point() {
        let square = [[1.0, 1.0], [-1.0, 1.0], [-1.0, -1.0], [1.0, -1.0], [0.0, 0.5]];
        assert_eq!(support_margin(&square, [0.0, 0.0]), 1.0);
        assert_eq!(support_margin(&square, [0.5, 0.0]), 0.5);
        assert_eq!(support_margin(&square, [3.0, 0.0]), -2.0);
        assert_eq!(support_margin(&square, [1.0, 0.0]), 0.0);
        let segment = [[0.0, 0.0], [2.0, 0.0]];
        assert_eq!(support_margin(&segment, [1.0, 0.0]), 0.0);
        assert_eq!(support_margin(&segment, [1.0, 0.5]), -0.5);
        assert_eq!(support_margin(&[[1.0, 1.0]], [1.0, 4.0]), -3.0);
        assert_eq!(support_margin(&[], [0.0, 0.0]), f32::NEG_INFINITY);
        let collinear = [[0.0, 0.0], [1.0, 0.0], [2.0, 0.0]];
        assert_eq!(support_margin(&collinear, [1.0, -1.0]), -1.0);
    }
}
