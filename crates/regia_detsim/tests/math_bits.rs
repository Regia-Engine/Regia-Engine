// SPDX-License-Identifier: MPL-2.0
//! The bits of `regia_detsim::math` over a fixed input set, pinned by hash (ADR-8013).
//!
//! The same hashes must come out on x86-64 (Linux, Windows) and aarch64 (macOS, Android, the
//! Steam Deck is x86-64): that is the cross-architecture half of the lockstep policy (ADR-8012 A-4).
//! A change of any hash means `libm` changed or a wrapper changed, which is a sim-version change.
//! Run it on another host with `cargo test -p regia_detsim --test math_bits -- --nocapture`; the
//! per-function hashes are printed so a mismatch names its function.

use regia_detsim::math;

/// FNV-1a over 32-bit words.
#[derive(Clone, Copy)]
struct Fnv(u64);

impl Fnv {
    const fn new() -> Self {
        Fnv(0xcbf2_9ce4_8422_2325)
    }

    fn word(&mut self, w: u32) {
        for b in w.to_le_bytes() {
            self.0 ^= u64::from(b);
            self.0 = self.0.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }

    /// An `f32` with every NaN folded to one pattern: NaN payloads and signs from hardware
    /// operations differ between x86-64 and aarch64, so a lockstep hash must never see them.
    fn f32(&mut self, x: f32) {
        self.word(if x.is_nan() { 0x7fc0_0000 } else { x.to_bits() });
    }

    fn f64(&mut self, x: f64) {
        let bits = if x.is_nan() {
            0x7ff8_0000_0000_0000
        } else {
            x.to_bits()
        };
        self.word(bits as u32);
        self.word((bits >> 32) as u32);
    }
}

/// The input set: a linear sweep, finite random bit patterns over every exponent, and edge values.
fn inputs() -> Vec<f32> {
    let mut xs: Vec<f32> = (0..2048).map(|i| (i as f32 - 1024.0) / 32.0).collect();
    let mut state: u32 = 0x2545_f491;
    while xs.len() < 6144 {
        state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        let x = f32::from_bits(state);
        if x.is_finite() {
            xs.push(x);
        }
    }
    xs.extend([
        0.0,
        -0.0,
        f32::MIN_POSITIVE,
        f32::from_bits(1),
        -f32::from_bits(0x0040_0000),
        f32::MAX,
        f32::MIN,
        f32::EPSILON,
        core::f32::consts::PI,
        core::f32::consts::FRAC_PI_2,
        -core::f32::consts::FRAC_PI_4,
        1.0e10,
        -1.0e10,
        88.72283,
        -103.97208,
        f32::INFINITY,
        f32::NEG_INFINITY,
    ]);
    xs
}

/// Per-function hashes in a fixed order: sin, cos, atan2, sqrt, sqrt_f64, powf, exp.
fn hashes() -> [u64; 7] {
    let xs = inputs();
    let mut h = [Fnv::new(); 7];
    for (i, &x) in xs.iter().enumerate() {
        let y = xs[(i * 7 + 3) % xs.len()];
        h[0].f32(math::sin(x));
        h[1].f32(math::cos(x));
        h[2].f32(math::atan2(x, y));
        h[3].f32(math::sqrt(x.abs()));
        h[4].f64(math::sqrt_f64(f64::from(x.abs()) * 1.5));
        // A base in (0, 16] and an exponent in [-8, 8] keep most results finite.
        h[5].f32(math::powf(x.abs() % 16.0 + 1.0e-3, y % 8.0));
        h[6].f32(math::exp(x % 90.0));
    }
    h.map(|f| f.0)
}

/// The hashes on x86-64 Linux, `libm` 0.2.16 (recorded by the F-01 lane, 2026-11-16), at
/// [`PINNED_MATH_BITS_VERSION`]. `atan2` re-pinned for ADR-8876 (version 1, the canonical correctly rounded
/// `atan2`); its version-0 hash is [`PINNED_ATAN2_LIBM_V0`].
const PINNED: [u64; 7] = [
    0xdc2a_4c4e_9ff1_50a5,
    0x47e1_7430_fed9_df91,
    0xba66_bb23_f65f_3b09,
    0x3d57_d234_da14_8389,
    0x9171_0b5b_726d_b308,
    0xb828_ff8b_10fb_40d5,
    0x2526_a84c_c4af_195d,
];

#[test]
fn math_bits_match_the_pinned_hashes_on_every_arch() {
    let got = hashes();
    let names = ["sin", "cos", "atan2", "sqrt", "sqrt_f64", "powf", "exp"];
    for (name, (g, p)) in names.iter().zip(got.iter().zip(PINNED.iter())) {
        println!("{name:>8}: {g:#018x} (pinned {p:#018x})");
    }
    println!("arch {}, os {}", std::env::consts::ARCH, std::env::consts::OS);
    assert_eq!(
        got, PINNED,
        "regia_detsim::math bits moved: a libm or wrapper change is a sim-version change"
    );
}

/// The `regia_detsim::MATH_BITS_VERSION` the [`PINNED`] hashes belong to (ADR-8876): a hash that moves
/// needs a version bump, and the bump re-pins here.
const PINNED_MATH_BITS_VERSION: u32 = 1;

/// The `atan2` hash of [`hashes`] at `MATH_BITS_VERSION` 0 (the bare libm `atan2f`, F-01 lane,
/// 2026-11-16): `math::atan2_libm` must keep it.
const PINNED_ATAN2_LIBM_V0: u64 = 0x33f9_10bf_467e_c8f8;

#[test]
fn math_bits_version_names_the_pinned_bits() {
    assert_eq!(
        regia_detsim::MATH_BITS_VERSION,
        PINNED_MATH_BITS_VERSION,
        "MATH_BITS_VERSION moved: re-pin PINNED on every arch with the bump's cause"
    );
}

/// ADR-8876: the version-0 `atan2` stays available, bit for bit, for paths that must match a tool built
/// on this libm (Stage A world_geom compat, the tunnel bake's portal yaw).
#[test]
fn atan2_libm_keeps_the_version_0_bits() {
    let xs = inputs();
    let mut h = Fnv::new();
    for (i, &x) in xs.iter().enumerate() {
        let y = xs[(i * 7 + 3) % xs.len()];
        h.f32(math::atan2_libm(x, y));
    }
    assert_eq!(
        h.0, PINNED_ATAN2_LIBM_V0,
        "atan2_libm moved off the version-0 bits"
    );
    let ys = sweep_f32(200_000, 0x0123_4567_89ab_cdef);
    let xs = sweep_f32(200_000, 0xfedc_ba98_7654_3210);
    for (&y, &x) in ys.iter().zip(&xs) {
        let (ours, theirs) = (math::atan2_libm(y, x), libm::atan2f(y, x));
        assert!(
            ours.to_bits() == theirs.to_bits() || (ours.is_nan() && theirs.is_nan()),
            "atan2_libm({y:e}, {x:e})"
        );
    }
    // The version-0 signed-zero behavior the canonical atan2 drops.
    assert_eq!(math::atan2_libm(-0.0, -1.0), -core::f32::consts::PI);
    assert_eq!(math::atan2_libm(-0.0, 1.0).to_bits(), (-0.0f32).to_bits());
}

#[test]
fn spot_values_are_libm_0_2_16() {
    // Values a reader can check by hand against any correctly rounded reference.
    assert_eq!(math::sqrt(4.0), 2.0);
    assert_eq!(math::sqrt_f64(2.25), 1.5);
    assert_eq!(math::exp(0.0), 1.0);
    assert_eq!(math::powf(2.0, 10.0), 1024.0);
    assert_eq!(math::sin(0.0).to_bits(), 0.0_f32.to_bits());
    assert_eq!(math::sin(-0.0).to_bits(), (-0.0_f32).to_bits());
    assert_eq!(math::cos(0.0), 1.0);
    assert_eq!(math::atan2(0.0, 1.0), 0.0);
    assert_eq!(math::atan2(1.0, 0.0), core::f32::consts::FRAC_PI_2);
    assert!(math::sqrt(-1.0).is_nan());
}

// ---- The extended math, `DetRng` and the integer helpers (lane det-math-rng-fixed) ----

use regia_detsim::fixed::{self, Mm};
use regia_detsim::rng::{DetRng, DetRngParts};

/// `n` inputs from a seeded xorshift: raw bit patterns (every class, NaNs included) and, every other
/// draw, a value in a moderate range so the functions see their interesting domain too.
fn sweep_f32(n: usize, seed: u64) -> Vec<f32> {
    let mut x = seed;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bits = (x >> 32) as u32;
        out.push(if i % 2 == 0 {
            f32::from_bits(bits)
        } else {
            // [-16, 16) in steps of 2^-23.
            (bits >> 4) as f32 / 8_388_608.0 * 2.0 - 16.0
        });
    }
    out
}

fn sweep_f64(n: usize, seed: u64) -> Vec<f64> {
    let mut x = seed;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        out.push(if i % 2 == 0 {
            f64::from_bits(x)
        } else {
            (x >> 11) as f64 / 9_007_199_254_740_992.0 * 32.0 - 16.0
        });
    }
    out
}

#[test]
fn new_math_wrappers_match_libm_bits() {
    const N: usize = 1_000_000;
    let xs = sweep_f32(N, 0x9e37_79b9_7f4a_7c15);
    let ys = sweep_f32(N, 0xd1b5_4a32_d192_ed03);
    type F1 = (&'static str, fn(f32) -> f32, fn(f32) -> f32);
    let unary: [F1; 13] = [
        ("tan", math::tan, libm::tanf),
        ("asin", math::asin, libm::asinf),
        ("acos", math::acos, libm::acosf),
        ("atan", math::atan, libm::atanf),
        ("ln", math::ln, libm::logf),
        ("log2", math::log2, libm::log2f),
        ("log10", math::log10, libm::log10f),
        ("exp2", math::exp2, libm::exp2f),
        ("cbrt", math::cbrt, libm::cbrtf),
        ("sinh", math::sinh, libm::sinhf),
        ("cosh", math::cosh, libm::coshf),
        ("tanh", math::tanh, libm::tanhf),
        ("sin", math::sin, libm::sinf),
    ];
    for (name, ours, theirs) in unary {
        for &x in &xs {
            assert_eq!(ours(x).to_bits(), theirs(x).to_bits(), "{name}({x:e})");
        }
    }
    for (&x, &y) in xs.iter().zip(&ys) {
        assert_eq!(
            math::hypot(x, y).to_bits(),
            libm::hypotf(x, y).to_bits(),
            "hypot({x:e}, {y:e})"
        );
        let (s, c) = math::sin_cos(x);
        // Two calls: exactly the single functions, and libm's own sinf/cosf.
        assert_eq!(s.to_bits(), math::sin(x).to_bits());
        assert_eq!(c.to_bits(), math::cos(x).to_bits());
        assert_eq!(c.to_bits(), libm::cosf(x).to_bits());
    }

    let xs = sweep_f64(N, 0x2545_f491_4f6c_dd1d);
    let ys = sweep_f64(N, 0x0123_4567_89ab_cdef);
    type D1 = (&'static str, fn(f64) -> f64, fn(f64) -> f64);
    let unary: [D1; 15] = [
        ("sin_f64", math::sin_f64, libm::sin),
        ("cos_f64", math::cos_f64, libm::cos),
        ("tan_f64", math::tan_f64, libm::tan),
        ("asin_f64", math::asin_f64, libm::asin),
        ("acos_f64", math::acos_f64, libm::acos),
        ("atan_f64", math::atan_f64, libm::atan),
        ("exp_f64", math::exp_f64, libm::exp),
        ("exp2_f64", math::exp2_f64, libm::exp2),
        ("ln_f64", math::ln_f64, libm::log),
        ("log2_f64", math::log2_f64, libm::log2),
        ("log10_f64", math::log10_f64, libm::log10),
        ("cbrt_f64", math::cbrt_f64, libm::cbrt),
        ("sinh_f64", math::sinh_f64, libm::sinh),
        ("cosh_f64", math::cosh_f64, libm::cosh),
        ("tanh_f64", math::tanh_f64, libm::tanh),
    ];
    for (name, ours, theirs) in unary {
        for &x in &xs {
            assert_eq!(ours(x).to_bits(), theirs(x).to_bits(), "{name}({x:e})");
        }
    }
    type D2 = (&'static str, fn(f64, f64) -> f64, fn(f64, f64) -> f64);
    let binary: [D2; 3] = [
        ("atan2_f64", math::atan2_f64, libm::atan2),
        ("powf_f64", math::powf_f64, libm::pow),
        ("hypot_f64", math::hypot_f64, libm::hypot),
    ];
    for (name, ours, theirs) in binary {
        for (&x, &y) in xs.iter().zip(&ys) {
            assert_eq!(
                ours(x, y).to_bits(),
                theirs(x, y).to_bits(),
                "{name}({x:e}, {y:e})"
            );
        }
    }
    for &x in &xs {
        let (s, c) = math::sin_cos_f64(x);
        assert_eq!(
            (s.to_bits(), c.to_bits()),
            (libm::sin(x).to_bits(), libm::cos(x).to_bits())
        );
    }
}

#[test]
fn new_math_spot_values() {
    // Values a reader can check by hand.
    assert_eq!(math::tan(0.0), 0.0);
    assert_eq!(math::atan(1.0), core::f32::consts::FRAC_PI_4);
    assert_eq!(math::asin(1.0), core::f32::consts::FRAC_PI_2);
    assert_eq!(math::acos(1.0), 0.0);
    assert!(math::asin(1.5).is_nan());
    assert_eq!(math::ln(1.0), 0.0);
    assert_eq!(math::ln(0.0), f32::NEG_INFINITY);
    assert!(math::ln(-1.0).is_nan());
    assert_eq!(math::log2(1024.0), 10.0);
    assert_eq!(math::log10(1000.0), 3.0);
    assert_eq!(math::exp2(10.0), 1024.0);
    assert_eq!(math::hypot(3.0, 4.0), 5.0);
    assert_eq!(math::hypot(f32::MAX, 1.0), f32::MAX);
    assert_eq!(math::cbrt(-27.0), -3.0);
    assert_eq!(math::tanh(f32::INFINITY), 1.0);
    assert_eq!(math::cosh(0.0), 1.0);
    assert_eq!(math::sinh(-0.0).to_bits(), (-0.0_f32).to_bits());
    assert_eq!(math::log2_f64(0.5), -1.0);
    assert_eq!(math::powf_f64(2.0, 10.0), 1024.0);
}

/// FNV-1a of a stream's first `n` outputs.
fn stream_hash(mut r: DetRng, n: usize) -> u64 {
    let mut h = Fnv::new();
    for _ in 0..n {
        let v = r.next_u64();
        h.word(v as u32);
        h.word((v >> 32) as u32);
    }
    h.0
}

#[test]
fn det_rng_streams_are_independent_and_stable() {
    // The generator is the published xoshiro256**: the reference implementation's outputs from the
    // state {1, 2, 3, 4}.
    let mut r = DetRng::from_state(0, [1, 2, 3, 4]).expect("non-zero state");
    let reference: [u64; 10] = [
        11_520,
        0,
        1_509_978_240,
        1_215_971_899_390_074_240,
        1_216_172_134_540_287_360,
        607_988_272_756_665_600,
        16_172_922_978_634_559_625,
        8_476_171_486_693_032_832,
        10_595_114_339_597_558_777,
        2_904_607_092_377_533_576,
    ];
    for want in reference {
        assert_eq!(r.next_u64(), want);
    }
    assert_eq!(DetRng::from_state(7, [0; 4]), None);

    // Pinned: the first outputs of the root and four streams (a change is a sim-version change).
    let root = DetRng::seed_from(0x5eed);
    let streams = [
        root.clone(),
        root.stream(0),
        root.stream(1),
        root.stream(u64::MAX),
        root.stream(1).stream(1),
    ];
    let got: Vec<u64> = streams.iter().map(|s| stream_hash(s.clone(), 64)).collect();
    let mut first = root.clone();
    let first4 = [
        first.next_u64(),
        first.next_u64(),
        first.next_u64(),
        first.next_u64(),
    ];
    println!("first4 {first4:#018x?}");
    println!("hashes {got:#018x?}");
    assert_eq!(first4, PINNED_ROOT_FIRST4);
    assert_eq!(got, PINNED_STREAM_HASHES);

    // Independent: no output is shared between the first 64 of any two streams.
    let mut seen = std::collections::BTreeSet::new();
    for s in &streams {
        let mut s = s.clone();
        for _ in 0..64 {
            assert!(seen.insert(s.next_u64()), "a value repeats across streams");
        }
    }
    // A stream depends on the key only, never on the draws made before.
    let mut drawn = root.clone();
    for _ in 0..1000 {
        drawn.next_u64();
    }
    assert_eq!(drawn.stream(1), root.stream(1));
    assert_ne!(root.stream(1), root.stream(2));
    assert_ne!(root.stream(5), DetRng::seed_from(5));
    // Paired outputs of two streams agree on about half their bits (no correlation).
    let (mut a, mut b) = (root.stream(1), root.stream(2));
    let mut same = 0u64;
    for _ in 0..10_000 {
        same += u64::from((!(a.next_u64() ^ b.next_u64())).count_ones());
    }
    let ratio = same as f64 / 640_000.0;
    assert!((0.49..0.51).contains(&ratio), "{ratio}");

    // Serde: the state round-trips mid-sequence, and an all-zero state is refused.
    let mut live = root.stream(3);
    live.next_u64();
    let json = serde_json::to_string(&live).expect("serialize");
    let mut back: DetRng = serde_json::from_str(&json).expect("deserialize");
    assert_eq!(back, live);
    assert_eq!(back.next_u64(), live.next_u64());
    let zero = serde_json::to_string(&DetRngParts { key: 1, s: [0; 4] }).expect("serialize");
    let err = serde_json::from_str::<DetRng>(&zero).expect_err("zero state");
    assert!(err.to_string().contains("det_rng_zero_state"), "{err}");
}

#[test]
fn det_rng_ranges_and_floats() {
    let mut r = DetRng::seed_from(42);
    for bound in [1u32, 2, 3, 7, 1000, u32::MAX] {
        for _ in 0..1000 {
            assert!(r.range_u32(bound) < bound);
        }
    }
    assert_eq!(r.range_u32(1), 0);
    for _ in 0..1000 {
        let v = r.range_i32(-5, 5);
        assert!((-5..5).contains(&v));
        let f = r.unit_f32();
        assert!((0.0..1.0).contains(&f));
        // A multiple of 2^-24, so exact in f32 and the same everywhere.
        assert_eq!((f * 16_777_216.0).fract(), 0.0);
        let d = r.unit_f64();
        assert!((0.0..1.0).contains(&d));
        assert_eq!((d * 9_007_199_254_740_992.0).fract(), 0.0);
    }
    assert_eq!(r.range_i32(3, 3), 3);
    assert_eq!(r.range_i32(3, -3), 3);
    let full = r.range_i32(i32::MIN, i32::MAX);
    assert!(full < i32::MAX);
    // A d6 over many rolls: every face, roughly evenly.
    let mut faces = [0u32; 6];
    for _ in 0..60_000 {
        faces[r.range_u32(6) as usize] += 1;
    }
    assert!(faces.iter().all(|&c| (9_500..10_500).contains(&c)), "{faces:?}");
    let mut deck: Vec<u32> = (0..52).collect();
    r.shuffle(&mut deck);
    let mut sorted = deck.clone();
    sorted.sort_unstable();
    assert_eq!(sorted, (0..52).collect::<Vec<_>>());
    assert_ne!(deck, sorted);
}

#[test]
fn fixed_helpers_public_surface() {
    assert_eq!(Mm::from_m_f32(1.5), Some(Mm(1500)));
    assert_eq!(Mm(1500).to_m_f32(), 1.5);
    assert_eq!(fixed::dot([3, 4], [3, 4]), 25);
    assert_eq!(fixed::cross([1, 0], [0, 1]), 1);
    assert_eq!(fixed::isqrt_u64(fixed::len2([3_000, 4_000])), 5_000);
    let json = serde_json::to_string(&Mm(-7)).expect("serialize");
    assert_eq!(json, "-7");
}

/// The first four outputs of `DetRng::seed_from(0x5eed)` (recorded on x86-64 Linux, 2026-11-18).
const PINNED_ROOT_FIRST4: [u64; 4] = [
    0xef33_f170_5524_4b74,
    0xe1f5_9111_2fb5_051b,
    0xd8ab_0564_0214_863a,
    0xf985_e1f2_fb89_7b03,
];

/// FNV-1a of the first 64 outputs of the root, `stream(0)`, `stream(1)`, `stream(u64::MAX)` and
/// `stream(1).stream(1)` of `seed_from(0x5eed)`.
const PINNED_STREAM_HASHES: [u64; 5] = [
    0xfe65_cd76_9c9c_bef1,
    0x643d_756b_12a8_dc08,
    0x2414_20b5_ef20_77fb,
    0x33e1_4cc7_51f2_82ca,
    0x286e_e7eb_9814_2a10,
];
