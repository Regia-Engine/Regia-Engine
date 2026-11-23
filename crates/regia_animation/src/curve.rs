// SPDX-License-Identifier: MIT
// Portions ported from Fyrox (https://github.com/FyroxEngine/Fyrox, commit 0876f67),
// fyrox-math/src/curve.rs. Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors,
// MIT licence (see docs/ported-sources.toml).
//
// Permission is hereby granted, free of charge, to any person obtaining a copy of this software
// and associated documentation files (the "Software"), to deal in the Software without restriction,
// including without limitation the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the Software is furnished to do so,
// subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all copies or substantial
// portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR IMPLIED, INCLUDING BUT NOT
// LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN
// NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY,
// WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
// SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//! Editable float curves: keys with constant, linear or cubic (Hermite) interpolation.
//!
//! Portions ported from Fyrox (<https://github.com/FyroxEngine/Fyrox>), `fyrox-math/src/curve.rs`,
//! Copyright (c) 2019-present Dmitry Stepanov and Fyrox Engine contributors, MIT licence
//! (full notice in `THIRD_PARTY_NOTICES.md`). Adapted: serde instead of Fyrox's visitor, no
//! per-key UUIDs (keys are addressed by index), and cubic tangents are slopes in value per
//! second scaled by the key span (a standard Hermite segment) instead of Fyrox's
//! value-range scaling. Bounds are measured by sampling instead of the closed-form extrema.
//!
//! The runtime clip format stays linear ([`TrackCurve`]): the authoring layer
//! ([`crate::authoring`]) bakes a [`Curve`] to keys at the clip's sample rate.

use serde::{Deserialize, Serialize};

use regia_core::math::det;

use crate::clip::TrackCurve;

/// How the segment that *starts* at a key interpolates towards the next key.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub enum CurveKeyKind {
    /// Hold the key's value until the next key (step).
    #[default]
    Constant,
    /// Straight line to the next key.
    Linear,
    /// Hermite cubic. Tangents are slopes (value units per second).
    Cubic {
        /// Slope arriving at this key (used by the segment that ends here).
        left_tangent: f32,
        /// Slope leaving this key (used by the segment that starts here).
        right_tangent: f32,
    },
}

impl CurveKeyKind {
    /// A cubic key from tangent angles in radians (the editor's handle angles). The tangent
    /// uses the deterministic kernel so two machines converting the same angles write the same
    /// slope (audit-v040 L11).
    #[must_use]
    pub fn cubic_from_angles(left: f32, right: f32) -> Self {
        Self::Cubic {
            left_tangent: det::tan(left),
            right_tangent: det::tan(right),
        }
    }
}

/// One key: a location (seconds) and a value.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct CurveKey {
    /// Time of the key in seconds.
    pub location: f32,
    /// Value at the key.
    pub value: f32,
    /// Interpolation of the segment starting at this key.
    pub kind: CurveKeyKind,
}

impl CurveKey {
    /// A key.
    #[must_use]
    pub fn new(location: f32, value: f32, kind: CurveKeyKind) -> Self {
        Self {
            location,
            value,
            kind,
        }
    }
}

/// Hermite basis (Hermite 1864; e.g. Foley et al., *Computer Graphics*, § 11.2.1): `p0`, `p1`
/// end values, `m0`, `m1` end derivatives with respect to the normalised parameter `t`.
#[must_use]
pub fn hermite(p0: f32, p1: f32, m0: f32, m1: f32, t: f32) -> f32 {
    let t2 = t * t;
    let t3 = t2 * t;
    (2.0 * t3 - 3.0 * t2 + 1.0) * p0 + (t3 - 2.0 * t2 + t) * m0 + (-2.0 * t3 + 3.0 * t2) * p1 + (t3 - t2) * m1
}

/// Interpolates the segment `left → right` at normalised `t`; `span` is the segment length in
/// seconds (scales slopes into the normalised parameter).
fn interpolate(left: &CurveKey, right: &CurveKey, t: f32, span: f32) -> f32 {
    match (left.kind, right.kind) {
        (CurveKeyKind::Constant, _) => {
            if t >= 1.0 {
                right.value
            } else {
                left.value
            }
        }
        (CurveKeyKind::Linear, _) => left.value + (right.value - left.value) * t,
        (CurveKeyKind::Cubic { right_tangent, .. }, CurveKeyKind::Cubic { left_tangent, .. }) => hermite(
            left.value,
            right.value,
            right_tangent * span,
            left_tangent * span,
            t,
        ),
        // Cubic into a constant/linear key: the arriving slope is zero (Fyrox's rule).
        (CurveKeyKind::Cubic { right_tangent, .. }, _) => {
            hermite(left.value, right.value, right_tangent * span, 0.0, t)
        }
    }
}

/// A named float curve: keys sorted by location.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Curve {
    /// Keys, sorted by `location`.
    pub keys: Vec<CurveKey>,
}

impl From<Vec<CurveKey>> for Curve {
    fn from(mut keys: Vec<CurveKey>) -> Self {
        for key in &mut keys {
            key.location = finite_location(key.location);
        }
        sort_keys(&mut keys);
        Self { keys }
    }
}

/// A key location the curve can sample around: NaN becomes 0 and an infinity the largest finite
/// value of its sign. A non-finite location made `value_at` return NaN between the last real key
/// and it, which then baked into clip data (audit-gameplay-muse L9).
fn finite_location(location: f32) -> f32 {
    if location.is_nan() {
        0.0
    } else {
        location.clamp(f32::MIN, f32::MAX)
    }
}

fn sort_keys(keys: &mut [CurveKey]) {
    keys.sort_by(|a, b| a.location.total_cmp(&b.location));
}

impl Curve {
    /// Whether the curve has no keys.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Inserts a key keeping the order (the total order `From<Vec<CurveKey>>` sorts by); returns
    /// its index. A non-finite location is made finite first, as in `From<Vec<CurveKey>>`: NaN
    /// becomes 0 and an infinity the largest finite value of its sign.
    pub fn add_key(&mut self, mut key: CurveKey) -> usize {
        key.location = finite_location(key.location);
        let pos = self
            .keys
            .partition_point(|k| k.location.total_cmp(&key.location).is_lt());
        self.keys.insert(pos, key);
        pos
    }

    /// Removes the key at `index` (no-op when out of range).
    pub fn remove_key(&mut self, index: usize) -> Option<CurveKey> {
        (index < self.keys.len()).then(|| self.keys.remove(index))
    }

    /// Moves key `index` to `location`/`value`, re-sorting; returns the key's new index. A
    /// non-finite `location` is made finite as in [`add_key`](Self::add_key).
    pub fn move_key(&mut self, index: usize, location: f32, value: f32) -> Option<usize> {
        let mut key = self.remove_key(index)?;
        key.location = location;
        key.value = value;
        Some(self.add_key(key))
    }

    /// Location of the last key (0 for an empty curve).
    #[must_use]
    pub fn max_location(&self) -> f32 {
        self.keys.last().map_or(0.0, |k| k.location)
    }

    /// Value at `location`: clamps to the first/last key outside the key range; 0 when empty.
    #[must_use]
    pub fn value_at(&self, location: f32) -> f32 {
        let (Some(first), Some(last)) = (self.keys.first(), self.keys.last()) else {
            return 0.0;
        };
        if location <= first.location {
            return first.value;
        }
        if location >= last.location {
            return last.value;
        }
        let end = self.keys.partition_point(|k| k.location <= location);
        let (Some(left), Some(right)) = (self.keys.get(end.saturating_sub(1)), self.keys.get(end)) else {
            return last.value;
        };
        let span = right.location - left.location;
        if span <= 0.0 {
            return right.value;
        }
        interpolate(left, right, (location - left.location) / span, span)
    }

    /// `[min_location, min_value, max_location, max_value]`, measured by sampling each segment
    /// (cubic overshoot included). Zero-size for one key; all zero when empty.
    #[must_use]
    pub fn bounds(&self) -> [f32; 4] {
        let (Some(first), Some(last)) = (self.keys.first(), self.keys.last()) else {
            return [0.0; 4];
        };
        let mut lo = first.value;
        let mut hi = first.value;
        for pair in self.keys.windows(2) {
            let span = pair[1].location - pair[0].location;
            for i in 0..=16 {
                let t = i as f32 / 16.0;
                let v = if span > 0.0 {
                    interpolate(&pair[0], &pair[1], t, span)
                } else {
                    pair[1].value
                };
                lo = lo.min(v);
                hi = hi.max(v);
            }
        }
        [first.location, lo, last.location, hi]
    }

    /// Bakes the curve to the runtime's linear [`TrackCurve`] at `sample_rate` over
    /// `0..=duration` (keys at the sample grid plus every authored key, so steps and corners
    /// survive). A curve with fewer than two keys bakes to a static value.
    #[must_use]
    pub fn bake(&self, sample_rate: f32, duration: f32) -> TrackCurve<f32> {
        if self.keys.len() < 2 {
            return TrackCurve::Static(self.keys.first().map_or(0.0, |k| k.value));
        }
        let rate = if sample_rate > 0.0 { sample_rate } else { 30.0 };
        // Non-finite or negative spans (a typo in a sidecar) bake over `0..=0` instead of
        // panicking in `clamp` below.
        let end = crate::num::finite_or(duration.max(self.max_location()), 0.0).max(0.0);
        let steps = ((end * rate).ceil() as usize).clamp(1, 1 << 16);
        let mut times: Vec<f32> = (0..=steps).map(|i| (i as f32 / rate).min(end)).collect();
        for key in &self.keys {
            times.push(crate::num::clamp_between(key.location, 0.0, end));
            // Constant segments step: add a sample just before the next key.
            if matches!(key.kind, CurveKeyKind::Constant) {
                let next = self.keys.iter().find(|k| k.location > key.location);
                if let Some(next) = next {
                    times.push((next.location - 1e-4).max(key.location));
                }
            }
        }
        times.sort_by(f32::total_cmp);
        times.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        let values = times.iter().map(|&t| self.value_at(t)).collect();
        TrackCurve::Animated {
            times,
            values,
            exact_rotation: false,
        }
    }

    /// An editable curve from a runtime track (linear keys).
    #[must_use]
    pub fn from_track(track: &TrackCurve<f32>) -> Self {
        match track {
            TrackCurve::Static(v) => Self {
                keys: vec![CurveKey::new(0.0, *v, CurveKeyKind::Linear)],
            },
            TrackCurve::Animated { times, values, .. } => Self {
                keys: times
                    .iter()
                    .zip(values)
                    .map(|(&t, &v)| CurveKey::new(t, v, CurveKeyKind::Linear))
                    .collect(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ramp() -> Curve {
        Curve::from(vec![
            CurveKey::new(1.0, 2.0, CurveKeyKind::Linear),
            CurveKey::new(0.0, 0.0, CurveKeyKind::Linear),
        ])
    }

    /// A key at a non-finite location (an editor slip such as `0.0 / 0.0`) must not make the
    /// curve sample NaN between the last real key and it (audit-gameplay-muse L9).
    #[test]
    fn non_finite_key_locations_never_sample_nan() {
        let mut added = ramp();
        added.add_key(CurveKey::new(f32::NAN, 5.0, CurveKeyKind::Linear));
        let mut moved = ramp();
        moved.move_key(0, f32::INFINITY, 1.0);
        let built = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
            CurveKey::new(f32::NAN, 1.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
            CurveKey::new(1.0, 2.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
        ]);
        for curve in [&added, &moved, &built] {
            assert!(
                curve.keys.iter().all(|k| k.location.is_finite()),
                "every key location is finite: {:?}",
                curve.keys
            );
            for x in [-1.0, 0.0, 0.5, 1.0, 2.0, 1e6] {
                assert!(curve.value_at(x).is_finite(), "value_at({x}) on {:?}", curve.keys);
            }
        }
    }

    #[test]
    fn keys_sort_and_sample_exactly_at_keys() {
        let curve = ramp();
        assert_eq!(curve.keys[0].location, 0.0, "sorted on construction");
        assert_eq!(curve.value_at(0.0), 0.0);
        assert_eq!(curve.value_at(1.0), 2.0);
        assert!((curve.value_at(0.25) - 0.5).abs() < 1e-6);
        // Clamped outside.
        assert_eq!(curve.value_at(-3.0), 0.0);
        assert_eq!(curve.value_at(9.0), 2.0);
        assert_eq!(Curve::default().value_at(0.5), 0.0);
    }

    #[test]
    fn constant_keys_step() {
        let curve = Curve::from(vec![
            CurveKey::new(0.0, 1.0, CurveKeyKind::Constant),
            CurveKey::new(1.0, 5.0, CurveKeyKind::Constant),
        ]);
        assert_eq!(curve.value_at(0.99), 1.0);
        assert_eq!(curve.value_at(1.0), 5.0);
    }

    #[test]
    fn cubic_with_matching_slopes_reproduces_a_line() {
        // A Hermite segment whose end slopes equal the chord slope is the chord.
        let slope = 2.0;
        let curve = Curve::from(vec![
            CurveKey::new(
                0.0,
                0.0,
                CurveKeyKind::Cubic {
                    left_tangent: slope,
                    right_tangent: slope,
                },
            ),
            CurveKey::new(
                2.0,
                4.0,
                CurveKeyKind::Cubic {
                    left_tangent: slope,
                    right_tangent: slope,
                },
            ),
        ]);
        for i in 0..=20 {
            let t = i as f32 * 0.1;
            assert!((curve.value_at(t) - slope * t).abs() < 1e-4, "t={t}");
        }
        // Flat tangents ease in and out: the midpoint is the average, the quarter is below the chord.
        let flat = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
            CurveKey::new(1.0, 1.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
        ]);
        assert!((flat.value_at(0.5) - 0.5).abs() < 1e-5);
        assert!(flat.value_at(0.25) < 0.25);
    }

    #[test]
    fn edit_operations_keep_order() {
        let mut curve = ramp();
        let at = curve.add_key(CurveKey::new(0.5, 7.0, CurveKeyKind::Linear));
        assert_eq!(at, 1);
        let moved = curve.move_key(1, 3.0, 1.0).expect("key exists");
        assert_eq!(moved, 2, "moved past the last key");
        assert_eq!(curve.max_location(), 3.0);
        assert!(curve.remove_key(7).is_none());
        assert_eq!(curve.remove_key(0).map(|k| k.location), Some(0.0));
        assert_eq!(curve.keys.len(), 2);
    }

    #[test]
    fn bounds_include_cubic_overshoot() {
        let curve = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::cubic_from_angles(1.2, 1.2)),
            CurveKey::new(1.0, 0.0, CurveKeyKind::cubic_from_angles(-1.2, -1.2)),
        ]);
        let [x0, lo, x1, hi] = curve.bounds();
        assert_eq!((x0, x1), (0.0, 1.0));
        assert!(hi > 0.3, "overshoot above the keys: {hi}");
        assert!(lo <= 0.0);
    }

    #[test]
    fn bake_matches_the_curve_at_samples_and_keys() {
        let curve = Curve::from(vec![
            CurveKey::new(0.0, 0.0, CurveKeyKind::cubic_from_angles(0.0, 0.0)),
            CurveKey::new(0.5, 1.0, CurveKeyKind::Constant),
            CurveKey::new(1.0, 0.0, CurveKeyKind::Linear),
        ]);
        let baked = curve.bake(30.0, 1.0);
        match &baked {
            TrackCurve::Animated { times, values, .. } => {
                assert!(times.windows(2).all(|w| w[0] < w[1]), "strictly increasing");
                for (t, v) in times.iter().zip(values) {
                    assert!((curve.value_at(*t) - v).abs() < 1e-6);
                }
            }
            TrackCurve::Static(_) => panic!("two+ keys bake to an animated track"),
        }
        // The step holds right up to the next key.
        assert!((baked.sample(0.99) - 1.0).abs() < 1e-3, "{}", baked.sample(0.99));
        assert!(matches!(Curve::default().bake(30.0, 1.0), TrackCurve::Static(v) if v == 0.0));
        let round = Curve::from_track(&baked);
        assert_eq!(round.keys.len(), baked.key_count());
    }
}
