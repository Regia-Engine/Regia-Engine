// SPDX-License-Identifier: MPL-2.0
//! Float-test scoring math (`docs/specs/33-spatial-queries.md` § 3.5): clamp, normalize, peak,
//! shape, and the multi-value combination. Pure functions over `f32`.

use super::asset::{Combine, Origin, Shape};

/// Resolved score settings of one float test.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreCurve {
    /// Lower clamp bound.
    pub lo: f32,
    /// Upper clamp bound.
    pub hi: f32,
    /// Normalization origin.
    pub origin: Origin,
    /// Peak position, if any.
    pub peak_at: Option<f32>,
    /// Curve shape.
    pub shape: Shape,
}

impl ScoreCurve {
    /// The shaped value of the raw value `v`, in 0..=1 (§ 3.5 steps 2-5).
    ///
    /// `n` is clamped to 0..=1 before the shape is applied, so `Root` never sees a negative
    /// argument and every contribution stays within the weight.
    pub fn shaped(&self, v: f32) -> f32 {
        // A reversed pair (for example `Fixed(10)` and `Fixed(0)`) is read as the same interval
        // the other way round: `f32::clamp` would panic, and `max.min` would pin every value to `hi`.
        let (lo, hi) = if self.lo > self.hi {
            (self.hi, self.lo)
        } else {
            (self.lo, self.hi)
        };
        let v_c = v.max(lo).min(hi);
        let mut n = match self.origin {
            Origin::Floor => {
                let den = hi - lo;
                if den == 0.0 { 0.0 } else { (v_c - lo) / den }
            }
            Origin::Zero => {
                if hi == 0.0 {
                    0.0
                } else {
                    v_c / hi
                }
            }
        };
        if let Some(r) = self.peak_at {
            let den = (lo - r).abs().max((hi - r).abs());
            n = if den == 0.0 {
                1.0
            } else {
                1.0 - (v_c - r).abs() / den
            };
        }
        apply_shape(self.shape, n.clamp(0.0, 1.0))
    }

    /// The shaped value an unreachable item takes for a weight's sign: the curve's worst end.
    pub fn worst(&self, weight: f32) -> f32 {
        let a = apply_shape(self.shape, 0.0);
        let b = apply_shape(self.shape, 1.0);
        if weight >= 0.0 { a.min(b) } else { a.max(b) }
    }
}

/// `shape(n)` (§ 3.5 step 5).
pub fn apply_shape(shape: Shape, n: f32) -> f32 {
    match shape {
        Shape::Flat => 1.0,
        Shape::Linear => n,
        Shape::Quadratic => n * n,
        Shape::Falling => 1.0 - n,
        Shape::Root => n.sqrt(),
    }
}

/// Combines per-value shaped values (§ 3.5 "Several anchor values"). `None` for an empty list.
pub fn combine(op: Combine, values: &[f32]) -> Option<f32> {
    if values.is_empty() {
        return None;
    }
    Some(match op {
        Combine::Mean => values.iter().sum::<f32>() / values.len() as f32,
        Combine::Lowest => values.iter().copied().fold(f32::INFINITY, f32::min),
        Combine::Highest => values.iter().copied().fold(f32::NEG_INFINITY, f32::max),
        Combine::Product => values.iter().product(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn curve(lo: f32, hi: f32, origin: Origin, peak_at: Option<f32>, shape: Shape) -> ScoreCurve {
        ScoreCurve {
            lo,
            hi,
            origin,
            peak_at,
            shape,
        }
    }

    fn close(a: f32, b: f32) {
        assert!((a - b).abs() <= 1e-6, "{a} != {b}");
    }

    #[test]
    fn each_shape_over_floor_origin() {
        let v = 7.0; // lo 2, hi 12 -> n = 0.5
        close(curve(2.0, 12.0, Origin::Floor, None, Shape::Flat).shaped(v), 1.0);
        close(
            curve(2.0, 12.0, Origin::Floor, None, Shape::Linear).shaped(v),
            0.5,
        );
        close(
            curve(2.0, 12.0, Origin::Floor, None, Shape::Quadratic).shaped(v),
            0.25,
        );
        close(
            curve(2.0, 12.0, Origin::Floor, None, Shape::Falling).shaped(v),
            0.5,
        );
        close(
            curve(2.0, 12.0, Origin::Floor, None, Shape::Root).shaped(v),
            0.5f32.sqrt(),
        );
    }

    #[test]
    fn a_reversed_clamp_reads_as_the_swapped_interval() {
        let reversed = curve(12.0, 2.0, Origin::Floor, None, Shape::Linear);
        let straight = curve(2.0, 12.0, Origin::Floor, None, Shape::Linear);
        for v in [-5.0, 2.0, 7.0, 12.0, 40.0] {
            close(reversed.shaped(v), straight.shaped(v));
        }
        close(reversed.shaped(2.0), 0.0);
    }

    #[test]
    fn zero_origin_divides_by_hi() {
        // n = v / hi = 6 / 12.
        close(
            curve(2.0, 12.0, Origin::Zero, None, Shape::Linear).shaped(6.0),
            0.5,
        );
        // The clamp applies first: 1 -> lo 2 -> 2 / 12.
        close(
            curve(2.0, 12.0, Origin::Zero, None, Shape::Linear).shaped(1.0),
            2.0 / 12.0,
        );
    }

    #[test]
    fn values_outside_the_bounds_clamp() {
        let c = curve(0.0, 10.0, Origin::Floor, None, Shape::Linear);
        close(c.shaped(-5.0), 0.0);
        close(c.shaped(50.0), 1.0);
    }

    #[test]
    fn zero_denominators() {
        close(
            curve(3.0, 3.0, Origin::Floor, None, Shape::Linear).shaped(3.0),
            0.0,
        );
        close(
            curve(0.0, 0.0, Origin::Zero, None, Shape::Linear).shaped(0.0),
            0.0,
        );
        // peak_at with lo == hi == r: the denominator is zero, n = 1.
        close(
            curve(4.0, 4.0, Origin::Floor, Some(4.0), Shape::Linear).shaped(4.0),
            1.0,
        );
    }

    #[test]
    fn peak_at_falls_off_toward_the_farther_bound() {
        // lo 0, hi 10, r 4: denominator max(4, 6) = 6.
        let c = curve(0.0, 10.0, Origin::Floor, Some(4.0), Shape::Linear);
        close(c.shaped(4.0), 1.0);
        close(c.shaped(10.0), 0.0);
        close(c.shaped(1.0), 0.5);
        close(c.shaped(7.0), 0.5);
    }

    #[test]
    fn worst_end_depends_on_the_weight_sign() {
        let c = curve(0.0, 1.0, Origin::Floor, None, Shape::Linear);
        close(c.worst(1.0), 0.0);
        close(c.worst(-1.0), 1.0);
        let falling = curve(0.0, 1.0, Origin::Floor, None, Shape::Falling);
        close(falling.worst(1.0), 0.0);
        close(falling.worst(-1.0), 1.0);
    }

    #[test]
    fn combine_operators() {
        let v = [0.2, 0.5, 0.8];
        close(combine(Combine::Mean, &v).unwrap_or(f32::NAN), 0.5);
        close(combine(Combine::Lowest, &v).unwrap_or(f32::NAN), 0.2);
        close(combine(Combine::Highest, &v).unwrap_or(f32::NAN), 0.8);
        close(combine(Combine::Product, &v).unwrap_or(f32::NAN), 0.08);
        assert_eq!(combine(Combine::Mean, &[]), None);
    }
}
