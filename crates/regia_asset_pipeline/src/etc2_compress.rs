// SPDX-License-Identifier: MPL-2.0
//! ETC2 and EAC block compression for Android devices without ASTC (ADR-1379).
//!
//! Original code written from the Khronos Data Format Specification 1.3, chapter "ETC2
//! Compressed Texture Image Formats" (the bit layouts, the intensity modifier tables and the EAC
//! modifier tables are the specification's). What is encoded:
//!
//! - ETC2 RGB8 ([`compress_etc2_rgb8`], 8 bytes a 4×4 block): the ETC1-compatible "individual"
//!   and "differential" modes, both sub-block orientations, and the ETC2 planar (a least-squares
//!   plane per channel), T and H modes (two colour groups by 2-means, every distance; ADR-2063),
//!   whichever has the least error. The differential mode's base + delta never leaves 0..=31; the
//!   ETC2 modes set their free bits so the decoder selects them.
//! - ETC2 RGBA8 ([`compress_etc2_rgba8`], 16 bytes): an EAC alpha block, then an ETC2 RGB8 block.
//! - EAC R11 ([`compress_eac_r11`], 8 bytes) for one-channel data and EAC RG11
//!   ([`compress_eac_rg11`], 16 bytes) for tangent-space normals (X, Y; the forward shader rebuilds
//!   Z, as for BC5).
//!
//! [`decode_etc2_rgba8`] decodes with the runtime's decoder (`regia_asset::etc2_decode`, every
//! mode); the cook's tests and the size report measure the encoder with it.
//!
//! Blocks run left to right, top to bottom. A level that is not a whole number of blocks (the tail
//! mips) is padded by repeating its last row and column.

/// The ETC1/ETC2 intensity modifier tables: `[small, large]` per table codeword. A pixel index
/// `msb lsb` selects `+small` (00), `+large` (01), `-small` (10) or `-large` (11).
const INTENSITY: [[i32; 2]; 8] = [
    [2, 8],
    [5, 17],
    [9, 29],
    [13, 42],
    [18, 60],
    [24, 80],
    [33, 106],
    [47, 183],
];

/// The EAC modifier tables, indexed by the 4-bit table index then the 3-bit pixel index.
const EAC_MODIFIERS: [[i32; 8]; 16] = [
    [-3, -6, -9, -15, 2, 5, 8, 14],
    [-3, -7, -10, -13, 2, 6, 9, 12],
    [-2, -5, -8, -13, 1, 4, 7, 12],
    [-2, -4, -6, -13, 1, 3, 5, 12],
    [-3, -6, -8, -12, 2, 5, 7, 11],
    [-3, -7, -9, -11, 2, 6, 8, 10],
    [-4, -7, -8, -11, 3, 6, 7, 10],
    [-3, -5, -8, -11, 2, 4, 7, 10],
    [-2, -6, -8, -10, 1, 5, 7, 9],
    [-2, -5, -8, -10, 1, 4, 7, 9],
    [-2, -4, -8, -10, 1, 3, 7, 9],
    [-2, -5, -7, -10, 1, 4, 6, 9],
    [-3, -4, -7, -10, 2, 3, 6, 9],
    [-1, -2, -3, -10, 0, 1, 2, 9],
    [-4, -6, -8, -9, 3, 5, 7, 8],
    [-3, -5, -7, -9, 2, 4, 6, 8],
];

/// The modifier a 2-bit ETC pixel index selects from table `table`.
fn modifier(table: usize, index: usize) -> i32 {
    let [small, large] = INTENSITY[table];
    match index {
        0 => small,
        1 => large,
        2 => -small,
        _ => -large,
    }
}

/// Pixel `(x, y)` of a block is pixel number `x * 4 + y` in every ETC and EAC index field.
fn pixel_number(x: usize, y: usize) -> usize {
    x * 4 + y
}

/// Whether pixel `(x, y)` is in the second sub-block (`flip`: 4×2 halves stacked; else 2×4 halves
/// side by side).
fn second_half(flip: bool, x: usize, y: usize) -> bool {
    if flip { y >= 2 } else { x >= 2 }
}

/// Expands a 4-bit colour component to 8 bits.
fn expand4(c: u8) -> u8 {
    (c << 4) | c
}

/// Expands a 5-bit colour component to 8 bits.
fn expand5(c: u8) -> u8 {
    (c << 3) | (c >> 2)
}

/// The 16 RGBA texels of the block at block coordinates `(bx, by)`, edge texels repeated past the
/// image, indexed `y * 4 + x`.
fn block_texels(width: u32, height: u32, rgba: &[u8], bx: u32, by: u32) -> [[u8; 4]; 16] {
    let mut out = [[0u8; 4]; 16];
    for y in 0..4u32 {
        for x in 0..4u32 {
            let px = (bx * 4 + x).min(width - 1);
            let py = (by * 4 + y).min(height - 1);
            let i = ((py * width + px) * 4) as usize;
            out[(y * 4 + x) as usize] = [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]];
        }
    }
    out
}

/// One ETC1-mode candidate: the two expanded base colours, the per-half table and the pixel
/// indices, with its squared error.
struct EtcCandidate {
    bits_colors: u64,
    flip: bool,
    tables: [usize; 2],
    indices: [usize; 16],
    error: u64,
}

/// The best table and pixel indices for the pixels of one half against base colour `base`.
/// Returns (error, table, indices by pixel number; other pixels untouched).
fn fit_half(
    texels: &[[u8; 4]; 16],
    flip: bool,
    half: bool,
    base: [i32; 3],
    indices: &mut [usize; 16],
) -> (u64, usize) {
    let mut best = (u64::MAX, 0usize, [0usize; 16]);
    for table in 0..8 {
        let mut error = 0u64;
        let mut chosen = [0usize; 16];
        for y in 0..4 {
            for x in 0..4 {
                if second_half(flip, x, y) != half {
                    continue;
                }
                let t = texels[y * 4 + x];
                let mut pixel_best = (u64::MAX, 0usize);
                for index in 0..4 {
                    let m = modifier(table, index);
                    let mut e = 0u64;
                    for (b, tc) in base.iter().zip(t.iter()) {
                        let d = (b + m).clamp(0, 255) - i32::from(*tc);
                        e += (d * d) as u64;
                    }
                    if e < pixel_best.0 {
                        pixel_best = (e, index);
                    }
                }
                error += pixel_best.0;
                chosen[pixel_number(x, y)] = pixel_best.1;
            }
            if error >= best.0 {
                break;
            }
        }
        if error < best.0 {
            best = (error, table, chosen);
        }
    }
    for y in 0..4 {
        for x in 0..4 {
            if second_half(flip, x, y) == half {
                let p = pixel_number(x, y);
                indices[p] = best.2[p];
            }
        }
    }
    (best.0, best.1)
}

/// The mean RGB of one half, in 0..=255.
fn half_mean(texels: &[[u8; 4]; 16], flip: bool, half: bool) -> [f32; 3] {
    let mut sum = [0u32; 3];
    for y in 0..4 {
        for x in 0..4 {
            if second_half(flip, x, y) == half {
                for (acc, v) in sum.iter_mut().zip(texels[y * 4 + x].iter()) {
                    *acc += u32::from(*v);
                }
            }
        }
    }
    sum.map(|s| s as f32 / 8.0)
}

/// Quantizes a mean to `levels - 1` steps (15 for 4 bits, 31 for 5).
fn quantize(mean: [f32; 3], max: f32) -> [i32; 3] {
    mean.map(|m| (m * max / 255.0).round().clamp(0.0, max) as i32)
}

/// Encodes one RGB block in the ETC1-compatible modes, trying both orientations, the individual
/// mode (with a one-step shift of each base along the grey axis) and the differential mode.
fn encode_etc_rgb_block(texels: &[[u8; 4]; 16]) -> u64 {
    let mut best: Option<EtcCandidate> = None;
    let mut consider = |cand: EtcCandidate| {
        if best.as_ref().is_none_or(|b| cand.error < b.error) {
            best = Some(cand);
        }
    };
    for flip in [false, true] {
        let means = [half_mean(texels, flip, false), half_mean(texels, flip, true)];

        // Individual mode: two 4-bit colours, each half fitted on its own.
        let q4 = [quantize(means[0], 15.0), quantize(means[1], 15.0)];
        let mut halves = [(u64::MAX, 0usize, [0i32; 3]); 2];
        let mut indices = [0usize; 16];
        for (h, q) in q4.iter().enumerate() {
            for shift in [0, -1, 1] {
                let c = q.map(|v| (v + shift).clamp(0, 15));
                let base = c.map(|v| i32::from(expand4(v as u8)));
                let mut trial = indices;
                let (error, table) = fit_half(texels, flip, h == 1, base, &mut trial);
                if error < halves[h].0 {
                    halves[h] = (error, table, c);
                    indices = trial;
                }
            }
        }
        let (c1, c2) = (halves[0].2, halves[1].2);
        let colors = ((c1[0] as u64) << 60)
            | ((c2[0] as u64) << 56)
            | ((c1[1] as u64) << 52)
            | ((c2[1] as u64) << 48)
            | ((c1[2] as u64) << 44)
            | ((c2[2] as u64) << 40);
        consider(EtcCandidate {
            bits_colors: colors,
            flip,
            tables: [halves[0].1, halves[1].1],
            indices,
            error: halves[0].0 + halves[1].0,
        });

        // Differential mode: a 5-bit colour and a 3-bit signed delta per channel. The second
        // colour is pulled into delta range of the first, so base + delta stays in 0..=31.
        let q5a = quantize(means[0], 31.0);
        let mut q5b = quantize(means[1], 31.0);
        for (b, a) in q5b.iter_mut().zip(q5a.iter()) {
            *b = (*b).clamp(a - 4, a + 3).clamp(0, 31);
        }
        let mut indices = [0usize; 16];
        let base_a = q5a.map(|v| i32::from(expand5(v as u8)));
        let base_b = q5b.map(|v| i32::from(expand5(v as u8)));
        let (ea, ta) = fit_half(texels, flip, false, base_a, &mut indices);
        let (eb, tb) = fit_half(texels, flip, true, base_b, &mut indices);
        let mut colors = 0u64;
        for (c, (a, b)) in q5a.iter().zip(q5b.iter()).enumerate() {
            let delta = ((b - a) as u64) & 0b111;
            let shift = 59 - c * 8;
            colors |= (*a as u64) << shift;
            colors |= delta << (shift - 3);
        }
        colors |= 1 << 33;
        consider(EtcCandidate {
            bits_colors: colors,
            flip,
            tables: [ta, tb],
            indices,
            error: ea + eb,
        });
    }
    let Some(best) = best else { return 0 };
    // The ETC2-only modes: whichever of planar, T and H beats the ETC1 modes by the most.
    let etc2_modes = [
        encode_planar(texels),
        encode_t_or_h(texels, false),
        encode_t_or_h(texels, true),
    ];
    if let Some((error, bits)) = etc2_modes.into_iter().flatten().min_by_key(|(e, _)| *e)
        && error < best.error
    {
        return bits;
    }
    let mut bits = best.bits_colors;
    bits |= (best.tables[0] as u64) << 37;
    bits |= (best.tables[1] as u64) << 34;
    if best.flip {
        bits |= 1 << 32;
    }
    for (p, &index) in best.indices.iter().enumerate() {
        bits |= (((index as u64) >> 1) & 1) << (16 + p);
        bits |= ((index as u64) & 1) << p;
    }
    bits
}

/// Expands a 6-bit colour component to 8 bits.
fn expand6(c: i32) -> i32 {
    (c << 2) | (c >> 4)
}

/// Expands a 7-bit colour component to 8 bits.
fn expand7(c: i32) -> i32 {
    (c << 1) | (c >> 6)
}

/// The squared error of one channel of a planar block with expanded origin, horizontal and
/// vertical colours `o`, `h`, `v` against `texels[..][channel]` (the decoder's rule:
/// `clamp((x (H - O) + y (V - O) + 4 O + 2) >> 2)`).
fn planar_error(texels: &[[u8; 4]; 16], channel: usize, o: i32, h: i32, v: i32) -> u64 {
    let mut error = 0u64;
    for y in 0..4i32 {
        for x in 0..4i32 {
            let p = ((x * (h - o) + y * (v - o) + 4 * o + 2) >> 2).clamp(0, 255);
            let d = p - i32::from(texels[(y * 4 + x) as usize][channel]);
            error += (d * d) as u64;
        }
    }
    error
}

/// Encodes one RGB block in the planar mode: per channel the least-squares plane through the 16
/// texels (origin O at texel (0, 0), H one block step right, V one step down), each corner
/// quantized to 6 / 7 / 6 bits and refined by ±1 on the exact decoded error. Returns the error
/// and the block, or `None` when no setting of the free bits selects the planar mode (never, by
/// construction; kept as a guard).
fn encode_planar(texels: &[[u8; 4]; 16]) -> Option<(u64, u64)> {
    // Normal equations of pred = O (1 - x/4 - y/4) + H x/4 + V y/4 over the block.
    let mut m = [[0f64; 3]; 3];
    let mut rhs = [[0f64; 3]; 3];
    for y in 0..4 {
        for x in 0..4 {
            let (b, c) = (f64::from(x) / 4.0, f64::from(y) / 4.0);
            let basis = [1.0 - b - c, b, c];
            let texel = texels[(y * 4 + x) as usize];
            for (row, &bi) in m.iter_mut().zip(&basis) {
                for (cell, &bj) in row.iter_mut().zip(&basis) {
                    *cell += bi * bj;
                }
            }
            for (r, &t) in rhs.iter_mut().zip(&texel) {
                for (cell, &bi) in r.iter_mut().zip(&basis) {
                    *cell += bi * f64::from(t);
                }
            }
        }
    }
    let det3 = |a: [[f64; 3]; 3]| {
        a[0][0] * (a[1][1] * a[2][2] - a[1][2] * a[2][1]) - a[0][1] * (a[1][0] * a[2][2] - a[1][2] * a[2][0])
            + a[0][2] * (a[1][0] * a[2][1] - a[1][1] * a[2][0])
    };
    let det = det3(m);
    let mut fields = [[0i32; 3]; 3]; // [channel][O, H, V], quantized
    let mut error = 0u64;
    for (ch, (field, rhs_ch)) in fields.iter_mut().zip(&rhs).enumerate() {
        // Cramer's rule for O, H, V of this channel.
        let mut solved = [0f64; 3];
        for (k, value) in solved.iter_mut().enumerate() {
            let mut a = m;
            for (r, &v) in a.iter_mut().zip(rhs_ch) {
                r[k] = v;
            }
            *value = det3(a) / det;
        }
        let (max, expand): (i32, fn(i32) -> i32) = if ch == 1 { (127, expand7) } else { (63, expand6) };
        let q = solved.map(|v| ((v * f64::from(max) / 255.0).round() as i32).clamp(0, max));
        let mut best = (u64::MAX, q);
        for d in 0..27 {
            let trial = [q[0] + d % 3 - 1, q[1] + d / 3 % 3 - 1, q[2] + d / 9 - 1];
            if trial.iter().any(|&t| !(0..=max).contains(&t)) {
                continue;
            }
            let e = planar_error(texels, ch, expand(trial[0]), expand(trial[1]), expand(trial[2]));
            if e < best.0 {
                best = (e, trial);
            }
        }
        error += best.0;
        *field = best.1;
    }
    let [r, g, b] = fields.map(|f| f.map(|v| v as u64));
    let mut bits = 1u64 << 33;
    bits |= r[0] << 57;
    bits |= (g[0] >> 6) << 56;
    bits |= (g[0] & 0x3f) << 49;
    bits |= (b[0] >> 5) << 48;
    bits |= ((b[0] >> 3) & 0b11) << 43;
    bits |= (b[0] & 0b111) << 39;
    bits |= (r[1] >> 1) << 34;
    bits |= (r[1] & 1) << 32;
    bits |= g[1] << 25;
    bits |= b[1] << 19;
    bits |= r[2] << 13;
    bits |= g[2] << 6;
    bits |= b[2];
    // The free bits (63, 55, 47..45, 42): red and green must stay in the differential range and
    // blue must leave it, which is what selects the planar mode.
    let in_range = |bits: u64, base: u32, delta: u32| {
        let v = ((bits >> base) & 0x1f) as i32;
        let raw = ((bits >> delta) & 0b111) as i32;
        (0..=31).contains(&(v + if raw >= 4 { raw - 8 } else { raw }))
    };
    (0..64u64)
        .map(|free| {
            bits | ((free & 1) << 63)
                | (((free >> 1) & 1) << 55)
                | (((free >> 2) & 0b111) << 45)
                | (((free >> 5) & 1) << 42)
        })
        .find(|&b| in_range(b, 59, 56) && in_range(b, 51, 48) && !in_range(b, 43, 40))
        .map(|b| (error, b))
}

/// The T and H modes' distance table.
const DISTANCE: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

/// The 16 texels split into two colour groups (2-means from the two texels farthest apart):
/// `true` for the second group. Every texel in one group when the block is flat.
fn two_groups(texels: &[[u8; 4]; 16]) -> [bool; 16] {
    let dist = |a: [f32; 3], b: [f32; 3]| a.iter().zip(&b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>();
    let rgb = texels.map(|t| [f32::from(t[0]), f32::from(t[1]), f32::from(t[2])]);
    let (mut c0, mut c1, mut far) = (rgb[0], rgb[0], -1.0f32);
    for (i, a) in rgb.iter().enumerate() {
        for b in &rgb[i + 1..] {
            let d = dist(*a, *b);
            if d > far {
                (c0, c1, far) = (*a, *b, d);
            }
        }
    }
    let mut groups = [false; 16];
    for _ in 0..4 {
        for (g, t) in groups.iter_mut().zip(&rgb) {
            *g = dist(*t, c1) < dist(*t, c0);
        }
        let mean = |second: bool| {
            let (mut sum, mut n) = ([0f32; 3], 0f32);
            for (g, t) in groups.iter().zip(&rgb) {
                if *g == second {
                    for (s, v) in sum.iter_mut().zip(t) {
                        *s += v;
                    }
                    n += 1.0;
                }
            }
            (n > 0.0).then(|| sum.map(|s| s / n))
        };
        c0 = mean(false).unwrap_or(c0);
        c1 = mean(true).unwrap_or(c1);
    }
    groups
}

/// The 4-bit quantized mean of the texels in group `second`.
fn group_color(texels: &[[u8; 4]; 16], groups: &[bool; 16], second: bool) -> [i32; 3] {
    let (mut sum, mut n) = ([0u32; 3], 0u32);
    for (g, t) in groups.iter().zip(texels) {
        if *g == second {
            for (s, v) in sum.iter_mut().zip(t) {
                *s += u32::from(*v);
            }
            n += 1;
        }
    }
    let mean = sum.map(|s| if n == 0 { 0.0 } else { s as f32 / n as f32 });
    quantize(mean, 15.0)
}

/// Picks the best of four paint colours per texel: the squared error and the 2-bit indices (by
/// pixel number).
fn paint_fit(texels: &[[u8; 4]; 16], paint: &[[i32; 3]; 4]) -> (u64, [usize; 16]) {
    let mut error = 0u64;
    let mut indices = [0usize; 16];
    for y in 0..4 {
        for x in 0..4 {
            let t = texels[y * 4 + x];
            let mut best = (u64::MAX, 0usize);
            for (i, p) in paint.iter().enumerate() {
                let e: u64 = p
                    .iter()
                    .zip(t.iter())
                    .map(|(c, v)| {
                        let d = c - i32::from(*v);
                        (d * d) as u64
                    })
                    .sum();
                if e < best.0 {
                    best = (e, i);
                }
            }
            error += best.0;
            indices[pixel_number(x, y)] = best.1;
        }
    }
    (error, indices)
}

/// One T / H candidate: its error, the two 4-bit base colours, the distance index and the pixel
/// indices (by pixel number).
struct PaintChoice {
    error: u64,
    first: [i32; 3],
    second: [i32; 3],
    index: usize,
    indices: [usize; 16],
}

/// Encodes one RGB block in the T mode (`h == false`: one base colour alone, the other ± a
/// distance) or the H mode (`h`: both bases ± a distance), from a two-group split of the texels,
/// trying every distance (and for T both groups as the lone colour). Returns the error and the
/// block, with the free bits set so the right channel overflows (red for T, green and not red for
/// H); `None` when no encoding exists (never, by construction; kept as a guard).
fn encode_t_or_h(texels: &[[u8; 4]; 16], h: bool) -> Option<(u64, u64)> {
    let groups = two_groups(texels);
    let colors = [
        group_color(texels, &groups, false),
        group_color(texels, &groups, true),
    ];
    let expand = |c: [i32; 3]| c.map(|v| i32::from(expand4(v as u8)));
    let plus = |c: [i32; 3], d: i32| c.map(|v| (v + d).clamp(0, 255));
    let mut best: Option<PaintChoice> = None;
    for (first, second) in [(colors[0], colors[1]), (colors[1], colors[0])] {
        let (e1, e2) = (expand(first), expand(second));
        for (index, &d) in DISTANCE.iter().enumerate() {
            if h {
                // The distance index's low bit is the ordering of the two bases: (R1 G1 B1) >=
                // (R2 G2 B2) as 12-bit numbers. Equal bases only take an odd index.
                let key = |c: [i32; 3]| (c[0] << 8) | (c[1] << 4) | c[2];
                let ordered = key(first) >= key(second);
                if usize::from(ordered) != index & 1 {
                    continue;
                }
            }
            let paint = if h {
                [plus(e1, d), plus(e1, -d), plus(e2, d), plus(e2, -d)]
            } else {
                [e1, plus(e2, d), e2, plus(e2, -d)]
            };
            let (error, indices) = paint_fit(texels, &paint);
            if best.as_ref().is_none_or(|b| error < b.error) {
                best = Some(PaintChoice {
                    error,
                    first,
                    second,
                    index,
                    indices,
                });
            }
        }
    }
    let PaintChoice {
        error,
        first: c1,
        second: c2,
        index,
        indices,
    } = best?;
    let [c1, c2] = [c1, c2].map(|c| c.map(|v| v as u64));
    let index = index as u64;
    let mut bits = 1u64 << 33;
    if h {
        bits |= c1[0] << 59;
        bits |= (c1[1] >> 1) << 56;
        bits |= (c1[1] & 1) << 52;
        bits |= (c1[2] >> 3) << 51;
        bits |= (c1[2] & 0b111) << 47;
        bits |= c2[0] << 43;
        bits |= c2[1] << 39;
        bits |= c2[2] << 35;
        bits |= (index >> 2) << 34;
        bits |= ((index >> 1) & 1) << 32;
    } else {
        bits |= (c1[0] >> 2) << 59;
        bits |= (c1[0] & 0b11) << 56;
        bits |= c1[1] << 52;
        bits |= c1[2] << 48;
        bits |= c2[0] << 44;
        bits |= c2[1] << 40;
        bits |= c2[2] << 36;
        bits |= (index >> 1) << 34;
        bits |= (index & 1) << 32;
    }
    for (p, &i) in indices.iter().enumerate() {
        bits |= (((i as u64) >> 1) & 1) << (16 + p);
        bits |= ((i as u64) & 1) << p;
    }
    let in_range = |bits: u64, base: u32, delta: u32| {
        let v = ((bits >> base) & 0x1f) as i32;
        let raw = ((bits >> delta) & 0b111) as i32;
        (0..=31).contains(&(v + if raw >= 4 { raw - 8 } else { raw }))
    };
    // Free bits: T 63..61 and 58 (red must overflow); H 63, 55..53 and 50 (red in range, green
    // out of it).
    (0..32u64)
        .map(|free| {
            if h {
                bits | ((free & 1) << 63) | (((free >> 1) & 0b111) << 53) | (((free >> 4) & 1) << 50)
            } else {
                bits | ((free & 0b111) << 61) | (((free >> 3) & 1) << 58)
            }
        })
        .find(|&b| {
            if h {
                in_range(b, 59, 56) && !in_range(b, 51, 48)
            } else {
                !in_range(b, 59, 56)
            }
        })
        .map(|b| (error, b))
}

/// One ETC2 RGB8 block decoded by the runtime's decoder (every mode), as RGB texels indexed
/// `y * 4 + x`.
#[cfg(test)]
fn decode_etc_rgb_block(bits: u64) -> [[u8; 3]; 16] {
    regia_asset::etc2_decode::decode_block(regia_asset::etc2_decode::Etc2Format::Rgb8, &bits.to_be_bytes())
        .map(|t| [t[0], t[1], t[2]])
}

/// The value an EAC block's pixel index decodes to, in the block's precision: 8-bit alpha, or
/// 11-bit for R11 (`eleven`).
fn eac_value(base: i32, multiplier: i32, table: usize, index: usize, eleven: bool) -> i32 {
    let m = EAC_MODIFIERS[table][index];
    if eleven {
        let v = if multiplier == 0 {
            base * 8 + 4 + m
        } else {
            base * 8 + 4 + m * multiplier * 8
        };
        v.clamp(0, 2047)
    } else {
        (base + m * multiplier).clamp(0, 255)
    }
}

/// Encodes 16 values (8-bit alpha, or 11-bit targets with `eleven`) indexed by pixel number as
/// one EAC block. Searches every table, the multipliers around the one the value range implies
/// (and 0 for an 11-bit block), and the bases around the one that centres the table on the range.
fn encode_eac_block(values: &[i32; 16], eleven: bool) -> u64 {
    let (lo, hi) = values
        .iter()
        .fold((i32::MAX, i32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    // Work in the base's units: 8 per step in 11-bit mode.
    let unit = if eleven { 8.0 } else { 1.0 };
    let mut best = (u64::MAX, 0u64);
    for (table, mods) in EAC_MODIFIERS.iter().enumerate() {
        let span = (mods[7] - mods[3]) as f32;
        let estimate = ((hi - lo) as f32 / unit / span).round() as i32;
        let mut multipliers: Vec<i32> = (estimate - 1..=estimate + 1).map(|m| m.clamp(1, 15)).collect();
        if eleven {
            multipliers.push(0);
        }
        multipliers.dedup();
        for multiplier in multipliers {
            let scale = if eleven && multiplier == 0 {
                1.0
            } else {
                multiplier as f32 * unit
            };
            let centre = (lo + hi) as f32 / 2.0 - (mods[3] + mods[7]) as f32 * scale / 2.0;
            let centre = if eleven { (centre - 4.0) / 8.0 } else { centre };
            for delta in -1..=1 {
                let base = (centre.round() as i32 + delta).clamp(0, 255);
                let mut error = 0u64;
                let mut index_bits = 0u64;
                for (p, &target) in values.iter().enumerate() {
                    let mut pixel = (u64::MAX, 0usize);
                    for index in 0..8 {
                        let d = eac_value(base, multiplier, table, index, eleven) - target;
                        let e = (d * d) as u64;
                        if e < pixel.0 {
                            pixel = (e, index);
                        }
                    }
                    error += pixel.0;
                    index_bits |= (pixel.1 as u64) << (45 - 3 * p);
                }
                if error < best.0 {
                    let bits = ((base as u64) << 56)
                        | ((multiplier as u64) << 52)
                        | ((table as u64) << 48)
                        | index_bits;
                    best = (error, bits);
                }
            }
        }
    }
    best.1
}

/// Decodes one EAC block into 16 values indexed by pixel number (8-bit, or 11-bit with `eleven`).
#[cfg(test)]
fn decode_eac_block(bits: u64, eleven: bool) -> [i32; 16] {
    let base = (bits >> 56) as i32;
    let multiplier = ((bits >> 52) & 0xf) as i32;
    let table = ((bits >> 48) & 0xf) as usize;
    let mut out = [0i32; 16];
    for (p, v) in out.iter_mut().enumerate() {
        let index = ((bits >> (45 - 3 * p)) & 0b111) as usize;
        // An 8-bit alpha block with multiplier 0 is not written by this encoder; decode it as the base.
        *v = if !eleven && multiplier == 0 {
            base
        } else {
            eac_value(base, multiplier, table, index, eleven)
        };
    }
    out
}

/// An 8-bit value to the 11-bit EAC scale.
fn to_eleven(v: u8) -> i32 {
    (i32::from(v) * 2047 + 127) / 255
}

/// Runs `encode` on every block of a `width`×`height` RGBA8 image and concatenates the blocks
/// (`block_bytes` each, big-endian 64-bit words as the format stores them). Block rows are split
/// across threads.
fn encode_blocks(
    width: u32,
    height: u32,
    rgba: &[u8],
    block_bytes: usize,
    encode: impl Fn(&[[u8; 4]; 16]) -> Vec<u64> + Sync,
) -> Result<Vec<u8>, Etc2CompressError> {
    if width == 0 || height == 0 || rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        return Err(Etc2CompressError::BadBuffer {
            len: rgba.len(),
            width,
            height,
        });
    }
    let blocks_x = width.div_ceil(4);
    let blocks_y = height.div_ceil(4);
    let row_bytes = blocks_x as usize * block_bytes;
    let mut out = vec![0u8; row_bytes * blocks_y as usize];
    let threads = etc2_threads(width, height, blocks_y);
    let rows_per_thread = (blocks_y as usize).div_ceil(threads);
    let encode = &encode;
    std::thread::scope(|scope| {
        for (chunk_index, chunk) in out.chunks_mut(rows_per_thread * row_bytes).enumerate() {
            scope.spawn(move || {
                for (row_in_chunk, row) in chunk.chunks_mut(row_bytes).enumerate() {
                    let by = (chunk_index * rows_per_thread + row_in_chunk) as u32;
                    for bx in 0..blocks_x {
                        let texels = block_texels(width, height, rgba, bx, by);
                        let words = encode(&texels);
                        let at = bx as usize * block_bytes;
                        for (w, word) in words.iter().enumerate() {
                            row[at + w * 8..at + w * 8 + 8].copy_from_slice(&word.to_be_bytes());
                        }
                    }
                }
            });
        }
    });
    Ok(out)
}

/// Threads for one ETC2 level: one below 512×512 texels (thread start-up costs more than it saves,
/// and a caller cooking several maps in parallel must not multiply its threads per level), else
/// the machine's parallelism, at most 16 and one per block row, like the BCn bands (audit
/// concurrency-1103 pipeline L2). Rows are independent, so the bytes do not depend on the count.
fn etc2_threads(width: u32, height: u32, blocks_y: u32) -> usize {
    if u64::from(width) * u64::from(height) < 512 * 512 {
        return 1;
    }
    std::thread::available_parallelism()
        .map_or(1, usize::from)
        .min(16)
        .min(blocks_y as usize)
        .max(1)
}

/// ETC2 / EAC compression failure.
#[derive(Debug, thiserror::Error)]
pub enum Etc2CompressError {
    /// The buffer is not `width × height × 4` bytes, or a dimension is zero.
    #[error("rgba buffer length {len} does not match {width}x{height}")]
    BadBuffer {
        /// Pixel buffer length.
        len: usize,
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },
    /// The compressed data is not a whole number of blocks for the size.
    #[error("{len} bytes is not {blocks} blocks of {block_bytes} bytes")]
    BadBlocks {
        /// Data length.
        len: usize,
        /// Blocks the size needs.
        blocks: usize,
        /// Bytes per block of the format.
        block_bytes: usize,
    },
}

/// Compresses RGBA8 (alpha ignored) to ETC2 RGB8, 8 bytes a 4×4 block. Any size is accepted.
///
/// # Errors
/// The buffer does not match the size.
pub fn compress_etc2_rgb8(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Etc2CompressError> {
    let _span = tracing::info_span!("etc2_rgb8", width, height).entered();
    encode_blocks(width, height, rgba, 8, |t| vec![encode_etc_rgb_block(t)])
}

/// Compresses RGBA8 to ETC2 RGBA8 (EAC alpha block, then ETC2 RGB block), 16 bytes a block.
///
/// # Errors
/// The buffer does not match the size.
pub fn compress_etc2_rgba8(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Etc2CompressError> {
    let _span = tracing::info_span!("etc2_rgba8", width, height).entered();
    encode_blocks(width, height, rgba, 16, |t| {
        let alpha = pixel_values(t, |px| i32::from(px[3]));
        vec![encode_eac_block(&alpha, false), encode_etc_rgb_block(t)]
    })
}

/// Compresses the R channel of RGBA8 to EAC R11 (unsigned), 8 bytes a block.
///
/// # Errors
/// The buffer does not match the size.
pub fn compress_eac_r11(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Etc2CompressError> {
    let _span = tracing::info_span!("eac_r11", width, height).entered();
    encode_blocks(width, height, rgba, 8, |t| {
        vec![encode_eac_block(&pixel_values(t, |px| to_eleven(px[0])), true)]
    })
}

/// Compresses the R and G channels of RGBA8 to EAC RG11 (unsigned), 16 bytes a block: a
/// tangent-space normal's X and Y.
///
/// # Errors
/// The buffer does not match the size.
pub fn compress_eac_rg11(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, Etc2CompressError> {
    let _span = tracing::info_span!("eac_rg11", width, height).entered();
    encode_blocks(width, height, rgba, 16, |t| {
        vec![
            encode_eac_block(&pixel_values(t, |px| to_eleven(px[0])), true),
            encode_eac_block(&pixel_values(t, |px| to_eleven(px[1])), true),
        ]
    })
}

/// One value per texel of a block, indexed by pixel number (`x * 4 + y`).
fn pixel_values(texels: &[[u8; 4]; 16], f: impl Fn(&[u8; 4]) -> i32) -> [i32; 16] {
    let mut out = [0i32; 16];
    for y in 0..4 {
        for x in 0..4 {
            out[pixel_number(x, y)] = f(&texels[y * 4 + x]);
        }
    }
    out
}

/// The ETC2 / EAC formats this module writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Etc2Kind {
    /// ETC2 RGB8 (8 bytes a block).
    Rgb8,
    /// ETC2 RGBA8: EAC alpha + ETC2 RGB (16 bytes).
    Rgba8,
    /// EAC R11 unsigned (8 bytes).
    R11,
    /// EAC RG11 unsigned (16 bytes).
    Rg11,
}

impl Etc2Kind {
    /// Bytes per 4×4 block.
    #[must_use]
    pub fn block_bytes(self) -> usize {
        match self {
            Self::Rgb8 | Self::R11 => 8,
            Self::Rgba8 | Self::Rg11 => 16,
        }
    }
}

/// Decodes `data` of `kind` for a `width`×`height` level back to RGBA8 with the runtime's decoder
/// (`regia_asset::etc2_decode`, every mode). Channels a format does not store come back as the
/// hardware returns them: alpha 255 for RGB8, `R 0 0 255` for R11 and `R G 0 255` for RG11.
///
/// # Errors
/// `data` is not the number of blocks the size needs.
pub fn decode_etc2_rgba8(
    kind: Etc2Kind,
    width: u32,
    height: u32,
    data: &[u8],
) -> Result<Vec<u8>, Etc2CompressError> {
    use regia_asset::etc2_decode::{Etc2Format, decode_etc2};
    let blocks = width.div_ceil(4) as usize * height.div_ceil(4) as usize;
    let block_bytes = kind.block_bytes();
    let bad = || Etc2CompressError::BadBlocks {
        len: data.len(),
        blocks,
        block_bytes,
    };
    if data.len() != blocks * block_bytes {
        return Err(bad());
    }
    let format = match kind {
        Etc2Kind::Rgb8 => Etc2Format::Rgb8,
        Etc2Kind::Rgba8 => Etc2Format::Rgba8,
        Etc2Kind::R11 => Etc2Format::R11,
        Etc2Kind::Rg11 => Etc2Format::Rg11,
    };
    decode_etc2(format, width, height, data).map_err(|_| bad())
}

#[cfg(test)]
mod tests {
    /// Audit concurrency-1103 pipeline L2: small levels encode on the calling thread and large ones
    /// on at most 16.
    #[test]
    fn etc2_thread_count_is_gated_and_capped() {
        assert_eq!(super::etc2_threads(256, 256, 64), 1);
        assert_eq!(super::etc2_threads(4096, 4, 1), 1);
        let big = super::etc2_threads(4096, 4096, 1024);
        assert!((1..=16).contains(&big), "{big}");
    }

    use super::*;

    /// PSNR in dB over the first `channels` channels of two RGBA8 images.
    fn psnr(a: &[u8], b: &[u8], channels: usize) -> f64 {
        let mut sum = 0f64;
        let mut n = 0f64;
        for (pa, pb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
            for (va, vb) in pa.iter().zip(pb.iter()).take(channels) {
                let d = f64::from(*va) - f64::from(*vb);
                sum += d * d;
                n += 1.0;
            }
        }
        if sum == 0.0 {
            return f64::INFINITY;
        }
        10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
    }

    /// A smooth gradient with a soft disc: the kind of content ETC handles well.
    fn gradient(w: u32, h: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let dx = x as f32 - w as f32 / 2.0;
                let dy = y as f32 - h as f32 / 2.0;
                let disc = if dx * dx + dy * dy < (w as f32 / 4.0).powi(2) {
                    60
                } else {
                    0
                };
                out.extend_from_slice(&[
                    (x * 255 / (w - 1)) as u8,
                    (y * 255 / (h - 1)) as u8,
                    (128 + disc) as u8,
                    ((x + y) * 255 / (w + h - 2)) as u8,
                ]);
            }
        }
        out
    }

    #[test]
    fn a_flat_block_round_trips_exactly_in_every_format() -> Result<(), Etc2CompressError> {
        // Values that the 4-bit expansion hits exactly (17 × n), with a modifier of 0 not
        // available in ETC: the encoder must still land within the smallest modifier.
        let rgba: Vec<u8> = [0x88u8, 0x44, 0xcc, 0x80].repeat(16);
        let rgb = decode_etc2_rgba8(Etc2Kind::Rgb8, 4, 4, &compress_etc2_rgb8(4, 4, &rgba)?)?;
        for px in rgb.as_chunks::<4>().0 {
            for (got, want) in px.iter().zip(rgba.iter()).take(3) {
                assert!(got.abs_diff(*want) <= 2, "{px:?}");
            }
            assert_eq!(px[3], 255);
        }
        let with_alpha = decode_etc2_rgba8(Etc2Kind::Rgba8, 4, 4, &compress_etc2_rgba8(4, 4, &rgba)?)?;
        assert!(with_alpha.as_chunks::<4>().0.iter().all(|px| px[3] == 0x80));
        let r = decode_etc2_rgba8(Etc2Kind::R11, 4, 4, &compress_eac_r11(4, 4, &rgba)?)?;
        assert!(r.as_chunks::<4>().0.iter().all(|px| px[0] == 0x88 && px[1] == 0));
        let rg = decode_etc2_rgba8(Etc2Kind::Rg11, 4, 4, &compress_eac_rg11(4, 4, &rgba)?)?;
        assert!(
            rg.as_chunks::<4>()
                .0
                .iter()
                .all(|px| px[0] == 0x88 && px[1] == 0x44)
        );
        Ok(())
    }

    #[test]
    fn block_sizes_follow_the_formats_and_partial_blocks_pad() -> Result<(), Etc2CompressError> {
        let rgba = gradient(10, 6);
        assert_eq!(compress_etc2_rgb8(10, 6, &rgba)?.len(), 3 * 2 * 8);
        assert_eq!(compress_etc2_rgba8(10, 6, &rgba)?.len(), 3 * 2 * 16);
        assert_eq!(compress_eac_r11(10, 6, &rgba)?.len(), 3 * 2 * 8);
        assert_eq!(compress_eac_rg11(10, 6, &rgba)?.len(), 3 * 2 * 16);
        // The 1×1 and 2×2 tail mips are one block each.
        assert_eq!(compress_etc2_rgb8(1, 1, &[9, 9, 9, 255])?.len(), 8);
        assert_eq!(compress_etc2_rgb8(2, 2, &[9; 16])?.len(), 8);
        assert!(compress_etc2_rgb8(4, 4, &[0; 12]).is_err());
        assert!(compress_etc2_rgb8(0, 4, &[]).is_err());
        assert!(decode_etc2_rgba8(Etc2Kind::Rgb8, 8, 8, &[0; 8]).is_err());
        Ok(())
    }

    #[test]
    fn gradients_compress_with_useful_quality() -> Result<(), Etc2CompressError> {
        let (w, h) = (64, 64);
        let rgba = gradient(w, h);
        let rgb = decode_etc2_rgba8(Etc2Kind::Rgb8, w, h, &compress_etc2_rgb8(w, h, &rgba)?)?;
        let rgb_psnr = psnr(&rgba, &rgb, 3);
        assert!(rgb_psnr > 30.0, "ETC2 RGB8 {rgb_psnr:.1} dB");
        let rgba_dec = decode_etc2_rgba8(Etc2Kind::Rgba8, w, h, &compress_etc2_rgba8(w, h, &rgba)?)?;
        let alpha: Vec<u8> = rgba
            .iter()
            .enumerate()
            .map(|(i, &v)| if i % 4 == 3 { v } else { 0 })
            .collect();
        let alpha_dec: Vec<u8> = rgba_dec
            .iter()
            .enumerate()
            .map(|(i, &v)| if i % 4 == 3 { v } else { 0 })
            .collect();
        let alpha_psnr = psnr(&alpha, &alpha_dec, 4);
        assert!(alpha_psnr > 40.0, "EAC alpha {alpha_psnr:.1} dB");
        let r = decode_etc2_rgba8(Etc2Kind::R11, w, h, &compress_eac_r11(w, h, &rgba)?)?;
        let r_psnr = psnr(&rgba, &r, 1);
        assert!(r_psnr > 40.0, "EAC R11 {r_psnr:.1} dB");
        Ok(())
    }

    /// The mode a block was written in (by the decoder's selection rule).
    fn mode_of(bits: u64) -> &'static str {
        if (bits >> 33) & 1 == 0 {
            return "individual";
        }
        let overflow = |shift: u32| {
            let base = ((bits >> shift) & 0x1f) as i32;
            let raw = ((bits >> (shift - 3)) & 0b111) as i32;
            !(0..=31).contains(&(base + if raw >= 4 { raw - 8 } else { raw }))
        };
        match (overflow(59), overflow(51), overflow(43)) {
            (true, _, _) => "t",
            (false, true, _) => "h",
            (false, false, true) => "planar",
            _ => "differential",
        }
    }

    /// Each mode is chosen where it fits: halves far apart take the individual mode, a gentle
    /// ramp planar blocks, and two-colour blocks with sharp edges (text, UI, hard shadows) the T or
    /// H mode; every block decodes close to its source.
    #[test]
    fn blocks_take_the_mode_that_fits_them() -> Result<(), Etc2CompressError> {
        let mut halves = Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                let v = if x % 4 < 2 { 250 } else { 3 } as u8;
                halves.extend_from_slice(&[v, (100 + x * 3 + y) as u8, 255 - v, 255]);
            }
        }
        let ramp: Vec<u8> = (0..64u32)
            .flat_map(|i| {
                [
                    (i % 8 * 9 + 20) as u8,
                    (i / 8 * 7 + 40) as u8,
                    (200 - i % 8 * 5 - i / 8 * 6) as u8,
                    255,
                ]
            })
            .collect();
        // Two saturated colours in a diagonal pattern: no sub-block split fits it.
        let two_colors: Vec<u8> = (0..64u32)
            .flat_map(|i| {
                if (i % 8 + i / 8) % 3 == 0 {
                    [230, 40, 30, 255]
                } else {
                    [20, 60, 210, 255]
                }
            })
            .collect();
        let mut seen = std::collections::BTreeSet::new();
        for (name, image, floor) in [
            ("halves", &halves, 30.0),
            ("ramp", &ramp, 40.0),
            ("two colors", &two_colors, 30.0),
        ] {
            let data = compress_etc2_rgb8(8, 8, image)?;
            let modes: Vec<&str> = data
                .as_chunks::<8>()
                .0
                .iter()
                .map(|b| mode_of(u64::from_be_bytes(*b)))
                .collect();
            seen.extend(modes.iter().copied());
            let decoded = decode_etc2_rgba8(Etc2Kind::Rgb8, 8, 8, &data)?;
            let db = psnr(image, &decoded, 3);
            assert!(db > floor, "{name}: {db:.1} dB, modes {modes:?}");
        }
        assert!(seen.contains("planar"), "{seen:?}");
        assert!(seen.contains("t") || seen.contains("h"), "{seen:?}");
        Ok(())
    }

    /// A block of exactly two 4-bit colours encodes in T or H mode and decodes within the
    /// smallest distance of its source.
    #[test]
    fn a_two_color_block_encodes_in_t_or_h_mode() {
        let mut texels = [[0u8; 4]; 16];
        for (i, t) in texels.iter_mut().enumerate() {
            *t = if i % 3 == 0 {
                [0xee, 0x22, 0x11, 255]
            } else {
                [0x11, 0x33, 0xdd, 255]
            };
        }
        let bits = encode_etc_rgb_block(&texels);
        assert!(
            matches!(mode_of(bits), "t" | "h"),
            "{}: {bits:016x}",
            mode_of(bits)
        );
        let back = decode_etc_rgb_block(bits);
        for (t, d) in texels.iter().zip(back.iter()) {
            assert!(
                t.iter().zip(d).all(|(a, b)| a.abs_diff(*b) <= 3),
                "{t:?} vs {d:?}"
            );
        }
    }

    /// A block that is an exact plane (within the 6/7/6-bit grid) encodes planar and decodes
    /// back within rounding; the ETC1 modes cannot follow a ramp that steep that closely.
    #[test]
    fn a_plane_encodes_as_a_planar_block() {
        let mut texels = [[0u8; 4]; 16];
        for y in 0..4 {
            for x in 0..4 {
                texels[y * 4 + x] = [
                    (20 + 50 * x) as u8,
                    (30 + 40 * y) as u8,
                    (200 - 20 * x - 25 * y) as u8,
                    255,
                ];
            }
        }
        let bits = encode_etc_rgb_block(&texels);
        let b = ((bits >> 43) & 0x1f) as i32 + {
            let raw = ((bits >> 40) & 0b111) as i32;
            if raw >= 4 { raw - 8 } else { raw }
        };
        assert!(
            (bits >> 33) & 1 == 1 && !(0..=31).contains(&b),
            "planar: {bits:016x}"
        );
        let back = decode_etc_rgb_block(bits);
        for (t, d) in texels.iter().zip(back.iter()) {
            assert!(
                t.iter().zip(d).all(|(a, b)| a.abs_diff(*b) <= 4),
                "{t:?} vs {d:?}"
            );
        }
    }

    #[test]
    fn a_known_block_decodes_to_the_specified_texels() {
        // Individual mode, no flip: left half base 0x88 grey with table 0, right half 0xcc grey
        // with table 7. Pixel (0,0) index 00 (+2), pixel (1,0) index 01 (+8), pixel (2,0) index
        // 10 (-47), pixel (3,0) index 11 (-183); the other right-half pixels index 00 (+47).
        let mut bits: u64 =
            (0x8u64 << 60) | (0xc << 56) | (0x8 << 52) | (0xc << 48) | (0x8 << 44) | (0xc << 40);
        bits |= 7 << 34;
        let set = |bits: &mut u64, x: usize, y: usize, index: u64| {
            let p = pixel_number(x, y);
            *bits |= (((index >> 1) & 1) << (16 + p)) | ((index & 1) << p);
        };
        set(&mut bits, 1, 0, 1);
        set(&mut bits, 2, 0, 2);
        set(&mut bits, 3, 0, 3);
        let texels = decode_etc_rgb_block(bits);
        assert_eq!(texels[0], [0x8a; 3]);
        assert_eq!(texels[1], [0x90; 3]);
        assert_eq!(texels[2], [0xcc - 47; 3]);
        assert_eq!(texels[3], [0xcc - 183; 3]);
        assert_eq!(texels[6], [0xcc + 47; 3]);
        // The encoder's bit order matches the decoder's: a 4×4 image of those texels encodes to
        // something that decodes within the smallest step.
        let mut rgba = Vec::new();
        for t in &texels {
            rgba.extend_from_slice(&[t[0], t[1], t[2], 255]);
        }
        let back = decode_etc_rgb_block(encode_etc_rgb_block(&block_texels(4, 4, &rgba, 0, 0)));
        let worst = back
            .iter()
            .zip(texels.iter())
            .map(|(a, b)| a[0].abs_diff(b[0]))
            .max()
            .unwrap_or(0);
        assert!(worst <= 24, "worst texel error {worst}");
    }

    #[test]
    fn an_eac_block_decodes_by_the_specified_rule() {
        // Base 128, multiplier 2, table 13 ([-1,-2,-3,-10,0,1,2,9]); pixel 0 index 4 (0) and
        // pixel 1 index 7 (+9): alpha 128 and 146; R11 128*8+4 = 1028 and 1028 + 9*2*8 = 1172.
        let bits = (128u64 << 56) | (2 << 52) | (13 << 48) | (4 << 45) | (7 << 42);
        let alpha = decode_eac_block(bits, false);
        assert_eq!((alpha[0], alpha[1]), (128, 146));
        let eleven = decode_eac_block(bits, true);
        assert_eq!((eleven[0], eleven[1]), (1028, 1172));
        // Multiplier 0 in 11-bit mode steps by one unit: 1028 + 9.
        let fine = (128u64 << 56) | (13 << 48) | (7 << 45);
        assert_eq!(decode_eac_block(fine, true)[0], 1037);
    }
}
