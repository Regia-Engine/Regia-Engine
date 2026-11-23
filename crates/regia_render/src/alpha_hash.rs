// SPDX-License-Identifier: MPL-2.0
//! Hashed alpha thresholds (ADR-1489): the CPU mirror of the noise phase `alpha_rejects` in
//! `lab_forward.wgsl` uses for masked materials under temporal AA, and a spectral measure of
//! the 1 px stipple a lattice threshold leaves in the resolved image.
//!
//! [`threshold`] is that phase, not the value the shader compares `alpha` with. With the hash
//! flag off (`frame.interior_half.w` bit 0 clear) the shader compares `alpha < cutoff`. With the
//! flag on it compares `alpha < mix(cutoff, noise, minified)`, where `noise` is this phase
//! unless blue noise replaces it, and `minified` is 0 on a magnified texel.
//!
//! The hashed test (Wyman & McGuire, "Hashed Alpha Testing", I3D 2017) keeps a pixel of a
//! minified mask when `alpha` is above a per-pixel threshold, and the temporal resolve averages
//! the kept and rejected frames into fractional coverage. The threshold is a per-pixel phase
//! advanced by the golden ratio every frame (an R1 sequence, [`GOLDEN_STEP`]), so each pixel
//! walks evenly spread thresholds.
//!
//! The phase:
//! - On a **held view** (the unjittered view-projection equals the last frame's, or there is no
//!   last frame) it is
//!   interleaved gradient noise ([`ign_phase`], Jimenez 2014). Its 3 × 3 neighbourhoods are the
//!   most even of any pattern, so the resolve's neighbourhood clip keeps the converging
//!   history.
//! - On a **moving view** it is [`hierarchy_phase`]. IGN is a single lattice: neighbours differ by
//!   0.556 across and 0.309 down, a plane wave at [`IGN_LATTICE`] cycles per pixel. Under a
//!   moving camera the history weight is lower and the reprojected history mixes neighbours, so
//!   the resolved coverage keeps an error that is a function of the IGN phase alone: the same
//!   lattice, a 1 px near-checkerboard stipple on every hashed canopy (the s1_t21 oak in look
//!   round 4). The hierarchy phase is uniform, every aligned 2 × 2 block holds one phase in each
//!   quarter of `[0, 1)`, and its residual has no spectral peak.
//!
//! [`stipple_energy`] measures the stipple: the share of an image's high-frequency power at the
//! IGN lattice and at the `(-1)^(x+y)` checkerboard.

use std::f32::consts::TAU;

/// The per-frame step of the threshold: the golden ratio's fractional part (an R1 sequence).
pub const GOLDEN_STEP: f32 = 0.618_034;

/// Levels of 2 × 2 cells in [`hierarchy_phase`]: an 8 × 8 pixel hierarchy, 64 ranks (the last
/// hash dithers inside a rank's step).
pub const HIERARCHY_LEVELS: u32 = 3;

/// Frequency of the interleaved-gradient-noise lattice in cycles per pixel, `(fx, fy)` folded
/// into `[-0.5, 0.5)`: `fract(52.9829189 × 0.06711056) − 1` and `fract(52.9829189 × 0.00583715)`.
pub const IGN_LATTICE: [f32; 2] = [-0.444_3, 0.309_3];

/// The PCG output permutation (O'Neill 2014; the variant of Jarzynski & Olano, "Hash Functions
/// for GPU Rendering", JCGT 2020): `alpha_hash_pcg` in `lab_forward.wgsl`.
#[must_use]
pub fn pcg(v: u32) -> u32 {
    let s = v.wrapping_mul(747_796_405).wrapping_add(2_891_336_453);
    let w = ((s >> ((s >> 28) + 4)) ^ s).wrapping_mul(277_803_737);
    (w >> 22) ^ w
}

/// Interleaved gradient noise (Jimenez, "Next Generation Post Processing in Call of Duty:
/// Advanced Warfare", SIGGRAPH 2014) at fragment position `frag` (pixel centres at `.5`): the
/// held-view phase.
#[must_use]
pub fn ign_phase(frag: [f32; 2]) -> f32 {
    (52.982_918 * (frag[0] * 0.067_110_56 + frag[1] * 0.005_837_15).fract()).fract()
}

/// The moving-view phase of pixel `pix`: its rank in a [`HIERARCHY_LEVELS`]-level hierarchy of
/// 2 × 2 cells, each cell ordering its four children by a permutation hashed from the cell's
/// coordinates and level (the random-digit-permutation pixel ordering of Ahmed & Wonka,
/// "Screen-Space Blue-Noise Diffusion of Monte Carlo Sampling Error via Hierarchical Ordering
/// of Pixels", SIGGRAPH Asia 2020; Owen 1995 scrambling). The finest level is the most
/// significant digit. `alpha_hash_phase` in `lab_forward.wgsl`, bit for bit in the rank.
#[must_use]
pub fn hierarchy_phase(pix: [u32; 2]) -> f32 {
    let mut rank = 0u32;
    let mut h = 0u32;
    for level in 0..HIERARCHY_LEVELS {
        let pos = ((pix[0] >> level) & 1) | (((pix[1] >> level) & 1) << 1);
        let cell = [pix[0] >> (level + 1), pix[1] >> (level + 1)];
        h = pcg(cell[0].wrapping_mul(0x8da6_b343)
            ^ cell[1].wrapping_mul(0xd816_3841)
            ^ (level + 1).wrapping_mul(0xcb1a_b31f));
        rank = rank * 4 + permuted_digit(pos, h % 24);
    }
    (rank as f32 + (h >> 8) as f32 * (1.0 / 16_777_216.0)) * (1.0 / 64.0)
}

/// Digit (0..4) that permutation `p` (0..24) of S4 gives child `pos` (0..4) of a cell: child
/// `p % 4` takes 0, the other three take 1, 2, 3 in one of six orders (rotation, reflection).
fn permuted_digit(pos: u32, p: u32) -> u32 {
    let a = p & 3;
    if pos == a {
        return 0;
    }
    let k = (p >> 2) % 3;
    let mut j = pos - u32::from(pos > a);
    if p >= 12 {
        j = 2 - j;
    }
    1 + (j + k) % 3
}

/// The noise phase at fragment `frag` (pixel centres at `.5`) on frame `frame` (the forward
/// pass's temporal noise index): the phase of the view's kind (`held`: [`ign_phase`], else
/// [`hierarchy_phase`]) advanced by [`GOLDEN_STEP`] per frame. The shader does not compare
/// `alpha` with this value directly. See the module docs.
#[must_use]
pub fn threshold(frag: [f32; 2], frame: f32, held: bool) -> f32 {
    let phase = if held {
        ign_phase(frag)
    } else {
        hierarchy_phase([frag[0].max(0.0) as u32, frag[1].max(0.0) as u32])
    };
    (phase + GOLDEN_STEP * frame).fract()
}

/// The forward pass's alpha test for a masked material without blue noise: `alpha_rejects` in
/// `lab_forward.wgsl`. With `hashed` off (`frame.interior_half.w` bit 0 clear) it is
/// `alpha < cutoff`. With it on, `alpha < mix(cutoff, noise, minified)` with `noise` =
/// [`threshold`]`(frag, frame, held)`: a magnified texel (`minified = 0`) still compares
/// `cutoff`, a fully minified one (`minified = 1`) compares the noise.
#[must_use]
pub fn rejects(
    alpha: f32,
    cutoff: f32,
    minified: f32,
    frag: [f32; 2],
    frame: f32,
    held: bool,
    hashed: bool,
) -> bool {
    if !hashed {
        return alpha < cutoff;
    }
    let noise = threshold(frag, frame, held);
    alpha < cutoff + (noise - cutoff) * minified
}

/// How much of an image's fine-scale power sits on the patterns of a stippled hashed
/// test ([`stipple_energy`]). Shares are of the power above a quarter of the sampling
/// frequency (radius 0.25 cycles per pixel), after a Hann window and removing the mean.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct StippleEnergy {
    /// Share within 0.03 cycles per pixel of the IGN lattice, `±`[`IGN_LATTICE`].
    pub lattice: f32,
    /// Share in the checkerboard corner (`|fx|` and `|fy|` both at least 0.45): the
    /// `(-1)^(x+y)` pattern.
    pub checker: f32,
    /// The strongest single frequency over the mean fine-scale power: about 10-40 for noise,
    /// hundreds or more for a lattice.
    pub peak_ratio: f32,
}

/// Measures the stipple of a `width` × `height` luminance image (row-major): a separable
/// discrete Fourier transform (no FFT; cost `width × height × (width + height)`). An image with
/// fewer than 8 pixels on a side, or without fine-scale power, measures zero.
#[must_use]
pub fn stipple_energy(luma: &[f32], width: usize, height: usize) -> StippleEnergy {
    let zero = StippleEnergy {
        lattice: 0.0,
        checker: 0.0,
        peak_ratio: 0.0,
    };
    if width < 8 || height < 8 || luma.len() < width * height {
        return zero;
    }
    let mean = luma[..width * height].iter().sum::<f32>() / (width * height) as f32;
    let hann = |i: usize, n: usize| 0.5 - 0.5 * (TAU * i as f32 / (n - 1) as f32).cos();
    // Rows: re/im[y][kx].
    let (cos_w, sin_w) = twiddles(width);
    let mut re = vec![0.0f32; width * height];
    let mut im = vec![0.0f32; width * height];
    for y in 0..height {
        let wy = hann(y, height);
        let row = &luma[y * width..(y + 1) * width];
        for kx in 0..width {
            let (mut sr, mut si) = (0.0f32, 0.0f32);
            for (x, v) in row.iter().enumerate() {
                let s = (v - mean) * wy * hann(x, width);
                let t = (kx * x) % width;
                sr += s * cos_w[t];
                si -= s * sin_w[t];
            }
            re[y * width + kx] = sr;
            im[y * width + kx] = si;
        }
    }
    // Columns: power[ky][kx].
    let (cos_h, sin_h) = twiddles(height);
    let freq = |k: usize, n: usize| {
        if 2 * k < n {
            k as f32 / n as f32
        } else {
            k as f32 / n as f32 - 1.0
        }
    };
    let (mut fine, mut lattice, mut checker, mut peak, mut bins) = (0.0f64, 0.0f64, 0.0f64, 0.0f32, 0usize);
    for kx in 0..width {
        let fx = freq(kx, width);
        for ky in 0..height {
            let fy = freq(ky, height);
            if fx.hypot(fy) < 0.25 {
                continue;
            }
            let (mut sr, mut si) = (0.0f32, 0.0f32);
            for y in 0..height {
                let t = (ky * y) % height;
                let (r, i) = (re[y * width + kx], im[y * width + kx]);
                sr += r * cos_h[t] + i * sin_h[t];
                si += i * cos_h[t] - r * sin_h[t];
            }
            let p = sr * sr + si * si;
            fine += f64::from(p);
            bins += 1;
            peak = peak.max(p);
            let near = |c: [f32; 2]| (fx - c[0]).hypot(fy - c[1]) < 0.03;
            if near(IGN_LATTICE) || near([-IGN_LATTICE[0], -IGN_LATTICE[1]]) {
                lattice += f64::from(p);
            }
            if fx.abs() >= 0.45 && fy.abs() >= 0.45 {
                checker += f64::from(p);
            }
        }
    }
    if fine <= 0.0 || bins == 0 {
        return zero;
    }
    StippleEnergy {
        lattice: (lattice / fine) as f32,
        checker: (checker / fine) as f32,
        peak_ratio: (f64::from(peak) * bins as f64 / fine) as f32,
    }
}

/// `cos` and `sin` of `2π t / n` for `t` in `0..n`.
fn twiddles(n: usize) -> (Vec<f32>, Vec<f32>) {
    (0..n)
        .map(|t| {
            let a = TAU * t as f32 / n as f32;
            (a.cos(), a.sin())
        })
        .unzip()
}

#[cfg(test)]
mod tests;
