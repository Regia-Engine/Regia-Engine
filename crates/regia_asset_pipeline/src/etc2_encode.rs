// SPDX-License-Identifier: MPL-2.0
//! ETC2 / EAC block encoder that searches every RGB mode, with the reference decoder its tests
//! measure against.
//!
//! This is the full-mode companion of [`crate::etc2`] (ADR-1379), which encodes only the
//! ETC1-compatible individual and differential modes. Block texels are `[[u8; 4]; 16]` in row
//! order (`y * 4 + x`); the RGB encoders ignore alpha. Formats written:
//!
//! - **ETC2 RGB8** ([`encode_block_rgb`], 8 bytes a 4×4 block): all five modes.
//!   Individual and differential (the ETC1 subset) are searched near-exhaustively — both
//!   sub-block orientations, every intensity modifier table, 4-bit/5-bit base colours from the
//!   sub-block mean plus its ±1 cube, the differential mode keeping the second base inside the
//!   3-bit signed delta range so `base + delta` never leaves 0..=31. T and H are searched over
//!   candidate colour pairs built from the block's means, per-channel extremes and a 2-means
//!   split, every ordered pair and every distance (H only the distances its colour order
//!   allows). Planar is a per-channel least-squares plane through the 16 texels, quantized to
//!   the mode's 6/7-bit anchors. Every candidate is scored by weighted squared error and the
//!   block with the smallest error wins.
//! - **ETC2 RGBA8** ([`encode_block_rgba`], 16 bytes): an EAC 8-bit alpha block followed by an
//!   ETC2 RGB8 block.
//! - **EAC R11** ([`encode_block_r11`], 8 bytes): one unsigned 11-bit channel.
//!
//! The reference decoders ([`decode_block_rgb`], [`decode_block_rgba`], [`decode_block_r11`],
//! [`decode_image`]) and [`classify_block`] are the tests' oracle. The encoder also decodes every
//! block it returns and checks it lands in the mode the search picked, falling back to the
//! individual mode otherwise, so a block never decodes differently than intended.
//!
//! **Bit layouts** (Khronos Data Format Specification, ETC2). A block is a big-endian 64-bit
//! word. Pixel `p = x * 4 + y` (column order) has its 2-bit index MSB at bit `16 + p` and LSB at
//! bit `p`. The ETC1 modes pack `R1|G1|B1` nibbles in the high halves of bytes 0-2 and
//! `R2|G2|B2` in the low halves, the two 3-bit table codewords, the diff bit (33) and the flip
//! bit (32) in byte 3. T, H and planar move their colour fields around a few unused bits that
//! the encoder sets to trip the mode selection (`t_nibbles`, `h_nibbles`, `planar_anchors`
//! document each field). T paints `C1, C2 + d, C2, C2 - d`; H paints `C1 ± d, C2 ± d`, the
//! distance index's lowest bit being whether `C1 >= C2` as 12-bit numbers. The T/H distance
//! table is `3, 6, 11, 16, 23, 32, 41, 64`. An EAC block packs
//! `base(8) | multiplier(4) | table(4) | 16 × 3-bit indices` from bit 47 down.
//!
//! **Mode selection.** The diff bit 0 always selects the individual mode; with the diff bit 1,
//! the three 5-bit + 3-bit sums are checked in the order R, G, B: the R sum outside 0..=31 is
//! T, else the G sum outside is H, else the B sum outside is planar, and all inside is the
//! differential mode.
//!
//! **Verification.** The ETC1-mode and EAC layouts, tables and decode rules are byte-identical
//! to the ones in [`crate::etc2`] (ADR-1379). The T, H and planar decoders were cross-checked
//! (ADR-2333) against an independent MIT/Apache decoder (`texture2ddecoder` 0.1.2, in a scratch
//! crate outside the repository): bit-exact on random block words of every mode and on every
//! block this encoder writes. The delivered version of this module read the three ETC2 modes
//! wrongly; [`ModeSet::ETC1`] still restricts encoding to the two ETC1 modes when wanted.
//!
//! Determinism: no hash maps, no wall clock, no randomness in the encoder — the same texels and
//! options always produce the same bytes. Inputs are bytes, so NaN cannot enter; the planar
//! least-squares solve works on a fixed 3×3 system with determinant 6400 and cannot divide by
//! zero.

use std::fmt;

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

/// The T and H mode distance table, indexed by the 3-bit distance index (T: `da(2) db(1)`; H:
/// `da db` and the colour-order bit).
const DISTANCES: [i32; 8] = [3, 6, 11, 16, 23, 32, 41, 64];

/// The T mode's unused bits (63..61 and 58): set so the R sum leaves 0..=31.
const T_FREE: u64 = (0b111 << 61) | (1 << 58);

/// The H mode's unused bits (63 keeps the R sum inside 0..=31; 55..53 and 50 push the G sum out).
const H_FREE: u64 = (1 << 63) | (0b111 << 53) | (1 << 50);

/// The planar mode's unused bits (63 and 55 keep the R and G sums inside 0..=31; 47..45 and 42
/// push the B sum out).
const PLANAR_FREE: u64 = (1 << 63) | (1 << 55) | (0b111 << 45) | (1 << 42);

/// A hand-built differential grey block (base 16, delta 0, table 0, all indices +2) that decodes
/// as grey 134 in every texel. Returned when every mode search is disabled.
const FALLBACK_BITS: u64 = (16u64 << 59) | (16 << 51) | (16 << 43) | (1 << 33);

/// Expands a 4-bit colour component to 8 bits.
const fn expand4(c: u8) -> i32 {
    ((c << 4) | c) as i32
}

/// Expands a 5-bit colour component to 8 bits.
const fn expand5(c: u8) -> i32 {
    ((c << 3) | (c >> 2)) as i32
}

/// Expands a 6-bit planar anchor component to 8 bits.
const fn expand6(c: u8) -> i32 {
    ((c << 2) | (c >> 4)) as i32
}

/// Expands a 7-bit planar anchor component to 8 bits.
const fn expand7(c: u8) -> i32 {
    ((c << 1) | (c >> 6)) as i32
}

/// A 3-bit two's-complement value as `i32`.
const fn sext3(v: u64) -> i32 {
    let v = v as i32;
    if v >= 4 { v - 8 } else { v }
}

/// Pixel `(x, y)` of a block is pixel number `x * 4 + y` in every ETC and EAC index field.
const fn pixel_number(x: usize, y: usize) -> usize {
    x * 4 + y
}

/// An 8-bit value on the 11-bit EAC scale.
fn to_eleven(v: u8) -> i32 {
    (i32::from(v) * 2047 + 127) / 255
}

/// An 11-bit EAC value back to 8 bits.
fn from_eleven(v: i32) -> u8 {
    ((v * 255 + 1023) / 2047).clamp(0, 255) as u8
}

/// Which ETC2 RGB mode a block decodes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RgbMode {
    /// Two independent 4-bit base colours (the ETC1 "individual" mode).
    Individual,
    /// A 5-bit base colour and a 3-bit signed delta per channel (the ETC1 "differential" mode).
    Differential,
    /// Two base colours, one repeated, plus its shifted copy (the iPACKMAN-derived "T" mode).
    T,
    /// Two base colours plus their average ± a distance (the iPACKMAN-derived "H" mode).
    H,
    /// Three anchor colours interpolated over the block (the "planar" mode).
    Planar,
}

/// The set of RGB block modes [`EncodeOptions`] may use.
///
/// The constants cover the common cases; the fields allow any subset. Ties between modes are
/// broken in the order individual, differential, T, H, planar, so a restricted set only changes
/// which block wins, never the searches themselves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModeSet {
    /// Search the ETC1 individual mode (two 4-bit base colours).
    pub individual: bool,
    /// Search the ETC1 differential mode (5-bit base + 3-bit signed delta).
    pub differential: bool,
    /// Search the ETC2 T mode.
    pub t: bool,
    /// Search the ETC2 H mode.
    pub h: bool,
    /// Search the ETC2 planar mode.
    pub planar: bool,
}

impl ModeSet {
    /// Every mode (the default).
    pub const ALL: Self = Self {
        individual: true,
        differential: true,
        t: true,
        h: true,
        planar: true,
    };

    /// Only the two ETC1-compatible modes — the subset whose bit layout is byte-identical to the
    /// GPU-validated encoder in [`crate::etc2`].
    pub const ETC1: Self = Self {
        individual: true,
        differential: true,
        t: false,
        h: false,
        planar: false,
    };

    /// Whether any mode is enabled.
    #[must_use]
    pub fn any(self) -> bool {
        self.individual || self.differential || self.t || self.h || self.planar
    }
}

impl Default for ModeSet {
    fn default() -> Self {
        Self::ALL
    }
}

/// Options for the block searches.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EncodeOptions {
    /// Weight the channels 2 : 4 : 1 (an integer round of the Rec. 709 luminance weights) in
    /// the RGB mode search instead of treating them equally. This changes which block wins,
    /// never the search order; EAC blocks carry one channel and ignore it.
    pub perceptual: bool,
    /// Which RGB modes may be used.
    pub modes: ModeSet,
}

/// The formats [`encode_image`] writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Etc2Format {
    /// ETC2 RGB8, 8 bytes a block, alpha ignored.
    Rgb8,
    /// ETC2 RGBA8: an EAC alpha block then an ETC2 RGB block, 16 bytes a block.
    Rgba8,
    /// EAC R11 unsigned (the R channel), 8 bytes a block.
    R11,
}

impl Etc2Format {
    /// Bytes per 4×4 block.
    #[must_use]
    pub const fn block_bytes(self) -> usize {
        match self {
            Self::Rgb8 | Self::R11 => 8,
            Self::Rgba8 => 16,
        }
    }
}

/// ETC2 / EAC encode or decode failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Etc2EncodeError {
    /// The pixel buffer is not `width × height × 4` bytes, or a dimension is zero.
    BadBuffer {
        /// Pixel buffer length.
        len: usize,
        /// Image width.
        width: u32,
        /// Image height.
        height: u32,
    },
    /// The block data is not a whole number of blocks for the size.
    BadBlockData {
        /// Data length.
        len: usize,
        /// Length the size needs.
        needed: usize,
    },
}

impl fmt::Display for Etc2EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::BadBuffer { len, width, height } => {
                write!(f, "rgba buffer length {len} does not match {width}x{height}x4")
            }
            Self::BadBlockData { len, needed } => {
                write!(f, "block data length {len} is not the needed {needed}")
            }
        }
    }
}

impl std::error::Error for Etc2EncodeError {}

/// Channel weights for the RGB searches: `1,1,1` uniform or `2,4,1` perceptual.
fn weights(opts: &EncodeOptions) -> [u32; 3] {
    if opts.perceptual { [2, 4, 1] } else { [1, 1, 1] }
}

// ---------------------------------------------------------------------------
// Mode classification and reference decoding
// ---------------------------------------------------------------------------

/// The three 5-bit + 3-bit differential sums (R, G, B) the mode selection reads from a block.
fn block_sums(bits: u64) -> [i32; 3] {
    [
        ((bits >> 59) & 0x1f) as i32 + sext3((bits >> 56) & 7),
        ((bits >> 51) & 0x1f) as i32 + sext3((bits >> 48) & 7),
        ((bits >> 43) & 0x1f) as i32 + sext3((bits >> 40) & 7),
    ]
}

/// Which ETC2 RGB mode a block decodes through: diff bit 0 is always the individual mode; with
/// the diff bit 1, the R sum outside 0..=31 is T, else the G sum outside is H, else the B sum
/// outside is planar, and all inside is the differential mode.
#[must_use]
pub fn classify_block(block: &[u8; 8]) -> RgbMode {
    let bits = u64::from_be_bytes(*block);
    let [r, g, b] = block_sums(bits);
    if (bits >> 33) & 1 == 0 {
        RgbMode::Individual
    } else if !(0..=31).contains(&r) {
        RgbMode::T
    } else if !(0..=31).contains(&g) {
        RgbMode::H
    } else if !(0..=31).contains(&b) {
        RgbMode::Planar
    } else {
        RgbMode::Differential
    }
}

/// Whether pixel `(x, y)` is in the second sub-block (`flip`: 4×2 halves stacked, else 2×4
/// halves side by side).
const fn second_half(flip: bool, x: usize, y: usize) -> bool {
    if flip { y >= 2 } else { x >= 2 }
}

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

/// The two 4-bit base colours of a T block: `R1` split over bits 60..59 and 57..56, `G1`
/// 55..52, `B1` 51..48, `R2` 47..44, `G2` 43..40, `B2` 39..36.
fn t_nibbles(bits: u64) -> [[u8; 3]; 2] {
    let nib = |shift: u32| ((bits >> shift) & 0xf) as u8;
    let r1 = ((((bits >> 59) & 3) << 2) | ((bits >> 56) & 3)) as u8;
    [[r1, nib(52), nib(48)], [nib(44), nib(40), nib(36)]]
}

/// The two 4-bit base colours of an H block: `R1` 62..59, `G1` split over 58..56 and 52, `B1`
/// over 51 and 49..47, `R2` 46..43, `G2` 42..39, `B2` 38..35.
fn h_nibbles(bits: u64) -> [[u8; 3]; 2] {
    let nib = |shift: u32| ((bits >> shift) & 0xf) as u8;
    let g1 = ((((bits >> 56) & 7) << 1) | ((bits >> 52) & 1)) as u8;
    let b1 = ((((bits >> 51) & 1) << 3) | ((bits >> 47) & 7)) as u8;
    [[nib(59), g1, b1], [nib(43), nib(39), nib(35)]]
}

/// The H mode's colour-order bit, the lowest bit of its distance index: 1 when the first base
/// colour, read as the 12-bit number `R1 G1 B1`, is at least the second.
fn h_order_bit(c1: [u8; 3], c2: [u8; 3]) -> u64 {
    let key = |c: [u8; 3]| (u32::from(c[0]) << 8) | (u32::from(c[1]) << 4) | u32::from(c[2]);
    u64::from(key(c1) >= key(c2))
}

/// `colour` plus `d` in every channel, clamped to 0..=255.
fn offset(colour: [i32; 3], d: i32) -> [i32; 3] {
    colour.map(|v| (v + d).clamp(0, 255))
}

/// The four paint colours of a T block: `C1`, `C2 + d`, `C2`, `C2 - d`, with the distance
/// index `da(2) db(1)` (bits 35..34 and 32).
fn t_paints(bits: u64) -> [[i32; 3]; 4] {
    let [c1, c2] = t_nibbles(bits).map(|c| c.map(expand4));
    let dist = DISTANCES[((((bits >> 34) & 3) << 1) | ((bits >> 32) & 1)) as usize];
    [c1, offset(c2, dist), c2, offset(c2, -dist)]
}

/// The four paint colours of an H block: `C1 + d`, `C1 - d`, `C2 + d`, `C2 - d`, with the
/// distance index `da db order` (bits 34 and 32, then [`h_order_bit`]).
fn h_paints(bits: u64) -> [[i32; 3]; 4] {
    let [n1, n2] = h_nibbles(bits);
    let index = (((bits >> 34) & 1) << 2) | (((bits >> 32) & 1) << 1) | h_order_bit(n1, n2);
    let dist = DISTANCES[index as usize];
    let (c1, c2) = (n1.map(expand4), n2.map(expand4));
    [
        offset(c1, dist),
        offset(c1, -dist),
        offset(c2, dist),
        offset(c2, -dist),
    ]
}

/// The three planar anchor colours (O, H, V) of a block, expanded to 8 bits: `RO` 62..57, `GO`
/// over 56 and 54..49, `BO` over 48, 44..43 and 41..39, `RH` over 38..34 and 32, `GH` 31..25,
/// `BH` 24..19, `RV` 18..13, `GV` 12..6, `BV` 5..0.
fn planar_anchors(bits: u64) -> [[i32; 3]; 3] {
    let field = |shift: u32, width: u32| ((bits >> shift) & ((1 << width) - 1)) as u8;
    let ro = field(57, 6);
    let go = (field(56, 1) << 6) | field(49, 6);
    let bo = (field(48, 1) << 5) | (field(43, 2) << 3) | field(39, 3);
    let rh = (field(34, 5) << 1) | field(32, 1);
    [
        [expand6(ro), expand7(go), expand6(bo)],
        [expand6(rh), expand7(field(25, 7)), expand6(field(19, 6))],
        [expand6(field(13, 6)), expand7(field(6, 7)), expand6(field(0, 6))],
    ]
}

/// Sets the mode's unused bits (`free`) so the block decodes through `mode`: every subset of
/// `free` in increasing order, the first that classifies as `mode`. Each T, H and planar layout
/// has such a subset for any colour bits (the free bits sit at the top of the 5-bit sums and
/// deltas the selection reads), so `None` only means a caller bug.
fn force_mode(bits: u64, free: u64, mode: RgbMode) -> Option<u64> {
    let base = bits & !free;
    let mut subset = 0u64;
    loop {
        if classify_block(&(base | subset).to_be_bytes()) == mode {
            return Some(base | subset);
        }
        if subset == free {
            return None;
        }
        subset = subset.wrapping_sub(free) & free;
    }
}

/// The planar reconstruction of one channel: the anchors interpolated with H and V sitting one
/// texel step beyond the block edge, rounded to nearest by the `+2` term.
fn planar_channel(x: i32, y: i32, o: i32, h: i32, v: i32) -> i32 {
    ((x * (h - o) + y * (v - o) + 4 * o + 2) >> 2).clamp(0, 255)
}

/// Decodes one ETC2 RGB8 block into 16 RGBA texels (row order `y * 4 + x`, alpha 255), by the
/// mode [`classify_block`] selects.
#[must_use]
pub fn decode_block_rgb(block: &[u8; 8]) -> [[u8; 4]; 16] {
    let bits = u64::from_be_bytes(*block);
    let mut out = [[0u8, 0, 0, 255]; 16];
    match classify_block(block) {
        RgbMode::Individual | RgbMode::Differential => {
            let flip = (bits >> 32) & 1 == 1;
            let pairs: [(i32, i32); 3] = std::array::from_fn(|c| {
                if (bits >> 33) & 1 == 1 {
                    let shift = 59 - 8 * c;
                    let base = ((bits >> shift) & 0x1f) as u8;
                    let delta = sext3((bits >> (shift - 3)) & 7);
                    let second = i32::from(base) + delta;
                    (expand5(base), expand5(second.clamp(0, 31) as u8))
                } else {
                    let shift = 60 - 8 * c;
                    (
                        expand4(((bits >> shift) & 0xf) as u8),
                        expand4(((bits >> (shift - 4)) & 0xf) as u8),
                    )
                }
            });
            let bases = [pairs.map(|p| p.0), pairs.map(|p| p.1)];
            let tables = [
                usize::from(((bits >> 37) & 7) as u8),
                usize::from(((bits >> 34) & 7) as u8),
            ];
            for y in 0..4usize {
                for x in 0..4usize {
                    let p = pixel_number(x, y);
                    let index = ((((bits >> (16 + p)) & 1) << 1) | ((bits >> p) & 1)) as usize;
                    let h = usize::from(second_half(flip, x, y));
                    let m = modifier(tables[h], index);
                    let t = &mut out[y * 4 + x];
                    for c in 0..3usize {
                        t[c] = (bases[h][c] + m).clamp(0, 255) as u8;
                    }
                }
            }
        }
        RgbMode::T | RgbMode::H => {
            let paints = if classify_block(block) == RgbMode::T {
                t_paints(bits)
            } else {
                h_paints(bits)
            };
            for y in 0..4usize {
                for x in 0..4usize {
                    let p = pixel_number(x, y);
                    let index = ((((bits >> (16 + p)) & 1) << 1) | ((bits >> p) & 1)) as usize;
                    let paint = paints[index];
                    let t = &mut out[y * 4 + x];
                    for c in 0..3usize {
                        t[c] = paint[c] as u8;
                    }
                }
            }
        }
        RgbMode::Planar => {
            let [o, h, v] = planar_anchors(bits);
            for y in 0..4usize {
                for x in 0..4usize {
                    let t = &mut out[y * 4 + x];
                    for c in 0..3usize {
                        t[c] = planar_channel(x as i32, y as i32, o[c], h[c], v[c]) as u8;
                    }
                }
            }
        }
    }
    out
}

/// The value an EAC block's pixel index decodes to: 8-bit alpha, or 11-bit with `eleven`
/// (`base * 8 + 4 + modifier * multiplier * 8`, the multiplier-0 fine step included).
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

/// Decodes one EAC block's 8-bit alpha values, by pixel number (`x * 4 + y`).
fn decode_eac_alpha(bits: u64) -> [i32; 16] {
    let base = ((bits >> 56) & 0xff) as i32;
    let multiplier = ((bits >> 52) & 0xf) as i32;
    let table = usize::from(((bits >> 48) & 0xf) as u8);
    let mut out = [0i32; 16];
    for (p, v) in out.iter_mut().enumerate() {
        let index = ((bits >> (45 - 3 * p)) & 7) as usize;
        *v = eac_value(base, multiplier, table, index, false);
    }
    out
}

/// Decodes one EAC R11 block into 16 11-bit values, by pixel number.
fn decode_eac_eleven(bits: u64) -> [i32; 16] {
    let base = ((bits >> 56) & 0xff) as i32;
    let multiplier = ((bits >> 52) & 0xf) as i32;
    let table = usize::from(((bits >> 48) & 0xf) as u8);
    let mut out = [0i32; 16];
    for (p, v) in out.iter_mut().enumerate() {
        let index = ((bits >> (45 - 3 * p)) & 7) as usize;
        *v = eac_value(base, multiplier, table, index, true);
    }
    out
}

/// Decodes one ETC2 RGBA8 block (EAC alpha word first) into 16 RGBA texels, row order
/// `y * 4 + x`.
#[must_use]
pub fn decode_block_rgba(block: &[u8; 16]) -> [[u8; 4]; 16] {
    let mut alpha_word = [0u8; 8];
    alpha_word.copy_from_slice(&block[..8]);
    let alpha = decode_eac_alpha(u64::from_be_bytes(alpha_word));
    let mut rgb_block = [0u8; 8];
    rgb_block.copy_from_slice(&block[8..16]);
    let mut out = decode_block_rgb(&rgb_block);
    for y in 0..4usize {
        for x in 0..4usize {
            out[y * 4 + x][3] = alpha[pixel_number(x, y)] as u8;
        }
    }
    out
}

/// Decodes one EAC R11 block into 16 R values (row order `y * 4 + x`), rounded to 8 bits the
/// same way [`encode_block_r11`] prepared its targets.
#[must_use]
pub fn decode_block_r11(block: &[u8; 8]) -> [u8; 16] {
    let values = decode_eac_eleven(u64::from_be_bytes(*block));
    let mut out = [0u8; 16];
    for y in 0..4usize {
        for x in 0..4usize {
            out[y * 4 + x] = from_eleven(values[pixel_number(x, y)]);
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Encoding: shared machinery
// ---------------------------------------------------------------------------

/// A fully packed candidate block with its weighted squared error and the mode the search that
/// produced it intended.
struct Cand {
    err: u64,
    bits: u64,
    mode: RgbMode,
}

/// The 16 texels as `i32` RGB triples in row order, ready for error maths.
fn texel_block(texels: &[[u8; 4]; 16]) -> [[i32; 3]; 16] {
    let mut out = [[0i32; 3]; 16];
    for (o, t) in out.iter_mut().zip(texels.iter()) {
        for c in 0..3usize {
            o[c] = i32::from(t[c]);
        }
    }
    out
}

/// The row-order texel indices of the two sub-blocks for an orientation.
const fn half_pixels(flip: bool) -> [[usize; 8]; 2] {
    if flip {
        [[0, 1, 2, 3, 4, 5, 6, 7], [8, 9, 10, 11, 12, 13, 14, 15]]
    } else {
        [[0, 1, 4, 5, 8, 9, 12, 13], [2, 3, 6, 7, 10, 11, 14, 15]]
    }
}

/// The rounded mean colour of eight texels, in 0..=255.
fn mean_of(texels: &[[i32; 3]; 16], pixels: &[usize; 8]) -> [i32; 3] {
    let mut sum = [0i32; 3];
    for &i in pixels {
        for c in 0..3usize {
            sum[c] += texels[i][c];
        }
    }
    sum.map(|s| (s + 4) / 8)
}

/// Quantizes an 8-bit value to `levels` steps (15, 31, 63 or 127), rounded.
fn quantize(v: i32, levels: i32) -> i32 {
    (v * levels + 127) / 255
}

#[cfg(test)]
thread_local! {
    /// Candidate evaluations (`fit_half` and `fit_paints` calls) on this thread: the tests'
    /// deterministic work bound.
    static EVALS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// Counts one candidate evaluation (test builds only; a no-op otherwise).
fn count_eval() {
    #[cfg(test)]
    EVALS.with(|c| c.set(c.get() + 1));
}

/// The best intensity table and its squared error for one sub-block against `base`: every table,
/// every pixel's best of its four modifiers, with an early out once a table cannot beat the best
/// so far. When no channel of `base` can clamp for any modifier (the common case), the per-pixel
/// error is the algebraic form `u + 2·m·v + wsum·m²` of `sum_c w_c·(base_c + m − texel_c)²`;
/// otherwise it is computed directly with the clamp.
fn fit_half(texels: &[[i32; 3]; 16], pixels: &[usize; 8], base: [i32; 3], w: &[u32; 3]) -> (u64, u8) {
    count_eval();
    let wsum = (w[0] + w[1] + w[2]) as i64;
    let mut best_err = u64::MAX;
    let mut best_table = 0u8;
    for (ti, &[s, l]) in INTENSITY.iter().enumerate() {
        let mods = [s, l, -s, -l];
        let safe = base.iter().all(|&b| b + l <= 255 && b - l >= 0);
        let mut err = 0u64;
        for &i in pixels {
            let t = texels[i];
            let mut pixel_err = u64::MAX;
            if safe {
                let mut u = 0i64;
                let mut v = 0i64;
                for c in 0..3usize {
                    let d = i64::from(base[c] - t[c]);
                    u += i64::from(w[c]) * d * d;
                    v += i64::from(w[c]) * d;
                }
                for &m in &mods {
                    let e = u + 2 * i64::from(m) * v + wsum * i64::from(m) * i64::from(m);
                    if e >= 0 && (e as u64) < pixel_err {
                        pixel_err = e as u64;
                    }
                }
            } else {
                for &m in &mods {
                    let mut e = 0u64;
                    for c in 0..3usize {
                        let d = (base[c] + m).clamp(0, 255) - t[c];
                        e += u64::from(w[c]) * (d * d) as u64;
                    }
                    if e < pixel_err {
                        pixel_err = e;
                    }
                }
            }
            err += pixel_err;
            if err >= best_err {
                break;
            }
        }
        if err < best_err {
            best_err = err;
            best_table = ti as u8;
        }
    }
    (best_err, best_table)
}

/// The per-pixel modifier choices (0..=3) of one sub-block for a known base and table, in the
/// local order of `pixels`. Ties go to the lower modifier index.
fn selectors_half(
    texels: &[[i32; 3]; 16],
    pixels: &[usize; 8],
    base: [i32; 3],
    table: u8,
    w: &[u32; 3],
) -> [u8; 8] {
    let mut out = [0u8; 8];
    for (k, &i) in pixels.iter().enumerate() {
        let t = texels[i];
        let mut best = (u64::MAX, 0usize);
        for index in 0..4usize {
            let m = modifier(usize::from(table), index);
            let mut e = 0u64;
            for c in 0..3usize {
                let d = (base[c] + m).clamp(0, 255) - t[c];
                e += u64::from(w[c]) * (d * d) as u64;
            }
            if e < best.0 {
                best = (e, index);
            }
        }
        out[k] = best.1 as u8;
    }
    out
}

/// Writes one sub-block's selector bits into a block word.
fn pack_selectors(bits: &mut u64, pixels: &[usize; 8], sels: &[u8; 8]) {
    for (k, &i) in pixels.iter().enumerate() {
        let p = pixel_number(i % 4, i / 4);
        let s = u64::from(sels[k]);
        *bits |= ((s >> 1) & 1) << (16 + p);
        *bits |= (s & 1) << p;
    }
}

/// Packs the table and flip fields shared by the individual and differential layouts.
fn pack_etc1_common(bits: &mut u64, tables: [u8; 2], flip: bool) {
    *bits |= (u64::from(tables[0]) & 7) << 37;
    *bits |= (u64::from(tables[1]) & 7) << 34;
    if flip {
        *bits |= 1 << 32;
    }
}

/// The ±1 cube around a quantized colour, each channel clamped to `lo..=hi`, as 4-bit nibbles.
fn cube_around(q: [u8; 3], lo: i32, hi: i32) -> Vec<[u8; 3]> {
    let mut out = Vec::with_capacity(27);
    for dr in -1i32..=1 {
        for dg in -1i32..=1 {
            for db in -1i32..=1 {
                let c = [i32::from(q[0]) + dr, i32::from(q[1]) + dg, i32::from(q[2]) + db]
                    .map(|v| v.clamp(lo, hi) as u8);
                out.push(c);
            }
        }
    }
    out
}

/// Searches the individual mode: both orientations, each sub-block's 4-bit colour from its mean
/// ±1 in every channel, every intensity table. The diff bit stays 0, so the block always decodes
/// as the individual mode.
fn search_individual(texels: &[[i32; 3]; 16], w: &[u32; 3]) -> Option<Cand> {
    let mut best: Option<Cand> = None;
    for flip in [false, true] {
        let halves = half_pixels(flip);
        let mut fits = [(0u64, [0u8; 3], 0u8, [0i32; 3]); 2];
        for h in 0..2usize {
            let mean = mean_of(texels, &halves[h]);
            let q = mean.map(|m| quantize(m, 15).clamp(0, 15) as u8);
            let mut best_half = (u64::MAX, [0u8; 3], 0u8, [0i32; 3]);
            for nib in cube_around(q, 0, 15) {
                let base = nib.map(expand4);
                let (err, table) = fit_half(texels, &halves[h], base, w);
                if err < best_half.0 {
                    best_half = (err, nib, table, base);
                }
            }
            fits[h] = best_half;
        }
        let total = fits[0].0 + fits[1].0;
        if best.as_ref().is_none_or(|b| total < b.err) {
            let mut bits = 0u64;
            for c in 0..3usize {
                bits |= u64::from(fits[0].1[c]) << (60 - 8 * c);
                bits |= u64::from(fits[1].1[c]) << (56 - 8 * c);
            }
            pack_etc1_common(&mut bits, [fits[0].2, fits[1].2], flip);
            let s0 = selectors_half(texels, &halves[0], fits[0].3, fits[0].2, w);
            let s1 = selectors_half(texels, &halves[1], fits[1].3, fits[1].2, w);
            pack_selectors(&mut bits, &halves[0], &s0);
            pack_selectors(&mut bits, &halves[1], &s1);
            best = Some(Cand {
                err: total,
                bits,
                mode: RgbMode::Individual,
            });
        }
    }
    best
}

/// One memoized `fit_half` over 5-bit colours for a fixed sub-block, keyed by the packed 15-bit
/// colour: the differential search re-fits the same second colours many times.
struct Fit5Memo<'a> {
    texels: &'a [[i32; 3]; 16],
    pixels: &'a [usize; 8],
    w: &'a [u32; 3],
    seen: Vec<(u16, u64, u8)>,
}

impl Fit5Memo<'_> {
    fn new<'a>(texels: &'a [[i32; 3]; 16], pixels: &'a [usize; 8], w: &'a [u32; 3]) -> Fit5Memo<'a> {
        Fit5Memo {
            texels,
            pixels,
            w,
            seen: Vec::new(),
        }
    }

    fn fit(&mut self, colour: [u8; 3]) -> (u64, u8) {
        let key = (u16::from(colour[0]) << 10) | (u16::from(colour[1]) << 5) | u16::from(colour[2]);
        for &(k, err, table) in &self.seen {
            if k == key {
                return (err, table);
            }
        }
        let base = colour.map(expand5);
        let (err, table) = fit_half(self.texels, self.pixels, base, self.w);
        self.seen.push((key, err, table));
        (err, table)
    }
}

/// A differential pick: total error, the two 5-bit colours and their two table codewords.
type DiffPick = (u64, [u8; 3], [u8; 3], u8, u8);

/// Searches the differential mode: both orientations, the first sub-block's 5-bit colour from
/// its mean ±1 in every channel, the second from its mean clamped into the 3-bit signed delta
/// range of the first (so every `base + delta` stays in 0..=31 by construction) and then varied
/// ±1 per channel inside that range. All intensity tables, memoized per colour.
fn search_differential(texels: &[[i32; 3]; 16], w: &[u32; 3]) -> Option<Cand> {
    let mut best: Option<Cand> = None;
    for flip in [false, true] {
        let halves = half_pixels(flip);
        let mean0 = mean_of(texels, &halves[0]);
        let mean1 = mean_of(texels, &halves[1]);
        let q0 = mean0.map(|m| quantize(m, 31).clamp(0, 31) as u8);
        let q1 = mean1.map(|m| quantize(m, 31).clamp(0, 31) as u8);
        let mut memo0 = Fit5Memo::new(texels, &halves[0], w);
        let mut memo1 = Fit5Memo::new(texels, &halves[1], w);
        let best_total = best.as_ref().map_or(u64::MAX, |c| c.err);
        let mut best_flip: Option<DiffPick> = None;
        for a in cube_around(q0, 0, 31) {
            let (ea, ta) = memo0.fit(a);
            if ea >= best_total || best_flip.as_ref().is_some_and(|b| ea >= b.0) {
                continue;
            }
            // the second colour's window: within the 3-bit signed delta of `a`, and 0..=31
            let windows = a.map(|v| {
                let lo = (i32::from(v) - 4).max(0);
                let hi = (i32::from(v) + 3).min(31);
                (lo, hi)
            });
            for d0 in -1i32..=1 {
                for d1 in -1i32..=1 {
                    for d2 in -1i32..=1 {
                        let pick = |ch: usize, d: i32| -> u8 {
                            let (lo, hi) = windows[ch];
                            let c = i32::from(q1[ch]).clamp(lo, hi) + d;
                            c.clamp(lo, hi) as u8
                        };
                        let b = [pick(0, d0), pick(1, d1), pick(2, d2)];
                        let (eb, tb) = memo1.fit(b);
                        let total = ea + eb;
                        if best_flip.as_ref().is_none_or(|f| total < f.0) {
                            best_flip = Some((total, a, b, ta, tb));
                        }
                    }
                }
            }
        }
        if let Some((total, a, b, ta, tb)) = best_flip {
            let mut bits = 1u64 << 33;
            for c in 0..3usize {
                let shift = 59 - 8 * c;
                bits |= u64::from(a[c]) << shift;
                bits |= (u64::from(b[c].wrapping_sub(a[c])) & 7) << (shift - 3);
            }
            pack_etc1_common(&mut bits, [ta, tb], flip);
            let base_a = a.map(expand5);
            let base_b = b.map(expand5);
            let s0 = selectors_half(texels, &halves[0], base_a, ta, w);
            let s1 = selectors_half(texels, &halves[1], base_b, tb, w);
            pack_selectors(&mut bits, &halves[0], &s0);
            pack_selectors(&mut bits, &halves[1], &s1);
            if best.as_ref().is_none_or(|c| total < c.err) {
                best = Some(Cand {
                    err: total,
                    bits,
                    mode: RgbMode::Differential,
                });
            }
        }
    }
    best
}

/// The weighted squared error and per-pixel paint choices (by pixel number) of fitting four
/// paints to the block. Ties go to the lower paint index.
fn fit_paints(texels: &[[i32; 3]; 16], paints: &[[i32; 3]; 4], w: &[u32; 3]) -> (u64, [u8; 16]) {
    count_eval();
    let mut err = 0u64;
    let mut sels = [0u8; 16];
    for y in 0..4usize {
        for x in 0..4usize {
            let t = texels[y * 4 + x];
            let mut best = (u64::MAX, 0usize);
            for (pi, paint) in paints.iter().enumerate() {
                let mut e = 0u64;
                for c in 0..3usize {
                    let d = paint[c] - t[c];
                    e += u64::from(w[c]) * (d * d) as u64;
                }
                if e < best.0 {
                    best = (e, pi);
                }
            }
            err += best.0;
            sels[pixel_number(x, y)] = best.1 as u8;
        }
    }
    (err, sels)
}

/// The candidate colour nibbles for the T and H searches: the block mean and the four sub-block
/// means (both orientations) quantized to 4 bits, plus the per-channel min and max corners.
fn paint_palette(texels: &[[i32; 3]; 16]) -> Vec<[u8; 3]> {
    fn push(list: &mut Vec<[u8; 3]>, c: [i32; 3]) {
        let nib = c.map(|v| quantize(v, 15).clamp(0, 15) as u8);
        if !list.contains(&nib) {
            list.push(nib);
        }
    }
    let mut list: Vec<[u8; 3]> = Vec::new();
    push(
        &mut list,
        [
            (texels.iter().map(|t| t[0]).sum::<i32>() + 8) / 16,
            (texels.iter().map(|t| t[1]).sum::<i32>() + 8) / 16,
            (texels.iter().map(|t| t[2]).sum::<i32>() + 8) / 16,
        ],
    );
    for flip in [false, true] {
        let halves = half_pixels(flip);
        for half in &halves {
            push(&mut list, mean_of(texels, half));
        }
    }
    let lo = [
        texels.iter().map(|t| t[0]).min().unwrap_or(0),
        texels.iter().map(|t| t[1]).min().unwrap_or(0),
        texels.iter().map(|t| t[2]).min().unwrap_or(0),
    ];
    let hi = [
        texels.iter().map(|t| t[0]).max().unwrap_or(0),
        texels.iter().map(|t| t[1]).max().unwrap_or(0),
        texels.iter().map(|t| t[2]).max().unwrap_or(0),
    ];
    push(&mut list, lo);
    push(&mut list, hi);
    for centre in two_means(texels) {
        push(&mut list, centre);
    }
    list
}

/// The two centres of a 2-means split of the block's colours: seeded with the texels at the
/// two ends of the widest channel (lowest index on ties), then four Lloyd steps (each texel to
/// the nearer centre, ties to the first; a centre that loses every texel keeps its place).
/// Deterministic: integer means, fixed iteration count.
fn two_means(texels: &[[i32; 3]; 16]) -> [[i32; 3]; 2] {
    let range = |c: usize| {
        let lo = texels.iter().map(|t| t[c]).min().unwrap_or(0);
        let hi = texels.iter().map(|t| t[c]).max().unwrap_or(0);
        hi - lo
    };
    let mut wide = 0usize;
    for c in 1..3usize {
        if range(c) > range(wide) {
            wide = c;
        }
    }
    let mut lo_i = 0usize;
    let mut hi_i = 0usize;
    for (i, t) in texels.iter().enumerate() {
        if t[wide] < texels[lo_i][wide] {
            lo_i = i;
        }
        if t[wide] > texels[hi_i][wide] {
            hi_i = i;
        }
    }
    let mut centres = [texels[lo_i], texels[hi_i]];
    for _ in 0..4 {
        let mut sums = [[0i32; 3]; 2];
        let mut counts = [0i32; 2];
        for t in texels {
            let d = centres.map(|c| (0..3usize).map(|k| (c[k] - t[k]).pow(2)).sum::<i32>());
            let k = usize::from(d[1] < d[0]);
            for (s, v) in sums[k].iter_mut().zip(t) {
                *s += v;
            }
            counts[k] += 1;
        }
        for k in 0..2usize {
            if counts[k] > 0 {
                centres[k] = sums[k].map(|s| (s + counts[k] / 2) / counts[k]);
            }
        }
    }
    centres
}

/// Searches the T or the H mode (`t_mode`) over the paint palette: every ordered pair of
/// distinct palette colours as `(C1, C2)` and every distance. In H mode the distance index's
/// lowest bit is the colour-order bit, so a pair only takes the distances whose index matches
/// its order (the swapped pair, also in the loop, takes the others). The unused bits are then
/// set to trip the mode selection, so every 4-bit colour pair is encodable.
fn search_t_or_h(texels: &[[i32; 3]; 16], w: &[u32; 3], t_mode: bool) -> Option<Cand> {
    let palette = paint_palette(texels);
    let mut best: Option<Cand> = None;
    for (ii, &c1) in palette.iter().enumerate() {
        for (jj, &c2) in palette.iter().enumerate() {
            if ii == jj {
                continue;
            }
            let p1 = c1.map(expand4);
            let p2 = c2.map(expand4);
            let order = h_order_bit(c1, c2);
            for (di, &dist) in DISTANCES.iter().enumerate() {
                let di = di as u64;
                if !t_mode && di & 1 != order {
                    continue;
                }
                let paints = if t_mode {
                    [p1, offset(p2, dist), p2, offset(p2, -dist)]
                } else {
                    [
                        offset(p1, dist),
                        offset(p1, -dist),
                        offset(p2, dist),
                        offset(p2, -dist),
                    ]
                };
                let (err, sels) = fit_paints(texels, &paints, w);
                if best.as_ref().is_some_and(|c| err >= c.err) {
                    continue;
                }
                let n = |v: u8| u64::from(v);
                let mut bits = if t_mode {
                    (n(c1[0] >> 2) << 59)
                        | (n(c1[0] & 3) << 56)
                        | (n(c1[1]) << 52)
                        | (n(c1[2]) << 48)
                        | (n(c2[0]) << 44)
                        | (n(c2[1]) << 40)
                        | (n(c2[2]) << 36)
                        | ((di >> 1) << 34)
                        | ((di & 1) << 32)
                } else {
                    (n(c1[0]) << 59)
                        | (n(c1[1] >> 1) << 56)
                        | (n(c1[1] & 1) << 52)
                        | (n(c1[2] >> 3) << 51)
                        | (n(c1[2] & 7) << 47)
                        | (n(c2[0]) << 43)
                        | (n(c2[1]) << 39)
                        | (n(c2[2]) << 35)
                        | ((di >> 2) << 34)
                        | (((di >> 1) & 1) << 32)
                };
                bits |= 1 << 33;
                for (p, &s) in sels.iter().enumerate() {
                    let s = u64::from(s);
                    bits |= ((s >> 1) & 1) << (16 + p);
                    bits |= (s & 1) << p;
                }
                let (free, mode) = if t_mode {
                    (T_FREE, RgbMode::T)
                } else {
                    (H_FREE, RgbMode::H)
                };
                if let Some(bits) = force_mode(bits, free, mode) {
                    best = Some(Cand { err, bits, mode });
                }
            }
        }
    }
    best
}

/// Determinant of a 3×3 matrix.
fn det3(m: [[f64; 3]; 3]) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1]) - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

/// Solves `m · x = rhs` by Cramer's rule (the planar normal matrix has determinant 6400, so the
/// division is always safe).
fn solve3(m: [[f64; 3]; 3], rhs: [f64; 3]) -> [f64; 3] {
    let d = det3(m);
    let mut x = [0f64; 3];
    for col in 0..3usize {
        let mut mm = m;
        for row in 0..3usize {
            mm[row][col] = rhs[row];
        }
        x[col] = det3(mm) / d;
    }
    x
}

/// Packs planar anchors (6/7/6-bit O, H, V) into a block word, the unused bits set so the block
/// decodes as planar (see [`planar_anchors`] for the fields).
fn planar_pack(anchors: [[u8; 3]; 3]) -> Option<u64> {
    let [o, h, v] = anchors.map(|a| a.map(u64::from));
    let bits = (o[0] << 57)
        | ((o[1] >> 6) << 56)
        | ((o[1] & 0x3f) << 49)
        | ((o[2] >> 5) << 48)
        | (((o[2] >> 3) & 3) << 43)
        | ((o[2] & 7) << 39)
        | ((h[0] >> 1) << 34)
        | (1 << 33)
        | ((h[0] & 1) << 32)
        | (h[1] << 25)
        | (h[2] << 19)
        | (v[0] << 13)
        | (v[1] << 6)
        | v[2];
    force_mode(bits, PLANAR_FREE, RgbMode::Planar)
}

/// The weighted squared error of a packed planar block against the texels, through the decode
/// path.
fn planar_error(bits: u64, texels: &[[i32; 3]; 16], w: &[u32; 3]) -> u64 {
    let [o, h, v] = planar_anchors(bits);
    let mut err = 0u64;
    for y in 0..4usize {
        for x in 0..4usize {
            let t = texels[y * 4 + x];
            for c in 0..3usize {
                let d = planar_channel(x as i32, y as i32, o[c], h[c], v[c]) - t[c];
                err += u64::from(w[c]) * (d * d) as u64;
            }
        }
    }
    err
}

/// One channel's quantized planar anchors (O, H, V): each rounded from its least-squares value
/// to the channel's 6 bits (7 for green), then the ±1 neighbourhood of all three tried together
/// (27 triples, the first of equal error kept), as rounding each anchor alone is not the best
/// triple once the plane is interpolated.
fn refine_planar_channel(texels: &[[i32; 3]; 16], c: usize, values: [i32; 3]) -> [u8; 3] {
    let (levels, expand): (i32, fn(u8) -> i32) = if c == 1 { (127, expand7) } else { (63, expand6) };
    let start = values.map(|v| quantize(v, levels));
    let mut best = (u64::MAX, start.map(|q| q.clamp(0, levels) as u8));
    for d_o in -1i32..=1 {
        for d_h in -1i32..=1 {
            for d_v in -1i32..=1 {
                let q = [start[0] + d_o, start[1] + d_h, start[2] + d_v];
                if q.iter().any(|&v| !(0..=levels).contains(&v)) {
                    continue;
                }
                let q = q.map(|v| v as u8);
                let [o, h, v] = q.map(expand);
                let mut err = 0u64;
                for y in 0..4i32 {
                    for x in 0..4i32 {
                        let d = planar_channel(x, y, o, h, v) - texels[(y * 4 + x) as usize][c];
                        err += (d * d) as u64;
                    }
                }
                if err < best.0 {
                    best = (err, q);
                }
            }
        }
    }
    best.1
}

/// Searches the planar mode: a per-channel least-squares plane `a·x + b·y + c` over the 16
/// texels, anchors `O = c`, `H = c + 4a`, `V = c + 4b` (the H and V anchors sit one texel step
/// beyond the block edge, which is what the reconstruction divides by), quantized to the mode's
/// 6/7-bit fields. The layout's unused bits make every such block decode as planar.
fn search_planar(texels: &[[i32; 3]; 16], w: &[u32; 3]) -> Option<Cand> {
    let m = [[56.0, 36.0, 24.0], [36.0, 56.0, 24.0], [24.0, 24.0, 16.0]];
    let mut o8 = [0i32; 3];
    let mut h8 = [0i32; 3];
    let mut v8 = [0i32; 3];
    for c in 0..3usize {
        let mut sxv = 0f64;
        let mut syv = 0f64;
        let mut sv = 0f64;
        for y in 0..4usize {
            for x in 0..4usize {
                let value = f64::from(texels[y * 4 + x][c]);
                sxv += x as f64 * value;
                syv += y as f64 * value;
                sv += value;
            }
        }
        let [a, b, cc] = solve3(m, [sxv, syv, sv]);
        o8[c] = cc.clamp(0.0, 255.0).round() as i32;
        h8[c] = (cc + 4.0 * a).clamp(0.0, 255.0).round() as i32;
        v8[c] = (cc + 4.0 * b).clamp(0.0, 255.0).round() as i32;
    }
    let mut anchors = [[0u8; 3]; 3];
    for c in 0..3usize {
        let [o, h, v] = refine_planar_channel(texels, c, [o8[c], h8[c], v8[c]]);
        anchors[0][c] = o;
        anchors[1][c] = h;
        anchors[2][c] = v;
    }
    let bits = planar_pack(anchors)?;
    Some(Cand {
        err: planar_error(bits, texels, w),
        bits,
        mode: RgbMode::Planar,
    })
}

/// Encodes one EAC block from 16 values by pixel number (`x * 4 + y`): 8-bit alpha, or 11-bit
/// targets with `eleven`. Every modifier table, the multipliers around the one the value range
/// implies (plus the fine-step multiplier 0 for 11-bit), and the bases around the one that
/// centres the table on the range, ±2.
fn encode_eac(values: &[i32; 16], eleven: bool) -> u64 {
    let (lo, hi) = values
        .iter()
        .fold((i32::MAX, i32::MIN), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    let unit = if eleven { 8.0 } else { 1.0 };
    let mut best = (u64::MAX, 0u64);
    for (ti, mods) in EAC_MODIFIERS.iter().enumerate() {
        let span = f64::from(mods[7] - mods[3]);
        let estimate = (f64::from(hi - lo) / unit / span).round() as i32;
        let mut multipliers: Vec<i32> = Vec::new();
        for m in estimate - 1..=estimate + 1 {
            let m = m.clamp(1, 15);
            if !multipliers.contains(&m) {
                multipliers.push(m);
            }
        }
        if eleven {
            multipliers.push(0);
        }
        for &multiplier in &multipliers {
            let scale = if eleven && multiplier == 0 {
                1.0
            } else {
                f64::from(multiplier) * unit
            };
            let mut centre = f64::from(lo + hi) / 2.0 - f64::from(mods[3] + mods[7]) * scale / 2.0;
            if eleven {
                centre = (centre - 4.0) / 8.0;
            }
            let centre_i = centre.round() as i32;
            for delta in -2i32..=2 {
                let base = (centre_i + delta).clamp(0, 255);
                let mut error = 0u64;
                let mut index_bits = 0u64;
                for (p, &target) in values.iter().enumerate() {
                    let mut pixel = (u64::MAX, 0usize);
                    for index in 0..8usize {
                        let d = eac_value(base, multiplier, ti, index, eleven) - target;
                        let e = (d * d) as u64;
                        if e < pixel.0 {
                            pixel = (e, index);
                        }
                    }
                    error += pixel.0;
                    index_bits |= (pixel.1 as u64) << (45 - 3 * p);
                }
                if error < best.0 {
                    best = (
                        error,
                        ((base as u64) << 56)
                            | ((multiplier as u64) << 52)
                            | ((ti as u64) << 48)
                            | index_bits,
                    );
                }
            }
        }
    }
    best.1
}

/// The mode a candidate block actually decodes through.
fn cand_mode(bits: u64) -> RgbMode {
    let bytes = bits.to_be_bytes();
    classify_block(&bytes)
}

/// Keeps the best candidate so far; the first mode wins ties.
fn consider(best: &mut Option<Cand>, cand: Option<Cand>) {
    if let Some(c) = cand
        && best.as_ref().is_none_or(|b| c.err < b.err)
    {
        *best = Some(c);
    }
}

/// Encodes one 4×4 block of RGBA texels (row order `y * 4 + x`; alpha ignored) to an ETC2 RGB8
/// block, searching every mode in `opts.modes` and keeping the block with the least weighted
/// squared error. The winner is decoded again and must land in the mode that produced it; if it
/// does not, the individual-mode block is used instead, so a block never decodes differently
/// than the search intended.
#[must_use]
pub fn encode_block_rgb(texels: &[[u8; 4]; 16], opts: &EncodeOptions) -> [u8; 8] {
    let t = texel_block(texels);
    let w = weights(opts);
    let modes = opts.modes;
    let mut best: Option<Cand> = None;
    if modes.individual {
        consider(&mut best, search_individual(&t, &w));
    }
    if modes.differential {
        consider(&mut best, search_differential(&t, &w));
    }
    if modes.t {
        consider(&mut best, search_t_or_h(&t, &w, true));
    }
    if modes.h {
        consider(&mut best, search_t_or_h(&t, &w, false));
    }
    if modes.planar {
        consider(&mut best, search_planar(&t, &w));
    }
    match best {
        Some(cand) if cand_mode(cand.bits) == cand.mode => cand.bits.to_be_bytes(),
        Some(_) => match search_individual(&t, &w) {
            // an ETC1 candidate always decodes through its own mode by construction
            Some(fallback) => fallback.bits.to_be_bytes(),
            None => FALLBACK_BITS.to_be_bytes(),
        },
        None => FALLBACK_BITS.to_be_bytes(),
    }
}

/// Encodes one 4×4 block of RGBA texels (row order `y * 4 + x`) to an ETC2 RGBA8 block: an EAC
/// 8-bit alpha block followed by an ETC2 RGB8 block.
#[must_use]
pub fn encode_block_rgba(texels: &[[u8; 4]; 16], opts: &EncodeOptions) -> [u8; 16] {
    let mut alpha = [0i32; 16];
    for y in 0..4usize {
        for x in 0..4usize {
            alpha[pixel_number(x, y)] = i32::from(texels[y * 4 + x][3]);
        }
    }
    let alpha_bits = encode_eac(&alpha, false);
    let rgb = encode_block_rgb(texels, opts);
    let mut out = [0u8; 16];
    out[..8].copy_from_slice(&alpha_bits.to_be_bytes());
    out[8..].copy_from_slice(&rgb);
    out
}

/// Encodes the R channel of one 4×4 block of RGBA texels (row order `y * 4 + x`) to an EAC R11
/// (unsigned 11-bit) block.
#[must_use]
pub fn encode_block_r11(texels: &[[u8; 4]; 16]) -> [u8; 8] {
    let mut values = [0i32; 16];
    for y in 0..4usize {
        for x in 0..4usize {
            values[pixel_number(x, y)] = to_eleven(texels[y * 4 + x][0]);
        }
    }
    encode_eac(&values, true).to_be_bytes()
}

/// The 16 RGBA texels of the block at block coordinates `(bx, by)`, edge texels repeated past
/// the image, in row order.
fn block_texels(width: u32, height: u32, rgba: &[u8], bx: u32, by: u32) -> [[u8; 4]; 16] {
    let mut out = [[0u8; 4]; 16];
    for y in 0..4u32 {
        for x in 0..4u32 {
            let px = (bx * 4 + x).min(width - 1);
            let py = (by * 4 + y).min(height - 1);
            // in usize: `py * width * 4` leaves u32 from 2^30 pixels on
            let i = (py as usize * width as usize + px as usize) * 4;
            out[(y * 4 + x) as usize] = [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]];
        }
    }
    out
}

/// Checks that `rgba` is a `width × height × 4` buffer with non-zero dimensions.
fn check_buffer(width: u32, height: u32, rgba: &[u8]) -> Result<(), Etc2EncodeError> {
    let needed = u128::from(width) * u128::from(height) * 4;
    if width == 0 || height == 0 || needed != rgba.len() as u128 {
        return Err(Etc2EncodeError::BadBuffer {
            len: rgba.len(),
            width,
            height,
        });
    }
    Ok(())
}

/// Encodes a `width × height × 4` RGBA8 image block by block. Any size is accepted: a partial
/// trailing block repeats the image's last row and column, as every block format requires whole
/// 4×4 blocks. Blocks run left to right, top to bottom; words are stored big-endian.
///
/// # Errors
/// The buffer does not match the size, or a dimension is zero.
pub fn encode_image(
    width: u32,
    height: u32,
    rgba: &[u8],
    format: Etc2Format,
    opts: &EncodeOptions,
) -> Result<Vec<u8>, Etc2EncodeError> {
    check_buffer(width, height, rgba)?;
    let _span = tracing::info_span!("etc2_encode", width, height).entered();
    let blocks_x = width.div_ceil(4) as usize;
    let blocks_y = height.div_ceil(4) as usize;
    let block_bytes = format.block_bytes();
    let mut out = vec![0u8; blocks_x * blocks_y * block_bytes];
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let texels = block_texels(width, height, rgba, bx as u32, by as u32);
            let at = (by * blocks_x + bx) * block_bytes;
            match format {
                Etc2Format::Rgb8 => {
                    out[at..at + 8].copy_from_slice(&encode_block_rgb(&texels, opts));
                }
                Etc2Format::Rgba8 => {
                    out[at..at + 16].copy_from_slice(&encode_block_rgba(&texels, opts));
                }
                Etc2Format::R11 => {
                    out[at..at + 8].copy_from_slice(&encode_block_r11(&texels));
                }
            }
        }
    }
    Ok(out)
}

/// Decodes an [`encode_image`] buffer of the given format back to a `width × height × 4` RGBA8
/// image. Channels a format does not store come back as the hardware returns them: alpha 255
/// for RGB8, `R 0 0 255` for R11.
///
/// # Errors
/// The data is not the number of blocks the size needs.
pub fn decode_image(
    width: u32,
    height: u32,
    data: &[u8],
    format: Etc2Format,
) -> Result<Vec<u8>, Etc2EncodeError> {
    let blocks_x = width.div_ceil(4) as usize;
    let blocks_y = height.div_ceil(4) as usize;
    let needed = blocks_x * blocks_y * format.block_bytes();
    if width == 0 || height == 0 || data.len() != needed {
        return Err(Etc2EncodeError::BadBlockData {
            len: data.len(),
            needed,
        });
    }
    let mut out = vec![0u8; width as usize * height as usize * 4];
    for by in 0..blocks_y {
        for bx in 0..blocks_x {
            let at = (by * blocks_x + bx) * format.block_bytes();
            let texels = match format {
                Etc2Format::Rgb8 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&data[at..at + 8]);
                    decode_block_rgb(&b)
                }
                Etc2Format::Rgba8 => {
                    let mut b = [0u8; 16];
                    b.copy_from_slice(&data[at..at + 16]);
                    decode_block_rgba(&b)
                }
                Etc2Format::R11 => {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&data[at..at + 8]);
                    let mut t = [[0u8, 0, 0, 255]; 16];
                    for (o, r) in t.iter_mut().zip(decode_block_r11(&b)) {
                        o[0] = r;
                    }
                    t
                }
            };
            for (i, texel) in texels.iter().enumerate() {
                let (x, y) = (bx * 4 + i % 4, by * 4 + i / 4);
                if x < width as usize && y < height as usize {
                    let o = (y * width as usize + x) * 4;
                    out[o..o + 4].copy_from_slice(texel);
                }
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
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
            f64::INFINITY
        } else {
            10.0 * (255.0f64 * 255.0 / (sum / n)).log10()
        }
    }

    /// A deterministic xorshift generator, explicit seed, bytes only.
    struct Rng(u64);

    impl Rng {
        fn draw(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }

        fn byte(&mut self) -> u8 {
            (self.draw() >> 33) as u8
        }
    }

    /// A 4×4 block filled with one colour.
    fn solid(c: [u8; 4]) -> [[u8; 4]; 16] {
        [c; 16]
    }

    const OPTS: &EncodeOptions = &EncodeOptions {
        perceptual: false,
        modes: ModeSet::ALL,
    };

    const OPTS_ETC1: &EncodeOptions = &EncodeOptions {
        perceptual: false,
        modes: ModeSet::ETC1,
    };

    /// A smooth gradient with a soft disc, like the cook's test image.
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
    fn solid_rgb_blocks_decode_exactly() {
        // grey 0x80: individual base 0x88 with the -8 modifier lands on 0x80 exactly, so the
        // round trip is lossless; 0 and 255 go through the clamp
        for c in [
            [0x80u8, 0x80, 0x80, 255],
            [0u8, 0, 0, 255],
            [255u8, 255, 255, 255],
        ] {
            let block = encode_block_rgb(&solid(c), OPTS);
            let decoded = decode_block_rgb(&block);
            for px in decoded {
                for ch in 0..3usize {
                    assert_eq!(px[ch], c[ch], "colour {c:?} channel {ch}");
                }
            }
        }
        let block = encode_block_rgb(&solid([0x80, 0x80, 0x80, 255]), OPTS);
        assert_eq!(classify_block(&block), RgbMode::Individual);
    }

    #[test]
    fn solid_rgba_block_decodes_alpha_exactly() {
        // EAC table 13 has a 0 modifier (index 4), so any solid alpha is exact
        for a in [0u8, 1, 0x37, 128, 200, 255] {
            let block = encode_block_rgba(&solid([10, 200, 30, a]), OPTS);
            let decoded = decode_block_rgba(&block);
            for px in decoded {
                assert_eq!(px[3], a);
            }
        }
    }

    #[test]
    fn solid_r11_block_decodes_exactly() {
        let block = encode_block_r11(&solid([0x88, 0x44, 0, 255]));
        let decoded = decode_block_r11(&block);
        for v in decoded {
            assert_eq!(v, 0x88);
        }
    }

    #[test]
    fn gradient_image_rgb_reaches_its_psnr_floor() -> Result<(), Etc2EncodeError> {
        // the existing ETC1-only encoder measures > 30 dB on this image; the wider base search
        // and the three extra modes buy at least 2 dB more
        let (w, h) = (32u32, 32u32);
        let rgba = gradient(w, h);
        let data = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
        let decoded = decode_image(w, h, &data, Etc2Format::Rgb8)?;
        let db = psnr(&rgba, &decoded, 3);
        assert!(db > 31.0, "ETC2 RGB8 {db:.1} dB");
        Ok(())
    }

    #[test]
    fn gradient_image_rgba_alpha_reaches_its_psnr_floor() -> Result<(), Etc2EncodeError> {
        // the alpha channel is a smooth ramp (range 255 over 32 pixels, 8 per block-row); the
        // EAC tables track it far below this floor
        let (w, h) = (32u32, 32u32);
        let rgba = gradient(w, h);
        let data = encode_image(w, h, &rgba, Etc2Format::Rgba8, OPTS)?;
        let decoded = decode_image(w, h, &data, Etc2Format::Rgba8)?;
        let alpha: Vec<u8> = rgba
            .iter()
            .enumerate()
            .map(|(i, v)| if i % 4 == 3 { *v } else { 0 })
            .collect();
        let alpha_d: Vec<u8> = decoded
            .iter()
            .enumerate()
            .map(|(i, v)| if i % 4 == 3 { *v } else { 0 })
            .collect();
        let db = psnr(&alpha, &alpha_d, 4);
        assert!(db > 38.0, "EAC alpha {db:.1} dB");
        let db = psnr(&rgba, &decoded, 3);
        assert!(db > 31.0, "ETC2 RGB part {db:.1} dB");
        Ok(())
    }

    #[test]
    fn gradient_image_r11_reaches_its_psnr_floor() -> Result<(), Etc2EncodeError> {
        // 11-bit precision over a smooth 8-bit ramp: quantization is invisible at this floor
        let (w, h) = (32u32, 32u32);
        let rgba = gradient(w, h);
        let data = encode_image(w, h, &rgba, Etc2Format::R11, OPTS_ETC1)?;
        let decoded = decode_image(w, h, &data, Etc2Format::R11)?;
        let db = psnr(&rgba, &decoded, 1);
        assert!(db > 40.0, "EAC R11 {db:.1} dB");
        Ok(())
    }

    #[test]
    fn noise_image_stays_above_its_psnr_floors() -> Result<(), Etc2EncodeError> {
        // pure uniform noise is the worst case: no mode beats the per-sub-block variance by
        // much, which is about 10 dB; noise of +-24 around a gradient is the realistic case
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (w, h) = (16u32, 16u32);
        let mut pure = Vec::new();
        for _ in 0..w * h {
            pure.extend_from_slice(&[rng.byte(), rng.byte(), rng.byte(), 255]);
        }
        let base = gradient(w, h);
        let mut bounded = Vec::new();
        for p in base.as_chunks::<4>().0 {
            bounded.extend_from_slice(&[
                p[0].saturating_add(rng.byte() % 48).saturating_sub(24),
                p[1].saturating_add(rng.byte() % 48).saturating_sub(24),
                p[2],
                255,
            ]);
        }
        let data = encode_image(w, h, &pure, Etc2Format::Rgb8, OPTS)?;
        let db = psnr(&pure, &decode_image(w, h, &data, Etc2Format::Rgb8)?, 3);
        assert!(db > 10.0, "pure noise {db:.1} dB");
        let data = encode_image(w, h, &bounded, Etc2Format::Rgb8, OPTS)?;
        let db = psnr(&bounded, &decode_image(w, h, &data, Etc2Format::Rgb8)?, 3);
        assert!(db > 23.0, "bounded noise {db:.1} dB");
        Ok(())
    }

    /// Two solid halves of exactly representable far-apart colours: the individual mode is
    /// lossless here (the quantized-mean ±1 cube contains a base whose modifiers hit both
    /// colours exactly: 51-17 = 34, 170-17 = 153, 238-17 = 221, 34+17 = 51); differential and T
    /// (C1 and C2 as two of its paints) can at best tie it, so the tie-break (individual first)
    /// picks individual.
    fn individual_block() -> [[u8; 4]; 16] {
        let a = [expand4(2) as u8, expand4(9) as u8, expand4(2) as u8, 255];
        let b = [expand4(13) as u8, expand4(3) as u8, expand4(13) as u8, 255];
        let mut t = [[0u8; 4]; 16];
        for y in 0..4usize {
            for x in 0..4usize {
                t[y * 4 + x] = if x < 2 { a } else { b };
            }
        }
        t
    }

    #[test]
    fn individual_mode_is_selected_for_split_solid_block() {
        let texels = individual_block();
        let block = encode_block_rgb(&texels, OPTS);
        assert_eq!(classify_block(&block), RgbMode::Individual);
        let decoded = decode_block_rgb(&block);
        for px in decoded {
            assert!(px[0] == 34 || px[0] == 221, "px {px:?}");
        }
    }

    /// Grey halves of four levels each, `165 ± 2, ± 8` left and `181 ± 2, ± 8` right (table 0
    /// around the 5-bit bases 20 and 22, delta +2). The individual mode would need the 4-bit
    /// base 165 or 181 (the centre of a symmetric four-level set), which is no multiple of 17;
    /// eight distinct values defeat the four T/H paints, and the pattern is not a plane.
    fn differential_block() -> [[u8; 4]; 16] {
        let rows = [
            [157u8, 173, 173, 189],
            [163, 167, 179, 183],
            [167, 163, 183, 179],
            [173, 157, 189, 173],
        ];
        let mut t = [[0u8; 4]; 16];
        for (y, row) in rows.iter().enumerate() {
            for (x, &v) in row.iter().enumerate() {
                t[y * 4 + x] = [v, v, v, 255];
            }
        }
        t
    }

    #[test]
    fn differential_mode_is_selected_for_close_halves() {
        // the delivered block (solid 163 | solid 183) is exact in the individual mode too
        // (187 - 24 and 170 + 13), so the tie went to individual: see `differential_block`
        let texels = differential_block();
        let block = encode_block_rgb(&texels, OPTS);
        assert_eq!(classify_block(&block), RgbMode::Differential);
        // the constraint the mode lives or dies on: base + delta in 0..=31 for all channels
        let bits = u64::from_be_bytes(block);
        for c in 0..3usize {
            let shift = 59 - 8 * c;
            let base = ((bits >> shift) & 0x1f) as i32;
            let delta = sext3((bits >> (shift - 3)) & 7);
            assert!((0..=31).contains(&(base + delta)), "channel {c}: {base}+{delta}");
        }
        assert_eq!(decode_block_rgb(&block), texels, "differential is exact here");
    }

    #[test]
    fn t_mode_is_selected_for_three_colour_block() {
        // T paints C1 = (68,0,68), C2 = (68,85,68) and C2+3 = (71,88,71) (distance index 0) all
        // exactly, while one ETC1 sub-block base cannot cover two greens 3 or 85 apart (no
        // intensity table has two modifiers at those distances), and H needs C1 - d to clamp
        // green to 0 with 68 + d a multiple of 17, which no distance gives
        let a = [68u8, 0, 68, 255];
        let b = [68u8, 85, 68, 255];
        let b3 = [71u8, 88, 71, 255];
        let pattern = [a, b, b3, a, b3, a, b, b3, b, b3, a, b, a, b3, b, a];
        let block = encode_block_rgb(&pattern, OPTS);
        assert_eq!(classify_block(&block), RgbMode::T);
        let decoded = decode_block_rgb(&block);
        let worst = decoded
            .iter()
            .zip(pattern.iter())
            .map(|(d, s)| (0..3).map(|c| d[c].abs_diff(s[c])).max().unwrap_or(0))
            .max()
            .unwrap_or(0);
        assert_eq!(worst, 0, "T mode should be exact here");
    }

    /// H paints with C1 = (68,68,51), C2 = (68,68,204) and d = 23: `C1 ± d` = (91,91,74) and
    /// (45,45,28), `C2 ± d` = (91,91,227) and (45,45,181), each sub-block holding all four. The
    /// ETC1 modes add one grey offset to a base, which cannot map R 45 to both B 28 and B 181; T
    /// holds one `C2 ± d` pair and C1 but not the fourth colour; planar is not a plane here.
    fn h_block() -> [[u8; 4]; 16] {
        let p = [
            [91u8, 91, 74, 255],
            [45, 45, 28, 255],
            [91, 91, 227, 255],
            [45, 45, 181, 255],
        ];
        let order = [[0usize, 1, 2, 3], [2, 3, 0, 1], [1, 0, 3, 2], [3, 2, 1, 0]];
        let mut t = [[0u8; 4]; 16];
        for (y, row) in order.iter().enumerate() {
            for (x, &i) in row.iter().enumerate() {
                t[y * 4 + x] = p[i];
            }
        }
        t
    }

    #[test]
    fn h_mode_is_selected_for_symmetric_block() {
        // the delivered block was built for the delivered (non-standard) H paints
        let pattern = h_block();
        let block = encode_block_rgb(&pattern, OPTS);
        assert_eq!(classify_block(&block), RgbMode::H);
        assert_eq!(decode_block_rgb(&block), pattern, "H mode should be exact here");
    }

    #[test]
    fn planar_mode_is_selected_for_smooth_ramp() {
        // a steep smooth plane no sub-block split can follow: five distinct values per half at
        // steps of 13 while the intensity modifiers offer four levels around a base; planar's
        // anchors hit it within quantization
        let mut texels = [[0u8; 4]; 16];
        for y in 0..4i32 {
            for x in 0..4i32 {
                let r = (96 + 13 * x + 13 * y).clamp(0, 255) as u8;
                let g = (156 - 13 * x + 5 * y).clamp(0, 255) as u8;
                let b = (60 + 5 * x + 9 * y).clamp(0, 255) as u8;
                texels[(y * 4 + x) as usize] = [r, g, b, 255];
            }
        }
        let block = encode_block_rgb(&texels, OPTS);
        assert_eq!(classify_block(&block), RgbMode::Planar);
        let err = |d: &[[u8; 4]; 16]| -> u64 {
            texels
                .iter()
                .zip(d.iter())
                .map(|(s, g)| (0..3).map(|c| u64::from(s[c].abs_diff(g[c])).pow(2)).sum::<u64>())
                .sum()
        };
        let all = decode_block_rgb(&block);
        let etc1 = decode_block_rgb(&encode_block_rgb(&texels, OPTS_ETC1));
        assert!(
            err(&all) < err(&etc1),
            "planar {} vs etc1 {}",
            err(&all),
            err(&etc1)
        );
    }

    #[test]
    fn differential_constraint_holds_over_whole_images() -> Result<(), Etc2EncodeError> {
        // the hard invariant: every differential block's base + delta stays in 0..=31 (it is
        // what keeps the block from decoding as T, H or planar instead)
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let (w, h) = (16u32, 16u32);
        let mut mixed = gradient(w, h);
        for b in mixed.iter_mut() {
            *b = b.wrapping_add(rng.byte() % 24).wrapping_sub(12);
        }
        for rgba in [gradient(w, h), mixed] {
            let data = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
            for block in data.as_chunks::<8>().0 {
                if classify_block(block) == RgbMode::Differential {
                    let bits = u64::from_be_bytes(*block);
                    for c in 0..3usize {
                        let shift = 59 - 8 * c;
                        let base = ((bits >> shift) & 0x1f) as i32;
                        let delta = sext3((bits >> (shift - 3)) & 7);
                        assert!((0..=31).contains(&(base + delta)));
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn mode_census_of_a_gradient_counts_every_block() -> Result<(), Etc2EncodeError> {
        let (w, h) = (32u32, 32u32);
        let rgba = gradient(w, h);
        let data = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
        let mut counts = [0usize; 5];
        for block in data.as_chunks::<8>().0 {
            counts[match classify_block(block) {
                RgbMode::Individual => 0,
                RgbMode::Differential => 1,
                RgbMode::T => 2,
                RgbMode::H => 3,
                RgbMode::Planar => 4,
            }] += 1;
        }
        assert_eq!(counts.iter().sum::<usize>(), 64);
        // a smooth gradient is planar's case: the delivered test asserted ETC1 blocks, which
        // only held while its planar mode was unreadable by real decoders
        assert!(counts[4] > 0, "planar: {counts:?}");
        let etc1 = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS_ETC1)?;
        for block in etc1.as_chunks::<8>().0 {
            let mode = classify_block(block);
            assert!(
                matches!(mode, RgbMode::Individual | RgbMode::Differential),
                "{mode:?}"
            );
        }
        Ok(())
    }

    #[test]
    fn eac_alpha_ramp_block_holds_its_floor() {
        // one block over a 90-level alpha ramp (60..=150, six levels a texel): the EAC tables
        // put eight non-uniform levels on it, so the worst texel error is about half the
        // widest modifier gap, and the search centres the best table on the ramp
        let mut texels = [[0u8; 4]; 16];
        for y in 0..4usize {
            for x in 0..4usize {
                let v = (60 + 6 * (y * 4 + x)).clamp(0, 255) as u8;
                texels[y * 4 + x] = [30, 90, 150, v];
            }
        }
        let block = encode_block_rgba(&texels, OPTS);
        let decoded = decode_block_rgba(&block);
        let mut worst = 0u8;
        let mut sum = 0f64;
        for (d, s) in decoded.iter().zip(texels.iter()) {
            worst = worst.max(d[3].abs_diff(s[3]));
            let e = f64::from(d[3].abs_diff(s[3]));
            sum += e * e;
        }
        assert!(worst <= 12, "alpha ramp worst error {worst}");
        let db = 10.0 * (255.0f64 * 255.0 / (sum / 16.0)).log10();
        assert!(db > 30.0, "alpha ramp {db:.1} dB");
    }

    #[test]
    fn partial_blocks_pad_the_edge_and_round_trip() -> Result<(), Etc2EncodeError> {
        for (w, h) in [(1u32, 1u32), (2, 2), (3, 5), (7, 3), (10, 6)] {
            let mut rgba = vec![9u8; (w * h * 4) as usize];
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    rgba[i] = (x * 40) as u8;
                    rgba[i + 1] = (y * 40) as u8;
                    rgba[i + 2] = 90;
                    rgba[i + 3] = 200;
                }
            }
            let blocks_x = w.div_ceil(4) as usize;
            let blocks_y = h.div_ceil(4) as usize;
            let rgb = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
            assert_eq!(rgb.len(), blocks_x * blocks_y * 8);
            let rgba_data = encode_image(w, h, &rgba, Etc2Format::Rgba8, OPTS)?;
            assert_eq!(rgba_data.len(), blocks_x * blocks_y * 16);
            let r11 = encode_image(w, h, &rgba, Etc2Format::R11, OPTS_ETC1)?;
            assert_eq!(r11.len(), blocks_x * blocks_y * 8);
            // the padded texels repeat real pixels, and solid alpha 200 is exact
            let decoded = decode_image(w, h, &rgba_data, Etc2Format::Rgba8)?;
            for px in decoded.as_chunks::<4>().0 {
                assert_eq!(px[3], 200);
            }
        }
        Ok(())
    }

    #[test]
    fn bad_buffers_and_bad_block_data_are_rejected() {
        assert!(encode_image(0, 4, &[], Etc2Format::Rgb8, OPTS).is_err());
        assert!(encode_image(4, 0, &[], Etc2Format::Rgb8, OPTS).is_err());
        assert!(encode_image(4, 4, &[0u8; 12], Etc2Format::Rgb8, OPTS).is_err());
        assert!(encode_image(4, 4, &[0u8; 65], Etc2Format::Rgb8, OPTS).is_err());
        // one 4x4 RGB8 block is exactly 8 bytes (the delivered test expected an error here)
        assert!(decode_image(4, 4, &[0u8; 8], Etc2Format::Rgb8).is_ok());
        assert!(decode_image(4, 4, &[0u8; 7], Etc2Format::Rgb8).is_err());
        assert!(decode_image(4, 4, &[0u8; 8], Etc2Format::Rgba8).is_err());
        assert!(decode_image(0, 4, &[], Etc2Format::Rgb8).is_err());
        let err = encode_image(4, 4, &[0u8; 12], Etc2Format::Rgb8, OPTS).err();
        assert_eq!(
            err,
            Some(Etc2EncodeError::BadBuffer {
                len: 12,
                width: 4,
                height: 4
            })
        );
        assert!(err.is_some_and(|e| e.to_string().contains("12")));
    }

    #[test]
    fn encoding_is_deterministic() -> Result<(), Etc2EncodeError> {
        let (w, h) = (16u32, 16u32);
        let rgba = gradient(w, h);
        let a = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
        let b = encode_image(w, h, &rgba, Etc2Format::Rgb8, OPTS)?;
        assert_eq!(a, b);
        let texels = block_texels(w, h, &rgba, 1, 2);
        assert_eq!(encode_block_rgb(&texels, OPTS), encode_block_rgb(&texels, OPTS));
        assert_eq!(encode_block_rgba(&texels, OPTS), encode_block_rgba(&texels, OPTS));
        let same_opts = EncodeOptions {
            perceptual: false,
            modes: ModeSet::ALL,
        };
        assert_eq!(
            encode_block_rgb(&texels, &same_opts),
            encode_block_rgb(&texels, OPTS)
        );
        Ok(())
    }

    #[test]
    fn search_work_per_block_is_bounded() {
        // replaces the delivered wall-clock guard (no clock budgets in tests): counts candidate
        // evaluations instead. Bound: individual 2 orientations x 2 halves x 27 bases = 108;
        // differential at most 2 x (27 + 27 x 27) memoized fits = 1512; T and H at most 9 x 8
        // ordered palette pairs x 8 distances = 576 each. An accidentally exhaustive search
        // (for example all 4096 colour pairs) breaks it at once.
        let bound = 108 + 1512 + 2 * 576;
        let mut rng = Rng(0x0bad_5eed_1234_5678);
        let rgba = gradient(16, 16);
        let mut blocks: Vec<[[u8; 4]; 16]> = (0..16u32)
            .map(|i| block_texels(16, 16, &rgba, i % 4, i / 4))
            .collect();
        for _ in 0..16 {
            let mut t = [[0u8; 4]; 16];
            for px in &mut t {
                *px = [rng.byte(), rng.byte(), rng.byte(), 255];
            }
            blocks.push(t);
        }
        blocks.extend([
            individual_block(),
            differential_block(),
            h_block(),
            solid([0, 0, 0, 0]),
        ]);
        for t in &blocks {
            EVALS.with(|c| c.set(0));
            let _ = encode_block_rgb(t, OPTS);
            let evals = EVALS.with(std::cell::Cell::get);
            assert!(evals > 0 && evals <= bound, "{evals} evaluations (bound {bound})");
        }
    }

    #[test]
    fn r11_decode_matches_hand_computed_block() {
        // the hand vector the GPU-validated etc2_compress decoder was checked against: base
        // 128, multiplier 2, table 13 ([-1,-2,-3,-10,0,1,2,9]): alpha 128 and 146 for indices
        // 4 and 7; R11 128*8+4 = 1028 and 1028 + 9*2*8 = 1172; multiplier 0 steps by the bare
        // modifier: 1028 + 9
        let bits = (128u64 << 56) | (2 << 52) | (13 << 48) | (4 << 45) | (7 << 42);
        let alpha = decode_eac_alpha(bits);
        assert_eq!((alpha[0], alpha[1]), (128, 146));
        let eleven = decode_eac_eleven(bits);
        assert_eq!((eleven[0], eleven[1]), (1028, 1172));
        let fine = (128u64 << 56) | (13 << 48) | (7 << 45);
        assert_eq!(decode_eac_eleven(fine)[0], 1037);
    }

    #[test]
    fn etc1_decoder_matches_hand_computed_block() {
        // individual mode, no flip: left half 0x88 grey with table 0, right half 0xcc grey with
        // table 7. Pixel (0,0) index 00 (+2) = 0x8a, (1,0) index 01 (+8) = 0x90, (2,0) index
        // 10 (-47) = 0x99 = 0xcc - 47, (3,0) index 11 (-183) = 0xcc - 183, pixel (2,1) index
        // 00 (+47) = 0xcc + 47
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
        let texels = decode_block_rgb(&bits.to_be_bytes());
        assert_eq!(texels[0], [0x8a, 0x8a, 0x8a, 255]);
        assert_eq!(texels[1], [0x90, 0x90, 0x90, 255]);
        assert_eq!(texels[2], [0xcc - 47, 0xcc - 47, 0xcc - 47, 255]);
        assert_eq!(texels[3], [0xcc - 183, 0xcc - 183, 0xcc - 183, 255]);
        assert_eq!(texels[6], [0xcc + 47, 0xcc + 47, 0xcc + 47, 255]);
    }

    #[test]
    fn exhaustive_search_matches_or_beats_the_existing_encoder() -> Result<(), Etc2EncodeError> {
        // the full search only adds modes to this module's ETC1 search, so it never loses to it;
        // the existing `crate::etc2` encoder now also searches T, H and planar (integ/v034), so
        // the full search is compared with it (it was compared with the ETC1 search while the
        // existing encoder was ETC1-only)
        let (w, h) = (16u32, 16u32);
        let mut rng = Rng(0x853c_49e6_748f_ea9b);
        let mut images = [gradient(w, h), vec![0u8; (w * h * 4) as usize]];
        for p in images[1].as_chunks_mut::<4>().0 {
            p[0] = rng.byte();
            p[1] = rng.byte();
            p[2] = rng.byte();
            p[3] = 255;
        }
        for rgba in &images {
            let mine_all = encode_image(w, h, rgba, Etc2Format::Rgb8, OPTS)?;
            let mine_dec = decode_image(w, h, &mine_all, Etc2Format::Rgb8)?;
            let mine_etc1 = encode_image(w, h, rgba, Etc2Format::Rgb8, OPTS_ETC1)?;
            let etc1_dec = decode_image(w, h, &mine_etc1, Etc2Format::Rgb8)?;
            let existing =
                crate::etc2::compress_etc2_rgb8(w, h, rgba).map_err(|_| Etc2EncodeError::BadBuffer {
                    len: rgba.len(),
                    width: w,
                    height: h,
                })?;
            let existing_dec = crate::etc2::decode_etc2_rgba8(crate::etc2::Etc2Kind::Rgb8, w, h, &existing)
                .map_err(|_| Etc2EncodeError::BadBlockData {
                len: existing.len(),
                needed: existing.len(),
            })?;
            let e_all = psnr(rgba, &mine_dec, 3);
            let e_etc1 = psnr(rgba, &etc1_dec, 3);
            let e_existing = psnr(rgba, &existing_dec, 3);
            assert!(e_all >= e_etc1 - 1e-9, "{e_all} vs {e_etc1}");
            assert!(e_all >= e_existing - 1e-9, "{e_all} vs {e_existing}");
        }
        Ok(())
    }

    #[test]
    fn perceptual_weighting_changes_output_deterministically() {
        let texels = individual_block();
        let uniform = encode_block_rgb(&texels, OPTS);
        let perceptual_opts = EncodeOptions {
            perceptual: true,
            modes: ModeSet::ALL,
        };
        let perceptual = encode_block_rgb(&texels, &perceptual_opts);
        let _ = decode_block_rgb(&uniform);
        let _ = decode_block_rgb(&perceptual);
        assert_eq!(perceptual, encode_block_rgb(&texels, &perceptual_opts));
    }

    #[test]
    fn adversarial_blocks_never_panic() {
        let mut rng = Rng(0xd1b5_4a32_d192_ed03);
        let mut blocks: Vec<[[u8; 4]; 16]> = vec![
            solid([0, 0, 0, 0]),
            solid([255, 255, 255, 255]),
            solid([0, 255, 0, 128]),
        ];
        for i in 0..16usize {
            let mut t = [[0u8; 4]; 16];
            for px in &mut t {
                let extreme = [0u8, 255];
                *px = [
                    extreme[(i + (rng.draw() % 2) as usize) % 2],
                    extreme[(i / 2) % 2],
                    extreme[i % 2],
                    255,
                ];
            }
            blocks.push(t);
        }
        for t in &blocks {
            let rgb = encode_block_rgb(t, OPTS);
            let _ = classify_block(&rgb);
            let _ = decode_block_rgb(&rgb);
            let rgba = encode_block_rgba(t, OPTS);
            let _ = decode_block_rgba(&rgba);
            let r11 = encode_block_r11(t);
            let _ = decode_block_r11(&r11);
        }
        // degenerate option set: no modes at all still returns a valid block
        let none = EncodeOptions {
            perceptual: false,
            modes: ModeSet {
                individual: false,
                differential: false,
                t: false,
                h: false,
                planar: false,
            },
        };
        assert!(!none.modes.any());
        let block = encode_block_rgb(&blocks[0], &none);
        let _ = decode_block_rgb(&block);
    }

    #[test]
    fn every_mode_block_round_trips_through_the_decoder() {
        // the synthetic blocks of the individual, differential and H tests decode back to their
        // texels (the encoder's round-trip guard pins the mode for the others)
        for texels in [individual_block(), differential_block(), h_block()] {
            let block = encode_block_rgb(&texels, OPTS);
            let decoded = decode_block_rgb(&block);
            let worst = decoded
                .iter()
                .zip(texels.iter())
                .map(|(d, s)| (0..3).map(|c| d[c].abs_diff(s[c])).max().unwrap_or(0))
                .max()
                .unwrap_or(0);
            assert!(worst <= 1, "worst {worst}");
        }
    }

    /// Sets the 2-bit index of pixel number `p` in a block word.
    fn set_index(bits: &mut u64, p: usize, index: u64) {
        *bits |= ((index >> 1) & 1) << (16 + p);
        *bits |= (index & 1) << p;
    }

    #[test]
    fn t_and_h_packing_decode_through_their_paint_rules() {
        // T, hand-packed from the specification's layout: C1 = (4,0,4), C2 = (4,5,4), distance
        // index 0 (d = 3); paints C1, C2 + d, C2, C2 - d
        let t = (1u64 << 59) // R1 = 0b0100: high pair 01 at 60..59
            | (4 << 48) // B1 (R1's low pair 00 at 57..56, G1 = 0 at 55..52)
            | (4 << 44) // R2
            | (5 << 40) // G2
            | (4 << 36) // B2
            | (1 << 33);
        let mut bits = force_mode(t, T_FREE, RgbMode::T).unwrap_or(0);
        assert_eq!(bits & !T_FREE, t, "only the unused bits change");
        for (p, index) in [(0usize, 0u64), (1, 1), (2, 2), (3, 3)] {
            set_index(&mut bits, p, index);
        }
        let bytes = bits.to_be_bytes();
        assert_eq!(classify_block(&bytes), RgbMode::T);
        let texels = decode_block_rgb(&bytes);
        // pixel numbers 0..=3 are column x = 0, rows 0..=3
        assert_eq!(texels[0], [68, 0, 68, 255], "index 0 is C1");
        assert_eq!(texels[4], [71, 88, 71, 255], "index 1 is C2 + d");
        assert_eq!(texels[8], [68, 85, 68, 255], "index 2 is C2");
        assert_eq!(texels[12], [65, 82, 65, 255], "index 3 is C2 - d");

        // H: C1 = (4,4,3), C2 = (4,4,12), da = 1, db = 0. C1 < C2 as 12-bit numbers, so the
        // order bit is 0: distance index 0b100 = 4, d = 23; paints C1 + d, C1 - d, C2 + d, C2 - d
        let h = (4u64 << 59) // R1
            | (2 << 56) // G1 = 0b0100: high three bits 010
            | (3 << 47) // B1 = 0b0011: bit 51 = 0, low three bits 011 (G1's low bit 0 at 52)
            | (4 << 43) // R2
            | (4 << 39) // G2
            | (12 << 35) // B2
            | (1 << 34) // da
            | (1 << 33);
        let mut hbits = force_mode(h, H_FREE, RgbMode::H).unwrap_or(0);
        assert_eq!(hbits & !H_FREE, h, "only the unused bits change");
        for (p, index) in [(0usize, 0u64), (1, 1), (2, 2), (3, 3)] {
            set_index(&mut hbits, p, index);
        }
        let hbytes = hbits.to_be_bytes();
        assert_eq!(classify_block(&hbytes), RgbMode::H);
        let ht = decode_block_rgb(&hbytes);
        assert_eq!(ht[0], [91, 91, 74, 255]);
        assert_eq!(ht[4], [45, 45, 28, 255]);
        assert_eq!(ht[8], [91, 91, 227, 255]);
        assert_eq!(ht[12], [45, 45, 181, 255]);

        // the same block with C1 and C2 swapped: C1 >= C2 sets the order bit, distance index 5,
        // d = 32
        let swapped = (4u64 << 59)
            | (2 << 56)
            | (1 << 51)
            | (4 << 47)
            | (4 << 43)
            | (4 << 39)
            | (3 << 35)
            | (1 << 34)
            | (1 << 33);
        let sbits = force_mode(swapped, H_FREE, RgbMode::H).unwrap_or(0);
        let st = decode_block_rgb(&sbits.to_be_bytes());
        assert_eq!(st[0], [100, 100, 236, 255], "C1 + 32 with C1 = (68,68,204)");
    }

    #[test]
    fn decoder_matches_independent_decoder_vectors() {
        // random T, H and planar words and the texels (row order, RGB hex) an independent
        // MIT/Apache decoder (texture2ddecoder 0.1.2, scratch crate outside the repository,
        // ADR-2333) returns for them
        let vectors: [(u64, &str); 6] = [
            (
                0x7414_718e_8286_34b4,
                "ffb920ce7900ffb920ce7900ff5331ce7900ff5331ce7900ce1300ffb920ce7900ffb920ffb920ce1300ffb920ff5331",
            ),
            (
                0xfa93_912a_4ddc_e4c8,
                "ee9933991122991122ee9933ee9933ee9933ee9933b0283999112282000b82000b82000b82000b82000b991122b02839",
            ),
            (
                0xd0d8_042e_c5d0_c9f5,
                "a2580090733b7e8f766baab08056366d71715b8cab49a7e65d536c4b6ea6398ae126a5ff3b51a1286cdc1687ff04a2ff",
            ),
            (
                0xf321_c3a7_e790_9467,
                "dc43bacc33aacc33aadc43badc43badc43bacc33aacc33aadc43badc43babc239acc33aabb2211cc33aabb2211bc239a",
            ),
            (
                0x2628_052b_d01c_5929,
                "4d28084f5209517d0a53a70b5c50305e7b3160a53262cf336c79576ea35870cd5972f75a7ba17f7dcb807ff58181ff82",
            ),
            (
                0x88eb_e66f_60a4_fdd5,
                "0000d60000d60000d60000d63a29fff5f5ff3a29ffa3a3b4a3a3b40000d60000d6a3a3b43a29ffa3a3b40000d60000d6",
            ),
        ];
        let modes = [
            RgbMode::H,
            RgbMode::T,
            RgbMode::Planar,
            RgbMode::T,
            RgbMode::Planar,
            RgbMode::H,
        ];
        for ((word, hex), mode) in vectors.iter().zip(modes) {
            let block = word.to_be_bytes();
            assert_eq!(classify_block(&block), mode, "{word:#x}");
            let got: String = decode_block_rgb(&block)
                .iter()
                .map(|p| format!("{:02x}{:02x}{:02x}", p[0], p[1], p[2]))
                .collect();
            assert_eq!(&got, hex, "{word:#x}");
        }
    }

    #[test]
    fn unused_bits_always_reach_the_wanted_mode() {
        // every colour-field content of every ETC2 mode is encodable: setting the unused bits
        // reaches the mode and touches nothing else
        let mut rng = Rng(0x7e57_ab1e_0000_0003);
        for _ in 0..4000 {
            let word = rng.draw() | (1 << 33);
            for (free, mode) in [
                (T_FREE, RgbMode::T),
                (H_FREE, RgbMode::H),
                (PLANAR_FREE, RgbMode::Planar),
            ] {
                let forced = force_mode(word, free, mode);
                assert!(forced.is_some(), "{word:#x} {mode:?}");
                let forced = forced.unwrap_or(0);
                assert_eq!(classify_block(&forced.to_be_bytes()), mode);
                assert_eq!(forced & !free, word & !free);
            }
        }
    }

    #[test]
    fn planar_packing_matches_the_reconstruction_formula() {
        // anchors O = (24,30,40), H = (89,78,73), V = (24,78,73): the unused bits force the
        // planar mode for any anchors
        let anchors: [[u8; 3]; 3] = [[6, 15, 10], [22, 39, 18], [6, 39, 18]];
        let bits = planar_pack(anchors).unwrap_or(0);
        // the fields by hand from the specification's layout: RO 62..57, GO 56 + 54..49, BO 48
        // + 44..43 + 41..39, RH 38..34 + 32, then GH, BH, RV, GV, BV packed down to bit 0
        let want = (6u64 << 57)
            | (15 << 49)
            | (1 << 43) // BO = 0b001010: bit 48 = 0, 44..43 = 01, 41..39 = 010
            | (2 << 39)
            | (11 << 34) // RH = 0b010110: 38..34 = 01011, 32 = 0
            | (1 << 33)
            | (39 << 25)
            | (18 << 19)
            | (6 << 13)
            | (39 << 6)
            | 18;
        assert_eq!(bits & !PLANAR_FREE, want);
        let bytes = bits.to_be_bytes();
        assert_eq!(classify_block(&bytes), RgbMode::Planar);
        let texels = decode_block_rgb(&bytes);
        let [o, h, v] = planar_anchors(bits);
        assert_eq!([o[0], o[1], o[2]], [24, 30, 40]);
        assert_eq!([h[0], h[1], h[2]], [89, 78, 73]);
        assert_eq!([v[0], v[1], v[2]], [24, 78, 73]);
        for (x, y) in [(0usize, 0usize), (3, 0), (0, 3), (3, 3), (1, 2)] {
            let want = [
                planar_channel(x as i32, y as i32, o[0], h[0], v[0]) as u8,
                planar_channel(x as i32, y as i32, o[1], h[1], v[1]) as u8,
                planar_channel(x as i32, y as i32, o[2], h[2], v[2]) as u8,
                255,
            ];
            assert_eq!(texels[y * 4 + x], want, "at {x},{y}");
        }
    }

    #[test]
    fn planar_refinement_never_loses_to_plain_rounding() {
        // the plainly rounded triple is one of the 27 tried, so the refined channel error can
        // only be lower or equal; on random blocks it is strictly lower for some channel
        let channel_err = |texels: &[[i32; 3]; 16], c: usize, q: [u8; 3]| -> u64 {
            let expand = if c == 1 { expand7 } else { expand6 };
            let [o, h, v] = q.map(expand);
            let mut err = 0u64;
            for y in 0..4i32 {
                for x in 0..4i32 {
                    let d = planar_channel(x, y, o, h, v) - texels[(y * 4 + x) as usize][c];
                    err += (d * d) as u64;
                }
            }
            err
        };
        let mut rng = Rng(0x91a2_b3c4_d5e6_f708);
        let mut strictly_better = 0usize;
        for _ in 0..300 {
            let mut texels = [[0i32; 3]; 16];
            for t in &mut texels {
                *t = [
                    i32::from(rng.byte()),
                    i32::from(rng.byte()),
                    i32::from(rng.byte()),
                ];
            }
            let values = [
                i32::from(rng.byte()),
                i32::from(rng.byte()),
                i32::from(rng.byte()),
            ];
            for c in 0..3usize {
                let levels = if c == 1 { 127 } else { 63 };
                let plain = values.map(|v| quantize(v, levels) as u8);
                let refined = refine_planar_channel(&texels, c, values);
                let (e_plain, e_refined) = (channel_err(&texels, c, plain), channel_err(&texels, c, refined));
                assert!(e_refined <= e_plain, "{e_refined} > {e_plain}");
                strictly_better += usize::from(e_refined < e_plain);
            }
        }
        assert!(strictly_better > 0);
    }
}
