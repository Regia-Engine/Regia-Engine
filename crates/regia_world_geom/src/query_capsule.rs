// SPDX-License-Identifier: MPL-2.0
//! The capsule wall push and its vertical rule (spec 121 § 5.4; lane W-30, ADR-8323).
//!
//! T3 settles the disc push only. The capsule's vertical rule is Regia's (spec 121 § 5.4: "W-30
//! settles it with its vectors"): the rounded ends of a vertical capsule meet floors and roofs that
//! cross the body inside its radius. The rule is original code; its geometry is the textbook
//! sphere-against-plane contact (provenance row "capsule vertical rule").

use crate::WorldGeom;
use crate::query::{Band, GeomState, Push, SpanBuf};
use regia_detsim::float::{det_clamp, det_max, det_min};

/// `1 - sqrt(3) / 2`: how far the capsule's rounded end rises over its lowest point at half the
/// radius from the axis (the f32 nearest the exact value; a literal, so no square root runs).
pub const HALF_RADIUS_RISE: f32 = 0.133_974_6;

/// The capsule's sample offsets, as multiples of the radius, in this fixed order: the axis, then
/// eight points at half the radius, counter-clockwise from +X. The third column is the end's rise
/// at that distance, as a multiple of the radius.
const SAMPLES: [[f32; 3]; 9] = [
    [0.0, 0.0, 0.0],
    [0.5, 0.0, HALF_RADIUS_RISE],
    [0.353_553_38, 0.353_553_38, HALF_RADIUS_RISE],
    [0.0, 0.5, HALF_RADIUS_RISE],
    [-0.353_553_38, 0.353_553_38, HALF_RADIUS_RISE],
    [-0.5, 0.0, HALF_RADIUS_RISE],
    [-0.353_553_38, -0.353_553_38, HALF_RADIUS_RISE],
    [0.0, -0.5, HALF_RADIUS_RISE],
    [0.353_553_38, -0.353_553_38, HALF_RADIUS_RISE],
];

impl WorldGeom {
    /// A vertical capsule of `radius` with its feet at `band.lo` and its head at `band.hi`: the
    /// disc push of [`WorldGeom::push_disc`] over the band, then the vertical rule of
    /// [`WorldGeom::clamp_capsule`] at the pushed centre. Returns the pushed centre and the clamped
    /// band. A non-finite input, `radius <= 0` or `band.lo > band.hi` returns the input unchanged.
    #[must_use]
    pub fn push_capsule(&self, state: &GeomState, x: f32, z: f32, radius: f32, band: Band) -> (Push, Band) {
        let push = self.push_disc(state, x, z, radius, band);
        let band = self.clamp_capsule(state, push.xz[0], push.xz[1], radius, band);
        (push, band)
    }

    /// The capsule's vertical rule alone (ADR-8323), with no wall push. The radius is clamped to
    /// [0.05, `wall_pad_m`] as the disc push clamps it. The column is read at the axis and at eight
    /// points at half the radius ([`HALF_RADIUS_RISE`]), in a fixed order:
    ///
    /// - **feet over floors**: a floor (the ground included) above the rounded foot at a sample and
    ///   no higher than `band.lo + radius` (or the band's middle, if lower) lifts the feet until the
    ///   foot rests on it; the highest lift wins;
    /// - **head under roofs**: a roof under the rounded head at a sample and no lower than
    ///   `band.hi - radius` (or the band's middle, if higher) lowers the head the same way; the lowest
    ///   head wins.
    ///
    /// A surface further inside the band than one radius is not the capsule's end: it is an obstacle,
    /// and the mover's step and body-band tests answer it. When the roof leaves less room than the
    /// lifted feet, the head is set to the feet (`hi == lo`): the capsule does not fit there. Bad
    /// input returns `band` unchanged. Allocates nothing.
    #[must_use]
    pub fn clamp_capsule(&self, state: &GeomState, x: f32, z: f32, radius: f32, band: Band) -> Band {
        if ![x, z, radius, band.lo, band.hi].iter().all(|v| v.is_finite())
            || radius <= 0.0
            || band.lo > band.hi
            || self.settings().wall_pad_m < 0.05
        {
            return band;
        }
        let r = det_clamp(radius, 0.05, self.settings().wall_pad_m);
        let mid = band.lo + (band.hi - band.lo) * 0.5;
        let foot_top = det_min(band.lo + r, mid);
        let head_low = det_max(band.hi - r, mid);
        let mut lo = band.lo;
        let mut hi = band.hi;
        let mut spans = SpanBuf::default();
        for [ox, oz, rise] in SAMPLES {
            let sx = x + ox * r;
            let sz = z + oz * r;
            let end = rise * r;
            self.floors_at(state, sx, sz, &mut spans);
            for span in spans.spans() {
                let f = span.floor.y;
                if f > band.lo + end && f <= foot_top {
                    lo = det_max(lo, f - end);
                }
                if let Some(lid) = span.ceiling {
                    let c = lid.y;
                    if c < band.hi - end && c >= head_low {
                        hi = det_min(hi, c + end);
                    }
                }
            }
        }
        Band {
            lo,
            hi: det_max(hi, lo),
        }
    }
}
