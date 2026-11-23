// SPDX-License-Identifier: MPL-2.0
//! Wire hit tests of the Material Graph canvas (spec 80 § 12.1, w11): which drawn wire the
//! pointer is on, and where on it. egui-snarl 0.12 has no wire hook, so the canvas tests the
//! curves it knows were drawn:
//!
//! - a **plain wire** is the cubic Bézier the canvas asks snarl to draw (its style is set to
//!   `Bezier3` with the frame [`WIRE_FRAME`]): [`wire_curve`] gives its four control points;
//! - a **rerouted wire** is the canvas's own polyline ([`super::canvas::reroute_path`]).
//!
//! The distance to a cubic Bézier is found by adaptive de Casteljau subdivision: a piece whose
//! inner control points lie within [`FLATNESS`] of its chord is measured as that chord (a line
//! segment), otherwise it is split at t = ½ and both halves are measured; the nearer one wins.
//! Pure functions: the panel feeds them the pins snarl placed.

use egui::{Pos2, pos2};

/// How far a plain wire runs level out of its output pin and into its input pin before it bends
/// (canvas points): snarl's wire frame, which the canvas sets so the curve here is the one drawn.
/// Shorter wires bend sooner (a sixth of the pin-to-pin distance, snarl's frame downscale).
pub const WIRE_FRAME: f32 = 30.0;

/// Pointer distance (canvas points) within which a click is on a wire.
pub const WIRE_HIT: f32 = 6.0;

/// Chord distance (canvas points) at which a Bézier piece counts as straight.
pub const FLATNESS: f32 = 0.1;

/// Most subdivision levels (a piece 2^-16 of the curve is short whatever its shape).
const MAX_DEPTH: u32 = 16;

/// The cubic Bézier control points of the plain wire from an output pin at `from` to an input
/// pin at `to`: level out of `from` and into `to` by the frame ([`WIRE_FRAME`], at most a sixth
/// of the pin distance).
#[must_use]
pub fn wire_curve(from: Pos2, to: Pos2) -> [Pos2; 4] {
    let frame = WIRE_FRAME.min((from - to).length() / 6.0);
    [from, pos2(from.x + frame, from.y), pos2(to.x - frame, to.y), to]
}

/// The point of a cubic Bézier at parameter `t` (0..1).
#[must_use]
pub fn bezier_point(c: &[Pos2; 4], t: f32) -> Pos2 {
    let u = 1.0 - t;
    let v = c[0].to_vec2() * (u * u * u)
        + c[1].to_vec2() * (3.0 * u * u * t)
        + c[2].to_vec2() * (3.0 * u * t * t)
        + c[3].to_vec2() * (t * t * t);
    v.to_pos2()
}

/// Distance from `p` to the segment `a`..`b`, and the segment parameter (0..1) of the nearest
/// point.
#[must_use]
pub fn segment_distance(p: Pos2, a: Pos2, b: Pos2) -> (f32, f32) {
    let ab = b - a;
    let len2 = ab.length_sq();
    let s = if len2 > 0.0 {
        ((p - a).dot(ab) / len2).clamp(0.0, 1.0)
    } else {
        0.0
    };
    ((a + ab * s - p).length(), s)
}

/// Splits a cubic Bézier at t = ½ (de Casteljau): the two halves.
fn split(c: &[Pos2; 4]) -> ([Pos2; 4], [Pos2; 4]) {
    let mid = |a: Pos2, b: Pos2| a + (b - a) * 0.5;
    let (p01, p12, p23) = (mid(c[0], c[1]), mid(c[1], c[2]), mid(c[2], c[3]));
    let (p012, p123) = (mid(p01, p12), mid(p12, p23));
    let m = mid(p012, p123);
    ([c[0], p01, p012, m], [m, p123, p23, c[3]])
}

/// Whether the inner control points lie within [`FLATNESS`] of the chord (the convex hull is
/// then that thin around the curve).
fn flat(c: &[Pos2; 4]) -> bool {
    segment_distance(c[1], c[0], c[3]).0 <= FLATNESS && segment_distance(c[2], c[0], c[3]).0 <= FLATNESS
}

/// Distance from `p` to the cubic Bézier `c`, and the curve parameter (0..1) of the nearest
/// point, by adaptive subdivision. Pieces whose control hull is farther than the best distance
/// found so far are skipped (the curve lies inside its hull).
#[must_use]
pub fn bezier_distance(p: Pos2, c: &[Pos2; 4]) -> (f32, f32) {
    let mut best = (f32::INFINITY, 0.0);
    let mut stack = vec![(*c, 0.0f32, 1.0f32, 0u32)];
    while let Some((piece, t0, t1, depth)) = stack.pop() {
        let hull = egui::Rect::from_points(&piece);
        let outside = {
            let dx = (hull.min.x - p.x).max(p.x - hull.max.x).max(0.0);
            let dy = (hull.min.y - p.y).max(p.y - hull.max.y).max(0.0);
            dx.hypot(dy)
        };
        if outside > best.0 {
            continue;
        }
        if depth >= MAX_DEPTH || flat(&piece) {
            let (d, s) = segment_distance(p, piece[0], piece[3]);
            if d < best.0 {
                best = (d, t0 + (t1 - t0) * s);
            }
            continue;
        }
        let (left, right) = split(&piece);
        let tm = 0.5 * (t0 + t1);
        stack.push((right, tm, t1, depth + 1));
        stack.push((left, t0, tm, depth + 1));
    }
    best
}

/// Distance from `p` to a polyline, and the index of the nearest segment (`path[i]..path[i+1]`).
/// `None` for fewer than two points.
#[must_use]
pub fn polyline_distance(p: Pos2, path: &[Pos2]) -> Option<(f32, usize)> {
    path.windows(2)
        .enumerate()
        .map(|(i, w)| (segment_distance(p, w[0], w[1]).0, i))
        .min_by(|a, b| a.0.total_cmp(&b.0))
}

/// One wire as the canvas drew it this frame (graph space).
#[derive(Clone, Debug, PartialEq)]
pub struct DrawnWire {
    /// The node whose input the wire feeds.
    pub to: String,
    /// That input slot.
    pub slot: String,
    /// The curve.
    pub shape: WireShape,
}

/// The drawn form of a wire.
#[derive(Clone, Debug, PartialEq)]
pub enum WireShape {
    /// A plain snarl wire: its cubic Bézier ([`wire_curve`]).
    Plain([Pos2; 4]),
    /// A rerouted wire: the canvas's polyline through its points
    /// ([`super::canvas::reroute_path`]), with [`super::canvas::REROUTE_SAMPLES`] samples per span.
    Rerouted(Vec<Pos2>),
}

/// Where a pointer hit a wire.
#[derive(Clone, Debug, PartialEq)]
pub struct WireHit {
    /// Index into the wires tested.
    pub index: usize,
    /// Distance from the pointer (canvas points).
    pub distance: f32,
    /// The nearest point on the wire (graph space): where a new reroute point goes.
    pub at: Pos2,
    /// For a rerouted wire, the span hit: the new point goes after this many points.
    pub span: usize,
}

/// The wire nearest to `p` within `threshold` (canvas points), if any; ties go to the first.
#[must_use]
pub fn hit_wire(p: Pos2, wires: &[DrawnWire], threshold: f32) -> Option<WireHit> {
    let mut best: Option<WireHit> = None;
    for (index, wire) in wires.iter().enumerate() {
        let (distance, at, span) = match &wire.shape {
            WireShape::Plain(c) => {
                let (d, t) = bezier_distance(p, c);
                (d, bezier_point(c, t), 0)
            }
            WireShape::Rerouted(path) => {
                let Some((d, segment)) = polyline_distance(p, path) else {
                    continue;
                };
                let (a, b) = (path[segment], path[segment + 1]);
                let (_, s) = segment_distance(p, a, b);
                (d, a + (b - a) * s, segment / super::canvas::REROUTE_SAMPLES)
            }
        };
        if distance <= threshold && best.as_ref().is_none_or(|b| distance < b.distance) {
            best = Some(WireHit {
                index,
                distance,
                at,
                span,
            });
        }
    }
    best
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_curve_leaves_and_enters_level_and_ends_on_the_pins() {
        let (from, to) = (pos2(0.0, 0.0), pos2(400.0, 120.0));
        let c = wire_curve(from, to);
        assert_eq!(bezier_point(&c, 0.0), from);
        assert_eq!(bezier_point(&c, 1.0), to);
        assert_eq!(c[1].y, from.y, "level out of the output pin");
        assert_eq!(c[2].y, to.y, "level into the input pin");
        assert!(c[1].x > from.x && c[2].x < to.x);
        // A short wire bends sooner: a sixth of the pin distance.
        let short = wire_curve(pos2(0.0, 0.0), pos2(60.0, 0.0));
        assert!((short[1].x - 10.0).abs() < 1e-4, "{short:?}");
    }

    /// Every sampled point of the curve is at distance ~0, at its own parameter; a point off
    /// the curve measures to a brute-force minimum over 10 000 samples.
    #[test]
    fn subdivision_distance_matches_dense_sampling() {
        let c = wire_curve(pos2(10.0, 20.0), pos2(310.0, -140.0));
        for k in 0..=20 {
            let t = k as f32 / 20.0;
            // Within the flatness bound: a flat piece's chord is at most `FLATNESS` from it.
            let (d, at) = bezier_distance(bezier_point(&c, t), &c);
            assert!(d <= FLATNESS + 1e-3, "t {t}: {d}");
            // The parameter is the chord's, linear in the flat piece: its point is within a
            // canvas point of the curve point.
            assert!(
                (bezier_point(&c, at) - bezier_point(&c, t)).length() < 1.0,
                "t {t} → {at}"
            );
        }
        for p in [
            pos2(150.0, 0.0),
            pos2(-40.0, 25.0),
            pos2(400.0, -200.0),
            pos2(160.0, -60.0),
        ] {
            let brute = (0..=10_000)
                .map(|k| (bezier_point(&c, k as f32 / 10_000.0) - p).length())
                .fold(f32::INFINITY, f32::min);
            let (d, _) = bezier_distance(p, &c);
            assert!((d - brute).abs() <= FLATNESS + 0.01, "{p:?}: {d} vs {brute}");
        }
    }

    #[test]
    fn the_nearest_wire_within_the_threshold_is_hit() {
        let plain = DrawnWire {
            to: "m".into(),
            slot: "a".into(),
            shape: WireShape::Plain(wire_curve(pos2(0.0, 0.0), pos2(300.0, 0.0))),
        };
        let rerouted_path =
            super::super::canvas::reroute_path(pos2(0.0, 200.0), &[pos2(150.0, 300.0)], pos2(300.0, 200.0));
        let rerouted = DrawnWire {
            to: "m".into(),
            slot: "b".into(),
            shape: WireShape::Rerouted(rerouted_path.clone()),
        };
        let wires = [plain, rerouted];
        let hit = hit_wire(pos2(150.0, 3.0), &wires, WIRE_HIT).expect("on the plain wire");
        assert_eq!(hit.index, 0);
        assert!((hit.at - pos2(150.0, 0.0)).length() < 0.5, "{hit:?}");
        assert!(
            hit_wire(pos2(150.0, 40.0), &wires, WIRE_HIT).is_none(),
            "between the wires"
        );
        // The rerouted wire: a point on its second span (after the reroute point).
        let on_second = rerouted_path[super::super::canvas::REROUTE_SAMPLES + 4];
        let hit = hit_wire(on_second, &wires, WIRE_HIT).expect("on the rerouted wire");
        assert_eq!((hit.index, hit.span), (1, 1), "{hit:?}");
        let on_first = rerouted_path[3];
        assert_eq!(hit_wire(on_first, &wires, WIRE_HIT).map(|h| h.span), Some(0));
    }
}
