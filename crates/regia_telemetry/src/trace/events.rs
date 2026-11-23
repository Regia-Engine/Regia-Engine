// SPDX-License-Identifier: MPL-2.0
//! Trace events and their wire form (`docs/formats/regiatrace-v1.md`, spec 54 § 4.2): the in-memory [`Event`], the chunk encoder
//! ([`Encoder`]) and the reader ([`read_trace`]).

use std::collections::HashMap;
use std::sync::Arc;

use super::file::{self, Chunk, Cursor, Header, TraceReadError, chunk, put_varint};

/// A lane of the timeline. Agents and game code open ranges on CPU lanes only.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lane {
    /// The main (frame) thread.
    Main,
    /// The render thread.
    Render,
    /// Worker thread `n`.
    Worker(u32),
    /// GPU pass intervals, engine-written only.
    Gpu,
}

impl Lane {
    /// `main`, `render`, `worker:<n>` or `gpu`.
    #[must_use]
    pub fn name(self) -> String {
        match self {
            Self::Main => "main".to_owned(),
            Self::Render => "render".to_owned(),
            Self::Worker(n) => format!("worker:{n}"),
            Self::Gpu => "gpu".to_owned(),
        }
    }

    /// Parses [`Self::name`].
    #[must_use]
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "main" => Some(Self::Main),
            "render" => Some(Self::Render),
            "gpu" => Some(Self::Gpu),
            _ => name.strip_prefix("worker:")?.parse().ok().map(Self::Worker),
        }
    }

    fn code(self) -> (u8, u32) {
        match self {
            Self::Main => (0, 0),
            Self::Render => (1, 0),
            Self::Worker(n) => (2, n),
            Self::Gpu => (3, 0),
        }
    }

    fn from_code(kind: u8, n: u32) -> Option<Self> {
        Some(match kind {
            0 => Self::Main,
            1 => Self::Render,
            2 => Self::Worker(n),
            3 => Self::Gpu,
            _ => return None,
        })
    }
}

/// What happened.
#[derive(Clone, Debug, PartialEq)]
pub enum EventKind {
    /// An instant.
    Mark {
        /// Name.
        name: String,
        /// Optional JSON text (at most 1 KiB).
        data: Option<String>,
        /// App frame.
        frame: u64,
    },
    /// A range opened.
    RangeOpen {
        /// Range id.
        id: u64,
        /// Name.
        name: String,
        /// App frame.
        frame: u64,
        /// Nesting depth on its lane.
        depth: u32,
        /// The enclosing range, when any.
        parent: Option<u64>,
    },
    /// A range closed.
    RangeClose {
        /// Range id.
        id: u64,
        /// App frame.
        frame: u64,
        /// Closed because an outer range closed first, or at stop.
        forced: bool,
    },
    /// A frame began.
    FrameBegin {
        /// App frame.
        frame: u64,
    },
    /// A frame ended.
    FrameEnd {
        /// App frame.
        frame: u64,
    },
    /// A breadcrumb pass opened.
    PassOpen {
        /// Pass tag.
        tag: u16,
        /// Pass name.
        name: Arc<str>,
        /// App frame.
        frame: u64,
        /// Submission.
        submission: u64,
    },
    /// A breadcrumb pass closed.
    PassClose {
        /// Pass tag.
        tag: u16,
        /// Pass name.
        name: Arc<str>,
        /// App frame.
        frame: u64,
        /// Submission.
        submission: u64,
    },
    /// A timestamped GPU pass interval.
    GpuInterval {
        /// Pass tag.
        tag: u16,
        /// Pass name.
        name: Arc<str>,
        /// Begin, ns on the trace time base.
        begin_ns: u64,
        /// End, ns on the trace time base.
        end_ns: u64,
        /// App frame.
        frame: u64,
    },
    /// A log record.
    Log {
        /// 0 error .. 4 trace.
        level: u8,
        /// Target.
        target: String,
        /// Text.
        text: String,
    },
    /// A counter sample.
    Counter {
        /// Name.
        name: String,
        /// Value.
        value: f64,
    },
    /// A screenshot mark.
    Screenshot {
        /// Name.
        name: Option<String>,
        /// App frame.
        frame: u64,
        /// Width.
        width: u32,
        /// Height.
        height: u32,
        /// PNG path relative to the trace file's folder.
        file: String,
    },
}

/// One event: when, where, what.
#[derive(Clone, Debug, PartialEq)]
pub struct Event {
    /// Nanoseconds since process start.
    pub t_ns: u64,
    /// Lane.
    pub lane: Lane,
    /// Payload.
    pub kind: EventKind,
}

impl Event {
    /// A rough in-memory size, for the window's byte cap.
    #[must_use]
    pub fn approx_bytes(&self) -> usize {
        let strings = match &self.kind {
            EventKind::Mark { name, data, .. } => name.len() + data.as_ref().map_or(0, String::len),
            EventKind::RangeOpen { name, .. } | EventKind::Counter { name, .. } => name.len(),
            EventKind::Log { target, text, .. } => target.len() + text.len(),
            EventKind::Screenshot { name, file, .. } => name.as_ref().map_or(0, String::len) + file.len(),
            _ => 0,
        };
        64 + strings
    }
}

const BATCH_FLUSH_BYTES: usize = 64 * 1024;

/// Stack buffer for one event payload. The largest ordinary event is a code byte, a time delta,
/// a lane id and five `u64` fields. A varint is at most 10 bytes, so 96 bytes covers that and a
/// screenshot's six fields.
struct Scratch {
    buf: [u8; 96],
    len: usize,
}

impl Scratch {
    fn new() -> Self {
        Self { buf: [0; 96], len: 0 }
    }

    fn push(&mut self, byte: u8) {
        debug_assert!(self.len < self.buf.len(), "trace event exceeded the stack buffer");
        if let Some(slot) = self.buf.get_mut(self.len) {
            *slot = byte;
            self.len += 1;
        }
    }

    fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7F) as u8;
            v >>= 7;
            if v == 0 {
                self.push(byte);
                return;
            }
            self.push(byte | 0x80);
        }
    }

    fn raw(&mut self, bytes: &[u8]) {
        for byte in bytes {
            self.push(*byte);
        }
    }

    fn bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Encodes events into chunks. Strings, lanes and pass names are defined by chunks that precede
/// their first use.
#[derive(Default)]
pub struct Encoder {
    strings: HashMap<String, u64>,
    new_strings: Vec<u8>,
    lanes: HashMap<Lane, u64>,
    new_lanes: Vec<u8>,
    passes: HashMap<u16, u64>,
    new_passes: Vec<u8>,
    events: Vec<u8>,
    last_t: u64,
    /// Events encoded.
    pub events_written: u64,
}

impl Encoder {
    /// A fresh encoder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn sid(&mut self, s: &str) -> u64 {
        if let Some(&id) = self.strings.get(s) {
            return id;
        }
        let id = self.strings.len() as u64 + 1;
        self.strings.insert(s.to_owned(), id);
        put_varint(&mut self.new_strings, id);
        put_varint(&mut self.new_strings, s.len() as u64);
        self.new_strings.extend_from_slice(s.as_bytes());
        id
    }

    fn lane_id(&mut self, lane: Lane) -> u64 {
        if let Some(&id) = self.lanes.get(&lane) {
            return id;
        }
        let id = self.lanes.len() as u64;
        self.lanes.insert(lane, id);
        let name = self.sid(&lane.name());
        let (kind, n) = lane.code();
        put_varint(&mut self.new_lanes, id);
        self.new_lanes.push(kind);
        put_varint(&mut self.new_lanes, u64::from(n));
        put_varint(&mut self.new_lanes, name);
        id
    }

    fn pass_name(&mut self, tag: u16, name: &str) {
        if self.passes.contains_key(&tag) {
            return;
        }
        let sid = self.sid(name);
        self.passes.insert(tag, sid);
        put_varint(&mut self.new_passes, u64::from(tag));
        put_varint(&mut self.new_passes, sid);
    }

    /// Cuts the pending definitions and events into chunks appended to `out`.
    pub fn cut(&mut self, out: &mut Vec<u8>) {
        if !self.new_strings.is_empty() {
            out.extend(file::encode_chunk(chunk::STRINGS, &self.new_strings));
            self.new_strings.clear();
        }
        if !self.new_lanes.is_empty() {
            out.extend(file::encode_chunk(chunk::LANES, &self.new_lanes));
            self.new_lanes.clear();
        }
        if !self.new_passes.is_empty() {
            out.extend(file::encode_chunk(chunk::PASSES, &self.new_passes));
            self.new_passes.clear();
        }
        if !self.events.is_empty() {
            out.extend(file::encode_chunk(chunk::EVENTS, &self.events));
            self.events.clear();
        }
    }

    /// Encodes `e` into a stack buffer. Chunks that became complete are appended to `out`.
    /// Field lists are not allocated: a steady pass event only grows `events` until the next cut.
    pub fn push(&mut self, e: &Event, out: &mut Vec<u8>) {
        self.events_written += 1;
        let t = e.t_ns.max(self.last_t);
        match &e.kind {
            EventKind::Counter { name, value } => {
                let name = self.sid(name);
                let mut p = Scratch::new();
                p.varint(name);
                p.varint(t);
                p.raw(&value.to_le_bytes());
                self.cut(out);
                out.extend(file::encode_chunk(chunk::COUNTERS, p.bytes()));
                return;
            }
            EventKind::Screenshot {
                name,
                frame,
                width,
                height,
                file,
            } => {
                let name = name.as_deref().map_or(0, |n| self.sid(n));
                let file = self.sid(file);
                let mut p = Scratch::new();
                for v in [t, *frame, u64::from(*width), u64::from(*height), name, file] {
                    p.varint(v);
                }
                self.cut(out);
                out.extend(file::encode_chunk(chunk::SCREENSHOT, p.bytes()));
                return;
            }
            _ => {}
        }
        let lane = self.lane_id(e.lane);
        let mut fields = [0u64; 5];
        let (code, n) = match &e.kind {
            EventKind::Mark { name, data, frame } => {
                fields[0] = self.sid(name);
                fields[1] = data.as_deref().map_or(0, |d| self.sid(d));
                fields[2] = *frame;
                (1, 3)
            }
            EventKind::RangeOpen {
                id,
                name,
                frame,
                depth,
                parent,
            } => {
                fields[0] = *id;
                fields[1] = self.sid(name);
                fields[2] = *frame;
                fields[3] = u64::from(*depth);
                fields[4] = parent.map_or(0, |p| p + 1);
                (2, 5)
            }
            EventKind::RangeClose { id, frame, forced } => {
                fields[0] = *id;
                fields[1] = *frame;
                fields[2] = u64::from(*forced);
                (3, 3)
            }
            EventKind::FrameBegin { frame } => {
                fields[0] = *frame;
                (4, 1)
            }
            EventKind::FrameEnd { frame } => {
                fields[0] = *frame;
                (5, 1)
            }
            EventKind::PassOpen {
                tag,
                name,
                frame,
                submission,
            } => {
                self.pass_name(*tag, name);
                fields[0] = u64::from(*tag);
                fields[1] = *frame;
                fields[2] = *submission;
                (6, 3)
            }
            EventKind::PassClose {
                tag,
                name,
                frame,
                submission,
            } => {
                self.pass_name(*tag, name);
                fields[0] = u64::from(*tag);
                fields[1] = *frame;
                fields[2] = *submission;
                (9, 3)
            }
            EventKind::GpuInterval {
                tag,
                name,
                begin_ns,
                end_ns,
                frame,
            } => {
                self.pass_name(*tag, name);
                fields[0] = u64::from(*tag);
                fields[1] = *begin_ns;
                fields[2] = *end_ns;
                fields[3] = *frame;
                (7, 4)
            }
            EventKind::Log { level, target, text } => {
                fields[0] = u64::from(*level);
                fields[1] = self.sid(target);
                fields[2] = self.sid(text);
                (8, 3)
            }
            EventKind::Counter { .. } | EventKind::Screenshot { .. } => return,
        };
        let mut encoded = Scratch::new();
        encoded.push(code);
        encoded.varint(t - self.last_t);
        encoded.varint(lane);
        for field in &fields[..n] {
            encoded.varint(*field);
        }
        self.last_t = t;
        self.events.extend_from_slice(encoded.bytes());
        if self.events.len() >= BATCH_FLUSH_BYTES {
            self.cut(out);
        }
    }
}

/// What [`read_trace`] returns.
#[derive(Clone, Debug, Default)]
pub struct TraceFile {
    /// The header.
    pub header: Header,
    /// Events in file order (events, counters and screenshots interleaved as written).
    pub events: Vec<Event>,
    /// The file ended inside a chunk (truncated or damaged); everything before it was read.
    pub truncated: bool,
    /// Chunks of an unknown kind that were skipped.
    pub unknown_chunks: u32,
    /// The close index: `(second, file offset)`, empty when the file was not closed.
    pub index: Vec<(u64, u64)>,
}

#[derive(Default)]
struct Tables {
    strings: HashMap<u64, String>,
    lanes: HashMap<u64, Lane>,
    passes: HashMap<u16, Arc<str>>,
}

impl Tables {
    fn s(&self, id: u64) -> String {
        self.strings.get(&id).cloned().unwrap_or_default()
    }
}

/// Reads a trace file from disk.
pub fn read_trace(path: &std::path::Path) -> Result<TraceFile, TraceReadError> {
    let bytes = std::fs::read(path).map_err(|e| TraceReadError::Io(e.to_string()))?;
    read_trace_bytes(&bytes)
}

/// Reads a trace from memory.
pub fn read_trace_bytes(bytes: &[u8]) -> Result<TraceFile, TraceReadError> {
    let (header, from) = file::decode_header(bytes)?;
    let (chunks, truncated) = file::read_chunks(bytes, from);
    let mut tables = Tables::default();
    let mut out = TraceFile {
        header,
        truncated,
        ..TraceFile::default()
    };
    let mut last_t = 0u64;
    for c in &chunks {
        if decode_chunk(c, &mut tables, &mut out, &mut last_t).is_none() {
            out.truncated = true;
            break;
        }
    }
    Ok(out)
}

fn decode_chunk(c: &Chunk, t: &mut Tables, out: &mut TraceFile, last_t: &mut u64) -> Option<()> {
    let mut cur = Cursor::new(&c.payload);
    match c.kind {
        chunk::STRINGS => {
            while !cur.done() {
                let id = cur.varint()?;
                let len = usize::try_from(cur.varint()?).ok()?;
                t.strings
                    .insert(id, String::from_utf8_lossy(cur.take(len)?).into_owned());
            }
        }
        chunk::LANES => {
            while !cur.done() {
                let id = cur.varint()?;
                let kind = cur.u8()?;
                let n = u32::try_from(cur.varint()?).ok()?;
                let _name = cur.varint()?;
                t.lanes.insert(id, Lane::from_code(kind, n)?);
            }
        }
        chunk::PASSES => {
            while !cur.done() {
                let tag = u16::try_from(cur.varint()?).ok()?;
                let sid = cur.varint()?;
                t.passes.insert(tag, Arc::from(t.s(sid)));
            }
        }
        chunk::EVENTS => {
            while !cur.done() {
                let code = cur.u8()?;
                *last_t = last_t.checked_add(cur.varint()?)?;
                let lane = *t.lanes.get(&cur.varint()?)?;
                let pass = |tag: u64, t: &Tables| -> Option<(u16, Arc<str>)> {
                    let tag = u16::try_from(tag).ok()?;
                    Some((tag, t.passes.get(&tag).cloned().unwrap_or_else(|| Arc::from(""))))
                };
                let kind = match code {
                    1 => {
                        let name = t.s(cur.varint()?);
                        let d = cur.varint()?;
                        EventKind::Mark {
                            name,
                            data: (d != 0).then(|| t.s(d)),
                            frame: cur.varint()?,
                        }
                    }
                    2 => {
                        let id = cur.varint()?;
                        let name = t.s(cur.varint()?);
                        let frame = cur.varint()?;
                        let depth = u32::try_from(cur.varint()?).ok()?;
                        let p = cur.varint()?;
                        EventKind::RangeOpen {
                            id,
                            name,
                            frame,
                            depth,
                            parent: p.checked_sub(1),
                        }
                    }
                    3 => EventKind::RangeClose {
                        id: cur.varint()?,
                        frame: cur.varint()?,
                        forced: cur.varint()? != 0,
                    },
                    4 => EventKind::FrameBegin { frame: cur.varint()? },
                    5 => EventKind::FrameEnd { frame: cur.varint()? },
                    6 | 9 => {
                        let (tag, name) = pass(cur.varint()?, t)?;
                        let frame = cur.varint()?;
                        let submission = cur.varint()?;
                        if code == 6 {
                            EventKind::PassOpen {
                                tag,
                                name,
                                frame,
                                submission,
                            }
                        } else {
                            EventKind::PassClose {
                                tag,
                                name,
                                frame,
                                submission,
                            }
                        }
                    }
                    7 => {
                        let (tag, name) = pass(cur.varint()?, t)?;
                        EventKind::GpuInterval {
                            tag,
                            name,
                            begin_ns: cur.varint()?,
                            end_ns: cur.varint()?,
                            frame: cur.varint()?,
                        }
                    }
                    8 => EventKind::Log {
                        level: u8::try_from(cur.varint()?).ok()?,
                        target: t.s(cur.varint()?),
                        text: t.s(cur.varint()?),
                    },
                    _ => return None,
                };
                out.events.push(Event {
                    t_ns: *last_t,
                    lane,
                    kind,
                });
            }
        }
        chunk::COUNTERS => {
            let name = t.s(cur.varint()?);
            let at = cur.varint()?;
            out.events.push(Event {
                t_ns: at,
                lane: Lane::Main,
                kind: EventKind::Counter {
                    name,
                    value: cur.f64()?,
                },
            });
        }
        chunk::SCREENSHOT => {
            let at = cur.varint()?;
            let frame = cur.varint()?;
            let width = u32::try_from(cur.varint()?).ok()?;
            let height = u32::try_from(cur.varint()?).ok()?;
            let name = cur.varint()?;
            let file = t.s(cur.varint()?);
            out.events.push(Event {
                t_ns: at,
                lane: Lane::Main,
                kind: EventKind::Screenshot {
                    name: (name != 0).then(|| t.s(name)),
                    frame,
                    width,
                    height,
                    file,
                },
            });
        }
        chunk::INDEX => {
            while !cur.done() {
                out.index.push((cur.varint()?, cur.varint()?));
            }
        }
        _ => out.unknown_chunks += 1,
    }
    Some(())
}

#[cfg(test)]
mod decode_overflow_tests {
    use super::*;
    use crate::trace::file::{chunk, put_varint};

    /// Two events whose deltas overflow `u64` end the chunk (a truncated read) instead of
    /// panicking in debug builds or wrapping the timestamp in release builds.
    #[test]
    fn event_delta_overflow_ends_the_chunk() {
        let mut payload = Vec::new();
        for delta in [u64::MAX, 5] {
            payload.push(1); // Mark
            put_varint(&mut payload, delta);
            put_varint(&mut payload, 0); // lane id
            put_varint(&mut payload, 0); // name sid
            put_varint(&mut payload, 0); // no data
            put_varint(&mut payload, 0); // frame
        }
        let c = Chunk {
            kind: chunk::EVENTS,
            payload,
            offset: 0,
        };
        let mut tables = Tables::default();
        tables.lanes.insert(0, Lane::Main);
        let mut out = TraceFile::default();
        let mut last_t = 0u64;
        assert!(decode_chunk(&c, &mut tables, &mut out, &mut last_t).is_none());
        assert_eq!(last_t, u64::MAX);
    }

    /// A varint whose 10th byte carries payload bits beyond the one a `u64` has room for is
    /// corrupt: the shift would silently drop them and the event would decode with a truncated
    /// timestamp. The chunk ends instead (a truncated read) and no event is yielded.
    #[test]
    fn event_delta_varint_with_excess_10th_byte_bits_ends_the_chunk() {
        let mut payload = Vec::new();
        payload.push(1); // Mark
        // Ten bytes: nine continuation bytes, then a 10th with payload 3 (only 0 or 1 fits).
        payload.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x03]);
        payload.extend_from_slice(&[0, 0, 0, 0]); // lane 0, no name, no data, frame 0
        let c = Chunk {
            kind: chunk::EVENTS,
            payload,
            offset: 0,
        };
        let mut tables = Tables::default();
        tables.lanes.insert(0, Lane::Main);
        let mut out = TraceFile::default();
        let mut last_t = 0u64;
        assert!(decode_chunk(&c, &mut tables, &mut out, &mut last_t).is_none());
        assert!(out.events.is_empty(), "no event from a corrupt delta");
        assert_eq!(last_t, 0);
    }

    /// The full `u64` delta range still encodes and decodes: `1 << 63` is nine continuation
    /// bytes and a 10th with payload 1, the last bit a `u64` has room for.
    #[test]
    fn event_delta_varint_may_use_the_full_u64_range() {
        let mut payload = Vec::new();
        payload.push(1); // Mark
        payload.extend_from_slice(&[0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0x01]);
        payload.extend_from_slice(&[0, 0, 0, 0]); // lane 0, no name, no data, frame 0
        let c = Chunk {
            kind: chunk::EVENTS,
            payload,
            offset: 0,
        };
        let mut tables = Tables::default();
        tables.lanes.insert(0, Lane::Main);
        let mut out = TraceFile::default();
        let mut last_t = 0u64;
        assert!(decode_chunk(&c, &mut tables, &mut out, &mut last_t).is_some());
        assert_eq!(out.events.len(), 1);
        assert_eq!(last_t, 1 << 63);
    }
}
