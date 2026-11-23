// SPDX-License-Identifier: MPL-2.0
//! Reroute points (ADR-9970; P-186): the curve a rerouted wire takes from its output pin
//! through its points to its input pin, and the size of a point's grip. Pure functions; a panel
//! draws and drags with them.

use egui::{Pos2, vec2};

/// Samples per span of a rerouted wire (between two consecutive points).
pub const REROUTE_SAMPLES: usize = 12;

/// How far a rerouted wire runs straight out of its output pin and into its input pin before
/// it bends (canvas points): the phantom end knots sit this far out, level with the pins.
pub const REROUTE_LEAD: f32 = 60.0;

/// Radius of a reroute point's grip (canvas points).
pub const REROUTE_GRIP: f32 = 7.0;

/// The polyline of a wire from an output pin at `from` through `points` (in order) to an input
/// pin at `to`: a uniform Catmull-Rom spline through the knots `from, points…, to`, with
/// phantom end knots [`REROUTE_LEAD`] out level with each pin, so the wire leaves its output to
/// the right and enters its input from the left. Every knot is a vertex of the result exactly;
/// [`REROUTE_SAMPLES`] - 1 samples lie between consecutive knots.
#[must_use]
pub fn reroute_path(from: Pos2, points: &[Pos2], to: Pos2) -> Vec<Pos2> {
    let mut knots = Vec::with_capacity(points.len() + 4);
    knots.push(from - vec2(REROUTE_LEAD, 0.0));
    knots.push(from);
    knots.extend_from_slice(points);
    knots.push(to);
    knots.push(to + vec2(REROUTE_LEAD, 0.0));
    let spans = knots.len() - 3;
    let mut out = Vec::with_capacity(spans * REROUTE_SAMPLES + 1);
    for window in knots.windows(4) {
        let [p0, p1, p2, p3] = [window[0], window[1], window[2], window[3]];
        out.push(p1);
        for k in 1..REROUTE_SAMPLES {
            out.push(catmull_rom(p0, p1, p2, p3, k as f32 / REROUTE_SAMPLES as f32));
        }
    }
    out.push(to);
    out
}

/// The uniform Catmull-Rom segment from `p1` (t = 0) to `p2` (t = 1).
fn catmull_rom(p0: Pos2, p1: Pos2, p2: Pos2, p3: Pos2, t: f32) -> Pos2 {
    let (a, b, c, d) = (p0.to_vec2(), p1.to_vec2(), p2.to_vec2(), p3.to_vec2());
    let t2 = t * t;
    let t3 = t2 * t;
    let v =
        (b * 2.0 + (c - a) * t + (a * 2.0 - b * 5.0 + c * 4.0 - d) * t2 + (b * 3.0 - a - c * 3.0 + d) * t3)
            * 0.5;
    v.to_pos2()
}
