// SPDX-License-Identifier: MPL-2.0
//! EBML primitives (IETF RFC 8794): variable-length integers, element headers and bounded reads, over
//! an in-memory slice ([`Cursor`]) and over a seekable reader ([`Source`]). Every length read from the
//! file is checked against the remaining bytes and [`MAX_ALLOC`] before anything is allocated.

use std::io::{Read, Seek, SeekFrom};

use crate::error::MediaError;

/// The largest single allocation the demuxer makes from a length field in the file (64 MiB).
pub const MAX_ALLOC: u64 = 64 * 1024 * 1024;

/// An element size of all ones: "unknown size" (live-written files).
pub const UNKNOWN: Option<u64> = None;

/// An element header: its ID (marker bits kept, as the Matroska tables write IDs) and data size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// Element ID with its length marker, for example `0x1A45_DFA3`.
    pub id: u32,
    /// Data size in bytes; `None` for an unknown size.
    pub size: Option<u64>,
    /// Absolute offset of the first data byte.
    pub data_start: u64,
}

/// Length in bytes of a variable-length integer from its first byte (1..=8), or `None` for `0x00`.
pub fn vint_len(first: u8) -> Option<usize> {
    if first == 0 {
        None
    } else {
        Some(first.leading_zeros() as usize + 1)
    }
}

/// Decodes an element ID from its bytes (marker kept). IDs are 1..=4 bytes.
fn id_from(bytes: &[u8]) -> u32 {
    bytes.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b))
}

/// Decodes a data size from its bytes (marker removed); all value bits set means unknown.
fn size_from(bytes: &[u8]) -> Option<u64> {
    let len = bytes.len();
    let mut value = u64::from(bytes[0]) & (0xFF >> len);
    for b in &bytes[1..] {
        value = (value << 8) | u64::from(*b);
    }
    let all_ones = (1u64 << (7 * len)) - 1;
    (value != all_ones).then_some(value)
}

/// Big-endian unsigned integer of 0..=8 bytes.
pub fn uint(bytes: &[u8]) -> Result<u64, MediaError> {
    if bytes.len() > 8 {
        return Err(MediaError::invalid("unsigned integer longer than 8 bytes"));
    }
    Ok(bytes.iter().fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

/// Big-endian signed integer of 0..=8 bytes.
pub fn int(bytes: &[u8]) -> Result<i64, MediaError> {
    if bytes.is_empty() {
        return Ok(0);
    }
    let u = uint(bytes)?;
    let shift = 64 - 8 * bytes.len() as u32;
    Ok(((u << shift) as i64) >> shift)
}

/// IEEE float of 0, 4 or 8 bytes.
pub fn float(bytes: &[u8]) -> Result<f64, MediaError> {
    match bytes.len() {
        0 => Ok(0.0),
        4 => Ok(f64::from(f32::from_bits(uint(bytes)? as u32))),
        8 => Ok(f64::from_bits(uint(bytes)?)),
        n => Err(MediaError::invalid(format!("float of {n} bytes"))),
    }
}

/// UTF-8 string with trailing NULs removed (lossy on bad bytes; strings are informational).
pub fn string(bytes: &[u8]) -> String {
    let end = bytes.iter().rposition(|b| *b != 0).map_or(0, |i| i + 1);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// A bounds-checked cursor over an in-memory element body.
#[derive(Debug, Clone)]
pub struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `bytes`.
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    /// Bytes left.
    pub fn remaining(&self) -> usize {
        self.bytes.len() - self.pos
    }

    /// Whether every byte was consumed.
    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    /// One byte.
    pub fn byte(&mut self) -> Result<u8, MediaError> {
        let b = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| MediaError::invalid("unexpected end of element"))?;
        self.pos += 1;
        Ok(b)
    }

    /// `n` bytes as a borrowed slice.
    pub fn take(&mut self, n: usize) -> Result<&'a [u8], MediaError> {
        if n > self.remaining() {
            return Err(MediaError::invalid("element runs past its parent"));
        }
        let s = &self.bytes[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn vint_bytes(&mut self, max: usize) -> Result<&'a [u8], MediaError> {
        let first = *self
            .bytes
            .get(self.pos)
            .ok_or_else(|| MediaError::invalid("unexpected end of element"))?;
        let len = vint_len(first)
            .filter(|l| *l <= max)
            .ok_or_else(|| MediaError::invalid("bad variable-length integer"))?;
        self.take(len)
    }

    /// A variable-length unsigned value (marker removed); `None` for the reserved all-ones value.
    pub fn vint(&mut self) -> Result<Option<u64>, MediaError> {
        Ok(size_from(self.vint_bytes(8)?))
    }

    /// A child element: ID and body. Unknown sizes are not allowed inside a buffered parent.
    pub fn element(&mut self) -> Result<(u32, &'a [u8]), MediaError> {
        let id = id_from(self.vint_bytes(4)?);
        let size = self
            .vint()?
            .ok_or_else(|| MediaError::invalid("unknown size inside a sized element"))?;
        let size = usize::try_from(size).map_err(|_| MediaError::invalid("element size overflows"))?;
        Ok((id, self.take(size)?))
    }
}

/// A seekable byte source with a known length: the file being demuxed.
#[derive(Debug)]
pub struct Source<R> {
    inner: R,
    len: u64,
    pos: u64,
}

impl<R: Read + Seek> Source<R> {
    /// Wraps `inner`, measuring its length.
    pub fn new(mut inner: R) -> Result<Self, MediaError> {
        let len = inner.seek(SeekFrom::End(0)).map_err(MediaError::io)?;
        inner.seek(SeekFrom::Start(0)).map_err(MediaError::io)?;
        Ok(Self { inner, len, pos: 0 })
    }

    /// Total length of the source.
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the source is empty.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Current absolute offset.
    pub fn pos(&self) -> u64 {
        self.pos
    }

    /// Bytes left after the current offset.
    pub fn remaining(&self) -> u64 {
        self.len.saturating_sub(self.pos)
    }

    /// Moves to an absolute offset (clamped to the length).
    pub fn seek_to(&mut self, pos: u64) -> Result<(), MediaError> {
        let pos = pos.min(self.len);
        self.inner.seek(SeekFrom::Start(pos)).map_err(MediaError::io)?;
        self.pos = pos;
        Ok(())
    }

    /// Skips `n` bytes (clamped to the end).
    pub fn skip(&mut self, n: u64) -> Result<(), MediaError> {
        self.seek_to(self.pos.saturating_add(n))
    }

    fn fill(&mut self, buf: &mut [u8]) -> Result<(), MediaError> {
        if buf.len() as u64 > self.remaining() {
            return Err(MediaError::invalid("truncated file"));
        }
        self.inner.read_exact(buf).map_err(MediaError::io)?;
        self.pos += buf.len() as u64;
        Ok(())
    }

    /// Reads `n` bytes into a new buffer. Refuses lengths past the end of the file or over [`MAX_ALLOC`].
    pub fn read_vec(&mut self, n: u64) -> Result<Vec<u8>, MediaError> {
        if n > MAX_ALLOC {
            return Err(MediaError::LimitExceeded(format!(
                "element of {n} bytes is over the 64 MiB allocation cap"
            )));
        }
        if n > self.remaining() {
            return Err(MediaError::invalid("element runs past the end of the file"));
        }
        let mut buf = vec![0u8; n as usize];
        self.fill(&mut buf)?;
        Ok(buf)
    }

    fn vint_bytes(&mut self, max: usize) -> Result<([u8; 8], usize), MediaError> {
        let mut out = [0u8; 8];
        self.fill(&mut out[..1])?;
        let len = vint_len(out[0])
            .filter(|l| *l <= max)
            .ok_or_else(|| MediaError::invalid("bad variable-length integer"))?;
        if len > 1 {
            self.fill(&mut out[1..len])?;
        }
        Ok((out, len))
    }

    /// Reads an element header at the current offset.
    pub fn header(&mut self) -> Result<Header, MediaError> {
        let (id_bytes, id_len) = self.vint_bytes(4)?;
        let (size_bytes, size_len) = self.vint_bytes(8)?;
        Ok(Header {
            id: id_from(&id_bytes[..id_len]),
            size: size_from(&size_bytes[..size_len]),
            data_start: self.pos,
        })
    }

    /// Scans forward from the current offset for the 4-byte ID `id`, leaving the source at its first
    /// byte. Reads in 64 KiB windows; returns `false` at the end of the file.
    pub fn scan_for(&mut self, id: u32) -> Result<bool, MediaError> {
        let needle = id.to_be_bytes();
        let mut window = vec![0u8; 64 * 1024];
        let mut start = self.pos;
        while start < self.len {
            let n = (self.len - start).min(window.len() as u64) as usize;
            self.seek_to(start)?;
            self.fill(&mut window[..n])?;
            if let Some(i) = window[..n].windows(4).position(|w| w == needle) {
                self.seek_to(start + i as u64)?;
                return Ok(true);
            }
            if n < 4 {
                break;
            }
            start += (n - 3) as u64;
        }
        self.seek_to(self.len)?;
        Ok(false)
    }
}

/// Appends an EBML element (ID with marker, 8-byte size, body). Used by the fixture muxer.
pub fn write_element(out: &mut Vec<u8>, id: u32, body: &[u8]) {
    let id_bytes = id.to_be_bytes();
    let skip = id_bytes.iter().position(|b| *b != 0).unwrap_or(3);
    out.extend_from_slice(&id_bytes[skip..]);
    write_size(out, body.len() as u64);
    out.extend_from_slice(body);
}

/// Appends an 8-byte EBML size.
pub fn write_size(out: &mut Vec<u8>, size: u64) {
    out.push(0x01);
    out.extend_from_slice(&size.to_be_bytes()[1..]);
}

/// Minimal big-endian bytes of an unsigned value (at least one byte).
pub fn uint_bytes(v: u64) -> Vec<u8> {
    let b = v.to_be_bytes();
    let skip = b.iter().position(|x| *x != 0).unwrap_or(7);
    b[skip..].to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_sizes_and_unknown() {
        assert_eq!(size_from(&[0x81]), Some(1));
        assert_eq!(size_from(&[0x40, 0x02]), Some(2));
        assert_eq!(size_from(&[0xFF]), None);
        assert_eq!(size_from(&[0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]), None);
        assert_eq!(vint_len(0x1A), Some(4));
        assert_eq!(vint_len(0), None);
    }

    #[test]
    fn signed_and_float() {
        assert_eq!(int(&[0xFF, 0xFE]).ok(), Some(-2));
        assert_eq!(int(&[0x7F]).ok(), Some(127));
        assert_eq!(float(&1.5f32.to_bits().to_be_bytes()).ok(), Some(1.5));
        assert!(float(&[1, 2, 3]).is_err());
    }

    #[test]
    fn written_element_round_trips() {
        let mut out = Vec::new();
        write_element(&mut out, 0x4286, &uint_bytes(1));
        let mut c = Cursor::new(&out);
        let (id, body) = c.element().expect("element");
        assert_eq!(id, 0x4286);
        assert_eq!(uint(body).ok(), Some(1));
        assert!(c.is_empty());
    }

    #[test]
    fn cursor_refuses_overlong_child() {
        // ID 0x86, size 100, but only 2 body bytes.
        let bytes = [0x86, 0xE4, 1, 2];
        assert!(Cursor::new(&bytes).element().is_err());
    }
}
