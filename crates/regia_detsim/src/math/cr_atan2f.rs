// SPDX-License-Identifier: MIT
// Portions ported from the CORE-MATH project (https://gitlab.inria.fr/core-math/core-math,
// src/binary32/atan2/atan2f.c, commit 567e1efc10401e672029eeab01a295471324ad3a), Copyright (c) 2022-2025
// Alexei Sibidanov and Paul Zimmermann, MIT licence:
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.
//
// Changes (ADR-8876): Rust; the C hexadecimal float constants are `f64::from_bits` of the same values
// (generated from the C source, the C literal beside each); `__builtin_fma` is `libm::fma` through
// `math::fma_f64`, the one libm boundary `cargo xtask det-scan` audits (an exact IEEE
// operation, so every correct implementation gives the same bits and no host FMA unit is assumed);
// round-to-nearest only (Rust's float environment), so the directed-rounding nudge is kept as written
// but only its nearest-mode effect is reachable; no errno. The NaN result keeps the C `x + y`; the
// caller (`math::atan2`) canonicalizes it.

//! Correctly rounded binary32 `atan2` (CORE-MATH `cr_atan2f`), the core of `math::atan2`.
//!
//! A fast path evaluates a degree-6/6 rational approximation of `atan(z)`, `z = min(|y|,|x|) /
//! max(|y|,|x|)`, in binary64 and adds the quadrant offset. When that binary64 result lies within
//! 8 units of its 29th bit below the binary32 rounding point (the rounding cannot be decided from it),
//! a second path evaluates a degree-63 odd polynomial in double-double arithmetic, and tiny ratios
//! (`|y/x| < 2^-25`) take `z - z^3/3` with a correction for the binary32 rounding boundary. The
//! authors prove the result is the correctly rounded `atan2(y, x)` for every pair of binary32 inputs.

use super::fma_f64 as fma;

/// `f64` from its IEEE bits, for the C hexadecimal constants.
const fn f(bits: u64) -> f64 {
    f64::from_bits(bits)
}

/// π (`0x1.921fb54442d18p+1`).
const PI: f64 = f(0x4009_21fb_5444_2d18);
/// π/2 (`0x1.921fb54442d18p+0`).
const PI2: f64 = f(0x3ff9_21fb_5444_2d18);
/// The low word of π/2 (`0x1.1a62633145c07p-54`).
const PI2L: f64 = f(0x3c91_a626_3314_5c07);
/// The low word of π, `2 * PI2L` (exact).
const PIL: f64 = f(0x3ca1_a626_3314_5c07);
/// 3π/4 (`0x1.2d97c7f3321d2p+1`).
const PI3_4: f64 = f(0x4002_d97c_7f33_21d2);
/// π/4 (`0x1.921fb54442d18p-1`).
const PI_4: f64 = f(0x3fe9_21fb_5444_2d18);
/// -1/3 rounded to nearest (`-0x1.5555555555555p-2`).
const MINUS_THIRD: f64 = f(0xbfd5_5555_5555_5555);
/// `0x1p-60`.
const TWO_M60: f64 = f(0x3c30_0000_0000_0000);

/// Numerator of the fast-path rational approximation (`cn[]`).
const CN: [f64; 7] = [
    f(0x3ff0_0000_0000_0000), // 0x1p+0
    f(0x4004_0e06_98f9_4c35), // 0x1.40e0698f94c35p+1
    f(0x4002_48c5_da34_7f0d), // 0x1.248c5da347f0dp+1
    f(0x3fed_8733_8657_2976), // 0x1.d873386572976p-1
    f(0x3fc4_6fa4_0b20_f1d0), // 0x1.46fa40b20f1dp-3
    f(0x3f83_3f5e_041e_ed0f), // 0x1.33f5e041eed0fp-7
    f(0x3f15_46bb_f286_67c5), // 0x1.546bbf28667c5p-14
];

/// Denominator of the fast-path rational approximation (`cd[]`).
const CD: [f64; 7] = [
    f(0x3ff0_0000_0000_0000), // 0x1p+0
    f(0x4006_b8b1_43a3_f6da), // 0x1.6b8b143a3f6dap+1
    f(0x4008_4212_01d1_8ed5), // 0x1.8421201d18ed5p+1
    f(0x3ff8_221d_0869_14eb), // 0x1.8221d086914ebp+0
    f(0x3fd6_7065_7e3a_07ba), // 0x1.670657e3a07bap-2
    f(0x3fa0_f495_1fd1_e72d), // 0x1.0f4951fd1e72dp-5
    f(0x3f4b_3874_b879_8286), // 0x1.b3874b8798286p-11
];

/// Double-double coefficients of the accurate-path polynomial in `z^2` (`c[32][2]`).
const C: [[f64; 2]; 32] = [
    [f(0x3ff0_0000_0000_0000), f(0xba88_c1da_c549_2248)], // 0x1p+0, -0x1.8c1dac5492248p-87
    [f(0xbfd5_5555_5555_5555), f(0xbc75_5553_bf3a_2abe)], // -0x1.5555555555555p-2, -0x1.55553bf3a2abep-56
    [f(0x3fc9_9999_9999_999a), f(0xbc69_9dee_d1ec_9071)], // 0x1.999999999999ap-3, -0x1.99deed1ec9071p-57
    [f(0xbfc2_4924_9249_2492), f(0xbc5f_d99c_8d18_269a)], // -0x1.2492492492492p-3, -0x1.fd99c8d18269ap-58
    [f(0x3fbc_71c7_1c71_c717), f(0xbc26_51ee_e4c4_d9d0)], // 0x1.c71c71c71c717p-4, -0x1.651eee4c4d9dp-61
    [f(0xbfb7_45d1_745d_1649), f(0xbc56_3268_3d6c_44a6)], // -0x1.745d1745d1649p-4, -0x1.632683d6c44a6p-58
    [f(0x3fb3_b13b_13b1_1c63), f(0x3c5b_f69c_1f8a_f41d)], // 0x1.3b13b13b11c63p-4, 0x1.bf69c1f8af41dp-58
    [f(0xbfb1_1111_110e_6338), f(0x3c23_c3e4_31e8_bb68)], // -0x1.11111110e6338p-4, 0x1.3c3e431e8bb68p-61
    [f(0x3fae_1e1e_1dc4_5c4a), f(0xbc4b_e2db_05c7_7bbf)], // 0x1.e1e1e1dc45c4ap-5, -0x1.be2db05c77bbfp-59
    [f(0xbfaa_f286_b816_4b4f), f(0x3c2a_4673_491f_0942)], // -0x1.af286b8164b4fp-5, 0x1.a4673491f0942p-61
    [f(0x3fa8_6185_e9ad_4846), f(0x3c4e_12e3_2d79_fcee)], // 0x1.86185e9ad4846p-5, 0x1.e12e32d79fceep-59
    [f(0xbfa6_42c6_d516_1fae), f(0x3c43_ce76_c1ca_03f0)], // -0x1.642c6d5161faep-5, 0x1.3ce76c1ca03fp-59
    [f(0x3fa4_7ad6_f277_e5bf), f(0xbc3a_bd8d_85bd_b714)], // 0x1.47ad6f277e5bfp-5, -0x1.abd8d85bdb714p-60
    [f(0xbfa2_f64a_2ee8_896d), f(0x3c2e_f87d_4b61_5323)], // -0x1.2f64a2ee8896dp-5, 0x1.ef87d4b615323p-61
    [f(0x3fa1_a6a2_b317_41b5), f(0x3c1a_5d9d_9735_47ee)], // 0x1.1a6a2b31741b5p-5, 0x1.a5d9d973547eep-62
    [f(0xbfa0_7fbd_ad65_e0a6), f(0xbc26_5ac0_7f5d_35f4)], // -0x1.07fbdad65e0a6p-5, -0x1.65ac07f5d35f4p-61
    [f(0x3f9e_e993_2a9a_5f8b), f(0x3c2f_8b96_23f6_f55a)], // 0x1.ee9932a9a5f8bp-6, 0x1.f8b9623f6f55ap-61
    [f(0xbf9c_e8b5_b958_4dc6), f(0x3c2f_e5af_96e8_ea2d)], // -0x1.ce8b5b9584dc6p-6, 0x1.fe5af96e8ea2dp-61
    [f(0x3f9a_c9cb_2880_87b7), f(0xbc34_50cd_fcea_f5ca)], // 0x1.ac9cb288087b7p-6, -0x1.450cdfceaf5cap-60
    [f(0xbf98_4b02_5351_f3e6), f(0x3c25_7956_1b0d_73da)], // -0x1.84b025351f3e6p-6, 0x1.579561b0d73dap-61
    [f(0x3f95_2f5b_8ecd_d52b), f(0x3c30_36bd_2c6f_ba47)], // 0x1.52f5b8ecdd52bp-6, 0x1.036bd2c6fba47p-60
    [f(0xbf91_63a8_c449_09dc), f(0x3c31_8f73_5ffb_9f16)], // -0x1.163a8c44909dcp-6, 0x1.18f735ffb9f16p-60
    [f(0x3f8a_400d_ce3e_ea6f), f(0xbc2c_9056_9c0c_1b5c)], // 0x1.a400dce3eea6fp-7, -0x1.c90569c0c1b5cp-61
    [f(0xbf81_caa7_8ae6_db3a), f(0xbc24_c60f_8161_ea09)], // -0x1.1caa78ae6db3ap-7, -0x1.4c60f8161ea09p-61
    [f(0x3f75_2672_453c_0731), f(0x3c18_34ef_b598_c338)], // 0x1.52672453c0731p-8, 0x1.834efb598c338p-62
    [f(0xbf65_850c_5be1_37cf), f(0xbc04_45fc_150c_a7f5)], // -0x1.5850c5be137cfp-9, -0x1.445fc150ca7f5p-63
    [f(0x3f52_3eb9_8d22_e1ca), f(0xbbf3_88fb_af1d_7830)], // 0x1.23eb98d22e1cap-10, -0x1.388fbaf1d783p-64
    [f(0xbf38_f4e9_74a4_0741), f(0x3bd2_7119_8a97_da34)], // -0x1.8f4e974a40741p-12, 0x1.271198a97da34p-66
    [f(0x3f1a_5cf2_e9cf_76e5), f(0xbbb8_87eb_4a63_b665)], // 0x1.a5cf2e9cf76e5p-14, -0x1.887eb4a63b665p-68
    [f(0xbef4_20c2_7071_9e32), f(0x3b8e_fd59_5b27_888b)], // -0x1.420c270719e32p-16, 0x1.efd595b27888bp-71
    [f(0x3ec3_ba2d_69b5_1677), f(0xbb64_fb06_829c_dfc7)], // 0x1.3ba2d69b51677p-19, -0x1.4fb06829cdfc7p-73
    [f(0xbe82_9b7e_6f67_6385), f(0xbb2a_783b_6de7_18fb)], // -0x1.29b7e6f676385p-23, -0x1.a783b6de718fbp-77
];

/// The quadrant offsets, indexed by `sign(y) * 4 + sign(x) * 2 + (|y| > |x|)`.
const OFF: [f64; 8] = [0.0, PI2, PI, PI2, -0.0, -PI2, -PI, -PI2];
/// The low words of [`OFF`].
const OFFL: [f64; 8] = [0.0, PI2L, PIL, PI2L, -0.0, -PI2L, -PIL, -PI2L];
/// The binary32 bits of `+inf` (the exponent mask): above it, a NaN.
const INF: u32 = 0xff << 23;
/// `+1` for a clear sign bit, `-1` for a set one.
const SGN: [f64; 2] = [1.0, -1.0];
/// The selector the C code multiplies by to pick `x/y` or `y/x` without a branch.
const M: [f64; 2] = [0.0, 1.0];

/// `0.0 * SGN[s]` of the C source: `+0` for a clear sign bit, `-0` for a set one.
#[inline]
fn signed_zero(s: usize) -> f32 {
    if s == 1 { -0.0 } else { 0.0 }
}

/// Double-double product `(xh + xl) * (ch + cl)` as `(high, low)`.
#[inline]
fn muldd(xh: f64, xl: f64, ch: f64, cl: f64) -> (f64, f64) {
    let ahlh = ch * xl;
    let alhh = cl * xh;
    let ahhh = ch * xh;
    let mut ahhl = fma(ch, xh, -ahhh);
    ahhl += alhh + ahlh;
    let h = ahhh + ahhl;
    let l = (ahhh - h) + ahhl;
    (h, l)
}

/// Horner evaluation of the double-double polynomial `c` at `xh + xl`, as `(high, low)`.
fn polydd(xh: f64, xl: f64, c: &[[f64; 2]]) -> (f64, f64) {
    let mut i = c.len() - 1;
    let (mut ch, mut cl) = (c[i][0], c[i][1]);
    while i > 0 {
        i -= 1;
        let (h, l) = muldd(xh, xl, ch, cl);
        let th = h + c[i][0];
        let tl = (c[i][0] - th) + h;
        ch = th;
        cl = l + (tl + c[i][1]);
    }
    (ch, cl)
}

/// `atan2(y, x)` for `|y/x| < 2^-25`: `z - z^3/3`, `z = y/x` with its rounding error, nudged off a
/// binary32 rounding boundary so the final rounding is single.
fn tiny(y: f32, x: f32) -> f32 {
    let dy = f64::from(y);
    let dx = f64::from(x);
    let z = dy / dx;
    let mut e = fma(-z, dx, dy);
    // z * x + e = y, so y/x = z + e/x.
    let zz = z * z;
    let cz = MINUS_THIRD * z;
    e = e / dx + cz * zz;
    let mut t = z.to_bits();
    if t & 0x0fff_ffff == 0 {
        // A boundary case: move t one binary64 unit toward the true value so rounding t to binary32
        // does not double-round.
        if z * e > 0.0 {
            t += 1;
        } else {
            t -= 1;
        }
    }
    f64::from_bits(t) as f32
}

/// The correctly rounded binary32 `atan2(y, x)` (IEEE 754 § 9.2.1 special values, signed zeros as the
/// standard gives them); a NaN input gives a NaN (`x + y`, not canonical).
pub(super) fn cr_atan2f(y: f32, x: f32) -> f32 {
    let ux = x.to_bits();
    let uy = y.to_bits();
    let ax = ux & 0x7fff_ffff;
    let ay = uy & 0x7fff_ffff;
    let sy = (uy >> 31) as usize;
    let sx = (ux >> 31) as usize;
    if ay >= INF || ax >= INF {
        // x or y is NaN or infinite.
        if ay > INF || ax > INF {
            return x + y;
        }
        let yinf = ay == INF;
        let xinf = ax == INF;
        if yinf && xinf {
            return if sx == 1 {
                (PI3_4 * SGN[sy]) as f32
            } else {
                (PI_4 * SGN[sy]) as f32
            };
        }
        if xinf {
            return if sx == 1 {
                (PI * SGN[sy]) as f32
            } else {
                signed_zero(sy)
            };
        }
        // y is infinite, x finite.
        return (PI2 * SGN[sy]) as f32;
    }
    if ay == 0 {
        if ax == 0 {
            let i = sy * 4 + sx * 2;
            return if sx == 1 {
                (OFF[i] + OFFL[i]) as f32
            } else {
                OFF[i] as f32
            };
        }
        if sx == 0 {
            return signed_zero(sy);
        }
    }
    let gt = usize::from(ay > ax);
    let i = sy * 4 + sx * 2 + gt;

    let zx = f64::from(x);
    let zy = f64::from(y);
    // z = x/y when |y| > |x|, y/x otherwise.
    let mut z = (M[gt] * zx + M[1 - gt] * zy) / (M[gt] * zy + M[1 - gt] * zx);
    let d = ax as i32 - ay as i32;
    let r = if d < (27 << 23) && d > -(27 << 23) {
        let z2 = z * z;
        let z4 = z2 * z2;
        let z8 = z4 * z4;
        let mut cn0 = CN[0] + z2 * CN[1];
        let cn2 = CN[2] + z2 * CN[3];
        let mut cn4 = CN[4] + z2 * CN[5];
        let cn6 = CN[6];
        cn0 += z4 * cn2;
        cn4 += z4 * cn6;
        cn0 += z8 * cn4;
        let mut cd0 = CD[0] + z2 * CD[1];
        let cd2 = CD[2] + z2 * CD[3];
        let mut cd4 = CD[4] + z2 * CD[5];
        let cd6 = CD[6];
        cd0 += z4 * cd2;
        cd4 += z4 * cd6;
        cd0 += z8 * cd4;
        cn0 / cd0
    } else {
        1.0
    };
    z *= SGN[gt];
    let mut r = z * r + OFF[i];
    let res = r.to_bits();
    if (res.wrapping_add(8) & 0x0fff_ffff) <= 16 {
        // The fast result is too close to a binary32 rounding boundary.
        if ay < ax && ((ax - ay) >> 23) >= 25 {
            return tiny(y, x);
        }
        let (mut zh, mut zl) = if gt == 0 {
            let h = zy / zx;
            (h, fma(h, -zx, zy) / zx)
        } else {
            let h = zx / zy;
            (h, fma(h, -zy, zx) / zy)
        };
        let (z2h, z2l) = muldd(zh, zl, zh, zl);
        let (ph, pl) = polydd(z2h, z2l, &C);
        zh *= SGN[gt];
        zl *= SGN[gt];
        let (ph, pl) = muldd(zh, zl, ph, pl);
        let sh = ph + OFF[i];
        let sl = ((OFF[i] - sh) + ph) + pl + OFFL[i];
        let rf = sh as f32;
        let th = f64::from(rf);
        let dh = sh - th;
        let mut tm = dh + sl;
        if th + th * TWO_M60 == th - th * TWO_M60 {
            let mut tth = th.to_bits();
            tth &= 0x7ff << 52;
            tth = tth.wrapping_sub(24 << 52);
            if tm.abs() > f64::from_bits(tth) {
                tm *= 1.25;
            } else {
                tm *= 0.75;
            }
        }
        r = th + tm;
    }
    r as f32
}
