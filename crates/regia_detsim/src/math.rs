// SPDX-License-Identifier: MPL-2.0
// Portions copied from Direct Order 27 (ir-sim/src/math.rs, commit f7e8f4740), ADR-8012.
// Mark: as-is (lines 1-50, the wrappers). DO27's source-scan test (lines 52-256) became
// `cargo xtask det-scan` (`xtask/src/det_scan.rs`, adapted); its hover call-site test stays with the
// hover step (plan row S-07). Changes: doc comments name Regia's records instead of DO27's; the
// crate-level `#![allow(dead_code)]` is gone because the functions are public. ADR-8876: `atan2` is no
// longer the copied wrapper (it is the correctly rounded, canonical `atan2`; the copied bits are
// `atan2_libm`), and the module doc says so.

//! Pure-Rust libm wrappers for the tick path.
//!
//! Every transcendental on a lockstep sim tick must go through this module so Linux, Windows, macOS
//! and Android (x86-64 and aarch64) produce bit-identical floats (ADR-8012 A-4). Do not call
//! `f32::{sin,cos,atan2,sqrt,powf,exp,sin_cos}` on the tick path; `clippy.toml` in this crate denies
//! those methods, and `cargo xtask det-scan` rejects them in every crate on the lockstep-closure list
//! (`docs/ci/lockstep-closure.toml`).
//!
//! The functions are `libm` 0.2.16 (pinned exactly in the workspace), the same bits Direct Order 27's
//! sim runs, so a host that moves its sim onto this crate keeps its checksums. `sqrt` is an IEEE
//! operation and correctly rounded everywhere; it is routed here anyway so the tick path has one code
//! path on every host.
//!
//! [`atan2`] is the exception: since [`crate::MATH_BITS_VERSION`] 1 it is the correctly rounded
//! `atan2` with canonical zero and NaN handling (ADR-8876), not libm's `atan2f` ([`atan2_libm`]).

mod cr_atan2f;

/// The exact fused multiply-add `a * b + c` (IEEE 754 `fusedMultiplyAdd`, one rounding) via libm's software
/// `fma`: the same bits on every host, with or without an FMA unit. Only the correctly rounded `atan2` uses it
/// (ADR-8876); tick code keeps writing `a * b + c` (ADR-8012 A-4).
#[inline]
pub(crate) fn fma_f64(a: f64, b: f64, c: f64) -> f64 {
    libm::fma(a, b, c)
}

/// `sin(x)` via libm (`sinf`).
#[inline]
pub fn sin(x: f32) -> f32 {
    libm::sinf(x)
}

/// `cos(x)` via libm (`cosf`).
#[inline]
pub fn cos(x: f32) -> f32 {
    libm::cosf(x)
}

/// The canonical lockstep `atan2(y, x)` ([`crate::MATH_BITS_VERSION`] 1, ADR-8876).
///
/// The result is defined by the mathematics, not by a library build, so any host or tool can reproduce
/// it bit for bit:
///
/// - a finite, nonzero or infinite input pair gives the **correctly rounded** binary32 `atan2(y, x)`
///   (round to nearest, ties to even), with the IEEE 754-2019 § 9.2.1 values for infinities
///   (`atan2(±inf, +inf) = ±π/4`, `atan2(±inf, -inf) = ±3π/4`, `atan2(±inf, x) = ±π/2`,
///   `atan2(y, -inf) = ±π` by the sign of `y`). The algorithm is CORE-MATH's `cr_atan2f` (Sibidanov and
///   Zimmermann, MIT; `docs/provenance.md`), which its authors prove correctly rounded for every input
///   pair; any other correctly rounded `atan2f` gives the same bits on these inputs;
/// - a signed-zero input counts as `+0`: `atan2(±0, x > 0) = +0`, `atan2(±0, x < 0) = +π`,
///   `atan2(±0, ±0) = +0`, `atan2(y, ±0) = ±π/2` by the sign of `y`;
/// - a NaN input gives the canonical NaN (`float::CANONICAL_NAN_BITS`, `0x7fc0_0000`), whatever its sign
///   or payload;
/// - a zero result is `+0`, never `-0` (`atan2(-1, +inf)` and an underflowed `atan2(-tiny, huge)`).
///
/// So the result lies in `[-π, π]`, is `-π` only for a negative nonzero `y` with a negative `x` (or
/// `-inf`), and never carries a `-0` or a NaN payload into a hash. Version 0 (the regia-absorb copy) was
/// the bare libm `atan2f`, still available as [`atan2_libm`] for byte parity with external tools.
#[inline]
pub fn atan2(y: f32, x: f32) -> f32 {
    crate::float::canonical(cr_atan2f::cr_atan2f(
        crate::float::canonical(y),
        crate::float::canonical(x),
    ))
}

/// The bare libm 0.2.16 `atan2f(y, x)`: [`atan2`]'s bits before [`crate::MATH_BITS_VERSION`] 1.
///
/// Signed zeros keep their sign (`atan2_libm(-0.0, -1.0) = -π`), a NaN result keeps the hardware's NaN
/// sign and payload (`x + y`, which differs between x86-64 and aarch64 for two NaN inputs), and the
/// result can be 1 ulp from the correctly rounded value. Not for new tick code: it exists for paths whose
/// bytes must match a tool that calls this libm (the source game's Stage A world_geom compat path and the
/// tunnel bake's portal yaw, ADR-8167, ADR-8579, ADR-8876).
#[inline]
pub fn atan2_libm(y: f32, x: f32) -> f32 {
    libm::atan2f(y, x)
}

/// `sqrt(x)` via libm (`sqrtf`) for a single code path across hosts.
#[inline]
pub fn sqrt(x: f32) -> f32 {
    libm::sqrtf(x)
}

/// `sqrt(x)` for an `f64` ratio (path budgets on a non-square grid).
/// libm, same as [`sqrt`], so hosts agree.
#[inline]
pub fn sqrt_f64(x: f64) -> f64 {
    libm::sqrt(x)
}

/// `powf(x, y)` via libm (`powf`).
#[inline]
pub fn powf(x: f32, y: f32) -> f32 {
    libm::powf(x, y)
}

/// `exp(x)` via libm (`expf`).
#[inline]
pub fn exp(x: f32) -> f32 {
    libm::expf(x)
}

/// `sin(x)` for an `f64` angle (overview marker rings). libm, same as [`sin`].
#[inline]
pub fn sin_f64(x: f64) -> f64 {
    libm::sin(x)
}

/// `cos(x)` for an `f64` angle (overview marker rings). libm, same as [`cos`].
#[inline]
pub fn cos_f64(x: f64) -> f64 {
    libm::cos(x)
}

// Regia original from here on (lane det-math-rng-fixed): the rest of the libm surface that
// `cargo xtask det-scan` bans in std form, as one-call wrappers of the same pinned libm 0.2.16, and
// the `f64` forms. Nothing above this line changed; `tests/math_bits.rs` pins the bits of both halves.

/// `tan(x)` via libm (`tanf`).
#[inline]
pub fn tan(x: f32) -> f32 {
    libm::tanf(x)
}

/// `asin(x)` via libm (`asinf`). NaN outside `[-1, 1]`.
#[inline]
pub fn asin(x: f32) -> f32 {
    libm::asinf(x)
}

/// `acos(x)` via libm (`acosf`). NaN outside `[-1, 1]`.
#[inline]
pub fn acos(x: f32) -> f32 {
    libm::acosf(x)
}

/// `atan(x)` via libm (`atanf`).
#[inline]
pub fn atan(x: f32) -> f32 {
    libm::atanf(x)
}

/// Natural logarithm `ln(x)` via libm (`logf`). `-inf` at `±0`, NaN below zero.
#[inline]
pub fn ln(x: f32) -> f32 {
    libm::logf(x)
}

/// `log2(x)` via libm (`log2f`).
#[inline]
pub fn log2(x: f32) -> f32 {
    libm::log2f(x)
}

/// `log10(x)` via libm (`log10f`).
#[inline]
pub fn log10(x: f32) -> f32 {
    libm::log10f(x)
}

/// `2^x` via libm (`exp2f`).
#[inline]
pub fn exp2(x: f32) -> f32 {
    libm::exp2f(x)
}

/// `sqrt(x² + y²)` without intermediate overflow, via libm (`hypotf`).
#[inline]
pub fn hypot(x: f32, y: f32) -> f32 {
    libm::hypotf(x, y)
}

/// Cube root via libm (`cbrtf`); defined for negative `x`.
#[inline]
pub fn cbrt(x: f32) -> f32 {
    libm::cbrtf(x)
}

/// `sinh(x)` via libm (`sinhf`).
#[inline]
pub fn sinh(x: f32) -> f32 {
    libm::sinhf(x)
}

/// `cosh(x)` via libm (`coshf`).
#[inline]
pub fn cosh(x: f32) -> f32 {
    libm::coshf(x)
}

/// `tanh(x)` via libm (`tanhf`).
#[inline]
pub fn tanh(x: f32) -> f32 {
    libm::tanhf(x)
}

/// `(sin(x), cos(x))` as two libm calls, [`sin`] then [`cos`].
///
/// Deliberately not libm's fused `sincosf`: two calls give exactly the bits of [`sin`] and [`cos`],
/// so code that mixes `sin_cos` with the single functions never sees two answers for one angle.
#[inline]
pub fn sin_cos(x: f32) -> (f32, f32) {
    (sin(x), cos(x))
}

// `sin_f64` and `cos_f64` are defined above (they predate this lane's f64 forms; same libm calls).

/// `(sin(x), cos(x))` for `f64` as two libm calls, [`sin_f64`] then [`cos_f64`] (see [`sin_cos`]).
#[inline]
pub fn sin_cos_f64(x: f64) -> (f64, f64) {
    (sin_f64(x), cos_f64(x))
}

/// `tan(x)` for `f64` via libm (`tan`).
#[inline]
pub fn tan_f64(x: f64) -> f64 {
    libm::tan(x)
}

/// `asin(x)` for `f64` via libm (`asin`).
#[inline]
pub fn asin_f64(x: f64) -> f64 {
    libm::asin(x)
}

/// `acos(x)` for `f64` via libm (`acos`).
#[inline]
pub fn acos_f64(x: f64) -> f64 {
    libm::acos(x)
}

/// `atan(x)` for `f64` via libm (`atan`).
#[inline]
pub fn atan_f64(x: f64) -> f64 {
    libm::atan(x)
}

/// `atan2(y, x)` for `f64` via libm (`atan2`).
#[inline]
pub fn atan2_f64(y: f64, x: f64) -> f64 {
    libm::atan2(y, x)
}

/// `exp(x)` for `f64` via libm (`exp`).
#[inline]
pub fn exp_f64(x: f64) -> f64 {
    libm::exp(x)
}

/// `2^x` for `f64` via libm (`exp2`).
#[inline]
pub fn exp2_f64(x: f64) -> f64 {
    libm::exp2(x)
}

/// Natural logarithm for `f64` via libm (`log`).
#[inline]
pub fn ln_f64(x: f64) -> f64 {
    libm::log(x)
}

/// `log2(x)` for `f64` via libm (`log2`).
#[inline]
pub fn log2_f64(x: f64) -> f64 {
    libm::log2(x)
}

/// `log10(x)` for `f64` via libm (`log10`).
#[inline]
pub fn log10_f64(x: f64) -> f64 {
    libm::log10(x)
}

/// `x^y` for `f64` via libm (`pow`).
#[inline]
pub fn powf_f64(x: f64, y: f64) -> f64 {
    libm::pow(x, y)
}

/// `sqrt(x² + y²)` for `f64` via libm (`hypot`).
#[inline]
pub fn hypot_f64(x: f64, y: f64) -> f64 {
    libm::hypot(x, y)
}

/// Cube root for `f64` via libm (`cbrt`).
#[inline]
pub fn cbrt_f64(x: f64) -> f64 {
    libm::cbrt(x)
}

/// `sinh(x)` for `f64` via libm (`sinh`).
#[inline]
pub fn sinh_f64(x: f64) -> f64 {
    libm::sinh(x)
}

/// `cosh(x)` for `f64` via libm (`cosh`).
#[inline]
pub fn cosh_f64(x: f64) -> f64 {
    libm::cosh(x)
}

/// `tanh(x)` for `f64` via libm (`tanh`).
#[inline]
pub fn tanh_f64(x: f64) -> f64 {
    libm::tanh(x)
}
