// SPDX-License-Identifier: MPL-2.0
//! The canonical lockstep `atan2` (ADR-8876, `MATH_BITS_VERSION` 1).
//!
//! - `canonical_atan2_matches_the_edge_vector_golden`: 690 committed edge vectors (±0, ±inf, NaNs of
//!   every sign and payload, axes, subnormals, tiny and huge ratios), expected bits from an independent
//!   decimal oracle, never from `math::atan2`.
//! - `canonical_atan2_is_correctly_rounded_on_the_published_hard_cases`: CORE-MATH's published hard
//!   cases for binary32 `atan2`, the pairs closest to a rounding boundary, against the same oracle.
//! - `canonical_atan2_is_correctly_rounded_on_a_sweep`: a million seeded pairs against binary64
//!   `atan2` rounded once, wherever that rounding is decided (binary64 is about 2^29 times finer).
//! - `signed_zero_and_nan_policy`: the policy written out case by case.
//!
//! Every test runs off the source tree (the vectors are compiled in), so a target leg runs the test
//! binary on its own: x86-64 Linux, Windows, the Steam Deck, aarch64 macOS and Android. Each vector test
//! prints one `regia-atan2-vectors {json}` line with the result hash, so the legs compare hashes.

use regia_detsim::math;

const EDGE: &str = include_str!("fixtures/atan2/edge_vectors.txt");
const HARD: &str = include_str!("fixtures/atan2/hard_vectors.txt");

/// `(y bits, x bits, expected bits)` rows of a vector file.
fn rows(text: &str) -> Vec<(u32, u32, u32)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .map(|l| {
            let mut it = l
                .split_whitespace()
                .map(|t| u32::from_str_radix(t, 16).expect("hex bits in a vector row"));
            let row = (
                it.next().expect("y"),
                it.next().expect("x"),
                it.next().expect("r"),
            );
            assert!(it.next().is_none(), "three columns per row: {l}");
            row
        })
        .collect()
}

/// FNV-1a over the result bits, in row order.
fn hash(bits: impl Iterator<Item = u32>) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for w in bits {
        for b in w.to_le_bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Compare every row; print the leg line; fail with the first mismatches named.
fn check(name: &str, text: &str, min_rows: usize) {
    let rows = rows(text);
    assert!(rows.len() >= min_rows, "{name}: {} rows", rows.len());
    let got: Vec<u32> = rows
        .iter()
        .map(|&(y, x, _)| math::atan2(f32::from_bits(y), f32::from_bits(x)).to_bits())
        .collect();
    let want_hash = hash(rows.iter().map(|r| r.2));
    let got_hash = hash(got.iter().copied());
    println!(
        "regia-atan2-vectors {{\"set\":\"{name}\",\"target\":\"{}-{}\",\"rows\":{},\"hash\":\"{got_hash:016x}\",\"expected\":\"{want_hash:016x}\"}}",
        std::env::consts::ARCH,
        std::env::consts::OS,
        rows.len()
    );
    let bad: Vec<String> = rows
        .iter()
        .zip(&got)
        .filter(|((_, _, r), g)| r != *g)
        .take(12)
        .map(|((y, x, r), g)| {
            format!(
                "atan2({:e} [{y:08x}], {:e} [{x:08x}]) = {g:08x}, want {r:08x}",
                f32::from_bits(*y),
                f32::from_bits(*x)
            )
        })
        .collect();
    let mismatches = rows.iter().zip(&got).filter(|((_, _, r), g)| r != *g).count();
    assert!(
        bad.is_empty(),
        "{name}: {mismatches} of {} rows differ from the canonical atan2 (first {}):\n{}",
        rows.len(),
        bad.len(),
        bad.join("\n")
    );
}

#[test]
fn canonical_atan2_matches_the_edge_vector_golden() {
    check("edge", EDGE, 690);
}

#[test]
fn canonical_atan2_is_correctly_rounded_on_the_published_hard_cases() {
    check("hard", HARD, 4500);
}

/// xorshift64* stream of binary32 values: raw bit patterns every third draw (all classes) and finite
/// values of moderate exponent otherwise, so most pairs land in the polynomial path.
fn sweep(n: usize, mut s: u64) -> Vec<f32> {
    (0..n)
        .map(|i| {
            s ^= s >> 12;
            s ^= s << 25;
            s ^= s >> 27;
            let r = s.wrapping_mul(0x2545_f491_4f6c_dd1d);
            let bits = (r >> 32) as u32;
            if i % 3 == 0 {
                f32::from_bits(bits)
            } else {
                // Exponent in [2^-20, 2^20], random sign and mantissa.
                let exp = 107 + ((bits >> 23) % 41);
                f32::from_bits((bits & 0x807f_ffff) | (exp << 23))
            }
        })
        .collect()
}

#[test]
fn canonical_atan2_is_correctly_rounded_on_a_sweep() {
    const N: usize = 1_000_000;
    let ys = sweep(N, 0x9e37_79b9_7f4a_7c15);
    let xs = sweep(N, 0xd1b5_4a32_d192_ed03);
    let (mut checked, mut undecided) = (0usize, 0usize);
    let mut bad = Vec::new();
    for (&y, &x) in ys.iter().zip(&xs) {
        if !y.is_finite() || !x.is_finite() || y == 0.0 || x == 0.0 {
            continue;
        }
        let wide = libm::atan2(f64::from(y), f64::from(x));
        // Below the normal range the binary32 spacing is fixed; skip it here (the vector files cover it).
        if wide.abs() < 1.0e-37 {
            continue;
        }
        // Where does `wide` sit between two binary32 neighbours? Within 2^-20 of a binary32 ulp of the
        // midpoint, binary64's own error (under 1 binary64 ulp, 2^-29 of a binary32 ulp) cannot decide.
        let narrow = wide as f32;
        let lo = f64::from(narrow);
        let ulp = f64::from(f32::from_bits(narrow.to_bits() + 1)) - lo;
        let frac = (wide - lo) / ulp;
        if (frac.abs() - 0.5).abs() < 1.0 / 1_048_576.0 {
            undecided += 1;
            continue;
        }
        checked += 1;
        let ours = math::atan2(y, x);
        let want = regia_detsim::float::canonical(narrow);
        if ours.to_bits() != want.to_bits() && bad.len() < 12 {
            bad.push(format!(
                "atan2({y:e} [{:08x}], {x:e} [{:08x}]) = {:08x}, correctly rounded {:08x} (binary64 {wide:e})",
                y.to_bits(),
                x.to_bits(),
                ours.to_bits(),
                want.to_bits()
            ));
        }
    }
    println!("sweep: {checked} pairs checked, {undecided} undecided by binary64");
    assert!(checked > N / 2, "only {checked} pairs checked");
    assert!(
        bad.is_empty(),
        "math::atan2 is not the correctly rounded atan2 on these pairs:\n{}",
        bad.join("\n")
    );
}

#[test]
fn signed_zero_and_nan_policy() {
    use core::f32::consts::{FRAC_PI_2, FRAC_PI_4, PI};
    let bits = |y: f32, x: f32| math::atan2(y, x).to_bits();
    // A signed-zero input counts as +0.
    assert_eq!(bits(-0.0, 1.0), 0, "atan2(-0, +1) is +0");
    assert_eq!(bits(0.0, -1.0), PI.to_bits(), "atan2(+0, -1) is +π");
    assert_eq!(bits(-0.0, -1.0), PI.to_bits(), "atan2(-0, -1) is +π, never -π");
    assert_eq!(bits(-0.0, -0.0), 0, "atan2(-0, -0) is +0");
    assert_eq!(bits(0.0, -0.0), 0, "atan2(+0, -0) is +0");
    assert_eq!(bits(-0.0, f32::NEG_INFINITY), PI.to_bits());
    assert_eq!(bits(1.0, -0.0), FRAC_PI_2.to_bits());
    assert_eq!(bits(-1.0, -0.0), (-FRAC_PI_2).to_bits());
    // A zero result is +0.
    assert_eq!(bits(-1.0, f32::INFINITY), 0, "atan2(-1, +inf) is +0");
    assert_eq!(
        bits(-f32::from_bits(1), 1.0e30),
        0,
        "an underflowed negative result is +0"
    );
    // Nonzero values keep their sign.
    assert_eq!(bits(-1.0, -1.0e30), (-PI).to_bits());
    assert_eq!(bits(-1.0, f32::NEG_INFINITY), (-PI).to_bits());
    assert_eq!(bits(f32::INFINITY, f32::INFINITY), FRAC_PI_4.to_bits());
    assert_eq!(
        bits(f32::NEG_INFINITY, f32::NEG_INFINITY),
        0xc016_cbe4,
        "-3π/4 correctly rounded"
    );
    // Any NaN input gives the canonical NaN, whatever its sign or payload.
    for nan in [0x7fc0_0000u32, 0xffc0_0000, 0x7f80_0001, 0xffa0_0000, 0xffff_ffff] {
        let n = f32::from_bits(nan);
        assert_eq!(bits(n, 1.0), 0x7fc0_0000, "atan2(NaN {nan:08x}, 1)");
        assert_eq!(bits(1.0, n), 0x7fc0_0000, "atan2(1, NaN {nan:08x})");
        assert_eq!(bits(n, n), 0x7fc0_0000, "atan2(NaN {nan:08x}, NaN)");
        assert_eq!(bits(-0.0, n), 0x7fc0_0000, "atan2(-0, NaN {nan:08x})");
    }
}
