// SPDX-License-Identifier: MPL-2.0
//! The tileable ripple map, made on the CPU from a fixed seed (no asset, the same bytes on every
//! machine).
//!
//! The surface is a sum of sines (Finch, "Effective Water Simulation from Physical Models", GPU
//! Gems 2004) whose wave vectors are whole cycles per tile, so it tiles exactly. Each texel stores
//! the surface slope along x and z (red, green), the mean squared slope (blue; equal to the squared
//! slope at full resolution, larger once a mip averages several ripples, so the shader reads the
//! lost detail as roughness, after Olano and Baker's LEAN mapping) and the height (alpha). The mips
//! average all four channels, which is exact for the mean slope and the second moment.

/// Edge of the top level (texels).
pub const RIPPLE_MAP_SIZE: u32 = 256;

/// Slopes are stored in `−SLOPE_RANGE..=SLOPE_RANGE` (red and green).
pub const SLOPE_RANGE: f32 = 1.0;

/// Squared slopes are stored in `0..=SLOPE_SQ_RANGE` (blue).
pub const SLOPE_SQ_RANGE: f32 = 0.5;

/// Root-mean-square slope of the map before `WaterLook::normal_strength`.
pub const RMS_SLOPE: f32 = 0.13;

/// Waves summed.
const WAVES: usize = 40;

/// One mip level, `Rgba8Unorm` texels row by row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RippleLevel {
    /// Edge length (texels).
    pub size: u32,
    /// `size × size × 4` bytes.
    pub texels: Vec<u8>,
}

/// The decoded value of one texel: slope x, slope z, mean squared slope, height (0..1).
#[must_use]
pub fn decode(texel: [u8; 4]) -> [f32; 4] {
    let u = |b: u8| f32::from(b) / 255.0;
    [
        (u(texel[0]) * 2.0 - 1.0) * SLOPE_RANGE,
        (u(texel[1]) * 2.0 - 1.0) * SLOPE_RANGE,
        u(texel[2]) * SLOPE_SQ_RANGE,
        u(texel[3]),
    ]
}

fn encode(v: [f32; 4]) -> [u8; 4] {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    [
        q(v[0] / SLOPE_RANGE * 0.5 + 0.5),
        q(v[1] / SLOPE_RANGE * 0.5 + 0.5),
        q(v[2] / SLOPE_SQ_RANGE),
        q(v[3]),
    ]
}

/// A small deterministic generator (64-bit LCG, Knuth's MMIX constants).
struct Lcg(u64);

impl Lcg {
    fn next_unit(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
}

/// Builds every level of the ripple map, top level first, down to 1×1.
#[must_use]
pub fn ripple_levels() -> Vec<RippleLevel> {
    let n = RIPPLE_MAP_SIZE as usize;
    let mut rng = Lcg(0x5EED_0F3A_7E20_2026);
    // (kx, kz) in whole cycles per tile, amplitude, phase.
    let mut waves = Vec::with_capacity(WAVES);
    while waves.len() < WAVES {
        let kx = (rng.next_unit() * 41.0).floor() - 20.0;
        let kz = (rng.next_unit() * 41.0).floor() - 20.0;
        let k = (kx * kx + kz * kz).sqrt();
        if !(2.0..=20.0).contains(&k) {
            continue;
        }
        // Height amplitude falling as k^-1.6: slope amplitude falls slowly (k^-0.6), so the fine
        // ripples still show as sparkle.
        let amp = k.powf(-1.6);
        let phase = rng.next_unit() * std::f32::consts::TAU;
        waves.push((kx, kz, amp, phase));
    }
    let mut slope = vec![[0.0f32; 4]; n * n];
    let mut sum_sq = 0.0f64;
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for y in 0..n {
        for x in 0..n {
            let u = x as f32 / n as f32;
            let v = y as f32 / n as f32;
            let mut h = 0.0f32;
            let mut sx = 0.0f32;
            let mut sz = 0.0f32;
            for &(kx, kz, amp, phase) in &waves {
                let arg = std::f32::consts::TAU * (kx * u + kz * v) + phase;
                h += amp * arg.sin();
                let c = amp * std::f32::consts::TAU * arg.cos();
                sx += c * kx;
                sz += c * kz;
            }
            slope[y * n + x] = [sx, sz, 0.0, h];
            sum_sq += f64::from(sx * sx + sz * sz);
            lo = lo.min(h);
            hi = hi.max(h);
        }
    }
    let rms = ((sum_sq / (n * n) as f64).sqrt() as f32).max(1.0e-6);
    let scale = RMS_SLOPE / rms;
    let span = (hi - lo).max(1.0e-6);
    for texel in &mut slope {
        texel[0] *= scale;
        texel[1] *= scale;
        texel[2] = texel[0] * texel[0] + texel[1] * texel[1];
        texel[3] = (texel[3] - lo) / span;
    }
    let mut levels = Vec::new();
    let mut size = n;
    let mut current = slope;
    loop {
        levels.push(RippleLevel {
            size: size as u32,
            texels: current.iter().flat_map(|t| encode(*t)).collect(),
        });
        if size == 1 {
            break;
        }
        let half = size / 2;
        let mut next = vec![[0.0f32; 4]; half * half];
        for y in 0..half {
            for x in 0..half {
                let mut acc = [0.0f32; 4];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let t = current[(2 * y + dy) * size + 2 * x + dx];
                    for (a, v) in acc.iter_mut().zip(t) {
                        *a += 0.25 * v;
                    }
                }
                next[y * half + x] = acc;
            }
        }
        current = next;
        size = half;
    }
    levels
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_complete_and_deterministic() {
        let a = ripple_levels();
        let b = ripple_levels();
        assert_eq!(a, b);
        assert_eq!(a.len(), RIPPLE_MAP_SIZE.trailing_zeros() as usize + 1);
        for level in &a {
            assert_eq!(level.texels.len(), (level.size * level.size * 4) as usize);
        }
        assert_eq!(a.last().map(|l| l.size), Some(1));
    }

    /// The map tiles: the slope across the wrap seam is as continuous as inside the map.
    #[test]
    fn map_tiles_seamlessly() {
        let top = &ripple_levels()[0];
        let n = top.size as usize;
        let at = |x: usize, y: usize| {
            let i = (y * n + x) * 4;
            decode([
                top.texels[i],
                top.texels[i + 1],
                top.texels[i + 2],
                top.texels[i + 3],
            ])
        };
        let mut seam = 0.0f32;
        let mut inner = 0.0f32;
        for y in 0..n {
            seam = seam.max((at(0, y)[3] - at(n - 1, y)[3]).abs());
            inner = inner.max((at(1, y)[3] - at(0, y)[3]).abs());
        }
        assert!(seam <= inner * 1.5 + 2.0 / 255.0, "seam {seam} inner {inner}");
    }

    /// Averaging loses slope detail into the blue channel: on the 1×1 level the mean slope is
    /// near zero while the mean squared slope keeps the full map's energy.
    #[test]
    fn coarse_levels_keep_slope_variance() {
        let levels = ripple_levels();
        let last = levels.last().expect("levels");
        let t = decode([last.texels[0], last.texels[1], last.texels[2], last.texels[3]]);
        assert!(t[0].abs() < 0.02 && t[1].abs() < 0.02, "{t:?}");
        let expected = RMS_SLOPE * RMS_SLOPE;
        assert!(
            (t[2] - expected).abs() < 0.2 * expected + 0.004,
            "{} vs {expected}",
            t[2]
        );
    }
}
