// SPDX-License-Identifier: MPL-2.0
//! Little-endian byte writer and reader shared by every `regia_net` codec.
//!
//! Every decoder in this crate reads untrusted bytes from the network, so the reader never panics:
//! each read checks the remaining length and returns a [`WireError`]. Length-prefixed fields carry
//! an explicit maximum so a hostile length word cannot make the decoder allocate unboundedly.

use thiserror::Error;

/// Why a byte sequence could not be decoded.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WireError {
    /// The input ended before a field was complete.
    #[error("truncated: needed {needed} more bytes at offset {at}")]
    Truncated {
        /// Byte offset of the field that did not fit.
        at: usize,
        /// Bytes the field needed.
        needed: usize,
    },
    /// A length-prefixed field declared more bytes than its maximum.
    #[error("field too long: {len} bytes, maximum {max}")]
    TooLong {
        /// Declared length.
        len: usize,
        /// Allowed maximum.
        max: usize,
    },
    /// A string field was not valid UTF-8.
    #[error("string field is not valid UTF-8")]
    Utf8,
    /// Bytes were left over after the last field.
    #[error("{0} trailing bytes after the last field")]
    Trailing(usize),
    /// A field held a value outside its documented range.
    #[error("invalid value: {0}")]
    Invalid(&'static str),
}

/// Appends little-endian fields to a growable buffer.
///
/// A length-prefix that does not fit records [`WireError`] on the writer. [`Self::finish_checked`]
/// returns that error. [`Self::finish`] still returns the bytes written before the failure.
#[derive(Debug, Default, Clone)]
pub struct WireWriter {
    buf: Vec<u8>,
    error: Option<WireError>,
}

impl WireWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty writer with room for `capacity` bytes.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
            error: None,
        }
    }

    /// An empty writer that writes into `buf`'s storage (its contents are dropped, its capacity
    /// kept): for a buffer a caller reuses across messages.
    pub(crate) fn reusing(mut buf: Vec<u8>) -> Self {
        buf.clear();
        Self { buf, error: None }
    }

    /// Makes room for at least `additional` more bytes.
    pub(crate) fn reserve(&mut self, additional: usize) {
        self.buf.reserve(additional);
    }

    /// Drops what was written (and any recorded failure) and keeps the storage.
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
        self.error = None;
    }

    /// Records the first encode failure. Later failures stay behind that one.
    pub fn fail(&mut self, error: WireError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// Bytes written so far.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// True when nothing has been written.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Writes one byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    /// Writes a `u16`.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a `u32`.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes a `u64`.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes an `i32`.
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Writes an `f32` as its IEEE-754 bits.
    pub fn f32(&mut self, v: f32) {
        self.buf.extend_from_slice(&v.to_bits().to_le_bytes());
    }

    /// Writes raw bytes with no length prefix.
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Writes a `u16` length prefix followed by the bytes. Fails when `bytes` is longer than `max`
    /// (or than `u16::MAX`).
    pub fn bytes_u16(&mut self, bytes: &[u8], max: usize) -> Result<(), WireError> {
        let max = max.min(usize::from(u16::MAX));
        if bytes.len() > max {
            let err = WireError::TooLong {
                len: bytes.len(),
                max,
            };
            self.fail(err.clone());
            return Err(err);
        }
        self.u16(bytes.len() as u16);
        self.raw(bytes);
        Ok(())
    }

    /// Writes a `u8` length prefix followed by the UTF-8 bytes. Fails when the string is longer
    /// than `max` bytes (or than 255).
    pub fn str_u8(&mut self, s: &str, max: usize) -> Result<(), WireError> {
        let max = max.min(usize::from(u8::MAX));
        if s.len() > max {
            let err = WireError::TooLong { len: s.len(), max };
            self.fail(err.clone());
            return Err(err);
        }
        self.u8(s.len() as u8);
        self.raw(s.as_bytes());
        Ok(())
    }

    /// The written bytes, including any fields written before an encode failure.
    pub fn finish(self) -> Vec<u8> {
        self.buf
    }

    /// The written bytes, or the first encode failure.
    pub fn finish_checked(self) -> Result<Vec<u8>, WireError> {
        match self.error {
            Some(err) => Err(err),
            None => Ok(self.buf),
        }
    }

    /// The written bytes, borrowed.
    pub fn as_slice(&self) -> &[u8] {
        &self.buf
    }
}

/// Reads little-endian fields from a byte slice without ever panicking.
#[derive(Debug, Clone)]
pub struct WireReader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> WireReader<'a> {
    /// A reader at the start of `buf`.
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    /// Current byte offset.
    pub fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not read yet.
    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// True when every byte has been read.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// Takes the next `n` bytes.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.remaining() < n {
            return Err(WireError::Truncated {
                at: self.pos,
                needed: n,
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// Takes the next `N` bytes as an array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        let slice = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(slice);
        Ok(out)
    }

    /// Reads one byte.
    pub fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.array::<1>()?[0])
    }

    /// Reads a `u16`.
    pub fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    /// Reads a `u32`.
    pub fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    /// Reads a `u64`.
    pub fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    /// Reads an `i32`.
    pub fn i32(&mut self) -> Result<i32, WireError> {
        Ok(i32::from_le_bytes(self.array()?))
    }

    /// Reads an `f32` from its IEEE-754 bits.
    pub fn f32(&mut self) -> Result<f32, WireError> {
        Ok(f32::from_bits(self.u32()?))
    }

    /// Reads a `u16` length prefix and that many bytes; the length must not exceed `max`.
    pub fn bytes_u16(&mut self, max: usize) -> Result<&'a [u8], WireError> {
        let len = usize::from(self.u16()?);
        if len > max {
            return Err(WireError::TooLong { len, max });
        }
        self.take(len)
    }

    /// Reads a `u8` length prefix and that many UTF-8 bytes; the length must not exceed `max`.
    pub fn str_u8(&mut self, max: usize) -> Result<&'a str, WireError> {
        let len = usize::from(self.u8()?);
        if len > max {
            return Err(WireError::TooLong { len, max });
        }
        std::str::from_utf8(self.take(len)?).map_err(|_| WireError::Utf8)
    }

    /// Fails when bytes are left over.
    pub fn finish(&self) -> Result<(), WireError> {
        match self.remaining() {
            0 => Ok(()),
            n => Err(WireError::Trailing(n)),
        }
    }
}

const FNV64_OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
const FNV64_PRIME: u64 = 0x0000_0100_0000_01b3;

/// FNV-1a 64-bit hash (public-domain algorithm by Fowler, Noll and Vo). Used for stable protocol
/// ids, never for anything that must resist an attacker.
pub fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = FNV64_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV64_PRIME);
    }
    hash
}

/// Compares two byte slices in time that depends only on their lengths.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_every_field_kind() {
        let mut w = WireWriter::new();
        w.u8(7);
        w.u16(0xBEEF);
        w.u32(0xDEAD_BEEF);
        w.u64(u64::MAX - 3);
        w.i32(-42);
        w.f32(1.5);
        w.bytes_u16(b"abc", 8).expect("fits");
        w.str_u8("name", 16).expect("fits");
        let bytes = w.finish();
        let mut r = WireReader::new(&bytes);
        assert_eq!(r.u8(), Ok(7));
        assert_eq!(r.u16(), Ok(0xBEEF));
        assert_eq!(r.u32(), Ok(0xDEAD_BEEF));
        assert_eq!(r.u64(), Ok(u64::MAX - 3));
        assert_eq!(r.i32(), Ok(-42));
        assert_eq!(r.f32(), Ok(1.5));
        assert_eq!(r.bytes_u16(8), Ok(&b"abc"[..]));
        assert_eq!(r.str_u8(16), Ok("name"));
        assert_eq!(r.finish(), Ok(()));
    }

    #[test]
    fn truncation_and_hostile_lengths_are_errors() {
        let mut r = WireReader::new(&[1, 2]);
        assert!(matches!(r.u32(), Err(WireError::Truncated { .. })));
        let mut r = WireReader::new(&[0xFF, 0xFF, 1]);
        assert!(matches!(r.bytes_u16(64), Err(WireError::TooLong { .. })));
        let mut w = WireWriter::new();
        assert!(w.str_u8("too long", 3).is_err());
        assert!(w.is_empty(), "a refused field writes nothing");
    }

    #[test]
    fn fnv_and_constant_time_eq() {
        assert_eq!(fnv1a_64(b""), FNV64_OFFSET_BASIS);
        assert_ne!(fnv1a_64(b"a"), fnv1a_64(b"b"));
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }
}
