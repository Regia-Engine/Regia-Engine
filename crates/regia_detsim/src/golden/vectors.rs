// SPDX-License-Identifier: MPL-2.0
//! The `math_rng_fixed_vectors` golden: a fixed table of inputs through every [`crate::math`]
//! wrapper added after the first seven, every [`crate::rng::DetRng`] draw and every
//! [`crate::fixed`] helper, hashed per function family.
//!
//! Each tick runs [`PER_TICK`] table entries; tick 0 holds the special values (`±0`, subnormals,
//! `±MIN_POSITIVE`, `±MAX`, `±inf`, NaN, the edges of `exp`/`sinh` overflow and `asin`/`acos`
//! domain, large trig arguments). Later ticks hold a sweep of `[-8, 8]`, uniform values in
//! `[-1, 1)` and raw bit patterns (all of them, NaNs and infinities included) drawn from the seed.
//! The state is one FNV-1a accumulator per family, named after the function, so a target whose
//! bits differ reports the first tick (the table rows) and the function that moved. NaN results are
//! folded to one pattern (x86-64 and aarch64 differ in NaN payloads), as everywhere in the harness.

use crate::fixed::{self, Mm, Mm2};
use crate::math;
use crate::rng::DetRng;
use crate::tick::TickRate;

use super::{DetRun, DetScenario, StateHasher};

/// Table entries per tick.
pub const PER_TICK: usize = 64;

/// Ticks of the committed run (`PER_TICK * TICKS` table entries).
pub const TICKS: u32 = 64;

/// The function families, in hash order. The first is the field [`DetScenario::flip_bit`] flips.
pub const FAMILIES: [&str; 37] = [
    "tan",
    "asin",
    "acos",
    "atan",
    "ln",
    "log2",
    "log10",
    "exp2",
    "hypot",
    "cbrt",
    "sinh",
    "cosh",
    "tanh",
    "sin_cos",
    "sin_f64",
    "cos_f64",
    "sin_cos_f64",
    "tan_f64",
    "asin_f64",
    "acos_f64",
    "atan_f64",
    "atan2_f64",
    "exp_f64",
    "exp2_f64",
    "ln_f64",
    "log2_f64",
    "log10_f64",
    "powf_f64",
    "hypot_f64",
    "cbrt_f64",
    "sinh_f64",
    "cosh_f64",
    "tanh_f64",
    "rng",
    "rng_range",
    "rng_unit",
    "fixed",
];

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fold(acc: &mut u64, word: u64) {
    for b in word.to_le_bytes() {
        *acc ^= u64::from(b);
        *acc = acc.wrapping_mul(FNV_PRIME);
    }
}

fn f32w(x: f32) -> u64 {
    u64::from(super::canonical_f32_bits(x))
}

fn f64w(x: f64) -> u64 {
    if x.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        x.to_bits()
    }
}

/// An `Option<Mm>` as one word: the value, or a tag no value has.
fn mm_word(m: Option<Mm>) -> u64 {
    m.map_or(0x1_0000_0000, |m| u64::from(m.get() as u32))
}

/// The special f32 inputs (tick 0).
fn specials_f32() -> Vec<f32> {
    let mut v = vec![
        0.0,
        -0.0,
        f32::from_bits(1),
        -f32::from_bits(1),
        f32::from_bits(0x007f_ffff),
        -f32::from_bits(0x007f_ffff),
        f32::MIN_POSITIVE,
        -f32::MIN_POSITIVE,
        f32::MAX,
        f32::MIN,
        f32::INFINITY,
        f32::NEG_INFINITY,
        f32::NAN,
        f32::EPSILON,
        1.0,
        -1.0,
        0.5,
        -0.5,
        2.0,
        1.0 - f32::EPSILON / 2.0,
        -(1.0 - f32::EPSILON / 2.0),
        1.0 + f32::EPSILON,
        core::f32::consts::PI,
        -core::f32::consts::PI,
        core::f32::consts::FRAC_PI_2,
        core::f32::consts::FRAC_PI_4,
        core::f32::consts::E,
        10.0,
        // exp2/sinh/cosh overflow and underflow edges.
        88.72283,
        89.5,
        -87.3,
        -103.97208,
        127.0,
        128.0,
        -126.0,
        -149.0,
        -150.0,
        // tanh saturation, cbrt of a cube.
        9.0,
        -9.0,
        27.0,
        -0.001,
        // Large trig arguments (argument reduction).
        1.0e4,
        -1.0e6,
        1.0e10,
        -1.0e20,
        1.0e30,
        3.0e38,
        102_940.94, // the f32 102940.9375
        // Meters near the i32 millimeter edges.
        2_147_483.5,
        -2_147_483.5,
        2_147_483.5 + 0.25,
        -(2_147_483.5 + 0.25),
        0.0625,
        -0.0625,
    ];
    v.truncate(PER_TICK);
    v
}

/// The special f64 inputs (tick 0, padded with the f32 specials widened).
fn specials_f64() -> Vec<f64> {
    vec![
        0.0,
        -0.0,
        f64::from_bits(1),
        -f64::from_bits(1),
        f64::from_bits(0x000f_ffff_ffff_ffff),
        f64::MIN_POSITIVE,
        -f64::MIN_POSITIVE,
        f64::MAX,
        f64::MIN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::NAN,
        f64::EPSILON,
        1.0,
        -1.0,
        1.0 - f64::EPSILON / 2.0,
        1.0 + f64::EPSILON,
        core::f64::consts::PI,
        core::f64::consts::FRAC_PI_2,
        709.78,
        710.0,
        -745.13,
        -746.0,
        1024.0,
        -1074.0,
        -1075.0,
        20.0,
        1.0e22,
        1.0e300,
        -1.0e300,
        6.0e15,
    ]
}

/// The deterministic golden over the extended math, the RNG and the integer helpers.
#[derive(Debug, Clone)]
pub struct MathRngFixedScenario {
    xs: Vec<f32>,
    xs64: Vec<f64>,
    acc: [u64; FAMILIES.len()],
    rng_a: DetRng,
    rng_b: DetRng,
}

impl Default for MathRngFixedScenario {
    fn default() -> Self {
        MathRngFixedScenario::new()
    }
}

impl MathRngFixedScenario {
    /// A scenario before its first reset.
    pub fn new() -> Self {
        let root = DetRng::seed_from(0);
        MathRngFixedScenario {
            xs: Vec::new(),
            xs64: Vec::new(),
            acc: [FNV_OFFSET; FAMILIES.len()],
            rng_a: root.stream(1),
            rng_b: root.stream(2),
        }
    }

    /// The accumulator of `family` (a [`FAMILIES`] name).
    pub fn family_hash(&self, family: &str) -> Option<u64> {
        FAMILIES.iter().position(|f| *f == family).map(|i| self.acc[i])
    }

    fn run_math(&mut self, i: usize) {
        let n = self.xs.len();
        let x = self.xs[i];
        let y = self.xs[(i * 7 + 3) % n];
        let x64 = self.xs64[i];
        let y64 = self.xs64[(i * 7 + 3) % n];
        let a = &mut self.acc;
        let f32s = [
            math::tan(x),
            math::asin(x),
            math::acos(x),
            math::atan(x),
            math::ln(x),
            math::log2(x),
            math::log10(x),
            math::exp2(x),
            math::hypot(x, y),
            math::cbrt(x),
            math::sinh(x),
            math::cosh(x),
            math::tanh(x),
        ];
        for (k, r) in f32s.into_iter().enumerate() {
            fold(&mut a[k], f32w(r));
        }
        let (s, c) = math::sin_cos(x);
        fold(&mut a[13], f32w(s) | (f32w(c) << 32));
        let f64s = [math::sin_f64(x64), math::cos_f64(x64)];
        for (k, r) in f64s.into_iter().enumerate() {
            fold(&mut a[14 + k], f64w(r));
        }
        let (s, c) = math::sin_cos_f64(x64);
        fold(&mut a[16], f64w(s));
        fold(&mut a[16], f64w(c));
        let f64s = [
            math::tan_f64(x64),
            math::asin_f64(x64),
            math::acos_f64(x64),
            math::atan_f64(x64),
            math::atan2_f64(x64, y64),
            math::exp_f64(x64),
            math::exp2_f64(x64),
            math::ln_f64(x64),
            math::log2_f64(x64),
            math::log10_f64(x64),
            math::powf_f64(x64.abs(), y64),
            math::hypot_f64(x64, y64),
            math::cbrt_f64(x64),
            math::sinh_f64(x64),
            math::cosh_f64(x64),
            math::tanh_f64(x64),
        ];
        for (k, r) in f64s.into_iter().enumerate() {
            fold(&mut a[17 + k], f64w(r));
        }
        // Fixed: the f32 meter conversions of the table value.
        fold(&mut a[36], mm_word(Mm::from_m_f32(x)));
        fold(&mut a[36], mm_word(Some(Mm::from_m_f32_saturating(x))));
    }

    fn run_rng(&mut self, tick: u32) {
        let a = &mut self.acc;
        for r in [&mut self.rng_a, &mut self.rng_b] {
            for _ in 0..4 {
                fold(&mut a[33], r.next_u64());
            }
            fold(&mut a[33], u64::from(r.next_u32()));
            for bound in [0u32, 1, 2, 3, 6, 7, 10, 100, 1000, (1 << 31) + 1, u32::MAX] {
                fold(&mut a[34], u64::from(r.range_u32(bound)));
            }
            for bound in [0u64, 3, (1 << 63) + 1, u64::MAX] {
                fold(&mut a[34], r.range_u64(bound));
            }
            fold(&mut a[34], u64::from(r.range_i32(-1000, 1000) as u32));
            fold(&mut a[34], u64::from(r.range_i32(i32::MIN, i32::MAX) as u32));
            fold(&mut a[34], u64::from(r.range_i32(5, 5) as u32));
            fold(&mut a[35], f32w(r.unit_f32()));
            fold(&mut a[35], f64w(r.unit_f64()));
            let mut deck: [u8; 16] = core::array::from_fn(|i| i as u8);
            r.shuffle(&mut deck);
            fold(
                &mut a[34],
                u64::from_le_bytes([
                    deck[0], deck[1], deck[2], deck[3], deck[4], deck[5], deck[6], deck[7],
                ]),
            );
            fold(
                &mut a[34],
                u64::from_le_bytes([
                    deck[8], deck[9], deck[10], deck[11], deck[12], deck[13], deck[14], deck[15],
                ]),
            );
            // Integer helpers on drawn values: full-range vectors, then small ones.
            for shift in [0u32, 20] {
                let v = [r.next_u32() as i32 >> shift, r.next_u32() as i32 >> shift];
                let w = [r.next_u32() as i32 >> shift, r.next_u32() as i32 >> shift];
                fold(&mut a[36], fixed::dot(v, w) as u64);
                fold(&mut a[36], fixed::cross(v, w) as u64);
                fold(&mut a[36], fixed::len2(v));
                fold(&mut a[36], fixed::isqrt_u64(fixed::len2(v)));
                let m = Mm(v[0]);
                fold(&mut a[36], u64::from(m.to_m_f32().to_bits()));
                fold(&mut a[36], mm_word(m.checked_add(Mm(w[0]))));
                fold(&mut a[36], mm_word(Some(m.saturating_mul(w[1]))));
                fold(&mut a[36], mm_word(Mm2(fixed::dot(v, w)).isqrt()));
            }
            fold(&mut a[36], fixed::isqrt_u64(r.next_u64()));
        }
        fold(&mut a[33], u64::from(tick));
        // The streams of a fixed key, made mid-run: they must not depend on the draws above.
        let mut late = DetRng::seed_from(u64::from(tick)).stream(u64::MAX);
        fold(&mut a[33], late.next_u64());
    }
}

impl DetScenario for MathRngFixedScenario {
    fn name(&self) -> &str {
        "math_rng_fixed_vectors"
    }

    fn default_run(&self) -> DetRun {
        DetRun::new(0x5eed_0005, TICKS, TickRate::DEFAULT)
    }

    fn reset(&mut self, run: &DetRun) {
        let total = PER_TICK * run.ticks.max(1) as usize;
        let root = DetRng::seed_from(run.seed);
        let mut table = root.stream(0);
        let mut xs = specials_f32();
        let mut xs64 = specials_f64();
        xs64.truncate(xs.len());
        // The f64 specials are fewer than the f32 ones: pad with the f32 specials widened.
        while xs64.len() < xs.len() {
            xs64.push(f64::from(xs[xs64.len()]));
        }
        let mut i = 0u32;
        while xs.len() < total {
            let region = (xs.len() / PER_TICK) % 4;
            let (x, x64) = match region {
                // A sweep of [-8, 8] in steps of 1/64 (wrapping).
                0 => {
                    let v = (i % 1025) as f32 / 64.0 - 8.0;
                    (v, f64::from(v) + f64::from(v) * 1.0e-9)
                }
                // Uniform in [-1, 1) for the inverse trig domain.
                1 => (table.unit_f32() * 2.0 - 1.0, table.unit_f64() * 2.0 - 1.0),
                // Raw bit patterns (NaNs and infinities included).
                2 => (f32::from_bits(table.next_u32()), f64::from_bits(table.next_u64())),
                // Moderate magnitudes: [-1, 1) scaled by 2^k, k in [-20, 20].
                _ => {
                    let k = table.range_i32(-20, 21);
                    let s = math::exp2(k as f32);
                    (
                        (table.unit_f32() * 2.0 - 1.0) * s,
                        (table.unit_f64() * 2.0 - 1.0) * f64::from(s),
                    )
                }
            };
            xs.push(x);
            xs64.push(x64);
            i += 1;
        }
        xs.truncate(total);
        xs64.truncate(total);
        self.xs = xs;
        self.xs64 = xs64;
        self.acc = [FNV_OFFSET; FAMILIES.len()];
        self.rng_a = root.stream(1);
        self.rng_b = root.stream(u64::MAX);
    }

    fn step(&mut self, tick: u32) {
        let start = tick as usize * PER_TICK;
        for i in start..(start + PER_TICK).min(self.xs.len()) {
            self.run_math(i);
        }
        self.run_rng(tick);
    }

    fn hash_state(&self, h: &mut StateHasher) {
        for (name, acc) in FAMILIES.into_iter().zip(self.acc) {
            h.u64(name, acc);
        }
        for (r, names) in [
            (&self.rng_a, ["rng_a.s0", "rng_a.s1", "rng_a.s2", "rng_a.s3"]),
            (&self.rng_b, ["rng_b.s0", "rng_b.s1", "rng_b.s2", "rng_b.s3"]),
        ] {
            for (name, w) in names.into_iter().zip(r.state()) {
                h.u64(name, w);
            }
        }
    }

    fn flip_bit(&mut self, bit: u32) -> bool {
        self.acc[0] ^= 1 << (bit % 64);
        true
    }
}
