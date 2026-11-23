// SPDX-License-Identifier: MPL-2.0
//! Saving, resuming and sending a lockstep state (DC-3.7a).
//!
//! A snapshot is the bytes a sim needs to continue bit-for-bit from a tick boundary: a minority
//! seat resyncs from the majority's state (N224), a late joiner starts from it, a desync bundle or a
//! lockstep replay carries it, and a rollback restores it. The pieces:
//!
//! - [`DetWriter`] / [`DetReader`]: a canonical little-endian byte codec. Integers are fixed width,
//!   `bool` is one byte (`0` or `1`), lengths are `u32`, and floats are written as their **raw IEEE
//!   bits**: unlike the desync hash's canonical float rule ([`crate::hash::canonical_f32_bits`]), a
//!   restore must give back `-0.0` and every NaN payload exactly, because a later tick may read them.
//!   Every read is bounds-checked and returns a [`DetStateError`]; nothing in this module panics on
//!   input bytes. Both are also a serde `Serializer` / `Deserializer` pair ([`DetWriter::put`],
//!   [`DetReader::take`]), so a game can derive its state instead of writing the fields by hand. A
//!   hash map or hash set is written sorted by its entries' bytes, so the same entries give the same
//!   bytes in every process ([`DetWriter::put`] has the rule).
//! - [`DetState`]: a type that saves and loads itself, with its own 8-byte tag and version.
//! - [`snapshot`] / [`restore`]: a framed snapshot. The header carries a magic, the frame version,
//!   the state's tag and version, the tick, the body length and a [`Mix`] hash over all of them and
//!   the body, so a bit flipped in transit is refused (`bad_hash`) instead of desyncing later.
//! - [`SnapshotRing`]: checkpoints keyed by tick, kept every `every` ticks for `capacity_ticks`
//!   ticks and bounded in bytes, for rollback and resync.
//!
//! The engine implements [`DetState`] for [`crate::ids::DetIdAllocator`], [`crate::hash::SystemRing`],
//! [`crate::tick::TickRate`] and its own scenarios, and [`crate::harness::SimHarness`] saves and
//! resumes a whole run.
//!
//! **Version rule.** [`DetState::STATE_VERSION`] belongs to the game. A change to what `save` writes is
//! a sim-version change: bump the version, and old snapshots are refused with `bad_version` naming
//! both versions. There is no migrator, because a snapshot is not saved content: it lives as long as a
//! match, a desync bundle or a replay recorded by the same build.
//!
//! ```
//! use regia_detsim::state::{self, DetReader, DetState, DetStateError, DetWriter};
//!
//! #[derive(Debug, PartialEq)]
//! struct Ball {
//!     pos: [f32; 2],
//!     bounces: u32,
//! }
//!
//! impl DetState for Ball {
//!     const STATE_TAG: [u8; 8] = *b"ball\0\0\0\0";
//!     const STATE_VERSION: u32 = 1;
//!     fn save(&self, w: &mut DetWriter) {
//!         w.f32(self.pos[0]);
//!         w.f32(self.pos[1]);
//!         w.u32(self.bounces);
//!     }
//!     fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError> {
//!         Ok(Ball { pos: [r.f32()?, r.f32()?], bounces: r.u32()? })
//!     }
//! }
//!
//! let ball = Ball { pos: [-0.0, 2.5], bounces: 3 };
//! let bytes = state::snapshot(&ball, 120).unwrap();
//! let (tick, back): (u64, Ball) = state::restore(&bytes).unwrap();
//! assert_eq!((tick, &back), (120, &ball));
//! assert_eq!(back.pos[0].to_bits(), (-0.0f32).to_bits()); // raw bits, not the hash's rule
//! ```

use std::collections::VecDeque;
use std::fmt;

use serde::de::{self, DeserializeOwned, DeserializeSeed, IntoDeserializer, Visitor};
use serde::ser::{self, Serialize};

use crate::hash::Mix;

#[cfg(test)]
mod tests;

/// The first four bytes of every framed snapshot.
pub const SNAPSHOT_MAGIC: [u8; 4] = *b"RGDS";

/// Version of the frame layout (the header below, not the state inside it). The engine's to bump.
pub const FRAME_VERSION: u16 = 1;

/// Bytes in a frame header: magic 4, frame version 2, kind 2, tag 8, state version 4, tick 8,
/// body length 4, body hash 8.
pub const HEADER_LEN: usize = 40;

/// Largest number of bytes one sequence preallocates before its elements are actually read. A
/// length prefix is input, so it never sizes an allocation by itself; past this the vector grows
/// only as decoded elements arrive.
pub const PREALLOC_BYTES: usize = 4096;

/// Longest detail text a [`DetStateError::BadValue`] keeps.
const MAX_DETAIL: usize = 160;

/// Reader-side limits. Both are settings: a lobby that sends large states raises them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DetReadLimits {
    /// Largest input, in bytes, a reader accepts (frame included). Larger input is `length_limit`.
    pub max_bytes: usize,
    /// Largest element count any single length prefix may carry. Larger is `length_limit`.
    pub max_len: u32,
}

impl DetReadLimits {
    /// 64 MiB of input and 16 Mi elements per sequence.
    pub const DEFAULT: DetReadLimits = DetReadLimits {
        max_bytes: 64 << 20,
        max_len: 1 << 24,
    };
}

impl Default for DetReadLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Why a state could not be read (or, for `length_limit` and `bad_value`, written).
///
/// Each variant has a stable lowercase code ([`DetStateError::code`]) for agent reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetStateError {
    /// The input ended before a value: `needed` bytes at byte `offset`. Code `truncated`.
    Truncated {
        /// Byte offset of the read that failed.
        offset: usize,
        /// Bytes that read needed.
        needed: usize,
    },
    /// A length or the whole input is over a [`DetReadLimits`] limit (or a written length does not
    /// fit a `u32`, or a ring entry is over the ring's byte budget). Code `length_limit`.
    LengthLimit {
        /// The length found.
        len: u64,
        /// The limit it broke.
        limit: u64,
    },
    /// The frame or the state has another version than this build reads. Code `bad_version`.
    BadVersion {
        /// The version in the bytes.
        found: u32,
        /// The version this build reads.
        expected: u32,
    },
    /// The bytes are not a snapshot (wrong magic) or not a snapshot of this type (wrong tag).
    /// Code `bad_tag`.
    BadTag {
        /// The tag in the bytes (the magic, zero-padded, when the magic is wrong).
        found: [u8; 8],
        /// The tag this type has.
        expected: [u8; 8],
    },
    /// Bytes were left over after the value (or the frame's body). Code `trailing_bytes`.
    TrailingBytes {
        /// Bytes left over.
        left: usize,
    },
    /// A value is out of its domain (a `bool` byte of 2, an id past the cap, a broken invariant), or
    /// serde reported an error. Code `bad_value`.
    BadValue {
        /// What was wrong (at most 160 bytes).
        detail: String,
    },
    /// The frame's body hash does not match its bytes: the snapshot was damaged. Code `bad_hash`.
    BadHash {
        /// The hash in the header.
        found: u64,
        /// The hash of the bytes received.
        expected: u64,
    },
}

impl DetStateError {
    /// A [`DetStateError::BadValue`] with `detail` (cut to 160 bytes), for [`DetState::load`]
    /// implementations that check their invariants.
    pub fn bad_value(detail: impl fmt::Display) -> Self {
        let mut detail = detail.to_string();
        if detail.len() > MAX_DETAIL {
            let mut cut = MAX_DETAIL;
            while !detail.is_char_boundary(cut) {
                cut -= 1;
            }
            detail.truncate(cut);
        }
        DetStateError::BadValue { detail }
    }

    /// Stable lowercase ASCII code: `truncated`, `length_limit`, `bad_version`, `bad_tag`,
    /// `trailing_bytes`, `bad_value` or `bad_hash`.
    pub const fn code(&self) -> &'static str {
        match self {
            DetStateError::Truncated { .. } => "truncated",
            DetStateError::LengthLimit { .. } => "length_limit",
            DetStateError::BadVersion { .. } => "bad_version",
            DetStateError::BadTag { .. } => "bad_tag",
            DetStateError::TrailingBytes { .. } => "trailing_bytes",
            DetStateError::BadValue { .. } => "bad_value",
            DetStateError::BadHash { .. } => "bad_hash",
        }
    }
}

/// Printable form of an 8-byte tag: ASCII graphics kept, every other byte as `\xNN`.
fn tag_text(tag: &[u8; 8]) -> String {
    let mut out = String::new();
    for &b in tag {
        if b.is_ascii_graphic() {
            out.push(char::from(b));
        } else if b != 0 {
            out.push_str(&format!("\\x{b:02x}"));
        }
    }
    out
}

impl fmt::Display for DetStateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DetStateError::Truncated { offset, needed } => {
                write!(f, "truncated: needed {needed} bytes at byte {offset}")
            }
            DetStateError::LengthLimit { len, limit } => {
                write!(f, "length_limit: length {len} is over the limit {limit}")
            }
            DetStateError::BadVersion { found, expected } => write!(
                f,
                "bad_version: the snapshot has version {found}, this build reads version {expected}"
            ),
            DetStateError::BadTag { found, expected } => write!(
                f,
                "bad_tag: the snapshot is tagged \"{}\", expected \"{}\"",
                tag_text(found),
                tag_text(expected)
            ),
            DetStateError::TrailingBytes { left } => {
                write!(f, "trailing_bytes: {left} bytes left after the value")
            }
            DetStateError::BadValue { detail } => write!(f, "bad_value: {detail}"),
            DetStateError::BadHash { found, expected } => write!(
                f,
                "bad_hash: the header says {found:016x}, the bytes hash to {expected:016x}"
            ),
        }
    }
}

impl std::error::Error for DetStateError {}

impl ser::Error for DetStateError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DetStateError::bad_value(msg)
    }
}

impl de::Error for DetStateError {
    fn custom<T: fmt::Display>(msg: T) -> Self {
        DetStateError::bad_value(msg)
    }
}

/// A type whose whole simulation state saves to bytes and loads back bit-identical.
///
/// Rules: `save` must not change the state (it takes `&self`) and must write every field a later
/// tick reads, in a fixed order (iterate ordered containers, never a hash map); `load` must give back
/// a value that steps to the same bits as the one saved (same orders, raw float bits; vector
/// capacities do not matter), and must check the invariants its later code relies on (an index in
/// range, two lengths that match), returning [`DetStateError::bad_value`] instead of panicking later.
pub trait DetState: Sized {
    /// Eight bytes naming the type in a snapshot's header. A snapshot of another type is `bad_tag`.
    const STATE_TAG: [u8; 8];
    /// The layout version of what `save` writes. Bumping it is a sim-version change; snapshots of
    /// another version are refused with `bad_version`.
    const STATE_VERSION: u32;
    /// Write the state.
    fn save(&self, w: &mut DetWriter);
    /// Read a state written by `save` of the same version.
    fn load(r: &mut DetReader<'_>) -> Result<Self, DetStateError>;
}

/// Canonical little-endian writer. Writing never fails on the spot: the first error (a length over
/// `u32::MAX`, a serde error) is kept and returned by [`DetWriter::into_bytes`].
#[derive(Debug, Default, Clone)]
pub struct DetWriter {
    buf: Vec<u8>,
    error: Option<DetStateError>,
}

impl DetWriter {
    /// An empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Bytes written so far.
    pub fn written(&self) -> usize {
        self.buf.len()
    }

    /// The bytes, or the first error a write hit.
    pub fn into_bytes(self) -> Result<Vec<u8>, DetStateError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(self.buf),
        }
    }

    /// Keep `error` if it is the first.
    pub fn fail(&mut self, error: DetStateError) {
        if self.error.is_none() {
            self.error = Some(error);
        }
    }

    /// One byte.
    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }
    /// Two bytes, little-endian.
    pub fn u16(&mut self, v: u16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Four bytes, little-endian.
    pub fn u32(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Eight bytes, little-endian.
    pub fn u64(&mut self, v: u64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Sixteen bytes, little-endian.
    pub fn u128(&mut self, v: u128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// One byte, two's complement.
    pub fn i8(&mut self, v: i8) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Two bytes, two's complement little-endian.
    pub fn i16(&mut self, v: i16) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Four bytes, two's complement little-endian.
    pub fn i32(&mut self, v: i32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Eight bytes, two's complement little-endian.
    pub fn i64(&mut self, v: i64) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// Sixteen bytes, two's complement little-endian.
    pub fn i128(&mut self, v: i128) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }
    /// One byte: `0` or `1`.
    pub fn bool(&mut self, v: bool) {
        self.buf.push(u8::from(v));
    }
    /// The raw IEEE bits (sign of zero and NaN payload kept), four bytes.
    pub fn f32(&mut self, v: f32) {
        self.u32(v.to_bits());
    }
    /// The raw IEEE bits (sign of zero and NaN payload kept), eight bytes.
    pub fn f64(&mut self, v: f64) {
        self.u64(v.to_bits());
    }
    /// A length prefix as a `u32`. Over `u32::MAX` keeps a `length_limit` error.
    pub fn length(&mut self, len: usize) {
        match u32::try_from(len) {
            Ok(n) => self.u32(n),
            Err(_) => {
                self.fail(DetStateError::LengthLimit {
                    len: len as u64,
                    limit: u64::from(u32::MAX),
                });
                self.u32(u32::MAX);
            }
        }
    }
    /// Bytes as they are, with no length (a fixed-size field).
    pub fn raw(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }
    /// A length, then the bytes.
    pub fn bytes(&mut self, bytes: &[u8]) {
        self.length(bytes.len());
        self.buf.extend_from_slice(bytes);
    }
    /// A length, then the UTF-8 bytes.
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }
    /// A length, then `bits` packed eight to a byte, lowest bit first; padding bits are zero.
    pub fn bits(&mut self, bits: &[bool]) {
        self.length(bits.len());
        for chunk in bits.chunks(8) {
            let mut byte = 0u8;
            for (i, &b) in chunk.iter().enumerate() {
                byte |= u8::from(b) << i;
            }
            self.buf.push(byte);
        }
    }
    /// A length, then each item through `each`.
    pub fn seq<T>(&mut self, items: &[T], mut each: impl FnMut(&mut Self, &T)) {
        self.length(items.len());
        for item in items {
            each(self, item);
        }
    }
    /// A nested [`DetState`]'s body (no header).
    pub fn state<S: DetState>(&mut self, state: &S) {
        state.save(self);
    }
    /// Any `Serialize` value in this codec (field values only, in declaration order, no names).
    ///
    /// The codec is not self-describing: sequences and maps need a known length, and
    /// `skip_serializing_if` is refused (`bad_value`), because the reader could not know a field was
    /// left out.
    ///
    /// **Order.** A hash map or hash set (std's, hashbrown's, or a type named like them: the outer
    /// type's name contains `Hash` and ends in `Map` or `Set`) is written in ascending order of each
    /// entry's encoded bytes (the key's bytes, then the value's), not in its iteration order, so equal
    /// contents give equal bytes whatever the hasher's seed. Byte order rather than the key's `Ord`:
    /// the serializer sees only `Serialize`, a hash map's key need not be `Ord`, and the codec's
    /// little-endian bytes are the same on every host. Every other sequence or map keeps its own order
    /// (a `Vec`'s, a `BTreeMap`'s key order, an `IndexMap`'s insertion order), because that order is
    /// part of its value. A custom `Serialize` that walks a hash container's iterator itself (for
    /// example `collect_seq(map.keys())`) is not recognized: collect it into a sorted `Vec` first.
    pub fn put<T: Serialize + ?Sized>(&mut self, value: &T) {
        if let Err(error) = value.serialize(&mut *self) {
            self.fail(error);
        }
    }
}

/// Bounds-checked little-endian reader over a byte slice. See the [module docs](self).
#[derive(Debug, Clone)]
pub struct DetReader<'a> {
    buf: &'a [u8],
    pos: usize,
    limits: DetReadLimits,
}

impl<'a> DetReader<'a> {
    /// A reader over `bytes`. Input longer than `limits.max_bytes` is `length_limit`.
    pub fn new(bytes: &'a [u8], limits: DetReadLimits) -> Result<Self, DetStateError> {
        if bytes.len() > limits.max_bytes {
            return Err(DetStateError::LengthLimit {
                len: bytes.len() as u64,
                limit: limits.max_bytes as u64,
            });
        }
        Ok(Self {
            buf: bytes,
            pos: 0,
            limits,
        })
    }

    /// The limits this reader enforces.
    pub const fn limits(&self) -> DetReadLimits {
        self.limits
    }

    /// Bytes read so far.
    pub const fn position(&self) -> usize {
        self.pos
    }

    /// Bytes not read yet.
    pub const fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    /// `Ok` when every byte was read, else `trailing_bytes`.
    pub const fn finish(&self) -> Result<(), DetStateError> {
        match self.remaining() {
            0 => Ok(()),
            left => Err(DetStateError::TrailingBytes { left }),
        }
    }

    /// The next `n` bytes, as they are.
    pub fn raw(&mut self, n: usize) -> Result<&'a [u8], DetStateError> {
        if n > self.remaining() {
            return Err(DetStateError::Truncated {
                offset: self.pos,
                needed: n,
            });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    /// The next `N` bytes as an array.
    pub fn array<const N: usize>(&mut self) -> Result<[u8; N], DetStateError> {
        let mut out = [0u8; N];
        out.copy_from_slice(self.raw(N)?);
        Ok(out)
    }

    /// One byte.
    pub fn u8(&mut self) -> Result<u8, DetStateError> {
        Ok(self.array::<1>()?[0])
    }
    /// Two bytes, little-endian.
    pub fn u16(&mut self) -> Result<u16, DetStateError> {
        Ok(u16::from_le_bytes(self.array()?))
    }
    /// Four bytes, little-endian.
    pub fn u32(&mut self) -> Result<u32, DetStateError> {
        Ok(u32::from_le_bytes(self.array()?))
    }
    /// Eight bytes, little-endian.
    pub fn u64(&mut self) -> Result<u64, DetStateError> {
        Ok(u64::from_le_bytes(self.array()?))
    }
    /// Sixteen bytes, little-endian.
    pub fn u128(&mut self) -> Result<u128, DetStateError> {
        Ok(u128::from_le_bytes(self.array()?))
    }
    /// One byte, two's complement.
    pub fn i8(&mut self) -> Result<i8, DetStateError> {
        Ok(i8::from_le_bytes(self.array()?))
    }
    /// Two bytes, two's complement little-endian.
    pub fn i16(&mut self) -> Result<i16, DetStateError> {
        Ok(i16::from_le_bytes(self.array()?))
    }
    /// Four bytes, two's complement little-endian.
    pub fn i32(&mut self) -> Result<i32, DetStateError> {
        Ok(i32::from_le_bytes(self.array()?))
    }
    /// Eight bytes, two's complement little-endian.
    pub fn i64(&mut self) -> Result<i64, DetStateError> {
        Ok(i64::from_le_bytes(self.array()?))
    }
    /// Sixteen bytes, two's complement little-endian.
    pub fn i128(&mut self) -> Result<i128, DetStateError> {
        Ok(i128::from_le_bytes(self.array()?))
    }
    /// One byte that must be `0` or `1` (else `bad_value`).
    pub fn bool(&mut self) -> Result<bool, DetStateError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            other => Err(DetStateError::bad_value(format_args!(
                "bool byte {other} at byte {}",
                self.pos - 1
            ))),
        }
    }
    /// Raw IEEE bits, four bytes.
    pub fn f32(&mut self) -> Result<f32, DetStateError> {
        Ok(f32::from_bits(self.u32()?))
    }
    /// Raw IEEE bits, eight bytes.
    pub fn f64(&mut self) -> Result<f64, DetStateError> {
        Ok(f64::from_bits(self.u64()?))
    }
    /// A `u32` length prefix, at most [`DetReadLimits::max_len`] (else `length_limit`).
    pub fn length(&mut self) -> Result<usize, DetStateError> {
        let n = self.u32()?;
        if n > self.limits.max_len {
            return Err(DetStateError::LengthLimit {
                len: u64::from(n),
                limit: u64::from(self.limits.max_len),
            });
        }
        Ok(n as usize)
    }
    /// A length, then that many bytes (borrowed from the input).
    pub fn bytes(&mut self) -> Result<&'a [u8], DetStateError> {
        let n = self.length()?;
        self.raw(n)
    }
    /// A length, then UTF-8 bytes (else `bad_value`).
    pub fn str(&mut self) -> Result<&'a str, DetStateError> {
        let bytes = self.bytes()?;
        std::str::from_utf8(bytes).map_err(|_| DetStateError::bad_value("a string is not UTF-8"))
    }
    /// Bits written by [`DetWriter::bits`]; nonzero padding bits are `bad_value`.
    pub fn bits(&mut self) -> Result<Vec<bool>, DetStateError> {
        let n = self.length()?;
        let packed = self.raw(n.div_ceil(8))?;
        let mut out = Vec::with_capacity(n);
        for (i, &byte) in packed.iter().enumerate() {
            let here = (n - i * 8).min(8);
            if here < 8 && byte >> here != 0 {
                return Err(DetStateError::bad_value("nonzero padding bits"));
            }
            for bit in 0..here {
                out.push((byte >> bit) & 1 == 1);
            }
        }
        Ok(out)
    }
    /// A length, then that many items through `each`. Preallocates at most [`PREALLOC_BYTES`] and
    /// never more items than bytes remain, so a forged length cannot allocate by itself.
    pub fn seq<T>(
        &mut self,
        mut each: impl FnMut(&mut Self) -> Result<T, DetStateError>,
    ) -> Result<Vec<T>, DetStateError> {
        let n = self.length()?;
        let cap = n
            .min(self.remaining())
            .min(PREALLOC_BYTES / std::mem::size_of::<T>().max(1));
        let mut out = Vec::with_capacity(cap);
        for _ in 0..n {
            out.push(each(self)?);
        }
        Ok(out)
    }
    /// A nested [`DetState`]'s body (no header).
    pub fn state<S: DetState>(&mut self) -> Result<S, DetStateError> {
        S::load(self)
    }
    /// A value written by [`DetWriter::put`].
    pub fn take<T: DeserializeOwned>(&mut self) -> Result<T, DetStateError> {
        T::deserialize(&mut *self)
    }
}

/// Which envelope a frame holds. A frame of another kind is refused with `bad_value`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotKind {
    /// A [`DetState`] body, from [`snapshot`].
    State,
    /// A [`crate::harness::SimHarness`] envelope (run, tick, then the scenario's body).
    Harness,
    /// A [`crate::replay::InputLog`]: per-tick inputs and recorded hashes (DC-3.7b).
    InputLog,
}

impl SnapshotKind {
    const fn word(self) -> u16 {
        match self {
            SnapshotKind::State => 0,
            SnapshotKind::Harness => 1,
            SnapshotKind::InputLog => 2,
        }
    }

    fn from_word(word: u16) -> Result<Self, DetStateError> {
        match word {
            0 => Ok(SnapshotKind::State),
            1 => Ok(SnapshotKind::Harness),
            2 => Ok(SnapshotKind::InputLog),
            other => Err(DetStateError::bad_value(format_args!("snapshot kind {other}"))),
        }
    }
}

/// A frame header, read and checked by [`read_header`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotHeader {
    /// What the body holds.
    pub kind: SnapshotKind,
    /// The state's [`DetState::STATE_TAG`].
    pub tag: [u8; 8],
    /// The state's [`DetState::STATE_VERSION`].
    pub version: u32,
    /// The tick the state was taken at (a boundary: that many ticks had run).
    pub tick: u64,
    /// Body bytes after the header.
    pub body_len: u32,
    /// The [`Mix`] hash of the header fields and the body.
    pub body_hash: u64,
}

/// The integrity hash: a [`Mix`] keyed by the tag, over kind, version, tick, then the body.
fn frame_hash(kind: SnapshotKind, tag: [u8; 8], version: u32, tick: u64, body: &[u8]) -> u64 {
    let mut mix = Mix::new(u64::from_le_bytes(tag));
    mix.u64(u64::from(kind.word()));
    mix.u64(u64::from(version));
    mix.u64(tick);
    mix.bytes(body);
    mix.finish()
}

/// Frame `body` (already written) with a header. Shared by [`snapshot`] and the harness.
pub(crate) fn frame(
    kind: SnapshotKind,
    tag: [u8; 8],
    version: u32,
    tick: u64,
    body: DetWriter,
) -> Result<Vec<u8>, DetStateError> {
    frame_bytes(kind, tag, version, tick, &body.into_bytes()?)
}

/// Frame a body written elsewhere (a host's own envelope, a relay that re-frames): the header for
/// `kind`, `tag`, `version` and `tick`, then `body`. A body over `u32::MAX` bytes is `length_limit`.
pub fn frame_bytes(
    kind: SnapshotKind,
    tag: [u8; 8],
    version: u32,
    tick: u64,
    body: &[u8],
) -> Result<Vec<u8>, DetStateError> {
    let body_len = u32::try_from(body.len()).map_err(|_| DetStateError::LengthLimit {
        len: body.len() as u64,
        limit: u64::from(u32::MAX),
    })?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&SNAPSHOT_MAGIC);
    out.extend_from_slice(&FRAME_VERSION.to_le_bytes());
    out.extend_from_slice(&kind.word().to_le_bytes());
    out.extend_from_slice(&tag);
    out.extend_from_slice(&version.to_le_bytes());
    out.extend_from_slice(&tick.to_le_bytes());
    out.extend_from_slice(&body_len.to_le_bytes());
    out.extend_from_slice(&frame_hash(kind, tag, version, tick, body).to_le_bytes());
    out.extend_from_slice(body);
    Ok(out)
}

/// Read and check a frame header: size limit, magic, frame version, kind, body length (truncated or
/// trailing) and the body hash. Does not check the tag or the state version; [`restore`] does.
pub fn read_header(bytes: &[u8], limits: DetReadLimits) -> Result<SnapshotHeader, DetStateError> {
    let mut r = DetReader::new(bytes, limits)?;
    let magic: [u8; 4] = r.array()?;
    if magic != SNAPSHOT_MAGIC {
        let mut found = [0u8; 8];
        found[..4].copy_from_slice(&magic);
        let mut expected = [0u8; 8];
        expected[..4].copy_from_slice(&SNAPSHOT_MAGIC);
        return Err(DetStateError::BadTag { found, expected });
    }
    let frame_version = r.u16()?;
    if frame_version != FRAME_VERSION {
        return Err(DetStateError::BadVersion {
            found: u32::from(frame_version),
            expected: u32::from(FRAME_VERSION),
        });
    }
    let kind = SnapshotKind::from_word(r.u16()?)?;
    let tag = r.array()?;
    let version = r.u32()?;
    let tick = r.u64()?;
    let body_len = r.u32()?;
    let body_hash = r.u64()?;
    let body = body_len as usize;
    if body > r.remaining() {
        return Err(DetStateError::Truncated {
            offset: r.position(),
            needed: body,
        });
    }
    if body < r.remaining() {
        return Err(DetStateError::TrailingBytes {
            left: r.remaining() - body,
        });
    }
    let actual = frame_hash(kind, tag, version, tick, r.raw(body)?);
    if actual != body_hash {
        return Err(DetStateError::BadHash {
            found: body_hash,
            expected: actual,
        });
    }
    Ok(SnapshotHeader {
        kind,
        tag,
        version,
        tick,
        body_len,
        body_hash,
    })
}

/// Check a frame of `kind` for type tag `tag` and version `version`; return its tick and a reader
/// over its body (with the same limits).
pub(crate) fn open_frame<'a>(
    bytes: &'a [u8],
    limits: DetReadLimits,
    kind: SnapshotKind,
    tag: [u8; 8],
    version: u32,
) -> Result<(u64, DetReader<'a>), DetStateError> {
    let header = read_header(bytes, limits)?;
    if header.kind != kind {
        return Err(DetStateError::bad_value(format_args!(
            "a {:?} snapshot, expected a {:?} snapshot",
            header.kind, kind
        )));
    }
    if header.tag != tag {
        return Err(DetStateError::BadTag {
            found: header.tag,
            expected: tag,
        });
    }
    if header.version != version {
        return Err(DetStateError::BadVersion {
            found: header.version,
            expected: version,
        });
    }
    let body = DetReader {
        buf: &bytes[HEADER_LEN..],
        pos: 0,
        limits,
    };
    Ok((header.tick, body))
}

/// Save `state`, taken at `tick`, as a framed snapshot. Never changes the state. Fails only when a
/// length does not fit a `u32` or a serde value refuses this codec.
pub fn snapshot<S: DetState>(state: &S, tick: u64) -> Result<Vec<u8>, DetStateError> {
    let _span = tracing::trace_span!("detsim_state_snapshot", tick).entered();
    let mut w = DetWriter::new();
    state.save(&mut w);
    frame(SnapshotKind::State, S::STATE_TAG, S::STATE_VERSION, tick, w)
}

/// [`restore_with`] under [`DetReadLimits::DEFAULT`].
pub fn restore<S: DetState>(bytes: &[u8]) -> Result<(u64, S), DetStateError> {
    restore_with(bytes, DetReadLimits::DEFAULT)
}

/// Load a snapshot written by [`snapshot`]: checks the frame ([`read_header`]), the tag, the version,
/// then loads the body and refuses bytes `load` left unread (`trailing_bytes`). Returns the tick.
pub fn restore_with<S: DetState>(bytes: &[u8], limits: DetReadLimits) -> Result<(u64, S), DetStateError> {
    let _span = tracing::trace_span!("detsim_state_restore", len = bytes.len()).entered();
    let (tick, mut body) = open_frame(bytes, limits, SnapshotKind::State, S::STATE_TAG, S::STATE_VERSION)?;
    let state = S::load(&mut body)?;
    body.finish()?;
    Ok((tick, state))
}

/// Body only (no frame): `state`'s [`DetState::save`] bytes. For a host that frames them itself.
pub fn to_bytes<S: DetState>(state: &S) -> Result<Vec<u8>, DetStateError> {
    let mut w = DetWriter::new();
    state.save(&mut w);
    w.into_bytes()
}

/// Body only (no frame): load `bytes` written by [`to_bytes`], refusing unread bytes.
pub fn from_bytes<S: DetState>(bytes: &[u8], limits: DetReadLimits) -> Result<S, DetStateError> {
    let mut r = DetReader::new(bytes, limits)?;
    let state = S::load(&mut r)?;
    r.finish()?;
    Ok(state)
}

/// Settings of a [`SnapshotRing`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotRingSettings {
    /// Ticks of history kept: a checkpoint older than `newest - capacity_ticks + 1` is dropped.
    /// At least 1.
    pub capacity_ticks: u64,
    /// Keep a checkpoint every `every` ticks (ticks that are multiples of it). At least 1.
    pub every: u64,
    /// Bytes the ring may hold in total; the oldest checkpoints drop to stay under it. A single
    /// checkpoint larger than this is refused with `length_limit`.
    pub max_bytes: usize,
}

impl Default for SnapshotRingSettings {
    /// 240 ticks (4 s at 60 Hz), a checkpoint every 10 ticks, 32 MiB.
    fn default() -> Self {
        SnapshotRingSettings {
            capacity_ticks: 240,
            every: 10,
            max_bytes: 32 << 20,
        }
    }
}

/// Checkpoints keyed by tick, for rollback and resync: bounded in ticks and in bytes, oldest first
/// out. A checkpoint at or below the newest one (a rewind) replaces the ones from there on.
#[derive(Debug, Clone)]
pub struct SnapshotRing {
    settings: SnapshotRingSettings,
    entries: VecDeque<(u64, Vec<u8>)>,
    bytes: usize,
}

impl SnapshotRing {
    /// An empty ring. `capacity_ticks` or `every` of 0 is `bad_value`.
    pub fn new(settings: SnapshotRingSettings) -> Result<Self, DetStateError> {
        if settings.capacity_ticks == 0 || settings.every == 0 {
            return Err(DetStateError::bad_value(
                "snapshot ring capacity_ticks and every must be at least 1",
            ));
        }
        Ok(SnapshotRing {
            settings,
            entries: VecDeque::new(),
            bytes: 0,
        })
    }

    /// The ring's settings.
    pub const fn settings(&self) -> SnapshotRingSettings {
        self.settings
    }

    /// Whether `tick` is on the checkpoint cadence.
    pub const fn due(&self, tick: u64) -> bool {
        tick.is_multiple_of(self.settings.every)
    }

    /// Keep `bytes` (a framed snapshot) as the checkpoint at `tick` if the tick is due; returns
    /// whether it was kept.
    pub fn push(&mut self, tick: u64, bytes: Vec<u8>) -> Result<bool, DetStateError> {
        if !self.due(tick) {
            return Ok(false);
        }
        if bytes.len() > self.settings.max_bytes {
            return Err(DetStateError::LengthLimit {
                len: bytes.len() as u64,
                limit: self.settings.max_bytes as u64,
            });
        }
        while let Some((last, _)) = self.entries.back() {
            if *last < tick {
                break;
            }
            self.pop_back();
        }
        self.bytes += bytes.len();
        self.entries.push_back((tick, bytes));
        let oldest_kept = tick.saturating_sub(self.settings.capacity_ticks - 1);
        while let Some((first, _)) = self.entries.front() {
            if *first >= oldest_kept && self.bytes <= self.settings.max_bytes {
                break;
            }
            self.pop_front();
        }
        Ok(true)
    }

    /// Snapshot `state` at `tick` and keep it, if the tick is due (the state is not saved
    /// otherwise). Returns whether it was kept.
    pub fn record<S: DetState>(&mut self, tick: u64, state: &S) -> Result<bool, DetStateError> {
        if !self.due(tick) {
            return Ok(false);
        }
        let bytes = snapshot(state, tick)?;
        self.push(tick, bytes)
    }

    fn pop_front(&mut self) {
        if let Some((_, bytes)) = self.entries.pop_front() {
            self.bytes -= bytes.len();
        }
    }

    fn pop_back(&mut self) {
        if let Some((_, bytes)) = self.entries.pop_back() {
            self.bytes -= bytes.len();
        }
    }

    /// The checkpoint taken at exactly `tick`.
    pub fn at(&self, tick: u64) -> Option<&[u8]> {
        let i = self.entries.partition_point(|(t, _)| *t < tick);
        match self.entries.get(i) {
            Some((t, bytes)) if *t == tick => Some(bytes),
            _ => None,
        }
    }

    /// The newest checkpoint at or before `tick`, with its tick (where a rollback to `tick` starts).
    pub fn latest_at_or_before(&self, tick: u64) -> Option<(u64, &[u8])> {
        let i = self.entries.partition_point(|(t, _)| *t <= tick);
        let (t, bytes) = self.entries.get(i.checked_sub(1)?)?;
        Some((*t, bytes))
    }

    /// Checkpoints held.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no checkpoint is held.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes held (the sum of the checkpoints' lengths; at most `max_bytes`).
    pub const fn bytes_used(&self) -> usize {
        self.bytes
    }

    /// Ticks of the checkpoints held, oldest first.
    pub fn ticks(&self) -> impl Iterator<Item = u64> + '_ {
        self.entries.iter().map(|(t, _)| *t)
    }

    /// Drop every checkpoint.
    pub fn clear(&mut self) {
        self.entries.clear();
        self.bytes = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// serde: `&mut DetWriter` is a Serializer, `&mut DetReader` a Deserializer.
// ---------------------------------------------------------------------------------------------

fn no_length() -> DetStateError {
    DetStateError::bad_value("a sequence or map without a known length")
}

/// `Some(n)` when the iterator knows its exact length (serde's own rule for `collect_seq`).
fn exact_len<I: Iterator>(iter: &I) -> Option<usize> {
    match iter.size_hint() {
        (lo, Some(hi)) if lo == hi => Some(lo),
        _ => None,
    }
}

/// Whether serde's `collect_map` / `collect_seq` argument `I` is a hash container, whose iteration
/// order follows its hasher (review det-core-review-c H1).
///
/// serde's impls pass `&Container` itself, so `std::any::type_name::<I>()` names the container: the
/// outer type's own name (generics and the module path removed) must contain `Hash` and end in `Map`
/// or `Set`. That covers std's `HashMap` / `HashSet`, hashbrown's and the wrappers named after them.
/// An ordered container (`BTreeMap`, `BTreeSet`, an insertion-ordered `IndexMap`, a `Vec`) keeps its
/// own order, because that order is part of its value. Only the outer type counts: a `Vec` of hash
/// maps is a `Vec`, and each map inside it sorts itself.
fn is_hash_container<I: ?Sized>() -> bool {
    hash_container_name(std::any::type_name::<I>())
}

/// [`is_hash_container`] on a type name (split out so the rule is unit-tested on plain strings).
fn hash_container_name(type_name: &str) -> bool {
    let mut outer = type_name.trim_start();
    loop {
        let next = outer
            .trim_start_matches('&')
            .trim_start_matches("mut ")
            .trim_start_matches("const ")
            .trim_start_matches('*');
        if next.len() == outer.len() {
            break;
        }
        outer = next;
    }
    let outer = outer.split('<').next().unwrap_or(outer);
    let own = outer.rsplit("::").next().unwrap_or(outer).trim();
    own.contains("Hash") && (own.ends_with("Map") || own.ends_with("Set"))
}

/// Write `entries` (`[start, split, end]` ranges of `scratch`: key bytes `start..split`, value bytes
/// `split..end`; a set element has `split == end`) as a `u32` count, then each entry's bytes, in
/// ascending order of its key bytes, then its value bytes. The order is a function of the entries
/// alone; entries whose bytes are equal write the same bytes in either order.
fn write_sorted(w: &mut DetWriter, scratch: &[u8], entries: &mut [[usize; 3]]) -> Result<(), DetStateError> {
    entries.sort_unstable_by(|a, b| {
        scratch[a[0]..a[1]]
            .cmp(&scratch[b[0]..b[1]])
            .then_with(|| scratch[a[1]..a[2]].cmp(&scratch[b[1]..b[2]]))
    });
    let n = u32::try_from(entries.len()).map_err(|_| DetStateError::LengthLimit {
        len: entries.len() as u64,
        limit: u64::from(u32::MAX),
    })?;
    w.u32(n);
    for e in entries.iter() {
        w.buf.extend_from_slice(&scratch[e[0]..e[2]]);
    }
    Ok(())
}

impl ser::Serializer for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn is_human_readable(&self) -> bool {
        false
    }
    fn serialize_bool(self, v: bool) -> Result<(), DetStateError> {
        DetWriter::bool(self, v);
        Ok(())
    }
    fn serialize_i8(self, v: i8) -> Result<(), DetStateError> {
        DetWriter::i8(self, v);
        Ok(())
    }
    fn serialize_i16(self, v: i16) -> Result<(), DetStateError> {
        DetWriter::i16(self, v);
        Ok(())
    }
    fn serialize_i32(self, v: i32) -> Result<(), DetStateError> {
        DetWriter::i32(self, v);
        Ok(())
    }
    fn serialize_i64(self, v: i64) -> Result<(), DetStateError> {
        DetWriter::i64(self, v);
        Ok(())
    }
    fn serialize_i128(self, v: i128) -> Result<(), DetStateError> {
        DetWriter::i128(self, v);
        Ok(())
    }
    fn serialize_u8(self, v: u8) -> Result<(), DetStateError> {
        DetWriter::u8(self, v);
        Ok(())
    }
    fn serialize_u16(self, v: u16) -> Result<(), DetStateError> {
        DetWriter::u16(self, v);
        Ok(())
    }
    fn serialize_u32(self, v: u32) -> Result<(), DetStateError> {
        DetWriter::u32(self, v);
        Ok(())
    }
    fn serialize_u64(self, v: u64) -> Result<(), DetStateError> {
        DetWriter::u64(self, v);
        Ok(())
    }
    fn serialize_u128(self, v: u128) -> Result<(), DetStateError> {
        DetWriter::u128(self, v);
        Ok(())
    }
    fn serialize_f32(self, v: f32) -> Result<(), DetStateError> {
        DetWriter::f32(self, v);
        Ok(())
    }
    fn serialize_f64(self, v: f64) -> Result<(), DetStateError> {
        DetWriter::f64(self, v);
        Ok(())
    }
    fn serialize_char(self, v: char) -> Result<(), DetStateError> {
        DetWriter::u32(self, u32::from(v));
        Ok(())
    }
    fn serialize_str(self, v: &str) -> Result<(), DetStateError> {
        DetWriter::str(self, v);
        Ok(())
    }
    fn serialize_bytes(self, v: &[u8]) -> Result<(), DetStateError> {
        DetWriter::bytes(self, v);
        Ok(())
    }
    fn serialize_none(self) -> Result<(), DetStateError> {
        DetWriter::u8(self, 0);
        Ok(())
    }
    fn serialize_some<T: Serialize + ?Sized>(self, value: &T) -> Result<(), DetStateError> {
        DetWriter::u8(self, 1);
        value.serialize(self)
    }
    fn serialize_unit(self) -> Result<(), DetStateError> {
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> Result<(), DetStateError> {
        Ok(())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
    ) -> Result<(), DetStateError> {
        DetWriter::u32(self, index);
        Ok(())
    }
    fn serialize_newtype_struct<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        value: &T,
    ) -> Result<(), DetStateError> {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: Serialize + ?Sized>(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        value: &T,
    ) -> Result<(), DetStateError> {
        DetWriter::u32(self, index);
        value.serialize(self)
    }
    fn serialize_seq(self, len: Option<usize>) -> Result<Self, DetStateError> {
        let len = len.ok_or_else(no_length)?;
        let n = u32::try_from(len).map_err(|_| DetStateError::LengthLimit {
            len: len as u64,
            limit: u64::from(u32::MAX),
        })?;
        DetWriter::u32(self, n);
        Ok(self)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self, DetStateError> {
        Ok(self)
    }
    fn serialize_tuple_struct(self, _name: &'static str, _len: usize) -> Result<Self, DetStateError> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self, DetStateError> {
        DetWriter::u32(self, index);
        Ok(self)
    }
    fn serialize_map(self, len: Option<usize>) -> Result<Self, DetStateError> {
        self.serialize_seq(len)
    }
    fn serialize_struct(self, _name: &'static str, _len: usize) -> Result<Self, DetStateError> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self, DetStateError> {
        DetWriter::u32(self, index);
        Ok(self)
    }

    // H1 (review det-core-review-c): a hash container's entries go out sorted by their encoded
    // bytes, so the snapshot is a function of the entries and not of the process's hasher seed.
    // Everything else keeps serde's default walk (`serialize_seq` / `serialize_map` in order).
    fn collect_seq<I>(self, iter: I) -> Result<(), DetStateError>
    where
        I: IntoIterator,
        <I as IntoIterator>::Item: Serialize,
    {
        let sort = is_hash_container::<I>();
        let mut iter = iter.into_iter();
        if !sort {
            let mut seq = ser::Serializer::serialize_seq(&mut *self, exact_len(&iter))?;
            iter.try_for_each(|item| ser::SerializeSeq::serialize_element(&mut seq, &item))?;
            return ser::SerializeSeq::end(seq);
        }
        let mut scratch = DetWriter::new();
        let mut entries = Vec::with_capacity(exact_len(&iter).unwrap_or(0));
        for item in iter {
            let start = scratch.buf.len();
            item.serialize(&mut scratch)?;
            let end = scratch.buf.len();
            entries.push([start, end, end]);
        }
        if let Some(error) = scratch.error {
            return Err(error);
        }
        write_sorted(self, &scratch.buf, &mut entries)
    }

    fn collect_map<K, V, I>(self, iter: I) -> Result<(), DetStateError>
    where
        K: Serialize,
        V: Serialize,
        I: IntoIterator<Item = (K, V)>,
    {
        let sort = is_hash_container::<I>();
        let mut iter = iter.into_iter();
        if !sort {
            let mut map = ser::Serializer::serialize_map(&mut *self, exact_len(&iter))?;
            iter.try_for_each(|(k, v)| ser::SerializeMap::serialize_entry(&mut map, &k, &v))?;
            return ser::SerializeMap::end(map);
        }
        let mut scratch = DetWriter::new();
        let mut entries = Vec::with_capacity(exact_len(&iter).unwrap_or(0));
        for (k, v) in iter {
            let start = scratch.buf.len();
            k.serialize(&mut scratch)?;
            let split = scratch.buf.len();
            v.serialize(&mut scratch)?;
            entries.push([start, split, scratch.buf.len()]);
        }
        if let Some(error) = scratch.error {
            return Err(error);
        }
        write_sorted(self, &scratch.buf, &mut entries)
    }
}

impl ser::SerializeSeq for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeTuple for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_element<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_field<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeMap for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_key<T: Serialize + ?Sized>(&mut self, key: &T) -> Result<(), DetStateError> {
        key.serialize(&mut **self)
    }
    fn serialize_value<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeStruct for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), DetStateError> {
        Err(DetStateError::bad_value(format_args!(
            "field `{key}` skipped: the det-state codec cannot skip fields"
        )))
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

impl ser::SerializeStructVariant for &mut DetWriter {
    type Ok = ();
    type Error = DetStateError;
    fn serialize_field<T: Serialize + ?Sized>(
        &mut self,
        _key: &'static str,
        value: &T,
    ) -> Result<(), DetStateError> {
        value.serialize(&mut **self)
    }
    fn skip_field(&mut self, key: &'static str) -> Result<(), DetStateError> {
        Err(DetStateError::bad_value(format_args!(
            "field `{key}` skipped: the det-state codec cannot skip fields"
        )))
    }
    fn end(self) -> Result<(), DetStateError> {
        Ok(())
    }
}

/// `left` more elements of a sequence, tuple, struct or map.
struct Counted<'r, 'de> {
    r: &'r mut DetReader<'de>,
    left: usize,
}

impl<'de> de::SeqAccess<'de> for Counted<'_, 'de> {
    type Error = DetStateError;
    fn next_element_seed<T: DeserializeSeed<'de>>(
        &mut self,
        seed: T,
    ) -> Result<Option<T::Value>, DetStateError> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        seed.deserialize(&mut *self.r).map(Some)
    }
    fn size_hint(&self) -> Option<usize> {
        // A hint sizes a preallocation: never more than the bytes left (serde caps it further).
        Some(self.left.min(self.r.remaining()))
    }
}

impl<'de> de::MapAccess<'de> for Counted<'_, 'de> {
    type Error = DetStateError;
    fn next_key_seed<K: DeserializeSeed<'de>>(&mut self, seed: K) -> Result<Option<K::Value>, DetStateError> {
        if self.left == 0 {
            return Ok(None);
        }
        self.left -= 1;
        seed.deserialize(&mut *self.r).map(Some)
    }
    fn next_value_seed<V: DeserializeSeed<'de>>(&mut self, seed: V) -> Result<V::Value, DetStateError> {
        seed.deserialize(&mut *self.r)
    }
    fn size_hint(&self) -> Option<usize> {
        Some(self.left.min(self.r.remaining()))
    }
}

impl<'de> de::EnumAccess<'de> for &mut DetReader<'de> {
    type Error = DetStateError;
    type Variant = Self;
    fn variant_seed<V: DeserializeSeed<'de>>(self, seed: V) -> Result<(V::Value, Self), DetStateError> {
        let index = DetReader::u32(self)?;
        let value = seed.deserialize(IntoDeserializer::<DetStateError>::into_deserializer(index))?;
        Ok((value, self))
    }
}

impl<'de> de::VariantAccess<'de> for &mut DetReader<'de> {
    type Error = DetStateError;
    fn unit_variant(self) -> Result<(), DetStateError> {
        Ok(())
    }
    fn newtype_variant_seed<T: DeserializeSeed<'de>>(self, seed: T) -> Result<T::Value, DetStateError> {
        seed.deserialize(self)
    }
    fn tuple_variant<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_seq(Counted { r: self, left: len })
    }
    fn struct_variant<V: Visitor<'de>>(
        self,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_seq(Counted {
            r: self,
            left: fields.len(),
        })
    }
}

impl<'de> de::Deserializer<'de> for &mut DetReader<'de> {
    type Error = DetStateError;

    fn is_human_readable(&self) -> bool {
        false
    }
    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DetStateError> {
        Err(DetStateError::bad_value(
            "the det-state codec is not self-describing (deserialize_any)",
        ))
    }
    fn deserialize_ignored_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, DetStateError> {
        Err(DetStateError::bad_value(
            "the det-state codec cannot skip a value (deserialize_ignored_any)",
        ))
    }
    fn deserialize_bool<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_bool(DetReader::bool(self)?)
    }
    fn deserialize_i8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_i8(DetReader::i8(self)?)
    }
    fn deserialize_i16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_i16(DetReader::i16(self)?)
    }
    fn deserialize_i32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_i32(DetReader::i32(self)?)
    }
    fn deserialize_i64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_i64(DetReader::i64(self)?)
    }
    fn deserialize_i128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_i128(DetReader::i128(self)?)
    }
    fn deserialize_u8<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u8(DetReader::u8(self)?)
    }
    fn deserialize_u16<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u16(DetReader::u16(self)?)
    }
    fn deserialize_u32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u32(DetReader::u32(self)?)
    }
    fn deserialize_u64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u64(DetReader::u64(self)?)
    }
    fn deserialize_u128<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u128(DetReader::u128(self)?)
    }
    fn deserialize_f32<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_f32(DetReader::f32(self)?)
    }
    fn deserialize_f64<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_f64(DetReader::f64(self)?)
    }
    fn deserialize_char<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        let code = DetReader::u32(self)?;
        let c =
            char::from_u32(code).ok_or_else(|| DetStateError::bad_value(format_args!("char {code:#x}")))?;
        visitor.visit_char(c)
    }
    fn deserialize_str<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_borrowed_str(DetReader::str(self)?)
    }
    fn deserialize_string<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_borrowed_str(DetReader::str(self)?)
    }
    fn deserialize_bytes<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_borrowed_bytes(DetReader::bytes(self)?)
    }
    fn deserialize_byte_buf<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_borrowed_bytes(DetReader::bytes(self)?)
    }
    fn deserialize_option<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        match DetReader::u8(self)? {
            0 => visitor.visit_none(),
            1 => visitor.visit_some(self),
            other => Err(DetStateError::bad_value(format_args!("option byte {other}"))),
        }
    }
    fn deserialize_unit<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_unit()
    }
    fn deserialize_unit_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_unit()
    }
    fn deserialize_newtype_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_newtype_struct(self)
    }
    fn deserialize_seq<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        let left = DetReader::length(self)?;
        visitor.visit_seq(Counted { r: self, left })
    }
    fn deserialize_tuple<V: Visitor<'de>>(self, len: usize, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_seq(Counted { r: self, left: len })
    }
    fn deserialize_tuple_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        len: usize,
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_seq(Counted { r: self, left: len })
    }
    fn deserialize_map<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        let left = DetReader::length(self)?;
        visitor.visit_map(Counted { r: self, left })
    }
    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_seq(Counted {
            r: self,
            left: fields.len(),
        })
    }
    fn deserialize_enum<V: Visitor<'de>>(
        self,
        _name: &'static str,
        _variants: &'static [&'static str],
        visitor: V,
    ) -> Result<V::Value, DetStateError> {
        visitor.visit_enum(self)
    }
    fn deserialize_identifier<V: Visitor<'de>>(self, visitor: V) -> Result<V::Value, DetStateError> {
        visitor.visit_u32(DetReader::u32(self)?)
    }
}
