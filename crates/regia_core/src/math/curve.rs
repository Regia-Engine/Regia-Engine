// SPDX-License-Identifier: MPL-2.0
//! Keyframed curves (`Curve<T>`), used by animation, audio, particles and UI tweening.

use glam::{Quat, Vec2, Vec3, Vec4};
use serde::{Deserialize, Serialize};

use super::Color;
use super::rng::det_slerp;

/// Values a [`Curve`] can interpolate.
pub trait Interpolate: Clone {
    /// Interpolates from `self` (`t = 0`) to `other` (`t = 1`).
    fn interpolate(&self, other: &Self, t: f32) -> Self;

    /// Cubic Hermite interpolation between `p0` and `p1` with tangents `m0`, `m1` given in value units
    /// per unit of curve time; `duration` is the segment length in curve time and `t ∈ [0, 1]`.
    fn hermite(p0: &Self, m0: &Self, p1: &Self, m1: &Self, t: f32, duration: f32) -> Self;
}

/// Cubic Hermite basis functions `(h00, h10, h01, h11)` at `t`.
#[inline]
fn hermite_basis(t: f32) -> (f32, f32, f32, f32) {
    let t2 = t * t;
    let t3 = t2 * t;
    (
        2.0 * t3 - 3.0 * t2 + 1.0,
        t3 - 2.0 * t2 + t,
        -2.0 * t3 + 3.0 * t2,
        t3 - t2,
    )
}

macro_rules! impl_interpolate_vector {
    ($($ty:ty),*) => {$(
        impl Interpolate for $ty {
            #[inline]
            fn interpolate(&self, other: &Self, t: f32) -> Self {
                *self + (*other - *self) * t
            }

            #[inline]
            fn hermite(p0: &Self, m0: &Self, p1: &Self, m1: &Self, t: f32, duration: f32) -> Self {
                let (h00, h10, h01, h11) = hermite_basis(t);
                *p0 * h00 + *m0 * (h10 * duration) + *p1 * h01 + *m1 * (h11 * duration)
            }
        }
    )*};
}

impl_interpolate_vector!(f32, Vec2, Vec3, Vec4);

impl Interpolate for Quat {
    /// Spherical linear interpolation along the shortest arc, on the deterministic kernel
    /// ([`crate::math::det_slerp`]): the same bits on every target.
    #[inline]
    fn interpolate(&self, other: &Self, t: f32) -> Self {
        det_slerp(*self, *other, t)
    }

    /// Component-wise Hermite on the four quaternion components followed by normalisation (the glTF 2.0
    /// rule for cubic-spline rotation channels). When the sum has no direction (keys `q` and `-q`, one
    /// rotation with two signs, cancel at the segment middle) the result is the shortest-arc slerp
    /// instead of a NaN quaternion.
    fn hermite(p0: &Self, m0: &Self, p1: &Self, m1: &Self, t: f32, duration: f32) -> Self {
        let v = Vec4::hermite(
            &Vec4::from(*p0),
            &Vec4::from(*m0),
            &Vec4::from(*p1),
            &Vec4::from(*m1),
            t,
            duration,
        );
        match v.try_normalize() {
            Some(unit) => Quat::from_vec4(unit),
            None => match (Vec4::from(*p0).try_normalize(), Vec4::from(*p1).try_normalize()) {
                (Some(a), Some(b)) => det_slerp(Quat::from_vec4(a), Quat::from_vec4(b), t.clamp(0.0, 1.0)),
                (Some(only), None) | (None, Some(only)) => Quat::from_vec4(only),
                (None, None) => Quat::IDENTITY,
            },
        }
    }
}

impl Interpolate for Color {
    /// Component-wise in linear space.
    #[inline]
    fn interpolate(&self, other: &Self, t: f32) -> Self {
        self.to_vec4().interpolate(&other.to_vec4(), t).into()
    }

    fn hermite(p0: &Self, m0: &Self, p1: &Self, m1: &Self, t: f32, duration: f32) -> Self {
        Vec4::hermite(
            &p0.to_vec4(),
            &m0.to_vec4(),
            &p1.to_vec4(),
            &m1.to_vec4(),
            t,
            duration,
        )
        .into()
    }
}

/// How a [`Curve`] moves from one key to the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
pub enum Interp {
    /// Hold the key's value until the next key.
    Step,
    /// Straight-line interpolation ([`Interpolate::interpolate`]; slerp for rotations).
    #[default]
    Linear,
    /// Cubic Hermite using the key's `out_tangent` and the next key's `in_tangent`.
    CubicHermite,
}

/// One key of a [`Curve`]. `interp` controls the segment that *starts* at this key.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Keyframe<T> {
    /// Curve time of the key.
    pub time: f32,
    /// Value at `time`.
    pub value: T,
    /// Interpolation towards the next key.
    pub interp: Interp,
    /// Incoming tangent (value units per unit time), used by a [`Interp::CubicHermite`] segment ending here.
    pub in_tangent: T,
    /// Outgoing tangent, used by a [`Interp::CubicHermite`] segment starting here.
    pub out_tangent: T,
}

impl<T: Interpolate> Keyframe<T> {
    /// A key with the given interpolation and both tangents set to `tangent`.
    pub fn new(time: f32, value: T, interp: Interp, tangent: T) -> Self {
        Self {
            time,
            value,
            interp,
            in_tangent: tangent.clone(),
            out_tangent: tangent,
        }
    }
}

/// A keyframed curve. Keys are kept sorted by time; sampling before the first or after the last key
/// returns that key's value.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(bound(serialize = "T: Serialize", deserialize = "T: Deserialize<'de>"))]
pub struct Curve<T> {
    #[serde(deserialize_with = "deserialize_sorted")]
    keys: Vec<Keyframe<T>>,
}

fn deserialize_sorted<'de, D, T>(deserializer: D) -> Result<Vec<Keyframe<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    let mut keys = Vec::<Keyframe<T>>::deserialize(deserializer)?;
    // The same rule as `Curve::new`: a NaN time has no place on the time axis.
    keys.retain(|k| !k.time.is_nan());
    keys.sort_by(|a, b| a.time.total_cmp(&b.time));
    Ok(keys)
}

impl<T> Default for Curve<T> {
    fn default() -> Self {
        Self { keys: Vec::new() }
    }
}

impl<T: Interpolate> Curve<T> {
    /// Creates a curve from keys in any order (sorted here; equal times keep their order).
    ///
    /// Keys whose time is NaN are dropped (they have no place on a time axis).
    pub fn new(mut keys: Vec<Keyframe<T>>) -> Self {
        keys.retain(|k| !k.time.is_nan());
        keys.sort_by(|a, b| a.time.total_cmp(&b.time));
        Self { keys }
    }

    /// A curve with one key: `value` everywhere.
    pub fn constant(value: T) -> Self {
        Self::new(vec![Keyframe::new(0.0, value.clone(), Interp::Step, value)])
    }

    /// Linear interpolation through `(time, value)` points. Each key's in and out tangents are set to that key's own value (not a slope) and are
    /// unused unless a key's `interp` is changed to [`Interp::CubicHermite`].
    pub fn linear(points: impl IntoIterator<Item = (f32, T)>) -> Self {
        Self::new(
            points
                .into_iter()
                .map(|(time, value)| Keyframe::new(time, value.clone(), Interp::Linear, value))
                .collect(),
        )
    }

    /// The keys, sorted by time.
    #[inline]
    pub fn keys(&self) -> &[Keyframe<T>] {
        &self.keys
    }

    /// Inserts a key, keeping the keys sorted (after existing keys with the same time).
    /// A key whose time is NaN is ignored.
    pub fn insert(&mut self, key: Keyframe<T>) {
        if key.time.is_nan() {
            return;
        }
        // Same ordering as `new` (`total_cmp`), so `-0.0` sorts before `+0.0` either way.
        let at = self.keys.partition_point(|k| k.time.total_cmp(&key.time).is_le());
        self.keys.insert(at, key);
    }

    /// Removes and returns the key at `index`, if any.
    pub fn remove(&mut self, index: usize) -> Option<Keyframe<T>> {
        (index < self.keys.len()).then(|| self.keys.remove(index))
    }

    /// `true` if the curve has no keys.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Time of the first key (0 for an empty curve).
    pub fn start_time(&self) -> f32 {
        self.keys.first().map_or(0.0, |k| k.time)
    }

    /// Time of the last key (0 for an empty curve).
    pub fn end_time(&self) -> f32 {
        self.keys.last().map_or(0.0, |k| k.time)
    }

    /// `end_time - start_time`.
    pub fn duration(&self) -> f32 {
        self.end_time() - self.start_time()
    }

    /// Value at `time`, or `None` for an empty curve.
    pub fn sample(&self, time: f32) -> Option<T> {
        let first = self.keys.first()?;
        // Index of the first key strictly after `time`.
        let next = self.keys.partition_point(|k| k.time <= time);
        if next == 0 {
            return Some(first.value.clone());
        }
        let a = &self.keys[next - 1];
        let Some(b) = self.keys.get(next) else {
            return Some(a.value.clone());
        };
        let duration = b.time - a.time;
        // `next` is the first key with time > `time` ≥ a.time, so duration > 0.
        let t = (time - a.time) / duration;
        Some(match a.interp {
            Interp::Step => a.value.clone(),
            Interp::Linear => a.value.interpolate(&b.value, t),
            Interp::CubicHermite => {
                T::hermite(&a.value, &a.out_tangent, &b.value, &b.in_tangent, t, duration)
            }
        })
    }

    /// Value at `time`, or `default` for an empty curve.
    pub fn sample_or(&self, time: f32, default: T) -> T {
        self.sample(time).unwrap_or(default)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(a: f32, b: f32) -> bool {
        (a - b).abs() < 1e-5
    }

    #[test]
    fn linear_keys_carry_their_own_value_as_tangents() {
        let curve = Curve::linear([(0.0, 0.0_f32), (1.0, 10.0)]);
        let keys = curve.keys();
        assert_eq!((keys[0].in_tangent, keys[0].out_tangent), (0.0, 0.0));
        assert_eq!((keys[1].in_tangent, keys[1].out_tangent), (10.0, 10.0));
    }

    #[test]
    fn empty_and_constant() {
        let empty = Curve::<f32>::default();
        assert_eq!(empty.sample(1.0), None);
        assert_eq!(empty.sample_or(1.0, 7.0), 7.0);
        let c = Curve::constant(3.0_f32);
        assert_eq!(c.sample(-100.0), Some(3.0));
        assert_eq!(c.sample(100.0), Some(3.0));
    }

    #[test]
    fn linear_and_step_sampling() {
        let mut c = Curve::linear([(2.0, 10.0_f32), (0.0, 0.0), (4.0, 0.0)]);
        assert_eq!(c.start_time(), 0.0);
        assert_eq!(c.duration(), 4.0);
        assert_eq!(c.sample(-1.0), Some(0.0));
        assert_eq!(c.sample(1.0), Some(5.0));
        assert_eq!(c.sample(2.0), Some(10.0));
        assert_eq!(c.sample(3.5), Some(2.5));
        assert_eq!(c.sample(9.0), Some(0.0));
        c.keys[0].interp = Interp::Step;
        assert_eq!(c.sample(1.999), Some(0.0));
        assert_eq!(c.sample(2.0), Some(10.0));
        c.insert(Keyframe::new(1.0, 4.0, Interp::Linear, 0.0));
        assert_eq!(
            c.keys().iter().map(|k| k.time).collect::<Vec<_>>(),
            [0.0, 1.0, 2.0, 4.0]
        );
        assert_eq!(c.sample(1.5), Some(7.0));
        assert!(c.remove(1).is_some());
        assert!(c.remove(10).is_none());
    }

    #[test]
    fn insert_orders_signed_zero_like_new() {
        let key = |t: f32, v: f32| Keyframe::new(t, v, Interp::Linear, v);
        let built = Curve::new(vec![key(0.0, 0.0), key(-0.0, 5.0), key(1.0, 1.0)]);
        let mut inserted = Curve::new(vec![key(0.0, 0.0), key(1.0, 1.0)]);
        inserted.insert(key(-0.0, 5.0));
        let values = |c: &Curve<f32>| c.keys().iter().map(|k| k.value).collect::<Vec<_>>();
        assert_eq!(values(&built), values(&inserted));
        assert_eq!(built.sample(0.0), inserted.sample(0.0));
    }

    #[test]
    fn hermite_reproduces_a_cubic() {
        // f(x) = x³ - 2x on [1, 3]: Hermite with exact end tangents reproduces any cubic exactly.
        let f = |x: f32| x * x * x - 2.0 * x;
        let df = |x: f32| 3.0 * x * x - 2.0;
        let c = Curve::new(vec![
            Keyframe::new(1.0, f(1.0), Interp::CubicHermite, df(1.0)),
            Keyframe::new(3.0, f(3.0), Interp::CubicHermite, df(3.0)),
        ]);
        for i in 0..=20 {
            let x = 1.0 + 2.0 * i as f32 / 20.0;
            let got = c.sample(x).expect("non-empty");
            assert!(close(got, f(x)), "x={x}: {got} vs {}", f(x));
        }
    }

    #[test]
    fn vector_color_and_quat_curves() {
        let v = Curve::linear([(0.0, Vec3::ZERO), (2.0, Vec3::new(2.0, 4.0, -2.0))]);
        assert_eq!(v.sample(0.5), Some(Vec3::new(0.5, 1.0, -0.5)));

        let c = Curve::linear([(0.0, Color::BLACK), (1.0, Color::WHITE)]);
        assert_eq!(c.sample(0.25), Some(Color::rgb(0.25, 0.25, 0.25)));

        let q0 = Quat::IDENTITY;
        let q1 = Quat::from_rotation_y(std::f32::consts::FRAC_PI_2);
        let q = Curve::linear([(0.0, q0), (1.0, q1)]);
        let mid = q.sample(0.5).expect("non-empty");
        assert!(mid.abs_diff_eq(Quat::from_rotation_y(std::f32::consts::FRAC_PI_4), 1e-6));

        // Zero tangents: a smoothstep from q0 to q1, unit length throughout.
        let h = Curve::new(vec![
            Keyframe::new(0.0, q0, Interp::CubicHermite, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
            Keyframe::new(1.0, q1, Interp::CubicHermite, Quat::from_xyzw(0.0, 0.0, 0.0, 0.0)),
        ]);
        for i in 0..=10 {
            let s = h.sample(i as f32 / 10.0).expect("non-empty");
            assert!(close(s.length(), 1.0));
        }
        assert!(h.sample(1.0).expect("non-empty").abs_diff_eq(q1, 1e-6));
    }

    #[test]
    fn serde_round_trip_sorts_keys() {
        let c = Curve::linear([(0.0, 1.0_f32), (1.0, 2.0)]);
        let ron = ron::to_string(&c).expect("serialize");
        assert_eq!(ron::from_str::<Curve<f32>>(&ron).expect("deserialize"), c);
        let unsorted = r#"(keys: [
            (time: 1.0, value: 2.0, interp: Linear, in_tangent: 0.0, out_tangent: 0.0),
            (time: 0.0, value: 1.0, interp: Linear, in_tangent: 0.0, out_tangent: 0.0),
        ])"#;
        let parsed: Curve<f32> = ron::from_str(unsorted).expect("deserialize");
        assert_eq!(parsed.sample(0.5), Some(1.5));
    }

    /// audit-v034b-numerics M3: deserialization kept NaN-time keys that `new`/`insert` drop, so
    /// sampling past the last real key returned NaN.
    #[test]
    fn deserialized_curves_drop_nan_time_keys_like_new() {
        let text = r#"(keys: [
            (time: 0.0, value: 1.0, interp: Linear, in_tangent: 0.0, out_tangent: 0.0),
            (time: 1.0, value: 2.0, interp: Linear, in_tangent: 0.0, out_tangent: 0.0),
            (time: NaN, value: 9.0, interp: Linear, in_tangent: 0.0, out_tangent: 0.0),
        ])"#;
        let parsed: Curve<f32> = ron::from_str(text).expect("deserialize");
        assert_eq!(parsed.keys().len(), 2);
        assert_eq!(parsed.sample(2.0), Some(2.0));
        assert_eq!(parsed.end_time(), 1.0);
    }

    /// audit-v034b-numerics M4: keys `q` and `-q` (one rotation, two signs) with zero tangents
    /// summed to a zero quaternion at the segment middle, and normalizing it gave NaN.
    #[test]
    fn hermite_between_a_quaternion_and_its_negation_stays_finite() {
        let q = Quat::from_rotation_y(0.8);
        let zero = Quat::from_xyzw(0.0, 0.0, 0.0, 0.0);
        let mid = Quat::hermite(&q, &zero, &(-q), &zero, 0.5, 1.0);
        assert!(mid.is_finite(), "{mid:?}");
        assert!(mid.is_normalized(), "{mid:?}");
        assert!(mid.dot(q).abs() > 0.999, "the same rotation: {mid:?} vs {q:?}");
        // Ordinary segments are unchanged: component-wise Hermite, then normalized.
        let r = Quat::from_rotation_x(0.5);
        let ordinary = Quat::hermite(&q, &zero, &r, &zero, 0.25, 1.0);
        let v = Vec4::hermite(
            &Vec4::from(q),
            &Vec4::ZERO,
            &Vec4::from(r),
            &Vec4::ZERO,
            0.25,
            1.0,
        );
        assert_eq!(ordinary, Quat::from_vec4(v).normalize());
    }
}
