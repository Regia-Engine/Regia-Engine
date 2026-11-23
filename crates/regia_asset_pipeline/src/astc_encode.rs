// SPDX-License-Identifier: MPL-2.0
//! Pure-Rust LDR ASTC block encoder (and matching reference decoder) for the Android cook.
//!
//! The Android texture lane uploads ASTC blocks produced by the vendored `astcenc`
//! (`crate::astc`). This module is the cleanroom, dependency-free counterpart: an encoder that
//! runs anywhere Rust runs (including on-device tooling and tests) with no FFI. It is original
//! code written from the published format description: the Khronos ASTC specification as
//! reproduced in chapter 23 of the Khronos Data Format Specification (the physical block
//! layout, color endpoint modes, Bounded Integer Sequence Encoding, integer unquantization,
//! blue contraction and weight infill) and the design
//! paper by Nystad et al., "Adaptive Scalable Texture Compression", HPG 2012. Provenance is
//! recorded in `docs/provenance.md`; the user-facing summary is
//! `book/reference/astc-encoder.md`.
//!
//! # Scope
//!
//! - 2D LDR blocks of 4×4, 5×5, 6×6 and 8×8 texels ([`BlockSize`]), 16 bytes each.
//! - One partition per block. The weight grid is the block footprint (one weight per texel),
//!   single plane (no dual plane).
//! - Direct color endpoint modes only: [`ColorEndpointMode::Luminance`],
//!   [`ColorEndpointMode::LuminanceAlpha`], [`ColorEndpointMode::Rgb`] (alpha decodes as 1)
//!   and [`ColorEndpointMode::Rgba`]. Constant blocks are stored as void-extent blocks.
//! - Endpoint fitting by principal axis (Pearson 1901) with alternating least-squares
//!   refinement of endpoints and weights, then exact per-texel weight quantization and
//!   best-of selection by decoded squared error. The endpoint quantization range is the
//!   largest that fits the bits left by the weights, exactly as a decoder derives it.
//!
//! The bundled decoder ([`decode_block`], [`decode_image`]) covers the subset this encoder
//! emits (single partition, the four direct endpoint modes, single plane, any weight grid up
//! to the footprint). Blocks outside that subset decode as opaque magenta, the error color.
//! The decoder exists so the encoder can be measured without the FFI codec; the test
//! suite cross-checks both directions against `crate::astc::decompress_astc_rgba8`.
//!
//! Everything is deterministic: fixed iteration counts, fixed candidate order, no hash-map
//! iteration, no threads, no clock. The same input always produces the same bytes.

use thiserror::Error;

/// Failure of an ASTC encode/decode request.
#[derive(Debug, Error)]
pub enum AstcEncodeError {
    /// The RGBA buffer length does not match `width * height * 4`.
    #[error("rgba buffer length {len} does not match {width}x{height}")]
    BadBuffer {
        /// Pixel buffer length in bytes.
        len: usize,
        /// Image width in texels.
        width: u32,
        /// Image height in texels.
        height: u32,
    },
    /// The compressed buffer is not the whole number of 16-byte blocks the size needs.
    #[error("compressed length {len} does not match {blocks} blocks")]
    BadBlocks {
        /// Compressed buffer length in bytes.
        len: usize,
        /// Number of blocks the image size needs.
        blocks: usize,
    },
    /// A width or height of zero was requested.
    #[error("invalid image dimensions {width}x{height}")]
    InvalidDimensions {
        /// Image width in texels.
        width: u32,
        /// Image height in texels.
        height: u32,
    },
    /// [`try_encode_block`] was given the wrong number of texels for the block size.
    #[error("block needs {expected} texels, got {actual}")]
    BadBlockTexels {
        /// Texels the block size needs.
        expected: usize,
        /// Texels supplied.
        actual: usize,
    },
    /// A BISE range that is not one of [`QUANT_RANGES`] was requested.
    #[error("invalid BISE quantization range {range}")]
    InvalidRange {
        /// The requested range.
        range: u32,
    },
    /// A value to BISE-encode lies outside its range.
    #[error("value {value} does not fit BISE range {range}")]
    ValueOutOfRange {
        /// The offending value.
        value: u32,
        /// The range it had to fit.
        range: u32,
    },
    /// A BISE bitstream ended before the requested values were read.
    #[error("BISE stream needs {needed} bits, has {available}")]
    TruncatedBits {
        /// Bits the read needs.
        needed: usize,
        /// Bits available in the stream.
        available: usize,
    },
}

/// A 2D ASTC block footprint supported by this encoder.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum BlockSize {
    /// 4×4 texels (8 bits per texel).
    Block4x4,
    /// 5×5 texels (5.12 bits per texel).
    Block5x5,
    /// 6×6 texels (about 3.56 bits per texel).
    Block6x6,
    /// 8×8 texels (2 bits per texel).
    Block8x8,
}

impl BlockSize {
    /// Every supported block size, smallest first.
    pub const ALL: [Self; 4] = [Self::Block4x4, Self::Block5x5, Self::Block6x6, Self::Block8x8];

    /// Block width in texels.
    pub fn width(self) -> u32 {
        match self {
            Self::Block4x4 => 4,
            Self::Block5x5 => 5,
            Self::Block6x6 => 6,
            Self::Block8x8 => 8,
        }
    }

    /// Block height in texels.
    pub fn height(self) -> u32 {
        self.width()
    }

    /// Number of texels in one block.
    pub fn texel_count(self) -> usize {
        (self.width() * self.height()) as usize
    }

    /// The block size with footprint `width`×`height`, if this encoder supports it.
    pub fn from_dims(width: u32, height: u32) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|b| b.width() == width && b.height() == height)
    }

    /// Compressed bits spent on each texel at this block size.
    pub fn bits_per_texel(self) -> f32 {
        128.0 / self.texel_count() as f32
    }
}

/// Encoder search effort. Higher effort never changes the format, only how many candidates
/// are fitted before the best is kept.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Quality {
    /// Few candidates: a heuristic endpoint mode and at most two weight ranges.
    Fast,
    /// The cook's everyday setting: several weight ranges and endpoint modes.
    #[default]
    Balanced,
    /// Every endpoint mode and weight range, plus endpoint level tweaking.
    Thorough,
}

/// A direct LDR color endpoint mode (CEM) this encoder emits. "Direct" means both endpoint
/// colors are stored as quantized values; the base+offset and base+scale modes of the format
/// are not emitted (the decoder reports blocks using them as invalid).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColorEndpointMode {
    /// One luminance pair; texels decode as `(L, L, L, 255)`. CEM 0.
    Luminance,
    /// Luminance and alpha pairs; texels decode as `(L, L, L, A)`. CEM 4.
    LuminanceAlpha,
    /// Red, green and blue pairs; alpha decodes as 255. CEM 8.
    Rgb,
    /// Red, green, blue and alpha pairs. CEM 12.
    Rgba,
}

impl ColorEndpointMode {
    /// The 4-bit CEM field stored in the block.
    pub fn cem_bits(self) -> u32 {
        match self {
            Self::Luminance => 0,
            Self::LuminanceAlpha => 4,
            Self::Rgb => 8,
            Self::Rgba => 12,
        }
    }

    /// The mode for a stored CEM field, if it is one of the four direct modes.
    pub fn from_cem_bits(bits: u32) -> Option<Self> {
        match bits {
            0 => Some(Self::Luminance),
            4 => Some(Self::LuminanceAlpha),
            8 => Some(Self::Rgb),
            12 => Some(Self::Rgba),
            _ => None,
        }
    }

    /// Number of quantized endpoint integers one block stores in this mode (one partition).
    /// Values are stored as per-channel pairs: `(c0, c1)` for each channel in turn.
    pub fn value_count(self) -> usize {
        match self {
            Self::Luminance => 2,
            Self::LuminanceAlpha => 4,
            Self::Rgb => 6,
            Self::Rgba => 8,
        }
    }
}

// ---------------------------------------------------------------------------
// Bounded Integer Sequence Encoding (BISE)
// ---------------------------------------------------------------------------

/// Every quantization range of the format, ascending. A range is `2^bits`, `3 * 2^bits`
/// (a trit above the bits) or `5 * 2^bits` (a quint above the bits). Endpoint integers may use
/// any of these; weights are limited to ranges up to 32 (see [`WEIGHT_RANGES`]).
pub const QUANT_RANGES: [u32; 21] = [
    2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32, 40, 48, 64, 80, 96, 128, 160, 192, 256,
];

/// The quantization ranges a weight may use, ascending. The block mode stores a 3-bit
/// range value `rho` (2..=7; 0 and 1 are invalid) plus a precision bit `P`: at `P = 0`
/// `rho` selects the first six entries (`rho - 2` is the index), at `P = 1` the last six.
/// One block-mode row (the one encoding 6..=9-sized square grids) has no room for `P`
/// and forces it to 0, so footprint grids at 6×6 and 8×8 only offer the first six.
pub const WEIGHT_RANGES: [u32; 12] = [2, 3, 4, 5, 6, 8, 10, 12, 16, 20, 24, 32];

/// How one BISE integer is split: a plain binary part holding the low `bits`, and above it
/// either nothing, one trit (0..3) or one quint (0..5), so `v = high * 2^bits + low`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BiseKind {
    /// Binary only.
    Bits,
    /// A trit above the bits; trits pack five to an 8-bit group (`3^5 = 243 < 256`).
    Trit,
    /// A quint above the bits; quints pack three to a 7-bit group (`5^3 = 125 < 128`).
    Quint,
}

/// The (kind, bit count) of a range, or `None` when `range` is not in [`QUANT_RANGES`].
fn bise_params(range: u32) -> Option<(BiseKind, u32)> {
    QUANT_RANGES.contains(&range).then(|| {
        if range.is_power_of_two() {
            (BiseKind::Bits, range.trailing_zeros())
        } else if range.is_multiple_of(3) && (range / 3).is_power_of_two() {
            (BiseKind::Trit, (range / 3).trailing_zeros())
        } else {
            (BiseKind::Quint, (range / 5).trailing_zeros())
        }
    })
}

/// Bits needed to BISE-encode `count` integers of the given `range`: the length of the
/// integer-sequence stream. Trit and quint groups are packed five-to-eight-bits and
/// three-to-seven-bits, and a short final group spends only its share of the packed
/// field (`ceil(count * 8 / 5)` / `ceil(count * 7 / 3)` bits, the spec's size formula):
/// the group's remaining packed bits sit past the stream end and read as zero, which
/// the interleaved layout and the packed-bit encoding are designed to accommodate.
///
/// # Errors
/// [`AstcEncodeError::InvalidRange`] when `range` is not in [`QUANT_RANGES`].
pub fn bise_bits(count: usize, range: u32) -> Result<usize, AstcEncodeError> {
    let (kind, bits) = bise_params(range).ok_or(AstcEncodeError::InvalidRange { range })?;
    let plain = count * bits as usize;
    Ok(match kind {
        BiseKind::Bits => plain,
        BiseKind::Trit => (count * 8).div_ceil(5) + plain,
        BiseKind::Quint => (count * 7).div_ceil(3) + plain,
    })
}

/// The five trits packed in the 8 bits `t`, by the spec's bit manipulations. Every
/// 8-bit input decodes (a few inputs are duplicates): the 256 inputs cover all
/// 243 trit tuples, which is what makes the table-search encoder below total.
fn unpack_trits(t: u32) -> [u32; 5] {
    let bit = |i: u32| (t >> i) & 1;
    let field = |hi: u32, lo: u32| (t >> lo) & ((1 << (hi - lo + 1)) - 1);
    let (c, t4, t3);
    if field(4, 2) == 0b111 {
        c = (field(7, 5) << 2) | field(1, 0);
        t4 = 2;
        t3 = 2;
    } else {
        c = field(4, 0);
        if field(6, 5) == 0b11 {
            t4 = 2;
            t3 = bit(7);
        } else {
            t4 = bit(7);
            t3 = field(6, 5);
        }
    }
    let cbit = |i: u32| (c >> i) & 1;
    let cfield = |hi: u32, lo: u32| (c >> lo) & ((1 << (hi - lo + 1)) - 1);
    let (t2, t1, t0);
    if cfield(1, 0) == 0b11 {
        t2 = 2;
        t1 = cbit(4);
        t0 = (cbit(3) << 1) | (cbit(2) & (cbit(3) ^ 1));
    } else if cfield(3, 2) == 0b11 {
        t2 = 2;
        t1 = 2;
        t0 = cfield(1, 0);
    } else {
        t2 = cbit(4);
        t1 = cfield(3, 2);
        t0 = (cbit(1) << 1) | (cbit(0) & (cbit(1) ^ 1));
    }
    [t0, t1, t2, t3, t4]
}

/// The three quints packed in the 7 bits `q`, by the spec's bit manipulations.
/// The 128 inputs cover all 125 quint tuples.
fn unpack_quints(q: u32) -> [u32; 3] {
    let bit = |i: u32| (q >> i) & 1;
    let field = |hi: u32, lo: u32| (q >> lo) & ((1 << (hi - lo + 1)) - 1);
    if field(2, 1) == 0b11 && field(6, 5) == 0 {
        let keep = bit(0) ^ 1;
        let q2 = (bit(0) << 2) | ((bit(4) & keep) << 1) | (bit(3) & keep);
        return [4, 4, q2];
    }
    let (q2, c);
    if field(2, 1) == 0b11 {
        q2 = 4;
        c = (field(4, 3) << 3) | (((field(6, 5) ^ 0b11) & 0b11) << 1) | bit(0);
    } else {
        q2 = field(6, 5);
        c = field(4, 0);
    }
    let (q1, q0) = if c & 0b111 == 0b101 {
        (4, (c >> 3) & 0b11)
    } else {
        ((c >> 3) & 0b11, c & 0b111)
    };
    [q0, q1, q2]
}

/// One chunk of a trit/quint group's interleaved stream layout: either the plain
/// `n`-bit part of value `index`, or one bit of the packed trit/quint field.
#[derive(Clone, Copy, Debug)]
enum IseChunk {
    /// The plain low bits of the group's value at this index.
    Plain(usize),
    /// Bit `b` of the packed field (`T` for trits, `Q` for quints).
    Packed(u32),
}

/// The chunk order of one group, from the spec's packing tables (shown there for
/// `n = 4`; the order is the same for every `n`, only the plain chunk widths vary).
/// Trit groups: `m0 T0 T1 m1 T2 T3 m2 T4 m3 T5 T6 m4 T7`.
/// Quint groups: `m0 Q0 Q1 Q2 m1 Q3 Q4 m2 Q5 Q6`.
fn ise_chunks(kind: BiseKind) -> &'static [IseChunk] {
    use IseChunk::{Packed, Plain};
    match kind {
        BiseKind::Trit => &[
            Plain(0),
            Packed(0),
            Packed(1),
            Plain(1),
            Packed(2),
            Packed(3),
            Plain(2),
            Packed(4),
            Plain(3),
            Packed(5),
            Packed(6),
            Plain(4),
            Packed(7),
        ],
        BiseKind::Quint => &[
            Plain(0),
            Packed(0),
            Packed(1),
            Packed(2),
            Plain(1),
            Packed(3),
            Packed(4),
            Plain(2),
            Packed(5),
            Packed(6),
        ],
        BiseKind::Bits => &[],
    }
}

/// Values per packed group (5 for trits, 3 for quints, 1 for plain bits).
fn ise_group_size(kind: BiseKind) -> usize {
    match kind {
        BiseKind::Trit => 5,
        BiseKind::Quint => 3,
        BiseKind::Bits => 1,
    }
}

/// A growing LSB-first bit sequence: bit `i` is bit `i % 8` of byte `i / 8`.
#[derive(Debug, Default)]
struct BitVec {
    /// Packed bytes.
    bytes: Vec<u8>,
    /// Number of valid bits.
    nbits: usize,
}

impl BitVec {
    /// Append the low `count` bits of `value`, least significant bit first.
    fn push_bits(&mut self, value: u32, count: u32) {
        for i in 0..count {
            let bit = (value >> i) & 1;
            let byte = self.nbits / 8;
            if byte == self.bytes.len() {
                self.bytes.push(0);
            }
            if bit == 1 {
                self.bytes[byte] |= 1 << (self.nbits % 8);
            }
            self.nbits += 1;
        }
    }

    /// Bit `index`, or 0 past the end.
    fn bit(&self, index: usize) -> u32 {
        if index >= self.nbits {
            return 0;
        }
        u32::from((self.bytes[index / 8] >> (index % 8)) & 1)
    }
}

/// A read cursor over a [`BitVec`]-shaped byte slice.
struct BitReader<'a> {
    /// Packed bytes being read.
    bytes: &'a [u8],
    /// Bits available (may be tighter than `bytes.len() * 8`).
    nbits: usize,
    /// Next bit to read.
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader over every bit of `bytes`.
    fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            nbits: bytes.len() * 8,
            pos: 0,
        }
    }

    /// Read `count` bits, least significant bit first.
    fn read_bits(&mut self, count: u32) -> Result<u32, AstcEncodeError> {
        if self.pos + count as usize > self.nbits {
            return Err(AstcEncodeError::TruncatedBits {
                needed: self.pos + count as usize,
                available: self.nbits,
            });
        }
        let mut value = 0u32;
        for i in 0..count {
            let index = self.pos + i as usize;
            let bit = u32::from((self.bytes[index / 8] >> (index % 8)) & 1);
            value |= bit << i;
        }
        self.pos += count as usize;
        Ok(value)
    }
}

/// The packed field (`T` or `Q`) encoding the high parts of one group whose present
/// values are the first `present` entries of `highs`, with every packed bit whose
/// abstract stream position lies in `forbidden` (past the stream's end, where a
/// decoder reads zero) clear. Found by exhaustive search over the 256/128 inputs
/// through the spec unpacking; the format's design guarantees a hit, and the
/// fallbacks keep the encoder total if that guarantee were ever violated.
fn ise_pack_highs(kind: BiseKind, highs: &[u32], present: usize, forbidden: u32) -> u32 {
    let limit = match kind {
        BiseKind::Trit => 256,
        BiseKind::Quint => 128,
        BiseKind::Bits => 1,
    };
    let matches = |packed: u32| -> bool {
        let got: &[u32] = match kind {
            BiseKind::Trit => &unpack_trits(packed),
            _ => &unpack_quints(packed),
        };
        got.iter().take(present).eq(highs.iter().take(present))
    };
    for packed in 0..limit {
        if packed & forbidden == 0 && matches(packed) {
            return packed;
        }
    }
    for packed in 0..limit {
        if matches(packed) {
            return packed;
        }
    }
    0
}

/// Write `values` as a BISE sequence of the given `range` into `out`, spending exactly
/// [`bise_bits`] bits. Trit/quint groups are laid out in the spec's interleaved chunk
/// order ([`ise_chunks`]); a short final group is truncated at the stream length, so
/// its packed field is chosen with all bits past the stream end clear.
fn bise_write(values: &[u32], range: u32, out: &mut BitVec) -> Result<(), AstcEncodeError> {
    let (kind, bits) = bise_params(range).ok_or(AstcEncodeError::InvalidRange { range })?;
    for &value in values {
        if value >= range {
            return Err(AstcEncodeError::ValueOutOfRange { value, range });
        }
    }
    if kind == BiseKind::Bits {
        for &value in values {
            out.push_bits(value, bits);
        }
        return Ok(());
    }
    let stream_len = bise_bits(values.len(), range)?;
    let group_size = ise_group_size(kind);
    let mask = (1u32 << bits) - 1;
    // Build the abstract full-group stream, then append only its first `stream_len`
    // bits (the decoder zero-fills the rest of the final group).
    let mut abstract_bits: Vec<u32> = Vec::new();
    for group in values.chunks(group_size) {
        let group_start = abstract_bits.len();
        let highs: Vec<u32> = group.iter().map(|v| v >> bits).collect();
        // Packed bits past the stream end must encode as zero.
        let mut forbidden = 0u32;
        let mut pos = group_start;
        for chunk in ise_chunks(kind) {
            match *chunk {
                IseChunk::Plain(_) => pos += bits as usize,
                IseChunk::Packed(b) => {
                    if pos >= stream_len {
                        forbidden |= 1 << b;
                    }
                    pos += 1;
                }
            }
        }
        let packed = ise_pack_highs(kind, &highs, group.len(), forbidden);
        for chunk in ise_chunks(kind) {
            match *chunk {
                IseChunk::Plain(i) => {
                    let m = group.get(i).map_or(0, |v| v & mask);
                    for b in 0..bits {
                        abstract_bits.push((m >> b) & 1);
                    }
                }
                IseChunk::Packed(b) => abstract_bits.push((packed >> b) & 1),
            }
        }
    }
    for &bit in abstract_bits.iter().take(stream_len) {
        out.push_bits(bit, 1);
    }
    Ok(())
}

/// Read `count` BISE integers of the given `range` from `reader`, mirroring
/// [`bise_write`]: consumes exactly [`bise_bits`] bits and zero-fills the rest of
/// the final group, as a decoder does at the end of a block's stream.
fn bise_read(reader: &mut BitReader<'_>, count: usize, range: u32) -> Result<Vec<u32>, AstcEncodeError> {
    let (kind, bits) = bise_params(range).ok_or(AstcEncodeError::InvalidRange { range })?;
    if kind == BiseKind::Bits {
        let mut out = Vec::with_capacity(count);
        for _ in 0..count {
            out.push(reader.read_bits(bits)?);
        }
        return Ok(out);
    }
    let stream_len = bise_bits(count, range)?;
    let mut stream: Vec<u32> = Vec::with_capacity(stream_len);
    for _ in 0..stream_len {
        stream.push(reader.read_bits(1)?);
    }
    let at = |index: usize| stream.get(index).copied().unwrap_or(0);
    let group_size = ise_group_size(kind);
    let group_bits: usize = ise_chunks(kind)
        .iter()
        .map(|c| match c {
            IseChunk::Plain(_) => bits as usize,
            IseChunk::Packed(_) => 1,
        })
        .sum();
    let mut out = Vec::with_capacity(count);
    let mut left = count;
    let mut group_index = 0usize;
    while left > 0 {
        let take = left.min(group_size);
        let base = group_index * group_bits;
        let mut packed = 0u32;
        let mut plain = [0u32; 5];
        let mut pos = base;
        for chunk in ise_chunks(kind) {
            match *chunk {
                IseChunk::Plain(i) => {
                    let mut m = 0u32;
                    for b in 0..bits {
                        m |= at(pos + b as usize) << b;
                    }
                    if i < plain.len() {
                        plain[i] = m;
                    }
                    pos += bits as usize;
                }
                IseChunk::Packed(b) => {
                    packed |= at(pos) << b;
                    pos += 1;
                }
            }
        }
        let highs: &[u32] = match kind {
            BiseKind::Trit => &unpack_trits(packed),
            _ => &unpack_quints(packed),
        };
        for i in 0..take {
            out.push((highs[i] << bits) | plain[i]);
        }
        left -= take;
        group_index += 1;
    }
    Ok(out)
}

/// BISE-encode `values` (each below `range`) into packed LSB-first bytes. The result holds
/// exactly [`bise_bits`] valid bits, zero-padded to a whole byte.
///
/// # Errors
/// [`AstcEncodeError::InvalidRange`] for an unknown range, or
/// [`AstcEncodeError::ValueOutOfRange`] when a value does not fit.
pub fn bise_encode(values: &[u32], range: u32) -> Result<Vec<u8>, AstcEncodeError> {
    let mut out = BitVec::default();
    bise_write(values, range, &mut out)?;
    Ok(out.bytes)
}

/// Decode `count` BISE integers of the given `range` from packed LSB-first `bytes`
/// (the inverse of [`bise_encode`]).
///
/// # Errors
/// [`AstcEncodeError::InvalidRange`] for an unknown range, or
/// [`AstcEncodeError::TruncatedBits`] when `bytes` holds fewer than [`bise_bits`] bits.
pub fn bise_decode(bytes: &[u8], count: usize, range: u32) -> Result<Vec<u32>, AstcEncodeError> {
    // Validate the range even when `count` is 0.
    let _ = bise_bits(count, range)?;
    let mut reader = BitReader::new(bytes);
    bise_read(&mut reader, count, range)
}

// ---------------------------------------------------------------------------
// Unquantization (quantized integer -> signal value)
// ---------------------------------------------------------------------------

/// Replicate the `n`-bit value `q` into `out_bits` by repeating its bit pattern from
/// the most significant side (the spec's expansion for bit-only ranges).
fn bit_replicate(q: u32, n: u32, out_bits: u32) -> u32 {
    if n == 0 || out_bits == 0 {
        return 0;
    }
    let mut out = 0u32;
    for i in 0..out_bits {
        let bit = (q >> (n - 1 - (i % n))) & 1;
        out |= bit << (out_bits - 1 - i);
    }
    out
}

/// The spec's multiplier-free unquantization core shared by color endpoints (9-bit
/// `A`/`B`, top-bit mask 0x80) and weights (7-bit, mask 0x20): `D` is the trit/quint,
/// `m` the plain bits whose lowest bit is `a`. `pattern` is the table's `B` layout,
/// most significant digit first, with letters naming the bits of `m` (`a` = bit 0).
/// The mapping scrambles the order of values in `q`, exactly as the spec warns;
/// encoders compensate by searching this table (see [`quantize8`]).
fn unquant_scramble(q: u32, bits: u32, c: u32, pattern: &str, wide: u32, top: u32) -> u32 {
    let d = q >> bits;
    let m = if bits == 0 { 0 } else { q & ((1 << bits) - 1) };
    let a = if m & 1 == 1 { (1 << wide) - 1 } else { 0 };
    let mut b = 0u32;
    for ch in pattern.chars() {
        let bit = match ch {
            '0' => 0,
            '1' => 1,
            letter => (m >> (letter as u32 - 'a' as u32)) & 1,
        };
        b = (b << 1) | bit;
    }
    let unq = (d * c + b) ^ a;
    (a & top) | (unq >> 2)
}

/// Expand quantized level `q` of `range` to an 8-bit endpoint value (0..=255), per the
/// spec's color unquantization: bit replication for power-of-two ranges and the
/// parameter-table scramble for trit/quint ranges. `range < 2` yields 0 and an
/// out-of-range `q` is clamped, so corrupt blocks still decode to something defined.
fn unquantize8(q: u32, range: u32) -> u32 {
    if range < 2 {
        return 0;
    }
    let q = q.min(range - 1);
    match range {
        3 => [0, 128, 255][q as usize],
        5 => (q * 255 + 2) / 4,
        6 => unquant_scramble(q, 1, 204, "000000000", 9, 0x80),
        10 => unquant_scramble(q, 1, 113, "000000000", 9, 0x80),
        12 => unquant_scramble(q, 2, 93, "b000b0bb0", 9, 0x80),
        20 => unquant_scramble(q, 2, 54, "b0000bb00", 9, 0x80),
        24 => unquant_scramble(q, 3, 44, "cb000cbcb", 9, 0x80),
        40 => unquant_scramble(q, 3, 26, "cb0000cbc", 9, 0x80),
        48 => unquant_scramble(q, 4, 22, "dcb000dcb", 9, 0x80),
        80 => unquant_scramble(q, 4, 13, "dcb0000dc", 9, 0x80),
        96 => unquant_scramble(q, 5, 11, "edcb000ed", 9, 0x80),
        160 => unquant_scramble(q, 5, 6, "edcb0000e", 9, 0x80),
        192 => unquant_scramble(q, 6, 5, "fedcb000f", 9, 0x80),
        _ => bit_replicate(q, range.trailing_zeros(), 8),
    }
}

/// The stored level of `range` whose [`unquantize8`] value is nearest to `v8`. The
/// unquantization scramble makes stored order meaningless, so this is an exhaustive
/// search; ties go to the lower unquantized value, then the lower level.
fn quantize8(v8: u32, range: u32) -> u32 {
    if range < 2 {
        return 0;
    }
    let mut best = (u32::MAX, u32::MAX, 0u32);
    for q in 0..range {
        let v = unquantize8(q, range);
        let err = v.abs_diff(v8);
        if err < best.0 || (err == best.0 && (v, q) < (best.1, best.2)) {
            best = (err, v, q);
        }
    }
    best.2
}

/// Expand a quantized weight level to the decoder's 0..=64 weight scale: unquantize to
/// 0..=63 (bit replication or the weight parameter table), then expand values above
/// 32 by one so the top level lands on 64, per the spec.
fn weight_unquantize64(q: u32, range: u32) -> u32 {
    if range < 2 {
        return 0;
    }
    let q = q.min(range - 1);
    let unq63 = match range {
        3 => [0, 32, 63][q as usize],
        5 => [0, 16, 32, 47, 63][q as usize],
        6 => unquant_scramble(q, 1, 50, "0000000", 7, 0x20),
        10 => unquant_scramble(q, 1, 28, "0000000", 7, 0x20),
        12 => unquant_scramble(q, 2, 23, "b000b0b", 7, 0x20),
        20 => unquant_scramble(q, 2, 13, "b0000b0", 7, 0x20),
        24 => unquant_scramble(q, 3, 11, "cb000cb", 7, 0x20),
        _ => bit_replicate(q, range.trailing_zeros(), 6),
    };
    if unq63 > 32 { unq63 + 1 } else { unq63 }
}

/// A 16-bit signal value to 8 bits, round to nearest (exact for `v * 257` inputs).
fn to_u8(v16: u32) -> u8 {
    ((v16.min(65535) * 255 + 32767) / 65535) as u8
}

/// An 8-bit channel to its exact 16-bit form (`v * 257`, so [`to_u8`] returns it unchanged).
fn to_u16(v: u8) -> u32 {
    u32::from(v) * 257
}

/// Interpolate two endpoint colors (8-bit component values, as endpoint decoding
/// produces them) at weight `w` (0..=64) to an RGBA8 texel, by the spec's weight
/// application for `decode_unorm8`: expand each component to 16 bits by replication,
/// take `(c0 * (64 - w) + c1 * w + 32) / 64`, and keep the top 8 bits. This is the
/// one interpolation both the decoder and the encoder's scoring use.
fn interpolate_texel(e0: [u32; 4], e1: [u32; 4], w: u32) -> [u8; 4] {
    let w = w.min(64);
    let mut out = [0u8; 4];
    for (slot, (a, b)) in out.iter_mut().zip(e0.iter().zip(e1.iter())) {
        let a16 = (a << 8) | a;
        let b16 = (b << 8) | b;
        let v = (a16 * (64 - w) + b16 * w + 32) >> 6;
        *slot = (v >> 8) as u8;
    }
    out
}

// ---------------------------------------------------------------------------
// Physical block layout (128 bits, LSB first)
// ---------------------------------------------------------------------------
//
// ```text
// bits  [0:11]   block mode (see `decode_block_mode` for the layout table)
// bits [11:13]   partition count - 1 (always 0 here: one partition)
// bits [13:17]   color endpoint mode (CEM)
// bits [17 .. 128-W]  the endpoint BISE stream (W = weight bit count)
// bits [128-W .. 128) the weight BISE stream: stream bit i at physical bit 127 - i
// ```
//
// Void-extent blocks (constant color) replace this layout: bits [0:9] hold `0b111111100`,
// the extent fields are all ones (the whole block is covered) with the dynamic-range bit at
// [9] clear for LDR, and bits [64:128] hold the constant RGBA color as four 16-bit values.

/// Low bits of a void-extent block: bits [0:9] equal this pattern.
const VOID_EXTENT_PATTERN: u32 = 0b1_1111_1100;
/// Mask for the void-extent test.
const VOID_EXTENT_MASK: u32 = 0x1FF;
/// Bits of the 128 available to endpoint + weight data with one partition: every bit from
/// bit 17 to bit 127 (128 minus block mode (11), partition count (2) and CEM (4)).
/// Endpoint bits = this minus the weight-bit count.
const DATA_BITS_ONE_PARTITION: usize = 111;
/// The color undecodable blocks produce (the format's error color).
const ERROR_TEXEL: [u8; 4] = [255, 0, 255, 255];

/// Read `count` bits starting at bit `pos` of the block (LSB first). Callers keep
/// `pos + count <= 128`.
fn get_bits(block: &[u8; 16], pos: usize, count: u32) -> u32 {
    let mut value = 0u32;
    for i in 0..count as usize {
        let index = pos + i;
        let bit = u32::from((block[index / 8] >> (index % 8)) & 1);
        value |= bit << i;
    }
    value
}

/// Write the low `count` bits of `value` starting at bit `pos` (LSB first).
fn set_bits(block: &mut [u8; 16], pos: usize, count: u32, value: u32) {
    for i in 0..count as usize {
        let index = pos + i;
        if (value >> i) & 1 == 1 {
            block[index / 8] |= 1 << (index % 8);
        }
    }
}

/// A decoded block-mode field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BlockModeDecoded {
    /// Weight grid width.
    grid_w: u32,
    /// Weight grid height.
    grid_h: u32,
    /// Weight quantization range (one of [`WEIGHT_RANGES`]).
    weight_range: u32,
    /// Whether the block carries a second (dual) weight plane.
    dual_plane: bool,
}

/// The weight range for block-mode range value `rho` and precision bit `p`, or `None`
/// for the invalid `rho` values 0 and 1.
fn weight_range_of(rho: u32, p: bool) -> Option<u32> {
    let index = rho.checked_sub(2)? as usize + if p { 6 } else { 0 };
    WEIGHT_RANGES.get(index).copied()
}

/// The (`rho`, precision bit) pair storing `range`, or `None` when it is not a weight range.
fn weight_range_index(range: u32) -> Option<(u32, bool)> {
    WEIGHT_RANGES.iter().position(|&r| r == range).map(|i| {
        if i < 6 {
            (i as u32 + 2, false)
        } else {
            ((i - 6) as u32 + 2, true)
        }
    })
}

/// Pack a block mode for a `grid_w`×`grid_h` single-plane weight grid at `weight_range`,
/// following the spec's 2D block-mode layout table (see [`decode_block_mode`]). Rows are
/// tried in table order, then the square-grid row that forces the precision bit to 0,
/// then the fixed-size rows. Returns `None` when no row can express the request
/// (including grids below 2 in either dimension, which the format cannot encode).
fn encode_block_mode(grid_w: u32, grid_h: u32, weight_range: u32) -> Option<u32> {
    let (rho, p) = weight_range_index(weight_range)?;
    let (r0, r1, r2) = (rho & 1, (rho >> 1) & 1, (rho >> 2) & 1);
    let head = (u32::from(p) << 9) | (r0 << 4) | (r2 << 1) | r1;
    // Rows 1-5: bits [1:0] hold rho[2:1], bits [3:2] select the row.
    if (4..=7).contains(&grid_w) && (2..=5).contains(&grid_h) {
        return Some(head | ((grid_w - 4) << 7) | ((grid_h - 2) << 5));
    }
    if (8..=11).contains(&grid_w) && (2..=5).contains(&grid_h) {
        return Some(head | ((grid_w - 8) << 7) | ((grid_h - 2) << 5) | (1 << 2));
    }
    if (2..=5).contains(&grid_w) && (8..=11).contains(&grid_h) {
        return Some(head | ((grid_h - 8) << 7) | ((grid_w - 2) << 5) | (1 << 3));
    }
    if (2..=5).contains(&grid_w) && (6..=7).contains(&grid_h) {
        return Some(head | ((grid_h - 6) << 7) | ((grid_w - 2) << 5) | (0b11 << 2));
    }
    if (2..=3).contains(&grid_w) && (2..=5).contains(&grid_h) {
        return Some(head | (1 << 8) | ((grid_w - 2) << 7) | ((grid_h - 2) << 5) | (0b11 << 2));
    }
    // Square-grid row: bits [10:9] hold H (dual-plane and precision forced to 0),
    // bit [8] = 1, bit [7] = 0, rho in bits [4] and [3:2], bits [1:0] = 0.
    if !p && (6..=9).contains(&grid_w) && (6..=9).contains(&grid_h) {
        return Some(
            ((grid_h - 6) << 9) | (1 << 8) | ((grid_w - 6) << 5) | (r0 << 4) | (r2 << 3) | (r1 << 2),
        );
    }
    // Rows with a fixed 12 (or the 6x10 / 10x6 pair): rho in bits [4] and [3:2].
    let tail = (u32::from(p) << 9) | (r0 << 4) | (r2 << 3) | (r1 << 2);
    if grid_w == 12 && (2..=5).contains(&grid_h) {
        return Some(tail | ((grid_h - 2) << 5));
    }
    if grid_h == 12 && (2..=5).contains(&grid_w) {
        return Some(tail | (1 << 7) | ((grid_w - 2) << 5));
    }
    if (grid_w, grid_h) == (6, 10) {
        return Some(tail | (0b11 << 7));
    }
    if (grid_w, grid_h) == (10, 6) {
        return Some(tail | (0b11 << 7) | (1 << 5));
    }
    None
}

/// Unpack a block mode per the spec's 2D layout table, or `None` for void-extent,
/// reserved, and invalid-`rho` patterns. Grid dimensions above the block footprint
/// are reported faithfully; the caller rejects them.
///
/// Bit 10 is the dual-plane flag and bit 9 the weight precision bit in most rows;
/// `rho` (the 3-bit weight range value, at least 2 in valid modes) is scattered, with
/// rho\[0\] at bit 4 and rho\[2:1\] at bits 1:0 or 3:2 depending on the row.
fn decode_block_mode(mode: u32) -> Option<BlockModeDecoded> {
    let bit = |i: u32| (mode >> i) & 1;
    let field = |hi: u32, lo: u32| (mode >> lo) & ((1 << (hi - lo + 1)) - 1);
    // Void-extent pattern (reported by the caller before this) and reserved patterns.
    if mode & 0x1FF == VOID_EXTENT_PATTERN || mode & 0xF == 0 {
        return None;
    }
    if field(8, 6) == 0b111 && field(1, 0) == 0 {
        return None;
    }
    let r0 = bit(4);
    if field(1, 0) != 0 {
        // Rows 1-5.
        let rho = (bit(1) << 2) | (bit(0) << 1) | r0;
        let dual_plane = bit(10) == 1;
        let range = weight_range_of(rho, bit(9) == 1)?;
        let (grid_w, grid_h) = match field(3, 2) {
            0b00 => (field(8, 7) + 4, field(6, 5) + 2),
            0b01 => (field(8, 7) + 8, field(6, 5) + 2),
            0b10 => (field(6, 5) + 2, field(8, 7) + 8),
            _ if bit(8) == 0 => (field(6, 5) + 2, bit(7) + 6),
            _ => (bit(7) + 2, field(6, 5) + 2),
        };
        return Some(BlockModeDecoded {
            grid_w,
            grid_h,
            weight_range: range,
            dual_plane,
        });
    }
    // Rows 6-10: rho[2:1] at bits 3:2.
    let rho = (bit(3) << 2) | (bit(2) << 1) | r0;
    match field(8, 7) {
        0b10 => Some(BlockModeDecoded {
            grid_w: field(6, 5) + 6,
            grid_h: field(10, 9) + 6,
            weight_range: weight_range_of(rho, false)?,
            dual_plane: false,
        }),
        0b00 => Some(BlockModeDecoded {
            grid_w: 12,
            grid_h: field(6, 5) + 2,
            weight_range: weight_range_of(rho, bit(9) == 1)?,
            dual_plane: bit(10) == 1,
        }),
        0b01 => Some(BlockModeDecoded {
            grid_w: field(6, 5) + 2,
            grid_h: 12,
            weight_range: weight_range_of(rho, bit(9) == 1)?,
            dual_plane: bit(10) == 1,
        }),
        _ => {
            let (grid_w, grid_h) = match field(6, 5) {
                0b00 => (6, 10),
                0b01 => (10, 6),
                _ => return None,
            };
            Some(BlockModeDecoded {
                grid_w,
                grid_h,
                weight_range: weight_range_of(rho, bit(9) == 1)?,
                dual_plane: bit(10) == 1,
            })
        }
    }
}

/// The endpoint quantization range a decoder derives: the largest range whose BISE encoding
/// of `value_count` integers fits `available_bits`. `None` when even range 2 does not fit.
fn endpoint_range_for(value_count: usize, available_bits: usize) -> Option<u32> {
    QUANT_RANGES
        .iter()
        .rev()
        .copied()
        .find(|&range| bise_bits(value_count, range).is_ok_and(|bits| bits <= available_bits))
}

/// Endpoint bits a single-partition block has when its weights spend `weight_bits`.
fn endpoint_bits_for(weight_bits: usize) -> Option<usize> {
    DATA_BITS_ONE_PARTITION.checked_sub(weight_bits)
}

/// The minimum endpoint bits a legal block must leave for `value_count` endpoint
/// integers: `ceil(13 * C / 5)`. Below this the endpoint range would fall under 6,
/// which the format does not support, so the block is illegal.
fn min_endpoint_bits(value_count: usize) -> usize {
    (13 * value_count).div_ceil(5)
}

/// Whether a weight configuration is legal: at most 64 weights, and their integer
/// sequence encoding spends between 24 and 96 bits (the spec's illegal-encoding list).
fn weight_config_legal(weight_count: usize, weight_bits: usize) -> bool {
    weight_count <= 64 && (24..=96).contains(&weight_bits)
}

/// Pack a void-extent block holding constant `color` (exact: every texel decodes to it).
fn pack_void_extent(color: [u8; 4]) -> [u8; 16] {
    let mut block = [0xFFu8; 16];
    // Bits [0:9] = pattern, bit [9] (dynamic range) = 0 for LDR; extents stay all ones.
    block[0] = 0b1111_1100;
    block[1] = 0b1111_1101;
    for (i, channel) in color.iter().enumerate() {
        let v = to_u16(*channel);
        block[8 + i * 2] = (v & 0xFF) as u8;
        block[8 + i * 2 + 1] = (v >> 8) as u8;
    }
    block
}

/// Whether `block` has the void-extent low-bit pattern.
fn is_void_extent(block: &[u8; 16]) -> bool {
    get_bits(block, 0, 9) & VOID_EXTENT_MASK == VOID_EXTENT_PATTERN
}

/// The constant color of a void-extent block (call only when [`is_void_extent`] holds).
fn void_extent_color(block: &[u8; 16]) -> [u8; 4] {
    let mut color = [0u8; 4];
    for (i, channel) in color.iter_mut().enumerate() {
        *channel = to_u8(get_bits(block, 64 + i * 16, 16));
    }
    color
}

// ---------------------------------------------------------------------------
// Symbolic blocks: packing and the reference decoder
// ---------------------------------------------------------------------------

/// One fully decided single-partition block: everything except the physical bits. The
/// endpoint range is not stored here; it follows from the weight bit count and the mode's
/// value count ([`endpoint_range_for`]), exactly as a decoder derives it.
#[derive(Clone, Debug)]
struct SymbolicBlock {
    /// Endpoint mode.
    cem: ColorEndpointMode,
    /// Weight quantization range.
    weight_range: u32,
    /// Quantized endpoint integers, per-channel pairs, length `cem.value_count()`.
    endpoint_q: Vec<u32>,
    /// Quantized weights, row-major over the weight grid (which is the block footprint).
    weight_q: Vec<u32>,
}

/// The spec's blue contraction, applied by a decoder to both RGB endpoints when the
/// stored order's RGB sums decrease (`s1 < s0`): red and green are averaged toward
/// blue. Encoders choose whether to use it (it can express near-grey endpoints at
/// half-step precision); this encoder instead stores endpoints in non-decreasing
/// RGB-sum order, so its endpoints decode direct and exact.
fn blue_contract(color: [u32; 4]) -> [u32; 4] {
    let [r, g, b, a] = color;
    [(r + b) >> 1, (g + b) >> 1, b, a]
}

/// Build the two endpoints (8-bit component values) from unquantized endpoint values
/// `v` (per-channel pairs) in mode `cem`. For the RGB modes, a decoder compares the
/// RGB sums `s0 = v0+v2+v4` and `s1 = v1+v3+v5`: if `s1 >= s0` the pairs decode in
/// stored order, otherwise the endpoints swap and both are blue-contracted. Missing
/// values read as 0, so a truncated slice still produces defined endpoints.
fn endpoints_from_values(cem: ColorEndpointMode, v: &[u32]) -> ([u32; 4], [u32; 4]) {
    let at = |i: usize| v.get(i).copied().unwrap_or(0);
    match cem {
        ColorEndpointMode::Luminance => ([at(0), at(0), at(0), 255], [at(1), at(1), at(1), 255]),
        ColorEndpointMode::LuminanceAlpha => ([at(0), at(0), at(0), at(2)], [at(1), at(1), at(1), at(3)]),
        ColorEndpointMode::Rgb | ColorEndpointMode::Rgba => {
            let alpha = |i: usize| {
                if cem == ColorEndpointMode::Rgba {
                    at(i)
                } else {
                    255
                }
            };
            let first = [at(0), at(2), at(4), alpha(6)];
            let second = [at(1), at(3), at(5), alpha(7)];
            let s0 = at(0) + at(2) + at(4);
            let s1 = at(1) + at(3) + at(5);
            if s1 >= s0 {
                (first, second)
            } else {
                (blue_contract(second), blue_contract(first))
            }
        }
    }
}

/// Pack a symbolic block into its 16 physical bytes for footprint `size`. The caller must
/// have quantized endpoints at the range [`endpoint_range_for`] derives for this block and
/// kept every weight below `weight_range`; on any inconsistency this returns `None` instead
/// of emitting a block a decoder would read differently.
fn pack_symbolic(sym: &SymbolicBlock, size: BlockSize) -> Option<[u8; 16]> {
    let grid_w = size.width();
    let grid_h = size.height();
    let weight_count = (grid_w * grid_h) as usize;
    if sym.weight_q.len() != weight_count || sym.endpoint_q.len() != sym.cem.value_count() {
        return None;
    }
    let weight_bits = bise_bits(weight_count, sym.weight_range).ok()?;
    if !weight_config_legal(weight_count, weight_bits) {
        return None;
    }
    let available = endpoint_bits_for(weight_bits)?;
    if available < min_endpoint_bits(sym.cem.value_count()) {
        return None;
    }
    let endpoint_range = endpoint_range_for(sym.cem.value_count(), available)?;
    let mode = encode_block_mode(grid_w, grid_h, sym.weight_range)?;

    let mut endpoint_stream = BitVec::default();
    bise_write(&sym.endpoint_q, endpoint_range, &mut endpoint_stream).ok()?;
    let mut weight_stream = BitVec::default();
    bise_write(&sym.weight_q, sym.weight_range, &mut weight_stream).ok()?;

    if endpoint_stream.nbits + weight_bits > DATA_BITS_ONE_PARTITION {
        return None;
    }
    let mut block = [0u8; 16];
    set_bits(&mut block, 0, 11, mode);
    // Partition count - 1 at [11:13] stays 0 (one partition).
    set_bits(&mut block, 13, 4, sym.cem.cem_bits());
    // Endpoint stream: contiguous from bit 17. The derived range fits by construction,
    // so these never reach the weight bits at the top of the block.
    for i in 0..endpoint_stream.nbits {
        set_bits(&mut block, 17 + i, 1, endpoint_stream.bit(i));
    }
    // Weight stream: logical bit i at physical bit 127 - i.
    for i in 0..weight_stream.nbits {
        set_bits(&mut block, 127 - i, 1, weight_stream.bit(i));
    }
    Some(block)
}

/// Gather `count` endpoint-stream bits from their physical home (contiguous from bit 17)
/// into a reader-ready [`BitVec`].
fn gather_endpoint_stream(block: &[u8; 16], count: usize) -> BitVec {
    let mut stream = BitVec::default();
    for i in 0..count {
        let physical = 17 + i;
        let bit = if physical < 128 {
            get_bits(block, physical, 1)
        } else {
            0
        };
        stream.push_bits(bit, 1);
    }
    stream
}

/// Gather `count` weight-stream bits from the top of the block down into a [`BitVec`].
fn gather_weight_stream(block: &[u8; 16], count: usize) -> BitVec {
    let mut stream = BitVec::default();
    for i in 0..count.min(128) {
        stream.push_bits(get_bits(block, 127 - i, 1), 1);
    }
    stream
}

/// Sample a weight grid (`grid_w`×`grid_h` values in 0..=64, row-major) at texel
/// `(x, y)` of a `foot_w`×`foot_h` footprint, by the spec's 2D weight infill exactly:
/// scale the texel coordinate by `D = (1024 + foot/2) / (foot - 1)`, map into
/// weight-grid space with `(c * (cells - 1) + 32) >> 6` as a 4.4 fixed-point
/// coordinate, and bilinearly blend the four surrounding weights with the spec's
/// sixteenth weights. Grid corners sit on corner texels, so a grid the size of the
/// footprint is the identity at every footprint this crate encodes.
fn sample_weight_grid(
    grid: &[u32],
    grid_w: u32,
    grid_h: u32,
    foot_w: u32,
    foot_h: u32,
    x: u32,
    y: u32,
) -> u32 {
    if grid.is_empty() || grid_w == 0 || grid_h == 0 {
        return 0;
    }
    // (Integral cell, fraction in sixteenths) along one axis.
    let axis = |pos: u32, foot: u32, cells: u32| -> (u32, u32) {
        if foot <= 1 || cells <= 1 {
            return (0, 0);
        }
        let d = (1024 + foot / 2) / (foot - 1);
        let g = (d * pos * (cells - 1) + 32) >> 6;
        (g >> 4, g & 0x0F)
    };
    let (js, fs) = axis(x, foot_w, grid_w);
    let (jt, ft) = axis(y, foot_h, grid_h);
    let v0 = (js + jt * grid_w) as usize;
    // The spec reads the four corners at v0, v0+1, v0+W, v0+W+1; on the last texel of
    // an axis the fraction is 0, so clamped fetches there are multiplied by zero.
    let at = |index: usize| grid.get(index.min(grid.len() - 1)).copied().unwrap_or(0);
    let p00 = at(v0);
    let p01 = at(v0 + 1);
    let p10 = at(v0 + grid_w as usize);
    let p11 = at(v0 + grid_w as usize + 1);
    let w11 = (fs * ft + 8) >> 4;
    let w10 = ft - w11;
    let w01 = fs - w11;
    let w00 = 16 + w11 - fs - ft;
    (p00 * w00 + p01 * w01 + p10 * w10 + p11 * w11 + 8) >> 4
}

/// What sort of block [`inspect_block`] found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockKind {
    /// A void-extent block: every texel is the stored constant color.
    VoidExtent,
    /// A normal single-partition block in one of the four direct endpoint modes.
    Normal,
    /// Anything else (reserved mode, multiple partitions, dual plane, grid larger than the
    /// footprint, or an endpoint range that cannot fit). Decodes as the error color.
    Invalid,
}

/// A report of how a block is laid out, from [`inspect_block`]. Fields that do not apply to
/// the block's kind are `None`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BlockInfo {
    /// The kind of block.
    pub kind: BlockKind,
    /// Endpoint mode (normal blocks).
    pub cem: Option<ColorEndpointMode>,
    /// Weight quantization range (normal blocks).
    pub weight_range: Option<u32>,
    /// Derived endpoint quantization range (normal blocks).
    pub endpoint_range: Option<u32>,
    /// Constant RGBA8 color (void-extent blocks).
    pub constant_color: Option<[u8; 4]>,
}

/// Inspect a block without decoding texels: its kind and, for normal blocks, the endpoint
/// mode and the weight/endpoint ranges a decoder derives. Useful to validate encoder output.
pub fn inspect_block(block: &[u8; 16], size: BlockSize) -> BlockInfo {
    let invalid = BlockInfo {
        kind: BlockKind::Invalid,
        cem: None,
        weight_range: None,
        endpoint_range: None,
        constant_color: None,
    };
    if is_void_extent(block) {
        return BlockInfo {
            kind: BlockKind::VoidExtent,
            cem: None,
            weight_range: None,
            endpoint_range: None,
            constant_color: Some(void_extent_color(block)),
        };
    }
    let Some(mode) = decode_block_mode(get_bits(block, 0, 11)) else {
        return invalid;
    };
    if mode.dual_plane || mode.grid_w > size.width() || mode.grid_h > size.height() {
        return invalid;
    }
    if get_bits(block, 11, 2) != 0 {
        return invalid;
    }
    let Some(cem) = ColorEndpointMode::from_cem_bits(get_bits(block, 13, 4)) else {
        return invalid;
    };
    let weight_count = (mode.grid_w * mode.grid_h) as usize;
    let Ok(weight_bits) = bise_bits(weight_count, mode.weight_range) else {
        return invalid;
    };
    if !weight_config_legal(weight_count, weight_bits) {
        return invalid;
    }
    let Some(available) = endpoint_bits_for(weight_bits) else {
        return invalid;
    };
    if available < min_endpoint_bits(cem.value_count()) {
        return invalid;
    }
    let Some(endpoint_range) = endpoint_range_for(cem.value_count(), available) else {
        return invalid;
    };
    BlockInfo {
        kind: BlockKind::Normal,
        cem: Some(cem),
        weight_range: Some(mode.weight_range),
        endpoint_range: Some(endpoint_range),
        constant_color: None,
    }
}

/// Decode one block to `size.texel_count()` RGBA8 texels in row-major order
/// (`y * width + x`). This reference decoder covers what the encoder emits (see the module
/// docs); void-extent blocks fill with their constant color and anything else outside the
/// subset fills with opaque magenta ([`BlockKind::Invalid`] in [`inspect_block`] terms).
pub fn decode_block(block: &[u8; 16], size: BlockSize) -> Vec<[u8; 4]> {
    let count = size.texel_count();
    let info = inspect_block(block, size);
    match info.kind {
        BlockKind::VoidExtent => {
            return vec![info.constant_color.unwrap_or(ERROR_TEXEL); count];
        }
        BlockKind::Invalid => return vec![ERROR_TEXEL; count],
        BlockKind::Normal => {}
    }
    let (Some(cem), Some(weight_range), Some(endpoint_range)) =
        (info.cem, info.weight_range, info.endpoint_range)
    else {
        return vec![ERROR_TEXEL; count];
    };
    let Some(mode) = decode_block_mode(get_bits(block, 0, 11)) else {
        return vec![ERROR_TEXEL; count];
    };

    let endpoint_count = cem.value_count();
    let Ok(endpoint_bits) = bise_bits(endpoint_count, endpoint_range) else {
        return vec![ERROR_TEXEL; count];
    };
    let endpoint_stream = gather_endpoint_stream(block, endpoint_bits);
    let mut reader = BitReader::new(&endpoint_stream.bytes);
    reader.nbits = endpoint_stream.nbits;
    let Ok(endpoint_q) = bise_read(&mut reader, endpoint_count, endpoint_range) else {
        return vec![ERROR_TEXEL; count];
    };
    let values: Vec<u32> = endpoint_q
        .iter()
        .map(|&q| unquantize8(q, endpoint_range))
        .collect();
    let (e0, e1) = endpoints_from_values(cem, &values);

    let weight_count = (mode.grid_w * mode.grid_h) as usize;
    let Ok(weight_bits) = bise_bits(weight_count, weight_range) else {
        return vec![ERROR_TEXEL; count];
    };
    let weight_stream = gather_weight_stream(block, weight_bits);
    let mut reader = BitReader::new(&weight_stream.bytes);
    reader.nbits = weight_stream.nbits;
    let Ok(weight_q) = bise_read(&mut reader, weight_count, weight_range) else {
        return vec![ERROR_TEXEL; count];
    };
    let grid: Vec<u32> = weight_q
        .iter()
        .map(|&q| weight_unquantize64(q, weight_range))
        .collect();

    let mut out = Vec::with_capacity(count);
    for y in 0..size.height() {
        for x in 0..size.width() {
            let w = sample_weight_grid(&grid, mode.grid_w, mode.grid_h, size.width(), size.height(), x, y);
            out.push(interpolate_texel(e0, e1, w));
        }
    }
    out
}

/// Decode a whole image of blocks (row-major, 16 bytes each) to RGBA8. Edge blocks that
/// overhang the image are decoded in full and clipped, mirroring [`encode_image`]'s
/// edge-replicated padding.
///
/// # Errors
/// [`AstcEncodeError::InvalidDimensions`] for a zero dimension, or
/// [`AstcEncodeError::BadBlocks`] when `blocks` is not the number of blocks the size needs.
pub fn decode_image(
    width: u32,
    height: u32,
    blocks: &[u8],
    size: BlockSize,
) -> Result<Vec<u8>, AstcEncodeError> {
    if width == 0 || height == 0 {
        return Err(AstcEncodeError::InvalidDimensions { width, height });
    }
    let blocks_x = width.div_ceil(size.width());
    let blocks_y = height.div_ceil(size.height());
    let expected = blocks_x as usize * blocks_y as usize;
    if blocks.len() != expected * 16 {
        return Err(AstcEncodeError::BadBlocks {
            len: blocks.len(),
            blocks: expected,
        });
    }
    let mut out = vec![0u8; width as usize * height as usize * 4];
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let at = ((by * blocks_x + bx) * 16) as usize;
            let mut raw = [0u8; 16];
            raw.copy_from_slice(&blocks[at..at + 16]);
            let texels = decode_block(&raw, size);
            for y in 0..size.height() {
                for x in 0..size.width() {
                    let (px, py) = (bx * size.width() + x, by * size.height() + y);
                    if px < width && py < height {
                        let src = texels[(y * size.width() + x) as usize];
                        let dst = ((py * width + px) * 4) as usize;
                        out[dst..dst + 4].copy_from_slice(&src);
                    }
                }
            }
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Encoder: endpoint line fitting
// ---------------------------------------------------------------------------

/// Texels projected into the space an endpoint mode fits: returns the points (coordinates in
/// 0..=255, only the first `dims` of each point are meaningful) and the dimension count.
/// Luminance modes fit the channel mean, which is what a grey decode reproduces best.
fn fit_points(cem: ColorEndpointMode, texels: &[[u8; 4]]) -> (Vec<[f32; 4]>, usize) {
    let lum = |p: &[u8; 4]| (f32::from(p[0]) + f32::from(p[1]) + f32::from(p[2])) / 3.0;
    match cem {
        ColorEndpointMode::Luminance => (texels.iter().map(|p| [lum(p), 0.0, 0.0, 0.0]).collect(), 1),
        ColorEndpointMode::LuminanceAlpha => (
            texels
                .iter()
                .map(|p| [lum(p), f32::from(p[3]), 0.0, 0.0])
                .collect(),
            2,
        ),
        ColorEndpointMode::Rgb => (
            texels
                .iter()
                .map(|p| [f32::from(p[0]), f32::from(p[1]), f32::from(p[2]), 0.0])
                .collect(),
            3,
        ),
        ColorEndpointMode::Rgba => (
            texels
                .iter()
                .map(|p| [f32::from(p[0]), f32::from(p[1]), f32::from(p[2]), f32::from(p[3])])
                .collect(),
            4,
        ),
    }
}

/// Clamp a fitted coordinate into the texel range, mapping any non-finite value to 0. Fitted
/// values derive from `u8` texels and guarded divisions, so this is a belt-and-braces guard
/// that keeps the encoder total on every input.
fn fit_clamp(v: f32) -> f32 {
    if v.is_finite() { v.clamp(0.0, 255.0) } else { 0.0 }
}

/// A fitted endpoint line in fit space. Per-texel weights were refined against these
/// endpoints while fitting; weight *quantization* then re-derives the best stored level per
/// texel from the decoded color, so only the endpoints leave the fit.
struct FitLine {
    /// First endpoint (weight 0), first `dims` coordinates meaningful.
    e0: [f32; 4],
    /// Second endpoint (weight 1).
    e1: [f32; 4],
}

/// Least-squares endpoints for `points` given per-texel `weights` in 0..=1: one 2×2
/// solve per dimension, sharing the Gram matrix of the `(1-w, w)` basis. Writes into
/// `e0`/`e1` (gamut-clamped) and returns whether the system was well-conditioned;
/// on a degenerate weight assignment the endpoints are left unchanged.
fn solve_endpoints(
    points: &[[f32; 4]],
    dims: usize,
    weights: &[f32],
    e0: &mut [f32; 4],
    e1: &mut [f32; 4],
) -> bool {
    let (mut s00, mut s01, mut s11) = (0f32, 0f32, 0f32);
    let (mut b0, mut b1) = ([0f32; 4], [0f32; 4]);
    for (p, &w) in points.iter().zip(weights.iter()) {
        let u = 1.0 - w;
        s00 += u * u;
        s01 += u * w;
        s11 += w * w;
        for i in 0..dims {
            b0[i] += u * p[i];
            b1[i] += w * p[i];
        }
    }
    let det = s00 * s11 - s01 * s01;
    if !det.is_finite() || det.abs() <= 1e-9 {
        return false;
    }
    for i in 0..dims {
        e0[i] = fit_clamp((b0[i] * s11 - b1[i] * s01) / det);
        e1[i] = fit_clamp((s00 * b1[i] - s01 * b0[i]) / det);
    }
    true
}

/// Fit the endpoint line: principal axis by power iteration on the covariance matrix (fixed
/// iteration count, start at the largest-variance axis), endpoints at the projection
/// extremes, then `ls_iters` rounds of alternating least squares (endpoints given weights,
/// then weights given endpoints), clamping into gamut each round.
fn fit_line(points: &[[f32; 4]], dims: usize, ls_iters: usize) -> FitLine {
    let n = points.len().max(1) as f32;
    let mut mean = [0f32; 4];
    for p in points {
        for (m, &c) in mean.iter_mut().zip(p.iter()).take(dims) {
            *m += c;
        }
    }
    for m in mean.iter_mut().take(dims) {
        *m /= n;
    }

    // Covariance (un-normalized; scale is irrelevant to the axis).
    let mut cov = [[0f32; 4]; 4];
    for p in points {
        let d: [f32; 4] = [p[0] - mean[0], p[1] - mean[1], p[2] - mean[2], p[3] - mean[3]];
        for (i, row) in cov.iter_mut().enumerate().take(dims) {
            for (slot, &dc) in row.iter_mut().zip(d.iter()).take(dims) {
                *slot += d[i] * dc;
            }
        }
    }

    // Principal axis: start on the largest diagonal (first such axis on a tie).
    let mut axis = [0f32; 4];
    let mut start = 0usize;
    for i in 1..dims {
        if cov[i][i] > cov[start][start] {
            start = i;
        }
    }
    axis[start] = 1.0;
    for _ in 0..32 {
        let mut next = [0f32; 4];
        for (i, row) in cov.iter().enumerate().take(dims) {
            next[i] = row.iter().zip(axis.iter()).take(dims).map(|(c, a)| c * a).sum();
        }
        let norm = next.iter().take(dims).map(|v| v * v).sum::<f32>().sqrt();
        if norm.is_finite() && norm > 1e-12 {
            for v in next.iter_mut().take(dims) {
                *v /= norm;
            }
            axis = next;
        } else {
            break;
        }
    }

    let proj: Vec<f32> = points
        .iter()
        .map(|p| (0..dims).map(|i| (p[i] - mean[i]) * axis[i]).sum::<f32>())
        .collect();
    let tmin = proj.iter().copied().fold(f32::INFINITY, f32::min);
    let tmax = proj.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let span = tmax - tmin;

    let mut e0 = [0f32; 4];
    let mut e1 = [0f32; 4];
    for i in 0..dims {
        e0[i] = fit_clamp(mean[i] + axis[i] * tmin);
        e1[i] = fit_clamp(mean[i] + axis[i] * tmax);
    }
    let mut weights: Vec<f32> = if span.is_finite() && span > 1e-9 {
        proj.iter().map(|t| ((t - tmin) / span).clamp(0.0, 1.0)).collect()
    } else {
        vec![0.0; points.len()]
    };

    for _ in 0..ls_iters {
        solve_endpoints(points, dims, &weights, &mut e0, &mut e1);
        // Weights given endpoints: project onto the segment, clamped to it.
        let d: [f32; 4] = [e1[0] - e0[0], e1[1] - e0[1], e1[2] - e0[2], e1[3] - e0[3]];
        let dd: f32 = (0..dims).map(|i| d[i] * d[i]).sum();
        if dd.is_finite() && dd > 1e-9 {
            for (p, w) in points.iter().zip(weights.iter_mut()) {
                let t: f32 = (0..dims).map(|i| (p[i] - e0[i]) * d[i]).sum::<f32>() / dd;
                *w = if t.is_finite() { t.clamp(0.0, 1.0) } else { 0.0 };
            }
        } else {
            weights.iter_mut().for_each(|w| *w = 0.0);
        }
    }
    FitLine { e0, e1 }
}

// ---------------------------------------------------------------------------
// Encoder: quantization and candidate search
// ---------------------------------------------------------------------------

/// A fitted coordinate in 0..=255 to the nearest 8-bit endpoint value.
fn fit_to_u8(v: f32) -> u32 {
    fit_clamp(v).round() as u32
}

/// Quantize fitted endpoints for `cem` at `endpoint_range`, taking the nearest stored
/// level per value. For the RGB modes the two endpoints are ordered by the RGB sum of
/// their *unquantized* values (non-decreasing), so a decoder takes the direct branch
/// and reproduces them exactly rather than swapping and blue-contracting; the weight
/// search that follows is indifferent to endpoint order. Returns levels in stored
/// per-channel pair order.
fn quantize_endpoints(cem: ColorEndpointMode, fit: &FitLine, endpoint_range: u32) -> Vec<u32> {
    let (a, b) = (fit.e0, fit.e1);
    let quant = |v: f32| quantize8(fit_to_u8(v), endpoint_range);
    match cem {
        ColorEndpointMode::Luminance => vec![quant(a[0]), quant(b[0])],
        ColorEndpointMode::LuminanceAlpha => {
            vec![quant(a[0]), quant(b[0]), quant(a[1]), quant(b[1])]
        }
        ColorEndpointMode::Rgb | ColorEndpointMode::Rgba => {
            let mut ea = [quant(a[0]), quant(a[1]), quant(a[2]), quant(a[3])];
            let mut eb = [quant(b[0]), quant(b[1]), quant(b[2]), quant(b[3])];
            let rgb_sum =
                |e: [u32; 4]| -> u32 { e[..3].iter().map(|&q| unquantize8(q, endpoint_range)).sum() };
            if rgb_sum(eb) < rgb_sum(ea) {
                std::mem::swap(&mut ea, &mut eb);
            }
            if cem == ColorEndpointMode::Rgba {
                vec![ea[0], eb[0], ea[1], eb[1], ea[2], eb[2], ea[3], eb[3]]
            } else {
                vec![ea[0], eb[0], ea[1], eb[1], ea[2], eb[2]]
            }
        }
    }
}

/// Decode quantized endpoints back to the two endpoints a decoder produces.
fn decoded_endpoints(
    cem: ColorEndpointMode,
    endpoint_q: &[u32],
    endpoint_range: u32,
) -> ([u32; 4], [u32; 4]) {
    let values: Vec<u32> = endpoint_q
        .iter()
        .map(|&q| unquantize8(q, endpoint_range))
        .collect();
    endpoints_from_values(cem, &values)
}

/// Squared error between two RGBA8 texels, summed over channels.
fn texel_sse(a: [u8; 4], b: [u8; 4]) -> u64 {
    a.iter()
        .zip(b.iter())
        .map(|(&x, &y)| {
            let d = i64::from(x) - i64::from(y);
            (d * d) as u64
        })
        .sum()
}

/// Choose the best stored weight level per texel for decoded endpoints `(e0, e1)`: the grid
/// is the footprint, so each texel is independent and the optimum is an exhaustive scan over
/// the at most 32 level colors (ties keep the lower level, keeping bytes deterministic).
fn quantize_weights(texels: &[[u8; 4]], e0: [u32; 4], e1: [u32; 4], weight_range: u32) -> Vec<u32> {
    let levels: Vec<[u8; 4]> = (0..weight_range)
        .map(|q| interpolate_texel(e0, e1, weight_unquantize64(q, weight_range)))
        .collect();
    texels
        .iter()
        .map(|&texel| {
            let mut best = (u64::MAX, 0u32);
            for (q, &color) in levels.iter().enumerate() {
                let err = texel_sse(texel, color);
                if err < best.0 {
                    best = (err, q as u32);
                }
            }
            best.1
        })
        .collect()
}

/// Squared error of `sym`'s fully decoded block against `texels`, summed over all channels.
/// Packing and decoding for real (instead of scoring a model of the decode) keeps best-of
/// selection honest about every quantization and layout step.
fn candidate_sse(sym: &SymbolicBlock, size: BlockSize, texels: &[[u8; 4]]) -> Option<([u8; 16], u64)> {
    #[cfg(test)]
    tests::SCORED.with(|c| c.set(c.get() + 1));
    let block = pack_symbolic(sym, size)?;
    let decoded = decode_block(&block, size);
    if decoded.len() != texels.len() {
        return None;
    }
    let sse = texels
        .iter()
        .zip(decoded.iter())
        .map(|(&a, &b)| texel_sse(a, b))
        .sum();
    Some((block, sse))
}

/// Squared error with endpoints `endpoint_q` but the already-chosen `weight_q` held fixed
/// (the cheap scoring the endpoint tweaker uses before weights are re-derived).
fn fixed_weight_sse(
    cem: ColorEndpointMode,
    endpoint_q: &[u32],
    endpoint_range: u32,
    weight_q: &[u32],
    weight_range: u32,
    texels: &[[u8; 4]],
) -> u64 {
    let (e0, e1) = decoded_endpoints(cem, endpoint_q, endpoint_range);
    texels
        .iter()
        .zip(weight_q.iter())
        .map(|(&texel, &q)| {
            texel_sse(
                texel,
                interpolate_texel(e0, e1, weight_unquantize64(q, weight_range)),
            )
        })
        .sum()
}

/// The achievable endpoint values of `endpoint_range` in ascending order, as
/// `(value, level)` pairs (first level producing each value). Steps in value space,
/// not level space, are the meaningful neighbours because unquantization scrambles
/// the stored order for trit/quint ranges.
fn endpoint_levels(endpoint_range: u32) -> Vec<(u32, u32)> {
    let mut levels: Vec<(u32, u32)> = (0..endpoint_range)
        .map(|q| (unquantize8(q, endpoint_range), q))
        .collect();
    levels.sort_unstable();
    levels.dedup_by(|next, keep| next.0 == keep.0);
    levels
}

/// Coordinate-descent tweak of endpoint values (one achievable value up or down,
/// weights held fixed, `passes` sweeps, stopping early when a sweep improves
/// nothing). Returns the improved levels; the caller re-derives weights and keeps
/// the tweak only if the true decoded error fell.
fn tweak_endpoints(sym: &SymbolicBlock, endpoint_range: u32, texels: &[[u8; 4]], passes: usize) -> Vec<u32> {
    let levels = endpoint_levels(endpoint_range);
    let mut current = sym.endpoint_q.clone();
    let mut score = fixed_weight_sse(
        sym.cem,
        &current,
        endpoint_range,
        &sym.weight_q,
        sym.weight_range,
        texels,
    );
    for _ in 0..passes {
        let mut improved = false;
        for i in 0..current.len() {
            let Some(pos) = levels.iter().position(|&(_, q)| q == current[i]) else {
                continue;
            };
            for delta in [1i64, -1] {
                let next = i64::try_from(pos).unwrap_or(0) + delta;
                let Some(&(_, next_q)) = usize::try_from(next).ok().and_then(|n| levels.get(n)) else {
                    continue;
                };
                let mut trial = current.clone();
                trial[i] = next_q;
                let trial_score = fixed_weight_sse(
                    sym.cem,
                    &trial,
                    endpoint_range,
                    &sym.weight_q,
                    sym.weight_range,
                    texels,
                );
                if trial_score < score {
                    current = trial;
                    score = trial_score;
                    improved = true;
                }
            }
        }
        if !improved {
            break;
        }
    }
    current
}

/// Search knobs for one [`Quality`].
struct SearchParams {
    /// Alternating least-squares rounds in [`fit_line`].
    ls_iters: usize,
    /// Endpoint tweak sweeps (0 disables tweaking).
    tweak_passes: usize,
    /// Cap on weight ranges tried per endpoint mode.
    max_weight_ranges: usize,
}

impl Quality {
    /// The search effort this quality spends per block.
    fn params(self) -> SearchParams {
        match self {
            Self::Fast => SearchParams {
                ls_iters: 1,
                tweak_passes: 0,
                max_weight_ranges: 2,
            },
            Self::Balanced => SearchParams {
                ls_iters: 3,
                tweak_passes: 1,
                max_weight_ranges: 5,
            },
            Self::Thorough => SearchParams {
                ls_iters: 6,
                tweak_passes: 2,
                max_weight_ranges: WEIGHT_RANGES.len(),
            },
        }
    }
}

/// The endpoint modes worth fitting for `texels` at `quality`, in the fixed evaluation order
/// (ties in decoded error keep the earlier mode, so greyscale blocks resolve to the luminance
/// modes that spend their bits best).
fn cem_candidates(texels: &[[u8; 4]], quality: Quality) -> Vec<ColorEndpointMode> {
    use ColorEndpointMode::{Luminance, LuminanceAlpha, Rgb, Rgba};
    if quality == Quality::Thorough {
        return vec![Luminance, LuminanceAlpha, Rgb, Rgba];
    }
    let opaque = texels.iter().all(|p| p[3] == 255);
    let spread = texels
        .iter()
        .map(|p| {
            let hi = p[0].max(p[1]).max(p[2]);
            let lo = p[0].min(p[1]).min(p[2]);
            hi - lo
        })
        .max()
        .unwrap_or(0);
    let grey = if quality == Quality::Fast {
        spread == 0
    } else {
        spread <= 16
    };
    let mut out = Vec::new();
    if grey && opaque {
        out.push(Luminance);
    }
    if grey && !opaque {
        out.push(LuminanceAlpha);
    }
    if opaque {
        out.push(Rgb);
    }
    if !opaque || quality == Quality::Balanced {
        out.push(Rgba);
    }
    out
}

/// Feasible (weight range, derived endpoint range) pairs for `cem` on `size` texels:
/// every weight range the footprint grid's block mode can express (at 6×6 and 8×8
/// only the low-precision ranges) that is also a legal configuration — 24..=96
/// weight bits and at least the minimum endpoint budget — so the search can never
/// settle on a block a real decoder would reject as illegal.
fn feasible_ranges(cem: ColorEndpointMode, size: BlockSize) -> Vec<(u32, u32)> {
    let count = size.texel_count();
    WEIGHT_RANGES
        .iter()
        .filter_map(|&wr| {
            encode_block_mode(size.width(), size.height(), wr)?;
            let bits = bise_bits(count, wr).ok()?;
            if !weight_config_legal(count, bits) {
                return None;
            }
            let available = endpoint_bits_for(bits)?;
            if available < min_endpoint_bits(cem.value_count()) {
                return None;
            }
            let er = endpoint_range_for(cem.value_count(), available)?;
            Some((wr, er))
        })
        .collect()
}

/// Pick about `max` entries of `feasible` for the search: evenly spaced entries across the
/// whole list (both ends included, indices rounded to nearest so mid-list ranges such as
/// weight range 16 are not skipped), plus the largest weight range that still affords
/// endpoint range 32 when that anchor is not already picked — the weights-vs-endpoints
/// trade needs both ends and the middle represented.
fn select_ranges(feasible: &[(u32, u32)], max: usize) -> Vec<(u32, u32)> {
    if feasible.len() <= max {
        return feasible.to_vec();
    }
    let mut picked: Vec<(u32, u32)> = Vec::new();
    let push = |picked: &mut Vec<(u32, u32)>, pair: (u32, u32)| {
        if !picked.contains(&pair) {
            picked.push(pair);
        }
    };
    let n = feasible.len();
    if max <= 1 {
        if let Some(&last) = feasible.last() {
            push(&mut picked, last);
        }
    } else {
        for i in 0..max {
            let index = (i * (n - 1) + (max - 1) / 2) / (max - 1);
            push(&mut picked, feasible[index.min(n - 1)]);
        }
        if let Some(&pair) = feasible.iter().rev().find(|&&(_, er)| er >= 32) {
            push(&mut picked, pair);
        }
    }
    picked.sort_unstable();
    picked.dedup();
    picked
}

/// Build and score one candidate: fit, quantize endpoints at `endpoint_range`, quantize
/// weights at `weight_range`, refit endpoints against the quantized weights, optionally
/// tweak endpoints, scoring by pack/decode at each step and keeping only improvements.
/// Returns the packed bytes and their decoded squared error.
fn evaluate_candidate(
    texels: &[[u8; 4]],
    size: BlockSize,
    cem: ColorEndpointMode,
    weight_range: u32,
    endpoint_range: u32,
    params: &SearchParams,
) -> Option<([u8; 16], u64)> {
    let (points, dims) = fit_points(cem, texels);
    let fit = fit_line(&points, dims, params.ls_iters);
    let endpoint_q = quantize_endpoints(cem, &fit, endpoint_range);
    let (e0, e1) = decoded_endpoints(cem, &endpoint_q, endpoint_range);
    let weight_q = quantize_weights(texels, e0, e1, weight_range);
    let mut sym = SymbolicBlock {
        cem,
        weight_range,
        endpoint_q,
        weight_q,
    };
    let mut best = candidate_sse(&sym, size, texels)?;
    // Refit endpoints against the *quantized* weights (two rounds): with few weight
    // levels the continuous line fit leaves endpoints at the texel extremes, while
    // the least-squares optimum for the stored levels is the assigned texels'
    // centroids (for two levels this alone is worth several dB). Each round is kept
    // only if the true decoded error falls.
    for _ in 0..2 {
        let fractions: Vec<f32> = sym
            .weight_q
            .iter()
            .map(|&q| weight_unquantize64(q, weight_range) as f32 / 64.0)
            .collect();
        let (mut re0, mut re1) = ([0f32; 4], [0f32; 4]);
        if !solve_endpoints(&points, dims, &fractions, &mut re0, &mut re1) {
            break;
        }
        let refit = FitLine { e0: re0, e1: re1 };
        let refit_q = quantize_endpoints(cem, &refit, endpoint_range);
        if refit_q == sym.endpoint_q {
            break;
        }
        let (re0d, re1d) = decoded_endpoints(cem, &refit_q, endpoint_range);
        let trial_sym = SymbolicBlock {
            cem,
            weight_range,
            endpoint_q: refit_q,
            weight_q: quantize_weights(texels, re0d, re1d, weight_range),
        };
        match candidate_sse(&trial_sym, size, texels) {
            Some(trial) if trial.1 < best.1 => {
                best = trial;
                sym = trial_sym;
            }
            _ => break,
        }
    }
    if params.tweak_passes > 0 {
        let tweaked_q = tweak_endpoints(&sym, endpoint_range, texels, params.tweak_passes);
        if tweaked_q != sym.endpoint_q {
            let (te0, te1) = decoded_endpoints(cem, &tweaked_q, endpoint_range);
            sym = SymbolicBlock {
                cem,
                weight_range,
                endpoint_q: tweaked_q,
                weight_q: quantize_weights(texels, te0, te1, weight_range),
            };
            if let Some(trial) = candidate_sse(&sym, size, texels)
                && trial.1 < best.1
            {
                best = trial;
            }
        }
    }
    Some(best)
}

/// Mean texel color, per-channel rounded (the defined fallback if the search somehow finds
/// no candidate; every size and mode has at least one legal configuration, so this is
/// unreachable in practice).
fn mean_color(texels: &[[u8; 4]]) -> [u8; 4] {
    if texels.is_empty() {
        return [0, 0, 0, 255];
    }
    let n = texels.len() as u32;
    let mut sum = [0u32; 4];
    for p in texels {
        for (s, &c) in sum.iter_mut().zip(p.iter()) {
            *s += u32::from(c);
        }
    }
    sum.map(|s| ((s + n / 2) / n) as u8)
}

/// Encode exactly `size.texel_count()` texels (the caller guarantees the length). Constant
/// blocks become void-extent blocks; everything else goes through the candidate search and
/// keeps the lowest decoded squared error (first candidate wins ties, hence deterministic).
fn encode_block_inner(texels: &[[u8; 4]], size: BlockSize, quality: Quality) -> [u8; 16] {
    if texels.is_empty() {
        return pack_void_extent([0, 0, 0, 255]);
    }
    if texels.iter().all(|p| *p == texels[0]) {
        return pack_void_extent(texels[0]);
    }
    let params = quality.params();
    let mut best: Option<([u8; 16], u64)> = None;
    for cem in cem_candidates(texels, quality) {
        let feasible = feasible_ranges(cem, size);
        for (weight_range, endpoint_range) in select_ranges(&feasible, params.max_weight_ranges) {
            let Some(candidate) =
                evaluate_candidate(texels, size, cem, weight_range, endpoint_range, &params)
            else {
                continue;
            };
            if best.as_ref().is_none_or(|(_, err)| candidate.1 < *err) {
                best = Some(candidate);
            }
            if best.as_ref().is_some_and(|(_, err)| *err == 0) {
                // Exact: no candidate can beat it, and ties keep the first anyway.
                if let Some((block, _)) = best {
                    return block;
                }
            }
        }
    }
    match best {
        Some((block, _)) => block,
        None => pack_void_extent(mean_color(texels)),
    }
}

// ---------------------------------------------------------------------------
// Public encoder entry points
// ---------------------------------------------------------------------------

/// Encode one block of RGBA8 texels (`pixels` in row-major order) to 16 ASTC bytes.
///
/// This entry point never fails and never panics: a short `pixels` slice is padded by
/// repeating its last texel (an empty slice encodes as opaque black), a long one is
/// truncated — the same edge-replication [`encode_image`] applies to partial blocks. Use
/// [`try_encode_block`] to reject a wrong texel count instead. Constant blocks encode as
/// void-extent blocks and decode exactly.
pub fn encode_block(pixels: &[[u8; 4]], block_size: BlockSize, quality: Quality) -> [u8; 16] {
    let needed = block_size.texel_count();
    if pixels.len() == needed {
        return encode_block_inner(pixels, block_size, quality);
    }
    let fill = pixels.last().copied().unwrap_or([0, 0, 0, 255]);
    let mut padded: Vec<[u8; 4]> = pixels.iter().take(needed).copied().collect();
    padded.resize(needed, fill);
    encode_block_inner(&padded, block_size, quality)
}

/// Encode one block like [`encode_block`], but reject a wrong texel count.
///
/// # Errors
/// [`AstcEncodeError::BadBlockTexels`] when `pixels.len() != block_size.texel_count()`.
pub fn try_encode_block(
    pixels: &[[u8; 4]],
    block_size: BlockSize,
    quality: Quality,
) -> Result<[u8; 16], AstcEncodeError> {
    let expected = block_size.texel_count();
    if pixels.len() != expected {
        return Err(AstcEncodeError::BadBlockTexels {
            expected,
            actual: pixels.len(),
        });
    }
    Ok(encode_block_inner(pixels, block_size, quality))
}

/// Encode a whole RGBA8 image to ASTC blocks (row-major, 16 bytes each,
/// `ceil(width / bw) * ceil(height / bh)` blocks). Partial edge blocks replicate the image's
/// last row and column, as the other codecs in this crate do.
///
/// # Errors
/// [`AstcEncodeError::InvalidDimensions`] for a zero dimension, or
/// [`AstcEncodeError::BadBuffer`] when `rgba` is not `width * height * 4` bytes.
pub fn encode_image(
    width: u32,
    height: u32,
    rgba: &[u8],
    block_size: BlockSize,
    quality: Quality,
) -> Result<Vec<u8>, AstcEncodeError> {
    let _span = tracing::info_span!("astc_encode_image", width, height).entered();
    if width == 0 || height == 0 {
        return Err(AstcEncodeError::InvalidDimensions { width, height });
    }
    if rgba.len() as u64 != u64::from(width) * u64::from(height) * 4 {
        return Err(AstcEncodeError::BadBuffer {
            len: rgba.len(),
            width,
            height,
        });
    }
    let (bw, bh) = (block_size.width(), block_size.height());
    let blocks_x = width.div_ceil(bw);
    let blocks_y = height.div_ceil(bh);
    let mut out = Vec::with_capacity(blocks_x as usize * blocks_y as usize * 16);
    let mut texels: Vec<[u8; 4]> = Vec::with_capacity(block_size.texel_count());
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            texels.clear();
            for y in 0..bh {
                for x in 0..bw {
                    let px = (bx * bw + x).min(width - 1);
                    let py = (by * bh + y).min(height - 1);
                    let i = ((py * width + px) * 4) as usize;
                    texels.push([rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]);
                }
            }
            out.extend_from_slice(&encode_block_inner(&texels, block_size, quality));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// A small deterministic generator (a 64-bit LCG; tests must not depend on entropy).
    struct Lcg(u64);

    impl Lcg {
        /// Next byte of the stream.
        fn byte(&mut self) -> u8 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 56) as u8
        }

        /// Next value below `range` (modulo reduction; bias is irrelevant in tests).
        fn below(&mut self, range: u32) -> u32 {
            u32::from(self.byte()) % range
        }
    }

    /// PSNR in dB of two equal-length byte buffers (99 when identical).
    fn psnr_bytes(a: &[u8], b: &[u8]) -> f64 {
        assert_eq!(a.len(), b.len(), "psnr inputs differ in length");
        let se: f64 = a
            .iter()
            .zip(b.iter())
            .map(|(&x, &y)| (f64::from(x) - f64::from(y)).powi(2))
            .sum();
        if se == 0.0 {
            return 99.0;
        }
        10.0 * (255.0f64 * 255.0 / (se / a.len() as f64)).log10()
    }

    /// PSNR in dB of decoded texels against the source texels (all four channels).
    fn psnr_texels(src: &[[u8; 4]], dec: &[[u8; 4]]) -> f64 {
        let flat = |t: &[[u8; 4]]| t.iter().flat_map(|p| p.iter().copied()).collect::<Vec<u8>>();
        psnr_bytes(&flat(src), &flat(dec))
    }

    /// Encode+decode one exact-size block and return the decoded texels.
    fn round_trip(texels: &[[u8; 4]], size: BlockSize, quality: Quality) -> Vec<[u8; 4]> {
        let block = encode_block(texels, size, quality);
        decode_block(&block, size)
    }

    /// A `w`×`h` RGBA8 image whose every texel comes from `f(x, y)`.
    fn image(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> Vec<u8> {
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                out.extend_from_slice(&f(x, y));
            }
        }
        out
    }

    // -- BISE ---------------------------------------------------------------

    /// Every single value of every range round-trips, and so do exhaustive pairs/triples
    /// for the small ranges plus structured patterns (zeros, max, ramps, seeded noise) at
    /// group-boundary counts for all ranges. The packed length always matches `bise_bits`.
    #[test]
    fn bise_round_trips_all_values_for_every_range_and_count() {
        let check = |values: &[u32], range: u32| {
            let bytes = bise_encode(values, range).expect("encode");
            assert_eq!(
                bytes.len(),
                bise_bits(values.len(), range).expect("bits").div_ceil(8),
                "range {range} count {}",
                values.len()
            );
            let back = bise_decode(&bytes, values.len(), range).expect("decode");
            assert_eq!(back, values, "range {range}");
        };
        for &range in &QUANT_RANGES {
            for v in 0..range {
                check(&[v], range);
            }
            for &count in &[2usize, 3, 4, 5, 6, 7, 8, 15, 16, 25, 36, 64] {
                check(&vec![0; count], range);
                check(&vec![range - 1; count], range);
                let ramp: Vec<u32> = (0..count).map(|i| (i as u32) % range).collect();
                check(&ramp, range);
                let rev: Vec<u32> = ramp.iter().rev().copied().collect();
                check(&rev, range);
                let mut rng = Lcg(0x5EED ^ u64::from(range));
                let noise: Vec<u32> = (0..count).map(|_| rng.below(range)).collect();
                check(&noise, range);
            }
        }
        // Exhaustive small combinations.
        for &range in &[2u32, 3, 4, 5, 6] {
            for a in 0..range {
                for b in 0..range {
                    check(&[a, b], range);
                    for c in 0..range {
                        check(&[a, b, c], range);
                    }
                }
            }
        }
    }

    #[test]
    fn bise_bits_matches_the_group_formula() {
        // Range 6 = trit + 1 bit: 5 values cost 8 + 5 = 13 bits; a 6th value spends
        // only its share of the next packed field, ceil(6*8/5) = 10, plus its plain bit.
        assert_eq!(bise_bits(5, 6).expect("bits"), 13);
        assert_eq!(bise_bits(6, 6).expect("bits"), 16);
        // Range 40 = quint + 3 bits: 3 values cost 7 + 9 = 16; the 4th adds
        // ceil(4*7/3) - 7 = 3 packed bits plus its 3 plain bits.
        assert_eq!(bise_bits(3, 40).expect("bits"), 16);
        assert_eq!(bise_bits(4, 40).expect("bits"), 22);
        // Range 256 = 8 plain bits; range 2 = 1 plain bit; empty costs nothing.
        assert_eq!(bise_bits(8, 256).expect("bits"), 64);
        assert_eq!(bise_bits(64, 2).expect("bits"), 64);
        assert_eq!(bise_bits(0, 12).expect("bits"), 0);
        // At counts large enough to amortize the group overhead, cost never falls as the
        // range grows (at tiny counts a packed trit group can cost more than plain bits).
        let costs: Vec<usize> = QUANT_RANGES
            .iter()
            .map(|&r| bise_bits(16, r).expect("bits"))
            .collect();
        assert!(costs.windows(2).all(|w| w[0] <= w[1]), "{costs:?}");
    }

    #[test]
    fn bise_rejects_bad_ranges_values_and_short_streams() {
        assert!(matches!(
            bise_encode(&[0], 7),
            Err(AstcEncodeError::InvalidRange { range: 7 })
        ));
        assert!(matches!(
            bise_encode(&[5], 5),
            Err(AstcEncodeError::ValueOutOfRange { value: 5, range: 5 })
        ));
        assert!(matches!(
            bise_decode(&[], 1, 256),
            Err(AstcEncodeError::TruncatedBits { .. })
        ));
        assert!(bise_bits(4, 0).is_err());
        assert!(bise_bits(4, 1).is_err());
        // An empty sequence is valid and round-trips.
        assert_eq!(bise_encode(&[], 12).expect("empty"), Vec::<u8>::new());
        assert_eq!(bise_decode(&[], 0, 12).expect("empty"), Vec::<u32>::new());
    }

    // -- Unquantization -----------------------------------------------------

    #[test]
    fn unquantize_covers_the_full_scale_with_distinct_values() {
        // The spec's trit/quint unquantization scrambles stored order, so the checks
        // run on the sorted value set: it must span the whole scale with one distinct
        // value per level and near-uniform steps.
        for &range in &QUANT_RANGES {
            let mut values: Vec<u32> = (0..range).map(|q| unquantize8(q, range)).collect();
            values.sort_unstable();
            assert_eq!(values.first(), Some(&0), "range {range}");
            assert_eq!(values.last(), Some(&255), "range {range}");
            let mut distinct = values.clone();
            distinct.dedup();
            assert_eq!(distinct.len(), range as usize, "range {range} values collide");
            // The multiplier-free table's steps are near-uniform, with a possible
            // one-step irregularity around the midpoint (range 40 steps 123 -> 132).
            let step = 255 / (range - 1) + 3;
            assert!(
                values.windows(2).all(|w| w[1] - w[0] <= step),
                "range {range} has an oversized gap: {values:?}"
            );
            // Weights: some level reaches each end of the 0..=64 scale.
            let weights: Vec<u32> = (0..range.min(32))
                .map(|q| weight_unquantize64(q, range.min(32)))
                .collect();
            if WEIGHT_RANGES.contains(&range) {
                assert!(weights.contains(&0) && weights.contains(&64), "range {range}");
                assert!(weights.iter().all(|&w| w <= 64), "range {range}");
            }
        }
        // Known spot values: bit replication and the scrambled trit order.
        assert_eq!(unquantize8(1, 16), 17);
        assert_eq!(unquantize8(31, 256), 31);
        assert_eq!(
            (0..6).map(|q| unquantize8(q, 6)).collect::<Vec<_>>(),
            [0, 255, 51, 204, 102, 153]
        );
        assert_eq!(weight_unquantize64(1, 3), 32);
        assert_eq!(weight_unquantize64(3, 5), 48);
    }

    #[test]
    fn quantize_inverts_unquantize_at_every_level() {
        for &range in &QUANT_RANGES {
            // Every achievable value quantizes back to a level giving that value.
            for q in 0..range {
                let v = unquantize8(q, range);
                assert_eq!(
                    unquantize8(quantize8(v, range), range),
                    v,
                    "range {range} level {q}"
                );
            }
            // Every 8-bit value quantizes to a level whose value is within one step.
            for v in 0..=255u32 {
                let back = unquantize8(quantize8(v, range), range);
                let step = 255 / (range - 1) + 1;
                assert!(back.abs_diff(v) <= step, "range {range} v {v}");
            }
        }
        // Degenerate ranges stay defined instead of dividing by zero.
        assert_eq!(unquantize8(0, 1), 0);
        assert_eq!(quantize8(123, 0), 0);
        assert_eq!(weight_unquantize64(0, 1), 0);
    }

    // -- Block mode and range derivation ------------------------------------

    #[test]
    fn block_mode_encode_decode_are_inverse() {
        // Every decodable mode re-encodes to a mode decoding identically (the bit
        // pattern may differ where rows overlap), across the whole 11-bit space.
        let mut decoded_some = 0;
        for mode in 0..2048u32 {
            let Some(d) = decode_block_mode(mode) else {
                continue;
            };
            decoded_some += 1;
            if d.dual_plane {
                continue; // the encoder only packs single-plane modes
            }
            let repacked = encode_block_mode(d.grid_w, d.grid_h, d.weight_range).unwrap_or_else(|| {
                panic!(
                    "mode {mode} grid {}x{} range {} unpackable",
                    d.grid_w, d.grid_h, d.weight_range
                )
            });
            assert_eq!(decode_block_mode(repacked), Some(d), "mode {mode}");
        }
        assert!(decoded_some > 500, "only {decoded_some} modes decoded");
        // The footprint grids: 4x4/5x5 offer every weight range; 6x6/8x8 only the
        // low-precision six (their block-mode row has no precision bit).
        for size in BlockSize::ALL {
            for &wr in &WEIGHT_RANGES {
                let packed = encode_block_mode(size.width(), size.height(), wr);
                let low = WEIGHT_RANGES[..6].contains(&wr);
                let expect = low || matches!(size, BlockSize::Block4x4 | BlockSize::Block5x5);
                assert_eq!(packed.is_some(), expect, "{size:?} range {wr}");
                if let Some(mode) = packed {
                    assert_eq!(
                        decode_block_mode(mode),
                        Some(BlockModeDecoded {
                            grid_w: size.width(),
                            grid_h: size.height(),
                            weight_range: wr,
                            dual_plane: false,
                        })
                    );
                }
            }
        }
        assert!(encode_block_mode(0, 4, 2).is_none());
        assert!(
            encode_block_mode(1, 1, 2).is_none(),
            "grids below 2 are not encodable"
        );
        assert!(encode_block_mode(4, 4, 7).is_none());
        // Reserved and void patterns must not decode as a block mode.
        assert!(decode_block_mode(0).is_none());
        assert!(decode_block_mode(VOID_EXTENT_PATTERN).is_none());
        assert!(
            decode_block_mode(0b111 << 6).is_none(),
            "reserved row, not void-extent"
        );
    }

    #[test]
    fn endpoint_range_is_the_largest_that_fits() {
        // 8 RGBA values in 47 bits (the 8x8, range-2 weights budget): range 48 costs
        // ceil(8*8/5) + 32 = 45, range 64 costs 48 > 47.
        assert_eq!(endpoint_range_for(8, 47), Some(48));
        // 2 luminance values in 7 bits: range 10 costs ceil(2*7/3) + 2 = 7,
        // range 12 costs ceil(2*8/5) + 4 = 8 > 7.
        assert_eq!(endpoint_range_for(2, 7), Some(10));
        // Nothing fits below the range-2 floor cost.
        assert_eq!(endpoint_range_for(8, 7), None);
        assert_eq!(endpoint_range_for(8, 8), Some(2));
        // More weight bits never raise the derived endpoint range.
        let mut prev = u32::MAX;
        for &wr in &WEIGHT_RANGES {
            let bits = bise_bits(16, wr).expect("bits");
            let Some(avail) = endpoint_bits_for(bits) else {
                continue;
            };
            let Some(er) = endpoint_range_for(8, avail) else {
                continue;
            };
            assert!(er <= prev, "weight range {wr}");
            prev = er;
        }
    }

    #[test]
    fn weight_grid_sampling_is_identity_at_the_footprint() {
        for size in BlockSize::ALL {
            let n = size.texel_count();
            let grid: Vec<u32> = (0..n).map(|i| ((i * 64) / n.max(1)) as u32).collect();
            for y in 0..size.height() {
                for x in 0..size.width() {
                    let w = sample_weight_grid(
                        &grid,
                        size.width(),
                        size.height(),
                        size.width(),
                        size.height(),
                        x,
                        y,
                    );
                    assert_eq!(w, grid[(y * size.width() + x) as usize], "{size:?} ({x},{y})");
                }
            }
        }
        // A 1x1 grid is constant; a 2x2 grid's center texel of 3x3 averages the corners.
        assert_eq!(sample_weight_grid(&[40], 1, 1, 4, 4, 3, 2), 40);
        assert_eq!(sample_weight_grid(&[0, 64, 64, 0], 2, 2, 3, 3, 1, 1), 32);
    }

    #[test]
    fn blue_contraction_only_fires_on_decreasing_rgb_sums() {
        // The contraction averages red and green toward blue.
        assert_eq!(blue_contract([100, 40, 200, 7]), [150, 120, 200, 7]);
        // Increasing sums: stored pairs decode direct, saturated blue included.
        let (e0, e1) = endpoints_from_values(ColorEndpointMode::Rgb, &[10, 20, 30, 40, 250, 255]);
        assert_eq!(e0, [10, 30, 250, 255]);
        assert_eq!(e1, [20, 40, 255, 255]);
        // Decreasing sums: endpoints swap and both contract.
        let (e0, e1) = endpoints_from_values(ColorEndpointMode::Rgb, &[200, 10, 100, 20, 240, 30]);
        assert_eq!(e0, blue_contract([10, 20, 30, 255]));
        assert_eq!(e1, blue_contract([200, 100, 240, 255]));
        // Equal sums take the direct branch.
        let (e0, e1) = endpoints_from_values(ColorEndpointMode::Rgba, &[100, 0, 0, 100, 0, 0, 9, 8]);
        assert_eq!(e0, [100, 0, 0, 9]);
        assert_eq!(e1, [0, 100, 0, 8]);
    }

    // -- Solid colors and void-extent ----------------------------------------

    #[test]
    fn solid_colors_are_exact_at_every_block_size() {
        let colors = [
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [200, 90, 40, 255],
            [0, 0, 255, 255],
            [12, 34, 56, 0],
            [0, 0, 0, 0],
            [1, 2, 3, 254],
        ];
        for size in BlockSize::ALL {
            for color in colors {
                let texels = vec![color; size.texel_count()];
                for quality in [Quality::Fast, Quality::Balanced, Quality::Thorough] {
                    let decoded = round_trip(&texels, size, quality);
                    assert!(
                        decoded.iter().all(|&p| p == color),
                        "{size:?} {color:?} {quality:?} decoded {decoded:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn solid_colors_use_void_extent_blocks() {
        for size in BlockSize::ALL {
            let texels = vec![[77u8, 150, 20, 200]; size.texel_count()];
            let block = encode_block(&texels, size, Quality::Balanced);
            let info = inspect_block(&block, size);
            assert_eq!(info.kind, BlockKind::VoidExtent, "{size:?}");
            assert_eq!(info.constant_color, Some([77, 150, 20, 200]));
            assert_eq!(get_bits(&block, 0, 9), VOID_EXTENT_PATTERN);
            assert_eq!(
                block[8], 77,
                "16-bit little-endian red low byte repeats the value"
            );
            assert_eq!(block[9], 77);
        }
        // A hand-built void-extent block decodes to its stored color.
        let block = pack_void_extent([10, 220, 130, 40]);
        assert!(
            decode_block(&block, BlockSize::Block6x6)
                .iter()
                .all(|&p| p == [10, 220, 130, 40])
        );
    }

    #[test]
    fn non_constant_blocks_are_valid_normal_blocks() {
        for size in BlockSize::ALL {
            let texels: Vec<[u8; 4]> = (0..size.texel_count())
                .map(|i| [(i * 7) as u8, (i * 3) as u8, 90, 255])
                .collect();
            let block = encode_block(&texels, size, Quality::Balanced);
            let info = inspect_block(&block, size);
            assert_eq!(info.kind, BlockKind::Normal, "{size:?}");
            assert!(info.cem.is_some() && info.weight_range.is_some());
            // The derived endpoint range must fit the budget the weights leave.
            let wr = info.weight_range.expect("weight range");
            let er = info.endpoint_range.expect("endpoint range");
            let cem = info.cem.expect("cem");
            let wbits = bise_bits(size.texel_count(), wr).expect("weight bits");
            let ebits = bise_bits(cem.value_count(), er).expect("endpoint bits");
            assert!(ebits <= DATA_BITS_ONE_PARTITION - wbits, "{size:?}");
            assert_eq!(get_bits(&block, 11, 2), 0, "one partition");
        }
    }

    // -- Quality: gradients, patterns, noise ---------------------------------
    //
    // Floor rationale (single partition, weight grid = footprint): texels of a 1-D ramp lie
    // on a line in RGBA space, so the only losses are endpoint and weight quantization,
    // which the bit budget keeps small except at 8x8 where range-2 weights dominate (two
    // reproduction levels per block of span S add about S^2/48 mean squared error in the
    // ramping channel). A 2-D ramp (R across, G down) is a *plane* patch per block, so a
    // one-segment fit keeps the residual of the best line through the patch; floors sit
    // several dB below the measured shape of that residual and fall as the block grows.

    #[test]
    fn one_dimensional_gradients_clear_their_psnr_floors() {
        // Blue is held at 60 so these generic-gradient floors measure the line fit and
        // quantization alone; blue-dominant ramps have their own test below.
        let src = image(64, 64, |x, _| [(x * 255 / 63) as u8, 90, 60, 255]);
        for (size, floor) in [
            (BlockSize::Block4x4, 38.0),
            (BlockSize::Block5x5, 36.0),
            (BlockSize::Block6x6, 34.0),
            (BlockSize::Block8x8, 28.0),
        ] {
            let blocks = encode_image(64, 64, &src, size, Quality::Balanced).expect("encode");
            let back = decode_image(64, 64, &blocks, size).expect("decode");
            let got = psnr_bytes(&src, &back);
            assert!(got >= floor, "{size:?} 1-D gradient PSNR {got:.1} < {floor}");
        }
        // The same ramp down the Y axis (row-major ordering must not matter).
        let src = image(48, 48, |_, y| {
            [(y * 255 / 47) as u8, (y * 128 / 47) as u8, 30, 255]
        });
        let blocks = encode_image(48, 48, &src, BlockSize::Block6x6, Quality::Balanced).expect("encode");
        let back = decode_image(48, 48, &blocks, BlockSize::Block6x6).expect("decode");
        assert!(psnr_bytes(&src, &back) >= 32.0, "vertical/diagonal-line gradient");
    }

    #[test]
    fn two_dimensional_gradients_clear_their_psnr_floors() {
        let src = image(64, 64, |x, y| {
            [(x * 255 / 63) as u8, (y * 255 / 63) as u8, 60, 255]
        });
        for (size, floor) in [
            (BlockSize::Block4x4, 32.0),
            (BlockSize::Block5x5, 30.0),
            (BlockSize::Block6x6, 29.0),
            (BlockSize::Block8x8, 25.0),
        ] {
            let blocks = encode_image(64, 64, &src, size, Quality::Balanced).expect("encode");
            let back = decode_image(64, 64, &blocks, size).expect("decode");
            let got = psnr_bytes(&src, &back);
            assert!(got >= floor, "{size:?} 2-D gradient PSNR {got:.1} < {floor}");
        }
    }

    #[test]
    fn greyscale_blocks_use_luminance_and_stay_grey() {
        // Each block spans the full 0..255 ramp, so quality is bounded by how many weight
        // levels the footprint grid can legally afford: many at 4x4/5x5, up to 6 at 6x6,
        // and only 2 at 8x8 (range-3 weights there spend 103 bits, over the 96-bit
        // legality cap). With two levels the bound is the two-centroid quantization of
        // a uniform ramp (about 16.8 dB), which the endpoint refit nearly reaches. The
        // floors track that reproduction-level bound with margin.
        for (size, floor) in [
            (BlockSize::Block4x4, 40.0),
            (BlockSize::Block5x5, 32.0),
            (BlockSize::Block6x6, 26.0),
            (BlockSize::Block8x8, 16.0),
        ] {
            let texels: Vec<[u8; 4]> = (0..size.texel_count())
                .map(|i| {
                    let v = (i * 255 / size.texel_count().saturating_sub(1).max(1)) as u8;
                    [v, v, v, 255]
                })
                .collect();
            let block = encode_block(&texels, size, Quality::Balanced);
            let info = inspect_block(&block, size);
            assert_eq!(info.cem, Some(ColorEndpointMode::Luminance), "{size:?}");
            let decoded = decode_block(&block, size);
            assert!(
                decoded
                    .iter()
                    .all(|p| p[0] == p[1] && p[1] == p[2] && p[3] == 255),
                "{size:?} decoded not grey"
            );
            assert!(psnr_texels(&texels, &decoded) >= floor, "{size:?} grey ramp");
        }
    }

    #[test]
    fn alpha_gradients_keep_color_and_track_alpha() {
        // One block spanning the full alpha ramp: the floor falls with block size because
        // the footprint weight grid affords fewer reproduction colors (at 8x8, RGBA
        // endpoints only fit next to range-2 weights, so alpha is tracked coarsely).
        for (size, floor, drift) in [
            (BlockSize::Block4x4, 38.0, 10u8),
            (BlockSize::Block5x5, 28.0, 14),
            (BlockSize::Block6x6, 24.0, 20),
            (BlockSize::Block8x8, 15.0, 14),
        ] {
            let n = size.texel_count();
            let texels: Vec<[u8; 4]> = (0..n)
                .map(|i| [210, 60, 30, (i * 255 / (n - 1).max(1)) as u8])
                .collect();
            let block = encode_block(&texels, size, Quality::Balanced);
            assert_eq!(
                inspect_block(&block, size).cem,
                Some(ColorEndpointMode::Rgba),
                "{size:?} saturated color with alpha ramp needs RGBA endpoints"
            );
            let decoded = decode_block(&block, size);
            assert!(psnr_texels(&texels, &decoded) >= floor, "{size:?} alpha ramp");
            // RGB stays near the constant color everywhere along the ramp (the bound is
            // the endpoint quantization step at each size: coarser endpoints drift more).
            let worst = decoded
                .iter()
                .map(|p| p[0].abs_diff(210).max(p[1].abs_diff(60)).max(p[2].abs_diff(30)))
                .max()
                .unwrap_or(0);
            assert!(worst <= drift, "{size:?} color drift {worst}");
        }
    }

    #[test]
    fn two_color_patterns_are_nearly_exact() {
        // A 2-color checkerboard is exactly two endpoints plus binary weights: every block
        // size can afford that, so reconstruction is near-lossless.
        let src = image(32, 32, |x, y| {
            if (x + y) % 2 == 0 {
                [240, 230, 220, 255]
            } else {
                [16, 24, 32, 255]
            }
        });
        for size in BlockSize::ALL {
            let blocks = encode_image(32, 32, &src, size, Quality::Balanced).expect("encode");
            let back = decode_image(32, 32, &blocks, size).expect("decode");
            assert!(psnr_bytes(&src, &back) >= 36.0, "{size:?} checkerboard");
        }
    }

    #[test]
    fn noise_clears_a_conservative_floor() {
        // White noise is the worst case for a one-segment block coder: a uniform channel
        // has variance ~5418 and the best line keeps only its projection, bounding PSNR in
        // the mid-teens at 4x4. The floor sits under that bound with margin; its job is to
        // catch a broken fit, which lands near 5 dB (flat mean) instead.
        let mut rng = Lcg(0xA57C);
        let noisy: Vec<u8> = (0..32 * 32 * 4).map(|_| rng.byte()).collect();
        let blocks = encode_image(32, 32, &noisy, BlockSize::Block4x4, Quality::Balanced).expect("encode");
        let back = decode_image(32, 32, &blocks, BlockSize::Block4x4).expect("decode");
        let got = psnr_bytes(&noisy, &back);
        assert!(
            (9.0..=25.0).contains(&got),
            "white noise PSNR {got:.1} outside [9, 25]"
        );

        // Grain over a gradient (the realistic case) does far better: +/-16 noise keeps
        // texels near the ramp line, so the floor only needs quantization headroom.
        let mut rng = Lcg(0x6A10);
        let mut grain = Vec::with_capacity(32 * 32 * 4);
        for _y in 0..32u32 {
            for x in 0..32u32 {
                let base = (x * 255 / 31) as i32;
                let d = i32::from(rng.byte() % 33) - 16;
                let v = (base + d).clamp(0, 255) as u8;
                grain.extend_from_slice(&[v, (x * 128 / 31) as u8, 80, 255]);
            }
        }
        let blocks = encode_image(32, 32, &grain, BlockSize::Block4x4, Quality::Balanced).expect("encode");
        let back = decode_image(32, 32, &blocks, BlockSize::Block4x4).expect("decode");
        assert!(psnr_bytes(&grain, &back) >= 24.0, "noisy gradient");
    }

    #[test]
    fn blue_dominant_gradients_stay_accurate() {
        // A ramp from black to sky blue has blue above red at the bright end. Endpoints
        // are stored in RGB-sum order, so they decode direct (no blue contraction) and
        // every channel, blue included, must stay accurate.
        let src = image(64, 16, |x, _| {
            let t = x * 255 / 63;
            [(t / 2) as u8, (t * 3 / 4) as u8, t as u8, 255]
        });
        let blocks = encode_image(64, 16, &src, BlockSize::Block4x4, Quality::Balanced).expect("encode");
        let back = decode_image(64, 16, &blocks, BlockSize::Block4x4).expect("decode");
        assert!(psnr_bytes(&src, &back) >= 30.0, "sky ramp");
        let worst_blue = src
            .chunks(4)
            .zip(back.chunks(4))
            .map(|(s, d)| s[2].abs_diff(d[2]))
            .max()
            .unwrap_or(0);
        assert!(worst_blue <= 12, "blue error {worst_blue}");
    }

    #[test]
    fn thorough_is_never_much_worse_than_fast() {
        // More search must not hurt (a small tolerance covers least-squares paths differing
        // between iteration counts; a regression beyond it means selection is broken).
        let src = image(48, 48, |x, y| {
            [
                (x * 255 / 47) as u8,
                (y * 255 / 47) as u8,
                ((x + y) * 255 / 94) as u8,
                255,
            ]
        });
        for size in [BlockSize::Block4x4, BlockSize::Block6x6] {
            let fast = encode_image(48, 48, &src, size, Quality::Fast).expect("fast");
            let best = encode_image(48, 48, &src, size, Quality::Thorough).expect("thorough");
            let fast_psnr = psnr_bytes(&src, &decode_image(48, 48, &fast, size).expect("decode"));
            let best_psnr = psnr_bytes(&src, &decode_image(48, 48, &best, size).expect("decode"));
            assert!(
                best_psnr >= fast_psnr - 0.25,
                "{size:?} thorough {best_psnr:.2} vs fast {fast_psnr:.2}"
            );
            assert!(fast_psnr >= 24.0, "{size:?} fast floor");
        }
    }

    #[test]
    fn hand_built_symbolic_block_decodes_as_a_ramp() {
        // Pack black->white endpoints with an ascending weight ramp without touching the
        // search, pinning the pack/decode path (streams, ranges, interpolation) directly.
        let size = BlockSize::Block4x4;
        let n = size.texel_count();
        let weight_range = 16;
        let bits = bise_bits(n, weight_range).expect("bits");
        let er = endpoint_range_for(8, endpoint_bits_for(bits).expect("budget")).expect("range");
        // Stored order is scrambled by unquantization, so pick levels by value.
        let (lo, hi) = (quantize8(0, er), quantize8(255, er));
        let sym = SymbolicBlock {
            cem: ColorEndpointMode::Rgba,
            weight_range,
            endpoint_q: vec![lo, hi, lo, hi, lo, hi, hi, hi],
            weight_q: (0..n)
                .map(|i| (i as u32 * (weight_range - 1)) / (n as u32 - 1))
                .collect(),
        };
        let block = pack_symbolic(&sym, size).expect("pack");
        assert_eq!(inspect_block(&block, size).kind, BlockKind::Normal);
        let decoded = decode_block(&block, size);
        assert_eq!(decoded[0], [0, 0, 0, 255]);
        assert_eq!(decoded[n - 1], [255, 255, 255, 255]);
        assert!(
            decoded.windows(2).all(|w| w[0][0] <= w[1][0]),
            "ramp must be monotone: {decoded:?}"
        );
    }

    // -- Images, edges, determinism, errors -----------------------------------

    #[test]
    fn partial_edge_blocks_round_trip() {
        // Sizes that are not whole blocks, including smaller than one block.
        for (w, h) in [(1u32, 1u32), (3, 5), (10, 7), (17, 13), (31, 1), (1, 29)] {
            let src = image(w, h, |x, y| {
                [(x * 37 % 256) as u8, (y * 53 % 256) as u8, 140, 255]
            });
            for size in BlockSize::ALL {
                let blocks = encode_image(w, h, &src, size, Quality::Fast).expect("encode");
                let expect = (w.div_ceil(size.width()) * h.div_ceil(size.height()) * 16) as usize;
                assert_eq!(blocks.len(), expect, "{w}x{h} {size:?}");
                let back = decode_image(w, h, &blocks, size).expect("decode");
                assert_eq!(back.len(), src.len());
                // Every block in the stream is individually valid.
                for raw in blocks.as_chunks::<16>().0 {
                    let block = *raw;
                    assert_ne!(inspect_block(&block, size).kind, BlockKind::Invalid, "{size:?}");
                }
            }
        }
        // A solid 1x1 image stays exact through the padding path at the largest block.
        let src = vec![9u8, 99, 199, 55];
        let blocks = encode_image(1, 1, &src, BlockSize::Block8x8, Quality::Balanced).expect("encode");
        assert_eq!(blocks.len(), 16);
        assert_eq!(
            decode_image(1, 1, &blocks, BlockSize::Block8x8).expect("decode"),
            src
        );
    }

    #[test]
    fn encoding_is_deterministic() {
        let mut rng = Lcg(0xDE7E);
        let src: Vec<u8> = (0..32 * 16 * 4).map(|_| rng.byte()).collect();
        for size in BlockSize::ALL {
            for quality in [Quality::Fast, Quality::Balanced, Quality::Thorough] {
                let a = encode_image(32, 16, &src, size, quality).expect("encode a");
                let b = encode_image(32, 16, &src, size, quality).expect("encode b");
                assert_eq!(a, b, "{size:?} {quality:?}");
            }
        }
        // Block-level determinism on a single textured block.
        let texels: Vec<[u8; 4]> = (0..36)
            .map(|i| [(i * 11) as u8, (i * 5) as u8, (i * 7) as u8, 200])
            .collect();
        assert_eq!(
            encode_block(&texels, BlockSize::Block6x6, Quality::Thorough),
            encode_block(&texels, BlockSize::Block6x6, Quality::Thorough)
        );
    }

    #[test]
    fn image_apis_reject_bad_inputs_without_panicking() {
        assert!(matches!(
            encode_image(0, 8, &[], BlockSize::Block4x4, Quality::Fast),
            Err(AstcEncodeError::InvalidDimensions { width: 0, height: 8 })
        ));
        assert!(matches!(
            encode_image(4, 4, &[0u8; 10], BlockSize::Block4x4, Quality::Fast),
            Err(AstcEncodeError::BadBuffer { .. })
        ));
        assert!(matches!(
            decode_image(8, 8, &[0u8; 16], BlockSize::Block4x4),
            Err(AstcEncodeError::BadBlocks { blocks: 4, .. })
        ));
        assert!(decode_image(0, 0, &[], BlockSize::Block4x4).is_err());
        assert!(matches!(
            try_encode_block(&[[1, 2, 3, 4]], BlockSize::Block4x4, Quality::Fast),
            Err(AstcEncodeError::BadBlockTexels {
                expected: 16,
                actual: 1
            })
        ));
    }

    #[test]
    fn encode_block_pads_and_truncates_instead_of_panicking() {
        // Empty: defined fallback is a void-extent opaque black block.
        let block = encode_block(&[], BlockSize::Block4x4, Quality::Fast);
        let info = inspect_block(&block, BlockSize::Block4x4);
        assert_eq!(info.kind, BlockKind::VoidExtent);
        assert_eq!(info.constant_color, Some([0, 0, 0, 255]));
        // Short: the last texel repeats, so a lone texel becomes a solid block.
        let block = encode_block(&[[5, 6, 7, 8]], BlockSize::Block8x8, Quality::Fast);
        assert!(
            decode_block(&block, BlockSize::Block8x8)
                .iter()
                .all(|&p| p == [5, 6, 7, 8])
        );
        // Long: extra texels are ignored; the result equals the exact-size encode.
        let many: Vec<[u8; 4]> = (0..100).map(|i| [i as u8, 2, 3, 255]).collect();
        assert_eq!(
            encode_block(&many, BlockSize::Block4x4, Quality::Balanced),
            encode_block(&many[..16], BlockSize::Block4x4, Quality::Balanced)
        );
    }

    // -- Adversarial blocks and the invalid path -------------------------------

    #[test]
    fn invalid_blocks_decode_as_opaque_magenta() {
        // Reserved block mode (the all-zero mode is one of the reserved patterns).
        let mut reserved = [0u8; 16];
        set_bits(&mut reserved, 13, 4, 12);
        assert_eq!(
            inspect_block(&reserved, BlockSize::Block4x4).kind,
            BlockKind::Invalid
        );
        assert!(
            decode_block(&reserved, BlockSize::Block4x4)
                .iter()
                .all(|&p| p == [255, 0, 255, 255])
        );
        // Grid larger than the footprint (8x8 grid in a 4x4 block).
        let mut big_grid = [0u8; 16];
        set_bits(&mut big_grid, 0, 11, encode_block_mode(8, 8, 2).expect("mode"));
        set_bits(&mut big_grid, 13, 4, 12);
        assert_eq!(
            inspect_block(&big_grid, BlockSize::Block4x4).kind,
            BlockKind::Invalid
        );
        // Multiple partitions and an unsupported endpoint mode are outside the subset.
        let mut multi = [0u8; 16];
        set_bits(&mut multi, 0, 11, encode_block_mode(4, 4, 2).expect("mode"));
        set_bits(&mut multi, 11, 2, 1);
        set_bits(&mut multi, 13, 4, 12);
        assert_eq!(
            inspect_block(&multi, BlockSize::Block4x4).kind,
            BlockKind::Invalid
        );
        let mut offset_mode = [0u8; 16];
        set_bits(&mut offset_mode, 0, 11, encode_block_mode(4, 4, 2).expect("mode"));
        set_bits(&mut offset_mode, 13, 4, 9);
        assert_eq!(
            inspect_block(&offset_mode, BlockSize::Block4x4).kind,
            BlockKind::Invalid
        );
        // Every byte pattern decodes to the right *number* of texels without panicking.
        for fill in [0x00u8, 0x55, 0xAA, 0xFF] {
            assert_eq!(decode_block(&[fill; 16], BlockSize::Block5x5).len(), 25);
        }
    }

    #[test]
    fn adversarial_texels_never_panic_and_stay_valid() {
        // One outlier in a flat field, alternating extremes, and seeded chaos at every size
        // and quality: the encoder must always return valid, inspectable blocks.
        let mut rng = Lcg(0xBAD5);
        for size in BlockSize::ALL {
            let n = size.texel_count();
            let mut outlier = vec![[10u8, 10, 10, 255]; n];
            outlier[n / 2] = [255, 255, 255, 255];
            let extremes: Vec<[u8; 4]> = (0..n)
                .map(|i| {
                    if i % 2 == 0 {
                        [0, 0, 0, 0]
                    } else {
                        [255, 255, 255, 255]
                    }
                })
                .collect();
            let chaos: Vec<[u8; 4]> = (0..n)
                .map(|_| [rng.byte(), rng.byte(), rng.byte(), rng.byte()])
                .collect();
            for texels in [&outlier, &extremes, &chaos] {
                for quality in [Quality::Fast, Quality::Balanced, Quality::Thorough] {
                    let block = encode_block(texels, size, quality);
                    assert_ne!(
                        inspect_block(&block, size).kind,
                        BlockKind::Invalid,
                        "{size:?} {quality:?}"
                    );
                    assert_eq!(decode_block(&block, size).len(), n);
                }
            }
            // The outlier block must keep the bright texel bright-ish (it is an endpoint).
            let decoded = round_trip(&outlier, size, Quality::Balanced);
            assert!(
                decoded[n / 2][0] > 180,
                "{size:?} outlier texel {:?}",
                decoded[n / 2]
            );
        }
    }

    #[test]
    fn fitted_non_finite_values_are_sanitized() {
        // The fit can only produce finite values from u8 texels, but the guards are part of
        // the contract: they are exercised directly so a refactor cannot drop them.
        assert_eq!(fit_clamp(f32::NAN), 0.0);
        assert_eq!(fit_clamp(f32::INFINITY), 0.0);
        assert_eq!(fit_clamp(300.0), 255.0);
        assert_eq!(fit_clamp(-4.0), 0.0);
        assert_eq!(to_u8(u32::MAX), 255);
        assert_eq!(unquantize8(u32::MAX, 6), unquantize8(5, 6));
    }

    // -- Speed guard ------------------------------------------------------------

    std::thread_local! {
        /// Candidate blocks packed and decoded for scoring on this thread (the speed guard's
        /// deterministic work measure; test builds only).
        pub(super) static SCORED: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    }

    /// Scored candidates for one `encode_image` call on this thread.
    fn scored_during(f: impl FnOnce()) -> u64 {
        let before = SCORED.with(std::cell::Cell::get);
        f();
        SCORED.with(std::cell::Cell::get) - before
    }

    #[test]
    fn fast_and_balanced_stay_inside_a_generous_time_budget() {
        // A regression tripwire, not a benchmark, measured in work instead of wall-clock time
        // (repo rule: no elapsed-time asserts). Every candidate costs one pack + decode, so
        // the search cannot explode without the count exploding. Per block, Fast fits at most
        // 2 endpoint modes x 3 weight ranges (2 spaced + the endpoint-range-32 anchor) x 3
        // scorings (initial + 2 refits); Balanced at most 3 modes x 6 ranges x 4 scorings
        // (initial + 2 refits + 1 tweak).
        let src = image(128, 128, |x, y| {
            [(x * 2) as u8, (y * 2) as u8, ((x * y) % 256) as u8, 255]
        });
        let mut blocks = Vec::new();
        let fast = scored_during(|| {
            blocks = encode_image(128, 128, &src, BlockSize::Block4x4, Quality::Fast).expect("encode");
        });
        assert_eq!(blocks.len(), 32 * 32 * 16);
        assert!(
            (1024..=1024 * 2 * 3 * 3).contains(&fast),
            "fast 128x128 scored {fast} candidates"
        );
        let quad = image(64, 64, |x, y| {
            [(x * 2) as u8, (y * 2) as u8, ((x * y) % 256) as u8, 255]
        });
        let balanced = scored_during(|| {
            blocks = encode_image(64, 64, &quad, BlockSize::Block6x6, Quality::Balanced).expect("encode");
        });
        assert_eq!(blocks.len(), 11 * 11 * 16);
        assert!(
            (121..=121 * 3 * 6 * 4).contains(&balanced),
            "balanced 64x64 scored {balanced} candidates"
        );
    }

    // -- Cross-check against the reference decoder ------------------------------
    //
    // These compare the module's bytes against the vendored astcenc decoder. They are
    // the conformance gate for every physical-layout detail (block-mode rows, BISE
    // interleaving and packed-field encodings, unquantization tables, infill,
    // blue contraction, void-extent bits): the module's own decoder round-trips by
    // construction, only the reference decoder proves the bytes mean the same thing
    // to a GPU.

    #[test]
    fn reference_decoder_reads_our_gradient() {
        let src = image(64, 64, |x, y| {
            [(x * 255 / 63) as u8, (y * 255 / 63) as u8, 110, 255]
        });
        for size in BlockSize::ALL {
            let blocks = encode_image(64, 64, &src, size, Quality::Balanced).expect("encode");
            let reference =
                crate::astc::decompress_astc_rgba8(64, 64, &blocks, (size.width(), size.height()), false)
                    .expect("reference decode");
            let ours = decode_image(64, 64, &blocks, size).expect("module decode");
            assert!(
                psnr_bytes(&ours, &reference) >= 40.0,
                "{size:?} module vs reference decode disagree"
            );
            assert!(psnr_bytes(&src, &reference) >= 24.0, "{size:?} reference PSNR");
        }
    }

    #[test]
    fn reference_decoder_reads_our_void_extent() {
        let src = [77u8, 150, 20, 200].repeat(16 * 16);
        let blocks = encode_image(16, 16, &src, BlockSize::Block4x4, Quality::Fast).expect("encode");
        let reference =
            crate::astc::decompress_astc_rgba8(16, 16, &blocks, (4, 4), false).expect("reference decode");
        assert!(
            reference.chunks(4).all(|p| p == [77, 150, 20, 200]),
            "void-extent disagrees with the reference decoder"
        );
    }

    #[test]
    fn reference_decoder_agrees_on_every_symbolic_configuration() {
        // For every block size, endpoint mode, and feasible weight range, pack a block
        // with seeded pseudo-random endpoint and weight levels and require the vendored
        // decoder to produce (nearly) the same texels as the module decoder. This
        // sweeps every block-mode row, BISE range shape (bits/trits/quints, including
        // truncated final groups), unquantization table, and the blue-contraction
        // branch (random pairs land on both sides of the RGB-sum comparison).
        let mut rng = Lcg(0xC055);
        let mut checked = 0usize;
        for size in BlockSize::ALL {
            let n = size.texel_count();
            for cem in [
                ColorEndpointMode::Luminance,
                ColorEndpointMode::LuminanceAlpha,
                ColorEndpointMode::Rgb,
                ColorEndpointMode::Rgba,
            ] {
                for (wr, er) in feasible_ranges(cem, size) {
                    let sym = SymbolicBlock {
                        cem,
                        weight_range: wr,
                        endpoint_q: (0..cem.value_count()).map(|_| rng.below(er)).collect(),
                        weight_q: (0..n).map(|_| rng.below(wr)).collect(),
                    };
                    let block = pack_symbolic(&sym, size).expect("pack");
                    let ours: Vec<u8> = decode_block(&block, size)
                        .iter()
                        .flat_map(|p| p.iter().copied())
                        .collect();
                    let (bw, bh) = (size.width(), size.height());
                    let reference = crate::astc::decompress_astc_rgba8(bw, bh, &block, (bw, bh), false)
                        .expect("reference decode");
                    let worst = ours
                        .iter()
                        .zip(&reference)
                        .map(|(a, b)| a.abs_diff(*b))
                        .max()
                        .unwrap_or(0);
                    assert!(
                        worst <= 1,
                        "{size:?} {cem:?} weight {wr} endpoint {er}: texel disagreement {worst}"
                    );
                    checked += 1;
                }
            }
        }
        assert!(checked >= 60, "only {checked} configurations checked");
    }
}
