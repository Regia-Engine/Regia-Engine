// SPDX-License-Identifier: MPL-2.0
//! Bit-level writer and reader for the replication codec (spec 71 § 3).
//!
//! Values are packed least-significant bit first, so a `bool` costs one bit and a quantized float
//! costs exactly its configured width. Every read checks the remaining length and returns a
//! [`DecodeError`]: the reader handles untrusted bytes and never panics.

use thiserror::Error;

/// Why a bit stream could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DecodeError {
    /// The input ended before a field was complete.
    #[error("truncated: needed {needed} more bits at bit {at}")]
    Truncated {
        /// Bit offset of the field that did not fit.
        at: usize,
        /// Bits the field needed.
        needed: usize,
    },
    /// A length prefix declared more elements or bytes than its maximum.
    #[error("field too long: {len}, maximum {max}")]
    TooLong {
        /// Declared length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// A field held a value outside its documented range.
    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

/// Bits [`BitWriter::write_varint`] takes for a value.
pub fn varint_bits(value: u64) -> usize {
    (64 - (value | 1).leading_zeros() as usize).div_ceil(7) * 8
}

/// Appends bit fields to a growable buffer.
#[derive(Debug, Default, Clone)]
pub struct BitWriter {
    bytes: Vec<u8>,
    len_bits: usize,
}

impl BitWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty writer with room for `bytes` bytes.
    pub fn with_capacity(bytes: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(bytes),
            len_bits: 0,
        }
    }

    /// Bits written so far.
    pub fn bit_len(&self) -> usize {
        self.len_bits
    }

    /// Whether nothing was written.
    pub fn is_empty(&self) -> bool {
        self.len_bits == 0
    }

    /// The bytes written so far (the last byte is zero-padded).
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Forgets what was written and keeps the buffer.
    pub fn clear(&mut self) {
        self.bytes.clear();
        self.len_bits = 0;
    }

    /// The finished buffer (the last byte is zero-padded).
    pub fn finish(self) -> Vec<u8> {
        self.bytes
    }

    /// Writes the low `bits` bits of `value` (`bits` at most 64; larger widths write 64).
    #[inline]
    pub fn write_bits(&mut self, value: u64, bits: u32) {
        #[cfg(test)]
        bump_write_bits();
        let bits = bits.min(64);
        if bits == 0 {
            return;
        }
        let v = if bits == 64 {
            value
        } else {
            value & ((1u64 << bits) - 1)
        };
        let used = (self.len_bits % 8) as u32;
        // The partial byte takes the low bits, whole new bytes the rest (at most 9).
        let mut rest = u128::from(v) << used;
        if used != 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last |= rest as u8;
            rest >>= 8;
        }
        let new_bytes = (used + bits).div_ceil(8) - u32::from(used != 0);
        let le = rest.to_le_bytes();
        self.bytes
            .extend_from_slice(le.get(..new_bytes as usize).unwrap_or(&le));
        self.len_bits += bits as usize;
    }

    /// One bit.
    #[inline]
    pub fn write_bool(&mut self, value: bool) {
        self.write_bits(u64::from(value), 1);
    }

    /// An unsigned varint: 7-bit groups, low group first, a continuation bit per group.
    pub fn write_varint(&mut self, mut value: u64) {
        loop {
            let group = value & 0x7f;
            value >>= 7;
            if value == 0 {
                self.write_bits(group, 8);
                return;
            }
            self.write_bits(group | 0x80, 8);
        }
    }

    /// A signed integer as a zig-zag varint (small magnitudes stay small).
    pub fn write_zigzag(&mut self, value: i64) {
        self.write_varint(((value << 1) ^ (value >> 63)) as u64);
    }

    /// An `f32` as its 32 raw bits.
    pub fn write_f32(&mut self, value: f32) {
        self.write_bits(u64::from(value.to_bits()), 32);
    }

    /// An `f64` as its 64 raw bits.
    pub fn write_f64(&mut self, value: f64) {
        self.write_bits(value.to_bits(), 64);
    }

    /// Raw bytes, eight bits each (no length prefix).
    ///
    /// Copies each chunk with [`BitWriter::append_slice`] instead of one `write_bits` per byte.
    /// Chunks stay at most 1 MiB so the bit count cannot overflow `usize`.
    pub fn write_bytes(&mut self, bytes: &[u8]) {
        const CHUNK: usize = 1 << 20;
        for chunk in bytes.chunks(CHUNK) {
            self.append_slice(chunk, chunk.len() * 8);
        }
    }

    /// Appends every bit of another writer.
    pub fn append(&mut self, other: &BitWriter) {
        self.append_slice(&other.bytes, other.len_bits);
    }

    /// Appends the first `bits` bits of `bytes` (a finished writer's bytes; `bits` is cut to what
    /// `bytes` holds).
    pub fn append_slice(&mut self, bytes: &[u8], bits: usize) {
        let bits = bits.min(bytes.len() * 8);
        let Some(whole) = bytes.get(..bits.div_ceil(8)) else {
            return;
        };
        let used = (self.len_bits % 8) as u32;
        if used == 0 {
            self.bytes.extend_from_slice(whole);
        } else {
            // Every source byte straddles two destination bytes.
            self.bytes.reserve(whole.len());
            let mut carry = self.bytes.pop().unwrap_or(0);
            for b in whole {
                self.bytes.push(carry | (*b << used));
                carry = *b >> (8 - used);
            }
            self.bytes.push(carry);
        }
        self.len_bits += bits;
        // The byte count and the zero padding of the last byte stay what `write_bits` leaves.
        self.bytes.truncate(self.len_bits.div_ceil(8));
        let spare = self.len_bits % 8;
        if spare != 0
            && let Some(last) = self.bytes.last_mut()
        {
            *last &= (1u8 << spare) - 1;
        }
    }
}

/// Reads bit fields from a byte slice.
#[derive(Debug, Clone)]
pub struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    /// A reader at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// The current bit offset.
    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    /// Bits left (including the zero padding of the last byte).
    pub fn remaining_bits(&self) -> usize {
        (self.bytes.len() * 8).saturating_sub(self.pos)
    }

    /// Reads `bits` bits (at most 64).
    pub fn read_bits(&mut self, bits: u32) -> Result<u64, DecodeError> {
        #[cfg(test)]
        bump_read_bits();
        if bits > 64 {
            return Err(DecodeError::Invalid("bit width above 64"));
        }
        if self.remaining_bits() < bits as usize {
            return Err(DecodeError::Truncated {
                at: self.pos,
                needed: bits as usize,
            });
        }
        let mut out = 0u64;
        let mut got = 0u32;
        while got < bits {
            let Some(byte) = self.bytes.get(self.pos / 8) else {
                return Err(DecodeError::Truncated {
                    at: self.pos,
                    needed: (bits - got) as usize,
                });
            };
            let off = (self.pos % 8) as u32;
            let take = (8 - off).min(bits - got);
            let chunk = (u64::from(*byte) >> off) & ((1u64 << take) - 1);
            out |= chunk << got;
            got += take;
            self.pos += take as usize;
        }
        Ok(out)
    }

    /// One bit.
    pub fn read_bool(&mut self) -> Result<bool, DecodeError> {
        Ok(self.read_bits(1)? == 1)
    }

    /// An unsigned varint written by [`BitWriter::write_varint`] (at most 10 groups).
    pub fn read_varint(&mut self) -> Result<u64, DecodeError> {
        let mut out = 0u64;
        for i in 0..10u32 {
            let group = self.read_bits(8)?;
            let shift = i * 7;
            if shift == 63 && (group & 0x7e) != 0 {
                return Err(DecodeError::Invalid("varint overflows 64 bits"));
            }
            out |= (group & 0x7f) << shift;
            if group & 0x80 == 0 {
                return Ok(out);
            }
        }
        Err(DecodeError::Invalid("varint longer than 10 groups"))
    }

    /// A zig-zag varint.
    pub fn read_zigzag(&mut self) -> Result<i64, DecodeError> {
        let v = self.read_varint()?;
        Ok(((v >> 1) as i64) ^ -((v & 1) as i64))
    }

    /// An `f32` from 32 raw bits.
    pub fn read_f32(&mut self) -> Result<f32, DecodeError> {
        Ok(f32::from_bits(self.read_bits(32)? as u32))
    }

    /// An `f64` from 64 raw bits.
    pub fn read_f64(&mut self) -> Result<f64, DecodeError> {
        Ok(f64::from_bits(self.read_bits(64)?))
    }

    /// `n` raw bytes. `n` is checked against the remaining input before allocating.
    ///
    /// An aligned cursor copies the slice. An unaligned cursor shifts each byte once
    /// (`(lo >> off) | (hi << (8 - off))`), the same bits `read_bits(8)` would return.
    pub fn read_bytes(&mut self, n: usize) -> Result<Vec<u8>, DecodeError> {
        let need = n.saturating_mul(8);
        if self.remaining_bits() < need {
            return Err(DecodeError::Truncated {
                at: self.pos,
                needed: need,
            });
        }
        let mut out = Vec::with_capacity(n);
        let off = (self.pos % 8) as u32;
        let start = self.pos / 8;
        if off == 0 {
            let Some(slice) = self.bytes.get(start..start + n) else {
                return Err(DecodeError::Truncated {
                    at: self.pos,
                    needed: need,
                });
            };
            out.extend_from_slice(slice);
        } else {
            let Some(src) = self.bytes.get(start..) else {
                return Err(DecodeError::Truncated {
                    at: self.pos,
                    needed: need,
                });
            };
            // One extra source byte holds the high bits of the last unaligned output byte.
            if src.len() < n.saturating_add(1) {
                return Err(DecodeError::Truncated {
                    at: self.pos,
                    needed: need,
                });
            }
            let shift = 8 - off;
            for pair in src.windows(2).take(n) {
                out.push((pair[0] >> off) | (pair[1] << shift));
            }
        }
        self.pos += need;
        Ok(out)
    }

    /// A length prefix (varint) checked against `max`.
    pub fn read_len(&mut self, max: usize) -> Result<usize, DecodeError> {
        let len = self.read_varint()?;
        let len = usize::try_from(len).unwrap_or(usize::MAX);
        if len > max {
            return Err(DecodeError::TooLong { len, max });
        }
        Ok(len)
    }
}

#[cfg(test)]
thread_local! {
    static WRITE_BITS_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
    static READ_BITS_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn bump_write_bits() {
    WRITE_BITS_CALLS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn bump_read_bits() {
    READ_BITS_CALLS.with(|c| c.set(c.get() + 1));
}

#[cfg(test)]
fn reset_write_bits_calls() {
    WRITE_BITS_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn write_bits_calls() -> u64 {
    WRITE_BITS_CALLS.with(|c| c.get())
}

#[cfg(test)]
fn reset_read_bits_calls() {
    READ_BITS_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
fn read_bits_calls() -> u64 {
    READ_BITS_CALLS.with(|c| c.get())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zigzag_round_trips_the_extremes() {
        for value in [i64::MIN, i64::MIN + 1, -1, 0, 1, i64::MAX] {
            let mut w = BitWriter::new();
            w.write_zigzag(value);
            let bytes = w.finish();
            assert_eq!(BitReader::new(&bytes).read_zigzag().ok(), Some(value));
        }
    }

    #[test]
    fn mixed_widths_round_trip() {
        let mut w = BitWriter::new();
        w.write_bool(true);
        w.write_bits(5, 3);
        w.write_bits(0x1234_5678_9abc_def0, 64);
        w.write_varint(300);
        w.write_zigzag(-7);
        w.write_f32(1.5);
        w.write_bytes(b"ab");
        assert_eq!(w.bit_len(), 1 + 3 + 64 + 16 + 8 + 32 + 16);
        let bytes = w.finish();
        let mut r = BitReader::new(&bytes);
        assert!(r.read_bool().expect("bool"));
        assert_eq!(r.read_bits(3).expect("3"), 5);
        assert_eq!(r.read_bits(64).expect("64"), 0x1234_5678_9abc_def0);
        assert_eq!(r.read_varint().expect("varint"), 300);
        assert_eq!(r.read_zigzag().expect("zz"), -7);
        assert_eq!(r.read_f32().expect("f32"), 1.5);
        assert_eq!(r.read_bytes(2).expect("bytes"), b"ab");
    }

    #[test]
    fn hostile_input_is_an_error_not_a_panic() {
        let mut r = BitReader::new(&[0xff; 3]);
        assert!(r.read_varint().is_err(), "unterminated varint");
        let mut r = BitReader::new(&[0xff; 12]);
        assert!(r.read_varint().is_err(), "varint past 10 groups");
        let mut r = BitReader::new(&[1]);
        assert!(r.read_bits(9).is_err());
        assert!(r.read_bits(65).is_err());
        let mut r = BitReader::new(&[0x80, 0x80, 0x01]);
        assert!(matches!(r.read_len(4), Err(DecodeError::TooLong { .. })));
        let mut r = BitReader::new(&[]);
        assert!(r.read_bytes(usize::MAX).is_err());
    }

    /// The stream as one bit per entry, the way the format defines it.
    fn bits_of(bytes: &[u8], len: usize) -> Vec<bool> {
        (0..len)
            .map(|i| bytes.get(i / 8).is_some_and(|b| b & (1 << (i % 8)) != 0))
            .collect()
    }

    #[test]
    fn writes_match_the_bit_stream_at_every_offset() {
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            seed ^ (seed >> 29)
        };
        let mut w = BitWriter::new();
        let mut expect: Vec<bool> = Vec::new();
        for _ in 0..4000 {
            let width = (next() % 65) as u32;
            let value = next();
            w.write_bits(value, width);
            expect.extend((0..width).map(|i| value & (1 << i) != 0));
            assert_eq!(w.as_bytes().len(), expect.len().div_ceil(8));
        }
        assert_eq!(w.bit_len(), expect.len());
        assert_eq!(bits_of(w.as_bytes(), expect.len()), expect);
    }

    #[test]
    fn append_matches_the_bit_stream_at_every_offset() {
        for offset in 0..16u32 {
            for len in [0u32, 1, 3, 7, 8, 9, 15, 16, 17, 64] {
                for cut in [len, len.saturating_sub(1), len.saturating_sub(5), len + 9] {
                    let mut src = BitWriter::new();
                    src.write_bits(0xd5aa_55c3_0ff0_9a65, len);
                    let mut w = BitWriter::new();
                    w.write_bits(0x5a5a, offset);
                    let mut expect = bits_of(w.as_bytes(), offset as usize);
                    let kept = (cut as usize).min(src.as_bytes().len() * 8);
                    expect.extend(bits_of(src.as_bytes(), kept));
                    w.append_slice(src.as_bytes(), cut as usize);
                    assert_eq!(w.bit_len(), expect.len(), "{offset} {len} {cut}");
                    assert_eq!(
                        w.as_bytes().len(),
                        expect.len().div_ceil(8),
                        "{offset} {len} {cut}"
                    );
                    assert_eq!(
                        bits_of(w.as_bytes(), expect.len()),
                        expect,
                        "{offset} {len} {cut}"
                    );
                    // The padding stays zero, so later writes OR into clean bits.
                    let spare = expect.len() % 8;
                    if spare != 0 {
                        assert_eq!(w.as_bytes().last().map_or(0, |b| b >> spare), 0);
                    }
                }
            }
        }
    }

    #[test]
    fn varint_bits_counts_what_write_varint_writes() {
        for v in [0, 1, 127, 128, 16_383, 16_384, u64::from(u32::MAX), u64::MAX] {
            let mut w = BitWriter::new();
            w.write_varint(v);
            assert_eq!(varint_bits(v), w.bit_len(), "{v}");
        }
    }

    #[test]
    fn append_concatenates_bit_exactly() {
        let mut a = BitWriter::new();
        a.write_bits(0b101, 3);
        let mut b = BitWriter::new();
        b.write_bits(0b11, 2);
        b.write_bits(0xabcd, 16);
        a.append(&b);
        assert_eq!(a.bit_len(), 21);
        let bytes = a.finish();
        let mut r = BitReader::new(&bytes);
        assert_eq!(r.read_bits(3).expect("a"), 0b101);
        assert_eq!(r.read_bits(2).expect("b"), 0b11);
        assert_eq!(r.read_bits(16).expect("c"), 0xabcd);
    }

    /// A bool leaves the cursor one bit off a byte. Payload bytes must match a bit-at-a-time
    /// write and come back intact, without one `write_bits` / `read_bits` call per byte.
    #[test]
    fn unaligned_write_bytes_matches_bit_at_a_time() {
        let payload: Vec<u8> = (0..4096u32)
            .map(|i| (i.wrapping_mul(17).wrapping_add(3)) as u8)
            .collect();
        let mut reference = BitWriter::new();
        reference.write_bool(true);
        for b in &payload {
            reference.write_bits(u64::from(*b), 8);
        }
        reset_write_bits_calls();
        let mut w = BitWriter::new();
        w.write_bool(true);
        let before = write_bits_calls();
        w.write_bytes(&payload);
        let calls = write_bits_calls() - before;
        assert_eq!(w.bit_len(), reference.bit_len());
        assert_eq!(w.as_bytes(), reference.as_bytes());
        assert_eq!(calls, 0, "write_bytes made {calls} write_bits calls");
        reset_read_bits_calls();
        let mut r = BitReader::new(w.as_bytes());
        assert!(r.read_bool().expect("bool"));
        let before = read_bits_calls();
        let got = r.read_bytes(payload.len()).expect("payload");
        let reads = read_bits_calls() - before;
        assert_eq!(got, payload);
        assert_eq!(reads, 0, "read_bytes made {reads} read_bits calls");
        // Every starting offset, including an aligned writer, matches the bit-at-a-time stream.
        for offset in 0..8u32 {
            let mut reference = BitWriter::new();
            reference.write_bits(0x5a, offset);
            for b in &payload[..64] {
                reference.write_bits(u64::from(*b), 8);
            }
            let mut w = BitWriter::new();
            w.write_bits(0x5a, offset);
            w.write_bytes(&payload[..64]);
            assert_eq!(w.as_bytes(), reference.as_bytes(), "offset {offset}");
            let mut r = BitReader::new(w.as_bytes());
            if offset > 0 {
                assert_eq!(r.read_bits(offset).expect("pad"), 0x5a & ((1 << offset) - 1));
            }
            assert_eq!(r.read_bytes(64).expect("back"), &payload[..64]);
        }
    }
}
