// SPDX-License-Identifier: MPL-2.0
//! Platform-independent transcendentals and the quaternion constructions built on them.
//!
//! The platform libm's `sin`, `cos`, `atan2`, `asin`, `acos`, `exp` and `tan` may differ in
//! the last bit between Linux, Windows, Android and macOS, and animation, gameplay and lockstep
//! code paths must produce the same bits on every machine (the audit-v040 animation
//! determinism lane; the same method as `regia_procgen::detmath`, spec 62 § 6.1 rule 3, and
//! the existing [`super::det_cos`] for spec 15 § 5). Every function here computes in `f64`
//! with range reduction plus a truncated Taylor series (textbook numerical analysis; see
//! `docs/provenance.md`) using only `+ - * /`, `floor` and `sqrt`, which IEEE 754 defines
//! exactly, then rounds once to `f32`. Rust never fuses mul+add, so the `f64` results are
//! bit-identical on every target, and the final `f32` rounding is exact.
//!
//! Error bounds are asserted by the tests in this module against the platform libm on a dense
//! fixed grid; they hold on every target because the algorithm never calls the platform libm.

use std::f64::consts::{FRAC_PI_2, LN_2, PI};

use super::{Quat, Vec3};

/// `(sin x, cos x)` of `q · π/2` (the reduction kernel, `q` in turns of a quarter circle).
///
/// Reduces `x = k·π/2 + r` with `|r| ≤ π/4`, evaluates the Taylor series of `sin r` and
/// `cos r` to `r^13` / `r^12` in `f64`, then picks the quadrant. Only `+ - * /` and `floor`.
pub(crate) fn sin_cos_quarters(q: f64) -> (f64, f64) {
    let k = (q + 0.5).floor();
    let x = (q - k) * FRAC_PI_2;
    let x2 = x * x;
    // Taylor series on [-π/4, π/4]; the truncation error is below 1e-15.
    let mut sin = 0.0;
    let mut cos = 0.0;
    for n in (0..8).rev() {
        let m = f64::from(2 * n);
        cos = 1.0 - cos * x2 / ((m + 1.0) * (m + 2.0));
        sin = 1.0 - sin * x2 / ((m + 2.0) * (m + 3.0));
    }
    let sin = x * sin;
    match (k as i64).rem_euclid(4) {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// `(sin x, cos x)` for `x` in radians. Absolute error well below one `f32` ulp over
/// [-1000, 1000] (tested on a fixed grid); non-finite input gives `(NaN, NaN)`.
#[inline]
pub fn sin_cos(x: f32) -> (f32, f32) {
    if !x.is_finite() {
        return (f32::NAN, f32::NAN);
    }
    let (s, c) = sin_cos_quarters(f64::from(x) / FRAC_PI_2);
    (s as f32, c as f32)
}

/// `sin x` for `x` in radians; see [`sin_cos`].
#[inline]
pub fn sin(x: f32) -> f32 {
    sin_cos(x).0
}

/// `cos x` for `x` in radians; see [`sin_cos`]. This is the function exported as
/// [`super::det_cos`] at the module root (kept there for spec 15 § 5 callers).
#[inline]
pub fn cos(x: f32) -> f32 {
    sin_cos(x).1
}

/// `atan(t)` for `|t| ≤ 1`: two argument halvings
/// (`atan(u) = 2·atan(u / (1 + sqrt(1 + u²)))`, so `|u| ≤ tan(π/16) ≈ 0.199`) then a 10-term
/// Taylor series, all in `f64`.
fn atan_unit(t: f64) -> f64 {
    let u1 = t / (1.0 + (1.0 + t * t).sqrt());
    let u = u1 / (1.0 + (1.0 + u1 * u1).sqrt());
    let u2 = u * u;
    let mut term = u;
    let mut sum = 0.0;
    let mut n = 1.0;
    for _ in 0..10 {
        sum += term / n;
        term *= -u2;
        n += 2.0;
    }
    4.0 * sum
}

/// `atan2(y, x)` in radians, in `(-π, π]`; `atan2(0, 0) = 0`, NaN input gives NaN, infinite
/// arguments follow IEEE (`atan2(±∞, x) = ±π/2`, `atan2(y, ±∞) = 0 or ±π`), like the platform
/// libm this replaces. Absolute error well below one `f32` ulp (tested on a fixed grid in every
/// quadrant).
#[inline]
pub fn atan2(y: f32, x: f32) -> f32 {
    if x.is_nan() || y.is_nan() {
        return f32::NAN;
    }
    if y.is_infinite() {
        return if y.is_sign_negative() {
            -(FRAC_PI_2 as f32)
        } else {
            FRAC_PI_2 as f32
        };
    }
    if x.is_infinite() {
        return if x.is_sign_negative() {
            if y.is_sign_negative() {
                -(PI as f32)
            } else {
                PI as f32
            }
        } else if y.is_sign_negative() {
            -0.0
        } else {
            0.0
        };
    }
    if x == 0.0 && y == 0.0 {
        return if y.is_sign_negative() { -0.0 } else { 0.0 };
    }
    let (x, y) = (f64::from(x), f64::from(y));
    atan2_f64(y, x) as f32
}

/// `atan(t)` in radians, in `(-π/2, π/2)`; see [`atan2`].
#[inline]
pub fn atan(t: f32) -> f32 {
    atan2(t, 1.0)
}

/// `asin(v)` for `|v| ≤ 1` (NaN outside), via `atan2(v, sqrt(1 - v²))`.
#[inline]
pub fn asin(v: f32) -> f32 {
    if !(-1.0..=1.0).contains(&v) {
        return f32::NAN;
    }
    atan2(v, (1.0 - f64::from(v) * f64::from(v)).sqrt() as f32)
}

/// `acos(v)` for `|v| ≤ 1` (NaN outside), via `atan2(sqrt(1 - v²), v)`.
#[inline]
pub fn acos(v: f32) -> f32 {
    if !(-1.0..=1.0).contains(&v) {
        return f32::NAN;
    }
    atan2((1.0 - f64::from(v) * f64::from(v)).sqrt() as f32, v)
}

/// `v · 2^n` by exact exponent steps (no overflow until `n > 1023 + 1000s`).
fn scale_pow2(mut v: f64, mut n: i32) -> f64 {
    while n > 1000 {
        v *= f64::from_bits(((1023 + 1000) as u64) << 52);
        n -= 1000;
    }
    while n < -1000 {
        v *= f64::from_bits(((1023 - 1000) as u64) << 52);
        n += 1000;
    }
    v * f64::from_bits(((1023 + n) as u64) << 52)
}

/// `e^x`. Returns `0` below about -103.97 (f32 underflow, including `-∞`) and `+∞` above about
/// 88.72 (f32 overflow, including `+∞`); NaN in, NaN out. The range is decided before the
/// reduction: for huge `|x|` the f64 range reduction itself loses every digit to cancellation
/// and the exponent scale saturates (`audit fixrev-06-k` M4).
#[inline]
pub fn exp(x: f32) -> f32 {
    if x.is_nan() {
        return f32::NAN;
    }
    if x > 88.8 {
        return f32::INFINITY;
    }
    if x < -104.0 {
        return 0.0;
    }
    let x = f64::from(x);
    let n = (x / LN_2 + 0.5).floor();
    let f = x - n * LN_2; // |f| ≤ ln2 / 2
    // e^f by its Taylor series to f^14.
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..15 {
        term *= f / f64::from(k);
        sum += term;
    }
    scale_pow2(sum, n as i32) as f32
}

/// `tan x = sin x / cos x` (a correctly rounded division of the two series; the quotient is
/// exact IEEE). Non-finite input gives NaN.
#[inline]
pub fn tan(x: f32) -> f32 {
    let (s, c) = sin_cos(x);
    s / c
}

/// The quaternion rotating `angle` radians about `axis` (assumed unit), built from
/// [`sin_cos`] instead of the platform libm so the bits match on every target
/// (glam's `Quat::from_axis_angle` with a deterministic kernel).
#[inline]
pub fn quat_from_axis_angle(axis: Vec3, angle: f32) -> Quat {
    let (s, c) = sin_cos(angle * 0.5);
    Quat::from_xyzw(axis.x * s, axis.y * s, axis.z * s, c)
}

/// The quaternion rotating `angle` radians about +X; see [`quat_from_axis_angle`].
#[inline]
pub fn quat_from_rotation_x(angle: f32) -> Quat {
    let (s, c) = sin_cos(angle * 0.5);
    Quat::from_xyzw(s, 0.0, 0.0, c)
}

/// The quaternion rotating `angle` radians about +Y; see [`quat_from_axis_angle`].
#[inline]
pub fn quat_from_rotation_y(angle: f32) -> Quat {
    let (s, c) = sin_cos(angle * 0.5);
    Quat::from_xyzw(0.0, s, 0.0, c)
}

/// The quaternion rotating `angle` radians about +Z; see [`quat_from_axis_angle`].
#[inline]
pub fn quat_from_rotation_z(angle: f32) -> Quat {
    let (s, c) = sin_cos(angle * 0.5);
    Quat::from_xyzw(0.0, 0.0, s, c)
}

/// Spherical interpolation through the shared deterministic f64 quaternion kernel.
///
/// Uses the same shortest-path and near-parallel contract as [`super::det_slerp`], so
/// callers of the module helper and the public math helper observe the same bits.
#[inline]
pub fn quat_slerp(start: Quat, end: Quat, s: f32) -> Quat {
    super::det_slerp(start, end, s)
}

/// The scaled rotation axis of `q` (`axis · angle`, glam's `Quat::to_scaled_axis` with the
/// `atan2` from [`atan2`]).
#[inline]
pub fn quat_to_scaled_axis(q: Quat) -> Vec3 {
    let v = Vec3::new(q.x, q.y, q.z);
    let length = v.length();
    if length >= 1.0e-8 {
        v * (2.0 * atan2(length, q.w) / length)
    } else {
        Vec3::ZERO
    }
}

/// Build a rotation matrix from a unit quaternion (glam's `Mat3::from_quat`: add/mul only,
/// so already platform-stable).
fn mat3_from_quat(q: Quat) -> [[f32; 3]; 3] {
    let x2 = q.x + q.x;
    let y2 = q.y + q.y;
    let z2 = q.z + q.z;
    let xx = q.x * x2;
    let xy = q.x * y2;
    let xz = q.x * z2;
    let yy = q.y * y2;
    let yz = q.y * z2;
    let zz = q.z * z2;
    let wx = q.w * x2;
    let wy = q.w * y2;
    let wz = q.w * z2;
    // Column-major: m[col][row].
    [
        [1.0 - (yy + zz), xy + wz, xz - wy],
        [xy - wz, 1.0 - (xx + zz), yz + wx],
        [xz + wy, yz - wx, 1.0 - (xx + yy)],
    ]
}

/// The yaw, pitch and roll of `q` as glam's `q.to_euler(EulerRot::YXZ)` computes them
/// (Shoemake, *Graphics Gems IV*, 1994, the same formulas as glam's `Mat3::to_euler_angles`),
/// but with [`atan2`] so the angles are bit-stable across platforms.
pub fn quat_to_euler_yxz(q: Quat) -> (f32, f32, f32) {
    let m = mat3_from_quat(q);
    // YXZ: i = Y, j = X, k = Z; non-repeated, parity odd, static frame.
    let (i, j, k) = (1usize, 0usize, 2usize);
    let col = |c: usize, r: usize| m[c][r];
    let cy = (col(i, i) * col(i, i) + col(j, i) * col(j, i)).sqrt();
    if cy > 16.0 * f32::EPSILON {
        let yaw = atan2(col(k, j), col(k, k));
        let pitch = atan2(-col(k, i), cy);
        let roll = atan2(col(j, i), col(i, i));
        (yaw, pitch, roll)
    } else {
        // Gimbal lock: pitch at ±90°, yaw and roll collapse; zero roll, same as glam.
        let yaw = atan2(-col(j, k), col(j, j));
        let pitch = atan2(-col(k, i), cy);
        (yaw, pitch, 0.0)
    }
}

/// The quaternion for the intrinsic YXZ Euler angles (yaw about +Y, then pitch about +X,
/// then roll about +Z), matching glam's `Quat::from_euler(EulerRot::YXZ, yaw, pitch, roll)`
/// with [`sin_cos`] in place of the libm (Shoemake's construction, same constants).
#[inline]
pub fn quat_from_euler_yxz(yaw: f32, pitch: f32, roll: f32) -> Quat {
    let (si, ci) = sin_cos(yaw * 0.5);
    let (sj, cj) = sin_cos(pitch * 0.5);
    let (sh, ch) = sin_cos(roll * 0.5);
    let cc = ci * ch;
    let cs = ci * sh;
    let sc = si * ch;
    let ss = si * sh;
    Quat::from_xyzw(
        cj * ss + sj * cc,
        cj * sc - sj * cs,
        cj * cs - sj * sc,
        cj * cc + sj * ss,
    )
}

/// The quaternion for the intrinsic XYZ Euler angles (about +X, then +Y, then +Z), matching
/// glam's `Quat::from_euler(EulerRot::XYZ, x, y, z)` with [`sin_cos`] in place of the libm
/// (Shoemake's construction: the middle angle negates for even parity and only the middle
/// quaternion component takes the parity sign).
#[inline]
pub fn quat_from_euler_xyz(x: f32, y: f32, z: f32) -> Quat {
    let (si, ci) = sin_cos(x * 0.5);
    let (sj, cj) = sin_cos(-y * 0.5);
    let (sh, ch) = sin_cos(z * 0.5);
    let cc = ci * ch;
    let cs = ci * sh;
    let sc = si * ch;
    let ss = si * sh;
    Quat::from_xyzw(
        cj * sc - sj * cs,
        -(cj * ss + sj * cc),
        cj * cs - sj * sc,
        cj * cc + sj * ss,
    )
}

use self::sin_cos_quarters as det_sin_cos_quarters;

/// `2^x` for f64: `x = n + f`, `2^n` by exact exponent-bit scaling, `e^(f·ln 2)` by the Taylor
/// series to `f^14`.
fn exp2_f64(x: f64) -> f64 {
    if x.is_nan() {
        return f64::NAN;
    }
    if x > 1024.0 {
        return f64::INFINITY;
    }
    if x < -1074.0 {
        return 0.0;
    }
    let n = (x + 0.5).floor();
    let f = (x - n) * LN_2;
    let mut term = 1.0;
    let mut sum = 1.0;
    for k in 1..15 {
        term *= f / f64::from(k);
        sum += term;
    }
    scale_pow2(sum, n as i32)
}

/// `log2(x)` for f64: mantissa/exponent split, `ln m = 2·atanh(s)` with `s = (m − 1)/(m + 1)`.
fn log2_f64(x: f64) -> f64 {
    if x.is_nan() || x < 0.0 {
        return f64::NAN;
    }
    if x == 0.0 {
        return f64::NEG_INFINITY;
    }
    if x.is_infinite() {
        return f64::INFINITY;
    }
    // Normalize subnormals.
    let (x, bias) = if x < f64::MIN_POSITIVE {
        (x * f64::from_bits((1023 + 64) << 52), -64.0)
    } else {
        (x, 0.0)
    };
    let bits = x.to_bits();
    let mut e = ((bits >> 52) & 0x7ff) as i64 - 1023;
    let mut m = f64::from_bits((bits & 0x000f_ffff_ffff_ffff) | (1023u64 << 52));
    if m > std::f64::consts::SQRT_2 {
        m *= 0.5;
        e += 1;
    }
    let s = (m - 1.0) / (m + 1.0);
    let s2 = s * s;
    let mut term = s;
    let mut sum = 0.0;
    let mut n = 1.0;
    for _ in 0..12 {
        sum += term / n;
        term *= s2;
        n += 2.0;
    }
    e as f64 + bias + 2.0 * sum / LN_2
}

/// `x^y` for f64: `exp2(y · log2(x))` (negative `x` with an integral `y` keeps its sign).
fn pow_f64(x: f64, y: f64) -> f64 {
    if y == 0.0 {
        return 1.0;
    }
    if x == 0.0 {
        return if y > 0.0 { 0.0 } else { f64::INFINITY };
    }
    if x < 0.0 {
        if y.floor() != y {
            return f64::NAN;
        }
        let r = exp2_f64(y * log2_f64(-x));
        let odd = (y / 2.0).floor() * 2.0 != y;
        return if odd { -r } else { r };
    }
    exp2_f64(y * log2_f64(x))
}

/// `atan2(y, x)` for f64, in `(−π, π]`; `(0, 0)` gives `0` (signed, like IEEE). NaN gives NaN;
/// infinite arguments follow IEEE (`atan2(±∞, x) = ±π/2`, `atan2(y, ±∞) = 0 or ±π`), matching
/// the platform libm (a gaze-pitch division can overflow its argument to ±∞).
fn atan2_f64(y: f64, x: f64) -> f64 {
    if x.is_nan() || y.is_nan() {
        return f64::NAN;
    }
    if y.is_infinite() {
        return if y.is_sign_negative() {
            -FRAC_PI_2
        } else {
            FRAC_PI_2
        };
    }
    if x.is_infinite() {
        return if x.is_sign_negative() {
            if y.is_sign_negative() { -PI } else { PI }
        } else if y.is_sign_negative() {
            -0.0
        } else {
            0.0
        };
    }
    if x == 0.0 && y == 0.0 {
        return if y.is_sign_negative() { -0.0 } else { 0.0 };
    }
    let (ax, ay) = (x.abs(), y.abs());
    let base = if ay <= ax {
        atan_unit(ay / ax)
    } else {
        FRAC_PI_2 - atan_unit(ax / ay)
    };
    let a = if x < 0.0 { PI - base } else { base };
    // `is_sign_negative`, not `< 0.0`: `-0.0 < 0.0` is false, but `atan2(-0, x < 0)` is `-π`.
    if y.is_sign_negative() { -a } else { a }
}

pub use super::rng::det_sin_cos;

/// `exp(x)` from a fixed `f64` series on `+`/`*` (no platform libm), so a stored damping factor or
/// decay envelope is bit-identical across Linux, Windows and Android. Underflow below `≈ −104`
/// gives `0`, overflow above `≈ 89` gives `+∞`, like the platform `exp` rounded to `f32`.
#[must_use]
pub fn det_exp(x: f32) -> f32 {
    const LOG2_E: f64 = std::f64::consts::LOG2_E;
    exp2_f64(f64::from(x) * LOG2_E) as f32
}

/// `x.powf(y)` from the deterministic `log2`/`exp2` kernels. `x < 0` with a non-integral `y`
/// gives NaN; `0^y` is `0` (`y > 0`), `1` (`y = 0`) or `+∞` (`y < 0`), like the platform `powf`.
#[must_use]
pub fn det_powf(x: f32, y: f32) -> f32 {
    pow_f64(f64::from(x), f64::from(y)) as f32
}

pub use super::rng::det_atan2;

/// `atan(t)` in radians from the same series as [`det_atan2`].
#[must_use]
pub fn det_atan(t: f32) -> f32 {
    atan2_f64(f64::from(t), 1.0) as f32
}

/// `asin(x)` for `|x| ≤ 1` (NaN outside) from the same series as [`det_atan2`].
#[must_use]
pub fn det_asin(x: f32) -> f32 {
    let x = f64::from(x);
    if !(-1.0..=1.0).contains(&x) {
        return f32::NAN;
    }
    atan2_f64(x, (1.0 - x * x).sqrt()) as f32
}

/// `acos(x)` for `|x| ≤ 1` (NaN outside) from the same series as [`det_atan2`].
#[must_use]
pub fn det_acos(x: f32) -> f32 {
    let x = f64::from(x);
    if !(-1.0..=1.0).contains(&x) {
        return f32::NAN;
    }
    atan2_f64((1.0 - x * x).sqrt(), x) as f32
}

/// `log10(x)` from the deterministic `log2` kernel scaled by the constant `log10(2)`. NaN for a
/// negative argument, `−∞` at `0`, like the platform `log10`.
#[must_use]
pub fn det_log10(x: f32) -> f32 {
    (log2_f64(f64::from(x)) * std::f64::consts::LOG10_2) as f32
}

/// Normalized linear blend of two rotations along the shorter arc: only `+`, `*` and one square
/// root, so the result is the same on every platform. The deterministic stand-in for `slerp`,
/// which reaches the platform libm through `acos`/`sin` (spec 35 § 12.13).
///
/// The dot products are written out as `(x·x′ + z·z′) + (y·y′ + w·w′)` rather than taken from
/// `Quat::dot`/`length`, whose horizontal add glam groups differently per target (that grouping on
/// SSE2, `(x + y) + (z + w)` on NEON, left to right under `scalar-math`).
#[must_use]
pub fn det_nlerp(from: Quat, to: Quat, alpha: f32) -> Quat {
    let to = if quat_dot(from, to) < 0.0 { -to } else { to };
    let blended = from * (1.0 - alpha) + to * alpha;
    let length = quat_dot(blended, blended).sqrt();
    if length > 0.0 { blended / length } else { from }
}

/// The four-component dot product in one fixed grouping, the one glam's SSE2 dot uses, so x86_64
/// keeps its bits and every other target matches them. Plain `f32` `*`/`+`: Rust never fuses them.
fn quat_dot(a: Quat, b: Quat) -> f32 {
    (a.x * b.x + a.z * b.z) + (a.y * b.y + a.w * b.w)
}

/// `Ry(yaw) · Rx(pitch) · Rz(roll)` as a quaternion, from [`det_sin_cos`] half angles composed in
/// `f64`: the same bits on every target, where the platform `sin`/`cos` of the euler construction
/// would not be (spec 35 § 12.13). The quaternion is built in `f64` and rounded once to `f32`.
#[must_use]
pub fn det_quat_from_yaw_pitch_roll(yaw: f32, pitch: f32, roll: f32) -> Quat {
    let (sy, cy) = det_sin_cos_quarters(f64::from(yaw) * 0.5 / FRAC_PI_2);
    let (sp, cp) = det_sin_cos_quarters(f64::from(pitch) * 0.5 / FRAC_PI_2);
    let (sr, cr) = det_sin_cos_quarters(f64::from(roll) * 0.5 / FRAC_PI_2);
    let x = cy * sp * cr + sy * cp * sr;
    let y = sy * cp * cr - cy * sp * sr;
    let z = cy * cp * sr - sy * sp * cr;
    let w = cy * cp * cr + sy * sp * sr;
    Quat::from_xyzw(x as f32, y as f32, z as f32, w as f32)
}

/// The `(yaw, pitch, roll)` with `rotation = Ry(yaw) · Rx(pitch) · Rz(roll)` (the inverse of
/// [`det_quat_from_yaw_pitch_roll`]), from the deterministic `asin`/`atan2` series. The input must
/// be a unit quaternion (callers that blend normalize first).
///
/// At the gimbal poles (pitch ±π/2 — the `Overhead` default) yaw and roll are degenerate: the
/// classic extraction divides by `cos(pitch)`, whose `atan2` arguments are then rounding noise.
/// glam's branch applies instead (Shoemake, *Graphics Gems IV*, 1994, the same formulas as
/// `Mat3::to_euler_angles`): roll is zeroed and the combined yaw read from the O(1) elements, so
/// the result always rebuilds the same rotation.
#[must_use]
pub fn det_yaw_pitch_roll(rotation: Quat) -> (f32, f32, f32) {
    let (x, y, z, w) = (
        f64::from(rotation.x),
        f64::from(rotation.y),
        f64::from(rotation.z),
        f64::from(rotation.w),
    );
    // Elements of R = Ry·Rx·Rz (row-major; the rest are not needed away from the pole).
    let m02 = 2.0 * (x * z + w * y);
    let m22 = 1.0 - 2.0 * (x * x + y * y);
    let m12 = 2.0 * (y * z - w * x);
    let m10 = 2.0 * (x * y + w * z);
    let m11 = 1.0 - 2.0 * (x * x + z * z);
    let s = (-m12).clamp(-1.0, 1.0);
    let pitch = atan2_f64(s, (1.0 - s * s).sqrt());
    // |cos(pitch)|: near zero the yaw/roll arguments above are noise. Zero the roll and read the
    // combined yaw ± roll from m20/m00, the same branch as glam's `to_euler(YXZ)`.
    let cy = (m02 * m02 + m22 * m22).sqrt();
    let (yaw, roll) = if cy > 16.0 * f64::from(f32::EPSILON) {
        (atan2_f64(m02, m22), atan2_f64(m10, m11))
    } else {
        let m20 = 2.0 * (x * z - w * y);
        let m00 = 1.0 - 2.0 * (y * y + z * z);
        (atan2_f64(-m20, m00), 0.0)
    };
    (yaw as f32, pitch as f32, roll as f32)
}

#[cfg(test)]
mod framework_tests {
    use super::*;

    #[test]
    fn det_sin_cos_tracks_libm_and_pins_its_bits() {
        let mut worst = 0.0f32;
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            let (s, c) = det_sin_cos(x);
            worst = worst.max((s - x.sin()).abs()).max((c - x.cos()).abs());
        }
        assert!(worst <= 2.0e-7, "{worst}");
        // Series on plain f64 +/*/sqrt: these bits hold on every target.
        let (s, c) = det_sin_cos(0.7);
        assert_eq!(s.to_bits(), 0x3f24_eb73);
        assert_eq!(c.to_bits(), 0x3f43_ccb3);
    }

    #[test]
    fn det_exp_tracks_libm_and_is_pinned() {
        let mut worst = 0.0f32;
        for i in -4000..=400 {
            let x = i as f32 * 0.01;
            let rel = (det_exp(x) - x.exp()).abs() / x.exp().max(1.0e-30);
            worst = worst.max(rel);
        }
        assert!(worst <= 2.0e-7, "{worst}");
        assert_eq!(det_exp(0.0), 1.0);
        // Pinned: damping factors and decay envelopes read these bits on every platform.
        assert_eq!(det_exp(-0.1).to_bits(), 0x3f67_a36d);
        assert_eq!(det_exp(-6.0f32 / 60.0).to_bits(), 0x3f67_a36d);
        assert_eq!(det_exp(-104.0), 0.0);
        assert_eq!(det_exp(90.0), f32::INFINITY);
    }

    #[test]
    fn det_powf_tracks_libm_on_positive_bases() {
        let mut worst = 0.0f32;
        for i in 1..2000 {
            let x = i as f32 * 0.001;
            let rel = (det_powf(x, 0.3) - x.powf(0.3)).abs() / x.powf(0.3).max(1.0e-30);
            worst = worst.max(rel);
        }
        assert!(worst <= 2.0e-6, "{worst}");
        assert_eq!(det_powf(0.0, 2.0), 0.0);
        assert_eq!(det_powf(-2.0, 3.0), -8.0);
        assert!(det_powf(-2.0, 0.5).is_nan());
        // Pinned: the walk-bob stride exponent.
        assert_eq!(det_powf(2.442_593_6, 0.3).to_bits(), 0x3fa7_53a5);
    }

    #[test]
    fn det_atan2_family_tracks_libm_in_every_quadrant() {
        let mut worst = 0.0f32;
        for i in 0..2000 {
            let a = -20.0 + 40.0 * (i as f32) / 1999.0;
            let (y, x) = (a.sin() * 3.0, (a * 0.37).cos());
            worst = worst.max((det_atan2(y, x) - y.atan2(x)).abs());
        }
        assert!(worst <= 2.0e-6, "{worst}");
        assert_eq!(det_atan2(0.0, 0.0), 0.0);
        assert_eq!(det_atan2(1.0, 0.0), FRAC_PI_2 as f32);
        assert!((det_atan(0.5) - 0.5f32.atan()).abs() < 1.0e-6);
        assert!((det_asin(0.5) - 0.5f32.asin()).abs() < 1.0e-6);
        assert!((det_acos(0.5) - 0.5f32.acos()).abs() < 1.0e-6);
        assert!(det_asin(2.0).is_nan() && det_acos(-2.0).is_nan());
        // Pinned: a quaternion extraction reads these.
        assert_eq!(det_atan2(1.0, 2.0).to_bits(), 0x3eed_6338);
    }

    #[test]
    fn det_log10_tracks_libm_and_is_pinned() {
        let mut worst = 0.0f32;
        for i in 1..2000 {
            let x = 1.0e-4 + (i as f32) * 0.001;
            worst = worst.max((det_log10(x) - x.log10()).abs());
        }
        assert!(worst <= 2.0e-6, "{worst}");
        assert_eq!(det_log10(1.0), 0.0);
        assert_eq!(det_log10(0.0), f32::NEG_INFINITY);
        assert!(det_log10(-1.0).is_nan());
        // Pinned: a sequencer fade reads this.
        assert_eq!(det_log10(0.5).to_bits(), 0xbe9a_209b);
    }

    fn assert_quat_close(a: Quat, b: Quat, what: &str) {
        // `angle_between` bottoms out near 4e-4 rad in f32 (its `acos` argument rounds to 1);
        // compare components instead.
        for (x, y) in [a.x, a.y, a.z, a.w].into_iter().zip([b.x, b.y, b.z, b.w]) {
            assert!((x - y).abs() < 2.0e-6, "{what}: {a:?} vs {b:?}");
        }
    }

    #[test]
    fn det_euler_round_trips_and_matches_glam() {
        for i in 0..200 {
            let yaw = i as f32 * 0.031 - 3.0;
            let pitch = (i as f32 * 0.017 - 1.5).clamp(-1.5, 1.5);
            let roll = i as f32 * 0.013 - 1.3;
            let q = det_quat_from_yaw_pitch_roll(yaw, pitch, roll);
            let glam_q = Quat::from_euler(crate::math::EulerRot::YXZ, yaw, pitch, roll);
            // Same rotation as glam's euler construction (within the det series error).
            assert_quat_close(q, glam_q, "euler construction");
            let (y2, p2, r2) = det_yaw_pitch_roll(q);
            let q2 = det_quat_from_yaw_pitch_roll(y2, p2, r2);
            // The extraction may pick the other sign of the quaternion (the same rotation).
            let neg = Quat::from_xyzw(-q2.x, -q2.y, -q2.z, -q2.w);
            let close = |a: Quat, b: Quat| {
                [a.x, a.y, a.z, a.w]
                    .into_iter()
                    .zip([b.x, b.y, b.z, b.w])
                    .all(|(x, y)| (x - y).abs() < 2.0e-6)
            };
            assert!(close(q, q2) || close(q, neg), "round trip: {q:?} vs {q2:?}");
            let _ = (y2, p2, r2);
        }
    }

    /// The extraction at the gimbal poles (pitch exactly ±π/2, the `Overhead` default): yaw and
    /// roll are degenerate there, but the result must still be *a* valid decomposition of the same
    /// rotation (glam's contract: roll zeroed, yaw from the O(1) matrix elements), not the
    /// atan2 of two ~1e-8 rounding-noise arguments (Muse read-only review of
    /// fix-framework1-determinism, high finding 1).
    #[test]
    fn det_yaw_pitch_roll_handles_the_gimbal_poles() {
        let close = |a: f32, b: f32, what: &str| assert!((a - b).abs() < 1.0e-5, "{what}: {a} vs {b}");
        for (yaw, pitch, roll) in [
            (0.7_f32, FRAC_PI_2 as f32, 0.0_f32),
            (-2.1, FRAC_PI_2 as f32, 1.3),
            (2.6, -(FRAC_PI_2 as f32), -0.9),
            (-0.3, -(FRAC_PI_2 as f32), 2.2),
            (0.0, FRAC_PI_2 as f32, 0.0),
        ] {
            let q = det_quat_from_yaw_pitch_roll(yaw, pitch, roll);
            let (y2, p2, r2) = det_yaw_pitch_roll(q);
            // Same decomposition as glam's `to_euler(YXZ)` on the same quaternion (pitch within
            // 1e-3: `sin(pitch)` cannot distinguish pitch from π/2 better than sqrt(f32 noise),
            // and glam's f32 pipeline rounds its matrix element to exactly `-1.0` — the ambiguity
            // is inherent to every asin-based extraction at the pole).
            let (gy, gp, gr) = q.to_euler(crate::math::EulerRot::YXZ);
            close(y2, gy, "yaw matches glam at the pole");
            assert!(
                (p2 - gp).abs() < 1.0e-3,
                "pitch matches glam at the pole: {p2} vs {gp}"
            );
            close(r2, gr, "roll matches glam at the pole");
            close(r2, 0.0, "roll is zeroed at the pole, like glam");
            // Ry(y)·Rx(p)·Rz(r) at the pole equals Ry(y ∓ r)·Rx(p) (roll folds into yaw; both
            // angles live in (−π, π], so compare the wrapped difference).
            let expect_yaw = if pitch > 0.0 { yaw - roll } else { yaw + roll };
            let wrapped = (y2 - expect_yaw + PI as f32).rem_euclid(2.0 * PI as f32) - PI as f32;
            close(wrapped, 0.0, "yaw folds the roll at the pole");
            assert!((p2 - pitch).abs() < 1.0e-3, "pitch at the pole: {pitch} -> {p2}");
            // And the decomposition rebuilds the same rotation (up to the quaternion sign):
            // the pre-fix code was tens of degrees off here.
            let q2 = det_quat_from_yaw_pitch_roll(y2, p2, r2);
            let neg = Quat::from_xyzw(-q2.x, -q2.y, -q2.z, -q2.w);
            let same = |a: Quat, b: Quat| {
                [a.x, a.y, a.z, a.w]
                    .into_iter()
                    .zip([b.x, b.y, b.z, b.w])
                    .all(|(x, y)| (x - y).abs() < 1.0e-3)
            };
            assert!(same(q, q2) || same(q, neg), "pole round trip: {q:?} vs {q2:?}");
        }
    }

    /// The pinned atan family must answer the platform libm's values on infinite arguments too:
    /// `atan(±∞) = ±π/2`, `atan2(y, ±∞)` = 0 or ±π. A walk-bob gaze pitch divides by a subnormal
    /// `fixation_m` and overflows the argument to ±∞; NaN stayed NaN (Muse read-only review of
    /// fix-framework1-determinism, medium finding 2).
    #[test]
    fn det_atan_family_matches_the_platform_on_infinite_inputs() {
        assert_eq!(det_atan(f32::INFINITY), FRAC_PI_2 as f32);
        assert_eq!(det_atan(f32::NEG_INFINITY), -(FRAC_PI_2 as f32));
        assert!(det_atan(f32::NAN).is_nan());
        assert_eq!(crate::math::det_atan2(f32::INFINITY, 1.0), FRAC_PI_2 as f32);
        assert_eq!(
            crate::math::det_atan2(f32::NEG_INFINITY, -1.0),
            -(FRAC_PI_2 as f32)
        );
        assert_eq!(crate::math::det_atan2(1.0, f32::INFINITY), 0.0);
        assert_eq!(crate::math::det_atan2(1.0, f32::NEG_INFINITY), PI as f32);
        assert_eq!(crate::math::det_atan2(-1.0, f32::NEG_INFINITY), -(PI as f32));
        // Signed zero: atan2(-0, x < 0) is -π, not +π.
        assert_eq!(det_atan(-0.0), -0.0);
        assert_eq!(crate::math::det_atan2(-0.0, -1.0), -(PI as f32));
        assert_eq!(crate::math::det_atan2(-0.0, 1.0), -0.0);
    }

    #[test]
    fn det_euler_pins_blended_quaternion_bits() {
        // Two poses 90 degrees of yaw apart, blended at t = 0.5 through nlerp + det extraction:
        // the exact chain the camera combine uses.
        let a = det_quat_from_yaw_pitch_roll(0.0, 0.0, 0.0);
        let b = det_quat_from_yaw_pitch_roll(1.570_796_4, 0.0, 0.0);
        let to = if a.dot(b) < 0.0 { -b } else { b };
        let blended = a * 0.5 + to * 0.5;
        let q = blended / blended.length();
        let (yaw, pitch, roll) = det_yaw_pitch_roll(q);
        assert_eq!(yaw.to_bits(), 0x3f49_0fda);
        // Signed zero follows IEEE atan2 now (`atan2(-0, x > 0) = -0.0`, like the platform libm);
        // the pitch argument is `-0.0` for this blend.
        assert_eq!(pitch.to_bits(), 0x8000_0000);
        assert_eq!(roll.to_bits(), 0);
    }

    /// `det_nlerp`'s dot products are written out as `(x·x′ + z·z′) + (y·y′ + w·w′)`, so its bits
    /// do not depend on how the target groups a horizontal add: glam's SSE2 dot uses that
    /// grouping, its NEON dot `(x + y) + (z + w)` and its `scalar-math` dot `((x + y) + z) + w`
    /// (the `cross-platform-determinism` feature). Before the fix this failed on aarch64 and with
    /// that feature on x86_64 (corr-core audit).
    #[test]
    fn det_nlerp_does_not_depend_on_the_simd_add_order() {
        fn dot(a: Quat, b: Quat) -> f32 {
            (a.x * b.x + a.z * b.z) + (a.y * b.y + a.w * b.w)
        }
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 40) as f32 / (1u64 << 24) as f32 * 2.0 - 1.0
        };
        for i in 0..4096 {
            // Raw (not normalized) inputs: `Quat::normalize` would itself go through the SIMD dot.
            let a = Quat::from_xyzw(next(), next(), next(), next());
            let b = Quat::from_xyzw(next(), next(), next(), next());
            let t = next().abs();
            let to = if dot(a, b) < 0.0 { -b } else { b };
            let blended = a * (1.0 - t) + to * t;
            let length = dot(blended, blended).sqrt();
            let want = if length > 0.0 { blended / length } else { a };
            let got = det_nlerp(a, b, t);
            assert_eq!(
                got.to_array().map(f32::to_bits),
                want.to_array().map(f32::to_bits),
                "case {i}: det_nlerp({a:?}, {b:?}, {t})"
            );
        }
        assert_eq!(det_nlerp(Quat::IDENTITY, Quat::IDENTITY, 0.5), Quat::IDENTITY);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The fixed accuracy grids: deterministic inputs, no clocks.
    #[test]
    fn sin_cos_tracks_libm_within_one_f32_ulp() {
        let mut worst = 0.0f32;
        for i in 0..200_001 {
            let x = -1000.0 + 2000.0 * (i as f32) / 200_000.0;
            let (s, c) = sin_cos(x);
            worst = worst.max((s - x.sin()).abs()).max((c - x.cos()).abs());
        }
        assert!(worst <= 2.0e-7, "worst sin_cos error {worst}");
        assert!(sin_cos(f32::NAN).0.is_nan() && sin_cos(f32::NAN).1.is_nan());
        assert_eq!(sin_cos(0.0), (0.0, 1.0));
    }

    /// The correctly rounded `atan2` reference (the `f64` libm rounded once to `f32`). The
    /// platform's `atan2f` is not one: macOS lands one ulp off glibc in places (for example
    /// `atan2(-7.855, -3.7)`: -2.0110068 there, -2.0110066 correctly rounded), and a 2e-7 bound is
    /// under one ulp in `[2, PI)` (ADR-5550).
    fn reference_atan2(y: f32, x: f32) -> f32 {
        f64::from(y).atan2(f64::from(x)) as f32
    }

    #[test]
    fn atan2_tracks_libm_in_every_quadrant() {
        let mut worst = 0.0f32;
        for i in 0..100_000 {
            let a = -1000.0 + 2000.0 * (i as f32) / 99_999.0;
            let (y, x) = (
                a.sin() * (1.0 + (i % 7) as f32),
                (a * 0.37).cos() * (1.0 + (i % 5) as f32),
            );
            worst = worst.max((atan2(y, x) - reference_atan2(y, x)).abs());
        }
        assert!(worst <= 2.0e-7, "worst atan2 error {worst}");
        assert_eq!(atan2(0.0, 0.0), 0.0);
        assert_eq!(atan2(1.0, 0.0), FRAC_PI_2 as f32);
        assert!(atan2(f32::NAN, 1.0).is_nan());
    }

    #[test]
    fn asin_acos_match_libm_on_the_domain() {
        for i in 0..100_000 {
            let v = -1.0 + 2.0 * (i as f32) / 99_999.0;
            assert!((asin(v) - v.asin()).abs() <= 1.0e-6, "asin({v})");
            // Near ±π the f32 ulp is 2.4e-7; a couple of ulps is the best any series can do.
            assert!((acos(v) - v.acos()).abs() <= 1.0e-6, "acos({v})");
        }
        assert!(asin(2.0).is_nan() && acos(-2.0).is_nan());
        assert_eq!(acos(1.0), 0.0);
        assert_eq!(asin(0.0), 0.0);
    }

    #[test]
    fn exp_tracks_libm_over_the_f32_range() {
        for i in 0..100_000 {
            // Stay in the normal f32 range: subnormal results round by absolute steps, so the
            // relative bound does not apply below about -87.3.
            let x = -87.0 + 175.7 * (i as f32) / 99_999.0;
            let got = exp(x);
            let want = x.exp();
            if want.is_finite() && got.is_finite() {
                let rel = (got - want).abs() / want;
                assert!(rel <= 3.0e-7, "exp({x}): {got} vs {want}");
            } else {
                assert_eq!(got.is_infinite(), want.is_infinite(), "exp({x})");
            }
        }
        assert_eq!(exp(0.0), 1.0);
        assert!(exp(f32::NAN).is_nan());
        // Non-finite and huge inputs take the guarded exits, not the reduction (whose f64
        // cancellation and saturated scale loop returned NaN before; fixrev-06-k M4).
        assert_eq!(exp(f32::INFINITY), f32::INFINITY);
        assert_eq!(exp(f32::NEG_INFINITY), 0.0);
        assert_eq!(exp(1.0e30), f32::INFINITY);
        assert_eq!(exp(-1.0e30), 0.0);
        assert_eq!(exp(88.8), f32::INFINITY);
        assert_eq!(exp(-104.0), 0.0);
        assert!(exp(88.0).is_finite());
    }

    /// `exp` at the ends of the `f32` range is `0` or `+∞`, never NaN (corr-core audit).
    /// `±∞` used to reach `x − n·ln 2` as `∞ − ∞`, and a huge finite argument saturated
    /// `n as i32`, so the series ran on a large `f` and the scaling loop ran about two million
    /// times.
    #[test]
    fn exp_saturates_at_the_ends_of_the_range() {
        for x in [f32::NEG_INFINITY, -f32::MAX, -1.0e30, -3.0e9, -105.0, -104.0] {
            assert_eq!(exp(x).to_bits(), 0.0f32.to_bits(), "exp({x})");
        }
        for x in [f32::INFINITY, f32::MAX, 1.0e30, 3.0e9, 89.0] {
            assert_eq!(exp(x), f32::INFINITY, "exp({x})");
        }
        // Just inside the ends the series still answers.
        assert!(exp(-103.0) > 0.0);
        assert!(exp(88.0).is_finite());
    }

    #[test]
    fn tan_tracks_libm_away_from_the_poles() {
        for i in 0..100_000 {
            let x = -1.5 + 3.0 * (i as f32) / 99_999.0; // |x| < π/2: no pole on the grid
            assert!((tan(x) - x.tan()).abs() <= 2.0e-6, "tan({x})");
        }
        assert_eq!(tan(0.0), 0.0);
    }

    /// The bits these produce are the lockstep pins: they come from the series alone, so they
    /// hold on every platform whatever its libm returns.
    #[test]
    fn quaternion_constructions_are_pinned() {
        // From glam's formulas with the det kernel; values locked against accidental changes.
        let (s, c) = sin_cos(0.35f32);
        assert_eq!(quat_from_rotation_y(0.7), Quat::from_xyzw(0.0, s, 0.0, c));
        let q = quat_from_axis_angle(Vec3::Y, 0.7);
        assert_eq!(q, Quat::from_xyzw(0.0, s, 0.0, c));
        // Round-trip: the YXZ euler decomposition inverts the construction.
        for i in 0..10_000 {
            let yaw = -3.0 + 6.0 * (i as f32) / 9999.0;
            let pitch = -1.4 + 2.8 * (i as f32) / 9999.0;
            let roll = -3.0 + 6.0 * (i as f32) / 9999.0;
            let q = quat_from_euler_yxz(yaw, pitch, roll);
            let (y2, p2, r2) = quat_to_euler_yxz(q);
            let q2 = quat_from_euler_yxz(y2, p2, r2);
            assert!(
                (q.x - q2.x).abs() < 1e-5 && (q.w - q2.w).abs() < 1e-5,
                "euler round trip {i}"
            );
        }
    }

    /// The det quaternion helpers must agree with glam's libm versions to within a few ulps
    /// (same formulas, different trig): if this drifts, the swap changed semantics.
    #[test]
    fn quaternion_helpers_match_glam_within_a_few_ulps() {
        let mut rng_state = 0x9e3779b97f4a7c15u64;
        let mut next = move || {
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 7;
            rng_state ^= rng_state << 17;
            rng_state
        };
        let check = |a: Quat, msg: &str| {
            let (y1, p1, r1) = quat_to_euler_yxz(a);
            let (y2, p2, r2) = a.to_euler(super::super::EulerRot::YXZ);
            assert!(
                (y1 - y2).abs() < 1e-5 && (p1 - p2).abs() < 1e-5 && (r1 - r2).abs() < 1e-5,
                "{msg} q={a:?} det=({y1},{p1},{r1}) glam=({y2},{p2},{r2})"
            );
        };
        for i in 0..10_000 {
            let f = |bits: u64| (bits >> 40) as f32 / (1u64 << 24) as f32 * 6.0 - 3.0;
            let yaw = f(next());
            let pitch = f(next()) * 0.45;
            let roll = f(next());
            let g1 = quat_from_euler_yxz(yaw, pitch, roll);
            let g2 = Quat::from_euler(super::super::EulerRot::YXZ, yaw, pitch, roll);
            assert!(g1.abs_diff_eq(g2, 2e-6), "from_euler_yxz {i}");
            let gx = quat_from_euler_xyz(yaw, pitch, roll);
            let gy = Quat::from_euler(super::super::EulerRot::XYZ, yaw, pitch, roll);
            assert!(gx.abs_diff_eq(gy, 2e-6), "from_euler_xyz {i}");
            check(g1, "to_euler_yxz");
            let axis = Vec3::new(f(next()), f(next()), f(next())).normalize_or(Vec3::X);
            let qa = quat_from_axis_angle(axis, yaw);
            let qb = Quat::from_axis_angle(axis, yaw);
            assert!(qa.abs_diff_eq(qb, 2e-6), "from_axis_angle {i}");
            let t = f(next()).abs().min(1.0);
            let s1 = quat_slerp(g1, gx, t);
            let s2 = g1.slerp(gx, t);
            assert!(s1.abs_diff_eq(s2, 2e-5), "slerp {i}");
            let sa = quat_to_scaled_axis(qa);
            let sb = qa.to_scaled_axis();
            assert!(sa.abs_diff_eq(sb, 2e-5), "to_scaled_axis {i}");
        }
    }
}
