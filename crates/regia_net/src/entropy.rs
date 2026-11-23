// SPDX-License-Identifier: MPL-2.0
//! Entropy coding of replication state frames (spec 72 § 18, ADR-3315).
//!
//! Range coding shrinks the state-channel payload below what field deltas and quantization
//! achieve alone (spec 72 § 15: "a later entropy-coding pass is a protocol version bump"). Only
//! entity state frames are compressed: never the handshake, authentication, chat or game-message
//! payloads, so attacker-chosen message bytes never sit next to secrets in a compressed blob
//! (the CRIME/BREACH class, ADR-3315). The stage runs before the link's encryption, on plaintext
//! that the transport authenticates, so a corrupted or truncated payload is rejected as a decode
//! error, never a panic.
//!
//! The coder is an adaptive binary range coder in the lineage of G. N. N. Martin's range
//! encoding (1979) with the carry-deferring byte emitter and 11-bit adaptive bit probabilities
//! of the public LZMA specification (`docs/provenance.md`). Every byte is coded bit by bit
//! against one of 64 adaptive models (8 bit positions × 8 contexts from the previous byte's
//! high bits), adapting at `1/32` per occurrence.
//!
//! The model state is **per packet**: the encoder and decoder both start from the fixed initial
//! probabilities for every frame, so a lost or reordered frame decodes identically however many
//! frames went missing or arrived out of order before it — no acknowledged baseline, resend or
//! re-synchronization is involved. This is the property the conditioner tests prove.
//!
//! Negotiation (ADR-3315): `NET_PROTOCOL_VERSION` 3 adds this stage; protocol 2 peers connect
//! unchanged with entropy off. A connection gets entropy when the server is configured for it
//! **and** the client's first `ACK` names protocol ≥ 3 (the client's build constant, which a
//! protocol 2 build never sends). The wire marker is a new frame tag, so a peer never has to
//! guess: a server only sends [`TAG_STATE_ENTROPY`] frames to connections it negotiated v3 with.

use serde::{Deserialize, Serialize};

/// The state-frame tag a v3 server writes for an entropy-negotiated connection (`TAG_STATE` is 7;
/// a new tag, not a flag inside the old frame, so no peer ever parses a frame it did not
/// negotiate).
pub const TAG_STATE_ENTROPY: u8 = 8;

/// Largest entropy-coded payload accepted by the decoder, in bytes. State frames are MTU-sized
/// (spec 72 § 7) except one oversize record at a time (16 KiB, `MAX_ENTITY_STATE_BYTES`); 1 MiB
/// bounds hostile length words far above anything legitimate.
pub const MAX_ENTROPY_PAYLOAD_BYTES: usize = 1 << 20;

/// Largest decompressed body accepted by the decoder, in bytes: an oversize record plus the
/// record-count prefix.
pub const MAX_ENTROPY_DECOMPRESSED_BYTES: usize = MAX_ENTROPY_PAYLOAD_BYTES + 64;

/// Entropy coding of replication state frames (spec 72 § 18; `replication.entropy` in
/// `config/net.ron`, `[net] entropy` in `server.toml`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EntropyMode {
    /// No entropy stage: state frames are byte-identical to protocol 2 (the pinned-bytes test in
    /// `state_channel.rs` guards this).
    #[default]
    Off,
    /// Adaptive binary range coding per frame, with a per-frame raw fallback when coding does
    /// not shrink the body.
    Range,
}

impl EntropyMode {
    /// Lowercase name (agent protocol, `net.entropy_stats`).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Range => "range",
        }
    }
}

/// The negotiated outcome for one connection: entropy on only when the server is configured for
/// it and the client's first acknowledged protocol speaks v3.
pub fn negotiated(client_protocol: u32, server: EntropyMode) -> EntropyMode {
    const ENTROPY_PROTOCOL: u32 = 3;
    if server == EntropyMode::Range && client_protocol >= ENTROPY_PROTOCOL {
        EntropyMode::Range
    } else {
        EntropyMode::Off
    }
}

/// Per-connection counters of the entropy stage (spec 72 § 18; `net.entropy_stats`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EntropyStats {
    /// State frames the entropy stage saw.
    pub frames: u64,
    /// Frames sent range-coded.
    pub coded: u64,
    /// Frames sent raw because coding did not shrink the body.
    pub raw_fallback: u64,
    /// Body bytes before the stage (record count plus records).
    pub bytes_in: u64,
    /// Body bytes after the stage (the flag, the coded length and the payload).
    pub bytes_out: u64,
}

impl EntropyStats {
    /// Bytes saved by the stage so far (`bytes_in − bytes_out`, 0 when negative).
    pub fn saved(&self) -> i64 {
        (self.bytes_in as i64).saturating_sub(self.bytes_out as i64)
    }
}

/// Why an entropy payload could not be decoded. Every variant maps to a `DecodeError` at the
/// frame layer; none panics.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EntropyError {
    /// The input ended inside the payload header or the coded stream.
    #[error("truncated entropy payload")]
    Truncated,
    /// A length word exceeded its bound.
    #[error("entropy payload too long: {len} bytes, maximum {max}")]
    TooLong {
        /// Declared length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// The coded stream does not decode to the declared length.
    #[error("entropy payload decoded to {decoded} bytes, expected {expected}")]
    LengthMismatch {
        /// Bytes produced.
        decoded: usize,
        /// Bytes the header declared.
        expected: usize,
    },
}

/// The adaptive binary probability model: an 11-bit counter in `(0, 2048)`, updated at `1/32`
/// per occurrence (the LZMA literal model).
#[derive(Debug, Clone, Copy)]
struct BitModel(u16);

impl BitModel {
    const INITIAL: Self = Self(1 << 10);

    #[inline]
    fn probability(&self) -> u32 {
        u32::from(self.0)
    }

    #[inline]
    fn update(&mut self, bit: u32) {
        // Adaptation at 1/16 per occurrence: MTU-sized frames (about a kilobyte) converge within
        // the packet, which is all the model ever sees (it resets per frame).
        let p = u32::from(self.0);
        self.0 = if bit == 0 {
            p + ((2048 - p) >> 4)
        } else {
            p - (p >> 4)
        } as u16;
    }
}

/// 8 bit positions × 8 contexts (the previous byte's high bits; 0 at a frame's start).
const MODEL_BITS: usize = 8;
const MODEL_CONTEXTS: usize = 8;
const MODEL_COUNT: usize = MODEL_BITS * MODEL_CONTEXTS;

#[derive(Debug, Clone)]
struct ModelSet {
    bits: [BitModel; MODEL_COUNT],
}

impl Default for ModelSet {
    fn default() -> Self {
        Self {
            bits: [BitModel::INITIAL; MODEL_COUNT],
        }
    }
}

impl ModelSet {
    /// Index of the model for one bit of a byte, given the previous byte (its top 3 bits select
    /// the context; no previous byte is context 0).
    #[inline]
    fn index(bit: usize, prev: u8) -> usize {
        usize::from(prev >> (8 - 3)) * MODEL_BITS + bit
    }
}

const TOP_VALUE: u32 = 1 << 24;

/// The range encoder (Martin 1979; the byte emitter of the public LZMA specification's range
/// encoder, re-derived here). Fresh for every frame: the model adapts within one frame only.
struct RangeEncoder {
    low: u64,
    range: u32,
    /// The deferred byte: carries propagate into it until the stream is flushed.
    cache: u8,
    /// Bytes delayed behind `cache` while a carry is still possible.
    cache_len: u64,
    out: Vec<u8>,
}

impl RangeEncoder {
    fn new(capacity: usize) -> Self {
        Self {
            low: 0,
            range: u32::MAX,
            cache: 0,
            cache_len: 1,
            out: Vec::with_capacity(capacity),
        }
    }

    fn shift_low(&mut self) {
        if self.low < 0xFF00_0000 || self.low > 0xFFFF_FFFF {
            let carry = (self.low >> 32) as u8;
            self.out.push(self.cache.wrapping_add(carry));
            for _ in 1..self.cache_len {
                self.out.push(0xFFu8.wrapping_add(carry));
            }
            // The next deferred byte is the top byte of `low` (MSB-first emission).
            self.cache = (self.low >> 24) as u8;
            self.cache_len = 1;
        } else {
            self.cache_len += 1;
        }
        self.low = (self.low << 8) & 0xFFFF_FFFF;
    }

    fn encode_bit(&mut self, prob: &mut BitModel, bit: u32) {
        let bound = (self.range >> 11) * prob.probability();
        if bit == 0 {
            self.range = bound;
        } else {
            self.low += u64::from(bound);
            self.range -= bound;
        }
        prob.update(bit);
        while self.range < TOP_VALUE {
            self.range <<= 8;
            self.shift_low();
        }
    }

    fn finish(mut self) -> Vec<u8> {
        for _ in 0..5 {
            self.shift_low();
        }
        self.out
    }
}

/// The matching decoder.
struct RangeDecoder<'a> {
    range: u32,
    code: u32,
    input: &'a [u8],
    pos: usize,
}

impl<'a> RangeDecoder<'a> {
    fn new(input: &'a [u8]) -> Result<Self, EntropyError> {
        if input.len() < 5 {
            return Err(EntropyError::Truncated);
        }
        let mut code = 0u32;
        for &b in &input[..5] {
            code = (code << 8) | u32::from(b);
        }
        Ok(Self {
            range: u32::MAX,
            code,
            input,
            pos: 5,
        })
    }

    fn decode_bit(&mut self, prob: &mut BitModel) -> Result<u32, EntropyError> {
        let bound = (self.range >> 11) * prob.probability();
        let bit = if self.code < bound {
            self.range = bound;
            0
        } else {
            self.code -= bound;
            self.range -= bound;
            1
        };
        prob.update(bit);
        while self.range < TOP_VALUE {
            self.range <<= 8;
            let next = match self.input.get(self.pos) {
                Some(&b) => b,
                None => return Err(EntropyError::Truncated),
            };
            self.pos += 1;
            self.code = (self.code << 8) | u32::from(next);
        }
        Ok(bit)
    }
}

/// Range-codes `body`; the result is the coded payload (a varint length prefix is not included).
pub fn encode_body(body: &[u8]) -> Vec<u8> {
    let mut encoder = RangeEncoder::new(body.len() / 2 + 16);
    let mut models = ModelSet::default();
    let mut prev = 0u8;
    for &byte in body {
        for bit in 0..MODEL_BITS {
            let b = (u32::from(byte) >> bit) & 1;
            encoder.encode_bit(&mut models.bits[ModelSet::index(bit, prev)], b);
        }
        prev = byte;
    }
    encoder.finish()
}

/// Decodes a [`encode_body`] payload back to exactly `expected_len` bytes.
pub fn decode_body(coded: &[u8], expected_len: usize) -> Result<Vec<u8>, EntropyError> {
    if expected_len > MAX_ENTROPY_DECOMPRESSED_BYTES {
        return Err(EntropyError::TooLong {
            len: expected_len,
            max: MAX_ENTROPY_DECOMPRESSED_BYTES,
        });
    }
    let mut decoder = RangeDecoder::new(coded)?;
    let mut models = ModelSet::default();
    let mut out = Vec::with_capacity(expected_len);
    let mut prev = 0u8;
    for _ in 0..expected_len {
        let mut byte = 0u8;
        for bit in 0..MODEL_BITS {
            let b = decoder.decode_bit(&mut models.bits[ModelSet::index(bit, prev)])?;
            byte |= (b as u8) << bit;
        }
        out.push(byte);
        prev = byte;
    }
    Ok(out)
}

/// A state frame with any entropy coding removed: [`Self::tag`] is the frame's original tag (7
/// or 8) and [`Self::bytes`] is the full frame; for a range-coded frame the bytes are the
/// rebuilt raw form (same header, flags byte 0, decoded body), else they borrow the input.
pub struct UnwrappedFrame<'a> {
    /// The original frame tag (`TAG_STATE` or [`TAG_STATE_ENTROPY`]).
    pub tag: u8,
    /// The frame with a coded body decoded; the body is raw from the flags byte on.
    pub bytes: std::borrow::Cow<'a, [u8]>,
}

/// Removes the entropy stage from one state frame. A protocol 2 `TAG_STATE` frame borrows
/// through. An entropy frame with the raw flag borrows through (the flags byte stays in place);
/// a range-coded frame is decoded and rebuilt. Every failure is a `DecodeError`, never a
/// panic: this runs on authenticated plaintext, so an error means tampering or a bug.
pub fn unwrap_frame(bytes: &[u8]) -> Result<UnwrappedFrame<'_>, crate::bits::DecodeError> {
    use crate::bits::{BitReader, DecodeError};
    let mut r = BitReader::new(bytes);
    let tag = u8::try_from(r.read_bits(8)?).map_err(|_| DecodeError::Invalid("frame tag"))?;
    if tag != TAG_STATE_ENTROPY {
        return Ok(UnwrappedFrame {
            tag,
            bytes: std::borrow::Cow::Borrowed(bytes),
        });
    }
    // tick and seq: the header reads identically on both tags; the values are re-written into
    // the rebuilt buffer so the header bits round trip exactly.
    let tick = r.read_varint()?;
    let seq = r.read_varint()?;
    match r.read_bits(8)? {
        0 => Ok(UnwrappedFrame {
            tag,
            bytes: std::borrow::Cow::Borrowed(bytes),
        }),
        1 => {
            let _decode_span = tracing::trace_span!("net_entropy_decode").entered();
            let len = usize::try_from(r.read_varint()?)
                .map_err(|_| DecodeError::Invalid("entropy payload length"))?;
            if len > MAX_ENTROPY_PAYLOAD_BYTES {
                return Err(DecodeError::TooLong {
                    len,
                    max: MAX_ENTROPY_PAYLOAD_BYTES,
                });
            }
            let coded = r.read_bytes(len)?;
            let body = decode_coded(&coded).map_err(|_| DecodeError::Invalid("entropy payload"))?;
            let mut w = crate::bits::BitWriter::with_capacity(3 + body.len());
            w.write_bits(u64::from(TAG_STATE_ENTROPY), 8);
            w.write_varint(tick);
            w.write_varint(seq);
            w.write_bits(0, 8); // flags: from here the body is raw
            w.write_bytes(&body);
            Ok(UnwrappedFrame {
                tag,
                bytes: std::borrow::Cow::Owned(w.finish()),
            })
        }
        _ => Err(DecodeError::Invalid("entropy flags")),
    }
}

/// Codes `plain`, returning the bytes to put on the wire: the coded payload when it is smaller
/// than `plain` (with a varint original-length header the decoder needs), otherwise `plain`
/// itself (the caller writes the raw fallback flag instead).
pub fn code_or_raw(plain: &[u8]) -> (Vec<u8>, bool) {
    let coded = encode_body(plain);
    if coded.len() < plain.len() {
        let mut out = Vec::with_capacity(coded.len() + 5);
        let mut len = plain.len() as u64;
        loop {
            let group = (len & 0x7f) as u8;
            len >>= 7;
            out.push(if len == 0 { group } else { group | 0x80 });
            if len == 0 {
                break;
            }
        }
        out.extend_from_slice(&coded);
        (out, true)
    } else {
        (plain.to_vec(), false)
    }
}

/// Decodes a [`code_or_raw`] coded payload (the `coded == true` case).
pub fn decode_coded(coded: &[u8]) -> Result<Vec<u8>, EntropyError> {
    // Varint original length, then the range-coded stream.
    let mut len = 0usize;
    let mut shift = 0u32;
    let mut pos = 0usize;
    loop {
        let Some(&group) = coded.get(pos) else {
            return Err(EntropyError::Truncated);
        };
        pos += 1;
        if shift >= usize::BITS {
            return Err(EntropyError::TooLong {
                len: usize::MAX,
                max: MAX_ENTROPY_DECOMPRESSED_BYTES,
            });
        }
        len |= usize::from(group & 0x7f) << shift;
        shift += 7;
        if group & 0x80 == 0 {
            break;
        }
    }
    if len > MAX_ENTROPY_DECOMPRESSED_BYTES {
        return Err(EntropyError::TooLong {
            len,
            max: MAX_ENTROPY_DECOMPRESSED_BYTES,
        });
    }
    let stream = coded.get(pos..).ok_or(EntropyError::Truncated)?;
    let out = decode_body(stream, len)?;
    if out.len() != len {
        return Err(EntropyError::LengthMismatch {
            decoded: out.len(),
            expected: len,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SplitMix64: a small seeded RNG for deterministic fixtures (no dependency).
    pub(crate) struct Rng(u64);

    impl Rng {
        pub(crate) fn new(seed: u64) -> Self {
            Self(seed)
        }

        pub(crate) fn next(&mut self) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        }

        /// An integer in `0..n`.
        pub(crate) fn below(&mut self, n: u64) -> u64 {
            self.next() % n.max(1)
        }

        /// A byte with the given zero-bit probability (skewed distributions compress).
        pub(crate) fn skewed_byte(&mut self, zero_chance: u32) -> u8 {
            let mut b = 0u8;
            for bit in 0..8 {
                if self.below(100) >= u64::from(zero_chance) {
                    b |= 1 << bit;
                }
            }
            b
        }
    }

    #[test]
    fn coder_round_trips_seeded_bodies() {
        let mut rng = Rng::new(0xC0FF_EE00_1357_9BDF);
        for len in [0usize, 1, 2, 7, 64, 255, 1024, 2048] {
            for _ in 0..16 {
                let body: Vec<u8> = (0..len).map(|_| rng.skewed_byte(55)).collect();
                let coded = encode_body(&body);
                let back = decode_body(&coded, body.len()).expect("decodes");
                assert_eq!(back, body, "len {len}");
            }
        }
    }

    #[test]
    fn coder_round_trips_adversarial_patterns() {
        for body in [
            vec![0u8; 512],
            vec![0xFF; 512],
            (0..512).map(|i| (i % 2) as u8 * 0xFF).collect::<Vec<_>>(),
            (0..513).map(|i| (i % 7) as u8).collect::<Vec<_>>(),
            b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_vec(),
        ] {
            let coded = encode_body(&body);
            assert_eq!(decode_body(&coded, body.len()).expect("decodes"), body);
        }
    }

    #[test]
    fn encoding_is_deterministic_for_a_fixed_seed() {
        let mut rng = Rng::new(42);
        let body: Vec<u8> = (0..400).map(|_| rng.skewed_byte(60)).collect();
        let a = encode_body(&body);
        let b = encode_body(&body);
        assert_eq!(a, b);
        // Pinned bytes: a change to the coder or the model that alters the stream is caught here.
        // (The exact literal is pinned from the first green run — see the lane report.)
        let expected: &[u8] = &[
            0, 84, 76, 197, 82, 117, 127, 91, 247, 209, 102, 103, 126, 184, 130, 13, 80, 243, 59, 126, 139,
            152, 51, 182, 9, 7, 49, 164, 179, 211, 36, 71, 30, 201, 199, 10, 116, 126, 144, 68, 6, 203, 214,
            218, 217, 255, 80, 110, 123, 42, 45, 253, 169, 246, 205, 8, 235, 165, 253, 232, 203, 3, 8, 85,
            23, 89, 76, 197, 124, 178, 147, 152, 159, 146, 213, 197, 12, 150, 219, 45, 80, 11, 79, 66, 136,
            178, 240, 166, 201, 160, 70, 59, 177, 244, 175, 185, 179, 163, 166, 75, 228, 156, 38, 234, 243,
            116, 38, 199, 112, 53, 233, 32, 245, 100, 106, 239, 57, 22, 75, 6, 4, 19, 179, 209, 172, 234,
            131, 19, 220, 227, 88, 126, 207, 21, 131, 247, 134, 253, 224, 190, 96, 42, 162, 56, 196, 123, 36,
            77, 101, 15, 164, 181, 232, 139, 82, 43, 172, 85, 217, 21, 85, 124, 40, 161, 59, 38, 194, 41,
            139, 102, 53, 74, 207, 72, 71, 71, 39, 189, 182, 25, 191, 243, 36, 70, 248, 142, 73, 41, 23, 245,
            39, 49, 238, 151, 83, 151, 188, 190, 94, 62, 39, 128, 169, 85, 72, 70, 88, 108, 246, 93, 119, 60,
            130, 42, 228, 164, 79, 25, 177, 201, 157, 6, 177, 235, 140, 6, 57, 168, 215, 5, 187, 106, 194,
            31, 155, 20, 113, 75, 2, 247, 241, 55, 244, 234, 82, 183, 205, 41, 71, 138, 83, 19, 217, 136,
            160, 63, 17, 79, 11, 68, 90, 120, 243, 12, 211, 59, 18, 143, 34, 230, 97, 184, 38, 227, 8, 31,
            244, 163, 112, 245, 164, 63, 106, 195, 100, 76, 70, 207, 229, 80, 95, 144, 92, 184, 105, 45, 35,
            65, 102, 46, 80, 111, 36, 90, 99, 118, 97, 23, 78, 146, 73, 116, 61, 132, 61, 40, 4, 69, 193, 28,
            249, 92, 4, 229, 210, 66, 132, 91, 138, 49, 142, 178, 133, 22, 18, 15, 67, 38, 233, 74, 123, 102,
            99, 236, 53, 64, 65, 18, 113, 168, 198, 191, 26, 97, 71, 238, 227, 46, 243, 102, 228, 174, 2, 94,
            106, 228, 242, 125, 30, 242, 104, 123, 153, 139, 110, 58, 130, 253, 57, 7, 211, 70, 76, 31, 152,
            165, 19, 118, 86, 25, 212, 18, 182, 54, 14, 113, 126, 122, 252, 34, 230, 225, 50, 176,
        ];
        assert_eq!(a, expected);
    }

    #[test]
    fn incompressible_bodies_take_the_raw_fallback() {
        let mut rng = Rng::new(7);
        // Near-uniform bytes: the coded stream is the body plus its length prefix plus five
        // flush bytes, so it is never smaller.
        let body: Vec<u8> = (0..512).map(|_| rng.next() as u8).collect();
        let (out, coded) = code_or_raw(&body);
        assert!(!coded);
        assert_eq!(out, body);
    }

    #[test]
    fn code_or_raw_round_trips_when_coded() {
        let mut rng = Rng::new(99);
        let body: Vec<u8> = (0..900).map(|_| rng.skewed_byte(65)).collect();
        let (out, coded) = code_or_raw(&body);
        assert!(coded, "skewed data should code smaller");
        assert!(out.len() < body.len());
        assert_eq!(decode_coded(&out).expect("decodes"), body);
    }

    #[test]
    fn truncated_and_corrupt_payloads_are_errors_not_panics() {
        let body = b"the quick brown fox jumps over the lazy dog, repeatedly and with feeling.";
        let (coded, true) = code_or_raw(body) else {
            panic!("fixture should code");
        };
        for cut in 0..coded.len() {
            let _ = decode_coded(&coded[..cut]);
        }
        let mut corrupt = coded.clone();
        for i in 0..corrupt.len() {
            corrupt[i] ^= 0x5A;
            let _ = decode_coded(&corrupt);
            corrupt[i] ^= 0x5A;
        }
        // A hostile original-length word is refused.
        assert!(matches!(
            decode_coded(&[0xFF, 0xFF, 0xFF, 0xFF, 0x7F]),
            Err(EntropyError::TooLong { .. })
        ));
        // An empty payload is truncated, not a panic.
        assert_eq!(decode_coded(&[]), Err(EntropyError::Truncated));
    }

    #[test]
    fn negotiation_gates_on_protocol_three_and_server_config() {
        assert_eq!(negotiated(3, EntropyMode::Range), EntropyMode::Range);
        assert_eq!(negotiated(4, EntropyMode::Range), EntropyMode::Range);
        // A protocol 2 (or 1) client never gets entropy, whatever the server config.
        assert_eq!(negotiated(2, EntropyMode::Range), EntropyMode::Off);
        assert_eq!(negotiated(1, EntropyMode::Range), EntropyMode::Off);
        // A server configured off never codes, whoever connects.
        assert_eq!(negotiated(3, EntropyMode::Off), EntropyMode::Off);
    }
}

/// The measurement harness (spec 72 § 18 task 1): builds representative state-frame bodies with a
/// seeded generator, runs them through the entropy stage and reports per-field-kind byte
/// accounting plus median/p95 savings per workload. Numbers are printed by the harness tests and
/// recorded in `docs/provenance.md`'s spec section and the lane report; nothing here asserts wall
/// clocks.
#[cfg(test)]
pub(crate) mod harness {
    use super::tests::Rng;
    use super::{code_or_raw, decode_coded};
    use crate::bits::{BitWriter, varint_bits};

    /// Byte accounting buckets for one workload run.
    #[derive(Debug, Default, Clone, Copy)]
    pub(crate) struct FieldBytes {
        /// Frame header and record framing: the tag, tick, seq, count, per-record ids and length
        /// prefixes, record kind and version/base ticks.
        pub headers: u64,
        /// Quantized floats: fixed-width bit fields for positions, velocities and angles.
        pub quantized: u64,
        /// Changed-field and component masks.
        pub masks: u64,
        /// Varints (component indices, small counts, anim bytes).
        pub varints: u64,
    }

    /// One measured frame.
    struct Frame {
        /// The frame body as it leaves the packer: record count plus records.
        body: Vec<u8>,
        fields: FieldBytes,
    }

    /// An entity in a generated workload.
    struct SimEnt {
        id: u32,
        /// Millimetre position; smooth motion produces small quantized deltas on the wire.
        x: i64,
        y: i64,
        z: i64,
        yaw: u32,
        anim: u8,
    }

    /// Appends a varint to `w`, counting it into `fields.headers`.
    fn varint(w: &mut BitWriter, fields: &mut FieldBytes, value: u64) {
        fields.headers += varint_bits(value) as u64 / 8;
        w.write_varint(value);
    }

    /// One moving-entity workload: `n` entities, `move_chance`/`16` of them change per tick,
    /// positions quantized to millimetres (21-bit fields), a 12-bit yaw, an 8-bit anim byte and
    /// an 8-bit changed-field mask per changed record.
    fn moving_workload(seed: u64, name: &str, n: u32, move_chance: u32, ticks: u32) -> Workload {
        let mut rng = Rng::new(seed);
        let mut ents: Vec<SimEnt> = (0..n)
            .map(|id| SimEnt {
                id,
                x: rng.below(200_000) as i64 - 100_000,
                y: 0,
                z: rng.below(200_000) as i64 - 100_000,
                yaw: (rng.below(4096)) as u32,
                anim: 0,
            })
            .collect();
        let mut frames = Vec::new();
        for _ in 0..ticks {
            let mut w = BitWriter::new();
            let mut fields = FieldBytes::default();
            // Record count placeholder: written after the records are known.
            let mut rec = BitWriter::new();
            let mut rec_fields = FieldBytes::default();
            let mut count = 0u64;
            for e in &mut ents {
                if rng.below(16) >= u64::from(move_chance) {
                    continue;
                }
                count += 1;
                // Per-record framing: id and body length.
                varint(&mut rec, &mut rec_fields, u64::from(e.id));
                let mut body = BitWriter::new();
                let mut bf = FieldBytes::default();
                // Kind (2 bits) and version_ago: a delta against the version the baseline holds —
                // almost always the previous send tick.
                bf.headers += 1;
                body.write_bits(2, 2);
                let ago = if rng.below(100) < 88 { 1 } else { 2 + rng.below(2) };
                varint(&mut body, &mut bf, ago);
                // Changed-field mask: biased like real update masks — usually just the transform,
                // yaw and anim only sometimes.
                let roll = rng.below(100);
                let mask: u64 = if roll < 70 {
                    1
                } else if roll < 85 {
                    3
                } else if roll < 93 {
                    5
                } else {
                    1 | (1 << rng.below(5))
                };
                bf.masks += 1;
                body.write_bits(mask, 8);
                if mask & 1 != 0 {
                    // Quantized position: three 21-bit absolute fields at 1 mm. Entities walk
                    // (metres per second, not per tick), so the high bits of the fields are
                    // stable for many frames — the entropy the stage exists to remove.
                    bf.quantized += (63 / 8) as u64;
                    let speed: i64 = if rng.below(100) < 90 { 128 } else { 640 };
                    e.x += rng.below((speed * 2) as u64) as i64 - speed;
                    e.z += rng.below((speed * 2) as u64) as i64 - speed;
                    body.write_bits((e.x & 0x1F_FFFF) as u64, 21);
                    body.write_bits((e.y & 0x1F_FFFF) as u64, 21);
                    body.write_bits((e.z & 0x1F_FFFF) as u64, 21);
                }
                if mask & 2 != 0 {
                    bf.quantized += (12 / 8) as u64;
                    e.yaw = (e.yaw + 1 + (rng.below(4) as u32)) & 0xFFF;
                    body.write_bits(u64::from(e.yaw), 12);
                }
                if mask & 4 != 0 {
                    bf.varints += 1;
                    e.anim = e.anim.wrapping_add(1);
                    body.write_bits(u64::from(e.anim), 8);
                }
                let body_bits = body.bit_len();
                rec_fields.headers += (varint_bits(body_bits as u64) / 8) as u64;
                rec.write_varint(body_bits as u64);
                rec.append(&body);
                rec_fields.headers += bf.headers;
                rec_fields.quantized += bf.quantized;
                rec_fields.masks += bf.masks;
                rec_fields.varints += bf.varints;
            }
            // Header: the frame tag, tick and seq are constant-width and excluded from the body
            // accounting; the record count is part of the entropy stage's input.
            fields.headers += (varint_bits(count) / 8) as u64;
            w.write_varint(count);
            w.append(&rec);
            fields.headers += rec_fields.headers;
            fields.quantized += rec_fields.quantized;
            fields.masks += rec_fields.masks;
            fields.varints += rec_fields.varints;
            frames.push(Frame {
                body: w.finish(),
                fields,
            });
        }
        Workload {
            name: name.to_owned(),
            frames,
        }
    }

    /// A mostly-static level workload: a spawn-heavy first frame, then sparse updates of a few
    /// entities per tick (spec 71 level binding).
    fn level_workload(seed: u64, n: u32, ticks: u32) -> Workload {
        moving_workload(seed, "level", n, 1, ticks)
    }

    pub(crate) struct Workload {
        name: String,
        frames: Vec<Frame>,
    }

    /// The harness result for one workload.
    #[derive(Debug)]
    pub(crate) struct Report {
        pub name: String,
        pub frames: usize,
        pub raw_bytes: u64,
        pub coded_bytes: u64,
        pub coded_frames: u64,
        pub median_saved_per_frame: i64,
        pub p95_saved_per_frame: i64,
        pub encode_nanos_per_frame: u128,
        pub decode_nanos_per_frame: u128,
        pub fields: FieldBytes,
    }

    /// The representative workloads (spec 72 § 18 task 1): the net_game template's avatars, 64
    /// and 256 moving replicated entities (the replication tests' scale), and level entities in
    /// the samples/stream_world class.
    pub(crate) fn workloads() -> Vec<Workload> {
        vec![
            moving_workload(0xA7A7_A7A7_0001, "avatars", 12, 14, 300),
            moving_workload(0xA7A7_A7A7_0002, "entities64", 64, 10, 300),
            moving_workload(0xA7A7_A7A7_0003, "entities256", 256, 6, 300),
            level_workload(0xA7A7_A7A7_0004, 512, 300),
        ]
    }

    fn percentile(sorted: &[i64], pct: usize) -> i64 {
        let idx = (sorted.len().saturating_sub(1)) * pct / 100;
        sorted.get(idx).copied().unwrap_or(0)
    }

    /// Runs `w` through the entropy stage and measures it. Timings are recorded for the report,
    /// never asserted.
    pub(crate) fn measure(w: &Workload) -> Report {
        let mut raw = 0u64;
        let mut coded_total = 0u64;
        let mut coded_frames = 0u64;
        let mut saved: Vec<i64> = Vec::new();
        let mut fields = FieldBytes::default();
        let mut enc_ns = 0u128;
        let mut dec_ns = 0u128;
        for f in &w.frames {
            raw += f.body.len() as u64;
            fields.headers += f.fields.headers;
            fields.quantized += f.fields.quantized;
            fields.masks += f.fields.masks;
            fields.varints += f.fields.varints;
            let t0 = std::time::Instant::now();
            let (coded, is_coded) = code_or_raw(&f.body);
            enc_ns += t0.elapsed().as_nanos();
            // The stage costs a flag byte and the coded length prefix on the wire.
            let out = coded.len() as u64 + if is_coded { 3 } else { 1 };
            coded_total += out;
            saved.push(f.body.len() as i64 - out as i64);
            if is_coded {
                coded_frames += 1;
                let t1 = std::time::Instant::now();
                assert_eq!(decode_coded(&coded).expect("round trip"), f.body);
                dec_ns += t1.elapsed().as_nanos();
            }
        }
        saved.sort_unstable();
        Report {
            name: w.name.clone(),
            frames: w.frames.len(),
            raw_bytes: raw,
            coded_bytes: coded_total,
            coded_frames,
            median_saved_per_frame: percentile(&saved, 50),
            p95_saved_per_frame: percentile(&saved, 95),
            encode_nanos_per_frame: enc_ns / w.frames.len() as u128,
            decode_nanos_per_frame: dec_ns / w.frames.len().max(1) as u128,
            fields,
        }
    }

    /// Prints the report table the lane records in the spec and its report.
    pub(crate) fn print_table(reports: &[Report]) {
        println!(
            "{:<12} {:>7} {:>10} {:>10} {:>7} {:>9} {:>9} {:>9} {:>9}",
            "workload", "frames", "raw", "entropy", "coded%", "med save", "p95 save", "enc ns", "dec ns"
        );
        for r in reports {
            println!(
                "{:<12} {:>7} {:>10} {:>10} {:>6}% {:>8}B {:>8}B {:>9} {:>9}",
                r.name,
                r.frames,
                r.raw_bytes,
                r.coded_bytes,
                r.coded_frames * 100 / r.frames as u64,
                r.median_saved_per_frame,
                r.p95_saved_per_frame,
                r.encode_nanos_per_frame,
                r.decode_nanos_per_frame,
            );
            println!(
                "  fields: headers={}B quantized={}B masks={}B varints={}B",
                r.fields.headers, r.fields.quantized, r.fields.masks, r.fields.varints
            );
        }
    }

    /// The harness test: every workload must round trip, and at least these two must show a real
    /// saving (spec 72 § 18 task 1 acceptance).
    #[test]
    fn harness_reports_savings_on_the_workloads() {
        let reports: Vec<Report> = workloads().iter().map(measure).collect();
        print_table(&reports);
        for name in ["avatars", "entities256"] {
            let r = reports.iter().find(|r| r.name == name).expect(name);
            assert!(
                r.coded_bytes < r.raw_bytes,
                "{name}: entropy must shrink the workload"
            );
        }
        let total_raw: u64 = reports.iter().map(|r| r.raw_bytes).sum();
        let total_coded: u64 = reports.iter().map(|r| r.coded_bytes).sum();
        assert!(total_coded < total_raw);
    }
}
