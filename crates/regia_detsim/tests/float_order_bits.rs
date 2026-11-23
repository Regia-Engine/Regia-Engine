// SPDX-License-Identifier: MPL-2.0
//! The bits of `regia_detsim::float` on the cases where std's `min`/`max`/`clamp` and generated NaNs
//! are not portable (ADR-8012 A-4, ADR-8085).
//!
//! Measured with `black_box` inputs (review sprint-v06-absorb-rev-a, row F1, and this file's
//! `std_answers_on_this_host_for_the_record`): x86-64 returns the second operand of `max`/`min` of
//! two zeros (and a standalone `-O3` build gave `-0` for `(-0.0f32).max(0.0)`, where `-O0` gives
//! `+0`); on aarch64 `max` of two zeros is always `+0` and `min` always `-0`; `0.0 / 0.0` is `0xffc0_0000` on x86-64 and `0x7fc0_0000` on aarch64.
//! So `atan2(a.max(b), -1.0)` is `-π` on one peer and `+π` on another. The replacements below must
//! give one answer everywhere, so every case is a bit assert, and every input goes through
//! `black_box` so the optimizer cannot fold it. The lane runs this file at `-O0`
//! (`CARGO_PROFILE_DEV_OPT_LEVEL=0`), at the dev profile and with `--release`, on x86-64 and aarch64.

use std::hint::black_box;

use regia_detsim::float::{
    canonical, canonical_f64, det_clamp, det_clamp_f64, det_max, det_max_f64, det_min, det_min_f64,
};
use regia_detsim::math;

const POS_ZERO: u32 = 0x0000_0000;
const CANON_NAN: u32 = 0x7fc0_0000;
const CANON_NAN_F64: u64 = 0x7ff8_0000_0000_0000;
/// `+π` as `atan2f(+0, -1)` returns it (`0x4049_0fdb`).
const PI_BITS: u32 = 0x4049_0fdb;

fn b(x: f32) -> f32 {
    black_box(x)
}

fn b64(x: f64) -> f64 {
    black_box(x)
}

/// A NaN made by the hardware at run time, the way a sim makes one by accident.
fn generated_nan() -> f32 {
    b(0.0) / b(0.0)
}

fn generated_nan_f64() -> f64 {
    b64(0.0) / b64(0.0)
}

#[test]
fn max_and_min_of_two_zeros_are_positive_zero_in_both_orders() {
    for (x, y) in [(0.0, -0.0), (-0.0, 0.0), (-0.0, -0.0), (0.0, 0.0)] {
        assert_eq!(det_max(b(x), b(y)).to_bits(), POS_ZERO, "det_max({x:?}, {y:?})");
        assert_eq!(det_min(b(x), b(y)).to_bits(), POS_ZERO, "det_min({x:?}, {y:?})");
        assert_eq!(
            det_max_f64(b64(x.into()), b64(y.into())).to_bits(),
            0,
            "det_max_f64({x:?}, {y:?})"
        );
        assert_eq!(
            det_min_f64(b64(x.into()), b64(y.into())).to_bits(),
            0,
            "det_min_f64({x:?}, {y:?})"
        );
    }
    // A negative operand against a zero: the zero wins `max` and the negative wins `min`.
    assert_eq!(det_max(b(-1.0), b(-0.0)).to_bits(), POS_ZERO);
    assert_eq!(det_min(b(1.0), b(-0.0)).to_bits(), POS_ZERO);
    assert_eq!(det_min(b(-1.0), b(0.0)), -1.0);
}

#[test]
fn atan2_after_a_zero_max_is_plus_pi_on_every_host() {
    // The review's desync: std gives -π on a peer whose `max` returned -0.
    for (x, y) in [(0.0, -0.0), (-0.0, 0.0)] {
        let heading = math::atan2(det_max(b(x), b(y)), b(-1.0));
        assert_eq!(heading.to_bits(), PI_BITS, "atan2(det_max({x:?}, {y:?}), -1)");
        let heading = math::atan2(det_min(b(x), b(y)), b(-1.0));
        assert_eq!(heading.to_bits(), PI_BITS, "atan2(det_min({x:?}, {y:?}), -1)");
    }
}

#[test]
fn generated_nans_canonicalize_to_one_pattern() {
    let made = [
        generated_nan(),
        math::sqrt(b(-1.0)),
        b(f32::INFINITY) - b(f32::INFINITY),
        b(f32::INFINITY) * b(0.0),
        f32::from_bits(black_box(0xffc0_0000_u32)),
        f32::from_bits(black_box(0x7f80_0001_u32)),
        f32::from_bits(black_box(0xff80_1234_u32)),
    ];
    for nan in made {
        assert!(nan.is_nan());
        assert_eq!(canonical(nan).to_bits(), CANON_NAN, "{:#010x}", nan.to_bits());
    }
    let made_f64 = [
        generated_nan_f64(),
        math::sqrt_f64(b64(-1.0)),
        f64::from_bits(b64(-0.0).to_bits() | 0x7ff8_0000_0000_0000),
        f64::from_bits(black_box(0x7ff0_0000_0000_0001_u64)),
    ];
    for nan in made_f64 {
        assert!(nan.is_nan());
        assert_eq!(
            canonical_f64(nan).to_bits(),
            CANON_NAN_F64,
            "{:#018x}",
            nan.to_bits()
        );
    }
    assert_eq!(canonical(b(-0.0)).to_bits(), POS_ZERO);
    assert_eq!(canonical_f64(b64(-0.0)).to_bits(), 0);
    // Everything else passes through bit for bit, subnormals and infinities included.
    for bits in [
        0x0000_0001_u32,
        0x8000_0001,
        0x3f80_0000,
        0xbf80_0000,
        0x7f80_0000,
        0xff80_0000,
        0x7f7f_ffff,
    ] {
        assert_eq!(canonical(b(f32::from_bits(bits))).to_bits(), bits);
    }
}

#[test]
fn a_nan_operand_is_ignored_and_two_nans_give_the_canonical_nan() {
    for nan in [
        generated_nan(),
        f32::from_bits(black_box(0xffc0_0000_u32)),
        f32::from_bits(black_box(0x7fc0_0001_u32)),
    ] {
        assert_eq!(det_max(nan, b(1.5)), 1.5);
        assert_eq!(det_max(b(1.5), nan), 1.5);
        assert_eq!(det_min(nan, b(-2.5)), -2.5);
        assert_eq!(det_min(b(-2.5), nan), -2.5);
        // NaN against a zero: the zero, as +0.
        assert_eq!(det_max(nan, b(-0.0)).to_bits(), POS_ZERO);
        assert_eq!(det_min(b(-0.0), nan).to_bits(), POS_ZERO);
        assert_eq!(det_max(nan, generated_nan()).to_bits(), CANON_NAN);
        assert_eq!(det_min(generated_nan(), nan).to_bits(), CANON_NAN);
    }
    let nan64 = generated_nan_f64();
    assert_eq!(det_max_f64(nan64, b64(1.5)), 1.5);
    assert_eq!(det_min_f64(b64(-2.5), nan64), -2.5);
    assert_eq!(det_max_f64(nan64, b64(-0.0)).to_bits(), 0);
    assert_eq!(det_max_f64(nan64, generated_nan_f64()).to_bits(), CANON_NAN_F64);
}

#[test]
fn clamp_canonicalizes_zero_and_nan_and_never_panics() {
    // (x, lo, hi, expected bits)
    let cases: [(f32, f32, f32, u32); 11] = [
        (0.5, 0.0, 1.0, 0x3f00_0000),
        (5.0, 0.0, 1.0, 0x3f80_0000),
        (-5.0, 0.0, 1.0, POS_ZERO),
        (-5.0, -0.0, 1.0, POS_ZERO),
        (-0.0, -1.0, 1.0, POS_ZERO),
        (-0.0, 0.0, 1.0, POS_ZERO),
        (0.0, -1.0, -0.0, POS_ZERO),
        (f32::INFINITY, -1.0, 1.0, 0x3f80_0000),
        (f32::NEG_INFINITY, -1.0, 1.0, 0xbf80_0000),
        // `lo > hi` (std panics): the std sequence, `lo` then `hi`.
        (0.0, 2.0, 1.0, 0x3f80_0000),
        // A NaN bound (std panics) is ignored.
        (3.0, f32::NAN, 1.0, 0x3f80_0000),
    ];
    for (x, lo, hi, want) in cases {
        let got = det_clamp(b(x), b(lo), b(hi)).to_bits();
        assert_eq!(got, want, "det_clamp({x:?}, {lo:?}, {hi:?}) = {got:#010x}");
        let got64 = det_clamp_f64(b64(x.into()), b64(lo.into()), b64(hi.into()));
        assert_eq!(
            got64.to_bits(),
            f64::from(f32::from_bits(want)).to_bits(),
            "det_clamp_f64({x:?}, {lo:?}, {hi:?}) = {got64:?}"
        );
    }
    // A NaN input stays NaN (as std), in the canonical pattern.
    assert_eq!(det_clamp(generated_nan(), b(0.0), b(1.0)).to_bits(), CANON_NAN);
    assert_eq!(
        det_clamp_f64(generated_nan_f64(), b64(0.0), b64(1.0)).to_bits(),
        CANON_NAN_F64
    );
}

/// The replacements are drop-in: away from the two hazards (a zero result, a NaN result) they give
/// std's bits, so a host that swaps `f32::max` for `det_max` moves no other value.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "std min/max/clamp are the reference this test compares the replacements with"
)]
fn away_from_zero_and_nan_the_replacements_equal_std() {
    let mut state: u32 = 0x9e37_79b9;
    let mut next = || {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        f32::from_bits(state)
    };
    let mut checked = 0;
    while checked < 20_000 {
        let (x, y, z) = (b(next()), b(next()), b(next()));
        if x.is_nan() || y.is_nan() || z.is_nan() || x == 0.0 || y == 0.0 || z == 0.0 {
            continue;
        }
        assert_eq!(det_max(x, y).to_bits(), x.max(y).to_bits(), "max {x:?} {y:?}");
        assert_eq!(det_min(x, y).to_bits(), x.min(y).to_bits(), "min {x:?} {y:?}");
        let (lo, hi) = if y <= z { (y, z) } else { (z, y) };
        assert_eq!(
            det_clamp(x, lo, hi).to_bits(),
            x.clamp(lo, hi).to_bits(),
            "clamp {x:?} {lo:?} {hi:?}"
        );
        let (x64, y64) = (f64::from(x) * 1.25, f64::from(y) * 0.75);
        assert_eq!(det_max_f64(x64, y64).to_bits(), x64.max(y64).to_bits());
        assert_eq!(det_min_f64(x64, y64).to_bits(), x64.min(y64).to_bits());
        checked += 1;
    }
}

/// Not an assertion: std's answers on this host and build for the F1 cases, printed so a run on
/// another architecture or optimization level records the hazard the replacements remove
/// (`cargo test -p regia_detsim --test float_order_bits -- --nocapture`).
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "prints std's non-portable answers for the record; asserts nothing about them"
)]
fn std_answers_on_this_host_for_the_record() {
    let rows = [
        ("0.0.max(-0.0)", b(0.0).max(b(-0.0)).to_bits()),
        ("(-0.0).max(0.0)", b(-0.0).max(b(0.0)).to_bits()),
        ("0.0.min(-0.0)", b(0.0).min(b(-0.0)).to_bits()),
        ("(-0.0).min(0.0)", b(-0.0).min(b(0.0)).to_bits()),
        ("0.0 / 0.0", generated_nan().to_bits()),
        ("sqrt(-1)", math::sqrt(b(-1.0)).to_bits()),
        (
            "atan2((-0.0).max(0.0), -1)",
            math::atan2(b(-0.0).max(b(0.0)), b(-1.0)).to_bits(),
        ),
        (
            "atan2(0.0.max(-0.0), -1)",
            math::atan2(b(0.0).max(b(-0.0)), b(-1.0)).to_bits(),
        ),
    ];
    println!("arch {}, os {}", std::env::consts::ARCH, std::env::consts::OS);
    for (what, bits) in rows {
        println!("{what:>28}: {bits:#010x}");
    }
}
