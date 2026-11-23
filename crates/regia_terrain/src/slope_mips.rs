// SPDX-License-Identifier: MPL-2.0
//! Precomputed heightfield slope pyramid: the per-pixel footprint normal of the photoreal
//! terrain path in two filtered fetches instead of 32 height loads.
//!
//! # Why
//!
//! The colour pass shades every pixel with the heightfield's own normal at the pixel's
//! footprint (`fragment_normal` in `terrain.wgsl`): a central difference of the box-filtered
//! height pyramid ([`crate::height_mips`]), four manually bilinear taps on each of the two
//! pyramid levels around the footprint — 32 `R32Float` loads per pixel. On DO27's maps that
//! was 0.6–1.8 ms of a 2.5–4.1 ms terrain pass at 1440p (lane do27-perf).
//!
//! # What
//!
//! Level `k` of this pyramid stores, per texel of height level `k`, the central-difference
//! slope `((h[x+1] − h[x−1]) / 2s, (h[z+1] − h[z−1]) / 2s)` (rise over run, `s` = the level's
//! texel spacing in metres; neighbours clamped at the border). A central difference of a
//! bilinear surface taken one texel each way is linear in the heights and keeps the
//! fractional position, so bilinear interpolation of these texels *is* the four-tap
//! difference of one integer level (`normal_level`), border clamping included — except
//! within half a level texel of the map edge, where the position clamps before the taps.
//! A pixel then reads two levels with hardware bilinear filtering and blends them by the
//! fractional footprint level. (The old path differenced the fractionally blended surface at a
//! fractional step; blending the two levels' slopes instead differs by a fraction of the
//! slope change between levels, ADR in `docs/decisions.md`.)
//!
//! Stored as `Rg16Float` (filterable on every backend, 4 bytes per texel: the same size as
//! the `R32Float` height pyramid). Half precision keeps the slope to 1 part in 2048 —
//! about 0.03° of normal on any slope.

use crate::height_mips::HeightMips;
use crate::sampler::TerrainMap;

/// Slope texels `(dh/dx, dh/dz)` of pyramid level `k` over the texel rectangle
/// `rect = (col_start, col_end, row_start, row_end)`, row-major, as `Rg16Float` bits
/// (two `u16` per texel). Level 0 reads `map`, level `k >= 1` reads `mips`. Empty when the
/// level does not exist or the rectangle is empty or out of range.
#[must_use]
pub fn slope_texels(map: &TerrainMap, mips: &HeightMips, k: u32, rect: (u32, u32, u32, u32)) -> Vec<u16> {
    let Some((w, h, data)) = level_data(map, mips, k) else {
        return Vec::new();
    };
    let (c0, c1, r0, r1) = rect;
    let (c1, r1) = (c1.min(w), r1.min(h));
    if c1 <= c0 || r1 <= r0 {
        return Vec::new();
    }
    let step = mips.texel_m(map.world_width()) * (1u32 << k) as f32;
    let inv = 0.5 / step.max(1e-9);
    let rw = (c1 - c0) as usize;
    let rows = (r1 - r0) as usize;
    let mut out = vec![0u16; rw * rows * 2];
    let fill = |row0: usize, chunk: &mut [u16]| {
        for (i, texels) in chunk.chunks_mut(rw * 2).enumerate() {
            let row = r0 + (row0 + i) as u32;
            let up = (row + 1).min(h - 1) as usize * w as usize;
            let down = row.saturating_sub(1) as usize * w as usize;
            let here = row as usize * w as usize;
            for (j, texel) in texels.chunks_mut(2).enumerate() {
                let col = c0 + j as u32;
                let right = (col + 1).min(w - 1) as usize;
                let left = col.saturating_sub(1) as usize;
                let col = col as usize;
                let gx = (data[here + right] - data[here + left]) * inv;
                let gz = (data[up + col] - data[down + col]) * inv;
                texel[0] = f16_bits(gx);
                texel[1] = f16_bits(gz);
            }
        }
    };
    // Level 0 of a 4k map is 16 M texels: split the rows over the cores (load time).
    let threads = std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .min(16);
    if threads < 2 || rw * rows < 1 << 16 {
        fill(0, &mut out);
        return out;
    }
    let per = rows.div_ceil(threads);
    std::thread::scope(|scope| {
        for (n, chunk) in out.chunks_mut(per * rw * 2).enumerate() {
            let fill = &fill;
            scope.spawn(move || fill(n * per, chunk));
        }
    });
    out
}

/// The texel rectangle of slope level `k` that changes when the height texels `rect` of
/// the same level change: one texel more on every side (the central difference), clamped
/// to the level. `rect` is `(col_start, col_end, row_start, row_end)`.
#[must_use]
pub fn dependent_rect(
    map: &TerrainMap,
    mips: &HeightMips,
    k: u32,
    rect: (u32, u32, u32, u32),
) -> (u32, u32, u32, u32) {
    let Some((w, h, _)) = level_data(map, mips, k) else {
        return (0, 0, 0, 0);
    };
    let (c0, c1, r0, r1) = rect;
    (
        c0.saturating_sub(1),
        (c1 + 1).min(w),
        r0.saturating_sub(1),
        (r1 + 1).min(h),
    )
}

/// Width and height of pyramid level `k` in texels (level 0 is the map), `None` past the top.
#[must_use]
pub fn level_size(map: &TerrainMap, mips: &HeightMips, k: u32) -> Option<(u32, u32)> {
    level_data(map, mips, k).map(|(w, h, _)| (w, h))
}

/// Width, height and metre heights of pyramid level `k` (level 0 is the map).
fn level_data<'a>(map: &'a TerrainMap, mips: &'a HeightMips, k: u32) -> Option<(u32, u32, &'a [f32])> {
    if k == 0 {
        return Some((map.width(), map.height(), map.heights()));
    }
    mips.level(k)
        .map(|level| (level.width, level.height, level.data.as_slice()))
}

/// IEEE 754 binary16 bits of `v` (round to nearest even; overflow to infinity).
#[must_use]
pub fn f16_bits(v: f32) -> u16 {
    let x = v.to_bits();
    let sign = ((x >> 16) & 0x8000) as u16;
    let exp = ((x >> 23) & 0xff) as i32;
    let mant = x & 0x007f_ffff;
    if exp == 0xff {
        return sign | 0x7c00 | if mant != 0 { 0x0200 } else { 0 };
    }
    let e = exp - 127 + 15;
    if e >= 0x1f {
        return sign | 0x7c00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        // Subnormal half: shift the implicit-1 mantissa into place, round to nearest even.
        let m = mant | 0x0080_0000;
        let shift = (14 - e) as u32;
        let half = m >> shift;
        let rem = m & ((1 << shift) - 1);
        let mid = 1 << (shift - 1);
        let round = u32::from(rem > mid || (rem == mid && (half & 1) == 1));
        return sign | (half + round) as u16;
    }
    let half = ((e as u32) << 10) | (mant >> 13);
    let rem = mant & 0x1fff;
    let round = u32::from(rem > 0x1000 || (rem == 0x1000 && (half & 1) == 1));
    sign | (half + round) as u16
}

/// The `f32` value of binary16 bits (tests and CPU mirrors).
#[must_use]
pub fn f16_value(bits: u16) -> f32 {
    let sign = if bits & 0x8000 != 0 { -1.0 } else { 1.0 };
    let exp = i32::from((bits >> 10) & 0x1f);
    let mant = f32::from(bits & 0x03ff);
    match exp {
        0 => sign * mant * 2f32.powi(-24),
        0x1f if mant == 0.0 => sign * f32::INFINITY,
        0x1f => f32::NAN,
        _ => sign * (1.0 + mant / 1024.0) * 2f32.powi(exp - 15),
    }
}

/// CPU mirror of the shader's `normal_slope`: the slope at level-0 sample coordinates `s`
/// from fractional level `m`, bilinear over [`slope_texels`] of the two nearest levels
/// (exact `f32`, before the `Rg16Float` rounding).
#[must_use]
pub fn sample_slope(map: &TerrainMap, mips: &HeightMips, s: [f32; 2], m: f32) -> [f32; 2] {
    let top = (mips.level_count() - 1) as f32;
    let mm = m.clamp(0.0, top);
    let k0 = mm.floor() as u32;
    let f = mm - k0 as f32;
    let a = slope_level(map, mips, s, k0);
    if f <= 0.0 || k0 as f32 >= top {
        return a;
    }
    let b = slope_level(map, mips, s, k0 + 1);
    [a[0] + (b[0] - a[0]) * f, a[1] + (b[1] - a[1]) * f]
}

/// Bilinear slope of one integer level at level-0 sample coordinates.
fn slope_level(map: &TerrainMap, mips: &HeightMips, s: [f32; 2], k: u32) -> [f32; 2] {
    let Some((w, h, data)) = level_data(map, mips, k) else {
        return [0.0, 0.0];
    };
    let scale = (1u32 << k) as f32;
    let centre = (scale - 1.0) * 0.5;
    let u = ((s[0] - centre) / scale).clamp(0.0, (w - 1) as f32);
    let v = ((s[1] - centre) / scale).clamp(0.0, (h - 1) as f32);
    let (x0, y0) = (u.floor() as u32, v.floor() as u32);
    let (x1, y1) = ((x0 + 1).min(w - 1), (y0 + 1).min(h - 1));
    let (fx, fy) = (u - x0 as f32, v - y0 as f32);
    let inv = 0.5 / (mips.texel_m(map.world_width()) * scale).max(1e-9);
    let at = |x: u32, y: u32| data[(y * w + x) as usize];
    let g = |x: u32, y: u32| {
        [
            (at((x + 1).min(w - 1), y) - at(x.saturating_sub(1), y)) * inv,
            (at(x, (y + 1).min(h - 1)) - at(x, y.saturating_sub(1))) * inv,
        ]
    };
    let (g00, g10, g01, g11) = (g(x0, y0), g(x1, y0), g(x0, y1), g(x1, y1));
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    [
        lerp(lerp(g00[0], g10[0], fx), lerp(g01[0], g11[0], fx), fy),
        lerp(lerp(g00[1], g10[1], fx), lerp(g01[1], g11[1], fx), fy),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bumpy_map(n: u32) -> TerrainMap {
        let mut heights = Vec::with_capacity((n * n) as usize);
        for row in 0..n {
            for col in 0..n {
                let (x, z) = (col as f32, row as f32);
                heights.push(
                    (x * 0.37).sin() * 3.0
                        + (z * 0.21).cos() * 5.0
                        + x * 0.1
                        + ((col * 7 + row * 13) % 5) as f32 * 0.4,
                );
            }
        }
        TerrainMap::from_heights(n, n, heights, (n - 1) as f32 * 2.0, 1.0, 0.0).expect("map")
    }

    /// The four-tap central difference of one integer level (`normal_level` in the shader),
    /// from the height pyramid directly.
    fn four_tap(map: &TerrainMap, mips: &HeightMips, s: [f32; 2], k: u32) -> [f32; 2] {
        let d = (1u32 << k) as f32;
        let step = mips.texel_m(map.world_width()) * d;
        let h = |u: f32, v: f32| mips.sample(map, [u, v], k as f32);
        [
            (h(s[0] + d, s[1]) - h(s[0] - d, s[1])) / (2.0 * step),
            (h(s[0], s[1] + d) - h(s[0], s[1] - d)) / (2.0 * step),
        ]
    }

    #[test]
    fn bilinear_slope_is_the_four_tap_difference_of_each_level() {
        let map = bumpy_map(65);
        let mips = HeightMips::build(&map);
        // Interior and the clamped one-texel border. Within half a texel of the map edge at
        // levels >= 1 the position itself clamps first (the four-tap form clamps each tap):
        // those points are left out.
        for k in 0..mips.level_count() - 1 {
            let scale = (1u32 << k) as f32;
            let (lw, _) = level_size(&map, &mips, k).expect("level");
            for &s in &[
                [0.0, 0.0],
                [0.4, 31.7],
                [17.25, 22.6],
                [40.5, 63.0],
                [64.0, 3.3],
                [63.6, 63.9],
            ] {
                let p = s.map(|v| (v - (scale - 1.0) * 0.5) / scale);
                if p.iter().any(|&v| v < 0.0 || v > (lw - 1) as f32) {
                    continue;
                }
                let want = four_tap(&map, &mips, s, k);
                let got = sample_slope(&map, &mips, s, k as f32);
                for a in 0..2 {
                    assert!(
                        (got[a] - want[a]).abs() < 1e-4,
                        "level {k} at {s:?} axis {a}: {} vs {}",
                        got[a],
                        want[a]
                    );
                }
            }
        }
    }

    #[test]
    fn texels_match_the_cpu_mirror_and_round_to_half() {
        let map = bumpy_map(300);
        let mips = HeightMips::build(&map);
        for k in 0..3u32 {
            let (w, h) = if k == 0 {
                (map.width(), map.height())
            } else {
                let l = mips.level(k).expect("level");
                (l.width, l.height)
            };
            // Large enough to take the threaded path at level 0.
            let texels = slope_texels(&map, &mips, k, (0, w, 0, h));
            assert_eq!(texels.len(), (w * h * 2) as usize);
            let scale = (1u32 << k) as f32;
            for &(col, row) in &[(0u32, 0u32), (5, 9), (w / 2, h - 1), (w - 1, h / 3)] {
                // Texel centre of level k at level-0 coordinates.
                let s = [
                    col as f32 * scale + (scale - 1.0) * 0.5,
                    row as f32 * scale + (scale - 1.0) * 0.5,
                ];
                let want = sample_slope(&map, &mips, s, k as f32);
                let at = ((row * w + col) * 2) as usize;
                for a in 0..2 {
                    let got = f16_value(texels[at + a]);
                    assert!(
                        (got - want[a]).abs() <= want[a].abs() * 1e-3 + 1e-6,
                        "level {k} texel ({col}, {row}) axis {a}: {got} vs {}",
                        want[a]
                    );
                }
            }
        }
    }

    #[test]
    fn a_region_update_matches_a_full_rebuild() {
        let n = 64u32;
        let mut map = bumpy_map(n);
        let mut mips = HeightMips::build(&map);
        let before: Vec<Vec<u16>> = (0..mips.level_count())
            .map(|k| slope_texels(&map, &mips, k, (0, u32::MAX, 0, u32::MAX)))
            .collect();
        let crater = crate::deform::Crater {
            center: [3.0, -5.0],
            radius: 9.0,
            depth: 2.0,
            rim_height: 0.4,
        };
        let dirty = crate::deform::apply_crater(&mut map, &crater).expect("crater");
        let mut rects = vec![(dirty.col_start, dirty.col_end, dirty.row_start, dirty.row_end)];
        rects.extend(mips.update_region(&map, &dirty));
        let mut patched = before;
        for (k, rect) in rects.into_iter().enumerate() {
            let k = k as u32;
            let r = dependent_rect(&map, &mips, k, rect);
            let part = slope_texels(&map, &mips, k, r);
            let w = if k == 0 {
                map.width()
            } else {
                mips.level(k).expect("level").width
            };
            let rw = (r.1 - r.0) as usize;
            for (i, row) in (r.2..r.3).enumerate() {
                let dst = ((row * w + r.0) * 2) as usize;
                patched[k as usize][dst..dst + rw * 2].copy_from_slice(&part[i * rw * 2..(i + 1) * rw * 2]);
            }
        }
        for k in 0..mips.level_count() {
            let fresh = slope_texels(&map, &mips, k, (0, u32::MAX, 0, u32::MAX));
            assert_eq!(
                patched[k as usize], fresh,
                "slope level {k} differs after a region update"
            );
        }
    }

    #[test]
    fn half_conversion_round_trips() {
        for v in [0.0f32, 1.0, -2.0, 0.5, 3.3333, -0.0007, 12.75, 1e-6] {
            let back = f16_value(f16_bits(v));
            assert!((back - v).abs() <= v.abs() * 1e-3 + 6e-8, "{v} -> {back}");
        }
        assert_eq!(f16_bits(1.0), 0x3c00);
        assert_eq!(f16_bits(65504.0), 0x7bff);
    }
}
