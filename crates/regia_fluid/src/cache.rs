// SPDX-License-Identifier: MPL-2.0
//! `.fluidcache` v1: a baked liquid run that replays without the solver (`docs/formats/fluidcache.md`).
//!
//! Little-endian binary. A header (magic `RFLC`, `format_version`, the header length, flags, frame
//! count, step, the quantization box) and then one record per frame: its byte length, a key/delta
//! tag, the step, the liquid positions quantized to 16 bits per axis over the domain, and an optional
//! whitewater stream (type tag, age, absolute position). Liquid positions of a delta frame are
//! zigzag LEB128 differences against the same particle index of the frame before (absolute when that
//! frame had fewer particles); every `keyframe_interval`-th frame is a key frame (differences against
//! zero), so [`FluidCache::play`] decodes at most one interval.
//!
//! Every count is checked against the bytes that remain before anything is allocated, so a truncated
//! or hostile file is refused with a [`CacheError`] and an allocation bounded by the input size.

use glam::Vec3;

use crate::frame::{FluidFrame, PointKind};
use crate::sim::FluidSim;

/// Current format version.
pub const FLUIDCACHE_FORMAT_VERSION: u32 = 1;

/// File magic.
pub const FLUIDCACHE_MAGIC: [u8; 4] = *b"RFLC";

/// Header bytes after the magic, the version and the header-length field (v1).
const HEADER_V1_LEN: u32 = 40;

/// Most frames one cache may hold.
pub const MAX_FRAMES: u32 = 100_000;

/// Most liquid or whitewater points of one frame.
pub const MAX_FRAME_POINTS: u32 = 4_000_000;

/// Flag: frames carry a whitewater stream.
pub const FLAG_WHITEWATER: u32 = 1;

/// The default key-frame interval.
pub const DEFAULT_KEYFRAME_INTERVAL: u32 = 30;

/// Why a cache could not be read or written. [`CacheError::code`] is the protocol code.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum CacheError {
    /// The data ended early.
    #[error("the fluid cache is truncated ({0})")]
    Truncated(&'static str),
    /// Not a fluid cache.
    #[error("not a fluid cache (bad magic)")]
    BadMagic,
    /// A version this build does not read.
    #[error("fluid cache version {0} is not supported (this build reads 1)")]
    BadVersion(u32),
    /// A count above its cap.
    #[error("the fluid cache claims too much: {0}")]
    TooLarge(String),
    /// Inconsistent data.
    #[error("the fluid cache is corrupt: {0}")]
    Corrupt(String),
    /// A frame index past the end.
    #[error("frame {index} is past the end ({count} frames)")]
    NoFrame {
        /// Requested.
        index: usize,
        /// Frames held.
        count: usize,
    },
}

impl CacheError {
    /// The lowercase protocol code.
    #[must_use]
    pub fn code(&self) -> &'static str {
        match self {
            Self::Truncated(_) => "fluidcache_truncated",
            Self::BadMagic => "fluidcache_bad_magic",
            Self::BadVersion(_) => "fluidcache_bad_version",
            Self::TooLarge(_) => "fluidcache_too_large",
            Self::Corrupt(_) => "fluidcache_corrupt",
            Self::NoFrame { .. } => "fluidcache_no_frame",
        }
    }
}

/// A recorded run.
#[derive(Debug, Clone, PartialEq)]
pub struct FluidCache {
    /// Flags ([`FLAG_WHITEWATER`]).
    pub flags: u32,
    /// The solver's fixed step (seconds).
    pub dt: f32,
    /// Quantization origin (the domain's minimum corner).
    pub origin: [f32; 3],
    /// Metres per quantization unit.
    pub quantum: f32,
    /// Every n-th frame is a key frame.
    pub keyframe_interval: u32,
    /// Encoded frame payloads.
    frames: Vec<Vec<u8>>,
    /// Most liquid points of a frame.
    max_points: u32,
    /// Most whitewater points of a frame.
    max_whitewater: u32,
    /// The quantized liquid of the last recorded frame (recording state; not stored).
    last: Vec<[u16; 3]>,
}

/// One decoded frame in quantized form.
#[derive(Debug, Clone, Default, PartialEq)]
struct Decoded {
    step: u64,
    liquid: Vec<[u16; 3]>,
    whitewater: Vec<(u8, u32, [u16; 3])>,
}

impl FluidCache {
    /// An empty cache for `sim`'s domain and step.
    #[must_use]
    pub fn for_sim(sim: &FluidSim, whitewater: bool) -> Self {
        let d = sim.dims();
        let extent = (d.max() - d.origin).max_element();
        Self {
            flags: if whitewater { FLAG_WHITEWATER } else { 0 },
            dt: sim.params().dt,
            origin: d.origin.to_array(),
            quantum: (extent / 65_535.0).max(f32::MIN_POSITIVE),
            keyframe_interval: DEFAULT_KEYFRAME_INTERVAL,
            frames: Vec::new(),
            max_points: 0,
            max_whitewater: 0,
            last: Vec::new(),
        }
    }

    /// Frames held.
    #[must_use]
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Whether frames carry whitewater.
    #[must_use]
    pub fn has_whitewater(&self) -> bool {
        self.flags & FLAG_WHITEWATER != 0
    }

    /// Encoded bytes of all frames.
    #[must_use]
    pub fn payload_bytes(&self) -> usize {
        self.frames.iter().map(Vec::len).sum()
    }

    fn quantize(&self, p: Vec3) -> [u16; 3] {
        let o = Vec3::from(self.origin);
        let q = ((p - o) / self.quantum).round();
        q.to_array().map(|v| {
            if v.is_finite() {
                v.clamp(0.0, 65_535.0) as u16
            } else {
                0
            }
        })
    }

    fn dequantize(&self, q: [u16; 3]) -> [f32; 3] {
        let o = self.origin;
        [0, 1, 2].map(|a| o[a] + f32::from(q[a]) * self.quantum)
    }

    /// Appends the current state of `sim` as a frame.
    pub fn record(&mut self, sim: &FluidSim) -> Result<(), CacheError> {
        let _span = tracing::info_span!(target: "regia_fluid", "fluidcache_record").entered();
        // 0 means "every frame is a key frame": normalize once so the in-memory value, the file
        // bytes and any `PartialEq` against a re-read cache all agree (the reader rejects 0).
        self.keyframe_interval = self.keyframe_interval.max(1);
        if self.frames.len() >= MAX_FRAMES as usize {
            return Err(CacheError::TooLarge(format!("more than {MAX_FRAMES} frames")));
        }
        let liquid: Vec<[u16; 3]> = sim.positions().iter().map(|p| self.quantize(*p)).collect();
        if liquid.len() > MAX_FRAME_POINTS as usize {
            return Err(CacheError::TooLarge(format!(
                "more than {MAX_FRAME_POINTS} points"
            )));
        }
        let key = self.frames.len().is_multiple_of(self.keyframe_interval as usize);
        let mut out = Vec::with_capacity(13 + liquid.len() * 3);
        out.push(u8::from(!key));
        out.extend_from_slice(&sim.steps().to_le_bytes());
        out.extend_from_slice(&(liquid.len() as u32).to_le_bytes());
        for (i, q) in liquid.iter().enumerate() {
            let base = if key { None } else { self.last.get(i) };
            for a in 0..3 {
                let prev = base.map_or(0, |b| i32::from(b[a]));
                write_varint(&mut out, zigzag(i32::from(q[a]) - prev));
            }
        }
        let mut ww_count = 0u32;
        if self.has_whitewater() {
            let ww = sim.whitewater().particles();
            if ww.len() > MAX_FRAME_POINTS as usize {
                return Err(CacheError::TooLarge(format!(
                    "more than {MAX_FRAME_POINTS} points"
                )));
            }
            ww_count = ww.len() as u32;
            out.extend_from_slice(&ww_count.to_le_bytes());
            for p in ww {
                out.push(p.kind as u8);
                write_varint(&mut out, age_centis(p.age));
                for v in self.quantize(p.position) {
                    out.extend_from_slice(&v.to_le_bytes());
                }
            }
        }
        self.max_points = self.max_points.max(liquid.len() as u32);
        self.max_whitewater = self.max_whitewater.max(ww_count);
        self.frames.push(out);
        self.last = liquid;
        Ok(())
    }

    /// The live frame as the cache stores it (positions and ages quantized), for comparing a replay
    /// with the run that recorded it.
    #[must_use]
    pub fn quantized_frame(&self, sim: &FluidSim) -> FluidFrame {
        let mut frame = FluidFrame {
            step: sim.steps(),
            ..FluidFrame::default()
        };
        for p in sim.positions() {
            frame.positions.push(self.dequantize(self.quantize(*p)));
            frame.kinds.push(PointKind::Liquid as u8);
            frame.ages.push(0.0);
        }
        if self.has_whitewater() {
            for p in sim.whitewater().particles() {
                frame.positions.push(self.dequantize(self.quantize(p.position)));
                frame.kinds.push(PointKind::from(p.kind) as u8);
                frame.ages.push(age_centis(p.age) as f32 / 100.0);
            }
        }
        frame
    }

    /// Frame `index`, decoded from the key frame before it. No solver runs.
    pub fn play(&self, index: usize) -> Result<FluidFrame, CacheError> {
        let _span = tracing::info_span!(target: "regia_fluid", "fluidcache_play").entered();
        if index >= self.frames.len() {
            return Err(CacheError::NoFrame {
                index,
                count: self.frames.len(),
            });
        }
        let mut start = index;
        while start > 0 && self.frames[start].first() != Some(&0) {
            start -= 1;
        }
        let mut prev: Vec<[u16; 3]> = Vec::new();
        let mut decoded = Decoded::default();
        for f in start..=index {
            decoded = decode_frame(&self.frames[f], &prev, self.has_whitewater())?;
            prev = std::mem::take(&mut decoded.liquid);
            if f == index {
                decoded.liquid = std::mem::take(&mut prev);
            }
        }
        let mut frame = FluidFrame {
            step: decoded.step,
            ..FluidFrame::default()
        };
        for q in &decoded.liquid {
            frame.positions.push(self.dequantize(*q));
            frame.kinds.push(PointKind::Liquid as u8);
            frame.ages.push(0.0);
        }
        for (tag, age, q) in &decoded.whitewater {
            frame.positions.push(self.dequantize(*q));
            frame.kinds.push(*tag);
            frame.ages.push(*age as f32 / 100.0);
        }
        Ok(frame)
    }

    /// The file bytes.
    #[must_use]
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(52 + self.payload_bytes() + self.frames.len() * 4);
        out.extend_from_slice(&FLUIDCACHE_MAGIC);
        out.extend_from_slice(&FLUIDCACHE_FORMAT_VERSION.to_le_bytes());
        out.extend_from_slice(&HEADER_V1_LEN.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&(self.frames.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.dt.to_le_bytes());
        for v in self.origin {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.quantum.to_le_bytes());
        // `record` normalizes 0 to 1; a hand-built cache still cannot write a value the reader
        // rejects.
        out.extend_from_slice(&self.keyframe_interval.max(1).to_le_bytes());
        out.extend_from_slice(&self.max_points.to_le_bytes());
        out.extend_from_slice(&self.max_whitewater.to_le_bytes());
        for f in &self.frames {
            out.extend_from_slice(&(f.len() as u32).to_le_bytes());
            out.extend_from_slice(f);
        }
        out
    }

    /// Reads and fully validates a cache (every frame decodes).
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, CacheError> {
        let _span = tracing::info_span!(target: "regia_fluid", "fluidcache_read").entered();
        let mut r = Reader { bytes, at: 0 };
        if r.take(4, "magic")? != FLUIDCACHE_MAGIC {
            return Err(CacheError::BadMagic);
        }
        let version = r.u32("format_version")?;
        if version != FLUIDCACHE_FORMAT_VERSION {
            return Err(CacheError::BadVersion(version));
        }
        let header_len = r.u32("header length")?;
        if header_len < HEADER_V1_LEN {
            return Err(CacheError::Corrupt(format!(
                "header length {header_len} is below {HEADER_V1_LEN}"
            )));
        }
        let header = r.take(header_len as usize, "header")?;
        let mut h = Reader { bytes: header, at: 0 };
        let flags = h.u32("flags")?;
        if flags & !FLAG_WHITEWATER != 0 {
            return Err(CacheError::Corrupt(format!("unknown flags {flags:#x}")));
        }
        let frame_count = h.u32("frame count")?;
        let dt = h.f32("dt")?;
        let origin = [h.f32("origin")?, h.f32("origin")?, h.f32("origin")?];
        let quantum = h.f32("quantum")?;
        let keyframe_interval = h.u32("keyframe interval")?;
        let max_points = h.u32("max points")?;
        let max_whitewater = h.u32("max whitewater")?;
        if !(dt.is_finite() && dt > 0.0)
            || !origin.iter().all(|v| v.is_finite())
            || !(quantum.is_finite() && quantum > 0.0)
        {
            return Err(CacheError::Corrupt(
                "dt, origin and quantum must be finite (dt, quantum > 0)".into(),
            ));
        }
        if keyframe_interval == 0 {
            return Err(CacheError::Corrupt("keyframe interval is 0".into()));
        }
        if frame_count > MAX_FRAMES || max_points > MAX_FRAME_POINTS || max_whitewater > MAX_FRAME_POINTS {
            return Err(CacheError::TooLarge(format!(
                "{frame_count} frames, {max_points} points, {max_whitewater} whitewater"
            )));
        }
        // A frame record is at least 4 + 13 bytes: refuse a count the bytes cannot hold before allocating.
        if u64::from(frame_count) * 17 > r.remaining() as u64 {
            return Err(CacheError::Truncated("frames"));
        }
        let whitewater = flags & FLAG_WHITEWATER != 0;
        let mut frames = Vec::with_capacity(frame_count as usize);
        let mut prev: Vec<[u16; 3]> = Vec::new();
        let (mut seen_points, mut seen_ww) = (0u32, 0u32);
        for n in 0..frame_count {
            let len = r.u32("frame length")?;
            let payload = r.take(len as usize, "frame")?;
            let decoded = decode_frame(payload, &prev, whitewater)?;
            let is_key = payload.first() == Some(&0);
            if n.is_multiple_of(keyframe_interval) != is_key {
                return Err(CacheError::Corrupt(format!(
                    "frame {n} key flag does not match the interval"
                )));
            }
            seen_points = seen_points.max(decoded.liquid.len() as u32);
            seen_ww = seen_ww.max(decoded.whitewater.len() as u32);
            frames.push(payload.to_vec());
            prev = decoded.liquid;
        }
        if r.remaining() != 0 {
            return Err(CacheError::Corrupt(format!(
                "{} bytes after the last frame",
                r.remaining()
            )));
        }
        if seen_points != max_points || seen_ww != max_whitewater {
            return Err(CacheError::Corrupt(
                "header maxima do not match the frames".into(),
            ));
        }
        Ok(Self {
            flags,
            dt,
            origin,
            quantum,
            keyframe_interval,
            frames,
            max_points,
            max_whitewater,
            last: prev,
        })
    }
}

/// The cook step: validates a `.fluidcache` and writes it in canonical form (a v1 file re-encodes
/// to itself; a future version's migrator plugs in here).
pub fn cook(bytes: &[u8]) -> Result<Vec<u8>, CacheError> {
    FluidCache::from_bytes(bytes).map(|c| c.to_bytes())
}

/// `regia upgrade-project`'s probe: the version of a `.fluidcache`, `Ok(None)` when the bytes are not one.
pub fn fluidcache_file_version(bytes: &[u8]) -> Result<Option<u32>, String> {
    if bytes.len() < 8 {
        return Err("file is too short to hold a fluid cache header".into());
    }
    if bytes[..4] != FLUIDCACHE_MAGIC {
        return Ok(None);
    }
    Ok(Some(u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]])))
}

/// The layout fingerprint `cargo xtask check-formats` hashes (a binary format has no serde shape:
/// this text names every field in order and changes with the layout).
#[must_use]
pub fn fluidcache_schema_fingerprint() -> String {
    "fluidcache: magic[4]=RFLC format_version:u32 header_len:u32 | flags:u32(bit0 whitewater) frame_count:u32 \
     dt:f32 origin:[f32;3] quantum:f32 keyframe_interval:u32 max_points:u32 max_whitewater:u32 | frame*: \
     len:u32 kind:u8(0 key,1 delta) step:u64 liquid_count:u32 liquid:[zigzag-leb128 x3]*(delta vs same index) \
     [whitewater_count:u32 (tag:u8 age_centis:leb128 pos:[u16;3])*]"
        .to_string()
}

/// No older versions: no migration steps.
#[must_use]
pub fn fluidcache_migration_sources() -> Vec<u32> {
    Vec::new()
}

fn age_centis(age: f32) -> u32 {
    if age.is_finite() {
        (age.max(0.0) * 100.0).round().min(u32::MAX as f32) as u32
    } else {
        0
    }
}

fn zigzag(v: i32) -> u32 {
    ((v << 1) ^ (v >> 31)) as u32
}

fn unzigzag(v: u32) -> i32 {
    ((v >> 1) as i32) ^ -((v & 1) as i32)
}

fn write_varint(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let byte = (v & 0x7f) as u8;
        v >>= 7;
        if v == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn remaining(&self) -> usize {
        self.bytes.len() - self.at
    }

    fn take(&mut self, n: usize, what: &'static str) -> Result<&'a [u8], CacheError> {
        if self.remaining() < n {
            return Err(CacheError::Truncated(what));
        }
        let s = &self.bytes[self.at..self.at + n];
        self.at += n;
        Ok(s)
    }

    fn u8(&mut self, what: &'static str) -> Result<u8, CacheError> {
        Ok(self.take(1, what)?[0])
    }

    fn u16(&mut self, what: &'static str) -> Result<u16, CacheError> {
        let b = self.take(2, what)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    fn u32(&mut self, what: &'static str) -> Result<u32, CacheError> {
        let b = self.take(4, what)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn u64(&mut self, what: &'static str) -> Result<u64, CacheError> {
        let b = self.take(8, what)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    fn f32(&mut self, what: &'static str) -> Result<f32, CacheError> {
        Ok(f32::from_bits(self.u32(what)?))
    }

    fn varint(&mut self, what: &'static str) -> Result<u32, CacheError> {
        let mut v = 0u32;
        for shift in [0u32, 7, 14, 21, 28] {
            let b = self.u8(what)?;
            if shift == 28 && b > 0x0f {
                return Err(CacheError::Corrupt(format!("{what}: varint overflows 32 bits")));
            }
            v |= u32::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(CacheError::Corrupt(format!("{what}: varint longer than 5 bytes")))
    }
}

fn decode_frame(payload: &[u8], prev: &[[u16; 3]], whitewater: bool) -> Result<Decoded, CacheError> {
    let mut r = Reader {
        bytes: payload,
        at: 0,
    };
    let kind = r.u8("frame kind")?;
    if kind > 1 {
        return Err(CacheError::Corrupt(format!("frame kind {kind}")));
    }
    let delta = kind == 1;
    let step = r.u64("step")?;
    let count = r.u32("liquid count")?;
    if count > MAX_FRAME_POINTS {
        return Err(CacheError::TooLarge(format!("{count} liquid points")));
    }
    // Each coordinate takes at least one byte.
    if u64::from(count) * 3 > r.remaining() as u64 {
        return Err(CacheError::Truncated("liquid points"));
    }
    let mut liquid = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        let base = if delta { prev.get(i) } else { None };
        let mut q = [0u16; 3];
        for (a, slot) in q.iter_mut().enumerate() {
            let d = unzigzag(r.varint("liquid point")?);
            let v = i64::from(base.map_or(0, |b| b[a])) + i64::from(d);
            *slot = u16::try_from(v)
                .map_err(|_| CacheError::Corrupt(format!("point {i} leaves the quantization box")))?;
        }
        liquid.push(q);
    }
    let mut ww = Vec::new();
    if whitewater {
        let n = r.u32("whitewater count")?;
        if n > MAX_FRAME_POINTS {
            return Err(CacheError::TooLarge(format!("{n} whitewater points")));
        }
        if u64::from(n) * 8 > r.remaining() as u64 {
            return Err(CacheError::Truncated("whitewater points"));
        }
        ww.reserve(n as usize);
        for _ in 0..n {
            let tag = r.u8("whitewater tag")?;
            if !matches!(
                PointKind::from_tag(tag),
                Some(PointKind::Foam | PointKind::Spray | PointKind::Bubble)
            ) {
                return Err(CacheError::Corrupt(format!("whitewater tag {tag}")));
            }
            let age = r.varint("whitewater age")?;
            let q = [
                r.u16("whitewater x")?,
                r.u16("whitewater y")?,
                r.u16("whitewater z")?,
            ];
            ww.push((tag, age, q));
        }
    }
    if r.remaining() != 0 {
        return Err(CacheError::Corrupt(format!(
            "{} bytes left in a frame",
            r.remaining()
        )));
    }
    Ok(Decoded {
        step,
        liquid,
        whitewater: ww,
    })
}
