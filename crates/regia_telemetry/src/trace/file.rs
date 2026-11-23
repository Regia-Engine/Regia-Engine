// SPDX-License-Identifier: MPL-2.0
//! The `.regiatrace` file (`docs/formats/regiatrace-v1.md`, spec 54 § 4.2): a header, then append-only chunks
//! `{ kind: u8, len: u32, payload, crc32: u32 }`, little-endian. A truncated file reads up to its
//! last complete chunk. The kind's high bit marks an LZ4 block payload (4-byte length prefix).

use std::io::Write;

use regia_framework::feedback::zip::crc32;

/// File magic.
pub const MAGIC: &[u8; 4] = b"RGTR";
/// Current format version.
pub const FORMAT_VERSION: u32 = 1;
/// Payloads above this are LZ4-compressed when that shrinks them.
pub const COMPRESS_ABOVE: usize = 1024;

/// The `regiatrace` layout fingerprint for `cargo xtask check-formats` (spec 54 § 4.4, ADR-9070):
/// the header fields, the chunk frame, every chunk kind and every event kind with its fields, as
/// `docs/formats/regiatrace-v1.md` lists them. Edit it with the layout; a change bumps
/// [`FORMAT_VERSION`] and keeps a decoder for every older version.
#[must_use]
pub fn regiatrace_schema_fingerprint() -> String {
    [
        "header:magic[4]=RGTR,format_version:u32,flags:u32,start_unix_ms:u64,gpu_clock_error_ns:u32,\
         engine_version:str,adapter:str,build_profile:str,platform:str",
        "chunk:kind:u8(high bit lz4 block),len:u32,payload[len],crc32:u32",
        "kind1 strings:(id varint,len varint,utf8)*",
        "kind2 lanes:(id varint,kind u8,n varint,name sid)*",
        "kind3 events:(kind u8,t_ns delta varint,lane varint,fields)*",
        "kind4 counters:(name sid,t_ns varint,f64)*",
        "kind5 passes:(tag varint,name sid)*",
        "kind6 screenshot:t_ns,frame,width,height,name sid or 0,file sid",
        "kind7 index:(second varint,file offset varint)*",
        "event1 mark:name sid,data sid or 0,frame",
        "event2 range_open:id,name sid,frame,depth,parent+1 or 0",
        "event3 range_close:id,frame,forced",
        "event4 frame_begin:frame",
        "event5 frame_end:frame",
        "event6 pass_open:tag,frame,submission",
        "event7 gpu_interval:tag,begin_ns,end_ns,frame",
        "event8 log:level,target sid,text sid",
        "event9 pass_close:tag,frame,submission",
    ]
    .join(";")
}

/// The versions a `regiatrace` migration step leaves: none. A trace is a diagnostic output and is
/// never rewritten; the reader keeps a decoder per version instead (spec 54 § 4.4).
#[must_use]
pub fn regiatrace_migration_sources() -> Vec<u32> {
    Vec::new()
}

const LZ4_BIT: u8 = 0x80;

/// Decompresses a size-prefixed LZ4 block, refusing a declared size no LZ4 stream of that length
/// can produce (an LZ4 block expands at most 255 times), so a hostile prefix cannot force a huge
/// allocation.
fn lz4_decompress_bounded(body: &[u8]) -> Option<Vec<u8>> {
    let (size, rest) = lz4_flex::block::uncompressed_size(body).ok()?;
    if size > rest.len().saturating_mul(255) {
        return None;
    }
    lz4_flex::block::decompress(rest, size).ok()
}

/// Chunk kinds.
pub mod chunk {
    /// Strings: `(id varint, len varint, utf8)*`, appended.
    pub const STRINGS: u8 = 1;
    /// Lanes: `(id varint, kind u8, n varint (worker index), name sid varint)*`.
    pub const LANES: u8 = 2;
    /// Events batch.
    pub const EVENTS: u8 = 3;
    /// Counters: `(name sid, t_ns varint, f64)*`.
    pub const COUNTERS: u8 = 4;
    /// Passes: `(tag u16 varint, name sid varint)*`.
    pub const PASSES: u8 = 5;
    /// Screenshot: `(t_ns, frame, w, h, name sid (0 none), file sid)`.
    pub const SCREENSHOT: u8 = 6;
    /// Index at close: `(second varint, file offset varint)*`.
    pub const INDEX: u8 = 7;
}

/// What a header says about the recording.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Header {
    /// [`FORMAT_VERSION`] of the file.
    pub format_version: u32,
    /// Reserved flags.
    pub flags: u32,
    /// Unix time at the start of the time base, ms.
    pub start_unix_ms: u64,
    /// Bound on the CPU/GPU clock conversion error, ns.
    pub gpu_clock_error_ns: u32,
    /// Engine version.
    pub engine_version: String,
    /// Adapter.
    pub adapter: String,
    /// Build profile.
    pub build_profile: String,
    /// Platform.
    pub platform: String,
}

/// Appends `v` as a LEB128 varint.
pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    loop {
        let byte = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// A cursor over a payload.
pub struct Cursor<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Cursor<'a> {
    /// A cursor at the start of `bytes`.
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// Whether everything was consumed.
    #[must_use]
    pub fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// One byte.
    pub fn u8(&mut self) -> Option<u8> {
        let b = *self.bytes.get(self.at)?;
        self.at += 1;
        Some(b)
    }

    /// A varint.
    pub fn varint(&mut self) -> Option<u64> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            if shift >= 64 {
                return None;
            }
            let payload = u64::from(b & 0x7F);
            if shift == 63 && payload > 1 {
                // The 10th byte has one payload bit of room in a u64; a real writer never emits
                // more and `<<` would silently shift the excess out, decoding a corrupt file as a
                // truncated number. Reject it so the chunk ends as a truncated read.
                return None;
            }
            v |= payload << shift;
            if b & 0x80 == 0 {
                return Some(v);
            }
            shift += 7;
        }
    }

    /// `n` bytes.
    pub fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.at.checked_add(n)?;
        let s = self.bytes.get(self.at..end)?;
        self.at = end;
        Some(s)
    }

    /// A little-endian `f64`.
    pub fn f64(&mut self) -> Option<f64> {
        let b = self.take(8)?;
        Some(f64::from_le_bytes(b.try_into().ok()?))
    }
}

fn put_str16(out: &mut Vec<u8>, s: &str) {
    let b = &s.as_bytes()[..s.len().min(usize::from(u16::MAX))];
    out.extend_from_slice(&u16::try_from(b.len()).unwrap_or(u16::MAX).to_le_bytes());
    out.extend_from_slice(b);
}

/// Encodes the file header.
#[must_use]
pub fn encode_header(h: &Header) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&FORMAT_VERSION.to_le_bytes());
    out.extend_from_slice(&h.flags.to_le_bytes());
    out.extend_from_slice(&h.start_unix_ms.to_le_bytes());
    out.extend_from_slice(&h.gpu_clock_error_ns.to_le_bytes());
    for s in [&h.engine_version, &h.adapter, &h.build_profile, &h.platform] {
        put_str16(&mut out, s);
    }
    out
}

/// Why a header or chunk stream could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceReadError {
    /// The file could not be read.
    Io(String),
    /// Not a trace file.
    BadMagic,
    /// A format version this build has no decoder for.
    UnsupportedVersion(u32),
    /// The header is cut short.
    ShortHeader,
}

impl std::fmt::Display for TraceReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "cannot read the trace: {e}"),
            Self::BadMagic => f.write_str("not a regiatrace file (bad magic)"),
            Self::UnsupportedVersion(v) => write!(
                f,
                "regiatrace format_version {v} is newer than this build reads (it reads up to {FORMAT_VERSION}); \
                 update the engine or export with a newer regia"
            ),
            Self::ShortHeader => f.write_str("the regiatrace header is cut short"),
        }
    }
}

impl std::error::Error for TraceReadError {}

/// Decodes a header; returns it and the offset of the first chunk.
pub fn decode_header(bytes: &[u8]) -> Result<(Header, usize), TraceReadError> {
    if bytes.len() < 4 || &bytes[..4] != MAGIC {
        return Err(TraceReadError::BadMagic);
    }
    let mut c = Cursor::new(&bytes[4..]);
    let u32le = |c: &mut Cursor<'_>| -> Option<u32> { Some(u32::from_le_bytes(c.take(4)?.try_into().ok()?)) };
    let version = u32le(&mut c).ok_or(TraceReadError::ShortHeader)?;
    if version == 0 || version > FORMAT_VERSION {
        return Err(TraceReadError::UnsupportedVersion(version));
    }
    let flags = u32le(&mut c).ok_or(TraceReadError::ShortHeader)?;
    let start = c
        .take(8)
        .and_then(|b| b.try_into().ok())
        .map(u64::from_le_bytes)
        .ok_or(TraceReadError::ShortHeader)?;
    let err = u32le(&mut c).ok_or(TraceReadError::ShortHeader)?;
    let mut strs = Vec::new();
    for _ in 0..4 {
        let len = c
            .take(2)
            .and_then(|b| b.try_into().ok())
            .map(u16::from_le_bytes)
            .ok_or(TraceReadError::ShortHeader)?;
        let s = c.take(usize::from(len)).ok_or(TraceReadError::ShortHeader)?;
        strs.push(String::from_utf8_lossy(s).into_owned());
    }
    let mut it = strs.into_iter();
    let header = Header {
        format_version: version,
        flags,
        start_unix_ms: start,
        gpu_clock_error_ns: err,
        engine_version: it.next().unwrap_or_default(),
        adapter: it.next().unwrap_or_default(),
        build_profile: it.next().unwrap_or_default(),
        platform: it.next().unwrap_or_default(),
    };
    Ok((header, 4 + c.at))
}

/// Encodes one chunk (LZ4 when the payload is large and compresses).
#[must_use]
pub fn encode_chunk(kind: u8, payload: &[u8]) -> Vec<u8> {
    let (kind, body) = if payload.len() > COMPRESS_ABOVE {
        let packed = lz4_flex::block::compress_prepend_size(payload);
        if packed.len() < payload.len() {
            (kind | LZ4_BIT, packed)
        } else {
            (kind, payload.to_vec())
        }
    } else {
        (kind, payload.to_vec())
    };
    let mut out = Vec::with_capacity(body.len() + 9);
    out.push(kind);
    out.extend_from_slice(&u32::try_from(body.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(&body);
    out.extend_from_slice(&crc32(&body).to_le_bytes());
    out
}

/// One decoded chunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chunk {
    /// Kind without the compression bit.
    pub kind: u8,
    /// Decoded payload.
    pub payload: Vec<u8>,
    /// Offset of the chunk in the file.
    pub offset: usize,
}

/// Reads chunks from `bytes[from..]` up to the last complete, CRC-valid chunk. Returns them and
/// whether the stream ended early (truncated, corrupt or undecodable tail).
#[must_use]
pub fn read_chunks(bytes: &[u8], from: usize) -> (Vec<Chunk>, bool) {
    let mut chunks = Vec::new();
    let mut at = from;
    while at < bytes.len() {
        let Some(head) = bytes.get(at..at + 5) else {
            return (chunks, true);
        };
        let kind = head[0];
        let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]) as usize;
        let body_at = at + 5;
        let Some(body) = body_at.checked_add(len).and_then(|e| bytes.get(body_at..e)) else {
            return (chunks, true);
        };
        let Some(crc) = bytes.get(body_at + len..body_at + len + 4) else {
            return (chunks, true);
        };
        if u32::from_le_bytes([crc[0], crc[1], crc[2], crc[3]]) != crc32(body) {
            return (chunks, true);
        }
        let payload = if kind & LZ4_BIT != 0 {
            match lz4_decompress_bounded(body) {
                Some(p) => p,
                None => return (chunks, true),
            }
        } else {
            body.to_vec()
        };
        chunks.push(Chunk {
            kind: kind & !LZ4_BIT,
            payload,
            offset: at,
        });
        at = body_at + len + 4;
    }
    (chunks, false)
}

/// A sink that counts what it wrote.
pub struct CountingWriter<W: Write> {
    inner: W,
    /// Bytes written.
    pub bytes: u64,
}

impl<W: Write> CountingWriter<W> {
    /// Wraps `inner`.
    pub fn new(inner: W) -> Self {
        Self { inner, bytes: 0 }
    }
    /// Writes `data`.
    pub fn put(&mut self, data: &[u8]) -> std::io::Result<()> {
        self.inner.write_all(data)?;
        self.bytes += data.len() as u64;
        Ok(())
    }
    /// Flushes.
    pub fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod lz4_tests {
    use super::*;

    fn raw_chunk(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![kind];
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        out.extend_from_slice(&crc32(body).to_le_bytes());
        out
    }

    /// A CRC-valid LZ4 chunk declaring a 4 GiB payload is read as truncated without allocating it.
    #[test]
    fn hostile_lz4_size_prefix_is_truncated() {
        let bytes = raw_chunk(3 | LZ4_BIT, &[0xff, 0xff, 0xff, 0xff, 0x10, 0x00]);
        let (chunks, truncated) = read_chunks(&bytes, 0);
        assert!(chunks.is_empty());
        assert!(truncated);
    }

    /// A real compressed chunk still round-trips through the bounded decoder.
    #[test]
    fn lz4_chunk_round_trips() {
        let payload = vec![7u8; COMPRESS_ABOVE * 4];
        let bytes = encode_chunk(3, &payload);
        assert_ne!(bytes[0] & LZ4_BIT, 0);
        let (chunks, truncated) = read_chunks(&bytes, 0);
        assert!(!truncated);
        assert_eq!(chunks[0].payload, payload);
    }
}
