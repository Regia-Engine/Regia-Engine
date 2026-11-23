// SPDX-License-Identifier: MPL-2.0
//! Linear-light accumulation of render-out samples (ADR-1404): the supersample block of every
//! subframe of one delivered frame is summed in linear light and quantized once, to 8 or 16 bits.
//!
//! Inputs are sRGB-encoded RGBA at the render size (8-bit codes from the classic readback, or
//! 16-bit codes from the deep readout). Each colour code is turned into linear light in 32-bit
//! fixed point through a table, summed per delivered pixel over the `factor × factor` block and
//! the subframes, and turned back into the code whose linear value is nearest to the average (the
//! midpoints between neighbouring codes decide, compared in integers). Alpha is averaged as is.
//! After the tables are built every step is integer arithmetic in a fixed order, so the result
//! is the same on every platform and thread count.

use std::sync::OnceLock;

/// Fixed-point scale of linear light: `1.0` is `2^LINEAR_BITS`.
const LINEAR_BITS: i32 = 32;

/// Bits per channel of the samples and of the result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 8-bit codes (`u8`).
    Eight,
    /// 16-bit codes (`u16`).
    Sixteen,
}

impl Depth {
    /// Bits per channel.
    pub fn bits(self) -> u32 {
        match self {
            Self::Eight => 8,
            Self::Sixteen => 16,
        }
    }

    fn max_code(self) -> u32 {
        (1 << self.bits()) - 1
    }
}

/// The sRGB decode of `code` out of `max`, as linear light in [`LINEAR_BITS`] fixed point.
fn srgb_code_to_linear(code: u32, max: u32) -> u64 {
    let c = f64::from(code) / f64::from(max);
    let linear = if c <= 0.04045 {
        c / 12.92
    } else {
        ((c + 0.055) / 1.055).powf(2.4)
    };
    (linear * 2f64.powi(LINEAR_BITS)).round() as u64
}

/// Linear value of every code, and the doubled midpoints between neighbours.
struct Tables {
    linear: Vec<u64>,
    twice_mid: Vec<u64>,
}

fn build(depth: Depth) -> Tables {
    let max = depth.max_code();
    let linear: Vec<u64> = (0..=max).map(|code| srgb_code_to_linear(code, max)).collect();
    let twice_mid = linear.windows(2).map(|w| w[0] + w[1]).collect();
    Tables { linear, twice_mid }
}

fn tables(depth: Depth) -> &'static Tables {
    static EIGHT: OnceLock<Tables> = OnceLock::new();
    static SIXTEEN: OnceLock<Tables> = OnceLock::new();
    match depth {
        Depth::Eight => EIGHT.get_or_init(|| build(Depth::Eight)),
        Depth::Sixteen => SIXTEEN.get_or_init(|| build(Depth::Sixteen)),
    }
}

/// The code whose linear value is nearest to `sum / count` (ties to the lower code): the number
/// of midpoints the average lies above. A float sRGB encode gives the first guess; integer
/// comparisons against the midpoints settle it, so the guess never changes the answer.
fn encode(sum: u64, count: u64, tables: &Tables) -> u32 {
    let mids = &tables.twice_mid;
    let above = |index: usize| 2 * u128::from(sum) > u128::from(mids[index]) * u128::from(count);
    let max = mids.len();
    let linear = sum as f64 / count.max(1) as f64 / 2f64.powi(LINEAR_BITS);
    let srgb = if linear <= 0.003_130_8 {
        linear * 12.92
    } else {
        1.055 * linear.powf(1.0 / 2.4) - 0.055
    };
    let mut code = ((srgb * max as f64).round().max(0.0) as usize).min(max);
    while code < max && above(code) {
        code += 1;
    }
    while code > 0 && !above(code - 1) {
        code -= 1;
    }
    code as u32
}

/// Sums of one delivered frame: per output pixel, linear RGB and raw alpha over the supersample
/// block of every sample added so far.
pub struct Accumulator {
    width: u32,
    height: u32,
    factor: u32,
    depth: Depth,
    sums: Vec<u64>,
    added: u32,
}

impl Accumulator {
    /// An empty accumulator for `width`×`height` delivered pixels, samples rendered at `factor`
    /// times that size, of `depth` bits.
    pub fn new(width: u32, height: u32, factor: u32, depth: Depth) -> Self {
        Self {
            width,
            height,
            factor: factor.max(1),
            depth,
            sums: vec![0; width as usize * height as usize * 4],
            added: 0,
        }
    }

    /// Forgets the samples (the next frame starts).
    pub fn clear(&mut self) {
        self.sums.fill(0);
        self.added = 0;
    }

    fn check(&self, len: usize, depth: Depth) -> Result<(), String> {
        let (w, h, f) = (self.width as usize, self.height as usize, self.factor as usize);
        if depth != self.depth {
            return Err(format!(
                "a {}-bit sample added to a {}-bit frame",
                depth.bits(),
                self.depth.bits()
            ));
        }
        if len != w * f * h * f * 4 {
            return Err(format!(
                "sample of {len} values, expected {}x{} RGBA",
                w * f,
                h * f
            ));
        }
        Ok(())
    }

    /// Adds a checked sample: bands of output rows in parallel (each pixel's sums are added in
    /// the same order whatever the band split, so the result does not depend on the thread count).
    fn add<T: Copy + Into<u32> + Sync>(&mut self, samples: &[T]) {
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_accumulate").entered();
        let table = tables(self.depth).linear.as_slice();
        let (w, h, f) = (self.width as usize, self.height as usize, self.factor as usize);
        let band = h.div_ceil(threads(h));
        std::thread::scope(|scope| {
            for (i, sums) in self.sums.chunks_mut(band * w * 4).enumerate() {
                scope.spawn(move || add_rows(sums, samples, table, i * band, w, f));
            }
        });
        self.added += 1;
    }

    /// Adds one 8-bit sRGB RGBA sample at the render size.
    ///
    /// # Errors
    ///
    /// The accumulator is 16-bit, or the sample is not the render size.
    pub fn add_rgba8(&mut self, rgba: &[u8]) -> Result<(), String> {
        self.check(rgba.len(), Depth::Eight)?;
        self.add(rgba);
        Ok(())
    }

    /// Adds one 16-bit sRGB RGBA sample at the render size.
    ///
    /// # Errors
    ///
    /// The accumulator is 8-bit, or the sample is not the render size.
    pub fn add_rgba16(&mut self, rgba: &[u16]) -> Result<(), String> {
        self.check(rgba.len(), Depth::Sixteen)?;
        self.add(rgba);
        Ok(())
    }

    /// Samples behind every delivered pixel: `factor² × added` (at least 1).
    fn count(&self) -> u64 {
        (u64::from(self.factor) * u64::from(self.factor) * u64::from(self.added)).max(1)
    }

    /// The codes of every channel, bands of pixels in parallel, joined in order.
    fn resolve<O: Send>(&self, narrow: impl Fn(u32) -> O + Sync) -> Result<Vec<O>, String> {
        let _span = tracing::debug_span!(target: "regia_agent", "render_out_resolve").entered();
        let tables = tables(self.depth);
        let count = self.count();
        let pixels = self.sums.as_chunks::<4>().0;
        let band = pixels.len().div_ceil(threads(self.height as usize)).max(1);
        let narrow = &narrow;
        let parts = std::thread::scope(|scope| {
            let handles: Vec<_> = pixels
                .chunks(band)
                .map(|chunk| {
                    scope.spawn(move || {
                        chunk
                            .iter()
                            .flat_map(|px| {
                                [
                                    encode(px[0], count, tables),
                                    encode(px[1], count, tables),
                                    encode(px[2], count, tables),
                                    ((px[3] + count / 2) / count) as u32,
                                ]
                            })
                            .map(narrow)
                            .collect::<Vec<O>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().map_err(|_| "a resolve thread panicked".to_string()))
                .collect::<Result<Vec<_>, _>>()
        })?;
        Ok(parts.into_iter().flatten().collect())
    }

    /// The delivered 8-bit frame.
    ///
    /// # Errors
    ///
    /// The accumulator is 16-bit.
    pub fn resolve_rgba8(&self) -> Result<Vec<u8>, String> {
        if self.depth != Depth::Eight {
            return Err("a 16-bit frame resolved to 8 bits".into());
        }
        self.resolve(|v| v as u8)
    }

    /// The delivered 16-bit frame.
    ///
    /// # Errors
    ///
    /// The accumulator is 8-bit.
    pub fn resolve_rgba16(&self) -> Result<Vec<u16>, String> {
        if self.depth != Depth::Sixteen {
            return Err("an 8-bit frame resolved to 16 bits".into());
        }
        self.resolve(|v| v as u16)
    }
}

/// Threads one accumulation or resolve uses for `rows` output rows: the machine's parallelism,
/// at most 8, at most one per row.
fn threads(rows: usize) -> usize {
    std::thread::available_parallelism()
        .map_or(1, std::num::NonZeroUsize::get)
        .clamp(1, 8)
        .min(rows.max(1))
}

/// Adds the output rows from `y0` on (`sums`, whole rows of `w` RGBA pixels) from a sample `f`
/// times larger.
fn add_rows<T: Copy + Into<u32>>(
    sums: &mut [u64],
    samples: &[T],
    table: &[u64],
    y0: usize,
    w: usize,
    f: usize,
) {
    let src_row = w * f * 4;
    for (i, out) in sums.chunks_mut(w * 4).enumerate() {
        let y = y0 + i;
        for dy in 0..f {
            let row = &samples[(y * f + dy) * src_row..][..src_row];
            for (x, px) in out.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                for dx in 0..f {
                    let at = (x * f + dx) * 4;
                    for (c, sum) in px.iter_mut().take(3).enumerate() {
                        let code: u32 = row[at + c].into();
                        *sum += table[code as usize];
                    }
                    let alpha: u32 = row[at + 3].into();
                    px[3] += u64::from(alpha);
                }
            }
        }
    }
}

/// 16-bit samples as little-endian bytes (what the frame hash reads).
pub fn le_bytes(rgba16: &[u16]) -> Vec<u8> {
    rgba16.iter().flat_map(|v| v.to_le_bytes()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px8(rgba: [u8; 4], n: usize) -> Vec<u8> {
        (0..n).flat_map(|_| rgba).collect()
    }

    #[test]
    fn every_code_survives_the_round_trip() {
        for depth in [Depth::Eight, Depth::Sixteen] {
            let t = tables(depth);
            for code in 0..=depth.max_code() {
                let linear = t.linear[code as usize];
                for count in [1u64, 4, 9, 64, 1024] {
                    assert_eq!(
                        encode(linear * count, count, t),
                        code,
                        "{depth:?} code {code} x{count}"
                    );
                }
            }
        }
    }

    #[test]
    fn linear_tables_are_strictly_increasing() {
        for depth in [Depth::Eight, Depth::Sixteen] {
            let t = tables(depth);
            assert!(t.linear.windows(2).all(|w| w[0] < w[1]), "{depth:?}");
            assert_eq!(t.linear[0], 0);
            assert_eq!(*t.linear.last().expect("table"), 1 << 32);
        }
    }

    #[test]
    fn the_float_guess_never_decides_the_code() {
        // The integer midpoint search gives the nearest code for any average.
        let t = tables(Depth::Sixteen);
        let mut seed = 0x9e37_79b9_u64;
        for _ in 0..20_000 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let count = 1 + (seed >> 54);
            let sum = (seed >> 20) % ((1u64 << 32) * count + 1);
            let code = encode(sum, count, t) as usize;
            let avg = sum as f64 / count as f64;
            let dist = |c: usize| (t.linear[c] as f64 - avg).abs();
            if code > 0 {
                assert!(dist(code) <= dist(code - 1) + 1e-6, "sum {sum} count {count}");
            }
            if code < t.linear.len() - 1 {
                assert!(dist(code) <= dist(code + 1) + 1e-6, "sum {sum} count {count}");
            }
        }
    }

    #[test]
    fn eight_bit_accumulation_matches_the_supersample_downsample() {
        // One subframe at factor 2 is the classic downsample (the nearest code in linear light).
        let mut rgba = Vec::new();
        for value in [0u8, 255, 255, 0] {
            rgba.extend_from_slice(&[value, value, value, 255]);
        }
        let mut acc = Accumulator::new(1, 1, 2, Depth::Eight);
        acc.add_rgba8(&rgba).expect("add");
        assert_eq!(acc.resolve_rgba8().expect("resolve"), [188, 188, 188, 255]);
        assert_eq!(
            acc.resolve_rgba8(),
            Ok(super::super::plan::downsample(&rgba, 1, 1, 2))
        );
    }

    #[test]
    fn subframes_average_in_linear_light() {
        // A pixel black in one subframe and white in the next is 50 % linear = sRGB 188 / 48 356.
        let mut acc = Accumulator::new(1, 1, 1, Depth::Eight);
        acc.add_rgba8(&px8([0, 0, 0, 255], 1)).expect("add");
        acc.add_rgba8(&px8([255, 255, 255, 255], 1)).expect("add");
        assert_eq!(acc.added, 2);
        assert_eq!(acc.resolve_rgba8().expect("resolve"), [188, 188, 188, 255]);
        let mut deep = Accumulator::new(1, 1, 1, Depth::Sixteen);
        deep.add_rgba16(&[0, 0, 0, 65535]).expect("add");
        deep.add_rgba16(&[65535, 65535, 65535, 65535]).expect("add");
        let out = deep.resolve_rgba16().expect("resolve");
        // sRGB encode of 0.5 = 0.735357 → 48 192 of 65 535.
        assert_eq!(out, [48_192, 48_192, 48_192, 65535]);
        deep.clear();
        assert_eq!(deep.added, 0);
        deep.add_rgba16(&[7, 8, 9, 10]).expect("add");
        assert_eq!(deep.resolve_rgba16().expect("resolve"), [7, 8, 9, 10]);
    }

    #[test]
    fn subframes_and_supersampling_combine_into_one_average() {
        // Two subframes at factor 2 equal one average over all eight samples.
        let a: Vec<u8> = [10u8, 200, 30, 120]
            .iter()
            .flat_map(|v| [*v, *v, *v, 255])
            .collect();
        let b: Vec<u8> = [250u8, 0, 90, 60]
            .iter()
            .flat_map(|v| [*v, *v, *v, 255])
            .collect();
        let mut acc = Accumulator::new(1, 1, 2, Depth::Eight);
        acc.add_rgba8(&a).expect("a");
        acc.add_rgba8(&b).expect("b");
        let t = tables(Depth::Eight);
        let sum: u64 = a
            .iter()
            .chain(&b)
            .step_by(4)
            .map(|c| t.linear[usize::from(*c)])
            .sum();
        let want = encode(sum, 8, t) as u8;
        assert_eq!(acc.resolve_rgba8().expect("resolve"), [want, want, want, 255]);
    }

    #[test]
    fn the_band_split_never_changes_the_result() {
        // Rows split over threads give the same sums as one serial pass over the whole frame.
        let (w, h, f) = (7usize, 13usize, 3usize);
        let mut seed = 0x2545_f491u32;
        let sample: Vec<u16> = (0..w * f * h * f * 4)
            .map(|_| {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (seed >> 16) as u16
            })
            .collect();
        let mut acc = Accumulator::new(w as u32, h as u32, f as u32, Depth::Sixteen);
        acc.add_rgba16(&sample).expect("add");
        let mut serial = vec![0u64; w * h * 4];
        add_rows(&mut serial, &sample, &tables(Depth::Sixteen).linear, 0, w, f);
        assert_eq!(acc.sums, serial);
        let t = tables(Depth::Sixteen);
        let want: Vec<u16> = serial
            .as_chunks::<4>()
            .0
            .iter()
            .flat_map(|px| {
                let n = (f * f) as u64;
                [
                    encode(px[0], n, t) as u16,
                    encode(px[1], n, t) as u16,
                    encode(px[2], n, t) as u16,
                    ((px[3] + n / 2) / n) as u16,
                ]
            })
            .collect();
        assert_eq!(acc.resolve_rgba16().expect("resolve"), want);
    }

    #[test]
    fn the_layout_is_kept_and_sizes_are_checked() {
        // 4x2 source at factor 2 -> 2x1: red block then blue block.
        let mut rgba = Vec::new();
        for _ in 0..2 {
            for x in 0..4 {
                rgba.extend_from_slice(if x < 2 {
                    &[255, 0, 0, 255]
                } else {
                    &[0, 0, 255, 255]
                });
            }
        }
        let mut acc = Accumulator::new(2, 1, 2, Depth::Eight);
        acc.add_rgba8(&rgba).expect("add");
        assert_eq!(
            acc.resolve_rgba8().expect("resolve"),
            [255, 0, 0, 255, 0, 0, 255, 255]
        );
        assert!(acc.add_rgba8(&rgba[..8]).is_err(), "wrong size");
        assert!(acc.add_rgba16(&[0; 32]).is_err(), "wrong depth");
        assert!(acc.resolve_rgba16().is_err());
        let deep = Accumulator::new(1, 1, 1, Depth::Sixteen);
        assert!(deep.resolve_rgba8().is_err());
    }

    /// Microbenchmark, never run by the gate (`docs/06-testing-and-ci.md` § 9): CPU cost of adding
    /// one render-size sample and of resolving a delivered frame, 8 and 16 bits, at 1440p and 4K.
    #[test]
    #[ignore = "microbenchmark: run by hand, see the ADR-1404 lane report"]
    fn perf_micro_accumulate_and_resolve() {
        use std::time::Instant;
        for (w, h) in [(2560u32, 1440u32), (3840, 2160)] {
            let n = (w * h * 4) as usize;
            let eight: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
            let sixteen: Vec<u16> = (0..n).map(|i| (i * 37 % 65_521) as u16).collect();
            for depth in [Depth::Eight, Depth::Sixteen] {
                let mut acc = Accumulator::new(w, h, 1, depth);
                let start = Instant::now();
                for _ in 0..4 {
                    match depth {
                        Depth::Eight => acc.add_rgba8(&eight).expect("add"),
                        Depth::Sixteen => acc.add_rgba16(&sixteen).expect("add"),
                    }
                }
                let add = start.elapsed() / 4;
                let start = Instant::now();
                let resolved = match depth {
                    Depth::Eight => acc.resolve_rgba8().map(|v| v.len()),
                    Depth::Sixteen => acc.resolve_rgba16().map(|v| v.len()),
                };
                let resolve = start.elapsed();
                assert_eq!(resolved, Ok(n));
                println!(
                    "perf-micro accumulate {w}x{h} {}-bit: add {add:?} per sample, resolve {resolve:?}",
                    depth.bits()
                );
            }
        }
    }

    #[test]
    fn a_deep_gradient_keeps_every_step_an_eight_bit_one_loses() {
        // A smooth ramp averaged from 16-bit samples has many more levels than 8 bits can hold:
        // the reason for `bit_depth: 16` (sky gradients and mist bands).
        let width = 1024u32;
        let ramp: Vec<u16> = (0..width)
            .flat_map(|x| {
                let v = (u32::from(u16::MAX) * x / (width - 1)) as u16 / 16;
                [v, v, v, u16::MAX]
            })
            .collect();
        let mut deep = Accumulator::new(width, 1, 1, Depth::Sixteen);
        deep.add_rgba16(&ramp).expect("add");
        let out = deep.resolve_rgba16().expect("resolve");
        let mut levels: Vec<u16> = out.iter().step_by(4).copied().collect();
        levels.dedup();
        assert_eq!(levels.len(), 1024, "no two neighbours collapse");
        let as8: std::collections::BTreeSet<u16> = out
            .iter()
            .step_by(4)
            .map(|v| (u32::from(*v) * 255 / 65535) as u16)
            .collect();
        assert!(
            as8.len() <= 17,
            "the same ramp in 8 bits has {} levels",
            as8.len()
        );
        assert_eq!(le_bytes(&[0x0102]), [0x02, 0x01]);
    }
}
