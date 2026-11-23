// SPDX-License-Identifier: MPL-2.0
//! Independent verification of `regia_asset_pipeline::bc6h_encode` against the BC6H format
//! itself (lane `sprint-zai-verify-bc6h`, ADR-2331).
//!
//! The module under test ships its own decoder, so its round-trip tests prove only that the
//! encoder and decoder agree with each other. This file carries a second, independently written
//! decoder (`spec`) built from the public BC6H description (Khronos Data Format Specification,
//! BPTC section, and the Direct3D 11 BC6H format page: all 14 modes, their bit layouts, the
//! endpoint transform, unquantization and the final `*31/64` scale), and validates that decoder
//! twice before trusting it:
//!
//! 1. against the cook's existing ISPC BC6H encoder (`intel_tex_2`, unsigned only): blocks it
//!    writes must decode close to the source (it is a production encoder, so a wrong mode layout
//!    in `spec` shows up as gross error in the blocks that use that mode);
//! 2. against a real Vulkan driver (Mesa lavapipe on the build server): random blocks of every mode, both
//!    formats, must decode bit-exactly as `spec` decodes them. Skips (passes with a message) when
//!    the adapter has no `TEXTURE_COMPRESSION_BC`.
//!
//! Then the module's blocks are decoded by `spec` and by the GPU: they must agree with the
//! module's own decoder exactly, and reproduce the source within the stated error floors.

use regia_asset_pipeline::bc6h_encode::{
    Bc6hFormat, Bc6hMetric, Bc6hModes, Bc6hOptions, decode_block, encode_block, encode_image,
};
use regia_render::RenderDevice;
use wgpu::util::DeviceExt;

/// Independent BC6H decoder (half-float bit patterns out).
mod spec {
    /// One BC6H mode: number as the format description counts them (1..=14), mode code, endpoint
    /// precision, delta widths per channel, and the bit layout in the description's notation.
    pub struct Mode {
        pub number: u8,
        pub code: u32,
        pub code_bits: u32,
        pub subsets: usize,
        pub transformed: bool,
        pub prec: u32,
        pub delta: [u32; 3],
        pub layout: &'static str,
    }

    /// Field `[a:b]`: bit `b` sits at the lowest block position and the bits run toward `a`, so
    /// `rw[9:0]` is rw0..rw9 in increasing position and `rw[10:15]` is rw15, rw14 .. rw10.
    pub const MODES: [Mode; 14] = [
        Mode {
            number: 1,
            code: 0b00,
            code_bits: 2,
            subsets: 2,
            transformed: true,
            prec: 10,
            delta: [5, 5, 5],
            layout: "m[1:0] gy[4] by[4] bz[4] rw[9:0] gw[9:0] bw[9:0] rx[4:0] gz[4] gy[3:0] gx[4:0] bz[0] gz[3:0] bx[4:0] bz[1] by[3:0] ry[4:0] bz[2] rz[4:0] bz[3]",
        },
        Mode {
            number: 2,
            code: 0b01,
            code_bits: 2,
            subsets: 2,
            transformed: true,
            prec: 7,
            delta: [6, 6, 6],
            layout: "m[1:0] gy[5] gz[4] gz[5] rw[6:0] bz[0] bz[1] by[4] gw[6:0] by[5] bz[2] gy[4] bw[6:0] bz[3] bz[5] bz[4] rx[5:0] gy[3:0] gx[5:0] gz[3:0] bx[5:0] by[3:0] ry[5:0] rz[5:0]",
        },
        Mode {
            number: 3,
            code: 0b00010,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 11,
            delta: [5, 4, 4],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[4:0] rw[10] gy[3:0] gx[3:0] gw[10] bz[0] gz[3:0] bx[3:0] bw[10] bz[1] by[3:0] ry[4:0] bz[2] rz[4:0] bz[3]",
        },
        Mode {
            number: 4,
            code: 0b00110,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 11,
            delta: [4, 5, 4],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[3:0] rw[10] gz[4] gy[3:0] gx[4:0] gw[10] gz[3:0] bx[3:0] bw[10] bz[1] by[3:0] ry[3:0] bz[0] bz[2] rz[3:0] gy[4] bz[3]",
        },
        Mode {
            number: 5,
            code: 0b01010,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 11,
            delta: [4, 4, 5],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[3:0] rw[10] by[4] gy[3:0] gx[3:0] gw[10] bz[0] gz[3:0] bx[4:0] bw[10] by[3:0] ry[3:0] bz[1] bz[2] rz[3:0] bz[4] bz[3]",
        },
        Mode {
            number: 6,
            code: 0b01110,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 9,
            delta: [5, 5, 5],
            layout: "m[4:0] rw[8:0] by[4] gw[8:0] gy[4] bw[8:0] bz[4] rx[4:0] gz[4] gy[3:0] gx[4:0] bz[0] gz[3:0] bx[4:0] bz[1] by[3:0] ry[4:0] bz[2] rz[4:0] bz[3]",
        },
        Mode {
            number: 7,
            code: 0b10010,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 8,
            delta: [6, 5, 5],
            layout: "m[4:0] rw[7:0] gz[4] by[4] gw[7:0] bz[2] gy[4] bw[7:0] bz[3] bz[4] rx[5:0] gy[3:0] gx[4:0] bz[0] gz[3:0] bx[4:0] bz[1] by[3:0] ry[5:0] rz[5:0]",
        },
        Mode {
            number: 8,
            code: 0b10110,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 8,
            delta: [5, 6, 5],
            layout: "m[4:0] rw[7:0] bz[0] by[4] gw[7:0] gy[5] gy[4] bw[7:0] gz[5] bz[4] rx[4:0] gz[4] gy[3:0] gx[5:0] gz[3:0] bx[4:0] bz[1] by[3:0] ry[4:0] bz[2] rz[4:0] bz[3]",
        },
        Mode {
            number: 9,
            code: 0b11010,
            code_bits: 5,
            subsets: 2,
            transformed: true,
            prec: 8,
            delta: [5, 5, 6],
            layout: "m[4:0] rw[7:0] bz[1] by[4] gw[7:0] by[5] gy[4] bw[7:0] bz[5] bz[4] rx[4:0] gz[4] gy[3:0] gx[4:0] bz[0] gz[3:0] bx[5:0] by[3:0] ry[4:0] bz[2] rz[4:0] bz[3]",
        },
        Mode {
            number: 10,
            code: 0b11110,
            code_bits: 5,
            subsets: 2,
            transformed: false,
            prec: 6,
            delta: [6, 6, 6],
            layout: "m[4:0] rw[5:0] gz[4] bz[0] bz[1] by[4] gw[5:0] gy[5] by[5] bz[2] gy[4] bw[5:0] gz[5] bz[3] bz[5] bz[4] rx[5:0] gy[3:0] gx[5:0] gz[3:0] bx[5:0] by[3:0] ry[5:0] rz[5:0]",
        },
        Mode {
            number: 11,
            code: 0b00011,
            code_bits: 5,
            subsets: 1,
            transformed: false,
            prec: 10,
            delta: [10, 10, 10],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[9:0] gx[9:0] bx[9:0]",
        },
        Mode {
            number: 12,
            code: 0b00111,
            code_bits: 5,
            subsets: 1,
            transformed: true,
            prec: 11,
            delta: [9, 9, 9],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[8:0] rw[10] gx[8:0] gw[10] bx[8:0] bw[10]",
        },
        Mode {
            number: 13,
            code: 0b01011,
            code_bits: 5,
            subsets: 1,
            transformed: true,
            prec: 12,
            delta: [8, 8, 8],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[7:0] rw[10:11] gx[7:0] gw[10:11] bx[7:0] bw[10:11]",
        },
        Mode {
            number: 14,
            code: 0b01111,
            code_bits: 5,
            subsets: 1,
            transformed: true,
            prec: 16,
            delta: [4, 4, 4],
            layout: "m[4:0] rw[9:0] gw[9:0] bw[9:0] rx[3:0] rw[10:15] gx[3:0] gw[10:15] bx[3:0] bw[10:15]",
        },
    ];

    /// Two-subset shapes (bit i = subset of texel i) and second-subset anchors: the first 32
    /// entries of the BPTC two-subset table, which BC6H shares.
    pub const PARTITION: [u16; 32] = [
        0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80, 0xc800, 0xffec, 0xfe80, 0xe800,
        0xffe8, 0xff00, 0xfff0, 0xf000, 0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce,
        0x088c, 0x3110, 0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c,
    ];
    /// Anchor texel of subset 1 per shape.
    pub const ANCHOR: [usize; 32] = [
        15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2,
        8, 8, 2, 2,
    ];
    const W3: [i32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
    const W4: [i32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

    /// The mode a block uses, `None` for the four reserved codes.
    pub fn mode_of(block: &[u8; 16]) -> Option<&'static Mode> {
        let low2 = u32::from(block[0] & 3);
        let low5 = u32::from(block[0] & 31);
        MODES.iter().find(|m| {
            if m.code_bits == 2 {
                m.code == low2
            } else {
                m.code == low5
            }
        })
    }

    /// Placement of every field bit: `(position, field, channel, endpoint, bit)` with field
    /// 0 = mode, 1 = endpoint, 2 = partition.
    pub fn placements(mode: &Mode) -> Vec<(u32, u8, usize, usize, u32)> {
        let mut out = Vec::new();
        let mut pos = 0u32;
        for token in mode.layout.split_whitespace() {
            let (name, range) = token.split_once('[').expect("layout token");
            let range = range.trim_end_matches(']');
            let (a, b) = match range.split_once(':') {
                Some((a, b)) => (a.parse::<i32>().expect("bit"), b.parse::<i32>().expect("bit")),
                None => {
                    let k = range.parse::<i32>().expect("bit");
                    (k, k)
                }
            };
            let step = if a >= b { 1 } else { -1 };
            let mut k = b;
            loop {
                let (field, channel, endpoint) = match name {
                    "m" => (0, 0, 0),
                    "d" => (2, 0, 0),
                    _ => {
                        let mut ch = name.chars();
                        let channel = match ch.next() {
                            Some('r') => 0,
                            Some('g') => 1,
                            _ => 2,
                        };
                        let endpoint = match ch.next() {
                            Some('w') => 0,
                            Some('x') => 1,
                            Some('y') => 2,
                            _ => 3,
                        };
                        (1, channel, endpoint)
                    }
                };
                out.push((pos, field, channel, endpoint, k as u32));
                pos += 1;
                if k == a {
                    break;
                }
                k += step;
            }
        }
        if mode.subsets == 2 {
            for k in 0..5 {
                out.push((pos, 2, 0, 0, k));
                pos += 1;
            }
        }
        out
    }

    fn sext(v: u32, bits: u32) -> i32 {
        let shift = 32 - bits;
        ((v << shift) as i32) >> shift
    }

    fn unquantize(comp: i32, prec: u32, signed: bool) -> i32 {
        if !signed {
            if prec >= 15 {
                comp
            } else if comp == 0 {
                0
            } else if comp == (1 << prec) - 1 {
                0xffff
            } else {
                ((comp << 16) + 0x8000) >> prec
            }
        } else if prec >= 16 {
            comp
        } else {
            let negative = comp < 0;
            let mag = comp.abs();
            let unq = if mag == 0 {
                0
            } else if mag >= (1 << (prec - 1)) - 1 {
                0x7fff
            } else {
                ((mag << 15) + 0x4000) >> (prec - 1)
            };
            if negative { -unq } else { unq }
        }
    }

    fn finish(v: i32, signed: bool) -> u16 {
        if !signed {
            ((v * 31) >> 6) as u16
        } else if v < 0 {
            0x8000 | ((((-v) * 31) >> 5) as u16)
        } else {
            ((v * 31) >> 5) as u16
        }
    }

    /// Decoded endpoints (unquantized, e0..e3) and the partition of a block.
    pub fn endpoints(block: &[u8; 16], signed: bool) -> Option<(&'static Mode, usize, [[i32; 3]; 4])> {
        let mode = mode_of(block)?;
        let bits = u128::from_le_bytes(*block);
        let mut raw = [[0u32; 3]; 4];
        let mut partition = 0usize;
        for (pos, field, channel, endpoint, k) in placements(mode) {
            let bit = ((bits >> pos) & 1) as u32;
            match field {
                1 => raw[endpoint][channel] |= bit << k,
                2 => partition |= (bit as usize) << k,
                _ => {}
            }
        }
        let prec = mode.prec;
        let mask = (1i64 << prec) - 1;
        let mut q = [[0i32; 3]; 4];
        for c in 0..3 {
            q[0][c] = if signed {
                sext(raw[0][c], prec)
            } else {
                raw[0][c] as i32
            };
            for k in 1..2 * mode.subsets {
                q[k][c] = if mode.transformed {
                    let d = sext(raw[k][c], mode.delta[c]);
                    let s = ((i64::from(raw[0][c]) + i64::from(d)) & mask) as u32;
                    if signed { sext(s, prec) } else { s as i32 }
                } else if signed {
                    sext(raw[k][c], prec)
                } else {
                    raw[k][c] as i32
                };
            }
        }
        let mut unq = [[0i32; 3]; 4];
        for k in 0..4 {
            for c in 0..3 {
                unq[k][c] = unquantize(q[k][c], prec, signed);
            }
        }
        Some((mode, partition, unq))
    }

    /// Decodes a block to half-float bit patterns. Reserved modes decode to zero.
    pub fn decode(block: &[u8; 16], signed: bool) -> [[u16; 3]; 16] {
        let Some((mode, partition, unq)) = endpoints(block, signed) else {
            return [[0; 3]; 16];
        };
        let bits = u128::from_le_bytes(*block);
        let (mut pos, index_bits) = if mode.subsets == 2 { (82u32, 3u32) } else { (65, 4) };
        let mut out = [[0u16; 3]; 16];
        for (i, texel) in out.iter_mut().enumerate() {
            let subset = if mode.subsets == 2 {
                usize::from((PARTITION[partition] >> i) & 1 == 1)
            } else {
                0
            };
            let anchor = i == 0 || (mode.subsets == 2 && ANCHOR[partition] == i);
            let n = index_bits - u32::from(anchor);
            let index = ((bits >> pos) & ((1u128 << n) - 1)) as usize;
            pos += n;
            let w = if index_bits == 3 { W3[index] } else { W4[index] };
            let (a, b) = (unq[2 * subset], unq[2 * subset + 1]);
            for c in 0..3 {
                let v = (a[c] * (64 - w) + b[c] * w + 32) >> 6;
                texel[c] = finish(v, signed);
            }
        }
        out
    }
}

/// Exact IEEE binary16 -> f32.
fn half_to_f32(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0f32 } else { 1.0 };
    let e = i32::from((h >> 10) & 0x1f);
    let m = f32::from(h & 0x3ff);
    let mag = match e {
        0 => m * 2f32.powi(-24),
        31 => f32::INFINITY,
        _ => (1.0 + m / 1024.0) * 2f32.powi(e - 15),
    };
    sign * mag
}

/// f32 -> binary16, round to nearest (ties away; enough for building ISPC input), clamped finite.
fn f32_to_half(v: f32) -> u16 {
    let v = if v.is_nan() {
        0.0
    } else {
        v.clamp(-65504.0, 65504.0)
    };
    let sign = if v.is_sign_negative() && v != 0.0 {
        0x8000u16
    } else {
        0
    };
    let a = v.abs();
    if a < 2f32.powi(-24) * 0.5 {
        return sign;
    }
    if a < 2f32.powi(-14) {
        return sign | ((a / 2f32.powi(-24)).round() as u16).min(0x3ff);
    }
    let e = a.log2().floor() as i32;
    let mut e = e.clamp(-14, 15);
    let mut m = ((a / 2f32.powi(e) - 1.0) * 1024.0).round() as i32;
    if m >= 1024 {
        m -= 1024;
        e += 1;
    }
    if m < 0 {
        m += 1024;
        e -= 1;
    }
    if e > 15 {
        return sign | 0x7bff;
    }
    sign | (((e + 15) as u16) << 10) | m as u16
}

const SIZE: u32 = 64;

/// Deterministic xorshift32.
struct Rng(u32);
impl Rng {
    fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }
    fn unit(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 / (1u32 << 24) as f32
    }
}

/// Named deterministic HDR test images, `SIZE`x`SIZE` RGB floats. Signed images add negatives.
fn images(signed: bool) -> Vec<(&'static str, Vec<f32>)> {
    let n = SIZE as usize;
    let mut out = Vec::new();
    let mut sky = Vec::with_capacity(n * n * 3);
    for y in 0..n {
        for x in 0..n {
            let t = y as f32 / n as f32;
            let s = x as f32 / n as f32;
            let l = 2f32.powf(6.0 * (1.0 - t) - 2.0) * (0.8 + 0.4 * s);
            sky.extend_from_slice(&[l * 0.6, l * 0.8, l * 1.2]);
        }
    }
    out.push(("sky_gradient", sky.clone()));
    let mut sun = sky;
    let mut rng = Rng(0x51ed_270b);
    for _ in 0..24 {
        let at = (rng.next_u32() as usize % (n * n)) * 3;
        let l = 500.0 + 9000.0 * rng.unit();
        sun[at..at + 3].copy_from_slice(&[l, l * 0.9, l * 0.7]);
    }
    out.push(("sun_spikes", sun));
    let mut noise = Vec::with_capacity(n * n * 3);
    let mut rng = Rng(0x2545_f491);
    for y in 0..n {
        for x in 0..n {
            let base = 2f32.powf(((x / 4 + y / 4) % 7) as f32 - 3.0);
            for _ in 0..3 {
                noise.push(base * (0.5 + rng.unit()));
            }
        }
    }
    out.push(("log_noise", noise));
    let mut hue = Vec::with_capacity(n * n * 3);
    for y in 0..n {
        for x in 0..n {
            let h = x as f32 / n as f32 * 6.0;
            let l = 2f32.powf(y as f32 / n as f32 * 10.0 - 4.0);
            let r = (1.0 - (h - 1.0).abs().min(1.0)).max(0.0) + (h - 5.0).max(0.0);
            let g = (1.0 - (h - 2.0).abs().min(1.0)).max(0.0);
            let b = (1.0 - (h - 4.0).abs().min(1.0)).max(0.0);
            hue.extend_from_slice(&[r * l, g * l, b * l]);
        }
    }
    out.push(("hue_sweep", hue));
    let mut dark = Vec::with_capacity(n * n * 3);
    for y in 0..n {
        for x in 0..n {
            dark.extend_from_slice(&[x as f32 * 0.0004, y as f32 * 0.0003, (x + y) as f32 * 0.0002]);
        }
    }
    out.push(("dark_ramp", dark));
    if signed {
        let mut normal = Vec::with_capacity(n * n * 3);
        for y in 0..n {
            for x in 0..n {
                let a = x as f32 / n as f32 * std::f32::consts::TAU;
                let b = y as f32 / n as f32 * 3.0;
                normal.extend_from_slice(&[a.cos() * b, a.sin() * b, (b - 1.5) * 2.0]);
            }
        }
        out.push(("signed_field", normal));
    }
    out
}

fn blocks_of(rgb: &[f32]) -> Vec<[[f32; 3]; 16]> {
    let n = SIZE as usize;
    let mut out = Vec::new();
    for by in 0..n / 4 {
        for bx in 0..n / 4 {
            let mut b = [[0f32; 3]; 16];
            for (i, t) in b.iter_mut().enumerate() {
                let at = ((by * 4 + i / 4) * n + bx * 4 + i % 4) * 3;
                *t = [rgb[at], rgb[at + 1], rgb[at + 2]];
            }
            out.push(b);
        }
    }
    out
}

/// Error summary of decoded texels against the source: RMS of `log2(1+|x|)` differences and
/// the worst relative error over texels with |x| >= 1/16 (the same denominator floor
/// everywhere, so numbers compare across encoders).
#[derive(Debug, Clone, Copy, Default)]
struct ErrStats {
    log_rmse: f64,
    worst_rel: f64,
    mean_rel: f64,
}

impl std::fmt::Display for ErrStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "log-RMSE {:.5}, mean rel {:.5}, worst rel {:.4}",
            self.log_rmse, self.mean_rel, self.worst_rel
        )
    }
}

fn error(src: &[[f32; 3]], dec: &[[f32; 3]]) -> ErrStats {
    let mut sq = 0.0f64;
    let mut worst = 0.0f64;
    let mut sum = 0.0f64;
    let mut n = 0usize;
    let mut m = 0usize;
    for (s, d) in src.iter().zip(dec) {
        for c in 0..3 {
            let ls = f64::from((1.0 + s[c].abs()).log2()) * f64::from(s[c].signum());
            let ld = f64::from((1.0 + d[c].abs()).log2()) * f64::from(d[c].signum());
            sq += (ls - ld) * (ls - ld);
            n += 1;
            let denom = f64::from(s[c].abs()).max(1.0 / 16.0);
            let rel = f64::from((s[c] - d[c]).abs()) / denom;
            worst = worst.max(rel);
            sum += rel;
            m += 1;
        }
    }
    ErrStats {
        log_rmse: (sq / n.max(1) as f64).sqrt(),
        worst_rel: worst,
        mean_rel: sum / m.max(1) as f64,
    }
}

fn spec_decode_f32(block: &[u8; 16], signed: bool) -> [[f32; 3]; 16] {
    let h = spec::decode(block, signed);
    let mut out = [[0f32; 3]; 16];
    for i in 0..16 {
        for c in 0..3 {
            out[i][c] = half_to_f32(h[i][c]);
        }
    }
    out
}

fn ispc_encode(rgb: &[f32]) -> Vec<[u8; 16]> {
    let n = SIZE as usize;
    let mut surface = Vec::with_capacity(n * n * 8);
    for t in rgb.chunks(3) {
        for v in [t[0], t[1], t[2], 1.0] {
            surface.extend_from_slice(&f32_to_half(v.max(0.0)).to_le_bytes());
        }
    }
    let s = intel_tex_2::RgbaSurface {
        data: &surface,
        width: SIZE,
        height: SIZE,
        stride: SIZE * 8,
    };
    let bytes = intel_tex_2::bc6h::compress_blocks(&intel_tex_2::bc6h::slow_settings(), &s);
    bytes.as_chunks::<16>().0.to_vec()
}

fn quantized_source(rgb: &[f32], signed: bool) -> Vec<[f32; 3]> {
    rgb.chunks(3)
        .map(|t| {
            let f = |v: f32| half_to_f32(f32_to_half(if signed { v } else { v.max(0.0) }));
            [f(t[0]), f(t[1]), f(t[2])]
        })
        .collect()
}

/// The source texels in the order `flatten` gives decoded blocks (block by block, each block
/// row-major), quantized to half floats.
fn block_order_source(rgb: &[f32], signed: bool) -> Vec<[f32; 3]> {
    let blocks = blocks_of(rgb);
    let flat: Vec<f32> = blocks
        .iter()
        .flat_map(|b| b.iter().flat_map(|t| t.iter().copied()))
        .collect();
    quantized_source(&flat, signed)
}

fn flatten(blocks: &[[[f32; 3]; 16]]) -> Vec<[f32; 3]> {
    blocks.iter().flat_map(|b| b.iter().copied()).collect()
}

/// Step 1: `spec` decodes a production encoder's blocks back to the source. A mode whose layout
/// `spec` got wrong decodes to garbage wherever the encoder used it: the delivered module's
/// blocks read this way scored a log-RMSE of 0.5 to 10. ISPC itself scores at most 0.136 (the
/// `log_noise` image is independent noise per channel, which no two-endpoint line fits) and at
/// most 0.27 on its worst block, so the floors are 0.2 per image and 0.5 per block.
#[test]
fn spec_decoder_reads_ispc_blocks_back_to_the_source() {
    let mut used = [0usize; 15];
    for (name, rgb) in images(false) {
        let src_blocks = blocks_of(&rgb);
        let blocks = ispc_encode(&rgb);
        assert_eq!(blocks.len(), src_blocks.len());
        let mut per_mode_worst = [0f64; 15];
        for (b, s) in blocks.iter().zip(&src_blocks) {
            let mode = spec::mode_of(b).map_or(0, |m| m.number as usize);
            used[mode] += 1;
            let e = error(&quantized_source(&s.concat(), false), &spec_decode_f32(b, false));
            per_mode_worst[mode] = per_mode_worst[mode].max(e.log_rmse);
        }
        let dec: Vec<[[f32; 3]; 16]> = blocks.iter().map(|b| spec_decode_f32(b, false)).collect();
        let e = error(&block_order_source(&rgb, false), &flatten(&dec));
        eprintln!("ispc {name}: {e}; worst block log-RMSE per mode {per_mode_worst:?}");
        assert!(
            e.log_rmse < 0.2,
            "{name}: spec decode of ISPC blocks log-RMSE {}",
            e.log_rmse
        );
        for (mode, worst) in per_mode_worst.iter().enumerate() {
            assert!(
                *worst < 0.5,
                "{name}: mode {mode} block decodes with log-RMSE {worst}"
            );
        }
    }
    eprintln!("ispc mode histogram (index = mode number, 0 = reserved): {used:?}");
    assert_eq!(used[0], 0, "ISPC never writes a reserved mode");
}

fn random_block(rng: &mut Rng, mode: &spec::Mode) -> [u8; 16] {
    let mut bytes = [0u8; 16];
    for chunk in bytes.as_chunks_mut::<4>().0 {
        *chunk = rng.next_u32().to_le_bytes();
    }
    let mask = (1u8 << mode.code_bits) - 1;
    bytes[0] = (bytes[0] & !mask) | mode.code as u8;
    bytes
}

/// Uploads `blocks` (a `SIZE`x`SIZE` image) and returns the texels the driver decodes.
fn gpu_decode(device: &RenderDevice, format: wgpu::TextureFormat, blocks: &[u8]) -> Vec<[f32; 4]> {
    const SHADER: &str = r"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var<storage, read_write> dst: array<vec4<f32>>;
@compute @workgroup_size(8, 8)
fn main(@builtin(global_invocation_id) id: vec3<u32>) {
    let size = textureDimensions(src);
    if (id.x >= size.x || id.y >= size.y) { return; }
    dst[id.y * size.x + id.x] = textureLoad(src, vec2<i32>(id.xy), 0);
}
";
    let d = &device.device;
    let texture = d.create_texture_with_data(
        &device.queue,
        &wgpu::TextureDescriptor {
            label: Some("bc6h verify src"),
            size: wgpu::Extent3d {
                width: SIZE,
                height: SIZE,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        },
        wgpu::util::TextureDataOrder::LayerMajor,
        blocks,
    );
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    let bytes = u64::from(SIZE * SIZE * 16);
    let storage = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bc6h verify dst"),
        size: bytes,
        usage: wgpu::BufferUsages::STORAGE | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = d.create_buffer(&wgpu::BufferDescriptor {
        label: Some("bc6h verify readback"),
        size: bytes,
        usage: wgpu::BufferUsages::MAP_READ | wgpu::BufferUsages::COPY_DST,
        mapped_at_creation: false,
    });
    let module = d.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("bc6h verify"),
        source: wgpu::ShaderSource::Wgsl(SHADER.into()),
    });
    let pipeline = d.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("bc6h verify"),
        layout: None,
        module: &module,
        entry_point: Some("main"),
        compilation_options: wgpu::PipelineCompilationOptions::default(),
        cache: None,
    });
    let bind = d.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("bc6h verify"),
        layout: &pipeline.get_bind_group_layout(0),
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: storage.as_entire_binding(),
            },
        ],
    });
    let mut encoder = d.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("bc6h verify"),
    });
    {
        let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor::default());
        pass.set_pipeline(&pipeline);
        pass.set_bind_group(0, &bind, &[]);
        pass.dispatch_workgroups(SIZE / 8, SIZE / 8, 1);
    }
    encoder.copy_buffer_to_buffer(&storage, 0, &staging, 0, bytes);
    device.queue.submit([encoder.finish()]);
    let (tx, rx) = std::sync::mpsc::channel();
    staging.slice(..).map_async(wgpu::MapMode::Read, move |r| {
        let _ = tx.send(r);
    });
    d.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    rx.recv().expect("map callback").expect("map");
    let data = staging
        .slice(..)
        .get_mapped_range()
        .expect("mapped range")
        .to_vec();
    staging.unmap();
    data.as_chunks::<16>()
        .0
        .iter()
        .map(|t| {
            let f = |k: usize| f32::from_le_bytes([t[k], t[k + 1], t[k + 2], t[k + 3]]);
            [f(0), f(4), f(8), f(12)]
        })
        .collect()
}

/// Rows of blocks in raster order -> the texel order `gpu_decode` returns.
fn texel_of(block: usize, i: usize) -> usize {
    let bw = (SIZE / 4) as usize;
    let (bx, by) = (block % bw, block / bw);
    (by * 4 + i / 4) * SIZE as usize + bx * 4 + i % 4
}

fn headless_bc() -> Option<RenderDevice> {
    let device = match RenderDevice::headless() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip: no headless device ({e})");
            return None;
        }
    };
    if !device
        .device
        .features()
        .contains(wgpu::Features::TEXTURE_COMPRESSION_BC)
    {
        eprintln!(
            "skip: {} has no TEXTURE_COMPRESSION_BC",
            device.adapter.get_info().name
        );
        return None;
    }
    eprintln!("adapter: {:?}", device.adapter.get_info());
    Some(device)
}

fn format_of(signed: bool) -> wgpu::TextureFormat {
    if signed {
        wgpu::TextureFormat::Bc6hRgbFloat
    } else {
        wgpu::TextureFormat::Bc6hRgbUfloat
    }
}

/// Counts texels where `blocks` decode differently on the GPU and in `cpu` (a decoder giving
/// half-float values as f32). Returns (mismatching texels, first mismatch description).
fn gpu_mismatches(
    device: &RenderDevice,
    signed: bool,
    blocks: &[[u8; 16]],
    cpu: impl Fn(&[u8; 16]) -> [[f32; 3]; 16],
) -> (usize, Option<String>) {
    let flat: Vec<u8> = blocks.iter().flatten().copied().collect();
    let gpu = gpu_decode(device, format_of(signed), &flat);
    let mut bad = 0;
    let mut first = None;
    for (bi, b) in blocks.iter().enumerate() {
        let c = cpu(b);
        for i in 0..16 {
            let g = gpu[texel_of(bi, i)];
            if (0..3).any(|k| c[i][k] != g[k]) {
                bad += 1;
                if first.is_none() {
                    first = Some(format!(
                        "block {bi} (mode {:?}) texel {i}: cpu {:?} gpu {:?}; bytes {:02x?}",
                        spec::mode_of(b).map(|m| m.number),
                        c[i],
                        g,
                        b
                    ));
                }
            }
        }
    }
    (bad, first)
}

/// Step 2: random blocks of every mode, both formats, decode on the GPU exactly as `spec`
/// decodes them (bit-exact half values).
#[test]
fn spec_decoder_matches_the_gpu_bit_exactly_on_random_blocks_of_every_mode() {
    let Some(device) = headless_bc() else { return };
    let per_image = ((SIZE / 4) * (SIZE / 4)) as usize;
    for signed in [false, true] {
        let mut rng = Rng(if signed { 0x0bad_5eed } else { 0x1234_5678 });
        // 256 blocks per image: cycle the 14 modes.
        let blocks: Vec<[u8; 16]> = (0..per_image)
            .map(|i| random_block(&mut rng, &spec::MODES[i % spec::MODES.len()]))
            .collect();
        let (bad, first) = gpu_mismatches(&device, signed, &blocks, |b| spec_decode_f32(b, signed));
        assert_eq!(
            bad, 0,
            "signed={signed}: {bad} texels differ from the GPU; first: {first:?}"
        );
    }
}

fn delivered_options() -> Vec<(&'static str, Bc6hOptions)> {
    vec![
        ("default", Bc6hOptions::default()),
        (
            "tone_mapped",
            Bc6hOptions {
                metric: Bc6hMetric::ToneMapped,
                modes: Bc6hModes::all(),
            },
        ),
        (
            "log",
            Bc6hOptions {
                metric: Bc6hMetric::LogDomain,
                modes: Bc6hModes::all(),
            },
        ),
    ]
}

fn module_format(signed: bool) -> Bc6hFormat {
    if signed {
        Bc6hFormat::Signed
    } else {
        Bc6hFormat::Unsigned
    }
}

/// Step 3a (CPU): the module's blocks, read by the independent decoder, equal what the module's
/// own decoder says and stay close to the source.
#[test]
fn module_blocks_decode_identically_under_the_independent_decoder() {
    let mut total_bad = 0usize;
    let mut total = 0usize;
    let mut report = Vec::new();
    for signed in [false, true] {
        let format = module_format(signed);
        for (name, rgb) in images(signed) {
            let src = blocks_of(&rgb);
            let blocks: Vec<[u8; 16]> = src
                .iter()
                .map(|b| encode_block(b, format, &Bc6hOptions::default()))
                .collect();
            let mut modes = [0usize; 15];
            let mut bad = 0usize;
            let mut spec_dec = Vec::new();
            for b in &blocks {
                modes[spec::mode_of(b).map_or(0, |m| m.number as usize)] += 1;
                let own = decode_block(b, format);
                let ind = spec_decode_f32(b, signed);
                bad += (0..16).filter(|&i| own[i] != ind[i]).count();
                spec_dec.push(ind);
            }
            let own: Vec<[[f32; 3]; 16]> = blocks.iter().map(|b| decode_block(b, format)).collect();
            let q = block_order_source(&rgb, signed);
            let e_own = error(&q, &flatten(&own));
            let e_spec = error(&q, &flatten(&spec_dec));
            report.push(format!(
                "signed={signed} {name}: {bad}/{} texels differ; spec-decoded {e_spec}; module-decoded {e_own}; modes as the format reads them {modes:?}",
                blocks.len() * 16
            ));
            total_bad += bad;
            total += blocks.len() * 16;
        }
    }
    for line in &report {
        eprintln!("{line}");
    }
    assert_eq!(
        total_bad, 0,
        "{total_bad}/{total} texels decode differently under the BC6H format"
    );
}

/// Step 3b (GPU): the module's blocks decode on a real driver exactly as the module's decoder
/// says, for every metric.
#[test]
fn module_blocks_decode_on_the_gpu_like_the_module_decoder() {
    let Some(device) = headless_bc() else { return };
    let mut failures = Vec::new();
    for signed in [false, true] {
        let format = module_format(signed);
        for (name, rgb) in images(signed) {
            for (opt_name, options) in delivered_options() {
                let blocks = encode_image(SIZE, SIZE, &rgb, format, &options).expect("encode");
                let blocks: Vec<[u8; 16]> = blocks.as_chunks::<16>().0.to_vec();
                let (bad, first) = gpu_mismatches(&device, signed, &blocks, |b| decode_block(b, format));
                if bad > 0 {
                    failures.push(format!(
                        "signed={signed} {name} {opt_name}: {bad}/4096 texels; first {first:?}"
                    ));
                }
            }
        }
    }
    for f in &failures {
        eprintln!("{f}");
    }
    assert!(
        failures.is_empty(),
        "{} image/option cases decode differently on the GPU",
        failures.len()
    );
}

/// Every mode by itself: with only mode `n` enabled the module must write mode `n`, and those
/// blocks must decode identically under the independent decoder and on the GPU. (The default
/// search picks some modes rarely, so the identity tests above do not reach every writer.)
#[test]
fn every_mode_forced_alone_decodes_identically_on_cpu_and_gpu() {
    let modes = [
        (1u8, Bc6hModes::MODE_1),
        (2, Bc6hModes::MODE_2),
        (3, Bc6hModes::MODE_3),
        (4, Bc6hModes::MODE_4),
        (5, Bc6hModes::MODE_5),
        (6, Bc6hModes::MODE_6),
        (7, Bc6hModes::MODE_7),
        (8, Bc6hModes::MODE_8),
        (9, Bc6hModes::MODE_9),
        (10, Bc6hModes::MODE_10),
        (11, Bc6hModes::MODE_11),
        (12, Bc6hModes::MODE_12),
        (13, Bc6hModes::MODE_13),
        (14, Bc6hModes::MODE_14),
    ];
    let device = headless_bc();
    let mut gpu_failures = Vec::new();
    for signed in [false, true] {
        let format = module_format(signed);
        // Two images per format: smooth (deltas fit) and spiky (deltas clamp).
        let all = images(signed);
        for (name, rgb) in all
            .iter()
            .filter(|(n, _)| *n == "sky_gradient" || *n == "sun_spikes" || *n == "signed_field")
        {
            for (number, flag) in modes {
                let options = Bc6hOptions {
                    metric: Bc6hMetric::LogDomain,
                    modes: flag,
                };
                let blocks = encode_image(SIZE, SIZE, rgb, format, &options).expect("encode");
                let blocks: Vec<[u8; 16]> = blocks.as_chunks::<16>().0.to_vec();
                for (bi, b) in blocks.iter().enumerate() {
                    assert_eq!(
                        spec::mode_of(b).map(|m| m.number),
                        Some(number),
                        "{name} block {bi}"
                    );
                    assert_eq!(
                        decode_block(b, format),
                        spec_decode_f32(b, signed),
                        "{name} mode {number} block {bi}"
                    );
                }
                if let Some(device) = &device {
                    let (bad, first) = gpu_mismatches(device, signed, &blocks, |b| decode_block(b, format));
                    if bad > 0 {
                        gpu_failures.push(format!(
                            "signed={signed} {name} mode {number}: {bad} texels; {first:?}"
                        ));
                    }
                }
            }
        }
    }
    assert!(gpu_failures.is_empty(), "{gpu_failures:#?}");
}

/// Quality and speed against the ISPC encoder (unsigned; ISPC writes no signed BC6H), both
/// decoded by `spec`. A report, not a gate: run with `--ignored --nocapture`, ideally in release.
#[test]
#[ignore = "report: cargo test --release --test bc6h_encode_verify -- --ignored --nocapture"]
fn quality_and_speed_report_vs_ispc() {
    for (name, rgb) in images(false) {
        let q = block_order_source(&rgb, false);
        let t = std::time::Instant::now();
        let ispc = ispc_encode(&rgb);
        let ispc_s = t.elapsed().as_secs_f64();
        let ispc_dec: Vec<[[f32; 3]; 16]> = ispc.iter().map(|b| spec_decode_f32(b, false)).collect();
        eprintln!(
            "{name:>14} ispc-slow   {} {:.3} Mtexel/s",
            error(&q, &flatten(&ispc_dec)),
            4096.0 / ispc_s / 1e6
        );
        for (opt_name, options) in delivered_options() {
            let t = std::time::Instant::now();
            let blocks = encode_image(SIZE, SIZE, &rgb, Bc6hFormat::Unsigned, &options).expect("encode");
            let s = t.elapsed().as_secs_f64();
            let dec: Vec<[[f32; 3]; 16]> = blocks
                .as_chunks::<16>()
                .0
                .iter()
                .map(|b| spec_decode_f32(b, false))
                .collect();
            eprintln!(
                "{name:>14} module-{opt_name:<11} {} {:.3} Mtexel/s",
                error(&q, &flatten(&dec)),
                4096.0 / s / 1e6
            );
        }
    }
}

/// Per-block numbers for the module's own gradient tests: worst and mean relative error per
/// metric and mode set, and the mode chosen. A report: `--ignored --nocapture`.
#[test]
#[ignore = "report: cargo test --test bc6h_encode_verify gradient_report -- --ignored --nocapture"]
fn gradient_report() {
    let mut forward = [[0f32; 3]; 16];
    for (i, t) in forward.iter_mut().enumerate() {
        let f = i as f32 / 15.0;
        *t = [1.0 + 99.0 * f, 1.0 + 49.0 * f, 1.0 + 9.0 * f];
    }
    let mut reversed = forward;
    reversed.reverse();
    let mut negative = [[0f32; 3]; 16];
    for (i, t) in negative.iter_mut().enumerate() {
        let f = i as f32 / 15.0;
        *t = [-100.0 + 99.0 * f, -10.0 + 9.0 * f, -50.0 + 45.0 * f];
    }
    let one = Bc6hModes::MODE_11
        .union(Bc6hModes::MODE_12)
        .union(Bc6hModes::MODE_13)
        .union(Bc6hModes::MODE_14);
    let cases = [
        ("forward", forward, false),
        ("reversed", reversed, false),
        ("negative", negative, true),
    ];
    for (name, texels, signed) in cases {
        if !signed {
            // ISPC on the same block (a 4x4 image), as the achievable reference.
            let mut surface = Vec::new();
            for t in &texels {
                for v in [t[0], t[1], t[2], 1.0] {
                    surface.extend_from_slice(&f32_to_half(v).to_le_bytes());
                }
            }
            let s = intel_tex_2::RgbaSurface {
                data: &surface,
                width: 4,
                height: 4,
                stride: 32,
            };
            let bytes = intel_tex_2::bc6h::compress_blocks(&intel_tex_2::bc6h::slow_settings(), &s);
            let mut block = [0u8; 16];
            block.copy_from_slice(&bytes[..16]);
            let dec = spec_decode_f32(&block, false);
            let worst = (0..16)
                .flat_map(|i| (0..3).map(move |c| (i, c)))
                .map(|(i, c)| {
                    f64::from((texels[i][c] - dec[i][c]).abs())
                        / f64::from(texels[i][c].abs()).max(1.0 / 1024.0)
                })
                .fold(0.0, f64::max);
            eprintln!(
                "{name:>8}       ispc slow: mode {:?} worst {worst:.4}",
                spec::mode_of(&block).map(|m| m.number)
            );
        }
        for (set_name, modes) in [
            ("all", Bc6hModes::all()),
            ("one-subset", one),
            ("mode11", Bc6hModes::MODE_11),
        ] {
            for metric in [Bc6hMetric::Linear, Bc6hMetric::ToneMapped, Bc6hMetric::LogDomain] {
                let options = Bc6hOptions { metric, modes };
                let block = encode_block(&texels, module_format(signed), &options);
                let dec = spec_decode_f32(&block, signed);
                let rel: Vec<f64> = (0..16)
                    .map(|i| {
                        (0..3)
                            .map(|c| {
                                f64::from((texels[i][c] - dec[i][c]).abs())
                                    / f64::from(texels[i][c].abs()).max(1.0 / 1024.0)
                            })
                            .fold(0.0, f64::max)
                    })
                    .collect();
                let worst = rel.iter().copied().fold(0.0, f64::max);
                let mean = rel.iter().sum::<f64>() / 16.0;
                eprintln!(
                    "{name:>8} {set_name:>10} {metric:?}: mode {:?} worst {worst:.4} mean {mean:.4}",
                    spec::mode_of(&block).map(|m| m.number)
                );
            }
        }
    }
}
