// SPDX-License-Identifier: MIT OR Apache-2.0
// Portions ported from nannou (https://github.com/nannou-org/nannou, commit bffa988),
// nannou_core/src/math.rs. Copyright (c) 2017 MindBuffer, Copyright (c) 2019 nannou-org; dual-licensed
// MIT OR Apache-2.0, used here under the MIT licence (see docs/ported-sources.toml).
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
//! Angle conversions, vector rotation, range mapping and clamping helpers used by the geometry
//! types.

use core::ops::Add;

use num_traits::{Float, NumCast, One};

const ONE_TURN_DEGREES_F32: f32 = 360.0;
const ONE_TURN_DEGREES_F64: f64 = 360.0;

/// Functions for converting between angle representations.
///
/// Only implemented for `f32` and `f64`.
pub trait ConvertAngle {
    /// Degrees to radians.
    fn deg_to_rad(self) -> Self;
    /// Radians to degrees.
    fn rad_to_deg(self) -> Self;
    /// Turns (1 = a full circle) to radians.
    fn turns_to_rad(self) -> Self;
    /// Radians to turns (1 = a full circle).
    fn rad_to_turns(self) -> Self;
}

/// Short-hand for retrieving the angle of the vector in radians.
pub trait Vec2Angle {
    /// The angle of the vector in radians.
    ///
    /// Implemented internally as `glam::Vec2::X.angle_between(self)`.
    fn angle(self) -> f32;
}

/// Rotation of a 2D vector about the origin.
pub trait Vec2Rotate {
    /// Rotate the vector around 0.0.
    fn rotate(self, radians: f32) -> Self;
}

/// Create a transformation matrix that will cause a vector to point at `dir` using `up` for
/// orientation.
// NOTE: Remove this if we can land something similar upstream in `glam`.
pub trait Mat4LookTo {
    /// A right-handed view matrix at `eye` looking along `dir`.
    fn look_to_rh(eye: glam::Vec3, dir: glam::Vec3, up: glam::Vec3) -> glam::Mat4 {
        // A zero `dir` looks down -Z; a `dir` parallel to `up` picks any perpendicular side axis,
        // so the matrix is always finite for finite input.
        let f = dir.try_normalize().unwrap_or(glam::Vec3::NEG_Z);
        let s = f
            .cross(up)
            .try_normalize()
            .unwrap_or_else(|| f.any_orthonormal_vector());
        let u = s.cross(f);
        glam::Mat4::from_cols(
            glam::vec4(s.x, u.x, -f.x, 0.0),
            glam::vec4(s.y, u.y, -f.y, 0.0),
            glam::vec4(s.z, u.z, -f.z, 0.0),
            glam::vec4(-eye.dot(s), -eye.dot(u), eye.dot(f), 1.0),
        )
    }

    /// A left-handed view matrix at `eye` looking along `dir`.
    fn look_to_lh(eye: glam::Vec3, dir: glam::Vec3, up: glam::Vec3) -> glam::Mat4 {
        Self::look_to_rh(eye, -dir, up)
    }
}

impl ConvertAngle for f32 {
    fn deg_to_rad(self) -> Self {
        self * core::f32::consts::TAU / ONE_TURN_DEGREES_F32
    }
    fn rad_to_deg(self) -> Self {
        self * ONE_TURN_DEGREES_F32 / core::f32::consts::TAU
    }
    fn turns_to_rad(self) -> Self {
        self * core::f32::consts::TAU
    }
    fn rad_to_turns(self) -> Self {
        self / core::f32::consts::TAU
    }
}

impl ConvertAngle for f64 {
    fn deg_to_rad(self) -> Self {
        self * core::f64::consts::TAU / ONE_TURN_DEGREES_F64
    }
    fn rad_to_deg(self) -> Self {
        self * ONE_TURN_DEGREES_F64 / core::f64::consts::TAU
    }
    fn turns_to_rad(self) -> Self {
        self * core::f64::consts::TAU
    }
    fn rad_to_turns(self) -> Self {
        self / core::f64::consts::TAU
    }
}

impl Vec2Angle for glam::Vec2 {
    fn angle(self) -> f32 {
        glam::Vec2::X.angle_to(self)
    }
}

impl Vec2Rotate for glam::Vec2 {
    fn rotate(self, radians: f32) -> Self {
        let rad_cos = radians.cos();
        let rad_sin = radians.sin();
        let x = self.x * rad_cos - self.y * rad_sin;
        let y = self.x * rad_sin + self.y * rad_cos;
        glam::vec2(x, y)
    }
}

impl Mat4LookTo for glam::Mat4 {}

/// Maps a value from an input range to an output range.
///
/// # Panics
///
/// When a value cannot be represented as `f64` or the result cannot be represented as `Y` (for
/// example a NaN mapped into an integer range).
///
/// Note that `map_range` doesn't clamp the output: if `val` is outside the input range, the mapped
/// value will be outside the output range. (Use `clamp` to restrict the output, if desired.)
///
/// # Examples
/// ```
/// # use regia_core::math::geom::{clamp, map_range};
/// assert_eq!(map_range(128, 0, 255, 0.0, 1.0), 0.5019607843137255);
/// ```
/// ```
/// # use regia_core::math::geom::{clamp, map_range};
/// assert_eq!(map_range(3, 0, 10, 0.0, 1.0), 0.3);
/// ```
/// ```
/// # use regia_core::math::geom::{clamp, map_range};
/// // When the value is outside the input range, the result will be outside the output range.
/// let result = map_range(15, 0, 10, 0.0, 1.0);
/// assert_eq!(result, 1.5);
/// assert_eq!(clamp(result, 0.0, 1.0), 1.0);
/// ```
// TODO: Should consider refactoring this to only allow for conversions between types that cannot
// fail. This would break some code but make users think about issues like converting to signed
// ranges with unsigned types, etc. Would also reduce size of code generated due to all the panic
// branches.
pub fn map_range<X, Y>(val: X, in_min: X, in_max: X, out_min: Y, out_max: Y) -> Y
where
    X: NumCast,
    Y: NumCast,
{
    macro_rules! unwrap_or_panic {
        ($result:expr, $arg:expr) => {
            $result.unwrap_or_else(|| panic!("[map_range] failed to cast {} arg to `f64`", $arg))
        };
    }

    let val_f: f64 = unwrap_or_panic!(NumCast::from(val), "first");
    let in_min_f: f64 = unwrap_or_panic!(NumCast::from(in_min), "second");
    let in_max_f: f64 = unwrap_or_panic!(NumCast::from(in_max), "third");
    let out_min_f: f64 = unwrap_or_panic!(NumCast::from(out_min), "fourth");
    let out_max_f: f64 = unwrap_or_panic!(NumCast::from(out_max), "fifth");

    // A zero-width input range would divide by zero and cast NaN/infinity (a panic for integers).
    if in_max_f == in_min_f {
        return unwrap_or_panic!(NumCast::from(out_min_f), "fourth");
    }

    NumCast::from((val_f - in_min_f) / (in_max_f - in_min_f) * (out_max_f - out_min_f) + out_min_f)
        .unwrap_or_else(|| panic!("[map_range] failed to cast result to target type"))
}

/// The max between two partially ordered values.
pub fn partial_max<T>(a: T, b: T) -> T
where
    T: PartialOrd,
{
    if a >= b { a } else { b }
}

/// The min between two partially ordered values.
pub fn partial_min<T>(a: T, b: T) -> T
where
    T: PartialOrd,
{
    if a <= b { a } else { b }
}

/// Clamp a value between some range.
pub fn clamp<T>(n: T, start: T, end: T) -> T
where
    T: PartialOrd,
{
    if start <= end {
        if n < start {
            start
        } else if n > end {
            end
        } else {
            n
        }
    } else if n < end {
        end
    } else if n > start {
        start
    } else {
        n
    }
}

/// `1 + 1` in any numeric type with a one.
pub fn two<S>() -> S
where
    S: Add<Output = S> + One,
{
    S::one() + S::one()
}

/// Models the C++ fmod function: the result has the sign of `numer` (the quotient truncates toward zero).
#[inline]
pub fn fmod<F>(numer: F, denom: F) -> F
where
    F: Float,
{
    // `%` on floats is the C `fmod` (exact, and `fmod(x, inf) == x`); `numer - trunc(numer / denom) * denom`
    // lost precision for large quotients and gave NaN for an infinite `denom` (`0 * inf`).
    numer % denom
}

/// Convert the given angle in degrees to the same angle in radians.
pub fn deg_to_rad<S>(s: S) -> S
where
    S: ConvertAngle,
{
    s.deg_to_rad()
}

/// Convert the given angle in radians to the same angle in degrees.
pub fn rad_to_deg<S>(s: S) -> S
where
    S: ConvertAngle,
{
    s.rad_to_deg()
}

/// Convert the given value as a number of "turns" into the equivalent angle in radians.
pub fn turns_to_rad<S>(s: S) -> S
where
    S: ConvertAngle,
{
    s.turns_to_rad()
}

/// Convert the given value in radians to the equivalent value as a number of turns.
pub fn rad_to_turns<S>(s: S) -> S
where
    S: ConvertAngle,
{
    s.rad_to_turns()
}

#[cfg(test)]
mod map_range_tests {
    use super::map_range;

    #[test]
    fn zero_width_input_range_maps_to_out_min() {
        assert_eq!(map_range(3.0_f32, 2.0, 2.0, 10.0_f32, 20.0), 10.0);
        // An integer target used to abort on the NaN/infinity cast.
        assert_eq!(map_range(3_i32, 2, 2, 7_u8, 9), 7);
    }

    #[test]
    fn regular_range_still_maps() {
        assert_eq!(map_range(15, 0, 10, 0.0_f64, 1.0), 1.5);
    }
}

#[cfg(test)]
mod fmod_tests {
    use super::fmod;

    #[test]
    fn fmod_matches_c_sign_convention() {
        assert_eq!(fmod(5.5f32, 2.0), 1.5);
        assert_eq!(fmod(-5.5f32, 2.0), -1.5);
        assert_eq!(fmod(5.5f64, -2.0), 1.5);
        assert_eq!(fmod(-5.5f64, -2.0), -1.5);
    }
}
