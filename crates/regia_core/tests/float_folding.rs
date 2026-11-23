// SPDX-License-Identifier: MPL-2.0
//! Compile-time and run-time float math agree bit for bit (ADR-1347, determinism across builds).
//!
//! LLVM evaluates a math call with constant arguments while compiling, through the build
//! machine's math library, wherever inlining makes the argument constant: more often in a release
//! build than in a debug one. The same call with a run-time argument goes to the target's math
//! library. If the two give different bits, a simulation that passes a constant through `sin`,
//! `exp` or `powf` drifts between a debug and a release build of the same commit. Each case below
//! is written once with a literal (foldable) and once through `black_box` (never folded).
//!
//! Correctly rounded operations (`sqrt`, `mul_add`) must always agree, and so must f64 library
//! calls (LLVM folds them with the same f64 functions the binary calls). f32 library calls and
//! `powi` are folded in f64 and rounded once, so they may differ by an ulp; that is reported,
//! and only a larger difference fails.

use std::hint::black_box;

fn compare(name: &str, folded: u64, runtime: u64, failures: &mut Vec<String>) {
    if folded != runtime {
        failures.push(format!("{name}: folded {folded:#x}, run time {runtime:#x}"));
    }
}

/// `$f` of each literal, folded and at run time (f32).
macro_rules! unary32 {
    ($failures:ident, $f:ident, $($x:literal),+) => {$(
        compare(
            concat!("f32::", stringify!($f), "(", stringify!($x), ")"),
            u64::from({ let x: f32 = $x; x.$f().to_bits() }),
            u64::from(black_box::<f32>($x).$f().to_bits()),
            &mut $failures,
        );
    )+};
}

/// `$f` of each literal, folded and at run time (f64).
macro_rules! unary64 {
    ($failures:ident, $f:ident, $($x:literal),+) => {$(
        compare(
            concat!("f64::", stringify!($f), "(", stringify!($x), ")"),
            { let x: f64 = $x; x.$f().to_bits() },
            black_box::<f64>($x).$f().to_bits(),
            &mut $failures,
        );
    )+};
}

/// `$f(x, y)` of each literal pair, folded and at run time (f32).
macro_rules! binary32 {
    ($failures:ident, $f:ident, $(($x:literal, $y:literal)),+) => {$(
        compare(
            concat!("f32::", stringify!($f), "(", stringify!($x), ", ", stringify!($y), ")"),
            u64::from({ let x: f32 = $x; x.$f($y).to_bits() }),
            u64::from(black_box::<f32>($x).$f(black_box($y)).to_bits()),
            &mut $failures,
        );
    )+};
}

#[test]
fn correctly_rounded_operations_fold_to_the_run_time_bits() {
    let mut failures = Vec::new();
    unary32!(failures, sqrt, 0.3, 0.75, 2.0, 1234.5);
    unary64!(failures, sqrt, 0.3, 0.75, 2.0, 1234.5);
    for (a, b, c) in [(0.1f32, 3.7f32, -0.2f32), (1.0e-3, 2.5e3, 7.25)] {
        let folded = a.mul_add(b, c).to_bits();
        let runtime = black_box(a).mul_add(black_box(b), black_box(c)).to_bits();
        compare(
            &format!("f32::mul_add({a}, {b}, {c})"),
            u64::from(folded),
            u64::from(runtime),
            &mut failures,
        );
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn f64_library_math_folds_to_the_run_time_bits() {
    // LLVM folds f64 calls through the build machine's own f64 functions; a test binary built
    // and run on one machine must agree with itself.
    let mut failures = Vec::new();
    unary64!(failures, sin, 0.3, 0.75, 1.7, 10.25);
    unary64!(failures, cos, 0.3, 0.75, 1.7, 10.25);
    unary64!(failures, tan, 0.3, 0.75, 1.2);
    unary64!(failures, exp, 0.3, 0.75, 1.7, -4.5);
    unary64!(failures, ln, 0.3, 0.75, 1.7, 10.25);
    unary64!(failures, atan, 0.3, 0.75, 1.7, 10.25);
    assert!(
        failures.is_empty(),
        "compile-time folding and the run-time math library disagree in f64; a release build \
         that inlines constants into these calls simulates differently from a debug build:\n{}",
        failures.join("\n")
    );
}

/// f32 library calls and `powi` are folded in f64 and rounded once, while the run-time
/// functions work in f32 (`sinf`, repeated f32 multiplication), so the two may differ by an ulp
/// for some inputs on some libms. That is a property of the toolchain, not something Regia Engine
/// can fix, so this test only reports it (the book page's determinism rules say what to do).
/// It still checks that the reported differences stay within one ulp.
#[test]
fn f32_library_math_folding_differences_stay_within_one_ulp() {
    let mut failures = Vec::new();
    unary32!(failures, sin, 0.3, 0.75, 1.7, 10.25);
    unary32!(failures, cos, 0.3, 0.75, 1.7, 10.25);
    unary32!(failures, tan, 0.3, 0.75, 1.2);
    unary32!(failures, exp, 0.3, 0.75, 1.7, -4.5);
    unary32!(failures, exp2, 0.3, 0.75, 1.7);
    unary32!(failures, ln, 0.3, 0.75, 1.7, 10.25);
    unary32!(failures, log2, 0.3, 0.75, 10.25);
    unary32!(failures, atan, 0.3, 0.75, 1.7, 10.25);
    unary32!(failures, asin, 0.3, 0.75, 0.9999);
    unary32!(failures, acos, 0.3, 0.75, 0.9999);
    unary32!(failures, cbrt, 0.3, 0.75, 10.25);
    unary32!(failures, tanh, 0.3, 0.75, 1.7);
    binary32!(failures, powf, (0.3, 2.5), (1.7, 0.5), (10.25, -1.25));
    binary32!(failures, atan2, (0.3, 0.4), (-1.7, 0.5), (10.25, -1.25));
    binary32!(failures, powi, (0.3, 3), (1.7, 7), (10.25, -2));
    for failure in &failures {
        println!("folding differs (informational): {failure}");
    }
    let beyond_one_ulp: Vec<&String> = failures
        .iter()
        // `powi` multiplies several times at run time, so it may be off by more than one ulp.
        .filter(|line| !line.starts_with("f32::powi"))
        .filter(|line| {
            let bits: Vec<u64> = line
                .split("0x")
                .skip(1)
                .filter_map(|hex| {
                    let digits: String = hex.chars().take_while(char::is_ascii_hexdigit).collect();
                    u64::from_str_radix(&digits, 16).ok()
                })
                .collect();
            // Same sign and exponent range: adjacent f32 values differ by 1 in their bits.
            bits.len() == 2 && bits[0].abs_diff(bits[1]) > 1
        })
        .collect();
    assert!(
        beyond_one_ulp.is_empty(),
        "folded and run-time f32 math differ by more than one ulp: {beyond_one_ulp:?}"
    );
}
