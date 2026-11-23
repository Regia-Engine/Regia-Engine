// SPDX-License-Identifier: MPL-2.0
//! BC7 (BPTC unorm) texture block encoder: quality-oriented, deterministic, plain safe Rust.
//!
//! Original code written from the Khronos Data Format Specification 1.3, section "BPTC"
//! (the mode table, the bit layout, the two-subset partition table 114, the second-subset
//! anchor table 118 and the interpolation-weight table 119 are the specification's; the same
//! tables drive the runtime decoder in `regia_asset::bcn_decode`, and this encoder's tests
//! round-trip through that decoder, so the two cannot silently disagree). No code from any
//! other encoder was used; the fitting method is the standard one from the literature named
//! in `docs/provenance.md` (principal-axis line fit plus alternating least-squares refinement
//! in the style of Lloyd's least-squares quantization; the principal-axis start for block
//! endpoints follows Castaño's 2007 DXT compression report).
//!
//! # What is encoded
//!
//! One 4x4 RGBA block becomes 16 bytes. The encoder searches, per block, over the BC7 modes
//! it implements and keeps the candidate with the smallest weighted squared error measured
//! on texels decoded exactly as a GPU (and `regia_asset::bcn_decode`) would decode them:
//!
//! - Mode 6: one subset, 7-bit endpoints with a per-endpoint P-bit (8 effective bits),
//!   RGBA, 4-bit indices. The only mode at [`Bc7Quality::Fast`].
//! - Mode 1: two subsets, 6-bit endpoints with one shared P-bit per subset, RGB, 3-bit
//!   indices.
//! - Mode 3: two subsets, 7-bit endpoints with per-endpoint P-bits, RGB, 2-bit indices.
//! - Mode 7: two subsets, 5-bit endpoints with per-endpoint P-bits, RGBA, 2-bit indices.
//! - Mode 5: one subset, channel rotation, separate colour and alpha index sets (2 bits
//!   each), 7-bit colour and 8-bit alpha endpoints.
//! - Mode 4: one subset, channel rotation, separate colour and alpha index sets (2 and
//!   3 bits, assignable either way by the index-selection bit), 5-bit colour and 6-bit
//!   alpha endpoints.
//!
//! Modes 0 and 2 (three subsets) are not emitted; the table below still describes them so
//! the layout knowledge lives in one place next to the decoder's copy.
//!
//! # Method
//!
//! For each candidate subset the two endpoints start from a principal-axis (PCA) fit of the
//! subset's texels in the channel-weighted space, followed by a fixed number of
//! least-squares endpoint re-solves with the interpolation weights held fixed (the normal
//! equations for all channels share one 2x2 system). Endpoints are then quantized to the
//! mode's stored precision; the P-bit (per endpoint, or shared per subset in mode 1) is the
//! bit that minimises the weighted endpoint error, searching a small window of stored
//! values around the ideal for each channel. Per-texel indices are chosen by projecting
//! the texel onto the endpoint segment and testing the neighbouring discrete weights by
//! their true (integer, decoder-formula) error. One or more further quantized passes
//! re-solve the least-squares endpoints from the chosen indices and re-quantize, keeping
//! the best pass. Endpoint order per subset is flipped when that puts the subset's anchor
//! texel in the low half of the index range, which the anchor's shortened index field
//! requires; for the dual-index modes the colour and alpha orientations are tried
//! independently for the same reason.
//!
//! [`Bc7Quality::Normal`] pre-selects the few two-subset partitions with the smallest
//! within-subset variance and fully fits only those, for every partitioned mode.
//! [`Bc7Quality::High`] fits all 64 partitions. The number of quantized refinement passes
//! also grows with quality; pass counts only ever append snapshots and keep the best, so
//! for any block the full search is never worse than mode 6 alone (a test pins this).
//!
//! # Determinism and failure behaviour
//!
//! The encoder is a pure function of its input: fixed iteration counts, fixed tie-breaks
//! (lowest index, earliest candidate), no hash maps, no clocks, no randomness. Channel
//! weights that are negative or non-finite are replaced by 1.0, and an all-zero weight
//! vector falls back to all-ones, so no option value can make it panic. [`encode_image`]
//! returns an empty vector on a degenerate request (zero dimension or a buffer that does
//! not match `width * height * 4`); [`try_encode_image`] returns the typed error instead.
//! Partial edge blocks are padded by replicating the last row and column, exactly like the
//! crate's other block encoders.

use thiserror::Error;

/// How much of the BC7 mode space [`encode_block`] searches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Bc7Quality {
    /// Mode 6 only (one subset, 8-effective-bit RGBA endpoints, 4-bit indices).
    Fast,
    /// Modes 1, 3, 4, 5, 6 and 7; partitioned modes fully fit only the few partitions
    /// with the smallest within-subset variance. The cook's default trade-off.
    #[default]
    Normal,
    /// Modes 1, 3, 4, 5, 6 and 7 with all 64 two-subset partitions fitted, plus an extra
    /// quantized refinement pass.
    High,
}

/// Short alias for [`Bc7Quality`].
pub type Quality = Bc7Quality;

/// Options for [`encode_block`] and [`encode_image`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EncodeOptions {
    /// Which modes and partitions to search.
    pub quality: Bc7Quality,
    /// Per-channel squared-error weights `[r, g, b, a]` used both when fitting and when
    /// ranking candidates. A negative or non-finite entry is treated as 1.0; an all-zero
    /// vector is treated as all-ones. A single zero entry simply makes the encoder
    /// indifferent to that channel's error.
    pub channel_weights: [f32; 4],
}

impl Default for EncodeOptions {
    fn default() -> Self {
        Self {
            quality: Bc7Quality::Normal,
            channel_weights: [1.0; 4],
        }
    }
}

impl EncodeOptions {
    /// [`Bc7Quality::Fast`] with uniform channel weights.
    #[must_use]
    pub fn fast() -> Self {
        Self {
            quality: Bc7Quality::Fast,
            ..Self::default()
        }
    }

    /// [`Bc7Quality::Normal`] with uniform channel weights.
    #[must_use]
    pub fn normal() -> Self {
        Self::default()
    }

    /// [`Bc7Quality::High`] with uniform channel weights.
    #[must_use]
    pub fn high() -> Self {
        Self {
            quality: Bc7Quality::High,
            ..Self::default()
        }
    }

    /// This options value with different channel weights (see
    /// [`EncodeOptions::channel_weights`] for how degenerate weights are treated).
    #[must_use]
    pub fn with_channel_weights(mut self, channel_weights: [f32; 4]) -> Self {
        self.channel_weights = channel_weights;
        self
    }
}

/// Errors from [`try_encode_image`].
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum Bc7EncodeError {
    /// The image has a zero dimension.
    #[error("bc7 encode: image {width}x{height} has a zero dimension")]
    EmptyImage {
        /// Image width in texels.
        width: u32,
        /// Image height in texels.
        height: u32,
    },
    /// The pixel buffer is not `width * height * 4` bytes.
    #[error("bc7 encode: rgba buffer length {len} does not match {width}x{height}")]
    BadBuffer {
        /// Image width in texels.
        width: u32,
        /// Image height in texels.
        height: u32,
        /// Pixel buffer length in bytes.
        len: usize,
    },
    /// The image is too large for its block grid to be addressed on this platform.
    #[error("bc7 encode: image {width}x{height} is too large to encode")]
    TooLarge {
        /// Image width in texels.
        width: u32,
        /// Image height in texels.
        height: u32,
    },
}

/// BC7 two-subset partitions: bit `i` is the subset of texel `i` (Khronos DF 1.3 table 114).
/// Identical to the decoder's table in `regia_asset::bcn_decode`.
const P2: [u16; 64] = [
    0xcccc, 0x8888, 0xeeee, 0xecc8, 0xc880, 0xfeec, 0xfec8, 0xec80, 0xc800, 0xffec, 0xfe80, 0xe800, 0xffe8,
    0xff00, 0xfff0, 0xf000, 0xf710, 0x008e, 0x7100, 0x08ce, 0x008c, 0x7310, 0x3100, 0x8cce, 0x088c, 0x3110,
    0x6666, 0x366c, 0x17e8, 0x0ff0, 0x718e, 0x399c, 0xaaaa, 0xf0f0, 0x5a5a, 0x33cc, 0x3c3c, 0x55aa, 0x9696,
    0xa55a, 0x73ce, 0x13c8, 0x324c, 0x3bdc, 0x6996, 0xc33c, 0x9966, 0x0660, 0x0272, 0x04e4, 0x4e40, 0x2720,
    0xc936, 0x936c, 0x39c6, 0x639c, 0x9336, 0x9cc6, 0x817e, 0xe718, 0xccf0, 0x0fcc, 0x7744, 0xee22,
];

/// Anchor texel of the second subset of a two-subset partition (Khronos DF 1.3 table 118).
const A2: [u8; 64] = [
    15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 15, 2, 8, 2, 2, 8, 8, 15, 2, 8, 2, 2, 8,
    8, 2, 2, 15, 15, 6, 8, 2, 8, 15, 15, 2, 8, 2, 2, 2, 15, 15, 6, 6, 2, 6, 8, 15, 15, 2, 2, 15, 15, 15, 15,
    15, 2, 2, 15,
];

/// Interpolation weights for 2-bit indices (Khronos DF 1.3 table 119).
const W2: [u32; 4] = [0, 21, 43, 64];
/// Interpolation weights for 3-bit indices (table 119).
const W3: [u32; 8] = [0, 9, 18, 27, 37, 46, 55, 64];
/// Interpolation weights for 4-bit indices (table 119).
const W4: [u32; 16] = [0, 4, 9, 13, 17, 21, 26, 30, 34, 38, 43, 47, 51, 55, 60, 64];

/// The interpolation-weight table for an index field of `bits` bits.
fn weights(bits: u32) -> &'static [u32] {
    match bits {
        2 => &W2,
        3 => &W3,
        _ => &W4,
    }
}

/// Per-mode layout, field for field the decoder's table in `regia_asset::bcn_decode`:
/// subsets, partition bits, rotation bits, index-selection bits, colour bits, alpha bits,
/// per-endpoint P-bits, shared P-bits, index bits, secondary index bits.
#[derive(Clone, Copy)]
struct Bc7Mode {
    subsets: usize,
    partition_bits: u32,
    rotation_bits: u32,
    index_selection_bits: u32,
    color_bits: u32,
    alpha_bits: u32,
    endpoint_pbits: bool,
    shared_pbits: bool,
    index_bits: u32,
    index2_bits: u32,
}

/// All eight BC7 modes, in mode-number order. The encoder emits 1, 3, 4, 5, 6 and 7.
const MODES: [Bc7Mode; 8] = [
    Bc7Mode {
        subsets: 3,
        partition_bits: 4,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 4,
        alpha_bits: 0,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 3,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 6,
        alpha_bits: 0,
        endpoint_pbits: false,
        shared_pbits: true,
        index_bits: 3,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 3,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 5,
        alpha_bits: 0,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 0,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 2,
        index_selection_bits: 1,
        color_bits: 5,
        alpha_bits: 6,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 3,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 2,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 8,
        endpoint_pbits: false,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 2,
    },
    Bc7Mode {
        subsets: 1,
        partition_bits: 0,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 7,
        alpha_bits: 7,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 4,
        index2_bits: 0,
    },
    Bc7Mode {
        subsets: 2,
        partition_bits: 6,
        rotation_bits: 0,
        index_selection_bits: 0,
        color_bits: 5,
        alpha_bits: 5,
        endpoint_pbits: true,
        shared_pbits: false,
        index_bits: 2,
        index2_bits: 0,
    },
];

/// Modes the encoder emits, in the order candidates are considered (ties keep the
/// earliest candidate, so mode 6 wins an exact tie over every other mode).
const SEARCH_MODES: [usize; 6] = [6, 1, 3, 7, 5, 4];

/// Power-iteration steps for the principal axis. Fixed at every quality so that pass one
/// of a candidate is identical across qualities (see the module docs).
const PCA_ITERATIONS: usize = 12;
/// Float least-squares refinement steps before quantization, at every quality.
const FLOAT_REFINE_ITERATIONS: usize = 4;
/// Two-subset partitions fully fitted per partitioned mode at [`Bc7Quality::Normal`].
const NORMAL_PARTITIONS: usize = 6;

/// The subset of texel `texel` under `partition` for a mode with `subsets` subsets.
/// Only one- and two-subset modes are encoded; anything else is subset 0.
fn subset_of(subsets: usize, partition: usize, texel: usize) -> usize {
    if subsets == 2 {
        usize::from((P2[partition] >> texel) & 1 == 1)
    } else {
        0
    }
}

/// Whether texel `texel` is an anchor (its stored index loses its top bit): texel 0
/// always is, plus the second subset's anchor in a two-subset mode. Matches the decoder.
fn is_anchor(subsets: usize, partition: usize, texel: usize) -> bool {
    texel == 0 || (subsets == 2 && usize::from(A2[partition]) == texel)
}

/// The anchor texel of `subset` (the one texel of the subset with a shortened index).
fn anchor_texel(subsets: usize, partition: usize, subset: usize) -> usize {
    if subsets == 2 && subset == 1 {
        usize::from(A2[partition])
    } else {
        0
    }
}

/// Expands a `bits`-bit endpoint component to 8 bits by replicating its high bits, the
/// decoder's rule in `regia_asset::bcn_decode`.
fn expand(v: u32, bits: u32) -> u32 {
    if bits >= 8 {
        return v & 255;
    }
    (v << (8 - bits)) | (v >> (2 * bits).saturating_sub(8))
}

/// The decoder's rounded interpolation of one component: `((64 - w) * a + w * b + 32) >> 6`.
fn lerp_component(a: u32, b: u32, w: u32) -> u32 {
    ((64 - w) * a + w * b + 32) >> 6
}

/// Sanitizes user channel weights: a negative or non-finite entry becomes 1.0, and an
/// all-zero (or all-sanitized-to-zero, impossible here) vector becomes all-ones. Always
/// returns finite, non-negative weights that are not all zero.
fn sanitize_weights(weights: [f32; 4]) -> [f32; 4] {
    let clean = weights.map(|w| if w.is_finite() && w >= 0.0 { w } else { 1.0 });
    if clean.iter().all(|w| *w == 0.0) {
        [1.0; 4]
    } else {
        clean
    }
}

/// A fitted endpoint pair in float texel space (channels outside the fit mask hold the
/// subset mean and are never quantized from here).
#[derive(Clone, Copy)]
struct LineFit {
    e0: [f32; 4],
    e1: [f32; 4],
}

/// Clamps a float component to the texel range; a non-finite value becomes 0. Internal
/// values are derived from `u8` texels and sanitized weights, so this is belt and braces.
fn clamp_component(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 255.0) } else { 0.0 }
}

/// The discrete interpolation weight (as a fraction) for texel `p` projected onto the
/// segment `e0..e1`, in the channel-weighted metric over `mask` channels: the continuous
/// projection clamped to `[0, 1]`, snapped to the nearest entry of the `index_bits`
/// weight table (ties keep the lower weight).
fn projected_weight(
    p: [f32; 4],
    fit: LineFit,
    mask: [bool; 4],
    chan_weights: [f32; 4],
    index_bits: u32,
) -> f32 {
    let mut num = 0.0f32;
    let mut den = 0.0f32;
    for c in 0..4 {
        if mask[c] {
            let d = fit.e1[c] - fit.e0[c];
            num += chan_weights[c] * (p[c] - fit.e0[c]) * d;
            den += chan_weights[c] * d * d;
        }
    }
    let t = if den > 1e-12 {
        (num / den).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let table = weights(index_bits);
    let mut best = 0.0f32;
    let mut best_err = f32::MAX;
    for &w in table {
        let f = w as f32 / 64.0;
        let err = (f - t).abs();
        if err < best_err {
            best_err = err;
            best = f;
        }
    }
    best
}

/// Least-squares endpoint re-solve with the per-texel weights `ws` (aligned with `list`)
/// held fixed: minimise `sum |p - ((1 - w) e0 + w e1)|^2` over the masked channels. All
/// channels share the same 2x2 normal-equation coefficients. Returns `None` when the
/// system is (near-)singular, i.e. every weight is equal, so the caller keeps its fit.
fn ls_solve(
    working: &[[f32; 4]; 16],
    list: &[usize],
    ws: &[f32],
    mask: [bool; 4],
    fallback: LineFit,
) -> Option<LineFit> {
    let (mut a, mut b, mut c) = (0.0f32, 0.0f32, 0.0f32);
    let (mut x, mut y) = ([0.0f32; 4], [0.0f32; 4]);
    for (i, &t) in list.iter().enumerate() {
        let w = ws[i];
        let u = 1.0 - w;
        a += u * u;
        b += u * w;
        c += w * w;
        for ch in 0..4 {
            if mask[ch] {
                x[ch] += u * working[t][ch];
                y[ch] += w * working[t][ch];
            }
        }
    }
    let det = a * c - b * b;
    if det <= 1e-9 || !det.is_finite() {
        return None;
    }
    let mut fit = fallback;
    for ch in 0..4 {
        if mask[ch] {
            fit.e0[ch] = clamp_component((c * x[ch] - b * y[ch]) / det);
            fit.e1[ch] = clamp_component((a * y[ch] - b * x[ch]) / det);
        }
    }
    Some(fit)
}

/// Principal-axis fit of the texels in `list`, then [`FLOAT_REFINE_ITERATIONS`]
/// alternating projection / least-squares steps. The axis is the dominant eigenvector of
/// the channel-weighted covariance, found by power iteration from the texel farthest
/// from the mean (deterministic; ties keep the earliest texel). Initial endpoints are the
/// extreme projections on that axis.
fn principal_line_fit(
    working: &[[f32; 4]; 16],
    list: &[usize],
    mask: [bool; 4],
    chan_weights: [f32; 4],
    index_bits: u32,
) -> LineFit {
    let mut mean = [0.0f32; 4];
    for &t in list {
        for ch in 0..4 {
            mean[ch] += working[t][ch];
        }
    }
    let n = list.len().max(1) as f32;
    for m in &mut mean {
        *m /= n;
    }
    let degenerate = LineFit { e0: mean, e1: mean };
    if list.len() <= 1 {
        return degenerate;
    }
    // Channel scales of the weighted metric; a zero-weight channel still gets a small
    // scale so the axis stays defined and its endpoints stay near the data.
    let scale = chan_weights.map(|w| w.max(1e-4).sqrt());
    let scaled = |t: usize| -> [f32; 4] {
        let mut s = [0.0f32; 4];
        for ch in 0..4 {
            if mask[ch] {
                s[ch] = (working[t][ch] - mean[ch]) * scale[ch];
            }
        }
        s
    };
    let mut cov = [[0.0f32; 4]; 4];
    let mut dir = [0.0f32; 4];
    let mut far = -1.0f32;
    for &t in list {
        let s = scaled(t);
        for i in 0..4 {
            for j in 0..4 {
                cov[i][j] += s[i] * s[j];
            }
        }
        let d2: f32 = s.iter().map(|v| v * v).sum();
        if d2 > far {
            far = d2;
            dir = s;
        }
    }
    let norm = dir.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm <= 1e-9 || !norm.is_finite() {
        return degenerate;
    }
    for v in &mut dir {
        *v /= norm;
    }
    for _ in 0..PCA_ITERATIONS {
        let mut next = [0.0f32; 4];
        for i in 0..4 {
            for j in 0..4 {
                next[i] += cov[i][j] * dir[j];
            }
        }
        let m = next.iter().map(|v| v * v).sum::<f32>().sqrt();
        if m <= 1e-12 || !m.is_finite() {
            break;
        }
        for (d, v) in dir.iter_mut().zip(next) {
            *d = v / m;
        }
    }
    let (mut t_min, mut t_max) = (f32::MAX, f32::MIN);
    for &t in list {
        let s = scaled(t);
        let proj: f32 = s.iter().zip(dir).map(|(a, b)| a * b).sum();
        t_min = t_min.min(proj);
        t_max = t_max.max(proj);
    }
    let mut fit = degenerate;
    for ch in 0..4 {
        if mask[ch] {
            fit.e0[ch] = clamp_component(mean[ch] + dir[ch] * t_min / scale[ch]);
            fit.e1[ch] = clamp_component(mean[ch] + dir[ch] * t_max / scale[ch]);
        }
    }
    let mut ws = [0.0f32; 16];
    for _ in 0..FLOAT_REFINE_ITERATIONS {
        for (i, &t) in list.iter().enumerate() {
            ws[i] = projected_weight(working[t], fit, mask, chan_weights, index_bits);
        }
        match ls_solve(working, list, &ws[..list.len()], mask, fit) {
            Some(next) => fit = next,
            None => break,
        }
    }
    fit
}

/// A quantized endpoint pair: the stored (pre-P-bit) component values, the per-endpoint
/// P-bits (duplicated when the mode shares one P-bit over the pair), and the expanded
/// 8-bit endpoints a decoder would produce.
#[derive(Clone, Copy)]
struct QuantPair {
    stored: [[u32; 4]; 2],
    pbits: [u32; 2],
    decoded: [[u32; 4]; 2],
}

/// The stored precision of channel `ch` in `desc` (colour bits for RGB, alpha bits for A).
fn channel_bits(desc: Bc7Mode, ch: usize) -> u32 {
    if ch < 3 { desc.color_bits } else { desc.alpha_bits }
}

/// The best stored value for one ideal component at `bits` stored bits; `pbit` is the
/// fixed P-bit when the mode has one (the compared value is then `(stored << 1) | pbit`
/// at `bits + 1` precision). Searches a small window around the ideal's nearest value;
/// the expansion is monotonic, so the optimum is inside it. Returns
/// `(stored, expanded)`, ties keeping the smaller stored value.
fn best_stored(ideal: f32, bits: u32, pbit: Option<u32>) -> (u32, u32) {
    let ideal = clamp_component(ideal);
    let (mut best_stored, mut best_decoded, mut best_err) = (0u32, 0u32, f32::MAX);
    match pbit {
        None => {
            let max_v = (1i32 << bits) - 1;
            let approx = (ideal / 255.0 * max_v as f32).round() as i32;
            for v in (approx - 4)..=(approx + 4) {
                if (0..=max_v).contains(&v) {
                    let decoded = expand(v as u32, bits);
                    let err = (decoded as f32 - ideal).abs();
                    if err < best_err {
                        (best_stored, best_decoded, best_err) = (v as u32, decoded, err);
                    }
                }
            }
        }
        Some(p) => {
            let combined_bits = bits + 1;
            let max_v = (1i32 << combined_bits) - 1;
            let approx = (ideal / 255.0 * max_v as f32).round() as i32;
            for v in (approx - 8)..=(approx + 8) {
                if (0..=max_v).contains(&v) && (v as u32 & 1) == p {
                    let decoded = expand(v as u32, combined_bits);
                    let err = (decoded as f32 - ideal).abs();
                    if err < best_err {
                        (best_stored, best_decoded, best_err) = ((v as u32) >> 1, decoded, err);
                    }
                }
            }
        }
    }
    (best_stored, best_decoded)
}

/// Quantizes one endpoint under a fixed P-bit choice (`None` when the mode has no
/// P-bits), returning its stored and expanded components. Channels outside `mask` get
/// stored 0 and expanded 255 for an absent alpha channel, 0 otherwise (an unmasked
/// colour channel only occurs in the dual-index alpha fit, where it is discarded).
fn quantize_endpoint(
    ideal: [f32; 4],
    desc: Bc7Mode,
    mask: [bool; 4],
    pbit: Option<u32>,
) -> ([u32; 4], [u32; 4]) {
    let (mut stored, mut decoded) = ([0u32; 4], [0u32; 4]);
    for ch in 0..4 {
        if mask[ch] {
            let (s, d) = best_stored(ideal[ch], channel_bits(desc, ch), pbit);
            stored[ch] = s;
            decoded[ch] = d;
        } else if ch == 3 && desc.alpha_bits == 0 {
            decoded[ch] = 255;
        }
    }
    (stored, decoded)
}

/// Weighted squared error between an ideal float endpoint and its expansion.
fn endpoint_error(ideal: [f32; 4], decoded: [u32; 4], mask: [bool; 4], chan_weights: [f32; 4]) -> f32 {
    let mut err = 0.0f32;
    for ch in 0..4 {
        if mask[ch] {
            let d = decoded[ch] as f32 - clamp_component(ideal[ch]);
            err += chan_weights[ch] * d * d;
        }
    }
    err
}

/// Quantizes an endpoint pair, choosing P-bits by weighted endpoint error: per endpoint
/// for `endpoint_pbits` modes, one shared bit for `shared_pbits` modes (mode 1), and no
/// bit otherwise. Ties keep P-bit 0.
fn quantize_pair(fit: LineFit, desc: Bc7Mode, mask: [bool; 4], chan_weights: [f32; 4]) -> QuantPair {
    let ideals = [fit.e0, fit.e1];
    if desc.shared_pbits {
        let mut best: Option<(f32, QuantPair)> = None;
        for p in 0..2u32 {
            let (s0, d0) = quantize_endpoint(ideals[0], desc, mask, Some(p));
            let (s1, d1) = quantize_endpoint(ideals[1], desc, mask, Some(p));
            let err = endpoint_error(ideals[0], d0, mask, chan_weights)
                + endpoint_error(ideals[1], d1, mask, chan_weights);
            let pair = QuantPair {
                stored: [s0, s1],
                pbits: [p, p],
                decoded: [d0, d1],
            };
            if best.as_ref().is_none_or(|(e, _)| err < *e) {
                best = Some((err, pair));
            }
        }
        match best {
            Some((_, pair)) => pair,
            None => QuantPair {
                stored: [[0; 4]; 2],
                pbits: [0; 2],
                decoded: [[0; 4]; 2],
            },
        }
    } else if desc.endpoint_pbits {
        let mut pair = QuantPair {
            stored: [[0; 4]; 2],
            pbits: [0; 2],
            decoded: [[0; 4]; 2],
        };
        for (e, ideal) in ideals.iter().enumerate() {
            let mut chosen: Option<(f32, u32, [u32; 4], [u32; 4])> = None;
            for p in 0..2u32 {
                let (s, d) = quantize_endpoint(*ideal, desc, mask, Some(p));
                let err = endpoint_error(*ideal, d, mask, chan_weights);
                if chosen.as_ref().is_none_or(|(e0, _, _, _)| err < *e0) {
                    chosen = Some((err, p, s, d));
                }
            }
            if let Some((_, p, s, d)) = chosen {
                pair.stored[e] = s;
                pair.decoded[e] = d;
                pair.pbits[e] = p;
            }
        }
        pair
    } else {
        let (s0, d0) = quantize_endpoint(ideals[0], desc, mask, None);
        let (s1, d1) = quantize_endpoint(ideals[1], desc, mask, None);
        QuantPair {
            stored: [s0, s1],
            pbits: [0; 2],
            decoded: [d0, d1],
        }
    }
}

/// Assigns an index to every texel in `list` (written aligned with `list`) against the
/// expanded endpoint pair `decoded`, returning the weighted squared error over `mask`
/// channels measured with the decoder's integer interpolation. The index starts from the
/// texel's projection onto the segment and tests the neighbouring weights by true error.
/// `anchor_pos` is the position in `list` of the subset's anchor texel, whose stored
/// index loses its top bit: it is restricted to the low half of the range and searched
/// exhaustively over that half.
fn assign_indices(
    working: &[[f32; 4]; 16],
    list: &[usize],
    decoded: [[u32; 4]; 2],
    index_bits: u32,
    mask: [bool; 4],
    chan_weights: [f32; 4],
    anchor_pos: Option<usize>,
) -> ([u32; 16], f64) {
    let table = weights(index_bits);
    let max_index = table.len() - 1;
    let half = 1usize << (index_bits - 1);
    let float_pair = LineFit {
        e0: decoded[0].map(|v| v as f32),
        e1: decoded[1].map(|v| v as f32),
    };
    let mut out = [0u32; 16];
    let mut total = 0.0f64;
    for (pos, &t) in list.iter().enumerate() {
        let target = working[t];
        let guess_w = projected_weight(target, float_pair, mask, chan_weights, index_bits);
        let mut guess = 0usize;
        let mut guess_err = f32::MAX;
        for (k, &w) in table.iter().enumerate() {
            let err = (w as f32 / 64.0 - guess_w).abs();
            if err < guess_err {
                guess_err = err;
                guess = k;
            }
        }
        let (lo, hi) = if anchor_pos == Some(pos) {
            (0usize, half - 1)
        } else {
            (guess.saturating_sub(2), (guess + 2).min(max_index))
        };
        let (mut best_k, mut best_err) = (lo, f64::MAX);
        for (k, &w) in table.iter().enumerate().take(hi + 1).skip(lo) {
            let mut err = 0.0f64;
            for ch in 0..4 {
                if mask[ch] {
                    let v = lerp_component(decoded[0][ch], decoded[1][ch], w) as f64;
                    let d = v - f64::from(target[ch].clamp(0.0, 255.0));
                    err += f64::from(chan_weights[ch]) * d * d;
                }
            }
            if err < best_err {
                (best_k, best_err) = (k, err);
            }
        }
        out[pos] = best_k as u32;
        total += best_err;
    }
    (out, total)
}

/// One fully specified BC7 block before bit packing.
#[derive(Clone, Copy)]
struct Candidate {
    mode: usize,
    partition: usize,
    rotation: u32,
    index_selection: u32,
    /// Stored (pre-P-bit) endpoint components, `[endpoint][channel]`; at most 6 endpoints.
    stored: [[u32; 4]; 6],
    /// Per-endpoint P-bits (a shared P-bit is duplicated over its two endpoints).
    pbits: [u32; 6],
    index: [u32; 16],
    index2: [u32; 16],
}

impl Candidate {
    /// A zeroed candidate for `mode` (endpoints and indices filled in by the builders).
    fn new(mode: usize, partition: usize, rotation: u32, index_selection: u32) -> Self {
        Self {
            mode,
            partition,
            rotation,
            index_selection,
            stored: [[0; 4]; 6],
            pbits: [0; 6],
            index: [0; 16],
            index2: [0; 16],
        }
    }
}

/// The endpoint pair with its two endpoints exchanged.
fn swapped(pair: QuantPair) -> QuantPair {
    QuantPair {
        stored: [pair.stored[1], pair.stored[0]],
        pbits: [pair.pbits[1], pair.pbits[0]],
        decoded: [pair.decoded[1], pair.decoded[0]],
    }
}

/// A quantized pair as a float line (the least-squares fallback in its orientation).
fn pair_float(pair: QuantPair) -> LineFit {
    LineFit {
        e0: pair.decoded[0].map(|v| v as f32),
        e1: pair.decoded[1].map(|v| v as f32),
    }
}

/// The result of fitting one subset of a single-index mode.
struct SubsetResult {
    pair: QuantPair,
    /// Per-texel indices aligned with the subset's texel list.
    indices: [u32; 16],
    error: f64,
}

/// Fits one subset of a single-index mode: principal-axis fit, then `passes` quantized
/// passes (quantize, assign indices in both endpoint orientations under the anchor
/// restriction, re-solve least squares), keeping the best pass by true decoded error.
/// Pass one does not depend on `passes`, so a larger pass count never does worse.
fn fit_subset(
    working: &[[f32; 4]; 16],
    list: &[usize],
    desc: Bc7Mode,
    mask: [bool; 4],
    chan_weights: [f32; 4],
    anchor: usize,
    passes: usize,
) -> SubsetResult {
    let anchor_pos = list.iter().position(|&t| t == anchor);
    let mut fit = principal_line_fit(working, list, mask, chan_weights, desc.index_bits);
    let mut best: Option<SubsetResult> = None;
    for pass in 0..passes {
        let pair = quantize_pair(fit, desc, mask, chan_weights);
        let mut pass_best: Option<SubsetResult> = None;
        for flip in [false, true] {
            let oriented = if flip { swapped(pair) } else { pair };
            let (indices, error) = assign_indices(
                working,
                list,
                oriented.decoded,
                desc.index_bits,
                mask,
                chan_weights,
                anchor_pos,
            );
            let result = SubsetResult {
                pair: oriented,
                indices,
                error,
            };
            if pass_best.as_ref().is_none_or(|b| error < b.error) {
                pass_best = Some(result);
            }
        }
        if let Some(result) = pass_best {
            if pass + 1 < passes {
                let table = weights(desc.index_bits);
                let mut ws = [0.0f32; 16];
                for (i, &k) in result.indices[..list.len()].iter().enumerate() {
                    ws[i] = table[k as usize] as f32 / 64.0;
                }
                let fallback = pair_float(result.pair);
                fit = ls_solve(working, list, &ws[..list.len()], mask, fallback).unwrap_or(fallback);
            }
            if best.as_ref().is_none_or(|b| result.error < b.error) {
                best = Some(result);
            }
        }
    }
    match best {
        Some(result) => result,
        None => SubsetResult {
            pair: quantize_pair(fit, desc, mask, chan_weights),
            indices: [0; 16],
            error: f64::MAX,
        },
    }
}

/// Texels as floats in the rotated working space for `rotation` (the decoder applies the
/// same channel/alpha swap after interpolation, and the swap is its own inverse).
fn working_texels(texels: &[[u8; 4]; 16], rotation: u32) -> [[f32; 4]; 16] {
    (*texels).map(|px| {
        let mut w = px;
        match rotation {
            1 => w.swap(0, 3),
            2 => w.swap(1, 3),
            3 => w.swap(2, 3),
            _ => {}
        }
        w.map(f32::from)
    })
}

/// Channel weights permuted into the rotated working space for `rotation`.
fn working_weights(chan_weights: [f32; 4], rotation: u32) -> [f32; 4] {
    let mut w = chan_weights;
    match rotation {
        1 => w.swap(0, 3),
        2 => w.swap(1, 3),
        3 => w.swap(2, 3),
        _ => {}
    }
    w
}

/// Builds a candidate for a single-index mode (1, 3, 6 or 7) under `partition`.
fn build_single_candidate(
    texels: &[[u8; 4]; 16],
    mode: usize,
    partition: usize,
    chan_weights: [f32; 4],
    passes: usize,
) -> Candidate {
    let desc = MODES[mode];
    let working = working_texels(texels, 0);
    let mask = if desc.alpha_bits > 0 {
        [true, true, true, true]
    } else {
        [true, true, true, false]
    };
    let mut candidate = Candidate::new(mode, partition, 0, 0);
    for subset in 0..desc.subsets {
        let mut list = [0usize; 16];
        let mut count = 0usize;
        for t in 0..16 {
            if subset_of(desc.subsets, partition, t) == subset {
                list[count] = t;
                count += 1;
            }
        }
        if count == 0 {
            continue;
        }
        let list = &list[..count];
        let anchor = anchor_texel(desc.subsets, partition, subset);
        let result = fit_subset(&working, list, desc, mask, chan_weights, anchor, passes);
        candidate.stored[2 * subset] = result.pair.stored[0];
        candidate.stored[2 * subset + 1] = result.pair.stored[1];
        candidate.pbits[2 * subset] = result.pair.pbits[0];
        candidate.pbits[2 * subset + 1] = result.pair.pbits[1];
        for (pos, &t) in list.iter().enumerate() {
            candidate.index[t] = result.indices[pos];
        }
    }
    candidate
}

/// One pass's snapshot of a dual-index candidate (modes 4 and 5).
struct DualSnapshot {
    stored: [[u32; 4]; 2],
    color_index: [u32; 16],
    alpha_index: [u32; 16],
    error: f64,
}

/// Builds a candidate for a dual-index mode (4 or 5) under a channel `rotation` and an
/// `index_selection` (mode 4 only; mode 5 stores no selection bit and always uses 0).
/// Colour (in rotated space) and alpha are separate line fits sharing one endpoint pair;
/// their relative orientation is free, so all four colour/alpha orientation combinations
/// are tried and the anchor restriction (texel 0, for both index sets) picks between them.
fn build_dual_candidate(
    texels: &[[u8; 4]; 16],
    mode: usize,
    rotation: u32,
    index_selection: u32,
    chan_weights: [f32; 4],
    passes: usize,
) -> Candidate {
    let desc = MODES[mode];
    let working = working_texels(texels, rotation);
    let wrot = working_weights(chan_weights, rotation);
    let (color_bits, alpha_bits) = if index_selection == 0 {
        (desc.index_bits, desc.index2_bits)
    } else {
        (desc.index2_bits, desc.index_bits)
    };
    let list: [usize; 16] = std::array::from_fn(|i| i);
    let color_mask = [true, true, true, false];
    let alpha_mask = [false, false, false, true];
    let mut color_fit = principal_line_fit(&working, &list, color_mask, wrot, color_bits);
    let mut alpha_fit = principal_line_fit(&working, &list, alpha_mask, wrot, alpha_bits);
    let mut best: Option<DualSnapshot> = None;
    for pass in 0..passes {
        let cq = quantize_pair(color_fit, desc, color_mask, wrot);
        let aq = quantize_pair(alpha_fit, desc, alpha_mask, wrot);
        let mut pass_best: Option<(DualSnapshot, usize, usize)> = None;
        for cf in 0..2usize {
            for af in 0..2usize {
                let decoded = [
                    [
                        cq.decoded[cf][0],
                        cq.decoded[cf][1],
                        cq.decoded[cf][2],
                        aq.decoded[af][3],
                    ],
                    [
                        cq.decoded[1 - cf][0],
                        cq.decoded[1 - cf][1],
                        cq.decoded[1 - cf][2],
                        aq.decoded[1 - af][3],
                    ],
                ];
                let (color_index, color_error) =
                    assign_indices(&working, &list, decoded, color_bits, color_mask, wrot, Some(0));
                let (alpha_index, alpha_error) =
                    assign_indices(&working, &list, decoded, alpha_bits, alpha_mask, wrot, Some(0));
                let error = color_error + alpha_error;
                let snapshot = DualSnapshot {
                    stored: [
                        [
                            cq.stored[cf][0],
                            cq.stored[cf][1],
                            cq.stored[cf][2],
                            aq.stored[af][3],
                        ],
                        [
                            cq.stored[1 - cf][0],
                            cq.stored[1 - cf][1],
                            cq.stored[1 - cf][2],
                            aq.stored[1 - af][3],
                        ],
                    ],
                    color_index,
                    alpha_index,
                    error,
                };
                if pass_best.as_ref().is_none_or(|(b, _, _)| error < b.error) {
                    pass_best = Some((snapshot, cf, af));
                }
            }
        }
        if let Some((snapshot, cf, af)) = pass_best {
            if pass + 1 < passes {
                let (ctable, atable) = (weights(color_bits), weights(alpha_bits));
                let mut cws = [0.0f32; 16];
                let mut aws = [0.0f32; 16];
                for i in 0..16 {
                    cws[i] = ctable[snapshot.color_index[i] as usize] as f32 / 64.0;
                    aws[i] = atable[snapshot.alpha_index[i] as usize] as f32 / 64.0;
                }
                let color_fallback = LineFit {
                    e0: cq.decoded[cf].map(|v| v as f32),
                    e1: cq.decoded[1 - cf].map(|v| v as f32),
                };
                let alpha_fallback = LineFit {
                    e0: aq.decoded[af].map(|v| v as f32),
                    e1: aq.decoded[1 - af].map(|v| v as f32),
                };
                color_fit =
                    ls_solve(&working, &list, &cws, color_mask, color_fallback).unwrap_or(color_fallback);
                alpha_fit =
                    ls_solve(&working, &list, &aws, alpha_mask, alpha_fallback).unwrap_or(alpha_fallback);
            }
            if best.as_ref().is_none_or(|b| snapshot.error < b.error) {
                best = Some(snapshot);
            }
        }
    }
    let mut candidate = Candidate::new(mode, 0, rotation, index_selection);
    if let Some(snapshot) = best {
        candidate.stored[0] = snapshot.stored[0];
        candidate.stored[1] = snapshot.stored[1];
        if index_selection == 0 {
            candidate.index = snapshot.color_index;
            candidate.index2 = snapshot.alpha_index;
        } else {
            candidate.index = snapshot.alpha_index;
            candidate.index2 = snapshot.color_index;
        }
    }
    candidate
}

/// The expanded 8-bit endpoints a decoder derives from a candidate's stored values and
/// P-bits, mirroring `regia_asset::bcn_decode` exactly.
fn expanded_endpoints(desc: Bc7Mode, stored: [[u32; 4]; 6], pbits: [u32; 6]) -> [[u32; 4]; 6] {
    let has_p = desc.endpoint_pbits || desc.shared_pbits;
    let color_bits = desc.color_bits + u32::from(has_p);
    let alpha_bits = desc.alpha_bits + u32::from(has_p && desc.alpha_bits > 0);
    let mut ep = [[0u32; 4]; 6];
    for e in 0..desc.subsets * 2 {
        for ch in 0..3 {
            let mut v = stored[e][ch];
            if has_p {
                v = (v << 1) | pbits[e];
            }
            ep[e][ch] = expand(v, color_bits);
        }
        ep[e][3] = if desc.alpha_bits > 0 {
            let mut v = stored[e][3];
            if has_p {
                v = (v << 1) | pbits[e];
            }
            expand(v, alpha_bits)
        } else {
            255
        };
    }
    ep
}

/// Decodes a candidate to texels with the decoder's integer arithmetic (subset lookup,
/// weight selection including dual-index swap, then channel rotation).
fn render_candidate(candidate: &Candidate) -> [[u8; 4]; 16] {
    let desc = MODES[candidate.mode];
    let ep = expanded_endpoints(desc, candidate.stored, candidate.pbits);
    std::array::from_fn(|t| {
        let s = subset_of(desc.subsets, candidate.partition, t);
        let (e0, e1) = (ep[2 * s], ep[2 * s + 1]);
        let (cw, aw) = if desc.index2_bits == 0 {
            let w = weights(desc.index_bits)[candidate.index[t] as usize];
            (w, w)
        } else if candidate.index_selection == 0 {
            (
                weights(desc.index_bits)[candidate.index[t] as usize],
                weights(desc.index2_bits)[candidate.index2[t] as usize],
            )
        } else {
            (
                weights(desc.index2_bits)[candidate.index2[t] as usize],
                weights(desc.index_bits)[candidate.index[t] as usize],
            )
        };
        let mut px = [
            lerp_component(e0[0], e1[0], cw) as u8,
            lerp_component(e0[1], e1[1], cw) as u8,
            lerp_component(e0[2], e1[2], cw) as u8,
            lerp_component(e0[3], e1[3], aw) as u8,
        ];
        match candidate.rotation {
            1 => px.swap(0, 3),
            2 => px.swap(1, 3),
            3 => px.swap(2, 3),
            _ => {}
        }
        px
    })
}

/// Weighted squared error between the source texels and a decoded block.
fn weighted_sse(texels: &[[u8; 4]; 16], decoded: &[[u8; 4]; 16], chan_weights: [f32; 4]) -> f64 {
    let mut total = 0.0f64;
    for (a, b) in texels.iter().zip(decoded) {
        for ch in 0..4 {
            let d = f64::from(a[ch]) - f64::from(b[ch]);
            total += f64::from(chan_weights[ch]) * d * d;
        }
    }
    total
}

/// LSB-first bit writer over one 128-bit block, the exact inverse of the decoder's
/// reader in `regia_asset::bcn_decode`. Values are masked to their field width so a
/// stray out-of-range value can never corrupt a neighbouring field.
struct BitWriter {
    value: u128,
    at: u32,
}

impl BitWriter {
    fn put(&mut self, v: u32, n: u32) {
        if n == 0 {
            return;
        }
        let mask = (1u128 << n) - 1;
        self.value |= (u128::from(v) & mask) << self.at;
        self.at += n;
    }
}

/// Packs a candidate into its 16 bytes, in the decoder's field order: mode bits (the
/// first set bit names the mode), partition, rotation, index selection, endpoint colour
/// components channel-major, endpoint alpha components, P-bits, primary indices (anchor
/// texels one bit short), secondary indices (texel 0 one bit short).
fn write_candidate(candidate: &Candidate) -> [u8; 16] {
    let desc = MODES[candidate.mode];
    let mut bits = BitWriter {
        value: 1u128 << candidate.mode,
        at: candidate.mode as u32 + 1,
    };
    bits.put(candidate.partition as u32, desc.partition_bits);
    bits.put(candidate.rotation, desc.rotation_bits);
    bits.put(candidate.index_selection, desc.index_selection_bits);
    let endpoints = desc.subsets * 2;
    for ch in 0..3 {
        for &stored in &candidate.stored[..endpoints] {
            bits.put(stored[ch], desc.color_bits);
        }
    }
    for &stored in &candidate.stored[..endpoints] {
        bits.put(stored[3], desc.alpha_bits);
    }
    if desc.endpoint_pbits {
        for &pbit in &candidate.pbits[..endpoints] {
            bits.put(pbit, 1);
        }
    } else if desc.shared_pbits {
        for s in 0..desc.subsets {
            bits.put(candidate.pbits[2 * s], 1);
        }
    }
    for (t, &ix) in candidate.index.iter().enumerate() {
        let n = desc.index_bits - u32::from(is_anchor(desc.subsets, candidate.partition, t));
        bits.put(ix, n);
    }
    if desc.index2_bits > 0 {
        for (t, &ix) in candidate.index2.iter().enumerate() {
            bits.put(ix, desc.index2_bits - u32::from(t == 0));
        }
    }
    debug_assert_eq!(bits.at, 128, "mode {} must fill the block", candidate.mode);
    bits.value.to_le_bytes()
}

/// The number of quantized refinement passes for a quality level.
fn passes_for(quality: Bc7Quality) -> usize {
    match quality {
        Bc7Quality::Fast => 1,
        Bc7Quality::Normal => 2,
        Bc7Quality::High => 3,
    }
}

/// The `limit` two-subset partitions with the smallest weighted within-subset variance
/// (the Normal shortlist), best first; ties keep the lower partition number. Alpha joins
/// the score only when the block is not fully opaque, since modes 1 and 3 cannot spend
/// bits on it.
fn partition_shortlist(texels: &[[u8; 4]; 16], chan_weights: [f32; 4], limit: usize) -> Vec<usize> {
    let use_alpha = texels.iter().any(|px| px[3] != 255);
    let mask = [true, true, true, use_alpha];
    let mut scored: [(f64, usize); 64] = std::array::from_fn(|p| {
        let (mut sums, mut counts) = ([[0.0f64; 4]; 2], [0.0f64; 2]);
        for (t, px) in texels.iter().enumerate() {
            let s = subset_of(2, p, t);
            counts[s] += 1.0;
            for ch in 0..4 {
                if mask[ch] {
                    sums[s][ch] += f64::from(px[ch]);
                }
            }
        }
        let mut means = [[0.0f64; 4]; 2];
        for s in 0..2 {
            if counts[s] > 0.0 {
                for ch in 0..4 {
                    means[s][ch] = sums[s][ch] / counts[s];
                }
            }
        }
        let mut score = 0.0f64;
        for (t, px) in texels.iter().enumerate() {
            let s = subset_of(2, p, t);
            for ch in 0..4 {
                if mask[ch] {
                    let d = f64::from(px[ch]) - means[s][ch];
                    score += f64::from(chan_weights[ch]) * d * d;
                }
            }
        }
        (score, p)
    });
    scored.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.iter().take(limit.min(64)).map(|&(_, p)| p).collect()
}

/// A mode-6 block holding one solid colour as closely as its endpoints allow (within one
/// level a channel: an endpoint's single P-bit gives every channel the same parity).
/// The fallback when the search somehow produced no candidate.
fn solid_block(color: [u8; 4]) -> [u8; 16] {
    let desc = MODES[6];
    let ideal = color.map(f32::from);
    let pair = quantize_pair(
        LineFit { e0: ideal, e1: ideal },
        desc,
        [true, true, true, true],
        [1.0; 4],
    );
    let mut candidate = Candidate::new(6, 0, 0, 0);
    candidate.stored[0] = pair.stored[0];
    candidate.stored[1] = pair.stored[1];
    candidate.pbits[0] = pair.pbits[0];
    candidate.pbits[1] = pair.pbits[1];
    write_candidate(&candidate)
}

#[cfg(test)]
thread_local! {
    /// Test-only work counter: candidates ranked by [`encode_block`] on this thread (the
    /// deterministic stand-in for a wall-clock speed budget).
    static RANKED_CANDIDATES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Encodes one 4x4 RGBA block (row-major texels) into its 16 BC7 bytes, searching the
/// modes `opts.quality` selects and keeping the candidate with the smallest weighted
/// squared error (see the module docs for the method and the mode list). Deterministic:
/// the same input always yields the same bytes.
#[must_use]
pub fn encode_block(texels: &[[u8; 4]; 16], opts: &EncodeOptions) -> [u8; 16] {
    let chan_weights = sanitize_weights(opts.channel_weights);
    let passes = passes_for(opts.quality);
    let mut best: Option<(f64, Candidate)> = None;
    let consider = |candidate: Candidate, best: &mut Option<(f64, Candidate)>| {
        #[cfg(test)]
        RANKED_CANDIDATES.with(|n| n.set(n.get() + 1));
        let error = weighted_sse(texels, &render_candidate(&candidate), chan_weights);
        if best.as_ref().is_none_or(|(e, _)| error < *e) {
            *best = Some((error, candidate));
        }
    };
    consider(
        build_single_candidate(texels, 6, 0, chan_weights, passes),
        &mut best,
    );
    if opts.quality != Bc7Quality::Fast && !best.as_ref().is_some_and(|(e, _)| *e == 0.0) {
        let partitions: Vec<usize> = match opts.quality {
            Bc7Quality::High => (0..64).collect(),
            _ => partition_shortlist(texels, chan_weights, NORMAL_PARTITIONS),
        };
        for &mode in &SEARCH_MODES {
            match mode {
                6 => {}
                1 | 3 | 7 => {
                    for &partition in &partitions {
                        consider(
                            build_single_candidate(texels, mode, partition, chan_weights, passes),
                            &mut best,
                        );
                    }
                }
                5 => {
                    for rotation in 0..4 {
                        consider(
                            build_dual_candidate(texels, 5, rotation, 0, chan_weights, passes),
                            &mut best,
                        );
                    }
                }
                _ => {
                    for rotation in 0..4 {
                        for selection in 0..2 {
                            consider(
                                build_dual_candidate(texels, 4, rotation, selection, chan_weights, passes),
                                &mut best,
                            );
                        }
                    }
                }
            }
            if best.as_ref().is_some_and(|(e, _)| *e == 0.0) {
                break;
            }
        }
    }
    match best {
        Some((_, candidate)) => write_candidate(&candidate),
        None => solid_block(texels[0]),
    }
}

/// Encodes a `width` x `height` RGBA8 image (row-major, tightly packed) into BC7 blocks:
/// `ceil(width / 4) * ceil(height / 4)` blocks of 16 bytes, block rows top to bottom.
/// Partial edge blocks are padded by replicating the last row and column of texels.
///
/// A degenerate request (a zero dimension, or a buffer that is not
/// `width * height * 4` bytes) yields an empty vector; use [`try_encode_image`] to get
/// the typed [`Bc7EncodeError`] instead. Never panics.
#[must_use]
pub fn encode_image(rgba: &[u8], width: u32, height: u32, opts: &EncodeOptions) -> Vec<u8> {
    try_encode_image(rgba, width, height, opts).unwrap_or_default()
}

/// Fallible form of [`encode_image`].
///
/// # Errors
///
/// [`Bc7EncodeError::EmptyImage`] for a zero dimension,
/// [`Bc7EncodeError::BadBuffer`] when `rgba.len() != width * height * 4`, and
/// [`Bc7EncodeError::TooLarge`] when the block grid cannot be addressed.
pub fn try_encode_image(
    rgba: &[u8],
    width: u32,
    height: u32,
    opts: &EncodeOptions,
) -> Result<Vec<u8>, Bc7EncodeError> {
    if width == 0 || height == 0 {
        return Err(Bc7EncodeError::EmptyImage { width, height });
    }
    let expected = u128::from(width) * u128::from(height) * 4;
    if expected != rgba.len() as u128 {
        return Err(Bc7EncodeError::BadBuffer {
            width,
            height,
            len: rgba.len(),
        });
    }
    let (bw, bh) = (width.div_ceil(4), height.div_ceil(4));
    let total = u128::from(bw) * u128::from(bh) * 16;
    let total = usize::try_from(total).map_err(|_| Bc7EncodeError::TooLarge { width, height })?;
    let mut out = Vec::with_capacity(total);
    for by in 0..bh {
        for bx in 0..bw {
            let mut block = [[0u8; 4]; 16];
            for y in 0..4u32 {
                for x in 0..4u32 {
                    let px = (bx * 4 + x).min(width - 1);
                    let py = (by * 4 + y).min(height - 1);
                    let i = (py as usize * width as usize + px as usize) * 4;
                    block[(y * 4 + x) as usize] = [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]];
                }
            }
            out.extend_from_slice(&encode_block(&block, opts));
        }
    }
    Ok(out)
}

/// The BC7 mode number stored in a block: the position of the first set low bit, exactly
/// as a decoder reads it. `None` for the reserved all-zero mode field, which decodes as
/// transparent black and which this encoder never emits.
#[must_use]
pub fn block_mode(block: &[u8; 16]) -> Option<u8> {
    let value = u128::from_le_bytes(*block);
    (0u8..8).find(|&m| (value >> u32::from(m)) & 1 == 1)
}

/// Decodes one BC7 block into its 16 row-major RGBA8 texels, via the crate's reference
/// decoder (`regia_asset::bcn_decode`, the same decoder the DDS importer and the
/// renderer's CPU fallback use). Provided so callers and tests can round-trip without
/// depending on `regia_asset` directly.
#[must_use]
pub fn decode_block(block: &[u8; 16]) -> [[u8; 4]; 16] {
    regia_asset::bcn_decode::decode_block(regia_asset::bcn_decode::BcnKind::Bc7, block)
}

#[cfg(test)]
mod tests {
    use super::*;
    use regia_asset::bcn_decode::{BcnKind, decode_bcn};

    /// Peak signal-to-noise ratio over all channels; an exact round-trip reports 99 dB.
    fn psnr(original: &[u8], decoded: &[u8]) -> f64 {
        assert_eq!(original.len(), decoded.len(), "psnr inputs must match in length");
        let mut sse = 0.0f64;
        for (a, b) in original.iter().zip(decoded) {
            let d = f64::from(*a) - f64::from(*b);
            sse += d * d;
        }
        if sse == 0.0 {
            return 99.0;
        }
        let mse = sse / original.len().max(1) as f64;
        10.0 * (255.0 * 255.0 / mse).log10()
    }

    /// Round-trips an image through the encoder and the `regia_asset` reference decoder
    /// (independently written from the same specification), returning the decoded bytes.
    fn roundtrip(rgba: &[u8], width: u32, height: u32, opts: &EncodeOptions) -> Vec<u8> {
        let blocks = encode_image(rgba, width, height, opts);
        let want = (width.div_ceil(4) * height.div_ceil(4) * 16) as usize;
        assert_eq!(blocks.len(), want, "block byte count for {width}x{height}");
        decode_bcn(BcnKind::Bc7, width, height, &blocks).expect("reference decode")
    }

    /// Unweighted squared error of one encoded block, decoded by the reference decoder.
    fn block_sse(texels: &[[u8; 4]; 16], opts: &EncodeOptions) -> f64 {
        let decoded = decode_block(&encode_block(texels, opts));
        weighted_sse(texels, &decoded, [1.0; 4])
    }

    /// The texels of block `(bx, by)` of an image, with the edge replication
    /// [`encode_image`] applies (test-side copy for the per-block checks).
    fn image_block(rgba: &[u8], width: u32, height: u32, bx: u32, by: u32) -> [[u8; 4]; 16] {
        let mut block = [[0u8; 4]; 16];
        for y in 0..4u32 {
            for x in 0..4u32 {
                let px = (bx * 4 + x).min(width - 1);
                let py = (by * 4 + y).min(height - 1);
                let i = ((py * width + px) * 4) as usize;
                block[(y * 4 + x) as usize] = [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]];
            }
        }
        block
    }

    /// A diagonal colour gradient: every channel affine in `x + y`, so each block's
    /// texels lie on one line in RGB space - the case BC7's line model fits best.
    fn gradient_image(width: u32, height: u32) -> Vec<u8> {
        let span = (width + height).saturating_sub(2).max(1) as f32;
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let s = (x + y) as f32 / span;
                out.push((20.0 + 200.0 * s).round() as u8);
                out.push((230.0 - 180.0 * s).round() as u8);
                out.push((40.0 + 160.0 * s).round() as u8);
                out.push(255);
            }
        }
        out
    }

    /// A separable gradient (red from `x`, green from `y`, blue from both): each block's
    /// texels span a plane, not a line, so partitions have to earn their keep.
    fn planar_gradient_image(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let r = x * 255 / width.max(1);
                let g = y * 255 / height.max(1);
                out.extend_from_slice(&[r as u8, g as u8, ((r + g) / 2) as u8, 255]);
            }
        }
        out
    }

    /// A black/white checker with `cell`-texel squares.
    fn checker_image(width: u32, height: u32, cell: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let v = if (x / cell + y / cell).is_multiple_of(2) {
                    16
                } else {
                    240
                };
                out.extend_from_slice(&[v, v, v, 255]);
            }
        }
        out
    }

    /// A solid colour over a varying alpha ramp (alpha affine in `x`).
    fn alpha_ramp_image(width: u32, height: u32) -> Vec<u8> {
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for _y in 0..height {
            for x in 0..width {
                let a = (x * 255 / width.max(1)) as u8;
                out.extend_from_slice(&[200, 80, 40, a]);
            }
        }
        out
    }

    /// Deterministic xorshift32 stream (explicit seed; no library RNG, no clock).
    fn xorshift(state: &mut u32) -> u32 {
        let mut s = *state;
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        *state = s;
        s
    }

    /// Independent uniform random texels - incompressible by design (see the white-noise
    /// PSNR test for why its floor is low).
    fn white_noise_image(width: u32, height: u32, seed: u32) -> Vec<u8> {
        let mut state = seed.max(1);
        (0..width * height * 4)
            .map(|_| (xorshift(&mut state) >> 24) as u8)
            .collect()
    }

    /// Spatially correlated (band-limited) noise: random knot colours every 16 texels,
    /// bilinearly interpolated. Real texture detail is correlated like this, not white.
    fn value_noise_image(width: u32, height: u32, seed: u32) -> Vec<u8> {
        let knots_x = (width / 16 + 2) as usize;
        let knots_y = (height / 16 + 2) as usize;
        let mut state = seed.max(1);
        let knots: Vec<[f32; 3]> = (0..knots_x * knots_y)
            .map(|_| {
                [
                    (xorshift(&mut state) >> 24) as f32,
                    (xorshift(&mut state) >> 24) as f32,
                    (xorshift(&mut state) >> 24) as f32,
                ]
            })
            .collect();
        let knot = |kx: usize, ky: usize| knots[ky.min(knots_y - 1) * knots_x + kx.min(knots_x - 1)];
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            for x in 0..width {
                let (kx, ky) = ((x / 16) as usize, (y / 16) as usize);
                let (fx, fy) = ((x % 16) as f32 / 16.0, (y % 16) as f32 / 16.0);
                for ch in 0..3 {
                    let top = knot(kx, ky)[ch] * (1.0 - fx) + knot(kx + 1, ky)[ch] * fx;
                    let bottom = knot(kx, ky + 1)[ch] * (1.0 - fx) + knot(kx + 1, ky + 1)[ch] * fx;
                    out.push((top * (1.0 - fy) + bottom * fy).round().clamp(0.0, 255.0) as u8);
                }
                out.push(255);
            }
        }
        out
    }

    /// Left half: a red ramp down the rows. Right half: a blue ramp. One line cannot fit
    /// both halves; a vertical two-subset partition fits each exactly-ish.
    fn two_halves_block() -> [[u8; 4]; 16] {
        std::array::from_fn(|i| {
            let (x, y) = ((i % 4) as u8, (i / 4) as u8);
            if x < 2 {
                [y * 60, 10, 10, 255]
            } else {
                [10, 10, y * 60, 255]
            }
        })
    }

    /// Colour checker times an independent 4-step alpha ramp by row: colour and alpha
    /// need unrelated indices, which only the dual-index modes (4 and 5) have.
    fn separate_alpha_block() -> [[u8; 4]; 16] {
        std::array::from_fn(|i| {
            let (x, y) = (i % 4, i / 4);
            let v = if (x + y) % 2 == 0 { 0u8 } else { 255u8 };
            [v, v, v, (y * 85) as u8]
        })
    }

    // PSNR floors below are set from what the format can represent, with headroom for a
    // correct-but-not-optimal search: mode 6 endpoints are effectively 8-bit and its
    // 4-bit indices give 16 steps along each subset's segment, so content whose blocks
    // lie near a line (or a line per partition subset) quantizes to about a level RMS,
    // i.e. the high-40s dB; floors sit a few dB under that.

    #[test]
    fn solid_colour_is_exact_within_one_level() {
        // One P-bit per endpoint forces one parity on all four channels, so an arbitrary
        // colour is at most one level off in each channel, never more.
        for color in [
            [1, 2, 3, 4],
            [254, 253, 252, 251],
            [17, 99, 201, 128],
            [128, 128, 128, 128],
        ] {
            let texels = [color; 16];
            for opts in [EncodeOptions::fast(), EncodeOptions::high()] {
                let decoded = decode_block(&encode_block(&texels, &opts));
                for px in decoded {
                    for ch in 0..4 {
                        assert!(
                            px[ch].abs_diff(color[ch]) <= 1,
                            "colour {color:?} decoded as {px:?} at {opts:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn all_zero_and_all_full_blocks_are_exact() {
        // 0 and 255 are representable endpoints at every precision this encoder emits.
        for color in [[0, 0, 0, 0], [255, 255, 255, 255], [0, 0, 0, 255]] {
            let texels = [color; 16];
            let decoded = decode_block(&encode_block(&texels, &EncodeOptions::high()));
            assert!(decoded.iter().all(|px| *px == color), "colour {color:?}");
        }
    }

    #[test]
    fn gradient_psnr_above_45_db_at_high() {
        let img = gradient_image(32, 32);
        let out = roundtrip(&img, 32, 32, &EncodeOptions::high());
        let p = psnr(&img, &out);
        assert!(p > 45.0, "gradient PSNR {p:.1} dB");
    }

    #[test]
    fn gradient_psnr_above_40_db_at_normal_and_fast() {
        // One line per block already fits this gradient, so even Fast (mode 6 alone)
        // stays in the 40s; the floor leaves room for its single refinement pass.
        let img = gradient_image(32, 32);
        for opts in [EncodeOptions::normal(), EncodeOptions::fast()] {
            let out = roundtrip(&img, 32, 32, &opts);
            let p = psnr(&img, &out);
            assert!(p > 40.0, "gradient PSNR {p:.1} dB at {opts:?}");
        }
    }

    #[test]
    fn planar_gradient_psnr_above_32_db_at_high() {
        // A plane per block: a single segment leaves a systematic residual, partitions
        // cut it roughly in half per subset, landing well below the line case.
        let img = planar_gradient_image(16, 16);
        let out = roundtrip(&img, 16, 16, &EncodeOptions::high());
        let p = psnr(&img, &out);
        assert!(p > 32.0, "planar gradient PSNR {p:.1} dB");
    }

    #[test]
    fn checker_psnr_is_high() {
        // Two colours a block are two endpoints: a 1-texel checker is endpoint-exact
        // (parity aside), a 2-texel checker also matches partition shapes.
        for (cell, floor) in [(1, 40.0), (2, 35.0)] {
            let img = checker_image(16, 16, cell);
            let out = roundtrip(&img, 16, 16, &EncodeOptions::high());
            let p = psnr(&img, &out);
            assert!(p > floor, "checker cell {cell} PSNR {p:.1} dB");
        }
    }

    #[test]
    fn value_noise_psnr_above_30_db_at_high() {
        // Band-limited noise: within a block the surface is a smooth bilinear patch, so
        // a line (or two, partitioned) tracks it to a few levels RMS.
        let img = value_noise_image(32, 32, 0x1234_5678);
        let out = roundtrip(&img, 32, 32, &EncodeOptions::high());
        let p = psnr(&img, &out);
        assert!(p > 30.0, "value noise PSNR {p:.1} dB");
    }

    #[test]
    fn white_noise_psnr_above_the_information_floor() {
        // I.i.d. uniform texels carry ~32 bits of entropy each and BC7 spends 8, so no
        // encoder can approach the smooth-content numbers: the residual variance of the
        // best line through 16 uniform points in 4D is a large fraction of the signal
        // variance (255^2 / 12), putting the ceiling in the mid-teens dB. The 9 dB floor
        // only verifies that the fit is real and nothing degenerates to garbage; the
        // signal itself bounds what any BC7 encoder could score here.
        let img = white_noise_image(16, 16, 0x0bad_f00d);
        let out = roundtrip(&img, 16, 16, &EncodeOptions::high());
        let p = psnr(&img, &out);
        assert!(p > 9.0, "white noise PSNR {p:.1} dB");
    }

    #[test]
    fn alpha_ramp_psnr_above_45_db() {
        let img = alpha_ramp_image(32, 16);
        let out = roundtrip(&img, 32, 16, &EncodeOptions::high());
        let p = psnr(&img, &out);
        assert!(p > 45.0, "alpha ramp PSNR {p:.1} dB");
    }

    #[test]
    fn every_block_of_the_synthetic_images_roundtrips() {
        // Per-block complement to the whole-image PSNR tests: no single block of the
        // structured images may be wildly off (mean error under 8 levels a channel).
        let images: [(Vec<u8>, u32, u32); 4] = [
            (gradient_image(16, 16), 16, 16),
            (checker_image(16, 16, 1), 16, 16),
            (value_noise_image(16, 16, 7), 16, 16),
            (alpha_ramp_image(16, 16), 16, 16),
        ];
        let opts = EncodeOptions::normal();
        for (img, w, h) in &images {
            for by in 0..h / 4 {
                for bx in 0..w / 4 {
                    let block = image_block(img, *w, *h, bx, by);
                    let sse = block_sse(&block, &opts);
                    assert!(sse < 64.0 * 64.0, "block ({bx}, {by}) SSE {sse:.0}");
                }
            }
        }
    }

    #[test]
    fn full_search_is_never_worse_than_mode6_only() {
        // High fits strictly more candidates than Fast on the same deterministic pass
        // trajectory, so its decoded error can only tie or improve, block for block.
        let fast = EncodeOptions::fast();
        let high = EncodeOptions::high();
        let mut blocks: Vec<[[u8; 4]; 16]> = vec![two_halves_block(), separate_alpha_block()];
        for (img, w, h) in [
            (gradient_image(16, 16), 16, 16),
            (checker_image(16, 16, 2), 16, 16),
            (white_noise_image(16, 16, 99), 16, 16),
            (alpha_ramp_image(16, 16), 16, 16),
            (planar_gradient_image(16, 16), 16, 16),
        ] {
            for by in 0..h / 4 {
                for bx in 0..w / 4 {
                    blocks.push(image_block(&img, w, h, bx, by));
                }
            }
        }
        for (i, block) in blocks.iter().enumerate() {
            let f = block_sse(block, &fast);
            let s = block_sse(block, &high);
            assert!(
                s <= f + 1e-3,
                "block {i}: high SSE {s:.1} exceeds fast SSE {f:.1}"
            );
        }
    }

    #[test]
    fn partial_blocks_use_edge_replication() {
        // 6x5 and 5x7 both need a 2x2 block grid; decoded cropping must give the image
        // back at its own size with no edge blow-up.
        for (w, h) in [(6u32, 5u32), (5, 7), (1, 1), (3, 9)] {
            let img = gradient_image(w, h);
            let out = roundtrip(&img, w, h, &EncodeOptions::normal());
            assert_eq!(out.len(), (w * h * 4) as usize);
            let p = psnr(&img, &out);
            assert!(p > 35.0, "{w}x{h} PSNR {p:.1} dB");
        }
    }

    #[test]
    fn output_length_matches_the_block_grid() {
        for (w, h) in [(4u32, 4u32), (8, 4), (4, 8), (7, 5), (16, 16), (31, 17)] {
            let img = vec![128u8; (w * h * 4) as usize];
            let blocks = encode_image(&img, w, h, &EncodeOptions::fast());
            assert_eq!(
                blocks.len(),
                (w.div_ceil(4) * h.div_ceil(4) * 16) as usize,
                "{w}x{h}"
            );
        }
    }

    #[test]
    fn encoding_is_deterministic_across_repeats() {
        let img = value_noise_image(16, 16, 0xdead_beef);
        for opts in [
            EncodeOptions::fast(),
            EncodeOptions::normal(),
            EncodeOptions::high(),
        ] {
            let a = encode_image(&img, 16, 16, &opts);
            let b = encode_image(&img, 16, 16, &opts);
            assert_eq!(a, b, "image bytes differ at {opts:?}");
            let block = image_block(&img, 16, 16, 1, 1);
            assert_eq!(encode_block(&block, &opts), encode_block(&block, &opts));
        }
    }

    #[test]
    fn fast_emits_only_mode6() {
        let img = white_noise_image(16, 16, 5);
        let blocks = encode_image(&img, 16, 16, &EncodeOptions::fast());
        for block in blocks.as_chunks::<16>().0 {
            assert_eq!(block_mode(block), Some(6));
        }
    }

    #[test]
    fn decoder_reads_back_the_mode_bits_written() {
        // The mode is the first set low bit, which is how both `block_mode` and the
        // reference decoder read it; every emitted block must carry an implemented mode
        // and decode (through `regia_asset`) to the same texels `decode_block` gives.
        let img = value_noise_image(16, 16, 42);
        let blocks = encode_image(&img, 16, 16, &EncodeOptions::high());
        for block in blocks.as_chunks::<16>().0 {
            let mode = block_mode(block).expect("encoder never writes the reserved mode");
            assert!([1, 3, 4, 5, 6, 7].contains(&mode), "unimplemented mode {mode}");
            let first_set = (0u8..8).find(|&m| block[0] & (1u8 << m) != 0);
            assert_eq!(first_set, Some(mode), "low byte disagrees with block_mode");
            let via_asset = regia_asset::bcn_decode::decode_block(BcnKind::Bc7, block);
            assert_eq!(via_asset, decode_block(block));
        }
        assert_eq!(block_mode(&[0u8; 16]), None, "reserved mode reads as None");
    }

    #[test]
    fn anchor_texel_can_sit_at_the_far_endpoint() {
        // Texel 0 is white, everything else near-black: fit naively with texel 0 at the
        // segment's far end and its shortened anchor index could never reach white; the
        // orientation flip must put white at endpoint 0 instead.
        let mut texels = [[8u8, 8, 8, 255]; 16];
        texels[0] = [255, 255, 255, 255];
        for opts in [EncodeOptions::fast(), EncodeOptions::high()] {
            let decoded = decode_block(&encode_block(&texels, &opts));
            assert!(
                decoded[0][0] > 200 && decoded[0][1] > 200 && decoded[0][2] > 200,
                "anchor decoded as {:?} at {opts:?}",
                decoded[0]
            );
            assert!(decoded[15][0] < 40, "far texel decoded as {:?}", decoded[15]);
        }
    }

    #[test]
    fn separate_alpha_content_picks_a_dual_index_mode() {
        let texels = separate_alpha_block();
        let encoded = encode_block(&texels, &EncodeOptions::high());
        let mode = block_mode(&encoded).expect("mode");
        assert!([4, 5].contains(&mode), "expected a dual-index mode, got {mode}");
        let decoded = decode_block(&encoded);
        for (want, got) in texels.iter().zip(decoded) {
            for ch in 0..4 {
                assert!(got[ch].abs_diff(want[ch]) <= 4, "want {want:?}, got {got:?}");
            }
        }
    }

    #[test]
    fn two_halves_pick_a_partitioned_mode_and_beat_mode6() {
        let texels = two_halves_block();
        let encoded = encode_block(&texels, &EncodeOptions::high());
        let mode = block_mode(&encoded).expect("mode");
        assert!(
            [1, 3, 7].contains(&mode),
            "expected a partitioned mode, got {mode}"
        );
        let high = block_sse(&texels, &EncodeOptions::high());
        let fast = block_sse(&texels, &EncodeOptions::fast());
        assert!(
            high < fast * 0.5,
            "partitioned SSE {high:.0} vs mode-6 SSE {fast:.0}"
        );
    }

    #[test]
    fn block_order_is_row_major() {
        // Four solid 4x4 quadrants of an 8x8 image must decode near their own colours,
        // which pins both the block walk and the texel walk to row-major.
        let colors = [
            [255u8, 0, 0, 255],
            [0, 255, 0, 255],
            [0, 0, 255, 255],
            [255, 255, 0, 255],
        ];
        let mut img = Vec::new();
        for y in 0..8u32 {
            for x in 0..8u32 {
                img.extend_from_slice(&colors[((y / 4) * 2 + x / 4) as usize]);
            }
        }
        let out = roundtrip(&img, 8, 8, &EncodeOptions::fast());
        for (want, got) in img.as_chunks::<4>().0.iter().zip(out.as_chunks::<4>().0) {
            for ch in 0..4 {
                assert!(got[ch].abs_diff(want[ch]) <= 1, "want {want:?}, got {got:?}");
            }
        }
    }

    #[test]
    fn empty_and_mismatched_images_are_errors_not_panics() {
        let opts = EncodeOptions::fast();
        assert_eq!(
            try_encode_image(&[], 0, 4, &opts),
            Err(Bc7EncodeError::EmptyImage { width: 0, height: 4 })
        );
        assert_eq!(
            try_encode_image(&[], 4, 0, &opts),
            Err(Bc7EncodeError::EmptyImage { width: 4, height: 0 })
        );
        assert_eq!(
            try_encode_image(&[0u8; 10], 4, 4, &opts),
            Err(Bc7EncodeError::BadBuffer {
                width: 4,
                height: 4,
                len: 10
            })
        );
        // The infallible wrapper's documented fallback is an empty vector.
        assert!(encode_image(&[], 0, 0, &opts).is_empty());
        assert!(encode_image(&[0u8; 10], 4, 4, &opts).is_empty());
    }

    #[test]
    fn huge_dimensions_do_not_panic_or_allocate() {
        let opts = EncodeOptions::fast();
        assert!(matches!(
            try_encode_image(&[], u32::MAX, u32::MAX, &opts),
            Err(Bc7EncodeError::BadBuffer { .. })
        ));
        assert!(encode_image(&[], u32::MAX, 1, &opts).is_empty());
    }

    #[test]
    fn degenerate_channel_weights_fall_back_deterministically() {
        let block = image_block(&gradient_image(8, 8), 8, 8, 0, 0);
        for weights in [
            [f32::NAN, f32::INFINITY, -1.0, 0.0],
            [0.0, 0.0, 0.0, 0.0],
            [f32::NEG_INFINITY; 4],
            [-0.0, 1.0, 1.0, 1.0],
        ] {
            let opts = EncodeOptions::fast().with_channel_weights(weights);
            let a = encode_block(&block, &opts);
            let b = encode_block(&block, &opts);
            assert_eq!(a, b, "weights {weights:?} not deterministic");
            assert_eq!(block_mode(&a), Some(6));
            // Sanitized weights still produce a sane fit of a smooth block.
            let sse = weighted_sse(&block, &decode_block(&a), [1.0; 4]);
            assert!(sse < 64.0 * 64.0, "weights {weights:?} SSE {sse:.0}");
        }
    }

    #[test]
    fn sanitize_weights_matches_its_contract() {
        assert_eq!(
            sanitize_weights([f32::NAN, -2.0, f32::INFINITY, 0.5]),
            [1.0, 1.0, 1.0, 0.5]
        );
        assert_eq!(sanitize_weights([0.0; 4]), [1.0; 4]);
        assert_eq!(sanitize_weights([0.0, 2.0, 0.0, 0.0]), [0.0, 2.0, 0.0, 0.0]);
    }

    #[test]
    fn single_outlier_texel_stays_visible() {
        // Fifteen mid-grey texels and one saturated red: the red must survive as the
        // reddest decoded texel instead of being averaged away entirely.
        let mut texels = [[128u8, 128, 128, 255]; 16];
        texels[5] = [255, 0, 0, 255];
        let decoded = decode_block(&encode_block(&texels, &EncodeOptions::normal()));
        assert!(decoded[5][0] > 160, "outlier decoded as {:?}", decoded[5]);
        assert!(decoded[5][0] > decoded[0][0], "outlier is not the reddest texel");
    }

    #[test]
    fn grayscale_ramp_block_is_near_exact_at_fast() {
        // Sixteen greys 0, 17, ..., 255: one segment, 4-bit weights land within a level
        // or two of every step (weight table spacing is ~4/64 of the 255 span).
        let texels: [[u8; 4]; 16] = std::array::from_fn(|i| {
            let v = (i * 17) as u8;
            [v, v, v, 255]
        });
        let decoded = decode_block(&encode_block(&texels, &EncodeOptions::fast()));
        for (want, got) in texels.iter().zip(decoded) {
            for ch in 0..3 {
                assert!(got[ch].abs_diff(want[ch]) <= 3, "want {want:?}, got {got:?}");
            }
            // Alpha is 255 everywhere, but an endpoint whose RGB is 0 spends its P-bit
            // on even parity and can only reach alpha 254: allow that one level.
            assert!(got[3] >= 254, "alpha faded to {}", got[3]);
        }
    }

    #[test]
    fn channel_weights_change_the_tradeoff() {
        // A block whose red and alpha ramps disagree: weighting alpha to zero must not
        // give a worse red fit than weighting everything equally (it frees the fit).
        let texels: [[u8; 4]; 16] = std::array::from_fn(|i| [(i * 17) as u8, 100, 100, (255 - i * 17) as u8]);
        let red_sse = |opts: &EncodeOptions| {
            let decoded = decode_block(&encode_block(&texels, opts));
            texels
                .iter()
                .zip(decoded)
                .map(|(a, b)| (f64::from(a[0]) - f64::from(b[0])).powi(2))
                .sum::<f64>()
        };
        let uniform = red_sse(&EncodeOptions::fast());
        let red_only = red_sse(&EncodeOptions::fast().with_channel_weights([1.0, 0.0, 0.0, 0.0]));
        assert!(
            red_only <= uniform + 1e-3,
            "red-only {red_only:.1} vs uniform {uniform:.1}"
        );
    }

    #[test]
    fn mode_tables_agree_with_the_decoder_layout() {
        // Every mode's fields must exactly fill a 128-bit block when laid out the way
        // the writer lays them out; a drift here is what the reference-decoder
        // round-trips would otherwise have to catch.
        for (mode, desc) in MODES.iter().enumerate() {
            let endpoints = desc.subsets as u32 * 2;
            let mut total = mode as u32
                + 1
                + desc.partition_bits
                + desc.rotation_bits
                + desc.index_selection_bits
                + 3 * endpoints * desc.color_bits
                + endpoints * desc.alpha_bits;
            total += if desc.endpoint_pbits {
                endpoints
            } else if desc.shared_pbits {
                desc.subsets as u32
            } else {
                0
            };
            let anchors = match desc.subsets {
                1 => 1,
                2 => 2,
                _ => 3,
            };
            total += 16 * desc.index_bits - anchors;
            if desc.index2_bits > 0 {
                total += 16 * desc.index2_bits - 1;
            }
            assert_eq!(total, 128, "mode {mode} layout");
        }
        // Partition anchors must sit in the subset they anchor (decoder's invariant).
        for (p, &anchor) in A2.iter().enumerate() {
            assert_eq!(subset_of(2, p, 0), 0, "partition {p} texel 0");
            assert_eq!(subset_of(2, p, usize::from(anchor)), 1, "partition {p} anchor");
        }
    }

    #[test]
    fn fast_256x256_encodes_within_a_generous_budget() {
        // Speed guard. The repo forbids wall-clock asserts in tests (verification lane,
        // muse-verify-bc7), so the budget is counted in work, not seconds: Fast ranks exactly
        // one mode-6 candidate per block, which is what catches an accidental blow-up such as
        // the partition search leaking into Fast. Measured: ~30 ms for this image in the dev
        // profile on the build server. Normal and High are pinned to their documented candidate sets.
        let ranked = |rgba: &[u8], w: u32, h: u32, opts: &EncodeOptions| {
            RANKED_CANDIDATES.with(|n| n.set(0));
            let blocks = encode_image(rgba, w, h, opts);
            (blocks, RANKED_CANDIDATES.with(std::cell::Cell::get))
        };
        let img = value_noise_image(256, 256, 0xfeed_face);
        let (blocks, n) = ranked(&img, 256, 256, &EncodeOptions::fast());
        assert_eq!(blocks.len(), 64 * 64 * 16);
        assert_eq!(n, 64 * 64, "Fast must rank one candidate per block");
        // One block that no candidate fits exactly, so no early exit: mode 6, then
        // 3 partitioned modes x (6 shortlisted | 64) partitions, 4 mode-5 rotations and
        // 8 mode-4 rotation/selection pairs.
        let noise = white_noise_image(4, 4, 3);
        let (_, normal) = ranked(&noise, 4, 4, &EncodeOptions::normal());
        assert_eq!(normal, 1 + 3 * 6 + 4 + 8, "Normal candidate count");
        let (_, high) = ranked(&noise, 4, 4, &EncodeOptions::high());
        assert_eq!(high, 1 + 3 * 64 + 4 + 8, "High candidate count");
    }
}
